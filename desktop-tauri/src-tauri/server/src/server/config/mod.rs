//! 网关自身配置的读写与运行时快照。
//!
//! 与 Node 版 `loadConfig` / `saveConfig` / `applyConfig` 对齐（server.mjs 242-264 行）。
//!
//! ── 持久化：统一库的 `kv` 表（本切片从 config.json 迁过来）────────
//! 改造前是一整份 `{config_dir}/config.json`；现在它的每个**顶层键就是 `kv`
//! 表的一行**（键名与配置文件里的一字不差），值是该键那段 JSON 文本。
//! 读是一次全取（跳过 `RESERVED_KV_KEYS` 里的固定键），写是一个事务里的
//! 「逐键 UPSERT + 删掉新配置里已没有的键」。旧文件由
//! `db::migrate::import_config` 一次性搬入。
//!
//! ── 「全量保留未知字段」这条不变量在库形态下怎么实现 ──────────
//! 改造前：整个 JSON 对象是**松散**的，模块刻意不做 struct 映射，底稿一律是
//! `serde_json::Map`；谁写盘都只能改自己那几项，写回时整份照写，**未知字段因此
//! 从结构上不可能丢**。
//! 进库后这条不变量**换了一种实现方式，但强度不变**：
//!   - 读侧取**所有**非保留键（不做白名单过滤）→ `raw` 里自然带着未知键；
//!   - 写侧按 `raw` 逐键 UPSERT → 未知键**原样写回**；
//!   - 唯一会删除的键是「不在 `raw` 里的那些」，而 `raw` 是读出来的底稿 ——
//!     所以「不认识的键」永远不会被删。
//! 也就是说：**未知字段的保留不再依赖「整份照写」这个动作，而依赖「读全量 +
//! 只删已知的缺失项」**。这条推理链是库形态下唯一保证，改动 `read_conn`
//! 的过滤条件或 `stale_keys` 的判据都会破坏它 —— 两处都有注释指向这里。
//! 一个具体的后果：`providerRoute` 这种**已下线但仍留在配置里**的历史键，
//! 依然会被读出来、写回去，不会因为代码里没人再用它就消失。
//!
//! ── 与固定 kv 键的关系（一条硬约束）─────────────────────────
//! 配置的顶层键**绝不能与 `db::schema::RESERVED_KV_KEYS` 相撞**：撞名的后果是
//! 配置写入把别人的状态当成「配置里已删掉的键」删掉。当前键集合逐项核对过
//! （结论与核对方法见 `db::schema` 模块头）。
//!
//! ── 运行期可变 ──────────────────────────────────────────────
//! Node 版改了配置直接改 `opts` 对象，请求热路径不再读盘；Rust 里用
//! `RwLock<Option<RuntimeConfig>>` 持有同一份「当前生效值」，于是
//! POST /api/config 改完 API Key 后鉴权中间件立即生效（不重启、不读库）。
//!
//! 优先级：配置里的值 > 环境变量 > 内置默认值。
//! 注意 `WORKBUDDY_PROXY_API_KEY` 是「启动时注入」语义 —— 它会写进内存快照，
//! 但 POST /api/config 传 null 可以把它清掉（对应 Node 版 `opts.apiKey = null`）。
//!
//! ── 启动期时序（本模块最容易读错的一处）─────────────────────
//! `init` 在 `bootstrap` 里**早于迁移**被调用，而且必须早：`db::migrate` 的
//! `import_logs` / `import_requests` / `import_debug` 要读 `storage_dirs()` 与
//! `retention_settings()`。但配置在库里、库要 `Db::open` 之后才存在 ——
//! `read_raw` 用「库优先、必要时回落旧文件」化解这个循环，完整论证见那里的注释。
//!
//! ── 文件布局 ────────────────────────────────────────────────
//! ```text
//! server/config/
//!   mod.rs    读写入口与内存快照（本文件）
//!   types.rs  键名常量、各设置类型、默认值与取值范围（对外契约）
//!   parse.rs  从 raw 底稿取值：类型判定、越界回落、环境变量兜底
//!   sql.rs    `kv` 表的行级读写（本模块唯一出现 SQL 的地方）
//! ```
//!
//! 配置目录本身（`~/.agent2api`）的事实来源在 `crate::paths::config_dir`，
//! 本模块只做转发；从 1.x 升级上来的一次性目录迁移在 `config_migration`，
//! 这里只保留旧目录名常量与旧目录路径访问器（`LEGACY_DIR_NAME` 仍是全仓
//! 唯一的字面量）。

use std::path::{Path, PathBuf};
use std::sync::RwLock;

use serde_json::{Map, Value};

use crate::server::db::Db;

pub(crate) mod parse;
pub(crate) mod sql;
mod types;

pub use types::*;

use parse::*;

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
    /// 上游请求的四项超时（设置页「通用 → 请求超时」区域）。
    ///
    /// 与 retry 同一理由：转发层**每次发送 / 每次分片**都要取它（改完设置
    /// 下一个请求就用新值，不重启进程），解析一次存下来最省事。
    timeouts: TimeoutSettings,
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
    /// 出站请求体黑名单指纹脱敏开关（设置页「通用 → 指纹脱敏」）。
    ///
    /// 与 `debug_mode` 同一理由：转发层**每次发送前**都要判一次（改完开关下一个
    /// 请求就生效，不重启进程），从 `Value` 里翻一次要处理类型判定，解析一次存
    /// 下来最省事 —— 这条判定在转发热路径上。
    sanitize_fingerprints: bool,
    /// 面板机器人校验开关（设置页「通用 → 机器人校验」，ALTCHA proof-of-work）。
    ///
    /// 与 `debug_mode` 同一理由：登录 / 注册端点逐请求判一次（改完开关下一个
    /// 请求就生效），解析一次存下来最省事。默认 `true`，见 `KEY_CAPTCHA_ENABLED`；
    /// 配置项缺失时可由环境变量 `AGENT2API_CAPTCHA_ENABLED` 兜底（默认 1 开、0 关）。
    captcha_enabled: bool,
    /// 系统提示词设置（设置页「通用 → 系统提示词」）。
    ///
    /// 与 `sanitize_fingerprints` 同一理由（转发层逐请求取一次，改完下一个请求
    /// 生效），另外多一件事：**提示词文件在构造快照时就读完**（见 `prompt_from`），
    /// 于是转发热路径上一次磁盘 IO 都没有；文件读不到时这里已经是「内置默认 +
    /// 一条原因」的形态，转发层不必再处理失败路径。
    prompt: PromptSettings,
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

    /// 出站指纹脱敏是否开启（转发层每次发送前判一次，见字段说明）
    pub fn sanitize_fingerprints(&self) -> bool {
        self.sanitize_fingerprints
    }

    /// 面板机器人校验开关（登录 / 注册端点逐请求判一次）。
    pub fn captcha_enabled(&self) -> bool {
        self.captcha_enabled
    }

    /// 系统提示词设置（界面 / 日志用；转发层要的是下面的借用视图）
    pub fn prompt_settings(&self) -> &PromptSettings {
        &self.prompt
    }

    /// 转发层要的**提示词决定**：模式 + 提示词文本的借用视图。
    ///
    /// 借用而不是克隆：文本可能有几百行，而本方法在**每个请求**上调用一次
    /// （`upstream::forward` 取快照），克隆一份纯属浪费。生命周期绑在
    /// `&self` 上 —— 调用方必须让快照活过整条转发链（见 `upstream::forward`
    /// 里那个 `config` 局部变量的说明）。
    pub fn prompt_plan(&self) -> crate::server::core::prompt::PromptPlan<'_> {
        crate::server::core::prompt::PromptPlan {
            mode: self.prompt.mode,
            text: &self.prompt.text,
            source: self.prompt.source,
        }
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

/// 配置目录：`paths::config_dir` 是唯一实现（桌面侧 `gateway::config_dir` 转发这里），
/// 「壳读 key」与「服务端读 key」仍指向同一个目录，改路径只需改 paths 一处。
pub fn config_dir() -> PathBuf {
    crate::paths::config_dir()
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

/// 读配置底稿：优先统一库，**必要时回落旧文件**。
///
/// ── 为什么需要回落（时序：先有鸡还是先有蛋）─────────────────
/// `config::init()` 在 `bootstrap` 里**早于** `Db::open` 被调用，而且它必须
/// 早：`db::migrate` 的 `import_logs` / `import_requests` / `import_debug`
/// 三项要读 `storage_dirs()`（旧文件的候选目录）与 `retention_settings()`
/// （日志裁剪天数），两者都来自配置快照。顺序反了，那三项就会拿默认目录去找
/// 旧文件（找不到 → 用户的日志永远留在旧文件里）并按默认 30 天裁一次日志
/// （**不可逆地把长保留期的旧日志裁掉**）。所以配置快照必须在迁移之前就位。
/// 但配置在库里，而库要 `Db::open` 之后才存在 —— 这就是那个循环。
///
/// 解法是**读路径分流**，不是改顺序：
///   - `db` 已装入 → 从库读（正常路径，唯一的真相来源）；
///   - `db` 未装入 / 库不可用 / **迁移还没跑过** → 回落读旧
///     `{config_dir}/config.json`，与改造前的行为逐字相同。
/// 第三条是关键：全新安装时库里没有配置，回落读到的是空对象（旧文件也不存在），
/// 与改造前一致；老用户升级后第一次启动时，回落读到的是他真正的旧配置 ——
/// 于是迁移项还没跑，快照里已经是正确的值，日志裁剪天数、旧文件目录全都对。
/// 迁移把旧文件搬进库并改名成 `.migrated` 之后，回落这一路自然失效
/// （文件没了），下次启动就走库那条路。
///
/// 为什么用「标记键」而不是「库里有没有配置键」判断要不要回落：运行期写一次
/// 配置就会让库里出现配置键，但**迁移还没跑**（旧文件里可能还有别的键没搬完）。
/// 标记键与迁移数据在同一个事务里落（见 `sql::import_conn`），所以「标记在」
/// 等价于「这份旧文件已经完整导入过」，是唯一可靠的判据。
fn read_raw() -> Map<String, Value> {
    if let Some(db) = db() {
        if let Some((raw, migrated)) = sql::read_snapshot(&db) {
            // 迁移未完成：库里可能只有运行期写的零星几项，其余仍在旧文件里，
            // 因此两份都要（旧文件里的项优先，它是用户完整的旧配置）。
            // 迁移完成后（标记在）就以库为唯一真相，不再看文件。
            if migrated {
                return raw;
            }
            return merge_legacy(raw);
        }
    }
    // 库读不出来（未装入 / 锁中毒 / SQL 出错）：回落旧文件，与改造前一致。
    // 不在这里报错：这个函数在启动极早期就会被调用，而调用方（`init`）
    // 已经能通过 `db()` 是否为空判断降级状态。
    read_legacy_file()
}

/// 读旧 `{config_dir}/config.json`（缺失 / 损坏 / 类型不符都当空对象，
/// 对应 Node 版 `loadConfig` 的 catch 分支）。
///
/// 只在两种情形被走到：`db` 未装入（启动极早期 / 库打不开），以及迁移尚未
/// 完成（此时它是用户完整旧配置的所在）。迁移完成后这个文件已被改名成
/// `.migrated`，本函数读不到东西 —— 那正是「不该再看它」的表达方式。
fn read_legacy_file() -> Map<String, Value> {
    let Ok(text) = std::fs::read_to_string(config_file()) else {
        return Map::new();
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

/// 合并「库里的项」与「旧文件里的项」：旧文件优先（迁移未完成时的过渡形态）。
///
/// 只有 `kv` 与旧文件同时有内容才会用到（迁移跑一半、或用户在迁移前就改过
/// 配置）。旧文件优先的理由：它是用户**完整**的那份配置，而库里的几项可能是
/// 运行期刚写进去的零散值；反过来的话，用户会发现改过的项被旧文件的值盖回去。
fn merge_legacy(store: Map<String, Value>) -> Map<String, Value> {
    let legacy = read_legacy_file();
    if legacy.is_empty() {
        return store;
    }
    let mut merged = store;
    for (key, value) in legacy {
        merged.insert(key, value);
    }
    merged
}

/// 由磁盘内容 + 环境变量构造运行期配置（对应 Node 版 applyConfig 的优先级）
fn build(raw: Map<String, Value>) -> RuntimeConfig {
    let retention = retention_from(&raw);
    let scheduled = scheduled_from(&raw);
    let retry = retry_from(&raw);
    let timeouts = timeouts_from(&raw);
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
        timeouts,
        log_dir: string_field(&raw, KEY_LOG_DIR),
        request_stats_dir: string_field(&raw, KEY_REQUEST_STATS_DIR),
        debug_dir: string_field(&raw, KEY_DEBUG_DIR),
        // 只有字面 `true` 算开启（手改文件写 "1" / "yes" 一律当关）：与
        // 「写坏回落」同一取向 —— 这个开关控制是否把凭据落盘，宁可少采
        debug_mode: raw.get(KEY_DEBUG_MODE).and_then(Value::as_bool).unwrap_or(false),
        // 只有字面 `false` 算关闭：**默认开**（缺失 → true）。这个开关是「要不要
        // 剥离会被上游误拦的模板句」，默认关会让新用户一上来就撞 400 code=11128
        // ——与 debug_mode 的「默认关」取向相反，因为两者的默认值代价不同。
        sanitize_fingerprints: raw
            .get(KEY_SANITIZE_FINGERPRINTS)
            .and_then(Value::as_bool)
            .unwrap_or(true),
        // 只有字面 `false` 算关闭：**默认开**。登录 / 注册的暴破与抢注防护
        // 宁可多一道不可少一道（见 KEY_CAPTCHA_ENABLED 的说明）。配置项缺失
        // 时环境变量兜底：登录页人机验证组件环境变量，默认为1开启，0为关闭
        // （见 env_captcha_enabled）
        captcha_enabled: raw
            .get(KEY_CAPTCHA_ENABLED)
            .and_then(Value::as_bool)
            .unwrap_or_else(env_captcha_enabled),
        // 系统提示词：模式非法/缺失 → passthrough（默认），文件读不到 → 内置默认
        // + 一条原因（见 `prompt_from`）
        prompt: prompt_from(&raw),
        raw,
    }
}

/// 从原始配置解析系统提示词设置（**在这里就把文件读完**，见字段说明）。
///
/// 口径与既有配置一致：
///   - 模式非法 / 缺失 → `passthrough`（默认）。手改文件写坏了**不报错**，
///     与「写坏回落」的既有取向相同；走接口的非法值由 `api::prompt` 拦成 400；
///   - `passthrough`：不读文件、不加载文本（`text` 为空串，转发层据此零开销透传）；
///   - `custom` / `append`：文件非空就读它；读失败（不存在 / 非 UTF-8 / 空文件）
///     回落**内置默认**并把原因记进 `file_error` —— 桌面应用不能因为一个提示词
///     文件启动不了、也不能因此拒绝转发，而原因会显示在设置页上，用户当场能改；
///   - 文件未指定 → 内置默认。
fn prompt_from(raw: &Map<String, Value>) -> PromptSettings {
    use crate::server::core::prompt::{PromptMode, PromptSource, BUILT_IN_PROMPT};

    let mode = raw
        .get(KEY_PROMPT_MODE)
        .and_then(Value::as_str)
        .and_then(PromptMode::parse)
        .unwrap_or_default();
    let file = string_field(raw, KEY_PROMPT_FILE);
    if !matches!(mode, PromptMode::Custom | PromptMode::Append) {
        return PromptSettings { mode, file, ..PromptSettings::default() };
    }
    let built_in = || PromptSettings {
        mode,
        file: file.clone(),
        text: BUILT_IN_PROMPT.to_string(),
        source: PromptSource::BuiltIn,
        file_error: None,
    };
    let Some(path) = file
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
    else {
        return built_in();
    };
    match read_prompt_file(path) {
        Ok(text) => PromptSettings {
            mode,
            file,
            text,
            source: PromptSource::File,
            file_error: None,
        },
        Err(error) => PromptSettings { file_error: Some(error), ..built_in() },
    }
}

/// 读提示词文件（UTF-8 文本）；`Err` 是**可直接显示给用户**的原因。
///
/// 空文件（或只有空白）算读失败：它在 `custom` 模式下等于「什么都不做」
/// （空文本被 `prompt::rewrite` 当成「不动」），静默失效比显式回落更坏 ——
/// 所以按「没配」处理，回落内置默认并说明原因。
pub fn read_prompt_file(path: &str) -> Result<String, String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err("提示词文件路径为空".to_string());
    }
    match std::fs::read_to_string(trimmed) {
        Ok(text) if text.trim().is_empty() => {
            Err(format!("提示词文件是空的：{trimmed}（已回落内置默认提示词）"))
        }
        Ok(text) => Ok(text),
        Err(error) => Err(format!("读不到提示词文件 {trimmed}: {error}")),
    }
}

/// 进程内全局配置快照：所有模块共用，避免每个请求都读盘
static CONFIG: RwLock<Option<RuntimeConfig>> = RwLock::new(None);

/// 初始化全局配置（启动时调用一次；重复调用会重新读库，幂等）。
///
/// ── 签名为什么从 `init()` 改成接 `Db` ───────────────────────
/// 配置的真相来源从 `config.json` 变成了统一库的 `kv` 表，路径这件事由 `Db`
/// 唯一持有（`Db::file()`）。与 `LogStore::with_db` / `RequestStats::with_db`
/// / `AccountStore::with_db` 同一形态，各 store 的构造
/// 方式保持一致；`Option<Db>` 也一致（库打不开时仍能构造，只是降级）。
///
/// 装入时机是 `bootstrap` 里的第一件事（`Db::open` 之后、迁移之前）——
/// 为什么必须早于迁移，见 `read_raw` 的论证（日志/统计/报文三项迁移依赖它）。
/// 装入时只读一次并构造快照；**迁移跑完不必重新装入**：迁移是「把旧文件里的
/// 东西搬进库」，而快照已经从旧文件读到了同样的值（`merge_legacy`），
/// 重装只是白读一遍库。
pub fn init(db: Option<Db>) -> RuntimeConfig {
    let db = db.filter(|db| {
        // 打不开的库句柄不该装进来（`Db` 只在打开成功时才有值，这里是防御）
        let _ = db;
        true
    });
    let _ = DB.set(db);
    let snapshot = build(read_raw());
    if let Ok(mut guard) = CONFIG.write() {
        *guard = Some(snapshot.clone());
    }
    snapshot
}

/// 重读配置并替换内存快照（迁移跑完之后由调用点补一次）。
///
/// 为什么需要它：`init` 在迁移前后读到的值可能是**两种形态**（迁移前从旧文件
/// 回落读、迁移后从库读）。两者在正常情况下等价（迁移就是把旧文件搬进库），
/// 但迁移可能**部分失败**（某个键写不进、事务回滚），此时库里的配置与快照
/// 会有差异。迁移结束时调用一次，让快照与库对齐 —— 否则本次运行会用一份
/// 「迁移想写但没写成」的值跑，而用户下次启动看到的又是另一份。
///
/// 只在迁移真的产生过结果时才由调用点触发（`db::migrate` 的返回非空）；
/// 没有任何旧文件时不必多读一次库。
pub fn reload() -> RuntimeConfig {
    let snapshot = build(read_raw());
    if let Ok(mut guard) = CONFIG.write() {
        *guard = Some(snapshot.clone());
    }
    snapshot
}

/// 进程级库句柄（与 `logging` / `core::debug_traffic` 同一模式）。
///
/// `OnceLock<Option<Db>>` 而不是 `OnceLock<Db>`：`Db::open` 可能失败，而
/// `init` 的签名要接 `Option<Db>`（调用点手里就是它）—— 包在 `Option` 里让
/// 「未初始化」与「初始化了但库不可用」共用同一条取值路径（都是 `None`）。
static DB: std::sync::OnceLock<Option<Db>> = std::sync::OnceLock::new();

/// 进程级库句柄（未初始化或库不可用时为 `None`）
fn db() -> Option<Db> {
    DB.get().and_then(|slot| slot.as_ref()).cloned()
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
/// —— 热路径上没必要。读锁取一份快照克隆（错误码名单是 `Arc`，
/// 克隆只付一次指针自增）。未初始化时给默认值。
pub fn retry_settings() -> RetrySettings {
    if let Ok(guard) = CONFIG.read() {
        if let Some(config) = guard.as_ref() {
            return config.retry.clone();
        }
    }
    RetrySettings::default()
}

/// 用一个变换函数原子地更新配置（读 → 改 → 落库 → 回写内存）。
///
/// `mutate` 只改内存快照；落库由本函数统一负责，避免两处都写库。
///
/// 「原子」有两层：内存快照的替换是一次写锁（`CONFIG.write()`），而库那一侧
/// 由 `sql::write_all` 的一个事务保证（多行 UPSERT + 删除要么全成、要么全不成）。
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

/// 把整份配置写进统一库（对应 Node 版 `saveConfig`）。
///
/// ── 与文件形态的语义差异（必须看清）─────────────────────────
/// 改造前这是「整文件覆盖」：`raw` 里没有的键自然就不在文件里了，于是
/// 「删掉一个配置项」（`set_api_key(None)` 会 `raw.remove`）能生效。
/// 进库后行是**逐键**存在的，如果只做 UPSERT，被删掉的键会留在库里 ——
/// 用户下次启动会看到刚清掉的那项又回来了。所以
/// `sql::write_all` 在同一个事务里做「逐键 UPSERT + 删掉不在 `raw` 里的配置键」，
/// 删除范围**排除** `db::schema::RESERVED_KV_KEYS`（那些是别的模块的状态）。
///
/// ── 为什么成功与否都要让内存快照生效 ────────────────────────
/// 本函数只负责落库；内存快照由调用方（`update`）在之后统一刷新。写失败
/// （库不可用、事务回滚）返回 `false`，但**本次运行内的改动仍然生效** ——
/// 与改造前「写文件失败只打日志、不中断请求」一致：失败影响的只是
/// 「重启后还在不在」，不该让正在跑的请求或被改的配置报错。
///
/// ── 日志为什么在 {@link sql::write_all} 之外打 ──────────────
/// `Db::with` 持的是**全局唯一那把连接锁**，而 `logging::log` 的入库那一路
/// 要往同一个库写 `logs` 表 —— `std::sync::Mutex` 不可重入，在闭包里打日志
/// 等于当场死锁。所以 `write_all` 只把 `Result` 交出来，这里在锁外记
/// （与日志库的处置相同）。
///
/// 注意本函数**在日志库初始化之前就可能被调用**（`bootstrap` 里
/// `config::init` 早于 `logging::init_store`）：那时 `logging::log` 的入库
/// 那一路会静默丢弃，只剩控制台 —— 这也正是它该用的通道（`console_line`
/// 与 `log` 的控制台部分是同一个输出）。
pub fn save_raw(raw: &Map<String, Value>) -> bool {
    let Some(db) = db() else {
        crate::server::logging::log(
            "[Config]",
            "❌ 保存失败: 数据库不可用（本次改动仅内存生效）",
        );
        return false;
    };
    match sql::write_all(&db, raw) {
        Ok(()) => true,
        Err(error) => {
            crate::server::logging::log("[Config]", &format!("❌ 保存失败: {error}"));
            false
        }
    }
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
        let mut next = config.retry.clone();
        if let Some(count) = patch.count {
            config.raw.insert(KEY_RETRY_COUNT.to_string(), Value::from(count));
            next.count = count;
        }
        if let Some(count) = patch.account_switch_count {
            config
                .raw
                .insert(KEY_RETRY_ACCOUNT_SWITCH_COUNT.to_string(), Value::from(count));
            next.account_switch_count = count;
        }
        if let Some(seconds) = patch.interval_seconds {
            config
                .raw
                .insert(KEY_RETRY_INTERVAL_SECONDS.to_string(), Value::from(seconds));
            next.interval_seconds = seconds;
        }
        if let Some(codes) = &patch.no_retry_codes {
            // raw 底稿存「与 API 契约同形」的整数数组（写侧已校验排序去重，
            // 这里原样落库）；内存快照换成新的共享切片
            let array: Vec<Value> = codes.iter().map(|code| Value::from(*code)).collect();
            config
                .raw
                .insert(KEY_RETRY_NO_RETRY_CODES.to_string(), Value::Array(array));
            next.no_retry_codes = std::sync::Arc::from(codes.as_slice());
        }
        config.retry = next;
    })
}

// ─── 上游请求超时（connectTimeoutSeconds / headersTimeoutSeconds /
//     streamIdleTimeoutSeconds / bodyTimeoutSeconds）──────────────────

/// 只取四项超时的轻量读取（**不克隆整份 raw**）。
///
/// 与 `retry_settings()` 同一取舍：转发层每次发送、每个分片都要取一次，
/// 读锁取一份 `Copy` 快照即可。未初始化时给默认值。
pub fn timeout_settings() -> TimeoutSettings {
    if let Ok(guard) = CONFIG.read() {
        if let Some(config) = guard.as_ref() {
            return config.timeouts;
        }
    }
    TimeoutSettings::default()
}

/// 更新四项超时（`None` = 该项不动），返回是否写盘成功。
///
/// 调用方（`timeouts_api::put_timeouts`）**必须先校验范围**：本函数按
/// 「已合法」处理，越界值会被 `bounded_int_field` 的回读逻辑丢弃。
/// 与 `set_retry` 同一模式：内存快照与 raw 底稿一起改。
pub fn set_timeouts(patch: TimeoutPatch) -> bool {
    update(|config| {
        let mut next = config.timeouts;
        let mut write = |key: &str, value: i64, slot: &mut i64| {
            config.raw.insert(key.to_string(), Value::from(value));
            *slot = value;
        };
        if let Some(seconds) = patch.connect_seconds {
            write(KEY_TIMEOUT_CONNECT_SECONDS, seconds, &mut next.connect_seconds);
        }
        if let Some(seconds) = patch.headers_seconds {
            write(KEY_TIMEOUT_HEADERS_SECONDS, seconds, &mut next.headers_seconds);
        }
        if let Some(seconds) = patch.stream_idle_seconds {
            write(
                KEY_TIMEOUT_STREAM_IDLE_SECONDS,
                seconds,
                &mut next.stream_idle_seconds,
            );
        }
        if let Some(seconds) = patch.body_seconds {
            write(KEY_TIMEOUT_BODY_SECONDS, seconds, &mut next.body_seconds);
        }
        config.timeouts = next;
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

// ─── 出站指纹脱敏（sanitizeBlacklistFingerprints）─────────────

/// 写入出站指纹脱敏开关。
///
/// 与 `set_debug_mode` 同一模式：内存快照与 raw 底稿一起改 —— 前者让下一个请求
/// 立刻用新值（转发层逐请求读快照），后者保证写盘时不吃掉 config.json 里的
/// 其它字段。返回是否落盘成功（失败时内存仍已更新，见调用点）。
pub fn set_sanitize_fingerprints(enabled: bool) -> bool {
    update(|config| {
        config
            .raw
            .insert(KEY_SANITIZE_FINGERPRINTS.to_string(), Value::Bool(enabled));
        config.sanitize_fingerprints = enabled;
    })
}

/// 写入机器人校验开关（设置页「通用 → 机器人校验」）。
///
/// 与 `set_sanitize_fingerprints` 同一模式：内存立即生效（登录 / 注册端点
/// 逐请求读快照），写盘时不吃掉 config.json 里的其它字段。
pub fn set_captcha_enabled(enabled: bool) -> bool {
    update(|config| {
        config
            .raw
            .insert(KEY_CAPTCHA_ENABLED.to_string(), Value::Bool(enabled));
        config.captcha_enabled = enabled;
    })
}

// ─── 系统提示词（promptMode / promptFile）───────────────────────

/// 写入系统提示词设置（模式 + 文件），并**按新值重新解析一遍生效文本**。
///
/// 与 `set_sanitize_fingerprints` 同一模式，但多一步：写完两个键之后重跑一次
/// [`prompt_from`]。只改字段不重解析会出现「模式换了、`text` 还是上一份」的
/// 静默错配（`custom` 模式却拿着空文本 = 什么都不做），而重解析顺带把
/// 「文件此刻读不到」的原因一起刷新 —— 用户改完路径立刻能在界面上看到结果。
///
/// `file` 为空串/None 时**删掉这个键**（而不是写空串）：与 `set_api_key(None)`
/// 同一语义，配置里不留下没意义的空值。
///
/// 返回是否落盘成功（失败时内存仍已更新，见调用点）。
pub fn set_prompt(mode: crate::server::core::prompt::PromptMode, file: Option<String>) -> bool {
    update(|config| {
        config
            .raw
            .insert(KEY_PROMPT_MODE.to_string(), Value::String(mode.as_str().to_string()));
        match file.filter(|text| !text.trim().is_empty()) {
            Some(path) => {
                config
                    .raw
                    .insert(KEY_PROMPT_FILE.to_string(), Value::String(path));
            }
            None => {
                config.raw.remove(KEY_PROMPT_FILE);
            }
        }
        config.prompt = prompt_from(&config.raw);
    })
}

// ─── 数据保存目录（logDir / requestStatsDir / debugDir）────────
//
// ── 为什么这里**只剩读**，没有对应的写函数（T8 收尾的一处）──────
// 三个键在数据全部进统一库之后**失去了消费方**：日志、请求统计、调试报文都
// 落在 `{config_dir}/agent2api.db` 里，不再有「各自的保存目录」。曾经的那个
// 写入口 `set_storage_dir(key, dir)` 是给设置页三个「更改…」按钮用的，
// 而它现在**一个调用方都没有**（那三个按钮已随单页改成只读而删除），
// 于是整体删掉 —— 留一个没人调用的写函数，下次有人读到它只会以为
// 「保存位置还能改」，然后发现改完什么都不会发生。
//
// 那为什么 `storage_dirs()` 与三个键常量**必须保留**：它们还有真实消费者，
// 而且是**迁移项** —— `db::migrate` 的 `logs` / `requests` / `debug` 三项要
// 用 `storage_dirs()` 去**用户当年自定义过的目录**里找旧文件（见那三项的
// `legacy_file`）。用户升级前的 `logDir: "D:\\logs"` 正是那批历史数据的所在，
// 少了这条读取，那三个目录里的旧数据就永远找不到（迁移会以为「没有旧文件」，
// 而用户会在界面上看到「已升级」却发现日志全空了）。
//
// 一句话总结这个不对称：**读是历史数据的入口，写是已经不存在的功能**。
// 将来若真的支持「更换数据库位置」，那应该是「新建库 + 导入 + 切配置目录」，
// 而不是把某一个键改个值 —— 所以那时也不该把这个写函数拿回来。

/// 按当前快照解析出的三类数据保存目录（**只服务迁移项找旧文件**）。
#[derive(Clone, Debug)]
pub struct StorageDirs {
    /// 事件日志旧文件（`logs.jsonl`）的目录
    pub log_dir: PathBuf,
    /// 请求日志旧文件（`requests.jsonl` + `request-daily.jsonl`）的目录
    pub request_stats_dir: PathBuf,
    /// 调试模式原始报文旧文件（`debug-traffic.jsonl`）的目录
    pub debug_dir: PathBuf,
}

/// 解析三类数据的保存目录：配置里写了**绝对路径**就用它，否则回落配置目录。
///
/// 「写坏回落而不是报错」的取舍：目录是启动期就要用的值（迁移项找旧文件的
/// 候选目录），这里报错只会让迁移整批跳过 —— 而相对路径 / 空串最多是
/// 「手改文件写得不规范」，回落到默认目录是任何情况下都安全的行为。
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
