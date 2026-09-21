//! 请求统计存储：`requests` 表（明细）+ `request_daily` 表（按天聚合）。
//!
//! ── 内部分层（拆开是为了每片都保持单一职责、单文件不过长）─────
//! ```text
//! server/
//!   request_stats.rs     存储本体：记账 / 裁剪 / 查询 / 报表装配（本文件）
//!   request_stats/
//!     sql.rs      行级 SQL：明细的读写删裁、聚合的整行 UPSERT（唯一出现 SQL 的地方）
//!     record.rs   数据类型与 JSON 契约（RequestEntry / DailyEntry / Retention…）
//!     report.rs   报表计算：纯函数（区间、补零、命中率、topModel、连续天数）
//!     clock.rs    本地时区工具（chrono::Local 在整模块的唯一使用点）
//!     backfill.rs 旧聚合行的口径回填（**只由迁移项调用**，见那边的说明）
//!     legacy.rs   旧文件导入的入口：解析旧 JSONL、算迁移期边界、写库+裁剪
//!                 （同样只由迁移项调用，运行期一次都不会走到）
//! ```
//!
//! ── 读侧接口的接线状态 ──────────────────────────────────────
//! 读侧（`usage_summary` / `query_requests` / `prune` / `stats` / `clear`）
//! 已由 `api::stats_api` 的三条路由接上，这些接口上的 `#[allow(dead_code)]`
//! 已全部移除：新增的公开接口若没人调用会直接报 warning，便于及时发现漏接的路由。
//! 仍保留 allow 的只有 `file()` 系访问器与 `import_legacy_*`
//! （理由写在各自的注释里）。
//!
//! ── 为什么是两张表（原来的「两个文件」）──────────────────────
//! 这个取舍**一字未变**，只是载体从文件换成了表：
//!   - `requests`：请求明细，一行一条。短窗口指标（缓存命中率的 10 分钟 / 1 小时
//!     窗口、近 24 小时趋势）必须逐条算，按天的聚合行给不出这种精度。
//!   - `request_daily`：按天聚合，一行一天。热力图固定 365 天、`all` 区间可能跨年，
//!     都超出明细的保留期，所以这份**寿命独立于明细**：明细被裁掉后当天的聚合行
//!     仍在，历史曲线不会因为裁明细而出现空洞。
//! 两个文件变两张表还顺带解决了原来的两个麻烦：**明细与聚合的一致性**（原来是
//! 两次独立写盘，中间崩溃会留下「明细删了、聚合还在」的错位）现在由**一个事务**
//! 保证；**删除后的按天重算**（原来是内存 retain + 两个文件整份重写）现在只需
//! 重算涉及的那几行（见 `clear_where`）。
//!
//! ── 落盘策略：全部消失 ──────────────────────────────────────
//! 改造前明细是「追加写为主 + 攒够 `COMPACT_STEP` 次才整文件重写」，聚合是
//! 「变更满 50 次或距上次落盘超 60 秒才重写整个文件」（延迟落盘）。进数据库后
//! 这两套机制整体消失：
//!   - 明细一条 `INSERT`，聚合一行 UPSERT，**每次记账都立即提交**（`record` 里
//!     一个事务做完两件事），没有「内存比文件多」的对齐问题，也没有整文件重写；
//!   - 于是 `dirty` / `appends_since_compact` / `COMPACT_STEP` / `insert_sorted` /
//!     延迟落盘的那对计数器与时钟全部不再存在；
//!   - 退出路径的 `flush()` 因此不再是「补写未落盘的内容」——它的新职责见该方法。
//!
//! ── 并发模型 ────────────────────────────────────────────────
//! 所有公开方法取 `&self`，真正的串行化由 `Db` 那把 Mutex 负责：每个公开方法都在
//! **一次** `Db::with` / `with_mut` 调用里跑完（多语句操作用事务包住），所以两次
//! 并发记账不可能交错。本结构里那把 `inner` 锁是第二道闸，作用与取舍见
//! [`RequestStats::guard`]。
//! 锁中毒时的取向与 T2/T3 一致：`Db` 中毒返回 `None`（连接状态可能停在事务中间，
//! 不能拿半截数据当真相），`inner` 那把则 `into_inner()` 接管（它保护的是空标记）。

mod backfill;
mod clock;
// 旧文件导入的入口（只给 `db::migrate` 的迁移项调用，运行期不参与）。
// `pub(crate)` 而不是私有：迁移项在 `db::migrate` 里，与 `request_stats`
// 不是父子模块，私有模块它够不到。
pub(crate) mod legacy;
mod record;
// 报表计算层（纯函数）与「模型请求日志」的查询实现。
//
// 这里**不放模块级 allow**：本层已由 `api::stats_api` 的报表路由接线，
// 于是「新增了一个算好却没人用的函数」会直接报 warning —— 那正是我们想知道的。
// 模块内部各函数仍应是 `pub(super)`：只有 `request_stats.rs` 能调到它们，
// 对外暴露面收敛在这一层。
mod report;
mod sql;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, RwLock};

use chrono::{Duration as ChronoDuration, NaiveDate};
use serde_json::{json, Value};

use crate::server::db::Db;

use clock::{date_key, day_of, local_midnight_ms, today};
use record::{DailyEntry, MAX_DAILY_DAYS, MAX_ENTRIES};
use report::{
    build_accounts, build_providers, build_top_model, cache_rates, cache_trend_24h, daily_trend,
    entry_json, heatmap, normalize_range, push_account_accum, push_model_accum,
    push_provider_accum, range_bounds, range_totals, streak,
};

pub use record::{
    AttemptDetail, NewRequestEntry, RequestEntry, RequestQuery, Retention, SensitiveHit,
    DEFAULT_LIMIT, MAX_LIMIT,
};

/// 明细窗口：四档缓存命中率里最长的是 7 天，趋势窗口是 24 小时 ——
/// 报表要从明细里取的数据**全部落在这个窗口内**，所以查询按它下界，
/// 不必把 2 万条明细整体读回内存再逐条丢。
const SUMMARY_WINDOW_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// 请求统计存储本体。所有公开方法取 `&self`，内部一把 `Mutex` 串行化调用序列。
pub struct RequestStats {
    /// SQLite 句柄。`None` = 数据库打开失败（启动时已记日志）。
    ///
    /// ── 为什么不做「文件回退」─────────────────────────────────
    /// 与 `LogStore` / `AccountStore` 的选择一致（见那两处的说明），在请求统计上
    /// 还多一条：这里的写入发生在**每个请求的收尾路径**上（`api::chat` 的记账点），
    /// 回退实现会变成一条高频执行、却只在数据库坏掉时才第一次运行的分支 ——
    /// 那是最坏的一类「没人测过的代码」。而统计的可接受降级很明确：
    /// **少记一条不影响任何业务**（报表少一个数）。所以库不可用时按各方法各自的
    /// 合理空值回落（见每个方法的说明），不写第二个文件。
    db: Option<Db>,
    /// 库文件路径（`stats()` 的 `file` / `dailyFile` 与排障用）。
    ///
    /// 语义从「明细文件 / 聚合文件」变成「装着这两份数据的库文件」—— 与
    /// `LogStore::file()` / `AccountStore::file()` 的处理一致。库打不开时回落到
    /// 约定路径（`{config_dir}/agent2api.db`）：这个值会显示在报表页与设置页的
    /// 存储概况里（`api::storage_api` 读它），给一个有意义的位置比给空串更有用。
    ///
    /// 为什么是 `RwLock` 而不是裸字段：与 `LogStore::file_path` 同一处理 ——
    /// 字段本身不再被任何写路径改动（`relocate` 已随单库语义删除），用 `RwLock`
    /// 是为了让「读现值」这件事与改造前同形，改动面最小。
    file_path: RwLock<PathBuf>,
    /// 保留期取值回调。**每次裁剪时动态调用**，于是设置改完天数下一次记账 /
    /// `prune` 就用新值，不需要重启进程（这正是把保留期做成回调而非构造参数的原因）。
    /// 用 `Arc` 是因为 `RequestStats` 本身被放进 `ServerState`，回调要能独立持有。
    get_retention: Arc<dyn Fn() -> Retention + Send + Sync>,
    /// 让「一次公开调用」的多个步骤不被另一次公开调用插进来。
    ///
    /// ── 它现在到底保证什么（诚实版）─────────────────────────────
    /// 数据一致性**不靠它**：真正把语句串起来的是 `Db` 内部那把 Mutex + SQLite
    /// 事务（每个写方法都在一个事务里跑完，读方法各是一次 `Db::with`）。
    /// 它保证的是调用序列的原子性 —— 比如 `clear_where` 的「删除 + 重算」不被
    /// 另一次 `clear_where` 交错成「A 删完、B 删完、A 重算、B 重算」（重算读的是
    /// 库里的当前状态，交错虽然不会算错，但两次调用各自的 `stats()` 返回值会
    /// 与自己的删除量对不上）。保留它还有一个现实理由：与 T2/T3 两个 store
    /// 的并发形状一致，读代码的人不必为这一个模块另建一套心智模型。
    /// **硬约束**：持这把锁期间绝不能再调 `logging::log` 一类的写日志函数 ——
    /// 日志要写同一个库，`std::sync::Mutex` 不可重入，会当场死锁。
    inner: Mutex<()>,
}

impl RequestStats {
    /// 构造存储句柄（库不可用时降级为空操作，见 `db` 字段的说明）。
    ///
    /// ── 签名为什么从 `new(directory, ...)` 改成接 `Db` ──────────
    /// 统计不再有「自己的目录」：数据在统一库的两张表里，路径这件事由 `Db`
    /// 唯一持有（`Db::file()`）。与 `AccountStore::with_db` /
    /// `LogStore::with_db` 同一形态，三个 store 的构造方式保持一致；
    /// `Option<Db>` 也一致（库打不开时仍能构造，只是降级）。
    ///
    /// ── 为什么不再有 `load()` ──────────────────────────────────
    /// 改造前构造时要读两个文件、按保留期各裁一遍、再对旧聚合行做一次口径回填
    /// （`backfill`）。现在库就是全集，没有可载入的东西：
    ///   - 「启动时裁一次超期行」由 `record` / `prune` 天然承担（它们每次都带上边界）；
    ///   - 「旧聚合行缺账号维度」只可能出现在**从旧文件迁上来的那批数据**里
    ///     （新代码写出的行三个维度恒齐），所以那次回填搬进了迁移项
    ///     （`import_legacy_daily`），运行期不再需要；
    ///   - 那条 `[Stats] 已载入请求统计 N 条明细` 的控制台输出随之取消 ——
    ///     它对应的「载入了多少」这个事实已经不成立。
    ///
    /// 回调每次裁剪时被调用，应读**内存快照**（如 `config::retention_settings()`），
    /// 不要每次读盘 —— 记账是每个请求都要走一次的路径。
    pub fn with_db(
        db: Option<Db>,
        get_retention: impl Fn() -> Retention + Send + Sync + 'static,
    ) -> Self {
        let file_path = match db.as_ref() {
            Some(db) => db.file().to_path_buf(),
            None => crate::server::config::config_dir().join(crate::server::db::FILE_NAME),
        };
        Self {
            db,
            file_path: RwLock::new(file_path),
            get_retention: Arc::new(get_retention),
            inner: Mutex::new(()),
        }
    }

    /// 取锁；中毒（某次持锁 panic）时接管继续用 —— 与 `LogStore::guard` /
    /// `AccountStore::guard` 同一取向：这把锁保护的是「调用序列」而不是数据本身，
    /// 锁内没有会被 panic 打断的中间态（真正的数据一致性由数据库事务保证）。
    fn guard(&self) -> MutexGuard<'_, ()> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// 库文件路径（`stats()` 的 `file` / `dailyFile` 与排障用）。
    ///
    /// 为什么是 `RwLock` 而不是裸字段：与 `LogStore::file_path` 同一处理 ——
    /// 字段本身不再被任何写路径改动（`relocate` 与两个 `*_file()` 访问器已随
    /// 单库语义删除），用 `RwLock` 是为了让「读现值」这件事与改造前同形，
    /// 改动面最小。
    fn file(&self) -> PathBuf {
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
    /// `std::sync::Mutex` 不可重入，会当场死锁。
    fn with_conn<T>(
        &self,
        _guard: &MutexGuard<'_, ()>,
        action: impl FnOnce(&rusqlite::Connection) -> rusqlite::Result<T>,
    ) -> Option<T> {
        let db = self.db.as_ref()?;
        match db.with(action) {
            Some(Ok(value)) => Some(value),
            Some(Err(error)) => {
                eprintln!("[Stats] 统计数据库操作失败: {error}");
                None
            }
            None => {
                eprintln!("[Stats] 统计数据库不可用（连接已标记中毒）");
                None
            }
        }
    }

    /// 与 [`RequestStats::with_conn`] 同理，但给 `&mut Connection` ——
    /// **需要事务的操作用它**：`record`（插明细 + 改聚合）、`clear_where`、
    /// `clear`、`prune`。
    fn with_conn_mut<T>(
        &self,
        _guard: &MutexGuard<'_, ()>,
        action: impl FnOnce(&mut rusqlite::Connection) -> rusqlite::Result<T>,
    ) -> Option<T> {
        let db = self.db.as_ref()?;
        match db.with_mut(action) {
            Some(Ok(value)) => Some(value),
            Some(Err(error)) => {
                eprintln!("[Stats] 统计数据库操作失败: {error}");
                None
            }
            None => {
                eprintln!("[Stats] 统计数据库不可用（连接已标记中毒）");
                None
            }
        }
    }

    /// 当前保留期（每次调用都取回调的实时值并归一化）
    fn retention(&self) -> Retention {
        (self.get_retention)().normalized()
    }

    /// 保留期的两条边界（明细用毫秒下界，聚合用日期键下界）。
    ///
    /// 两者都从「本地今天」往前推，且**每次调用都重算**（回调动态取），
    /// 于是设置里改完天数，下一次记账/裁剪就生效，不需要重启进程。
    fn retention_bounds(&self) -> RetentionBounds {
        RetentionBounds::of(self.retention())
    }

    /// 记一条请求：插一条明细 + 更新当天聚合（**一个事务**）。
    ///
    /// ── 与旧「内存累加」的语义等价性 ────────────────────────────
    /// 旧实现是「`insert_sorted` 进内存数组 + `guard.daily.entry(...).or_insert(...)`
    /// 累加 + 按需落盘」；现在是「`INSERT` 一行 + 读出当天聚合行、`fold_into_daily`
    /// 累加、整行 UPSERT」。三条性质逐一对应：
    ///   - **累加口径**：`fold_into_daily` 一个字都没改，总量与三个维度仍由它
    ///     一处推进（这是「各维之和 = 当天总量」的保证，见该函数的说明）；
    ///   - **顺序**：`ORDER BY ts, row_id` 与旧的「按 ts 升序的数组」同序，
    ///     同 ts 的条目按写入先后（`row_id` 递增）—— 与 `insert_sorted` 的
    ///     「同毫秒先到的在前」逐位一致；
    ///   - **可见性**：旧实现在同一次持锁里完成「进内存 + 落盘」，所以并发的
    ///     读取方要么看到整条记录、要么完全看不到；这里对应事务的原子提交，
    ///     提交前别的事务读不到这条明细，也不会读到「明细进了、聚合没进」的中间态
    ///     （那是旧实现两次独立写盘时会留下的错位）。
    ///
    /// ── 库存不可用时不报错、不抛错 ──────────────────────────────
    /// 记账在请求收尾路径上（`api::chat` 的 `record_entry`），它的失败**不能**
    /// 影响已经转发成功的请求：丢一条统计只是报表少一个数。
    pub fn record(&self, entry: NewRequestEntry) {
        let record = entry.normalize();
        let date = date_key(day_of(record.ts));
        let bounds = self.retention_bounds();
        let guard = self.guard();
        let _ = self.with_conn_mut(&guard, |conn| {
            let tx = conn.transaction()?;
            sql::insert_request(&tx, &record)?;
            // 保留期：每次记账顺手把超期数据裁掉。只在启动与 prune 时裁是不够的：
            // 桌面端常驻数周不重启，那样明细会一直涨到容量上限才开始丢，
            // 用户设的 30 天等于没生效。
            sql::delete_expired_requests(&tx, bounds.requests_ms)?;
            // 两个容量维度（旧实现分别是 `insert_sorted` 里那次 drain 与
            // `trim_daily` 里的 while 循环）。两个 trim 函数**自己先判条数**
            // 再决定发不发 DELETE —— 这是记账热路径，不该为「没超限」白跑一条
            // 带子查询的删除语句（见 `sql::trim_requests_capacity` 的说明）。
            sql::trim_requests_capacity(&tx, MAX_ENTRIES)?;
            // 聚合：**读-改-写当天那一行**（为什么不是增量的 UPDATE：见 `sql.rs` 模块头）
            let mut day = sql::select_daily_row(&tx, &date)?
                .unwrap_or_else(|| DailyEntry::new(date.clone()));
            fold_into_daily(&mut day, &record);
            sql::upsert_daily(&tx, &day)?;
            sql::delete_daily_before(&tx, &bounds.daily_key)?;
            sql::trim_daily_capacity(&tx, MAX_DAILY_DAYS)?;
            tx.commit()
        });
    }

    /// 报表聚合。数据来自两张表，计算是 `report` 的纯函数。
    ///
    /// `range` 的非法值一律按 `"7"` 处理，并在结果里回显归一化后的值：
    /// 报表是只读展示，为一次拼错的参数让整页报错，不如给个合理默认
    /// （前端也不用为这个场景做错误态）。路由层（`api::stats_api::stats_summary`）
    /// 另外对非法值给 400 —— 那是用户在选择器上显式选的值，静默换区间会
    /// 让页面显示的数据与选项对不上；两层各管一件事，这里保留兜底。
    ///
    /// ── 读取窗口 ────────────────────────────────────────────────
    /// 聚合整表读（一年最多 365 行）；明细只取**近 7 天** ——
    /// `cacheRates` 最长的窗口是 7 天、`cacheTrend24h` 是 24 小时，
    /// 窗口外的条目在纯函数里本来就会被逐条判掉，不必先读回内存。
    /// `now` 在这里取**一次**并同时用于查询上界与两个纯函数的窗口判定：
    /// 取两次的话，两次之间到达的条目可能落在「查出来了但不在窗口内」（无害）
    /// 或「在窗口内但没查出来」（少算）这两种边界上，而前者只是白读、后者是错。
    ///
    /// 降级：库不可用时**按空库算同一份结果**（不是另写一套空值 JSON）——
    /// 前端读到的形状与「这段时间没有数据」完全一致，所以它拿到的是
    /// 「暂无数据」的空态，而不是 undefined 造成的渲染异常
    /// （`ui/report.js` 的 `overviewHtml` / `paintRank` 都按 `Number(x) || 0`
    /// 与 `Array.isArray` 消费，空态文案是「所选范围内还没有请求记录」）。
    pub fn usage_summary(&self, range: &str) -> Value {
        let now = clock::now_ms();
        let loaded = {
            let guard = self.guard();
            self.with_conn(&guard, |conn| {
                let daily = sql::select_daily_map(conn)?;
                let entries = sql::select_between(conn, now - SUMMARY_WINDOW_MS, now)?;
                Ok((daily, entries))
            })
        };
        let (daily, entries) = loaded.unwrap_or_default();
        summarize(&daily, &entries, range, now)
    }

    /// 明细查询：倒序（新在前）分页。
    ///
    /// 顺序由 SQL 的 `ORDER BY ts DESC, row_id DESC` 保证 —— 与旧的「内存恒按 ts
    /// 升序，倒序 == 反向遍历」逐位一致，也不会出现「长请求晚收尾导致记录顺序
    /// 飘忽」的翻页错乱（同 ts 的次序由 `row_id` 定死）。
    ///
    /// 过滤条件与 `clear_where` **共用同一份** `sql::FilterPlan`（见那里的说明）：
    /// 「页面上筛出来的 N 条」与「清空删掉的那批」必然是同一个集合。
    ///
    /// 降级：库不可用时给 `{entries: [], total: 0, matched: 0}` ——
    /// 前端 `ui/requests-panel.js` 对这个形状的读法是 `Number(result?.total) || 0`
    /// 与 `Array.isArray(result?.entries)`，空表会显示「暂无请求日志」（`emptyText`），
    /// 不会崩也不会误报有内容。**注意它不会把读数清成 0 而不作声**：
    /// 读取**失败**（HTTP 错误）走的是另一条路（`render('读取请求日志失败…')`
    /// 与徽标退成「—」），而这里返回的是「成功但为空」——两者的区别在前端是有意的。
    pub fn query_requests(&self, filter: &RequestQuery) -> Value {
        // limit 夹在 [1, MAX_LIMIT]：前端传 0 或十万都不该让接口躺平
        let limit = filter.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let plan = sql::FilterPlan::of(filter);
        let loaded = {
            let guard = self.guard();
            self.with_conn(&guard, |conn| {
                let total = sql::count_all(conn)?;
                let matched = sql::count_matching(conn, &plan)?;
                let entries = sql::select_page_desc(conn, &plan, filter.offset, limit)?;
                Ok((total, matched, entries))
            })
        };
        let (total, matched, entries) = loaded.unwrap_or((0, 0, Vec::new()));
        // 每行经 `entry_json` 补一个派生字段 `providerLabel`（id → label 的换算；
        // 换算处与汇总的 providers 数组同一个函数，两处名字必然一致）。
        // 汇总是**反序列化回 Value**，不是另一套结构：契约字段仍由 record.rs
        // 的 serde 注解决定，这里只加不删
        let rows: Vec<Value> = entries.iter().map(entry_json).collect();
        json!({
            "entries": rows,
            "total": total,
            "matched": matched,
        })
    }

    /// 按筛选条件清空明细，并**重算受影响日期**的按天聚合。
    /// 返回 `{ removed: 删除条数, ...stats() }`。
    ///
    /// 为什么聚合要重算而不是留着：聚合行是「当天全部明细」的累计，删掉其中
    /// 一部分后数字就对不上账（报表的请求数会大于明细能数出的请求数）。
    /// 受影响的日期（被删明细涉及的那些天）从**剩余明细**重新聚合 ——
    /// 聚合的全部字段都由明细逐条累加而来（`record` 与重算共用 `fold_into_daily`），
    /// 口径不会漂。
    ///
    /// ── 为什么是「整行重算」而不是「从原聚合行里减去命中的量」────
    /// 减法只能把总量列改对，三个维度数组（模型 / provider / 账号）里的量
    /// 没法可靠地减 —— 被删的条目分别属于哪些组、那些组减完是否该消失，
    /// 都要在减法里重写一遍「怎么分组、怎么建组、空组怎么处理」，
    /// 而「各维之和 = 当天总量」这条对账前提就变成了两处维护。
    /// 重算走的是与记账**同一个函数**（`fold_into_daily`），
    /// 这条不变式由同一段代码保证（这也是 `backfill` 模块头强调的取舍）。
    ///
    /// ── 涉及的日子怎么定 ────────────────────────────────────────
    /// 删之前先取回命中明细的 `ts`，在 Rust 侧用 `date_key(day_of(ts))` 折算日期
    /// —— **不能**用 SQL 的 `date(ts)`：它按 UTC 切分，而本模块的「一天」是本地
    /// 自然日（UTC+8 的凌晨会整体偏一格，见 `clock` 模块头）。
    ///
    /// 整个过程在**一个事务**里：中断不会留下「明细删了、聚合没重算」的错位
    /// （旧实现是两个文件两次写盘，中间崩溃就会留下这种状态）。
    ///
    /// 与 `clear()` 同一取舍：不改保留期设置。
    /// 全部条件都缺省时不会走到这里（路由层直接走 `clear()`）。
    ///
    /// 降级：库不可用时返回 `removed: 0` ——「一条都没删」是唯一诚实的答案，
    /// 谎报删除条数会让前端提示「已清空 N 条」而库里什么都没变。
    pub fn clear_where(&self, filter: &RequestQuery) -> Value {
        let plan = sql::FilterPlan::of(filter);
        let removed = {
            let guard = self.guard();
            self.with_conn_mut(&guard, |conn| {
                let tx = conn.transaction()?;
                // ① 命中的明细涉及哪些天（这些天的聚合要重算）
                let affected: BTreeSet<NaiveDate> = sql::select_matching_ts(&tx, &plan)?
                    .into_iter()
                    .map(day_of)
                    .collect();
                let removed = sql::delete_matching(&tx, &plan)?;
                // ② 从剩余明细整行重算（一条不剩的日子整天删除）
                for day in &affected {
                    rebuild_day(&tx, *day)?;
                }
                tx.commit()?;
                Ok(removed)
            })
            .unwrap_or(0)
        };
        // 统计在**新的**一次取锁里重取（`stats()` 自己也要取这把锁，而
        // `std::sync::Mutex` 不可重入 —— 所以必须先放掉上面那把）
        let mut stats = self.stats();
        if let Some(object) = stats.as_object_mut() {
            object.insert("removed".to_string(), Value::from(removed as u64));
        }
        stats
    }

    /// 立即按当前保留期裁剪（供「改小保留天数后立即清理」用）。
    ///
    /// 只按**时间维度**裁（明细按 `ts`、聚合按 `date` 键），再各收一次容量上限
    /// （手改库塞进十万行的兜底）。「立刻生效」在数据库上天然成立 ——
    /// 没有延迟落盘这一说，所以调用方（改小设置后点清理）期望的「盘上也干净了」
    /// 直接就是事实。
    ///
    /// 降级：库不可用时什么都不做 —— `prune` 的契约是「尽力清理」，
    /// 它没有返回值可以报告失败（调用方 `stats_api` 调完就返回响应），
    /// 而失败已经由 `with_conn_mut` 打了一行控制台。
    pub fn prune(&self) {
        let bounds = self.retention_bounds();
        let guard = self.guard();
        let _ = self.with_conn_mut(&guard, |conn| {
            let tx = conn.transaction()?;
            sql::delete_expired_requests(&tx, bounds.requests_ms)?;
            sql::trim_requests_capacity(&tx, MAX_ENTRIES)?;
            sql::delete_daily_before(&tx, &bounds.daily_key)?;
            sql::trim_daily_capacity(&tx, MAX_DAILY_DAYS)?;
            tx.commit()
        });
    }

    /// 清空明细与按天聚合，返回清空后的存储概况（供「清空统计数据」按钮）。
    ///
    /// 为什么不是「用 `prune` 裁到 0 天」：保留期的下限是 1 天（`normalized()`
    /// 把 0 夹成 1），因此 `prune` 永远留得住今天的数据，表达不了「清空」。
    /// 这里直接删两张表的全部行，语义明确：清空后立即读到的就是 0 条，
    /// 重开程序也不会把已删的数据载回来。**不改保留期设置**：清数据与改配置是两件事。
    ///
    /// 降级：库不可用时返回空统计（`total: 0`）。调用方从响应里看不出
    /// 「其实没清掉」，但那一行 `[Stats] 统计数据库操作失败` 已经打到控制台 ——
    /// 与 T3 的 `LogStore::clear` 同一取向：清空统计失败不是会让请求出错的事。
    pub fn clear(&self) -> Value {
        {
            let guard = self.guard();
            let _ = self.with_conn_mut(&guard, |conn| {
                let tx = conn.transaction()?;
                sql::delete_all_requests(&tx)?;
                sql::delete_all_daily(&tx)?;
                tx.commit()
            });
        }
        self.stats()
    }

    /// 存储概况（排障与报表页脚用）
    ///
    /// `file` / `dailyFile` 都指向**库文件**（不再有明细文件与聚合文件之分，
    /// 见 `file_path` 的说明）。`firstTs` / `lastTs` 取明细 `ts` 的最小 / 最大值，
    /// 与旧实现的「升序数组首尾元素的 ts」等价。
    ///
    /// 降级：库不可用时给全 0 且 `firstTs` / `lastTs` 为 `null`。
    /// 消费方 `api::storage_api` 读的是 `total` 与 `dailyDays`（设置页的
    /// 存储概况显示「请求记录 N 条 / 报表 N 天」）；库不可用时那两个值都是 0，
    /// 而 `storage_api` 会同时给出 `available: false`，界面据此显示「—」
    /// 而不是一排看着正常的 0（0 与「真的没有数据」在数字上无法区分）。
    pub fn stats(&self) -> Value {
        let retention = self.retention();
        let loaded = {
            let guard = self.guard();
            self.with_conn(&guard, |conn| {
                let total = sql::count_all(conn)?;
                let days = sql::count_daily(conn)?;
                let first = sql::min_ts(conn)?;
                let last = sql::max_ts(conn)?;
                Ok((total, days, first, last))
            })
        };
        let (total, daily_days, first_ts, last_ts) = loaded.unwrap_or((0, 0, None, None));
        let file = self.file().to_string_lossy().to_string();
        json!({
            "total": total,
            "dailyDays": daily_days,
            "maxEntries": MAX_ENTRIES,
            "maxDailyDays": MAX_DAILY_DAYS,
            "file": file,
            // 与 file 同值：明细与聚合同居一个库。保留这个字段是为了不破坏
            // 既有读取方（`file` 的语义已经说明它指库文件），但**不要再把两者
            // 的字节数相加** —— 改造前前端就这么算过，而它们指向同一个文件，
            // 结果是大小被算了两遍。设置页现在只读 `database.bytes` 一个数。
            "dailyFile": file,
            "firstTs": first_ts,
            "lastTs": last_ts,
            "today": date_key(today()),
            "retention": {
                "requestDays": retention.request_days,
                "dailyDays": retention.daily_days,
            },
        })
    }

    /// 退出时收尾：把 WAL 并回主库文件并清掉 `-wal` / `-shm` 残留。
    ///
    /// ── 为什么不再是「补写未落盘的内容」────────────────────────
    /// 旧实现有一套延迟落盘（聚合行变更满 50 次或距上次落盘超 60 秒才重写整个
    /// 文件），停机路径必须显式 `flush()` 一次，否则用户刚看到的请求在下次启动后
    /// 会少一截（`server/mod.rs` 的停机注释写的就是这件事）。进数据库后**每次
    /// 记账都立即提交**，没有任何「内存里还没落盘」的状态，那个理由整体消失。
    ///
    /// ── 那为什么还要做一次 checkpoint ────────────────────────────
    /// 注意它**不是**为了数据安全：WAL 本身是崩溃安全的（`synchronous=NORMAL`
    /// 下掉电最多丢最近若干已提交事务，库不会损坏），已提交的数据下次打开时
    /// 由 WAL 自动恢复。做它有两个实在的好处：
    ///   1. 会话结束后主库文件就是全部数据 —— 用户备份、拷贝、看「占用多大」时
    ///      只有一个文件是真相（不 checkpoint 的话数据散在 `-wal` 里，
    ///      `api::storage_api` 的 `file_size` 只统计主库文件，会显得比实际小）；
    ///   2. 清掉 `-wal` / `-shm` 残留文件，不留一个「进程退了但盘上还有半截
    ///      WAL」的观感。
    /// 代价是停机时一次全量检查点（把 WAL 里的页写回主库）—— 退出路径上的一次
    /// IO，用户感知不到（库通常在几十 MB 以内）。
    /// `TRUNCATE` 会在检查点完整完成后把 WAL 截成 0 字节；若中途有读锁挡着
    /// （本例只有一个连接，实际不会），SQLite 会保留 WAL 而不报错 —— 所以这里
    /// 忽略返回值是安全的：失败只意味着「这次没收敛」，数据仍然完好。
    pub fn flush(&self) {
        let guard = self.guard();
        let _ = self.with_conn(&guard, |conn| {
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
            Ok(())
        });
    }
}

/// 保留期的两条边界（明细用毫秒下界，聚合用日期键下界）。
///
/// `pub(crate)`：迁移项要拿它算同一套边界（见 `legacy_bounds`），
/// 而字段仍只在本模块内被读。
pub(crate) struct RetentionBounds {
    /// 明细的毫秒下界（本地日期零点）
    requests_ms: i64,
    /// 聚合的日期键下界（`YYYY-MM-DD`）
    daily_key: String,
}

impl RetentionBounds {
    /// 从归一化后的保留期算出两条边界。
    ///
    /// 保留 N 天 = 含今天在内的 N 个自然日，所以往前推 N-1 天。
    /// 运行期与迁移项**共用这一份**：迁移导入旧数据后也要按同一套边界裁一遍，
    /// 两处各算一份迟早分叉（而分叉的后果是「同一个设置在迁移前后裁掉的天数不同」）。
    fn of(retention: Retention) -> Self {
        let now_day = today();
        Self {
            requests_ms: local_midnight_ms(now_day - ChronoDuration::days(retention.request_days - 1)),
            daily_key: date_key(now_day - ChronoDuration::days(retention.daily_days - 1)),
        }
    }
}

/// 重算某一天的聚合行：从**剩余明细**整行重算；一条不剩就删掉那一行。
///
/// `day` 是本地自然日，区间按 `[当天零点, 次日零点)` 取 —— 与 `date_key(day_of(ts))`
/// 的归档口径同源（都用 `clock` 的本地时区工具），所以「某条明细属于哪一天」
/// 与「重算时按哪个区间取它」不可能对不上。
fn rebuild_day(conn: &rusqlite::Connection, day: NaiveDate) -> rusqlite::Result<()> {
    let key = date_key(day);
    let start = local_midnight_ms(day);
    // 开区间上界减 1 毫秒，等价于 `[start, next_start)`；用 1ms 而不是直接取
    // `next_start - 1` 是为了避开「夏令时切换当天次日零点不存在」这类边界
    // （`local_midnight_ms` 有兜底，但让它只负责一个方向更简单）
    let end = local_midnight_ms(day + ChronoDuration::days(1)).saturating_sub(1);
    let entries = sql::select_between(conn, start, end)?;
    if entries.is_empty() {
        sql::delete_daily(conn, &key)?;
        return Ok(());
    }
    let mut rebuilt = DailyEntry::new(key);
    for item in &entries {
        fold_into_daily(&mut rebuilt, item);
    }
    sql::upsert_daily(conn, &rebuilt)
}

/// 报表装配：把两张表的读数交给 `report` 的纯函数，拼出 `/api/stats/summary` 的响应。
///
/// 为什么把这段从 `usage_summary` 里抽出来：库不可用时的降级值是「按空库算同一份
/// 结果」（见该方法的说明），抽成纯函数后两条路径**必然同形** ——
/// 手写一份全零的 JSON 迟早会与正常路径的形状漂开，而形状漂开是静默的
/// （前端只会显示空白或 0，不会报错）。
fn summarize(
    daily: &BTreeMap<String, DailyEntry>,
    entries: &[RequestEntry],
    range: &str,
    now: i64,
) -> Value {
    let now_day = today();
    let range = normalize_range(range);
    let (start_date, end_date) = range_bounds(range, daily, now_day);

    // overview / dailyTrend / heatmap 走聚合（跨年；明细只有请求天数）
    let totals = range_totals(daily, &start_date, &end_date);
    let top_model = build_top_model(&totals.model_totals, totals.tokens);
    // providers 与 topModel **同源同区间**：都从这次的 `range_totals` 出，
    // 于是「按 provider 的请求数之和」必然等于 overview.requests，
    // 前端把它们并排显示时不会出现互相对不上的数
    let providers = build_providers(&totals.provider_totals);
    // accounts 与 providers / topModel **同源同区间**（同上）：账号排行的
    // 请求数之和也等于 overview.requests
    let accounts = build_accounts(&totals.account_totals);
    let trend = daily_trend(daily, &start_date, &end_date);
    let map = heatmap(daily, now_day);
    let consecutive = streak(daily, now_day);

    // cacheRates / cacheTrend24h 走明细（窗口 ≤7 天，明细够用）
    let rates = cache_rates(entries, now);
    let cache_trend = cache_trend_24h(entries, now);

    json!({
        "range": range,
        "startDate": start_date,
        "endDate": end_date,
        "overview": {
            "requests": totals.requests,
            "successful": totals.successful,
            "tokens": totals.tokens,
            "activeDays": totals.active_days,
            "streak": consecutive,
            "topModel": top_model,
        },
        // 按 provider 维度的区间汇总（**新增字段，不改既有字段**）。
        // 前端按「存在则展示、缺失则隐藏」消费，所以旧前端拿到它只会忽略。
        // 恒为数组（无数据时是空数组而不是 null）：前端不必判两种空形态
        "providers": providers,
        // 按账号维度的区间汇总（**新增字段，不改既有字段**，与 providers 同形态）。
        // 账号是比 provider 更细的一维（一家可挂多个账号），所以这张排行回答的是
        // 「具体哪个登录态在出力」——同一家的多个账号会各占一行。
        "accounts": accounts,
        "heatmap": map,
        "cacheRates": rates,
        "cacheTrend24h": cache_trend,
        "dailyTrend": trend,
    })
}

/// 把一条明细累加进当天的聚合行。
///
/// `record` 的记账与 `clear_where` 的重算（还有迁移的口径回填）共用这一段：
/// 聚合行的每个字段都**只**由明细逐条累加而来，几条路径手写几遍必然漂移
/// （对不上账）。三个维度（模型 / provider / 账号）与总量并列累计，
/// 各维求和都等于当天总量，这是报表之间能对账的前提。
fn fold_into_daily(day: &mut DailyEntry, item: &RequestEntry) {
    let success = item.is_success();
    day.requests += 1;
    if success {
        day.successful += 1;
    }
    day.tokens += item.total_tokens;
    day.cache_hit_tokens += item.cache_read_tokens;
    day.cache_input_tokens += item.prompt_tokens;
    push_model_accum(&mut day.model_tokens, &item.model, item.total_tokens, 1);
    // 空 provider 也建组（见 push_provider_accum 的注释）
    push_provider_accum(
        &mut day.provider_stats,
        &item.provider,
        1,
        i64::from(success),
        item.total_tokens,
    );
    push_account_accum(
        &mut day.account_stats,
        &item.account_id,
        &item.account_name,
        1,
        i64::from(success),
        item.total_tokens,
    );
}
