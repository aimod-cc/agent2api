//! 账号选路与 429 轮换（从 mod.rs 拆出，单文件行数约定）。
//!
//! 对应 Node 版 workbuddy-upstream-client.mjs 的这几段：
//!   selectTargetAccount   三级选路（优先级挑 → 全限额/全忙时恢复最早或按余量挤占 → 全禁用报 503）
//!   withProxyNotice       代理解析失败时记日志并回退直连
//!   requireSession        无可用登录态时报 401
//!   markAccountLimited    限额标记落盘 + 恢复时间文案
//!   reportLimitEvent      429 结构化事件上报（桌面端日志页的「账号 A → B」链路）
//!
//! ── 为什么是自由函数而不是 `impl UpstreamService` 的方法 ──────
//! 这些函数无一例外都要「读账号存储 + 读/写限额 + 打日志」，本质是对
//! 账号存储的操作，而不是转发器自身的行为；拆成 `fn(&UpstreamService, ...)`
//! 后 mod.rs 只保留「一次转发的编排」，两类关注点不再互相淹没。
//! 作为 `mod.rs` 的子模块声明（`mod rotate;`），它可以直接看到
//! `UpstreamService` 的私有字段（Rust 的私有项对后代模块可见），
//! 所以不需要为它们开访问器。
//!
//! ── 全局一条队列：候选集合 = 「能提供该模型的那些 provider」的全部账号 ──
//! 选路入口收一个 `providers` 集合（`router::route_for_forward` 给出的、清单里
//! 有这个模型名的家），候选账号先按 `account.provider ∈ providers` 过滤，再按
//! **全局**优先级挑 —— 四家账号混在同一条队里，谁的号小谁先用。
//! 「workbuddy 的请求不会借到 raccoon 的账号」这条仍然成立：raccoon 若不提供
//! 这个模型名，它就不在 `providers` 里。限额冷却键仍是账号记录内的
//! `rateLimits[model]`，账号唯一确定 provider，无需改结构。
//!
//! ── 冷却键里的 `model` 是**上游真名**（本文件另一个要紧的口径）──────
//! 上面那个「账号唯一确定 provider」的结论正是真名解析的前提：同一个请求名
//! 在 workbuddy 与 catpaw 两家可能各自映射到**不同**的上游模型，而每条账号
//! 记录只属于一家，所以「按账号所属的家解析真名」永远只有一个答案。
//! 判定侧用 [`routing::CooldownKeys`]，写入侧（`mark_account_limited`）由调用方
//! 传入已经解析好的真名 —— 两处必须同源，否则冷却会写在一个键上、查在另一个
//! 键上（那正是 2026-09 那次「映射生效了、请求却仍然打到已限额账号」的成因，
//! 见 `routing::CooldownKeys` 的说明）。
//!
//! ── 11128 退避去哪了 ───────────────────────────────────────
//! 改造前 `request_with_waf_retry` 在本文件里（含「11128 → 10s/25s」的判定）。
//! 那个码是 workbuddy 的专属知识，已随转发改造搬进
//! `providers::workbuddy::retry_advice`；**重试循环**留在编排层
//! （`provider_loop::send_with_retry`），因为「重试几次、打什么日志」
//! 是编排职责（架构文档 §4.3）。

use std::collections::HashMap;

use serde_json::{json, Value};

use crate::server::core::proxies::ResolvedProxy;
use crate::server::core::routing;
use crate::server::errors::{self, GatewayError};
use crate::server::logging;

use super::{account_display, format_reset_text, has_access_token, priority_of, RouteTarget,
            UpstreamService};

/// 取指定账号的会话；`account_id` 为 None 时回落到**该 provider 的**默认登录态。
///
/// 没有可用登录态时报 401（文案对齐 Node 的 requireSession，去掉
/// 「node server.mjs --login」那半句 —— 壳内 Rust 版没有那个命令行入口）。
///
/// `provider` 是本次转发要用的 provider id：默认登录态的派生必须收窄到它，
/// 否则「只有 raccoon 账号」的机器上 workbuddy 请求会借到 raccoon 的凭证。
pub(super) async fn session_for(
    service: &UpstreamService,
    provider: &str,
    account_id: Option<&str>,
) -> Result<Value, GatewayError> {
    if let Some(id) = account_id {
        if let Some(entry) = service.store.get_session_by_id(id) {
            return Ok(entry.session);
        }
        // 指定账号在两次读盘之间被删掉：回落到该 provider 的默认登录态
        // （对应 Node 的 `?? await requireSession()`）
    }
    let session = service
        .auth
        .get_current_session_for(provider)
        .await
        .map_err(|error| error.to_gateway_error())?;
    match session {
        Some(session) if has_access_token(&session) => Ok(session),
        _ => Err(GatewayError::with_status(
            401,
            "当前没有可用登录态：请先在桌面端完成登录",
        )),
    }
}

/// 本次请求使用的账号（对照 Node 的 selectTargetAccount，三级顺序）。
///
///   1. 按**全局**优先级选（跳过禁用、限额冷却中与已达并发上限的账号），
///      逐个向前找——跳过没有可用凭证的记录（避免选到空账号）；
///   2. 剩下的启用账号都在限额冷却期 / 已达并发上限 → 先在「未达并发上限」
///      的账号里挑恢复最早的一个试一次；一个都没有（全部达到并发上限）→
///      按**余量**挤占账号强塞（取舍见下方代码处的说明），
///      把上游真实的 429（含恢复时间）返回给客户端；
///   3. 全部禁用 → 明确报 503，绝不回退到已禁用账号。
///
/// 候选集合 = `providers` 里各家的全部账号（见模块头）。`providers` 非空。
///
/// `keys` 是请求名到各家上游真名的解析器：限额冷却按**真名**判定（理由见
/// `routing::CooldownKeys`）—— 传请求名会让别名请求的冷却查不到、已限额的
/// 账号被反复选中。第三级「恢复最早的那个」同样按真名读 `resetAt`。
pub(super) async fn select_target_account(
    service: &UpstreamService,
    providers: &[&str],
    keys: &routing::CooldownKeys<'_>,
    tried_ids: &[String],
) -> Result<RouteTarget, GatewayError> {
    let accounts = accounts_in_providers(service, providers);
    // 这些家都没有账号记录 → 用第一家的默认登录态（环境变量旁路等）
    if accounts.is_empty() {
        return Ok(RouteTarget {
            provider: providers.first().copied().unwrap_or_default().to_string(),
            account_id: None,
            account: None,
            proxy: None,
            priority: None,
            proxy_notice: None,
        });
    }

    // 在途计数快照取一次（锁是纳秒级的内存操作，见 connections.rs）：
    // 第一级与第二级共用同一份，两级看到的「谁在忙」是同一时刻的事实。
    let counts = connection_counts(service);
    let mut excluded: Vec<String> = tried_ids.to_vec();
    let now = logging::now_ms();
    loop {
        let picked = routing::pick_account_by_priority(&accounts, keys, &counts, &excluded, now);
        let Some(picked) = picked else {
            break;
        };
        let Some(id) = routing::account_id(&picked).map(str::to_string) else {
            break;
        };
        if let Some(entry) = service.store.get_session_by_id(&id) {
            return Ok(with_proxy_notice(picked, entry.proxy, entry.proxy_error, id));
        }
        excluded.push(id);
    }

    let enabled: Vec<Value> = accounts
        .iter()
        .filter(|account| !matches!(account.get("enabled"), Some(Value::Bool(false))))
        .cloned()
        .collect();
    if enabled.is_empty() {
        return Err(GatewayError::with_status(
            503,
            "所有账号均已禁用，无账号可转发：请在账号页启用至少一个账号",
        ));
    }

    // ── 第二级兜底（并发感知）──────────────────────────────────
    // 启用中的账号都在限额冷却期内 / 已达并发上限时，仍挑一个试一次：
    //   ① 先在「启用 + 未尝试 + 未达并发上限」的账号里挑**恢复最早**的
    //      —— 上游若已实际解除限额可直接成功，否则把真实 429 与恢复时间
    //      返回给客户端（与改造前同一条路，只多了并发这一道闸）；
    //   ② 一个都没有（未尝试的账号**全部**达到并发上限）→ 挑**余量最大**
    //      的账号强塞。
    //
    // ── 为什么 ② 强塞而不是报 503（本级的取舍）──────────────────
    // 限流是「等得起」的：冷却有明确恢复时间，到点账号自然回来；并发挤占
    // 则是**即刻自愈**的 —— 在途请求一轮对话结束后计数立刻回落（见
    // connections.rs 的 Drop）。全部账号都忙时拒绝请求（503）只会把本可
    // 服务的请求直接挡在门外，而强塞只意味着短暂超载 1-2 个（软上限口径见
    // `routing::max_concurrent_of`）。所以：能等就等（①），等不了就挤（②），
    // 永不因并发上限直接 503。
    let mut best_effort: Option<Value> = None;
    let mut best_reset = f64::INFINITY;
    for account in &enabled {
        let Some(id) = routing::account_id(account) else {
            continue;
        };
        if tried_ids.iter().any(|tried| tried == id) {
            continue;
        }
        // 已达并发上限的账号不进「正常兜底」候选 —— 它们是 ② 的原料
        let limit = routing::max_concurrent_of(account);
        if limit > 0 && counts.get(id).copied().unwrap_or(0) >= limit as usize {
            continue;
        }
        let reset = routing::rate_limit_reset_at(account, keys, now);
        let reset = if reset > 0 { reset as f64 } else { f64::INFINITY };
        if reset < best_reset {
            best_reset = reset;
            best_effort = Some(account.clone());
        }
    }
    if best_effort.is_none() {
        // ① 无果：在「全部达到并发上限」（或没有未尝试账号）时按余量挤占。
        // 挑出的账号走与 ① 同一条收尾（取会话 → 组装选路结果）。
        if let Some(account) = squeeze_by_headroom(&enabled, &counts, tried_ids, keys, now) {
            if let Some(id) = routing::account_id(&account).map(str::to_string) {
                if let Some(entry) = service.store.get_session_by_id(&id) {
                    return Ok(with_proxy_notice(account, entry.proxy, entry.proxy_error, id));
                }
            }
        }
    }
    if let Some(account) = best_effort {
        if let Some(id) = routing::account_id(&account).map(str::to_string) {
            if let Some(entry) = service.store.get_session_by_id(&id) {
                return Ok(with_proxy_notice(account, entry.proxy, entry.proxy_error, id));
            }
        }
    }
    Ok(RouteTarget {
        provider: providers.first().copied().unwrap_or_default().to_string(),
        account_id: None,
        account: None,
        proxy: None,
        priority: None,
        proxy_notice: None,
    })
}

/// 账号记录上的 provider id（缺失按默认 provider）。
/// 实现与出处都在 `routing` —— 那里也要按 provider 收窄候选（`pick_for_model`），
/// 一处实现两处用，免得「缺失回落默认家」这条规则被抄成两份、日后改歪一份。
pub(super) use crate::server::core::routing::provider_of;

/// 限额冷却键的解析器（请求名 → 各家上游真名）。
///
/// 与 `provider_of` 同一模式：定义与论证都在 `routing`，这里只做转出 ——
/// 编排层（`provider_loop`）要建它、本模块的选路函数要收它，两处都写全路径
/// 只会让签名更长；而它是**冷却键口径**的单一事实来源，不该在别处再写一遍。
pub(super) use crate::server::core::routing::CooldownKeys;

/// 候选账号池：`providers` 里各家的全部账号（公开形态，文件顺序）。
///
/// 为什么在公开快照上过滤而不是用 `store.accounts_for_provider`：后者按
/// 「启用且有凭证」过滤掉了禁用账号，而本模块的第三级选路（全禁用 → 503）
/// 必须**看见**禁用账号才能给出准确文案。两者口径不同、各有用途。
pub(super) fn accounts_in_providers(service: &UpstreamService, providers: &[&str]) -> Vec<Value> {
    let snapshot = service.store.list_accounts();
    routing::accounts_of(&snapshot)
        .into_iter()
        .filter(|account| providers.contains(&provider_of(account)))
        .collect()
}

/// 组装选路结果；代理解析失败时把提示**带进选路结果**（本次回退直连）。
///
/// ── 提示为什么是返回值的一部分而不是当场打日志 ────────────────
/// 改造前这里直接 `logging::log` 一行运行日志。那是**逐请求**的事实：代理配错
/// 的账号上每条请求都会刷一行，而请求日志那边看不到「这次其实是直连的」。
/// 现在把文案挂在 `RouteTarget::proxy_notice` 上，由转发层记进本轮尝试明细
/// （见那个字段的说明），运行日志不再写。
///
/// 两个来源合并成一条文案：账号存储里记的 `proxyError`（上次解析失败的原因）
/// 与本次现场解析失败的原因。**后者优先** —— 它说明的是「这次为什么直连」，
/// 而 `proxyError` 可能是更早的旧状态。
pub(super) fn with_proxy_notice(
    account: Value,
    proxy: Value,
    proxy_error: Option<String>,
    account_id: String,
) -> RouteTarget {
    let mut notice = proxy_error.map(|error| {
        format!(
            "账号「{}」代理不可用，本次直连: {error}",
            account_display(&account)
        )
    });
    let resolved = match ResolvedProxy::from_json(&proxy) {
        Ok(proxy) => proxy,
        Err(reason) => {
            // 会话里的 proxy 由账号存储解析过（成功才会带过来），
            // 这里失败说明数据在两次读盘之间变了：按直连兜底
            notice = Some(format!("账号代理不可用（{reason}），本次回退直连"));
            None
        }
    };
    RouteTarget {
        provider: provider_of(&account).to_string(),
        account_id: Some(account_id),
        priority: priority_of(&account),
        account: Some(account),
        proxy: resolved,
        proxy_notice: notice,
    }
}

/// 记录限额并返回可读的恢复时间文本（对照 Node 的 markAccountLimited）。
///
/// `account_id` 已经唯一确定了 provider（id 在整份账号文件里唯一、且每条记录
/// 只属于一家），所以冷却键 = `provider×账号×模型` 天然成立。
///
/// `model` 必须是**上游实际收到的真名**（不是客户端请求名）—— 调用方从
/// `SendBody::wire_model` 取（见 `upstream::payload`）。传请求名会把冷却写在
/// 上游永远不会记额度的键上，判定侧也就查不到它（见 `routing::CooldownKeys`）。
pub(super) fn mark_account_limited(
    service: &UpstreamService,
    account_id: &str,
    model: &str,
    status: i32,
    upstream_code: Option<i64>,
    reset_at: Option<i64>,
    message: &str,
) -> String {
    // Node 从 `error.body` 里再解析一次 msg 取恢复时间；Rust 侧的错误文案
    // 已经带上上游原文，`reset_at` 由适配器的 classify_error 解析后传进来，
    // 这里再兜一次文本解析（两条路径结果一致：都来自同一份上游文案）
    let parsed = if let Some(reset_at) = reset_at.filter(|value| *value > 0) {
        reset_at
    } else {
        errors::parse_quota_reset_at(message)
    };
    let entry = service.store.mark_rate_limited(
        account_id,
        model,
        status as i64,
        upstream_code,
        if parsed > 0 { Some(parsed as f64) } else { None },
        message,
    );
    match entry {
        Some(entry) => {
            let reset = entry.get("resetAt").and_then(Value::as_f64).unwrap_or(0.0);
            format_reset_text(reset)
        }
        None => String::new(),
    }
}

/// 该账号对某模型当前的限额恢复时间戳（未限额 0）——上报事件时用，
/// 与 Node 的 `limitEntry?.resetAt` 同源（都从账号记录里现读）。
///
/// `model` 是上游真名（见 `mark_account_limited` 的说明），与写入侧同一个键。
pub(super) fn account_limit_reset_at(service: &UpstreamService, account_id: &str, model: &str) -> i64 {
    let snapshot = service.store.list_accounts();
    let accounts = routing::accounts_of(&snapshot);
    accounts
        .iter()
        .find(|account| routing::account_id(account) == Some(account_id))
        .map(|account| {
            account
                .get("rateLimits")
                .and_then(|limits| limits.get(model))
                .and_then(|limit| limit.get("resetAt"))
                .and_then(Value::as_f64)
                .map(|value| value as i64)
                .unwrap_or(0)
        })
        .unwrap_or(0)
}

/// 全部账号达到并发上限时的**挤占**挑选：余量（`limit - count`）最大者优先。
///
/// ── 排序口径（任务约定，改动前先读）──────────────────────────
///   · **不限（maxConcurrent == 0）= 无穷大余量**，永远排在有限上限之前 ——
///     用 `i64::MAX` 近似：任何真实的 `limit - count` 都比它小；
///   · 同余量挑**恢复最早**的（与 ① 的「恢复最早试一次」同一偏好：优先选
///     「最可能马上恢复额度」的账号多打一个）；
///   · `>` 严格比较保持候选顺序稳定（同余量同恢复时间时先见者优先）。
///
/// 挑中即打一行 verbose（verbose 级别不进运行日志页，只在终端/调试时可见
/// —— 这是逐请求的事件，info 级会刷屏，但排障时必须找得到「为什么明明
/// 设了上限还打到这个账号」的答案）。没有可挤的账号（全部已尝试过）时
/// 返回 None，由调用方落到尾部的「无账号」兜底。
fn squeeze_by_headroom(
    enabled: &[Value],
    counts: &HashMap<String, usize>,
    tried_ids: &[String],
    keys: &routing::CooldownKeys<'_>,
    now: i64,
) -> Option<Value> {
    let mut squeezed: Option<Value> = None;
    let mut best_headroom = i64::MIN;
    let mut best_reset = f64::INFINITY;
    for account in enabled {
        let Some(id) = routing::account_id(account) else {
            continue;
        };
        if tried_ids.iter().any(|tried| tried == id) {
            continue;
        }
        let count = counts.get(id).copied().unwrap_or(0) as i64;
        let limit = routing::max_concurrent_of(account) as i64;
        let headroom = if limit > 0 { limit - count } else { i64::MAX };
        let reset = routing::rate_limit_reset_at(account, keys, now);
        let reset = if reset > 0 { reset as f64 } else { f64::INFINITY };
        if headroom > best_headroom || (headroom == best_headroom && reset < best_reset) {
            best_headroom = headroom;
            best_reset = reset;
            squeezed = Some(account.clone());
        }
    }
    if let Some(account) = &squeezed {
        logging::verbose(
            "[Upstream]",
            &format!(
                "全部账号达到并发上限，按余量挤占账号「{}」",
                account_display(account)
            ),
        );
    }
    squeezed
}

/// 当前在途请求计数的快照（账号 id → 请求数），选路的并发过滤用。
///
/// 从 `Connections` 的表里一次读出（`snapshot` 锁内克隆，纳秒级）。
/// 拿不到运行时句柄的调用方传**空表** = 不做并发过滤（`pick_for_model`
/// 的调用点 `api::session` 有句柄，正常传真值；`describe_route_decision`
/// 属排障快照，传空表）。
pub(super) fn connection_counts(service: &UpstreamService) -> HashMap<String, usize> {
    service.connections().snapshot().into_iter().collect()
}

/// 下一个可用账号（全局队列，限定在 `providers` 各家的账号里，跳过已尝试的）。
///
/// `keys` 见 [`select_target_account`]：冷却按各家真名判定。
/// 走与第一级选路同一个 `pick_account_by_priority`，并发上限的过滤
/// （与软上限口径）由此自动获得 —— 换号顺延不会把请求塞回一个已达上限的账号。
pub(super) fn pick_next_account(
    service: &UpstreamService,
    providers: &[&str],
    keys: &routing::CooldownKeys<'_>,
    tried_ids: &[String],
) -> Option<Value> {
    let accounts = accounts_in_providers(service, providers);
    let counts = connection_counts(service);
    routing::pick_account_by_priority(&accounts, keys, &counts, tried_ids, logging::now_ms())
}

/// 该账号对该模型此前是否处于限额状态（用于「已恢复可用」日志的去噪）。
///
/// `model` 是上游真名（见 `mark_account_limited` 的说明）：读的键必须与写入
/// 侧同一个，否则「清掉一条不存在的记录」会被误判成「恢复可用」而多打一行日志
/// （或者反过来，真正的恢复不写日志）。
pub(super) fn account_had_limit(service: &UpstreamService, account_id: &str, model: &str) -> bool {
    let snapshot = service.store.list_accounts();
    routing::accounts_of(&snapshot)
        .iter()
        .find(|account| routing::account_id(account) == Some(account_id))
        .map(|account| {
            account
                .get("rateLimits")
                .and_then(|limits| limits.get(model))
                .is_some()
        })
        .unwrap_or(false)
}

/// 429 限额事件上报到运行日志（对照 Node 的 reportLimitEvent）。
///
/// 结构化 data 让前端能直接展示「账号 A → 账号 B」的切换链路与恢复时间
/// （logs-panel 读 `data.from` / `data.to` / `data.resetAtText`）。
/// `priority` 是**目标账号**的优先级（Node 在降级分支传 `next.priority`，
/// 其余分支是 `?? null`）。
///
/// `provider` 进 data（多提供商后同一条日志要能分辨是哪一家在切换）。
/// 前端只读自己认识的键，多一个键不影响既有展示。
#[allow(clippy::too_many_arguments)]
pub(super) fn report_limit_event(
    level: &str,
    message: &str,
    from: Option<&str>,
    to: Option<&str>,
    model: &str,
    upstream_code: Option<i64>,
    status_code: i32,
    reset_at: i64,
    priority: Option<i64>,
    provider: &str,
) {
    logging::log_event(
        level,
        "account",
        message,
        Some(json!({
            "model": model,
            "provider": provider,
            "from": from.unwrap_or(""),
            "to": to.unwrap_or(""),
            "priority": priority.map(Value::from).unwrap_or(Value::Null),
            "status": if status_code == 0 { 429 } else { status_code },
            "code": upstream_code.map(Value::from).unwrap_or(Value::Null),
            "resetAt": reset_at,
            "resetAtText": if reset_at > 0 { format_reset_text(reset_at as f64) } else { String::new() },
        })),
    );
}
