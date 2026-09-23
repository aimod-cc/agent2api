//! Qoder 的上游流消费管道：流式首帧预读 + 流式透传（issue #8 的修复）。
//!
//! ── 职责与拆分（单文件行数约定）────────────────────────────
//! `drive_stream`（流式透传）与首帧预读（`prefetch_stream_head`）在转发热
//! 路径上强耦合：预读消费掉的半行要交给续传、预读装上的空闲守卫要一路带
//! 到透传、两者共用同一套 `parse_sse_line` 解析与 `LimitContext` 记账 ——
//! 集中一个文件才好对照着改。非流式聚合（`drive_aggregate`）留在 `mod.rs`：
//! 它不参与预读移交。
//!
//! ── 首帧预读（issue #8 的修复）────────────────────────────
//! Qoder 的业务错误不体现在 HTTP 状态码上（恒 200），只写在 SSE 信封的
//! `statusCodeValue` 里。改造前流式分支拿到 200 就返回 `Ok(Stream)`，编排层
//! 的账号循环随即退出 —— 之后 `drive_stream` 读到的信封错误只能落冷却标记
//! 并把错误帧发给客户端，「换下一个账号重试」永远轮不到（账号页明明显示
//! 已进冷却，客户端却直接收到额度不足）。
//!
//! `prefetch_stream_head` 把首个事件拦在返回之前：是业务错误就转成带状态码
//! 的 `Err` 交回编排层 —— 此刻还没有任何字节下发给客户端，HTTP 状态头也没
//! 发出，换号无损（与非流式路径 `drive_aggregate` 同一语义）；拿到内容帧才
//! 返回 `Ok(Stream)`，预读到的帧由 `drive_stream` 开头补发，后续字节无缝续传。
//!
//! ── 空闲保护 ───────────────────────────────────────────────
//! 预读装上的 `stall::idle_guard` 一路带到 `drive_stream`：首帧等待与后续
//! 分片共享设置页的「流式响应空闲超时」。改造前这条管道没有任何空闲保护
//! （通用层 `ForwardStream::new` 有，Qoder 自己的管道漏了），上游 200 后
//! 僵死会让请求一直挂到传输层后备超时才收场。

use futures::stream::BoxStream;
use futures::StreamExt;
use serde_json::Value;

use crate::server::core::egress;
use crate::server::core::upstream::stall::{self, IDLE_TIMEOUT_PREFIX};
use crate::server::core::upstream::usage::RequestTelemetry;
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::chat::Translator;
use super::stream::{self, SseEvent};
use super::{chat, record_limited, LimitContext};

/// 流式首帧预读：把上游拉到**第一个内容帧**（或流结束）才决定返回什么。
///
/// 返回 `(预读到的内容帧, 剩余字节流)`。剩余流已装上空闲守卫、reqwest 错误
/// 已折进 `io::Error`、半行缓冲已拼回流头 —— `drive_stream` 接手后按既有
/// 语义续传。额度/鉴权类**首帧前**错误的去向见模块头。
pub(super) async fn prefetch_stream_head(
    response: reqwest::Response,
    limit: &LimitContext,
    telemetry: &std::sync::Arc<RequestTelemetry>,
) -> Result<
    (Vec<Value>, BoxStream<'static, Result<bytes::Bytes, std::io::Error>>),
    GatewayError,
> {
    // 传输层错误先折进 io::Error（文案与通用层 `ForwardStream::new` 同一出处）；
    // 空闲守卫从这里武装，随剩余流一路带给 `drive_stream`
    let source = response.bytes_stream().map(|item| {
        item.map_err(|error| std::io::Error::other(egress::describe_error_detail(&error)))
    });
    let mut source = stall::idle_guard(
        Box::pin(source),
        std::time::Duration::from_millis(
            crate::server::config::timeout_settings().stream_idle_ms(),
        ),
    );
    let capture = telemetry.capture();
    let mut lines = stream::LineBuffer::new();
    let mut prefetched: Vec<Value> = Vec::new();
    loop {
        let chunk = match source.next().await {
            Some(Ok(chunk)) => chunk,
            // 首帧之前的传输失败（中断 / 空闲超时）同样交回编排层：还没下发
            // 任何字节、客户端的 200 头也没发出 —— 换下一个账号再试。文案
            // 与 `drive_aggregate` 的传输中断同一出处。
            Some(Err(error)) => {
                return Err(GatewayError::with_status(
                    502,
                    format!("Qoder 上游流中断: {error}"),
                ));
            }
            None => break,
        };
        // 调试模式：预读消费掉的字节也要旁路给采集器 —— drive_stream 续用
        // 同一个实例（`telemetry.capture()` 返回同一 Arc），字节不丢
        if let Some(capture) = capture.as_deref() {
            capture.push(&chunk);
        }
        let mut got_content = false;
        for data in lines.push(&chunk) {
            match stream::parse_sse_line(&data) {
                SseEvent::Skip => {}
                // 流直接收尾（无内容无错误）：剩余流原样交给 drive_stream，
                // 由它走正常收尾（finish 帧 + [DONE]）
                SseEvent::Done => return Ok((prefetched, with_pending_tail(source, lines))),
                // 首帧之前的业务错误：冷却落库（与非流式同一落点）+ Err 交回
                // 编排层换号。任何分类都返回（Unknown 的 502 也一样），与
                // `drive_aggregate` 的口径一致
                SseEvent::Error { status, kind, raw, message, pricing_url, .. } => {
                    record_limited(
                        &limit.store,
                        &limit.account_id,
                        &limit.model,
                        status,
                        kind,
                        &message,
                    );
                    return Err(chat::business_error(
                        status,
                        kind,
                        &raw,
                        &message,
                        pricing_url.as_deref(),
                    ));
                }
                SseEvent::Chunk(chunk) => {
                    prefetched.push(chunk);
                    got_content = true;
                }
            }
        }
        // 「这一批字节里见到过内容帧」才停：一个 HTTP 分片可能拆出多行，
        // 后面的行还在这批里 —— 继续解析，直到出内容 / 流结束才回到外层。
        // 一个分片都没出内容就继续拉下一片（[DONE] 与空批次都走这里）
        if got_content {
            break;
        }
    }
    Ok((prefetched, with_pending_tail(source, lines)))
}

/// 把未凑成完整行的缓冲字节拼回流头（见 `LineBuffer::take_tail` 的说明）。
///
/// 空 tail 直接原样返回流（常规情况零开销）；非空时插一个人工分片，
/// 后续解析无缝续上。
fn with_pending_tail(
    source: BoxStream<'static, Result<bytes::Bytes, std::io::Error>>,
    lines: stream::LineBuffer,
) -> BoxStream<'static, Result<bytes::Bytes, std::io::Error>> {
    let mut lines = lines;
    let tail = lines.take_tail();
    if tail.is_empty() {
        source
    } else {
        Box::pin(futures::stream::iter([Ok(bytes::Bytes::from(tail))]).chain(source))
    }
}

/// 一个内层 chunk → 若干 OpenAI delta 帧写进通道（流式下发的唯一入口）。
///
/// 返回 `false` 表示客户端已断开（发送失败），调用方应立即结束循环。
/// 「首个产出前补 role 帧」「空 delta 不发」的规则都在这里：预读补发与
/// 主循环续传共用同一套，两条路径的帧序不会漂移。
async fn emit_chunk(
    sender: &tokio::sync::mpsc::Sender<Result<bytes::Bytes, std::io::Error>>,
    translator: &mut Translator,
    telemetry: &std::sync::Arc<RequestTelemetry>,
    chunk: &Value,
) -> bool {
    let deltas = translator.consume(chunk, Some(telemetry));
    if deltas.is_empty() {
        return true;
    }
    // 第一次产出内容前补一帧 role（OpenAI 的既有形态）
    if translator.take_role_frame() {
        let frame = translator.chunk_frame(
            serde_json::json!({ "role": "assistant", "content": "" }),
            None,
        );
        if send_frame(sender, &frame).await.is_err() {
            return false;
        }
    }
    for delta in &deltas {
        let frame = translator.chunk_frame(chat::delta_json(delta), None);
        if send_frame(sender, &frame).await.is_err() {
            return false;
        }
    }
    true
}

/// 流式：把上游字节 → OpenAI SSE 帧写进通道。
///
/// `prefetched` 是预读阶段已经解析出来的内容帧（见 `prefetch_stream_head`）：
/// 编排层拿到 `Ok(Stream)` 之后由本函数补发 —— 记账与下发走与主循环完全
/// 同一套（`emit_chunk`），帧序与「边读边发」没有差别。
///
/// 错误处理分两段（与通用层同一形态）：
///   - **首帧之前**的错误已在预读阶段转成 `Err` 交回编排层换号，走不到这里；
///   - 流开始后的中途错误：客户端可能已收到部分内容（HTTP 头早已发出），
///     换号不再可能 —— 只能补「错误帧 + [DONE]」收尾，同时写 telemetry
///     让请求日志能解释「为什么这条是失败的」。
pub(super) async fn drive_stream(
    mut source: BoxStream<'static, Result<bytes::Bytes, std::io::Error>>,
    mut translator: Translator,
    telemetry: std::sync::Arc<RequestTelemetry>,
    limit: LimitContext,
    sender: tokio::sync::mpsc::Sender<Result<bytes::Bytes, std::io::Error>>,
    prefetched: Vec<Value>,
) {
    let mut lines = stream::LineBuffer::new();
    let mut failed: Option<String> = None;
    // 调试模式的采集器：预读阶段旁路的是同一个实例，这里续上（解析之前采
    // 原始字节 —— 采的是上游原样吐出的内容）
    let capture = telemetry.capture();

    // 预读到的内容帧先补发（字节已在预读时旁路给采集器，这里只走翻译）
    for chunk in &prefetched {
        if !emit_chunk(&sender, &mut translator, &telemetry, chunk).await {
            return;
        }
    }

    'outer: while let Some(item) = source.next().await {
        let chunk = match item {
            Ok(chunk) => chunk,
            Err(error) => {
                // 上游流中断 / 空闲超时：**正常路径**（客户端断开、上游主动
                // 结束、保护性判定），不 panic。先把已累积的 reasoning 冲刷
                // 出去，再补「错误帧 + [DONE]」收尾（与通用层 ForwardStream
                // 一致）。空闲超时是保护性判定不是中断：文案不带「上游流
                // 中断」前缀（与通用层同一处理，见 stall 的模块头）
                let text = error.to_string();
                failed = Some(if text.starts_with(IDLE_TIMEOUT_PREFIX) {
                    text
                } else {
                    format!("上游流中断: {text}")
                });
                break;
            }
        };
        if let Some(capture) = capture.as_deref() {
            capture.push(&chunk);
        }
        for data in lines.push(&chunk) {
            match stream::parse_sse_line(&data) {
                SseEvent::Skip => {}
                SseEvent::Done => break 'outer,
                SseEvent::Error { status, kind, raw, message, pricing_url, .. } => {
                    // 上游的业务错误（HTTP 200 里的信封错误）：转成带状态码的
                    // 文案写进流，形态与通用层的收尾一致。额度类在这里落冷却
                    // （record_limited 自带「进入冷却」的运行日志）——但错误
                    // 本身只能随流下发给客户端：流已开始，换号不再可能
                    // （与首帧前错误的差别，见 prefetch_stream_head 的说明）
                    record_limited(
                        &limit.store,
                        &limit.account_id,
                        &limit.model,
                        status,
                        kind,
                        &message,
                    );
                    let error = chat::business_error(
                        status,
                        kind,
                        &raw,
                        &message,
                        pricing_url.as_deref(),
                    );
                    failed = Some(error.message);
                    break 'outer;
                }
                SseEvent::Chunk(chunk) => {
                    if !emit_chunk(&sender, &mut translator, &telemetry, &chunk).await {
                        return;
                    }
                }
            }
        }
    }
    // 尾行（上游没以换行收尾时）
    if failed.is_none() {
        for data in lines.finish() {
            match stream::parse_sse_line(&data) {
                SseEvent::Chunk(chunk) => {
                    if !emit_chunk(&sender, &mut translator, &telemetry, &chunk).await {
                        return;
                    }
                }
                SseEvent::Error { status, kind, raw, message, pricing_url, .. } => {
                    record_limited(
                        &limit.store,
                        &limit.account_id,
                        &limit.model,
                        status,
                        kind,
                        &message,
                    );
                    let error = chat::business_error(
                        status,
                        kind,
                        &raw,
                        &message,
                        pricing_url.as_deref(),
                    );
                    failed = Some(error.message);
                }
                _ => {}
            }
        }
    }

    if let Some(message) = failed {
        telemetry.note_error(&message);
        logging::log("[Qoder]", &format!("❌ {message}"));
        // 失败前先把拆解器缓冲里已确定的内容冲刷出去（用户已经看到的部分不丢）
        for delta in translator.finish() {
            let frame = translator.chunk_frame(chat::delta_json(&delta), None);
            if send_frame(&sender, &frame).await.is_err() {
                return;
            }
        }
        let _ = sender
            .send(Ok(bytes::Bytes::from(stream::sse_frame(
                &serde_json::json!({
                    "error": { "message": message, "type": "proxy_error" }
                }),
            ))))
            .await;
        let _ = sender
            .send(Ok(bytes::Bytes::from(stream::sse_done())))
            .await;
        return;
    }

    // 正常收尾：先把拆解器的缓冲冲刷出来，再发 finish 帧
    // （工具调用已按 delta 逐片下发，这里不重复补）
    for delta in translator.finish() {
        let frame = translator.chunk_frame(chat::delta_json(&delta), None);
        if send_frame(&sender, &frame).await.is_err() {
            return;
        }
    }
    let finish = translator.final_finish();
    let frame = translator.chunk_frame(serde_json::json!({}), Some(&finish));
    let _ = send_frame(&sender, &frame).await;
    if translator.usage.is_some() {
        let usage = translator.usage_frame();
        let _ = send_frame(&sender, &usage).await;
    }
    let _ = sender.send(Ok(bytes::Bytes::from(stream::sse_done()))).await;
}

/// 一帧 SSE 写进通道；客户端断开时返回 Err（由调用方结束循环）
async fn send_frame(
    sender: &tokio::sync::mpsc::Sender<Result<bytes::Bytes, std::io::Error>>,
    value: &Value,
) -> Result<(), ()> {
    let frame = stream::sse_frame(value);
    sender.send(Ok(bytes::Bytes::from(frame))).await.map_err(|_| ())
}
