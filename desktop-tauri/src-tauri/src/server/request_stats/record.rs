//! 请求统计的数据类型与 JSON 契约（唯一事实来源）。
//!
//! ── 字段名为什么逐个写 `#[serde(rename)]` ─────────────────────
//! 前端的统计报表与「模型请求日志」直接读这些键名，所以字段名**就是契约**。
//! 这里不用 `rename_all = "camelCase"` 而是逐字段显式写出：改动字段名时
//! diff 里必然出现 `rename` 那一行，不会因为「顺手调整 rename 策略」而
//! 静默改掉线上格式。
//!
//! ── `RequestEntry` 的序列化用途变了，但键名契约没变 ──────────
//! 改造前它整体序列化成 JSONL 的一行落盘；现在明细进 `requests` 表的**列**
//! （每字段一列，见 `db/schema.rs`），序列化只剩两个用途：
//!   - `report::entry_json` 把它转成 `/api/stats/requests` 的响应行
//!     （前端读的就是这套键名，所以 `rename` 一个都不能少）；
//!   - 迁移项解析**旧文件**（`parse_requests_jsonl`）反序列化回来。
//! 换句话说：它现在是「API 契约 + 旧格式解析器」，不再是落盘格式。
//!
//! ── 为什么读入侧全字段带 `default` ───────────────────────────
//! 两个地方要靠它容错，都还在生效：
//!   - **旧文件解析**（迁移项）：明细 30 天、聚合一年，中间可能跨好几个版本，
//!     缺少新字段的旧行必须还能读进来（缺的按 0 / 空算），否则升级一次
//!     就会把用户已有的曲线整段丢掉；
//!   - **聚合的三个 JSON 列**（`model_tokens` / `provider_stats` /
//!     `account_stats`）：整体读写，条目结构也可能随版本增加列，
//!     `default` 让「多一个键」不至于让整列解析失败。

use serde::{Deserialize, Serialize};

/// 明细保留的最大条数（超出丢最旧的）。
///
/// 与按时间的保留期是双保险：保留期管「多久」，上限管「多少」——
/// 短时间内的突发流量不该按天数比例吃光磁盘。
///
/// 改造前它是「内存数组的环形保留」（在 `insert_sorted` 里 drain）；现在由
/// `sql::trim_requests_capacity` 用一条 `DELETE` 守住，**按 ts 保留最新的 N 条**
/// —— 与旧实现「从升序数组头部 drain 掉溢出部分」等价（头部就是 ts 最小的那些）。
pub const MAX_ENTRIES: usize = 20_000;

/// 聚合行保留的最大天数（兜底：手改库塞进十万行时不至于把报表撑爆）
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
    /// 序列化出来的键名就是 `provider`，与字段名相同（本文件的 `rename` 都出现在
    /// 两者不一致的字段上）。`default` 保证缺键的旧行照常读入；空值**照给**
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
    /// **每一次上游尝试的明细**（`[{provider, status, error}]`，按发生顺序）。
    ///
    /// ── 与 `attempts` 的关系（这是本字段存在的全部理由）────────────
    /// `attempts` 只回答「试了几次」，`provider` 只回答「最后是谁扛的」——
    /// 中间那几轮换了谁、为什么换，在改造前**没有任何落点**，所以请求日志的
    /// 「重试」列只能显示一个没有信息量的「重试」标记。本字段就是那段历史。
    ///
    /// 条数上限：采集侧保证 `attempt_details.len() <= attempts`，而 `attempts`
    /// 的上界是 `MAX_ROUTE_ATTEMPTS + 1`（32+1）。采集侧另有
    /// `MAX_ATTEMPT_DETAILS`（24）的体积闸 —— 截断时明细会比 `attempts` 短，
    /// 前端据此显示「只保留前 N 条」（见 requests-panel）。
    ///
    /// ── 为什么存 JSON 文本列而不是拆表 ──────────────────────────
    /// 它是「一条明细的附属历史」，**整体读写**（写入时整份给、读取时整份取），
    /// 没有任何按单次尝试查询的需求。拆一张 `request_attempts` 表要引入
    /// 外键、级联删除与分页 join，而收益只是「能 SQL 查某一次尝试」——
    /// 那是排障场景，`sqlite3` 里对 JSON 列用 `json_each` 一样能查。
    /// 与 `request_daily` 的三个 `*Stats` JSON 列同一取舍（见 schema.rs）。
    ///
    /// ── 旧行怎么办 ──────────────────────────────────────────────
    /// `default` 读成空表：旧行没有这个键，空表是「不知道明细」，与「一次都没试」
    /// 不是一回事，但展示层对两者的处理相同（重试链为空时该列只在有敏感词命中
    /// 时才有内容）。与 `provider` 的空串同一约定：**不给旧数据猜值**。
    #[serde(rename = "attemptDetails", default)]
    pub attempt_details: Vec<AttemptDetail>,
    /// **本次请求命中的敏感词**（`[{word, count}]`，按次数降序；空表 = 没命中）。
    ///
    /// 结构与 `core::upstream::usage::SensitiveHit` **逐字段相同**（`word` /
    /// `count`）：那是它的产生处，这里只是落库的镜像。两处刻意不做类型别名、
    /// 各自独立定义 —— `request_stats` 是存储层，不该为了省一次转换就依赖
    /// `core::upstream` 的运行时类型（那样这个存储契约会被一个与存储无关的
    /// 模块绑住，将来转发层重构会牵动库格式）。
    ///
    /// 展示位置：请求日志「重试」列里的紫色标签（悬停看命中词），与 OmniProxy
    /// 的 `SensitiveMaskedTag` 同形。命中**不再**在「日志」页展示 —— 改造前那
    /// 边靠 `[Desensitize] 已脱敏命中…` 那行应用日志间接呈现（见 logs-panel
    /// 的模块头），现在逐条请求有了直接落点，日志页那层间接展示整体移除；
    /// 应用日志那一行本身**仍然保留**，它是排障时的实时记录，与请求日志的
    /// 用途不同（一个回答「刚才发生了什么」，一个回答「这条请求命中了什么」）。
    #[serde(rename = "sensitiveHits", default)]
    pub sensitive_hits: Vec<SensitiveHit>,
}

/// 一个被命中的敏感词及其次数（存储契约）。
///
/// 与 `core::upstream::usage::SensitiveHit` 同形但各自独立定义，理由见
/// `RequestEntry::sensitive_hits`。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SensitiveHit {
    /// 命中的词
    #[serde(default)]
    pub word: String,
    /// 本次请求里命中的次数
    #[serde(default)]
    pub count: i64,
}

/// 单次上游尝试的明细（存储契约）。
///
/// 与 `core::upstream::usage::AttemptDetail` 同形但各自独立定义，理由同上。
/// `provider` 空串表示「这一轮没记下承载者」—— 采集侧保证它非空
/// （`note_attempt_started` 的入参就是 provider_id），所以空串只可能来自
/// 手工改过的库，展示层照常给占位文案。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AttemptDetail {
    /// 这一轮实际发送的 provider id
    #[serde(default)]
    pub provider: String,
    /// 这一轮的 HTTP 状态码（None = 未定论或传输层失败，见采集侧的说明）
    #[serde(default)]
    pub status: Option<i64>,
    /// 这一轮的失败摘要（成功时为 None）
    #[serde(default)]
    pub error: Option<String>,
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
    /// 正常路径用不到 —— 明细进库前已经过 `NewRequestEntry::normalize`。
    /// 但**从旧文件导入的历史明细可能来自更早的版本**：1.x 的 `normalize` 只做
    /// 数值夹取，没有「失败清零」这一段，那时失败的请求也照记 token
    /// （旧文件里确有这类行：`status: 200` + 流未完整下发的摘要 + 六位数 token）。
    /// 回填聚合要拿这些旧行重算，必须先把它们过一遍今天的口径，
    /// 否则报表会把「失败也算用量」这个旧规则带进 token 曲线 ——
    /// 而当前契约是「失败的请求不产生用量」。
    ///
    /// 唯一调用点：`backfill::rebuild_legacy_days`（由**迁移项**在导入旧数据时
    /// 调用）。运行期不再有调用点 —— 库里的行都出自今天的 `normalize`。
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
    /// W4 接上持久化：见 `normalize` 末尾的透传）。
    ///
    /// `None` = 一次都没发出去就失败了（请求体非法 / 模型不存在 / 无可用账号），
    /// 入库时归一成空串，聚合层归入「未知」组。
    pub provider: Option<String>,
    /// 下游请求的模型名（客户端原值；空串 = 未点名 / 未记录）
    pub client_model: String,
    /// 实际发给上游的模型名（空串 = 一次都没发出去 / 未记录）
    pub upstream_model: String,
    /// 每一次上游尝试的明细（见 `RequestEntry::attempt_details`）。
    /// 空表 = 一次都没发出去（转发前就失败）或采集侧没记上。
    pub attempt_details: Vec<AttemptDetail>,
    /// 本次请求命中的敏感词（见 `RequestEntry::sensitive_hits`）。空表 = 没命中。
    pub sensitive_hits: Vec<SensitiveHit>,
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
            // 两条都是「有采集才有值」：转发前就失败的请求走 `record_early_failure`，
            // 那里构造的 telemetry 是空的，于是两个空表如实表达「没发生过尝试 /
            // 没命中过词」—— 而不是留一个需要读侧再判一次的 None
            attempt_details: Vec::new(),
            sensitive_hits: Vec::new(),
        }
    }

    /// 归一化成可入库的条目。
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
        // provider 从入参透传到记账条目（W2b 留的丢弃点在此接上）。
        // `None`（转发链没走到选路就失败）与空串（上游返回了空 id）都归一成
        // **空串**：两者在报表语义上都是「未知承载者」，多一种表示只会让
        // 聚合与前端各写一遍「None 也算空」。trim 一下，避免旧文件或异常
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
            // ── 两个明细字段的归一（本次改造）───────────────────────
            // 都做「清掉空项 + 夹到合理形状」，让读侧（SQL false 编码、前端渲染）
            // 拿到的永远是可直接消费的表：
            //   · provider 空串的明细项**保留**（它表达「这一轮没记下是谁」，
            //     与前缀字段 `provider` 的空串语义一致，读侧本来就要处理）；
            //   · error 空串收敛成 None（与 `error` 字段同一口径：空串不是错误）；
            //   · 敏感词的 count 夹到 ≥0（负数只可能来自手改的库，展示层不该
            //     为它写分支）；word 空的项**丢弃**（一个没有词的命中项没有
            //     任何信息量，留着只会渲染出一行「× 3」）。
            attempt_details: self
                .attempt_details
                .into_iter()
                .map(|item| AttemptDetail {
                    provider: item.provider.trim().to_string(),
                    status: item.status,
                    error: item.error.filter(|text| !text.is_empty()),
                })
                .collect(),
            sensitive_hits: self
                .sensitive_hits
                .into_iter()
                .map(|hit| SensitiveHit {
                    word: hit.word.trim().to_string(),
                    count: hit.count.max(0),
                })
                .filter(|hit| !hit.word.is_empty())
                .collect(),
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
    /// 单天无模型明细时整键省略（`skip_serializing_if`）——
    /// 这条在**旧文件解析**时仍生效（迁移读的就是旧格式），入库时那三个列由
    /// `sql::encode_accum` 统一写成 JSON 文本（空表则是 `[]`，
    /// 与 DDL 的默认值同形）。
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
/// 存两份会出现「对手改过的库，两列对不上账」这种无法判定的状态，
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
/// 的匹配规则）；但名字也要一起留下 —— 账号被删除后（明细与聚合的寿命
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
