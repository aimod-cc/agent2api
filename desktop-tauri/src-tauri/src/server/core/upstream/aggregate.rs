//! 上游 SSE 流聚合（对照 Node 版 aggregateSseCompletion，600-670 行）。
//!
//! 上游**只支持流式**（`stream:false` 返回 code=11101），所以客户端要非流式时，
//! 代理内部仍以 `stream:true` 请求上游，再把 SSE 帧聚合成完整的
//! OpenAI `chat.completion` 结构返回。
//!
//! 聚合规则逐条对照 Node：
//!   - 按行切 `data:`，`[DONE]` 与空行跳过
//!   - `delta.content` / `delta.reasoning_content` 字符串拼接
//!   - `delta.tool_calls` 按 `index` 合并（id/type 覆盖、function.name 与
//!     function.arguments 追加）
//!   - `usage` 取最后一次出现（上游在末尾 chunk 下发）
//!   - `finish_reason` 取最后一次非空
//!   - 帧里的 `error` 字段 → 抛 502（上游把错误写在流里）
//!   - id/created 缺失时给 `wb-agg-<毫秒>` / 当前秒兜底
//!
//! ── usage 旁路提取（请求统计）────────────────────────────────
//! `consume_chunk` 里顺手把 `usage` 抄进 `usage::RequestTelemetry`。
//! **不影响响应体**：决定下发 `usage` 字段的是 `self.usage`（本文件的
//! 既有逻辑），旁路只写另一个结构体，两者互不干扰。

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::StreamExt;
use serde_json::{json, Map, Value};

use crate::server::errors::GatewayError;

use super::sse::ModelRewrite;
use super::usage::RequestTelemetry;

/// 流式读取的上限保护：单条 SSE 行长度（解析失败的行会原样丢掉，
/// 但不能让一条畸形行把内存吃满）
const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

/// 聚合后的完整响应 + 调试用的 chunk 计数
pub struct AggregatedCompletion {
    pub body: Value,
    pub chunk_count: usize,
}

/// 把上游响应体（SSE 字节流）聚合成一个完整 chat.completion。
///
/// `on_chunk` 参数已去掉：Node 里 `controller.signal.aborted` 检查的作用是
/// 「客户端断开时停止聚合」，Rust 侧等价物是 caller 在 select! 里取消整个
/// future（见 forward.rs）—— 无需在循环里重复检查。
///
/// `telemetry` 是 usage 旁路槽（`Arc` 而非引用：聚合要在 await 之间持用，
/// 而转发链路本身是异步的）。它只被写入、不参与 `acc` 的构建 ——
/// **不影响返回的 body**（usage 的透传口径由 aggregate 自己的
/// `self.usage` 负责，与这里无关，两条路径互不干扰）。
///
/// `model_rewrite` 与流式分支同源（适配器的 `sse_model_rewrite()`）：
/// **非流式响应体里的 `model` 同样要回写**成客户端请求的名字 ——
/// 客户端拿到的 model 名不该因为「要不要流式」而变样（源实现
/// `forwardChatCompletions` 的非流式分支也是 `payload.model = requestedModel`）。
pub async fn aggregate_sse_completion(
    response: reqwest::Response,
    telemetry: Arc<RequestTelemetry>,
    model_rewrite: Option<ModelRewrite>,
) -> Result<AggregatedCompletion, GatewayError> {
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut acc = CompletionAccumulator { rewrite: model_rewrite, ..Default::default() };
    // 首响采集：聚合路径不走 RecordingStream（客户端要的是完整 JSON，
    // 没有下发流可言），所以第一个**上游** chunk 在这里记 —— 它就是
    // 「上游开始吐内容」的时刻。非流式请求的用时要等聚合完才有意义，
    // 首响补上了「上游是快是慢」这半边（与流式路径同口径：首次为准）。
    let mut first_chunk_seen = false;
    while let Some(item) = stream.next().await {
        let chunk = item.map_err(|error| {
            GatewayError::with_status(
                502,
                format!("上游流中断: {}", crate::server::core::egress::describe_error_detail(&error)),
            )
        })?;
        if !first_chunk_seen {
            first_chunk_seen = true;
            telemetry.note_first_frame();
        }
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        // 逐行消费（只处理到最后一个 '\n' 之前的内容）
        while let Some(index) = buffer.find('\n') {
            let line = buffer[..index].trim().to_string();
            buffer.drain(..=index);
            if line.is_empty() {
                continue;
            }
            acc.consume_line(&line, &telemetry)?;
        }
        if buffer.len() > MAX_LINE_BYTES {
            return Err(GatewayError::with_status(502, "上游返回的单行数据过大，已中断"));
        }
    }
    // 尾行（上游没以换行结尾）
    let tail = buffer.trim().to_string();
    if !tail.is_empty() {
        acc.consume_line(&tail, &telemetry)?;
    }

    Ok(acc.into_completion())
}

/// 聚合状态（Node 的局部变量集中到这里，便于 `consume_line` 返回 Result）
#[derive(Default)]
struct CompletionAccumulator {
    id: String,
    model: String,
    created: i64,
    role: String,
    content: String,
    reasoning: String,
    finish: String,
    usage: Option<Value>,
    chunk_count: usize,
    tool_calls: BTreeMap<i64, Value>,
    /// model 名回写参数（None = 不改写，沿用上游给的 model）
    rewrite: Option<ModelRewrite>,
}

impl CompletionAccumulator {
    /// 处理一行 SSE（`data: {...}` / `data: [DONE]` / 其他）
    fn consume_line(&mut self, line: &str, telemetry: &RequestTelemetry) -> Result<(), GatewayError> {
        let Some(data) = line.strip_prefix("data:") else {
            return Ok(());
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            return Ok(());
        }
        // Node: `try { handleChunk(JSON.parse(line)) } catch { /* 非 JSON 行忽略 */ }`
        // —— 非 JSON 行静默忽略，但 handleChunk 抛出的 WorkBuddyUpstreamError 要透出
        let Ok(chunk) = serde_json::from_str::<Value>(data) else {
            return Ok(());
        };
        self.consume_chunk(&chunk, telemetry)
    }

    fn consume_chunk(&mut self, chunk: &Value, telemetry: &RequestTelemetry) -> Result<(), GatewayError> {
        let Some(object) = chunk.as_object() else {
            return Ok(());
        };
        // ── usage 旁路提取（请求统计）──────────────────────────────
        // 放在错误判定**之前**：上游把错误写在流里时也可能带上已消耗的
        // usage（提示词已计费），旁路先抄一份不影响下面照常抛 502。
        // 为什么这里的失败同样是「无副作用」的：只读 `chunk` 的一个成员，
        // 不碰 `self.usage`（那才是决定响应体 usage 字段的那份），
        // 所以返回值与不接钩子时完全一致。
        if let Some(usage) = object.get("usage") {
            telemetry.report_usage(usage);
        }
        if let Some(error) = object.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .unwrap_or("上游流式返回错误");
            return Err(GatewayError::with_status(502, message.to_string()));
        }
        self.chunk_count += 1;
        if let Some(id) = object.get("id").and_then(Value::as_str) {
            if !id.is_empty() {
                self.id = id.to_string();
            }
        }
        if let Some(model) = object.get("model").and_then(Value::as_str) {
            self.model = model.to_string();
        }
        // Node: `created = chunk.created || created` —— 真值判定（0 不覆盖已有值）
        if let Some(created) = object.get("created") {
            if js_number_truthy(created) {
                self.created = created.as_i64().or_else(|| created.as_f64().map(|value| value as i64)).unwrap_or(0);
            }
        }
        if let Some(usage) = object.get("usage") {
            if usage.is_object() {
                self.usage = Some(usage.clone());
            }
        }

        let Some(choice) = object
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        else {
            return Ok(());
        };
        let delta = choice.get("delta").cloned().unwrap_or(json!({}));
        if let Some(role) = delta.get("role").and_then(Value::as_str) {
            if !role.is_empty() {
                self.role = role.to_string();
            }
        }
        if let Some(content) = delta.get("content").and_then(Value::as_str) {
            self.content.push_str(content);
        }
        if let Some(reasoning) = delta.get("reasoning_content").and_then(Value::as_str) {
            self.reasoning.push_str(reasoning);
        }
        if let Some(finish) = choice.get("finish_reason").and_then(Value::as_str) {
            if !finish.is_empty() {
                self.finish = finish.to_string();
            }
        }
        if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in tool_calls {
                let index = call
                    .get("index")
                    .and_then(Value::as_i64)
                    // Node: `Number.isInteger(tc.index) ? tc.index : 0`
                    .unwrap_or(0);
                let entry = self
                    .tool_calls
                    .entry(index)
                    .or_insert_with(|| {
                        json!({
                            "id": "",
                            "type": "function",
                            "function": { "name": "", "arguments": "" },
                        })
                    });
                merge_tool_call(entry, call);
            }
        }
        Ok(())
    }

    /// 组装最终 body（对应 Node 的 handleChunk 结束后的返回对象）
    fn into_completion(self) -> AggregatedCompletion {
        let mut message = Map::new();
        message.insert(
            "role".to_string(),
            Value::String(if self.role.is_empty() { "assistant".to_string() } else { self.role }),
        );
        message.insert("content".to_string(), Value::String(self.content));
        if !self.reasoning.is_empty() {
            message.insert("reasoning_content".to_string(), Value::String(self.reasoning));
        }
        if !self.tool_calls.is_empty() {
            message.insert(
                "tool_calls".to_string(),
                Value::Array(self.tool_calls.into_values().collect()),
            );
        }
        let mut body = Map::new();
        body.insert(
            "id".to_string(),
            Value::String(if self.id.is_empty() {
                format!("wb-agg-{}", crate::server::logging::now_ms())
            } else {
                self.id
            }),
        );
        body.insert("object".to_string(), Value::String("chat.completion".to_string()));
        body.insert(
            "created".to_string(),
            Value::from(if self.created != 0 {
                self.created
            } else {
                crate::server::logging::now_ms() / 1000
            }),
        );
        // model 名回写（见 `aggregate_sse_completion` 的说明）：配置了回写时
        // 用客户端请求的名字，否则沿用上游给的（可能为空串 —— 与改造前一致）
        let model = match &self.rewrite {
            Some(rewrite) => rewrite.requested.clone(),
            None => self.model,
        };
        body.insert("model".to_string(), Value::String(model));
        body.insert(
            "choices".to_string(),
            json!([{
                "index": 0,
                "message": Value::Object(message),
                "finish_reason": if self.finish.is_empty() { "stop".to_string() } else { self.finish },
            }]),
        );
        // Node 的返回对象里 `usage` 是**始终存在**的键：上游没下发时是 null
        // （JSON.stringify 会保留 null，而不是丢掉这个键）——
        // OpenAI SDK 对 `usage: null` 与缺键的处理不完全一样，照抄更稳。
        body.insert(
            "usage".to_string(),
            self.usage.unwrap_or(Value::Null),
        );
        AggregatedCompletion { body: Value::Object(body), chunk_count: self.chunk_count }
    }
}

/// 合并一个 tool_call 增量到已累积的条目上（id/type 覆盖、name/arguments 追加）
fn merge_tool_call(entry: &mut Value, incoming: &Value) {
    let Some(target) = entry.as_object_mut() else {
        return;
    };
    if let Some(id) = incoming.get("id").and_then(Value::as_str) {
        if !id.is_empty() {
            target.insert("id".to_string(), Value::String(id.to_string()));
        }
    }
    if let Some(kind) = incoming.get("type").and_then(Value::as_str) {
        if !kind.is_empty() {
            target.insert("type".to_string(), Value::String(kind.to_string()));
        }
    }
    let Some(function) = incoming.get("function") else {
        return;
    };
    let Some(target_function) = target
        .get_mut("function")
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    if let Some(name) = function.get("name").and_then(Value::as_str) {
        let current = target_function
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        target_function.insert("name".to_string(), Value::String(format!("{current}{name}")));
    }
    if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
        let current = target_function
            .get("arguments")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        target_function.insert(
            "arguments".to_string(),
            Value::String(format!("{current}{arguments}")),
        );
    }
}

/// 真值判定（Node 的 `chunk.created || created`）
fn js_number_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().map(|item| item != 0.0).unwrap_or(false),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}
