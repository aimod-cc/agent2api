//! Qoder 上游 SSE 的解析与翻译（移植来源 `Qoder-Proxy/src/upstream.mjs` 的
//! `parseSseLine` 与 `protocol.mjs` 的 `ThinkingParser`）。
//!
//! ── 上游的流长什么样 ────────────────────────────────────────
//! 它**再包了一层**：外层信封是 `{ statusCodeValue, body }`，真正的响应在
//! `body` 字段里，而且是一个**内层 JSON 字符串**，结构才与 OpenAI 的
//! `chat.completion.chunk` 一致：
//!
//! ```text
//! data: {"statusCodeValue":200,"body":"{\"choices\":[{\"delta\":{\"content\":\"你\"}}]}"}
//! ```
//!
//! 还有两处与 OpenAI 不同，都必须在这里处理：
//!   1. **业务错误不体现在 HTTP 状态码上**（永远是 200），而是放在信封的
//!      `statusCodeValue` 里 —— 不在这里分类，客户端会把一条额度错误
//!      当成一次正常结束的空回答；
//!   2. **思考内容有时混在正文里**（`<thinking>…</thinking>` 一类标签），
//!      需要按标签拆开分别下发 `reasoning_content` 与 `content`。
//!
//! ── 为什么思考拆解要处理跨分片边界 ───────────────────────────
//! 上游会把标签和内容切在任意位置（`<thi` + `nking>`）。若只是对每片做字符串
//! 替换，碎片会漏给客户端。`ThinkingParser` 因此维护一个缓冲：只下发**确定的**
//! 前缀，把可能是标签开头/结尾的尾巴留在缓冲里等下一片 —— 这是它唯一的存在
//! 理由，也是不能简化成一次 `replace` 的原因。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：零 unwrap/expect/panic。

use serde_json::Value;

use super::protocol::{classify_upstream_error, THINK_TAGS, UpstreamKind};

/// 上游 SSE 里解析出的一条事件（源实现 `parseSseLine` 的返回联合）
pub enum SseEvent {
    /// 与语义无关的行（空行、非 JSON 行）
    Skip,
    /// 流结束（`data: [DONE]`）
    Done,
    /// 一个正常的数据块（内层 JSON，已是 OpenAI chunk 形状）
    Chunk(Value),
    /// 上游业务错误（信封里的 statusCodeValue != 200）
    Error {
        /// 信封里的业务状态码（**不是** HTTP 状态码 —— 那一个始终是 200）
        status: u16,
        /// 分类结果（决定「换账号 / 刷新重试 / 原样透传」）
        kind: UpstreamKind,
        /// 上游原文（截断后）
        raw: String,
        /// 分类后的人话
        message: String,
        /// 额度类错误带出的定价页链接
        pricing_url: Option<String>,
    },
}

/// 解析一行 `data:` 之后的内容（源实现 `parseSseLine`）。
pub fn parse_sse_line(data: &str) -> SseEvent {
    let trimmed = data.trim();
    if trimmed.is_empty() {
        return SseEvent::Skip;
    }
    if trimmed == "[DONE]" {
        return SseEvent::Done;
    }
    let Ok(envelope) = serde_json::from_str::<Value>(trimmed) else {
        return SseEvent::Skip;
    };

    // 业务错误在信封的 statusCodeValue 里（HTTP 状态码始终是 200）
    if let Some(status) = envelope.get("statusCodeValue").and_then(Value::as_i64) {
        if status != 200 {
            let raw: String = match envelope.get("body") {
                Some(Value::String(text)) => text.clone(),
                Some(other) => other.to_string(),
                None => String::new(),
            };
            let classified = classify_upstream_error(status as u16, &raw);
            return SseEvent::Error {
                status: status as u16,
                kind: classified.kind,
                raw: raw.chars().take(500).collect(),
                message: classified.message,
                pricing_url: classified.pricing_url,
            };
        }
    }

    let Some(inner) = envelope.get("body") else {
        return SseEvent::Skip;
    };
    if inner.as_str() == Some("[DONE]") {
        return SseEvent::Done;
    }
    if inner.is_null() {
        return SseEvent::Skip;
    }
    let chunk = match inner {
        Value::String(text) => match serde_json::from_str::<Value>(text) {
            Ok(parsed) => parsed,
            Err(_) => return SseEvent::Skip,
        },
        other => other.clone(),
    };
    SseEvent::Chunk(chunk)
}

/// 一行 SSE 输出的形态：`data: <payload>\n\n`
pub fn sse_frame(value: &Value) -> String {
    format!("data: {}\n\n", serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string()))
}

/// `data: [DONE]\n\n`
pub fn sse_done() -> String {
    "data: [DONE]\n\n".to_string()
}

/// 线协议解析器：把上游字节流切成一行行的 `data:` 事件。
///
/// 按**字节**缓冲而不是 String：思考内容里中文占大头，而 TCP 分片完全可能把
/// 一个 3 字节的汉字切成两半 —— 逐片解码会把那半个字变成替换字符。
/// 只在完整行上解码就没有这个问题。
#[derive(Default)]
pub struct LineBuffer {
    tail: Vec<u8>,
}

impl LineBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// 吃一段上游字节，吐出其中**完整的** `data:` 行（已去掉 `data:` 前缀）。
    ///
    /// 非 `data:` 行（注释、心跳）直接忽略：本家的上游只用 `data:` 承载内容。
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.tail.extend_from_slice(chunk);
        let mut out = Vec::new();
        let mut start = 0usize;
        while let Some(offset) = self.tail[start..].iter().position(|byte| *byte == b'\n') {
            let end = start + offset;
            let line = String::from_utf8_lossy(&self.tail[start..end]).to_string();
            let line = line.strip_suffix('\r').unwrap_or(&line).to_string();
            if let Some(data) = line.strip_prefix("data:") {
                out.push(data.to_string());
            }
            start = end + 1;
        }
        self.tail.drain(..start);
        out
    }

    /// 流结束：吐出尾行（上游没以换行收尾时的最后一条）。
    ///
    /// 与通用 SSE 层「不冲刷 tail」的取向不同：那里的 tail 可能是被切断的
    /// 残帧，宁可丢掉；而 Qoder 的上游**确实可能不发收尾换行**，
    /// 丢掉它就会丢掉最后一条内容（含 finish_reason 与 usage）。
    pub fn finish(&mut self) -> Vec<String> {
        let tail = std::mem::take(&mut self.tail);
        let line = String::from_utf8_lossy(&tail).to_string();
        let line = line.strip_suffix('\r').unwrap_or(&line).to_string();
        if let Some(data) = line.strip_prefix("data:") {
            if !data.trim().is_empty() {
                return vec![data.to_string()];
            }
        }
        Vec::new()
    }
}

/// 拆解产出的一段内容
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThinkingPiece {
    pub text: String,
    /// 真 = 思考内容（应下发 `reasoning_content`），假 = 正文
    pub is_thinking: bool,
}

/// 思考标签拆解器（源实现 `ThinkingParser` 的状态机）。
///
/// ── 为什么产出是「攒起来等取」而不是回调（与源实现的差别）───────
/// 源实现用回调，因为 JS 里那次调用是「一片进、就地下发」。Rust 侧这个解析器
/// 必须**跨片存活**（开标签被切成两半时状态要留到下一片），于是它得作为一个
/// 长驻字段挂在翻译器上。回调形态没法这样挂：`FnMut` 闭包要捕获调用方的可变
/// 状态，与「解析器自己就是那个状态」直接冲突（借用检查会拦住，硬写只能到处
/// 传 `Rc<RefCell<_>>`）。改成「解析器持有输出队列，调用方取走」后，
/// 解析器就是一个普通结构体，可以随翻译器一起活到流结束。
///
/// ── 用法（**必须**调 `finish`）──────────────────────────────
/// 内部保留可能是标签前缀的尾巴（见模块头），不收尾的话最后几个字符会永远
/// 留在缓冲里（表现为回答末尾少字）。
pub struct ThinkingParser {
    /// 已拆解、待调用方取走的产出
    out: Vec<ThinkingPiece>,
    buffer: String,
    in_thinking: bool,
    /// 进入思考时用的那个闭标签（开标签可能是 `<think>` 或 `<thinking>`）
    active_close: &'static str,
    finished: bool,
}

impl Default for ThinkingParser {
    fn default() -> Self {
        Self::new()
    }
}

impl ThinkingParser {
    pub fn new() -> Self {
        Self {
            out: Vec::new(),
            buffer: String::new(),
            in_thinking: false,
            active_close: THINK_TAGS[0].1,
            finished: false,
        }
    }

    /// 喂入一段增量
    pub fn push(&mut self, chunk: &str) {
        if chunk.is_empty() {
            return;
        }
        self.buffer.push_str(chunk);
        self.drain(false);
    }

    /// 收尾：把残留内容按当前状态输出（幂等，重复调用不会有额外产出）
    pub fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.drain(true);
        if !self.buffer.is_empty() {
            let rest = std::mem::take(&mut self.buffer);
            self.emit(rest, self.in_thinking);
        }
    }

    /// 取走当前累积的产出
    pub fn take(&mut self) -> Vec<ThinkingPiece> {
        std::mem::take(&mut self.out)
    }

    /// 记一段产出（空串不入队 —— 调用方按「有没有内容」决定要不要发帧）
    fn emit(&mut self, text: String, is_thinking: bool) {
        if text.is_empty() {
            return;
        }
        self.out.push(ThinkingPiece { text, is_thinking });
    }

    /// 这段文本的尾巴有多长可能是某个标签的前缀（需要留到下一片再判断）
    fn trailing_prefix_len(text: &str) -> usize {
        let mut max = 0usize;
        for (open, close) in THINK_TAGS {
            for tag in [*open, *close] {
                let limit = text.len().min(tag.len() - 1);
                // 只可能在**字符**边界上，从长到短试
                for len in (1..=limit).rev() {
                    if !text.is_char_boundary(text.len() - len) {
                        continue;
                    }
                    if text.ends_with(&tag[..len]) {
                        if len > max {
                            max = len;
                        }
                        break;
                    }
                }
            }
        }
        max
    }

    /// 由闭标签反查对应开标签的长度（进思考状态时要用它跳过开标签）
    fn open_len_of(close_tag: &str) -> usize {
        THINK_TAGS
            .iter()
            .find(|(_, close)| *close == close_tag)
            .map(|(open, _)| open.len())
            .unwrap_or(0)
    }

    /// 状态机主体：在正文里找开标签、在思考里找闭标签
    fn drain(&mut self, is_final: bool) {
        // 防病态输入下的无限循环（与源实现的 guard 同一目的）
        let mut guard = 0;
        while !self.buffer.is_empty() && guard < 1000 {
            guard += 1;

            if self.in_thinking {
                let close = self.active_close;
                if let Some(position) = self.buffer.find(close) {
                    if position > 0 {
                        let text = self.buffer[..position].to_string();
                        self.emit(text, true);
                    }
                    let rest = self.buffer[position + close.len()..].to_string();
                    self.buffer = strip_leading_newline(&rest);
                    self.in_thinking = false;
                    continue;
                }
                if is_final {
                    let text = std::mem::take(&mut self.buffer);
                    self.emit(text, true);
                    return;
                }
                // 留出可能是闭标签前缀的尾部
                let keep = Self::trailing_prefix_len(&self.buffer);
                let safe = self.buffer.len() - keep;
                if safe > 0 {
                    let text = self.buffer[..safe].to_string();
                    self.emit(text, true);
                    self.buffer = self.buffer[safe..].to_string();
                }
                return;
            }

            // 正文状态：找最早出现的开标签或闭标签
            let mut best_open: Option<(usize, &'static str)> = None;
            let mut best_close: Option<usize> = None;
            for (open, close) in THINK_TAGS {
                if let Some(position) = self.buffer.find(open) {
                    if best_open.map(|(at, _)| position < at).unwrap_or(true) {
                        best_open = Some((position, *close));
                    }
                }
                if let Some(position) = self.buffer.find(close) {
                    if best_close.map(|at| position < at).unwrap_or(true) {
                        best_close = Some(position);
                    }
                }
            }

            let take_open = match (best_open, best_close) {
                (Some((open_at, _)), Some(close_at)) => open_at < close_at,
                (Some(_), None) => true,
                _ => false,
            };

            if let Some((open_at, close_tag)) = best_open.filter(|_| take_open) {
                if open_at > 0 {
                    let text = self.buffer[..open_at].to_string();
                    self.emit(text, false);
                }
                let open_len = Self::open_len_of(close_tag);
                let rest = self.buffer[open_at + open_len..].to_string();
                self.buffer = rest;
                self.active_close = close_tag;
                self.in_thinking = true;
                continue;
            }

            if let Some(close_at) = best_close {
                // 单独的闭标签：丢弃它（源实现同款处理）
                if close_at > 0 {
                    let text = self.buffer[..close_at].to_string();
                    self.emit(text, false);
                }
                let close_len = THINK_TAGS
                    .iter()
                    .filter(|(_, close)| {
                        self.buffer[close_at..].starts_with(*close)
                    })
                    .map(|(_, close)| close.len())
                    .max()
                    .unwrap_or(0);
                let rest = self.buffer[close_at + close_len..].to_string();
                self.buffer = strip_leading_newline(&rest);
                continue;
            }

            if is_final {
                let text = std::mem::take(&mut self.buffer);
                self.emit(text, false);
                return;
            }
            let keep = Self::trailing_prefix_len(&self.buffer);
            let safe = self.buffer.len() - keep;
            if safe > 0 {
                let text = self.buffer[..safe].to_string();
                self.emit(text, false);
                self.buffer = self.buffer[safe..].to_string();
            }
            return;
        }
    }
}

/// 吃掉闭标签后紧跟的换行（源实现同款：先试 `\n\n` 两个，再试单个）。
///
/// 为什么是**两个**而不是一个：上游在闭标签后习惯留一个空行再接着写正文
/// （`</thinking>\n\n正文`），只吃一个会让正文以一个空行开头。
/// 源实现就是这个顺序（`startsWith('\n\n') → slice(2)`，否则 `'\n' → slice(1)`）。
/// `\r\n` 分支是本项目对 Windows 风格换行的加固，语义与单 `\n` 一致。
fn strip_leading_newline(text: &str) -> String {
    if let Some(rest) = text.strip_prefix("\n\n") {
        return rest.to_string();
    }
    if let Some(rest) = text.strip_prefix("\r\n") {
        return rest.to_string();
    }
    if let Some(rest) = text.strip_prefix('\n') {
        return rest.to_string();
    }
    text.to_string()
}

/// 去掉文本里的思考标签。
///
/// 两处用途：上游在 `reasoning_content` 里偶带标签（要剥掉）、
/// 以及上游把思考直接混在正文里而我们没启用拆解器时的兜底清理。
pub fn strip_thinking_tags(text: &str) -> String {
    let mut out = text.to_string();
    for (open, close) in THINK_TAGS {
        out = out.replace(open, "").replace(close, "");
    }
    out
}
