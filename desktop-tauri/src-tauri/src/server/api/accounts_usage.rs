//! `/api/accounts/usage` 的实现（从 `api::accounts` 拆出，单文件行数约定）。
//!
//! 余额 / 积分查询是**四家混查**：目标集合跨全部启用账号，逐账号按所属 provider
//! 分流到 `ProviderAdapter::query_usage`。本文件只负责并发调度与单账号失败的
//! 收敛 —— 「这个账号的余额怎么查」是 provider 知识，全在各家适配器里。
//!
//! 拆分口径：`api::accounts` 的 `resolve_batch_targets`（目标集合解析，签到也要
//! 用）留在原文件，**查询与结果组装**（本文件）跟着 `/api/accounts/usage`
//! 这条接口走 —— 改余额功能的形状时只需要翻这一处。

use serde_json::{json, Value};

use crate::server::core::providers::adapter::adapter_for;
use crate::server::http::ok_json;
use crate::server::logging;
use crate::server::ServerState;

use axum::response::Response;

/// 余额查询失败的原因：区分「没有凭证」（Node 的早退分支，少 name 键）与
/// 「请求失败」（带 name 键）。第二个字段是给前端的**机器可识别标记**（当前
/// 只有「未配置查询凭证」用，见 `adapter::USAGE_NOT_CONFIGURED_CODE`）：前端据此
/// 把这一条显示成中性提示而不是红色失败，比按文案匹配可靠。
enum UsageFailure {
    /// 账号没有可用凭证 —— Node 里那个 `return { id, usage:null, error }`
    NoCredentials,
    Request(String, Option<String>),
}

/// 单账号余额查询：任何失败都收敛为 `{error}` 而不是抛出（批量查询不被单账号
/// 拖垮），刷新重试在 `query_usage_inner` 里。
async fn query_usage_for(state: &ServerState, account: &Value) -> Value {
    let id = account.get("id").and_then(Value::as_str).unwrap_or("").to_string();
    let name = account.get("name").cloned().unwrap_or(Value::Null);
    // 凭证与 provider 都从目标集合里的账号对象读，**不回读账号文件**：
    // 20 个账号并发时那次回读会变成 20 次整库读盘，而答案已在手上。
    // `hasCredentials` 是 store 逐条给出的统一判据（缺省视为有）。
    let has_credentials = account.get("hasCredentials").and_then(Value::as_bool) != Some(false);
    let outcome = if has_credentials {
        let provider_id = account
            .get("provider")
            .and_then(Value::as_str)
            .unwrap_or(crate::server::core::providers::DEFAULT_PROVIDER_ID);
        query_usage_inner(state, provider_id, &id).await
    } else {
        Err(UsageFailure::NoCredentials)
    };
    match outcome {
        Ok(usage) => json!({ "id": id, "name": name, "usage": usage, "error": Value::Null }),
        // Node 的这条早退 `return` **不含 name 键**（只有 catch 分支才带）
        Err(UsageFailure::NoCredentials) => {
            json!({ "id": id, "usage": Value::Null, "error": "没有可用凭证" })
        }
        Err(UsageFailure::Request(message, code)) => {
            logging::verbose("[Accounts]", &format!("账号 {id} 余额查询失败: {message}"));
            json!({ "id": id, "name": name, "usage": Value::Null, "error": message, "code": code })
        }
    }
}

/// 查询一个账号的余额 / 积分（**按账号所属 provider 分流到适配器**）。
///
/// ── 401：刷新后重试一次，但**只有能刷新的家才有这一步** ────────
/// 「401 说明 token 被服务端拒绝而非临期」——Node 版据此才走刷新重试，其它错误
/// 直接返回；四家的刷新协议各不相同，统一走 `refresh_access_token`（force 语义）。
///
/// 先问 `supports_refresh()` 是 **CatPaw 跳过重试的依据**：它上游没有刷新接口
/// （`X-Passport-Token` 过期只能在桌面端重新登录，见 `catpaw/adapter.rs`），对它
/// 的 401 做刷新重试会稳定失败，把一条「凭证过期」变成两条错误（刷新失败的信息
/// 盖住真正原因，用户反而看不出该做什么）。
///
/// workbuddy 也走适配器：它的 `query_usage` 转调既有计费服务（结果形状不变），
/// 而那里的 `AuthService::for_store` 与 `state.auth()` 是同一个上下文（都用
/// `default_context()`），因此与 `state.billing()` 的结果逐字相同。
async fn query_usage_inner(
    state: &ServerState,
    provider_id: &str,
    id: &str,
) -> Result<Value, UsageFailure> {
    // 注册表里没有的 id（前端比后端新、或手改过的账号文件）：明确报「未知的
    // 提供商」，不猜成任何一家（与全仓的 provider 口径一致）
    let Some(kind) = crate::server::core::providers::kind_from_id(provider_id) else {
        return Err(UsageFailure::Request(
            format!("未知的提供商 {provider_id}，无法查询余额"),
            None,
        ));
    };
    let adapter = adapter_for(kind);
    match adapter.query_usage(state.store(), id).await {
        Ok(usage) => Ok(usage),
        Err(error) => {
            // 不支持刷新的家直接返回原错误（CatPaw：重试必然失败，见上）
            if error.status_code != 401 || !adapter.supports_refresh() {
                return Err(UsageFailure::Request(error.message, error.code));
            }
            if let Err(refresh_error) = adapter.refresh_access_token(state.store(), id).await {
                return Err(UsageFailure::Request(refresh_error.message, None));
            }
            adapter
                .query_usage(state.store(), id)
                .await
                .map_err(|retry| UsageFailure::Request(retry.message, retry.code))
        }
    }
}

/// 该账号所属 provider 是否声明了余额能力（`ProviderAdapter::supports_usage`）。
///
/// 为什么批量查询要过滤掉「不支持」的家：不支持的实现会给**每个账号**产出一行
/// 501 —— 用户看到一片红，而那不是故障、只是能力缺失（当前四家都支持，
/// 但注册表是可扩展的）。判据是各家的恒定能力声明，不是这次请求成不成功。
/// 未知 provider id 一并跳过；显式指定 id 的调用路径仍会走到那条明确报错。
fn supports_usage(account: &Value) -> bool {
    let provider_id = account
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or(crate::server::core::providers::DEFAULT_PROVIDER_ID);
    crate::server::core::providers::kind_from_id(provider_id)
        .map(|kind| adapter_for(kind).supports_usage())
        .unwrap_or(false)
}

/// GET /api/accounts/usage
///
/// 逐账号并发查询余额 / 积分汇总（`{ results: [{id,name,usage,error,code?}], skipped }`）。
///
/// 目标集合是**全部启用账号**（`resolve_batch_targets(provider = None)`），逐账号
/// 按 `provider` 分流到 `ProviderAdapter::query_usage`。改造前这条接口只查
/// workbuddy —— 三家账号在这里被静默跳过，前端连按钮都不给。
///
/// **并发**是关键：Node 版用 `Promise.all`，20 个账号串行会让前端转圈 20 次
/// 往返。这里用 `join_all` 在同一个任务里并发轮询（每个 future 都是网络等待，
/// 天然交错），且**结果顺序与 targets 一致** —— 与 `Promise.all` 语义相同。
/// **超时由各适配器自己设**（15~20 秒）；接口层不叠加第二层超时 —— 那会让
/// 「上游慢」与「网关掐断」在日志里无法区分。
pub async fn accounts_usage(state: &ServerState) -> Response {
    // `provider = None`：跨四家取目标（签到那条仍按 workbuddy 过滤）
    let (targets, skipped) = match super::accounts::resolve_batch_targets(state, None, None) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    let futures: Vec<_> = targets
        .iter()
        .filter(|account| supports_usage(account))
        .map(|account| query_usage_for(state, account))
        .collect();
    let results = futures::future::join_all(futures).await;
    ok_json(json!({ "results": results, "skipped": skipped }))
}
