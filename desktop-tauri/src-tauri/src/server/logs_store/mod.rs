//! 运行日志存储（SQLite 的 `logs` 表；本切片从 `logs.jsonl` 迁过来）。
//!
//! 对照 Node 版 `src/workbuddy-logs.mjs` 逐条实现**行为**，但持久化形态换了：
//! Node 版是「一行一条 JSON 追加写 + 启动时载入内存」；这里是
//! `{config_dir}/agent2api.db` 的 `logs` 表（表结构见 `server/db/schema.rs`），
//! 库就是唯一真相，不再有内存快照。
//!
//! ── 「内存快照 + 文件镜像」这套机制为什么整体消失 ─────────────
//! 改造前 `Inner` 持一份 `Vec<LogEntry>` 当**有效全集**，文件只是它的落盘镜像。
//! 为了让两者一致，需要一整套约定：
//!   - `dirty`：内存被裁过（比文件少），下次写入要整文件收敛；
//!   - `appends_since_compact` + `COMPACT_STEP = 100`：满额后攒够一批追加才
//!     整文件重写，避免每写一条就全量落盘；
//!   - `load()` 载入后要按两个保留约束裁一遍，**并算出** `dirty` 与 `next_id`。
//! 这些全是「两份状态要手动对齐」的产物，而它们的 bug 面很宽（少标一次 dirty
//! 就会让已裁掉的行在重启后回来）。进数据库后：
//!   - 一份状态：`SELECT` 的结果就是全集，没有镜像要对齐；
//!   - `append` = 一条 `INSERT`（不再有「追加一行还是整文件重写」的分支）；
//!   - 裁剪 = 一条 `DELETE`（不再需要 dirty 记「文件比内存多」）；
//!   - `next_id` 不再靠 `load()` 扫最大值推导（见下面 `id` 一节）。
//! 上述三个字段与 `COMPACT_STEP` 因此**全部删除**。
//!
//! ── `MAX_ENTRIES = 500` 保留（它是产品语义）────────────────────
//! 容量上限不是文件机制的副产物，而是「日志页与导航徽标只关心最近 500 条」
//! 这条产品约定，Node 版与改造前都有。它现在表现为一条 `DELETE`（见
//! `sql::trim_to_capacity`），语义与旧实现的 `drain(0..溢出)` 逐字相同。
//!
//! ── 两个保留约束（取更严的那个）─────────────────────────────
//!   - **时间维度**：可配置的保留天数（默认 30 天，见
//!     `config::DEFAULT_LOG_RETENTION_DAYS`），管「最多多久」；
//!   - **容量维度**：`MAX_ENTRIES = 500`，管「最多多少条」。
//! 天数由外部回调提供（构造函数的第二个参数），**每次裁剪时动态取** ——
//! 于是设置页改完天数下一次写入就生效，不需要重启进程；回调读的是
//! `config::retention_settings()` 的内存快照（不读盘）。这条机制**保持不变**，
//! 只是裁剪动作从「内存 retain」变成 SQL `DELETE`。
//!
//! ── `id`：单调递增，且**只有 `clear()` 会回退**（本切片最需要小心的地方）──
//! `id` 的单调性是**有消费方依赖的不变量**，不是实现细节：
//! 导航徽标把 `stats.lastId` 存成已读水位（localStorage），再用
//! `GET /api/logs?level=error&sinceId=<水位>` 数未读错误（`ui/app.js` 的
//! `updateLogsBadge` / `refreshUnreadErrors`）；前端还有一条「`lastId < 水位`
//! ⇒ 日志被清空过，水位回落」的分支。
//!
//! 所以：
//!   - **`clear_where` / `prune` / 容量裁剪不回退** —— 删中间（甚至删掉最新那条）
//!     都不能让后续 id 与旧水位相等，否则 `id > 水位` 永远筛不到新日志；
//!   - **`clear()` 回退到 1** —— 与改造前逐字一致，且前端那条「水位回落」分支
//!     正是为它写的（`ui/app.js`：「日志被清空（id 重新从 1 数起）：水位必须
//!     跟着回落，否则新日志的 id 永远小于水位，徽标从此不再出现」）。
//! 具体怎么保证单调（为什么不能靠 rowid 自增、计数器放在哪、怎么自愈）见
//! `sql.rs` 模块头 —— 那里有实测结论：**rowid 自增会复用被删掉的号**。
//!
//! 并发模型：与改造前一样一把 `Mutex` 串行化整个操作，但它保护的东西变了 ——
//! 以前是「内存快照 + 文件」，现在是「数据库上的一次读改写周期」。注意真正的
//! 串行化由 `Db` 内部那把锁提供（每个操作在一次 `Db::with` 调用里跑完），
//! 本层的锁只是让「读-判断-写」这个周期对 `LogStore` 的调用方原子。
//!
//! 子模块：`sql.rs` 行级 SQL 访问层（本模块唯一出现 SQL 的地方）。
//!
//! JSON 字段名与 Node 版一致：`id/ts/level/category/message/data`。

mod sql;

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, RwLock};

use chrono::{Duration as ChronoDuration, Local, TimeZone};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::server::config;
use crate::server::db::Db;

/// 日志表的行数上限（环形容量，超出丢最旧的）。
///
/// 名字里的 `ENTRIES` 沿用 Node 版 `MAX_ENTRIES` 的对外命名（`logs_api` 把它
/// 当响应里的 `max` 字段透出），语义从「内存保留条数」变成「表里的行数上限」。
pub const MAX_ENTRIES: usize = 500;
/// 日志级别，数值越大越严重（用于「该级别及以上」筛选）
pub const LEVELS: [&str; 4] = ["debug", "info", "warn", "error"];
/// 分类字典：与桌面端筛选下拉一致（key → 中文标签）。
/// checkin / maintenance / update 是定时任务三分类（自动签到 / 凭证自动维护 /
/// 软件版本检查），只对登记映射之后落库的条目生效 —— 历史条目不迁移。
pub const CATEGORIES: [(&str, &str); 10] = [
    ("server", "服务"),
    ("auth", "登录"),
    ("account", "账号"),
    ("model", "模型"),
    ("upstream", "上游"),
    ("desensitize", "脱敏"),
    ("config", "配置"),
    ("checkin", "自动签到"),
    ("maintenance", "凭证自动维护"),
    ("update", "软件版本检查"),
];

const MAX_MESSAGE_LENGTH: usize = 1000;
const MAX_DATA_KEYS: usize = 24;

/// 一条日志。字段名用缩写 `ts` 与 Node 版 JSONL 完全对齐，
/// 前端 logs-panel.js 读的是 `entry.ts` / `entry.level` / `entry.message`。
///
/// **形状一字未改**：它仍是 `to_jsonl` 的序列化单元（导出格式不变），
/// 也仍是查询结果的元素类型。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LogEntry {
    pub id: u64,
    pub ts: i64,
    pub level: String,
    pub category: String,
    pub message: String,
    #[serde(default)]
    pub data: Option<Value>,
}

/// 查询结果：`entries` 倒序（最新在前），`total` 为库里总量，`matched` 为过滤后数量
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryResult {
    pub entries: Vec<LogEntry>,
    pub total: usize,
    pub matched: usize,
}

/// 统计结果：各级别/分类计数（导航徽标与筛选下拉用）
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Stats {
    pub total: usize,
    pub max: usize,
    pub by_level: Map<String, Value>,
    pub by_category: Map<String, Value>,
    pub last_id: u64,
    pub file: String,
}

/// 查询条件（对应 workbuddy-log-routes.mjs 传给 logStore.query 的参数）
#[derive(Clone, Debug, Default)]
pub struct Query {
    pub limit: Option<usize>,
    /// 级别下限：debug < info < warn < error，选中级别及以上都返回
    pub level: Option<String>,
    pub category: Option<String>,
    pub keyword: Option<String>,
    pub since_id: Option<u64>,
    /// 起始毫秒时间戳（含）。None 不过滤 —— 这两个字段是**新增的可选**条件，
    /// 不传时过滤链与以前完全一致（向后兼容）
    pub start: Option<i64>,
    /// 结束毫秒时间戳（**不含**）。闭开区间 `[start, end)` 的取舍同
    /// `RequestQuery`：翻页时「上一页最后一条的 ts」可直接当下页的 end，
    /// 不会把同一条重复取到两次
    pub end: Option<i64>,
}

/// 级别归一：未知值一律 info（对应 Node 版 normalizeLevel）
///
/// `pub(crate)`：`db::migrate::import_logs` 要按**同一套口径**解析旧文件
/// （否则同一个 level 经迁移落库与经运行期写入会是两个值），共用一份实现。
pub(crate) fn normalize_level(value: &str) -> String {
    let lower = value.trim().to_lowercase();
    if LEVELS.contains(&lower.as_str()) {
        lower
    } else {
        "info".to_string()
    }
}

/// 级别序号，用于「该级别及以上」筛选；未知级别按 info
pub(crate) fn level_rank(level: &str) -> usize {
    LEVELS.iter().position(|item| *item == level).unwrap_or(1)
}

/// 分类归一：未知值一律 server（对应 Node 版 normalizeCategory）
///
/// `pub(crate)`：理由同 `normalize_level`（迁移项共用同一口径）。
pub(crate) fn normalize_category(value: &str) -> String {
    let lower = value.trim().to_lowercase();
    if CATEGORIES.iter().any(|(key, _)| *key == lower) {
        lower
    } else {
        "server".to_string()
    }
}

/// 附加数据裁剪：只留可序列化的浅层键值，避免把整个请求体塞进日志。
/// 对应 Node 版 normalizeData —— 非对象返回 None，最多保留 24 个键，
/// 字符串超长截断（Node 版在末尾追加省略号）。
///
/// `pub(crate)`：迁移项共用（理由同 `normalize_level`）。
pub(crate) fn normalize_data(value: Option<&Value>) -> Option<Value> {
    let Value::Object(map) = value? else {
        return None;
    };
    let mut out = Map::new();
    for (key, item) in map.iter() {
        if out.len() >= MAX_DATA_KEYS {
            break;
        }
        // Node 版只跳过 undefined（JSON 里不存在该形态），null 照原样保留
        let trimmed = match item {
            Value::String(text) if text.chars().count() > MAX_MESSAGE_LENGTH => {
                let head: String = text.chars().take(MAX_MESSAGE_LENGTH).collect();
                Value::String(format!("{head}…"))
            }
            other => other.clone(),
        };
        out.insert(key.clone(), trimmed);
    }
    if out.is_empty() {
        None
    } else {
        Some(Value::Object(out))
    }
}

/// 把消息里的连续空白压成单个空格再 trim（对应 Node 版 `.replace(/\s+/g, ' ')`）
///
/// `pub(crate)`：迁移项共用（理由同 `normalize_level`）。
pub(crate) fn squeeze_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !in_space {
                out.push(' ');
                in_space = true;
            }
        } else {
            out.push(ch);
            in_space = false;
        }
    }
    out.trim().to_string()
}

/// 截断超长消息（字符数，避免按字节切坏 UTF-8）
///
/// `pub(crate)`：迁移项共用（理由同 `normalize_level`）。
pub(crate) fn clamp_message(text: &str) -> String {
    if text.chars().count() <= MAX_MESSAGE_LENGTH {
        return text.to_string();
    }
    text.chars().take(MAX_MESSAGE_LENGTH).collect()
}

/// 把 `kv` 里的「下一个日志 id」高水位抬到至少 `value`。
///
/// `pub(crate)` 且**只给迁移项用**（`db::migrate::import_logs`）：整批导入
/// 历史行之后必须把高水位推到 `MAX(id) + 1`，否则运行期的 `append` 会从 1 开始
/// 分配 id、撞上历史行的主键（那条新日志会静默丢失，见迁移项的说明）。
/// 运行期自己不需要它 —— `allocate_next_id` 每次都会同时考虑计数器与
/// `MAX(id)`，本来就自愈。
pub(crate) fn sql_raise_next_id(
    conn: &rusqlite::Connection,
    value: u64,
) -> rusqlite::Result<()> {
    sql::raise_next_id(conn, value)
}

/// 日志存储本体。所有公开方法都取 `&self`，内部 `Mutex` 串行化。
pub struct LogStore {
    /// SQLite 句柄。`None` = 数据库打开失败（启动时已记日志）。
    ///
    /// ── 为什么不做「文件回退」─────────────────────────────────
    /// 与 `AccountStore` 的选择一致（见那边的 `Inner::db` 注释），理由在日志上
    /// 更直白：**日志是所有 store 里最不该有第二套落地路径的一个**。它的写入
    /// 发生在每个请求的路径上（`logging::log` 被转发链路、账号刷新、定时任务
    /// 都调用），回退实现会变成一个高频执行、却只在数据库坏掉时才第一次运行的
    /// 分支 —— 那是最坏的一类「没人测过的代码」。
    /// 而日志的可接受降级非常明确：**丢一条日志不影响任何业务**。所以库不可用
    /// 时按方法各自的合理空值回落（见每个方法的说明），不写第二个文件。
    db: Option<Db>,
    /// 库文件路径（供 `stats().file` 展示）。
    ///
    /// 取自 `Db::file()`，语义是「装着日志数据的库文件」—— 与 T2 对
    /// `AccountStore::file()` 的处理一致。库打不开时回落到约定路径
    /// （`{config_dir}/agent2api.db`）：这个值会显示在日志页的「文件」一栏
    /// （`ui/logs-panel.js:268`），给一个有意义的位置比给空串更有用。
    ///
    /// 为什么是 `RwLock` 而不是裸字段：字段本身不再被任何写路径改动
    /// （`relocate` 已随单库语义删除，见 `file()` 的说明），用 `RwLock` 是为了让
    /// 「读现值」这件事与改造前同形，改动面最小。
    file_path: RwLock<PathBuf>,
    /// 保留天数回调。**每次裁剪时动态调用**，于是设置改完天数下一次写入
    /// 就用新值，不需要重启进程（与 `RequestStats::get_retention` 同一模式）。
    /// 用 `Arc` 是为了让 `LogStore` 本身仍能放进 `OnceLock`（不需要 `&self`
    /// 生命周期）。
    get_retention_days: Arc<dyn Fn() -> i64 + Send + Sync>,
    /// 串行化「读-判断-写」这个周期。
    ///
    /// 真正把语句串起来的是 `Db` 内部那把锁（每个操作在一次 `Db::with` 调用里
    /// 跑完）；本锁让「先查总数再决定插/裁」这类多步逻辑对 `LogStore` 的调用方
    /// 保持原子 —— 与改造前那把锁保护的**周期**相同，只是保护的对象从
    /// 「内存 + 文件」换成了「数据库访问」。
    inner: Mutex<()>,
}

impl LogStore {
    /// 构造存储句柄（库不可用时降级为空操作，见 `db` 字段的说明）。
    ///
    /// ── 签名为什么从 `new(directory, ...)` 改成接 `Db` ──────────
    /// 日志不再有「自己的目录」：数据在统一库的 `logs` 表里，路径这件事由
    /// `Db` 唯一持有（`Db::file()`）。与 T2 的 `AccountStore::with_db` 同一形态，
    /// 两个 store 的构造方式保持一致；`Option<Db>` 也一致（库打不开时仍能构造，
    /// 只是降级）。
    ///
    /// ── 为什么不再有 `load()` ──────────────────────────────────
    /// 改造前构造时要读文件、按保留期裁一遍、算 `next_id` 并标 `dirty`。
    /// 现在库就是全集，没有可载入的东西；「启动时裁一次过期行」这件事由
    /// `append` / `prune` 天然承担（它们每次都会带上 `cutoff`）。
    /// 那条 `[Logs] 已载入运行日志 N 条` 的控制台输出也随之取消 ——
    /// 它对应的「载入了多少」这个事实已经不成立。
    ///
    /// 回调每次裁剪时被调用，应读**内存快照**（如 `config::retention_settings()`），
    /// 不要每次读盘 —— 写日志是相对频繁的路径。
    pub fn with_db(
        db: Option<Db>,
        get_retention_days: impl Fn() -> i64 + Send + Sync + 'static,
    ) -> Self {
        let file_path = match db.as_ref() {
            Some(db) => db.file().to_path_buf(),
            None => config::config_dir().join(crate::server::db::FILE_NAME),
        };
        Self {
            db,
            file_path: RwLock::new(file_path),
            get_retention_days: Arc::new(get_retention_days),
            inner: Mutex::new(()),
        }
    }

    /// 取锁；锁中毒（某次持锁 panic）时接管继续用 —— 与 `AccountStore::guard`
    /// 同一取向：这把锁保护的是「周期」而不是数据本身，锁内没有会被 panic
    /// 打断的中间态；真正的数据一致性由数据库事务保证。
    fn guard(&self) -> MutexGuard<'_, ()> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// 库文件路径（排障与 `Stats.file` 用）。
    ///
    /// 语义从「日志文件」变成「装着日志数据的库文件」；前端把它显示在
    /// 「文件」一栏，用户据此知道日志落在哪、该备份什么。
    ///
    /// 为什么是 `RwLock` 而不是裸字段：与 `RequestStats::file_path` 同一处理
    /// —— 字段本身不再被任何写路径改动（`relocate` 已随单库语义删除），
    /// 用 `RwLock` 是为了让「读现值」这件事与改造前同形，改动面最小。
    pub fn file(&self) -> PathBuf {
        match self.file_path.read() {
            Ok(guard) => guard.clone(),
            // 中毒恢复：路径是纯数据，继续用内部值
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// 在已取锁的前提下执行一次数据库读操作。
    ///
    /// `None` = 库不可用（未打开 / 连接中毒）或 SQL 失败，由调用方决定降级值
    /// （各方法的说明里写了为什么是那个值）。
    ///
    /// 失败只打**控制台**：绝不能走 `logging::log` —— 日志要写同一个库，
    /// `std::sync::Mutex` 不可重入，会当场死锁。这与改造前 `write_all` /
    /// `write_append` 用 `eprintln!` 是同一个理由，只是现在这条纪律更要紧
    /// （库是共享的，而死锁会带走整个进程）。
    fn with_conn<T>(
        &self,
        _guard: &MutexGuard<'_, ()>,
        action: impl FnOnce(&rusqlite::Connection) -> rusqlite::Result<T>,
    ) -> Option<T> {
        let db = self.db.as_ref()?;
        match db.with(action) {
            Some(Ok(value)) => Some(value),
            Some(Err(error)) => {
                eprintln!("[Logs] 日志数据库操作失败: {error}");
                None
            }
            None => {
                eprintln!("[Logs] 日志数据库不可用（连接已标记中毒）");
                None
            }
        }
    }

    /// 与 [`LogStore::with_conn`] 同理，但给 `&mut Connection`
    /// —— **需要事务的操作用它**：`append`（插入 + 两条裁剪）、
    /// `clear_where`（按条件删）、`clear`（清空 + 重置 id）。
    fn with_conn_mut<T>(
        &self,
        _guard: &MutexGuard<'_, ()>,
        action: impl FnOnce(&mut rusqlite::Connection) -> rusqlite::Result<T>,
    ) -> Option<T> {
        let db = self.db.as_ref()?;
        match db.with_mut(action) {
            Some(Ok(value)) => Some(value),
            Some(Err(error)) => {
                eprintln!("[Logs] 日志数据库操作失败: {error}");
                None
            }
            None => {
                eprintln!("[Logs] 日志数据库不可用（连接已标记中毒）");
                None
            }
        }
    }

    /// 当前保留期的毫秒下界（本地时区「今天往前推 N-1 天」的零点）。
    ///
    /// 保留 N 天 = 含今天在内的 N 个自然日，所以往前推 N-1 天 ——
    /// 与 `RequestStats::retention_bounds` 的口径**逐字一致**：
    /// 事件日志与请求日志的「30 天」必须是同一个 30 天。
    /// 取值范围复用 `config` 的两个常量（那里是唯一事实来源，读侧夹紧与
    /// 写侧校验共用同一组边界，手改 config.json 写个天文数字也不会让裁剪空转）。
    pub(crate) fn retention_cutoff_ms(&self) -> i64 {
        let days = (self.get_retention_days)()
            .clamp(config::RETENTION_MIN_DAYS, config::RETENTION_MAX_DAYS);
        let day = Local::now().date_naive() - ChronoDuration::days(days - 1);
        day.and_hms_opt(0, 0, 0)
            .and_then(|naive| Local.from_local_datetime(&naive).earliest())
            .map(|value| value.timestamp_millis())
            // 兜底：连当天零点都构造不出来时退到 epoch（极端日期越界才会发生）
            .unwrap_or(0)
    }

    /// 立即按当前保留天数裁剪（供「改小保留天数后立即清理」用）。
    ///
    /// 只按**时间维度**裁：容量维度（`MAX_ENTRIES`）在 `append` 里已经守住，
    /// 且改保留天数不会让条数超限。返回裁掉的条数（供调用方打一行控制台日志，
    /// 确认「改小天数确实删了东西」）。
    ///
    /// 与改造前的分工**一字未变**：那里是「内存 retain + 立刻整文件落盘」，
    /// 这里是「一条 DELETE」——「立刻生效」在数据库上天然成立（没有延迟落盘
    /// 这一说），所以那段「为什么必须立即落盘而不是等下次写入」的注释不再需要。
    ///
    /// 降级：库不可用时返回 0（「没删掉东西」是唯一诚实的答案 ——
    /// 谎报一个删除条数会让设置页显示「已清理 N 条」而磁盘上什么也没发生）。
    pub fn prune(&self) -> usize {
        let guard = self.guard();
        let cutoff = self.retention_cutoff_ms();
        self.with_conn(&guard, |conn| sql::delete_expired(conn, cutoff, None))
            .unwrap_or(0)
    }

    /// 追加一条日志，返回生成的条目。
    ///
    /// `ts` 为 None 时用当前时间；level/category 都会归一。
    /// 消息为空（或全是空白）时返回 None —— 与 Node 版 append 行为一致。
    ///
    /// ── 一次事务里完成「插入 + 两条裁剪」───────────────────────
    /// 改造前这些都在内存里做、必要时整文件重写；现在必须在**同一个事务**里：
    /// 中断在「插入之后、裁剪之前」会留下一批该被裁掉的行，而下次启动不再有
    /// `load()` 那一步去收敛它们（那是文件时代特有的补偿机制）。
    ///
    /// ── 两个保留约束依次生效（取更严的那个）─────────────────────
    /// ① 时间维度：删 `ts < cutoff` 的行。**例外：刚写入的这条**
    ///    （按 id 精确排除）即使超期也留下 —— `append` 的契约是「返回写入的
    ///    条目」，不能返回一条当场就被删掉的记录（`ts` 允许补写历史，
    ///    所以「刚写的这条超期」是真实可达的）。这条例外逐字保留。
    /// ② 容量维度：`MAX_ENTRIES`。**只在超限时**发那条 DELETE
    ///    （改造前是 `if len > MAX` 才 drain，同样是按需）。
    ///
    /// ── 返回值的保证 ──────────────────────────────────────────
    /// 只要插入成功就返回那条记录 —— 裁剪**不会**把它删掉：① 的例外按 id
    /// 排除了它，② 保留的是 id 最大的 N 条而它刚拿到当前最大 id。
    /// 因此返回值只依赖插入是否成功，不依赖后续裁剪的结果。
    ///
    /// 降级：库不可用时返回 None（调用方 `logging::log` 本来就把返回值丢掉，
    /// 丢一条日志不影响任何业务）。
    pub fn append(&self, entry: NewEntry<'_>) -> Option<LogEntry> {
        let text = clamp_message(&squeeze_whitespace(entry.message));
        if text.is_empty() {
            return None;
        }

        let guard = self.guard();
        let cutoff = self.retention_cutoff_ms();
        let ts = entry.ts.unwrap_or_else(crate::server::logging::now_ms);
        let level = normalize_level(entry.level);
        let category = normalize_category(entry.category);
        let data = normalize_data(entry.data);

        self.with_conn_mut(&guard, |conn| {
            let tx = conn.transaction()?;
            // id 由 kv 里的高水位分配（不能靠 rowid 自增：它会复用被删掉的号，
            // 见 `sql.rs` 模块头的实测结论）
            let id = sql::allocate_next_id(&tx)?;
            let record = LogEntry { id, ts, level, category, message: text, data };
            sql::insert(&tx, &record)?;
            sql::delete_expired(&tx, cutoff, Some(id))?;
            if sql::count_all(&tx)? > MAX_ENTRIES {
                sql::trim_to_capacity(&tx, MAX_ENTRIES)?;
            }
            tx.commit()?;
            Ok(record)
        })
    }

    /// 查询：按级别（含以上）、分类、关键词、起始 id、时间区间过滤，
    /// 倒序返回最新在前。
    /// `limit` 语义与 Node 版一致：对过滤结果取**最后** N 条（即最新的 N 条）。
    ///
    /// `start` / `end` 是**新增的可选**条件（闭开区间 `[start, end)`，
    /// 单位毫秒）：不传时（None）过滤链与以前完全一致 —— 这是向后兼容的关键，
    /// 老调用方与老前端拿到的结果不会因为多了时间维度而变。
    ///
    /// ── `limit` 的语义怎么保证与改造前逐字一致 ──────────────────
    /// 旧实现是「对过滤结果取尾部 N 条」（数组按写入顺序排，尾部即最新），
    /// 再 `reverse()` 成倒序。现在 `sql::select_matching_desc` 返回的是
    /// **按 id 降序的全部命中行**（已经是最新在前），这里只做一次
    /// `truncate(size)` ——「截前 N 条」等价于旧实现的「取尾部 N 条再倒序」。
    /// **关键词过滤发生在截断之前**（在 SQL 取回那一步就过滤掉了，见
    /// `sql::FilterPlan`），与旧实现「先跑完整过滤链再切片」的顺序相同。
    ///
    /// 降级：库不可用时返回全空（`QueryResult` 三个字段都是自然的空值形态：
    /// 前端把空 entries 渲染成「暂无日志」，不会崩也不会误报有内容）。
    pub fn query(&self, query: &Query) -> QueryResult {
        let guard = self.guard();
        let plan = sql::FilterPlan::of(query);
        let loaded = self.with_conn(&guard, |conn| {
            let total = sql::count_all(conn)?;
            let entries = sql::select_matching_desc(conn, &plan)?;
            Ok((total, entries))
        });
        let Some((total, filtered)) = loaded else {
            return QueryResult { entries: Vec::new(), total: 0, matched: 0 };
        };

        let matched = filtered.len();
        // limit 缺省 200，并夹在 [1, MAX_ENTRIES]（对应 Node 版 Math.min/Math.max）
        let size = query.limit.unwrap_or(200).clamp(1, MAX_ENTRIES);
        let mut entries = filtered;
        entries.truncate(size);
        QueryResult { entries, total, matched }
    }

    /// 按筛选条件删除：返回（删除条数，删除后的统计）。
    ///
    /// 与 `clear()` 的差别：只删命中的条目，未命中的保留；**id 不回退**
    /// （保持单调递增）—— 已读水位（导航徽标）依赖「新条目 id > 旧水位」，
    /// 删除后这条性质依然成立。高水位存在 `kv` 里，删行根本不碰它。
    ///
    /// ── 过滤条件与 `query` 共用同一份构造 ──────────────────────
    /// 两者都先用 `sql::FilterPlan::of(query)` 编译条件、再走同一个
    /// `sql::select_matching_desc` ——「查询里看到的 N 条」与「这里删掉的 N 条」
    /// 因此是**同一批**，不再需要靠两处手写保持一致（旧实现在内存里共用
    /// `Query::matches`；现在共用的层次更靠下：连 SQL 的 WHERE 片段都是同一份）。
    /// `limit` 不参与（`FilterPlan` 只编译筛选条件，它不算筛选）。
    ///
    /// 实现上「先按 id 查出命中的行、再按 id 删除」两步在**一个事务**里：
    /// 关键词必须在 Rust 侧过滤（`sql.rs` 模块头有理由），所以没法把条件
    /// 直接塞进一条 DELETE 的 WHERE。
    ///
    /// 降级：库不可用时返回 `(0, 空统计)` —— 「一条都没删」。
    pub fn clear_where(&self, query: &Query) -> (usize, Stats) {
        let removed = {
            let guard = self.guard();
            let plan = sql::FilterPlan::of(query);
            self.with_conn_mut(&guard, |conn| {
                let tx = conn.transaction()?;
                let hits = sql::select_matching_desc(&tx, &plan)?;
                let ids: Vec<u64> = hits.iter().map(|item| item.id).collect();
                let removed = sql::delete_by_ids(&tx, &ids)?;
                tx.commit()?;
                Ok(removed)
            })
            .unwrap_or(0)
        };
        // 统计在**新的**一次取锁里重取（`stats()` 自己也要取这把锁，而
        // `std::sync::Mutex` 不可重入 —— 所以必须先放掉上面那把）
        (removed, self.stats())
    }

    /// 各级别 / 分类计数
    ///
    /// 形状**一字未改**：四个级别键与十个分类键恒定存在（值为 0 也要在），
    /// 前端拿它填筛选下拉与徽标上限。库只回报「有的那些」，其余在这里补 0。
    ///
    /// 降级：库不可用时给全 0（`total: 0` / 各级别分类都是 0 / `last_id: 0`）——
    /// 前端据此渲染「暂无日志」，而尾部那个 0 水位在 `app.js` 里**不会**被拿去
    /// 推进已读水位：`updateLogsBadge` 明确写了「stats 缺失（接口失败）时什么都
    /// 不做：不能拿默认的 lastId=0 去判断水位」。所以这个降级是安全的。
    pub fn stats(&self) -> Stats {
        let mut by_level = Map::new();
        for level in LEVELS {
            by_level.insert(level.to_string(), Value::from(0));
        }
        let mut by_category = Map::new();
        for (key, _) in CATEGORIES {
            by_category.insert(key.to_string(), Value::from(0));
        }
        let file = self.file().to_string_lossy().to_string();

        let loaded = {
            let guard = self.guard();
            self.with_conn(&guard, |conn| {
                let total = sql::count_all(conn)?;
                let last_id = sql::max_id(conn)?;
                let levels = sql::level_counts(conn)?;
                let categories = sql::category_counts(conn)?;
                Ok((total, last_id, levels, categories))
            })
        };
        let Some((total, last_id, levels, categories)) = loaded else {
            return Stats {
                total: 0,
                max: MAX_ENTRIES,
                by_level,
                by_category,
                last_id: 0,
                file,
            };
        };

        // 库里的 level/category 恒为字典内的值（写入侧归一过），所以这里是
        // 「把已有键的值填进去」；万一有越界的值（有人用 sqlite3 手工改库），
        // `entry()` 会如实补进去计数 —— 不隐藏它的存在，让日志页显示得出来。
        for (level, count) in levels {
            let slot = by_level.entry(level).or_insert_with(|| Value::from(0));
            *slot = Value::from(count.max(0) as u64);
        }
        for (category, count) in categories {
            let slot = by_category.entry(category).or_insert_with(|| Value::from(0));
            *slot = Value::from(count.max(0) as u64);
        }
        Stats { total, max: MAX_ENTRIES, by_level, by_category, last_id, file }
    }

    /// 清空：删掉全部日志并**把 id 重置为 1**，返回清空后的统计
    /// （对应 Node 版 clear）。
    ///
    /// ── id 重置是**有意**与 `clear_where` 不同的行为 ─────────────
    /// 依据是前端 `ui/app.js` 的 `updateLogsBadge`：它有一条
    /// 「`lastId < lastSeenLogId` ⇒ 认定日志被清空过、水位回落」的分支，
    /// 而那条分支正是为 `clear()` 写的（源码注释逐字写着「日志被清空（id 重新
    /// 从 1 数起）：水位必须跟着回落」）。若这里不回退，那条分支就永远不会触发
    /// （新 id 继续从高位涨，恒大于水位）—— 徽标逻辑**碰巧**仍然工作，但前端
    /// 那段代码会变成死代码，且「清空后从 1 重新计数」这个改造前就有的可观察
    /// 行为被无声改掉了。结论：**保持回退**，行为一致优先。
    ///
    /// 高水位与行数一起重置：先 `DELETE FROM logs`（清行），再把 `kv` 里的
    /// 计数器写回 1。两步在一个事务里 —— 中断在中间会留下「行已清空、id 仍从
    /// 高位继续」的错位，那会让前端的水位回落分支失效。
    ///
    /// 降级：库不可用时返回空统计（`total: 0`）。调用方从响应里看不出
    /// 「其实没清掉」，但那一行 `[Logs] 日志数据库操作失败` 已经打到控制台 ——
    /// 与改造前 `write_all` 失败只打 `eprintln!` 同一取向：日志清空失败
    /// 不是会让请求出错的事。
    pub fn clear(&self) -> Stats {
        {
            let guard = self.guard();
            let _ = self.with_conn_mut(&guard, |conn| {
                let tx = conn.transaction()?;
                sql::delete_all(&tx)?;
                sql::reset_next_id(&tx)?;
                tx.commit()?;
                Ok(())
            });
        }
        self.stats()
    }

    /// 导出为 JSONL 文本（桌面端「导出」按钮用）。空日志返回空串。
    ///
    /// **格式一字未变**：一行一条 `LogEntry` 的 JSON（字段顺序由 `serde` 的
    /// 结构体字段序决定，与改造前同一份 `render_jsonl` 序列化方式），
    /// 空列表返回空串。数据来源从「内存快照」变成「按 id 升序的一次查询」——
    /// 顺序与旧的「文件顺序」相同。
    ///
    /// 降级：库不可用时返回空串（导出一个空附件，比 500 更能说明「当前没有
    /// 可导出的内容」；调用方 `download_logs` 直接把它当响应体）。
    pub fn to_jsonl(&self) -> String {
        let guard = self.guard();
        let entries = self.with_conn(&guard, sql::select_all_asc);
        render_jsonl(&entries.unwrap_or_default())
    }

    /// 库里的条目数（Node 版 `createLogStore` 导出的 `get size()` 对等物；
    /// 日志页统计走 stats()，这里保留供排障与后续容量判定）。
    ///
    /// 改造前读的是内存 `Vec` 的长度，现在是 `COUNT(*)` —— 语义（当前条数）
    /// 不变，数据来源跟着存储走。
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        let guard = self.guard();
        self.with_conn(&guard, sql::count_all).unwrap_or(0)
    }

    /// 空判定（与 `len()` 配套；Node 的 size getter 为 0 时的等价写法）
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 把条目列表渲染成 JSONL 文本。空列表返回空串
/// （对应 Node 版 toJsonl：`entries.length ? ... : ''`）。
///
/// 序列化失败的条目直接跳过：这里已经是错误处理路径，
/// 不能因为某条日志含异常结构就把整份导出搞成空文件。
/// **序列化方式与改造前逐字相同**（`serde_json::to_string` + 结构体字段序）。
fn render_jsonl(entries: &[LogEntry]) -> String {
    if entries.is_empty() {
        return String::new();
    }
    let mut text = String::new();
    for item in entries {
        if let Ok(line) = serde_json::to_string(item) {
            text.push_str(&line);
            text.push('\n');
        }
    }
    text
}

/// `append` 的入参：借用了外部字符串，避免调用方为每条日志都构造 String。
pub struct NewEntry<'a> {
    pub level: &'a str,
    pub category: &'a str,
    pub message: &'a str,
    pub data: Option<&'a Value>,
    pub ts: Option<i64>,
}
