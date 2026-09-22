//! SSE reasoning 帧合并（对照 Node 版 workbuddy-upstream-client.mjs 的
//! createReasoningCoalescingStream，114-176 行）。
//!
//! ── 为什么需要它 ────────────────────────────────────────────
//! 上游把思考（reasoning_content）拆成 1-2 个词一帧的小分片，部分客户端按
//! SSE 帧渲染思考块，会把一句思考碎成几十个小块。透传时把相邻 reasoning 帧
//! 攒到 ≥ REASONING_COALESCE_CHARS（或遇到非 reasoning 事件/流结束）再下发，
//! **只影响分帧粒度，不改变任何内容**。
//!
//! ── 契约（逐条对照 Node 的 Transform）────────────────────────
//!   - 按 `\n` 切行，**跨 chunk 的半行留在 tail 里**（网络分片不会按行对齐，
//!     没有 tail 缓冲就会把一帧 JSON 截成两半，两边各自解析失败）
//!   - 空行跳过；非 `data:` 行**原样 + `\n\n`** 透传（Node 也是这样补的，
//!     虽然会改变原始帧边界，但这是既有行为，保持一致）
//!   - `data: [DONE]` → 先冲刷累积的 reasoning 帧，再原样发 `data: [DONE]\n\n`
//!   - `data:` 里不是合法 JSON → 原样透传（不吞掉上游的异常帧）
//!   - 合法 JSON：记录帧元数据（id/model/created，取**最近一帧**的有效值）
//!   - 纯 reasoning 增量（有 reasoning_content 且没有 content/tool_calls/function_call）
//!     → 只累积，攒够阈值才发一帧
//!   - 其余事件 → 先把累积的 reasoning 冲刷成一帧，再原样透传该事件
//!   - 流结束时（flush）冲刷剩余的 reasoning
//!
//! 实现方式是**手写状态机**而不是 `Stream` 适配器：转发链路需要
//! 「请求 → 响应头 → 字节流」三段的显式控制（见 forward.rs），
//! 状态机让「一个 chunk 进、零到多个 chunk 出」这件事直白可读。
//!
//! ── usage 旁路提取（请求统计）────────────────────────────────
//! 本状态机是 SSE 逐行解析的**唯一入口**，所以 usage 提取挂在这里
//! （`handle_line` 里那一段）：只读一眼 JSON 的 `usage` 成员、写进
//! `usage::RequestTelemetry`，完全不参与帧的构造 —— 帧内容与不接钩子时
//! 逐字节一致（详见该处的注释）。
//!
//! ── model 名回写（Agent2API W3-T4）────────────────────────────
//! 小浣熊上游会把响应 chunk 的 `model` 换成它自己的内部名，而客户端认的是自己
//! 请求时给的名字（源实现 `raccoon-sse-pipe.mjs` 的 `rewriteSseLine`）。
//! 「要不要改写」由**适配器**回答（`ProviderAdapter::sse_model_rewrite`），
//! 本状态机只是唯一的下发出口，因此改写动作落在这里。
//! **默认关闭**：`rewrite` 为 None 时，帧的字节与接入前完全一致
//! （workbuddy 的透传逐字节不变是硬要求）。

use std::sync::Arc;

use bytes::Bytes;
use serde_json::Value;

use crate::server::logging;

use super::usage::RequestTelemetry;

/// 思考（reasoning_content）帧合并阈值（照抄 Node 的 REASONING_COALESCE_CHARS）
pub const REASONING_COALESCE_CHARS: usize = 60;

/// 一帧的输出：`Bytes` 已经是完整的 `data: ...\n\n` 字节串
pub type Frame = Bytes;

/// model 名回写的参数（见模块头；只有声明了 `sse_model_rewrite()` 的适配器才有）
#[derive(Clone, Debug)]
pub struct ModelRewrite {
    /// 客户端请求的模型名（回写值）
    pub requested: String,
}

/// 帧元数据（对应 Node 的 `meta = { id, model, created }`）
#[derive(Default, Clone, Debug)]
struct FrameMeta {
    id: Option<Value>,
    model: Option<Value>,
    created: Option<Value>,
}

/// reasoning 合并状态机
pub struct ReasoningCoalescer {
    acc: String,
    meta: FrameMeta,
    /// 跨 chunk 的半行缓冲（网络分片不会按行对齐）
    ///
    /// 缓冲的是**字节**而不是 String：思考内容里中文字符占大头，而 TCP 分片
    /// 完全可能把一个 3 字节的汉字切成两半。Node 版是 `tail += chunk.toString('utf8')`，
    /// 分片落在字符中间时那一半会被解码成 U+FFFD —— 属于上游分片碰巧导致的
    /// 内容损坏。这里按字节缓冲、只在完整行上解码，行为上更正确；
    /// 对「正常分片」的输出与 Node 完全一致。
    tail: Vec<u8>,
    /// usage 旁路槽（可选）：见 `report_usage` 与 `usage.rs` 的模块说明。
    /// 放在这里是因为**本状态机就是 SSE 逐行解析的唯一入口** —— 上游每个
    /// `data:` 帧都会经过 `handle_line`，顺手读一眼 `usage` 不需要再插一层
    /// 转发器，也就不会给透传路径增加任何中间结构。
    telemetry: Option<Arc<RequestTelemetry>>,
    /// model 名回写参数（可选；见模块头）。None = 原样透传上游的 model 字段
    rewrite: Option<ModelRewrite>,
}

impl Default for ReasoningCoalescer {
    fn default() -> Self {
        Self::new()
    }
}

impl ReasoningCoalescer {
    pub fn new() -> Self {
        Self {
            acc: String::new(),
            meta: FrameMeta::default(),
            tail: Vec::new(),
            telemetry: None,
            rewrite: None,
        }
    }

    /// 带 usage 旁路槽的合并器（流式转发用；`new()` 保留给无统计需求的调用点）
    pub fn with_telemetry(telemetry: Arc<RequestTelemetry>) -> Self {
        Self { telemetry: Some(telemetry), ..Self::new() }
    }

    /// 设置 model 名回写（转发链路按适配器的 `sse_model_rewrite()` 决定是否调用）。
    /// 未被调用时行为与接入前逐字节一致。
    pub fn with_model_rewrite(mut self, rewrite: Option<ModelRewrite>) -> Self {
        self.rewrite = rewrite;
        self
    }

    /// 吃一段上游字节，吐出要下发给客户端的帧（0..n 个）
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Frame> {
        let mut out: Vec<Frame> = Vec::new();
        self.tail.extend_from_slice(chunk);
        // 逐行消费：只处理到最后一个 '\n' 为止，剩下的（半行）留在 tail 里
        let mut start = 0usize;
        while let Some(offset) = self.tail[start..].iter().position(|byte| *byte == b'\n') {
            let end = start + offset;
            let line = String::from_utf8_lossy(&self.tail[start..end]).to_string();
            self.handle_line(&line, &mut out);
            start = end + 1;
        }
        self.tail.drain(..start);
        out
    }

    /// 流结束：冲刷累积的 reasoning。
    ///
    /// ── 为什么不冲刷 tail（与 Node 一致，且更安全）────────────────
    /// `tail` 里只剩「不以 `\n` 结尾的半行」。两种情形：
    ///   a. 上游把最后一个事件发完却没有收尾换行 → 那半行其实是一条完整帧；
    ///   b. 上游在帧中间被切断 → 那半行是残缺 JSON。
    /// Node 的 flush 只冲刷 acc、**不动 tail**，于是 (b) 被丢掉、(a) 也被丢掉。
    /// 这里保持同一行为：把残缺帧当普通行透传会送出一条非法 `data:` 行，
    /// 客户端解析器大概率直接报错；宁可少一帧（且上游正常都以 `\n\n` 收尾，
    /// 这条路径实际不会走到），也不要制造一次客户端侧解析失败。
    pub fn finish(&mut self) -> Vec<Frame> {
        let mut out: Vec<Frame> = Vec::new();
        if !self.acc.is_empty() {
            out.push(self.coalesced_frame());
            self.acc.clear();
        }
        out
    }

    /// 处理一行（已去掉行尾的 `\r`）
    fn handle_line(&mut self, raw: &str, out: &mut Vec<Frame>) {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.trim().is_empty() {
            return;
        }
        if !line.starts_with("data:") {
            out.push(plain_frame(line));
            return;
        }
        let data = line[5..].trim();
        if data == "[DONE]" {
            if !self.acc.is_empty() {
                out.push(self.coalesced_frame());
                self.acc.clear();
            }
            out.push(Bytes::from_static(b"data: [DONE]\n\n"));
            return;
        }
        let Ok(chunk) = serde_json::from_str::<Value>(data) else {
            // 解析失败的行原样透传：上游的异常帧不能被我们吞掉
            out.push(plain_frame(line));
            return;
        };
        if chunk.is_object() {
            // ── usage 旁路提取（请求统计）──────────────────────────
            // 位置放在「已确定这是合法 JSON 对象」之后、「本行还没被改写」之前。
            // 为什么**不会影响透传**：这里只读 `chunk` 的一个成员并把它拷进
            // 另一个结构体，既不修改 `chunk` 也不参与下面 `out` 的构造 ——
            // 无论命中与否，本函数吐出的帧都与不接这个钩子时逐字节一致。
            // 提取失败（usage 缺失/非对象）静默跳过：统计少记一条可以接受，
            // 因此这里没有错误分支，也就没有「异常帧被吞掉」的可能。
            if let Some(telemetry) = &self.telemetry {
                if let Some(usage) = chunk.get("usage") {
                    telemetry.report_usage(usage);
                }
            }
            // 元数据取上游最近一帧的有效值（Node: `if (chunkObj.id) meta.id = ...`）
            if let Some(id) = chunk.get("id").filter(|value| is_truthy(value)) {
                self.meta.id = Some(id.clone());
            }
            if let Some(model) = chunk.get("model").filter(|value| is_truthy(value)) {
                self.meta.model = Some(model.clone());
            }
            if let Some(created) = chunk.get("created").filter(|value| is_truthy(value)) {
                self.meta.created = Some(created.clone());
            }
        }
        let delta = chunk
            .get("choices")
            .and_then(|choices| choices.get(0))
            .and_then(|choice| choice.get("delta"));
        let reasoning = delta
            .and_then(|delta| delta.get("reasoning_content"))
            .and_then(Value::as_str)
            .map(str::to_string);
        // Node: `typeof content === 'string' && content.length > 0 || tool_calls?.length || function_call`
        let other_delta = delta
            .map(|delta| {
                let content = delta
                    .get("content")
                    .and_then(Value::as_str)
                    .map(|text| !text.is_empty())
                    .unwrap_or(false);
                let tool_calls = delta
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .map(|items| !items.is_empty())
                    .unwrap_or(false);
                // `delta.function_call` 只判存在性（Node 是真值判定）
                let function_call = delta.get("function_call").map(is_truthy).unwrap_or(false);
                content || tool_calls || function_call
            })
            .unwrap_or(false);

        if reasoning.is_some() && !other_delta {
            if let Some(text) = reasoning {
                self.acc.push_str(&text);
            }
            if self.acc.chars().count() >= REASONING_COALESCE_CHARS {
                out.push(self.coalesced_frame());
                self.acc.clear();
            }
            return;
        }
        // 非纯 reasoning 事件：先冲刷累积，再透传（可回写 model，见模块头）
        if !self.acc.is_empty() {
            out.push(self.coalesced_frame());
            self.acc.clear();
        }
        out.push(self.rewritten_frame(line, &chunk));
    }

    /// 透传一帧，必要时把 `model` 改写成客户端请求的名字。
    ///
    /// ── 为什么只改 model、不做别的 ─────────────────────────────
    /// 回写是**客户端可见语义**的修正（客户端按自己给的名字识别响应），
    /// 因此值得做；帧里的其它字段一律原样透传 —— 上游的异常/噪声字段
    /// （空 tool_calls 数组之类）即便看起来像是 bug，也不该由网关擅自删除，
    /// 那会让「代理看到的内容」与「上游发的」出现无从解释的差异。
    /// 未配置回写（workbuddy）时返回**原行的字节**，透传逐字节不变。
    ///
    /// 序列化用 `serde_json::to_string`（紧凑、无空格），与源实现
    /// `JSON.stringify(parsed)` 同形；失败时退回原行（宁可少一次改写，
    /// 也不要下发一条拼坏的帧）。
    fn rewritten_frame(&self, line: &str, chunk: &Value) -> Frame {
        let Some(rewrite) = &self.rewrite else {
            return plain_frame(line);
        };
        let Some(object) = chunk.as_object() else {
            return plain_frame(line);
        };
        // 上游没带 model 时**不补**：源实现同样只在 `'model' in parsed` 时才改写
        // （补一个客户端自己会填的字段没有意义，反而给「上游到底回了什么」加噪音）
        if !object.contains_key("model") {
            return plain_frame(line);
        }
        let current = object.get("model").and_then(Value::as_str).unwrap_or("");
        if current == rewrite.requested {
            return plain_frame(line);
        }
        let mut next = chunk.clone();
        if let Some(map) = next.as_object_mut() {
            map.insert(
                "model".to_string(),
                Value::String(rewrite.requested.clone()),
            );
        }
        match serde_json::to_string(&next) {
            Ok(text) => Bytes::from(format!("data: {text}\n\n")),
            Err(_) => plain_frame(line),
        }
    }

    /// 合并帧：`{id, object:'chat.completion.chunk', created, model, choices:[...]}`
    ///
    /// id/created 的兜底文案照抄 Node（`wb-coalesce-<毫秒>` / 当前秒）。
    ///
    /// ── 为什么手写 JSON 文本而不是 `json!` + 序列化 ──────────────
    /// 本项目的 serde_json 未开 `preserve_order`，`Value::Object` 是按键排序的
    /// BTreeMap，序列化出来的成员顺序是 `choices, created, id, model, object`；
    /// 而 Node 的 `JSON.stringify` 保持对象字面量的**书写顺序**。
    /// 两者都是合法 JSON、语义等价，但这是**我们自己拼出来的帧**（不同于
    /// 透传帧 —— 那些是上游原始字节，逐字节不变），完全可以做到与 Node
    /// 完全一致。既然做得到，就让「相同输入 → 相同字节输出」成立，
    /// 客户端/测试做字节比对时不会出现假阳性差异。
    /// 各值用 `serde_json::to_string` 逐个转义，拼出的仍是合法 JSON。
    fn coalesced_frame(&self) -> Frame {
        let id = match &self.meta.id {
            Some(value) if is_truthy(value) => value.clone(),
            _ => Value::String(format!("wb-coalesce-{}", logging::now_ms())),
        };
        let created = match &self.meta.created {
            Some(value) if is_truthy(value) => value.clone(),
            _ => Value::from(logging::now_ms() / 1000),
        };
        let model = match &self.meta.model {
            // 配置了回写时，合并帧也用客户端请求的名字（源实现 takeFrame 就是
            // 用 requestedModel 拼这一帧）；未配置回写时沿用上游最近一帧的值
            _ if self.rewrite.is_some() => Value::String(
                self.rewrite
                    .as_ref()
                    .map(|rewrite| rewrite.requested.clone())
                    .unwrap_or_default(),
            ),
            Some(value) if is_truthy(value) => value.clone(),
            _ => Value::String(String::new()),
        };
        // 逐个值做 JSON 编码（字符串会带引号并转义，数字/布尔/null 原样）
        let encode = |value: &Value| serde_json::to_string(value).unwrap_or_else(|_| "null".to_string());
        let text = format!(
            r#"{{"id":{},"object":"chat.completion.chunk","created":{},"model":{},"choices":[{{"index":0,"delta":{{"reasoning_content":{}}},"finish_reason":null}}]}}"#,
            encode(&id),
            encode(&created),
            encode(&model),
            encode(&Value::String(self.acc.clone())),
        );
        Bytes::from(format!("data: {text}\n\n"))
    }
}

/// 一帧 SSE：`data: <JSON>\n\n`（Node 的 sseFrame）
pub fn sse_frame(value: &Value) -> Frame {
    let text = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    Bytes::from(format!("data: {text}\n\n"))
}

/// 非 `data:` 行的透传形态：原样 + `\n\n`（Node: `line + '\n\n'`）
fn plain_frame(line: &str) -> Frame {
    Bytes::from(format!("{line}\n\n"))
}

/// 真值判定（Node 里这一串 `if (chunkObj.id)` 都是真值判定）
fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().map(|item| item != 0.0).unwrap_or(false),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}
