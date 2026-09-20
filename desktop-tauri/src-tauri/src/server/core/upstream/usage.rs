//! usage 旁路提取（请求统计用）：从上游 chunk 里抄出 token 用量与承载账号。
//!
//! ── 为什么叫「旁路」──────────────────────────────────────────
//! 调用点都在**已经解析好、马上就要原样下发**的帧旁边（`sse.rs` 的逐行解析
//! 分支、`aggregate.rs` 的聚合分支）：只读一眼 JSON、往共享槽位写一份拷贝，
//! 不参与帧的构造、不改动任何要下发的字节。所以无论提取成功、失败还是字段
//! 缺失，客户端收到的内容与接入前逐字一致 —— 这是本文件的硬约束。
//!
//! ── 为什么字段名要兼容多种写法 ───────────────────────────────
//! 上游是 OpenAI 兼容接口，但不同版本/不同接入点对用量字段的命名并不统一：
//!   prompt_tokens（OpenAI 标准） / input_tokens（部分兼容实现）
//!   completion_tokens / output_tokens
//!   缓存命中：prompt_tokens_details.cached_tokens（OpenAI 标准）
//!            / cache_read_tokens / cache_read_input_tokens（Anthropic 风格）
//! 取到哪个算哪个：宁可少记一项，也不要因为字段名对不上而把整条记成 0。
//!
//! ── 为什么回调语义是「每次都上报、由读取侧覆盖」────────────────
//! usage 通常只在流的最后一个 chunk 出现（也有实现会在中途补发增量帧），
//! 于是「最后一次上报即最终值」。把覆盖策略留给读取侧的好处是上报侧完全
//! 无状态：不需要知道「这一帧是不是最后一帧」，也就不可能因为判断错而丢数据。
//!
//! ── 为什么这里不返回 Result ─────────────────────────────────
//! 上报是纯附加动作：任何一步（字段类型不对、锁中毒）都不能影响转发链路。
//! 所以所有方法都吞掉异常、不 panic —— 统计少记一条是可接受的，把用户请求
//! 搞坏不可接受（release 是 panic=abort，一次 panic 会带走整个桌面应用）。

use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::Value;

use crate::server::logging;

/// 一次上报的用量（四种 token 计数，字段名对应存储契约）
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsageTokens {
    pub prompt: i64,
    pub completion: i64,
    pub total: i64,
    pub cache_read: i64,
}

/// 从 usage 对象里提取 token 数（字段名兼容见模块头部）。
///
/// 返回 None 只有一种情况：`usage` 不是对象（null / 数字 / 字符串 / 数组）。
/// 对象内单个字段缺失按 0 算；`total_tokens` 缺失时按 prompt + completion 补。
pub fn extract_usage(usage: &Value) -> Option<UsageTokens> {
    let object = usage.as_object()?;
    let prompt = number_field(object.get("prompt_tokens"))
        .or_else(|| number_field(object.get("input_tokens")))
        .unwrap_or(0);
    let completion = number_field(object.get("completion_tokens"))
        .or_else(|| number_field(object.get("output_tokens")))
        .unwrap_or(0);
    let total = number_field(object.get("total_tokens")).unwrap_or(prompt + completion);
    let cache_read = object
        .get("prompt_tokens_details")
        .and_then(|details| number_field(details.get("cached_tokens")))
        .or_else(|| number_field(object.get("cache_read_tokens")))
        .or_else(|| number_field(object.get("cache_read_input_tokens")))
        .unwrap_or(0);
    Some(UsageTokens { prompt, completion, total, cache_read })
}

/// 取数值字段：`as_i64` 对 `1.0` 这类浮点形态会失败，所以再补一次 f64 转换
/// （上游不同实现给整数/浮点都有先例）；非数值类型返回 None，交给上层兜底。
fn number_field(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    value
        .as_i64()
        .or_else(|| value.as_f64().map(|number| number as i64))
}

/// 记账点要的最终快照（一次性读走，避免读字段时逐次加锁）
#[derive(Clone, Debug, Default)]
pub struct TelemetrySnapshot {
    /// 实际尝试的**账号数**（0 = 还没走到选路就失败了）。
    ///
    /// 口径说明：一次「尝试」= 选路循环的一轮 = 向一个账号发一次上游请求
    /// （见 `forward` 的选路循环）。同一账号内的 11128 退避重试
    /// （`request_with_waf_retry` 的 10s/25s 两次）**不计入** —— 它换的是时间
    /// 不是账号，报表里「换了几个账号才成功」比「总共打了几次上游」更常用。
    ///
    /// 存储契约是「含首次、恒 ≥1」，所以 0 只出现在「一次都没发出去」时，
    /// 由记账点 `.max(1)` 归一。
    pub attempts: i64,
    /// 本条请求的关联 id（转发开始前生成一次，全链路不变）。
    ///
    /// 用途：请求日志条目与调试模式的原始报文（`core::debug_traffic`）用同一个
    /// id 关联 —— 日志页的「详情」列拿它去取报文。空串 = 该条没有（本字段引入
    /// 前落盘的旧行），前端据此不显示详情入口。
    pub id: String,
    /// 最终承载本次请求的账号（用默认登录态转发、或选路结果无账号时为空串）
    pub account_id: String,
    /// 账号展示名，取值顺序与限额日志一致：账号名 → 会话昵称 → 账号 id
    /// （所以账号 id 为空但会话带昵称时，这里仍可能非空）
    pub account_name: String,
    /// 最终承载本次请求的 **provider id**（Agent2API W2b-T3 新增；
    /// `None` = 还没走到选路就失败了，例如请求体非法 / 模型不存在）。
    ///
    /// 为什么存 id 字符串而不是 `ProviderKind`：这个值要跨模块交给记账点
    /// （`api::chat` → `request_stats`），落进请求日志的 `provider` 字段，
    /// 最终出现在报表与前端 —— 契约里它就是 id 字符串（architecture §3.6）。
    /// 存字符串省掉一次「kind → id」的转换与两处枚举依赖。
    pub provider: Option<String>,
    /// 实际发给上游的模型名（映射 + 备援按家改写后的**最终值**；空串 =
    /// 一次都没发出去，例如转发前就失败）。
    ///
    /// 与 `provider` 同一「最后一次为准」口径：429 换账号（可能换家）后，
    /// 请求日志的「上游模型」应该跟着实际承载的那一次走。采集点在发送体
    /// 决定处（`upstream::payload::send_body`，每家首次尝试时覆盖一次）。
    /// 空串口径与 `account_id` 一致：键恒在、空串表示没有。
    pub upstream_model: String,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    pub cache_read_tokens: i64,
    /// 上游首帧到达的**绝对时刻**（毫秒 Unix 时间戳；None = 全程没有帧到达，
    /// 例如转发前就失败 / 请求尚未开始下发）。
    ///
    /// 为什么存绝对时刻而不是相对耗时：采集点（响应流）与记账点（请求收尾）
    /// 分布在两处，「请求开始时刻」只有记账点知道 —— 存绝对值让采集点完全
    /// 不需要知道口径，减法在记账点做一次（`record_entry`），与 durationMs
    /// 用同一个 `started_at`，两列的参考点不可能各说各话。
    ///
    /// 对应 OmniProxy 请求日志的 `ttfb_ms`（time to first byte）：它回答
    /// 「上游多久开始吐内容」，把「等上游出首字」与「生成完整段内容」两段
    /// 耗时分开 —— 只有 durationMs 时，一个 30 秒的请求看不出是上游慢
    /// 还是内容长。
    pub first_response_at: Option<i64>,
    /// 中断 / 异常原因（成功为 None）
    pub error: Option<String>,
}

/// usage 上报槽：转发链路往里写，记账点在收尾时读。
///
/// 为什么用共享槽位而不是把数据一路 return 出来：流式转发的收尾发生在
/// **handler 返回之后**（由响应流自己被 axum 拉取），那时转发函数早已返回，
/// 只能靠一个双方都能拿到的句柄传话。槽位只在一条请求内共享，不存在争用。
pub struct RequestTelemetry {
    inner: Mutex<TelemetrySnapshot>,
    /// 调试模式的原始报文采集器（`core::debug_traffic`；None = 未开启调试模式）。
    ///
    /// 挂在这里而不是层层传参：流式路径的采集发生在 `ForwardStream::poll_next`
    /// （handler 早已返回），非流式发生在聚合函数里，两者手上都只有 telemetry
    /// —— 转发层在**即将发送前**把它装进来，两条路径各自从同一个槽位取。
    ///
    /// 独立一把锁（不与 `inner` 共用）：采集器的读写都在转发热路径上，
    /// 与「记账字段」的锁分开可以避免两处互不相关的写互相等待。
    capture: Mutex<Option<Arc<super::super::debug_traffic::TrafficCapture>>>,
}

impl Default for RequestTelemetry {
    fn default() -> Self {
        Self::new()
    }
}

impl RequestTelemetry {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(TelemetrySnapshot::default()),
            capture: Mutex::new(None),
        }
    }

    /// 建一个已带关联 id 的槽位（三个对话入口都走这条：转发开始前生成一次）。
    ///
    /// 为什么不把 id 生成塞进 `new()`：`RequestTelemetry::new()` 还被
    /// 「转发前就失败」的记账路径用（`record_early_failure`），那些请求没有
    /// 原始报文可采，凭空生成一个 id 只会让日志里多出一批没有详情的行。
    pub fn with_id() -> Self {
        let telemetry = Self::new();
        telemetry.ensure_id(&super::request::new_request_id());
        telemetry
    }

    /// 装入调试模式的采集器（转发层即将发送前调一次）。
    pub fn set_capture(&self, capture: Arc<crate::server::core::debug_traffic::TrafficCapture>) {
        let mut guard = self
            .capture
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = Some(capture);
    }

    /// 取采集器（未开启调试模式时为 None，调用点据此完全跳过采集）
    pub fn capture(
        &self,
    ) -> Option<Arc<crate::server::core::debug_traffic::TrafficCapture>> {
        self.capture
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// 生成并记下本条请求的关联 id（转发开始前调一次）。
    ///
    /// **首次为准**：重复调用不覆盖 —— id 一旦生成，请求日志与调试报文就必须
    /// 认它，中途换一个会让已经落盘的报文变成孤儿。空串参数不写（调用方拿不到
    /// id 时保持空，前端据此不显示详情入口）。
    pub fn ensure_id(&self, id: &str) {
        if id.is_empty() {
            return;
        }
        let mut guard = self.lock();
        if guard.id.is_empty() {
            guard.id = id.to_string();
        }
    }

    /// 本条请求的关联 id（未生成时为空串）
    pub fn id(&self) -> String {
        self.lock().id.clone()
    }

    /// 记一次「已向这个账号发出上游请求」（选路循环每轮调一次）。
    ///
    /// 账号取**最后一次**：429 轮换后真正承载请求的是最后那个账号，
    /// 报表里「这条请求算在谁头上」应该跟着实际承载者走。
    /// attempts 是累计值 —— 它要回答的是「这条请求换了几个账号才发出」。
    ///
    /// `provider` 同样是**最后一次**（多提供商轮询后真正承载请求的那家）。
    pub fn note_attempt(&self, account_id: Option<&str>, account_name: &str, provider: &str) {        let mut guard = self.lock();
        guard.attempts += 1;
        guard.account_id = account_id.unwrap_or("").to_string();
        guard.account_name = account_name.to_string();
        if !provider.is_empty() {
            guard.provider = Some(provider.to_string());
        }
    }

    /// 记录「这一次尝试实际发给上游的模型名」（覆盖式，最后一次为准）。
    ///
    /// 与 `note_attempt` 的 provider 同一口径：429 换家后，最终值是最后一次
    /// 尝试发出的名字。空串不写（没有「清空」的语义 —— 槽位初始就是空串，
    /// 写空只可能来自调用方的兜底路径，那些路径不该覆盖已采到的值）。
    pub fn note_upstream_model(&self, model: &str) {
        if model.is_empty() {
            return;
        }
        let mut guard = self.lock();
        guard.upstream_model = model.to_string();
    }

    /// 上报一次 usage（覆盖式，最后一次为准；字段名兼容见 `extract_usage`）。
    pub fn report_usage(&self, usage: &Value) {
        let Some(tokens) = extract_usage(usage) else {
            return;
        };
        let mut guard = self.lock();
        guard.prompt_tokens = tokens.prompt;
        guard.completion_tokens = tokens.completion;
        guard.total_tokens = tokens.total;
        guard.cache_read_tokens = tokens.cache_read;
    }

    /// 记一次「上游首帧已到达」。
    ///
    /// **首次为准**（与 note_error 相同、与 report_usage 相反）：首帧只有一次，
    /// 重跑计数只会把「第一个字节什么时候到的」改写成「后来某次调用的时刻」。
    /// 采集点在响应流上（Streaming 的 RecordingStream / 聚合的第一个 chunk），
    /// 同一条流上会被反复调用，幂等性由这里的 None 判断保证。
    pub fn note_first_frame(&self) {
        let mut guard = self.lock();
        if guard.first_response_at.is_none() {
            guard.first_response_at = Some(logging::now_ms());
        }
    }

    /// 记一条中断 / 异常原因（成功请求不会被调用）。
    ///
    /// **首次为准**（与 usage 的「最后一次为准」相反）：先到的那条是根因，
    /// 之后到达的多半是它的连带现象（例如断流后客户端断开又触发一次收尾），
    /// 覆盖掉反而把根因埋了。
    pub fn note_error(&self, message: &str) {
        if message.is_empty() {
            return;
        }
        let mut guard = self.lock();
        if guard.error.is_none() {
            guard.error = Some(message.to_string());
        }
    }

    /// 取当前快照（记账点收尾时调一次）
    pub fn snapshot(&self) -> TelemetrySnapshot {
        self.lock().clone()
    }

    /// 取锁；中毒时继续用内部值 —— 统计数据的完整性远不如「转发不因统计而崩」
    /// 重要（与 RequestStats / LogStore 同一取向）
    fn lock(&self) -> MutexGuard<'_, TelemetrySnapshot> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}
