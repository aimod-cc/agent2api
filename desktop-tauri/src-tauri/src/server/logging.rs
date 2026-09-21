//! 双通道日志：控制台 + 运行日志库（桌面端「日志」页）。
//!
//! 对照 Node 版 server.mjs 174-240 行的 emit/log/logEvent：
//!   - 控制台保持原格式 `[HH:mm:ss.SSS] TAG text`，时间用 **UTC**
//!     （对齐 Node 的 `new Date().toISOString().slice(11, 23)`，不是本地时间）
//!   - 同时追加到日志库，级别由标签/文案推断（inferLevel）
//!   - `verbose` 级别（debug）只有开了 --verbose 才入库，避免刷屏
//!
//! 为什么用 `eprintln!` 而不是 `println!`：本项目是 GUI 程序，Windows 下
//! 没有控制台时 `println!` 可能触发 panic（release 是 panic=abort，
//! 会直接带走整个桌面应用）。stderr 的写入失败只会被忽略，是安全通道。
//!
//! 全局实例：日志库在启动时初始化一次，之后所有模块共用（Node 版同样是
//! 一个模块级 logStore 常量）。用 `OnceLock` 而不是 `Mutex<Option<...>>`，
//! 读路径无锁；写入的串行化由 LogStore 内部的锁负责。

use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{TimeZone, Utc};
use serde_json::Value;

use crate::server::db::Db;
use crate::server::logs_store::{LogEntry, LogStore, NewEntry};

/// 标签 → 日志分类，与桌面端筛选下拉一致（照抄 Node 版 TAG_CATEGORY）。
/// 后续切片使用新标签时必须在这里登记，否则会落到默认分类 server。
///
/// 定时任务三分类（自动签到 / 凭证自动维护 / 软件版本检查）是**新增**的分类：
/// 登记前 `[Checkin]` / `[Maintenance]` / `[Update]` 都落到 server —— 历史条目
/// 保持原分类不动，分类筛选只对今后落库的条目生效。
const TAG_CATEGORY: &[(&str, &str)] = &[
    ("[Server]", "server"),
    ("[Init]", "server"),
    ("[Config]", "config"),
    ("[Security]", "config"),
    ("[Login]", "auth"),
    ("[Auth]", "auth"),
    ("[Accounts]", "account"),
    ("[Models]", "model"),
    ("[Model]", "model"),
    ("[Upstream]", "upstream"),
    ("[Desensitize]", "desensitize"),
    ("[Logs]", "server"),
    ("[Billing]", "upstream"),
    ("[Activity]", "upstream"),
    ("[Checkin]", "checkin"),
    ("[Maintenance]", "maintenance"),
    ("[Update]", "update"),
    // [HTTP] 为每个入站请求的 verbose 日志，类别与服务日志相同
    ("[HTTP]", "server"),
];

/// 进程内唯一的日志库实例。启动时由 `init_store` 装入。
static STORE: OnceLock<LogStore> = OnceLock::new();

/// 是否需要把 debug 级别也上报入库（对应 Node 版 `opts.verbose`）
static VERBOSE: OnceLock<bool> = OnceLock::new();

/// 初始化日志库。重复调用只生效一次（OnceLock 语义），返回是否本次装入成功。
///
/// 在 `ServerState::bootstrap` 里调用 —— 数据库就绪之后、其余模块之前，
/// 之后其它模块再写日志就能入库。
///
/// ── 参数为什么从 `directory` 改成 `Db` ─────────────────────
/// 日志数据现在在统一库的 `logs` 表里（本切片从 `logs.jsonl` 迁过来），
/// 不再有「日志自己的目录」。与 T2 的 `AccountStore::with_db(db)` 同一形态：
/// 路径由 `Db` 唯一持有，日志库只是它的一个使用者。
/// 接 `Option<Db>` 是因为 `ServerState::bootstrap` 手里就是 `Option<Db>`
/// （库打不开时仍要能启动）：`None` 时日志库照常装起来，但写入静默丢弃、
/// 读取返回空 —— 降级细节见 `LogStore` 各方法的说明。
///
/// 保留天数走**回调**（每次裁剪时动态取 `config::retention_settings()`）：
/// 于是设置页改完天数，下一次写日志 / 显式 prune 就生效，不需要重启进程
/// （与 `RequestStats` 的 `get_retention` 同一模式）。
/// 读的是配置的**内存快照**而不是每次读盘 —— 写日志是相对频繁的路径。
pub fn init_store(db: Option<Db>, verbose: bool) -> bool {
    let _ = VERBOSE.set(verbose);
    if STORE.get().is_some() {
        return false;
    }
    let _ = STORE.set(LogStore::with_db(db, || {
        crate::server::config::retention_settings().log_days
    }));
    true
}

/// 取全局日志库；未初始化时返回 None（此时只有控制台通道）
fn store() -> Option<&'static LogStore> {
    STORE.get()
}

/// 是否开启 verbose（默认关闭，与 Node 版一致）
pub fn is_verbose() -> bool {
    *VERBOSE.get().unwrap_or(&false)
}

/// 当前毫秒 Unix 时间戳（对应 Node 版 `Date.now()`）。
/// 日志条目的 `ts` 字段用它；时间戳只用于展示与排序，不能用它做时间基准。
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// 控制台时间前缀：`[HH:mm:ss.SSS]`，**UTC**（对齐 Node 的 toISOString）。
fn timestamp_prefix() -> String {
    match Utc.timestamp_millis_opt(now_ms()).single() {
        Some(now) => now.format("[%H:%M:%S%.3f]").to_string(),
        None => "[--:--:--.---]".to_string(),
    }
}

/// 标签 → 分类；未登记的标签落到 server（对应 Node 版 `TAG_CATEGORY[tag] || 'server'`）
fn category_of(tag: &str) -> &'static str {
    TAG_CATEGORY
        .iter()
        .find(|(name, _)| *name == tag)
        .map(|(_, category)| *category)
        .unwrap_or("server")
}

/// 从标签与消息推断级别，照抄 Node 版 inferLevel 的正则语义，但不引入 regex 依赖：
///   - 消息以 ❌ 开头 / 含「失败」/ 标签以 ❌ 开头 → error
///   - 消息或标签以 ⚠️ 开头 → warn
///   - 其余 info
fn infer_level(tag: &str, text: &str) -> &'static str {
    // Node 的 /\b失败\b/ 在中文里等价于「含失败二字」（中文两侧都是非单词字符）
    if text.starts_with('❌') || text.contains("失败") || tag.starts_with('❌') {
        return "error";
    }
    if text.starts_with('⚠') || tag.starts_with('⚠') {
        return "warn";
    }
    "info"
}

/// 只写控制台（不写日志库）。日志库自身出错时用它，
/// 否则「写日志失败」这件事会再触发一次写日志，形成递归。
pub fn console_line(tag: &str, text: &str) {
    eprintln!("{} {tag} {text}", timestamp_prefix());
}

/// 双通道日志（对应 Node 版 log()）。
///
/// 入库前会做与 Node 版相同的处理：把各段参数用空格连接后 trim，
/// 空文案直接跳过（只打控制台）。级别由 infer_level 推断。
pub fn log(tag: &str, text: &str) {
    console_line(tag, text);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return;
    }
    let Some(store) = store() else {
        return;
    };
    // 与 Node 版一致：message 为「标签 + 文案」，前端列表里能直接看到来源
    let message = format!("{tag} {trimmed}");
    store.append(NewEntry {
        level: infer_level(tag, trimmed),
        category: category_of(tag),
        message: &message,
        data: None,
        ts: None,
    });
}

/// 显式指定级别的日志，其余行为与 `log()` 完全一致。
///
/// 汇总类文案（「成功 X，跳过 Y，失败 Z」）的级别必须由**结果**决定而不是
/// 靠文案推断：失败数为 0 的正常轮次里也有「失败」二字，`infer_level`
/// 会把它误判成 error（导航徽标只统计 error，等于一跑定时任务就亮红标）。
/// 调用方手里就有失败数，按它选 `"info"` / `"error"` 即可。
pub fn log_with_level(tag: &str, text: &str, level: &str) {
    console_line(tag, text);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return;
    }
    let Some(store) = store() else {
        return;
    };
    let message = format!("{tag} {trimmed}");
    store.append(NewEntry {
        level,
        category: category_of(tag),
        message: &message,
        data: None,
        ts: None,
    });
}

/// 调试日志：只有开关打开时才入库（对应 Node 版 verbose()）。
/// 控制台始终输出，方便排障时用普通启动也能看到细节。
pub fn verbose(tag: &str, text: &str) {
    console_line(tag, text);
    if !is_verbose() {
        return;
    }
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return;
    }
    let Some(store) = store() else {
        return;
    };
    let message = format!("{tag} {trimmed}");
    store.append(NewEntry {
        level: "debug",
        category: category_of(tag),
        message: &message,
        data: None,
        ts: None,
    });
}

/// 结构化事件上报（对应 Node 版 logEvent）。
///
/// 429 自动切换这类需要带字段的事件走这里，桌面端日志页能按 category/data
/// 精确展示与筛选（logs-panel.js 读 `data.from` / `data.to` / `data.resetAtText`）。
///
/// `message` 为空时返回 None 且不落库。控制台不重复打印 —— 这类事件
/// 调用方通常已经用 `log()` 打过一行可读文案了。
pub fn log_event(
    level: &str,
    category: &str,
    message: &str,
    data: Option<Value>,
) -> Option<LogEntry> {
    let store = store()?;
    store.append(NewEntry {
        level,
        category,
        message,
        data: data.as_ref(),
        ts: None,
    })
}

/// 供路由层读取日志库（查询/统计/清空）。未初始化时返回 None，
/// 路由层据此返回 503「日志模块未启用」，与 Node 版行为一致。
pub fn store_ref() -> Option<&'static LogStore> {
    store()
}
