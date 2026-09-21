//! 本地存储的**唯一真相**：单文件 SQLite 库 `{config_dir}/agent2api.db`。
//!
//! ── 这一层要解决什么问题 ────────────────────────────────────
//! 改造前所有数据都是「文件 + 全量内存快照」：config.json、accounts.json、
//! logs.jsonl、requests.jsonl、request-daily.jsonl、debug-traffic.jsonl、
//! desensitize.json、desktop-settings.json。每个 store 自己实现「载入 / 裁剪 /
//! 整文件重写 / 崩溃后从半截文件恢复」，同一件事（比如保留期裁剪、未知字段
//! 兜底、并发写入的串行化）被抄了七八遍，口径各有细微差别。交给 SQLite 之后：
//! 事务、索引、原子落盘都由库负责，各 store 不再需要持有全量快照。
//!
//! ── 接线进度（各切片逐个接过来）─────────────────────────────
//! 本模块提供连接、schema 与旧文件迁移框架；**具体的 store 由后续切片逐个
//! 接过来**，所以短期内会出现「一部分数据在库里、另一部分还在旧文件里」的
//! 并存状态，那是迁移过程中的正常中间态，不是 bug。
//!   已接入：**账号**（`core::account_store` → `accounts` 表；旧
//!           `accounts.json` 由 `migrate::import_accounts` 一次性搬入）、
//!           **事件日志**（`logs_store` → `logs` 表；旧 `logs.jsonl`
//!           由 `migrate::import_logs` 一次性搬入）、
//!           **请求统计**（`request_stats` → `requests` / `request_daily`
//!           两张表；旧 `requests.jsonl` / `request-daily.jsonl` 由
//!           `migrate::import_requests` / `migrate::import_daily` 一次性搬入）、
//!           **调试报文**（`core::debug_traffic` → `debug_traffic` 表；
//!           旧 `debug-traffic.jsonl` 由 `migrate::import_debug` 一次性搬入）、
//!           **脱敏词表**（`core::desensitize` → `kv` 表的 `desensitize` 键；
//!           旧 `desensitize.json` 由 `migrate::import_desensitize` 一次性搬入）、
//!           **网关配置**（`config` → `kv` 表，**每个顶层键一行**；旧
//!           `config.json` 由 `migrate::import_config` 一次性搬入）、
//!           **桌面设置**（壳侧 `settings` → `kv` 表的 `desktopSettings` 键；
//!           旧 `desktop-settings.json` 由 `migrate::import_settings` 一次性搬入）
//!   未接入：无 —— 八个旧文件全部迁完（本切片是最后一个）。
//!
//! ── `kv` 的两类键（本切片起两类的写入方都齐了）───────────────
//! 配置的顶层键（原名直用）与「其它零散状态」的固定键（`desktopSettings` /
//! `priorityScope` / `logsNextId` / `desensitize` / `configMigrated`）共用这一张
//! 表，而**配置的写侧要做「删除已不存在的键」**：它必须知道哪些键不归自己管，
//! 否则写一次配置就会把别人的状态删掉。这条边界的唯一事实来源是
//! [`schema::RESERVED_KV_KEYS`]，完整约束见 `schema` 模块头的
//! 「两类键名绝不能相撞」一节（新增固定键时**必须**回去核对）。
//!
//! ```text
//! server/db/
//!   mod.rs     连接与句柄（本文件）：Db::open / with / with_mut / file
//!   schema.rs  全部建表 DDL 与版本管理（PRAGMA user_version 逐版本升级）
//!   migrate/   旧文件 → 数据库的一次性迁移（框架 + 每个迁移项一个文件）
//! ```
//!
//! ── 为什么单连接而不是连接池 ────────────────────────────────
//! 本项目是单进程单文件，写入天然被 SQLite 串行化（同一时刻只允许一个写
//! 事务），池子只能并发出「读」——而我们的读集中在少数几条 API 上，量级远
//! 没到需要并发的程度。代价却实打实：每条连接一份页缓存、多写者互相
//! SQLITE_BUSY、以及「哪个连接看到哪个快照」这类只在多连接下才存在的 bug。
//! 单连接 + 一把 Mutex 与现在「内存快照 + 单锁」的形状最接近，迁移期两边
//! 行为好对照。真要并发读，先按 SQLite 官方建议上 WAL + 多连接，而不是
//! 先引入池化库。
//!
//! ── 为什么是 Mutex 而不是 RwLock ────────────────────────────
//! `rusqlite::Connection` 的方法大多取 `&self`（`prepare` / `execute` /
//! `query_row`），看上去 `RwLock<Connection>` 能让多个读并发。但：
//!   1. **写入是串行的**，读锁挡不住写，真正的并发收益只有「纯读同时发生」
//!      这一个场景；
//!   2. 事务需要 `&mut self`（写事务更是独占），一旦有写，RwLock 立刻退化成
//!      「全员排队」，还多出一条「锁升级会死锁」的约束要绕；
//!   3. 读写锁的公平性与写饿死问题在 `std` 的实现里不保证，排查成本高。
//! 结论：`Mutex` 语义简单、与「SQLite 单写者」这个事实一致，不用 `RwLock`。
//! （对比 `request_stats` / `logs_store` 的 `file_path` 用 RwLock：那是**读多写
//! 极少**的纯内存字段（只为保留旧访问器的形状而存在），与这里「每次访问都可能
//! 写盘」的形态不同，不是同一类取舍。）
//!
//! ── 硬约束：持锁期间绝不做网络请求 ──────────────────────────
//! 这条是本项目既有的、跨模块的一致要求（见 `core::account_store` 模块头）：
//! 这里只有一把锁，持锁期间做一次出网请求，等于把**所有**管理 API 与转发
//! 记账全部堵在那一秒上。访问器的闭包形态（`with` / `with_mut`）本身就是为
//! 这条约束服务的：闭包里只放「读/写数据库」的几行，需要出网的事先在闭包内
//! 取出快照、出了闭包再发请求、然后单独回写一次。
//!
//! ── 打开失败为什么不是致命错误 ──────────────────────────────
//! `Db::open` 返回 `Result<Db, String>`，而调用方（`ServerState::bootstrap`）
//! 把失败降级成 `Option<Db> = None`：release 构建是 panic=abort，磁盘满 /
//! 权限不足 / 库文件损坏都不该让整个桌面应用闪退。网关降级启动至少还能给出
//! 可读的错误页面与日志。各 store 的接线点要按「数据库可能不存在」写。

pub mod migrate;
pub mod schema;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusqlite::Connection;

/// 数据库文件名。放配置目录（`~/.agent2api`）里，与 config.json / 日志并排 ——
/// 用户在设置页看到的「配置目录」就是这一个位置，备份与排障只需要它。
/// 路径由 `config::config_dir()` 派生，本模块不自己去查 home 目录。
/// （接入本库的五个 store —— 账号、事件日志、请求统计、调试报文、脱敏词表 ——
/// 改造后都不再有自己的文件名，它们的数据都在本库的各张表里。）
pub const FILE_NAME: &str = "agent2api.db";

/// SQLite 连接句柄。`Clone` 共享**同一个**连接（内部 `Arc`）。
///
/// 与 `AccountStore` 的 `Arc<Inner>` 是同一个模式：`ServerState` 是 `Clone` 的
/// （axum handler 靠克隆拿状态），所以可变状态一律做成「轻量句柄 + 内部锁」，
/// 让 handler 拿到就能用，不必关心加锁。克隆 `Db` 不会打开第二条连接。
///
/// `Send + Sync + 'static` 由 `Arc<Mutex<Connection>>` 提供：
/// `rusqlite::Connection` 是 `Send` 但**不是** `Sync`（它内部有 SQLite 的连接
/// 状态），`Mutex<T>: Sync` 要求 `T: Send`，正好成立 —— 这也正是必须包一层
/// `Mutex` 而不能直接把 `Connection` 塞进 `Arc` 的原因（那会直接编译不过）。
#[derive(Clone)]
pub struct Db {
    /// 连接本体 + 库文件路径。路径与连接同放一份 `Arc` 里，`file()` 因此不必
    /// 再取一次锁 —— 路径从打开后就不再变（不搬家：换目录是「新建库 + 导入」，
    /// 不是改这个字段）。
    inner: Arc<DbInner>,
}

struct DbInner {
    /// 单连接。锁中毒时按「数据库不可用」处理，**不用** `into_inner()` 接管 ——
    /// 与 `core::egress` / `logs_store` 的取向不同，理由见 `with` 的注释。
    conn: Mutex<Connection>,
    /// 库文件路径（打开时确定，之后不变）
    file: PathBuf,
}

impl Db {
    /// 打开（必要时创建）数据库并完成初始化：建父目录 → 连接 → 设 PRAGMA →
    /// 跑 schema 迁移。
    ///
    /// **旧文件迁移不在这里**（`migrate::run_legacy_migrations` 由调用点在
    /// 打开成功后自己跑）：迁移要往表里写数据、还要给用户报「导入了多少条」，
    /// 而本函数的返回值只有一个 `Db` —— 把迁移塞进来就只能吞掉那个结果，
    /// 或者为它加一个专门的返回值（于是「打开数据库」这件事被迫知道迁移）。
    /// 分开之后各司其职，且调用点能按自己的日志通道汇报迁移结果。
    /// （现在那个调用点是 `POST /api/upgrade/run`，不是启动流程 ——
    /// 启动只探测「还有没有旧文件」，见 `migrate::has_pending`。）
    ///
    /// 返回 `String` 错误而不是自定义错误枚举：与 `AccountStoreError` 的字符串
    /// 承载一致 —— 这里唯一的消费者是启动日志，调用方只需要「一句能打给人看的
    /// 话」，不需要按错误分支处理。
    ///
    /// 父目录必须自己建：首次运行时 `{config_dir}` 可能还不存在（全新安装、
    /// 用户刚改过保存位置），而 SQLite 的 `open` 只创建**库文件**，不会
    /// 递归建目录 —— 缺目录时它会直接以「无法打开数据库文件」失败。
    pub fn open(path: &Path) -> Result<Db, String> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| format!("创建数据库目录失败: {error}"))?;
            }
        }
        let conn = Connection::open(path).map_err(|error| format!("打开数据库失败: {error}"))?;
        apply_pragmas(&conn)?;
        schema::migrate(&conn).map_err(|error| format!("建表/升级 schema 失败: {error}"))?;
        Ok(Db {
            inner: Arc::new(DbInner {
                conn: Mutex::new(conn),
                file: path.to_path_buf(),
            }),
        })
    }

    /// 取锁执行闭包（普通读写）。**锁中毒（某次持锁 panic）时返回 `None`**，
    /// 调用方据此保守回落（返回默认值 / 跳过这一步 / 走旧文件路径）。
    ///
    /// ── 为什么不学其它模块的 `poisoned.into_inner()` ──────────
    /// 那些模块（`request_stats` / `core::debug_traffic`）持的是自管理状态
    /// （前者是内存快照 + 落盘，现在只剩一个「调用序列」标记），panic 打断最多
    /// 丢一次未落盘的写，接管继续用是划算的 —— 它们的注释里写的是
    /// 「统计数据的完整性远不如服务不因统计而崩重要」。
    /// 这里不同：被 panic 打断的很可能是**一个未提交的 SQLite 事务**或一次
    /// 走到一半的多语句写。`into_inner()` 之后我们无从知道库停在哪个中间态，
    /// 继续读会把「半截数据」当成真相喂给上层 —— 那比「这一项功能暂时不可用」
    /// 严重得多。所以中毒即视为不可用：`None` 让调用方显式选择降级行为
    /// （比如日志明细这一项先不落库），故障面被限制在单个 store 上。
    /// 注意这与「打开失败降级」是同一取向：**数据库的问题不该让服务崩**，
    /// 但也绝不该悄悄给出错误答案。
    ///
    /// ── 与各 store 那把锁的取向不冲突 ──────────────────────────
    /// `AccountStore::guard` / `LogStore::guard` **仍然** `into_inner()` 接管 ——
    /// 它们保护的是「一次读改写周期」这个调度点，锁内没有需要保持一致的数据
    /// （真正的数据在库里）。两处判据不同正是因为**保护对象不同**：这里持的是
    /// 连接本身（状态可能停在事务中间），那里持的是空标记。
    pub fn with<T>(&self, f: impl FnOnce(&Connection) -> T) -> Option<T> {
        match self.inner.conn.lock() {
            Ok(conn) => Some(f(&conn)),
            Err(_) => None,
        }
    }

    /// 取锁执行闭包，但给的是 `&mut Connection` —— **需要事务的批量写用这个**。
    ///
    /// `Connection::transaction()` 的签名是 `&mut self`（写事务是独占的，
    /// 期间不允许在同一连接上再发语句，类型系统要求独占借用才安全）。
    /// `with` 只能给 `&Connection`，拿不到事务。两条路可以绕：
    ///   - `unchecked_transaction()`（取 `&self`）：能编过，但它把「事务期间
    ///     不得借用连接」这条规则变成运行期检查，用错就是 `SQLITE_MISUSE`；
    ///   - 提供 `with_mut`：把「我要写一批」这件事写进调用点的类型里。
    /// 取后者：调用方一眼能看出哪些操作是批量写，而 `with` / `with_mut` 的
    /// 选择就是一句「要不要事务」，不必记住哪个 API 是 unsafe 变体。
    ///
    /// 与 `with` 共用同一把锁，因此两者**不会**并发：一个 `with_mut` 里跑事务
    /// 期间，其它 `with` 调用会照常在锁上排队。这正是单连接想要的效果。
    ///
    /// 调用点：`AccountStore::with_conn_mut`（账号层的批量写与整份状态差异写 ——
    /// 批量改/删、`migrate_startup`、三家旧数据导入、导入合并）。
    pub fn with_mut<T>(&self, f: impl FnOnce(&mut Connection) -> T) -> Option<T> {
        match self.inner.conn.lock() {
            Ok(mut conn) => Some(f(&mut conn)),
            Err(_) => None,
        }
    }

    /// 库文件路径（排障直读 / 启动日志展示）。
    ///
    /// 返回 `&Path` 而不是 `PathBuf`：路径在打开时定死（**不搬家** ——
    /// 换保存位置是「新建库 + 导入」，不是改这个字段），调用方只是拿去看看或
    /// 拼串，不需要所有权。
    /// 本方法**不取锁**：路径不在锁保护范围内，读它不该和正在跑的事务抢锁
    /// （启动日志那一行就是典型场景 —— 那时可能正有别的访问在跑）。
    pub fn file(&self) -> &Path {
        &self.inner.file
    }
}

/// 打开连接后必须设置的 PRAGMA。
///
/// 用 `execute_batch` 一次跑完而不是逐条 `pragma_update`：`journal_mode` 这个
/// PRAGMA **会返回一行结果**（新/旧的日志模式），走 `pragma_update` 会碰到
/// 「语句返回了行却没人取」的返回值处理问题；`execute_batch` 只要求语句跑完，
/// 不解析结果行，这几条一条串写最省事。
///
/// 三条各自的作用：
///   - `journal_mode=WAL`：写不再阻塞读（回滚日志模式下写期间读会被挡住）。
///     注意 WAL 是**写进库文件头的持久设置**，设一次以后一直有效；这里每次
///     打开都设一遍是为了幂等，且能从回滚日志模式库里自动切过来。
///     代价是多出 `-wal` / `-shm` 两个附属文件，备份时要么用 SQLite 的
///     在线备份，要么三件一起拷（后续「导出/清理」任务需要注意）。
///   - `synchronous=NORMAL`：WAL 下的推荐档位 —— 事务提交不再每次 fsync
///     WAL 文件，只在检查点落盘时 fsync。掉电**不会损坏库**（WAL 本身是
///     崩溃安全的），最多丢最近若干已提交事务；换来的是写入快一个量级。
///     本项目存的是日志/统计/账号缓存，丢最后几条可以接受，因此不用 FULL。
///   - `foreign_keys=ON`：SQLite 默认**关闭**外键约束（为兼容旧版而留的坑），
///     必须逐连接开启。现在表之间还没有外键，但先开着 —— 后续切片加关联表时
///     不会因为「忘了开这个 PRAGMA」而静默失去约束。
fn apply_pragmas(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         PRAGMA foreign_keys=ON;",
    )
    .map_err(|error| format!("设置数据库 PRAGMA 失败: {error}"))
}
