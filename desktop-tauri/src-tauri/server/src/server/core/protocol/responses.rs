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
    content_parts, content_text, event_frame, freeform, is_truthy, json_text, random_id,
    string_field, string_value, tool_plan, SseLineBuffer,
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
    // 工具声明：收集 + 摊平（namespace 展开、custom 记名）。两个来源都要收 ——
    // 顶层 `tools`，以及 `input[]` 里 `type:"additional_tools"` 的项（Codex 的
    // Responses Lite 路径把工具塞在 input 里、顶层 `tools` 为 null）。详见
    // `tool_plan` 模块头：只认顶层字段是 2026-09「Codex 调不动工具」的根因。
    let plan = tool_plan::plan_tools(body);
    if !plan.declarations.is_empty() {
        let mut converted: Vec<Value> = Vec::new();
        let mut downgraded: Vec<String> = Vec::new();
        let mut ignored: Vec<String> = Vec::new();
        for tool in &plan.declarations {
            if let Some(function) = tool_to_chat(tool) {
                converted.push(function);
                continue;
            }
            // 没转出来的分两类。custom（freeform）是**要**降级的 —— 漏了就是
            // 静默失效（Codex 的 exec / apply_patch 全部失效即由此而来）；
            // 其余（web_search / local_shell 等 Responses 原生工具）上游确实
            // 没有对应物，只能丢，但要留痕，否则同样是静默失效
            if freeform::is_custom_tool(tool) {
                if let Some(function) = freeform::downgrade_custom_tool(tool) {
                    downgraded.push(string_field(tool, "name"));
                    converted.push(function);
                    continue;
                }
            }
            ignored.push(tool_plan::tool_kind_label(tool));
        }
        if !ignored.is_empty() {
            logging::log(
                "[Responses]",
                &format!(
                    "⚠️ 上游 Chat 接口无对应形态，已丢弃这些工具声明：{}",
                    ignored.join(", ")
                ),
            );
        }
        if !downgraded.is_empty() {
            logging::verbose(
                "[Responses]",
                &format!(
                    "自由文本（custom）工具已降级为 function：{}",
                    downgraded.join(", ")
                ),
            );
        }
        if !converted.is_empty() {
            logging::verbose(
                "[Responses]",
                &format!(
                    "工具声明 {} 条（含命名空间展开）→ 已转发 {} 条",
                    plan.declarations.len(),
                    converted.len()
                ),
            );
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
            let mut pending = PendingReasoning::default();
            push_input_item(&mut messages, input, &mut pending)?;
            merge_successive_assistants(&mut messages);
        }
        return Ok(messages);
    };
    // 跨项状态：reasoning 项的正文要挂到**它旁边那条** assistant 消息上，
    // 而两者在 `input[]` 里是平级的两个项（见 `PendingReasoning` 的说明）
    let mut pending = PendingReasoning::default();
    for item in items {
        if let Some(text) = item.as_str() {
            messages.push(json!({ "role": "user", "content": text }));
            continue;
        }
        push_input_item(&mut messages, item, &mut pending)?;
    }
    merge_successive_assistants(&mut messages);
    Ok(messages)
}

/// 把**连续的多条 assistant 消息合并成一条**。
///
/// ── 为什么必须合并（这是 400 的真正原因）────────────────────
/// DeepSeek 明确拒绝连续的 assistant 消息，报错原文：
/// `does not support successive user or assistant messages … You should
/// interleave the user/assistant messages in the message sequence.`
///
/// 而 Codex 走 Responses 时**每一轮都可能产生连续的 assistant 项**：它把
/// 「先说一句话」和「发起工具调用」拆成两个平级项
/// （`reasoning → message → custom_tool_call`），转换后就是两条紧挨着的
/// assistant 消息。真实数据：43 条抓包请求里，唯一出现连续 assistant 的那条
/// 就是唯一一次 400；同一批里连续 **user** 消息有 1~7 对却全部成功 ——
/// 可见被拒的确实是「连续 assistant」这个形状，与 reasoning 无关。
///
/// ── 合并规则 ────────────────────────────────────────────────
/// 后一条并入前一条，字段按语义合并：
///   - `content`：两段文本**换行拼接**（前一条是引言、后一条是动作说明，
///     都是模型的话，拼起来语义不变）；
///   - `tool_calls`：**数组相加**（并行/连续调用，上游按数组逐条执行）；
///   - `reasoning_content`：保留先到的（同一轮的思考本来就该一致，
///     真有出入时以先到的为准，与 [`PendingReasoning`] 同一口径）。
///
/// 合并结果正是 DeepSeek 接受的形状 —— 也是 Codex 自己发出来的合法形状
/// （`content` 与 `tool_calls` 同在一条 assistant 上）。
fn merge_successive_assistants(messages: &mut Vec<Value>) {
    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    for message in messages.drain(..) {
        let can_merge = message.get("role").and_then(Value::as_str) == Some("assistant")
            && out
                .last()
                .and_then(|last| last.get("role"))
                .and_then(Value::as_str)
                == Some("assistant");
        if !can_merge {
            out.push(message);
            continue;
        }
        let Some(previous) = out.last_mut().and_then(Value::as_object_mut) else {
            out.push(message);
            continue;
        };
        let Some(incoming) = message.as_object() else {
            continue;
        };
        merge_assistant_fields(previous, incoming);
    }
    *messages = out;
}

/// 把 `incoming` 的字段并进 `previous`（见 [`merge_successive_assistants`]）
fn merge_assistant_fields(previous: &mut Map<String, Value>, incoming: &Map<String, Value>) {
    // content：两段文本换行拼接。空串/Null 视为「没有这段」
    let merged_text = {
        let a = text_of(previous.get("content"));
        let b = text_of(incoming.get("content"));
        match (a.is_empty(), b.is_empty()) {
            (true, true) => None,
            (false, true) => Some(a),
            (true, false) => Some(b),
            (false, false) => Some(format!("{a}\n{b}")),
        }
    };
    match merged_text {
        Some(text) => {
            previous.insert("content".to_string(), Value::String(text));
        }
        None => {
            // 两条都没有正文：保持 Null（带 tool_calls 的 assistant 常是这种）
            if !previous.contains_key("content") {
                previous.insert("content".to_string(), Value::Null);
            }
        }
    }
    // tool_calls：数组相加
    let incoming_calls = incoming.get("tool_calls").and_then(Value::as_array);
    if let Some(calls) = incoming_calls.filter(|calls| !calls.is_empty()) {
        match previous.get_mut("tool_calls").and_then(Value::as_array_mut) {
            Some(existing) => existing.extend(calls.iter().cloned()),
            None => {
                previous.insert("tool_calls".to_string(), Value::Array(calls.clone()));
            }
        }
    }
    // reasoning_content：保留先到的（`entry` 语义：已有就不覆盖）
    if let Some(reasoning) = incoming.get("reasoning_content").cloned() {
        previous
            .entry("reasoning_content".to_string())
            .or_insert(reasoning);
    }
}

/// 取消息 `content` 的纯文本形态（字符串直接用；其余交给 `content_text`）
fn text_of(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(other) => content_text(other),
        None => String::new(),
    }
}

/// 等待归位的 reasoning 正文（跨 input 项的暂存）。
///
/// ── 为什么需要跨项状态 ──────────────────────────────────────
/// Responses 的 reasoning 是**独立于** function_call 的兄弟项，两者平级：
///
/// ```text
/// input: [ … , {type:"reasoning", summary:[…]}, {type:"function_call", …} , … ]
/// ```
///
/// 而 Chat 侧没有「独立的思考项」这个概念 —— 思考只能作为 `reasoning_content`
/// 挂在某条 assistant 消息上。于是转换必须把这两项**合并**成一条消息。
///
/// ── 为什么必须合并（不合并就是 400）────────────────────────
/// DeepSeek 等 thinking 模型要求：请求带 `tools` 时，历史里每个
/// assistant 轮次的 `reasoning_content` 都要完整回传，漏掉就返回
/// `400 the reasoning content from the previous turn must be passed back in
/// thinking mode`。原实现把 reasoning 项整个丢掉，于是每一条带工具调用的
/// 历史都缺 reasoning —— Codex 走 Responses 时必然踩中（它每轮都带工具）。
///
/// ── 同一轮的**每条** assistant 都要带它（不只是带工具调用的那条）──────
/// 一个轮次在 `input[]` 里可能是**多项**，Codex 实测的形状：
///
/// ```text
/// [1] reasoning            ← 这一轮的思考（只有一个）
/// [2] message  assistant   '我来扫描一下常见的开发工具。'   ← 同轮正文
/// [3] custom_tool_call     ← 同轮工具调用
/// ```
///
/// 这里的关键：**`[2]` 与 `[3]` 都要带同一份 reasoning**。上游把「连续的
/// assistant 消息」当同一个轮次核对，只给 `[3]` 挂、让 `[2]` 空着会被判
/// `reasoning_content_missing`（真实事故：43 条请求里唯一出现「连续 assistant」
/// 结构的那条就是唯一一次 400）。
///
/// 所以 `take()` 在**每条** assistant 类消息上都会调用，且**取走后不清空** ——
/// 本轮内后续的 assistant 消息还要接着用同一份思考。清空时机是「轮次结束」，
/// 由 [`PendingReasoning::end_turn`] 负责，而轮次边界就是**下一条 user 消息**。
///
/// ── 为什么只做「向后挂」 ────────────────────────────────────
/// Responses 的输出顺序固定是 reasoning 在前、它对应的 message /
/// function_call 在后（`response.output[]` 的顺序，客户端原样回传），
/// 所以只需要「暂存 → 本轮后续的 assistant 项取走」这一个方向。
///
/// 反方向（assistant 先到、reasoning 后到）**故意不做回填**：回填只能挂到
/// 「最后一条」assistant 上，而那一条很可能是**上一轮**的（例如
/// `… function_call → function_call_output → reasoning(属于下一轮) → …`），
/// 挂错轮次比丢掉更糟 —— 上游会认为这一轮带了别轮的思考。
///
/// 一直没有 assistant 可挂时**丢弃**（轮次结束时清空）：挂到 user / tool
/// 消息上会污染对话。
#[derive(Default)]
struct PendingReasoning {
    /// 本轮（或本轮尚未消费的那一段）的 reasoning 正文
    text: Option<String>,
    /// 这段正文是否已经挂到过消息上。
    ///
    /// 用来区分「同一轮的第二个 reasoning 项」与「下一轮的第一个 reasoning 项」：
    /// 前者该与已有正文**拼接**，后者该**替换**掉它。
    /// 判据是「中间有没有 assistant 消息消费过」—— 消费过就说明上一段属于
    /// 上一轮（`reasoning → tool_call → reasoning` 这种形状里，两段 reasoning
    /// 分属两轮，拼在一起会把上一轮的思考带到这一轮，与挂错轮次同一类错误）。
    attached: bool,
}

impl PendingReasoning {
    /// 记下一段 reasoning。
    ///
    /// 上一段**已经被消费**时替换（新的一轮开始了）；否则拼接（同一轮里
    /// 模型拆成了多个 reasoning 项）。
    fn stash(&mut self, text: String) {
        if self.attached {
            self.text = Some(text);
            self.attached = false;
            return;
        }
        match self.text.as_mut() {
            Some(existing) => {
                existing.push('\n');
                existing.push_str(&text);
            }
            None => self.text = Some(text),
        }
    }

    /// 本轮要挂到 assistant 消息上的 reasoning（**不清空** —— 同一轮的每条
    /// assistant 消息都要带同一份，见结构体文档），同时记下「已被消费」
    fn attach(&mut self) -> Option<String> {
        let text = self.text.clone();
        if text.is_some() {
            self.attached = true;
        }
        text
    }

    /// 轮次结束（遇到新的 user / system 消息）：清掉本轮的 reasoning
    fn end_turn(&mut self) {
        self.text = None;
        self.attached = false;
    }
}

/// 单个 input 项 → 零到一条 Chat 消息（追加进 `messages`）
///
/// `pending` 是跨项的 reasoning 暂存（见 [`PendingReasoning`]）：reasoning 项
/// 自己不产生消息，只把正文交给相邻的 assistant 项。
fn push_input_item(
    messages: &mut Vec<Value>,
    item: &Value,
    pending: &mut PendingReasoning,
) -> Result<(), ConvertError> {
    let kind = string_field(item, "type").to_lowercase();
    match kind.as_str() {
        // 函数调用与其结果：合成 assistant(tool_calls) + tool 两条消息。
        // 上游要求 tool 消息必须紧跟对应的 assistant，所以这里一次推两条。
        "function_call" => {
            let call_id = call_id_of(item);
            // 命名空间工具的历史里，名字在 `name`、分组在 `namespace` 两个字段；
            // 给上游必须是展平后的名字，否则模型看到的工具名前后不一致
            let name = tool_plan::flatten_call_name(item);
            let arguments = {
                let raw = json_text(item.get("arguments").unwrap_or(&Value::Null));
                if raw.is_empty() { "{}".to_string() } else { raw }
            };
            messages.push(tool_call_message(
                &call_id,
                &name,
                arguments,
                pending.attach(),
            ));
        }
        "function_call_output" => {
            messages.push(json!({
                "role": "tool",
                "tool_call_id": call_id_of(item),
                "content": tool_output_text(item.get("output")),
            }));
        }
        // 自由文本工具（custom）的调用与结果：形态与 function_call 同构，只是
        // 「参数」在 `input` 里而不是 `arguments`。必须还原成同一对消息，否则
        // 多轮里模型看不到自己上一步调过什么 —— 工具调用即便成功，第二轮也会
        // 退化成凭空重问。
        "custom_tool_call" => {
            let name = tool_plan::flatten_call_name(item);
            // 两种来源都要认：客户端回传的原生形态是 `input` 裸文本；
            // 但历史若来自我们自己的回程（已降级），id 相同、字段也可能是
            // `arguments`。取到哪个用哪个，都不丢内容
            let raw = {
                let input = string_field(item, "input");
                if input.is_empty() {
                    json_text(item.get("arguments").unwrap_or(&Value::Null))
                } else {
                    input
                }
            };
            messages.push(tool_call_message(
                &call_id_of(item),
                &name,
                // 按降级时的约定包成 `{"input": "…"}`，与出站声明一致
                freeform::wrap_freeform_input(&raw),
                pending.attach(),
            ));
        }
        "custom_tool_call_output" => {
            messages.push(json!({
                "role": "tool",
                "tool_call_id": call_id_of(item),
                "content": tool_output_text(item.get("output")),
            }));
        }
        // reasoning 项：正文不属于任何一轮对话，但**不能丢** —— 它要挂到本轮的
        // assistant 消息上（见 [`PendingReasoning`]）。这里只暂存，
        // 由本轮后续的 message / function_call / custom_tool_call 消费
        "reasoning" => {
            if let Some(text) = reasoning_text_of(item) {
                pending.stash(text);
            }
        }
        "input_text" | "text" => {
            messages.push(json!({ "role": "user", "content": string_field(item, "text") }));
            pending.end_turn();
        }
        "input_image" | "input_file" | "image_url" => {
            // 裸内容块（不在 message 里）：包成一条 user 消息
            messages.push(json!({ "role": "user", "content": [item.clone()] }));
            pending.end_turn();
        }
        // 工具声明项：不是对话内容，已经在 `tool_plan::plan_tools` 里
        // 提升成请求级工具声明。这里静默跳过 —— 落到下面的兜底分支会报一条
        // 误导性的「内容会缺一段」（它本来就不该出现在对话里）。
        // 注意这里必须写字面量：用常量名会被当成变量绑定，从而吞掉**所有**
        // 输入项（编译器只在有后续分支时才报 unreachable pattern）
        "additional_tools" => {}
        // 消息项（含 type 缺失/为 "message" 的常规形态）
        _ => {
            let role = {
                let raw = string_field(item, "role").to_lowercase();
                if raw == "developer" { "system".to_string() } else if raw.is_empty() { "user".to_string() } else { raw }
            };
            let content = item.get("content").or_else(|| item.get("text"));
            let Some(content) = content else {
                // 认不出的项在这里被丢掉。原实现直接 return，不报错也不记日志 ——
                // 一旦是工具相关的新类型（如 local_shell_call），症状就是「历史
                // 莫名其妙少了东西」，无从归因。宁可吵一点，也要留下类型名。
                let unknown = string_field(item, "type");
                if !unknown.is_empty() && unknown != "message" {
                    logging::log(
                        "[Responses]",
                        &format!("⚠️ 输入项类型 {unknown} 无法转换，已跳过（内容会缺一段）"),
                    );
                }
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
            // assistant 项的 reasoning 有两个来源，**先看项内的、再拿本轮的**：
            //   - 项内的 `summary`：客户端把思考塞在同一条消息里时用这个；
            //   - 本轮暂存（`PendingReasoning`）：Codex 把 reasoning 作为**独立
            //     兄弟项**发来，转换时挂到本轮每条 assistant 消息上。
            //
            // 这里**只读 `summary`、绝不读 `content`** —— message 项的 `content`
            // 是**正文**（`{type:"output_text", text:…}`），把它当思考会让
            // `reasoning_content` 变成正文的副本。那是修过的真实 bug。
            //
            // 纯文本 assistant 消息**也要**带 reasoning（不是只给带工具调用的
            // 那条）：Codex 一轮里可能先发正文、再发工具调用，两条是**连续的
            // assistant 消息**，上游按轮次核对，缺任何一条都判
            // `reasoning_content_missing`（真实事故，见 [`PendingReasoning`]）。
            if role == "assistant" {
                let reasoning = summary_text_of(item).or_else(|| pending.attach());
                if let Some(reasoning) = reasoning {
                    message.insert("reasoning_content".to_string(), Value::String(reasoning));
                }
            } else if role != "tool" {
                // 非 assistant、非 tool 的消息 = 上一轮结束：清掉本轮的 reasoning，
                // 免得它被挂到下一轮的 assistant 消息上（挂错轮次会 400）。
                // `tool` 结果**不算**轮次边界 —— 同一轮里工具结果之后还有
                // assistant 消息（那正是 Codex 的常规形状），reasoning 要接着用。
                pending.end_turn();
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

/// 工具调用项 → Chat 的 assistant(tool_calls) 消息。
///
/// `reasoning` 是同轮次 reasoning 项的正文（见 [`PendingReasoning`]）：挂上它
/// 才能满足 thinking 模型「带 tools 时 reasoning_content 必须回传」的要求。
/// 为 None 时不写这个字段 —— 写空串与不写是两回事，上游对空串的判定各家不一。
fn tool_call_message(
    call_id: &str,
    name: &str,
    arguments: String,
    reasoning: Option<String>,
) -> Value {
    let mut message = Map::new();
    message.insert("role".to_string(), Value::String("assistant".to_string()));
    message.insert("content".to_string(), Value::Null);
    message.insert(
        "tool_calls".to_string(),
        json!([{
            "id": call_id,
            "type": "function",
            "function": { "name": name, "arguments": arguments },
        }]),
    );
    if let Some(reasoning) = reasoning {
        message.insert("reasoning_content".to_string(), Value::String(reasoning));
    }
    Value::Object(message)
}

/// **一个 `reasoning` 项**的正文（`summary` 优先，退到 `content`）。
///
/// 只给 `reasoning` 项用 —— 那个类型的 `content` 确实是思考正文。
/// **绝不能拿它读普通 `message` 项**：message 的 `content` 是对话正文，
/// 读出来会让 `reasoning_content` 变成正文的副本（见 [`summary_text_of`]）。
fn reasoning_text_of(item: &Value) -> Option<String> {
    summary_text_of(item).or_else(|| text_blocks_of(item.get("content")))
}

/// **`message` 项**的思考正文：只认 `summary`，绝不回退到 `content`。
///
/// ── 为什么这个区分是必需的（真实事故）────────────────────────
/// 两条协议里「正文」与「思考」的字段位置不同：
///   - `message` 项：`content: [{type:"output_text", text:"正文"}]`，无 `summary`；
///   - `reasoning` 项：`summary: [{type:"summary_text", text:"思考"}]`。
///
/// 早先这里共用了一个「summary 优先、退到 content」的读法，于是**纯文本
/// assistant 轮次**（message 项）把正文读成了思考，转换结果里
/// `reasoning_content` 与 `content` 一模一样。后果不是「多带了一份无害的
/// 数据」：DeepSeek 的 thinking 校验按轮次核对 reasoning，这一轮的思考被
/// 认成错的，紧接着那条真正需要 reasoning 的工具调用轮次反而拿不到它
/// （reasoning 项在文本项之前到达，被文本项先消费掉了），上游照样回
/// `400 reasoning_content_missing`。
fn summary_text_of(item: &Value) -> Option<String> {
    text_blocks_of(item.get("summary"))
}

/// 一个块数组里的 `text` 拼接（空数组 / 非数组 / 全空文本都给 None）
fn text_blocks_of(blocks: Option<&Value>) -> Option<String> {
    let parts = blocks.and_then(Value::as_array)?;
    let text: String = parts
        .iter()
        .map(|part| string_field(part, "text"))
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty() { None } else { Some(text) }
}

/// 工具输出 → 文本（Chat 的 tool 消息 content 只接受字符串）
///
/// `pub(super)`：出站方向（`responses_outbound`）把 chat 的 tool 消息折回
/// `function_call_output` 时用同一口径，两处各写一份迟早分叉。
pub(super) fn tool_output_text(output: Option<&Value>) -> String {
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
///
/// `pub(super)`：出站方向（`responses_outbound`）把 chat 的 image_url 块折成
/// `input_image` 时取的是同一个值，口径共用一处。
pub(super) fn image_url_of(part: &Value) -> String {
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
    // 已经嵌套好的（客户端混用两种形态）：原样保留
    if tool_plan::is_nested_function(tool) {
        return Some(tool.clone());
    }
    let kind = string_field(tool, "type").to_lowercase();
    if kind != "function" && !kind.is_empty() {
        // 非 function 类型：上游的 Chat 接口不认，丢掉而不是发过去让上游报错
        // —— 客户端要的是「能跑」，不是「原样报错」。
        // custom（freeform）也落在这里，但调用方会先把它捞去降级
        // （见 `chat_from_responses` 的工具循环），不会真的丢。
        return None;
    }
    tool_plan::nested_from_flat(tool)
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

/// 一次工具调用 → Responses 的 output item。
///
/// 两件事都由 `plan` 决定：
///   · 名字原本是 custom（自由文本）→ 输出 `custom_tool_call`，参数是裸文本
///     `input` 而不是 `arguments` JSON。类型给错的话，Codex 会把自由文本当成
///     JSON 参数去解析，工具照样跑不起来；
///   · 名字来自命名空间 → 补回 `namespace` 字段并把 `name` 还原成原名。Codex
///     的工具路由按 `{name, namespace}` 精确查表，缺了它直接报
///     `unsupported call`。
///
/// `call_id` 由调用方给（上游的真实 id，或自造兜底）：三个回程出口
/// （非流式 / 流式 / 聚合）共用这一处口径，避免各写各的、日后只改一处。
fn tool_call_item(name: &str, call_id: &str, arguments: &str, plan: &tool_plan::ToolPlan) -> Value {
    let item = if plan.is_custom(name) {
        json!({
            "type": "custom_tool_call",
            "id": random_id("ctc"),
            "status": "completed",
            "call_id": call_id,
            "name": name,
            "input": freeform::unwrap_freeform_input(arguments),
        })
    } else {
        json!({
            "type": "function_call",
            "id": random_id("fc"),
            "status": "completed",
            "call_id": call_id,
            "name": name,
            "arguments": arguments,
        })
    };
    tool_plan::restore_namespace(item, name, plan)
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
    let plan = tool_plan::plan_tools(request);
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
            let name = call.pointer("/function/name").map(string_value).unwrap_or_default();
            let arguments = call
                .pointer("/function/arguments")
                .map(json_text)
                .unwrap_or_else(|| "{}".to_string());
            let call_id = {
                let raw = string_field(call, "id");
                if raw.is_empty() { random_id("call") } else { raw }
            };
            output.push(tool_call_item(&name, &call_id, &arguments, &plan));
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
    /// 请求里声明为 custom（freeform）的工具名：命中时工具项要发
    /// `custom_tool_call` 与 `response.custom_tool_call_input.*`，
    /// 而不是 `function_call` 与 `response.function_call_arguments.*`
    plan: tool_plan::ToolPlan,
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

/// 收尾阶段一个工具的定稿（`close_tools` 用）。
///
/// 单独开一个结构体而不是元组：字段已有六个，元组下标读起来谁是谁全靠数。
struct ToolFinal {
    index: i64,
    item_id: String,
    call_id: String,
    name: String,
    announced: bool,
    item: Value,
}

impl ResponsesStream {
    pub fn new(model: &str, request: &Value) -> Self {
        Self {
            buffer: SseLineBuffer::new(),
            response_id: random_id("resp"),
            created: logging::now_ms() / 1000,
            model: model.to_string(),
            request: request.clone(),
            plan: tool_plan::plan_tools(request),
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
                // custom 项的 id 前缀与 function 不同（ctc_ vs fc_），而这里
                // 是唯一能改的时刻：宣告之后 item_id 已经在事件里发出去，
                // 再改就对不上了
                if self.plan.is_custom(&tool.name) {
                    tool.item_id = random_id("ctc");
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

        let is_custom = self.plan.is_custom(&name);
        if announced_now {
            // 自由文本工具的「参数」是裸文本 input，item 形态与 function 不同
            // —— 见官方 custom_tool_call 项的定义
            let item = if is_custom {
                json!({
                    "type": "custom_tool_call",
                    "id": item_id,
                    "status": "in_progress",
                    "call_id": call_id,
                    "name": name,
                    "input": "",
                })
            } else {
                json!({
                    "type": "function_call",
                    "id": item_id,
                    "status": "in_progress",
                    "call_id": call_id,
                    "name": name,
                    "arguments": "",
                })
            };
            // 命名空间工具要在这里就带上 `namespace`：客户端在**宣告时**就按
            // `{name, namespace}` 建路由，等到收尾才补已经晚了
            let item = tool_plan::restore_namespace(item, &name, &self.plan);
            out.push(response_event(
                &mut self.sequence,
                "response.output_item.added",
                json!({ "output_index": index, "item": item }),
            ));
            // 宣告前已攒下的参数要补发（名字与参数可能同一帧到达）
            if !buffered.is_empty() {
                out.extend(self.tool_delta_event(index, &item_id, &call_id, &name, &buffered, is_custom));
            }
        }
        let arguments = call
            .pointer("/function/arguments")
            .and_then(Value::as_str)
            .unwrap_or("");
        if !arguments.is_empty() && !announced_now {
            out.extend(self.tool_delta_event(index, &item_id, &call_id, &name, arguments, is_custom));
        }
        out
    }

    /// 工具增量的一个事件帧（function 与 custom 的形态完全不同）。
    ///
    /// **custom 一律不发增量**，返回空。理由是内容形态对不上：上游按 JSON 分片
    /// 吐 `{"input": "…"}`，而 `custom_tool_call_input.delta` 的语义是**自由文本
    /// 本身的增量** —— 原样转发会让客户端把这些 JSON 外壳当成输入累积起来，最后
    /// 拿到的是一段带 `{"input":` 前缀的文本，工具照样执行不了。而增量是分片
    /// 到达的，中途无法可靠地剥出 JSON 外壳（可能停在转义序列中间）。
    ///
    /// 完整输入由收尾的 `custom_tool_call_input.done` 一次性交付（见
    /// `close_tools`），那一路拿到的是完整 `arguments`，可以正确解包。
    /// 客户端本来就必须处理 `.done`，少发增量不影响结果。
    fn tool_delta_event(
        &mut self,
        index: i64,
        item_id: &str,
        call_id: &str,
        name: &str,
        delta: &str,
        is_custom: bool,
    ) -> Vec<bytes::Bytes> {
        if is_custom {
            return Vec::new();
        }
        vec![self.event(
            "response.function_call_arguments.delta",
            json!({
                "output_index": index,
                "item_id": item_id,
                "call_id": call_id,
                "name": name,
                "delta": delta,
            }),
        )]
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
        let mut finals: Vec<ToolFinal> = Vec::new();
        for key in keys {
            let Some(tool) = self.tools.get_mut(&key) else {
                continue;
            };
            let call_id = if tool.call_id.is_empty() { random_id("call") } else { tool.call_id.clone() };
            // 只拿到 index、没拿到名字的残片：不能宣告（宣告要 name），
            // 但也不能丢 —— 补一个占位名，否则客户端少一次工具调用
            let name = if tool.name.is_empty() { "unknown".to_string() } else { tool.name.clone() };
            let arguments = if tool.arguments.is_empty() { "{}".to_string() } else { tool.arguments.clone() };
            // 用统一口径构造 item：custom 命中时要输出 custom_tool_call 与
            // 裸文本 input，类型给错客户端就按 JSON 解析，工具照样跑不起来
            let item = {
                let mut value = tool_call_item(&name, &call_id, &arguments, &self.plan);
                // item_id 要沿用宣告时那个：客户端按它对上号
                if let Some(map) = value.as_object_mut() {
                    map.insert("id".to_string(), Value::String(tool.item_id.clone()));
                }
                value
            };
            self.output_items.push((tool.index, item.clone()));
            finals.push(ToolFinal {
                index: tool.index,
                item_id: tool.item_id.clone(),
                call_id,
                name,
                announced: tool.announced,
                item,
            });
        }
        // 事件构造与状态借用分开（同上）：这里已不再借 `self.tools`
        for final_ in finals {
            let is_custom = final_
                .item
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind == "custom_tool_call");
            if final_.announced {
                if is_custom {
                    // 自由文本工具按 `input` 收尾，且不带 call_id / name
                    let input = string_field(&final_.item, "input");
                    out.push(self.event(
                        "response.custom_tool_call_input.done",
                        json!({
                            "output_index": final_.index,
                            "item_id": final_.item_id,
                            "input": input,
                        }),
                    ));
                } else {
                    let arguments = string_field(&final_.item, "arguments");
                    out.push(response_event(
                        &mut self.sequence,
                        "response.function_call_arguments.done",
                        json!({
                            "output_index": final_.index,
                            "item_id": final_.item_id,
                            "call_id": final_.call_id,
                            "name": final_.name,
                            "arguments": arguments,
                        }),
                    ));
                }
            }
            out.push(self.event(
                "response.output_item.done",
                json!({ "output_index": final_.index, "item": final_.item }),
            ));
        }
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
        let plan = tool_plan::plan_tools(request);
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
            let name = if tool.name.is_empty() { "unknown".to_string() } else { tool.name };
            let call_id = if tool.call_id.is_empty() { random_id("call") } else { tool.call_id };
            let arguments = if tool.arguments.is_empty() { "{}".to_string() } else { tool.arguments };
            // 与另外两个回程出口共用口径：custom 命中时输出 custom_tool_call
            // （item id 的前缀由 tool_call_item 内部按类型选，不必在这里管）
            output.push(tool_call_item(&name, &call_id, &arguments, &plan));
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
