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

/// 一条请求明细。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RequestEntry {
    /// 请求发起时刻（毫秒 Unix 时间戳）
    pub ts: i64,
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
}

/// `attempts` 的 serde 默认值（载入缺该字段的旧行时按 1 次算）
fn one() -> i64 {
    1
}

impl RequestEntry {
    /// 是否成功（2xx）—— 统计口径里 `successful` 用它判定
    pub(super) fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
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
    pub model: String,
    pub account_id: String,
    pub account_name: String,
    pub status: i64,
    pub duration_ms: i64,
    pub attempts: i64,
    pub error: Option<String>,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    pub cache_read_tokens: i64,
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
            attempts: 1,
            error: None,
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
            cache_read_tokens: 0,
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
        let failed = !(200..300).contains(&self.status);
        let token = |value: i64| if failed { 0 } else { value.max(0) };
        RequestEntry {
            ts,
            model: self.model,
            account_id: self.account_id,
            account_name: self.account_name,
            status: self.status,
            duration_ms: self.duration_ms.max(0),
            attempts: self.attempts.max(1),
            error: self.error.filter(|text| !text.is_empty()),
            prompt_tokens: token(self.prompt_tokens),
            completion_tokens: token(self.completion_tokens),
            total_tokens: token(self.total_tokens),
            cache_read_tokens: token(self.cache_read_tokens),
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
