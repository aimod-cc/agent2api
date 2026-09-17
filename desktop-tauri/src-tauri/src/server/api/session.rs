//! 会话相关路由（对照 Node 版 server.mjs 776-869、963-974 行逐条实现）。
//!
//!   GET    /api/session                 前端首屏状态（**免鉴权**）
//!   POST   /api/session/login/start     发起无头登录，最多等 15 秒拿 authUrl
//!   GET    /api/session/login/wait      轮询登录结果 ?state=
//!   POST   /api/session/login/cancel    取消登录（关弹窗/用户放弃）
//!   POST   /api/session/refresh         刷新当前账号 token
//!   POST   /api/session/logout          清除登录态（删掉当前账号）
//!   POST   /auth/login                  同步登录（等完成才响应）
//!   POST   /auth/logout                 清除登录态
//!
//! ── 鉴权分组（务必与 Node 版一致）────────────────────────────
//! 只有 `GET /api/session` 是免鉴权的（前端首屏拿不到 key 时也要能显示状态）；
//! 其余全部 checkApiKey。在 http.rs 里它们分别挂在 public / protected 组。
//!
//! ── 字段来源（/api/session）────────────────────────────────
//!   health          ← auth.get_config_summary()（纯本地：只查凭证是否存在）
//!   session         ← auth.get_status()（含临期自动刷新）
//!   accounts        ← store.list_accounts()
//!   lastRequestModel← config.json 的 lastRequestModel
//!   defaultModel    ← config.json 的 defaultModel
//!   proxies         ← Clash 摘要（切片 3 起为实时读取的真实值）
//!   models          ← 模型目录（切片 4 起为真实清单，含远程刷新结果）
//!   desensitize     ← 脱敏摘要（切片 5 起为真值：enabled/termCount/roles）

use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::response::Response;
use serde_json::{json, Value};

use crate::server::api::health::UNCONFIGURED_REASON;
use crate::server::config;
use crate::server::core::login::AUTH_URL_WAIT_MS;
use crate::server::errors::management_error;
use crate::server::http::{ok_json, parse_body};
use crate::server::logging;
use crate::server::ServerState;

/// GET /api/session —— 免鉴权（前端首屏在配置 API Key 之前也要能读）
pub async fn get_session(State(state): State<ServerState>) -> Response {
    let snapshot = config::current();
    let summary = state.auth().get_config_summary();
    let configured = summary
        .get("configured")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    ok_json(json!({
        "health": {
            "upstreamConfigured": configured,
            "upstreamBaseUrl": summary.get("baseUrl").cloned().unwrap_or(Value::Null),
            "authSource": summary.get("authSource").cloned().unwrap_or(Value::Null),
            "unavailableReason": if configured {
                Value::Null
            } else {
                summary
                    .get("unavailableReason")
                    .cloned()
                    .unwrap_or_else(|| Value::String(UNCONFIGURED_REASON.to_string()))
            },
        },
        "session": state.auth().get_status().await,
        // accounts.currentAccountId 即首选账号（由优先级派生，见 core::account_store）
        "accounts": state.store().list_accounts(),
        // 最近一次实际请求用的模型：账号页的「模型」筛选默认选它
        "lastRequestModel": snapshot.last_request_model(),
        "defaultModel": snapshot.default_model(),
        "proxies": proxies_summary(),
        // 模型目录真值（对照 server.mjs 802-804）：形状 {id, name, isDefault, credits}
        "models": state.models().session_models(),
        // 脱敏摘要（对照 server.mjs 805-808）：只有 enabled/termCount/roles，
        // 完整词表在 GET /api/desensitize（前端面板不许它覆盖完整状态）
        "desensitize": state.desensitize().summary(),
    }))
}

/// Clash 摘要（Node 版这里只带三项，完整形态在 /api/proxies）。
///
/// 三项的含义：是否读到 Clash Verge 配置 / 读不到的原因 / 可选项数量
/// （混合端口 + 各监听器）。切片 3 起是真实值。
fn proxies_summary() -> Value {
    let clash = crate::server::core::proxies::clash_proxy_options();
    json!({
        "clashAvailable": clash.get("available").and_then(Value::as_bool).unwrap_or(false),
        "clashError": clash.get("error").cloned().unwrap_or(Value::Null),
        "optionCount": clash
            .get("options")
            .and_then(Value::as_array)
            .map(|items| items.len())
            .unwrap_or(0),
    })
}

// ─── POST /api/session/login/start ──────────────────────────

/// 发起登录：起任务后最多等 15 秒拿 authUrl。
///
/// 拿不到就 502 `{success:false, error}` —— 注意**任务仍在后台跑**
/// （Node 版同样如此：返回 502 只是这一次没等到 URL）。
pub async fn login_start(State(state): State<ServerState>, body: Bytes) -> Response {
    // edition 解析失败不是错误（Node 版 `catch { edition = null }`）
    let edition = parse_body(&body)
        .ok()
        .and_then(|payload| {
            payload
                .get("edition")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .filter(|value| !value.is_empty());
    let handle = state.login().start(edition.as_deref());
    match state
        .login()
        .wait_for_auth_url(&handle, Duration::from_millis(AUTH_URL_WAIT_MS))
        .await
    {
        Ok((task_state, auth_url, task_edition)) => ok_json(json!({
            "state": task_state,
            "authUrl": auth_url,
            "edition": task_edition,
        })),
        Err(error) => {
            logging::log("[Login]", &format!("❌ 发起登录失败: {error}"));
            management_error(502, error)
        }
    }
}

// ─── GET /api/session/login/wait ────────────────────────────

/// 轮询登录结果（三种响应：pending / done+error / done+session）
pub async fn login_wait(
    State(state): State<ServerState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let task_state = params.get("state").cloned().unwrap_or_default();
    let Some(task) = state.login().tasks().get(&task_state) else {
        return management_error(404, "登录任务不存在或已过期");
    };
    ok_json(task.snapshot().to_wait_response())
}

// ─── POST /api/session/login/cancel ─────────────────────────

pub async fn login_cancel(State(state): State<ServerState>, body: Bytes) -> Response {
    let task_state = parse_body(&body)
        .ok()
        .and_then(|payload| {
            payload
                .get("state")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default();
    let canceled = state.login().tasks().cancel(&task_state);
    ok_json(json!({ "canceled": canceled }))
}

// ─── POST /api/session/refresh ──────────────────────────────

/// POST /api/session/refresh
///
/// 出错时走**最外层 catch → errorPayload**（OpenAI 风格 `{error:{message,type}}`），
/// 而不是管理 API 的 `{success:false,error}` 信封 —— Node 版这条就在大 try 里，
/// 刷新失败的响应形状与 /api/accounts/refresh 不同，别混用。
pub async fn session_refresh(State(state): State<ServerState>) -> Response {
    match state.auth().refresh_stored_session().await {
        Ok(_) => ok_json(json!({ "session": state.auth().get_status().await })),
        Err(error) => {
            logging::log("[Auth]", &format!("❌ {}", error.message));
            use axum::response::IntoResponse;
            error.to_gateway_error().into_response()
        }
    }
}

// ─── POST /api/session/logout 与 POST /auth/logout ─────────

pub async fn session_logout(State(state): State<ServerState>) -> Response {
    state.auth().clear_session();
    crate::server::http::ok_empty()
}

/// POST /auth/logout —— 与 /api/session/logout 同一动作与同一响应形状
pub async fn auth_logout(State(state): State<ServerState>) -> Response {
    state.auth().clear_session();
    crate::server::http::ok_empty()
}

// ─── POST /auth/login ───────────────────────────────────────

/// 同步登录（对照 server.mjs 601-610 行的 `runLogin`）：等登录完成才响应。
///
/// 桌面端不用这条（它走 /api/session/login/* 的异步三步），保留它是为了
/// 与 Node 版的命令行/脚本入口保持契约一致。
pub async fn auth_login(State(state): State<ServerState>, body: Bytes) -> Response {
    let edition = parse_body(&body)
        .ok()
        .and_then(|payload| {
            payload
                .get("edition")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .filter(|value| !value.is_empty());
    match state.login().run_login(edition.as_deref()).await {
        Ok(session) => crate::server::http::raw_json(session),
        Err(error) => {
            logging::log("[Login]", &format!("❌ {}", error.message));
            // /auth/login 失败走最外层 catch → errorPayload（OpenAI 风格 body），
            // 而不是管理 API 的 `{success:false,error}` 信封 —— 与 Node 版一致
            use axum::response::IntoResponse;
            error.to_gateway_error().into_response()
        }
    }
}
