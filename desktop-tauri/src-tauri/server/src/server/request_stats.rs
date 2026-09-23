//! 请求统计存储：`requests` 表（明细）+ `request_daily` 表（按天聚合）+
//! `request_raw` 表（原始正文，预览对话的地基，见 `record::MAX_RAW_BODY_BYTES`）。
//!
//! ── 内部分层（拆开是为了每片都保持单一职责、单文件不过长）─────
//! ```text
//! server/
//!   request_stats.rs     存储本体：记账（进行中行 → 在途回写 → 终态收尾）/
//!                        查询 / 读取（本文件）
//!   request_stats/
//!     sql.rs      行级 SQL：明细的读写删裁 + 跨表共用的 FilterPlan（原始 SQL 唯一
//!                 出现的地方之一；另一处是 daily.rs 与杂项子模块各自的语句）
//!     daily.rs    行级 SQL：`request_daily` 聚合表的编解码与读-改-写
//!     record.rs   数据类型与 JSON 契约（RequestEntry / DailyEntry / Retention…）
//!     report.rs   报表计算：纯函数（区间、补零、命中率、topModel、连续天数）
//!     aggregate.rs 聚合计算：日报折算（fold_into_daily）/ 整行重算 / 报表装配
//!     maintenance.rs 维护动作：两模式清理 / 清理预览 / 数据库压缩（VACUUM）
//!     clock.rs    本地时区工具（chrono::Local 在整模块的唯一使用点）
//!     backfill.rs 旧聚合行的口径回填（**只由迁移项调用**，见那边的说明）
//!     legacy.rs   旧文件导入的入口：解析旧 JSONL、算迁移期边界、写库+裁剪
//!                 （同样只由迁移项调用，运行期一次都不会走到）
//! ```
//!
//! ── 读侧接口的接线状态 ──────────────────────────────────────
//! 读侧（`usage_summary` / `query_requests` / `prune` / `stats` / 清理与压缩）
//! 已由 `api::stats_api` 的路由接上，这些接口上的 `#[allow(dead_code)]`
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
//! `request_raw` 是本次新增的第三张表：下游原始正文（请求侧 / 响应侧各 128 KiB
//! 上限），独立成表的理由（大字段不进列表查询）见 `db/schema.rs` 的 V3_SCHEMA。
//! 两个文件变两张表还顺带解决了原来的两个麻烦：**明细与聚合的一致性**（原来是
//! 两次独立写盘，中间崩溃会留下「明细删了、聚合还在」的错位）现在由**一个事务**
//! 保证；正文随明细的**同步清理**（淘汰 / 清空时先删正文再删明细）见
//! `maintenance.rs` 模块头的约定。
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
// 聚合计算（日报折算 / 重算 / 报表装配）与维护动作（两模式清理 / 预览 /
// 数据库压缩）：各自的拆分理由见两个子模块的模块头。
mod aggregate;
mod daily;
mod maintenance;
mod record;
// 报表计算层（纯函数）与「模型请求日志」的查询实现。
//
// 这里**不放模块级 allow**：本层已由 `api::stats_api` 的报表路由接线，
// 于是「新增了一个算好却没人用的函数」会直接报 warning —— 那正是我们想知道的。
// 模块内部各函数仍应是 `pub(super)`：只有 `request_stats.rs` 能调到它们，
// 对外暴露面收敛在这一层。
mod report;
mod sql;

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, RwLock};

use chrono::Duration as ChronoDuration;
use serde_json::{json, Value};

use crate::server::db::Db;
use crate::server::logging;

use clock::{date_key, day_of, local_midnight_ms, now_ms, today};
use record::DailyEntry;
// 拆到子模块的三样，这里引回：record / usage_summary 的折算入口、
// remap 的库操作体、报表装配（均为 `pub(super)`，见 aggregate.rs 的模块头）
use aggregate::{fold_into_daily, remap_model_days, summarize};
use report::{entry_json, provider_label};

// MAX_ENTRIES / MAX_DAILY_DAYS / MAX_RAW_ROWS / MAX_RAW_BODY_BYTES 一并在此
// re-export：本模块内部要用（记账容量闸、raw_body 的截断判定），api::pipeline
// 的采集侧也要用（同一份上限常量，见 record::MAX_RAW_BODY_BYTES 的说明）。
// CompactStart 在此 re-export：api::stats_api 的 compact 路由按它分派响应。
// VacuumStatus 只在本模块内（字段类型 / maintenance 的方法）可见。
use maintenance::VacuumStatus;
pub use maintenance::CompactStart;
pub use record::{
    AttemptDetail, NewRequestEntry, RequestEntry, RequestQuery, Retention, RetryEvent,
    RunningProgress, SensitiveHit, DEFAULT_LIMIT, MAX_DAILY_DAYS, MAX_ENTRIES, MAX_LIMIT,
    MAX_RAW_BODY_BYTES, MAX_RAW_ROWS,
};

/// 明细窗口：四档缓存命中率里最长的是 7 天，趋势窗口是 24 小时 ——
/// 报表要从明细里取的数据**全部落在这个窗口内**，所以查询按它下界，
/// 不必把 2 万条明细整体读回内存再逐条丢。
const SUMMARY_WINDOW_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// 筛选下拉的候选上限（模型名与 provider id 各一份）。
///
/// 200 是个「够用且不失控」的数：真实使用的模型名通常在几十个量级，而这份清单
/// 按出现次数降序，尾部那些只出现一两次的值本来也没人会用下拉去选。
/// 截断的只是**候选**，不影响筛选能力 —— 前端会把当前选中的值保留在列表里
/// （见 `ui/requests-panel.js` 的 `fillFilterSelect`）。
const MAX_FILTER_OPTIONS: usize = 200;

/// 「进行中」行的陈旧阈值：status=0 且开始时刻距今超过 1 小时视为僵尸行。
///
/// 为什么是 1 小时：进行中行由收尾记账（`record` 的 UPDATE）消除；正常请求
/// 从转发开始到收尾最多几分钟（上游超时 + 重试链的全长），1 小时还停在
/// status=0 的行只剩两种成因 —— 网关进程崩溃（桌面端常驻数周，崩溃行会
/// 一直留着）与响应流任务泄漏。它们没有任何终态值（token 全 0、无状态码），
/// 留在列表里只会伪装成「一条永远在跑的请求」，所以到点清掉。清理时机挂在
/// `record_started` 上（每次有新请求开始时顺手扫一遍），不需要定时器。
const RUNNING_STALE_MS: i64 = 60 * 60 * 1000;

/// 进程重启后遗留的进行中行按 `ts` 判定：早于本次启动时刻的全是孤儿。
/// 以进程启动时间做阈值而不是固定时长，能覆盖长请求在重启时仍在途的场景。
fn stale_cutoff() -> i64 {
    std::cmp::max(now_ms() - RUNNING_STALE_MS, crate::server::logging::process_started_at())
}

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
    /// 数据库压缩（VACUUM）的运行状态。`Arc` 是因为完成回调在**另一个线程**
    /// 里执行，它要能独立更新这份状态（`RequestStats` 本体不必被线程持有）。
    ///
    /// 为什么不做成模块级 `static`：`RequestStats` 本来就是进程级单例
    /// （`ServerState` 里那一份 `Arc`），状态挂在实例上让「有多个实例的测试」
    /// 各自独立；OmniProxy 用模块级 let 变量是因为它的 router 是文件级单例，
    /// 两边语义等价，这里选与本项目结构一致的挂法。
    vacuum: Arc<VacuumStatus>,
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
            vacuum: Arc::new(VacuumStatus::new()),
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
    /// ── 「先 UPDATE 后 INSERT」（本次改造：进行中行的收尾）───────
    /// 转发开始前 `record_started` 已经插了一条 status=0 的行；本方法是它的
    /// **终态收尾**：先按 `id + status=0` UPDATE 补全全部终态字段（row_id 与
    /// 「进行中」期间在页面上占的位置都保持不变），UPDATE 影响 0 行再 INSERT ——
    /// 转发前就失败等没有 started 行的路径维持原有行为。为什么不用唯一索引 +
    /// UPSERT：存量数据 id 可能重复、不能假设唯一，论证见
    /// `sql::update_running_request`。两条路径都会走到下面的聚合折算 ——
    /// 「进行中行不折算、终态统一折算一次」，日报口径与改造前等价。
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
            // 先补全进行中行；没有进行中行（影响 0 行）才新插
            let updated = sql::update_running_request(&tx, &record)?;
            if updated == 0 {
                sql::insert_request(&tx, &record)?;
            }
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
            let mut day = daily::select_daily_row(&tx, &date)?
                .unwrap_or_else(|| DailyEntry::new(date.clone()));
            fold_into_daily(&mut day, &record);
            daily::upsert_daily(&tx, &day)?;
            daily::delete_daily_before(&tx, &bounds.daily_key)?;
            daily::trim_daily_capacity(&tx, MAX_DAILY_DAYS)?;
            tx.commit()
        });
    }

    /// 记一条**进行中**的请求（status=0；转发开始前、模型解析成功后调用）。
    ///
    /// ── 为什么要有这一步 ────────────────────────────────────────
    /// 改造前一条明细要等到转发收尾才落库：流式请求在**最后一个字节发完**时
    /// 才记账（`RecordingStream` 的 settle），于是一条跑了半分钟的请求在这半
    /// 分钟里对请求日志页完全不可见。进行中行把「这条请求开始了」提前到转发
    /// 开始那一刻：列表页能看到它（status=0，前端显示为进行中），收尾时由
    /// `record` 的 UPDATE 把终态字段补全 —— 一次请求仍然恰好一行。
    ///
    /// ── 生命周期（started → update → stale 清理）────────────────
    ///   1. `record_started`：插 status=0 行（只有 id / ts / 模型名）；
    ///   2. `record`：UPDATE 该行补全终态字段（UPDATE 影响 0 行则说明没有
    ///      started 行 —— 转发前就失败的路径，走 INSERT，行为同改造前）；
    ///   3. 若进程崩溃 / 流任务泄漏，行永远停在步骤 1 —— `delete_stale_running`
    ///      在每次 `record_started` 时顺手把「超过 [`RUNNING_STALE_MS`] 还在
    ///      进行中」的行删掉（阈值见该常量的说明）。
    ///
    /// 进行中行**不进日报**（`fold_into_daily` 按 status=0 跳过）：它没有
    /// 任何终态值，折算进聚合会让报表凭空多一次请求、而 token 是 0 ——
    /// 等它收尾时终态行会正常折算，一次也不多、一次也不少。
    ///
    /// ── 幂等与降级 ──────────────────────────────────────────────
    /// 同 id 已有进行中行时跳过（重复调用不产生第二行）；库不可用时静默跳过
    /// —— 这一步的失败只是「列表里晚一点才看到这条请求」，与统计整体的
    /// 「少记不影响请求」同一取向。
    pub fn record_started(&self, id: &str, ts: i64, model: &str, client_model: &str) {
        if id.is_empty() {
            return;
        }
        let guard = self.guard();
        let _ = self.with_conn_mut(&guard, |conn| {
            let tx = conn.transaction()?;
            sql::insert_started_request(&tx, id, ts, model, client_model)?;
            // 陈旧清理与插入同事务：僵尸行的判定时点与本次开始时点一致，
            // 中断也只影响「这次有没有清成」，不会留下半删状态
            sql::finish_stale_running(&tx, stale_cutoff(), now_ms())?;
            tx.commit()
        });
    }

    /// 收尾上一次运行遗留的进行中行（启动时调一次）。
    ///
    /// `record_started` 里那次清理要等第一条请求进来才跑；启动时刻主动扫一遍，
    /// 界面一打开就不会再看到「上一条卡死的请求」。判定与清理口径完全一致
    /// （同 `stale_cutoff()` 与 `finish_stale_running`），不引入第二套规则。
    pub fn sweep_stale_running(&self) {
        let guard = self.guard();
        let _ = self.with_conn_mut(&guard, |conn| {
            let tx = conn.transaction()?;
            sql::finish_stale_running(&tx, stale_cutoff(), now_ms())?;
            tx.commit()
        });
    }

    /// 回写一条「进行中」行的**在途**字段（转发期间每次状态真的变化时调一次）。
    ///
    /// ── 为什么需要它 ────────────────────────────────────────────
    /// `record_started` 插的那行只有 id / ts / 模型名，而选路与发送体定稿都发生在
    /// 它**之后** —— 于是整段转发期间列表里那一行的「提供商 / 账号 / 上游模型」
    /// 都是空的（前端显示成「—」），而这几样其实在请求真正发出去之前就已经确定。
    /// 改造前 `requests` 表上只有两条写路径：收尾（`record` 的 UPDATE）与僵尸行
    /// 清理。本方法是第三条：**在途回写**，让进行中行从「只有模型名」变成
    /// 「谁在承载、转发的是什么、已经试了几轮」。
    ///
    /// ── 谁调它、多久调一次 ──────────────────────────────────────
    /// `api::pipeline::live_row_sink` 生成的钩子，由 `RequestTelemetry` 在**内容
    /// 真的变了**时调用（同一份状态重复上报只写一次，见 `usage::LiveStamp`）——
    /// 所以一次请求的写入次数是「状态变化次数」而不是「上报次数」（流式路径每
    /// 收一帧都会 `note_first_frame`，靠指纹闸挡在库外）。接线在 api 层、telemetry
    /// 只持有一个闭包：core 不认识存储层（依赖方向见 `core/mod.rs` 的约定）。
    ///
    /// ── 幂等与降级 ──────────────────────────────────────────────
    /// 匹配 `id + status = 0`（与收尾同一条件）：收尾之后打空，在途回写晚到一步
    /// 不会把终态覆盖回去；同一条请求写多少次都只是覆盖同一行的同几列。库不可用
    /// 时静默跳过 —— 与 `record_started` 同一取向：少一次在途刷新不影响任何业务，
    /// 界面退化成「进行中行只有模型名」的既有样子。
    pub fn update_running(&self, id: &str, progress: &RunningProgress) {
        if id.is_empty() {
            return;
        }
        let guard = self.guard();
        let _ = self.with_conn_mut(&guard, |conn| sql::update_running_progress(conn, id, progress));
    }

    /// 存一条请求的**原始正文**（`request_raw` 表；两侧各截断到
    /// `api::pipeline::MAX_RAW_BODY_BYTES`，截断在采集处完成）。
    ///
    /// ── 为什么独立于 `record` ───────────────────────────────────
    /// 正文的大字段不该混进明细记账的热路径（每次记账多搬几百 KB）；同时
    /// 只有部分请求有正文可存（`record_early_failure` 没有请求体、错误响应
    /// 没有响应体）。独立方法让调用方（`api::pipeline::record_entry`）在明细
    /// 记账后按需调一次，两侧全空时不写行。
    ///
    /// `id` 为空直接跳过（正文按 id 与明细关联，没有 id 的正文无从取回）。
    /// UPSERT 覆盖同 id（`sql::upsert_raw`）；容量闸 [`MAX_RAW_ROWS`] 每次写入
    /// 顺手收一次。失败只打控制台 —— 正文是排障辅助，丢一侧不能影响请求。
    pub fn store_raw(&self, id: &str, ts: i64, request_body: Option<&str>, response_body: Option<&str>) {
        let id = id.trim();
        if id.is_empty() {
            return;
        }
        let request = request_body.unwrap_or("");
        let response = response_body.unwrap_or("");
        // 两侧全空不值得占一个容量名额：表的约定是「有行 ⇔ 有正文」
        if request.is_empty() && response.is_empty() {
            return;
        }
        // 字节长度合计（截断后的落库体积；与 debug_traffic 的 size 口径同理）
        let size = (request.len() + response.len()) as i64;
        let guard = self.guard();
        let _ = self.with_conn_mut(&guard, |conn| {
            let tx = conn.transaction()?;
            sql::upsert_raw(&tx, id, ts, request, response, size)?;
            sql::trim_raw_capacity(&tx, MAX_RAW_ROWS)?;
            tx.commit()
        });
    }

    /// 按 id 取一条原始正文（详情弹窗的数据源；无行返回 None，路由层给 404）。
    ///
    /// `truncated` 是读取时的**启发式判定**：任一侧长度达到采集上限
    /// （`api::pipeline::MAX_RAW_BODY_BYTES`）就认为可能被截断过 —— 恰好等于
    /// 上限而没截断的报文会被误标，代价只是前端多显示一个「已截断」提示；
    /// 反过来（截断了却不标）会让用户把半截响应当完整正文，那才是要避免的。
    pub fn raw_body(&self, id: &str) -> Option<Value> {
        let id = id.trim();
        if id.is_empty() {
            return None;
        }
        let loaded = {
            let guard = self.guard();
            self.with_conn(&guard, |conn| sql::select_raw(conn, id))
        }?;
        let Some((raw_id, request_body, response_body)) = loaded else {
            return None;
        };
        // 上限常量放存储层（record::MAX_RAW_BODY_BYTES），采集与读取共用一份，
        // 依赖方向保持 api → request_stats（理由见该常量的说明）
        let truncated = request_body.len() >= MAX_RAW_BODY_BYTES
            || response_body.len() >= MAX_RAW_BODY_BYTES;
        Some(json!({
            "id": raw_id,
            "requestBody": request_body,
            "responseBody": response_body,
            "truncated": truncated,
        }))
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
                let daily = daily::select_daily_map(conn)?;
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
    /// 过滤条件与清理接口**共用同一份** `sql::FilterPlan`（见那里的说明）：
    /// 「页面上筛出来的 N 条」与「清空删掉的那批」必然是同一个集合。
    ///
    /// 响应里的 `running` 是**同一筛选条件下**仍处进行中（status=0）的条数：
    /// 列表页徽标用它显示「还有几条没跑完」，一条 COUNT 得出，不拉明细。
    ///
    /// 降级：库不可用时给 `{entries: [], total: 0, matched: 0, running: 0}` ——
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
                let running = sql::count_running(conn, &plan)?;
                let entries = sql::select_page_desc(conn, &plan, filter.offset, limit)?;
                Ok((total, matched, running, entries))
            })
        };
        let (total, matched, running, entries) = loaded.unwrap_or((0, 0, 0, Vec::new()));
        // 每行经 `entry_json` 补一个派生字段 `providerLabel`（id → label 的换算；
        // 换算处与汇总的 providers 数组同一个函数，两处名字必然一致）。
        // 汇总是**反序列化回 Value**，不是另一套结构：契约字段仍由 record.rs
        // 的 serde 注解决定，这里只加不删
        let rows: Vec<Value> = entries.iter().map(entry_json).collect();
        json!({
            "entries": rows,
            "total": total,
            "matched": matched,
            "running": running,
        })
    }

    /// 筛选下拉的候选清单：明细里出现过的模型名与 provider（各按出现次数降序）。
    ///
    /// 形状 `{models: ["…"], providers: [{id, label}]}` —— provider 带 label 是
    /// 为了让前端不必维护第二份 id → 展示名映射（与明细行的 `providerLabel`
    /// 同一个换算函数，见 `report::provider_label`）。
    ///
    /// ── 为什么是独立端点而不是塞进列表响应 ──────────────────────
    /// 候选清单与「这一页显示什么」无关：它只在进页面时拉一次，之后翻页 / 换筛选
    /// 都不该重算（一次全表 GROUP BY 挂到每个分页请求上，是白烧）。反过来，
    /// 列表响应里塞一份清单会让每页多背几百个字符串，而消费方一个都用不上。
    ///
    /// 降级：库不可用时给空清单（`{models: [], providers: []}`）——
    /// 前端据此只保留「全部」选项，筛选器退化成不可用但界面不报错；
    /// 与 `query_requests` 的空表降级同一取向（少一个功能好过整页失败）。
    pub fn filter_options(&self) -> Value {
        let loaded = {
            let guard = self.guard();
            self.with_conn(&guard, |conn| {
                sql::select_filter_options(conn, MAX_FILTER_OPTIONS)
            })
        };
        let (models, providers) = loaded.unwrap_or((Vec::new(), Vec::new()));
        let providers: Vec<Value> = providers
            .into_iter()
            .map(|id| json!({ "id": id, "label": provider_label(&id) }))
            .collect();
        json!({ "models": models, "providers": providers })
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
            daily::delete_daily_before(&tx, &bounds.daily_key)?;
            daily::trim_daily_capacity(&tx, MAX_DAILY_DAYS)?;
            tx.commit()
        });
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
                let days = daily::count_daily(conn)?;
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

    /// 模型维度的一次性口径订正：把「按请求名（映射别名）累计」的历史聚合行，
    /// 用现存明细按新口径（上游真名，见 `model_stat_key`）重算。
    ///
    /// ── 为什么需要它 ──────────────────────────────────────────
    /// 改口径前写出的聚合行，模型维度记的是请求侧解析名 —— 客户端点名映射
    /// 别名时，用量被记在别名名下（同一个上游模型在环形图里拆成好几行）。
    /// 明细里一直有 `upstreamModel`（schema v2 起就有列），所以明细覆盖到的
    /// 天数可以真正重算，不是猜。
    ///
    /// ── 为什么是 kv 标记 + 启动期一次 ──────────────────────────
    /// 与 `backfill::rebuild_legacy_days`（缺账号维度回填）不同：那次的候选
    /// 「聚合行缺 accountStats」可以从聚合行本身检测（缺列即候选），幂等免费；
    /// 这次的错行（模型键是别名）与对行（模型键是真名）在聚合行里**长得一样**，
    /// 没有可检测的特征，幂等必须靠显式标记（`kv` 表，与 `legacy::MARKER_*`
    /// 同一机制）。
    ///
    /// ── 拿不准就不动 ──────────────────────────────────────────
    /// 沿用 `backfill` 的安全边界：明细会被保留期（天数 + 容量闸）裁掉，
    /// 当天「明细条数 ≥ 聚合行 requests」才说明明细足以代表当天，才重算；
    /// 不够的天原样保留 —— 用残缺明细重算会让当天数字凭空缩水，比留着别名
    /// 分组更糟。超出明细保留期的历史天因此**不修**：那些天没有重算依据。
    ///
    /// ── 调用时机（与 `account_bootstrap` 同一模式）──────────────
    /// 启动时一次（`server/mod.rs`）；待迁移旧文件还没导入时（`upgrade_pending`）
    /// 跳过 —— 跑了会把幂等标记打早，导入完成的旧聚合行就永远轮不到订正；
    /// 升级完成后由 `api::upgrade_api` 补调一次。两处都调不会做两遍活
    /// （标记 + 条数判据，见上）。
    pub fn remap_model_dimension_once(&self) {
        // 日志不能在持 `inner` 锁期间打（会写同一个库，`Mutex` 不可重入，
        // 见 `guard` 的说明），先把结果拿出来，锁释放后再记
        let outcome = {
            let guard = self.guard();
            self.with_conn_mut(&guard, remap_model_days)
        };
        match outcome {
            Some(changed) if !changed.is_empty() => logging::log(
                "[Stats]",
                &format!(
                    "🔁 已按上游真名重算 {} 天的模型用量（此前按请求名 / 映射别名累计）：{}",
                    changed.len(),
                    changed.join("、")
                ),
            ),
            Some(_) => {}
            // Err（已由 with_conn_mut 打控制台日志）与库不可用都在这一支：
            // 标记没写，下次启动自然再试，订正不会因此丢
            None => {}
        }
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
