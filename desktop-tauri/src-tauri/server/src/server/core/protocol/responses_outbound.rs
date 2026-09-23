//! Responses 协议的**出站**转换：chat 请求 → Responses 上游请求、
//! 上游 Responses SSE → chat SSE（自定义提供商转发的 `responses` 分支）。
//!
//! ── 与 `responses.rs` 的方向关系 ─────────────────────────────
//! `responses.rs` 服务的是**下游入口**（`api::protocol` 的 `/v1/responses`）：
//! 把客户端发来的 Responses 请求翻成 chat、把回程的 chat 响应/SSE 翻回
//! Responses。本文件是它的**反方向**，服务自定义提供商的**上游**：
//! 转发层手里的统一货币是 chat，但这家上游说 Responses，于是
//!   - 请求：`responses_request_from_chat`（chat 体 → Responses 体，逐字段
//!     对着 `chat_from_responses` 反推）；
//!   - 响应：`ChatFromResponsesStream`（上游 Responses SSE → 标准 chat SSE，
//!     之后照走既有的 `ForwardStream` / 聚合器 —— 那两层只认 chat 帧）。
//!
//! ── 上游恒以 `stream:true` 被请求 ────────────────────────────
//! 与 chat 协议同一条策略（各家上游的流式才是完整能力；非流式下游由网关
//! 收流聚合）。因此响应侧只需要流式转换器；非流式 JSON 路径不会有上游
//! 来源。事件流的折法参考 OmniProxy 的 `responsesStreamToChat`，字段口径
//! 按本项目 chat 侧（`responses.rs` 回程消费的形状）收敛。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! 同 `mod.rs`：零 unwrap/expect/panic；解析失败退化「跳过该帧」。

use std::collections::BTreeMap;

use serde_json::{json, Map, Value};

use super::{
    chat_frame, content_parts, content_text, is_truthy, json_number_of, json_text, random_id,
    string_field, string_value, SseLineBuffer,
};
use super::responses::{image_url_of, tool_output_text, ConvertError};
use super::tool_plan;

// ─── 请求：Chat → Responses ─────────────────────────────────

/// Chat Completions 请求体 → Responses 请求体。
///
/// ── 逐字段对着 [`super::responses::chat_from_responses`] 反推 ────
/// 那份转换把 Responses 请求折成 chat，字段映射全在里面；这里逐条走它的
/// 完整清单（漏一条的症状是「客户端参数静默失效」）：
///   - `model`（由调用方给上游真名，见 `providers::custom::forward`）
///   - `messages` → `instructions`（system 消息）+ `input`（message /
///     function_call / function_call_output 项）
///   - `stream` / `max_output_tokens`（← `max_completion_tokens` / `max_tokens`）
///   - `temperature` / `top_p` / `service_tier` / `parallel_tool_calls` /
///     `user` / `metadata`
///   - `tools`（chat 嵌套 function → Responses 扁平 function）
///   - `tool_choice`、`response_format` → `text.format`
///   - `reasoning_effort` → `reasoning:{effort, summary:"auto"}`
///   - `store: false`（本网关无状态；对上游同样不存）
///
/// 不支持的 chat 字段（`n` / `logprobs` 等）**原样丢弃**：Responses 没有
/// 对应形态，发送上游不认识的键只会换来一次 400。
pub fn responses_request_from_chat(chat: &Value, model: &str) -> Result<Value, ConvertError> {
    let Some(messages) = chat.get("messages").and_then(Value::as_array) else {
        return Err("缺少 messages 数组".to_string());
    };
    let mut instructions: Vec<String> = Vec::new();
    let mut items: Vec<Value> = Vec::new();
    for message in messages {
        let role = {
            let raw = string_field(message, "role").to_lowercase();
            if raw == "developer" {
                "system".to_string()
            } else if raw.is_empty() {
                "user".to_string()
            } else {
                raw
            }
        };
        match role.as_str() {
            // system / developer → 顶层 instructions（多条用空行拼接，
            // 与 anthropic 侧 system_text 的拼接口径一致）
            "system" => {
                let text = content_text(message.get("content").unwrap_or(&Value::Null));
                if !text.trim().is_empty() {
                    instructions.push(text);
                }
            }
            // tool 结果 → function_call_output 项（call_id 是配对的钥匙，
            // 缺了它上游在多轮里认不出这条结果属于哪次调用）
            "tool" => {
                items.push(json!({
                    "type": "function_call_output",
                    "call_id": string_field(message, "tool_call_id"),
                    "output": tool_output_text(message.get("content")),
                }));
            }
            "assistant" => push_assistant_items(&mut items, message),
            // user 与「认不出的角色」都当 user（与 chat_from_responses 的
            // 兜底同一取向：能跑比报错好）
            _ => {
                let content =
                    chat_content_to_responses(message.get("content").unwrap_or(&Value::Null), "user");
                items.push(json!({ "type": "message", "role": "user", "content": content }));
            }
        }
    }

    let mut out = Map::new();
    out.insert("model".to_string(), Value::String(model.to_string()));
    out.insert("input".to_string(), Value::Array(items));
    out.insert(
        "stream".to_string(),
        Value::Bool(chat.get("stream").and_then(Value::as_bool).unwrap_or(false)),
    );
    if !instructions.is_empty() {
        out.insert(
            "instructions".to_string(),
            Value::String(instructions.join("\n\n")),
        );
    }
    // max_output_tokens ← max_completion_tokens（Responses 口径的字段名），
    // 退到 max_tokens；非正数视为没给（0 / 负数发上去只会被上游拒）
    let max_output = ["max_completion_tokens", "max_tokens"]
        .iter()
        .find_map(|key| chat.get(*key).and_then(Value::as_i64).filter(|value| *value > 0));
    if let Some(value) = max_output {
        out.insert("max_output_tokens".to_string(), Value::from(value));
    }
    for key in ["temperature", "top_p", "service_tier", "parallel_tool_calls", "user", "metadata"] {
        if let Some(value) = chat.get(key).filter(|value| !value.is_null()) {
            out.insert(key.to_string(), value.clone());
        }
    }
    // 工具声明：chat 只会有嵌套 function 形态（Responses 原生工具在
    // chat 侧没有表达，不会出现在这里）；转不出来的条目丢弃并留下痕迹
    if let Some(tools) = chat.get("tools").and_then(Value::as_array) {
        let converted: Vec<Value> = tools.iter().filter_map(tool_to_responses).collect();
        if !converted.is_empty() {
            out.insert("tools".to_string(), Value::Array(converted));
        }
    }
    if let Some(choice) = chat.get("tool_choice").filter(|value| is_truthy(value)) {
        out.insert("tool_choice".to_string(), tool_choice_to_responses(choice));
    }
    // response_format → text.format（json_schema 的嵌套 → 扁平）
    if let Some(format) = chat.get("response_format").filter(|value| is_truthy(value)) {
        out.insert("text".to_string(), json!({ "format": format_to_responses(format) }));
    }
    // reasoning_effort → reasoning.effort（本网关各上游的「思考档位」统一
    // 从这里进；summary:"auto" 与参考实现同款 —— 让上游回思考摘要）
    if let Some(effort) = chat
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        out.insert(
            "reasoning".to_string(),
            json!({ "effort": effort, "summary": "auto" }),
        );
    }
    // store:false 与模块头同一理由（网关无状态，也不许上游存）
    out.insert("store".to_string(), Value::Bool(false));
    Ok(Value::Object(out))
}

/// 一条 assistant 消息 → 零到多个 input 项（顺序：reasoning → message →
/// function_call）。
///
/// 顺序不是随意的：回程的 `chat_from_responses` 按「reasoning 项在前、
/// 本轮 assistant 项在后」消费（`PendingReasoning`），反向也必须产出同一
/// 形状，多轮历史才能被上游正确读取。
fn push_assistant_items(items: &mut Vec<Value>, message: &Value) {
    let reasoning = {
        let from_field = string_field(message, "reasoning_content");
        if from_field.is_empty() {
            string_field(message, "reasoning")
        } else {
            from_field
        }
    };
    if !reasoning.is_empty() {
        items.push(json!({
            "type": "reasoning",
            "id": random_id("rs"),
            "summary": [{ "type": "summary_text", "text": reasoning }],
        }));
    }
    // 正文：Responses 不需要空的 assistant message（只带工具调用的轮次
    // 只发 function_call 项）；空正文跳过也能避免上游把空消息判成错误
    let text = content_text(message.get("content").unwrap_or(&Value::Null));
    if !text.is_empty() {
        items.push(json!({
            "type": "message",
            "id": random_id("msg"),
            "role": "assistant",
            "content": [{ "type": "output_text", "text": text }],
        }));
    }
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            let call_id = {
                let raw = string_field(call, "id");
                if raw.is_empty() {
                    random_id("call")
                } else {
                    raw
                }
            };
            let name = call
                .pointer("/function/name")
                .map(string_value)
                .unwrap_or_default();
            let arguments = {
                let raw = json_text(call.pointer("/function/arguments").unwrap_or(&Value::Null));
                if raw.is_empty() {
                    "{}".to_string()
                } else {
                    raw
                }
            };
            items.push(json!({
                "type": "function_call",
                "id": random_id("fc"),
                "call_id": call_id,
                "name": name,
                "arguments": arguments,
            }));
        }
    }
}

/// Chat content（字符串或块数组）→ Responses 的 input content。
///
/// 口径对着 `chat_from_responses` 的 `content_to_chat` 反推：
///   - 纯文本（字符串 / 全文本块）保持该有的形态（user 可用字符串，
///     assistant 用 `[{type:"output_text"}]`）；
///   - 图片块 → `input_image`（url 取 `image_url` 的字符串或 `{url}` 形态）；
///   - 其余块（音频/文件等）**丢弃** —— 它们在 chat 侧本来就没有来源，
///     透传一个上游不认识的类型只会让整条请求 400。
fn chat_content_to_responses(content: &Value, role: &str) -> Value {
    let text_kind = if role == "assistant" { "output_text" } else { "input_text" };
    if let Some(text) = content.as_str() {
        if role == "assistant" {
            return if text.is_empty() {
                Value::String(String::new())
            } else {
                json!([{ "type": "output_text", "text": text }])
            };
        }
        return Value::String(text.to_string());
    }
    let parts = content_parts(content);
    if parts.is_empty() {
        return Value::String(String::new());
    }
    let mut out: Vec<Value> = Vec::new();
    for part in parts {
        if let Some(text) = part.as_str() {
            out.push(json!({ "type": text_kind, "text": text }));
            continue;
        }
        let kind = string_field(part, "type").to_lowercase();
        match kind.as_str() {
            "text" | "input_text" | "output_text" => {
                out.push(json!({ "type": text_kind, "text": string_field(part, "text") }));
            }
            "image_url" | "input_image" if role == "user" => {
                let url = image_url_of(part);
                if !url.is_empty() {
                    out.push(json!({ "type": "input_image", "image_url": url }));
                }
            }
            _ => {}
        }
    }
    if out.is_empty() {
        Value::String(String::new())
    } else {
        Value::Array(out)
    }
}

/// Chat 工具声明（嵌套 function）→ Responses 工具声明（扁平）。
///
/// `parameters` 缺省给空对象 schema（`nested_from_flat` 的同一兜底）；
/// `strict` 原样搬运（两协议同名）。
fn tool_to_responses(tool: &Value) -> Option<Value> {
    if !tool_plan::is_nested_function(tool) {
        // 非嵌套形态（裸 name/parameters 或原生工具类型）：出站没有可靠的
        // 翻译口径，丢弃而不是发一个半成品上去
        return None;
    }
    let function = tool.get("function")?;
    let name = string_field(function, "name");
    if name.is_empty() {
        return None;
    }
    let mut out = Map::new();
    out.insert("type".to_string(), Value::String("function".to_string()));
    out.insert("name".to_string(), Value::String(name));
    if let Some(description) = function.get("description").filter(|value| is_truthy(value)) {
        out.insert("description".to_string(), description.clone());
    }
    let parameters = function
        .get("parameters")
        .filter(|value| is_truthy(value))
        .cloned()
        .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
    out.insert("parameters".to_string(), parameters);
    if let Some(strict) = function.get("strict") {
        out.insert("strict".to_string(), strict.clone());
    }
    Some(Value::Object(out))
}

/// Chat 的 tool_choice → Responses 的 tool_choice
/// （`{type:"function", function:{name}}` → `{type:"function", name}`；
/// 字符串形态两协议同名，原样保留）。
fn tool_choice_to_responses(choice: &Value) -> Value {
    if choice.is_string() {
        return choice.clone();
    }
    let name = {
        let nested = choice.pointer("/function/name").map(string_value).unwrap_or_default();
        if nested.is_empty() {
            string_field(choice, "name")
        } else {
            nested
        }
    };
    if name.is_empty() {
        return choice.clone();
    }
    json!({ "type": "function", "name": name })
}

/// Chat 的 `response_format` → Responses 的 `text.format`
/// （`{type:"json_schema", json_schema:{…}}` → `{type:"json_schema", …}`）。
fn format_to_responses(format: &Value) -> Value {
    let kind = string_field(format, "type");
    if kind != "json_schema" {
        return format.clone();
    }
    let mut out = Map::new();
    out.insert("type".to_string(), Value::String("json_schema".to_string()));
    for key in ["name", "description", "schema", "strict"] {
        if let Some(value) = format
            .pointer(&format!("/json_schema/{key}"))
            .filter(|value| !value.is_null())
        {
            out.insert(key.to_string(), value.clone());
        }
    }
    Value::Object(out)
}

// ─── 响应：Responses SSE → Chat SSE（流式）────────────────────

/// 上游 Responses SSE 字节流 → 标准 chat SSE 字节流（状态机）。
///
/// 输出帧的形状必须让下游既有消费层认得（`ForwardStream` 的 reasoning 合并
/// 与 usage 提取、聚合器的 `choices[0].delta` 读取、下游两种协议出口的
/// 反向翻译都吃这个形状），关键约定：
///   - 每个 chat chunk 都是 `data: {"id","object":"chat.completion.chunk",
///     "created","model","choices":[{"index":0,"delta":{…},"finish_reason":…}]}`
///   - `response.output_text.delta` → `delta.content`；
///     `response.reasoning_summary_text.delta` → `delta.reasoning_content`；
///     `response.function_call_arguments.delta` → `delta.tool_calls[…]`
///   - `response.completed` → 收尾帧（`finish_reason`）+ usage 帧
///     （`choices: []`，usage 折成 chat 口径）+ `data: [DONE]`
///   - `error` / `response.failed` → `data: {"error":{…}}` + `data: [DONE]`
///     （与 ForwardStream 的断流收尾同形状：聚合器据此转 502）
///
/// usage 帧会同时被两处读到（ForwardStream 的旁路提取 + 聚合器的 usage
/// 覆盖）—— 幂等覆盖，以最后一处为准，不影响含义。
pub struct ChatFromResponsesStream {
    buffer: SseLineBuffer,
    /// 输出帧的 model（发给上游的真名；ForwardStream 的回写层会按需改成
    /// 客户端请求名，此处不必区分）
    model: String,
    id: String,
    created: i64,
    started: bool,
    finished: bool,
    /// 已宣告的工具：item_id / call_id → 槽位（chat 的 tool_calls index +
    /// 是否已发过参数增量）
    tools: BTreeMap<String, ToolSlot>,
    next_tool_index: i64,
    /// 上游最近的 usage（Responses 口径；收尾时折成 chat 形态）
    usage: Option<Value>,
}

/// 一个工具的 chat 侧状态（key 是上游给的 item_id / call_id）
#[derive(Default)]
struct ToolSlot {
    index: i64,
    has_arguments: bool,
}

impl ChatFromResponsesStream {
    pub fn new(model: &str) -> Self {
        Self {
            buffer: SseLineBuffer::new(),
            model: model.to_string(),
            id: String::new(),
            created: crate::server::logging::now_ms() / 1000,
            started: false,
            finished: false,
            tools: BTreeMap::new(),
            next_tool_index: 0,
            usage: None,
        }
    }

    /// 吃一段上游字节，吐出要下发的 chat SSE 帧
    pub fn push(&mut self, chunk: &[u8]) -> Vec<bytes::Bytes> {
        let mut out = Vec::new();
        for payload in self.buffer.push(chunk) {
            match payload {
                None => out.extend(self.finish()),
                Some(data) => {
                    if let Ok(value) = serde_json::from_str::<Value>(&data) {
                        out.extend(self.consume(&value));
                    }
                }
            }
        }
        out
    }

    /// 上游流结束（没有终态事件时的兜底收尾）
    pub fn finish(&mut self) -> Vec<bytes::Bytes> {
        if self.finished {
            return Vec::new();
        }
        // 先冲刷缓冲里残留的最后一帧
        let mut out = Vec::new();
        for payload in self.buffer.finish() {
            if let Some(data) = payload {
                if let Ok(value) = serde_json::from_str::<Value>(&data) {
                    out.extend(self.consume(&value));
                }
            }
        }
        out.extend(self.complete(None));
        out
    }

    /// 一个上游 Responses 事件 → 零到多个 chat 帧
    fn consume(&mut self, event: &Value) -> Vec<bytes::Bytes> {
        if self.finished {
            return Vec::new();
        }
        // usage 的位置各家不一：官方在 response.completed 的 response.usage
        // 里，部分网关直接放事件顶层。两处都收（事件流里的最近一次为准）
        let usage = event
            .get("usage")
            .filter(|usage| usage.is_object())
            .or_else(|| event.pointer("/response/usage").filter(|usage| usage.is_object()));
        if let Some(usage) = usage {
            self.usage = Some(usage.clone());
        }
        let kind = string_field(event, "type").to_lowercase();
        match kind.as_str() {
            "response.created" => {
                if let Some(id) = event
                    .pointer("/response/id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                {
                    self.id = id.to_string();
                }
                self.start()
            }
            // 正文增量（refusal 是官方的拒答文本，chat 侧没有专用通道，
            // 与正文同路 —— 丢掉会让客户端看到一段空回答）
            "response.output_text.delta" | "response.refusal.delta" => {
                let Some(text) = event
                    .get("delta")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                else {
                    return Vec::new();
                };
                let mut out = self.start();
                out.push(self.delta_frame(json!({ "content": text })));
                out
            }
            // 思考增量 → reasoning_content（chat 侧的统一表达）
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                let Some(text) = event
                    .get("delta")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                else {
                    return Vec::new();
                };
                let mut out = self.start();
                out.push(self.delta_frame(json!({ "reasoning_content": text })));
                out
            }
            // 工具宣告：拿到 id / name 才能在 chat 侧建立 tool_calls 槽位
            "response.output_item.added" => {
                let item = event.get("item").unwrap_or(&Value::Null);
                let item_kind = string_field(item, "type").to_lowercase();
                if !matches!(item_kind.as_str(), "function_call" | "custom_tool_call") {
                    return Vec::new();
                }
                let mut out = self.start();
                let key = tool_key(event, item);
                out.extend(self.ensure_tool(&key, item));
                out
            }
            "response.function_call_arguments.delta" => {
                let delta = string_field(event, "delta");
                if delta.is_empty() {
                    return Vec::new();
                }
                let mut out = self.start();
                let key = tool_key(event, event);
                out.extend(self.ensure_tool(&key, event));
                out.extend(self.push_arguments(&key, &delta));
                out
            }
            // 自由文本工具（custom）的输入增量：分片是裸文本，中途无法可靠
            // 剥出 JSON 壳（见 `ResponsesStream::tool_delta_event` 同款取舍），
            // 按原样进 chat 的 function.arguments；完整包壳只在一个分片都没
            // 来过的 `.done` 兜底里做（wrap_freeform_input）
            "response.custom_tool_call_input.delta" => {
                let delta = {
                    let raw = event.get("delta").unwrap_or(event.get("input").unwrap_or(&Value::Null));
                    json_text(raw)
                };
                if delta.is_empty() {
                    return Vec::new();
                }
                let mut out = self.start();
                let key = tool_key(event, event);
                out.extend(self.ensure_tool(&key, event));
                out.extend(self.push_arguments(&key, &delta));
                out
            }
            // 参数收尾：增量一个都没来过时（少数网关只发 .done）补完整参数
            "response.function_call_arguments.done" => {
                let key = tool_key(event, event);
                if self.has_arguments(&key) {
                    return Vec::new();
                }
                let arguments = {
                    let raw = json_text(event.get("arguments").unwrap_or(&Value::Null));
                    if raw.is_empty() { "{}".to_string() } else { raw }
                };
                let mut out = self.start();
                out.extend(self.ensure_tool(&key, event));
                out.extend(self.push_arguments(&key, &arguments));
                out
            }
            "response.custom_tool_call_input.done" => {
                let key = tool_key(event, event);
                if self.has_arguments(&key) {
                    return Vec::new();
                }
                let input = json_text(event.get("input").unwrap_or(&Value::Null));
                let arguments = super::freeform::wrap_freeform_input(&input);
                let mut out = self.start();
                out.extend(self.ensure_tool(&key, event));
                out.extend(self.push_arguments(&key, &arguments));
                out
            }
            // 项收尾：与上面 .done 同口径的补漏（两端都可能先到）
            "response.output_item.done" => {
                let item = event.get("item").unwrap_or(&Value::Null);
                let item_kind = string_field(item, "type").to_lowercase();
                let arguments = match item_kind.as_str() {
                    "function_call" => {
                        let raw = json_text(item.get("arguments").unwrap_or(&Value::Null));
                        if raw.is_empty() { "{}".to_string() } else { raw }
                    }
                    "custom_tool_call" => {
                        super::freeform::wrap_freeform_input(&json_text(item.get("input").unwrap_or(&Value::Null)))
                    }
                    _ => return Vec::new(),
                };
                let key = tool_key(event, item);
                if self.has_arguments(&key) {
                    return Vec::new();
                }
                let mut out = self.start();
                out.extend(self.ensure_tool(&key, item));
                out.extend(self.push_arguments(&key, &arguments));
                out
            }
            // 上游错误（事件流里的失败）：折成 chat 错误帧 + [DONE]
            "error" | "response.failed" => self.fail(event),
            // 终态：收尾（usage 已在开头收过）
            "response.completed" | "response.done" => self.complete(None),
            "response.incomplete" => {
                let reason = event
                    .pointer("/response/incomplete_details/reason")
                    .map(string_value)
                    .unwrap_or_default();
                let reason = if reason.is_empty() { "length".to_string() } else { reason };
                self.complete(Some(&reason))
            }
            _ => Vec::new(),
        }
    }

    /// 首帧（role assistant）：与官方 bridge 同款 —— 空响应或只有工具的流
    /// 也能建立 assistant 消息（下游按 delta 累积时不会拿到「无角色」的帧）
    fn start(&mut self) -> Vec<bytes::Bytes> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        if self.id.is_empty() {
            self.id = random_id("chatcmpl");
        }
        vec![self.delta_frame(json!({ "role": "assistant", "content": "" }))]
    }

    /// 宣告一个工具（若未宣告过）：chat 的 tool_calls 首帧带 id / name / 空参数
    fn ensure_tool(&mut self, key: &str, item: &Value) -> Vec<bytes::Bytes> {
        if self.tools.contains_key(key) {
            return Vec::new();
        }
        let index = self.next_tool_index;
        self.next_tool_index += 1;
        self.tools.insert(key.to_string(), ToolSlot { index, has_arguments: false });
        let call_id = {
            let raw = string_field(item, "call_id");
            if raw.is_empty() {
                let id = string_field(item, "id");
                if id.is_empty() { random_id("call") } else { id }
            } else {
                raw
            }
        };
        let name = string_field(item, "name");
        vec![self.delta_frame(json!({
            "tool_calls": [{
                "index": index,
                "id": call_id,
                "type": "function",
                "function": { "name": name, "arguments": "" },
            }],
        }))]
    }

    /// 追加一段工具参数（槽位不存在时静默跳过 —— 没宣告过就没有 index 可挂）
    fn push_arguments(&mut self, key: &str, delta: &str) -> Vec<bytes::Bytes> {
        let Some(slot) = self.tools.get_mut(key) else {
            return Vec::new();
        };
        slot.has_arguments = true;
        let index = slot.index;
        vec![self.delta_frame(json!({
            "tool_calls": [{ "index": index, "function": { "arguments": delta } }],
        }))]
    }

    fn has_arguments(&self, key: &str) -> bool {
        self.tools.get(key).map(|slot| slot.has_arguments).unwrap_or(false)
    }

    /// 收尾：finish_reason 帧 + usage 帧 + [DONE]（只做一次）
    fn complete(&mut self, incomplete_reason: Option<&str>) -> Vec<bytes::Bytes> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        let mut out = self.start();
        let finish_reason = match incomplete_reason {
            Some(reason) => reason.to_string(),
            // Responses 没有 chat 的 finish_reason，按输出形态推导：
            // 有工具调用 → tool_calls；否则 stop（length/content_filter
            // 由 response.incomplete 的 reason 直接给出）
            None => {
                if self.tools.is_empty() {
                    "stop".to_string()
                } else {
                    "tool_calls".to_string()
                }
            }
        };
        out.push(self.finish_frame(&finish_reason));
        if let Some(usage) = self.usage.take() {
            // usage 帧（choices 空数组）：ForwardStream 的旁路提取与聚合器
            // 都从这一帧读（形状对齐官方 include_usage 的收尾帧）
            out.push(chat_frame(&json!({
                "id": self.id,
                "object": "chat.completion.chunk",
                "created": self.created,
                "model": self.model,
                "choices": [],
                "usage": chat_usage_from_responses(&usage),
            })));
        }
        out.push(bytes::Bytes::from_static(b"data: [DONE]\n\n"));
        out
    }

    /// 上游错误 → chat 错误帧 + [DONE]（只做一次）
    fn fail(&mut self, event: &Value) -> Vec<bytes::Bytes> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        let error = event
            .get("error")
            .filter(|error| is_truthy(error))
            .or_else(|| event.pointer("/response/error").filter(|error| is_truthy(error)));
        let message = match error {
            Some(error) => {
                let text = string_field(error, "message");
                if text.is_empty() { string_value(error) } else { text }
            }
            None => string_value(event),
        };
        let message = if message.trim().is_empty() {
            "上游流式返回错误".to_string()
        } else {
            message
        };
        vec![
            chat_frame(&json!({
                "error": { "message": message, "type": "upstream_error" },
            })),
            bytes::Bytes::from_static(b"data: [DONE]\n\n"),
        ]
    }

    /// 一个 delta 帧（`choices[0].delta = delta`，finish_reason 为 null）
    fn delta_frame(&self, delta: Value) -> bytes::Bytes {
        chat_frame(&json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": Value::Null }],
        }))
    }

    /// 收尾帧（空 delta + finish_reason）
    fn finish_frame(&self, finish_reason: &str) -> bytes::Bytes {
        chat_frame(&json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{ "index": 0, "delta": {}, "finish_reason": finish_reason }],
        }))
    }
}

/// 事件 / 项里的工具身份键：call_id 优先，退到 id / item_id / output_index。
///
/// 同一次调用的各个事件（added / delta / done）必须映射到同一个键，
/// 上游保证这几个字段在各事件里一致（官方与参考实现都按这个优先链取值）。
fn tool_key(event: &Value, item: &Value) -> String {
    for text in [
        string_field(item, "call_id"),
        string_field(item, "id"),
        string_field(event, "item_id"),
    ] {
        if !text.is_empty() {
            return text;
        }
    }
    event.get("output_index").map(string_value).unwrap_or_default()
}

/// Responses 的 usage → chat 的 usage（字段名与明细结构按 chat 口径折）。
///
/// 与 `responses.rs::usage_to_responses` 互为反函数，字段清单逐条对照：
/// `input_tokens`→`prompt_tokens`、`output_tokens`→`completion_tokens`、
/// `input_tokens_details.cached_tokens`→`prompt_tokens_details.cached_tokens`、
/// `output_tokens_details.reasoning_tokens`→`completion_tokens_details.reasoning_tokens`。
fn chat_usage_from_responses(usage: &Value) -> Value {
    let input = json_number_of(usage, &["input_tokens", "prompt_tokens"]);
    let output = json_number_of(usage, &["output_tokens", "completion_tokens"]);
    let cached = usage
        .pointer("/input_tokens_details/cached_tokens")
        .and_then(Value::as_i64)
        .filter(|value| *value != 0)
        .unwrap_or_else(|| json_number_of(usage, &["cache_read_input_tokens"]));
    let reasoning = usage
        .pointer("/output_tokens_details/reasoning_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let mut out = Map::new();
    out.insert("prompt_tokens".to_string(), Value::from(input));
    if cached != 0 {
        out.insert(
            "prompt_tokens_details".to_string(),
            json!({ "cached_tokens": cached }),
        );
    }
    out.insert("completion_tokens".to_string(), Value::from(output));
    if reasoning != 0 {
        out.insert(
            "completion_tokens_details".to_string(),
            json!({ "reasoning_tokens": reasoning }),
        );
    }
    out.insert(
        "total_tokens".to_string(),
        Value::from(json_number_of(usage, &["total_tokens"]).max(input + output)),
    );
    Value::Object(out)
}
