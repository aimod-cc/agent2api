//! 运行日志存储（`{config_dir}/logs.jsonl`）。
//!
//! 对照 Node 版 `src/workbuddy-logs.mjs` 逐条实现：
//!   - 一行一条 JSON，追加写；内存里保留最近 MAX_ENTRIES 条（环形保留）
//!   - 启动时载入历史，因此重启后仍能查到之前的记录
//!   - 满额后不立即整文件重写，攒够 COMPACT_STEP 条再收敛（避免每写一条就全量落盘）
//!
//! 文件路径与 Node 版一致（`{directory}/logs.jsonl`）而非 `logs/` 子目录 ——
//! 用户已有的历史日志就在这个位置，换目录会让升级后查不到旧记录。
//!
//! ── 两个保留约束（Node 版只有前者，后者是本次新增）─────────────
//!   - **容量维度**：`MAX_ENTRIES = 500` 的环形保留，管「最多多少条」，
//!     短时间内的日志风暴不该按天数比例吃光内存。**保持原样不动**。
//!   - **时间维度**：可配置的保留天数（默认 30 天，见
//!     `config::DEFAULT_LOG_RETENTION_DAYS`），管「最多多久」，
//!     避免用户把保留期设短之后旧日志还赖在文件里。
//!
//! 两者**取更严的那个**：先按天数裁、再按条数裁，谁先到线谁生效。
//!
//! 天数由外部回调提供（`new` 的第二个参数），**每次裁剪时动态取** ——
//! 于是设置页改完天数下一次写入就生效，不需要重启进程；回调读的是
//! `config::retention_settings()` 的内存快照（不读盘）。
//!
//! 并发模型：Node 版是单线程事件循环，天然串行；这里用 `Mutex` 包住
//! 「内存快照 + 落盘」整体操作，保证多请求并发时 id 不会重复、文件不会交错写。
//!
//! JSON 字段名与 Node 版一致：`id/ts/level/category/message/data`。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use chrono::{Duration as ChronoDuration, Local, TimeZone};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::server::config;

/// 日志文件名（与 Node 版 FILE_NAME 一致）
pub const FILE_NAME: &str = "logs.jsonl";
/// 内存/文件里保留的最大条数（环形保留，超出丢最旧的）
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
/// 满额后攒够这么多条追加，才整文件重写一次（对应 Node 版 COMPACT_STEP）
const COMPACT_STEP: usize = 100;

/// 一条日志。字段名用缩写 `ts` 与 Node 版 JSONL 完全对齐，
/// 前端 logs-panel.js 读的是 `entry.ts` / `entry.level` / `entry.message`。
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

/// 查询结果：`entries` 倒序（最新在前），`total` 为内存总量，`matched` 为过滤后数量
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
fn normalize_level(value: &str) -> String {
    let lower = value.trim().to_lowercase();
    if LEVELS.contains(&lower.as_str()) {
        lower
    } else {
        "info".to_string()
    }
}

/// 级别序号，用于「该级别及以上」筛选；未知级别按 info
fn level_rank(level: &str) -> usize {
    LEVELS.iter().position(|item| *item == level).unwrap_or(1)
}

impl Query {
    /// 这条日志是否命中本查询的**筛选**条件。
    ///
    /// `query()` 与 `clear_where()` 共用这一份过滤链 —— 查询里看到的「筛选出
    /// N 条」与删除时删掉的那 N 条必须是同一批，两处手写必然漂移。
    /// `limit` 是分页参数不算筛选，不在其中。
    fn matches(&self, item: &LogEntry) -> bool {
        // 级别下限：与 Node 版一致，未知级别不参与过滤
        if let Some(rank) = self
            .level
            .as_deref()
            .map(|value| value.trim().to_lowercase())
            .filter(|value| LEVELS.contains(&value.as_str()))
            .map(|value| level_rank(&value))
        {
            if level_rank(&item.level) < rank {
                return false;
            }
        }
        // 分类：同样要求是已知分类才过滤
        if let Some(want) = self
            .category
            .as_deref()
            .map(|value| value.trim().to_lowercase())
            .filter(|value| CATEGORIES.iter().any(|(key, _)| *key == *value))
        {
            if item.category != want {
                return false;
            }
        }
        if let Some(want) = self
            .keyword
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_lowercase)
        {
            if !item.message.to_lowercase().contains(&want) {
                return false;
            }
        }
        if let Some(from) = self.since_id {
            if item.id <= from {
                return false;
            }
        }
        // 时间区间：闭开 [start, end)
        if let Some(from) = self.start {
            if item.ts < from {
                return false;
            }
        }
        if let Some(to) = self.end {
            if item.ts >= to {
                return false;
            }
        }
        true
    }
}

/// 分类归一：未知值一律 server（对应 Node 版 normalizeCategory）
fn normalize_category(value: &str) -> String {
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
fn normalize_data(value: Option<&Value>) -> Option<Value> {
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
fn squeeze_whitespace(text: &str) -> String {
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
fn clamp_message(text: &str) -> String {
    if text.chars().count() <= MAX_MESSAGE_LENGTH {
        return text.to_string();
    }
    text.chars().take(MAX_MESSAGE_LENGTH).collect()
}

/// 日志存储本体。所有公开方法都取 `&self`，内部 `Mutex` 串行化。
pub struct LogStore {
    /// 保存目录。包 `RwLock` 是为了**运行中换目录**（迁移，见 `relocate`）：
    /// 普通写路径都持 `inner` 锁后才碰文件，锁序恒为 inner → directory，
    /// 不会互等；文件路径不另存字段 —— 它永远由目录派生，两处必然一致。
    directory: RwLock<PathBuf>,
    /// 保留天数回调。**每次裁剪时动态调用**，于是设置改完天数下一次写入
    /// 就用新值，不需要重启进程（与 `RequestStats::get_retention` 同一模式）。
    /// 用 `Arc` 是为了让 `LogStore` 本身仍能放进 `OnceLock`（不需要 `&self` 生命周期）
    get_retention_days: Arc<dyn Fn() -> i64 + Send + Sync>,
    inner: Mutex<Inner>,
}

struct Inner {
    entries: Vec<LogEntry>,
    next_id: u64,
    /// 文件比内存上限大，或已攒够一批追加 —— 下次写入时整文件收敛
    dirty: bool,
    appends_since_compact: usize,
}

impl LogStore {
    /// 构造并载入历史日志。目录不存在不报错，首次写入时自动创建。
    ///
    /// **签名与 `RequestStats::new(directory, get_retention)` 统一**：两者是
    /// 「同一份数据的两半」（事件日志 / 请求统计），数据目录同源、保留期同样
    /// 由回调动态提供，构造方式不该有两套写法。
    ///
    /// 原签名 `new(directory)`（固定 30 天）在本改造中被替换掉而非保留为
    /// 重载：它只有一个调用点（`logging::init_store`，本次必须改它才能接上配置），
    /// 留一个无人调用的默认构造器只会变成需要 `#[allow(dead_code)]` 说明的死代码。
    /// 保留天数的默认值因此只有一份事实来源：`config::DEFAULT_LOG_RETENTION_DAYS`
    /// （缺配置时由 `config::retention_settings()` 兜底给出）。
    ///
    /// 回调每次裁剪时被调用，应读**内存快照**（如 `config::retention_settings()`），
    /// 不要每次读盘 —— 写日志是相对频繁的路径。
    pub fn new(
        directory: impl AsRef<Path>,
        get_retention_days: impl Fn() -> i64 + Send + Sync + 'static,
    ) -> Self {
        let directory = RwLock::new(directory.as_ref().to_path_buf());
        let store = Self {
            directory,
            get_retention_days: Arc::new(get_retention_days),
            inner: Mutex::new(Inner {
                entries: Vec::new(),
                next_id: 1,
                dirty: false,
                appends_since_compact: 0,
            }),
        };
        store.load();
        store
    }

    /// 当前保存目录（迁移会换它，读侧每次都取现值而不是缓存）
    fn directory_of(&self) -> PathBuf {
        match self.directory.read() {
            Ok(guard) => guard.clone(),
            // 中毒恢复：目录路径是纯数据，继续用内部值（与 inner 的取向一致）
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// 当前日志文件的完整路径（由目录派生）
    fn file_path(&self) -> PathBuf {
        self.directory_of().join(FILE_NAME)
    }

    /// 当前保留期的毫秒下界（本地时区「今天往前推 N-1 天」的零点）。
    ///
    /// 保留 N 天 = 含今天在内的 N 个自然日，所以往前推 N-1 天 ——
    /// 与 `RequestStats::retention_bounds` 的口径**逐字一致**：
    /// 事件日志与请求日志的「30 天」必须是同一个 30 天。
    /// 取值范围复用 `config` 的两个常量（那里是唯一事实来源，读侧夹紧与
    /// 写侧校验共用同一组边界，手改 config.json 写个天文数字也不会让裁剪空转）。
    fn retention_cutoff_ms(&self) -> i64 {
        let days = (self.get_retention_days)()
            .clamp(config::RETENTION_MIN_DAYS, config::RETENTION_MAX_DAYS);
        let day = Local::now().date_naive() - ChronoDuration::days(days - 1);
        day.and_hms_opt(0, 0, 0)
            .and_then(|naive| Local.from_local_datetime(&naive).earliest())
            .map(|value| value.timestamp_millis())
            // 兜底：连当天零点都构造不出来时退到 epoch（极端日期越界才会发生）
            .unwrap_or(0)
    }

    /// 日志文件路径（桌面端「日志」页会直接展示它）。
    /// Node 版 `createLogStore` 返回对象里的 `file` 取值的对等物
    /// （stats() 的 `file` 字段已表达同一事实，这里保留供直读）。
    #[allow(dead_code)]
    pub fn file(&self) -> PathBuf {
        self.file_path()
    }

    /// 载入历史：逐行解析，损坏行跳过（不影响其余日志）；
    /// 载入后按两个保留约束收敛（先时间后容量），文件比内存多时标 dirty，
    /// 下次写入把文件收敛回同样的集合。
    fn load(&self) {
        let Ok(text) = std::fs::read_to_string(self.file_path()) else {
            // 文件不存在是正常情况（首次运行），不打扰用户
            return;
        };
        let mut parsed: Vec<LogEntry> = Vec::new();
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
                continue;
            };
            let Some(object) = value.as_object() else {
                continue;
            };
            let message = object
                .get("message")
                .map(|item| item.as_str().unwrap_or_default().to_string())
                .unwrap_or_default();
            let level = object.get("level").and_then(Value::as_str).unwrap_or("info");
            let category = object.get("category").and_then(Value::as_str).unwrap_or("server");
            parsed.push(LogEntry {
                id: object.get("id").and_then(Value::as_u64).unwrap_or(parsed.len() as u64 + 1),
                ts: object.get("ts").and_then(Value::as_i64).unwrap_or(0),
                level: normalize_level(level),
                category: normalize_category(category),
                message: clamp_message(&message),
                data: normalize_data(object.get("data")),
            });
        }
        let total = parsed.len();
        // ① 时间维度：保留天数（与运行期同一套 cutoff，口径不会漂）
        let cutoff = self.retention_cutoff_ms();
        parsed.retain(|item| item.ts >= cutoff);
        // ② 容量维度：环形上限（保持不变）
        let entries: Vec<LogEntry> = if parsed.len() > MAX_ENTRIES {
            parsed.split_off(parsed.len() - MAX_ENTRIES)
        } else {
            parsed
        };
        let next_id = entries.iter().map(|item| item.id).max().unwrap_or(0) + 1;

        if let Ok(mut guard) = self.inner.lock() {
            // dirty 的语义是「文件内容比内存多」：两种裁剪（超天数 / 超条数）
            // 都会造成这种差异，任一发生就要在下次写入时整文件收敛 ——
            // 否则重开程序又会把已裁掉的行载回来
            guard.dirty = entries.len() < total;
            guard.entries = entries;
            guard.next_id = next_id;
            let count = guard.entries.len();
            drop(guard);
            // 只用控制台通道（crate::server::logging::console_line）：
            // 往日志库里写「日志库已载入」会自己套自己。
            // Node 版这里传的也是 console.log 而不是网关的 log()，行为一致。
            crate::server::logging::console_line(
                "[Logs]",
                &format!("已载入运行日志 {count} 条"),
            );
        }
    }

    /// 立即按当前保留天数裁剪，并落盘（供「改小保留天数后立即清理」用）。
    ///
    /// 与 `append` 里那次顺带裁剪的分工：那里只是「不涨过头」（每次写日志顺手摘掉
    /// 已过期的头部若干条），这里则是用户显式要求的清理，所以要**立刻落盘** ——
    /// 调用方（设置页保存后）期待的是盘上也干净了，而不是等下一次日志写入才收敛。
    ///
    /// 只按时间维度裁：容量维度（MAX_ENTRIES）在 append/load 时已经守住，
    /// 且改保留天数不会让条数超限。返回裁掉的条数（供调用方打一行控制台日志，
    /// 确认「改小天数确实删了东西」）。
    pub fn prune(&self) -> usize {
        let cutoff = self.retention_cutoff_ms();
        let Ok(mut guard) = self.inner.lock() else {
            return 0;
        };
        let before = guard.entries.len();
        // 逐条判定而不是「二分找分界点再 drain」：`ts` 是调用方传入的，
        // 补写历史日志时可能逆序；上限 500 条，O(n) 扫描的成本可以忽略
        guard.entries.retain(|item| item.ts >= cutoff);
        let removed = before - guard.entries.len();
        if removed == 0 {
            return 0;
        }
        // 立即整文件收敛：内存与文件要么一起干净要么一起不干净。
        // 落盘在**持锁期间**完成（与 `append` 的整文件重写路径同理）：
        // 若先放锁再写，期间并发的 `append` 追加的那一行会被这次重写吃掉
        // （内存里有、文件里没有，重开程序就少一条）。清理是低频的用户动作，
        // 持锁写盘的开销可以接受。
        let text = render_jsonl(&guard.entries);
        guard.dirty = false;
        guard.appends_since_compact = 0;
        self.write_all(&text);
        removed
    }

    /// 追加一条日志并落盘，返回生成的条目。
    ///
    /// `ts` 为 None 时用当前时间；level/category 都会归一。
    /// 消息为空（或全是空白）时返回 None —— 与 Node 版 append 行为一致。
    ///
    /// 落盘在**持锁期间**完成：Node 版是单线程事件循环，内存快照与文件内容
    /// 天然一致；这里若把写文件放到锁外，并发追加会交错写入、文件里的 id 顺序
    /// 也会乱。日志属于低频事件（系统级动作才记），持锁写盘的开销可以接受。
    pub fn append(&self, entry: NewEntry<'_>) -> Option<LogEntry> {
        let text = clamp_message(&squeeze_whitespace(entry.message));
        if text.is_empty() {
            return None;
        }

        let Ok(mut guard) = self.inner.lock() else {
            return None;
        };
        let record = LogEntry {
            id: guard.next_id,
            ts: entry.ts.unwrap_or_else(crate::server::logging::now_ms),
            level: normalize_level(entry.level),
            category: normalize_category(entry.category),
            message: text,
            data: normalize_data(entry.data),
        };
        guard.next_id += 1;
        guard.entries.push(record.clone());
        // ── 两个保留约束依次生效（见文件头：取更严的那个）──────
        // ① 时间维度：按保留天数裁掉过期条目。
        //    用 `retain` 而不是「数组头部连续摘掉」的写法：`ts` 由调用方传入
        //    （`NewEntry::ts` 允许补写历史），数组不保证严格按 ts 有序，
        //    逐条判定对乱序也安全；上限 500 条，一次 O(n) 扫描在这个频率下可忽略。
        //    例外：**刚写入的这条**（按 id 精确匹配）即使超期也留下 ——
        //    `append` 的契约是「返回写入的条目」，不能返回一条当场就被删掉的记录。
        let cutoff = self.retention_cutoff_ms();
        let before = guard.entries.len();
        guard.entries.retain(|item| item.ts >= cutoff || item.id == record.id);
        if guard.entries.len() != before {
            // 内存比文件少 → 标 dirty，让本次写入把整文件收敛掉
            // （否则文件里仍留着过期行，重开程序它们又回来了）
            guard.dirty = true;
        }
        // ② 容量维度：环形上限（**保持不变**，与时间约束取更严的那个）
        if guard.entries.len() > MAX_ENTRIES {
            let overflow = guard.entries.len() - MAX_ENTRIES;
            guard.entries.drain(0..overflow);
        }

        // 收敛判定与 Node 版 persist 一致：启动时发现文件超限（dirty），
        // 或满额后已攒够 COMPACT_STEP 次追加，才整文件重写，否则只追加一行
        let compact = guard.dirty || guard.appends_since_compact >= COMPACT_STEP;
        if compact {
            guard.dirty = false;
            guard.appends_since_compact = 0;
        } else if guard.entries.len() >= MAX_ENTRIES {
            guard.appends_since_compact += 1;
        }

        // 落盘在持锁期间完成：整文件重写会覆盖全部内容，
        // 若期间有并发追加插入，那一行会被这次重写丢掉（内存有、文件没有），
        // 重开程序就少一条日志。日志是低频事件，持锁写盘的开销可以接受。
        if compact {
            let text = render_jsonl(&guard.entries);
            self.write_all(&text);
        } else if let Ok(line) = serde_json::to_string(&record) {
            self.write_append(&line);
        }
        drop(guard);
        Some(record)
    }

    /// 整文件覆盖写（环形收敛与清空用）
    fn write_all(&self, text: &str) {
        if let Err(error) = std::fs::create_dir_all(self.directory_of()) {
            eprintln!("[Logs] 日志写入失败: {error}");
            return;
        }
        if let Err(error) = std::fs::write(self.file_path(), text) {
            eprintln!("[Logs] 日志写入失败: {error}");
        }
    }

    /// 追加一行（正常路径）
    fn write_append(&self, line: &str) {
        use std::io::Write;
        if let Err(error) = std::fs::create_dir_all(self.directory_of()) {
            eprintln!("[Logs] 日志写入失败: {error}");
            return;
        }
        let result = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.file_path())
            .and_then(|mut file| file.write_all(format!("{line}\n").as_bytes()));
        if let Err(error) = result {
            eprintln!("[Logs] 日志写入失败: {error}");
        }
    }

    /// 查询：按级别（含以上）、分类、关键词、起始 id、时间区间过滤，
    /// 倒序返回最新在前。
    /// `limit` 语义与 Node 版一致：对过滤结果取**最后** N 条（即最新的 N 条）。
    ///
    /// `start` / `end` 是**新增的可选**条件（闭开区间 `[start, end)`，
    /// 单位毫秒）：不传时（None）过滤链与以前完全一致 —— 这是向后兼容的关键，
    /// 老调用方与老前端拿到的结果不会因为多了时间维度而变。
    pub fn query(&self, query: &Query) -> QueryResult {
        let Ok(guard) = self.inner.lock() else {
            return QueryResult { entries: Vec::new(), total: 0, matched: 0 };
        };
        let total = guard.entries.len();

        // 过滤链在 `Query::matches`（与 clear_where 共用一份，口径不会漂）
        let filtered: Vec<&LogEntry> =
            guard.entries.iter().filter(|item| query.matches(item)).collect();

        let matched = filtered.len();
        // limit 缺省 200，并夹在 [1, MAX_ENTRIES]（对应 Node 版 Math.min/Math.max）
        let size = query.limit.unwrap_or(200).clamp(1, MAX_ENTRIES);
        let start = filtered.len().saturating_sub(size);
        let mut entries: Vec<LogEntry> = filtered[start..].iter().map(|item| (*item).clone()).collect();
        entries.reverse();
        QueryResult { entries, total, matched }
    }

    /// 按筛选条件删除：返回（删除条数，删除后的统计）。
    ///
    /// 与 `clear()` 的差别：只删命中的条目，未命中的保留；**id 不回退**
    /// （保持单调递增）—— 已读水位（导航徽标）依赖「新条目 id > 旧水位」，
    /// 删除后这条性质依然成立。文件在持锁期间整体重写：
    /// 内存里留下的就是文件里该有的。
    pub fn clear_where(&self, query: &Query) -> (usize, Stats) {
        let Ok(mut guard) = self.inner.lock() else {
            return (0, self.stats());
        };
        let before = guard.entries.len();
        guard.entries.retain(|item| !query.matches(item));
        let removed = before - guard.entries.len();
        if removed > 0 {
            let text = render_jsonl(&guard.entries);
            self.write_all(&text);
            guard.dirty = false;
            guard.appends_since_compact = 0;
        }
        let stats = self.stats();
        (removed, stats)
    }

    /// 迁移到新目录：按内存整份写出到新位置，成功后切换目录并删除旧文件。
    ///
    /// 为什么是「按内存重写」而不是「复制文件」：内存就是**有效全集**
    /// （载入时已按保留期 / 上限裁剪，文件里多出来的旧行本就该被下次写入收敛），
    /// 重写一遍顺带把新位置的文件收敛到位。
    ///
    /// 全程持 `inner` 锁：迁移期间的写入会等到迁移完成（日志是低频事件，
    /// 秒级阻塞可接受）。`progress(已写字节, 总字节)` 供界面画进度条；
    /// 写新文件**中途失败**时不切目录、不动旧文件 —— 本次迁移安全失败，
    /// 新目录里可能留下的半截文件会被下一次成功的迁移覆盖（File::create 截断）。
    pub fn relocate(
        &self,
        new_dir: &Path,
        progress: impl Fn(u64, u64) + Send + Sync,
    ) -> Result<(), String> {
        let old_dir = self.directory_of();
        let old_file = old_dir.join(FILE_NAME);
        let new_dir = new_dir.to_path_buf();
        if old_dir == new_dir {
            return Err("新目录与当前保存位置相同".to_string());
        }
        let Ok(mut guard) = self.inner.lock() else {
            return Err("日志库正被占用，请稍后重试".to_string());
        };
        let text = render_jsonl(&guard.entries);
        let total = text.len() as u64;
        progress(0, total);
        std::fs::create_dir_all(&new_dir).map_err(|error| format!("创建目录失败: {error}"))?;
        let new_file = new_dir.join(FILE_NAME);
        write_chunked(&new_file, text.as_bytes(), &progress)?;
        // 两个关键步骤都成功才切目录：此后写入都落新文件，
        // dirty 计数清零（新文件刚按当前内存整份写出，两边一致）
        if let Ok(mut directory) = self.directory.write() {
            *directory = new_dir.clone();
        }
        guard.dirty = false;
        guard.appends_since_compact = 0;
        drop(guard);
        // 旧文件删掉，「搬家」不留歧义；删失败不回滚 —— 数据已在新位置，
        // 旧文件顶多留一份历史副本（下次手动清掉即可）
        if let Err(error) = std::fs::remove_file(&old_file) {
            crate::server::logging::console_line(
                "[Logs]",
                &format!("⚠️ 旧日志文件删除失败（数据已在新位置生效）: {error}"),
            );
        }
        Ok(())
    }

    /// 各级别 / 分类计数
    pub fn stats(&self) -> Stats {
        let mut by_level = Map::new();
        for level in LEVELS {
            by_level.insert(level.to_string(), Value::from(0));
        }
        let mut by_category = Map::new();
        for (key, _) in CATEGORIES {
            by_category.insert(key.to_string(), Value::from(0));
        }

        let Ok(guard) = self.inner.lock() else {
            return Stats {
                total: 0,
                max: MAX_ENTRIES,
                by_level,
                by_category,
                last_id: 0,
                file: self.file_path().to_string_lossy().to_string(),
            };
        };
        for item in &guard.entries {
            let level = by_level.entry(item.level.clone()).or_insert_with(|| Value::from(0));
            *level = Value::from(level.as_u64().unwrap_or(0) + 1);
            let category = by_category
                .entry(item.category.clone())
                .or_insert_with(|| Value::from(0));
            *category = Value::from(category.as_u64().unwrap_or(0) + 1);
        }
        Stats {
            total: guard.entries.len(),
            max: MAX_ENTRIES,
            by_level,
            by_category,
            last_id: guard.entries.last().map(|item| item.id).unwrap_or(0),
            file: self.file_path().to_string_lossy().to_string(),
        }
    }

    /// 清空：内存与文件同时归零，返回清空后的统计（对应 Node 版 clear）
    pub fn clear(&self) -> Stats {
        if let Ok(mut guard) = self.inner.lock() {
            guard.entries.clear();
            guard.next_id = 1;
            guard.dirty = false;
            guard.appends_since_compact = 0;
        }
        if let Err(error) = std::fs::create_dir_all(self.directory_of()) {
            eprintln!("[Logs] 日志清空失败: {error}");
        } else if let Err(error) = std::fs::write(self.file_path(), "") {
            eprintln!("[Logs] 日志清空失败: {error}");
        }
        self.stats()
    }

    /// 导出为 JSONL 文本（桌面端「导出」按钮用）。空日志返回空串。
    pub fn to_jsonl(&self) -> String {
        let Ok(guard) = self.inner.lock() else {
            return String::new();
        };
        render_jsonl(&guard.entries)
    }

    /// 内存中的条目数（Node 版 `createLogStore` 导出的 `get size()` 对等物；
    /// 日志页统计走 stats()，这里保留供排障与后续容量判定）。
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.inner.lock().map(|guard| guard.entries.len()).unwrap_or(0)
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

/// 分块写文件并回报进度，`progress(已写字节, 总字节)`。
///
/// 迁移的数据量可能到几 MB（请求明细上限 2 万条）：一次性 write 也就是一瞬，
/// 但分块让进度回调有东西可画，代价只是几轮循环。`pub(crate)` 供
/// `RequestStats::relocate` 复用 —— 两个存储的迁移共用同一份「怎么写、怎么报」。
pub(crate) fn write_chunked(
    path: &Path,
    bytes: &[u8],
    progress: &impl Fn(u64, u64),
) -> Result<(), String> {
    use std::io::Write;
    let mut file = std::fs::File::create(path).map_err(|error| format!("创建文件失败: {error}"))?;
    let total = bytes.len() as u64;
    let mut written = 0u64;
    for block in bytes.chunks(256 * 1024) {
        file.write_all(block).map_err(|error| format!("写入文件失败: {error}"))?;
        written += block.len() as u64;
        progress(written, total);
    }
    file.flush().map_err(|error| format!("写入文件失败: {error}"))?;
    Ok(())
}

/// `append` 的入参：借用了外部字符串，避免调用方为每条日志都构造 String。
pub struct NewEntry<'a> {
    pub level: &'a str,
    pub category: &'a str,
    pub message: &'a str,
    pub data: Option<&'a Value>,
    pub ts: Option<i64>,
}
