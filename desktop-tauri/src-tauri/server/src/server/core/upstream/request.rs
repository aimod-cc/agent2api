//! 对话转发的**传输层**：请求发送、上游错误解析、追踪 id 生成。
//!
//! ── 本文件为什么只做传输（Agent2API 改造 W2b-T3）────────────
//! 改造前这里还有头集合、URL 拼接、system 注入与 11128/6004 的判定 ——
//! 那些全是 **workbuddy 专属知识**（X-IDE-* 头、`/v2/chat/completions`、
//! 首条消息必须是 system、11128 敏感词拦截码），已整体搬进
//! `core::providers::workbuddy`。改造后 provider 差异全部收在适配器里，
//! 本文件对「上游是哪一家」一无所知：它只认 `TransportRequest` 这个
//! **协议无关**的形态（URL + 头 + 已序列化的 body + 出口）。
//!
//! 只保留的三件事：
//!   1. `send_chat_request`：按出口取 reqwest Client 发一次请求，
//!      不读 body（SSE 需要 `bytes_stream()`）；
//!   2. `read_upstream_error`：把上游错误响应归一化成 `{code, message}`
//!      —— 「怎么读一个 HTTP 错误体」是协议层的事，与哪一家无关；
//!   3. `new_request_id`：一轮对话的追踪 id（适配器拼追踪头时用）。
//!
//! ── 出网 ────────────────────────────────────────────────────
//! 只走 `core::egress::client_for`：同一出口共用一个连接池，
//! 客户端上的 connect/read 超时也一并复用（见 egress 头部的旋钮映射）。

use std::time::Duration;

use serde_json::{json, Value};

use crate::server::core::egress;
use crate::server::core::proxies::ResolvedProxy;
use crate::server::errors::GatewayError;

/// 单次对话请求的总超时（除退避重试外的等待时间）。
///
/// 与 Node 版的差别：Node 的 proxyFetch 不设总超时（bodyTimeout: 0），
/// 完全靠客户端断开与上游主动结束。Rust 侧的 reqwest client 用
/// `read_timeout(600s)` 限制「两次数据之间」的间隔（见 core::egress 的说明），
/// 这里不再叠加总超时 —— 那会掐断长回答的 SSE 流。
pub const NO_TOTAL_TIMEOUT: Option<u64> = None;

/// 单次尝试**等待上游响应头**的上限（毫秒）。
///
/// 与 OmniProxy 的 `headers_timeout`（默认 300 秒）同义：连接建立之后、
/// 响应头到达之前的静默等待必须有个上限 —— `read_timeout` 管的是「两次数据
/// 之间」，虽然首包之前的等待也计入它，但 600 秒 × 重试链（同账号重发 ×
/// 换号）会把一条请求拖到几十分钟，而客户端早就等不及了。
///
/// 为什么 300 秒是安全的：网关请求上游**恒带 `stream: true`**（见
/// `UpstreamService::forward` 的说明），响应头在 SSE 建立时就到达，
/// 与「模型思考多久」无关；非流式的长回答也走这条路径（本地聚合）。
/// 超时按传输层失败处理（502 + 既有退避重试），与连接失败同一档。
pub const HEADERS_TIMEOUT_MS: u64 = 300_000;

/// 一次上游请求的全部素材（协议无关形态；provider 差异在构造阶段已消解）
pub struct TransportRequest {
    /// 上游完整 URL（由适配器给出）
    pub url: String,
    /// 请求头（由适配器给出；不含出网代理相关）
    pub headers: Vec<(String, String)>,
    /// 已序列化的请求体（由编排层从适配器的 `plan.body` 序列化而来）
    pub payload: String,
    /// 出网代理（账号级；与 provider 无关，由编排层解析后带上）
    pub proxy: Option<ResolvedProxy>,
}

/// 归一化后的上游错误：`{code, message}`
///
/// `message` 的兜底是响应文本的前 500 个字符（Node 的 `text.slice(0, 500)`）。
/// 适配器的 `classify_error` 收的就是本结构的 JSON 形态（`to_value`）——
/// 于是「上游错误怎么读」与「这条错误属于哪一档」彻底分开：
/// 前者是本文件（协议层），后者是适配器（provider 层）。
pub struct UpstreamErrorDetail {
    pub code: Option<i64>,
    pub message: String,
}

impl UpstreamErrorDetail {
    /// 适配器入参形态：`{code, message}`（键名与上游 JSON 一致，
    /// 于是适配器可以直接用 `error_body.get("code")` 读，不需要中间结构体）
    pub fn to_value(&self) -> Value {
        json!({
            "code": self.code.map(Value::from).unwrap_or(Value::Null),
            "message": self.message,
        })
    }
}

/// 上游错误响应 → `{code, message}`（对照 Node 的 readUpstreamError）。
///
/// `capture`：调试模式的采集器（None = 未开启）。**必须在这里采** —— 本函数
/// 用 `response.text()` 把响应体整个吃掉，调用方拿不到第二份；不在这里顺手
/// 旁路，错误响应体就永远进不了调试报文（而失败现场恰恰是最需要看的那一半）。
pub async fn read_upstream_error(
    response: reqwest::Response,
    capture: Option<&crate::server::core::debug_traffic::TrafficCapture>,
) -> UpstreamErrorDetail {
    if let Some(capture) = capture {
        capture.attach_response(response.status().as_u16(), response.headers());
    }
    let text = response.text().await.unwrap_or_default();
    if let Some(capture) = capture {
        capture.push(text.as_bytes());
    }
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

/// 发一次上游请求（不读 body，保留原始响应给流式转发与错误解析）。
///
/// 与 `core::auth_http::send_raw` 的分工：那个把响应读成文本，用于管理接口；
/// 这个把 `reqwest::Response` 原样交给调用方，SSE 需要 `bytes_stream()`。
pub async fn send_chat_request(
    plan: &TransportRequest,
) -> Result<reqwest::Response, UpstreamRequestError> {
    let client = egress::client_for(plan.proxy.as_ref());
    let mut builder = client.post(&plan.url).body(plan.payload.clone());
    for (key, value) in &plan.headers {
        builder = builder.header(key, value);
    }
    if let Some(timeout) = NO_TOTAL_TIMEOUT {
        builder = builder.timeout(Duration::from_millis(timeout));
    }
    // 出口说明（失败文案用）：文案照抄 Node 的 fetchViaProxy
    let via = match &plan.proxy {
        Some(proxy) if !proxy.label.is_empty() => format!("经代理 {}", proxy.label),
        Some(proxy) => format!("经代理 {}", proxy.host),
        None => "直连".to_string(),
    };
    // 等待响应头有上限（见 HEADERS_TIMEOUT_MS 的说明）：超时后 future 被丢弃，
    // 上游连接随之关闭（与客户端断开时的取消是同一机制）
    match tokio::time::timeout(Duration::from_millis(HEADERS_TIMEOUT_MS), builder.send()).await {
        Ok(Ok(response)) => Ok(response),
        Ok(Err(error)) => Err(UpstreamRequestError {
            // 网络层错误（ECONNREFUSED、代理鉴权失败、DNS…）：带上根因与出口说明，
            // 便于判断是不是代理配错了
            message: format!(
                "上游请求失败（{via}）: {}",
                egress::describe_error_detail(&error)
            ),
        }),
        Err(_elapsed) => Err(UpstreamRequestError {
            message: format!(
                "上游响应超时（等待响应头超过 {} 秒，出口 {via}）",
                HEADERS_TIMEOUT_MS / 1000
            ),
        }),
    }
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
