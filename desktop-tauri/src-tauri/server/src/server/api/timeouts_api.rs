//! GET/PUT /api/timeouts —— 上游请求的四项超时（连接 / 等待响应 / 流式空闲 / 非流式响应）。
//!
//! ── 四项对应转发链路的四个阶段（与 OmniProxy 的同名设置一一对应）──
//!   - `connectTimeoutSeconds`：建立上游 TCP/TLS 连接或代理隧道（含到代理那段）
//!     —— 落在出网客户端的 `connect_timeout`（见 `core::egress`）。
//!   - `headersTimeoutSeconds`：请求发出 → 响应头到达
//!     —— 落在 `upstream::request` 发送处的等待上限。
//!   - `streamIdleTimeoutSeconds`：流式响应相邻数据之间的空闲
//!     —— 落在 `upstream::ForwardStream` 的逐分片计时（收到新数据即重置）。
//!   - `bodyTimeoutSeconds`：非流式响应体读完的总预算
//!     —— 落在 `upstream::aggregate` 的聚合循环（一次性计时，不重置）。
//!
//! 四项统一存在配置库（键名契约见 `config.rs`），由设置页
//! 「通用 → 请求超时」经 HTTP 桥读写 —— 与 /api/retry 同一模式：
//! 独立端点、允许部分字段、返回生效后的全量值、无副作用（不重启进程）。
//! 保存后对下一个请求立即生效（转发层逐请求读内存快照，不读盘）。

use axum::body::Bytes;
use axum::extract::State;
use axum::response::Response;
use serde_json::{json, Value};

use crate::server::config::{
    self, TimeoutPatch, TimeoutSettings, KEY_TIMEOUT_BODY_SECONDS, KEY_TIMEOUT_CONNECT_SECONDS,
    KEY_TIMEOUT_HEADERS_SECONDS, KEY_TIMEOUT_STREAM_IDLE_SECONDS, TIMEOUT_MAX_SECONDS,
    TIMEOUT_MIN_SECONDS,
};
use crate::server::errors;
use crate::server::http::{ok_json, parse_body};
use crate::server::logging;
use crate::server::ServerState;

/// GET /api/timeouts
pub async fn get_timeouts(State(_state): State<ServerState>) -> Response {
    ok_json(timeouts_json(config::timeout_settings()))
}

/// PUT /api/timeouts —— body `{connectTimeoutSeconds?, headersTimeoutSeconds?,
/// streamIdleTimeoutSeconds?, bodyTimeoutSeconds?}`
///
/// 允许部分字段（未出现的项保持原值，null 同义）。校验通过后：写配置库
/// → 返回**生效后**的值（前端直接用响应刷新界面，不必再 GET 一次）。
pub async fn put_timeouts(State(_state): State<ServerState>, body: Bytes) -> Response {
    let payload = match parse_body(&body) {
        Ok(value) => value,
        Err(error) => return errors::management_error(400, error.message),
    };
    let Some(object) = payload.as_object() else {
        return errors::management_error(400, "请求体必须是 JSON 对象");
    };

    // 校验与写盘分两步（与 put_retry 同一顺序）：四项里有一项非法时整体
    // 不落盘，避免出现「连接超时改了、流空闲没改」的半套设置
    let mut patch = TimeoutPatch::default();
    let mut targets = [
        (
            KEY_TIMEOUT_CONNECT_SECONDS,
            object.get(KEY_TIMEOUT_CONNECT_SECONDS),
            &mut patch.connect_seconds,
        ),
        (
            KEY_TIMEOUT_HEADERS_SECONDS,
            object.get(KEY_TIMEOUT_HEADERS_SECONDS),
            &mut patch.headers_seconds,
        ),
        (
            KEY_TIMEOUT_STREAM_IDLE_SECONDS,
            object.get(KEY_TIMEOUT_STREAM_IDLE_SECONDS),
            &mut patch.stream_idle_seconds,
        ),
        (
            KEY_TIMEOUT_BODY_SECONDS,
            object.get(KEY_TIMEOUT_BODY_SECONDS),
            &mut patch.body_seconds,
        ),
    ];
    let mut updated = false;
    for (key, value, slot) in targets.iter_mut() {
        let Some(value) = value else {
            continue;
        };
        // null 视为「这一项不改」：前端整份回传时未填项常见为 null
        if value.is_null() {
            continue;
        }
        match parse_seconds(key, value) {
            Ok(number) => {
                **slot = Some(number);
                updated = true;
            }
            Err(message) => return errors::management_error(400, message),
        }
    }

    if updated && !config::set_timeouts(patch) {
        // 写盘失败：内存快照已更新（本次运行仍生效），但重启后会回到旧值 ——
        // 必须让用户知道，否则「改了设置重启又变回去」会被当成玄学问题
        logging::log("[Config]", "⚠️  请求超时设置写入失败，本次运行内仍生效");
    }

    let settings = config::timeout_settings();
    if updated {
        logging::log(
            "[Config]",
            &format!(
                "请求超时已更新: 连接 {} 秒 / 等待响应 {} 秒 / 流空闲 {} 秒 / 非流式 {} 秒",
                settings.connect_seconds,
                settings.headers_seconds,
                settings.stream_idle_seconds,
                settings.body_seconds,
            ),
        );
    }
    ok_json(timeouts_json(settings))
}

/// 四项超时的响应体（GET 与 PUT 共用，键名与 config.rs 的常量必然一致 ——
/// 与 `retry_api::retry_json` 同一手法：键用常量标识符而不是手写字符串）。
/// 键名对前端是**契约**（`settings-panel.js` 的 TIMEOUT_FIELDS 逐字对齐）。
fn timeouts_json(settings: TimeoutSettings) -> Value {
    json!({
        KEY_TIMEOUT_CONNECT_SECONDS: settings.connect_seconds,
        KEY_TIMEOUT_HEADERS_SECONDS: settings.headers_seconds,
        KEY_TIMEOUT_STREAM_IDLE_SECONDS: settings.stream_idle_seconds,
        KEY_TIMEOUT_BODY_SECONDS: settings.body_seconds,
    })
}

/// 单个秒数的校验：必须是 1–3600 的整数，否则给出可读的 400 文案。
/// 与 `retry_api::parse_bounded_int` 同一口径（只认 JSON 数字，
/// 容忍 `30.0` 这种整值浮点；字符串 `"30"` 视为非法）。
fn parse_seconds(key: &str, value: &Value) -> Result<i64, String> {
    let number = match value {
        Value::Number(number) => number.as_i64().or_else(|| {
            number
                .as_f64()
                .filter(|raw| raw.is_finite() && raw.fract() == 0.0)
                .map(|raw| raw as i64)
        }),
        _ => None,
    };
    match number {
        Some(number) if (TIMEOUT_MIN_SECONDS..=TIMEOUT_MAX_SECONDS).contains(&number) => Ok(number),
        _ => Err(format!(
            "{key} 必须是 {TIMEOUT_MIN_SECONDS}-{TIMEOUT_MAX_SECONDS} 的整数（收到: {value}）"
        )),
    }
}
