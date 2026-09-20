//! 网关自身配置（`{config_dir}/config.json`）的读写与运行时快照。
//!
//! 与 Node 版 `loadConfig` / `saveConfig` / `applyConfig` 对齐（server.mjs 242-264 行）。
//! 关键点：整个 JSON 对象是**松散**的 —— 后续切片会往同一个文件里加
//! autoCheckin、lastRequestModel、desensitize 等字段。因此这里刻意**不做 struct
//! 映射**，底稿一律是 `serde_json::Map`，全量保留未知字段：谁写盘都只能改自己那几项，
//! 绝不能因为结构体定义不全而把别人的字段吃掉。
//!
//! 运行期可变：Node 版改了配置直接改 `opts` 对象，请求热路径不再读盘；
//! Rust 里用 `RwLock<Option<RuntimeConfig>>` 持有同一份「当前生效值」，
//! 于是 POST /api/config 改完 API Key 后鉴权中间件立即生效（不重启、不读文件）。
//!
//! 优先级：配置文件的值 > 环境变量 > 内置默认值。
//! 注意 `WORKBUDDY_PROXY_API_KEY` 是「启动时注入」语义 —— 它会写进内存快照，
//! 但 POST /api/config 传 null 可以把它清掉（对应 Node 版 `opts.apiKey = null`）。
//!
//! 配置目录本身（`~/.agent2api`）的事实来源在 `crate::gateway::config_dir`，
//! 本模块只做转发；从 1.x 升级上来的一次性目录迁移在 `config_migration`，
//! 这里只保留旧目录名常量与旧目录路径访问器（`LEGACY_DIR_NAME` 仍是全仓
//! 唯一的字面量）。

use std::path::{Path, PathBuf};
use std::sync::RwLock;

use serde_json::{Map, Value};


/// 默认模型：客户端未指定模型时使用（对应 Node 版 `--default-model` 默认值）
pub const DEFAULT_MODEL: &str = "auto";
/// 计费接口默认语言（对应 Node 版 `--locale` 默认值）
pub const DEFAULT_LOCALE: &str = "zh-CN";

// ─── 保留期设置的键名与边界（config.json 里的字段名**就是契约**）─────
// 命名风格与既有字段（apiKey / locale / lastRequestModel / autoCheckin）一致：
// camelCase。这里把键名提成常量，是因为**读侧与写侧必须用同一个字符串** ——
// 任一处手写拼错都不会报错，只会静默地读到默认值。

/// 事件日志（logs.jsonl）保留天数
pub const KEY_LOG_RETENTION_DAYS: &str = "logRetentionDays";
/// 请求日志（requests.jsonl）保留天数
pub const KEY_REQUEST_RETENTION_DAYS: &str = "requestRetentionDays";
/// 按天聚合（request-daily.jsonl）保留天数
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
/// 响应体**完整落到 `debug-traffic.jsonl`（见 `core::debug_traffic`），请求日志
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
// 转发层的退避重试读这三个值（见 `upstream::provider_loop::send_with_retry`）。
//
// ── 为什么分两档次数（同一家 / 换了家）──────────────────────
// 一次转发失败后的处置有两条路：**在原提供商上再试**（可能换该家的下一个
// 账号，也可能只是同账号重发），以及**换一家提供商再试**。这两件事的代价与
// 收益完全不同：前者便宜、可能只是瞬时抖动；后者要跨到另一家的额度与限流上，
// 用户往往希望「先在本家多试几次，实在不行再换家」。所以两档各有一个次数，
// 而不是共用一个。
//
// 判定口径（`provider_loop::attempt_queue` 记账）：
//   - 请求**首次选中的那一家**用 `retryCount`；
//   - 一旦选中的账号属于**另一家**，预算就换成 `retryCrossProviderCount`，
//     且从零开始计（不是接着上一档扣）—— 「换家之后还能试 5 次」是用户填
//     这个数字时的直觉，接着扣会得到「换家后只剩 2 次」这种没人能预期的结果。

/// **同一提供商内**的重试次数（0 = 失败立即报错，不重试）
pub const KEY_RETRY_COUNT: &str = "retryCount";
/// **换到别的提供商之后**的重试次数（0 = 换家后不再重试）
pub const KEY_RETRY_CROSS_PROVIDER_COUNT: &str = "retryCrossProviderCount";
/// 两次重试之间的等待秒数
pub const KEY_RETRY_INTERVAL_SECONDS: &str = "retryIntervalSeconds";

/// 同一提供商的重试次数默认值：失败后再试 3 次（连同首次共 4 次发送）
pub const DEFAULT_RETRY_COUNT: i64 = 3;
/// 换家后的重试次数默认值：换到另一家后再试 5 次。
///
/// 比同一家那档更宽是刻意的：能走到换家说明本家确实不通（额度耗尽 / 持续
/// 5xx），此时多给几次试错机会比快速失败更符合预期。
pub const DEFAULT_RETRY_CROSS_PROVIDER_COUNT: i64 = 5;
/// 重试间隔默认值：5 秒
pub const DEFAULT_RETRY_INTERVAL_SECONDS: i64 = 5;

/// 次数与间隔的合法范围。
///
/// 上限 10 次 / 300 秒：次数过多或间隔过长都会让客户端干等（重试是「再发一次」，
/// 与「换一个账号」不是一回事）。下限 0：次数 0 = 关闭该档重试，间隔 0 = 立即重发。
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
    /// **同一提供商内**的最大重试次数（0 = 不重试）
    pub count: i64,
    /// **换到别的提供商之后**的最大重试次数（0 = 换家后不重试）
    pub cross_provider_count: i64,
    /// 两次重试之间的间隔（秒）
    pub interval_seconds: i64,
}

impl RetrySettings {
    /// 间隔的毫秒形态（转发层的 sleep 直接用）
    pub fn delay_ms(&self) -> u64 {
        self.interval_seconds.max(0) as u64 * 1000
    }

    /// 某个阶段的重试预算（`switched` = 已经换到别的提供商）。
    ///
    /// 负值按 0 处理：`bounded_int_field` 已保证范围，这里是防御性的
    /// （负数转 `usize` 会回绕成天文数字，那会让重试变成死循环）。
    pub fn budget(&self, switched: bool) -> usize {
        let value = if switched { self.cross_provider_count } else { self.count };
        value.max(0) as usize
    }
}

impl Default for RetrySettings {
    fn default() -> Self {
        Self {
            count: DEFAULT_RETRY_COUNT,
            cross_provider_count: DEFAULT_RETRY_CROSS_PROVIDER_COUNT,
            interval_seconds: DEFAULT_RETRY_INTERVAL_SECONDS,
        }
    }
}

/// 请求重试的**部分**更新入参（`None` = 该项不动）。
#[derive(Clone, Copy, Debug, Default)]
pub struct RetryPatch {
    pub count: Option<i64>,
    pub cross_provider_count: Option<i64>,
    pub interval_seconds: Option<i64>,
}

/// 运行期生效的配置快照。
///
/// 字段是「本切片真正会用到的」子集，其余未知字段留在 `raw` 里原样保留，
/// 写盘时一起回写。
#[derive(Clone, Debug, Default)]
pub struct RuntimeConfig {
    api_key: Option<String>,
    locale: String,
    default_model: String,
    last_request_model: Option<String>,
    /// 三档保留天数（事件日志 / 请求日志 / 按天聚合）。
    ///
    /// 为什么解析进字段而不是让调用方每次去 `raw` 里翻：保留期要被**每次记账 / 写日志**
    /// 取用（回调形式），从 `Value` 里逐个取值要处理类型不符、缺字段、范围夹紧，
    /// 放在这里解析一次即可；`raw` 仍是写盘时的唯一底稿。
    retention: RetentionSettings,
    /// 六条间隔型定时任务的开关与间隔（设置页「定时任务」区域）。
    ///
    /// 与保留期同一理由：凭证维护与模型刷新的循环**每一轮都要重读**它
    /// （改完设置下一轮生效，不重启进程），从 `Value` 里翻一次要处理一堆
    /// 类型与范围判定，解析一次存下来最省事。
    scheduled: ScheduledSettings,
    /// 请求重试的次数与间隔（设置页「通用 → 请求重试」区域）。
    ///
    /// 与保留期同一理由：转发层**每次重试判定**都要取它（改完设置下一个
    /// 失败请求就用新值，不重启进程），解析一次存下来最省事。
    retry: RetrySettings,
    /// 事件日志的保存目录（原始配置值；None = 未设置，用配置目录）。
    /// 低频字段（启动 + 设置页读写），不值得为它发明解析层，存原始值即可。
    log_dir: Option<String>,
    /// 请求日志的保存目录（原始配置值；None = 未设置）
    request_stats_dir: Option<String>,
    /// 调试模式原始报文的保存目录（原始配置值；None = 未设置）
    debug_dir: Option<String>,
    /// 调试模式开关（设置页「通用 → 调试模式」）。
    ///
    /// 与保留期同一理由：转发层**每次发送前**都要判一次（改完开关下一个请求
    /// 就生效，不重启进程），从 `Value` 里翻一次要处理类型判定，解析一次存下来
    /// 最省事 —— 这条判定在转发热路径上。
    debug_mode: bool,
    /// 磁盘上那份 JSON 对象（含未知字段），写盘时的全量底稿
    raw: Map<String, Value>,
}

impl RuntimeConfig {
    /// 是否启用了鉴权：`apiKeys` 里有启用的 Key、或旧字段 / 环境变量给了 Key
    /// （多 Key 的解析见 `core::api_keys`）
    pub fn api_key_set(&self) -> bool {
        !self.active_api_keys().is_empty()
    }

    /// 当前**启用**的全部明文 Key（鉴权中间件逐把比对）；空 = 免鉴权
    pub fn active_api_keys(&self) -> Vec<String> {
        crate::server::core::api_keys::active_keys_from(&self.raw)
    }

    /// 计费接口语言（Accept-Language）
    pub fn locale(&self) -> &str {
        &self.locale
    }

    /// 默认模型
    pub fn default_model(&self) -> &str {
        &self.default_model
    }

    /// 最近一次实际转发的模型（账号页「模型」筛选的默认值）
    pub fn last_request_model(&self) -> Option<&str> {
        self.last_request_model.as_deref()
    }

    /// 调试模式是否开启（转发层每次发送前判一次，见字段说明）
    pub fn debug_mode(&self) -> bool {
        self.debug_mode
    }

    /// 掩码后的 API Key，格式照抄 server.mjs 920 行：前 6 后 4。
    /// 短 key 会前后重叠 —— Node 的 slice(0,6)/slice(-4) 也是这样，保持一致。
    pub fn masked_api_key(&self) -> Option<String> {
        let key = self.api_key.as_ref().filter(|key| !key.is_empty())?;
        let head: String = key.chars().take(6).collect();
        let total = key.chars().count();
        let tail: String = key.chars().skip(total.saturating_sub(4)).collect();
        Some(format!("{head}...{tail}"))
    }

    /// 原始 JSON 底稿（后续切片读自定义字段用）
    pub fn raw(&self) -> &Map<String, Value> {
        &self.raw
    }
}

/// 配置目录：复用壳侧实现，保证「壳读 key」与「服务端读 key」指向同一个目录
/// （唯一事实来源在 `crate::gateway::config_dir`，改路径只需改那一处）
pub fn config_dir() -> PathBuf {
    crate::gateway::config_dir()
}

/// config.json 的完整路径
pub fn config_file() -> PathBuf {
    config_dir().join("config.json")
}

/// 旧版配置目录名（仅用于一次性目录迁移）。
///
/// **这是全仓唯一一处允许出现 `.workbuddy-proxy` 字面量的地方** ——
/// 别处的路径一律走 `config_dir()`（事实来源在 `gateway::config_dir`），
/// 否则改名会出现两套口径。
const LEGACY_DIR_NAME: &str = ".workbuddy-proxy";

/// 旧版配置目录的完整路径（`{用户主目录}/.workbuddy-proxy`），供迁移使用。
pub(crate) fn legacy_config_dir() -> PathBuf {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(LEGACY_DIR_NAME)
}

// 迁移本身（唯一入口 `config_migration::migrate_config_dir`）在
// `server/config_migration.rs`；本模块只提供上面这个旧目录路径，
// 保证 `.workbuddy-proxy` 字面量全仓只有一处。

/// 环境变量里的 API Key（去空白，空串当未配置）
fn env_api_key() -> Option<String> {
    std::env::var("WORKBUDDY_PROXY_API_KEY")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// 环境变量里的非空字符串
fn env_text(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// 从原始 JSON 里取非空字符串字段
fn string_field(map: &Map<String, Value>, key: &str) -> Option<String> {
    map.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// 从原始 JSON 里取「带范围边界的整数」：缺字段 / 类型不符 / 非整数 / 越界
/// 一律回落到 `default`。
///
/// **越界回落而非夹紧**：手改 config.json 写了天文数字时，静默夹紧会让人
/// 以为「设置生效了」，回默认值至少与用户在设置页看到的不一致时好排查。
/// 走接口写入的值在各自的 PUT 处理器里已按同一范围校验过（那里是 400 报错）。
///
/// 接受 `30.0` 这种整值浮点（与 `parse_days` 同一口径）：JSON 里
/// `30` 与 `30.0` 都是合法数字，为后者回落到默认值会显得莫名其妙
/// （用户手改文件时把 30 写成 30.0 是完全可能的事）。
fn bounded_int_field(map: &Map<String, Value>, key: &str, default: i64, min: i64, max: i64) -> i64 {
    let parsed = map.get(key).and_then(|value| match value {
        Value::Number(number) => number.as_i64().or_else(|| {
            number
                .as_f64()
                .filter(|raw| raw.is_finite() && raw.fract() == 0.0)
                .map(|raw| raw as i64)
        }),
        _ => None,
    });
    parsed.filter(|value| (min..=max).contains(value)).unwrap_or(default)
}

/// 从原始 JSON 里取天数（`bounded_int_field` 的保留期特化）
fn days_field(map: &Map<String, Value>, key: &str, default: i64) -> i64 {
    bounded_int_field(map, key, default, RETENTION_MIN_DAYS, RETENTION_MAX_DAYS)
}

/// 由原始 JSON 解析三档保留天数（缺字段各自用默认值）
fn retention_from(map: &Map<String, Value>) -> RetentionSettings {
    let defaults = RetentionSettings::default();
    RetentionSettings {
        log_days: days_field(map, KEY_LOG_RETENTION_DAYS, defaults.log_days),
        request_days: days_field(map, KEY_REQUEST_RETENTION_DAYS, defaults.request_days),
        daily_days: days_field(map, KEY_DAILY_RETENTION_DAYS, defaults.daily_days),
    }
}

// ─── 间隔型定时任务的解析（scheduledTasks.*）──────────────────

/// 从 `scheduledTasks` 里取一条任务的原始子对象；缺失 / 类型不符当空对象
/// （于是 `enabled` 用默认值、`interval` 也用默认值，与「用户没配过」等价）。
fn task_object(map: &Map<String, Value>, key: &str) -> Map<String, Value> {
    map.get(KEY_SCHEDULED_TASKS)
        .and_then(Value::as_object)
        .and_then(|tasks| tasks.get(key))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

/// 从一条任务的子对象里取间隔值。
///
/// 与 `days_field` 同一口径（**越界回落而非夹紧**、接受整值浮点、只在
/// `min..=max` 内才采纳），只是范围由调用方给：两类任务的合理区间差着量级
/// （后端维护按分钟、前端刷新按秒），共用一份范围常量会逼其中一类放宽边界。
fn interval_field(task: &Map<String, Value>, default: i64, min: i64, max: i64) -> i64 {
    let parsed = task.get("interval").and_then(|value| match value {
        Value::Number(number) => number.as_i64().or_else(|| {
            number
                .as_f64()
                .filter(|raw| raw.is_finite() && raw.fract() == 0.0)
                .map(|raw| raw as i64)
        }),
        _ => None,
    });
    parsed.filter(|value| (min..=max).contains(value)).unwrap_or(default)
}

/// 从一条任务的子对象里取开关。
///
/// **缺字段按开启**：这些任务在本次改造前都是无条件运行的（凭证维护每 10 分钟、
/// 两个前端面板每 10 秒、模型刷新随 /v1/models 触发；定时查询积分是后来新增的，
/// 它没有「改造前」—— 缺字段同样按开启，与新装用户第一次打开的行为一致），
/// 升级上来的 config.json 里没有 `scheduledTasks` —— 若把「没配过」读成「关闭」，
/// 用户什么也没动，凭证却不再自动续期了。写侧（`PUT /api/scheduled-tasks`）
/// 则要求显式布尔值。
fn task_enabled(task: &Map<String, Value>, default: bool) -> bool {
    task.get("enabled").and_then(Value::as_bool).unwrap_or(default)
}

/// 由原始 JSON 解析六条间隔型任务（缺字段各自用默认值）
fn scheduled_from(map: &Map<String, Value>) -> ScheduledSettings {
    let defaults = ScheduledSettings::default();
    let task = |key: &str, interval: i64, min: i64, max: i64| {
        let object = task_object(map, key);
        IntervalTask {
            enabled: task_enabled(&object, true),
            interval: interval_field(&object, interval, min, max),
        }
    };
    ScheduledSettings {
        credential_maintenance: task(
            KEY_CREDENTIAL_MAINTENANCE,
            defaults.credential_maintenance.interval,
            INTERVAL_MIN_MINUTES,
            INTERVAL_MAX_MINUTES,
        ),
        model_refresh: task(
            KEY_MODEL_REFRESH,
            defaults.model_refresh.interval,
            INTERVAL_MIN_MINUTES,
            INTERVAL_MAX_MINUTES,
        ),
        logs_auto_refresh: task(
            KEY_LOGS_AUTO_REFRESH,
            defaults.logs_auto_refresh.interval,
            INTERVAL_MIN_SECONDS,
            INTERVAL_MAX_SECONDS,
        ),
        requests_auto_refresh: task(
            KEY_REQUESTS_AUTO_REFRESH,
            defaults.requests_auto_refresh.interval,
            INTERVAL_MIN_SECONDS,
            INTERVAL_MAX_SECONDS,
        ),
        report_auto_refresh: task(
            KEY_REPORT_AUTO_REFRESH,
            defaults.report_auto_refresh.interval,
            INTERVAL_MIN_SECONDS,
            INTERVAL_MAX_SECONDS,
        ),
        update_check: task(
            KEY_UPDATE_CHECK,
            defaults.update_check.interval,
            INTERVAL_MIN_MINUTES,
            INTERVAL_MAX_MINUTES,
        ),
        usage_query: task(
            KEY_USAGE_QUERY,
            defaults.usage_query.interval,
            INTERVAL_MIN_MINUTES,
            INTERVAL_MAX_MINUTES,
        ),
    }
}

// ─── 请求重试设置的解析（retryCount / retryCrossProviderCount / retryIntervalSeconds）──

/// 由原始 JSON 解析请求重试设置（缺字段各自用默认值）
fn retry_from(map: &Map<String, Value>) -> RetrySettings {
    let defaults = RetrySettings::default();
    RetrySettings {
        count: bounded_int_field(
            map,
            KEY_RETRY_COUNT,
            defaults.count,
            RETRY_MIN_COUNT,
            RETRY_MAX_COUNT,
        ),
        cross_provider_count: bounded_int_field(
            map,
            KEY_RETRY_CROSS_PROVIDER_COUNT,
            defaults.cross_provider_count,
            RETRY_MIN_COUNT,
            RETRY_MAX_COUNT,
        ),
        interval_seconds: bounded_int_field(
            map,
            KEY_RETRY_INTERVAL_SECONDS,
            defaults.interval_seconds,
            RETRY_MIN_INTERVAL_SECONDS,
            RETRY_MAX_INTERVAL_SECONDS,
        ),
    }
}

// ─── 历史路由优先级（providerRoute，只读，供账号迁移）───────────

/// 读磁盘上残留的 `providerRoute` 覆盖值：`[(providerId, 优先级)]`，只含文件里
/// 写了合法数字的那些键。账号存储把「按家分队」的旧号码合并成全局队列时，
/// 用它还原旧版的实际跨家顺序（缺省的家按注册表顺序，见 `store_admin`）。
///
/// 直接读盘而不走内存快照：这是启动期一次性的低频调用，且账号迁移可能早于
/// `config::init`，读盘最不依赖初始化顺序。
pub fn legacy_provider_route() -> Vec<(String, u32)> {
    let raw = read_raw();
    let Some(Value::Object(table)) = raw.get(KEY_PROVIDER_ROUTE) else {
        return Vec::new();
    };
    table
        .iter()
        .filter_map(|(id, value)| number_u32(value).map(|rank| (id.clone(), rank)))
        .collect()
}

/// JSON 值 → u32：数字（含整值浮点）/ 数字字符串，负数与非有限值不认。
fn number_u32(value: &Value) -> Option<u32> {
    let number = match value {
        Value::Number(number) => number.as_f64()?,
        Value::String(text) => text.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    if !number.is_finite() || number < 0.0 {
        return None;
    }
    Some(number.min(u32::MAX as f64) as u32)
}

/// 读磁盘上的 config.json（缺失/损坏都当空对象，对应 Node 版 catch 分支）
fn read_raw() -> Map<String, Value> {
    let Ok(text) = std::fs::read_to_string(config_file()) else {
        return Map::new();
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

/// 由磁盘内容 + 环境变量构造运行期配置（对应 Node 版 applyConfig 的优先级）
fn build(raw: Map<String, Value>) -> RuntimeConfig {
    let retention = retention_from(&raw);
    let scheduled = scheduled_from(&raw);
    let retry = retry_from(&raw);
    RuntimeConfig {
        // 文件里有就用文件的，否则环境变量兜底（对应 `if (config.apiKey && !opts.apiKey)`）
        api_key: string_field(&raw, "apiKey").or_else(env_api_key),
        locale: env_text("WORKBUDDY_LOCALE")
            .or_else(|| string_field(&raw, "locale"))
            .unwrap_or_else(|| DEFAULT_LOCALE.to_string()),
        default_model: env_text("WORKBUDDY_DEFAULT_MODEL").unwrap_or_else(|| DEFAULT_MODEL.to_string()),
        last_request_model: string_field(&raw, "lastRequestModel"),
        retention,
        scheduled,
        retry,
        log_dir: string_field(&raw, KEY_LOG_DIR),
        request_stats_dir: string_field(&raw, KEY_REQUEST_STATS_DIR),
        debug_dir: string_field(&raw, KEY_DEBUG_DIR),
        // 只有字面 `true` 算开启（手改文件写 "1" / "yes" 一律当关）：与
        // 「写坏回落」同一取向 —— 这个开关控制是否把凭据落盘，宁可少采
        debug_mode: raw.get(KEY_DEBUG_MODE).and_then(Value::as_bool).unwrap_or(false),
        raw,
    }
}

/// 进程内全局配置快照：所有模块共用，避免每个请求都读盘
static CONFIG: RwLock<Option<RuntimeConfig>> = RwLock::new(None);

/// 初始化全局配置（启动时调用一次；重复调用会重新读盘，幂等）
pub fn init() -> RuntimeConfig {
    let snapshot = build(read_raw());
    if let Ok(mut guard) = CONFIG.write() {
        *guard = Some(snapshot.clone());
    }
    snapshot
}

/// 读取当前生效配置的克隆。
///
/// 未初始化时按「空磁盘 + 环境变量」临时构造一份，保证任何初始化顺序都不会 panic。
/// 返回克隆而不是引用：避免调用方持有读锁跨越 await 与文件 IO。
pub fn current() -> RuntimeConfig {
    if let Ok(guard) = CONFIG.read() {
        if let Some(config) = guard.as_ref() {
            return config.clone();
        }
    }
    build(Map::new())
}

/// 只取保留期设置的轻量读取（**不克隆整份 raw**）。
///
/// 为什么不让调用方用 `current().retention()`：保留期是在**每次记账 / 写日志**
/// 上调用的（`RequestStats::record` → `retention_bounds`，`LogStore::append`
/// → 裁剪），而 `current()` 每次都会克隆整个 `raw` Map —— 热路径上没必要。
/// 这里只读锁取一个 `Copy` 值。
///
/// 读的是内存快照而不是磁盘：`update()` 落盘后会同步刷新快照，所以
/// 「设置页刚保存 → 下一次裁剪就用新值」成立，且不必每次读文件。
/// 未初始化（理论上只有启动极早期）时给默认值。
pub fn retention_settings() -> RetentionSettings {
    if let Ok(guard) = CONFIG.read() {
        if let Some(config) = guard.as_ref() {
            return config.retention;
        }
    }
    RetentionSettings::default()
}

/// 只取定时任务设置的轻量读取（**不克隆整份 raw**）。
///
/// 与 `retention_settings()` 同一取舍：凭证维护与模型刷新的循环**每一轮**都要
/// 问一次「现在开着吗、间隔多久」（这正是「改完设置下一轮生效」的实现方式），
/// 而 `current()` 每次都会克隆整个 `raw` Map —— 循环里没必要。
/// 读锁取一个 `Copy` 值即可。未初始化时给默认值。
pub fn scheduled_settings() -> ScheduledSettings {
    if let Ok(guard) = CONFIG.read() {
        if let Some(config) = guard.as_ref() {
            return config.scheduled;
        }
    }
    ScheduledSettings::default()
}

/// 只取请求重试设置的轻量读取（**不克隆整份 raw**）。
///
/// 与 `retention_settings()` 同一取舍：转发层每个失败请求都要问一次
/// 「还能重试几次、间隔多久」，而 `current()` 每次都会克隆整个 `raw` Map
/// —— 热路径上没必要。读锁取一个 `Copy` 值即可。未初始化时给默认值。
pub fn retry_settings() -> RetrySettings {
    if let Ok(guard) = CONFIG.read() {
        if let Some(config) = guard.as_ref() {
            return config.retry;
        }
    }
    RetrySettings::default()
}

/// 用一个变换函数原子地更新配置（读 → 改 → 落盘 → 回写内存）。
///
/// `mutate` 只改内存快照；落盘由本函数统一负责，避免两处都写文件。
fn update<F>(mutate: F) -> bool
where
    F: FnOnce(&mut RuntimeConfig),
{
    let mut next = current();
    mutate(&mut next);
    let saved = save_raw(&next.raw);
    if let Ok(mut guard) = CONFIG.write() {
        *guard = Some(next);
    }
    saved
}

/// 把整份原始 JSON 写盘（对应 Node 版 saveConfig）。
///
/// 目录不存在时自动创建；写失败只打控制台日志、不中断请求
/// （Node 版同样返回 false 让请求继续跑）。
pub fn save_raw(raw: &Map<String, Value>) -> bool {
    let dir = config_dir();
    if let Err(error) = std::fs::create_dir_all(&dir) {
        crate::server::logging::log("[Config]", &format!("❌ 创建配置目录失败: {error}"));
        return false;
    }
    // 缩进与 Node 版 JSON.stringify(config, null, 2) 一致，便于用户手改
    let text = match serde_json::to_string_pretty(&Value::Object(raw.clone())) {
        Ok(text) => text,
        Err(error) => {
            crate::server::logging::log("[Config]", &format!("❌ 保存失败: {error}"));
            return false;
        }
    };
    if let Err(error) = std::fs::write(config_file(), text) {
        crate::server::logging::log("[Config]", &format!("❌ 保存失败: {error}"));
        return false;
    }
    true
}

/// 设置 API Key：`None` 表示删除（对应 Node 版 `body.apiKey === null` 分支）。
/// 返回是否写盘成功；无论成功与否内存快照都已更新（本次运行立即生效）。
pub fn set_api_key(api_key: Option<String>) -> bool {
    update(|config| match api_key.clone() {
        Some(key) => {
            config.raw.insert("apiKey".to_string(), Value::String(key.clone()));
            config.api_key = Some(key);
        }
        None => {
            config.raw.remove("apiKey");
            config.api_key = None;
        }
    })
}

/// 整份替换 `apiKeys` 列表，并删掉旧的单 Key 字段 `apiKey`（从此只有一份真相；
/// 环境变量注入的 Key 不在文件里，`active_api_keys` 仍会把它算进去）。
pub fn replace_api_keys(list: Value) -> bool {
    update(move |config| {
        config
            .raw
            .insert(crate::server::core::api_keys::KEY_API_KEYS.to_string(), list.clone());
        config.raw.remove("apiKey");
        config.api_key = None;
    })
}

/// 更新语言（只接受非空字符串，对应 Node 版 `typeof body.locale === 'string' && body.locale`）
pub fn set_locale(locale: &str) -> bool {
    let locale = locale.to_string();
    update(|config| {
        config.raw.insert("locale".to_string(), Value::String(locale.clone()));
        config.locale = locale.clone();
    })
}

/// 记住本次请求用的模型（对应 Node 版 rememberRequestModel：仅在变化时写盘）
pub fn remember_request_model(model: &str) {
    let trimmed = model.trim();
    if trimmed.is_empty() || current().last_request_model.as_deref() == Some(trimmed) {
        return;
    }
    let value = trimmed.to_string();
    update(|config| {
        config
            .raw
            .insert("lastRequestModel".to_string(), Value::String(value.clone()));
        config.last_request_model = Some(value.clone());
    });
}

/// 写入 config.json 里任意字段（其他字段原样保留）。低频路径专用。
pub fn update_raw_field(key: &str, value: Value) -> bool {
    let key = key.to_string();
    update(move |config| {
        config.raw.insert(key.clone(), value.clone());
        // apiKey 属于「生效字段」，写它时要同步内存里的值
        if key == "apiKey" {
            config.api_key = string_field(&config.raw, "apiKey");
        }
    })
}

/// 更新三档保留天数（`None` = 该项不动），返回是否写盘成功。
///
/// 调用方（`stats_api::put_retention`）**必须先校验范围**：本函数按「已合法」
/// 处理，越界值会被 `days_field` 的回读逻辑丢弃（那会让用户以为设置生效了）。
///
/// 内存快照与 raw 底稿一起改（与 `set_api_key` 同一模式）：前者让下一次裁剪
/// 立刻用新值，后者保证写盘时不会把字段吃掉。无论写盘成功与否内存都已更新
/// （与其它 setter 一致），所以「设置页保存 → 立即清理」不依赖磁盘 IO。
pub fn set_retention(patch: RetentionPatch) -> bool {
    update(|config| {
        let mut next = config.retention;
        // 只写传进来的项：缺省项保持原值，也**不落盘**成默认值 ——
        // 否则「只改日志天数」会把另外两项一并固化成默认值，抹掉用户设置
        let mut apply = |key: &str, value: Option<i64>, slot: &mut i64| {
            if let Some(days) = value {
                config.raw.insert(key.to_string(), Value::from(days));
                *slot = days;
            }
        };
        apply(KEY_LOG_RETENTION_DAYS, patch.log_days, &mut next.log_days);
        apply(
            KEY_REQUEST_RETENTION_DAYS,
            patch.request_days,
            &mut next.request_days,
        );
        apply(KEY_DAILY_RETENTION_DAYS, patch.daily_days, &mut next.daily_days);
        config.retention = next;
    })
}

/// 更新一条间隔型任务（`None` = 该项不动），返回是否写盘成功。
///
/// `key` 必须是本模块的 `KEY_CREDENTIAL_MAINTENANCE` 等四个常量之一 ——
/// 它们是 `scheduledTasks` 下的子键，**不在这里做白名单校验**：调用方
/// （`scheduled_tasks::configure`）已经按任务 id 查过注册表，认不出的 id
/// 在那一层就被拒了。
///
/// 调用方**必须先校验间隔范围**（与 `set_retention` 同一约定）：本函数按
/// 「已合法」处理，越界值会被 `interval_field` 的回读逻辑丢弃。
///
/// 与 `set_retention` 同一模式：内存快照与 raw 底稿一起改 —— 前者让正在跑的
/// 循环下一轮就用新间隔（不必重启进程），后者保证写盘时不吃掉兄弟字段
/// （只改一条任务时，`scheduledTasks` 下其余各条必须原样保留）。
pub fn set_scheduled_task(
    key: &str,
    patch: IntervalTaskPatch,
    min: i64,
    max: i64,
) -> bool {
    let key = key.to_string();
    update(move |config| {
        // 先在 raw 里把这条任务的子对象取出来（不存在就建一个），再逐项写入。
        // 用 `entry` 形态而不是「重建整个 scheduledTasks」：后者会抹掉其它三条
        // 任务的设置，以及将来可能加进去的兄弟字段。
        let root = config
            .raw
            .entry(KEY_SCHEDULED_TASKS.to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if !root.is_object() {
            // 文件里被手改成了非对象（如字符串）：整块替换成对象。
            // 不静默忽略 —— 那会让保存「成功」但值没落盘，比覆盖更糟。
            *root = Value::Object(Map::new());
        }
        let Some(tasks) = root.as_object_mut() else {
            return;
        };
        let entry = tasks
            .entry(key.clone())
            .or_insert_with(|| Value::Object(Map::new()));
        if !entry.is_object() {
            *entry = Value::Object(Map::new());
        }
        let Some(task) = entry.as_object_mut() else {
            return;
        };
        if let Some(enabled) = patch.enabled {
            task.insert("enabled".to_string(), Value::Bool(enabled));
        }
        if let Some(interval) = patch.interval {
            // 写入前按范围收口：调用方已校验过，这里再夹一次只是防御
            //（手改文件与接口两条路径都不该把越界值落到盘上）
            task.insert(
                "interval".to_string(),
                Value::from(interval.clamp(min, max)),
            );
        }
        // 关键一步：重解析内存快照。**不能**只改 raw ——
        // 循环读的是 `scheduled_settings()` 里的解析结果，不同步刷新的后果是
        // 「界面上改完、循环还是按旧间隔跑」（且要等下次重启才生效），
        // 与保留期那套「改完立刻生效」的承诺不一致。
        config.scheduled = scheduled_from(&config.raw);
    })
}

/// 更新请求重试设置（`None` = 该项不动），返回是否写盘成功。
///
/// 调用方（`retry_api::put_retry`）**必须先校验范围**：本函数按「已合法」
/// 处理，越界值会被 `bounded_int_field` 的回读逻辑丢弃（那会让用户以为
/// 设置生效了）。
///
/// 与 `set_retention` 同一模式：内存快照与 raw 底稿一起改 —— 前者让下一个
/// 失败请求立刻用新值，后者保证写盘时不吃掉 config.json 里的其它字段。
pub fn set_retry(patch: RetryPatch) -> bool {
    update(|config| {
        let mut next = config.retry;
        if let Some(count) = patch.count {
            config.raw.insert(KEY_RETRY_COUNT.to_string(), Value::from(count));
            next.count = count;
        }
        if let Some(count) = patch.cross_provider_count {
            config
                .raw
                .insert(KEY_RETRY_CROSS_PROVIDER_COUNT.to_string(), Value::from(count));
            next.cross_provider_count = count;
        }
        if let Some(seconds) = patch.interval_seconds {
            config
                .raw
                .insert(KEY_RETRY_INTERVAL_SECONDS.to_string(), Value::from(seconds));
            next.interval_seconds = seconds;
        }
        config.retry = next;
    })
}

// ─── 调试模式（debugMode）────────────────────────────────────

/// 写入调试模式开关。
///
/// 与 `set_retry` 同一模式：内存快照与 raw 底稿一起改 —— 前者让下一个请求
/// 立刻用新值（转发层逐请求读快照），后者保证写盘时不吃掉 config.json 里的
/// 其它字段。返回是否落盘成功（失败时内存仍已更新，见调用点）。
pub fn set_debug_mode(enabled: bool) -> bool {
    update(|config| {
        config
            .raw
            .insert(KEY_DEBUG_MODE.to_string(), Value::Bool(enabled));
        config.debug_mode = enabled;
    })
}

// ─── 数据保存目录（logDir / requestStatsDir / debugDir）────────

/// 按当前快照解析出的三类数据保存目录（设置页「保存位置」与三个存储的启动用）。
#[derive(Clone, Debug)]
pub struct StorageDirs {
    /// 事件日志（logs.jsonl）的目录
    pub log_dir: PathBuf,
    /// 请求日志（requests.jsonl + request-daily.jsonl）的目录
    pub request_stats_dir: PathBuf,
    /// 调试模式原始报文（debug-traffic.jsonl）的目录
    pub debug_dir: PathBuf,
}

/// 解析三类数据的保存目录：配置里写了**绝对路径**就用它，否则回落配置目录。
///
/// 「写坏回落而不是报错」的取舍：目录是启动期就要用的值（两个存储的构造参数），
/// 这里报错只会让整个网关起不来 —— 而相对路径 / 空串最多是「手改文件写得不规范」，
/// 回落到默认目录是任何情况下都安全的行为。
pub fn storage_dirs() -> StorageDirs {
    let base = config_dir();
    // 只从内存快照读（`init` 之后才有意义；未初始化时回落默认 —— 与
    // `retention_settings()` 同一兜底取向）。三次读锁合成一次，减少锁往返。
    let (log_dir, request_stats_dir, debug_dir) = match CONFIG.read() {
        Ok(guard) => match guard.as_ref() {
            Some(config) => (
                resolve_dir(config.log_dir.as_deref(), &base),
                resolve_dir(config.request_stats_dir.as_deref(), &base),
                resolve_dir(config.debug_dir.as_deref(), &base),
            ),
            None => (base.clone(), base.clone(), base.clone()),
        },
        // 锁中毒：配置快照读不出来，回落默认目录（与各读取函数同一取向）
        Err(_) => (base.clone(), base.clone(), base.clone()),
    };
    StorageDirs {
        log_dir,
        request_stats_dir,
        debug_dir,
    }
}

/// 单个目录的解析：非空 + 绝对路径才算数，其余回落 `base`
fn resolve_dir(raw: Option<&str>, base: &Path) -> PathBuf {
    raw.map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| base.to_path_buf())
}

/// 写入 / 清除一类数据的保存目录。`dir` 为 None 或空串 = 清除（回到默认目录）。
///
/// 与 `set_retention` 同一套 `update` 原子写法：raw 与解析后的字段一起刷新，
/// 于是「保存成功 → `storage_dirs()` 立即读到新值」，不需要重启进程。
pub fn set_storage_dir(key: &str, dir: Option<&str>) -> bool {
    let key = key.to_string();
    let dir = dir
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty());
    update(move |config| {
        match &dir {
            Some(path) => {
                config.raw.insert(key.clone(), Value::String(path.clone()));
            }
            None => {
                config.raw.remove(&key);
            }
        }
        // 同步内存快照里的原始值（`storage_dirs` 读的是它，不是 raw）
        if key == KEY_LOG_DIR {
            config.log_dir = dir.clone();
        } else if key == KEY_REQUEST_STATS_DIR {
            config.request_stats_dir = dir.clone();
        } else if key == KEY_DEBUG_DIR {
            config.debug_dir = dir;
        }
    })
}
