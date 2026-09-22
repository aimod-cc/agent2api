//! CatPaw 入站准备层：原始 OpenAI 请求体 → 本轮执行所需的归一化素材。
//!
//! ── 这一层在做什么 ──────────────────────────────────────────
//! 一个客户端请求进来时是「无状态 OpenAI 形态」（完整历史 + tools + 各种参数），
//! 而上游 round/turn 要的是它自己的消息块与字段。本文件把前者翻译成后者，
//! 并在此过程中做三件必须在**出网之前**完成的事：
//!
//! ```text
//!   ① 参数解析：model → modelType、reasoning_effort → effort、
//!      context_window → context（models.rs）
//!   ② 工具归一化：tools / tool_choice → toolConfigs / 选中集（tools.rs）
//!   ③ 消息归一化：messages → 上游消息块（messages.rs）
//!      └ 内联图片压缩（>60KB 的 base64 是 round 请求体超限的主因）
//! ```
//!
//! 与 `conversation.rs` 的分工：本文件只产出「素材」，不碰会话注册表、不认识
//! 轮次模式；判定与编排都在那边。拆开的好处是「翻译」与「时序」各自可读 ——
//! 前者是纯函数的组合，后者是状态机。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：零 unwrap/expect/panic；不发网络请求、不持锁。

use serde_json::Value;

use super::image_compress::compress_if_needed;
use super::messages::{normalize_messages, NormalizeOptions};
use super::models::{
    resolve_context_window, resolve_effort, resolve_model_request, CatPawError, ModelResolution,
};
use super::tools::{normalize_tools, select_tools, tool_choice_mode, ToolChoice};
use super::conversation::ConversationRequest;

/// 一次请求的全部「已归一化素材」
pub(super) struct Prepared {
    /// 模型解析结果（`modelType` 与展示名）
    pub resolution: ModelResolution,
    /// 归一化后的上游消息块（不含 system / developer）
    pub messages: Vec<Value>,
    /// 本轮要提交的那条消息（增量轮次的末尾消息）
    pub final_message: Value,
    /// `system` 消息抽离出的系统提示（round/turn 的 `systemPromptContext`）
    pub system_prompt: Option<String>,
    /// `developer` 消息抽离出的规则文本（round/turn 的 `rulesMessage`）
    pub rules_message: Option<String>,
    /// `reasoning_effort` → `declarativeParams.effort`
    pub effort: Option<String>,
    /// `context_window` → `declarativeParams.context`
    pub context: Option<String>,
    /// 归一化后的 `tool_choice`（收尾校验「上游有没有按 choice 返回」用）
    pub choice: ToolChoice,
    /// 按 `tool_choice` 裁剪后**本轮实际下发**的工具集。
    ///
    /// 在准备阶段就算好（而不是发 turn 时才算）的理由：`tool_choice` 指向一个
    /// 不存在的工具、或 `required` 但一个工具都没给，都是**客户端入参错误** ——
    /// 原实现在 `prepareRequest` 阶段就抛（早于 round），客户端拿到的是一条干净的
    /// 400 而不是「先建了会话再失败」。放到后面会让一次错误请求在上游留下一个
    /// 刚创建就被判失败的 conversation。
    pub selected_tools: Vec<Value>,
}

impl Prepared {
}

/// 归一化 + 参数解析（原实现 `prepareRequest` 在网关语义下的形态）。
///
/// ── 校验顺序照抄原实现（`prepareRequest` 的调用顺序）───────────
/// 消息 → effort → tools → tool_choice 的**报错优先级**是客户端可感知的行为
/// （同时给错两处时先报哪一个），因此这里保持同一顺序：
/// 本函数把 messages 放在最后只是为了先算完参数（校验本身在 `normalize_messages`
/// 内部完成，顺序仍是「参数先、消息后」）。
pub(super) async fn prepare(request: &ConversationRequest) -> Result<Prepared, CatPawError> {
    let body = &request.body;
    let resolution = resolve_model_request(body.get("model").unwrap_or(&Value::Null))?;
    let effort = resolve_effort(body)?;
    let context = resolve_context_window(body, &resolution)?;
    let choice = tool_choice_mode(body.get("tool_choice"))?;
    let all_tools = normalize_tools(body.get("tools"))?;
    // 工具集裁剪同时是入参校验（`required` 没给工具、指定的工具不存在都报 400）：
    // 放在这里是为了让这类错误**不经过 round**（见 `selected_tools` 的说明）
    let selected_tools = select_tools(&all_tools, &choice)?;
    // `parallel_tool_calls` 只做类型校验：上游的 turn 请求体里没有对应字段，
    // 原实现也是「收了但不用」（校验失败报 400，避免客户端以为它生效了）
    if let Some(value) = body.get("parallel_tool_calls").filter(|value| !value.is_null()) {
        if !value.is_boolean() {
            return Err(CatPawError::bad_request("parallel_tool_calls 必须是布尔值"));
        }
    }

    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| CatPawError::bad_request("messages 必须是非空数组"))?;
    // `NormalizeOptions::default()`：不允许多余的尾部 tool_call（`allow_trailing_tool_calls`
    // 只对「工具续接」那条路径有意义，而它由 conversation.rs 自己从原始消息里切片段）
    let normalized = normalize_messages(messages, NormalizeOptions::default())?;
    // 归一化后的内联图片过压缩（见 `compress_images` 的说明）
    let messages = compress_images(normalized.messages).await;
    let final_message = messages
        .last()
        .cloned()
        .ok_or_else(|| CatPawError::bad_request("system/developer 之外至少需要一条对话消息"))?;
    Ok(Prepared {
        resolution,
        messages,
        final_message,
        system_prompt: normalized.system_prompt,
        rules_message: normalized.rules_message,
        effort,
        context,
        choice,
        selected_tools,
    })
}

/// 归一化消息里的内联图片压缩（`image_compress.rs` 的调用点）。
///
/// ── 为什么走 `spawn_blocking` ────────────────────────────────
/// 压缩是纯 CPU 活（最多 4 次 1568px 缩放 + JPEG 编码，可能几百毫秒），直接在
/// tokio worker 上跑会挡住同一 worker 上的其它任务。`image_compress.rs` 的模块头
/// 明确建议调用方按需包这一层。
///
/// ── 为什么先收集 (下标, URL) 再回写 ─────────────────────────
/// 这样即便后台任务失败（被取消）也只影响「这一张没压」，不会丢消息。
/// 压缩产物比原图大时 `compress_if_needed` 会原样返回，所以「值没变」就是
/// 「不需要回写」的判据。
async fn compress_images(messages: Vec<Value>) -> Vec<Value> {
    // (消息下标, 块下标, 原始 URL)
    let mut targets: Vec<(usize, usize, String)> = Vec::new();
    for (message_index, message) in messages.iter().enumerate() {
        let Some(blocks) = message.get("content").and_then(Value::as_array) else {
            continue;
        };
        for (block_index, block) in blocks.iter().enumerate() {
            if block.get("type").and_then(Value::as_str) != Some("image_url") {
                continue;
            }
            let url = block
                .pointer("/imageUrl/url")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if url.starts_with("data:image/") {
                targets.push((message_index, block_index, url.to_string()));
            }
        }
    }
    if targets.is_empty() {
        return messages;
    }
    let urls: Vec<String> = targets.iter().map(|(_, _, url)| url.clone()).collect();
    let compressed = tokio::task::spawn_blocking(move || {
        urls.into_iter().map(|url| compress_if_needed(&url)).collect::<Vec<_>>()
    })
    .await;
    let Ok(compressed) = compressed else {
        return messages;
    };
    if compressed.len() != targets.len() {
        return messages;
    }
    let mut messages = messages;
    for ((message_index, block_index, original), next) in targets.into_iter().zip(compressed) {
        if next == original {
            continue;
        }
        // 只替换 url：detail 等其它字段原样保留（压缩换的只是像素数据）
        if let Some(url) = messages
            .get_mut(message_index)
            .and_then(|message| message.get_mut("content"))
            .and_then(Value::as_array_mut)
            .and_then(|blocks| blocks.get_mut(block_index))
            .and_then(|block| block.pointer_mut("/imageUrl/url"))
        {
            *url = Value::String(next);
        }
    }
    messages
}
