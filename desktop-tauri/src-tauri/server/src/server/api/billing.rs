//! 积分 / 签到 / 运营活动路由（对照 server.mjs 871-911 行逐条实现）。
//!
//!   GET  /api/usage                     积分简报（queryCreditsSummary）
//!   GET  /api/checkin/status            签到活动状态
//!   POST /api/checkin                   领取每日签到（result.success ? 200 : 409）
//!   POST /api/checkin/claim-and-report  签到 + 回报最新积分
//!   GET  /api/activity/banner           运营 banner
//!   GET  /api/activity/ambassador       大使状态
//!
//! ── 响应形态的两个坑（务必别统一）──────────────────────────
//!   ① 成功响应都是管理 API 信封 `{success:true, data}`；
//!   ② `POST /api/checkin` 的**状态码随结果变**：领取成功 200、已领取 409，
//!      且 body 的 `success` 跟着结果走（不是恒 true）。前端账号页据此把
//!      「今天已签到」显示成一条 warn 而不是错误。
//!   ③ 未登录/上游失败时，Node 版这几条都在最外层大 try 里 → 走 errorPayload
//!      的 OpenAI 风格 body（`{error:{message,type,upstream_code?}}`），
//!      **不是**管理 API 的 `{success:false,error}` 信封。所以这里用
//!      `BillingError::to_gateway_error().into_response()`。

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::server::core::billing::BillingError;
use crate::server::http::ok_json;
use crate::server::logging;
use crate::server::ServerState;

/// 计费错误 → 响应。
///
/// 走 OpenAI 风格 body（理由见模块头部第 ③ 条），状态码保留 BillingError 里的值
/// （504 超时 / 401 登录态过期 / 上游 HTTP 码 / 400 国际版无签到活动）。
fn billing_error(error: BillingError) -> Response {
    logging::log("[Billing]", &format!("❌ {}", error.message));
    error.to_gateway_error().into_response()
}

/// GET /api/usage —— 积分简报
///
/// Node 版是 `billing.queryCreditsSummary({ locale: opts.locale })`，
/// locale 来自 config.json 的 locale（默认 zh-CN）。
pub async fn get_usage(State(state): State<ServerState>) -> Response {
    match state.billing().query_credits_summary_default().await {
        Ok(data) => ok_json(data),
        Err(error) => billing_error(error),
    }
}

/// GET /api/checkin/status —— 签到活动状态（无活动/无数据时为 null）
pub async fn checkin_status(State(state): State<ServerState>) -> Response {
    match state.billing().get_checkin_status(None).await {
        Ok(data) => ok_json(data),
        Err(error) => billing_error(error),
    }
}

/// POST /api/checkin —— 领取每日签到积分
///
/// **状态码语义**（server.mjs 885-889 行）：
///   `sendJson(res, result.success ? 200 : 409, { success: result.success, data: result })`
/// 即「已领取」这次调用会返回 409 + `success:false`，body 里带着上游的
/// msg（如「今日已签到」）—— 前端按 warn 展示，不是错误。
pub async fn claim_checkin(State(state): State<ServerState>) -> Response {
    let result = match state.billing().claim_daily_checkin(None).await {
        Ok(result) => result,
        Err(error) => return billing_error(error),
    };
    let success = result
        .get("success")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if !success {
        let detail = result.get("msg").and_then(serde_json::Value::as_str).unwrap_or("");
        logging::log("[Billing]", &format!("签到未领取（{detail}）"));
    }
    // 状态码与 body 的 success 都由结果决定，不能用 ok_json（它恒为 200/true）
    let status = if success {
        axum::http::StatusCode::OK
    } else {
        axum::http::StatusCode::CONFLICT
    };
    (status, axum::Json(json!({ "success": success, "data": result }))).into_response()
}

/// POST /api/checkin/claim-and-report —— 签到 + 查余额
pub async fn claim_and_report(State(state): State<ServerState>) -> Response {
    match state.billing().checkin_and_report_default().await {
        Ok(data) => ok_json(data),
        Err(error) => billing_error(error),
    }
}

/// GET /api/activity/banner —— 运营 banner（拉不到时为 null）
pub async fn activity_banner(State(state): State<ServerState>) -> Response {
    let data = state.billing().get_activity_banner(None).await;
    ok_json(data)
}

/// GET /api/activity/ambassador —— 大使状态（拉不到时为 null）
pub async fn activity_ambassador(State(state): State<ServerState>) -> Response {
    let data = state.billing().get_ambassador_status(None).await;
    ok_json(data)
}
