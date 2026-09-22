//! 定时签到路由（对照 server.mjs 726-746 行）。
//!
//!   GET  /api/auto-checkin        读取开关、触发时刻、上次结果
//!   POST /api/auto-checkin        修改设置 { enabled?, time? }
//!   POST /api/auto-checkin/run    立即执行一次
//!
//! ── 错误信封（照抄 Node 的落点）────────────────────────────
//! Node 里这三条在 handler 的最外层大 try 里，抛出的 AutoCheckinConfigError
//! 经 errorPayload 变成 **OpenAI 风格** body：
//!   `{"error":{"message":"…","type":"invalid_request_error"}}`
//! 而不是管理 API 的 `{success:false,error}`。实测（2026-09-17，Node v1.0.1）：
//!   POST /api/auto-checkin {"time":"99:99"} → 400 时间超出范围（00:00 - 23:59）
//!   POST /api/auto-checkin {}               → 400 没有需要更新的字段
//! 所以这里用 `to_gateway_error().into_response()`，状态码取错误自带的值
//! （AutoCheckinConfigError 固定 400，见 core::auto_checkin 的注释）。
//!
//! ── run 的响应形状 ─────────────────────────────────────────
//! 成功：`{success:true, data:{...summary, state}}` —— summary 的字段
//! （at/date/reason/succeeded/total/skipped/failed/failedCount）与 state 合并
//! （state 在后，同名字段以 state 为准，照抄 Node 的对象展开顺序）。
//! 正在执行中（runNow 返回 null）：抛 AutoCheckinConfigError → 400
//! 「签到正在执行中，请稍候」。

use axum::body::Bytes;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use serde_json::Value;

use crate::server::core::auto_checkin::AutoCheckinConfigError;
use crate::server::http::{ok_json, parse_body};
use crate::server::logging;
use crate::server::ServerState;

/// 配置错误 → 响应（OpenAI 风格 body，状态码用错误自带的值）
fn config_error(error: AutoCheckinConfigError) -> Response {
    logging::log("[Checkin]", &format!("❌ {}", error.message));
    crate::server::errors::GatewayError::with_status(error.status_code, error.message)
        .into_response()
}

/// GET /api/auto-checkin —— 自动签到状态
pub async fn get_state(State(state): State<ServerState>) -> Response {
    ok_json(state.auto_checkin().state())
}

/// POST /api/auto-checkin —— 修改设置（{ enabled?, time? }）
pub async fn configure(State(state): State<ServerState>, body: Bytes) -> Response {
    // 空 body 视为 {}（Node 的 `|| '{}'`）；非法 JSON 在 Node 里由 JSON.parse
    // 抛出的 SyntaxError 走到 errorPayload → 500 proxy_error（实测如此），
    // 这里保持一致：不额外做 400 转换。
    let payload = match parse_body(&body) {
        Ok(value) => value,
        Err(error) => {
            return crate::server::errors::GatewayError::new(error.message).into_response()
        }
    };
    if !payload.is_object() {
        // Node: `configure(body)` 对非对象解构得到 undefined → 两个字段都不 patch
        // → 抛「没有需要更新的字段」（AutoCheckinConfigError → 400）
        return config_error(AutoCheckinConfigError {
            message: "没有需要更新的字段".to_string(),
            status_code: 400,
        });
    }
    match state.auto_checkin().configure(&payload) {
        Ok(data) => ok_json(data),
        Err(error) => config_error(error),
    }
}

/// POST /api/auto-checkin/run —— 立即执行一次
///
/// `run_now()` 返回 None 表示已有一次在执行（本次跳过）→ 400
/// 「签到正在执行中，请稍候」。注意签到失败也返回 None，但那种情况下
/// lastResult 里已经写了失败原因，Node 同样无法区分（两条路径都落到 400）。
pub async fn run_now(State(state): State<ServerState>) -> Response {
    let Some(summary) = state.auto_checkin().run_now().await else {
        return config_error(AutoCheckinConfigError {
            message: "签到正在执行中，请稍候".to_string(),
            status_code: crate::server::core::auto_checkin::CHECKIN_BUSY_STATUS,
        });
    };
    // `{...summary, state}`：summary 先、state 后
    let merged = match summary {
        Value::Object(map) => {
            let mut merged = map;
            if let Value::Object(state_map) = state.auto_checkin().state() {
                for (key, value) in state_map {
                    merged.insert(key, value);
                }
            }
            Value::Object(merged)
        }
        other => other,
    };
    ok_json(merged)
}
