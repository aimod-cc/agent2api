//! Responses 的 custom（freeform）工具 ↔ Chat function 工具的互相降级。
//!
//! ── 为什么需要这一层 ────────────────────────────────────────
//! Responses 协议里有一类工具声明**没有 JSON 参数表**：
//!
//! ```json
//! {"type":"custom","name":"exec",
//!  "format":{"type":"grammar","syntax":"lark","definition":"…"}}
//! ```
//!
//! 模型对它的回复也不是 `arguments` JSON，而是一段**自由文本**——落在
//! `custom_tool_call` 项的 `input` 字段里。Codex 的 `exec` / `apply_patch`
//! 都是这个形态。
//!
//! 而本项目的上游是 Chat Completions 接口：只认「function + JSON schema 参数」。
//! 把 custom 声明原样转发没有意义（上游不认识），丢掉更糟：模型收不到任何工具
//! 声明，于是把**调用意图当正文吐出来**，客户端侧表现为一段没人解析的标记文本，
//! 磁盘上什么都不会发生，而 HTTP 200、无报错、日志无异常 —— 静默失效。
//!
//! 所以这里做双向降级：
//!   - 出站：custom 声明 → 单 `input` 字符串参数的 function 声明，把「这是自由
//!     文本」的说明与 grammar 折进 description（[`downgrade_custom_tool`]）；
//!   - 入站：上游回的 `arguments` → Responses 的裸 `input`（[`unwrap_freeform_input`]），
//!     以及反方向（[`wrap_freeform_input`]），供多轮历史回灌。
//!
//! 回程要不要还原成 `custom_tool_call` 由调用方决定，它只需要知道「哪些工具名
//! 原本是 custom 的」—— 那是 [`custom_tool_names`] 的事。

use serde_json::{json, Value};

use super::{json_text, string_field};

/// 降级后承载自由文本的字段名。
///
/// Chat 的 function 必须有 JSON schema 参数，而自由文本工具没有参数表，
/// 于是统一包成一个字段 —— 出入两个方向都用这个名字对齐。
pub const FREEFORM_FIELD: &str = "input";

/// 折进 description 的自由文本说明。
///
/// 不写这段的话，模型会按普通 function 的习惯去编 JSON 参数，或者把正文用
/// markdown 代码围栏包起来 —— 两种都会让工具拿到不能执行的东西。
const FREEFORM_HINT: &str = "\n\n[自由文本工具] 该工具没有 JSON 参数表：唯一参数 \
`input` 是要原样交给工具执行的完整文本（源码、补丁或命令）。请直接把该文本放进 \
input 字段，不要转义成字符串字面量、不要加 markdown 代码围栏、不要附加任何解释。";

/// 是否是 custom（freeform）工具声明
pub fn is_custom_tool(tool: &Value) -> bool {
    string_field(tool, "type").eq_ignore_ascii_case("custom")
}

/// custom 工具声明 → Chat 的 function 工具（出站降级）。
///
/// 名字缺失时返回 `None`（没有名字的 function 上游也认不了）。
pub fn downgrade_custom_tool(tool: &Value) -> Option<Value> {
    let name = string_field(tool, "name");
    if name.is_empty() {
        return None;
    }
    let mut description = string_field(tool, "description");
    description.push_str(FREEFORM_HINT);
    if let Some(grammar) = grammar_hint(tool) {
        description.push_str(&grammar);
    }
    Some(json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": {
                "type": "object",
                "properties": {
                    FREEFORM_FIELD: {
                        "type": "string",
                        "description": "该工具的完整自由文本输入，原样交给工具执行。",
                    },
                },
                "required": [FREEFORM_FIELD],
            },
        },
    }))
}

/// 工具声明里的 grammar（`format: {type:"grammar", syntax, definition}`）→ 提示段落。
///
/// grammar 是自由文本工具的「参数表」：不告诉模型文法，它写出来的 input 大概率
/// 不合式。所以原样折进 description，不截断 —— 截断过的文法比没有更糟。
fn grammar_hint(tool: &Value) -> Option<String> {
    let format = tool.get("format")?;
    let definition = string_field(format, "definition");
    if definition.is_empty() {
        return None;
    }
    let syntax = string_field(format, "syntax");
    let syntax = if syntax.is_empty() { "lark" } else { &syntax };
    Some(format!(
        "\n\n[输入语法] input 必须符合以下 {syntax} 文法：\n{definition}"
    ))
}

/// 上游回的 `arguments` → Responses 的裸 `input`（回程还原）。
///
/// 正常形态是降级时约定的 `{"input": "…"}`。但模型不一定听话：可能直接给裸文本，
/// 也可能把字段名写错。三种都尽量把内容取出来 —— 这里弄丢内容等于把工具调用吞掉，
/// 比多带一点噪音严重得多。
pub fn unwrap_freeform_input(arguments: &str) -> String {
    let trimmed = arguments.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    match serde_json::from_str::<Value>(trimmed) {
        Ok(Value::Object(fields)) => match fields.get(FREEFORM_FIELD) {
            Some(Value::String(text)) => text.clone(),
            Some(other) => json_text(other),
            // 空对象（`{}`）是「这次调用没有内容」，别把 JSON 字面量当文本喂给工具
            None if fields.is_empty() => String::new(),
            // 有字段但没 input：整段当自由文本，至少内容不丢
            None => trimmed.to_string(),
        },
        // 包成了 JSON 字符串（`"…"`）：剥掉引号与转义，取里面的原文
        Ok(Value::String(text)) => text,
        // 裸文本（没包成 JSON）：本来就是要的东西，原样用
        _ => trimmed.to_string(),
    }
}

/// Responses 的裸 `input` → Chat 的 `arguments` JSON（回灌历史时用）。
///
/// 与出站降级对齐：历史里的 `custom_tool_call` 也要以 function 调用的样子
/// 出现在给上游的 messages 里，否则模型看到的工具形态前后不一致。
pub fn wrap_freeform_input(input: &str) -> String {
    json!({ FREEFORM_FIELD: input }).to_string()
}
