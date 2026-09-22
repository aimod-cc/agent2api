//! 凭证自动维护：把「已过期 / 临期」的账号凭证批量刷一遍。
//!
//! ── 这个模块解决什么问题 ─────────────────────────────────────
//! 转发链路本来就有懒刷新（各家适配器的 `ensure_access_token` 在临期时自己续期），
//! 所以**转发不受影响**。缺的是「没人请求的时候」这一面：账号页上的「有效期」
//! 一直显示已过期，用户以为凭证坏了 —— 而它只是没被碰过。
//! 本模块就是那个「定期碰一下」的动作：遍历账号 → 问适配器要不要刷 → 刷。
//!
//! ── 为什么判定逻辑在适配器而不是这里 ─────────────────────────
//! 「过期时间在哪个字段、什么算临期」是**各家的知识**：workbuddy 的过期时间在
//! 会话的 `auth.expiresAt`、小浣熊在 JWT 的 `exp`（公开形态里叫 `tokenExpiresAt`）、
//! AutoClaw 在凭证的 `expiresAt`，三家的临期窗口还各不相同（5 分钟 / 5 分钟 /
//! 5 分钟）。壳侧曾按「顶层 `expiresAt`」这一个字段名做判断，于是小浣熊账号
//! （字段名不同）**永远被判成无需刷新** —— 全部过期也不刷，且界面上看不出原因。
//! 现在这条判断收进 `ProviderAdapter::credentials_expiring`，加一家 provider
//! 不必再改本模块，也不会再出现「字段名对不上」的静默失效。
//!
//! ── 为什么先过滤再刷新（重要）────────────────────────────────
//! 先过 `supports_refresh()` 与 `credentials_expiring()` 两道闸，再动手调
//! `refresh_access_token`。理由：那个方法对「没有续期手段」的家会返回 Err
//! （这是它的契约，见 `api::accounts::refresh_account` 的用法）——
//! 拿全部账号直接调，会把 CatPaw（没有刷新接口）、没有 refreshToken 的账号
//! 全部变成失败记录，每轮刷屏一次日志，真正需要关注的失败反而被淹掉。
//!
//! ── 为什么失败不上抛（HTTP 层永远 2xx）──────────────────────
//! 这是一个**批量**动作，「整体跑完了，其中两个账号没刷上」是最常见的结局。
//! 把单个账号的失败变成整体非 2xx，会让调用方（后台维护、将来的手动入口）
//! 必须去解析错误响应才能知道「到底刷成了几个」，而它真正要的就是那份逐条
//! 结果。因此失败全部落在 `results[i].status = "failed"` 里，由界面决定怎么
//! 展示。真正会抛错的只有「整个请求都发不出去」那一层（由 HTTP 客户端负责）。
//!
//! ── 锁与 panic 的两条硬约束 ─────────────────────────────────
//!   1. **不持锁跨 await**：账号快照与逐账号的临期判定都是同步的（各自的取数
//!      函数内部取锁即释放），网络刷新在锁外逐个 await；
//!   2. **不 unwrap/expect**：本模块在后台任务与 HTTP handler 两条路径上跑，
//!      而 release 是 `panic = "abort"`，任何一处 panic 都会带走整个应用。
//!
//! 与 `auto_checkin.rs` 同构：外层只负责「何时做」，做事的逻辑在这里，
//! 调用点（启动 + 周期循环）在 `ServerState::bootstrap`。

use serde_json::{json, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::core::providers::adapter::adapter_for;
use crate::server::core::providers::{kind_from_id, kind_id, ProviderKind};
use crate::server::logging;

/// 逐条结果的状态取值（**字符串本身就是给前端与日志看的契约**）
pub const STATUS_REFRESHED: &str = "refreshed";
/// 不需要刷新，或该家不支持主动刷新
pub const STATUS_SKIPPED: &str = "skipped";
/// 尝试刷新但失败（结果里带可读的 `message`）
pub const STATUS_FAILED: &str = "failed";

/// 周期维护的**默认**间隔说明：10 分钟。
///
/// 为什么默认是 10 分钟（而不是更密）：三家的临期窗口是 2~5 分钟，10 分钟的间隔
/// 意味着「进入窗口后最多等一轮就被刷」——对「账号页显示得对不对」这个目的
/// 足够。反过来，绝大多数轮次是空操作（没有临期账号时直接早退），
/// 间隔调密只是让空转变多，不会让凭证更早恢复；调疏则会让列表上的
/// 「已过期」停留更久。
///
/// ── 间隔是可配的，本模块不再持有它 ────────────────────────────
/// 调度由 `core::scheduled_tasks` 负责，间隔取自 config.json 的
/// `scheduledTasks.credentialMaintenance.interval`；默认值（10 分钟）定义在
/// `config::DEFAULT_CREDENTIAL_MAINTENANCE_MINUTES`，在定时任务页可改。
/// 本模块因此不再导出间隔常量 —— 留一个没人读的数字只会让人以为改它有效。

/// 遍历全部账号，把「该家支持刷新 且 凭证已过期 / 临期」的那些刷新一遍。
///
/// 返回**逐账号**结果：`[{ id, provider, status, message? }, ...]`，顺序与账号
/// 文件一致（快照顺序）。`status` 取本模块顶部的三个常量之一；`message` 只在
/// `skipped`（说明为什么跳过）与 `failed`（上游/本地的失败原因）时出现。
///
/// ── 结果里的账号范围 ─────────────────────────────────────────
/// **每个账号都有一条**（含禁用账号与不支持刷新的家），这样汇总里的
/// 成功 / 跳过 / 失败三个数能对上账号总数，界面不必自己补算。
/// 代价是每轮返回十几到二十条记录 —— 这是 127.0.0.1 上的一次本地调用，
/// 相比「要刷新谁」这个信息本身，传输开销可以忽略。
///
/// ── 失败隔离 ────────────────────────────────────────────────
/// 单个账号刷新失败只记一条失败结果与一行日志，**不影响其余账号**：
/// 一个 refreshToken 被吊销的账号不该让整批维护停摆。
///
/// ── 返回值不泄露凭证 ────────────────────────────────────────
/// 只回 `id` / `provider` / 状态 / 文案，绝不回 token 内容。`GatewayError` 的
/// 文案本来也由我们构造（不含凭证），与 `/api/accounts/refresh` 报给前端的是
/// 同一份口径。
pub async fn refresh_expiring_accounts(store: &AccountStore) -> Vec<Value> {
    let snapshot = store.list_accounts();
    let accounts: Vec<Value> = snapshot
        .get("accounts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    // 第一遍（同步）：分拣出「待刷新」的清单，其余当场落 skipped 结果。
    // 账号快照与临期判定都在这一遍做完，下面那一遍只做网络动作。
    //
    // 结果按**账号下标**占位（`slots[i]`），刷新那一遍写回各自的位置 ——
    // 于是返回顺序与账号列表一致，而不是「先一堆跳过、后面才是刷新的」。
    // 界面上逐条对照时，顺序一致省掉一次按 id 重排。
    let mut slots: Vec<Option<Value>> = vec![None; accounts.len()];
    let mut pending: Vec<(usize, String, ProviderKind)> = Vec::new();
    for (index, account) in accounts.iter().enumerate() {
        let id = account
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if id.is_empty() {
            continue;
        }
        let provider = account
            .get("provider")
            .and_then(Value::as_str)
            .unwrap_or("");
        // 判据只有两条：该家支持刷新 + 这个账号的凭证确实临期。
        // **不按 enabled 过滤**：禁用是「转发别用它」，不是「别维护它」——
        // 账号页对禁用账号同样显示有效期，漏刷会让它一直挂着「已过期」，
        // 正是本次要修的那类问题；而刷新本身不改启用状态，没有副作用。
        // 未注册的 provider id（手改文件塞进来的）没有适配器可问，如实跳过
        let Some(kind) = kind_from_id(provider) else {
            slots[index] = Some(skipped(&id, provider, "未注册的提供商"));
            continue;
        };
        let adapter = adapter_for(kind);
        if !adapter.supports_refresh() {
            slots[index] = Some(skipped(
                &id,
                provider,
                "该提供商不支持主动刷新凭证（没有续期手段）",
            ));
            continue;
        }
        // 临期判定的详细理由见 trait 的 `credentials_expiring`：
        // 判不出来（没有过期信息 / 账号不存在 / 没有 refreshToken）一律 false
        if !adapter.credentials_expiring(store, &id) {
            slots[index] = Some(skipped(&id, provider, "凭证未过期或临期"));
            continue;
        }
        pending.push((index, id, kind));
    }

    if pending.is_empty() {
        // 绝大多数轮次走到这里（没有临期账号）：只留一行 verbose，
        // 不写日志库 —— 每 10 分钟一条「什么都没做」的记录会把日志页淹掉
        logging::verbose(
            "[Accounts]",
            &format!("凭证自动维护：无需刷新的账号（共 {} 个）", accounts.len()),
        );
        return slots.into_iter().flatten().collect();
    }

    logging::log(
        "[Accounts]",
        &format!("检测到 {} 个临期账号，开始自动刷新凭证…", pending.len()),
    );
    for (index, id, kind) in pending {
        let adapter = adapter_for(kind);
        let provider = kind_id(kind);
        // 走适配器而不是别处：这条已经是四家统一的刷新入口
        // （小浣熊 / AutoClaw 会按来源比较-再写回账号记录，workbuddy 走
        //  `AuthService::refresh_account`），这里不需要知道任何一家的协议。
        //
        // 逐个 await（不并发）：刷新是低频维护动作、单个账号要打一次上游，
        // 串行既避开风控也保证日志顺序稳定；失败被 match 收敛成一条结果，
        // 不会中断循环（见模块头的失败隔离）。
        slots[index] = Some(match adapter.refresh_access_token(store, &id).await {
            Ok(_) => {
                // 成功也打一条：用户在日志页能看到「网关自动刷新了谁的凭证」，
                // 否则「token 怎么自己变新了」是个无法追溯的问题
                logging::log("[Accounts]", &format!("✅ 已自动刷新账号 {id} 的凭证"));
                json!({
                    "id": id,
                    "provider": provider,
                    "status": STATUS_REFRESHED,
                })
            }
            Err(error) => {
                logging::log(
                    "[Accounts]",
                    &format!("❌ 账号 {id} 凭证自动刷新失败: {}", error.message),
                );
                json!({
                    "id": id,
                    "provider": provider,
                    "status": STATUS_FAILED,
                    "message": error.message,
                })
            }
        });
    }

    let results: Vec<Value> = slots.into_iter().flatten().collect();
    let (refreshed, skipped_count, failed) = summarize(&results);
    // 汇总级别跟结果走：「失败 0 个」是正常收尾，不能因文案带「失败」被抬成 error
    logging::log_with_level(
        "[Accounts]",
        &format!("凭证自动维护完成：成功 {refreshed}，跳过 {skipped_count}，失败 {failed}"),
        if failed > 0 { "error" } else { "info" },
    );
    results
}

/// 维护任务的**响应体**：`{ results, refreshed, skipped, failed }`。
///
/// 汇总与逐条一起给出，界面既能一句提示（「已自动刷新 N 个账号」）也能展开看
/// 每个账号为什么被跳过 / 失败。形状由本模块定，路由层原样透出 ——
/// handler 薄到只剩一行，`api/accounts.rs` 不必再拼一次 JSON。
pub async fn refresh_expiring_report(store: &AccountStore) -> Value {
    let results = refresh_expiring_accounts(store).await;
    let (refreshed, skipped_count, failed) = summarize(&results);
    json!({
        "results": results,
        "refreshed": refreshed,
        "skipped": skipped_count,
        "failed": failed,
    })
}

/// 统计逐条结果里的三档状态：`(refreshed, skipped, failed)`。
///
/// 供路由层拼汇总用（本模块内部打日志也走它）—— 统计口径只写一份，
/// 免得「日志说成功 2、响应说成功 3」这种对不上的情况。
pub fn summarize(results: &[Value]) -> (usize, usize, usize) {
    let count = |status: &str| {
        results
            .iter()
            .filter(|item| item.get("status").and_then(Value::as_str) == Some(status))
            .count()
    };
    (
        count(STATUS_REFRESHED),
        count(STATUS_SKIPPED),
        count(STATUS_FAILED),
    )
}

/// 一条 `skipped` 结果（`reason` 给用户看，说明为什么没刷）
fn skipped(id: &str, provider: &str, reason: &str) -> Value {
    json!({
        "id": id,
        "provider": provider,
        "status": STATUS_SKIPPED,
        "message": reason,
    })
}
