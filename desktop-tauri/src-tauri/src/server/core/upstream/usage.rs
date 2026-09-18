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

use std::sync::{Mutex, MutexGuard};

use serde_json::Value;

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
    /// 最终承载本次请求的账号（用默认登录态转发、或选路结果无账号时为空串）
    pub account_id: String,
    /// 账号展示名，取值顺序与限额日志一致：账号名 → 会话昵称 → 账号 id
    /// （所以账号 id 为空但会话带昵称时，这里仍可能非空）
    pub account_name: String,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    pub cache_read_tokens: i64,
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
}

impl Default for RequestTelemetry {
    fn default() -> Self {
        Self::new()
    }
}

impl RequestTelemetry {
    pub fn new() -> Self {
        Self { inner: Mutex::new(TelemetrySnapshot::default()) }
    }

    /// 记一次「已向这个账号发出上游请求」（选路循环每轮调一次）。
    ///
    /// 账号取**最后一次**：429 轮换后真正承载请求的是最后那个账号，
    /// 报表里「这条请求算在谁头上」应该跟着实际承载者走。
    /// attempts 是累计值 —— 它要回答的是「这条请求换了几个账号才发出」。
    pub fn note_attempt(&self, account_id: Option<&str>, account_name: &str) {
        let mut guard = self.lock();
        guard.attempts += 1;
        guard.account_id = account_id.unwrap_or("").to_string();
        guard.account_name = account_name.to_string();
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
