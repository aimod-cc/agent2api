//! CatPaw 入站消息归一化（Agent2API 二期 W3p-T-d1；移植来源
//! `catpaw-upstream-messages.mjs` 第 1-356 行 + `proxy-chat-utils.mjs` 的
//! `normalizeImageBlock`）。**纯函数：不做网络请求、不持锁、不读盘。**
//!
//! ── 上游要什么（UPSTREAM_PROTOCOL.md §4 是唯一权威）──────────
//! CatPaw 的 `round` / `turn` 不接受 OpenAI 的 `messages`，只接受它自己的
//! **消息块数组**：
//! ```text
//! user:      { type:"user",      messageId, content:[text | image_url], finished }
//! assistant: { type:"assistant", messageId, content:[text | tool_use],  finished }
//! tool:      { type:"tool",      messageId, content:[tool_result],      finished }
//! ```
//! 本模块把 OpenAI chat.completions 的 `messages` 翻译成这个形态，并把两条
//! 不属于 `messages` 的东西**抽离**出来（round 请求体里它们是独立字段）：
//! `systemPromptContext.systemPromptOverride`（来自 `system` 消息）与
//! `rulesMessage`（来自 `developer` 消息）。
//!
//! ── 归一化规则总览（每条都对应原实现的一段，改前请先读它）────
//!   1. `system` / `developer` 抽离，不进 `messages`；且必须位于所有对话
//!      消息之前（顺序错了，模型的「指令 vs 历史」分层就错了）；
//!   2. 连续多条 `tool` 消息（OpenAI 并行工具调用）**合并**为单条 tool 消息的
//!      多个 `tool_result` 块 —— 上游明确拒绝 tool 消息连续出现；
//!   3. 相邻 `assistant` 消息合并（上游要求 assistant 不带 toolCall 时
//!      后一条必须是 user）；两条都带 tool_use 时**无法安全合并**，保留原样
//!      交给配对校验报错（硬合并会让两轮的 tool_use/tool_result 配对错位）；
//!   4. assistant 的 tool_call 与随后的 tool 结果做**配对校验**，顺带用
//!      assistant 侧的 `toolName` 补齐 tool 消息缺失的 `toolName`
//!      （客户端常常只回 tool_call_id）—— 这个补齐**必须写回消息对象**，
//!      因为 toolName 是指纹的一部分；
//!   5. 客户端**打断未完成的工具调用**后直接发新 user 消息：补一条合成
//!      tool 消息（`toolResult: "[工具调用被用户中断]"`）保持配对，
//!      该合成消息只用于让校验通过，**不会单独提交给上游**；
//!   6. 图片块 `{type:"image_url", image_url:{url,detail}}` →
//!      `{type:"image_url", imageUrl:{url,detail}}`，data: URL 与 http(s) 都支持；
//!   7. 每条消息的 `messageId` 必须非空：客户端给了就用它的 `id`，没给就
//!      生成随机 UUID（上游 2026-09 起在 round 阶段逐条校验，空串会被拒，
//!      见 [`message_id`]）。
//!
//! ── 错误怎么报 ──────────────────────────────────────────────
//! 全部走 [`GatewayError::bad_request`]（400，客户端可见中文文案），
//! 文案照抄原实现 —— 这些提示是排障的主要线索，改措辞等于把已知问题的
//! 排查经验清零。本模块**不 panic**（release 是 panic=abort，架构文档 §8 第 5 条）。
//!
//! ── 与后续波次的分工 ────────────────────────────────────────
//! `tools` / `tool_choice` / `reasoning_effort` / `modelType` 的归一化属于
//! round/turn 请求体的组装（`conversation.rs`，T-d3），本模块只管 `messages`。
//! 图片压缩（>60KB）也不在这里：压缩发生在**请求体组装完成之后**
//! （`image_compress.rs`，T-d2），本模块只做格式转换与上限拒绝。
//!
//! ── 块级逻辑在 `blocks.rs` ──────────────────────────────────
//! 本文件管**消息级**流水线（角色分派、相邻合并、工具配对、指令抽离）；
//! text / image_url / tool_use 块的字段映射与 tool 参数的 JSON 校验收在
//! `blocks.rs`（拆文件的唯一原因是单文件行数约定，架构文档 §8 第 1 条）。

use serde_json::{json, Map, Value};

use crate::server::errors::GatewayError;

use super::blocks::{message_content, normalize_tool_calls, tool_result_content};
use super::fingerprint::js_truthy;

/// 打断未完成工具调用时补的合成 tool_result 文案（原实现逐字如此）。
///
/// 这条文案会**提交给上游**（客户端下一轮的历史里它就在那儿），
/// 改动会让模型对「上一轮为什么没结果」的理解发生变化，因此冻结。
pub const INTERRUPTED_TOOL_RESULT: &str = "[工具调用被用户中断]";

/// 一次归一化的产物（对应原实现 `normalizeMessages` 的返回对象）。
///
/// 三个字段的消费点（round 请求体，见 UPSTREAM_PROTOCOL §3.1）：
///   - `messages` → `body.messages`
///   - `system_prompt` → `body.systemPromptContext.systemPromptOverride`
///   - `rules_message` → `body.rulesMessage`
///
/// 后两者为 `None` 时**整个字段不出现**（原实现用对象展开条件添加）。
#[derive(Clone, Debug, Default)]
pub struct NormalizedMessages {
    /// 上游消息块数组（user / assistant / tool，**不含 system / developer**）
    pub messages: Vec<Value>,
    /// `system` 消息抽离的文本（多条按 `\n\n` 连接；无则 None）
    pub system_prompt: Option<String>,
    /// `developer` 消息抽离的文本（多条按 `\n\n` 连接；无则 None）
    pub rules_message: Option<String>,
}

/// 归一化选项（对应原实现 `normalizeMessages` 的 `options`）。
#[derive(Clone, Copy, Debug, Default)]
pub struct NormalizeOptions {
    /// 允许历史以「待响应的 assistant tool_call」结尾（对应原实现
    /// `allowTrailingToolCalls`）。
    ///
    /// 为 true 的场景：**工具续接**请求（上一轮结束时 assistant 带着
    /// tool_calls 落库，客户端下一条请求就是这轮的 tool 结果）。
    /// 全新会话 / 长会话新轮次都是 false —— 那时的历史必须以 tool 结果
    /// 或 user 消息收尾，否则视为「历史被截断」而报错。
    /// T-d3 按原实现传参：`Boolean(session) && resolved.source !== 'client-session'`。
    pub allow_trailing_tool_calls: bool,
}

/// 中间形态：归一化 + 相邻合并后、抽离 system/developer 之前的一条消息。
///
/// 为什么需要中间层：原实现先 `normalizeMessage` 全部消息、再做相邻合并扫描，
/// 最后在第二次遍历里抽离 system/developer 并做工具配对校验 —— 合成 tool 消息
/// （打断补配对）插在「pending 工具调用」与新 user 消息之间，位置不能变
/// （插错会让上游看到 tool 结果出现在它对应的 tool_call 之前）。
enum Stage1 {
    /// system / developer：此时只记角色与文本
    Directive { role: DirectiveRole, text: String },
    /// 已归一化的上游消息块
    Message(Value),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DirectiveRole {
    System,
    Developer,
}

impl DirectiveRole {
    fn name(self) -> &'static str {
        match self {
            DirectiveRole::System => "system",
            DirectiveRole::Developer => "developer",
        }
    }
}

/// 把 OpenAI `messages` 归一化成上游消息块（原实现 `normalizeMessages`）。
///
/// `messages` 必须是非空数组，否则报 400（原实现同）。
pub fn normalize_messages(
    messages: &[Value],
    options: NormalizeOptions,
) -> Result<NormalizedMessages, GatewayError> {
    if messages.is_empty() {
        return Err(GatewayError::bad_request("messages 必须是非空数组"));
    }
    let mut stage: Vec<Stage1> = Vec::with_capacity(messages.len());
    for (index, message) in messages.iter().enumerate() {
        stage.push(normalize_message(message, index)?);
    }
    assemble(merge_adjacent(stage), options)
}

/// 工具结果续接请求的归一化（原实现 `normalizeContinuationMessages`）。
///
/// 与 [`normalize_messages`] 的区别：**只做 `normalizeMessage` + tool 合并**，
/// 不合并相邻 assistant、不抽离 system/developer、不做配对校验。
/// 用途是「工具续接时从历史里切出那一条 tool 消息」
/// （对应 `session-registry.mjs` 的 `continuationToolMessage`，T-d3 会用）。
///
/// 空输入返回空 Vec（原实现同，不报错 —— 调用方拿它判「没有可续接的内容」）。
/// 出现 system/developer 时返回错误：续接片段里它们没有意义，原实现会在
/// 调用点的「只能有一条 tool」校验里失败，这里提前给出更准确的文案。
pub fn normalize_continuation_messages(messages: &[Value]) -> Result<Vec<Value>, GatewayError> {
    let mut normalized: Vec<Value> = Vec::with_capacity(messages.len());
    for (index, message) in messages.iter().enumerate() {
        match normalize_message(message, index)? {
            Stage1::Directive { role, .. } => {
                return Err(GatewayError::bad_request(format!(
                    "messages[{index}] 的 {} 必须位于所有对话消息之前",
                    role.name()
                )));
            }
            Stage1::Message(value) => normalized.push(value),
        }
    }
    let mut merged: Vec<Value> = Vec::with_capacity(normalized.len());
    for message in normalized {
        let mergeable = message_type(&message) == Some("tool")
            && merged.last().and_then(message_type) == Some("tool");
        if mergeable {
            if let Some(previous) = merged.last_mut() {
                append_content(previous, &message);
            }
            continue;
        }
        merged.push(message);
    }
    Ok(merged)
}

/// 单条消息归一化（原实现 `normalizeMessage`）。
fn normalize_message(message: &Value, index: usize) -> Result<Stage1, GatewayError> {
    let Some(object) = message.as_object() else {
        return Err(GatewayError::bad_request(format!("messages[{index}] 必须是对象")));
    };
    let role = object.get("role").and_then(Value::as_str).unwrap_or_default();
    match role {
        "system" | "developer" => normalize_directive(object, index, role),
        "user" => normalize_user(object, index),
        "assistant" => normalize_assistant(object, index),
        "tool" => normalize_tool(object, index),
        other => Err(GatewayError::bad_request(format!("不支持的消息角色: {other}"))),
    }
}

/// system / developer：只允许非空文本，不允许携带工具字段。
fn normalize_directive(
    object: &Map<String, Value>,
    index: usize,
    role: &str,
) -> Result<Stage1, GatewayError> {
    if has_tool_fields(object) {
        return Err(GatewayError::bad_request(format!(
            "messages[{index}] 的 {role} 不允许携带工具字段"
        )));
    }
    let content = message_content(object)?;
    let text = content
        .iter()
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    let only_text = content
        .iter()
        .all(|block| block.get("type").and_then(Value::as_str) == Some("text"));
    if !only_text || text.trim().is_empty() {
        return Err(GatewayError::bad_request(format!(
            "messages[{index}] 的 {role} 只允许非空文本内容"
        )));
    }
    let directive_role = if role == "system" {
        DirectiveRole::System
    } else {
        DirectiveRole::Developer
    };
    Ok(Stage1::Directive { role: directive_role, text })
}

/// user：内容非空。
///
/// `messageId` 由 [`message_id`] 兜底（缺失时生成随机 UUID）——
/// 这里曾有一处「不生成随机 messageId」的有意差异，2026-09 上游收紧
/// `messageId` 校验后已废弃，原因见 [`message_id`] 的说明。
fn normalize_user(object: &Map<String, Value>, index: usize) -> Result<Stage1, GatewayError> {
    if has_tool_fields(object) {
        return Err(GatewayError::bad_request(format!(
            "messages[{index}] 的 user 不允许携带工具字段"
        )));
    }
    let content = message_content(object)?;
    if content.is_empty() {
        return Err(GatewayError::bad_request(format!(
            "messages[{index}] 的 user 内容不能为空"
        )));
    }
    Ok(Stage1::Message(json!({
        "type": "user",
        "messageId": message_id(object),
        "content": content,
        "finished": finished_flag(object),
    })))
}

/// assistant：只允许文本块 + tool_calls；`reasoning_content` 挂到首个文本块上。
///
/// 挂载位置（原实现 `content.find(block => block.type === 'text')`）：
/// 有文本块就写进它的 `reasoningContent`；没有就**追加一个空 text 块**承载。
/// 那个空 text 块在指纹里会被跳过（`fingerprint::is_blank_text_block`）——
/// 两处是同一条规则的两半，不能只改一边。
fn normalize_assistant(
    object: &Map<String, Value>,
    index: usize,
) -> Result<Stage1, GatewayError> {
    let mut content = message_content(object)?;
    if content
        .iter()
        .any(|block| block.get("type").and_then(Value::as_str) != Some("text"))
    {
        return Err(GatewayError::bad_request(format!(
            "messages[{index}] 的 assistant 只允许文本与 tool_calls"
        )));
    }
    // `reasoning_content ?? reasoningContent`：**空值合并**，不是真值判定 ——
    // `reasoning_content: null` 会继续看 `reasoningContent`；
    // 两者都是 null 时 JS 的 reasoning 是 null，typeof 不是 string → 报错。
    let reasoning = match object.get("reasoning_content") {
        Some(value) if !value.is_null() => Some(value),
        _ => object.get("reasoningContent"),
    };
    if let Some(value) = reasoning {
        if !value.is_string() {
            return Err(GatewayError::bad_request(format!(
                "messages[{index}].reasoning_content 必须是字符串"
            )));
        }
    }
    // `if (reasoning)`：非空串才挂载（空串等于没给）
    if let Some(value) = reasoning.filter(|value| js_truthy(value)) {
        match content
            .iter_mut()
            .find(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        {
            Some(target) => {
                if let Some(target) = target.as_object_mut() {
                    target.insert("reasoningContent".to_string(), value.clone());
                }
            }
            None => content.push(json!({
                "type": "text",
                "text": "",
                "reasoningContent": value,
            })),
        }
    }
    for call in normalize_tool_calls(object)? {
        content.push(json!({
            "type": "tool_use",
            "toolCallId": call.id,
            "toolName": call.name,
            "toolParams": call.arguments,
        }));
    }
    Ok(Stage1::Message(json!({
        "type": "assistant",
        "messageId": message_id(object),
        "content": content,
        "finished": finished_flag(object),
    })))
}

/// tool：`tool_call_id` 必填；结果文本见 [`tool_result_content`]。
///
/// `requestedToolName` 是原实现 tool 消息上的一个附加字段（取 `message.name`，
/// 缺失时整个键不存在）。它**不进上游**（round/turn 只认 content 块里的
/// toolName），也**不参与指纹**（指纹只读 content 块的 toolName）；保留它只为
/// 与归一化产物逐字段对齐，方便对照原实现排查。
fn normalize_tool(object: &Map<String, Value>, index: usize) -> Result<Stage1, GatewayError> {
    let id = object
        .get("tool_call_id")
        .or_else(|| object.get("toolCallId"))
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| GatewayError::bad_request(format!("messages[{index}] tool_call_id 缺失")))?;
    // `typeof message.name === 'string' && message.name.trim() ? message.name.trim() : undefined`
    // —— 注意这里**是** trim 过的（与 id 不同）
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let result = tool_result_content(object, index)?;
    let mut block = Map::new();
    block.insert("type".to_string(), Value::String("tool_result".to_string()));
    block.insert("toolCallId".to_string(), Value::String(id.to_string()));
    if let Some(name) = name {
        block.insert("toolName".to_string(), Value::String(name.to_string()));
    }
    block.insert("toolResult".to_string(), Value::String(result));
    let mut message = Map::new();
    message.insert("type".to_string(), Value::String("tool".to_string()));
    message.insert("messageId".to_string(), message_id(object));
    message.insert("content".to_string(), Value::Array(vec![Value::Object(block)]));
    message.insert("finished".to_string(), finished_flag(object));
    if let Some(name) = name {
        message.insert("requestedToolName".to_string(), Value::String(name.to_string()));
    }
    Ok(Stage1::Message(Value::Object(message)))
}

/// 相邻消息合并（原实现第 249-265 行的 `merged` 循环）。
///
/// 两条规则（顺序敏感，先 tool 后 assistant）：
///   - 连续 tool：`content` 追加合并，保留前一条的 messageId / finished；
///   - 相邻 assistant 且**至少一条没有 tool_use**：`content` 追加合并；
///     两条都带 tool_use 时**不合并**（原实现同：无法安全合并，
///     留到配对校验里报错，比静默错位好）。
fn merge_adjacent(stage: Vec<Stage1>) -> Vec<Stage1> {
    let mut merged: Vec<Stage1> = Vec::with_capacity(stage.len());
    for item in stage {
        let Stage1::Message(message) = &item else {
            // system/developer 不参与合并（它们在下一步被抽离）
            merged.push(item);
            continue;
        };
        let previous = match merged.last() {
            Some(Stage1::Message(previous)) => previous,
            // 前一条是指令，或列表为空：不合并
            _ => {
                merged.push(item);
                continue;
            }
        };
        let current_type = message_type(message);
        let previous_type = message_type(previous);
        let mergeable = if current_type == Some("tool") && previous_type == Some("tool") {
            true
        } else if current_type == Some("assistant") && previous_type == Some("assistant") {
            !has_tool_use(previous) || !has_tool_use(message)
        } else {
            false
        };
        if mergeable {
            if let Some(Stage1::Message(previous)) = merged.last_mut() {
                append_content(previous, message);
            }
            continue;
        }
        merged.push(item);
    }
    merged
}

/// 抽离 system/developer + 工具配对校验 + 合成打断消息（原实现第 266-341 行）。
fn assemble(
    merged: Vec<Stage1>,
    options: NormalizeOptions,
) -> Result<NormalizedMessages, GatewayError> {
    let mut conversation: Vec<Value> = Vec::with_capacity(merged.len());
    let mut system_parts: Vec<String> = Vec::new();
    let mut developer_parts: Vec<String> = Vec::new();
    // 已出现过的 tool_call_id：重复出现说明客户端把同一轮工具调用塞了两次
    let mut known_tool_calls: Vec<String> = Vec::new();
    // 待响应工具调用：tool_call_id → toolName（assistant 声明、等 tool 结果）
    let mut pending_tool_calls: Vec<(String, String)> = Vec::new();

    for (index, item) in merged.into_iter().enumerate() {
        let message = match item {
            Stage1::Directive { role, text } => {
                if !conversation.is_empty() {
                    return Err(GatewayError::bad_request(format!(
                        "messages[{index}] 的 {} 必须位于所有对话消息之前",
                        role.name()
                    )));
                }
                match role {
                    DirectiveRole::System => system_parts.push(text),
                    DirectiveRole::Developer => developer_parts.push(text),
                }
                continue;
            }
            Stage1::Message(message) => message,
        };

        match message_type(&message) {
            Some("assistant") => {
                if !pending_tool_calls.is_empty() {
                    return Err(GatewayError::bad_request(format!(
                        "messages[{index}] 前缺少 assistant tool_call 对应的 tool 结果"
                    )));
                }
                let calls = collect_tool_use(&message);
                for (call_id, _) in &calls {
                    if known_tool_calls.iter().any(|known| known == call_id) {
                        return Err(GatewayError::bad_request(format!(
                            "messages[{index}] 重复的 tool_call_id: {call_id}"
                        )));
                    }
                    known_tool_calls.push(call_id.clone());
                }
                pending_tool_calls = calls;
                conversation.push(message);
            }
            Some("tool") => {
                conversation.push(resolve_tool_message(message, index, &mut pending_tool_calls)?);
            }
            _ => {
                if !pending_tool_calls.is_empty() {
                    if message_type(&message) == Some("user") {
                        // 客户端打断未完成的工具调用后直接发新消息：补一条合成
                        // tool 消息保持配对（该消息只用于校验，不会单独提交上游）
                        conversation.push(synth_interrupt_message(&pending_tool_calls));
                        pending_tool_calls.clear();
                    } else {
                        return Err(GatewayError::bad_request(format!(
                            "messages[{index}] 前缺少 assistant tool_call 对应的 tool 结果"
                        )));
                    }
                }
                conversation.push(message);
            }
        }
    }

    if !pending_tool_calls.is_empty() && !options.allow_trailing_tool_calls {
        return Err(GatewayError::bad_request(
            "最后一条 assistant tool_call 缺少对应的 tool 结果",
        ));
    }

    // 多条 system/developer 用空行连接（原实现 `filter(Boolean).join('\n\n')`；
    // 单条时结果就是它自己）
    Ok(NormalizedMessages {
        messages: conversation,
        system_prompt: join_directives(&system_parts),
        rules_message: join_directives(&developer_parts),
    })
}

/// 一条 tool 消息的配对校验 + `toolName` 回填（原实现第 294-305 行）。
///
/// 回填是**必须的**而非可选优化：客户端回显历史时常常只给 `tool_call_id`
/// （toolName 丢了），而 toolName 在指纹里（`fingerprint.rs`），
/// 不回填会让客户端下一轮的这条消息与注册表里的指纹对不上 → 误判历史被改写。
fn resolve_tool_message(
    message: Value,
    index: usize,
    pending_tool_calls: &mut Vec<(String, String)>,
) -> Result<Value, GatewayError> {
    let mut message = message;
    let Some(blocks) = message.get("content").and_then(Value::as_array) else {
        return Ok(message);
    };
    let mut rewritten: Vec<Value> = Vec::with_capacity(blocks.len());
    for block in blocks {
        let Some(object) = block.as_object() else {
            rewritten.push(block.clone());
            continue;
        };
        if object.get("type").and_then(Value::as_str) != Some("tool_result") {
            rewritten.push(block.clone());
            continue;
        }
        let call_id = object
            .get("toolCallId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let position = pending_tool_calls
            .iter()
            .position(|(pending_id, _)| pending_id == call_id);
        let Some(position) = position else {
            return Err(GatewayError::bad_request(format!(
                "messages[{index}] 的 tool_call_id 没有待响应的 assistant tool_call"
            )));
        };
        let (_, expected_name) = pending_tool_calls[position].clone();
        if let Some(actual) = object
            .get("toolName")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            if actual != expected_name {
                return Err(GatewayError::bad_request(format!(
                    "messages[{index}] 的 tool name 与 assistant tool_call 不一致"
                )));
            }
        }
        // 补齐 toolName（原实现 `block.toolName = pendingToolCalls.get(callId)`）
        let mut updated = object.clone();
        updated.insert("toolName".to_string(), Value::String(expected_name));
        rewritten.push(Value::Object(updated));
        pending_tool_calls.remove(position);
    }
    if let Some(target) = message.as_object_mut() {
        target.insert("content".to_string(), Value::Array(rewritten));
    }
    Ok(message)
}

/// 合成「工具调用被用户中断」的 tool 消息（原实现第 311-320 行）。
///
/// `messageId` 同样要非空（上游逐条校验，见 [`message_id`]）—— 原实现这里
/// 也是 `randomUUID()`。
fn synth_interrupt_message(pending: &[(String, String)]) -> Value {
    let blocks: Vec<Value> = pending
        .iter()
        .map(|(call_id, tool_name)| {
            json!({
                "type": "tool_result",
                "toolCallId": call_id,
                "toolName": tool_name,
                "toolResult": INTERRUPTED_TOOL_RESULT,
            })
        })
        .collect();
    json!({
        "type": "tool",
        "messageId": crate::server::core::upstream::request::new_request_id(),
        "content": blocks,
        "finished": true,
    })
}

/// 抽离文本的拼接：过滤空串（原实现 `filter(Boolean)`）后按 `\n\n` 连接，
/// 全空时返回 None（对应「字段不出现」）。
fn join_directives(parts: &[String]) -> Option<String> {
    let joined = parts
        .iter()
        .filter(|part| !part.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join("\n\n");
    if joined.is_empty() {
        None
    } else {
        Some(joined)
    }
}

/// 消息的 `type` 字段（阶段 2 的消息都是归一化产物，type 必有）
fn message_type(message: &Value) -> Option<&str> {
    message.get("type").and_then(Value::as_str)
}

/// 消息是否含 `tool_use` 块（原实现 `content.some(b => b.type === 'tool_use')`）
fn has_tool_use(message: &Value) -> bool {
    message
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
        })
        .unwrap_or(false)
}

/// 收集 assistant 的 `tool_use` 块为 `(toolCallId, toolName)`（按出现顺序）。
fn collect_tool_use(message: &Value) -> Vec<(String, String)> {
    message
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
                .map(|block| {
                    (
                        block
                            .get("toolCallId")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        block
                            .get("toolName")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `previous.content.push(...message.content)`（原实现两处合并共用）。
fn append_content(target: &mut Value, extra: &Value) {
    let Some(extra_blocks) = extra.get("content").and_then(Value::as_array) else {
        return;
    };
    if let Some(blocks) = target.get_mut("content").and_then(Value::as_array_mut) {
        blocks.extend(extra_blocks.iter().cloned());
    }
}

/// 是否携带工具字段（system/developer/user 三种角色都不允许）。
///
/// 原实现是 `message.tool_calls || message.toolCalls || message.tool_call_id`
/// 的真值判定：空数组在 JS 里是**真值**，所以 `"tool_calls": []` 也会被拒 ——
/// 保持这个口径，不要顺手改成「非空才拒」。
fn has_tool_fields(object: &Map<String, Value>) -> bool {
    ["tool_calls", "toolCalls", "tool_call_id"]
        .iter()
        .any(|key| object.get(*key).map(js_truthy).unwrap_or(false))
}

/// `messageId` 取值：非空字符串用它（**原样，不 trim**），否则生成随机 UUID
/// （对齐原实现的 `randomUUID()` 兜底）。
///
/// ── 为什么必须兜底成非空（2026-09 上游收紧）──────────────────
/// 这里一度改成「缺失时留空串」，理由是「上游对 messageId 缺失是宽容的」。
/// 上游现在会在 round 阶段校验：`消息列表第 N 条消息校验失败：
/// errorCode=400, unifyCode=1005010001, errorMsg=messageId 不能为空`，
/// 空串与缺失一样被拒（HTTP 仍是 200，业务码非 0，见
/// `openai::unwrap_api_data`）。所以每条消息都必须带一个非空 id。
///
/// 兜底用随机 UUID **不会**影响增量会话：指纹（`fingerprint.rs`）不读
/// `messageId`，同一条历史消息在两次请求里拿到不同 id 也不会被上游判成
/// 「历史被改写」—— 原实现（`catpaw-upstream-messages.mjs`）正是这么做的。
fn message_id(object: &Map<String, Value>) -> Value {
    match object.get("id").and_then(Value::as_str) {
        Some(id) if !id.trim().is_empty() => Value::String(id.to_string()),
        _ => Value::String(crate::server::core::upstream::request::new_request_id()),
    }
}

/// `finished: message.finished !== false`（只有显式 `false` 才是未完成）
fn finished_flag(object: &Map<String, Value>) -> Value {
    Value::Bool(object.get("finished").and_then(Value::as_bool) != Some(false))
}
