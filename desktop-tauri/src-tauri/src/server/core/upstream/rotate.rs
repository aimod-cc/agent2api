//! 账号选路与 429 轮换（从 mod.rs 拆出，单文件行数约定）。
//!
//! 对应 Node 版 workbuddy-upstream-client.mjs 的这几段：
//!   selectTargetAccount   三级选路（优先级挑 → 全限额时恢复最早 → 全禁用报 503）
//!   withProxyNotice       代理解析失败时记日志并回退直连
//!   requireSession        无可用登录态时报 401
//!   requestWithWafRetry   11128（敏感词拦截）的 10s/25s 退避重试
//!   markAccountLimited    限额标记落盘 + 恢复时间文案
//!   reportLimitEvent      429 结构化事件上报（桌面端日志页的「账号 A → B」链路）
//!
//! ── 为什么是自由函数而不是 `impl UpstreamService` 的方法 ──────
//! 这些函数无一例外都要「读账号存储 + 读/写限额 + 打日志」，本质是对
//! 账号存储的操作，而不是转发器自身的行为；拆成 `fn(&UpstreamService, ...)`
//! 后 mod.rs 只保留「一次转发的编排」，两类关注点不再互相淹没。
//! 作为 `mod.rs` 的**子模块**声明（`mod rotate;`），它可以直接看到
//! `UpstreamService` 的私有字段（Rust 的私有项对后代模块可见），
//! 所以不需要为它们额外开访问器。

use std::time::Duration;

use serde_json::{json, Value};

use crate::server::core::proxies::ResolvedProxy;
use crate::server::core::routing;
use crate::server::errors::{self, GatewayError};
use crate::server::logging;

use super::request::{
    read_upstream_error, send_chat_request, ChatRequestPlan, RATE_LIMIT_CODE, WAF_RETRY_DELAYS_MS,
};
use super::{account_display, format_reset_text, has_access_token, priority_of, RouteTarget,
            UpstreamService};

/// 取指定账号的会话；`account_id` 为 None 时回落到默认登录态。
///
/// 没有可用登录态时报 401（文案对齐 Node 的 requireSession，去掉
/// 「node server.mjs --login」那半句 —— 壳内 Rust 版没有那个命令行入口）。
pub(super) async fn session_for(service: &UpstreamService, account_id: Option<&str>) -> Result<Value, GatewayError> {
    if let Some(id) = account_id {
        if let Some(entry) = service.store.get_session_by_id(id) {
            return Ok(entry.session);
        }
        // 指定账号在两次读盘之间被删掉：回落到默认登录态
        // （对应 Node 的 `?? await requireSession()`）
    }
    let session = service
        .auth
        .get_current_session()
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
///   1. 按优先级选（跳过禁用与限额冷却中的账号），逐个向前找——
///      跳过没有可用凭证的记录（避免选到空账号）；
///   2. 启用中的账号都在该模型限额期内 → 挑恢复最早的一个试一次，
///      把上游真实的 429（含恢复时间）透传给客户端；
///   3. 全部禁用 → 明确报 503，绝不回退到已禁用账号。
pub(super) async fn select_target_account(
    service: &UpstreamService,
    model: &str,
    tried_ids: &[String],
) -> Result<RouteTarget, GatewayError> {
    let snapshot = service.store.list_accounts();
    let accounts = routing::accounts_of(&snapshot);
    // 没有账号记录（也不该有环境变量以外的凭证来源）→ 用默认登录态
    if accounts.is_empty() {
        return Ok(RouteTarget {
            account_id: None,
            account: None,
            proxy: None,
            priority: None,
        });
    }

    let mut excluded: Vec<String> = tried_ids.to_vec();
    let now = logging::now_ms();
    loop {
        let picked =
            routing::pick_account_by_priority(&accounts, model, &excluded, now);
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

    // 启用中的账号都在限额冷却期内：仍用恢复最早的那个试一次，
    // 上游若已实际解除限额可直接成功，否则把真实 429 与恢复时间返回给客户端
    let mut best_effort: Option<Value> = None;
    let mut best_reset = f64::INFINITY;
    for account in &enabled {
        let Some(id) = routing::account_id(account) else {
            continue;
        };
        if tried_ids.iter().any(|tried| tried == id) {
            continue;
        }
        let reset = routing::rate_limit_reset_at(account, model, now);
        let reset = if reset > 0 { reset as f64 } else { f64::INFINITY };
        if reset < best_reset {
            best_reset = reset;
            best_effort = Some(account.clone());
        }
    }
    if let Some(account) = best_effort {
        if let Some(id) = routing::account_id(&account).map(str::to_string) {
            if let Some(entry) = service.store.get_session_by_id(&id) {
                return Ok(with_proxy_notice(account, entry.proxy, entry.proxy_error, id));
            }
        }
    }
    Ok(RouteTarget { account_id: None, account: None, proxy: None, priority: None })
}

/// 组装选路结果；代理解析失败时记一条日志（本次回退直连）
pub(super) fn with_proxy_notice(
    account: Value,
    proxy: Value,
    proxy_error: Option<String>,
    account_id: String,
) -> RouteTarget {
    if let Some(proxy_error) = proxy_error.as_deref() {
        logging::log(
            "[Upstream]",
            &format!(
                "⚠️ 账号「{}」代理不可用，本次直连: {proxy_error}",
                account_display(&account)
            ),
        );
    }
    let resolved = match ResolvedProxy::from_json(&proxy) {
        Ok(proxy) => proxy,
        Err(reason) => {
            // 会话里的 proxy 由账号存储解析过（成功才会带过来），
            // 这里失败说明数据在两次读盘之间变了：按直连兜底并记一条
            logging::log(
                "[Upstream]",
                &format!("⚠️ 账号代理不可用（{reason}），本次回退直连"),
            );
            None
        }
    };
    RouteTarget {
        account_id: Some(account_id),
        priority: priority_of(&account),
        account: Some(account),
        proxy: resolved,
    }
}

/// 带 11128 退避的上游请求（对照 Node 的 requestWithWafRetry）。
///
/// 命中 11128（敏感词拦截）时按 10s/25s 退避重试（最多 2 次），
/// 其余错误原样抛出为统一的 GatewayError（带 upstream_code）。
pub(super) async fn request_with_waf_retry(
    plan: &ChatRequestPlan,
) -> Result<reqwest::Response, GatewayError> {
    let mut attempt = 0usize;
    loop {
        let response = send_chat_request(plan).await.map_err(|error| error.to_gateway_error())?;
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status().as_u16();
        let detail = read_upstream_error(response).await;
        if detail.code == Some(RATE_LIMIT_CODE) && attempt < WAF_RETRY_DELAYS_MS.len() {
            let delay = WAF_RETRY_DELAYS_MS[attempt];
            logging::log(
                "[Upstream]",
                &format!(
                    "⚠️ 上游敏感词拦截（11128），请检查提示词中的敏感词（可在「脱敏」页维护词表）；\
                     {}s 后重试（第 {}/{} 次）",
                    delay / 1000,
                    attempt + 1,
                    WAF_RETRY_DELAYS_MS.len(),
                ),
            );
            tokio::time::sleep(Duration::from_millis(delay)).await;
            attempt += 1;
            continue;
        }
        logging::log(
            "[Upstream]",
            &format!("上游错误 HTTP {status}: {}", detail.message),
        );
        // 11128 透传给客户端时追加敏感词指引，避免被误当成普通频率限制
        let hint = if detail.code == Some(RATE_LIMIT_CODE) {
            "；提示词可能命中上游敏感词，请检查提示词"
        } else {
            ""
        };
        let mut error = GatewayError::with_status(
            status as i32,
            format!("上游返回 {status}: {}{hint}", detail.message),
        );
        if let Some(code) = detail.code {
            error = error.upstream_code(code);
        }
        return Err(error);
    }
}

/// 记录限额并返回可读的恢复时间文本（对照 Node 的 markAccountLimited）
pub(super) fn mark_account_limited(service: &UpstreamService, account_id: &str, model: &str, error: &GatewayError) -> String {
    // Node 从 `error.body` 里再解析一次 msg 取恢复时间；Rust 侧的错误已经
    // 带上了上游文案（GatewayError.message 就是 detail.message），
    // 所以直接解析本字段即可 —— 结果与 Node 的 `detail.msg || error.message` 一致
    let reset_at = errors::parse_quota_reset_at(&error.message);
    let entry = service.store.mark_rate_limited(
        account_id,
        model,
        error.status_code as i64,
        error.upstream_code,
        if reset_at > 0 { Some(reset_at as f64) } else { None },
        &error.message,
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
/// 与 Node 的 `limitEntry?.resetAt` 同源（都从账号记录里现读）
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

/// 下一个可用账号（按优先级，跳过已尝试的）
pub(super) fn pick_next_account(service: &UpstreamService, model: &str, tried_ids: &[String]) -> Option<Value> {
    let snapshot = service.store.list_accounts();
    let accounts = routing::accounts_of(&snapshot);
    routing::pick_account_by_priority(&accounts, model, tried_ids, logging::now_ms())
}

/// 该账号对该模型此前是否处于限额状态（用于「已恢复可用」日志的去噪）
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
) {
    logging::log_event(
        level,
        "account",
        message,
        Some(json!({
            "model": model,
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