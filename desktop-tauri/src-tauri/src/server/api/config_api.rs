//! GET/POST/PUT /api/config —— 网关配置（API Key、语言）。
//!
//! 逐条对齐 Node 版 server.mjs 913-954 行：
//!
//! GET 返回：
//!   `{apiKey: 掩码或null, apiKeySet, locale, defaultModel,
//!     desensitize:{enabled,termCount,roles}}`
//!   掩码格式沿用 Node 版第 920 行：`前6字符...后4字符`。
//!
//! POST / PUT 接受 `{apiKey?, locale?}`（两者行为完全一致，PUT 是别名，
//! 见 `http::router` 的登记处）；
//!   - apiKey 非字符串、或长度 < 8 → 400 `{"success":false,"error":"API Key 至少需要 8 个字符"}`
//!   - apiKey 为 null → 删除 key
//!   - locale 非空字符串 → 更新语言
//!   - 写盘后返回 `{success:true,data:{apiKeySet,locale,defaultModel}}`
//!
//! 曾经的 `providerRoute`（provider 路由优先级）已随「账号全局一条队列」下线：
//! 先用哪一家由账号优先级决定，配置里不再有这一项（文件里残留的键原样保留、忽略）。
//!
//! 「运行中立即生效」：Node 版改的是内存里的 opts 对象；这里改的是
//! `server::config` 的 RwLock 快照，鉴权中间件每请求读它 —— 不用重启。
//!
//! `desensitize` 子对象是**精简摘要**（`{enabled, termCount, roles}`，对照
//! server.mjs 924 行）—— 完整词表与命中统计在 GET /api/desensitize。

use axum::body::Bytes;
use axum::extract::State;
use axum::response::Response;
use serde_json::{json, Value};

use crate::server::config;
use crate::server::errors;
use crate::server::http::{ok_json, parse_body};
use crate::server::logging;
use crate::server::ServerState;

/// API Key 最小长度（Node 版硬编码 8）
const MIN_API_KEY_LENGTH: usize = 8;
/// 长度不足时的文案，逐字照抄 Node 版
const API_KEY_TOO_SHORT: &str = "API Key 至少需要 8 个字符";

/// GET /api/config
pub async fn get_config(State(state): State<ServerState>) -> Response {
    let snapshot = config::current();
    ok_json(json!({
        "apiKey": snapshot.masked_api_key(),
        "apiKeySet": snapshot.api_key_set(),
        "locale": snapshot.locale(),
        "defaultModel": snapshot.default_model(),
        // 脱敏摘要（形状与 /api/session 的 desensitize 一致）
        "desensitize": state.desensitize().summary(),
    }))
}

/// POST /api/config（`PUT /api/config` 走同一处理函数，见模块头说明）
///
/// 校验顺序与 Node 版一致：先校验 apiKey（非法直接 400，locale 的改动也不落盘），
/// 再处理 locale。
pub async fn post_config(State(_state): State<ServerState>, body: Bytes) -> Response {
    // 空 body 视为 {}（Node 版 `|| '{}'`）；非法 JSON 给 400 而不是让它冒成 500
    let payload = match parse_body(&body) {
        Ok(value) => value,
        Err(error) => return errors::management_error(400, error.message),
    };
    let Some(object) = payload.as_object() else {
        return errors::management_error(400, "请求体必须是 JSON 对象");
    };

    // ── apiKey ──────────────────────────────────────────────
    if let Some(value) = object.get("apiKey") {
        match value {
            // null → 删除 key（对应 Node 版 delete config.apiKey + opts.apiKey = null）
            Value::Null => {
                config::set_api_key(None);
                logging::log("[Config]", "API Key 已清除，接口恢复免鉴权（仅监听 127.0.0.1）");
            }
            Value::String(text) => {
                if text.chars().count() < MIN_API_KEY_LENGTH {
                    return errors::management_error(400, API_KEY_TOO_SHORT);
                }
                config::set_api_key(Some(text.clone()));
                // 不打印 key 本身（日志会被导出与截图），只说明已更新
                logging::log("[Config]", "✅ API Key 已更新，客户端需带 Authorization: Bearer <key>");
            }
            // 非 null 且非字符串 → 同一个 400 文案（照抄 Node 版判定）
            _ => return errors::management_error(400, API_KEY_TOO_SHORT),
        }
    }

    // ── locale ──────────────────────────────────────────────
    if let Some(Value::String(text)) = object.get("locale") {
        if !text.is_empty() {
            config::set_locale(text);
            logging::log("[Config]", &format!("计费语言已更新为 {text}"));
        }
    }

    let snapshot = config::current();
    ok_json(json!({
        "apiKeySet": snapshot.api_key_set(),
        "locale": snapshot.locale(),
        "defaultModel": snapshot.default_model(),
    }))
}

