//! CatPaw 上游 HTTP 传输层：请求头、短请求 POST、状态回报、打断、turn 建流。
//!
//! ── 为什么单独一个文件 ──────────────────────────────────────
//! 这一层与 `upstream/request.rs` 是同一件事的 provider 版：只管「怎么把一个请求
//! 发出去、怎么读一个短响应、发不出去时报什么」，不含任何轮次语义。
//! 与那边的差别只有三点，都来自 CatPaw 的协议特性：
//!
//! ```text
//!   1. 请求头是 Cookie 形态（X-Passport-Token）而不是 Bearer（§9.1）；
//!   2. 短响应要过 unwrapApiData（上游把业务码写在 code 字段里）；
//!   3. turn 是 SSE：不读体、总超时 15 分钟（见 turn_request）。
//! ```
//!
//! ── 出网（账号级代理在这里生效）─────────────────────────────
//! 全部经 `core::egress::client_for(proxy)`：账号级出网代理（`ResolvedProxy`）
//! 由此生效，同一出口共用一个连接池，连接/读取超时旋钮也复用那边。
//! 本文件不做代理的选择与缓存 —— 那是 egress 的职责（架构文档 §2 的
//! 「账号级代理对所有 provider 生效」）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：零 unwrap/expect/panic；本文件不持任何锁。

use std::time::Duration;

use serde_json::{json, Value};

use crate::server::core::egress;
use crate::server::core::proxies::ResolvedProxy;
use crate::server::logging;

use super::conversation::{short_id, CatPawCredentials, ConversationRequest};
use super::models::CatPawError;
use super::turn_executor::TurnContext;
use super::{APP_KEY, DEFAULT_CLIENT_VERSION};

/// round / event / turn/stop 这类「一问一答」请求的总超时
/// （原实现 `REQUEST_TIMEOUT_MS = 30000`）
pub(super) const REQUEST_TIMEOUT_MS: u64 = 30_000;

/// `turn/stop` 的超时（原实现 `STOP_TIMEOUT_MS = 3000`）
pub(super) const STOP_TIMEOUT_MS: u64 = 3_000;

/// turn 的 SSE 超时（原实现 `SSE_TIMEOUT_MS = 15 * 60 * 1000`）。
///
/// 为什么是 15 分钟而不是更短：带工具循环的长回答可能跑很久，而这是**总超时**
/// （reqwest 的 `.timeout()` 覆盖整个响应体），设短了会把正在推进的长回答掐断 ——
/// 客户端一边收到内容一边被断流。
pub(super) const SSE_TIMEOUT_MS: u64 = 15 * 60 * 1000;

/// 上游请求头集合（架构文档 §9.1 / 原实现 `createHeaders`）。
///
/// 客户端的 `authorization` / `cookie` **不透传**（§9.1）：代理凭证与客户端
/// 凭证是两套独立体系，透传会把客户端的 key 泄露给上游。
fn request_headers(credentials: &CatPawCredentials, accept: &str) -> Vec<(String, String)> {
    let mut headers: Vec<(String, String)> = vec![
        ("Accept".to_string(), accept.to_string()),
        ("Content-Type".to_string(), "application/json".to_string()),
        // M-TRACEID 每次请求都是新的随机 UUID（去掉连字符，与原实现一致）
        (
            "M-TRACEID".to_string(),
            crate::server::core::upstream::request::new_request_id().replace('-', ""),
        ),
        ("M-APPKEY".to_string(), APP_KEY.to_string()),
        ("gray-set".to_string(), "new-agent-sdk".to_string()),
        ("enableHeartBeat".to_string(), "true".to_string()),
        ("X-Agent-Version".to_string(), DEFAULT_CLIENT_VERSION.to_string()),
    ];
    if !credentials.token.trim().is_empty() {
        headers.push((
            "Cookie".to_string(),
            format!("X-Passport-Token={}", credentials.token.trim()),
        ));
    }
    if !credentials.uid.trim().is_empty() {
        headers.push(("user-uid".to_string(), credentials.uid.trim().to_string()));
    }
    headers
}

/// 出口的可读描述（日志用；与 `upstream/mod.rs` 同口径）
pub(super) fn describe_proxy(proxy: Option<&ResolvedProxy>) -> String {
    match proxy {
        Some(proxy) if !proxy.label.is_empty() => proxy.label.clone(),
        Some(proxy) => proxy.host.clone(),
        None => "直连".to_string(),
    }
}

/// 一次「一问一答」类型的上游 POST：发请求 → 读体 → 校验 HTTP → `unwrapApiData`。
///
/// round / event / turn/stop 三个端点都是短请求，共用这一个函数：出网经
/// `egress::client_for`（连接池与超时旋钮都在那里，账号级代理在此生效），
/// 总超时由 `timeout_ms` 给出（与客户端上的 read_timeout 是叠加关系）。
///
/// `capture`：调试模式的采集器（`None` = 不采）。**只有承载对话内容的 round
/// 会传**（见 `conversation::submit_round`）—— 状态回报、turn/stop 这些
/// 控制类往返不是用户要看的「上游报文」，目录刷新（`catalog.rs`）同理。
/// 传进来时本函数负责把请求 envelope 与响应头/体都补上：它在唯一能同时
/// 拿到这三样的位置（响应一旦被 `text()` 读走就没有第二份）。
pub(super) async fn post_json(
    base_url: &str,
    path: &str,
    credentials: &CatPawCredentials,
    proxy: Option<&ResolvedProxy>,
    body: &Value,
    timeout_ms: u64,
    capture: Option<&crate::server::core::debug_traffic::TrafficCapture>,
) -> Result<Value, CatPawError> {
    let url = format!("{}{path}", base_url.trim_end_matches('/'));
    let payload = serde_json::to_string(body)
        .map_err(|error| CatPawError::upstream(format!("请求体序列化失败: {error}")))?;
    let client = egress::client_for(proxy);
    let mut builder = client
        .post(&url)
        .timeout(Duration::from_millis(timeout_ms))
        .body(payload);
    let headers = request_headers(credentials, "application/json");
    for (key, value) in &headers {
        builder = builder.header(key, value);
    }
    if let Some(capture) = capture {
        capture.reset_request(&url, "catpaw", &headers, body);
    }
    let response = builder.send().await.map_err(|error| {
        CatPawError::upstream(format!(
            "上游请求失败（{}）: {}",
            describe_proxy(proxy),
            egress::describe_error_detail(&error),
        ))
    })?;
    let status = response.status().as_u16();
    if let Some(capture) = capture {
        capture.attach_response(status, response.headers());
    }
    let text = response.text().await.unwrap_or_default();
    if let Some(capture) = capture {
        capture.push(text.as_bytes());
    }
    if !(200..300).contains(&status) {
        logging::log("[CatPaw]", &format!("上游 {path} HTTP {status}"));
        let detail: String = text.trim().chars().take(500).collect();
        return Err(CatPawError::http(
            status,
            if detail.is_empty() {
                format!("上游 {path} HTTP {status}")
            } else {
                format!("上游 {path} HTTP {status}: {detail}")
            },
        ));
    }
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }
    let parsed = serde_json::from_str::<Value>(&text).unwrap_or(Value::Null);
    super::openai::unwrap_api_data(parsed)
}

/// `event`：状态回报（`running` / `completed` / `failed` / `canceled`）
pub(super) async fn report_status(
    base_url: &str,
    credentials: &CatPawCredentials,
    proxy: Option<&ResolvedProxy>,
    conversation_id: &str,
    status: &str,
    error: Option<&CatPawError>,
) -> Result<Value, CatPawError> {
    let mut data = serde_json::Map::new();
    data.insert("status".to_string(), Value::String(status.to_string()));
    if let Some(error) = error {
        // 失败原因与业务码一起回报（原实现 `reportStatus` 的 data 字段：
        // failReason / failCode / unifyCode）
        data.insert("failReason".to_string(), Value::String(error.message.clone()));
        if let Some(code) = error.code {
            data.insert("failCode".to_string(), Value::from(code));
        }
        if let Some(code) = error.unify_code {
            data.insert("unifyCode".to_string(), Value::from(code));
        }
    }
    let body = json!({
        "conversationId": conversation_id,
        "eventType": "conversation",
        "data": Value::Object(data),
    });
    post_json(
        base_url,
        "/api/agent/conversation/event",
        credentials,
        proxy,
        &body,
        REQUEST_TIMEOUT_MS,
        // 状态回报是控制类往返，不是用户要看的「上游报文」：不采
        None,
    )
    .await
}

/// 终态上报（**吞掉失败**）：原实现 `reportTerminalStatus`。
///
/// 为什么吞：终态上报失败不该把已经成功的轮次变成失败 —— 客户端已经拿到完整
/// 回答了。它只影响上游侧的状态机（下一轮 round 可能被拒），而那条路径有
/// `conversation.rs` 的 `submit_round_with_self_heal` 兜底。
pub(super) async fn report_terminal(
    base_url: &str,
    credentials: &CatPawCredentials,
    proxy: Option<&ResolvedProxy>,
    conversation_id: &str,
    status: &str,
    error: Option<&CatPawError>,
) {
    if conversation_id.is_empty() {
        return;
    }
    if let Err(failure) =
        report_status(base_url, credentials, proxy, conversation_id, status, error).await
    {
        logging::verbose(
            "[CatPaw]",
            &format!("终态 {status} 上报失败: {}", failure.message),
        );
    }
}

/// `turn/stop`：打断一个正在执行的轮次（原实现 `stopTurn`，**尽力而为**）。
///
/// 失败只打日志：打断是收尾动作，把它的失败报成请求失败会掩盖真正的错误。
pub(super) async fn stop_turn(
    base_url: &str,
    credentials: &CatPawCredentials,
    proxy: Option<&ResolvedProxy>,
    conversation_id: &str,
    turn_request_id: &str,
) {
    if conversation_id.is_empty() || turn_request_id.is_empty() {
        return;
    }
    let body = json!({
        "conversationId": conversation_id,
        "turnRequestId": turn_request_id,
    });
    if let Err(error) = post_json(
        base_url,
        "/api/agent/conversation/turn/stop",
        credentials,
        proxy,
        &body,
        STOP_TIMEOUT_MS,
        // turn/stop 同理：控制类往返，不采
        None,
    )
    .await
    {
        logging::verbose("[CatPaw]", &format!("turn/stop 失败: {}", error.message));
    }
}

/// 发 turn 请求（SSE）。
///
/// 与 `post_json` 的差别只有两点：Accept 是 `text/event-stream`、**不读响应体**
/// （`bytes_stream()` 交给执行层逐 chunk 消费）；总超时用 `SSE_TIMEOUT_MS`
/// 而不是 30 秒 —— 长回答的 SSE 流会被 30 秒超时掐断。
///
/// HTTP 非 2xx 在这里就失败（上游拒绝执行轮次，比如会话状态不对）：那时响应体
/// 是错误 JSON 而不是 SSE，读出来当文案比让执行层去解析它更清楚。
pub(super) async fn turn_request(
    request: &ConversationRequest,
    ctx: &TurnContext,
    body: &Value,
) -> Result<reqwest::Response, CatPawError> {
    let url = format!("{}/api/agent/conversation/turn", request.base_url.trim_end_matches('/'));
    let payload = serde_json::to_string(body)
        .map_err(|error| CatPawError::upstream(format!("请求体序列化失败: {error}")))?;
    let client = egress::client_for(request.proxy.as_ref());
    let mut builder = client
        .post(&url)
        .timeout(Duration::from_millis(SSE_TIMEOUT_MS))
        .body(payload);
    for (key, value) in request_headers(&ctx.credentials, "text/event-stream") {
        builder = builder.header(key, value);
    }
    let response = builder.send().await.map_err(|error| {
        CatPawError::upstream(format!(
            "上游请求失败（{}）: {}",
            describe_proxy(request.proxy.as_ref()),
            egress::describe_error_detail(&error),
        ))
    })?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        let text = response.text().await.unwrap_or_default();
        logging::log(
            "[CatPaw]",
            &format!("上游 turn HTTP {status} conversationId={}", short_id(&ctx.conversation_id)),
        );
        let detail: String = text.trim().chars().take(500).collect();
        return Err(CatPawError::http(
            status,
            if detail.is_empty() {
                format!("上游 turn HTTP {status}")
            } else {
                format!("上游 turn HTTP {status}: {detail}")
            },
        ));
    }
    logging::verbose(
        "[CatPaw]",
        &format!(
            "turn 已建立 conversationId={} turnRequestId={}",
            short_id(&ctx.conversation_id),
            short_id(&ctx.turn_request_id),
        ),
    );
    Ok(response)
}
