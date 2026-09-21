//! 配置的**对外契约**：默认值、键名常量、各设置的形状与取值范围。
//!
//! ── 为什么单独一层 ──────────────────────────────────────────
//! 这些名字同时是**三处的契约**：`config.json`（现在是 `kv` 表的键）、
//! HTTP API 的响应体、以及前端 `config.X` 的读法。它们必须只有一处定义 ——
//! 读侧与写侧各写一遍字符串，任一处拼错都不报错，只会静默地读到默认值。
//! 抽成独立文件后「配置一共有哪些键、各自什么范围」一眼可见，
//! 而 `mod.rs` 只留「怎么读写」。
//!
//! ── 键名为什么一字不改（camelCase 保留）─────────────────────
//! 改造前它们是 `config.json` 的顶层键；进 `kv` 表后**仍然用原名**，
//! 不改成 snake_case：这些名字同时是 HTTP API 契约（`GET /api/config` 的
//! 响应体、前端 `config.X` 的读法），改名会让前端、账号迁移、模型规则三处
//! 同时受影响。数据库里的键名与 JSON 键一一对应，排障时能直接把库里的值
//! 贴进配置文件比对（约定见 `db::schema` 模块头）。
//! 另一面：这些键**绝不能与 `db::schema::RESERVED_KV_KEYS` 相撞** ——
//! 撞名的后果是配置写入把别人的状态删掉（论证见那里）。
//!
//! ── 两类常量 ────────────────────────────────────────────────
//!   - **键名**（`KEY_*`）：读写两侧共用的字符串；
//!   - **边界**（`DEFAULT_*` / `*_MIN_*` / `*_MAX_*`）：读侧回落与写侧校验
//!     共用同一份数字，避免「接口拒绝 60 而手改库接受它」这种两套口径。

/// 默认模型：客户端未指定模型时使用（对应 Node 版 `--default-model` 默认值）
pub const DEFAULT_MODEL: &str = "auto";
/// 计费接口默认语言（对应 Node 版 `--locale` 默认值）
pub const DEFAULT_LOCALE: &str = "zh-CN";

// ─── 保留期设置的键名与边界（config.json 里的字段名**就是契约**）─────
// 命名风格与既有字段（apiKey / locale / lastRequestModel / autoCheckin）一致：
// camelCase。这里把键名提成常量，是因为**读侧与写侧必须用同一个字符串** ——
// 任一处手写拼错都不会报错，只会静默地读到默认值。

/// 事件日志（统一库的 `logs` 表）保留天数
pub const KEY_LOG_RETENTION_DAYS: &str = "logRetentionDays";
/// 请求日志（统一库的 `requests` 表）保留天数
pub const KEY_REQUEST_RETENTION_DAYS: &str = "requestRetentionDays";
/// 按天聚合（统一库的 `request_daily` 表）保留天数
pub const KEY_DAILY_RETENTION_DAYS: &str = "dailyRetentionDays";

/// 事件日志的保存目录（config.json 键）。
///
/// 值是**绝对路径**；缺省 / 空串 / 相对路径（读侧视为写坏）都回落配置目录 ——
/// 两类数据各一个键，互不约束（可以搬到同一个目录，文件名不冲突）。
pub const KEY_LOG_DIR: &str = "logDir";
/// 请求日志（明细 + 按天聚合）的保存目录（config.json 键），语义同 `KEY_LOG_DIR`
pub const KEY_REQUEST_STATS_DIR: &str = "requestStatsDir";
/// 调试模式原始报文的保存目录（config.json 键），语义同 `KEY_LOG_DIR`
pub const KEY_DEBUG_DIR: &str = "debugDir";

/// 调试模式开关（config.json 键）。
///
/// 开启后转发层会把**发给上游的请求头（脱敏）与请求体、上游返回的响应头与
/// 响应体**完整落到统一库的 `debug_traffic` 表（见 `core::debug_traffic`），请求日志
/// 页的「详情」列据此展示。默认关闭 —— 报文体积可达数百 KB，常开会让日志目录
/// 迅速膨胀；关闭时采集路径完全不执行（零开销，见各采集点的 `if enabled`）。
pub const KEY_DEBUG_MODE: &str = "debugMode";

/// 三档保留天数的默认值（缺失时用它们）
pub const DEFAULT_LOG_RETENTION_DAYS: i64 = 30;
pub const DEFAULT_REQUEST_RETENTION_DAYS: i64 = 30;
pub const DEFAULT_DAILY_RETENTION_DAYS: i64 = 365;

/// **历史**路由优先级键：`{"workbuddy": 10, "raccoon": 20}`。
///
/// provider 路由优先级已随「账号全局一条队列」下线（先用哪一家由账号优先级
/// 决定）。这个键只在账号存储的启动迁移里读一次（`legacy_provider_route`），
/// 用来把旧版「按家分队」的号码按旧的实际顺序合并成全局队列；不再有写侧，
/// 文件里残留的值也不会被抹掉（未知字段全量保留）。
pub const KEY_PROVIDER_ROUTE: &str = "providerRoute";

/// 天数的合法范围：下限 1 天（保留 0 天等于什么都不存，不是有效配置），
/// 上限 10 年（防手改 config.json 写个天文数字让裁剪逻辑空转）。
///
/// **写侧（`stats_api::parse_days`）与读侧（`days_field`）共用这两个常量**：
/// 若两边各写一套数字，手改文件与走接口设值就会出现两套口径
/// （比如接口拒绝 5000 而读侧接受它）。两侧的处理方式不同是有意的：
/// 走接口的非法值给 400（用户当场能改），手改文件的非法值回落到默认（不打扰）。
pub const RETENTION_MIN_DAYS: i64 = 1;
pub const RETENTION_MAX_DAYS: i64 = 3650;

/// 三档保留天数（设置页「数据保留」区域）。
///
/// 用独立结构而不是三个散落的取值函数：三个值总是一起用（GET 一起返回、
/// 裁剪时各自取用），打包成一个 `Copy` 值让调用方一次拿到、不必多次读锁。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetentionSettings {
    /// 事件日志保留天数
    pub log_days: i64,
    /// 请求日志保留天数
    pub request_days: i64,
    /// 按天聚合保留天数
    pub daily_days: i64,
}

impl Default for RetentionSettings {
    fn default() -> Self {
        Self {
            log_days: DEFAULT_LOG_RETENTION_DAYS,
            request_days: DEFAULT_REQUEST_RETENTION_DAYS,
            daily_days: DEFAULT_DAILY_RETENTION_DAYS,
        }
    }
}

/// 保留期的**部分**更新入参（PUT /api/retention 允许只传其中几项）。
/// `None` = 这一项不动（对应「允许部分字段」的契约）。
#[derive(Clone, Copy, Debug, Default)]
pub struct RetentionPatch {
    pub log_days: Option<i64>,
    pub request_days: Option<i64>,
    pub daily_days: Option<i64>,
}

// ─── 定时任务设置的键名与边界（config.json 里的字段名**就是契约**）─────
//
// 间隔型任务（凭证维护 / 定时查询积分 / 模型刷新 / 软件版本检查 / 两个前端
// 自动刷新）打包在 `scheduledTasks`
// 对象下；自动签到不在其中 —— 它是**每天定点**型，时刻与上次执行结果由
// `core::auto_checkin` 自己管（`autoCheckin` 字段），本模块不重复持有。
// 页面上的这些间隔型任务是「同一个形状」，所以配置也写成同一形状，
// 免得读侧要按任务名各写一套解析。

/// 间隔型任务的配置对象键
pub const KEY_SCHEDULED_TASKS: &str = "scheduledTasks";
/// 凭证自动维护在 `scheduledTasks` 下的子键
pub const KEY_CREDENTIAL_MAINTENANCE: &str = "credentialMaintenance";
/// 模型目录定时刷新在 `scheduledTasks` 下的子键
pub const KEY_MODEL_REFRESH: &str = "modelRefresh";
/// 日志页自动刷新在 `scheduledTasks` 下的子键（**前端**定时器，后端只存配置）
pub const KEY_LOGS_AUTO_REFRESH: &str = "logsAutoRefresh";
/// 请求日志页自动刷新在 `scheduledTasks` 下的子键（同上）
pub const KEY_REQUESTS_AUTO_REFRESH: &str = "requestsAutoRefresh";
/// 报表页自动刷新在 `scheduledTasks` 下的子键（同上）
pub const KEY_REPORT_AUTO_REFRESH: &str = "reportAutoRefresh";
/// 软件版本检查在 `scheduledTasks` 下的子键（后端定时向 GitHub 查最新发布版本）
pub const KEY_UPDATE_CHECK: &str = "updateCheck";
/// 定时查询积分在 `scheduledTasks` 下的子键（后端定时查全部账号的余额 / 积分）
pub const KEY_USAGE_QUERY: &str = "usageQuery";
/// 敏感词库更新在 `scheduledTasks` 下的子键（后端定时拉仓库里的 `sensitive-words.json`）
pub const KEY_SENSITIVE_SYNC: &str = "sensitiveSync";

/// 凭证维护默认间隔（分钟）：与改造前的硬编码 600 秒一致
pub const DEFAULT_CREDENTIAL_MAINTENANCE_MINUTES: i64 = 10;
/// 模型目录定时刷新默认间隔（分钟）。
///
/// 保守取值：WorkBuddy 的 `/v3/config` 拉取**没有 TTL 早退**，每一轮都是真打
/// 上游（见 `providers::workbuddy` 的 `refresh_models`），间隔太密等于给上游
/// 添无谓的负载。一小时的粒度对「模型清单变了没」这个问题足够。
pub const DEFAULT_MODEL_REFRESH_MINUTES: i64 = 60;
/// 两个前端自动刷新的默认间隔（秒）：每秒一次。
///
/// 比改造前的硬编码 10 秒密得多，这是有意的：两条任务都只在**对应页面可见时**
/// 才请求（`document.hidden` 与当前页都判过），离开页面就完全静默，所以
/// 「密」的代价只落在用户正盯着那一页的时候 —— 而那正是他想要实时的时刻。
/// 两条接口都是本地读写（一条读日志库、一条查统计库），不出网。
pub const DEFAULT_LOGS_AUTO_REFRESH_SECONDS: i64 = 1;
pub const DEFAULT_REQUESTS_AUTO_REFRESH_SECONDS: i64 = 1;
pub const DEFAULT_REPORT_AUTO_REFRESH_SECONDS: i64 = 1;
/// 软件版本检查默认间隔（分钟）：每 5 分钟查一次 GitHub 最新发布。
///
/// GitHub 匿名限额是 60 次/小时/IP：5 分钟一次（12 次/小时）留足余量；
/// 下限仍是全局的 INTERVAL_MIN_MINUTES，但设到 1 分钟贴着限额跑没有意义。
pub const DEFAULT_UPDATE_CHECK_MINUTES: i64 = 5;
/// 定时查询积分的默认间隔（分钟）：每 10 分钟查一次全部账号的余额。
///
/// 与凭证维护同档：一条余额查询就是逐账号打一次上游的积分接口，
/// 10 分钟一次（每小时 6 轮）对这个「看一眼还剩多少」的需求足够，
/// 也不会因为间隔过密给上游添负担、触发风控。
pub const DEFAULT_USAGE_QUERY_MINUTES: i64 = 10;
/// 敏感词库更新的默认间隔（分钟）：每 10 分钟拉一次仓库里的 `sensitive-words.json`。
///
/// 取值理由与软件版本检查（5 分钟）同源，但更宽松：走 `raw.githubusercontent.com`
/// **不计入** GitHub API 的匿名限额（60 次/小时/IP），所以密集些也不会连累别的
/// 任务；而 10 分钟一轮（144 次/天）已经足够跟上「上游改了拦截规则 → 我们补词」
/// 这个节奏 —— 真正紧急时用户还有「立即执行」按钮。
pub const DEFAULT_SENSITIVE_SYNC_MINUTES: i64 = 10;

/// 间隔型任务的取值范围。上下限分两套（分钟 / 秒），因为两类任务的合理区间
/// 差着量级：后端维护任务按分钟（1 分钟～1 天），前端刷新按秒（1 秒～10 分钟）。
///
/// 秒级下限放到 1 秒：这两条任务是**页面可见才跑**的本地轮询（不出网、不打上游），
/// 密一点最坏是「多读几次本地库里的一页数据」，不会给任何外部服务添负担。
/// 原来的 5 秒下限没有技术理由，只是照着改造前的 10 秒兜底值随手划的。
///
/// 与保留期同样：**写侧（`scheduled_tasks::parse_interval`）与读侧
/// （`interval_field`）共用这些常量**，否则会出现「接口拒绝 60 而手改文件接受它」。
pub const INTERVAL_MIN_MINUTES: i64 = 1;
pub const INTERVAL_MAX_MINUTES: i64 = 1440;
pub const INTERVAL_MIN_SECONDS: i64 = 1;
pub const INTERVAL_MAX_SECONDS: i64 = 600;

/// 一个间隔型任务的配置：开关 + 间隔（单位由任务定义决定）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IntervalTask {
    pub enabled: bool,
    /// 间隔值，单位见任务定义（分钟或秒）
    pub interval: i64,
}

/// 七条间隔型任务的配置（设置页「定时任务」区域）。
///
/// 与 `RetentionSettings` 同一取舍：几个值总是一起用（GET 一次返回、各自循环
/// 各取所需），打包成一个 `Copy` 值让调用方一次拿到、不必多次读锁。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScheduledSettings {
    pub credential_maintenance: IntervalTask,
    pub model_refresh: IntervalTask,
    pub logs_auto_refresh: IntervalTask,
    pub requests_auto_refresh: IntervalTask,
    pub report_auto_refresh: IntervalTask,
    pub update_check: IntervalTask,
    pub usage_query: IntervalTask,
    pub sensitive_sync: IntervalTask,
}

impl Default for ScheduledSettings {
    fn default() -> Self {
        Self {
            credential_maintenance: IntervalTask {
                enabled: true,
                interval: DEFAULT_CREDENTIAL_MAINTENANCE_MINUTES,
            },
            model_refresh: IntervalTask {
                enabled: true,
                interval: DEFAULT_MODEL_REFRESH_MINUTES,
            },
            logs_auto_refresh: IntervalTask {
                enabled: true,
                interval: DEFAULT_LOGS_AUTO_REFRESH_SECONDS,
            },
            requests_auto_refresh: IntervalTask {
                enabled: true,
                interval: DEFAULT_REQUESTS_AUTO_REFRESH_SECONDS,
            },
            report_auto_refresh: IntervalTask {
                enabled: true,
                interval: DEFAULT_REPORT_AUTO_REFRESH_SECONDS,
            },
            update_check: IntervalTask {
                enabled: true,
                interval: DEFAULT_UPDATE_CHECK_MINUTES,
            },
            usage_query: IntervalTask {
                enabled: true,
                interval: DEFAULT_USAGE_QUERY_MINUTES,
            },
            sensitive_sync: IntervalTask {
                enabled: true,
                interval: DEFAULT_SENSITIVE_SYNC_MINUTES,
            },
        }
    }
}

/// 一条间隔型任务的**部分**更新入参（`None` = 该项不动）。
#[derive(Clone, Copy, Debug, Default)]
pub struct IntervalTaskPatch {
    pub enabled: Option<bool>,
    pub interval: Option<i64>,
}

// ─── 请求重试设置的键名与边界（config.json 里的字段名**就是契约**）─────
//
// 转发层的退避重试读这三个值（见 `upstream::provider_loop::send_with_retry`
// 与 `attempt_queue`）。
//
// ── 为什么分两档次数（同账号原地重发 / 换账号）──────────────
// 一次转发失败后的处置有两条路，代价与收益完全不同：
//   - **在同一个账号上原地重发**：便宜，可能只是瞬时抖动或敏感词误拦，
//     等一个间隔再发一次往往就好了 —— 这是 `retryCount`；
//   - **换一个账号再试**：换号要走另一份额度与限流，还可能是另一家，
//     用户往往希望「先在当前账号多试几次，实在不行再换号」—— 这是
//     `retryCrossProviderCount`。
//
// 判定口径（`provider_loop::attempt_queue` 记账）：
//   - 请求**首次选中的那个账号**用 `retryCount`，原地重发几次；
//   - **每换一个账号**（不分是同家的下一个还是另一家的）扣一次
//     `retryCrossProviderCount`，扣满就带着最后一次的错误收尾，
//     不再往下顺延。
//
// ── 第二档为什么按「账号」而不是按「提供商」（2026-09 修正）────
// 旧实现按提供商分段：同一家名下的所有账号算一段、共用一份原地重发预算，
// 只有跨家才重新给一份。这有两个后果，都与用户对这个数字的预期不符：
//   - 「能换几个账号」根本没有任何设置管 —— 只受队列里账号总数限制，
//     某家囤了 9 个账号时，一次请求会一路试到第 10 个（用户实测截图）；
//   - 用户填的 5 只在「跨家之后」生效，而跨家本身已经是队列走完的副产品，
//     等到那时往往早就没有可用账号了。
// 现在改成「换账号次数」：不管换到哪一家，换一次扣一次。键名保留
// `retryCrossProviderCount` 不变（改名会让老配置读不到、回落默认值，
// 得额外做迁移，不划算）—— 键名是历史包袱，语义以本节为准。

/// **同一账号内**的原地重发次数（0 = 失败立即换号，不重发）
pub const KEY_RETRY_COUNT: &str = "retryCount";
/// **换账号**的次数（0 = 不换号，直接收尾）。
///
/// ── 常量名与 JSON 键名为什么对不上 ─────────────────────────
/// 键名（`retryCrossProviderCount`）是**配置契约**：改名会让改了名的老配置
/// 读不到、静默回落成默认值，还得额外做一次迁移，不划算 —— 所以字符串
/// 原样冻结。常量名（Rust 侧标识符）说的才是真语义：**按账号计，不分家**，
/// 换到同家的下一个账号与换到另一家都算一次。语义详见本节开头的说明。
pub const KEY_RETRY_ACCOUNT_SWITCH_COUNT: &str = "retryCrossProviderCount";
/// 两次重试之间的等待秒数
pub const KEY_RETRY_INTERVAL_SECONDS: &str = "retryIntervalSeconds";

/// 同一账号的原地重发次数默认值：失败后再试 3 次（连同首发共 4 次发送）
pub const DEFAULT_RETRY_COUNT: i64 = 3;
/// 换账号的次数默认值：失败后最多换 5 个账号。
///
/// 比原地重发那档更宽是刻意的：换号的成本主要在「等到下一个可用账号」，
/// 而一个账号失败往往说明它这份额度确实不通，多换几个比快速失败更符合预期。
pub const DEFAULT_RETRY_SWITCH_COUNT: i64 = 5;
/// 重试间隔默认值：5 秒
pub const DEFAULT_RETRY_INTERVAL_SECONDS: i64 = 5;

/// 次数与间隔的合法范围。
///
/// 上限 10 次 / 300 秒：次数过多或间隔过长都会让客户端干等（重试是「再发一次」，
/// 换号也是「换个人再发一次」，都不是把请求拆成多次）。下限 0：次数 0 = 关闭
/// 该档（不重发 / 不换号），间隔 0 = 立即重发。
pub const RETRY_MIN_COUNT: i64 = 0;
pub const RETRY_MAX_COUNT: i64 = 10;
pub const RETRY_MIN_INTERVAL_SECONDS: i64 = 0;
pub const RETRY_MAX_INTERVAL_SECONDS: i64 = 300;

/// 请求重试设置（设置页「通用 → 请求重试」区域）。
///
/// 与 `RetentionSettings` 同一取舍：几个值总是一起用（转发层每次重试判定
/// 都取），打包成 `Copy` 值让调用方一次拿到、不必多次读锁。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetrySettings {
    /// **同一账号内**的原地重发次数（0 = 失败立即换号）
    pub count: i64,
    /// **换账号**的次数上限（0 = 不换号，直接收尾）。
    ///
    /// 字段名保留了旧措辞、JSON 键名也没变（见
    /// [`KEY_RETRY_ACCOUNT_SWITCH_COUNT`]）—— 键名是配置契约，改名会让老配置
    /// 读不到。此处标识符说的是真语义：**按账号计，不分家**，换到同家的下一个
    /// 账号与换到另一家都算一次。
    pub account_switch_count: i64,
    /// 两次重试之间的间隔（秒）
    pub interval_seconds: i64,
}

impl RetrySettings {
    /// 间隔的毫秒形态（转发层的 sleep 直接用）
    pub fn delay_ms(&self) -> u64 {
        self.interval_seconds.max(0) as u64 * 1000
    }

    /// 同一账号的原地重发预算（与 [`Self::switch_budget`] 是两个独立的口径）。
    ///
    /// 负值按 0 处理：`bounded_int_field` 已保证范围，这里是防御性的
    /// （负数转 `usize` 会回绕成天文数字，那会让重试变成死循环）。
    pub fn resend_budget(&self) -> usize {
        self.count.max(0) as usize
    }

    /// 换账号的次数上限（换一次扣一次，扣满即收尾）。
    pub fn switch_budget(&self) -> usize {
        self.account_switch_count.max(0) as usize
    }
}

impl Default for RetrySettings {
    fn default() -> Self {
        Self {
            count: DEFAULT_RETRY_COUNT,
            account_switch_count: DEFAULT_RETRY_SWITCH_COUNT,
            interval_seconds: DEFAULT_RETRY_INTERVAL_SECONDS,
        }
    }
}

/// 请求重试的**部分**更新入参（`None` = 该项不动）。
#[derive(Clone, Copy, Debug, Default)]
pub struct RetryPatch {
    pub count: Option<i64>,
    pub account_switch_count: Option<i64>,
    pub interval_seconds: Option<i64>,
}
