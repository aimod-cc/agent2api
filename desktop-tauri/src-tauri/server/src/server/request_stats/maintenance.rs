//! 请求统计的**维护动作**：两模式清理、清理预览与数据库压缩。
//!
//! ── 为什么单独一层 ──────────────────────────────────────────
//! 这批方法（`clear_where` / `clear_raw_where` / `clear` / `clear_preview` /
//! `db_bytes` / `compact`）与「记账 / 查询 / 报表」是两类生命周期完全不同的
//! 操作：前者由管理 API 显式触发、低频且重（整表删除、重建库文件），后者
//! 高频且必须轻（每个请求的收尾路径都在调）。拆开后 `request_stats.rs` 专注
//! 于记账与读侧，本文件集中「删数据 / 量磁盘 / 压缩」这组互相支撑的语义
//! —— 预览（`clear_preview`）与执行（两个 clear）、压缩（`compact`）与
//! 压缩状态（`VacuumStatus`）必须读同一段代码才能确认口径一致，放一起才
//! 拆不散。方法仍是 `impl RequestStats` 的一部分（Rust 允许 impl 块分文件），
//! 对外 API 形状与拆分前逐字相同。
//!
//! ── request_raw 的同步清理约定 ──────────────────────────────
//! 原始正文（`request_raw` 表）与明细同 id 关联：**每一次删明细都必须同步
//! 删正文**（先删正文再删明细，顺序理由见 `sql::delete_raw_matching`），
//! 否则明细没了正文还在 —— 既占容量闸名额，又永远读不出来。本文件的两个
//! clear 与全量 clear 都遵守这条约定；记账路径的容量 / 时间淘汰由
//! `sql::trim_requests_capacity` / `sql::delete_expired_requests` 内部处理。
//!
//! ── 并发模型（与 `request_stats.rs` 同一套）──────────────────
//! 所有方法取 `&self`，串行化靠 `RequestStats::guard` + `Db` 的连接锁：
//! 每个方法都在**一次** `with_conn` / `with_conn_mut` 里跑完（多语句用事务）。
//! **硬约束**：持锁期间绝不能调 `logging::log`（日志写同一个库，`Mutex`
//! 不可重入会死锁）—— `compact` 的完成日志因此在线程拿到结果、**释放锁之后**
//! 才打。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::server::logging;

use super::clock::now_ms;
use super::record::RequestQuery;
use super::{daily, sql, RequestStats};

impl RequestStats {
    /// 按筛选条件清空明细与对应原始正文（`DELETE mode=all` 带筛选时的执行体），
    /// 返回删除的明细条数。**不动按天聚合**。
    ///
    /// ── 为什么不再重算按天聚合 ──────────────────────────────────
    /// 改造前这里会把受影响日期的聚合行从剩余明细整行重算（让「报表请求数 =
    /// 剩余明细数」）。本次改为**不动日报**：日报是**全量聚合**（`fold_into_daily`
    /// 累计的是当天全部明细，含已被保留期裁掉的部分），带筛选的部分删除后
    /// 它无法被部分重算 —— 手工从聚合行里减掉命中量要重写一遍「分组 / 建组 /
    /// 空组」的口径，而「按剩余明细整行重算」会抹掉保留期外的历史贡献。
    /// 取舍：保留日报的历史读数（报表请求数可能大于当前明细能数出的量），
    /// 由 API 响应的 `note` 字段向用户说明（见 `stats_api::clear_stats_requests`）。
    ///
    /// 与全量 [`Self::clear`] 不同，这里**不**清调试模式的原始报文：命中的是
    /// 部分明细，调试报文可能还对应着留下的行（取舍见 `debug_traffic` 的说明）。
    ///
    /// 降级：库不可用时返回 0 ——「一条都没删」是唯一诚实的答案，
    /// 谎报删除条数会让前端提示「已清空 N 条」而库里什么都没变。
    pub fn clear_where(&self, filter: &RequestQuery) -> usize {
        let plan = sql::FilterPlan::of(filter);
        self.with_conn_mut(&self.guard(), |conn| {
            let tx = conn.transaction()?;
            // 先删命中的正文（明细删完就再也找不到要陪葬的正文 id 了）
            sql::delete_raw_matching(&tx, &plan)?;
            sql::delete_matching(&tx, &plan)
        })
        .unwrap_or(0)
    }

    /// 只清原始正文（`DELETE mode=raw` 的执行体）：按筛选条件命中的明细 id
    /// 删 `request_raw` 行，**不动**明细与日报 —— 报表的统计字段全部保留，
    /// 只抹掉「预览对话」用的正文。返回删除的正文行数。
    ///
    /// 明细行删除后正文会成为读不出来的孤儿（占容量闸名额、预览 404），
    /// 所以删正文必须挂在「删明细」的同一批 id 上；反过来只删正文、留明细
    /// 则是安全的（明细行还在，只是详情弹窗没有正文）。
    pub fn clear_raw_where(&self, filter: &RequestQuery) -> usize {
        let plan = sql::FilterPlan::of(filter);
        self.with_conn_mut(&self.guard(), |conn| sql::delete_raw_matching(conn, &plan))
            .unwrap_or(0)
    }

    /// 清空明细、按天聚合与原始正文（`DELETE mode=all` 不带筛选的执行体），
    /// 返回删除的明细条数。
    ///
    /// 为什么不是「用 `prune` 裁到 0 天」：保留期的下限是 1 天（`normalized()`
    /// 把 0 夹成 1），因此 `prune` 永远留得住今天的数据，表达不了「清空」。
    /// 这里直接删三张表的全部行，语义明确：清空后立即读到的就是 0 条，
    /// 重开程序也不会把已删的数据载回来。**不改保留期设置**：清数据与改配置是两件事。
    ///
    /// 降级：库不可用时返回 0 ——「一条都没删」是唯一诚实的答案，
    /// 谎报删除条数会让前端提示「已清空 N 条」而库里什么都没变。
    pub fn clear(&self) -> usize {
        self.with_conn_mut(&self.guard(), |conn| {
            let tx = conn.transaction()?;
            // 正文先于明细清（顺序理由见 sql::delete_raw_matching；
            // 全清时整表删，不需要按 id 找）
            sql::delete_all_raw(&tx)?;
            let removed = sql::delete_all_requests(&tx)?;
            daily::delete_all_daily(&tx)?;
            tx.commit()?;
            Ok(removed)
        })
        .unwrap_or(0)
    }

    /// 清理预览（弹窗的数据源）：当前筛选下**将删除**的明细数（`all`）、
    /// 其中仍带原始正文的条数（`raw`）、库文件磁盘占用（`dbBytes`），
    /// 以及压缩任务的运行状态（`vacuumRunning` / `lastVacuum`）。
    ///
    /// 三个读数与清理执行体共用同一份 `FilterPlan` —— 预览说删 N 条，
    /// 点确认删掉的就是 N 条。`dbBytes` 不按筛选计算（库文件没有「这一部分
    /// 属于这批筛选」的边界，VACUUM 后回收的空间也是整库的）。
    pub fn clear_preview(&self, filter: &RequestQuery) -> Value {
        let plan = sql::FilterPlan::of(filter);
        let loaded = self.with_conn(&self.guard(), |conn| {
            let all = sql::count_matching(conn, &plan)?;
            let raw = sql::count_raw_matching(conn, &plan)?;
            Ok((all, raw))
        });
        let (all, raw) = loaded.unwrap_or((0, 0));
        json!({
            "all": all,
            "raw": raw,
            "dbBytes": self.db_bytes(),
            "vacuumRunning": self.vacuum.is_running(),
            "lastVacuum": self.vacuum.last_finished(),
        })
    }

    /// 库文件的磁盘占用（主库 + WAL，字节）。
    ///
    /// WAL 是已提交数据的一部分（checkpoint 前它才是「新」副本），只报主库
    /// 会把占用算小 —— 报「磁盘还剩多少可回收」的场景两个文件都要算。
    /// 读不到（文件不存在 / 无权限）按 0：设置页显示 0 字节比整个接口 500 好。
    pub fn db_bytes(&self) -> u64 {
        let file = self.file();
        file_size(&file) + file_size(&PathBuf::from(format!("{}-wal", file.to_string_lossy())))
    }

    /// 压缩数据库：先 `PRAGMA wal_checkpoint(TRUNCATE)` 把 WAL 并回主库并截零，
    /// 再 `VACUUM` 重建整库文件回收空闲页。**后台线程**执行，返回启动结果。
    ///
    /// ── 为什么必须拿那把连接锁 ──────────────────────────────────
    /// 本项目存储层是「同步 rusqlite 单连接 + `Mutex`」：VACUUM 不允许在事务
    /// 里跑，且必须与其它读写互斥 —— 唯一正确的入口就是 `Db::with`（拿锁）。
    /// 持锁期间其余数据库操作（记账、日志写入、管理 API）会排队：桌面单文件
    /// 库通常几十 MB，VACUUM 秒级完成，这个停顿可以接受 —— 这也正是压缩做成
    /// 「后台线程 + 用户显式触发」而不是记账路径顺带做的理由。
    ///
    /// ── 为什么是 std::thread 而不是 tokio::spawn ────────────────
    /// `db.with` 是**阻塞**调用（拿不住锁就一直等），放进异步任务会占死一个
    /// tokio worker 线程；专用系统线程把阻塞留在运行时之外，跑完即回收。
    ///
    /// ── 状态与并发 ──────────────────────────────────────────────
    /// 运行标记在**启动前**原子置位（`VacuumStatus::try_start`），重复触发由
    /// 调用方给 409；完成时刻无论成败都记录（失败也是一次终局）。
    pub fn compact(&self) -> CompactStart {
        if !self.vacuum.try_start() {
            return CompactStart::AlreadyRunning;
        }
        let Some(db) = self.db.clone() else {
            // 库不可用：复位标记（否则 vacuumRunning 永远 true），如实上报
            self.vacuum.mark_finished();
            return CompactStart::Unavailable;
        };
        let vacuum = Arc::clone(&self.vacuum);
        std::thread::spawn(move || {
            let outcome = db.with(|conn| -> rusqlite::Result<()> {
                conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
                conn.execute_batch("VACUUM;")?;
                Ok(())
            });
            let ok = matches!(outcome, Some(Ok(())));
            // 先复位状态再打日志：日志要写同一个库（拿锁），若先打日志，
            // 状态会多卡一会儿；复位后的并发压缩请求与日志写入互不阻塞
            vacuum.mark_finished();
            if ok {
                logging::log("[Stats]", "数据库压缩完成（checkpoint + VACUUM）");
            } else {
                logging::log("[Stats]", "⚠️ 数据库压缩未完成：数据库不可用或压缩失败");
            }
        });
        CompactStart::Started
    }
}

/// [`RequestStats::compact`] 的启动结果（三种去向各有各的响应）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompactStart {
    /// 已在后台线程启动
    Started,
    /// 已有压缩任务在跑（调用方给 409）
    AlreadyRunning,
    /// 库不可用，无从压缩（调用方如实报错而不是谎报「已启动」）
    Unavailable,
}

/// [`RequestStats::compact`] 的运行状态。
///
/// 为什么挂在 `RequestStats` 上而不是模块级 `static`：`RequestStats` 本来就是
/// 进程级单例（`ServerState` 里那一份 `Arc`），状态挂在实例上让「有多个实例
/// 的测试」各自独立；OmniProxy 用模块级 let 变量是因为它的 router 是文件级
/// 单例，两边语义等价，这里选与本项目结构一致的挂法。
pub(super) struct VacuumStatus {
    /// 是否有压缩任务在跑（`swap` 保证「判定 + 置位」原子，防并发双开）
    running: AtomicBool,
    /// 最近一次压缩**完成**的时刻（毫秒 Unix；None = 本次运行还没完成过）。
    /// 无论成败都记录 —— 「完成」包括失败（失败也是一次终局，前端据此停止轮询）。
    last_finished_at: Mutex<Option<i64>>,
}

impl VacuumStatus {
    pub(super) fn new() -> Self {
        Self {
            running: AtomicBool::new(false),
            last_finished_at: Mutex::new(None),
        }
    }

    /// 尝试占用「正在运行」标记：返回 false 表示已有任务在跑。
    ///
    /// 用 `swap` 而不是「先 load 再 store」：判定与置位在同一条原子操作里，
    /// 两个并发请求只有一个能拿到 `false` 的旧值 —— 不需要额外加锁。
    fn try_start(&self) -> bool {
        !self.running.swap(true, Ordering::AcqRel)
    }

    fn mark_finished(&self) {
        if let Ok(mut slot) = self.last_finished_at.lock() {
            *slot = Some(now_ms());
        }
        self.running.store(false, Ordering::Release);
    }

    fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    fn last_finished(&self) -> Option<i64> {
        self.last_finished_at.lock().ok().and_then(|slot| *slot)
    }
}

/// 文件大小（读不到按 0；`db_bytes` 的辅助）
fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
}
