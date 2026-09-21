//! 对话链路的另外两条协议入口（对照 `/v1/chat/completions` 实现）。
//!
//!   POST /v1/responses   Responses API（OpenAI 新协议；Codex 等客户端在用）
//!   POST /v1/messages    Anthropic Messages API（Claude Code 等客户端在用）
//!
//! ── 这两个入口做什么 ────────────────────────────────────────
//! 两者都是**薄壳**：把下游协议的请求翻译成 Chat Completions，交给与
//! `/v1/chat/completions` **完全相同**的转发链路（模型解析、账号选路、轮换、
//! 脱敏、记账都不重写），再把回程翻译回下游协议。翻译规则在
//! `core::protocol` 里，公共管道在 `api::pipeline` 里 —— 本文件只负责
//! 「HTTP 层」：取 body、调管道、按协议形态写响应。
//!
//! ── 为什么不做成「每家适配器各支持两套协议」──────────────────
//! 见 `core::protocol` 模块头的完整论证。一句话：本项目所有上游经适配器层
//! 已归一为 Chat，所以在出入口转换一次（2 套）远优于在 6 家适配器里各写
//! （12 套），且新增上游时不必再补转换。
//!
//! ── 非流式请求为什么也让上游走流式 ──────────────────────────
//! 各家上游的流式接口才是完整能力（非流式在部分上游上是「聚合后再返回」的
//! 兼容层）。所以即使下游要非流式，我们也以 `stream:true` 请求上游，
//! 收完后聚合成本协议的 JSON —— 与 `/v1/chat/completions` 的非流式路径同一做法。
//! 为此，送给转发层的 body 里 `stream` 恒为 true，而「下游要不要流式」
//! 由各 handler 自己记住（`ForwardRequest.stream` 传的仍是下游的意愿，
//! 因为它还决定上游聚合/透传的分支，见 `provider_loop` 的 `ctx.stream`）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：零 unwrap/expect/panic。

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Extension, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use serde_json::{json, Value};

use crate::server::core::key_scope::KeyScope;
use crate::server::core::protocol::{anthropic, responses};
use crate::server::core::upstream::usage::RequestTelemetry;
use crate::server::core::upstream::{ForwardOutcome, ForwardRequest};
use crate::server::errors::GatewayError;
use crate::server::logging;
use crate::server::ServerState;

use super::pipeline::{
    self, error_response, json_response, record_early_failure, sse_response, RecordContext,
};

/// 把请求体解析成 JSON 对象（失败给 400 + 记账）
fn parse_object(
    state: &ServerState,
    started_at: i64,
    path: &str,
    body: &Bytes,
) -> Result<Value, Response> {
    let parsed = serde_json::from_slice::<Value>(body).ok();
    let Some(payload) = parsed.filter(Value::is_object) else {
        let error = GatewayError::bad_request("请求体必须是 JSON 对象");
        record_early_failure(state, started_at, "", &error);
        logging::verbose("[Model]", &format!("← POST {path} 请求体不是 JSON 对象"));
        return Err(error.payload_response());
    };
    Ok(payload)
}

/// 一次转发的公共前半段：解析模型 → 送转发层。
///
/// 返回 `(upstream_body, outcome, telemetry, model)`。
/// `upstream_body` 是**送给转发层**的 Chat 形态请求体（已填好模型）。
///
/// `scope` 是本次请求命中的网关 Key 的限制（R9，`None` = 不限制）：
/// **提供商**那一半进转发层（选路时按承载家过滤），**模型**那一半在调用方
/// 的 `pipeline::resolve_model` 里已经判过（见那里的说明 —— 两者分工不同：
/// 模型是入口校验，提供商是选路过滤）。
async fn forward_chat(
    state: &ServerState,
    payload: Value,
    client_headers: &HeaderMap,
    stream: bool,
    dedupe_key: String,
    scope: Option<KeyScope>,
) -> (Arc<RequestTelemetry>, Result<ForwardOutcome, GatewayError>) {
    let telemetry = Arc::new(RequestTelemetry::with_id());
    let outcome = state
        .upstream()
        .forward(ForwardRequest {
            body: payload,
            stream,
            dedupe_key,
            client_headers: client_headers.clone(),
            telemetry: telemetry.clone(),
            allowed_providers: scope,
        })
        .await;
    (telemetry, outcome)
}

// ─── POST /v1/responses ─────────────────────────────────────

/// POST /v1/responses
pub async fn responses_endpoint(
    State(state): State<ServerState>,
    headers: HeaderMap,
    // R9：中间件放进请求扩展的「命中 Key 的限制」（`None` = 不限制，见
    // `core::key_scope` 模块头）。用 `Option<Extension<_>>` 而不是裸
    // `Extension<_>` —— 后者在免鉴权模式下取不到会直接拒绝请求。
    key_scope: Option<Extension<KeyScope>>,
    body: Bytes,
) -> Response {
    let scope = key_scope.map(|Extension(scope)| scope);
    let started_at = logging::now_ms();
    let path = "/v1/responses";
    let raw = match parse_object(&state, started_at, path, &body) {
        Ok(value) => value,
        Err(response) => return response,
    };
    // 原始请求体留一份：回程要用它回显请求侧字段（instructions / tools / …），
    // 而下面会把 payload 改写成 Chat 形态
    let original = raw.clone();
    let stream = raw.get("stream").and_then(Value::as_bool).unwrap_or(false);

    // 协议翻译（有状态字段在这里被拒）
    let mut chat_body = match responses::chat_from_responses(&raw) {
        Ok(body) => body,
        Err(message) => {
            let error = GatewayError::bad_request(message);
            record_early_failure(&state, started_at, &pipeline::model_field_text(&raw), &error);
            return error.payload_response();
        }
    };
    if !chat_body
        .get("messages")
        .map(Value::is_array)
        .unwrap_or(false)
    {
        let error = GatewayError::bad_request("input 必须能转换成对话消息");
        record_early_failure(&state, started_at, "", &error);
        return error.payload_response();
    }

    // 模型解析走公共管道（默认模型回落 / 别名映射 / 目录校验）。
    // 第三参是这把 Key 的可用模型白名单（None = 不限制），同 /v1/chat/completions
    let requested_model = match pipeline::resolve_model(&state, &mut chat_body, scope.as_ref()) {
        Ok(model) => model,
        Err(error) => {
            record_early_failure(&state, started_at, &pipeline::model_field_text(&raw), &error);
            return error.payload_response();
        }
    };
    let user_agent = pipeline::user_agent_of(&headers);
    logging::verbose(
        "[Model]",
        &format!(
            "← POST {path} model={} stream={stream} bytes={} ua={user_agent}",
            if requested_model.is_empty() { "(未指定)" } else { &requested_model },
            body.len(),
        ),
    );
    pipeline::write_debug_files(&body, "POST", path, &user_agent);

    // 上游恒走流式（理由见模块头）
    // scope 的**提供商**那一半进转发层（选路时按承载家过滤）；
    // 模型那一半已在上面判过（分工见 forward_chat 的说明）
    let (telemetry, outcome) =
        forward_chat(&state, chat_body, &headers, stream, pipeline::sha256_hex(&body), scope).await;
    let context = RecordContext {
        stats: state.request_stats(),
        telemetry,
        started_at,
        model: requested_model.clone(),
        // 下游原始名取自客户端原始请求体（转换前的 model 字段）
        client_model: pipeline::model_field_text(&raw),
        status: 200,
    };

    match outcome {
        Ok(ForwardOutcome::Stream { status, stream: source }) => {
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
            let model = requested_model.clone();
            let request = original.clone();
            // 状态码进记账上下文（明细里记的必须是客户端实际看到的那个码）
            let context = RecordContext { status: i64::from(status.as_u16()), ..context };
            if stream {
                let mut machine = responses::ResponsesStream::new(&model, &request);
                let transformed = pipeline::transformed_stream(
                    source,
                    context,
                    pipeline::terminal::RESPONSES,
                    move |chunk| machine.push(chunk),
                );
                return sse_response(status, transformed);
            }
            // 下游要非流式：内部收流，聚合成本协议的 JSON
            let mut collector = responses::ResponsesCollector::new();
            collect_stream(source, &mut |chunk| collector.push(chunk)).await;
            collector.finish();
            let body = collector.into_response(&model, &request);
            pipeline::record_entry(&context, None);
            json_response(body)
        }
        Ok(ForwardOutcome::Completion { body: chat }) => {
            // 上游走了聚合路径（本不该发生：我们恒要流式，但 CatPaw 等
            // 有状态适配器可能直接给 Completion）。照样翻译，不丢请求。
            let response = responses::responses_from_chat(&chat, &requested_model, &original);
            pipeline::record_entry(&context, None);
            json_response(response)
        }
        Err(error) => {
            let message = error.message.clone();
            // 只在终端：`error_response` 会把同一条 message 记进请求日志
            // （见 `api::chat` 同名分支的说明）。
            logging::console_line("[Model]", &format!("❌ {message}"));
            error_response(context, &error, |status, error| {
                let code = anthropic::responses_error_code(status);
                GatewayError::with_status(i32::from(status), error.message.clone())
                    .with_code(code)
                    .payload_response()
            })
        }
    }
}

// ─── POST /v1/messages ──────────────────────────────────────

/// POST /v1/messages
pub async fn messages_endpoint(
    State(state): State<ServerState>,
    headers: HeaderMap,
    // R9：同 responses_endpoint（`Option` + 取不到 = 不限制）
    key_scope: Option<Extension<KeyScope>>,
    body: Bytes,
) -> Response {
    let scope = key_scope.map(|Extension(scope)| scope);
    let started_at = logging::now_ms();
    let path = "/v1/messages";
    let raw = match parse_object(&state, started_at, path, &body) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let stream = raw.get("stream").and_then(Value::as_bool).unwrap_or(false);

    // 协议翻译
    let mut chat_body = match anthropic::chat_from_anthropic(&raw) {
        Ok(body) => body,
        Err(message) => {
            let error = GatewayError::bad_request(message);
            record_early_failure(&state, started_at, &pipeline::model_field_text(&raw), &error);
            return anthropic_error_response(&error);
        }
    };
    // 第三参是这把 Key 的可用模型白名单（None = 不限制），与另两条入口同源
    let requested_model = match pipeline::resolve_model(&state, &mut chat_body, scope.as_ref()) {
        Ok(model) => model,
        Err(error) => {
            record_early_failure(&state, started_at, &pipeline::model_field_text(&raw), &error);
            return anthropic_error_response(&error);
        }
    };
    let user_agent = pipeline::user_agent_of(&headers);
    logging::verbose(
        "[Model]",
        &format!(
            "← POST {path} model={} stream={stream} bytes={} ua={user_agent}",
            if requested_model.is_empty() { "(未指定)" } else { &requested_model },
            body.len(),
        ),
    );
    pipeline::write_debug_files(&body, "POST", path, &user_agent);

    // scope 的**提供商**那一半进转发层（选路时按承载家过滤）；
    // 模型那一半已在上面判过（分工见 forward_chat 的说明）
    let (telemetry, outcome) =
        forward_chat(&state, chat_body, &headers, stream, pipeline::sha256_hex(&body), scope).await;
    let context = RecordContext {
        stats: state.request_stats(),
        telemetry,
        started_at,
        model: requested_model.clone(),
        // 下游原始名取自客户端原始请求体（转换前的 model 字段）
        client_model: pipeline::model_field_text(&raw),
        status: 200,
    };

    match outcome {
        Ok(ForwardOutcome::Stream { status, stream: source }) => {
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
            let model = requested_model.clone();
            let context = RecordContext { status: i64::from(status.as_u16()), ..context };
            if stream {
                let mut machine = anthropic::AnthropicStream::new(&model);
                let transformed = pipeline::transformed_stream(
                    source,
                    context,
                    pipeline::terminal::ANTHROPIC,
                    move |chunk| machine.push(chunk),
                );
                return sse_response(status, transformed);
            }
            let mut collector = anthropic::AnthropicCollector::new();
            collect_stream(source, &mut |chunk| collector.push(chunk)).await;
            collector.finish();
            let body = collector.into_response(&model);
            pipeline::record_entry(&context, None);
            json_response(body)
        }
        Ok(ForwardOutcome::Completion { body: chat }) => {
            let response = anthropic::anthropic_from_chat(&chat, &requested_model);
            pipeline::record_entry(&context, None);
            json_response(response)
        }
        Err(error) => {
            let message = error.message.clone();
            // 只在终端：`error_response` 会把同一条 message 记进请求日志
            // （见 `api::chat` 同名分支的说明）。
            logging::console_line("[Model]", &format!("❌ {message}"));
            error_response(context, &error, |status, error| {
                anthropic_error_response_with_status(status, &error.message)
            })
        }
    }
}

/// 把一条转发流收干（非流式聚合用）。
///
/// 上游流已经过 `ReasoningCoalescer`，所以这里拿到的字节就是标准 Chat SSE。
async fn collect_stream(
    mut source: Box<dyn futures::Stream<Item = Result<Bytes, std::io::Error>> + Send + Unpin>,
    sink: &mut impl FnMut(&[u8]),
) {
    use futures::StreamExt;
    while let Some(item) = source.next().await {
        match item {
            Ok(chunk) => sink(&chunk),
            // 上游中断：已收的内容保留（聚合层会把已有的部分成形），
            // 与流式路径「不丢用户已经看到的部分」同一取向
            Err(error) => {
                logging::verbose("[Model]", &format!("聚合时上游中断: {error}"));
                break;
            }
        }
    }
}

/// Anthropic 形态的错误响应（`{type:"error", error:{type, message}}`）
fn anthropic_error_response(error: &GatewayError) -> Response {
    anthropic_error_response_with_status(error.http_status().as_u16(), &error.message)
}

fn anthropic_error_response_with_status(status: u16, message: &str) -> Response {
    let kind = anthropic::error_kind_for_status(status);
    let body = json!({
        "type": "error",
        "error": { "type": kind, "message": message },
    });
    let code = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut response = json_response(body);
    *response.status_mut() = code;
    response
}
