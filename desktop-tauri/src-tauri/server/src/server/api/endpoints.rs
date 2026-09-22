//! GET /api/endpoints —— 把逆向出的接口清单直接暴露出来，便于排查。
//!
//! 对照 Node 版 server.mjs 688-711 行：响应含当前生效的 baseUrl / prefixPath /
//! platform / edition、完整 EDITIONS 表、clientVersion、一句 note，
//! 以及 AUTH/LLM/BILLING/ACTIVITY/CONFIG/CLOUD_AGENT/SIDECAR 七张端点表。
//!
//! **免鉴权**（Node 版这条没查 API Key）：它只暴露公开的产品配置，不含任何凭证；
//! 排查「到底是哪个端点不对」时往往还没配好 key，需要它直接可访问。
//!
//! 端点表本身在 `core::endpoints` 里以 json! 字面量复刻（含「函数型 path 被
//! JSON.stringify 丢弃」这类真实细节，见那边的注释），这里只负责拼装响应。

use axum::extract::State;
use axum::response::Response;
use serde_json::json;

use crate::server::core::endpoints::{
    activity_endpoints, auth_endpoints, billing_endpoints, cloud_agent_endpoints, config_endpoints,
    default_context, editions_to_json, llm_endpoints, sidecar_endpoints, WORKBUDDY_CLIENT_VERSION,
};
use crate::server::http::ok_json;
use crate::server::ServerState;

pub async fn handle(State(_state): State<ServerState>) -> Response {
    // 展示的是「进程默认上下文」：Node 版读的是 auth.baseUrl / auth.prefixPath，
    // 那是构造时按 --endpoint/--edition 归一后的结果，与当前账号无关
    let context = default_context();
    ok_json(json!({
        "baseUrl": context.base_url,
        "prefixPath": context.prefix,
        "platform": context.platform,
        "edition": context.edition,
        "editions": editions_to_json(),
        "clientVersion": WORKBUDDY_CLIENT_VERSION,
        "note": "登录/账号带 prefixPath；计费/签到/LLM 不带；多账号时端点按账号 edition 走",
        "auth": auth_endpoints(),
        "llm": llm_endpoints(),
        "billing": billing_endpoints(),
        "activity": activity_endpoints(),
        "config": config_endpoints(),
        "cloudAgent": cloud_agent_endpoints(),
        "sidecar": sidecar_endpoints(),
    }))
}
