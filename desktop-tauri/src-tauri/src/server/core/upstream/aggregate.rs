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

use std::collections::BTreeMap;

use futures::StreamExt;
use serde_json::{json, Map, Value};

use crate::server::errors::GatewayError;

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
pub async fn aggregate_sse_completion(
    response: reqwest::Response,
) -> Result<AggregatedCompletion, GatewayError> {
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut acc = CompletionAccumulator::default();

    while let Some(item) = stream.next().await {
        let chunk = item.map_err(|error| {
            GatewayError::with_status(
                502,
                format!("上游流中断: {}", crate::server::core::egress::describe_error_detail(&error)),
            )
        })?;
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        // 逐行消费（只处理到最后一个 '\n' 之前的内容）
        while let Some(index) = buffer.find('\n') {
            let line = buffer[..index].trim().to_string();
            buffer.drain(..=index);
            if line.is_empty() {
                continue;
            }
            acc.consume_line(&line)?;
        }
        if buffer.len() > MAX_LINE_BYTES {
            return Err(GatewayError::with_status(502, "上游返回的单行数据过大，已中断"));
        }
    }
    // 尾行（上游没以换行结尾）
    let tail = buffer.trim().to_string();
    if !tail.is_empty() {
        acc.consume_line(&tail)?;
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
}

impl CompletionAccumulator {
    /// 处理一行 SSE（`data: {...}` / `data: [DONE]` / 其他）
    fn consume_line(&mut self, line: &str) -> Result<(), GatewayError> {
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
        self.consume_chunk(&chunk)
    }

    fn consume_chunk(&mut self, chunk: &Value) -> Result<(), GatewayError> {
        let Some(object) = chunk.as_object() else {
            return Ok(());
        };
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
        body.insert("model".to_string(), Value::String(self.model));
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
