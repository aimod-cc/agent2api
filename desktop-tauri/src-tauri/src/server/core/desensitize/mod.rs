//! 内容脱敏（对照 Node 版 src/workbuddy-desensitize.mjs 全量移植）。
//!
//!   engine.rs  纯函数：词表清洗、匹配构造、文本改写、content/messages/body 遍历
//!   mod.rs     状态化：词表读写、默认词表迁移、命中统计（本文件）
//!
//! ── 与 Node 版的数据契约 ────────────────────────────────────
//! 词表持久化在 `{config_dir}/desensitize.json`，字段顺序与缩进**逐字节对齐**
//! Node 的 `JSON.stringify({enabled,terms,roles,defaultsVersion}, null, 2) + "\n"`：
//! 两边写出的文件内容完全一致（serde_json 的 Map 会按字母序输出，所以这里手工拼）。
//!
//! ── 并发模型 ────────────────────────────────────────────────
//! 句柄内部一把 `RwLock`，克隆共享同一份状态（与 ModelCatalog 同构）。
//! 硬约束：**热路径不持锁做重活** —— 转发时会话先克隆出 `Arc<TermMatcher>`
//! 快照再放锁，脱敏计算全程在锁外，最后只在写锁里合并一次计数。
//! 写盘（低频的用户操作）在写锁内完成，与 Node 单线程下的「改内存 → 写盘」
//! 语义等价：不会出现两个并发请求把旧快照写回去的情况。

mod engine;

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};

use serde_json::{json, Value};

use crate::server::logging;

// 只再导出有实际调用点的符号（ZWSP / VALID_ROLES / desensitize_text /
// desensitize_messages 是引擎内部与文档层面的 API，外部暂不需要，
// 留在 engine.rs 里即可 —— 多余再导出在 private 模块里会报 unused import）
pub use engine::{
    compile_terms, desensitize_body, normalize_roles, normalize_terms, utf16_len, Counter,
    TermMatcher, DEFAULT_ROLES, DEFAULT_TERM_MIGRATIONS, DEFAULT_TERMS, DEFAULT_TERMS_VERSION,
    FILE_NAME, MAX_TERM_LENGTH, MAX_TERMS,
};
/// 词/角色归一化用的 JS trim（删词路径复用，保证「加得进 == 删得掉」）
use engine::js_trim;

/// 统计快照里 topTerms 的默认条数（对应 Node 的 `statsSnapshot({top = 20})`）
const TOP_TERMS: usize = 20;

/// 命中统计（对应 Node 的 stats 闭包变量）。
///
/// `term_hits` 按首次命中顺序保存，排序用稳定排序 —— 与 Node 的
/// `[...map.entries()].sort((a,b)=>b[1]-a[1])` 完全一致（同次数时保持首次顺序）。
#[derive(Default)]
struct Stats {
    requests: usize,
    changed_requests: usize,
    total_hits: usize,
    term_hits: Vec<(String, usize)>,
}

impl Stats {
    /// 按命中次数降序取前 N 条
    fn top(&self, limit: usize) -> Vec<(String, usize)> {
        let mut list = self.term_hits.clone();
        list.sort_by(|left, right| right.1.cmp(&left.1));
        list.truncate(limit);
        list
    }

    fn snapshot(&self) -> Value {
        json!({
            "requests": self.requests,
            "changedRequests": self.changed_requests,
            "totalHits": self.total_hits,
            "topTerms": self
                .top(TOP_TERMS)
                .into_iter()
                .map(|(term, count)| json!({ "term": term, "count": count }))
                .collect::<Vec<_>>(),
        })
    }
}

/// 脱敏器内部状态（对应 Node 的闭包变量）
struct Inner {
    enabled: bool,
    terms: Vec<String>,
    roles: Vec<String>,
    /// 编译后的匹配器；词表为空时为 None（调用方据此跳过脱敏）
    matcher: Option<Arc<TermMatcher>>,
    /// 已合并到的默认词表版本（可能带小数：Node 存的是 Number）
    defaults_version: f64,
    file: PathBuf,
    stats: Stats,
}

/// 一次请求体处理的产物（对应 Node 的 `processBody` 返回）
pub struct ProcessOutcome {
    pub changed: bool,
    pub hits: usize,
    /// 命中的词列表（无计数，保持向后兼容）。
    /// Node 版 `processBody` 返回的 `matched` 对等物：api 层当前只用 term_counts
    /// 拼日志，这里保留完整三个字段以便排障时逐个对照 Node 的返回。
    #[allow(dead_code)]
    pub matched: Vec<String>,
    /// 按命中次数降序的每词次数
    pub term_counts: Vec<(String, usize)>,
}

/// 脱敏服务句柄：内部一把 `RwLock`，克隆共享同一份状态。
#[derive(Clone)]
pub struct Desensitizer {
    inner: Arc<RwLock<Inner>>,
}

impl Desensitizer {
    /// 构造脱敏器并读盘（对应 `createDesensitizer({directory, log})`）。
    ///
    /// 默认启用（无配置文件时即默认开启），词表为内置默认词表。
    pub fn new(directory: PathBuf) -> Self {
        let file = directory.join(FILE_NAME);
        let terms: Vec<String> = DEFAULT_TERMS.iter().map(|term| term.to_string()).collect();
        // 构造即编译一次默认词表：load() 在「没有词表文件」时直接返回，
        // 若这里留 None，全新用户（还没有 desensitize.json）会静默不脱敏 ——
        // Node 版是把 compileTerms 放在闭包初始化里，行为等价
        let matcher = Self::rebuild(&terms);
        let service = Self {
            inner: Arc::new(RwLock::new(Inner {
                enabled: true,
                terms,
                roles: DEFAULT_ROLES.iter().map(|role| role.to_string()).collect(),
                matcher,
                // 已合并到的默认词表版本：无配置文件（新用户）时直接视为最新，无需迁移
                defaults_version: DEFAULT_TERMS_VERSION as f64,
                file,
                stats: Stats::default(),
            })),
        };
        service.load();
        service
    }

    /// 词表文件完整路径（界面展示用）
    pub fn file(&self) -> PathBuf {
        self.with_read(|guard| guard.file.clone())
    }

    /// 只读访问内部状态。锁中毒（持锁 panic）时接管内部数据继续用 ——
    /// 与账号存储同一策略：宁可容忍一次中毒，也不要让网关永久不可用。
    /// 闭包内**只允许做取值/克隆**这类微秒级操作，绝不能做 IO 或网络。
    fn with_read<T>(&self, read: impl FnOnce(&Inner) -> T) -> T {
        match self.inner.read() {
            Ok(guard) => read(&guard),
            Err(poisoned) => read(&poisoned.into_inner()),
        }
    }

    /// 重新编译匹配器（对应 Node 的 rebuild()）
    fn rebuild(terms: &[String]) -> Option<Arc<TermMatcher>> {
        compile_terms(terms).map(Arc::new)
    }

    // ─── 读盘 / 写盘 / 迁移 ─────────────────────────────────

    /// 读词表文件并做默认词表迁移（对应 Node 的 load()）。
    ///
    /// 文件不存在时**什么都不做**：保持默认词表与默认启用态，也不打日志
    /// （Node 同样直接 return，启动日志只在真有文件时出现）。
    fn load(&self) {
        let file = self.file();
        let Ok(text) = std::fs::read_to_string(&file) else {
            return;
        };
        let parsed: Value = match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(error) => {
                // 解析失败只回退、不清空：当前内存里还是内置默认词表，
                // 加下来 migrate_defaults 也不会把用户的文件写坏
                logging::log("[Desensitize]", &format!("词表读取失败，回退默认词表: {error}"));
                return;
            }
        };

        let (count, enabled) = {
            let Ok(mut guard) = self.inner.write() else {
                return;
            };
            if let Some(flag) = parsed.get("enabled").and_then(Value::as_bool) {
                guard.enabled = flag;
            }
            if let Some(items) = parsed.get("terms").and_then(Value::as_array) {
                guard.terms = normalize_terms(&string_list(items));
            }
            if let Some(items) = parsed.get("roles") {
                guard.roles = normalize_roles(Some(items));
            }
            // 老版本写出的文件没有该字段，视为版本 1（只有初版默认词表）
            let version = js_number(parsed.get("defaultsVersion").unwrap_or(&Value::Null));
            guard.defaults_version = if version >= 1.0 { version } else { 1.0 };
            guard.matcher = Self::rebuild(&guard.terms);
            (guard.terms.len(), guard.enabled)
        };
        self.migrate_defaults();
        logging::log(
            "[Desensitize]",
            &format!("已加载词表：{count} 个词，{}", if enabled { "启用" } else { "停用" }),
        );
    }

    /// 默认词表补词迁移（一次性）：把 defaults_version 之后各版本登记的新默认词条
    /// 补进当前词表。只在 load() 里、读盘之后调用。
    ///
    /// 合并完无论是否真的补了词，都把版本标记推进到最新并落盘，避免每次启动
    /// 重复比对、重复写盘。用户主动删掉的词条不会被补回：这里只处理
    /// 「比用户已合并版本更新」的登记词条（对照 Node 的 migrateDefaults）。
    fn migrate_defaults(&self) {
        let current = self.with_read(|guard| guard.defaults_version);
        // 拷贝出 (版本, 词条表) 而不是持有引用：后面要在写锁里遍历，
        // 借用同一份静态表跨越加锁会让借用检查器为难
        let mut pending: Vec<(u32, &'static [&'static str])> = DEFAULT_TERM_MIGRATIONS
            .iter()
            .filter(|item| (item.0 as f64) > current)
            .map(|item| (item.0, item.1))
            .collect();
        if pending.is_empty() {
            return;
        }
        pending.sort_by_key(|item| item.0);

        let mut added: Vec<String> = Vec::new();
        {
            let Ok(mut guard) = self.inner.write() else {
                return;
            };
            let mut known: Vec<String> =
                guard.terms.iter().map(|term| term.to_lowercase()).collect();
            for (_, terms) in pending {
                // terms 是 &'static [&'static str]，`for term in terms` 得到 &&str
                for &term in terms {
                    let key = term.to_lowercase();
                    if known.iter().any(|item| item == &key) {
                        continue;
                    }
                    known.push(key);
                    added.push(term.to_string());
                }
            }
            let mut merged = guard.terms.clone();
            merged.extend(added.iter().cloned());
            guard.terms = normalize_terms(&merged);
            guard.defaults_version = DEFAULT_TERMS_VERSION as f64;
            guard.matcher = Self::rebuild(&guard.terms);
            self.save_locked(&guard);
        }
        logging::log(
            "[Desensitize]",
            &if added.is_empty() {
                format!("默认词表已是最新（v{DEFAULT_TERMS_VERSION}，无需补充词条）")
            } else {
                format!(
                    "默认词表已更新到 v{DEFAULT_TERMS_VERSION}，自动补充词条 {} 个：{}",
                    added.len(),
                    added.join("、")
                )
            },
        );
    }

    /// 写盘（对应 Node 的 save()）。调用方必须持有写锁 —— 这样并发保存是
    /// 串行的，不会出现「旧快照覆盖新快照」。失败只打日志、不影响请求。
    ///
    /// 缩进与字段顺序按 Node 的 `JSON.stringify(x, null, 2) + "\n"` 手工拼，
    /// 详见 `serialize_state`。
    fn save_locked(&self, inner: &Inner) -> bool {
        // file = 目录.join(FILE_NAME)，parent 必然存在；真取不到就按配置目录兜底
        let dir = match inner.file.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
            _ => crate::server::config::config_dir(),
        };
        if let Err(error) = std::fs::create_dir_all(&dir) {
            logging::log("[Desensitize]", &format!("词表保存失败: {error}"));
            return false;
        }
        let text = serialize_state(
            inner.enabled,
            &inner.terms,
            &inner.roles,
            inner.defaults_version,
        );
        if let Err(error) = std::fs::write(&inner.file, text) {
            logging::log("[Desensitize]", &format!("词表保存失败: {error}"));
            return false;
        }
        true
    }

    // ─── 主入口：处理一次入站请求体 ─────────────────────────

    /// 处理一次入站请求体，命中时**原地改写** `body` 并返回命中统计。
    ///
    /// 对应 Node 的 `processBody`：每次都记一次 requests（含关闭状态下的请求，
    /// 与 Node 一致）；未命中时不改 body、hits 为 0。
    pub fn process_body(&self, body: &mut Value) -> ProcessOutcome {
        // ① 读锁取快照（matcher 是 Arc，克隆极廉价），随即放锁
        let (enabled, matcher, roles) = {
            let guard = match self.inner.read() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            (guard.enabled, guard.matcher.clone(), guard.roles.clone())
        };
        let unchanged = ProcessOutcome {
            changed: false,
            hits: 0,
            matched: Vec::new(),
            term_counts: Vec::new(),
        };
        if !enabled || matcher.is_none() || body.get("messages").is_none() {
            self.record(1, 0, 0, None);
            return unchanged;
        }

        // ② 脱敏计算全程在锁外（纯字符串变换，无 IO、无 await）
        let mut counter = Counter::default();
        let next = desensitize_body(body, matcher.as_deref(), &roles, &mut counter);
        if counter.total == 0 {
            self.record(1, 0, 0, None);
            return unchanged;
        }
        let term_counts = counter.term_counts();
        // matched 是**首次命中顺序**（未排序），term_counts 是按次数降序 —— 两者不同源
        let matched = counter.matched_terms();
        if let Some(next) = next {
            *body = next;
        }
        let hits = counter.total;

        // ③ 合并统计（写锁，极短）
        self.record(1, 1, hits, Some(&term_counts));
        ProcessOutcome { changed: true, hits, matched, term_counts }
    }

    /// 累加一次请求的统计（对应 Node 里 `stats.*` 的几处自增）
    fn record(
        &self,
        requests: usize,
        changed: usize,
        hits: usize,
        term_counts: Option<&[(String, usize)]>,
    ) {
        let Ok(mut guard) = self.inner.write() else {
            return;
        };
        guard.stats.requests += requests;
        guard.stats.changed_requests += changed;
        guard.stats.total_hits += hits;
        if let Some(list) = term_counts {
            for (term, count) in list {
                match guard
                    .stats
                    .term_hits
                    .iter_mut()
                    .find(|(item, _)| item == term)
                {
                    Some(entry) => entry.1 += count,
                    None => guard.stats.term_hits.push((term.clone(), *count)),
                }
            }
        }
    }

    // ─── 状态与变更 ─────────────────────────────────────────

    /// 完整状态（对应 Node 的 getState）
    pub fn state(&self) -> Value {
        self.with_read(|guard| {
            json!({
                "enabled": guard.enabled,
                "terms": guard.terms.clone(),
                "roles": guard.roles.clone(),
                "termCount": guard.terms.len(),
                "defaults": {
                    "enabled": true,
                    "terms": DEFAULT_TERMS.iter().map(|term| term.to_string()).collect::<Vec<_>>(),
                    "roles": DEFAULT_ROLES.iter().map(|role| role.to_string()).collect::<Vec<_>>(),
                    "version": DEFAULT_TERMS_VERSION,
                },
                "file": guard.file.to_string_lossy(),
                "stats": guard.stats.snapshot(),
            })
        })
    }

    /// 精简摘要（/api/config 与 /api/session 用）：
    /// `{enabled, termCount, roles}` —— 形状照抄 server.mjs 806/924 行
    pub fn summary(&self) -> Value {
        self.with_read(|guard| {
            json!({
                "enabled": guard.enabled,
                "termCount": guard.terms.len(),
                "roles": guard.roles.clone(),
            })
        })
    }

    /// 当前词条数（路由日志里的 `before → after` 需要）
    pub fn term_count(&self) -> usize {
        self.with_read(|guard| guard.terms.len())
    }

    /// 全量替换词表（对应 setTerms）
    pub fn set_terms(&self, terms: &[String]) -> Value {
        {
            let Ok(mut guard) = self.inner.write() else {
                return self.state();
            };
            guard.terms = normalize_terms(terms);
            guard.matcher = Self::rebuild(&guard.terms);
            self.save_locked(&guard);
        }
        self.state()
    }

    /// 追加词（已存在的按忽略大小写跳过 —— 由 setTerms 的 normalizeTerms 兜住）
    pub fn add_terms(&self, terms: &[String]) -> Value {
        let mut merged = self.with_read(|guard| guard.terms.clone());
        merged.extend(terms.iter().cloned());
        self.set_terms(&merged)
    }

    /// 删除词（忽略大小写）。
    ///
    /// 与 Node 一致：drop 集合由**入参原样**构造（trim + 小写），
    /// 既不做长度上限过滤也不滤空串 —— 路由那层已经校验过，这里保持同一语义。
    pub fn remove_terms(&self, terms: &[String]) -> Value {
        let drop: Vec<String> = terms
            .iter()
            .map(|term| js_trim(term).to_lowercase())
            .collect();
        if drop.is_empty() {
            return self.state();
        }
        let kept: Vec<String> = self.with_read(|guard| {
            guard
                .terms
                .iter()
                .filter(|term| !drop.iter().any(|item| item == &term.to_lowercase()))
                .cloned()
                .collect()
        });
        self.set_terms(&kept)
    }

    /// 开关（对应 setEnabled(value, {persist})）。
    /// `persist: false` 只改运行态不写盘 —— 环境变量覆盖场景专用。
    pub fn set_enabled(&self, enabled: bool, persist: bool) -> Value {
        {
            let Ok(mut guard) = self.inner.write() else {
                return self.state();
            };
            guard.enabled = enabled;
            if persist {
                self.save_locked(&guard);
            }
        }
        self.state()
    }

    /// 作用角色（对应 setRoles）
    pub fn set_roles(&self, roles: Option<&Value>) -> Value {
        {
            let Ok(mut guard) = self.inner.write() else {
                return self.state();
            };
            guard.roles = normalize_roles(roles);
            self.save_locked(&guard);
        }
        self.state()
    }

    /// 恢复默认词表（不动 enabled，也不动 defaultsVersion —— 与 Node 一致）
    pub fn reset_terms(&self) -> Value {
        let defaults: Vec<String> = DEFAULT_TERMS.iter().map(|term| term.to_string()).collect();
        self.set_terms(&defaults)
    }

    /// 清空命中统计（对应 resetStats）
    pub fn reset_stats(&self) {
        let Ok(mut guard) = self.inner.write() else {
            return;
        };
        guard.stats = Stats::default();
    }
}

// ─── 进程级句柄 ────────────────────────────────────────────

/// 进程级脱敏器。**为什么不用 ServerState 而是全局**：
/// 对话链路的脱敏钩子 `api::chat::desensitize_body(&mut Value)` 是不带 state 的
/// 纯签名（切片 4 定下的接入点，本切片不改调用点），它要拿到的就是「当前生效的
/// 那一份词表」。全局句柄与 `config::current()` / `logging` 是同一个模式：
/// 启动时装一次，之后所有模块共用，避免为了传一个句柄把签名层层改一遍。
/// 句柄本身是 `Clone` 的轻量 Arc，ServerState 里那份与全局这份是**同一实例**。
static GLOBAL: OnceLock<Desensitizer> = OnceLock::new();

/// 初始化进程级脱敏器并读盘（启动时调用一次，幂等）。
pub fn init(directory: &Path) -> Desensitizer {
    let service = Desensitizer::new(directory.to_path_buf());
    let _ = GLOBAL.set(service.clone());
    service
}

/// 取进程级脱敏器；未初始化时用配置目录即时构造一份（保证任何调用顺序都不 panic）。
pub fn global() -> Desensitizer {
    if let Some(service) = GLOBAL.get() {
        return service.clone();
    }
    let service = Desensitizer::new(crate::server::config::config_dir());
    let _ = GLOBAL.set(service.clone());
    service
}

// ─── 文件格式与 JS 数值语义 ─────────────────────────────────

/// 从 JSON 数组里取字符串项（对应 normalizeTerms 的 `typeof raw !== 'string'`
/// 过滤：非字符串项直接跳过，不报错）。
fn string_list(items: &[Value]) -> Vec<String> {
    items
        .iter()
        .filter_map(|item| item.as_str().map(str::to_string))
        .collect()
}

/// JS `Number(x)` 语义：null/非法 → NaN，布尔 → 0/1，字符串走 JS 数字语法
/// （空串与纯空白都是 0）。只用于 defaultsVersion 这一处比较，无需完整实现。
fn js_number(value: &Value) -> f64 {
    match value {
        Value::Number(number) => number.as_f64().unwrap_or(f64::NAN),
        Value::Bool(flag) => {
            if *flag {
                1.0
            } else {
                0.0
            }
        }
        Value::Null => f64::NAN,
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return 0.0;
            }
            trimmed.parse::<f64>().unwrap_or(f64::NAN)
        }
        Value::Array(_) | Value::Object(_) => f64::NAN,
    }
}

/// 按 JS `JSON.stringify(value, null, 2)` 的格式序列化词表文件。
///
/// 手工拼而不是用 serde_json：Map 会按字母序输出，而 Node 的顺序是
/// `enabled / terms / roles / defaultsVersion` —— 文件会被用户与排障脚本
/// 直接打开看，字段顺序与缩进必须逐字节一致才能叫「格式兼容」。
fn serialize_state(enabled: bool, terms: &[String], roles: &[String], version: f64) -> String {
    let mut out = String::with_capacity(64 + terms.len() * 16);
    out.push_str("{\n");
    out.push_str(&format!("  \"enabled\": {enabled},\n"));
    push_string_array(&mut out, "terms", terms);
    out.push_str(",\n");
    push_string_array(&mut out, "roles", roles);
    out.push_str(",\n");
    out.push_str(&format!(
        "  \"defaultsVersion\": {}\n",
        js_number_text(version)
    ));
    out.push_str("}\n");
    out
}

/// 追加一个字符串数组字段（JS 的空数组渲染成 `[]`，不换行）
fn push_string_array(out: &mut String, key: &str, items: &[String]) {
    if items.is_empty() {
        out.push_str(&format!("  \"{key}\": []"));
        return;
    }
    out.push_str(&format!("  \"{key}\": [\n"));
    for (index, item) in items.iter().enumerate() {
        out.push_str("    ");
        out.push_str(&json_string(item));
        if index + 1 < items.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push_str("  ]");
}

/// JS `JSON.stringify(字符串)` 的转义规则（与 serde_json 对普通文本一致：
/// 只转义 `"` `\` 与控制字符，不转义 `/` 与非 ASCII）
fn json_string(value: &str) -> String {
    serde_json::to_string(&Value::String(value.to_string()))
        .unwrap_or_else(|_| "\"\"".to_string())
}

/// JS 数字转文本：整数不带小数点（`JSON.stringify(2)` → "2"），
/// 其它走 f64 的短表示；NaN/Infinity 在 JSON 里是 null。
fn js_number_text(value: f64) -> String {
    if !value.is_finite() {
        return "null".to_string();
    }
    if value.fract() == 0.0 && value.abs() < 9.007_199_254_740_992e15 {
        return format!("{}", value as i64);
    }
    format!("{value}")
}
