//! 对话转发的请求构造与上游错误解析。
//!
//! 对照 Node 版 workbuddy-upstream-client.mjs 的 buildHeaders / buildRequestInit /
//! chatCompletionsUrl / readUpstreamError / ensureLeadingSystemMessage 与几个常量。
//!
//! ── 端点（不带 prefixPath）──────────────────────────────────
//!   POST {endpoint}/v2/chat/completions
//!
//! ── 鉴权头（对齐桌面端 AuthService.buildHeaders + CLI ModelProviderImpl）──
//!   Authorization: Bearer <accessToken>
//!   X-User-Id / X-Enterprise-Id / X-Tenant-Id / X-Domain（企业账号条件头）
//!   User-Agent / X-IDE-Type / X-IDE-Name / X-IDE-Version / X-Product
//!   X-Agent-Intent: craft
//!   X-Conversation-ID / X-Session-ID / X-Request-ID 等会话追踪头
//!
//! **注意头集合与 billing/auth 不同**：这里按 Node 的 forwardChatCompletions
//! 走，多带 X-IDE-* / X-Agent-Intent 与四个追踪头，且 Accept 可被调用方覆盖
//! （SSE 转发时是 `text/event-stream`）。逐个对照，不要与计费那套混用。

use std::time::Duration;

use serde_json::{json, Value};

use crate::server::core::auth::AuthService;
use crate::server::core::egress;
use crate::server::core::endpoints::{normalize_endpoint, resolve_edition, EditionInfo};
use crate::server::core::proxies::ResolvedProxy;
use crate::server::errors::GatewayError;

/// 上游错误码 11128（历史文案：Illegal API invocation from an unapproved channel）。
///
/// 实测语义：多为提示词命中上游敏感词审核而被拦截（并非单纯的频率风控）。
/// 常量名沿用 Node 版的历史命名，不要据此理解成「频率限制」。
/// 拦截会持续一小段时间，期间客户端失败自动重试会形成重试风暴并给拦截续期，
/// 因此命中 11128 时退避间隔必须足够长、仅重试 2 次（见 WAF_RETRY_DELAYS_MS）。
pub const RATE_LIMIT_CODE: i64 = 11128;

/// 11128 的退避间隔（10 秒、25 秒），照抄 Node 版
pub const WAF_RETRY_DELAYS_MS: &[u64] = &[10_000, 25_000];

/// 上游要求首条消息必须是 system prompt，否则返回 400
/// （first message is not system prompt）。客户端没带 system 消息时注入一条兜底系统消息。
pub const DEFAULT_SYSTEM_PROMPT: &str = "你是一个得力助手";

/// 上游限额码 6004（HTTP 429）：账号×模型维度的用量限额，
/// msg 形如 `您的使用量已超出频率限制，将在 2026-09-11 19:43:46 UTC+8 重置…`
/// —— 恢复时间只在文本里，由 errors::parse_quota_reset_at 解析。
pub const QUOTA_LIMIT_CODE: i64 = 6004;

/// 单次对话请求的总超时（除 WAF 退避外的等待时间）。
///
/// 与 Node 版的差别：Node 的 proxyFetch 不设总超时（bodyTimeout: 0），
/// 完全靠客户端断开与上游主动结束。Rust 侧的 reqwest client 用
/// `read_timeout(600s)` 限制「两次数据之间」的间隔（见 core::egress 的说明），
/// 这里不再叠加总超时 —— 那会掐断长回答的 SSE 流。
pub const NO_TOTAL_TIMEOUT: Option<u64> = None;

/// 一次上游请求的全部素材
pub struct ChatRequestPlan {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub payload: String,
    pub proxy: Option<ResolvedProxy>,
}

/// 首条消息不是 system 时，在头部补一条系统消息（不改原数组，返回新 body）。
///
/// 已经是 system 开头、messages 为空/缺失时原样返回（同一引用，便于调用方
/// 判断是否改写）—— 空 messages 属客户端异常输入，交给上游按原样报错。
///
/// 与 Node 的差异只在**键顺序**：Node 用对象展开、Rust 用 `Map`，
/// 序列化后的成员顺序不同（严格解析器不关心顺序）。
pub fn ensure_leading_system_message(body: &Value) -> Option<Value> {
    let messages = body.get("messages").and_then(Value::as_array)?;
    if messages.is_empty() {
        return None;
    }
    // Node: `String(body.messages[0]?.role ?? '').toLowerCase()` ——
    // role 缺失/null 都当空串（不是把整条消息字符串化）
    let first_role = match messages[0].get("role") {
        Some(Value::String(text)) => text.to_lowercase(),
        Some(Value::Null) | None => String::new(),
        Some(other) => value_text(other).to_lowercase(),
    };
    if first_role == "system" {
        return None;
    }
    let mut next = body.clone();
    let Some(object) = next.as_object_mut() else {
        return None;
    };
    let mut with_system = Vec::with_capacity(messages.len() + 1);
    with_system.push(json!({ "role": "system", "content": DEFAULT_SYSTEM_PROMPT }));
    with_system.extend(messages.iter().cloned());
    object.insert("messages".to_string(), Value::Array(with_system));
    Some(next)
}

/// 客户端身份 + 鉴权 + 会话追踪头（对照 Node 的 buildHeaders）。
///
/// `request_id` 同时充当 X-Request-ID / X-Conversation-Request-ID /
/// X-Conversation-ID / X-Session-ID —— Node 在没有 conversationId 时就是
/// 这四者取同一个值（追踪一轮对话用）。
pub fn chat_headers(session: &Value, request_id: &str, accept: Option<&str>) -> Vec<(String, String)> {
    let edition: &'static EditionInfo = resolve_edition(
        session.get("edition").and_then(Value::as_str),
    );
    let mut headers: Vec<(String, String)> = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        // 完整三段 UA（含 CLI 扩展段）：与桌面客户端一致，服务端按此识别通道
        (
            "User-Agent".to_string(),
            crate::server::core::endpoints::user_agent_for_edition(Some(edition.id)),
        ),
        // 客户端身份头：服务端按此做客户端识别与白名单校验
        ("X-IDE-Type".to_string(), edition.ua_platform.to_string()),
        ("X-IDE-Name".to_string(), edition.product_name.to_string()),
        ("X-IDE-Version".to_string(), edition.client_version.to_string()),
        ("X-Product".to_string(), edition.product_name.to_string()),
        ("X-Agent-Intent".to_string(), "craft".to_string()),
        // 会话追踪
        ("X-Request-ID".to_string(), request_id.to_string()),
        ("X-Conversation-Request-ID".to_string(), request_id.to_string()),
        ("X-Conversation-ID".to_string(), request_id.to_string()),
        ("X-Session-ID".to_string(), request_id.to_string()),
    ];
    headers.extend(AuthService::build_auth_headers(session));
    if let Some(accept) = accept {
        headers.push(("Accept".to_string(), accept.to_string()));
    }
    headers
}

/// 对话接口 URL：`{session.endpoint || 默认端点}/v2/chat/completions`
pub fn chat_completions_url(session: &Value, fallback_base_url: &str) -> String {
    let endpoint = session
        .get("endpoint")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(normalize_endpoint)
        .unwrap_or_else(|| normalize_endpoint(fallback_base_url));
    format!("{endpoint}/v2/chat/completions")
}

/// 上游错误响应 → `{code, message}`（对照 Node 的 readUpstreamError）。
///
/// `message` 的兜底是响应文本的前 500 个字符（Node 的 `text.slice(0, 500)`）。
/// Node 侧还带一个 `body` 原文，调用方会从里面再解析一次 msg 取恢复时间；
/// Rust 侧的 `GatewayError.message` 已经是同一份上游文案（见 rotate.rs 的
/// `mark_account_limited`），不再重复透出原始 body。
pub struct UpstreamErrorDetail {
    pub code: Option<i64>,
    pub message: String,
}

pub async fn read_upstream_error(response: reqwest::Response) -> UpstreamErrorDetail {
    let text = response.text().await.unwrap_or_default();
    let mut code = None;
    let mut message: String = text.chars().take(500).collect();
    if let Ok(payload) = serde_json::from_str::<Value>(&text) {
        code = payload.get("code").and_then(Value::as_i64);
        // Node: `payload?.message || payload?.msg || payload?.error?.message || message`
        let candidate = payload
            .get("message")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .or_else(|| {
                payload
                    .get("msg")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
            })
            .or_else(|| {
                payload
                    .get("error")
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
            });
        if let Some(candidate) = candidate {
            message = candidate.to_string();
        }
    }
    UpstreamErrorDetail { code, message }
}

/// 发一次上游请求（不读 body，保留原始响应给流式转发）。
///
/// 与 `core::auth_http::send_raw` 的分工：那个把响应读成文本，用于管理接口；
/// 这个把 `reqwest::Response` 原样交给调用方，SSE 需要 `bytes_stream()`。
///
/// 出网仍然只走 `core::egress::client_for`：同一出口共用一个连接池，
/// 客户端上的 connect/read 超时也一并复用（见 egress 头部的旋钮映射）。
pub async fn send_chat_request(
    plan: &ChatRequestPlan,
) -> Result<reqwest::Response, UpstreamRequestError> {
    let client = egress::client_for(plan.proxy.as_ref());
    let mut builder = client
        .post(&plan.url)
        .body(plan.payload.clone());
    for (key, value) in &plan.headers {
        builder = builder.header(key, value);
    }
    if let Some(timeout) = NO_TOTAL_TIMEOUT {
        builder = builder.timeout(Duration::from_millis(timeout));
    }
    builder.send().await.map_err(|error| {
        // 网络层错误（ECONNREFUSED、代理鉴权失败、DNS…）：带上根因与出口说明，
        // 便于判断是不是代理配错了 —— 文案照抄 Node 的 fetchViaProxy
        let via = match &plan.proxy {
            Some(proxy) if !proxy.label.is_empty() => format!("经代理 {}", proxy.label),
            Some(proxy) => format!("经代理 {}", proxy.host),
            None => "直连".to_string(),
        };
        UpstreamRequestError {
            message: format!(
                "上游请求失败（{via}）: {}",
                egress::describe_error_detail(&error)
            ),
        }
    })
}

/// 传输层失败（统一收敛成 502，与 Node 的 fetchViaProxy 一致）
#[derive(Clone, Debug)]
pub struct UpstreamRequestError {
    pub message: String,
}

impl UpstreamRequestError {
    pub fn to_gateway_error(&self) -> GatewayError {
        GatewayError::with_status(502, self.message.clone())
    }
}

/// 是否为账号限额错误（HTTP 429 或上游 code 6004）—— 照抄 Node 的 isQuotaLimitError
pub fn is_quota_limit_error(status_code: i32, upstream_code: Option<i64>) -> bool {
    status_code == 429 || upstream_code == Some(QUOTA_LIMIT_CODE)
}

/// 生成一轮对话的追踪 id（对应 Node 的 `randomUUID()`）。
///
/// 用 RFC 4122 v4 形态：上游会把 X-Request-ID 记进服务端日志，
/// 保持标准 UUID 形态便于与官方客户端的行为对齐（也便于下游按 UUID 解析）。
/// 随机源取自 `RandomState`（由 OS 随机种子初始化）+ 进程内计数器 + 纳秒时钟，
/// 三者拼出的 id 在单机排障场景足够唯一 —— 它不是安全凭证，不用引 rand 依赖。
pub fn new_request_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let mut bytes = [0u8; 16];
    let mix = |label: &[u8], salt: u64| -> u64 {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write(label);
        hasher.write_u64(salt);
        hasher.finish()
    };
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    let pid = std::process::id() as u64;
    let first = mix(b"wb-request-id-a", nanos ^ pid);
    let second = mix(b"wb-request-id-b", counter.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    bytes[..8].copy_from_slice(&first.to_be_bytes());
    bytes[8..].copy_from_slice(&second.to_be_bytes());
    // 版本位 4 + 变体位 10xx（RFC 4122）
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
    )
}

/// JS `String(x)`（role 这类字段的容错文本化）
fn value_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        Value::Number(number) => number.to_string(),
        Value::Bool(flag) => flag.to_string(),
        other => other.to_string(),
    }
}
