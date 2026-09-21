//! 内容脱敏（对照 Node 版 src/workbuddy-desensitize.mjs 全量移植）。
//!
//!   engine.rs  纯函数：词表清洗、匹配构造、文本改写、content/messages/body 遍历
//!   sql.rs     `kv` 表里 `desensitize` 键的行级读写（本模块唯一出现 SQL 的文件）
//!   state.rs   持久化状态的形态：序列化 / 解析 / 迁移项的两个入口
//!   mod.rs     状态化：词表读写、默认词表迁移、命中统计（本文件）
//!
//! ── 持久化：`kv` 表的 `desensitize` 键（本切片从 desensitize.json 迁过来）──
//! 改造前是一整份 JSON 文件 `{config_dir}/desensitize.json`；现在一整份状态
//! （开关 / 词表 / 角色 / 作用提供商 / 已合并的默认词表版本）就是 `kv` 表的一行，
//! 读是一次主键查询、写是一条 UPSERT（语句只在 `sql.rs`）。旧文件由
//! `db::migrate::import_desensitize` 一次性搬入。
//!
//! ── 「逐字节对齐 Node 文件格式」这条约束随本次改造消失 ──────────
//! 改造前那条约束是**真实**的：`desensitize.json` 会被 Node 版与 Rust 版
//! **交替读写**，字段顺序与缩进飘一格，在对方眼里就是「文件被改过」，所以
//! 当时的 `serialize_state` 手工拼字符串（`JSON.stringify(x, null, 2) + "\n"`
//! 的逐字节复刻），连空数组渲染成 `[]` 而不换行都要对齐 —— serde_json 的 Map
//! 按字母序输出，不能直接用。
//! 进数据库后这份状态**只被本进程读写**：`kv` 那一行没有第二个读者（排障时
//! 用 `sqlite3` 看它、改它，而不是让另一个程序去读写它），「格式兼容」不再有
//! 任何消费者。于是手工拼装整体删除，改用普通序列化（`json!` +
//! `serde_json::to_string`）：字段顺序变成字母序、缩进消失，都不再有意义。
//! **保留下来的东西要看清**：五个键名（`enabled` / `terms` / `roles` /
//! `providers` / `defaultsVersion`）一字不改，`parse_state` 的宽容口径也一字
//! 不改 —— 迁移项读旧文件与运行期读库里的值走的是**同一份解析实现**，
//! 于是「迁移前的文件」与「迁移后的库」在行为上不可能分叉。
//!
//! ── providers：脱敏的作用提供商（Agent2API 改造 §3.5）────────
//! 状态里的 `providers: ["workbuddy"]`，取值为 `core::providers` 注册表里的
//! provider id。缺省（记录里没有这个键）按 `["workbuddy"]` 处理 ——
//! **读侧兜底、不强制写回**：只在用户真的改了配置（或改了词表触发保存）时才
//! 把键写进记录，不因为「读到一份缺键的旧记录」就改写它。
//!
//! 「转发前要不要脱敏」的**判定**不在本模块的算法里：转发层在**每家 provider
//! 真正发送前**按作用范围逐家判定（见 `process_body_for_provider`）——
//! 在范围内的那一家拿处理副本，不在范围内的拿客户端原始请求体（不处理、不计数）。
//!
//! ── 并发模型 ────────────────────────────────────────────────
//! 句柄内部一把 `RwLock`，克隆共享同一份状态（与 ModelCatalog 同构）。
//! 硬约束：**热路径不持锁做重活** —— 转发时会话先克隆出 `Arc<TermMatcher>`
//! 快照再放锁，脱敏计算全程在锁外，最后只在写锁里合并一次计数。
//! 持久化（低频的用户操作）在写锁内完成，与 Node 单线程下的「改内存 → 落盘」
//! 语义等价：不会出现两个并发请求把旧快照写回去的情况。
//!
//! **热路径零数据库调用**（改造前是零文件 IO，这条性质必须原样保持）：脱敏发生在
//! 每个转发的请求体上，词表只在「用户改了配置」时才变，所以 `process_body` /
//! `process_body_for_provider` 全程只用内存快照（`matcher` 是 `Arc`），不碰 `Db`。
//! 数据库只在三处出现：构造时读一次（`load`）、用户改配置时写一次
//! （`save_locked`）、迁移项搬旧文件（`import_legacy`）。

mod engine;
mod sql;
mod state;

use std::sync::{Arc, OnceLock, RwLock};

use serde_json::{json, Value};

use crate::server::core::providers::{DEFAULT_PROVIDER_ID, PROVIDERS};
use crate::server::db::Db;
use crate::server::logging;

// 只再导出有实际调用点的符号（ZWSP / VALID_ROLES / desensitize_text /
// desensitize_messages 是引擎内部与文档层面的 API，外部暂不需要，
// 留在 engine.rs 里即可 —— 多余再导出在 private 模块里会报 unused import）
pub use engine::{
    compile_terms, desensitize_body, normalize_roles, normalize_terms, utf16_len, Counter,
    TermMatcher, DEFAULT_ROLES, DEFAULT_TERM_MIGRATIONS, DEFAULT_TERMS, DEFAULT_TERMS_VERSION,
    MAX_TERM_LENGTH, MAX_TERMS,
};
/// 词/角色归一化用的 JS trim（删词路径复用，保证「加得进 == 删得掉」）
use engine::js_trim;
/// 持久化状态的两个入口（迁移项用；`load` / `save_locked` 也走它们的内部件）
pub(crate) use state::{import_legacy, legacy_present};
use state::{parse_state, state_value, PersistedState};

/// 统计快照里 topTerms 的默认条数（对应 Node 的 `statsSnapshot({top = 20})`）
const TOP_TERMS: usize = 20;

/// 脱敏 json 里的作用提供商键（架构文档 §3.5）。
/// **值一字不改**：迁移项读旧文件、`load` 读库里的值都按这个键名取。
const KEY_PROVIDERS: &str = "providers";

/// 缺省作用提供商（记录里没有 `providers` 键时按它处理）。
///
/// 取值是 `DEFAULT_PROVIDER_ID`（= workbuddy）而不是另写字面量：
/// 「老数据默认属于 workbuddy」这条口径在账号迁移与脱敏这里必须是同一个字符串。
pub fn default_provider_scope() -> Vec<String> {
    vec![DEFAULT_PROVIDER_ID.to_string()]
}

/// 作用提供商列表归一：按**注册表顺序**取交集（与 `normalize_roles` 同一手法）。
///
/// 规则：
///   - 非数组 / 全部项都不认识 → 回落缺省 `["workbuddy"]`（旧记录兜底）；
///   - 未知 id 静默丢弃（读侧宽容：手改库里的状态写错一个 id 不该让脱敏整体失效）；
///   - 去重（手写 `["workbuddy","workbuddy"]` 不产生重复项）。
///
/// **写接口（`/api/desensitize/providers`）另做严格校验**：未知 id 给 400
/// 而不是静默丢弃 —— 走接口的非法值要当场告诉用户（与保留期天数两侧
/// 口径不同但各有理由的处理方式一致）。
pub fn normalize_providers(input: Option<&Value>) -> Vec<String> {
    let Some(Value::Array(items)) = input else {
        return default_provider_scope();
    };
    let providers: Vec<String> = PROVIDERS
        .iter()
        .filter(|meta| items.iter().any(|item| item.as_str() == Some(meta.id)))
        .map(|meta| meta.id.to_string())
        .collect();
    if providers.is_empty() {
        return default_provider_scope();
    }
    providers
}

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
    /// 脱敏作用提供商（provider id，注册表顺序；缺省 `["workbuddy"]`）
    providers: Vec<String>,
    /// 编译后的匹配器；词表为空时为 None（调用方据此跳过脱敏）
    matcher: Option<Arc<TermMatcher>>,
    /// 已合并到的默认词表版本（可能带小数：Node 存的是 Number）
    defaults_version: f64,
    stats: Stats,
}

/// 一次请求体处理的产物（对应 Node 的 `processBody` 返回）
pub struct ProcessOutcome {
    pub changed: bool,
    pub hits: usize,
    /// 命中的词列表（无计数，保持向后兼容）。
    /// Node 版 `processBody` 返回的 `matched` 对等物：本模块的命中日志只用
    /// term_counts，这里保留完整三个字段以便排障时逐个对照 Node 的返回。
    #[allow(dead_code)]
    pub matched: Vec<String>,
    /// 按命中次数降序的每词次数
    pub term_counts: Vec<(String, usize)>,
}

/// 转发期入口（[`Desensitizer::process_body_for_provider`]）的返回：
/// 处理后的请求体副本 + 这一家的命中明细。
///
/// ── 为什么把两样打包返回，而不是各给一个方法 ────────────────
/// 命中明细只在**处理发生的同一瞬间**存在（`process_body` 的返回值里），
/// 拆成两个方法会让调用方要么处理两遍（词表匹配是热路径上最贵的部分）、
/// 要么自己把中间结果存起来 —— 那还不如由本模块一次交清。
/// 结构体也让将来「处理还要额外透出什么」有个自然的落点，不必再改签名。
pub struct ProcessedBody {
    /// 处理后的请求体（未勾选的家根本走不到这里，见上面那个方法）
    pub body: Value,
    /// 命中的词与次数（按次数降序；空表 = 这一家处理过但一个词都没命中）
    ///
    /// 它同时也是「有没有命中」的判据（`is_empty()`），所以不再单给一个
    /// `Changed` 字段：`changed` 与「`term_counts` 非空」在当前实现里等价
    /// （`process_body` 只在 `counter.total == 0` 时返回 changed=false），
    /// 多一个字段就多一处可能对不上的状态。
    pub term_counts: Vec<(String, usize)>,
}

/// 脱敏服务句柄：内部一把 `RwLock`，克隆共享同一份状态。
#[derive(Clone)]
pub struct Desensitizer {
    inner: Arc<RwLock<Inner>>,
    /// 统一库句柄。**不参与脱敏计算**（热路径零数据库调用，见模块头），
    /// 只被构造时的读、用户改配置时的写用到。`None` = 库不可用。
    db: Option<Db>,
}

impl Desensitizer {
    /// 构造脱敏器并读库（对应 `createDesensitizer({directory, log})`）。
    ///
    /// 默认启用（库里还没有这份状态时即默认开启），词表为内置默认词表。
    ///
    /// ── 签名为什么从 `new(directory)` 改成接 `Db` ──────────────
    /// 词表不再有「自己的目录」：状态在统一库 `kv` 表的一行里，路径这件事由
    /// `Db` 唯一持有（`Db::file()`）。与 `AccountStore::with_db` /
    /// `LogStore::with_db` / `RequestStats::with_db` 同一形态，四个 store 的
    /// 构造方式保持一致；`Option<Db>` 也一致（库打不开时仍能构造，只是降级）。
    pub fn with_db(db: Option<Db>) -> Self {
        let terms: Vec<String> = DEFAULT_TERMS.iter().map(|term| term.to_string()).collect();
        // 构造即编译一次默认词表：load() 在「库里还没有这份状态」时直接返回，
        // 若这里留 None，全新用户（还没保存过词表）会静默不脱敏 ——
        // Node 版是把 compileTerms 放在闭包初始化里，行为等价
        let matcher = Self::rebuild(&terms);
        let service = Self {
            inner: Arc::new(RwLock::new(Inner {
                enabled: true,
                terms,
                roles: DEFAULT_ROLES.iter().map(|role| role.to_string()).collect(),
                // 全新用户（库里还没有这份状态）：作用范围就是 workbuddy
                // （等于改造前的实际行为 —— 那时只有 workbuddy 一个上游）
                providers: default_provider_scope(),
                matcher,
                // 已合并到的默认词表版本：没有记录（新用户）时直接视为最新，无需迁移
                defaults_version: DEFAULT_TERMS_VERSION as f64,
                stats: Stats::default(),
            })),
            db,
        };
        service.load();
        service
    }

    /// 词表数据所在的文件（**就是库文件**；界面展示用）。
    ///
    /// 语义从「词表文件路径」变成「装着这份状态的库文件」—— 与 T2~T5 对
    /// `AccountStore::file()` / `LogStore::file()` / `RequestStats::request_file()`
    /// / `debug_traffic::file()` 的处理一致。前端 `ui/desensitize-panel.js` 直接
    /// 把这个字符串显示成「词表保存在 …」，进库后正确的说法就是库路径。
    ///
    /// 库不可用时给约定路径（而不是 `Option`）：与 `LogStore::file()` 同一取舍 ——
    /// 本方法的返回类型是被两个调用方（`api::desensitize` 的 `state().file` 与
    /// 启动日志）直接 `.display()` 的 `PathBuf`，改成 `Option` 会让它们各自
    /// 编一套「没有路径时显示什么」。约定路径是**排障时该看的地方**，即使本次
    /// 运行库没打开，那句话也仍然指向正确的文件（用户据此知道去哪找）。
    pub fn file(&self) -> std::path::PathBuf {
        match self.db.as_ref() {
            Some(db) => db.file().to_path_buf(),
            None => crate::server::config::config_dir().join(crate::server::db::FILE_NAME),
        }
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

    // ─── 读库 / 写库 / 默认词表迁移 ─────────────────────────

    /// 读状态并做默认词表迁移（对应 Node 的 load()）。
    ///
    /// 库里没有这个键时**什么都不做**：保持默认词表与默认启用态，也不打日志
    /// （Node 同样直接 return，启动日志只在真有状态记录时出现）。
    ///
    /// ── 为什么解析逻辑在 `parse_state`（而它同时被迁移项调用）──────
    /// 「一份状态记录长什么样、哪些值要宽容」是**数据契约**：迁移项读旧文件、
    /// 运行期读库里的值，两处必须得到完全一致的结果。各写一份解析迟早分叉，
    /// 分叉的后果是「同一个词表，经迁移落库与直接读库得到不同的行为」
    /// （与 T4 的 `parse_*`、T5 的 `parse_legacy_jsonl` 同一取舍）。
    fn load(&self) {
        let Some(db) = self.db.as_ref() else {
            return;
        };
        let text = match db.with(sql::load_text) {
            Some(Ok(Some(text))) => text,
            // 没有这个键 = 全新用户 / 还没保存过，保持默认态
            Some(Ok(None)) | None => return,
            Some(Err(error)) => {
                // 读失败只回退、不清空：当前内存里还是内置默认词表，
                // 接下来 migrate_defaults 也不会把用户的状态写坏
                logging::log(
                    "[Desensitize]",
                    &format!("词表读取失败，回退默认词表: {error}"),
                );
                return;
            }
        };
        let state = match parse_state(&text) {
            Ok(state) => state,
            Err(error) => {
                logging::log(
                    "[Desensitize]",
                    &format!("词表读取失败，回退默认词表: {error}"),
                );
                return;
            }
        };

        let (count, enabled) = {
            let Ok(mut guard) = self.inner.write() else {
                return;
            };
            apply_state(&mut guard, state);
            (guard.terms.len(), guard.enabled)
        };
        self.migrate_defaults();
        let provider_scope = self.with_read(|guard| guard.providers.join("、"));
        logging::log(
            "[Desensitize]",
            &format!(
                "已加载词表：{count} 个词，{}（作用提供商 {provider_scope}）",
                if enabled { "启用" } else { "停用" }
            ),
        );
    }

    /// 默认词表补词迁移（一次性）：把 defaults_version 之后各版本登记的新默认词条
    /// 补进当前词表。只在 load() 里、读状态之后调用。
    ///
    /// 合并完无论是否真的补了词，都把版本标记推进到最新并落库，避免每次启动
    /// 重复比对、重复写库。用户主动删掉的词条不会被补回：这里只处理
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

    /// 落库（对应 Node 的 save()）。调用方必须持有写锁 —— 这样并发保存是
    /// 串行的，不会出现「旧快照覆盖新快照」。失败只打日志、不影响请求。
    ///
    /// ── 日志为什么在锁外打（这是本方法最容易改错的地方）────────
    /// `db.with` 拿的是**全局唯一那把连接锁**，而 `logging::log` 的入库那一路
    /// 要往同一个库 `append` 一条日志（`logs` 表），它会再去取同一把锁 ——
    /// `std::sync::Mutex` 不可重入，**在闭包里打日志等于当场死锁**。
    /// 所以这里只在闭包内取得 `Result`，出了闭包再按结果打日志
    /// （`logs_store/sql.rs` 模块头与各 store 的「硬约束」说的都是这条）。
    ///
    /// 闭包内还**只做数据库写**、不打印任何东西：`eprintln!` 虽然不碰库，
    /// 但把它放进去会让「锁内到底做了什么」需要逐行核对，不如统一移出。
    fn save_locked(&self, inner: &Inner) -> bool {
        let Some(db) = self.db.as_ref() else {
            // 库不可用：内存状态照改（本次运行内生效），持久化静默失败。
            // 与改造前「写文件失败」的处理一致 —— 失败只影响「重启后还在不在」，
            // 不该让正在跑的请求或被改的配置报错。
            return false;
        };
        let text = match serde_json::to_string(&state_value(
            inner.enabled,
            &inner.terms,
            &inner.roles,
            &inner.providers,
            inner.defaults_version,
        )) {
            Ok(text) => text,
            // Value 序列化失败在实践中不可达（没有非字符串键、没有 NaN 以外的
            // 非法值），留一条明确的分支而不是 unwrap
            Err(error) => {
                logging::log(
                    "[Desensitize]",
                    &format!("词表保存失败: 序列化失败（{error}）"),
                );
                return false;
            }
        };
        match db.with(|conn| sql::save(conn, &text)) {
            Some(Ok(())) => true,
            Some(Err(error)) => {
                logging::log("[Desensitize]", &format!("词表保存失败: {error}"));
                false
            }
            None => {
                logging::log("[Desensitize]", "词表保存失败: 数据库不可用");
                false
            }
        }
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

    /// 转发前的**按 provider 作用范围**处理：本模块对外的转发期入口。
    ///
    /// 调用方在**某一家 provider 真正要发送之前**调用（凭证已就绪），于是：
    ///   - provider 不在作用范围内 → 返回 None：调用方用**客户端原始请求体**，
    ///     既不处理也不计数（未勾选的提供商不会多记命中）；
    ///   - 在范围内 → 用现有 [`Self::process_body`] 处理**副本**并打命中日志，
    ///     返回 [`ProcessedBody`]（处理后的副本 + 命中明细）。
    ///
    /// `scope` 是**本次请求**的作用范围快照（调用方在请求开始时用
    /// [`Self::provider_scope`] 取一次），于是同一次请求里各家 provider 的判定
    /// 用的是同一份范围，不会因为请求进行中改设置而漂移。
    ///
    /// 本方法只做「范围判定 + 调用现有处理 + 打日志」这三件事：词表、匹配与
    /// 改写算法全在 engine.rs，未做任何改动。
    ///
    /// ── 返回值为什么从 `Option<Value>` 变成 `Option<ProcessedBody>` ──
    /// `process_body` 早就算出了 `term_counts`（按次数降序的每词命中数），
    /// 但改造前只有**聚合统计**（`stats.term_hits` 里跨请求累加的那一份）与
    /// 一行日志消费它 —— 逐条请求的「这次命中了哪些词」当场就丢了。
    /// 于是请求日志那边只能显示一个「命中了敏感词」的布尔事实，
    /// 用户看到命中却不知道命中了什么（要去设置页的聚合统计里反推，
    /// 而那里面混着所有请求的累计值）。
    /// 把这份明细原样带出来交给转发层，让它顺手记进请求日志 ——
    /// **不改任何判定与改写逻辑**，只是不再把已经算出来的结果扔掉。
    pub fn process_body_for_provider(
        &self,
        provider_id: &str,
        scope: &[String],
        body: &Value,
    ) -> Option<ProcessedBody> {
        // 作用范围是 provider id 的精确匹配（与注册表里的 id 同一字符串）
        if !scope.iter().any(|item| item == provider_id) {
            return None;
        }
        let mut processed = body.clone();
        let outcome = self.process_body(&mut processed);
        if outcome.changed {
            // 只在**终端**留痕：命中明细已经跟着这条请求进了请求日志
            // （`sensitiveHits` → 「重试」列里的「敏」标签，悬停逐词看次数），
            // 运行日志页因此不再为每一次命中写一行（改造前那一行是
            // 「日志」页唯一的命中视图，现在有了更直接的去处）。
            logging::console_line("[Desensitize]", &hit_log_line(&outcome));
        }
        Some(ProcessedBody {
            body: processed,
            // 未命中时 `term_counts` 是空表（`process_body` 的 unchanged 分支），
            // 于是「处理过但没命中」与「命中了」在调用侧可以用 `is_empty()` 分开
            term_counts: outcome.term_counts,
        })
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

    /// 完整状态（对应 Node 的 getState）。
    ///
    /// `providers` 是 Agent2API 改造新增的字段（作用提供商，架构文档 §3.5）：
    /// 与 terms/roles 同级透出，前端设置页的「作用提供商」多选直接读它。
    ///
    /// `file` 现在报的是**库文件路径**（`file()` 的说明）：字段名与位置一字不改
    /// （前端 `ui/desensitize-panel.js` 直接读它塞进「词表保存在 …」那一行）。
    /// 它在闭包外取：`file()` 读的是 `Db` 而不是 `Inner`，不占这把读锁，也就不必
    /// （也不该）在持锁期间再去做一次与状态无关的取值。
    pub fn state(&self) -> Value {
        let file = self.file().to_string_lossy().to_string();
        self.with_read(|guard| {
            json!({
                "enabled": guard.enabled,
                "terms": guard.terms.clone(),
                "roles": guard.roles.clone(),
                "providers": guard.providers.clone(),
                "termCount": guard.terms.len(),
                "defaults": {
                    "enabled": true,
                    "terms": DEFAULT_TERMS.iter().map(|term| term.to_string()).collect::<Vec<_>>(),
                    "roles": DEFAULT_ROLES.iter().map(|role| role.to_string()).collect::<Vec<_>>(),
                    "providers": default_provider_scope(),
                    "version": DEFAULT_TERMS_VERSION,
                },
                "file": file,
                "stats": guard.stats.snapshot(),
            })
        })
    }

    /// 精简摘要（/api/config 与 /api/session 用）：
    /// `{enabled, termCount, roles, providers}` —— 形状照抄 server.mjs 806/924 行，
    /// `providers` 是 Agent2API 改造新增的一维（任务书要求摘要里带上它）。
    pub fn summary(&self) -> Value {
        self.with_read(|guard| {
            json!({
                "enabled": guard.enabled,
                "termCount": guard.terms.len(),
                "roles": guard.roles.clone(),
                "providers": guard.providers.clone(),
            })
        })
    }

    /// 脱敏作用提供商（provider id 列表）。
    ///
    /// 消费方：`upstream` 转发层在**请求开始时**取一次快照（同一次请求内各家
    /// provider 用同一份范围），随后每家即将发送前交给
    /// [`Self::process_body_for_provider`] 判定。本模块只提供数据，
    /// 判定与调用时机在转发层。
    pub fn provider_scope(&self) -> Vec<String> {
        self.with_read(|guard| guard.providers.clone())
    }

    /// 作用提供商（对应 setRoles 的同款写盘语义）：入参须已由路由层校验过。
    pub fn set_providers(&self, providers: &[String]) -> Value {
        {
            let Ok(mut guard) = self.inner.write() else {
                return self.state();
            };
            guard.providers = normalize_providers(Some(&Value::Array(
                providers.iter().cloned().map(Value::String).collect(),
            )));
            self.save_locked(&guard);
        }
        self.state()
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

/// 命中日志文案（照抄 Node 的 `已脱敏命中 N 处：词×次数、…`）。
///
/// 与 `provider_scope` 一样，这里只做**展示**：命中统计仍由 `process_body`
/// 里的 `record` 合并，文案与改造前逐字一致。
fn hit_log_line(outcome: &ProcessOutcome) -> String {
    let per_term = outcome
        .term_counts
        .iter()
        .map(|(term, count)| format!("{term}×{count}"))
        .collect::<Vec<_>>()
        .join("、");
    format!("已脱敏命中 {} 处：{per_term}", outcome.hits)
}

// ─── 进程级句柄 ────────────────────────────────────────────

/// 进程级脱敏器。**为什么不用 ServerState 而是全局**：
/// 转发层的处理调用点（`upstream::provider_loop` 在每家 provider 发送前调
/// [`Desensitizer::process_body_for_provider`]）要拿到的就是「当前生效的
/// 那一份词表」。全局句柄与 `config::current()` / `logging` 是同一个模式：
/// 启动时装一次，之后所有模块共用，避免为了传一个句柄把签名层层改一遍。
/// 句柄本身是 `Clone` 的轻量 Arc，ServerState 里那份与全局这份是**同一实例**。
static GLOBAL: OnceLock<Desensitizer> = OnceLock::new();

/// 初始化进程级脱敏器并读库（启动时调用一次，幂等）。
///
/// ── 签名为什么从 `init(directory)` 改成接 `Db` ──────────────
/// 与 `Desensitizer::with_db` 同一条理由：状态在统一库里，目录参数不再有任何
/// 消费者。调用点 `ServerState::bootstrap` 手里就是那个 `Option<Db>`（先 `clone`
/// 传给日志库与报文存储，这里再传一份 —— `Db` 是 `Arc` 句柄，克隆共享同一连接）。
/// `Option` 的语义：打不开库时脱敏器照常装起来（用默认词表与默认开关），
/// 只是改配置落不了库 —— 「库的问题不该让转发不可用」。
pub fn init(db: Option<Db>) -> Desensitizer {
    let service = Desensitizer::with_db(db);
    let _ = GLOBAL.set(service.clone());
    service
}

/// 取进程级脱敏器；未初始化时即时构造一份（保证任何调用顺序都不 panic）。
///
/// 兜底那份**没有库句柄**：它取默认词表与默认开关，改配置只在内存里生效
/// （`save_locked` 在 `db` 为 `None` 时返回 false）。这个分支只会在
/// 「`bootstrap` 还没跑到脱敏初始化、转发层就被人调到」时出现，而正常启动顺序
/// 下 `init` 总是先跑；给它一个约定路径去读写库反而更危险（那会绕过 `Db` 的
/// 单连接约定，凭空多出第二个连接）。
pub fn global() -> Desensitizer {
    if let Some(service) = GLOBAL.get() {
        return service.clone();
    }
    let service = Desensitizer::with_db(None);
    let _ = GLOBAL.set(service.clone());
    service
}

/// 重读库里的状态并替换内存快照（**数据迁移跑完之后**由调用点补一次）。
///
/// ── 为什么需要它（与 `config::reload` 同一条理由）─────────────
/// 本模块的状态是**启动时读一次库、之后只吃内存快照**的（热路径零数据库调用，
/// 见模块头）。而用户点「升级」导入旧数据这件事发生在启动**之后** ——
/// 那时 `init` 早就跑完了，内存里装的还是「库里什么都没有」时的默认词表。
/// 不重载的后果很具体：用户升级前配的词表进不了内存，脱敏按**默认词表**跑，
/// 界面上显示的还是默认词条（用户会以为自己的词表在升级里丢了）。
///
/// ── 为什么不能改用 `set_terms` 之类的写入口 ──────────────────
/// 那些入口的语义是「把这份词表写回库」（`save_locked`）。拿当前内存里的默认
/// 词表去写，正好**覆盖掉刚导进来的用户词表** —— 那是不可逆的数据破坏。
/// 重载必须是「读库 → 装进内存」这一个方向，所以单独一个入口。
///
/// ── 与 `init` 的关系 ────────────────────────────────────────
/// `init` 的 `OnceLock::set` 在已初始化时是**空操作**（保留旧实例），所以它
/// 做不到重载；而 `GLOBAL` 里的句柄是 `Clone` 的轻量 Arc，`load()` 又是
/// 「读库 → 应用状态」的完整实现，直接复用它即可。
///
/// 未初始化时**什么都不做**（`GLOBAL` 为空 = 正常启动顺序下还没跑到脱敏初始化，
/// 而那时也没有「升级后需要重载」这回事 —— 迁移由界面按钮触发，必然在初始化
/// 之后）。这里不去 `with_db(None)` 造一个兜底实例：那会让真正的 `init` 之后
/// 拿不到它（`OnceLock` 已被占），反而制造出一个没有库句柄的脱敏器。
pub fn reload() {
    if let Some(service) = GLOBAL.get() {
        service.load();
    }
}

// ─── 状态形态与 JS 数值语义（在 state.rs）──────────────────
//
//   这里的「一份状态记录长什么样、怎么解析、怎么序列化」以及迁移项用的两个
//   入口（`legacy_present` / `import_legacy`）都在 `state.rs` —— 那组函数不依赖
//   任何句柄，运行期与迁移项两边共用同一份实现（理由见该文件模块头）。

/// 把解析出的状态施加到一个 `Inner` 上（`load` 用）。
///
/// 「缺项保持当前值」这条口径在这里落地：`enabled` / `terms` 是 `Option`（`None`
/// 时**不动**当前值，而不是清空词表或强行打开开关）；`roles` / `providers` 交给
/// normalize 自己兜底（它们对非数组本来就有回落默认的实现）。
fn apply_state(guard: &mut Inner, state: PersistedState) {
    if let Some(flag) = state.enabled {
        guard.enabled = flag;
    }
    if let Some(terms) = state.terms {
        guard.terms = terms;
    }
    guard.roles = normalize_roles(state.roles.as_ref());
    // providers：记录里没有这个键 → normalize_providers 回落缺省
    // （**读侧兜底、不写回**：不因为读到一份缺键的旧记录就改写它，
    //  见模块头「providers」一节）
    guard.providers = normalize_providers(state.providers.as_ref());
    guard.defaults_version = state.defaults_version;
    guard.matcher = Desensitizer::rebuild(&guard.terms);
}

