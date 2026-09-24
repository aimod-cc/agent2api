//! Accio 上游 SSE 的解析与翻译（ADK 信封 → OpenAI Chat chunk）。
//!
//! ── 上游的流长什么样 ────────────────────────────────────────
//! `POST {llmBase}/generateContent` 回的是 SSE，每帧一个 JSON（**键名两种写法
//! 都接受**：客户端自己的 `fromJSON` 就是 camelCase / snake_case 双读的，
//! 我们不能假设网关只会给一种）：
//! ```text
//! data: {"content":{"role":"model","parts":[{"text":"你"}]},"partial":true}
//! data: {"content":{"role":"model","parts":[{"thought":true,"text":"想一想…"}]}}
//! data: {"content":{"role":"model","parts":[{"function_call":{"id":"call_1","name":"read","args_json":"{\"path\":\""}}]}}
//! data: {"content":{"role":"model","parts":[{"function_call":{"id":"call_1","name":"read","args_json":"a.txt\"}"}}]}}
//! data: {"turn_complete":true,"finish_reason":"STOP","usage_metadata":{"prompt_token_count":10,"candidates_token_count":5,"total_token_count":15}}
//! data: [DONE]
//! ```
//!
//! ── 三处与 OpenAI 不同，必须在这里处理 ───────────────────────
//!   1. **思考是 part 上的标记**（`{"thought":true,"text":…}`），不是独立字段
//!      → 翻成 `delta.reasoning_content`；
//!   2. **工具调用的参数是字符串**（`args_json`），而且**可能分片到达**
//!      （`tool_config` 里的 `streamFunctionCallArguments` 就是让上游这么发的）
//!      → 按「名字相同且已有内容仍是合法 JSON 就开新调用」的规则聚合；
//!   3. **业务错误写在帧里**（`error_code` / `error_message`），HTTP 常是 200
//!      → 首帧预读与流内都要判（见 `chat.rs` 与 `mod.rs` 的 `forward_conversation`）。
//!
//! ── panic=abort ────────────────────────────────────────────
//! 本文件在转发热路径上，绝不 unwrap/expect/panic：取值一律走 Option 链。

use serde_json::{json, Value};

/// 一帧里解析出的 part
#[derive(Clone, PartialEq, Debug)]
pub enum Part {
    /// 正文
    Text(String),
    /// 思考（`thought: true` 的文本 part）
    Thought(String),
    /// 工具调用（参数是字符串，可能分片）
    FunctionCall { id: String, name: String, args: String },
    /// 其它（内联数据、函数结果回显等）：转发热路径不消费
    Other,
}

/// 一帧（`data:` 行）解析后的形态
#[derive(Clone, Default, Debug)]
pub struct Frame {
    pub parts: Vec<Part>,
    pub turn_complete: bool,
    /// `finish_reason` 原文（可能为空）
    pub finish_reason: String,
    /// usage 对象（已翻成 OpenAI 字段名）
    pub usage: Option<Value>,
    pub error_code: String,
    pub error_message: String,
}

impl Frame {
    /// 业务错误帧（上游用 HTTP 200 + 帧内错误码表达额度/鉴权失败）
    pub fn is_error(&self) -> bool {
        !self.error_code.is_empty() || !self.error_message.is_empty()
    }

    /// 错误文案（分类用）
    pub fn error_text(&self) -> String {
        let mut text = String::new();
        if !self.error_code.is_empty() {
            text.push_str(&format!("[{}] ", self.error_code));
        }
        text.push_str(&self.error_message);
        text
    }
}

/// 双写取值：先 camelCase 再 snake_case
fn field<'a>(value: &'a Value, camel: &str, snake: &str) -> Option<&'a Value> {
    value
        .get(camel)
        .or_else(|| value.get(snake))
        .filter(|item| !item.is_null())
}

fn text_field(value: &Value, camel: &str, snake: &str) -> String {
    field(value, camel, snake)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

fn bool_field(value: &Value, camel: &str, snake: &str) -> bool {
    field(value, camel, snake)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// 单个 part → `Part`
fn parse_part(part: &Value) -> Part {
    let thought = part.get("thought").and_then(Value::as_bool).unwrap_or(false);
    if let Some(function_call) = part.get("functionCall").or_else(|| part.get("function_call")) {
        let id = text_field(function_call, "id", "id");
        let name = text_field(function_call, "name", "name");
        // 参数三种形态都见过：`argsJson`（字符串包 JSON）、`args_json`、
        // 以及客户端高层 API 的 `args`（对象）。取到哪个算哪个。
        let args = if let Some(raw) = function_call.get("argsJson").or_else(|| function_call.get("args_json")) {
            raw.as_str().map(str::to_string).unwrap_or_else(|| raw.to_string())
        } else if let Some(raw) = function_call.get("args") {
            if raw.is_string() {
                raw.as_str().unwrap_or("").to_string()
            } else {
                raw.to_string()
            }
        } else {
            String::new()
        };
        return Part::FunctionCall { id, name, args };
    }
    if let Some(text) = part.get("text").and_then(Value::as_str) {
        if thought {
            return Part::Thought(text.to_string());
        }
        return Part::Text(text.to_string());
    }
    Part::Other
}

/// usage 对象 → OpenAI 字段名（网关的统计与聚合层读的是这一套）
fn parse_usage(usage: &Value) -> Value {
    let number = |camel: &str, snake: &str| -> i64 {
        field(usage, camel, snake)
            .and_then(|value| value.as_i64().or_else(|| value.as_f64().map(|f| f as i64)))
            .unwrap_or(0)
    };
    let prompt = number("promptTokenCount", "prompt_token_count");
    let completion = number("candidatesTokenCount", "candidates_token_count");
    let thoughts = number("thoughtsTokenCount", "thoughts_token_count");
    let total = {
        let explicit = number("totalTokenCount", "total_token_count");
        if explicit > 0 {
            explicit
        } else {
            prompt + completion + thoughts
        }
    };
    let cached = number("cachedContentTokenCount", "cached_content_token_count");
    let mut usage = json!({
        "prompt_tokens": prompt,
        // 思考 token 计入输出（OpenAI 口径：思考也是模型产出的 token）
        "completion_tokens": completion + thoughts,
        "total_tokens": total,
    });
    if cached > 0 {
        usage["prompt_tokens_details"] = json!({ "cached_tokens": cached });
    }
    usage
}

/// 一帧 `data:` 的 JSON 文本 → [`Frame`]。
///
/// 解析失败返回 None（非 JSON 的心跳帧、`[DONE]` 之外的控制帧都按「无内容」跳过，
/// 与客户端 `processFrame` 的「空帧返回 null」同一取向）。
pub fn parse_frame(data: &str) -> Option<Frame> {
    let trimmed = data.trim();
    if trimmed.is_empty() || trimmed == "[DONE]" || trimmed == "[done]" {
        return None;
    }
    let value: Value = serde_json::from_str(trimmed).ok()?;
    let object = value.as_object()?;
    let mut frame = Frame {
        turn_complete: bool_field(&value, "turnComplete", "turn_complete"),
        finish_reason: text_field(&value, "finishReason", "finish_reason"),
        error_code: text_field(&value, "errorCode", "error_code"),
        error_message: text_field(&value, "errorMessage", "error_message"),
        ..Frame::default()
    };
    if let Some(usage) = field(&value, "usageMetadata", "usage_metadata") {
        if usage.is_object() {
            frame.usage = Some(parse_usage(usage));
        }
    }
    if let Some(content) = object.get("content") {
        if let Some(parts) = content.get("parts").and_then(Value::as_array) {
            for part in parts {
                frame.parts.push(parse_part(part));
            }
        }
    }
    Some(frame)
}

/// 一个 SSE 帧（下发用）：`data: {json}\n\n`
pub fn sse_frame(value: &Value) -> String {
    format!("data: {value}\n\n")
}

/// 流结束帧
pub fn sse_done() -> String {
    "data: [DONE]\n\n".to_string()
}

/// 逐行缓冲（跨分片半行拼接）：SSE 的 `data:` 行可能被切在任意位置
#[derive(Default)]
pub struct LineBuffer {
    pending: String,
}

impl LineBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂一段字节，产出**完整行**
    pub fn push(&mut self, chunk: &str) -> Vec<String> {
        self.pending.push_str(chunk);
        let mut lines = Vec::new();
        while let Some(index) = self.pending.find('\n') {
            let line: String = self.pending.drain(..=index).collect();
            let trimmed = line.trim_end_matches(['\r', '\n']).to_string();
            lines.push(trimmed);
        }
        lines
    }

    /// 取走还没成行的尾巴（**首帧预读换手时必须做**）。
    ///
    /// 预读把一个 chunk 里的完整行解析掉之后，同一 chunk 的末尾可能留着半行
    /// （`data: {"content":{"pa`）。缓冲区不跟着交给续传方，那半行就永远丢了 ——
    /// 续传方会把后半截当成一条新行去 JSON 解析，解析失败、**这一帧的内容
    /// 静默丢掉**（且只在「帧恰好被切在 chunk 边界」时复现，是最难查的那类）。
    pub fn take_pending(&mut self) -> String {
        std::mem::take(&mut self.pending)
    }
}

/// 面向客户端的增量
#[derive(Clone, Debug)]
pub enum Delta {
    Content(String),
    Reasoning(String),
    ToolCall { index: i64, id: Option<String>, name: Option<String>, arguments: Option<String> },
}

/// 累积中的工具调用
#[derive(Clone, Default)]
struct ToolAccum {
    id: String,
    name: String,
    args: String,
    /// 已经下发过 id/name（OpenAI 的下发规则是首帧带 id 与 name，之后只带参数）
    announced: bool,
}

/// 上游帧 → 客户端帧 的翻译状态（流式与非流式共用同一套累积规则）。
///
/// 与 Qoder 的 `Translator` 同一分工：解析与拆解只写一份，流式逐帧下发、
/// 非流式把产出聚合成一个完整 `chat.completion`。
pub struct Translator {
    pub response_id: String,
    created: i64,
    model_name: String,
    tool_state: Vec<ToolAccum>,
    role_sent: bool,
    usage: Option<Value>,
    finish_reason: Option<String>,
    content: String,
    reasoning: String,
    /// 收到过任何工具调用（决定收尾的 finish_reason 是 tool_calls 还是 stop）
    saw_tool_call: bool,
}

impl Translator {
    pub fn new(response_id: String, model_name: String) -> Self {
        Self {
            response_id,
            created: crate::server::logging::now_ms() / 1000,
            model_name,
            tool_state: Vec::new(),
            role_sent: false,
            usage: None,
            finish_reason: None,
            content: String::new(),
            reasoning: String::new(),
            saw_tool_call: false,
        }
    }

    /// 一帧 → 若干 delta（顺序即下发顺序）
    pub fn consume(
        &mut self,
        frame: &Frame,
        telemetry: Option<&std::sync::Arc<crate::server::core::upstream::usage::RequestTelemetry>>,
    ) -> Vec<Delta> {
        let mut out = Vec::new();
        if let Some(usage) = &frame.usage {
            self.usage = Some(usage.clone());
            if let Some(telemetry) = telemetry {
                telemetry.report_usage(usage);
            }
        }
        if !frame.finish_reason.is_empty() {
            self.finish_reason = Some(super::protocol::finish_reason(&frame.finish_reason).to_string());
        }
        for part in &frame.parts {
            match part {
                Part::Text(text) => {
                    if !text.is_empty() {
                        self.content.push_str(text);
                        out.push(Delta::Content(text.clone()));
                    }
                }
                Part::Thought(text) => {
                    if !text.is_empty() {
                        self.reasoning.push_str(text);
                        out.push(Delta::Reasoning(text.clone()));
                    }
                }
                Part::FunctionCall { id, name, args } => {
                    let index = self.accumulate_call(id, name, args);
                    // 先取快照再改标记：借用与可变借用分开，避免交叉持有
                    let (announce, call_id, call_name) = match self.tool_state.get(index as usize) {
                        Some(entry) => (entry.announced, entry.id.clone(), entry.name.clone()),
                        None => (true, String::new(), String::new()),
                    };
                    if let Some(entry) = self.tool_state.get_mut(index as usize) {
                        entry.announced = true;
                    }
                    // 首帧（要报 id/name）或有参数可发时下发；首帧即使参数为空也发，
                    // 否则客户端拿不到这个工具调用的名字
                    if announce || !args.is_empty() {
                        self.saw_tool_call = true;
                        out.push(Delta::ToolCall {
                            index,
                            id: announce.then(|| call_id.clone()).filter(|value| !value.is_empty()),
                            name: announce.then(|| call_name.clone()).filter(|value| !value.is_empty()),
                            arguments: Some(args.clone()),
                        });
                    }
                }
                Part::Other => {}
            }
        }
        out
    }

    /// 把一次 function_call part 并进累积表，返回它的 index。
    ///
    /// ── 分片与「一次一个」怎么区分 ───────────────────────────────
    /// 上游开了 `streamFunctionCallArguments`，参数可能分片到达（同名同 id 的
    /// 多个 part 各带一段）。但也可能一次给全（多个同名调用的完整 JSON）。
    /// 判据：**已有内容仍是合法 JSON 且新来的也是完整 JSON → 那是新调用**；
    /// 否则按续写拼上去。这样两种形态都能正确聚合（详见模块头要点 2）。
    fn accumulate_call(&mut self, id: &str, name: &str, args: &str) -> i64 {
        let can_extend = self.tool_state.last().is_some_and(|entry| {
            if !id.is_empty() && !entry.id.is_empty() && id != entry.id {
                return false;
            }
            if !name.is_empty() && !entry.name.is_empty() && name != entry.name {
                return false;
            }
            // 已有内容是完整 JSON 且新片段也完整 → 不是续写
            let previous_complete = !entry.args.is_empty() && serde_json::from_str::<Value>(&entry.args).is_ok();
            let incoming_complete = !args.is_empty() && serde_json::from_str::<Value>(args).is_ok();
            !(previous_complete && incoming_complete)
        });
        if can_extend {
            if let Some(entry) = self.tool_state.last_mut() {
                if entry.id.is_empty() && !id.is_empty() {
                    entry.id = id.to_string();
                }
                if entry.name.is_empty() && !name.is_empty() {
                    entry.name = name.to_string();
                }
                entry.args.push_str(args);
                return (self.tool_state.len() - 1) as i64;
            }
        }
        self.tool_state.push(ToolAccum {
            id: id.to_string(),
            name: name.to_string(),
            args: args.to_string(),
            announced: false,
        });
        (self.tool_state.len() - 1) as i64
    }

    /// 流式的 role 帧是否该发（第一次产出 delta 前补一帧 role）
    pub fn take_role_frame(&mut self) -> bool {
        if self.role_sent {
            return false;
        }
        self.role_sent = true;
        true
    }

    pub fn model_out(&self) -> String {
        self.model_name.clone()
    }

    pub fn response_id(&self) -> String {
        self.response_id.clone()
    }

    pub fn created(&self) -> i64 {
        self.created
    }

    /// 收尾的 finish_reason：上游给了就用它，没给按「有没有工具调用」推断
    pub fn final_finish_reason(&self) -> String {
        self.finish_reason
            .clone()
            .unwrap_or_else(|| if self.saw_tool_call { "tool_calls".to_string() } else { "stop".to_string() })
    }

    /// 收尾的工具调用（非流式聚合用）
    pub fn final_tool_calls(&self) -> Vec<Value> {
        self.tool_state
            .iter()
            .enumerate()
            .filter(|(_, entry)| !entry.name.is_empty())
            .map(|(index, entry)| {
                let arguments = if entry.args.is_empty() { "{}".to_string() } else { entry.args.clone() };
                json!({
                    "index": index,
                    "id": if entry.id.is_empty() { format!("call_{index}") } else { entry.id.clone() },
                    "type": "function",
                    "function": { "name": entry.name, "arguments": arguments },
                })
            })
            .collect()
    }

    pub fn content(&self) -> &str {
        &self.content
    }

    pub fn reasoning(&self) -> &str {
        &self.reasoning
    }

    pub fn usage(&self) -> Option<Value> {
        self.usage.clone()
    }
}
