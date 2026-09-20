//! 调试模式的**上游原始报文**存储：`{debug_dir}/debug-traffic.jsonl`。
//!
//! ── 存什么（只有上游侧，不含下游）───────────────────────────
//! 开启调试模式（config.json 的 `debugMode`）后，每次转发在**即将发送前**
//! 抓一份 envelope：
//!   - 请求侧：上游 URL、**脱敏后的**请求头、实际发出去的请求体（已按家改写
//!     过 model 的最终形态）；
//!   - 响应侧：上游状态码、**脱敏后的**响应头、响应体（流式为上游原始 SSE
//!     文本，非流式为聚合前的原始文本）。
//! 下游侧（客户端发来的请求）**刻意不采** —— 那是 `write_debug_files` 已有的
//! 职责（`{config_dir}/debug/last-request.json`），两者不重叠。
//!
//! ── 为什么单独一个文件，不塞进 requests.jsonl ────────────────
//! 一次 SSE 响应动辄数百 KB，塞进请求日志会让明细文件迅速膨胀、拖慢每一页的
//! 读取。分开之后请求日志的读取成本与调试模式无关；详情按 id 到这里取。
//! 文件与请求日志用**同一个 id 关联**（`record::RequestEntry::id`）。
//!
//! ── 脱敏（**硬要求**，不可关闭）─────────────────────────────
//! 请求头里的 `authorization` / `cookie` / `x-api-key` 等凭据类字段一律替换成
//! `[redacted]`（见 [`redact_headers`]）。这不是可选项：日志文件是明文落盘的，
//! 不脱敏等于把各家账号的 token 抄在磁盘上。响应头里的 `set-cookie` 同理。
//!
//! ── 容量控制（两道闸）───────────────────────────────────────
//!   1. **单条上限** [`MAX_ENTRY_BYTES`]：请求体与响应体**各自**超过就截断并
//!      标记 `truncated` —— 一个超长响应不该让整个文件不可用；
//!   2. **总量上限** [`MAX_ENTRIES`]：超过后**丢弃最旧的一半**（与事件日志
//!      的 dirty + 整文件重写同一套路），保证长期开着调试模式也不会撑爆磁盘。
//!
//! ── 并发模型 ────────────────────────────────────────────────
//! 与 `logs_store` / `request_stats` 同构：一把 `Mutex` 包住「内存快照 + 落盘」，
//! 锁中毒 `poisoned.into_inner()` 继续用 —— 调试数据的完整性远不如「服务不因
//! 调试而崩」重要。**所有失败都不影响请求**：写盘失败只打一行日志。

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock, RwLock};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::server::logging;

/// 文件名（与 `requests.jsonl` 同目录约定，可被「保存位置」迁移）
const FILE_NAME: &str = "debug-traffic.jsonl";

/// **请求体、响应体各自**的最大字节数（序列化后计；两边独立，不共享额度）。
///
/// 取 2 MiB：一次带长上下文的请求 + 一个完整 SSE 响应通常远小于它，
/// 而真超了的多半是异常（上游把整段 HTML 错误页吐回来），截断比整条丢弃有用
/// —— 用户至少能看到前半段。截断处补 `truncated: true` 让界面标注。
///
/// 为什么不是「两边合计」：带长上下文的请求实测有 3 MB，共享额度会让响应那一半
/// 被挤成 0（详见 [`record`]）。各自独立后单条理论上限是它的两倍，但那是排障
/// 场景可接受的代价 —— 总量另有 [`MAX_ENTRIES`] 兜底。
pub const MAX_ENTRY_BYTES: usize = 2 * 1024 * 1024;

/// 文件里保留的最大条数。超过后丢最旧的一半（整文件重写）。
///
/// 取 500：与事件日志的内存上限同量级，够看「最近这批请求发生了什么」；
/// 调试模式是**临时排障**用的，不是长期归档 —— 真要长期留，用户该用
/// 「保存位置」把它挪到大盘上，而不是指望这里无限增长。
pub const MAX_ENTRIES: usize = 500;

/// 文件的**总量**上限，超过后从最旧的开始丢（直到回到上限内）。
///
/// 为什么条数闸之外还要一道字节闸：单条体积差着两个数量级 —— 一条不带上下文
/// 的请求几十 KB，一条带长上下文的（实测）请求体 3 MB + 响应 1.4 MB。只看条数
/// 的话，500 条最坏能到 GB 级，而 [`init`] 是**整文件读进内存**的，启动时就会
/// 明显卡顿甚至吃爆内存。
///
/// 取 64 MiB：够装几十条典型的完整往返（正是排障要看的那批），读取耗时也在
/// 百毫秒级。真要留更多，说明这不是「临时排障」而是归档需求，该走导出。
pub const MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;

/// 凭据类请求头：命中即替换成 `[redacted]`（大小写不敏感）。
///
/// 名单在 OmniProxy 的基础上补齐了**我们五家用到的自定义头**：各家适配器
/// 会在头里带 token / cookie / 签名（见 `providers/*/adapter.rs` 的
/// `build_chat_request`）。宁可多脱几个（多脱只损失排障信息，少脱是事故）。
const SENSITIVE_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "cookie2",
    "set-cookie",
    "x-api-key",
    "api-key",
    "x-auth-token",
    "x-access-token",
    "x-refresh-token",
    "x-ide-token",
    "x-ide-auth",
    "x-workbuddy-token",
    "x-raccoon-token",
    "x-cline-token",
    "x-qoder-token",
    "x-catpaw-cookie",
    "x-autoclaw-token",
];

/// 脱敏占位符（与 OmniProxy 同字面量，便于对照两边的日志）
pub const REDACTED: &str = "[redacted]";

/// 一条原始报文记录（一行 JSON）。
///
/// 字段用 `Option` 表达「这一侧没采到」：请求发不出去时没有响应侧，
/// 关闭调试模式时整条不写。`id` 是关联键 —— 与请求日志的 `id` 同值。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrafficEntry {
    /// 关联键：请求日志条目 id（同一请求在两边同 id）
    pub id: String,
    /// 记录时刻（毫秒 Unix 时间戳）
    pub ts: i64,
    /// 上游 URL（含路径；查询串原样保留）
    #[serde(default)]
    pub url: String,
    /// 实际承载的 provider id
    #[serde(default)]
    pub provider: String,
    /// 发给上游的请求头（**已脱敏**）
    #[serde(default)]
    pub request_headers: Value,
    /// 发给上游的请求体（已按家改写过 model 的最终形态）
    #[serde(default)]
    pub request_body: Value,
    /// 上游响应状态码（未发出 / 未收到响应时缺失）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// 上游响应头（**已脱敏**）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_headers: Option<Value>,
    /// 上游响应体：流式为原始 SSE 文本，非流式为聚合前的原始文本
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_body: Option<String>,
    /// 响应体是否被截断（超过 [`MAX_ENTRY_BYTES`]）
    #[serde(default)]
    pub truncated: bool,
}

/// 头集合脱敏：凭据类字段换成 [`REDACTED`]，其余原样。
///
/// 收 `Vec<(String, String)>`（适配器给出的形态）与 `HeaderMap`（reqwest 响应）
/// 两种来源，统一输出 JSON 对象 —— 重名头按**后到覆盖**（与 HTTP 语义一致，
/// 且这两种来源里重名极少见）。
pub fn redact_headers<'a>(headers: impl IntoIterator<Item = (&'a str, &'a str)>) -> Value {
    let mut object = serde_json::Map::new();
    for (name, value) in headers {
        let lower = name.to_ascii_lowercase();
        let shown = if SENSITIVE_HEADERS.contains(&lower.as_str()) {
            REDACTED
        } else {
            value
        };
        object.insert(lower, Value::String(shown.to_string()));
    }
    Value::Object(object)
}

/// 进程级存储句柄（与 `logging::store_ref` 同一模式：可空 = 未启用）
static STORE: OnceLock<Mutex<TrafficStore>> = OnceLock::new();
/// 保存目录。包 `RwLock` 是为了**运行中换目录**（设置页的「保存位置」迁移）。
static DIRECTORY: RwLock<Option<PathBuf>> = RwLock::new(None);

struct TrafficStore {
    /// 内存快照，恒按 ts 升序（追加即有序 —— 单进程内时间单调）
    entries: VecDeque<TrafficEntry>,
    /// 上述条目的落盘体积合计（字节闸用）。
    ///
    /// 增量维护而不是每次重算：重算要把最多 64 MiB 全部序列化一遍，而 [`record`]
    /// 跑在请求路径上（流式响应结束时随采集器 drop 触发），那点延迟会直接体现
    /// 在客户端等待里。
    bytes: usize,
}

impl TrafficStore {
    fn new() -> Self {
        Self { entries: VecDeque::new(), bytes: 0 }
    }

    fn file(&self) -> Option<PathBuf> {
        directory().map(|dir| dir.join(FILE_NAME))
    }

    /// 追加一条并同步体积计数
    fn push(&mut self, entry: TrafficEntry) {
        self.bytes += entry_size(&entry);
        self.entries.push_back(entry);
    }

    /// 丢掉最旧的一条并同步体积计数
    fn pop_front(&mut self) -> Option<TrafficEntry> {
        let entry = self.entries.pop_front();
        if let Some(entry) = entry.as_ref() {
            self.bytes = self.bytes.saturating_sub(entry_size(entry));
        }
        entry
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }
}

/// 一条记录的落盘体积（含换行）
fn entry_size(entry: &TrafficEntry) -> usize {
    serde_json::to_string(entry).map(|line| line.len() + 1).unwrap_or(0)
}

/// 当前保存目录（未初始化时 None）
fn directory() -> Option<PathBuf> {
    DIRECTORY.read().ok().and_then(|guard| guard.clone())
}

/// 初始化存储：设定目录并载入已有内容（启动时调用一次；重复调用幂等）。
///
/// 载入失败（文件不存在 / 某行损坏）不报错：跳过坏行继续 —— 调试数据不值得
/// 让网关起不来。文件超过上限时只保留最后那些（条数与总量两道闸都过一遍，
/// 与 [`record`] 同口径）；**超限时立刻回写一次**，否则旧文件会一直躺在盘上
/// 占着空间，直到下次有新请求才被重写。
pub fn init(dir: PathBuf) {
    if let Ok(mut guard) = DIRECTORY.write() {
        *guard = Some(dir);
    }
    let store = STORE.get_or_init(|| Mutex::new(TrafficStore::new()));
    let mut guard = lock(store);
    guard.clear();
    let Some(file) = guard.file() else {
        return;
    };
    let Ok(text) = std::fs::read_to_string(&file) else {
        return;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<TrafficEntry>(line) {
            Ok(entry) => guard.push(entry),
            // 坏行跳过（见函数说明）
            Err(_) => continue,
        }
    }
    let mut trimmed = false;
    while guard.entries.len() > MAX_ENTRIES {
        guard.pop_front();
        trimmed = true;
    }
    while guard.bytes > MAX_TOTAL_BYTES && !guard.entries.is_empty() {
        guard.pop_front();
        trimmed = true;
    }
    if trimmed {
        if let Err(error) = write_all(&file, &guard.entries) {
            logging::verbose("[Debug]", &format!("调试报文启动裁剪失败: {error}"));
        }
    }
}

/// 换保存目录并搬走已有数据（设置页「保存位置」的迁移用）。
///
/// 与 `RequestStats::relocate` 同一语义：**先搬文件、成功才切目录** ——
/// 搬迁失败时旧目录与旧数据原封不动。目标目录不存在时创建。
pub fn relocate(dir: &Path) -> Result<(), String> {
    let store = STORE.get_or_init(|| Mutex::new(TrafficStore::new()));
    let guard = lock(store);
    let old_file = guard.file();
    if let Err(error) = std::fs::create_dir_all(dir) {
        return Err(format!("创建目录失败: {error}"));
    }
    let new_file = dir.join(FILE_NAME);
    if let Some(old_file) = old_file.as_deref() {
        if old_file != new_file && old_file.exists() {
            std::fs::rename(old_file, &new_file)
                .or_else(|_| {
                    // 跨盘符 rename 会失败：退化成「拷贝 + 删源」
                    std::fs::copy(old_file, &new_file).map(|_| ()).and_then(|_| std::fs::remove_file(old_file))
                })
                .map_err(|error| format!("搬迁文件失败: {error}"))?;
        }
    }
    if let Ok(mut guard) = DIRECTORY.write() {
        *guard = Some(dir.to_path_buf());
    }
    Ok(())
}

/// 文件路径（设置页概况用）
pub fn file() -> Option<PathBuf> {
    STORE.get().and_then(|store| lock(store).file())
}

/// 条数（设置页概况用）
pub fn count() -> usize {
    STORE.get().map(|store| lock(store).entries.len()).unwrap_or(0)
}

/// 落一条记录（**失败只打日志，绝不影响请求**）。
///
/// 容量控制在这里做，两道闸（见 [`MAX_ENTRIES`] / [`MAX_TOTAL_BYTES`]）任一
/// 触发都丢最旧的一半并整文件重写。
///
/// ── 截断的口径：请求体与响应体**各自独立**───────────────
/// 早先的实现是「响应体的额度 = 总上限 - 请求体大小」，结果是大上下文的请求
/// （实测有 3 MB 的请求体）会把响应体的额度挤成 0 —— 明明采到了完整响应，
/// 存下来只剩 1 个字符，正是最需要看的那种请求反而什么都看不到。
/// 现在两边各按 [`MAX_ENTRY_BYTES`] 独立截断：一个超大请求不该吃掉响应那一半。
/// 极端情况下单条可达两倍上限，总量由 [`MAX_TOTAL_BYTES`] 兜底。
pub fn record(mut entry: TrafficEntry) {
    let Some(store) = STORE.get() else {
        return;
    };
    let mut guard = lock(store);
    // 请求体与响应体各自按上限截断（按字符截，避免切断 UTF-8）
    let request_len = entry.request_body.to_string().len();
    if request_len > MAX_ENTRY_BYTES {
        entry.request_body = truncate_json_value(&entry.request_body, MAX_ENTRY_BYTES);
        entry.truncated = true;
    }
    if let Some(body) = entry.response_body.as_ref() {
        if body.len() > MAX_ENTRY_BYTES {
            entry.response_body = Some(truncate_chars(body, MAX_ENTRY_BYTES));
            entry.truncated = true;
        }
    }
    let overflow = guard.entries.len() >= MAX_ENTRIES
        || guard.bytes.saturating_add(entry_size(&entry)) > MAX_TOTAL_BYTES;
    if overflow {
        // 丢最旧的一半：整文件重写一次（与 logs_store 的 dirty 模式同套路）
        let keep = guard.entries.len() / 2;
        while guard.entries.len() > keep {
            guard.pop_front();
        }
    }
    guard.push(entry);
    let Some(file) = guard.file() else {
        return;
    };
    if overflow {
        if let Err(error) = write_all(&file, &guard.entries) {
            logging::verbose("[Debug]", &format!("调试报文重写失败: {error}"));
        }
        return;
    }
    // 常规路径：追加一行
    let Some(last) = guard.entries.back() else {
        return;
    };
    match serde_json::to_string(last) {
        Ok(line) => {
            if let Err(error) = append_line(&file, &line) {
                logging::verbose("[Debug]", &format!("调试报文写入失败: {error}"));
            }
        }
        Err(error) => logging::verbose("[Debug]", &format!("调试报文序列化失败: {error}")),
    }
}

/// 按 id 取一条（请求日志页的「详情」列用）
pub fn get(id: &str) -> Option<TrafficEntry> {
    let store = STORE.get()?;
    let guard = lock(store);
    guard.entries.iter().rev().find(|entry| entry.id == id).cloned()
}

/// 清空（设置页 / 请求日志「清空」时一并调用）
pub fn clear() {
    let Some(store) = STORE.get() else {
        return;
    };
    let mut guard = lock(store);
    guard.clear();
    if let Some(file) = guard.file() {
        let _ = std::fs::remove_file(file);
    }
}

/// 按字符截断（不切断 UTF-8；超长处补 `…`）
fn truncate_chars(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    // 按字节上限取一个安全的字符边界：从上限往回退到非续字节
    let mut end = max_bytes.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = text[..end].to_string();
    out.push('…');
    out
}

/// JSON 值超限时的截断：**在结构内截**，不把 JSON 文本切成半截。
///
/// 直接对 `to_string()` 的结果做字符串截断会产出非法 JSON（前端 `JSON.parse`
/// 失败，详情弹窗直接报错）。所以按值的类型分别处理，且守一条总原则：
/// **键一个都不丢，只压缩装不下的那些值** —— 「哪个字段有多大」本身就是排障
/// 信息（比如 tools 占了 140 KB），丢掉整个键比丢掉它的后半段更糟。
///
///   - 对象：小值（≤ 均分线）原样保留，大值平分「扣掉小值后剩下的额度」；
///   - 数组：头部装到 3/4 额度、尾部再装 1/4 —— 头部是 system 提示词与早期
///     上下文，尾部是**最新那几条**（往往正是要看的东西），中间丢掉的部分用
///     一条说明字符串标出；若头部一条都装不下（首元素就超了 3/4），说明这个
///     数组整体就是那一个大元素，改为截断它本身；
///   - 字符串：按字符截；
///   - 其它（数字 / 布尔 / null）：不会超限，原样返回。
///
/// 说明性内容用**中文短句**而不是省略号：它在详情弹窗里是可见的，要让用户
/// 一眼看出「这里被截过」，而不是疑惑报文怎么长这样。结果体积可能略超上限
/// （说明键与括号开销没算进额度），不影响用途。
fn truncate_json_value(value: &Value, max_bytes: usize) -> Value {
    const NOTE: &str = "…（内容过大，调试模式已截断）";
    match value {
        Value::String(text) => Value::String(truncate_chars(text, max_bytes)),
        Value::Object(map) => {
            let sizes: Vec<usize> = map.values().map(|item| item.to_string().len()).collect();
            if sizes.iter().sum::<usize>() <= max_bytes {
                return value.clone();
            }
            // 均分线：超过它的值必须压缩；没超过的原样保留，不参与分摊
            let fair = max_bytes / map.len().max(1);
            let small: usize = sizes.iter().filter(|size| **size <= fair).sum();
            let big = sizes.iter().filter(|size| **size > fair).count().max(1);
            let budget = max_bytes.saturating_sub(small) / big;
            let mut kept = serde_json::Map::new();
            for (key, item) in map {
                let fitted = if item.to_string().len() > fair {
                    truncate_json_value(item, budget.max(1024))
                } else {
                    item.clone()
                };
                kept.insert(key.clone(), fitted);
            }
            kept.insert("_truncated".to_string(), Value::String(NOTE.to_string()));
            Value::Object(kept)
        }
        Value::Array(items) => {
            let head_budget = max_bytes * 3 / 4;
            let mut kept: Vec<Value> = Vec::new();
            let mut used = 2;
            for item in items {
                let size = item.to_string().len() + 1;
                if used + size > head_budget {
                    break;
                }
                used += size;
                kept.push(item.clone());
            }
            if kept.is_empty() {
                if let Some(first) = items.first() {
                    kept.push(truncate_json_value(first, max_bytes.saturating_sub(2)));
                    kept.push(Value::String(NOTE.to_string()));
                }
                return Value::Array(kept);
            }
            // 尾部从最后往前装（不能越过头部已占的那些）
            let mut tail: Vec<Value> = Vec::new();
            for item in items.iter().rev() {
                if kept.len() + tail.len() >= items.len() {
                    break;
                }
                let size = item.to_string().len() + 1;
                if used + size > max_bytes {
                    break;
                }
                used += size;
                tail.push(item.clone());
            }
            if kept.len() + tail.len() < items.len() {
                kept.push(Value::String(NOTE.to_string()));
            }
            tail.reverse();
            kept.extend(tail);
            Value::Array(kept)
        }
        other => other.clone(),
    }
}

fn append_line(file: &Path, line: &str) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut handle = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)?;
    handle.write_all(line.as_bytes())?;
    handle.write_all(b"\n")?;
    Ok(())
}

fn write_all(file: &Path, entries: &VecDeque<TrafficEntry>) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut text = String::new();
    for entry in entries {
        match serde_json::to_string(entry) {
            Ok(line) => {
                text.push_str(&line);
                text.push('\n');
            }
            Err(_) => continue,
        }
    }
    let mut handle = std::fs::File::create(file)?;
    handle.write_all(text.as_bytes())
}

/// 锁中毒继续用（与 `logs_store` / `request_stats` 同一取向，见模块头）
fn lock(store: &Mutex<TrafficStore>) -> MutexGuard<'_, TrafficStore> {
    store.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 请求日志「详情」的响应形态：原始报文 + 请求日志的关联字段。
///
/// 由 `api::stats_api` 组装（本模块只给报文那半）。
pub fn detail_payload(entry: &TrafficEntry) -> Value {
    json!({
        "id": entry.id,
        "ts": entry.ts,
        "url": entry.url,
        "provider": entry.provider,
        "requestHeaders": entry.request_headers,
        "requestBody": entry.request_body,
        "status": entry.status,
        "responseHeaders": entry.response_headers,
        "responseBody": entry.response_body,
        "truncated": entry.truncated,
    })
}

// ─── 采集器（转发层用）────────────────────────────────────────

/// 一次转发的原始报文采集器。
///
/// ── 生命周期 ────────────────────────────────────────────────
/// 请求侧在**即将发送前**创建（此时 URL / 头 / 体都已定稿），响应侧在收到
/// 响应头时补上状态码与头，响应体随字节流累积，最后在**流结束或 drop 时**
/// 落盘（[`Drop`]）。用 Drop 而不是「结束时显式调用」的理由与
/// `RecordingStream` 相同：客户端断开、上游断流、请求被取消三条路径都不会
/// 走到「正常结束」，只靠显式调用会丢掉那些最需要看的失败现场。
///
/// ── 为什么共享 `Arc` ────────────────────────────────────────
/// 流式路径里采集器要跟着 `ForwardStream` 走（它可能在 handler 返回后才被
/// axum 拉取完），非流式路径里跟着聚合函数走。两条路径都持有同一个
/// `Arc<TrafficCapture>`，谁最后 drop 谁落盘。
pub struct TrafficCapture {
    state: Mutex<CaptureState>,
}

struct CaptureState {
    entry: TrafficEntry,
    /// 响应体累积（落盘时移到 entry.response_body）
    body: String,
    /// 是否真的发过上游请求（`reset_request` 置位）。
    ///
    /// 未置位 = 请求在到达转发层之前就失败了（没有可用账号、模型不存在…），
    /// 那种情况**不落盘**：报文里除了 id 与时刻什么都没有，留着只会让
    /// 「详情」列表混进一堆空条目。
    sent: bool,
    /// 已落盘（保证只写一次）
    done: bool,
}

impl TrafficCapture {
    /// 请求侧：转发开始时创建，只带 id（URL / 头 / 体等真正发送时由
    /// [`Self::reset_request`] 填上 —— 那时它们才定稿）。
    pub fn begin(id: &str) -> Self {
        Self {
            state: Mutex::new(CaptureState {
                entry: blank_entry(id, "", "", &[], &Value::Null),
                body: String::new(),
                sent: false,
                done: false,
            }),
        }
    }

    /// 重置为一次新的尝试（同一条请求内的退避重试 / 401 刷新重试）。
    ///
    /// **最后一次为准**（与 OmniProxy 的「重试覆盖为最后一次尝试」同口径）：
    /// 重试时把上一轮的响应现场清掉，用户看到的是最终真正生效的那次往返，
    /// 而不是「第一次失败 + 第二次成功」混在一起的两段 body。
    pub fn reset_request(
        &self,
        url: &str,
        provider: &str,
        headers: &[(String, String)],
        body: &Value,
    ) {
        let mut guard = self.lock();
        guard.entry = blank_entry(&guard.entry.id.clone(), url, provider, headers, body);
        guard.body.clear();
        // 走到这里说明请求体已构造、即将发出 —— 这条报文有内容可落
        guard.sent = true;
        // done 不重置：本条请求只写一行，重试不产生新条目
    }

    /// 响应头到达时补上状态码与响应头（**必须在 consume response 之前调**）。
    ///
    /// 顺带清空已累积的响应体：重试场景下上一次尝试的 body 不该混进来
    /// （见 [`Self::reset_request`] 的「最后一次为准」）。
    pub fn attach_response(&self, status: u16, headers: &reqwest::header::HeaderMap) {
        let mut guard = self.lock();
        guard.entry.status = Some(status);
        guard.body.clear();
        let pairs = headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.to_str().unwrap_or("[binary]")));
        guard.entry.response_headers = Some(redact_headers(pairs));
    }

    /// 响应体分片（流式逐 chunk、非流式逐 chunk 都走这里）。
    ///
    /// 累积到单条上限就不再追加（`truncated` 标记由落盘时的检查补）——
    /// 超长响应不该让内存无上限增长。
    pub fn push(&self, chunk: &[u8]) {
        let mut guard = self.lock();
        if guard.body.len() >= MAX_ENTRY_BYTES {
            return;
        }
        guard.body.push_str(&String::from_utf8_lossy(chunk));
    }

    /// 落盘（幂等：第二次调用什么都不做）。
    ///
    /// 没真发过请求（`sent` 为假）时直接丢弃：那种报文除了 id 什么都没有
    /// （见 `CaptureState::sent` 的说明）。
    pub fn finish(&self) {
        let mut guard = self.lock();
        if guard.done {
            return;
        }
        guard.done = true;
        if !guard.sent {
            return;
        }
        let body = std::mem::take(&mut guard.body);
        if !body.is_empty() {
            guard.entry.response_body = Some(body);
        }
        record(guard.entry.clone());
    }

    fn lock(&self) -> MutexGuard<'_, CaptureState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Drop for TrafficCapture {
    fn drop(&mut self) {
        self.finish();
    }
}

/// 一条空白记录的构造（`begin` / `reset_request` 共用）
fn blank_entry(
    id: &str,
    url: &str,
    provider: &str,
    headers: &[(String, String)],
    body: &Value,
) -> TrafficEntry {
    let pairs = headers
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()));
    TrafficEntry {
        id: id.to_string(),
        ts: logging::now_ms(),
        url: url.to_string(),
        provider: provider.to_string(),
        request_headers: redact_headers(pairs),
        request_body: body.clone(),
        status: None,
        response_headers: None,
        response_body: None,
        truncated: false,
    }
}

/// 调试模式是否开启（转发层的**唯一**判定入口）。
///
/// 每次调用都读配置快照（`config::current()` 是 RwLock 读 + 结构体克隆）：
/// 开关改完下一个请求就生效，不必重启。转发热路径上多一次读锁是可接受的
/// —— 与「每次重试判定都读重试设置」同一取向（见 `config::RetrySettings`）。
pub fn enabled() -> bool {
    crate::server::config::current().debug_mode()
}


