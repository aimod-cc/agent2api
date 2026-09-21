//! 调试模式：`GET/PUT /api/debug`（开关）与 `GET /api/debug/traffic`（详情）。
//!
//! ── 这个开关做什么 ──────────────────────────────────────────
//! 开启后转发层把**发给上游的请求（头脱敏 + 体）与上游返回的响应（状态码 +
//! 头脱敏 + 体）**完整落到统一库的 `debug_traffic` 表，请求日志页的
//! 「详情」列据此展示。采集与存储见 `core::debug_traffic`。
//!
//! ── 为什么详情单独一个端点 ────────────────────────────────────
//! 原始报文可达数百 KB（一个完整 SSE 响应），塞进请求日志列表会让每一页的
//! 响应体积爆掉。列表接口只给 `id`，详情按需拉 —— 与 OmniProxy「列表不返回
//! raw、详情接口才返回」同一取舍。
//!
//! ── 存储位置 ────────────────────────────────────────────────
//! 报文与其余数据同居 `{config_dir}/agent2api.db`（库位置见 `/api/storage`
//! 的单库概况），这里只管开关与读取。

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::response::Response;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::server::config;
use crate::server::core::debug_traffic;
use crate::server::errors;
use crate::server::http::{ok_json, parse_body};
use crate::server::logging;
use crate::server::ServerState;

/// GET /api/debug —— 当前开关状态与存储概况
pub async fn get_debug(State(_state): State<ServerState>) -> Response {
    ok_json(debug_json())
}

/// PUT /api/debug —— body `{debugMode: true|false}`
///
/// 写 config.json（键 `debugMode`），下一个请求立即生效（转发层逐请求读快照，
/// 不重启进程）。开启时顺带在事件日志里留一条记录 —— 它会让凭据类头被脱敏后
/// 落盘，用户该在日志里看得到这件事发生过。
pub async fn put_debug(State(_state): State<ServerState>, body: Bytes) -> Response {
    let payload = match parse_body(&body) {
        Ok(value) => value,
        Err(error) => return errors::management_error(400, error.message),
    };
    let Some(object) = payload.as_object() else {
        return errors::management_error(400, "请求体必须是 JSON 对象");
    };
    let Some(value) = object.get(config::KEY_DEBUG_MODE) else {
        return errors::management_error(400, format!("缺少 {}", config::KEY_DEBUG_MODE));
    };
    let Some(enabled) = value.as_bool() else {
        return errors::management_error(
            400,
            format!("{} 必须是 true 或 false", config::KEY_DEBUG_MODE),
        );
    };
    if !config::set_debug_mode(enabled) {
        logging::log("[Config]", "⚠️  调试模式设置写入 config.json 失败，本次运行内仍生效");
    }
    logging::log(
        "[Config]",
        if enabled {
            "调试模式已开启：转发时会把上游原始报文（凭据类头已脱敏）保存到本地"
        } else {
            "调试模式已关闭"
        },
    );
    ok_json(debug_json())
}

/// GET /api/debug/traffic?id=... —— 一条请求的原始报文
///
/// 找不到（开关没开过 / 已超出保留条数 / id 为空）时给 404：前端据此显示
/// 「该请求没有保存原始报文」而不是空面板。
pub async fn get_traffic(State(_state): State<ServerState>, Query(params): Query<Params>) -> Response {
    let id = params.id.trim();
    if id.is_empty() {
        return errors::management_error(400, "缺少 id");
    }
    match debug_traffic::get(id) {
        Some(entry) => ok_json(debug_traffic::detail_payload(&entry)),
        None => errors::management_error(
            404,
            "该请求没有保存原始报文（调试模式当时未开启，或记录已超出保留条数）",
        ),
    }
}

#[derive(Deserialize)]
pub struct Params {
    #[serde(default)]
    id: String,
}

/// 开关状态 + 存储概况（GET 与 PUT 共用，前端直接用响应刷新界面）
fn debug_json() -> Value {
    json!({
        config::KEY_DEBUG_MODE: config::current().debug_mode(),
        "count": debug_traffic::count(),
        "limit": debug_traffic::MAX_ENTRIES,
    })
}
