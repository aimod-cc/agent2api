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
//! 并发模型：Node 版是单线程事件循环，天然串行；这里用 `Mutex` 包住
//! 「内存快照 + 落盘」整体操作，保证多请求并发时 id 不会重复、文件不会交错写。
//!
//! JSON 字段名与 Node 版一致：`id/ts/level/category/message/data`。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// 日志文件名（与 Node 版 FILE_NAME 一致）
pub const FILE_NAME: &str = "logs.jsonl";
/// 内存/文件里保留的最大条数（环形保留，超出丢最旧的）
pub const MAX_ENTRIES: usize = 500;
/// 日志级别，数值越大越严重（用于「该级别及以上」筛选）
pub const LEVELS: [&str; 4] = ["debug", "info", "warn", "error"];
/// 分类字典：与桌面端筛选下拉一致（key → 中文标签）
pub const CATEGORIES: [(&str, &str); 7] = [
    ("server", "服务"),
    ("auth", "登录"),
    ("account", "账号"),
    ("model", "模型"),
    ("upstream", "上游"),
    ("desensitize", "脱敏"),
    ("config", "配置"),
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
    directory: PathBuf,
    file: PathBuf,
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
    pub fn new(directory: impl AsRef<Path>) -> Self {
        let directory = directory.as_ref().to_path_buf();
        let file = directory.join(FILE_NAME);
        let store = Self {
            directory,
            file,
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

    /// 日志文件路径（桌面端「日志」页会直接展示它）。
    /// Node 版 `createLogStore` 返回对象里的 `file` 取值的对等物
    /// （stats() 的 `file` 字段已表达同一事实，这里保留供直读）。
    #[allow(dead_code)]
    pub fn file(&self) -> PathBuf {
        self.file.clone()
    }

    /// 载入历史：逐行解析，损坏行跳过（不影响其余日志）；
    /// 文件比内存上限大时标 dirty，下次写入收敛回上限。
    fn load(&self) {
        let Ok(text) = std::fs::read_to_string(&self.file) else {
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
        let entries: Vec<LogEntry> = if total > MAX_ENTRIES {
            parsed.split_off(total - MAX_ENTRIES)
        } else {
            parsed
        };
        let next_id = entries.iter().map(|item| item.id).max().unwrap_or(0) + 1;

        if let Ok(mut guard) = self.inner.lock() {
            guard.dirty = total > MAX_ENTRIES;
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
        if let Err(error) = std::fs::create_dir_all(&self.directory) {
            eprintln!("[Logs] 日志写入失败: {error}");
            return;
        }
        if let Err(error) = std::fs::write(&self.file, text) {
            eprintln!("[Logs] 日志写入失败: {error}");
        }
    }

    /// 追加一行（正常路径）
    fn write_append(&self, line: &str) {
        use std::io::Write;
        if let Err(error) = std::fs::create_dir_all(&self.directory) {
            eprintln!("[Logs] 日志写入失败: {error}");
            return;
        }
        let result = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.file)
            .and_then(|mut file| file.write_all(format!("{line}\n").as_bytes()));
        if let Err(error) = result {
            eprintln!("[Logs] 日志写入失败: {error}");
        }
    }

    /// 查询：按级别（含以上）、分类、关键词、起始 id 过滤，倒序返回最新在前。
    /// `limit` 语义与 Node 版一致：对过滤结果取**最后** N 条（即最新的 N 条）。
    pub fn query(&self, query: &Query) -> QueryResult {
        let Ok(guard) = self.inner.lock() else {
            return QueryResult { entries: Vec::new(), total: 0, matched: 0 };
        };
        let total = guard.entries.len();

        // 级别下限：与 Node 版一致，未知级别不参与过滤
        let min_rank = query
            .level
            .as_deref()
            .map(|value| value.trim().to_lowercase())
            .filter(|value| LEVELS.contains(&value.as_str()))
            .map(|value| level_rank(&value));

        // 分类：同样要求是已知分类才过滤
        let category = query
            .category
            .as_deref()
            .map(|value| value.trim().to_lowercase())
            .filter(|value| CATEGORIES.iter().any(|(key, _)| *key == *value));

        let keyword = query
            .keyword
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_lowercase);

        let filtered: Vec<&LogEntry> = guard
            .entries
            .iter()
            .filter(|item| match min_rank {
                Some(rank) => level_rank(&item.level) >= rank,
                None => true,
            })
            .filter(|item| match category.as_deref() {
                Some(want) => item.category == want,
                None => true,
            })
            .filter(|item| match keyword.as_deref() {
                Some(want) => item.message.to_lowercase().contains(want),
                None => true,
            })
            .filter(|item| match query.since_id {
                Some(from) => item.id > from,
                None => true,
            })
            .collect();

        let matched = filtered.len();
        // limit 缺省 200，并夹在 [1, MAX_ENTRIES]（对应 Node 版 Math.min/Math.max）
        let size = query.limit.unwrap_or(200).clamp(1, MAX_ENTRIES);
        let start = filtered.len().saturating_sub(size);
        let mut entries: Vec<LogEntry> = filtered[start..].iter().map(|item| (*item).clone()).collect();
        entries.reverse();
        QueryResult { entries, total, matched }
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
                file: self.file.to_string_lossy().to_string(),
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
            file: self.file.to_string_lossy().to_string(),
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
        if let Err(error) = std::fs::create_dir_all(&self.directory) {
            eprintln!("[Logs] 日志清空失败: {error}");
        } else if let Err(error) = std::fs::write(&self.file, "") {
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

/// `append` 的入参：借用了外部字符串，避免调用方为每条日志都构造 String。
pub struct NewEntry<'a> {
    pub level: &'a str,
    pub category: &'a str,
    pub message: &'a str,
    pub data: Option<&'a Value>,
    pub ts: Option<i64>,
}
