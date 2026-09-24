//! POST /api/models/refresh —— 手动刷新模型清单（网关页「刷新模型清单」按钮）。
//!
//! ── 为什么单独开一个模块而不是塞进 api/chat.rs ────────────────
//! `chat.rs` 已经领了 `/v1/models`（**对外**的 OpenAI 只读探针）与
//! `/v1/chat/completions`。这条是**管理 API**：挂 protected 组、要 API Key、
//! 会真打上游、返回管理信封 `{success, data}` —— 与 chat.rs 的对外形状、鉴权
//! 分组、错误口径全都不同。混在一起会让「这条到底算不算对外契约」变得要翻代码
//! 才知道；独立文件 + 独立登记（与 `auto_checkin.rs` / `billing.rs` 同一分工）
//! 让分组区别在路由表上直接可见。
//!
//! ── 为什么永远返回 2xx（与 credential_maintenance 同一口径）────────
//! 「跑完了，但某一家刷失败」是**常态**：网络抖动、某个账号的 token 过期、
//! 上游临时 5xx。这些信息已经逐条写在 `results` 里（status = failed + message），
//! 调用方（前端）能精确到「哪家、为什么」。若整条 HTTP 也返回非 2xx，调用方就得
//! 先解析错误响应、再从错误体里捞出那批结果 —— 而且「部分成功」会被压成一个
//! 「失败」，用户看到的就是一句没信息量的报错，看不到另外两家其实刷到了。
//! 因此状态码只表达「这次刷新跑完了没有」（跑完 = 2xx）——
//! 与 `/api/accounts/refresh-expiring` 完全一致；那条也是「逐条结果表达个体失败」。
//!
//! 真正该非 2xx 的是「请求本身没法处理」：本接口的 body 是可选的
//! （`{accounts: {providerId: accountId}, providers: [providerId, ...]}`，
//! 见下），空体与非法 JSON 都落到「全部家 + 各家默认选取」—— 历史调用方
//! （定时任务、bridge 的旧签名）不传参数，不能因为加了这些可选项就把它们变成 400。

use axum::body::Bytes;
use axum::extract::State;
use axum::response::Response;
use serde_json::{json, Value};

use crate::server::core::providers::adapter::refresh_implemented_forced;
use crate::server::core::providers::catalog;
use crate::server::http::ok_json;
use crate::server::logging;
use crate::server::ServerState;

/// 逐家结果的**汇总**：`{results, refreshed, skipped, failed}`。
///
/// 形状与 `credential_maintenance::refresh_expiring_report` 一致（汇总 + 逐条），
/// 界面既能一句话给结论，也能展开看每家为什么。三个数是 `(成功, 跳过, 失败)`，
/// 与日志行共用同一份统计（避免「日志说成功 2、响应说成功 3」这种对不上）。
/// 状态字符串是**前后端契约**，取值与语义见 `refresh_implemented_forced`。
fn summary_of(results: &[Value]) -> (Value, (usize, usize, usize)) {
    let count = |status: &str| {
        results
            .iter()
            .filter(|item| item.get("status").and_then(Value::as_str) == Some(status))
            .count()
    };
    let (refreshed, skipped, failed) = (count("refreshed"), count("skipped"), count("failed"));
    (
        json!({
            "results": results,
            "refreshed": refreshed,
            "skipped": skipped,
            "failed": failed,
        }),
        (refreshed, skipped, failed),
    )
}

/// 解析可选的 `accounts` 点名表（`{providerId: accountId}`，见下）。
///
/// 空体 / 非 JSON / 缺 `accounts` 键 / 值不是对象 —— 一律返回空表（= 各家按
/// 默认选取）。解析失败**不报 400**：这个字段是新增的可选项，老调用方不带它
/// 是正常用法（见模块头）。
fn parse_accounts(body: &Bytes) -> serde_json::Map<String, Value> {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| value.get("accounts").cloned())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default()
}

/// 解析可选的 `providers` 范围白名单（`["workbuddy", ...]`，见下）。
///
/// 键**缺失**（空体 / 非 JSON / 老调用方）→ `None` = 全部已实现的家；
/// 键存在 → 按数组收集非空字符串（trim），**形状不对或空数组 = 空名单**
/// —— 空名单是有效的「什么都不刷」：「界面上看不到的家就不要刷」这条约束
/// 宁可少刷也不放开（放开正是这次要修的行为）。
fn parse_providers(body: &Bytes) -> Option<Vec<String>> {
    let value = serde_json::from_slice::<Value>(body).ok()?;
    let items = value.get("providers")?;
    Some(
        items
            .as_array()
            .map(|array| {
                array
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|id| !id.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
    )
}

/// POST /api/models/refresh
///
/// 逐家刷新「支持远程目录」的模型清单（强制绕过缓存），并把**刷新后的聚合清单**
/// 一起返回。
///
/// ── 为什么响应里带清单，而不是让前端再拉一次 /api/session ──────
/// 用户按下这个按钮的**唯一目的**就是「马上看到新清单」。拆成两次往返的话：
///   1. 中间那一刻的界面状态是「刷新成功 + 还是旧清单」，看起来像没生效；
///   2. 聚合清单的读取要再走一次 `store` 与各家 `list_models`（后者对 workbuddy
///      是加锁读 + 克隆整份清单），本来在本次请求里就已经做过了；
///   3. 多一次往返就多一次可能失败的机会，而这次失败会把「刷新成功」变成
///      「刷新了但界面没更新」——最难排查的一种。
/// 因此这里直接返回 `models`（形状 = `/api/session` 的 `models` 字段，
/// 同一个 `catalog::session_models`），前端就地重绘，零额外往返。
/// 形状复用同一个函数而不是另拼一份：加了新字段时两条出口自动一致。
/// ── 可选的 `accounts` 点名表（「模型来源」下拉）──────────────────
/// body（JSON 对象，可缺省）：`{"accounts": {"<providerId>": "<accountId>"}}` ——
/// 每家「用哪个账号去打目录接口」。缺失的 provider / 空串 = 该家按默认选取
/// （队首可用账号，判据同 `AccountStore::current_entry_for_provider`）；
/// 点名的家取不到那条账号时，逐条结果里如实给失败原因（不回落到队首）。
/// 逐条结果里的 `accountId` 是本轮**实际**用的账号，界面据此回读那一列。
///
/// ── 可选的 `providers` 范围白名单（「获取模型」弹窗的实际范围）────
/// `{"providers": ["<providerId>", ...]}` —— 只刷名单内的家，名单外的
/// **不打网络、也不进结果**。由前端按「模型管理页左栏实有清单的家 ∪ 有启用
/// 账号的家」组装（见 models-fetch-modal.js 的 scopeProviders）：用户在界面上
/// 看不到的家（没有启用账号、清单也为空）刷了只会得到「缺少登录态」的噪音行。
/// 键缺失 = 全部已实现的家 —— 定时任务与旧调用方走这条（判定细节见
/// `parse_providers`）。
///
/// 为什么体是可选的：这条接口的既有调用方（定时任务、bridge 的旧签名）不带
/// 参数，而「换个账号」「收窄范围」只是新增的维度，不能把老调用方变成 400。
pub async fn refresh_models(State(state): State<ServerState>, body: Bytes) -> Response {
    let store = state.store();
    let accounts = parse_accounts(&body);
    let providers = parse_providers(&body);
    let results = refresh_implemented_forced(store, &accounts, providers.as_deref()).await;
    // 汇总算一次：响应里那份与日志里的三个数同源（见 `summary_of`）
    let (mut payload, (refreshed, skipped, failed)) = summary_of(&results);
    // 一行汇总进日志库：用户在日志页能看到「点了刷新、结果如何」
    // （逐家的细节在各适配器自己的日志里，这里只记总数）。
    // 级别跟结果走：全成功是正常操作，别让「失败 0 个」的文案把它抬成 error
    logging::log_with_level(
        "[Models]",
        &format!("手动刷新模型清单：成功 {refreshed}，跳过 {skipped}，失败 {failed}"),
        if failed > 0 { "error" } else { "info" },
    );
    // 清单在**刷新之后**取：此时各家句柄里已是新内容（见上面「为什么带清单」）
    let models = catalog::session_models(store);
    if let Some(object) = payload.as_object_mut() {
        object.insert("models".to_string(), Value::Array(models));
    }
    ok_json(payload)
}
