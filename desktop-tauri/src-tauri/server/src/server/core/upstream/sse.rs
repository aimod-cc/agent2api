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
//!
//! ── 丢弃「整片只有换行」的 content 分片 ──────────────────────
//! WorkBuddy（copilot.tencent.com）与 AutoClaw 两条上游在生成慢的时候，会在
//! 相邻两个正文分片之间插一帧 `delta.content` 只含换行的分片 —— 那是它们的
//! 保活节拍（同一个流里 `: heartbeat` 注释行也在发），不是模型写的正文。
//! 实测口径（NAS 生产库 `request_raw` 存的是**下发给客户端的字节**）：受影响的
//! 请求里这类分片占正文分片的 36–39%，且**二值分布** —— 一条请求要么一个都没有，
//! 要么每片之间都插一个（同一账号同一分钟内两种请求并存；出问题的请求中位耗时
//! 15.3 秒 / 首响 4.1 秒，干净的只有 7.6 秒 / 2.8 秒，即「生成比保活节拍慢」时
//! 才发得出来）。客户端按 markdown 渲染时一个换行就是一个 `<br>`，于是一段
//! 「Let me run the unit tests」被排成一行一个词 —— 读起来就是「错行」。
//!
//! 判定与丢弃都放在本状态机：这里是 SSE 逐行解析的唯一出口，透传路径别处再动
//! 就得给每家插一层转发器。**默认关闭**（`strip_newline_chunks = false`），
//! 关着时帧的字节与接入前逐字一致；开与不开由**适配器**回答
//! （`ProviderAdapter::sse_strip_newline_chunks`）。判据刻意做成「宁可不丢」，
//! 见 [`is_newline_keepalive`]。

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

/// 下发帧的两项改写开关：都由**适配器**回答，通用层（本状态机与 `aggregate`）
/// 只执行，不在那里出现 provider 分支。
///
/// 把两项打包成一个结构而不是各加一个位置参数：它们在链路上**永远同源**
/// （都来自同一次 `adapter` 查询 + 同一个请求的模型名），拆成两个参数会让
/// `ForwardStream` / 聚合器的五六个签名各多一个裸 `bool`，调用点上根本读不出
/// 那个 `true` 是哪一项。
#[derive(Clone, Debug, Default)]
pub struct FramePolicy {
    /// model 名回写（None = 原样透传上游的 `model`）
    pub rewrite: Option<ModelRewrite>,
    /// 丢弃「整片只有换行」的 content 分片（见模块头与 [`is_newline_keepalive`]）
    pub strip_newline_chunks: bool,
}

impl FramePolicy {
    /// 只带 model 回写、其余按默认（自定义家的转发路径用它：那家的保活形态
    /// 没有实测过，不去动它的字节）
    pub fn with_rewrite(rewrite: Option<ModelRewrite>) -> Self {
        Self { rewrite, strip_newline_chunks: false }
    }
}

/// 这一帧是不是「整片只有换行」的保活分片（判据全部是**宁可不丢**的方向）。
///
/// 命中要同时满足下面每一条：
///   - `choices[0].delta.content` 是非空、且**只由换行符组成**的字符串；
///   - 同一个 delta 里没有别的有效载荷：`reasoning_content` 为空/缺失、
///     `tool_calls` 空数组、`function_call` 假值 —— 一帧里只要还有别的真东西，
///     它就不只是节拍，不能丢；
///   - `choices[0].finish_reason` 是 null 或空串：终端帧丢了客户端就等不到收尾；
///   - 顶层 `usage` 缺失或为 null：usage 帧同理（本函数只在旁路提取**之后**
///     才被调用，所以统计不会因为丢弃而少记）；
///   - 顶层没有 `error`：流内错误帧一律保留。
///
/// `delta.role` 存在**不影响**判定：AutoClaw 的每一帧都带 `role:"assistant"`，
/// 把它算进「有别的东西」就等于一条都丢不掉。
pub fn is_newline_keepalive(chunk: &Value) -> bool {
    let Some(object) = chunk.as_object() else {
        return false;
    };
    if object.contains_key("error") {
        return false;
    }
    if chunk.get("usage").is_some_and(|usage| !usage.is_null()) {
        return false;
    }
    let Some(choice) = chunk
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
    else {
        return false;
    };
    if choice.get("finish_reason").is_some_and(is_truthy) {
        return false;
    }
    let Some(delta) = choice.get("delta").and_then(Value::as_object) else {
        return false;
    };
    let newline_only = delta
        .get("content")
        .and_then(Value::as_str)
        .map(|text| !text.is_empty() && text.chars().all(|ch| ch == '\n' || ch == '\r'))
        .unwrap_or(false);
    if !newline_only {
        return false;
    }
    // 除 content 之外还有别的载荷就不算纯噪声
    let carries_reasoning = delta
        .get("reasoning_content")
        .and_then(Value::as_str)
        .map(|text| !text.is_empty())
        .unwrap_or(false);
    let carries_tool_calls = delta
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|items| !items.is_empty())
        .unwrap_or(false);
    let carries_function_call = delta.get("function_call").map(is_truthy).unwrap_or(false);
    !(carries_reasoning || carries_tool_calls || carries_function_call)
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
    /// 丢弃保活换行分片（见模块头与 [`is_newline_keepalive`]）。
    /// false = 帧的字节与接入前逐字一致。
    strip_newline_chunks: bool,
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
            strip_newline_chunks: false,
        }
    }

    /// 带 usage 旁路槽的合并器（流式转发用；`new()` 保留给无统计需求的调用点）
    pub fn with_telemetry(telemetry: Arc<RequestTelemetry>) -> Self {
        Self { telemetry: Some(telemetry), ..Self::new() }
    }

    /// 装上帧改写策略（model 名回写 + 保活换行的丢弃；见模块头）。
    /// 未被调用时行为与接入前逐字节一致。
    pub fn with_policy(mut self, policy: FramePolicy) -> Self {
        self.rewrite = policy.rewrite;
        self.strip_newline_chunks = policy.strip_newline_chunks;
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
        // ── 保活换行分片的丢弃（见模块头与 `is_newline_keepalive`）──────
        // 位置在 usage 旁路与元数据记录**之后**：这一帧带的 usage / id / model /
        // created 照常进统计与合并帧的元数据，丢的只是「把这一帧下发出去」这个动作。
        // 这里**不冲刷 acc** —— 一帧纯噪声不该把攒了一半的思考提前结掉，
        // 否则「开启丢弃」会顺带改变 reasoning 的分帧粒度（那是另一件事）。
        if self.strip_newline_chunks && is_newline_keepalive(&chunk) {
            return;
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn raw(value: Value) -> String {
        format!("data: {}\n\n", serde_json::to_string(&value).expect("测试里的帧必须是合法 JSON"))
    }

    /// 上游（WorkBuddy）形态的一帧：带 usage:null 与 finish_reason 两个尾巴字段，
    /// 与真实流里的样子对齐 —— 判据依赖这两处，测试不能给一个过于干净的形状。
    fn frame(delta: Value) -> String {
        raw(json!({
            "id": "cmb-1",
            "object": "chat.completion.chunk",
            "created": 1_700_000_000,
            "model": "deepseek-v4.1-flash",
            "choices": [{"index": 0, "delta": delta, "finish_reason": null}],
            "usage": Value::Null,
        }))
    }

    fn coalescer(strip: bool) -> ReasoningCoalescer {
        ReasoningCoalescer::new().with_policy(FramePolicy {
            rewrite: None,
            strip_newline_chunks: strip,
        })
    }

    fn feed(coalescer: &mut ReasoningCoalescer, text: &str) -> String {
        coalescer
            .push(text.as_bytes())
            .iter()
            .map(|frame| String::from_utf8_lossy(frame).to_string())
            .collect()
    }

    #[test]
    fn a_newline_only_chunk_passes_through_when_the_policy_is_off() {
        let keepalive = frame(json!({"content": "\n"}));
        let mut off = coalescer(false);
        let out = feed(&mut off, &keepalive);
        // 关着时是逐字节透传：整帧原样出去
        assert_eq!(out, keepalive, "关着时帧的字节必须一模一样");
        assert!(out.contains("\"content\":\"\\n\""), "关着时不能动这一帧: {out}");
    }

    #[test]
    fn a_newline_only_chunk_is_dropped_when_the_policy_is_on() {
        let mut on = coalescer(true);
        assert!(feed(&mut on, &frame(json!({"content": "\n"}))).is_empty());
        assert!(feed(&mut on, &frame(json!({"content": "\n\n\n"}))).is_empty());
        assert!(feed(&mut on, &frame(json!({"content": "\r\n"}))).is_empty());
    }

    #[test]
    fn autoclaw_shape_is_dropped_too_role_included() {
        // 本家每一帧都带 `role:"assistant"`，且没有 `finish_reason` 键 ——
        // role 若算「有别的有效载荷」，这一家一条都丢不掉（见判据的说明）。
        let dropped = raw(json!({
            "id": "20260927001447d076",
            "created": 1_790_439_287,
            "object": "chat.completion.chunk",
            "model": "glm-5.3-flash",
            "choices": [{"index": 0, "delta": {"role": "assistant", "content": "\n"}}],
        }));
        let mut on = coalescer(true);
        assert!(feed(&mut on, &dropped).is_empty());
        let mut off = coalescer(false);
        assert!(!feed(&mut off, &dropped).is_empty());
    }

    #[test]
    fn a_dropped_keepalive_does_not_flush_the_reasoning_accumulator() {
        let reasoning = frame(json!({"reasoning_content": "abc"}));
        let keepalive = frame(json!({"content": "\n"}));
        let mut on = coalescer(true);
        let mut out = feed(&mut on, &reasoning);
        out.push_str(&feed(&mut on, &keepalive));
        assert!(out.is_empty(), "纯噪声帧既不该下发，也不该把攒了一半的思考结掉: {out}");
        // 关掉时它是普通的「非 reasoning 事件」，照旧会冲刷 —— 这条对照证明
        // 开启丢弃确实改变了这一帧的处理路径，而不是一直如此
        let mut off = coalescer(false);
        let mut out_off = feed(&mut off, &reasoning);
        out_off.push_str(&feed(&mut off, &keepalive));
        assert!(out_off.contains("reasoning_content"), "关着时应先冲刷思考帧: {out_off}");
    }

    #[test]
    fn terminal_usage_and_error_frames_are_never_dropped() {
        let cases = vec![
            // 终端帧：丢了客户端就等不到收尾
            json!({"choices": [{"index": 0, "delta": {"content": "\n"}, "finish_reason": "stop"}]}),
            // usage 帧：丢了统计少一条
            json!({"choices": [{"index": 0, "delta": {"content": "\n"}}], "usage": {"total_tokens": 3}}),
            // 流内错误帧
            json!({"error": {"message": "\n"}, "choices": [{"index": 0, "delta": {"content": "\n"}}]}),
        ];
        for case in cases {
            let mut on = coalescer(true);
            let text = raw(case.clone());
            let out = feed(&mut on, &text);
            assert!(!out.is_empty(), "这类帧绝不能被当成保活噪声丢掉: {case}");
            assert_eq!(out, text, "保留时原样透传: {case}");
        }
    }

    #[test]
    fn text_bearing_frames_are_untouched_by_the_strip() {
        let mut on = coalescer(true);
        for delta in [
            json!({"content": " real"}),
            json!({"content": ".\n\n"}),
            json!({"content": "\n \n"}),
            json!({"content": "端"}),
        ] {
            let text = frame(delta.clone());
            assert_eq!(feed(&mut on, &text), text, "带正文的帧要逐字透传: {delta}");
        }
    }

    /// 生产抓包（NAS `request_raw` 的下发字节，WorkBuddy 的
    /// `deepseek-v4.1-flash`，请求 id `185d8dbf…`）里连续七帧的**原样字节**：
    /// 上游把保活换行夹在正文分片之间。最后一帧 `":\n"` 是「真文本 + 换行」，
    /// 一起丢掉就会把冒号也吞了 —— 这条测试锁住判据的边界。
    const CAPTURED: [&str; 7] = [
        r#"data: {"id":"cmb-22097200b9bf11f1a8cfb69a5c373198","model":"deepseek-v4.1-flash","object":"chat.completion.chunk","created":1790436599,"choices":[{"index":0,"delta":{"content":"A","reasoning_content":"","function_call":null,"refusal":"","tool_calls":[],"extra_fields":null},"logprobs":null,"finish_reason":""}],"usage":null}"#,
        r#"data: {"id":"cmb-22097200b9bf11f1a8cfb69a5c373198","model":"deepseek-v4.1-flash","object":"chat.completion.chunk","created":1790436599,"choices":[{"index":0,"delta":{"content":"\n","reasoning_content":"","function_call":null,"refusal":"","tool_calls":[],"extra_fields":null},"logprobs":null,"finish_reason":""}],"usage":null}"#,
        r#"data: {"id":"cmb-22097200b9bf11f1a8cfb69a5c373198","model":"deepseek-v4.1-flash","object":"chat.completion.chunk","created":1790436599,"choices":[{"index":0,"delta":{"content":" real","reasoning_content":"","function_call":null,"refusal":"","tool_calls":[],"extra_fields":null},"logprobs":null,"finish_reason":""}],"usage":null}"#,
        r#"data: {"id":"cmb-22097200b9bf11f1a8cfb69a5c373198","model":"deepseek-v4.1-flash","object":"chat.completion.chunk","created":1790436599,"choices":[{"index":0,"delta":{"content":"\n","reasoning_content":"","function_call":null,"refusal":"","tool_calls":[],"extra_fields":null},"logprobs":null,"finish_reason":""}],"usage":null}"#,
        r#"data: {"id":"cmb-22097200b9bf11f1a8cfb69a5c373198","model":"deepseek-v4.1-flash","object":"chat.completion.chunk","created":1790436599,"choices":[{"index":0,"delta":{"content":" finding","reasoning_content":"","function_call":null,"refusal":"","tool_calls":[],"extra_fields":null},"logprobs":null,"finish_reason":""}],"usage":null}"#,
        r#"data: {"id":"cmb-22097200b9bf11f1a8cfb69a5c373198","model":"deepseek-v4.1-flash","object":"chat.completion.chunk","created":1790436599,"choices":[{"index":0,"delta":{"content":"\n","reasoning_content":"","function_call":null,"refusal":"","tool_calls":[],"extra_fields":null},"logprobs":null,"finish_reason":""}],"usage":null}"#,
        r#"data: {"id":"cmb-22097200b9bf11f1a8cfb69a5c373198","model":"deepseek-v4.1-flash","object":"chat.completion.chunk","created":1790436599,"choices":[{"index":0,"delta":{"content":":\n","reasoning_content":"","function_call":null,"refusal":"","tool_calls":[],"extra_fields":null},"logprobs":null,"finish_reason":""}],"usage":null}"#,
    ];

    #[test]
    fn a_replayed_production_stream_keeps_only_the_text_bearing_frames() {
        let stream: String = CAPTURED.iter().map(|line| format!("{line}\n\n")).collect();
        let text_bearing: Vec<String> = vec![0, 2, 4, 6]
            .into_iter()
            .map(|index| format!("{}\n\n", CAPTURED[index]))
            .collect();

        // 开启：只留带正文的四帧，且每帧逐字节不变（丢帧是整帧丢，不重写）
        let mut on = coalescer(true);
        let dropped = feed(&mut on, &stream);
        assert_eq!(dropped, text_bearing.concat(), "开启时应当只留下带正文的帧");

        // 关闭：七帧原样出去 —— 与接入本功能前的字节完全一致
        let mut off = coalescer(false);
        assert_eq!(feed(&mut off, &stream), stream, "关掉时要逐字节透传整段流");

        // 客户端拼出来的正文（两种开关对照）
        let content_of = |text: &str| -> String {
            text.split("\n")
                .filter_map(|line| line.strip_prefix("data: "))
                .filter_map(|data| serde_json::from_str::<Value>(data).ok())
                .filter_map(|chunk| {
                    chunk
                        .get("choices")?
                        .get(0)?
                        .get("delta")?
                        .get("content")?
                        .as_str()
                        .map(str::to_string)
                })
                .collect()
        };
        assert_eq!(content_of(&dropped), "A real finding:\n");
        assert_eq!(content_of(&stream), "A\n real\n finding\n:\n");
    }

    #[test]
    fn the_predicate_requires_every_signal_to_be_empty() {
        assert!(is_newline_keepalive(&json!({"choices": [{"delta": {"content": "\n"}}]})));
        assert!(!is_newline_keepalive(&json!({"choices": [{"delta": {"content": "\n "}}]})));
        assert!(!is_newline_keepalive(&json!({"choices": [{"delta": {"content": ""}}]})));
        assert!(!is_newline_keepalive(&json!({"choices": [{"delta": {"content": 7}}]})));
        assert!(!is_newline_keepalive(&json!({"choices": [{"delta": {}}]})));
        assert!(!is_newline_keepalive(&json!({"choices": []})));
        assert!(!is_newline_keepalive(&json!({"choices": [{"delta": {"content": "\n"}, "finish_reason": "length"}]})));
        assert!(!is_newline_keepalive(&json!({"choices": [{"delta": {"content": "\n", "reasoning_content": "x"}}]})));
        assert!(!is_newline_keepalive(&json!({"choices": [{"delta": {"content": "\n", "tool_calls": [{"index": 0}]}}]})));
        assert!(!is_newline_keepalive(&json!({"choices": [{"delta": {"content": "\n", "function_call": {"name": "x"}}}]})));
        assert!(!is_newline_keepalive(&Value::Null));
        assert!(!is_newline_keepalive(&json!("")));
    }
}
