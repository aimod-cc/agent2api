//! Anthropic Messages 协议的**出站**转换：chat 请求 → Anthropic 上游请求、
//! 上游 Anthropic SSE → chat SSE（自定义提供商转发的 `anthropic` 分支）。
//!
//! ── 与 `anthropic.rs` 的方向关系 ─────────────────────────────
//! `anthropic.rs` 服务**下游入口**（`api::protocol` 的 `/v1/messages`）；
//! 本文件是它的反方向，服务自定义提供商的**上游**：
//!   - 请求：`anthropic_request_from_chat`（chat 体 → Messages 体，逐字段
//!     对着 `chat_from_anthropic` 反推）；
//!   - 响应：`ChatFromAnthropicStream`（上游 Anthropic SSE → 标准 chat SSE，
//!     之后照走既有的 `ForwardStream` / 聚合器）。
//!
//! ── 三处结构差异的处理（与 `anthropic.rs` 模块头互为镜像）────
//!   1. chat 的 system 消息 → 顶层 `system` 字段（多条空行拼接）；
//!   2. chat 的 `role:"tool"` 消息 → user 消息里的 `tool_result` 块；
//!   3. chat 的 `message.tool_calls` → assistant 消息里的 `tool_use` 块
//!      （`arguments` 字符串解析成对象；Anthropic 的 input 必须是对象）。
//!
//! ── thinking 的两条 Anthropic 硬规则（都在这里守）────────────
//!   - `thinking.type:"enabled"` 要求 `budget_tokens < max_tokens`：
//!     注入时若 max_tokens 不够大，抬到 `budget + 1024`（参考实现同款）；
//!   - thinking 开启时 Anthropic 拒绝 `temperature` / `top_p`（temperature
//!     只允许 1）：与其让上游 400，不如**不带**这两个可选参数 —— 与
//!     `model_rules` 里「不向任何上游发会弄坏请求的字段」同一取向。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! 同 `mod.rs`：零 unwrap/expect/panic；解析失败退化「跳过该帧」。

use std::collections::BTreeMap;

use serde_json::{json, Map, Value};

use super::{
    chat_frame, content_parts, content_text, is_truthy, json_text, random_id, string_field,
    string_value, SseLineBuffer, FIELD_CACHE_CONTROL, FIELD_IS_ERROR,
};
use super::anthropic::{parse_json_object, tool_result_text, DEFAULT_MAX_TOKENS};
use super::responses::ConvertError;
use crate::server::core::model_rules;

// ─── 请求：Chat → Anthropic ─────────────────────────────────

/// Chat Completions 请求体 → Anthropic Messages 请求体。
///
/// ── 逐字段对着 [`super::anthropic::chat_from_anthropic`] 反推 ────
///   - `model`（由调用方给上游真名）
///   - `system` ← chat 的 system / developer 消息（多条空行拼接）
///   - `messages` ← 其余消息（tool 消息拆进 user 的 tool_result 块；
///     tool_calls 进 assistant 的 tool_use 块；连续同角色合并成一条 ——
///     Anthropic 要求 user / assistant 交替）
///   - `max_tokens` ← `max_tokens` / `max_completion_tokens`，缺省
///     [`DEFAULT_MAX_TOKENS`]（Anthropic 必填）
///   - `temperature` / `top_p` / `stop_sequences`（← chat `stop`）
///   - `tools`（chat 嵌套 function → `{name, description, input_schema}`）
///   - `tool_choice`（`required`→`any`、`{type:function,function:{name}}`
///     →`{type:"tool",name}`）
///   - `thinking` ← chat `reasoning_effort`（等级折算 budget，见
///     [`thinking_budget`]；同时可能抬高 max_tokens）
pub fn anthropic_request_from_chat(chat: &Value, model: &str) -> Result<Value, ConvertError> {
    let Some(messages_in) = chat.get("messages").and_then(Value::as_array) else {
        return Err("缺少 messages 数组".to_string());
    };
    let mut system: Vec<String> = Vec::new();
    // system 上的缓存断点（任一条 system 消息带断点即生效 —— 断点标记的是
    // 「缓存前缀到此为止」，多段 system 拼接后断点落在整段末尾）
    let mut system_cache: Option<Value> = None;
    let mut messages: Vec<Value> = Vec::new();
    for message in messages_in {
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
        if role == "system" {
            let text = content_text(message.get("content").unwrap_or(&Value::Null));
            if !text.trim().is_empty() {
                system.push(text);
                if let Some(cache) =
                    message.get(FIELD_CACHE_CONTROL).filter(|value| value.is_object())
                {
                    system_cache = Some(cache.clone());
                }
            }
            continue;
        }
        let blocks = anthropic_blocks_of(message, &role);
        if blocks.is_empty() {
            continue;
        }
        let target_role = if role == "assistant" { "assistant" } else { "user" };
        append_merged(&mut messages, target_role, blocks);
    }

    let mut out = Map::new();
    out.insert("model".to_string(), Value::String(model.to_string()));
    out.insert("messages".to_string(), Value::Array(messages));
    out.insert(
        "stream".to_string(),
        Value::Bool(chat.get("stream").and_then(Value::as_bool).unwrap_or(false)),
    );
    if !system.is_empty() {
        match system_cache {
            Some(cache) => {
                // 带断点的 system 用块数组形态（Anthropic 两种都认），
                // cache_control 落在最后一块 —— 与入口暂存时的位置一致
                let mut blocks: Vec<Value> = system
                    .iter()
                    .map(|text| json!({ "type": "text", "text": text }))
                    .collect();
                if let Some(last) = blocks.last_mut() {
                    if let Some(object) = last.as_object_mut() {
                        object.insert("cache_control".to_string(), cache);
                    }
                }
                out.insert("system".to_string(), Value::Array(blocks));
            }
            None => {
                out.insert("system".to_string(), Value::String(system.join("\n\n")));
            }
        }
    }

    // thinking 先判定：它决定 max_tokens 的下限，也决定 temperature/top_p
    // 能不能带（见模块头第二条规则）
    let thinking = chat
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .and_then(thinking_budget);
    let mut max_tokens = ["max_tokens", "max_completion_tokens"]
        .iter()
        .find_map(|key| chat.get(*key).and_then(Value::as_i64).filter(|value| *value > 0))
        .unwrap_or(DEFAULT_MAX_TOKENS);
    if let Some(budget) = thinking {
        // Anthropic 要求 budget_tokens < max_tokens；不够就抬到 budget 之上
        if max_tokens <= budget {
            max_tokens = budget + 1024;
        }
    }
    out.insert("max_tokens".to_string(), Value::from(max_tokens));
    if thinking.is_some() {
        if let Some(budget) = thinking {
            out.insert(
                "thinking".to_string(),
                json!({ "type": "enabled", "budget_tokens": budget }),
            );
        }
    } else {
        for key in ["temperature", "top_p"] {
            if let Some(value) = chat.get(key).filter(|value| !value.is_null()) {
                out.insert(key.to_string(), value.clone());
            }
        }
    }
    // stop → stop_sequences（chat 允许字符串或数组，Anthropic 只认数组）
    if let Some(stop) = chat.get("stop").filter(|value| is_truthy(value)) {
        let list = match stop {
            Value::Array(items) => items.clone(),
            other => vec![other.clone()],
        };
        if !list.is_empty() {
            out.insert("stop_sequences".to_string(), Value::Array(list));
        }
    }
    if let Some(tools) = chat.get("tools").and_then(Value::as_array) {
        let converted: Vec<Value> = tools.iter().filter_map(tool_to_anthropic).collect();
        if !converted.is_empty() {
            out.insert("tools".to_string(), Value::Array(converted));
        }
    }
    if let Some(choice) = chat.get("tool_choice").filter(|value| is_truthy(value)) {
        out.insert("tool_choice".to_string(), tool_choice_to_anthropic(choice));
    }
    Ok(Value::Object(out))
}

/// 一条非 system 的 chat 消息 → Anthropic 内容块数组。
fn anthropic_blocks_of(message: &Value, role: &str) -> Vec<Value> {
    // tool 消息 → tool_result 块（挂在 user 消息上；Anthropic 要求
    // tool_result 与 assistant 的 tool_use 成对，tool_use_id 必须指向
    // 真实存在的 id —— 缺 id 说明客户端数据本身缺配对，伪造一个只会让
    // 上游的配对校验指向更莫名其妙的块，所以原样发、让上游如实报错）
    if role == "tool" {
        // is_error 恢复：入口翻译时暂存的「工具执行失败」标记（见
        // `anthropic::convert_message`）—— OpenAI 形出口没有这个概念，
        // 那条路径随 strip 剥离，只有这里能把它带回去
        let is_error = message.get(FIELD_IS_ERROR).and_then(Value::as_bool) == Some(true);
        let cache = message.get(FIELD_CACHE_CONTROL).filter(|value| value.is_object());
        let mut block = json!({
            "type": "tool_result",
            "tool_use_id": string_field(message, "tool_call_id"),
            "content": tool_result_text(message.get("content").unwrap_or(&Value::Null)),
        });
        if let Some(object) = block.as_object_mut() {
            if is_error {
                object.insert("is_error".to_string(), Value::Bool(true));
            }
            if let Some(cache) = cache {
                object.insert("cache_control".to_string(), cache.clone());
            }
        }
        return vec![block];
    }
    let mut blocks: Vec<Value> = Vec::new();
    if role == "assistant" {
        // 思考 → thinking 块（不带 signature：Chat 侧没有签名概念，
        // 见 `anthropic.rs` 模块头的方向约定）
        let reasoning = {
            let from_field = string_field(message, "reasoning_content");
            if from_field.is_empty() {
                string_field(message, "reasoning")
            } else {
                from_field
            }
        };
        if !reasoning.is_empty() {
            blocks.push(json!({ "type": "thinking", "thinking": reasoning }));
        }
    }
    // 正文：字符串 / 块数组（图片 → image 块，见 [`image_to_anthropic`]）
    match message.get("content").unwrap_or(&Value::Null) {
        Value::String(text) => {
            if !text.is_empty() {
                blocks.push(json!({ "type": "text", "text": text }));
            }
        }
        content => {
            for part in content_parts(content) {
                if let Some(text) = part.as_str() {
                    if !text.is_empty() {
                        blocks.push(json!({ "type": "text", "text": text }));
                    }
                    continue;
                }
                let kind = string_field(part, "type").to_lowercase();
                match kind.as_str() {
                    "text" | "input_text" | "output_text" => {
                        let text = string_field(part, "text");
                        if !text.is_empty() {
                            blocks.push(json!({ "type": "text", "text": text }));
                        }
                    }
                    "image_url" | "input_image" if role != "assistant" => {
                        if let Some(image) = image_to_anthropic(part) {
                            blocks.push(image);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    // 工具调用 → tool_use 块（input 必须是对象；`arguments` 解析不出时
    // 包一层 raw，与 `anthropic.rs` 的 `parse_json_object` 同口径）
    if role == "assistant" {
        if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                let id = {
                    let raw = string_field(call, "id");
                    if raw.is_empty() {
                        random_id("toolu")
                    } else {
                        raw
                    }
                };
                blocks.push(json!({
                    "type": "tool_use",
                    "id": id,
                    "name": call.pointer("/function/name").map(string_value).unwrap_or_default(),
                    "input": parse_json_object(
                        call.pointer("/function/arguments").unwrap_or(&Value::Null)
                    ),
                }));
            }
        }
    }
    // 消息级缓存断点 → 落回最后一个可挂载的块。Anthropic 不允许把
    // cache_control 挂在 thinking 块上，跳过；没有可挂的块时整段放弃
    // （给上游发一个非法位置的断点只会换来 400）
    if let Some(cache) = message.get(FIELD_CACHE_CONTROL).filter(|value| value.is_object()) {
        for block in blocks.iter_mut().rev() {
            let kind = string_field(block, "type");
            let mountable = matches!(kind.as_str(), "text" | "tool_use" | "tool_result" | "image");
            if mountable {
                if let Some(object) = block.as_object_mut() {
                    object.insert("cache_control".to_string(), cache.clone());
                }
                break;
            }
        }
    }
    blocks
}

/// 连续同角色的消息合并成一条（Anthropic 要求 user / assistant 交替，
/// 拆开的两条同角色消息会被上游拒）。
fn append_merged(messages: &mut Vec<Value>, role: &str, blocks: Vec<Value>) {
    if let Some(last) = messages.last_mut() {
        let same_role = last.get("role").and_then(Value::as_str) == Some(role);
        if same_role {
            if let Some(existing) = last.get_mut("content").and_then(Value::as_array_mut) {
                existing.extend(blocks);
                return;
            }
        }
    }
    messages.push(json!({ "role": role, "content": blocks }));
}

/// Chat 的 image_url 块 → Anthropic 的 image 块。
///
/// 两个来源都要认：`data:` URI（解码出 media_type 与 base64 数据）→
/// `source:{type:"base64"}`；http(s) 外链 → `source:{type:"url"}`。
/// 其余形态（相对路径、非法 data URI）丢弃 —— 发一个上游读不懂的 source
/// 只会让整条请求 400。
fn image_to_anthropic(part: &Value) -> Option<Value> {
    let source = part.get("image_url").unwrap_or(part);
    let url = match source {
        Value::String(text) => text.clone(),
        other => string_field(other, "url"),
    };
    let url = url.trim();
    if url.is_empty() {
        return None;
    }
    if let Some(rest) = url.strip_prefix("data:") {
        let (meta, data) = rest.split_once(',')?;
        // `data:<media_type>;base64,<data>`；没有 `;base64` 后缀的形态
        // （非 base64 编码）Anthropic 不接受，丢弃
        let media_type = meta.strip_suffix(";base64")?;
        if data.is_empty() {
            return None;
        }
        let media_type = if media_type.is_empty() {
            "image/png".to_string()
        } else {
            media_type.to_string()
        };
        return Some(json!({
            "type": "image",
            "source": { "type": "base64", "media_type": media_type, "data": data },
        }));
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        return Some(json!({
            "type": "image",
            "source": { "type": "url", "url": url },
        }));
    }
    None
}

/// Chat 工具声明（嵌套 function）→ Anthropic 工具（`input_schema` 形态）。
fn tool_to_anthropic(tool: &Value) -> Option<Value> {
    // chat 侧只会有嵌套形态；裸 function 对象也容忍（两种形态等价）
    let function = tool.get("function").unwrap_or(tool);
    let name = string_field(function, "name");
    if name.is_empty() {
        return None;
    }
    let mut out = Map::new();
    out.insert("name".to_string(), Value::String(name));
    if let Some(description) = function.get("description").filter(|value| is_truthy(value)) {
        out.insert("description".to_string(), description.clone());
    }
    let schema = function.get("parameters").unwrap_or(&Value::Null);
    out.insert("input_schema".to_string(), normalize_input_schema(schema));
    // 工具定义上的缓存断点随内部暂存字段恢复（工具清单大而稳定，是
    // Anthropic 提示缓存收益最高的一段；入口侧见 `anthropic::tool_to_chat`）
    if let Some(cache) = tool.get(FIELD_CACHE_CONTROL).filter(|value| value.is_object()) {
        out.insert("cache_control".to_string(), cache.clone());
    }
    Some(Value::Object(out))
}

/// `input_schema` 归一：Anthropic 要求顶层 `type:"object"`、`properties`
/// 存在（口径对齐参考实现 `normalizeAnthropicInputSchema`）。
fn normalize_input_schema(schema: &Value) -> Value {
    let kind = string_field(schema, "type").to_lowercase();
    if !schema.is_object() || (!kind.is_empty() && kind != "object") {
        return json!({ "type": "object", "properties": {} });
    }
    let mut out = schema.as_object().cloned().unwrap_or_default();
    out.insert("type".to_string(), Value::String("object".to_string()));
    out.entry("properties".to_string()).or_insert_with(|| json!({}));
    Value::Object(out)
}

/// Chat 的 tool_choice → Anthropic 的 tool_choice（`chat_from_anthropic`
/// 的 `tool_choice_to_chat` 的反向表：`required`↔`any`、function↔tool）。
fn tool_choice_to_anthropic(choice: &Value) -> Value {
    if let Some(text) = choice.as_str() {
        return match text {
            "required" => json!({ "type": "any" }),
            "none" => json!({ "type": "none" }),
            // `auto` 与其余未识别的字符串都落到 auto（Anthropic 只认这四个
            // 形态；发一个它不认识的字符串是必然的 400）
            _ => json!({ "type": "auto" }),
        };
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
        return json!({ "type": "auto" });
    }
    json!({ "type": "tool", "name": name })
}

/// chat 的 `reasoning_effort` → Anthropic 的 `budget_tokens`。
///
/// 一档一个值，按 [`model_rules::reasoning_rank`] 的强弱序折算；表外等级
/// （自定义输入）没有可翻译的目标，返回 None（与 `model_rules` 的
/// 「表外值不参与转发」同一闸门）：
///
/// ```text
///   rank 0..1（minimal / low）→ 1024
///   rank 2    （medium）      → 4096
///   rank 3    （high）        → 10240
///   rank 4..5（xhigh / max）  → 32768
/// ```
///
/// 分档值与参考实现 `thinkingBudget` 对齐（它的 low/medium/high/max 四个
/// 值原样落在这四档上）。
fn thinking_budget(effort: &str) -> Option<i64> {
    let rank = model_rules::reasoning_rank(effort)?;
    Some(match rank {
        0 | 1 => 1024,
        2 => 4096,
        3 => 10240,
        _ => 32768,
    })
}

// ─── 响应：Anthropic SSE → Chat SSE（流式）────────────────────

/// 上游 Anthropic SSE 字节流 → 标准 chat SSE 字节流（状态机）。
///
/// 事件折法（对照 `AnthropicStream` 的反方向与参考实现
/// `anthropicStreamToChat`，字段口径按本项目 chat 侧收敛）：
///   - `message_start` → 首帧（role assistant）+ 记下 message.id 与
///     `usage.input_tokens` / `cache_read_input_tokens` / `cache_creation_input_tokens`
///   - `content_block_start`（tool_use）→ `tool_calls` 宣告帧（id / name）
///   - `content_block_delta`：`text_delta` → `delta.content`；
///     `thinking_delta` → `delta.reasoning_content`；
///     `input_json_delta` → `delta.tool_calls[…]`；`signature_delta` 丢弃
///   - `message_delta` → 记 stop_reason 与 `usage.output_tokens`
///   - `message_stop` → 收尾帧 + usage 帧 + `data: [DONE]`
///   - `error` → `data: {"error":{…}}` + `data: [DONE]`（与 ForwardStream
///     的断流收尾同形状，聚合器据此转 502）
///   - `ping` 与非帧行忽略
pub struct ChatFromAnthropicStream {
    buffer: SseLineBuffer,
    /// 输出帧的 model（发给上游的真名；ForwardStream 的回写层按需改写）
    model: String,
    id: String,
    created: i64,
    started: bool,
    finished: bool,
    /// tool_use 块（按上游 content_block 的 index 记）→ 是否已发过参数
    /// （一个分片都没来时，`content_block_stop` 要补一个空对象参数帧）
    tools: BTreeMap<i64, bool>,
    input_tokens: i64,
    cache_read: i64,
    cache_creation: i64,
    output_tokens: i64,
    /// 已映射成 chat 口径的 finish_reason（`message_delta` 里给）
    finish_reason: Option<String>,
}

impl ChatFromAnthropicStream {
    pub fn new(model: &str) -> Self {
        Self {
            buffer: SseLineBuffer::new(),
            model: model.to_string(),
            id: String::new(),
            created: crate::server::logging::now_ms() / 1000,
            started: false,
            finished: false,
            tools: BTreeMap::new(),
            input_tokens: 0,
            cache_read: 0,
            cache_creation: 0,
            output_tokens: 0,
            finish_reason: None,
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

    /// 上游流结束（没有 `message_stop` 时的兜底收尾）
    pub fn finish(&mut self) -> Vec<bytes::Bytes> {
        if self.finished {
            return Vec::new();
        }
        let mut out = Vec::new();
        for payload in self.buffer.finish() {
            if let Some(data) = payload {
                if let Ok(value) = serde_json::from_str::<Value>(&data) {
                    out.extend(self.consume(&value));
                }
            }
        }
        out.extend(self.complete());
        out
    }

    /// 一个上游 Anthropic 事件 → 零到多个 chat 帧
    fn consume(&mut self, event: &Value) -> Vec<bytes::Bytes> {
        if self.finished {
            return Vec::new();
        }
        let kind = string_field(event, "type").to_lowercase();
        match kind.as_str() {
            "ping" => Vec::new(),
            "error" => self.fail(event),
            "message_start" => {
                if let Some(id) = event
                    .pointer("/message/id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                {
                    self.id = id.to_string();
                }
                if let Some(usage) = event.pointer("/message/usage").filter(|usage| usage.is_object()) {
                    self.input_tokens =
                        usage.get("input_tokens").and_then(Value::as_i64).unwrap_or(0);
                    self.cache_read = usage
                        .get("cache_read_input_tokens")
                        .and_then(Value::as_i64)
                        .unwrap_or(0);
                    self.cache_creation = usage
                        .get("cache_creation_input_tokens")
                        .and_then(Value::as_i64)
                        .unwrap_or(0);
                }
                self.start()
            }
            "content_block_start" => {
                let block = event.get("content_block").unwrap_or(&Value::Null);
                if string_field(block, "type").to_lowercase() != "tool_use" {
                    return Vec::new();
                }
                let index = event.get("index").and_then(Value::as_i64).unwrap_or(0);
                self.tools.insert(index, false);
                let mut out = self.start();
                let call_id = {
                    let raw = string_field(block, "id");
                    if raw.is_empty() {
                        random_id("toolu")
                    } else {
                        raw
                    }
                };
                out.push(self.delta_frame(json!({
                    "tool_calls": [{
                        "index": index,
                        "id": call_id,
                        "type": "function",
                        "function": { "name": string_field(block, "name"), "arguments": "" },
                    }],
                })));
                // `block.input` 已是完整对象时（部分上游不走 input_json_delta）
                // 直接作为首段参数下发
                if let Some(map) = block.get("input").and_then(Value::as_object) {
                    if !map.is_empty() {
                        self.tools.insert(index, true);
                        out.push(self.arguments_frame(index, &json_text(&Value::Object(map.clone()))));
                    }
                }
                out
            }
            "content_block_delta" => {
                let delta = event.get("delta").unwrap_or(&Value::Null);
                let delta_kind = string_field(delta, "type").to_lowercase();
                match delta_kind.as_str() {
                    // 正文增量
                    "text_delta" => {
                        let text = string_field(delta, "text");
                        if text.is_empty() {
                            return Vec::new();
                        }
                        let mut out = self.start();
                        out.push(self.delta_frame(json!({ "content": text })));
                        out
                    }
                    // 思考增量 → reasoning_content
                    "thinking_delta" => {
                        let text = string_field(delta, "thinking");
                        if text.is_empty() {
                            return Vec::new();
                        }
                        let mut out = self.start();
                        out.push(self.delta_frame(json!({ "reasoning_content": text })));
                        out
                    }
                    // 工具参数增量（按 content_block 的 index 关联）
                    "input_json_delta" => {
                        let partial = string_field(delta, "partial_json");
                        if partial.is_empty() {
                            return Vec::new();
                        }
                        let index = event.get("index").and_then(Value::as_i64).unwrap_or(0);
                        // 没见到 content_block_start 的防御登记（上游事件残缺
                        // 时也让参数有槽位可挂；宣告帧缺失由聚合侧兜底 name/id）
                        self.tools.insert(index, true);
                        let mut out = self.start();
                        out.push(self.arguments_frame(index, &partial));
                        out
                    }
                    // 思考签名：Chat 侧无处安放（见 `anthropic.rs` 模块头）
                    "signature_delta" => Vec::new(),
                    _ => Vec::new(),
                }
            }
            // 块收尾：工具块一个参数分片都没来过时补一个空对象帧
            // （对齐 `AnthropicStream::close_block` 的反方向处理）
            "content_block_stop" => {
                let index = event.get("index").and_then(Value::as_i64).unwrap_or(0);
                if self.tools.get(&index) == Some(&false) {
                    self.tools.insert(index, true);
                    return vec![self.arguments_frame(index, "{}")];
                }
                Vec::new()
            }
            // 收尾前的最后一帧：stop_reason 与最终 usage（output_tokens）
            "message_delta" => {
                if let Some(reason) = event.pointer("/delta/stop_reason").map(string_value) {
                    let reason = reason.trim().to_lowercase();
                    if !reason.is_empty() {
                        self.finish_reason = Some(match reason.as_str() {
                            "max_tokens" => "length".to_string(),
                            "tool_use" => "tool_calls".to_string(),
                            // end_turn / stop_sequence / refusal 等：
                            // Chat 侧没有对应值，归到 stop
                            _ => "stop".to_string(),
                        });
                    }
                }
                if let Some(usage) = event.get("usage").filter(|usage| usage.is_object()) {
                    if let Some(output) = usage.get("output_tokens").and_then(Value::as_i64) {
                        self.output_tokens = output;
                    }
                }
                Vec::new()
            }
            "message_stop" => self.complete(),
            _ => Vec::new(),
        }
    }

    /// 首帧（role assistant），只发一次
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

    /// 收尾：finish_reason 帧 + usage 帧 + [DONE]（只做一次）
    fn complete(&mut self) -> Vec<bytes::Bytes> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        let mut out = self.start();
        let finish_reason = self.finish_reason.clone().unwrap_or_else(|| {
            if self.tools.is_empty() {
                "stop".to_string()
            } else {
                "tool_calls".to_string()
            }
        });
        out.push(self.finish_frame(&finish_reason));
        // chat 口径：input_tokens 含缓存部分（`usage_to_anthropic` 的反向
        // 不等式 —— 那边是「减掉缓存」，这边加回来）
        let prompt_tokens = self.input_tokens + self.cache_read + self.cache_creation;
        out.push(chat_frame(&json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [],
            "usage": {
                "prompt_tokens": prompt_tokens,
                "completion_tokens": self.output_tokens,
                "total_tokens": prompt_tokens + self.output_tokens,
            },
        })));
        out.push(bytes::Bytes::from_static(b"data: [DONE]\n\n"));
        out
    }

    /// 上游错误 → chat 错误帧 + [DONE]（只做一次）
    fn fail(&mut self, event: &Value) -> Vec<bytes::Bytes> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        let error = event.get("error").filter(|error| is_truthy(error));
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

    /// 一个 delta 帧
    fn delta_frame(&self, delta: Value) -> bytes::Bytes {
        chat_frame(&json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": Value::Null }],
        }))
    }

    /// 工具参数帧（只带 index 与 function.arguments —— 增量形态，
    /// id / name 在宣告帧里已经给过）
    fn arguments_frame(&self, index: i64, arguments: &str) -> bytes::Bytes {
        self.delta_frame(json!({
            "tool_calls": [{ "index": index, "function": { "arguments": arguments } }],
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
