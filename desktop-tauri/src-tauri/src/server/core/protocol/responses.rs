//! Responses API（`POST /v1/responses`）↔ Chat Completions 的双向转换。
//!
//! ── 方向约定 ────────────────────────────────────────────────
//! 本模块所有函数名都是 `X_from_chat` / `chat_from_X` 的读法：
//!   - `*_from_chat`：把**下游**的 Responses 请求/响应翻译成 Chat
//!   - `chat_from_*`：把**上游**的 Chat 响应翻译回 Responses
//! 命名刻意与参考实现（`responsesRequestToChat` 之类）不同：那种「A to B」
//! 在双向代码里很容易看反方向，而方向搞反是这类模块最贵的 bug。
//!
//! ── 支持范围（有意收窄的部分，都在这里交代清楚）──────────────
//! Responses 有一批**有状态**字段（`previous_response_id` / `conversation` /
//! `prompt` / `background`）—— 它们依赖服务端保存历史，而本网关是无状态的
//! 转发层（上游是各家第三方服务，没有哪家实现 Responses 的服务端状态）。
//! 这些字段**直接报 400** 而不是静默忽略：静默忽略会让客户端以为多轮上下文
//! 被记住了，实际每一轮都在裸问，症状（模型「失忆」）很难归因到网关。
//! 参考实现同样把这条列为「无法转换」。
//!
//! `store` 字段我们固定下发 `false`：上游不存，告诉客户端「存了」是撒谎。
//!
//! ── 思考内容怎么表达 ────────────────────────────────────────
//! Chat 侧是 `reasoning_content`（字符串），Responses 侧是 `output` 数组里的
//! `{type:"reasoning", summary:[{type:"summary_text", text}]}` 项。
//! 回程（Responses → Chat）时把 reasoning 项折回 `reasoning_content`。

use serde_json::{json, Map, Value};

use super::{
    content_parts, content_text, event_frame, is_truthy, json_text, random_id,
    string_field, string_value, SseLineBuffer,
};
use crate::server::logging;

/// 无法跨协议转换时的错误文案（`Err` 的载荷）。
///
/// 为什么是「文案」而不是错误类型：调用点（`api::protocol`）把它原样包成
/// 400 响应，转换层不需要知道 HTTP 状态码 —— 保持纯函数才好单独推演。
pub type ConvertError = String;

/// 有状态字段：本网关无状态，这些字段一律拒绝（理由见模块头）
const STATEFUL_FIELDS: [&str; 4] =
    ["previous_response_id", "conversation", "prompt", "background"];

// ─── 请求：Responses → Chat ─────────────────────────────────

/// Responses 请求体 → Chat Completions 请求体。
///
/// `model` 由调用方保证已填充（模型路由在 handler 里统一做，见 `api::chat`），
/// 这里只做协议翻译。
pub fn chat_from_responses(body: &Value) -> Result<Value, ConvertError> {
    for field in STATEFUL_FIELDS {
        if body.get(field).map(is_truthy).unwrap_or(false) {
            return Err(format!(
                "字段 {field} 需要服务端保存对话状态，本网关是无状态转发，无法支持"
            ));
        }
    }
    let mut out = Map::new();
    out.insert("model".to_string(), body.get("model").cloned().unwrap_or(Value::Null));

    let messages = messages_from_input(body.get("instructions"), body.get("input"))?;
    out.insert("messages".to_string(), Value::Array(messages));
    out.insert(
        "stream".to_string(),
        Value::Bool(body.get("stream").and_then(Value::as_bool).unwrap_or(false)),
    );

    // max_output_tokens → max_completion_tokens（Responses 的字段名）
    if let Some(value) = body.get("max_output_tokens").filter(|value| is_truthy(value)) {
        out.insert("max_completion_tokens".to_string(), value.clone());
    }
    for key in ["temperature", "top_p", "service_tier", "parallel_tool_calls", "user", "metadata"] {
        if let Some(value) = body.get(key) {
            if !value.is_null() {
                out.insert(key.to_string(), value.clone());
            }
        }
    }
    // 工具：Responses 的形态是 `{type:"function", name, parameters}`（扁平的），
    // Chat 要的是 `{type:"function", function:{name, parameters}}`（嵌套一层）
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        let converted: Vec<Value> = tools.iter().filter_map(tool_to_chat).collect();
        if !converted.is_empty() {
            out.insert("tools".to_string(), Value::Array(converted));
        }
    }
    if let Some(choice) = body.get("tool_choice").filter(|value| is_truthy(value)) {
        out.insert("tool_choice".to_string(), tool_choice_to_chat(choice));
    }
    // text.format → response_format
    if let Some(format) = body.pointer("/text/format").filter(|value| is_truthy(value)) {
        out.insert("response_format".to_string(), format_to_chat(format));
    }
    // reasoning.effort → reasoning_effort（本项目的上游都认这个 Chat 扩展字段）
    if let Some(effort) = body.pointer("/reasoning/effort").filter(|value| has_effort(value)) {
        out.insert("reasoning_effort".to_string(), effort.clone());
    }
    Ok(Value::Object(out))
}

fn has_effort(value: &Value) -> bool {
    !string_value(value).trim().is_empty()
}

/// `instructions` + `input` → Chat 的 `messages` 数组。
///
/// `input` 有四种形态，都要处理：
///   - 字符串（单轮用户输入）
///   - 消息项数组（`{type:"message", role, content}` 或裸 `{role, content}`）
///   - 内容块数组（`input_text` / `input_image` / …，视为一条 user 消息）
///   - 上述的混合
fn messages_from_input(
    instructions: Option<&Value>,
    input: Option<&Value>,
) -> Result<Vec<Value>, ConvertError> {
    let mut messages: Vec<Value> = Vec::new();
    if let Some(text) = instructions.and_then(Value::as_str).filter(|text| !text.trim().is_empty()) {
        messages.push(json!({ "role": "system", "content": text }));
    }
    let Some(input) = input else {
        return Ok(messages);
    };
    if let Some(text) = input.as_str() {
        messages.push(json!({ "role": "user", "content": text }));
        return Ok(messages);
    }
    let Some(items) = input.as_array() else {
        // 单个对象（非数组）也接受：客户端偶尔直接给一个 message 项
        if input.is_object() {
            push_input_item(&mut messages, input)?;
        }
        return Ok(messages);
    };
    for item in items {
        if let Some(text) = item.as_str() {
            messages.push(json!({ "role": "user", "content": text }));
            continue;
        }
        push_input_item(&mut messages, item)?;
    }
    Ok(messages)
}

/// 单个 input 项 → 零到一条 Chat 消息（追加进 `messages`）
fn push_input_item(messages: &mut Vec<Value>, item: &Value) -> Result<(), ConvertError> {
    let kind = string_field(item, "type").to_lowercase();
    match kind.as_str() {
        // 函数调用与其结果：合成 assistant(tool_calls) + tool 两条消息。
        // 上游要求 tool 消息必须紧跟对应的 assistant，所以这里一次推两条。
        "function_call" => {
            let call_id = call_id_of(item);
            let name = string_field(item, "name");
            let arguments = {
                let raw = json_text(item.get("arguments").unwrap_or(&Value::Null));
                if raw.is_empty() { "{}".to_string() } else { raw }
            };
            messages.push(json!({
                "role": "assistant",
                "content": Value::Null,
                "tool_calls": [{
                    "id": call_id,
                    "type": "function",
                    "function": { "name": name, "arguments": arguments },
                }],
            }));
        }
        "function_call_output" => {
            messages.push(json!({
                "role": "tool",
                "tool_call_id": call_id_of(item),
                "content": tool_output_text(item.get("output")),
            }));
        }
        // reasoning 项：它的正文不属于任何一轮对话，跳过（Chat 侧没有对应位置）
        "reasoning" => {}
        "input_text" | "text" => {
            messages.push(json!({ "role": "user", "content": string_field(item, "text") }));
        }
        "input_image" | "input_file" | "image_url" => {
            // 裸内容块（不在 message 里）：包成一条 user 消息
            messages.push(json!({ "role": "user", "content": [item.clone()] }));
        }
        // 消息项（含 type 缺失/为 "message" 的常规形态）
        _ => {
            let role = {
                let raw = string_field(item, "role").to_lowercase();
                if raw == "developer" { "system".to_string() } else if raw.is_empty() { "user".to_string() } else { raw }
            };
            let content = item.get("content").or_else(|| item.get("text"));
            let Some(content) = content else {
                return Ok(());
            };
            let content = content_to_chat(content, &role);
            // assistant 的空消息要丢掉：上游对「空 assistant」的处理各家不一，
            // 而 Responses 的 reasoning-only 项会退化成空消息
            if role == "assistant"
                && content_text(&content).is_empty()
                && !content_is_structured(&content)
            {
                return Ok(());
            }
            let mut message = Map::new();
            message.insert("role".to_string(), Value::String(role.clone()));
            message.insert("content".to_string(), content.clone());
            // Responses 的 assistant 项可能带 reasoning：折回 reasoning_content
            if role == "assistant" {
                if let Some(reasoning) = reasoning_text_of(item) {
                    message.insert("reasoning_content".to_string(), Value::String(reasoning));
                }
            }
            messages.push(Value::Object(message));
        }
    }
    Ok(())
}

/// 内容是否是结构化（数组且含非文本块）——用于判断「空消息」时不能只看文本
fn content_is_structured(content: &Value) -> bool {
    content_parts(content).iter().any(|part| {
        let kind = string_field(part, "type").to_lowercase();
        !matches!(kind.as_str(), "" | "text" | "input_text" | "output_text")
    })
}

/// 一个 reasoning 项的正文（summary 或 content 里的 text 拼接）
fn reasoning_text_of(item: &Value) -> Option<String> {
    let parts = item
        .get("summary")
        .and_then(Value::as_array)
        .or_else(|| item.get("content").and_then(Value::as_array))?;
    let text: String = parts
        .iter()
        .map(|part| string_field(part, "text"))
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty() { None } else { Some(text) }
}

/// 工具输出 → 文本（Chat 的 tool 消息 content 只接受字符串）
fn tool_output_text(output: Option<&Value>) -> String {
    let Some(output) = output else {
        return "(empty)".to_string();
    };
    match output {
        Value::String(text) => {
            if text.is_empty() { "(empty)".to_string() } else { text.clone() }
        }
        Value::Null => "(empty)".to_string(),
        // 结构化输出（含图片等）：拍平成 JSON 文本，Chat 侧没有更好的表达
        other => {
            let text = json_text(other);
            if text.is_empty() { "(empty)".to_string() } else { text }
        }
    }
}

/// 调用 id：`call_id` 优先，退到 `id`，都没有则生成一个
fn call_id_of(item: &Value) -> String {
    let call_id = string_field(item, "call_id");
    if !call_id.is_empty() {
        return call_id;
    }
    let id = string_field(item, "id");
    if !id.is_empty() {
        return id;
    }
    random_id("call")
}

/// Responses 内容块 → Chat content（字符串或块数组）。
///
/// 纯文本时**降级成字符串**：Chat 的 content 允许字符串，而字符串形态对上游
/// 更友好（部分上游对块数组的处理不完整），也与「客户端本来就想说一句话」等价。
fn content_to_chat(content: &Value, role: &str) -> Value {
    if let Some(text) = content.as_str() {
        return Value::String(text.to_string());
    }
    let parts = content_parts(content);
    if parts.is_empty() {
        return Value::String(String::new());
    }
    let mut out: Vec<Value> = Vec::new();
    let mut only_text = true;
    for part in parts {
        if let Some(text) = part.as_str() {
            out.push(json!({ "type": "text", "text": text }));
            continue;
        }
        let kind = string_field(part, "type").to_lowercase();
        match kind.as_str() {
            "text" | "input_text" | "output_text" => {
                out.push(json!({ "type": "text", "text": string_field(part, "text") }));
            }
            "input_image" | "image_url" => {
                only_text = false;
                let url = image_url_of(part);
                out.push(json!({ "type": "image_url", "image_url": { "url": url } }));
            }
            // 文件/音频/视频：Chat 侧没有统一表达，转成文本占位会让模型看到
            // 一段无意义的 JSON —— 这里保留原始块，由上游自己决定认不认
            _ => {
                only_text = false;
                out.push(part.clone());
            }
        }
    }
    // 只有一个文本块时降级成字符串（与上面同理由）
    if only_text && out.len() == 1 {
        if let Some(text) = out[0].get("text").and_then(Value::as_str) {
            return Value::String(text.to_string());
        }
    }
    let _ = role;
    Value::Array(out)
}

/// 图片块的 url（`image_url` 可以是字符串或 `{url}` 对象）
fn image_url_of(part: &Value) -> String {
    let source = part.get("image_url").unwrap_or(part);
    match source {
        Value::String(url) => url.clone(),
        other => string_field(other, "url"),
    }
}

/// Responses 工具 → Chat 工具（扁平 → 嵌套 `function`）
fn tool_to_chat(tool: &Value) -> Option<Value> {
    // 字符串形态的工具名（`tools: ["web_search"]`）：不是 function，丢掉
    if tool.is_string() {
        return None;
    }
    let kind = string_field(tool, "type").to_lowercase();
    // 已经嵌套好的（客户端混用两种形态）：原样保留
    if kind == "function" && tool.get("function").is_some() {
        return Some(tool.clone());
    }
    if kind != "function" && !kind.is_empty() {
        // 非 function 类型（web_search 等）：上游的 Chat 接口不认，丢掉而不是
        // 发过去让上游报错 —— 客户端要的是「能跑」，不是「原样报错」
        return None;
    }
    let name = string_field(tool, "name");
    if name.is_empty() {
        return None;
    }
    let parameters = tool
        .get("parameters")
        .filter(|value| is_truthy(value))
        .cloned()
        .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
    let mut function = Map::new();
    function.insert("name".to_string(), Value::String(name));
    if let Some(description) = tool.get("description").filter(|value| is_truthy(value)) {
        function.insert("description".to_string(), description.clone());
    }
    function.insert("parameters".to_string(), parameters);
    // strict 是 Responses 的字段，Chat 的 function 里也认（部分上游支持）
    if let Some(strict) = tool.get("strict") {
        function.insert("strict".to_string(), strict.clone());
    }
    Some(json!({ "type": "function", "function": Value::Object(function) }))
}

/// Responses 的 tool_choice → Chat 的 tool_choice
fn tool_choice_to_chat(choice: &Value) -> Value {
    if let Some(text) = choice.as_str() {
        return match text {
            // Responses 的 "required" 在 Chat 里也是 "required"，语义一致
            other => Value::String(other.to_string()),
        };
    }
    // `{type:"function", name}` → `{type:"function", function:{name}}`
    let name = {
        let flat = string_field(choice, "name");
        if flat.is_empty() { string_field(choice, "function.name") } else { flat }
    };
    // `function.name` 是嵌套路径，string_field 取不到，单独处理
    let name = if name.is_empty() {
        choice.pointer("/function/name").map(string_value).unwrap_or_default()
    } else {
        name
    };
    if !name.is_empty() {
        return json!({ "type": "function", "function": { "name": name } });
    }
    choice.clone()
}

/// Responses 的 `text.format` → Chat 的 `response_format`
fn format_to_chat(format: &Value) -> Value {
    let kind = string_field(format, "type");
    if kind != "json_schema" {
        return format.clone();
    }
    // Responses 是扁平的 `{type, name, schema, strict}`，
    // Chat 要 `{type, json_schema:{name, schema, strict}}`
    let mut inner = Map::new();
    for key in ["name", "description", "schema", "strict"] {
        if let Some(value) = format.get(key).filter(|value| is_truthy(value)) {
            inner.insert(key.to_string(), value.clone());
        }
    }
    json!({ "type": "json_schema", "json_schema": Value::Object(inner) })
}

// ─── 响应：Chat → Responses（非流式）─────────────────────────

/// Chat 的 `chat.completion` → Responses 的 `response` 对象。
///
/// `request` 是**原始的 Responses 请求体**：回程要把一批请求侧字段
/// （instructions / temperature / tools / …）如实回显，客户端据此确认
/// 「我发的参数被接受了」。
pub fn responses_from_chat(chat: &Value, model: &str, request: &Value) -> Value {
    let choice = chat.pointer("/choices/0");
    let message = choice.and_then(|choice| choice.get("message")).cloned().unwrap_or(Value::Null);
    let finish = choice
        .and_then(|choice| choice.get("finish_reason"))
        .map(string_value)
        .unwrap_or_default();

    let mut output: Vec<Value> = Vec::new();
    let reasoning = {
        let from_field = string_field(&message, "reasoning_content");
        if from_field.is_empty() { string_field(&message, "reasoning") } else { from_field }
    };
    if !reasoning.is_empty() {
        output.push(json!({
            "type": "reasoning",
            "id": random_id("rs"),
            "summary": [{ "type": "summary_text", "text": reasoning }],
        }));
    }
    let text = content_text(message.get("content").unwrap_or(&Value::Null));
    let tool_calls = message.get("tool_calls").and_then(Value::as_array);
    // 没有工具调用时始终给一条 message 项（哪怕文本为空）：
    // 客户端的 `output_text` 解析依赖它存在，缺失会被当成「响应损坏」
    if !text.is_empty() || tool_calls.is_none() {
        output.push(json!({
            "type": "message",
            "id": random_id("msg"),
            "status": "completed",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": text, "annotations": [], "logprobs": [] }],
        }));
    }
    if let Some(calls) = tool_calls {
        for call in calls {
            output.push(json!({
                "type": "function_call",
                "id": random_id("fc"),
                "status": "completed",
                "call_id": string_field(call, "id"),
                "name": call.pointer("/function/name").map(string_value).unwrap_or_default(),
                "arguments": call
                    .pointer("/function/arguments")
                    .map(json_text)
                    .unwrap_or_else(|| "{}".to_string()),
            }));
        }
    }

    let incomplete = if finish == "length" {
        Some("max_output_tokens")
    } else if finish == "content_filter" {
        Some("content_filter")
    } else {
        None
    };
    let status = if incomplete.is_some() { "incomplete" } else { "completed" };
    let created = chat
        .get("created")
        .and_then(Value::as_i64)
        .unwrap_or_else(|| logging::now_ms() / 1000);
    let id = {
        let raw = string_field(chat, "id");
        if raw.is_empty() { random_id("resp") } else { raw }
    };
    let mut body = response_envelope(
        &id,
        model,
        status,
        output,
        usage_to_responses(chat.get("usage")),
        request,
        created,
        incomplete,
    );
    if let Some(map) = body.as_object_mut() {
        // output_text 是官方 SDK 的便捷字段（把所有 message 项的文本拼起来）
        map.insert("output_text".to_string(), Value::String(text));
    }
    body
}

/// Responses 响应体的信封（流式的 `response.completed` 也用同一个构造器）
#[allow(clippy::too_many_arguments)]
pub fn response_envelope(
    id: &str,
    model: &str,
    status: &str,
    output: Vec<Value>,
    usage: Value,
    request: &Value,
    created: i64,
    incomplete_reason: Option<&str>,
) -> Value {
    // 请求侧字段如实回显（缺省值与官方文档一致）
    let passthrough = |key: &str, fallback: Value| -> Value {
        request.get(key).filter(|value| !value.is_null()).cloned().unwrap_or(fallback)
    };
    let reasoning_effort = request
        .pointer("/reasoning/effort")
        .filter(|value| !value.is_null())
        .cloned()
        .unwrap_or(Value::Null);
    let reasoning_summary = request
        .pointer("/reasoning/summary")
        .filter(|value| !value.is_null())
        .cloned()
        .unwrap_or(Value::Null);
    json!({
        "id": id,
        "object": "response",
        "created_at": created,
        "status": status,
        "completed_at": if status == "completed" { Value::from(logging::now_ms() / 1000) } else { Value::Null },
        "background": false,
        "error": Value::Null,
        "incomplete_details": match incomplete_reason {
            Some(reason) => json!({ "reason": reason }),
            None => Value::Null,
        },
        "instructions": passthrough("instructions", Value::Null),
        "max_output_tokens": passthrough("max_output_tokens", Value::Null),
        "max_tool_calls": passthrough("max_tool_calls", Value::Null),
        "model": model,
        "output": output,
        "parallel_tool_calls": passthrough("parallel_tool_calls", Value::Bool(true)),
        "previous_response_id": Value::Null,
        "reasoning": { "effort": reasoning_effort, "summary": reasoning_summary },
        "store": false,
        "temperature": passthrough("temperature", Value::from(1)),
        "text": passthrough("text", json!({ "format": { "type": "text" } })),
        "tool_choice": passthrough("tool_choice", Value::String("auto".to_string())),
        "tools": request.get("tools").filter(|value| value.is_array()).cloned().unwrap_or_else(|| json!([])),
        "top_logprobs": passthrough("top_logprobs", Value::from(0)),
        "top_p": passthrough("top_p", Value::from(1)),
        "truncation": passthrough("truncation", Value::String("disabled".to_string())),
        "usage": usage,
        "user": passthrough("user", Value::Null),
        "metadata": passthrough("metadata", json!({})),
    })
}

/// Chat 的 usage → Responses 的 usage（字段名与明细结构都不同）
pub fn usage_to_responses(usage: Option<&Value>) -> Value {
    let Some(usage) = usage.filter(|value| value.is_object()) else {
        return Value::Null;
    };
    let number = |keys: &[&str]| -> i64 {
        for key in keys {
            if let Some(value) = usage.get(*key) {
                if let Some(parsed) = value.as_i64() {
                    return parsed;
                }
                if let Some(parsed) = value.as_f64().filter(|value| value.is_finite()) {
                    return parsed as i64;
                }
            }
        }
        0
    };
    let input = number(&["prompt_tokens", "input_tokens"]);
    let output = number(&["completion_tokens", "output_tokens"]);
    let cached = number(&[
        "prompt_tokens_details.cached_tokens",
        "prompt_cache_hit_tokens",
        "cache_read_input_tokens",
    ]);
    // 嵌套明细要单独取（上面的点号键取不到，这里补一次）
    let cached = {
        let nested = usage
            .pointer("/prompt_tokens_details/cached_tokens")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        if nested != 0 { nested } else { cached }
    };
    let reasoning = usage
        .pointer("/completion_tokens_details/reasoning_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let mut details = Map::new();
    if cached != 0 {
        details.insert("cached_tokens".to_string(), Value::from(cached));
    }
    let mut output_details = Map::new();
    if reasoning != 0 {
        output_details.insert("reasoning_tokens".to_string(), Value::from(reasoning));
    }
    let mut out = Map::new();
    out.insert("input_tokens".to_string(), Value::from(input));
    if !details.is_empty() {
        out.insert("input_tokens_details".to_string(), Value::Object(details));
    }
    out.insert("output_tokens".to_string(), Value::from(output));
    if !output_details.is_empty() {
        out.insert("output_tokens_details".to_string(), Value::Object(output_details));
    }
    out.insert(
        "total_tokens".to_string(),
        Value::from(number(&["total_tokens"]).max(input + output)),
    );
    Value::Object(out)
}

// ─── 流式：Chat SSE → Responses SSE ─────────────────────────

/// Chat 的 SSE 字节流 → Responses 的 SSE 字节流（状态机）。
///
/// ── Responses 流的事件顺序（客户端按这个顺序解析）───────────
///   response.created
///   response.output_item.added（每个 output 项一次）
///   response.content_part.added（message 项才有）
///   response.output_text.delta / response.function_call_arguments.delta（增量）
///   response.output_text.done / …（收尾）
///   response.output_item.done
///   response.completed（带完整 response 对象）
///
/// 顺序错乱会让官方 SDK 报「事件顺序非法」，所以下面的 `open_text` /
/// `close_text` / `open_tool` 严格维护「同一时刻只有一个项在写」。
pub struct ResponsesStream {
    buffer: SseLineBuffer,
    /// 下发给客户端的 response id（取上游首个 chunk 的 id，兜底自造）
    response_id: String,
    created: i64,
    model: String,
    request: Value,
    /// 事件序号（Responses 要求每个事件带单调递增的 sequence_number）
    sequence: i64,
    created_sent: bool,
    finished: bool,
    /// 文本项是否已打开 + 它的 output_index / item_id
    text_open: bool,
    text_index: i64,
    text_id: String,
    text: String,
    /// 思考项（reasoning）：先于文本出现，遇到文本或工具时要先收尾
    reasoning_open: bool,
    reasoning_closed: bool,
    reasoning_index: i64,
    reasoning_id: String,
    reasoning: String,
    /// 工具调用：按上游给的 index 累积（同一 index 的分片属于同一次调用）
    tools: std::collections::BTreeMap<i64, ToolAccum>,
    /// 下一个可用的 output_index
    next_index: i64,
    usage: Option<Value>,
    finish_reason: Option<String>,
    /// 已完成的 output 项（收尾时按 output_index 排序进 response.output）
    output_items: Vec<(i64, Value)>,
}

#[derive(Default)]
struct ToolAccum {
    index: i64,
    item_id: String,
    call_id: String,
    name: String,
    arguments: String,
    announced: bool,
}

impl ResponsesStream {
    pub fn new(model: &str, request: &Value) -> Self {
        Self {
            buffer: SseLineBuffer::new(),
            response_id: random_id("resp"),
            created: logging::now_ms() / 1000,
            model: model.to_string(),
            request: request.clone(),
            sequence: 0,
            created_sent: false,
            finished: false,
            text_open: false,
            text_index: -1,
            text_id: String::new(),
            text: String::new(),
            reasoning_open: false,
            reasoning_closed: false,
            reasoning_index: -1,
            reasoning_id: String::new(),
            reasoning: String::new(),
            tools: std::collections::BTreeMap::new(),
            next_index: 0,
            usage: None,
            finish_reason: None,
            output_items: Vec::new(),
        }
    }

    /// 吃一段上游字节，吐出要下发的 SSE 字节
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

    /// 上游流结束（没有 [DONE] 时的兜底收尾）
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
        if self.finished {
            return out;
        }
        self.finished = true;
        out.extend(self.emit_created());
        out.extend(self.close_reasoning());
        // 上游什么都没给（空流）：补一条空 message，否则客户端解析不到 output
        if !self.text_open && self.tools.is_empty() {
            out.extend(self.open_text());
        }
        out.extend(self.close_text());
        out.extend(self.close_tools());
        let output: Vec<Value> = {
            let mut items = self.output_items.clone();
            items.sort_by_key(|(index, _)| *index);
            items.into_iter().map(|(_, item)| item).collect()
        };
        let incomplete = match self.finish_reason.as_deref() {
            Some("length") => Some("max_output_tokens"),
            Some("content_filter") => Some("content_filter"),
            _ => None,
        };
        let status = if incomplete.is_some() { "incomplete" } else { "completed" };
        let response = response_envelope(
            &self.response_id,
            &self.model,
            status,
            output,
            usage_to_responses(self.usage.as_ref()),
            &self.request,
            self.created,
            incomplete,
        );
        out.push(self.event("response.completed", json!({ "response": response })));
        out
    }

    /// 一个上游 Chat chunk → 零到多个 Responses 事件
    fn consume(&mut self, chunk: &Value) -> Vec<bytes::Bytes> {
        if self.finished {
            return Vec::new();
        }
        let mut out = Vec::new();
        // 上游错误帧：转成 Responses 的 error + response.failed
        if let Some(error) = chunk.get("error").filter(|value| is_truthy(value)) {
            self.finished = true;
            out.extend(self.emit_created());
            let message = {
                let text = string_field(error, "message");
                if text.is_empty() { string_value(error) } else { text }
            };
            let code = error.get("code").filter(|value| is_truthy(value)).cloned().unwrap_or(Value::Null);
            out.push(self.event(
                "error",
                json!({ "code": code, "message": message, "param": Value::Null }),
            ));
            let mut failed = response_envelope(
                &self.response_id,
                &self.model,
                "failed",
                Vec::new(),
                usage_to_responses(self.usage.as_ref()),
                &self.request,
                self.created,
                None,
            );
            if let Some(map) = failed.as_object_mut() {
                map.insert(
                    "error".to_string(),
                    json!({ "code": code, "message": message }),
                );
            }
            out.push(self.event("response.failed", json!({ "response": failed })));
            return out;
        }
        if let Some(id) = chunk.get("id").and_then(Value::as_str).filter(|id| !id.is_empty()) {
            if !self.created_sent {
                self.response_id = id.to_string();
            }
        }
        if let Some(usage) = chunk.get("usage").filter(|value| value.is_object()) {
            self.usage = Some(usage.clone());
        }
        let choice = chunk.pointer("/choices/0");
        if let Some(finish) = choice
            .and_then(|choice| choice.get("finish_reason"))
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            self.finish_reason = Some(finish.to_string());
        }
        out.extend(self.emit_created());
        let Some(delta) = choice.and_then(|choice| choice.get("delta")) else {
            return out;
        };
        // 思考增量
        let reasoning = {
            let from_field = string_field(delta, "reasoning_content");
            if from_field.is_empty() { string_field(delta, "reasoning") } else { from_field }
        };
        if !reasoning.is_empty() {
            out.extend(self.open_reasoning());
            self.reasoning.push_str(&reasoning);
            out.push(self.event(
                "response.reasoning_summary_text.delta",
                json!({
                    "output_index": self.reasoning_index,
                    "item_id": self.reasoning_id,
                    "summary_index": 0,
                    "delta": reasoning,
                }),
            ));
        }
        // 正文增量：思考必须先收尾（同一时刻只能有一个项在写）
        if let Some(text) = delta.get("content").and_then(Value::as_str).filter(|text| !text.is_empty()) {
            out.extend(self.close_reasoning());
            out.extend(self.open_text());
            self.text.push_str(text);
            out.push(self.event(
                "response.output_text.delta",
                json!({
                    "output_index": self.text_index,
                    "item_id": self.text_id,
                    "content_index": 0,
                    "delta": text,
                }),
            ));
        }
        // 工具调用增量
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                out.extend(self.consume_tool(call));
            }
        }
        out
    }

    fn consume_tool(&mut self, call: &Value) -> Vec<bytes::Bytes> {
        let mut out = Vec::new();
        let key = call.get("index").and_then(Value::as_i64).unwrap_or(0);
        // 工具调用与正文互斥：先把文本与思考收尾
        out.extend(self.close_reasoning());
        out.extend(self.close_text());

        // ── 状态更新与事件构造**分两段**写 ──────────────────────────
        // 事件构造要动 `self.sequence`，而状态更新要借 `self.tools`。
        // 两者是不同字段，但写成「借用未结束就调 `&mut self` 的方法」会被
        // 借用检查拒绝 —— 所以状态更新收在一个块里（借用随块结束），
        // 块外只留值，再拼事件。
        let (index, item_id, call_id, name, announced_now, buffered) = {
            if !self.tools.contains_key(&key) {
                let next = self.next_index;
                self.next_index += 1;
                self.tools.insert(
                    key,
                    ToolAccum {
                        index: next,
                        item_id: random_id("fc"),
                        call_id: String::new(),
                        name: String::new(),
                        arguments: String::new(),
                        announced: false,
                    },
                );
            }
            let Some(tool) = self.tools.get_mut(&key) else {
                return out;
            };
            if let Some(id) = call.get("id").and_then(Value::as_str).filter(|id| !id.is_empty()) {
                tool.call_id = id.to_string();
            }
            if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
                if !name.is_empty() {
                    tool.name = name.to_string();
                }
            }
            if let Some(arguments) = call.pointer("/function/arguments").and_then(Value::as_str) {
                tool.arguments.push_str(arguments);
            }
            // 首次拿到名字才能宣告这一项（宣告要带 name）
            let announced_now = !tool.announced && !tool.name.is_empty();
            if announced_now {
                if tool.call_id.is_empty() {
                    tool.call_id = random_id("call");
                }
                tool.announced = true;
            }
            (
                tool.index,
                tool.item_id.clone(),
                tool.call_id.clone(),
                tool.name.clone(),
                announced_now,
                tool.arguments.clone(),
            )
        };

        if announced_now {
            out.push(response_event(
                &mut self.sequence,
                "response.output_item.added",
                json!({
                    "output_index": index,
                    "item": {
                        "type": "function_call",
                        "id": item_id,
                        "status": "in_progress",
                        "call_id": call_id,
                        "name": name,
                        "arguments": "",
                    },
                }),
            ));
            // 宣告前已攒下的参数要补发（名字与参数可能同一帧到达）
            if !buffered.is_empty() {
                out.push(response_event(
                    &mut self.sequence,
                    "response.function_call_arguments.delta",
                    json!({
                        "output_index": index,
                        "item_id": item_id,
                        "call_id": call_id,
                        "name": name,
                        "delta": buffered,
                    }),
                ));
            }
        }
        let arguments = call
            .pointer("/function/arguments")
            .and_then(Value::as_str)
            .unwrap_or("");
        if !arguments.is_empty() && !announced_now {
            out.push(response_event(
                &mut self.sequence,
                "response.function_call_arguments.delta",
                json!({
                    "output_index": index,
                    "item_id": item_id,
                    "call_id": call_id,
                    "name": name,
                    "delta": arguments,
                }),
            ));
        }
        out
    }

    fn emit_created(&mut self) -> Vec<bytes::Bytes> {
        if self.created_sent {
            return Vec::new();
        }
        self.created_sent = true;
        let response = response_envelope(
            &self.response_id,
            &self.model,
            "in_progress",
            Vec::new(),
            Value::Null,
            &self.request,
            self.created,
            None,
        );
        vec![self.event("response.created", json!({ "response": response }))]
    }

    fn open_reasoning(&mut self) -> Vec<bytes::Bytes> {
        if self.reasoning_open {
            return Vec::new();
        }
        self.reasoning_open = true;
        self.reasoning_index = self.next_index;
        self.next_index += 1;
        self.reasoning_id = random_id("rs");
        vec![
            self.event(
                "response.output_item.added",
                json!({
                    "output_index": self.reasoning_index,
                    "item": {
                        "type": "reasoning",
                        "id": self.reasoning_id,
                        "status": "in_progress",
                        "summary": [],
                    },
                }),
            ),
            self.event(
                "response.reasoning_summary_part.added",
                json!({
                    "output_index": self.reasoning_index,
                    "item_id": self.reasoning_id,
                    "summary_index": 0,
                    "part": { "type": "summary_text", "text": "" },
                }),
            ),
        ]
    }

    fn close_reasoning(&mut self) -> Vec<bytes::Bytes> {
        if !self.reasoning_open || self.reasoning_closed {
            return Vec::new();
        }
        self.reasoning_closed = true;
        let item = json!({
            "type": "reasoning",
            "id": self.reasoning_id,
            "status": "completed",
            "summary": [{ "type": "summary_text", "text": self.reasoning }],
        });
        self.output_items.push((self.reasoning_index, item.clone()));
        vec![
            self.event(
                "response.reasoning_summary_text.done",
                json!({
                    "output_index": self.reasoning_index,
                    "item_id": self.reasoning_id,
                    "summary_index": 0,
                    "text": self.reasoning,
                }),
            ),
            self.event(
                "response.reasoning_summary_part.done",
                json!({
                    "output_index": self.reasoning_index,
                    "item_id": self.reasoning_id,
                    "summary_index": 0,
                    "part": { "type": "summary_text", "text": self.reasoning },
                }),
            ),
            self.event(
                "response.output_item.done",
                json!({ "output_index": self.reasoning_index, "item": item }),
            ),
        ]
    }

    fn open_text(&mut self) -> Vec<bytes::Bytes> {
        if self.text_open {
            return Vec::new();
        }
        self.text_open = true;
        self.text_index = self.next_index;
        self.next_index += 1;
        self.text_id = random_id("msg");
        vec![
            self.event(
                "response.output_item.added",
                json!({
                    "output_index": self.text_index,
                    "item": {
                        "type": "message",
                        "id": self.text_id,
                        "status": "in_progress",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": "", "annotations": [] }],
                    },
                }),
            ),
            self.event(
                "response.content_part.added",
                json!({
                    "output_index": self.text_index,
                    "item_id": self.text_id,
                    "content_index": 0,
                    "part": { "type": "output_text", "text": "", "annotations": [] },
                }),
            ),
        ]
    }

    fn close_text(&mut self) -> Vec<bytes::Bytes> {
        if !self.text_open {
            return Vec::new();
        }
        self.text_open = false;
        let part = json!({
            "type": "output_text",
            "text": self.text,
            "annotations": [],
            "logprobs": [],
        });
        let item = json!({
            "type": "message",
            "id": self.text_id,
            "status": "completed",
            "role": "assistant",
            "content": [part.clone()],
        });
        self.output_items.push((self.text_index, item.clone()));
        vec![
            self.event(
                "response.output_text.done",
                json!({
                    "output_index": self.text_index,
                    "item_id": self.text_id,
                    "content_index": 0,
                    "text": self.text,
                }),
            ),
            self.event(
                "response.content_part.done",
                json!({
                    "output_index": self.text_index,
                    "item_id": self.text_id,
                    "content_index": 0,
                    "part": part,
                }),
            ),
            self.event(
                "response.output_item.done",
                json!({ "output_index": self.text_index, "item": item }),
            ),
        ]
    }

    fn close_tools(&mut self) -> Vec<bytes::Bytes> {
        let mut out = Vec::new();
        // 按 output_index 顺序收尾（与宣告顺序一致）
        let mut keys: Vec<i64> = self.tools.keys().copied().collect();
        keys.sort_by_key(|key| self.tools.get(key).map(|tool| tool.index).unwrap_or(0));
        // 先把每个工具收成「待收尾的值」，再统一构造事件 —— 与 consume_tool
        // 同一理由：事件构造要动 `self.sequence`，不能与 `self.tools` 的借用重叠
        let mut finals: Vec<(i64, String, String, String, String)> = Vec::new();
        for key in keys {
            let Some(tool) = self.tools.get_mut(&key) else {
                continue;
            };
            let call_id = if tool.call_id.is_empty() { random_id("call") } else { tool.call_id.clone() };
            // 只拿到 index、没拿到名字的残片：不能宣告（宣告要 name），
            // 但也不能丢 —— 补一个占位名，否则客户端少一次工具调用
            let name = if tool.name.is_empty() { "unknown".to_string() } else { tool.name.clone() };
            let arguments = if tool.arguments.is_empty() { "{}".to_string() } else { tool.arguments.clone() };
            let item = json!({
                "type": "function_call",
                "id": tool.item_id,
                "status": "completed",
                "call_id": call_id,
                "name": name,
                "arguments": arguments,
            });
            self.output_items.push((tool.index, item.clone()));
            if tool.announced {
                out.push(response_event(
                    &mut self.sequence,
                    "response.function_call_arguments.done",
                    json!({
                        "output_index": tool.index,
                        "item_id": tool.item_id,
                        "call_id": call_id,
                        "name": name,
                        "arguments": arguments,
                    }),
                ));
            }
            out.push(response_event(
                &mut self.sequence,
                "response.output_item.done",
                json!({ "output_index": tool.index, "item": item }),
            ));
            finals.push((tool.index, call_id, name, arguments, tool.item_id.clone()));
        }
        let _ = finals;
        out
    }

    /// 构造一个带 sequence_number 的事件帧。
    ///
    /// 内部转调自由函数 [`response_event`]：事件构造要动 `sequence`，
    /// 而调用点常常正借着 `self.tools` —— 写成 `&mut self` 方法会撞借用检查，
    /// 所以真正的实现在自由函数里，调用点传 `&mut self.sequence` 即可。
    fn event(&mut self, event: &str, data: Value) -> bytes::Bytes {
        response_event(&mut self.sequence, event, data)
    }
}

/// 构造一个带 `sequence_number` 的 Responses 事件帧。
///
/// Responses 协议要求每个事件带**单调递增**的序号，客户端（官方 SDK）
/// 会用它检测事件乱序与丢帧。序号由调用方持有的计数器提供。
pub fn response_event(sequence: &mut i64, event: &str, mut data: Value) -> bytes::Bytes {
    let current = *sequence;
    *sequence += 1;
    if let Some(map) = data.as_object_mut() {
        map.insert("type".to_string(), Value::String(event.to_string()));
        map.insert("sequence_number".to_string(), Value::from(current));
    }
    event_frame(event, &data)
}

/// 非流式聚合：把 Chat SSE 字节流收成一个 Responses 响应对象。
///
/// 非流式 Responses 请求也要上游走流式（各家上游的流式才是完整能力），
/// 收完后聚合成 JSON 返回 —— 与 `/v1/chat/completions` 的非流式路径同一思路。
pub struct ResponsesCollector {
    buffer: SseLineBuffer,
    text: String,
    reasoning: String,
    tools: std::collections::BTreeMap<i64, ToolAccum>,
    usage: Option<Value>,
    finish_reason: Option<String>,
    id: String,
    created: i64,
}

impl ResponsesCollector {
    pub fn new() -> Self {
        Self {
            buffer: SseLineBuffer::new(),
            text: String::new(),
            reasoning: String::new(),
            tools: std::collections::BTreeMap::new(),
            usage: None,
            finish_reason: None,
            id: String::new(),
            created: logging::now_ms() / 1000,
        }
    }

    pub fn push(&mut self, chunk: &[u8]) {
        for payload in self.buffer.push(chunk) {
            if let Some(data) = payload {
                if let Ok(value) = serde_json::from_str::<Value>(&data) {
                    self.consume(&value);
                }
            }
        }
    }

    pub fn finish(&mut self) {
        for payload in self.buffer.finish() {
            if let Some(data) = payload {
                if let Ok(value) = serde_json::from_str::<Value>(&data) {
                    self.consume(&value);
                }
            }
        }
    }

    fn consume(&mut self, chunk: &Value) {
        if let Some(id) = chunk.get("id").and_then(Value::as_str).filter(|id| !id.is_empty()) {
            if self.id.is_empty() {
                self.id = id.to_string();
            }
        }
        if let Some(created) = chunk.get("created").and_then(Value::as_i64) {
            self.created = created;
        }
        if let Some(usage) = chunk.get("usage").filter(|value| value.is_object()) {
            self.usage = Some(usage.clone());
        }
        let choice = chunk.pointer("/choices/0");
        if let Some(finish) = choice
            .and_then(|choice| choice.get("finish_reason"))
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            self.finish_reason = Some(finish.to_string());
        }
        let Some(delta) = choice.and_then(|choice| choice.get("delta")) else {
            return;
        };
        let reasoning = {
            let from_field = string_field(delta, "reasoning_content");
            if from_field.is_empty() { string_field(delta, "reasoning") } else { from_field }
        };
        self.reasoning.push_str(&reasoning);
        if let Some(text) = delta.get("content").and_then(Value::as_str) {
            self.text.push_str(text);
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                let key = call.get("index").and_then(Value::as_i64).unwrap_or(0);
                let entry = self.tools.entry(key).or_default();
                if let Some(id) = call.get("id").and_then(Value::as_str).filter(|id| !id.is_empty()) {
                    entry.call_id = id.to_string();
                }
                if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
                    if !name.is_empty() {
                        entry.name = name.to_string();
                    }
                }
                if let Some(arguments) = call.pointer("/function/arguments").and_then(Value::as_str) {
                    entry.arguments.push_str(arguments);
                }
            }
        }
    }

    /// 收成一个 Responses 响应对象
    pub fn into_response(self, model: &str, request: &Value) -> Value {
        let mut output: Vec<Value> = Vec::new();
        if !self.reasoning.is_empty() {
            output.push(json!({
                "type": "reasoning",
                "id": random_id("rs"),
                "summary": [{ "type": "summary_text", "text": self.reasoning }],
            }));
        }
        if !self.text.is_empty() || self.tools.is_empty() {
            output.push(json!({
                "type": "message",
                "id": random_id("msg"),
                "status": "completed",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": self.text, "annotations": [], "logprobs": [] }],
            }));
        }
        for (_, tool) in self.tools {
            let arguments = if tool.arguments.is_empty() { "{}".to_string() } else { tool.arguments };
            output.push(json!({
                "type": "function_call",
                "id": random_id("fc"),
                "status": "completed",
                "call_id": if tool.call_id.is_empty() { random_id("call") } else { tool.call_id },
                "name": if tool.name.is_empty() { "unknown".to_string() } else { tool.name },
                "arguments": arguments,
            }));
        }
        let incomplete = match self.finish_reason.as_deref() {
            Some("length") => Some("max_output_tokens"),
            Some("content_filter") => Some("content_filter"),
            _ => None,
        };
        let status = if incomplete.is_some() { "incomplete" } else { "completed" };
        let id = if self.id.is_empty() { random_id("resp") } else { self.id.clone() };
        let mut body = response_envelope(
            &id,
            model,
            status,
            output,
            usage_to_responses(self.usage.as_ref()),
            request,
            self.created,
            incomplete,
        );
        if let Some(map) = body.as_object_mut() {
            map.insert("output_text".to_string(), Value::String(self.text));
        }
        body
    }
}

impl Default for ResponsesCollector {
    fn default() -> Self {
        Self::new()
    }
}
