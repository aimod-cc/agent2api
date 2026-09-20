//! 协议转换：把下游的 **Responses**（`/v1/responses`）与 **Anthropic Messages**
//! （`/v1/messages`）翻译成网关内部统一的 **Chat Completions**，再把回程翻译回去。
//!
//! ── 为什么以 Chat 为枢纽，而不是在每个适配器里各写一套 ────────
//! 本项目六家上游（WorkBuddy / 小浣熊 / CatPaw / AutoClaw / Qoder / Cline）
//! **全部经适配器层归一为 OpenAI Chat 协议**：CatPaw 的 conversation 会话协议与
//! Qoder 的 COSY 信封都在各自的 `forward_conversation` 里被翻译成 OpenAI chunk
//! 下发（见 `providers/catpaw`、`providers/qoder` 的模块头）。也就是说，
//! 「上游说 Chat」这件事在适配器边界上**已经成立**。
//!
//! 于是协议转换只有两个可能的位置：
//!   ① 在每家适配器里各写一遍（6 家 × 2 协议 = 12 套转换）；
//!   ② 在网关出入口写一次（2 套转换），让适配器完全不知情。
//! ② 明显更优：适配器不必知道下游说的是什么协议（它本来就只认 Chat），
//! 新增一家上游时也不用再补一套转换。本模块就是②的落点。
//!
//! ── 与参考实现（OmniProxy）的结构差异 ────────────────────────
//! OmniProxy 以 **Responses 为枢纽**（Chat ↔ Responses ↔ Anthropic），
//! 因为它上游同时存在三种协议、且要支持「下游 Responses → 上游 Anthropic」
//! 这类直连。本项目上游**只有 Chat 一种**，所以枢纽选 Chat：
//! 每组转换都只走一跳，少一次中转，也少一处「中转丢字段」的风险。
//! 转换规则本身（消息/工具/思考档位/usage 字段映射）大量参考了 OmniProxy
//! 的 `gatewayProtocol*.ts`，并按本项目的口径收敛。
//!
//! ── 文件分工 ────────────────────────────────────────────────
//!   mod.rs        本文件：共享原语（取值/转义/随机 id/SSE 编解码/行缓冲）
//!   responses.rs  Responses ↔ Chat（请求、非流式响应、流式 SSE）
//!   anthropic.rs  Anthropic Messages ↔ Chat（同上）
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本模块零 unwrap/expect/panic，取值一律走 Option 链；
//! 不做网络、不碰文件、不认识 axum（纯函数 + 状态机，便于单独推演）。

pub mod anthropic;
pub mod responses;

use serde_json::Value;

/// JS 的 `String(value)`：非字符串也照转（`null` → 空串，与 `value == null ? ""` 同）。
///
/// 转换层到处要按「客户端给的可能是任何 JSON 类型」处理（手写的请求体里
/// `model` 可能是数字、`name` 可能是对象），而 JS 参考实现处处是 `stringValue`。
/// 统一走这一个函数，避免各处 `as_str().unwrap_or("")` 把数字静默丢掉。
pub fn string_value(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// 取一个对象成员并转成字符串（缺失 → 空串）
pub fn string_field(value: &Value, key: &str) -> String {
    value.get(key).map(string_value).unwrap_or_default()
}

/// JS 的 `jsonText(value)`：字符串原样返回，其余 JSON 序列化。
///
/// 用途是「把任意 JSON 塞进一个字符串字段」（工具参数、工具输出）。
/// 序列化失败只可能是内部数据坏了（`Value` 本身一定能序列化，除非有环），
/// 这里给空串而不是 panic。
pub fn json_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// JS 真值判定（`Boolean(x)`）——参考实现里满地的 `if (x)` 都是这个语义
pub fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().map(|item| item != 0.0).unwrap_or(false),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}
/// 生成一个带前缀的随机 id（`msg_<32位十六进制>` 形态）。
///
/// ── 为什么不用时间戳或自增 ────────────────────────────────────
/// 这些 id 会出现在**下发帧**里（`response.id` / `message.id` / `tool_use.id`），
/// 客户端可能用它做关联与去重。同一秒内的两次请求若拿到相同 id，
/// 客户端侧的关联就会串台 —— 时间戳做不到唯一，自增又要引入全局状态。
/// 随机源走 `getrandom`（本项目已是直接依赖，Qoder 的 PKCE 用它）。
///
/// 随机源失败（几乎不可能）时退回时间戳：id 的**唯一性**在那种情况下降级，
/// 但比返回空串好 —— 空 id 会让客户端直接解析失败。
pub fn random_id(prefix: &str) -> String {
    let mut bytes = [0u8; 16];
    if getrandom::getrandom(&mut bytes).is_err() {
        return format!("{prefix}_{:x}", crate::server::logging::now_ms());
    }
    let mut out = String::with_capacity(prefix.len() + 1 + 32);
    out.push_str(prefix);
    out.push('_');
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// 从内容块数组里抽出纯文本（只认 text 类块）。
///
/// 对应参考实现的 `contentText`：字符串直接返回；数组则把
/// `text` / `input_text` / `output_text` 三类块的 `text` 拼接。
/// 其它类型（图片、工具结果）**不参与拼接** —— 它们是结构化内容，
/// 拍平成文本会丢失语义，需要它们的调用点各自处理。
pub fn content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => {
            let mut out = String::new();
            for part in parts {
                if let Some(text) = part.as_str() {
                    out.push_str(text);
                    continue;
                }
                let kind = part.get("type").map(string_value).unwrap_or_default();
                let kind = kind.to_lowercase();
                if matches!(kind.as_str(), "text" | "input_text" | "output_text") {
                    out.push_str(&string_field(part, "text"));
                }
            }
            out
        }
        _ => String::new(),
    }
}

/// 内容块数组（非数组时给空切片语义）——大量调用点要遍历它
pub fn content_parts(content: &Value) -> &[Value] {
    match content.as_array() {
        Some(parts) => parts.as_slice(),
        None => &[],
    }
}
/// 组装一条带 `event:` 行的 SSE 帧（Responses 与 Anthropic 都要求事件名）。
///
/// Anthropic 官方 SSE **只发 `event:` 行 + `data:` 行**（`data` 里也带 `type`），
/// Responses 同形。两家的客户端解析器都按 `event:` 分派，所以事件名必须发。
pub fn event_frame(event: &str, value: &Value) -> bytes::Bytes {
    let text = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    bytes::Bytes::from(format!("event: {event}\ndata: {text}\n\n"))
}
/// 把一段文本按 SSE 规范拆成一个个 `data:` 载荷。
///
/// ── 为什么需要它（不能直接按 `\n` 切）────────────────────────
/// 上游字节流是**按 TCP 分片**到达的，一帧 JSON 完全可能被切成两半
/// （分片点落在一行中间）。所以必须有一个跨 chunk 的行缓冲：
/// 只处理到最后一个 `\n` 为止，剩下的半行留到下一段。
/// 这与 `core::upstream::sse` 的 ReasoningCoalescer 是同一套缓冲语义，
/// 但那个状态机还兼职「reasoning 合并 + 透传」，这里只需要**解析**。
///
/// 缓冲按**字节**而不是 String：思考内容里中文占大头，而分片完全可能把一个
/// 3 字节的汉字切成两半，按 String 缓冲会把那一半解码成 U+FFFD（内容损坏）。
#[derive(Default)]
pub struct SseLineBuffer {
    tail: Vec<u8>,
}

impl SseLineBuffer {
    pub fn new() -> Self {
        Self { tail: Vec::new() }
    }

    /// 吃一段字节，吐出其中**完整**的 `data:` 载荷（0..n 个）。
    ///
    /// 返回值里 `None` 表示 `[DONE]`（调用方据此收尾）。非 `data:` 行
    /// （`event:` / 注释 / 空行）在这里被跳过：本模块的调用方只关心载荷，
    /// 事件名从 JSON 的 `type` 字段读（两家的 SSE 都在 data 里带了 type，
    /// 这也是参考实现 `event.type || frame.event` 的取值顺序）。
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Option<String>> {
        let mut out: Vec<Option<String>> = Vec::new();
        self.tail.extend_from_slice(chunk);
        let mut start = 0usize;
        while let Some(offset) = self.tail[start..].iter().position(|byte| *byte == b'\n') {
            let end = start + offset;
            let line = String::from_utf8_lossy(&self.tail[start..end]).to_string();
            Self::take_line(&line, &mut out);
            start = end + 1;
        }
        self.tail.drain(..start);
        out
    }

    /// 流结束：把残留的半行也当一帧处理。
    ///
    /// ── 为什么这里**要**冲刷（与 ReasoningCoalescer 相反）────────
    /// 那个状态机冲刷 tail 会向下游透传一条可能残缺的帧，风险大于收益，
    /// 所以它刻意不冲刷。这里不同：我们是**翻译器**，残缺的 JSON 会被
    /// `serde_json` 拒绝并静默丢弃（不会构造出非法帧），而「上游发完最后一帧
    /// 却没带收尾换行」这种情况（`a` 类）能因此被正确识别 —— 少了这一步，
    /// 流的最后一个内容分片会凭空消失。
    pub fn finish(&mut self) -> Vec<Option<String>> {
        let mut out: Vec<Option<String>> = Vec::new();
        if self.tail.is_empty() {
            return out;
        }
        let line = String::from_utf8_lossy(&self.tail).to_string();
        self.tail.clear();
        Self::take_line(&line, &mut out);
        out
    }

    /// 处理一行：只取 `data:` 行，跳过事件名/注释/空行
    fn take_line(line: &str, out: &mut Vec<Option<String>>) {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let Some(rest) = line.strip_prefix("data:") else {
            return; // event: / id: / 注释 / 空行都不带载荷
        };
        let data = rest.trim();
        if data.is_empty() {
            return;
        }
        if data == "[DONE]" {
            out.push(None);
            return;
        }
        out.push(Some(data.to_string()));
    }
}
