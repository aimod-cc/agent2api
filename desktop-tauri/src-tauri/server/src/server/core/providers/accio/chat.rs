//! Accio 对话转发（会话式转发入口的实现主体）。
//!
//! ── 一次转发分三步（与 Qoder 同一分工）──────────────────────
//!   构造（[`build_plan`]）→ 发送（[`send`]）→ 翻译（[`drive_stream`] /
//!   [`drive_aggregate`]）。产出是编排层认识的 `ForwardOutcome`，因此网关的其余
//!   链路（脱敏、记账、错误写出、换号）零改动。
//!
//! ── 为什么走会话式转发入口（`is_stateful`）────────────────────
//! 无状态路径假设「请求体由通用层序列化后原样发出、响应是 OpenAI 的 SSE」。
//! Accio 两条都不满足：请求体是 Gemini 风格的信封（`contents` / `tools` /
//! `token`，见 `protocol.rs`），响应是 ADK 自己的帧（`content.parts` /
//! `turn_complete`，见 `stream.rs`）。通用层的透传既发不出正确的请求体，
//! 也翻译不了回来的帧。
//!
//! ── 流内业务错误为什么要判两次 ────────────────────────────────
//! 上游的业务错误**不体现在 HTTP 状态码上**（常是 200 + 帧里的 `error_code`）。
//! 因此：
//!   - **首帧预读**（[`prefetch_stream_head`]）：把第一个内容帧拦在返回之前，
//!     错误帧转成带状态码的 `Err` 交回编排层换号（此刻还没下发任何字节，
//!     换号无损）；
//!   - **流内**（[`drive_stream`] 的每帧）：中途才冒出来的额度/鉴权错误在
//!     错误现场落冷却（编排层已经退出，看不到它了）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：零 unwrap/expect/panic；不持锁穿越 await
//! （凭证进来前取好快照）。

use futures::StreamExt;
use serde_json::{json, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::core::egress;
use crate::server::core::proxies::ResolvedProxy;
use crate::server::core::upstream::usage::RequestTelemetry;
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::credentials::Credentials;
use super::endpoints;
use super::protocol::{self, UpstreamKind};
use super::stream::{self, Delta, Frame, LineBuffer};

/// 会话式转发用到的翻译器（`mod.rs` 构造它）
pub use super::stream::Translator;

/// 一次转发的完整计划（构造阶段不含网络，可在账号循环里对每个候选账号各跑一次）
pub struct ChatPlan {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// 客户端请求的模型名（下发帧的 `model` 字段）
    pub model_name: String,
    /// 上游模型 code（日志用）
    pub upstream_key: String,
    /// 要不要下发思考内容（上游声明支持思考且档位不是「关」）
    pub thinking: bool,
    /// 本次请求 id（`sg_k` 由它算出来，日志里对得上）
    pub request_id: String,
}

/// 客户端请求体 → 一次上游调用的完整计划。
pub fn build_plan(
    credentials: &Credentials,
    body: &Value,
    model_name: &str,
    effort: Option<&str>,
) -> Result<ChatPlan, GatewayError> {
    let entry = super::models::resolve(model_name, credentials.region).ok_or_else(|| {
        GatewayError::bad_request(format!(
            "模型不存在: {model_name}。Accio 可用模型见 GET /v1/models"
        ))
        .with_code("model_not_found")
    })?;
    let upstream_key = entry
        .get("upstreamKey")
        .or_else(|| entry.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if upstream_key.is_empty() {
        return Err(GatewayError::with_status(502, "Accio 模型目录缺少上游标识"));
    }
    let efforts: Vec<String> = entry
        .get("reasoningEfforts")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    // 档位按模型自己声明的集合收敛（发一个它没声明的档位要么被忽略要么 400）
    let resolved_effort = effort.and_then(|level| protocol::resolve_effort(level, &efforts));
    // 档位落点由目录条目的 `protocol` 决定（见 `models::EffortPlacement`）
    let placement = super::models::EffortPlacement::from_entry(&entry);

    let built = protocol::build_upstream_body(
        body,
        &upstream_key,
        &credentials.access_token,
        resolved_effort.as_deref(),
        placement,
    );
    let request_id = built
        .body
        .get("request_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let mut headers = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Accept".to_string(), "text/event-stream".to_string()),
        ("x-language".to_string(), endpoints::ACCEPT_LANGUAGE.to_string()),
        (
            "x-app-version".to_string(),
            std::env::var("ACCIO_APP_VERSION")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| endpoints::DEFAULT_APP_VERSION.to_string()),
        ),
        ("x-package-region".to_string(), credentials.region.package_region().to_string()),
        // **必带**：缺了它上游不报错，而是回一段「当前版本已不再支持，请升级」
        // 的普通文本（HTTP 200 + 正常帧形态），会被当成模型输出吐给下游。
        // 见 `endpoints::DEFAULT_APP_KEY`。
        ("appKey".to_string(), endpoints::app_key()),
    ];
    // 设备指纹（每条账号自带一个；缺省也能发，但带上更接近真实客户端）
    if !credentials.device_id.is_empty() {
        headers.push(("utdid".to_string(), credentials.device_id.clone()));
    }
    headers.push(("version".to_string(), endpoints::DEFAULT_APP_VERSION.to_string()));

    let body_bytes = serde_json::to_vec(&built.body)
        .map_err(|_| GatewayError::with_status(500, "Accio 请求体序列化失败"))?;
    Ok(ChatPlan {
        url: protocol::generate_content_url(&endpoints::llm_base(), &request_id),
        headers,
        body: body_bytes,
        model_name: model_name.to_string(),
        upstream_key,
        thinking: built.thinking,
        request_id,
    })
}

/// 发一次上游请求（不读体，交给调用方决定怎么消费）。
pub async fn send(
    plan: &ChatPlan,
    proxy: Option<&ResolvedProxy>,
) -> Result<reqwest::Response, GatewayError> {
    let client = egress::client_for(proxy);
    let mut builder = client.post(&plan.url).body(plan.body.clone());
    for (key, value) in &plan.headers {
        builder = builder.header(key, value);
    }
    let via = match proxy {
        Some(proxy) if !proxy.label.is_empty() => format!("经代理 {}", proxy.label),
        Some(proxy) => format!("经代理 {}", proxy.host),
        None => "直连".to_string(),
    };
    let budget = std::time::Duration::from_millis(
        crate::server::config::timeout_settings().headers_ms(),
    );
    match tokio::time::timeout(budget, builder.send()).await {
        Ok(Ok(response)) => Ok(response),
        Ok(Err(error)) if error.is_timeout() => Err(GatewayError::with_status(
            502,
            format!(
                "Accio 连接中超时({}秒，出口 {via})",
                crate::server::config::timeout_settings().connect_ms() / 1000
            ),
        )),
        Ok(Err(error)) => Err(GatewayError::with_status(
            502,
            format!("Accio 上游请求失败（{via}）: {}", egress::describe_error_detail(&error)),
        )),
        Err(_elapsed) => Err(GatewayError::with_status(
            502,
            format!("Accio 等待响应超时({}秒，出口 {via})", budget.as_secs()),
        )),
    }
}

/// 失败响应 → 客户端可用错误（流内错误另走 [`drive_stream`]）。
pub async fn http_error(status: u16, response: reqwest::Response) -> GatewayError {
    let text = response.text().await.unwrap_or_default();
    let detail: String = text.chars().take(300).collect();
    let kind = protocol::classify_upstream_error(status, &text);
    let mapped = match kind {
        // 额度用尽 → 429（编排层据此换号）
        UpstreamKind::Quota => 429,
        // 凭证失效 → 401（编排层据此刷新后重试一次）
        UpstreamKind::Auth => 401,
        UpstreamKind::Other => 502,
    };
    let message = if detail.is_empty() {
        format!("上游返回 {status}")
    } else {
        format!("上游返回 {status}: {detail}")
    };
    GatewayError::with_status(mapped, message).with_optional_code(Some(i64::from(status)))
}

/// 流式链路的限额记账素材（错误发生时用；handler 返回后流还在跑，所以带着走）
#[derive(Clone)]
pub struct LimitContext {
    pub store: AccountStore,
    pub account_id: String,
    pub model: String,
}

/// 把「这个账号对模型限额」记进账号库（冷却键与编排层同源）。
pub fn record_limited(limit: &LimitContext, status: u16, message: &str) {
    if limit.account_id.is_empty() {
        return;
    }
    // reset_at 给 None：上游没给结构化恢复时间，存储层落 10 分钟兜底
    limit.store.mark_rate_limited(
        &limit.account_id,
        &limit.model,
        i64::from(status),
        None,
        None,
        message,
    );
    logging::log(
        "[Accio]",
        &format!("⚠️ 账号 {} 对模型 {} 已限额，进入冷却", limit.account_id, limit.model),
    );
}

/// 一帧的错误 → 网关错误（额度类带 429、鉴权类带 401）
fn frame_error(frame: &Frame) -> GatewayError {
    let text = frame.error_text();
    let kind = protocol::classify_upstream_error(200, &text);
    let status = match kind {
        UpstreamKind::Quota => 429,
        UpstreamKind::Auth => 401,
        UpstreamKind::Other => 502,
    };
    GatewayError::with_status(status, format!("Accio 上游错误：{text}")).with_code("accio_upstream")
}

/// 面向客户端的 chunk 构造（流式）
fn chunk_frame(
    translator: &Translator,
    delta: Value,
    finish_reason: Value,
) -> Value {
    json!({
        "id": translator.response_id(),
        "object": "chat.completion.chunk",
        "created": translator.created(),
        "model": translator.model_out(),
        "choices": [{ "index": 0, "delta": delta, "finish_reason": finish_reason }],
    })
}

/// 把一批 delta 翻成下发的 SSE 帧
fn frames_for_deltas(translator: &mut Translator, deltas: &[Delta]) -> Option<String> {
    let mut out = String::new();
    if !deltas.is_empty() && translator.take_role_frame() {
        out.push_str(&stream::sse_frame(&chunk_frame(
            translator,
            json!({ "role": "assistant", "content": "" }),
            Value::Null,
        )));
    }
    for delta in deltas {
        let payload = match delta {
            Delta::Content(text) => json!({ "content": text }),
            Delta::Reasoning(text) => json!({ "reasoning_content": text }),
            Delta::ToolCall { index, id, name, arguments } => {
                let mut function = serde_json::Map::new();
                if let Some(name) = name {
                    function.insert("name".to_string(), Value::String(name.clone()));
                }
                if let Some(arguments) = arguments {
                    function.insert("arguments".to_string(), Value::String(arguments.clone()));
                }
                let mut call = serde_json::Map::new();
                call.insert("index".to_string(), json!(index));
                if let Some(id) = id {
                    call.insert("id".to_string(), Value::String(id.clone()));
                    call.insert("type".to_string(), Value::String("function".to_string()));
                }
                call.insert("function".to_string(), Value::Object(function));
                json!({ "tool_calls": [Value::Object(call)] })
            }
        };
        out.push_str(&stream::sse_frame(&chunk_frame(translator, payload, Value::Null)));
    }
    (!out.is_empty()).then_some(out)
}

/// 收尾帧（finish_reason + usage）
fn tail_frames(translator: &mut Translator) -> String {
    let mut out = String::new();
    if translator.take_role_frame() {
        out.push_str(&stream::sse_frame(&chunk_frame(
            translator,
            json!({ "role": "assistant", "content": "" }),
            Value::Null,
        )));
    }
    out.push_str(&stream::sse_frame(&chunk_frame(
        translator,
        json!({}),
        Value::String(translator.final_finish_reason()),
    )));
    if let Some(usage) = translator.usage() {
        out.push_str(&stream::sse_frame(&json!({
            "id": translator.response_id(),
            "object": "chat.completion.chunk",
            "created": translator.created(),
            "model": translator.model_out(),
            "choices": [],
            "usage": usage,
        })));
    }
    out.push_str(&stream::sse_done());
    out
}

/// 首帧预读：把上游拉到第一个**有内容或结束**的帧，决定「能不能交给客户端」。
///
/// 返回 `(预读到的帧, 剩余字节流)`。首帧前就撞上错误 → 交回带状态码的 `Err`，
/// 编排层据此换号（此刻一个字节都还没下发）。
pub(super) async fn prefetch_stream_head(
    response: reqwest::Response,
    limit: &LimitContext,
) -> Result<(Vec<Frame>, futures::stream::BoxStream<'static, Result<bytes::Bytes, std::io::Error>>), GatewayError> {
    let source = response.bytes_stream().map(|item| {
        item.map_err(|error| std::io::Error::other(egress::describe_error_detail(&error)))
    });
    let mut source = crate::server::core::upstream::stall::idle_guard(
        Box::pin(source),
        std::time::Duration::from_millis(crate::server::config::timeout_settings().stream_idle_ms()),
    );
    let mut buffer = LineBuffer::new();
    let mut frames: Vec<Frame> = Vec::new();
    while let Some(item) = source.next().await {
        let chunk = item.map_err(|error| {
            GatewayError::with_status(502, format!("Accio 上游流式传输中断: {error}"))
        })?;
        let text = String::from_utf8_lossy(&chunk).to_string();
        for line in buffer.push(&text) {
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let Some(frame) = stream::parse_frame(data) else {
                continue;
            };
            if frame.is_error() {
                let error = frame_error(&frame);
                if error.status_code == 429 {
                    record_limited(limit, 429, &error.message);
                }
                return Err(error);
            }
            let has_content = !frame.parts.is_empty();
            let finished = frame.turn_complete;
            if has_content || finished {
                frames.push(frame);
                // 半行换手：把缓冲区里还没成行的尾巴拼回流头（见 `take_pending`
                // 的说明）—— 不做这一步，被切在 chunk 边界的帧会静默丢内容
                let leftover = buffer.take_pending();
                let source: futures::stream::BoxStream<'static, Result<bytes::Bytes, std::io::Error>> =
                    if leftover.is_empty() {
                        source
                    } else {
                        Box::pin(
                            futures::stream::once(async move { Ok(bytes::Bytes::from(leftover)) })
                                .chain(source),
                        )
                    };
                return Ok((frames, source));
            }
        }
    }
    // 流在没有任何内容帧之前就结束了：交给下游按「空回答」收尾
    Ok((frames, source))
}

/// 流式：逐帧翻译并下发（在 spawn 的后台任务里跑）。
pub async fn drive_stream(
    mut source: futures::stream::BoxStream<'static, Result<bytes::Bytes, std::io::Error>>,
    mut translator: Translator,
    telemetry: std::sync::Arc<RequestTelemetry>,
    limit: LimitContext,
    sender: tokio::sync::mpsc::Sender<Result<bytes::Bytes, std::io::Error>>,
    prefetched: Vec<Frame>,
) {
    let mut buffer = LineBuffer::new();
    let mut failed: Option<GatewayError> = None;
    // 预读到的帧先补发（它们的字节已经被消费掉了）
    for frame in prefetched {
        let deltas = translator.consume(&frame, Some(&telemetry));
        if let Some(text) = frames_for_deltas(&mut translator, &deltas) {
            if sender.send(Ok(bytes::Bytes::from(text))).await.is_err() {
                return;
            }
        }
        if frame.turn_complete {
            let tail = tail_frames(&mut translator);
            let _ = sender.send(Ok(bytes::Bytes::from(tail))).await;
            return;
        }
    }
    while let Some(item) = source.next().await {
        let chunk = match item {
            Ok(chunk) => chunk,
            Err(error) => {
                failed = Some(GatewayError::with_status(
                    502,
                    format!("Accio 上游流式传输中断: {error}"),
                ));
                break;
            }
        };
        let text = String::from_utf8_lossy(&chunk).to_string();
        for line in buffer.push(&text) {
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let Some(frame) = stream::parse_frame(data) else {
                // `[DONE]` 或心跳：`[DONE]` 之前上游一般已经发过 turn_complete，
                // 这里兜底收尾一次（幂等）
                continue;
            };
            if frame.is_error() {
                let error = frame_error(&frame);
                if error.status_code == 429 {
                    record_limited(&limit, 429, &error.message);
                }
                telemetry.note_error(&error.message);
                failed = Some(error);
                break;
            }
            let deltas = translator.consume(&frame, Some(&telemetry));
            if let Some(text) = frames_for_deltas(&mut translator, &deltas) {
                if sender.send(Ok(bytes::Bytes::from(text))).await.is_err() {
                    // 客户端断开：直接退出，drop 掉 source 等于断开上游
                    return;
                }
            }
            if frame.turn_complete {
                let tail = tail_frames(&mut translator);
                let _ = sender.send(Ok(bytes::Bytes::from(tail))).await;
                return;
            }
        }
    }
    // 上游在 turn_complete 之前断了：把已经拿到的内容按正常收尾下发
    // （与通用层「流中断仍给客户端一个 finish 帧」的取向一致），错误只记日志
    if let Some(error) = failed {
        telemetry.note_error(&error.message);
        logging::verbose("[Accio]", &format!("流中途失败：{}", error.message));
    }
    let tail = tail_frames(&mut translator);
    let _ = sender.send(Ok(bytes::Bytes::from(tail))).await;
}

/// 非流式：拉完整条流再聚合成一个 `chat.completion`。
pub async fn drive_aggregate(
    response: reqwest::Response,
    mut translator: Translator,
    telemetry: std::sync::Arc<RequestTelemetry>,
    limit: &LimitContext,
) -> Result<Value, GatewayError> {
    let mut source = response.bytes_stream();
    let mut buffer = LineBuffer::new();
    loop {
        let Some(item) = source.next().await else {
            break;
        };
        let chunk = item.map_err(|error| {
            GatewayError::with_status(
                502,
                format!("Accio 上游流式传输中断: {}", egress::describe_error_detail(&error)),
            )
        })?;
        let text = String::from_utf8_lossy(&chunk).to_string();
        for line in buffer.push(&text) {
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let Some(frame) = stream::parse_frame(data) else {
                continue;
            };
            if frame.is_error() {
                let error = frame_error(&frame);
                if error.status_code == 429 {
                    record_limited(limit, 429, &error.message);
                }
                // 非流式这条路还没有下发任何内容，直接交回编排层换号
                return Err(error);
            }
            translator.consume(&frame, Some(&telemetry));
            if frame.turn_complete {
                return Ok(completion_body(&translator));
            }
        }
    }
    Ok(completion_body(&translator))
}

/// 聚合产出的完整响应（OpenAI `chat.completion`）
fn completion_body(translator: &Translator) -> Value {
    let mut message = serde_json::Map::new();
    message.insert("role".to_string(), Value::String("assistant".to_string()));
    let content = translator.content();
    message.insert(
        "content".to_string(),
        if content.is_empty() { Value::Null } else { Value::String(content.to_string()) },
    );
    let reasoning = translator.reasoning();
    if !reasoning.is_empty() {
        message.insert("reasoning_content".to_string(), Value::String(reasoning.to_string()));
    }
    let tool_calls = translator.final_tool_calls();
    if !tool_calls.is_empty() {
        message.insert("tool_calls".to_string(), Value::Array(tool_calls));
    }
    let mut body = json!({
        "id": translator.response_id(),
        "object": "chat.completion",
        "created": translator.created(),
        "model": translator.model_out(),
        "choices": [{
            "index": 0,
            "message": Value::Object(message),
            "finish_reason": translator.final_finish_reason(),
        }],
    });
    if let Some(usage) = translator.usage() {
        body["usage"] = usage;
    }
    body
}

/// 响应 id（与另外几家同形：`chatcmpl-` + 24 位随机）
pub fn response_id() -> String {
    let uuid = crate::server::core::upstream::request::new_request_id().replace('-', "");
    let short: String = uuid.chars().take(24).collect();
    format!("chatcmpl-{short}")
}
