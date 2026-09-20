//! 请求统计的数据类型与 JSON 契约（唯一事实来源）。
//!
//! ── 字段名为什么逐个写 `#[serde(rename)]` ─────────────────────
//! 前端的统计报表与「模型请求日志」直接读这些键名，所以字段名**就是契约**。
//! 这里不用 `rename_all = "camelCase"` 而是逐字段显式写出：改动字段名时
//! diff 里必然出现 `rename` 那一行，不会因为「顺手调整 rename 策略」而
//! 静默改掉线上格式。
//!
//! ── 为什么读入侧全字段带 `default` ───────────────────────────
//! 文件是长期存在的（明细 30 天、聚合一年），中间可能跨好几个版本。
//! 缺少新字段的旧行必须还能读进来（缺的按 0 / 空算），否则升级一次
//! 就会把用户已有的曲线整段丢掉。

use serde::{Deserialize, Serialize};

/// 明细在内存/文件里保留的最大条数（环形保留，超出丢最旧的）。
///
/// 与按时间的保留期是双保险：保留期管「多久」，上限管「多少」——
/// 短时间内的突发流量不该按天数比例吃光内存。
pub const MAX_ENTRIES: usize = 20_000;

/// 满额后攒够这么多条追加才整文件重写一次（对应 logs_store 的 COMPACT_STEP）
pub const COMPACT_STEP: usize = 100;

/// 聚合行在内存里保留的最大天数（兜底：手改文件塞进十万行时不至于撑爆内存）
pub const MAX_DAILY_DAYS: usize = 4000;

/// 查询分页的默认条数与上限。
/// 两个常量都被 `RequestStats::query_requests`（路由 `/api/stats/requests`）
/// 与 `api::stats_api` 的 `limit` 解析使用。
pub const DEFAULT_LIMIT: usize = 50;
pub const MAX_LIMIT: usize = 500;

/// 保留期设置（对应以后设置页里的两个天数，带默认值）。
///
/// 用独立结构而不是把天数直接摊进构造函数：将来设置里加字段时
/// `new(directory, get_retention)` 的签名不用变，调用方也不用跟着改。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Retention {
    /// 明细保留天数
    pub request_days: i64,
    /// 按天聚合保留天数
    pub daily_days: i64,
}

impl Default for Retention {
    fn default() -> Self {
        Self { request_days: 30, daily_days: 365 }
    }
}

impl Retention {
    /// 归一化：下限 1 天（保留 0 天等于什么都不存，不是有效配置），
    /// 上限 10 年（防手改 config.json 写个天文数字让裁剪逻辑空转）
    pub(super) fn normalized(self) -> Self {
        Self {
            request_days: self.request_days.clamp(1, 3650),
            daily_days: self.daily_days.clamp(1, 3650),
        }
    }
}

/// 一条请求日志。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RequestEntry {
    /// 请求发起时刻（毫秒 Unix 时间戳）
    pub ts: i64,
    /// 本条请求的关联 id（调试模式的原始报文按它关联，见 `core::debug_traffic`）。
    ///
    /// 旧行没有这个键（本字段引入前落盘的），`default` 读成空串 ——
    /// 前端据此不显示「详情」入口。
    #[serde(default)]
    pub id: String,
    pub model: String,
    #[serde(rename = "accountId", default)]
    pub account_id: String,
    #[serde(rename = "accountName", default)]
    pub account_name: String,
    /// HTTP 状态码；还没发出请求就失败时用 0
    #[serde(default)]
    pub status: i64,
    #[serde(rename = "durationMs", default)]
    pub duration_ms: i64,
    /// 上游首帧到达相对请求开始的耗时（毫秒）。
    ///
    /// 与 OmniProxy 请求日志的 `ttfb_ms` 同义：把「等上游出首字」与「生成
    /// 完整段内容」两段耗时分开 —— 只有 durationMs 时，一个 30 秒的请求
    /// 看不出是上游慢还是内容长。
    ///
    /// ── 为什么是 `Option`（null）而不是 0 ─────────────────────────
    /// 「没测到」与「测到了 0ms」是两回事。None 只出现在「全程没有任何帧
    /// 到达」的请求上（转发前就失败）；中途断流的失败请求**照记** ——
    /// 它确实收到过首帧，首响与「这条是不是失败」无关（首响是计时，
    /// 不是消耗，与「失败清零 token」的口径不同）。
    /// 旧版本写出的行没有这个键，`default` 读成 None，前端显示「-」。
    #[serde(rename = "firstResponseMs", default)]
    pub first_response_ms: Option<i64>,
    /// 尝试次数（含首次），恒 ≥1
    #[serde(default = "one")]
    pub attempts: i64,
    /// 错误摘要，成功为 null
    #[serde(default)]
    pub error: Option<String>,
    #[serde(rename = "promptTokens", default)]
    pub prompt_tokens: i64,
    #[serde(rename = "completionTokens", default)]
    pub completion_tokens: i64,
    #[serde(rename = "totalTokens", default)]
    pub total_tokens: i64,
    #[serde(rename = "cacheReadTokens", default)]
    pub cache_read_tokens: i64,
    /// 实际承载本次请求的 provider id（架构文档 §3.6）。
    ///
    /// ── 为什么是 `String` + 空串而不是 `Option<String>` ─────────
    /// 与 `accountId` / `accountName` 同一套「空串就是没有」的存储契约：
    /// 「旧版本写出的行没有这个键」「转发链一次都没发出去（选路前就失败）」
    /// 这两种情况在语义上都是「不知道是哪家承载的」，让它们收敛成同一个值，
    /// 下游（按 provider 聚合、API 透出、前端降级）就不必分三路判断。
    /// 空串在聚合层归入「未知」组，展示层显示「—」，**不给旧数据猜值**
    /// （旧数据全部来自单上游时代也不许补 workbuddy：那是猜，不是事实）。
    ///
    /// ── 为什么没有 `rename` 而只加 `default` ────────────────────
    /// 落盘的键名就是 `provider`，与字段名相同（本文件的 `rename` 都出现在
    /// 两者不一致的字段上）。`default` 保证缺键的旧行照常读入；空值**照写**
    /// （与 `accountId` / `accountName` 同一形态：键恒在、空串表示没有），
    /// 于是「有没有这个键」不再是前端要判的第三种情况。
    #[serde(default)]
    pub provider: String,
    /// **下游请求的**模型名（客户端请求体里的原值，映射 / 默认注入生效前；
    /// 空串 = 客户端没点名，或该行来自还没有此字段的旧版本）。
    ///
    /// 请求日志用它与 `upstreamModel` 分两行展示「⬆️ 转发的什么 / ⬇️ 请求的
    /// 什么」。**不要**把它与 `model` 混用：`model` 是解析后的名字（默认回落
    /// 与映射都已生效），报表按模型聚合的历史口径跟着 `model` 走。
    #[serde(rename = "clientModel", default)]
    pub client_model: String,
    /// 实际发给上游的模型名（映射 + 备援按家改写后的最终值；空串 =
    /// 一次都没发出去就失败了，或该行来自还没有此字段的旧版本）。
    ///
    /// 与 `model` 的差别只在「改写发生过」时出现：下游请求 `gpt-4o` 映射到
    /// `deepseek-v4-pro`、或请求名经备援落到别家时，`model` 记请求侧解析名，
    /// 这里记上游真正收到、也真正认识的名字。
    #[serde(rename = "upstreamModel", default)]
    pub upstream_model: String,
}

/// `attempts` 的 serde 默认值（载入缺该字段的旧行时按 1 次算）
fn one() -> i64 {
    1
}

impl RequestEntry {
    /// 是否成功 —— 2xx **且**没有错误摘要。
    ///
    /// ── 为什么不能只看状态码 ───────────────────────────────────
    /// 流式请求的 HTTP 200 在「响应头已就绪」时就发出去了，之后上游断流、
    /// 上游错误帧、翻译失败都发生在响应体里（协议层把原因写进
    /// `RequestTelemetry::error`）。只按 2xx 判定会把这一类请求记成成功，
    /// 报表的成功率与按 provider 的成功数随之虚高 —— 这正是 CatPaw 有状态
    /// 流式分支暴露出来的问题。有错误摘要 = 这次请求没有完整成功。
    ///
    /// 非流式失败（4xx/5xx）状态码本身就不是 2xx，两条判定都命中，不冲突。
    /// 旧数据里没有 error 字段的行按 None 载入，2xx 仍算成功（口径不变）。
    pub(super) fn is_success(&self) -> bool {
        (200..300).contains(&self.status) && self.error.is_none()
    }

    /// 按**当前**口径归一后的副本：失败请求的 token 一律清零。
    ///
    /// ── 为什么需要它 ────────────────────────────────────────────
    /// 正常路径用不到 —— 明细落盘前已经过 `NewRequestEntry::normalize`。
    /// 但**载入进来的历史明细可能来自更早的版本**：1.x 的 `normalize` 只做
    /// 数值夹取，没有「失败清零」这一段，那时失败的请求也照记 token
    /// （旧文件里确有这类行：`status: 200` + 流未完整下发的摘要 + 六位数 token）。
    /// 回填聚合要拿这些旧行重算，必须先把它们过一遍今天的口径，
    /// 否则报表会把「失败也算用量」这个旧规则带进 token 曲线 ——
    /// 而当前契约是「失败的请求不产生用量」。
    ///
    /// 幂等：已归一的条目再走一遍结果不变（失败的那几个字段恒为 0）。
    pub(super) fn normalized_for_aggregate(&self) -> Self {
        if self.is_success() {
            return self.clone();
        }
        let mut copy = self.clone();
        copy.prompt_tokens = 0;
        copy.completion_tokens = 0;
        copy.total_tokens = 0;
        copy.cache_read_tokens = 0;
        copy
    }
}

/// `record` 的入参。
///
/// 用结构体而非一长串参数：记账点在转发收尾处，字段会随切片增加
/// （错误分类、上游重试原因…），结构体加字段不破坏已有调用方。
/// `ts` 为 None 时取当前时间；数值字段都会夹到 ≥0，`attempts` 夹到 ≥1。
#[derive(Clone, Debug)]
pub struct NewRequestEntry {
    pub ts: Option<i64>,
    /// 本条请求的关联 id（调试模式原始报文的关联键；空串 = 不采）
    pub id: String,
    pub model: String,
    pub account_id: String,
    pub account_name: String,
    pub status: i64,
    pub duration_ms: i64,
    /// 上游首帧到达相对请求开始的耗时（采集点记绝对时刻，记账点做减法；
    /// `None` = 全程没有帧到达）。见 `RequestEntry::first_response_ms`。
    pub first_response_ms: Option<i64>,
    pub attempts: i64,
    pub error: Option<String>,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    pub cache_read_tokens: i64,
    /// 实际承载本次请求的 provider id（Agent2API 改造 W2b-T3 新增填入，
    /// W4 接上落盘：见 `normalize` 末尾的透传）。
    ///
    /// `None` = 一次都没发出去就失败了（请求体非法 / 模型不存在 / 无可用账号），
    /// 落盘时归一成空串，聚合层归入「未知」组。
    pub provider: Option<String>,
    /// 下游请求的模型名（客户端原值；空串 = 未点名 / 未记录）
    pub client_model: String,
    /// 实际发给上游的模型名（空串 = 一次都没发出去 / 未记录）
    pub upstream_model: String,
}

impl NewRequestEntry {
    /// 用最少的必填项起头，其余字段直接改结构体字段补 ——
    /// 记账点早期可能只有模型/状态码，token 数要等上游响应解包后才有。
    pub fn new(model: impl Into<String>, status: i64) -> Self {
        Self {
            ts: None,
            model: model.into(),
            account_id: String::new(),
            account_name: String::new(),
            status,
            duration_ms: 0,
            first_response_ms: None,
            attempts: 1,
            error: None,
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
            cache_read_tokens: 0,
            provider: None,
            client_model: String::new(),
            upstream_model: String::new(),
            id: String::new(),
        }
    }

    /// 归一化成可落盘的条目。
    ///
    /// 时间补全与数值夹取都集中在这里，于是 `record` 只处理「已合法」的数据。
    ///
    /// **失败请求的 token 一律清零**（契约要求）：上游返回错误时 usage 通常是
    /// 缺失的；万一某条错误响应带了半截 usage，清掉比记成「失败的请求也消耗了
    /// token」更符合报表语义 —— 那些 token 不会被计费，留在趋势图里会让
    /// 「失败暴增」看起来像「用量暴增」。
    pub(super) fn normalize(self) -> RequestEntry {
        let ts = self.ts.unwrap_or_else(super::clock::now_ms);
        // 「失败」= 非 2xx **或**带了错误摘要（与 `RequestEntry::is_success` 同一
        // 口径）：流式请求的 HTTP 200 在响应头阶段就发出去了，之后的上游断流/
        // 错误帧只能靠 `error` 表达。空串摘要按没有错误算（下面 filter 同口径）。
        let failed = !(200..300).contains(&self.status)
            || self.error.as_deref().is_some_and(|text| !text.is_empty());
        let token = |value: i64| if failed { 0 } else { value.max(0) };
        // provider 从入参透传到落盘条目（W2b 留的丢弃点在此接上）。
        // `None`（转发链没走到选路就失败）与空串（上游返回了空 id）都归一成
        // **空串**：两者在报表语义上都是「未知承载者」，多一种表示只会让
        // 聚合与前端各写一遍「None 也算空」。trim 一下，避免手改文件或异常
        // 写入带进来的空白造出一个看不见的独立分组。
        let provider = self
            .provider
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty())
            .unwrap_or_default();
        RequestEntry {
            ts,
            // 关联 id 透传（trim 的理由同 provider）：空串 = 没生成 / 旧行
            id: self.id.trim().to_string(),
            model: self.model,
            account_id: self.account_id,
            account_name: self.account_name,
            status: self.status,
            duration_ms: self.duration_ms.max(0),
            // 首响透传：负值（时钟回拨造成的理论值）夹成 0 在记账点已做，
            // 这里只管「有没有」，None（没测到）原样保留
            first_response_ms: self.first_response_ms,
            attempts: self.attempts.max(1),
            error: self.error.filter(|text| !text.is_empty()),
            prompt_tokens: token(self.prompt_tokens),
            completion_tokens: token(self.completion_tokens),
            total_tokens: token(self.total_tokens),
            cache_read_tokens: token(self.cache_read_tokens),
            provider,
            // 双名透传（trim 的理由与 provider 相同：空白不该造出一个
            // 「看起来不同的名字」）；空串语义 = 没有点名 / 没有发出去
            client_model: self.client_model.trim().to_string(),
            upstream_model: self.upstream_model.trim().to_string(),
        }
    }
}

/// 按天聚合行（契约字段名与任务约定逐字一致）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DailyEntry {
    /// 本地时区自然日 `YYYY-MM-DD`
    pub date: String,
    #[serde(default)]
    pub requests: i64,
    #[serde(default)]
    pub successful: i64,
    #[serde(default)]
    pub tokens: i64,
    /// 命中缓存的输入 token 数
    #[serde(rename = "cacheHitTokens", default)]
    pub cache_hit_tokens: i64,
    /// 计入缓存口径的输入 token 数（命中率的分母）
    #[serde(rename = "cacheInputTokens", default)]
    pub cache_input_tokens: i64,
    /// 当天的按模型累计（**契约之外的补充字段**）。
    ///
    /// 为什么必须存在：`topModel` 要按区间跨天累计，而明细只留 30 天，
    /// `"all"` / `"month"` 这类跨年区间只能靠聚合行算模型占比；
    /// 没有它，全年 tokens 冠军会在明细到期后突然变形。
    /// 前端只读自己认识的键，多一个 `modelTokens` 不影响既有契约。
    /// 单天无模型明细时整键省略（`skip_serializing_if`），
    /// 让文件在只有零散请求时也保持紧凑。
    #[serde(rename = "modelTokens", default, skip_serializing_if = "Vec::is_empty")]
    pub model_tokens: Vec<ModelAccum>,
    /// 当天的按 provider 累计（**契约之外的补充字段**，W4 新增）。
    ///
    /// 为什么不复用 `modelTokens` 的容器：provider 维度要多两个列
    /// （成功数 / 失败数），塞进 `ModelAccum` 会让模型那侧多出两个永远没人读的
    /// 字段，也让「模型累计」这个概念的读者要自己分辨哪几个字段有意义。
    ///
    /// 与 `modelTokens` 同理，这份累计必须存在：`topModel` 之外的任何区间级
    /// 分组统计都要跨过明细的 30 天保留期（`all` / `month` 可能跨年），
    /// 只靠明细算会在明细到期后突然少掉一大段。
    /// 键名不沿用 `*Tokens` 是因为它承载的语义比 tokens 宽（请求数与成功数）。
    #[serde(rename = "providerStats", default, skip_serializing_if = "Vec::is_empty")]
    pub provider_stats: Vec<ProviderAccum>,
    /// 当天的按账号累计（**契约之外的补充字段**）。
    ///
    /// 与 `providerStats` 同一理由：区间级的账号用量排行要跨过明细的 30 天
    /// 保留期，只有明细的话 `all` / `month` 会在明细到期后突然少掉一大段。
    #[serde(rename = "accountStats", default, skip_serializing_if = "Vec::is_empty")]
    pub account_stats: Vec<AccountAccum>,
}

impl DailyEntry {
    /// 空格子（新建某一天时用）
    pub(super) fn new(date: String) -> Self {
        Self {
            date,
            requests: 0,
            successful: 0,
            tokens: 0,
            cache_hit_tokens: 0,
            cache_input_tokens: 0,
            model_tokens: Vec::new(),
            provider_stats: Vec::new(),
            account_stats: Vec::new(),
        }
    }
}

/// 单个模型在某天的累计（聚合行的组成部分）
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelAccum {
    pub model: String,
    #[serde(default)]
    pub requests: i64,
    #[serde(default)]
    pub tokens: i64,
}

/// 单个 provider 在某天的累计（聚合行的组成部分，W4 新增）。
///
/// 只存「成功数」而**不存失败数**：失败数 = `requests - successful`，
/// 存两份会出现「对手改过的文件，两列对不上账」这种无法判定的状态，
/// 而报表要输出的 `failures` 由一次减法得出，没有信息损失。
///
/// `provider` 为空串表示当天有「未知承载者」的请求（旧版本写出的明细、
/// 或转发前就失败的请求）—— 与 `RequestEntry.provider` 同一口径。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderAccum {
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub requests: i64,
    #[serde(default)]
    pub successful: i64,
    #[serde(default)]
    pub tokens: i64,
}

/// 单个账号在某天的累计（聚合行的组成部分）。
///
/// 与 `ProviderAccum` 同形（requests / successful / tokens + 身份），
/// 因为两份累计回答的是同一个问题的两个视角：「谁承载的」与「哪个账号承载的」。
/// 失败数同样由减法得出，不另存一列（理由见 `ProviderAccum`）。
///
/// ── 为什么同时存 id 与 name ──────────────────────────────────
/// `accountId` 是身份、`accountName` 是**当时的展示名快照**：账号可以在账号页
/// 改名，名字变了不该把它拆成两行，所以身份只认 id（见 `push_account_accum`
/// 的匹配规则）；但名字也要留在文件里 —— 账号被删除后（明细与聚合的寿命
/// 都长于账号记录）报表仍要显示一个可读的名字，而不是一串 id。
///
/// 两个字段都为空表示「不知道是谁承载的」：走默认登录态转发（未配置账号列表）
/// 或旧版本写出的行。与 provider 的空串同一口径，由展示层给占位文案。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AccountAccum {
    #[serde(rename = "accountId", default)]
    pub account_id: String,
    #[serde(rename = "accountName", default)]
    pub account_name: String,
    #[serde(default)]
    pub requests: i64,
    #[serde(default)]
    pub successful: i64,
    #[serde(default)]
    pub tokens: i64,
}

/// 明细查询条件（「模型请求日志」页用）——
/// 路由 `/api/stats/requests` 的查询串解析结果（见 `api::stats_api`）
#[derive(Clone, Debug, Default)]
pub struct RequestQuery {
    pub offset: usize,
    /// 默认 `DEFAULT_LIMIT`，夹在 `[1, MAX_LIMIT]`
    pub limit: Option<usize>,
    /// 精确匹配模型名；None 不过滤
    pub model: Option<String>,
    /// `"ok"` = 2xx / `"error"` = 非 2xx / None 不过滤
    pub status: Option<String>,
    /// 起始毫秒时间戳（闭区间下界）
    pub start: Option<i64>,
    /// 结束毫秒时间戳（**开**区间上界）
    pub end: Option<i64>,
}
