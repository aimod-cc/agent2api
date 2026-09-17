//! GET /health —— 壳侧就绪探测与前端首屏健康摘要。
//!
//! shape 逐字段对齐 Node 版 server.mjs handleHealth（562-584 行）：
//!   status 按「上游是否配置」（configured 才 ok，否则 degraded）/
//!   upstreamBaseUrl / authApiBase / chatApiUrl / authSource / currentAccountId /
//!   tokenExpiresAt / canRefresh 全来自 `auth.get_config_summary()`；
//!   login 为 `auth.get_status()` 的完整形态（已登录/未登录两分支）；
//!   authRequired 按是否配置了 API Key / defaultModel / models 数量。
//!
//! 注意 /health 返回**裸对象**（不是 `{success,data}` 信封）—— Node 版这里
//! 用的是 sendJson 直接发对象，与 /api/* 不同。
//!
//! `get_config_summary()` 是纯本地的（只查凭证是否存在，不发网络请求），
//! 所以 health 不需要等转发链路。
//!
//! `models` 是目录条数（切片 4 起为真值）：Node 版取 `modelCatalog.list().length`，
//! 目录未刷新过时就是内置清单的条数，刷新后是远程清单的条数 ——
//! 这个字段只反映「当前内存里的目录」，不触发刷新（刷新由启动流程与
//! GET /v1/models 负责，/health 保持纯本地语义）。

use axum::extract::State;
use axum::response::Response;
use serde_json::{json, Value};

use crate::server::config;
use crate::server::http::raw_json;
use crate::server::ServerState;

/// 上游未配置时的说明文案。
///
/// 与 `core::auth::UNCONFIGURED_REASON` 同一份 —— Node 版这条有两处措辞：
/// `getConfigSummary()` 的「尚未登录（node server.mjs --login）」与启动横幅的
/// 「暂无可用登录态」。壳内 Rust 版没有那个命令行入口（登录走 UI 的
/// /api/session/login/*），指向一个不存在的命令只会误导用户，因此统一取后者。
pub const UNCONFIGURED_REASON: &str = crate::server::core::auth::UNCONFIGURED_REASON;

/// 处理 GET /health
pub async fn handle(State(state): State<ServerState>) -> Response {
    let snapshot = config::current();
    let summary = state.auth().get_config_summary();
    let status = state.auth().get_status().await;
    let healthy = summary
        .get("configured")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    raw_json(json!({
        "status": if healthy { "ok" } else { "degraded" },
        "transport": "upstream-api",
        "product": "WorkBuddy",
        "upstreamConfigured": healthy,
        "upstreamBaseUrl": summary.get("baseUrl").cloned().unwrap_or(Value::Null),
        "authApiBase": summary.get("authApiBase").cloned().unwrap_or(Value::Null),
        "chatApiUrl": summary.get("chatApiUrl").cloned().unwrap_or(Value::Null),
        "authSource": summary.get("authSource").cloned().unwrap_or(Value::Null),
        "currentAccountId": summary.get("currentAccountId").cloned().unwrap_or(Value::Null),
        "tokenExpiresAt": summary.get("tokenExpiresAt").cloned().unwrap_or(Value::Null),
        "canRefresh": summary.get("canRefresh").and_then(Value::as_bool).unwrap_or(false),
        "login": status,
        "unavailableReason": if healthy {
            Value::Null
        } else {
            summary
                .get("unavailableReason")
                .cloned()
                .unwrap_or_else(|| Value::String(UNCONFIGURED_REASON.to_string()))
        },
        "authRequired": snapshot.api_key_set(),
        "defaultModel": snapshot.default_model(),
        // 模型目录条数（对照 Node 的 `models: modelCatalog.list().length`）——
        // 切片 4 起是真值：目录未刷新过时是内置清单条数，刷新后是远程清单条数
        "models": state.models().count(),
    }))
}
