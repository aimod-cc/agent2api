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
    /// **本次请求命中的脱敏规则**（规则标签 + 次数；空表 = 一个都没命中）。
    ///
    /// ── 数据从哪来 ──────────────────────────────────────────────
    /// `core::sanitize` 的 `sanitize_body` 在剥离指纹时顺手算出命中明细
    /// （按次数降序的每条规则命中数）。字段名与形状**未随规则集更换而改动**
    /// —— 列名 `sensitive_hits`、元素 `{word, count}`、前端的「敏」标签与
    /// 悬停面板都照常工作，只是 `word` 里装的从「命中的敏感词」变成
    /// 「命中的规则标签」（如 `11128` / `cc_*=` / 模板句原文）。
    ///
    /// 采集点在 `payload::send_body`（某一家即将发送、拿到脱敏结果的那一刻），
    /// 由 [`Self::note_sensitive_hits`] 合并进来。
    ///
    /// ── 为什么是并集累加（与 attempts 的覆盖式相对）───────────────
    /// 见 `payload::send_body` 的说明：跨家降级时每个在范围内的家各处理一次，
    /// 合并计数才是「这次请求命中了什么」的正确读数。
    pub sensitive_hits: Vec<SensitiveHit>,
    /// **每一次上游尝试的明细**（按发生顺序，与账号轮换链一一对应）。
    ///
    /// ── 为什么需要它（Agent2API 请求日志改造）───────────────────
    /// 原先只有 `attempts`（次数）与 `provider`（最终承载者）两个读数，
    /// 于是请求日志里那一列只能显示一个「重试」标记 —— 用户看到「重试过」，
    /// 但看不到**换了谁、哪一次失败在哪、最后是谁扛下来的**。而这些信息在
    /// 转发链路里每一轮都是现成的（provider id、HTTP 状态码、错误摘要），
    /// 只是此前没有落点。
    ///
    /// ── 口径（与 `attempts` / `provider` 刻意不同）──────────────
    /// `attempts` 与 `provider` 都是「最后一次为准」的**覆盖式**读数；
    /// 本字段相反，是**追加式**的历史。两者不冲突：覆盖式回答「最终算谁头上」
    /// （报表聚合要的就是这个），追加式回答「这一路是怎么走过来的」。
    ///
    /// ── 为什么记「已发送」而不是「选路选到了」───────────────────
    /// 采集点在 `note_attempt` 旁边（该函数在**凭证已就绪、即将发送之前**
    /// 被调用），所以与 `attempts` 同源同频。选路过程中被跳过的账号
    /// （禁用 / 限额冷却 / 不支持该模型）**不进这条链** —— 它们没有产生任何
    /// 上游往返，混进来会让「切换路径」显示一串从没被请求过的名字。
    ///
    /// 条数上限见 [`MAX_ATTEMPT_DETAILS`]：这是一条请求内的内存小数组，
    /// 落库时序列化成 JSON 文本，必须有上界。
    pub attempts_detail: Vec<AttemptDetail>,
}

/// 一个被命中的敏感词及其次数（`TelemetrySnapshot::sensitive_hits` 的元素）。
///
/// 字段名与前端 `sensitiveHits` 契约里的键一致（`word` / `count`），
/// 与 OmniProxy 的 `SensitiveTermHit` 同形 —— 那边的 `sensitive_masked_terms`
/// 也是一份 `[{word, count}]`。同形不是巧合：两边的信息结构本来就一样
/// （命中词 × 次数），照抄形态省掉一层没意义的转换。
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SensitiveHit {
    /// 命中的词（词表里的原样字符串，未做大小写归一 —— 用户填的是什么就显示什么）
    pub word: String,
    /// 本次请求里命中的次数
    pub count: i64,
}

/// 单次上游尝试的明细（`TelemetrySnapshot::attempts_detail` 的元素）。
///
/// 字段刻意取窄：前端弹层要显示的就这几样（谁 → 成没成 → 为什么）。
/// 不记时间戳与耗时：那是「每次尝试各花了多久」的另一个问题，转发链路上
/// 没有现成的分段计时，为它加钩子要动 `send_with_retry` 的每一层 ——
/// 收益不抵改动面（总耗时与首响已经在明细里给出）。
///
/// ── `account` / `retries` / `notice` 为什么在这里（逐请求日志收敛）──
/// 改造前这三样各自在**运行日志**里打一行：「账号 X 转发失败按队列顺延」
/// 带账号名、「5 秒后重试（第 n/N 次）」带退避次数、「账号代理不可用，本次
/// 直连」带出口回退。它们全是**逐请求**的事实，却只能去「日志」页看，
/// 而请求日志页的同一行请求那里恰好缺这几样（弹层里只有 provider）。
/// 收进明细后，请求日志的一行 + 一次悬停就能回答「换了谁、每轮重试了几次、
/// 为什么重试、出口有没有降级」，运行日志不再需要为每一次转发写行。
///
/// 三者都是**有采集才有值**：旧行没有这些键，`default` 读成空/None，
/// 展示层对空值与旧数据一视同仁（不显示那一行），不给旧数据猜值。
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AttemptDetail {
    /// 这一轮实际发送的 provider id
    pub provider: String,
    /// 这一轮承载的**账号展示名**（账号名 → 会话昵称 → 账号 id 的兜底链，
    /// 与 `note_attempt` 给 `account_name` 的口径完全一致）。
    ///
    /// 与 `provider` 并列而不是替代它：一家可以有多个账号，弹层里
    /// 「WorkBuddy / aibjchat001@gmail.com」比只有家名更能定位到那一轮。
    /// 空串 = 该轮没有账号记录（用默认登录态转发），展示层给占位文案。
    #[serde(default)]
    pub account: String,
    /// 这一轮的 HTTP 状态码。
    ///
    /// `None` = 还在飞（请求尚未收尾，例如正在流式下发）或传输层失败
    /// （DNS / 连接 / 代理，压根没拿到响应头）。两者的区别由 `error` 表达：
    /// 有摘要 = 已定性为失败，没有摘要且 None = 这一轮还没定论。
    #[serde(default)]
    pub status: Option<i64>,
    /// 这一轮的失败摘要（成功或还在飞时为 None）。
    ///
    /// 成功的那一轮**也留在链里**（带 status、不带 error）—— 「切换路径」要
    /// 显示完整的一串（A → B → C），最后那个成功的 C 正是读者要找的答案。
    #[serde(default)]
    pub error: Option<String>,
    /// 这一轮**内部**的退避重试（`[{reason, status, delayMs}]`，按发生顺序）。
    ///
    /// ── 为什么挂在明细里而不是追加新的明细 ────────────────────
    /// 同账号内的退避重发**不算一次新尝试**（口径见 `TelemetrySnapshot::attempts`
    /// 的说明：`attempts` 回答「换了几个账号」，不是「打了几次上游」）。
    /// 若为它追加明细，弹层里的「切换路径」就会多出一串同名同账号的项，
    /// 把真正的换号链埋掉。所以它作为**那一轮的下级事件**存在这里。
    ///
    /// 空表 = 这一轮没有重试（绝大多数请求）。
    #[serde(default)]
    pub retries: Vec<RetryEvent>,
    /// 这一轮的**提示**（非失败、但值得记一笔的过程事实）。
    ///
    /// 目前只有一种来源：账号代理不可用、本次回退直连（改造前是
    /// `[Upstream] ⚠️ 账号代理不可用…` 那行运行日志）。它是**每一条**用该
    /// 账号的请求都会发生的事，正因如此更不该按请求往运行日志里灌。
    #[serde(default)]
    pub notice: Option<String>,
}

/// 一次**尝试内部**的退避重试（`AttemptDetail::retries` 的元素）。
///
/// 三个字段回答三个问题：为什么重试（`reason`）、上游当时怎么回的
/// （`status`）、等了多久（`delay_ms`）。不记重试序号：数组顺序就是发生顺序，
/// 前端按下标 +1 即可（与 `attempts_detail` 的序号同一手法）。
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RetryEvent {
    /// 重试原因（适配器给的 provider 专属措辞，或编排层的通用兜底措辞）
    pub reason: String,
    /// 触发重试的 HTTP 状态码；`None` = 传输层失败（压根没有响应头）
    #[serde(default)]
    pub status: Option<i64>,
    /// 本次退避时长（毫秒）
    ///
    /// 键名 camelCase：这个结构整体序列化进 `attemptDetails` 那个 JSON 列，
    /// 前端直接读它 —— 与本项目其余跨端字段（`attemptDetails` / `sensitiveHits`
    /// / `firstResponseMs`）同一写法。
    #[serde(rename = "delayMs", default)]
    pub delay_ms: u64,
}

/// `attempts_detail` 的长度上限。
///
/// 选路循环本身有 `MAX_ROUTE_ATTEMPTS`（32）兜底，但那只在**账号轮换**这条
/// 路径上生效；同账号内的退避重试（`send_with_retry` 的循环）不计入 attempts，
/// 也不会往这里追加。所以 32 已经是理论上界，这里再取一个更小的值作为
/// **落库体积**的上限：每条明细最多几行文本，24 条 × 几条 ≈ 几 KB，
/// 对一条请求日志来说是合理的。
///
/// 为什么是「保头」而不是「保尾」：链的开头是「第一次打给谁、为什么失败」，
/// 那正是排障最需要的（尾部几轮往往是同一个错误的重复）。
/// 截断不在数据里补标记项 —— 前端手上本来就有 `attempts`（总数），
/// 判 `attempts_detail.length < attempts` 即可显示「只保留前 N 条」，
/// 比在 JSON 里塞一条结构不同的伪明细更干净（那种伪项会让所有读侧都要先判类型）。
pub const MAX_ATTEMPT_DETAILS: usize = 24;

/// 单条尝试明细里错误摘要的字符上限。
///
/// 取值与请求日志那列的 `ERROR_SUMMARY_CHARS`（`api::pipeline`）**有意相同**：
/// 同一个失败在两个地方（列表的「错误」列、重试弹层里的某一次尝试）显示成长度
/// 不同的两段文案，会让读者以为它们说的是两件事。两处是独立常量而不是共享一个
/// —— 本模块在 `core` 下，不能反向依赖 `api`（见 `core/mod.rs` 的约定），
/// 修改其中一处时**必须**同步另一处，这条注释就是那个提醒。
pub const MAX_ATTEMPT_ERROR_CHARS: usize = 200;

/// 明细里账号展示名的字符上限。
///
/// 账号名是用户自己填的自由文本（导入的账号名可能是邮箱、昵称、一长串备注），
/// 而弹层里它与 provider 名同处一行。60 个字符足够放一个邮箱加中文昵称，
/// 再长就让它省略 —— 明细是「一眼看清谁承载了这一轮」的读数，不是账号名片。
pub const MAX_ATTEMPT_ACCOUNT_CHARS: usize = 60;

/// 单条重试原因的字符上限。
///
/// 比错误摘要短：重试原因是一句「为什么再试一次」（如「上游敏感词拦截（11128）」），
/// 不是完整的上游报文 —— 后者在 `error` 与调试模式的原始报文里都有。
pub const MAX_ATTEMPT_RETRY_REASON_CHARS: usize = 120;

/// 单条明细里提示文案的字符上限。
pub const MAX_ATTEMPT_NOTICE_CHARS: usize = 200;

/// 按字符截断（超出部分用 `…` 收尾）。
///
/// 不引 `request_stats::truncate_chars`：同名的实现住在 `api::pipeline` 与
/// `account_store::store_util` 里，本模块在 `core::upstream`，跨模块取一个
/// 五行的私有工具只会让依赖方向变乱（core 不认识 api）。按**字符**而不是字节：
/// 中文摘要按字节截会切出半个字。
fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut out: String = text.chars().take(limit).collect();
    out.push('…');
    out
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
    pub fn note_attempt(&self, account_id: Option<&str>, account_name: &str, provider: &str) {
        let mut guard = self.lock();
        guard.attempts += 1;
        guard.account_id = account_id.unwrap_or("").to_string();
        guard.account_name = account_name.to_string();
        if !provider.is_empty() {
            guard.provider = Some(provider.to_string());
        }
    }

    /// 追加一条**尝试明细**（这一轮发给了谁；结果稍后由
    /// [`Self::finish_last_attempt`] 补）。
    ///
    /// ── 为什么分两步写（先起头、后补结果）────────────────────────
    /// 一次尝试的「谁」在发送**之前**就确定了（那时才决定发送体、才有 provider），
    /// 而「成没成」要等上游响应头到手才知道。如果只在结果处记一条，那么
    /// **传输层失败**（`send_chat_request` 报错、压根没有响应）这条路径就永远
    /// 记不上 —— 而那恰恰是最需要看到的一类失败（DNS / 代理 / 连接被拒）。
    /// 分两步之后，任何一条走完的路径都至少留下「谁 + 结果」，不存在漏记。
    ///
    /// 与 [`Self::note_attempt`] 配对调用（同一次发送前），所以两者条数一致；
    /// 调用顺序必须是 `note_attempt` → `note_attempt_started`。
    ///
    /// `account` 与 `note_attempt` 的 `account_name` 传**同一个值**（调用方算
    /// 一次、两处用），于是弹层里「这一轮是谁在承载」与报表的账号列不可能对不上。
    ///
    /// 超过 [`MAX_ATTEMPT_DETAILS`] 时**丢弃**新条目（保头，理由见那个常量的说明）。
    /// 调用方不要靠本方法表达「截断了」——那是 `snapshot()` 的职责。
    pub fn note_attempt_started(&self, provider: &str, account: &str) {
        let mut guard = self.lock();
        if guard.attempts_detail.len() >= MAX_ATTEMPT_DETAILS {
            return;
        }
        guard.attempts_detail.push(AttemptDetail {
            provider: provider.to_string(),
            account: truncate_chars(account, MAX_ATTEMPT_ACCOUNT_CHARS),
            status: None,
            error: None,
            retries: Vec::new(),
            notice: None,
        });
    }

    /// 给**最后一条**尝试明细追加一次内部退避重试（`send_with_retry` 每退避
    /// 一次调一次）。
    ///
    /// 空 `reason` 直接丢弃：明细里出现一条「重试了，但不知道为什么」，
    /// 对排障没有任何价值，还会让弹层多一行噪音。
    pub fn note_attempt_retry(&self, reason: &str, status: Option<i64>, delay_ms: u64) {
        let reason = reason.trim();
        if reason.is_empty() {
            return;
        }
        let mut guard = self.lock();
        let Some(last) = guard.attempts_detail.last_mut() else {
            return;
        };
        last.retries.push(RetryEvent {
            reason: truncate_chars(reason, MAX_ATTEMPT_RETRY_REASON_CHARS),
            status,
            delay_ms,
        });
    }

    /// 给**最后一条**尝试明细记一条提示（目前只有代理回退直连）。
    ///
    /// **首次为准**（与 `note_error` 同一取向）：同一轮里代理提示只会有一条，
    /// 后来的（例如重试时又算了一次）不该覆盖先到的那条根因。
    /// 空串不写，理由同 [`Self::note_attempt_retry`]。
    pub fn note_attempt_notice(&self, notice: &str) {
        let notice = notice.trim();
        if notice.is_empty() {
            return;
        }
        let mut guard = self.lock();
        let Some(last) = guard.attempts_detail.last_mut() else {
            return;
        };
        if last.notice.is_none() {
            last.notice = Some(truncate_chars(notice, MAX_ATTEMPT_NOTICE_CHARS));
        }
    }

    /// 给**最后一条**尝试明细补上结果（成功也要调，此时 `status` 有值、
    /// `error` 传空）。
    ///
    /// 为什么只认最后一条：一次尝试的发送链是严格串行的（`send_with_retry`
    /// 内部循环 + 401 刷新重试都在同一轮里），响应回来时「最后起头的那条」
    /// 必然是它 —— 中间不会有别的尝试插进来。这条性质由 `provider_loop`
    /// 的 `'accounts` 循环保证（每轮只调一次起头，轮换才进入下一轮）。
    ///
    /// `status` 为 None 时（传输层失败）**不覆盖**已有的 None，`error` 照写：
    /// 于是那一条明细显示成「A → 失败（无状态码）：连接被拒绝」，
    /// 而不是显示一个凭空捏造的状态码。
    ///
    /// 错误摘要与请求日志的 `error` 用同一个截断上限（[`MAX_ATTEMPT_ERROR_CHARS`]），
    /// 截断在**这里**做而不是靠调用方：入口只有这一个，把上限钉在写入侧才能保证
    /// 「无论谁调用都不会写进一条超长的明细」。
    pub fn finish_last_attempt(&self, status: Option<i64>, error: Option<&str>) {
        let mut guard = self.lock();
        let Some(last) = guard.attempts_detail.last_mut() else {
            return;
        };
        if status.is_some() {
            last.status = status;
        }
        // 空串摘要按「没有错误」处理（与 RequestEntry.error 的归一同一口径）：
        // 上游返回空 body 时 read_upstream_error 会给出空 message，
        // 那不该渲染成「失败：」后面跟着一片空白
        if let Some(text) = error.filter(|text| !text.is_empty()) {
            last.error = Some(truncate_chars(text, MAX_ATTEMPT_ERROR_CHARS));
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

    /// 记一批敏感词命中（**并集累加**，见 `TelemetrySnapshot::sensitive_hits`）。
    ///
    /// 入参是脱敏模块的 `term_counts` 原样形态（`&[(String, usize)]`）——
    /// 不在这里重新排序或过滤：那份表已经是「按次数降序」的（`engine::Counter`
    /// 的口径），重排一遍只会引入第二套顺序定义。前端要展示的顺序就是它。
    ///
    /// 同一个词再次出现（跨家降级时另一家的词表也命中）时**合并计数**而不是
    /// 追加第二条：一份 `[{word, count}]` 里同一个词出现两次，没有任何读侧
    /// 会把它当成两件事，只会让「命中 3 次」这种读数变成「两条各 2 次」。
    ///
    /// 条数上界：规则集是硬编码的（7 条特征串 + 3 条正则 + 5 条改写，见
    /// `core::sanitize`），远小于改造前那份可维护词表，因此命中标签的条数
    /// 天然有界。这里不设额外的截断闸 —— 命中表落库是 JSON 文本，
    /// 前端展示时还会按次数降序截断（见 requests-panel 的弹层）。
    pub fn note_sensitive_hits(&self, term_counts: &[(String, usize)]) {
        if term_counts.is_empty() {
            return;
        }
        let mut guard = self.lock();
        for (term, count) in term_counts {
            let count = *count as i64;
            match guard
                .sensitive_hits
                .iter_mut()
                .find(|hit| hit.word == *term)
            {
                Some(hit) => hit.count += count,
                None => guard.sensitive_hits.push(SensitiveHit {
                    word: term.clone(),
                    count,
                }),
            }
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
