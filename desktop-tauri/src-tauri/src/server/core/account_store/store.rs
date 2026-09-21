//! 账号存储句柄：CRUD、当前账号派生、凭证/会话查询、限额标记、启动迁移。
//!
//! 对照 workbuddy-account-store.mjs 的 `createAccountStore` 返回值逐条移植。
//! 三条硬约束写在 `super::mod` 的头部：未知字段全量保留、优先级**全局唯一**
//! （所有提供商共用一条队列 —— 不是在 provider 内唯一，那是加多 provider 之前
//! 的旧口径）、provider 字段缺失时按 workbuddy 兜底。
//! 「全局唯一」的判据落在 `sql::priority_holder` 的 SQL 上（它**不带** provider
//! 条件），写入侧冲突一律 409，详见 `priority.rs` 模块头。
//!
//! ── 持久化：`accounts.json` → SQLite `accounts` 表（本切片的改造）────
//! 数据从 `{config_dir}/accounts.json` 换到 `{config_dir}/agent2api.db` 的
//! `accounts` 表（表结构见 `server/db/schema.rs`）。**对外契约一字未改**：
//! 本文件的 `pub fn` 签名、返回值、错误码全部与改造前相同，调用方
//! （`api::accounts`、`auth`、`routing`、各 provider 适配器）零改动。
//!
//! 行级 SQL 与投影列的口径集中在 `sql.rs`（账号存储里唯一出现 SQL 的文件）。
//! 这里只负责：句柄与锁、`AccountState` ↔ 表的两个端点、以及派生逻辑。
//!
//! ── 锁还在，但它保护的东西变了 ──────────────────────────────
//! `guard()` 仍然是那把 `Mutex<()>`，`with_lock` / `load_locked` / `save_locked`
//! 的签名也**必须**保持不变（`account_transfer` 直接用它们，见那边的模块头）。
//! 变化在于它保护的**不是文件**了：以前它串行化「整份文件读-改-写」，
//! 现在它串行化「数据库上的一次读改写周期」。语义等价 —— 两边都是「同一时刻
//! 只有一个账号域的写者」，而数据库自己的事务保证单次写入的原子性。
//! 为什么还要留着这把锁而不是全靠 SQLite：账号层的**读-改-写**（读出一条 →
//! 在 Rust 里改 → 写回）跨了多条语句，中间必须有东西挡住另一个写者；
//! SQLite 的事务做不到这件事，因为「改」发生在 Rust 内存里、不在事务范围内。
//!
//! 硬约束不变：持锁期间绝不做网络请求（见 super::mod 头部说明）。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::{json, Value};

use crate::server::core::account_store::sql;
use crate::server::core::account_store::state::{
    AccountState, CredentialsById, CurrentEntry, SessionById, StoredAccount,
};
use crate::server::core::endpoints::resolve_edition;
use crate::server::core::providers::DEFAULT_PROVIDER_ID;
use crate::server::core::proxies::{resolve_account_proxy, ProxyResolution};
use crate::server::db::Db;

/// 账号存储错误（对应 Node 版 AccountStoreError，带状态码 → 路由层直接用）。
#[derive(Clone, Debug)]
pub struct AccountStoreError {
    pub message: String,
    pub status_code: i32,
}

impl AccountStoreError {
    pub fn new(message: impl Into<String>, status_code: i32) -> Self {
        Self { message: message.into(), status_code }
    }

    pub(crate) fn bad_request(message: impl Into<String>) -> Self {
        Self::new(message, 400)
    }

    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        Self::new(message, 404)
    }

    /// 数据库不可用（连接打开失败，或打开后被判定中毒）。
    ///
    /// 取 **500 + 一句可读原因**：账号是网关的核心数据，没有库就没有账号可用，
    /// 这不是「某个字段没填对」那种 400，也不是「这条记录不存在」那种 404 ——
    /// 是服务端自身没有可用的存储。状态码与改造前的「账号文件保存失败: …」
    /// 一致（那条同样是 500），因此前端与路由层的分支一行都不用改。
    pub(crate) fn storage_unavailable(reason: &str) -> Self {
        Self::new(
            format!("账号数据库不可用（{reason}）：请检查磁盘空间与配置目录权限后重启应用"),
            500,
        )
    }
}
impl std::fmt::Display for AccountStoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.message)
    }
}

/// 共享的账号存储。
///
/// 用 `Arc<Inner>` 做轻量句柄：`ServerState` 可径直 clone，各 handler 拿到的是
/// 同一个 store。锁是**一把粗粒度 Mutex** —— Node 版是单线程事件循环，
/// 这里用一把锁换取与它等价的串行语义，锁粒度不做精细拆分。
/// 硬约束：持锁期间绝不做网络请求（见 super::mod 头部说明）。
#[derive(Clone)]
pub struct AccountStore {
    inner: Arc<Inner>,
}

struct Inner {
    /// SQLite 句柄。`None` = 数据库打开失败（启动时已记日志）。
    ///
    /// ── 为什么不做「文件回退」─────────────────────────────────
    /// 一个自然的想法是：库不可用时退回读写 `accounts.json`，让账号功能照常。
    /// 不做，理由有三条：
    ///   1. **回退路径永远没人测**。数据库打不开是罕见故障（磁盘满、权限、
    ///      库损坏），而回退分支只在那种时刻才被执行 —— 它是一段只在上线机器
    ///      上第一次运行的代码，本身就是新的故障源。真到那一刻，用户要的是
    ///      「一句能读懂的错误」，不是「看起来还能用、但数据写进了另一份文件、
    ///      下次数据库恢复后两边对不上」。
    ///   2. **两套实现会让口径分叉**。项目的三条不变量（未知字段全量保留、
    ///      优先级唯一、provider 兜底）在两套存储上各有实现，改一处忘一处是
    ///      必然的；而账号数据丢了是**不可逆**的用户损失。
    ///   3. **失败要吵，不要静默**。`None` 让每个账号操作显式报 500，
    ///      用户在界面上立刻看到「账号数据库不可用」，而不是过几天才发现
    ///      「我改的账号怎么没保存」。
    /// 与之相对，`Db::open` 失败**不阻断启动**（网关仍能跑转发、日志、模型）——
    /// 这是 `ServerState::bootstrap` 已有的决定，本层只是如实承接它的后果。
    db: Option<Db>,
    /// 库文件的**约定路径**（`{config_dir}/agent2api.db`）。
    ///
    /// 为什么单独存一份：`file()` / `file_string()` 的契约是「给出账号数据所在的
    /// 文件位置」，而它有两个消费方都要求这个值非空 —— `/api/session` 的
    /// `authFile`（显示在面板的「凭证文件」一栏）与 `ServerState::bootstrap`
    /// 的启动日志。数据库打开失败时若返回空串或空路径，用户看到的是「位置字段
    /// 是空的」而不是「库没打开」，排查方向会被带偏。
    /// 库正常时这个值只是备份信息（`Db::file()` 才是权威），因此不该让调用方
    /// 自己拼路径去猜 —— 拼法只在本层有一份。
    file_path: PathBuf,
    /// 串行化「读-改-写」整个周期；只保护账号域的数据库访问，不保护网络请求
    lock: Mutex<()>,
}

impl AccountStore {
    /// 用数据库句柄构造（**本切片起的主流构造方式**）。
    ///
    /// `db` 为 `None` 表示数据库打不开：不阻断构造（`ServerState::bootstrap`
    /// 需要能继续把状态装起来），但所有账号操作都会以 500 拒绝，理由见
    /// `Inner::db` 的注释。
    ///
    /// `file_path` 由 `db` 自己给出（`Db::file()`），于是「库在哪」这件事只有
    /// `Db` 一个事实来源；`db` 为 `None` 时退回约定路径（见 `Inner::file_path`）。
    pub fn with_db(db: Option<Db>) -> Self {
        let file_path = match db.as_ref() {
            Some(db) => db.file().to_path_buf(),
            None => crate::server::config::config_dir().join(crate::server::db::FILE_NAME),
        };
        Self {
            inner: Arc::new(Inner { db, file_path, lock: Mutex::new(()) }),
        }
    }

    // ── 改造中**删掉**的构造函数：`with_config_dir()` ──────────────
    // 它原本是「记住配置目录，读写时现拼 accounts.json 路径」。账号数据搬到
    // 库里之后它没有存在价值，且有**实际危害**：它会自己 `Db::open` 一次，
    // 于是同一个库上出现**第二条连接 + 第二把账号锁** —— 而账号层的读-改-写
    // 跨多条语句，两把锁互相不可见，两条 store 句柄会交错覆盖对方的修改。
    // 项目里已经有一条针对这个坑的注释（`providers::adapter` 的
    // 「自己再造一个就成了绕过主句柄的第二把锁」），留着这个入口等于把那个坑
    // 摆在手边。唯一调用点（`ServerState::bootstrap`）已经改为把自己打开的
    // `Db` 传进来，所以这里直接删掉，而不是留一个无人调用的构造器配
    // `#[allow(dead_code)]` 说明。

    /// 账号数据所在的**库文件**路径（`/api/session` 的 authFile 字段用它）。
    ///
    /// 语义从「账号文件」变成「装着账号数据的库文件」—— 这个字段显示在面板上
    /// 的「凭证文件」一栏，用户据此知道数据落在哪、该备份什么。库没打开时给的
    /// 是**约定路径**（而非空路径），理由见 `Inner::file_path`。
    pub fn file(&self) -> &Path {
        &self.inner.file_path
    }

    pub fn file_string(&self) -> String {
        self.inner.file_path.to_string_lossy().to_string()
    }

    /// 账号数据是否已落盘（Node 版账号存储导出的
    /// `existsSync: () => existsSync(filePath)` 的对等物）。
    ///
    /// 语义随存储一起变化：从「`accounts.json` 这个文件在不在」变成
    /// 「装着账号的库文件在不在」。当前管理 API 未调用，保留作为该导出的对等物，
    /// 便于后续「账号数据是否落盘」判定。
    #[allow(dead_code)]
    pub fn exists(&self) -> bool {
        self.file().exists()
    }

    /// 取锁；锁中毒（某次持锁 panic）不致命：直接接管内部数据继续用，
    /// 总好过让所有管理 API 永久 500。
    ///
    /// ── 为什么这里接管、而 `Db::with` 那边返回 None ─────────────
    /// 两者的取向看似矛盾，其实判据不同：这把锁保护的是**锁本身要串行化的那个
    /// 周期**（不含数据），它中毒说明某次账号操作 panic 了，锁内的数据（这里
    /// 根本没有内存数据）没有中间态可言 —— 接管后照常去数据库读写即可，
    /// 而数据库自己的事务保证每次写入是原子的。
    /// `Db::with` 那边不同：它持的是**连接**，panic 可能打断一个未提交的事务，
    /// 接管后会读到半截数据（详见那边的注释）。所以那边保守回落，这边照常接管。
    ///
    /// 数据库不可用的判定**不在这里**（这里没有返回值可以表达它）—— 每个操作
    /// 都经 `with_conn` / `with_conn_mut` 取连接，那两个函数负责返回错误。
    pub(crate) fn guard(&self) -> MutexGuard<'_, ()> {
        match self.inner.lock.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// 在已持锁的前提下执行一段数据库操作；库不可用或 SQL 出错时返回 Err。
    ///
    /// 这是本文件里访问数据库的**唯一入口**：`_guard` 参数不是装饰 —— 它把
    /// 「必须持锁」这条纪律写进签名（Rust 不允许凭空造一个 `MutexGuard`），
    /// 于是「忘了加锁」在编译期就不成立。参数名带下划线只表示函数体不读它。
    ///
    /// SQL 错误一律转成 500：与改造前「账号文件保存失败: {io error}」同级
    /// （那次也是 500）—— 数据库写入失败就是服务端存储出了问题，不是用户输入
    /// 的问题。错误文案里带上底层原因，用户与排障都能看到是哪一步坏了。
    pub(crate) fn with_conn<T>(
        &self,
        _guard: &MutexGuard<'_, ()>,
        action: impl FnOnce(&rusqlite::Connection) -> rusqlite::Result<T>,
    ) -> Result<T, AccountStoreError> {
        let Some(db) = self.inner.db.as_ref() else {
            return Err(AccountStoreError::storage_unavailable("数据库未打开"));
        };
        db.with(|conn| action(conn))
            .ok_or_else(|| AccountStoreError::storage_unavailable("连接不可用（已标记中毒）"))?
            .map_err(|error| AccountStoreError::new(format!("账号数据库操作失败: {error}"), 500))
    }

    /// 与 [`with_conn`] 同理，但给 `&mut Connection` —— **需要事务的批量写用这个**。
    ///
    /// `save_state`（整份状态的差异写入）与 `batch_update` / `batch_remove` /
    /// `migrate_startup` 走它：它们的写入要么全成、要么全不成。
    pub(crate) fn with_conn_mut<T>(
        &self,
        _guard: &MutexGuard<'_, ()>,
        action: impl FnOnce(&mut rusqlite::Connection) -> rusqlite::Result<T>,
    ) -> Result<T, AccountStoreError> {
        let Some(db) = self.inner.db.as_ref() else {
            return Err(AccountStoreError::storage_unavailable("数据库未打开"));
        };
        db.with_mut(|conn| action(conn))
            .ok_or_else(|| AccountStoreError::storage_unavailable("连接不可用（已标记中毒）"))?
            .map_err(|error| AccountStoreError::new(format!("账号数据库操作失败: {error}"), 500))
    }

    /// 读全部账号（**语义要求整份数据**的路径用它，见 `sql::load_all`）。
    ///
    /// 库不可用时返回**空列表**而不是报错：这个函数的签名（`pub`，且被
    /// `load_locked` 直接透出）没有表达失败的位置，而它的消费方多是「读快照
    /// 做展示/派生」—— 拿不到账号就是「没有可用账号」，各调用方本来就要处理
    /// 这个分支（未登录、无可用账号）。反过来，**写入**路径一律返回
    /// `Result`，数据库不可用时明确报 500，绝不让写操作静默成功。
    pub(crate) fn load(&self, _guard: &MutexGuard<'_, ()>) -> AccountState {
        let Some(db) = self.inner.db.as_ref() else {
            return AccountState::default();
        };
        let accounts = db
            .with(|conn| sql::load_all(conn))
            .and_then(Result::ok)
            .unwrap_or_default();
        let priority_scope = db
            .with(|conn| sql::load_priority_scope(conn))
            .and_then(Result::ok)
            .flatten();
        AccountState { accounts, priority_scope }
    }

    /// 写回全部账号（**按差异写**，见 `sql::save_state`）。
    ///
    /// 这个入口只服务「本来就是整份集合的操作」，眼下三处调用：
    ///   - `account_transfer::import_accounts`（导入合并，经 `save_locked`）；
    ///   - `promote_to_front`（置顶要整队重新编号，见 `store_crud.rs`）；
    ///   - `migrate_startup`（启动迁移的整队与拆池改名，见 `store_admin.rs`）。
    /// **不属于它的**：单条增删改（各自只碰一行，走 `sql::put` /
    /// `sql::update_in_place` / `sql::delete`）；批量操作与三家旧数据导入
    /// （都只需要「逐条写自己那几行」，各自在 `with_conn_mut` 里开事务、
    /// 直接调 `sql::*`，不经过这里）。
    /// 换句话说：走到这个函数的都是「手上的 `AccountState` 就是完整目标状态」
    /// 的场景，`save_state` 的按 id 差异比对才有意义。
    ///
    /// ── 错误文案为什么不再是「账号文件保存失败」──────────────
    /// 那个措辞（以及它带来的 500）是改造前唯一的一个 500 来源，现在账号数据
    /// 不再有「文件」这个概念 —— 留着它会让用户按「文件」的方向去排查
    /// （找 accounts.json、查文件锁、看磁盘权限），而真实原因在数据库那边。
    /// 状态码仍是 500（前端与路由层不按文案分支），文案改由 `with_conn_mut`
    /// 统一给出「账号数据库操作失败: {底层原因}」，底层原因里带着 SQLite 的
    /// 错误码与原话 —— 排障需要的那条信息比旧文案更具体。
    pub(crate) fn save(
        &self,
        state: &AccountState,
        guard: &MutexGuard<'_, ()>,
    ) -> Result<(), AccountStoreError> {
        self.with_conn_mut(guard, |conn| sql::save_state(conn, state))
    }

    /// 供导入导出子模块复用：在已持锁的前提下读全部账号
    pub fn load_locked(&self, guard: &MutexGuard<'_, ()>) -> AccountState {
        self.load(guard)
    }

    /// 供导入导出子模块复用：在已持锁的前提下写回全部账号
    pub fn save_locked(
        &self,
        state: &AccountState,
        guard: &MutexGuard<'_, ()>,
    ) -> Result<(), AccountStoreError> {
        self.save(state, guard)
    }

    /// 供导入导出子模块复用：加锁后执行一个「读-改-写」周期
    pub fn with_lock<T>(&self, action: impl FnOnce(&MutexGuard<'_, ()>) -> T) -> T {
        let guard = self.guard();
        action(&guard)
    }

    /// 按 id 取一条记录（**只读一行**；不存在或数据库不可用时 None）。
    ///
    /// 给「已经知道账号 id」的读路径用：`get_credentials_by_id` /
    /// `get_session_by_id` 与各家的 `*_account_record`。改造前它们都要
    /// 「读全量 → 在数组里 find」，现在是一次主键查询 —— 转发链路上这些函数
    /// 是**每个请求**都会调到的（`rotate` 按 id 取会话、适配器按 id 取记录），
    /// 于是「每次转发少解析 20 条记录的 JSON」是这条改造里收益最直接的一处。
    pub(crate) fn record_by_id(&self, _guard: &MutexGuard<'_, ()>, id: &str) -> Option<StoredAccount> {
        let db = self.inner.db.as_ref()?;
        db.with(|conn| sql::load_by_id(conn, id))
            .and_then(Result::ok)
            .flatten()
    }

    /// 取某个 provider 的全部账号（**只读这一家**）。
    ///
    /// `current_entry_for_provider` / `accounts_for_provider` 用它：两者的语义
    /// 都收窄在一家之内（「这一家的队首」/「这一家的启用账号」），
    /// 没有理由把别家的记录也读出来解析。过滤条件落在投影列 `provider` 上
    /// （该列的取值口径见 `sql.rs` 模块头）。
    pub(crate) fn records_for_provider(
        &self,
        _guard: &MutexGuard<'_, ()>,
        provider: &str,
    ) -> Vec<StoredAccount> {
        let Some(db) = self.inner.db.as_ref() else {
            return Vec::new();
        };
        db.with(|conn| sql::load_by_provider(conn, provider))
            .and_then(Result::ok)
            .unwrap_or_default()
    }

    // ─── 派生：当前账号 ──────────────────────────────────────

    /// 指定 provider 的「当前账号」= **该家**转发顺序里第一个「已启用且有可用凭证」的账号。
    ///
    /// 判据与转发层逐条相同（也是 `current_entry_for_provider` 的内核）：启用 +
    /// [`StoredAccount::has_credentials`]（桌面端实时账号记录里按设计没有 token，
    /// 但凭证在客户端登录态文件里，同样算「有凭证」）+ 按 (优先级, 加入时间) 取队首。
    /// 没有可用账号时返回 None（而不是回落到某个账号）—— 此时这一家转发确实拿不到
    /// 登录态，界面就不该给它的任何账号打「当前」标记。
    ///
    /// ── 作用域：这一家的队首 ──────────────────────────────────
    /// 全局队列之后转发不再逐家排队，本函数只剩「没有账号记录时回落到该家默认登录态」
    /// 与快照里 `currentAccountIds` 的参考用途。判据与全局的 `pick_current` 相同，
    /// 只是先缩到该 provider。历史上这里曾出现两个分歧（现已统一）：
    ///   1. 桌面端账号（raccoon/catpaw/autoclaw 的 `desktop: true`）被整体跳过 ——
    ///      转发明明会选它，界面却拿不到「它是当前账号」这个事实；
    ///   2. 只有小浣熊账号的机器上，workbuddy 的「当前账号」会是小浣熊的账号。
    /// 跨家展示（账号页每组卡片上的 ★）由调用方**逐家调用本函数**得到
    /// （见 `snapshot` 的 `currentAccountIds`），不要把不同家的结果塞进同一个字段。
    pub(crate) fn pick_current_for_provider(
        accounts: &[StoredAccount],
        provider: &str,
    ) -> Option<StoredAccount> {
        let mut candidates: Vec<StoredAccount> = accounts
            .iter()
            .filter(|item| item.provider() == provider && item.enabled() && item.has_credentials())
            .cloned()
            .collect();
        candidates.sort_by_key(StoredAccount::order_key);
        candidates.into_iter().next()
    }

    /// **全局**队首：所有提供商的账号排在同一条队列里，取第一个「已启用且有可用
    /// 凭证」的账号。这是账号页 ★ / 「首选」与 `/api/session` 的 `currentAccountId`
    /// 的数据源。对某个具体模型实际先用谁还要看它是否支持该模型、是否限流中，
    /// 那是转发层按请求逐次判定的（`routing::pick_account_by_priority`）。
    ///
    /// ── 为什么排除**没有转发能力**的家（历史用法，五家现已都能转发）──
    /// 这个值的消费方都把它当「会承接请求的那个账号」用：`/api/session` 的
    /// `currentAccountId` 决定顶栏显示的昵称/过期时间，`clear_session`（退出登录）
    /// 按它**删除账号**，界面的 ★ / 「设为首选」也按它渲染。Qoder 只有账号管理
    /// 能力那会儿（转发返回 501）必须排除：排进队首会让顶栏把它显示成当前登录态，
    /// 而「退出登录」会把它删掉。它接上推理协议后，这条过滤对它就自然失效了 ——
    /// 判据取自适配器的**恒定能力声明**（`supports_chat`），不写死 provider id，
    /// 因此两家各自的接线时点都不需要改这里。
    pub(crate) fn pick_current(accounts: &[StoredAccount]) -> Option<StoredAccount> {
        let mut candidates: Vec<StoredAccount> = accounts
            .iter()
            .filter(|item| item.enabled() && item.has_credentials() && forwards_requests(item))
            .cloned()
            .collect();
        candidates.sort_by_key(StoredAccount::order_key);
        candidates.into_iter().next()
    }

    /// 全局「当前账号」id（无账号或全部不可用时为 None）—— 全局队首，见 `pick_current`。
    ///
    /// 这里必须读**全部**账号：队首是跨家的全局派生（`pick_current` 要在所有
    /// provider 里比优先级），收窄到某一家就没有意义了。
    pub fn current_account_id(&self) -> Option<String> {
        let _guard = self.guard();
        let state = self.load(&_guard);
        Self::pick_current(&state.accounts).map(|record| record.id().to_string())
    }

    /// workbuddy 默认登录态的凭证与会话（供 workbuddy 专属链路实时取用：
    /// 模型目录拉取、登录态刷新等）；无可用账号时 None。
    ///
    /// **workbuddy 语义**（收窄到默认 provider）。转发链路不用它 ——
    /// 转发按全局队列选账号，只有「没有账号记录」时才回落到各家的默认登录态。
    pub fn get_current_entry(&self) -> Option<CurrentEntry> {
        self.current_entry_for_provider(DEFAULT_PROVIDER_ID)
    }

    /// 实际生效的账号（与 get_current_entry 同一套判据，独立入口只为语义清晰）
    pub fn get_active_entry(&self) -> Option<CurrentEntry> {
        self.get_current_entry()
    }

    /// 指定 provider 的「当前账号」凭证与会话（Agent2API W2b-T3）。
    ///
    /// 判据与 `pick_current_for_provider` 逐条相同（启用 + 有凭证 + 按 (优先级, 加入时间)
    /// 取队首），只是先在**该 provider 的账号组**里缩一遍。转发层逐家尝试时用它拿
    /// 「这一家的默认账号」，与 `accounts_for_provider` 的选路口径一致
    /// （后者用于显式指定的账号链，这里用于「没有账号记录/未登录」时的兜底会话）。
    ///
    /// ── 小浣熊的桌面端实时账号（W3-T4）─────────────────────────
    /// 它**记录里本来就没有 accessToken**（凭证在 `~/.box-agent/config/auth.json`），
    /// 因此判据从 `has_token()` 放宽成 `has_credentials()`：带 `desktop: true`
    /// 标记的 raccoon 记录也算「有凭证」，实际 token 由会话构造时实时读入
    /// （见 `session_from_record`）。只认 `has_token()` 会让这台机器上
    /// 小浣熊永远选不中账号，转发直接 401。
    ///
    /// 返回 None 表示该 provider 在账号数据里没有可用账号（此时
    /// `auth::get_current_session_for` 会尝试该 provider 的环境变量旁路）。
    pub fn current_entry_for_provider(&self, provider: &str) -> Option<CurrentEntry> {
        let _guard = self.guard();
        let records = self.records_for_provider(&_guard, provider);
        let mut candidates: Vec<StoredAccount> = records
            .into_iter()
            .filter(|item| item.enabled() && item.has_credentials())
            .collect();
        candidates.sort_by_key(StoredAccount::order_key);
        let record = candidates.into_iter().next()?;
        Some(CurrentEntry {
            id: record.id().to_string(),
            session: self.session_from_record(&record),
        })
    }

    /// 账号记录 → auth 模块的会话形态（端点/prefixPath/platform 按 edition 兜底）。
    ///
    /// `proxy` 是该账号解析出的出口（null = 直连），计费/签到等「拿着 session
    /// 直接发请求」的调用方无需再单独传代理参数；`proxyError` 非空表示配置的
    /// 代理解析失败，调用方应回退直连并提示。
    ///
    /// ── 小浣熊桌面端账号的凭证（W3-T4）─────────────────────────
    /// 这类记录**故意不落 token**（凭证在 `~/.box-agent/config/auth.json`，
    /// 客户端重新登录后下次请求即生效）。会话形态是「拿着就能发请求」的形状，
    /// 因此这里把 auth.json 的实时值填进 `auth` —— 否则转发链路的
    /// `has_access_token` 判定会把它当成没有登录态（401），而小浣熊适配器
    /// 也正是从 `auth.accessToken` 取 Authorization 头的。
    /// 读不到客户端登录态时退化成空 token：让上层的 401 文案去说明原因，
    /// 本函数不制造错误（它是被 CRUD 与选路大量复用的纯取值路径）。
    fn session_from_record(&self, record: &StoredAccount) -> Value {
        let edition = resolve_edition(record.edition().as_deref());
        let resolution = resolve_account_proxy(Some(&record.proxy()));
        let (proxy, proxy_error) = split_resolution(resolution);
        let proxy_error_value = proxy_error
            .clone()
            .map(Value::String)
            .unwrap_or(Value::Null);
        let live = live_desktop_credentials(record);
        let (access_token, refresh_token, expires_at) = match live {
            Some(credentials) => credentials,
            None => (
                record.access_token(),
                record.refresh_token(),
                record.expires_at().unwrap_or(0.0),
            ),
        };
        json!({
            "endpoint": record.endpoint().unwrap_or_else(|| edition.endpoint.to_string()),
            "prefixPath": record
                .prefix_path()
                .unwrap_or_else(|| edition.prefix_path.to_string()),
            "platform": record.platform().unwrap_or_else(|| edition.platform.to_string()),
            "edition": edition.id,
            "proxy": proxy,
            "proxyError": proxy_error_value,
            "auth": {
                "accessToken": access_token,
                "refreshToken": refresh_token,
                "tokenType": "Bearer",
                "expiresAt": expires_at,
                "refreshExpiresAt": record.refresh_expires_at().unwrap_or(0.0),
                "domain": record.domain(),
            },
            "account": {
                "uid": record.uid(),
                "nickname": record.nickname(),
                "type": record.account_type(),
                "enterpriseId": record.enterprise_id(),
                "enterpriseName": record.enterprise_name(),
            },
        })
    }

    /// 指定账号的凭证（Node 版 getCredentialsById）；无凭证/不存在时 None
    ///
    /// 小浣熊桌面端账号同样把 auth.json 的实时值填进 `access_token` /
    /// `refresh_token` / `expires_at`（理由见 `session_from_record`）——
    /// 「有没有可用凭证」在全仓是一条判据，不能因为凭证存在别处就分叉。
    pub fn get_credentials_by_id(&self, id: &str) -> Option<CredentialsById> {
        let _guard = self.guard();
        let record = self.record_by_id(&_guard, id)?;
        let live = live_desktop_credentials(&record);
        let (access_token, refresh_token, expires_at) = match live {
            Some(credentials) => credentials,
            None => (
                record.access_token(),
                record.refresh_token(),
                record.expires_at().unwrap_or(0.0),
            ),
        };
        if access_token.is_empty() {
            return None;
        }
        let edition = resolve_edition(record.edition().as_deref());
        let resolution = resolve_account_proxy(Some(&record.proxy()));
        let (proxy, proxy_error) = split_resolution(resolution);
        Some(CredentialsById {
            id: record.id().to_string(),
            name: record.name(),
            uid: record.uid(),
            access_token,
            refresh_token,
            expires_at: if expires_at > 0.0 { Some(expires_at) } else { None },
            endpoint: record.endpoint().unwrap_or_else(|| edition.endpoint.to_string()),
            prefix_path: record
                .prefix_path()
                .unwrap_or_else(|| edition.prefix_path.to_string()),
            platform: record.platform().unwrap_or_else(|| edition.platform.to_string()),
            edition: edition.id.to_string(),
            priority: record.priority(),
            enabled: record.enabled(),
            proxy,
            proxy_error,
        })
    }

    /// 指定账号的完整会话形态（Node 版 getSessionById）；不存在/无凭证时 None
    pub fn get_session_by_id(&self, id: &str) -> Option<SessionById> {
        let _guard = self.guard();
        let record = self.record_by_id(&_guard, id)?;
        if !record.has_credentials() {
            return None;
        }
        let resolution = resolve_account_proxy(Some(&record.proxy()));
        let (proxy, proxy_error) = split_resolution(resolution);
        Some(SessionById {
            id: record.id().to_string(),
            session: self.session_from_record(&record),
            proxy,
            proxy_error,
        })
    }
}

/// 桌面端实时账号的凭证（读取那一刻的值）；不是桌面端账号或读不到时 None。
///
/// 为什么不缓存到记录里：auth.json 是小浣熊生态的共享登录态，客户端/box-agent
/// 会在 token 临期时自行刷新并回写；把值缓存进账号记录只会得到一份过期副本，
/// 还会与「刷新成功后回写 auth.json」的路径打架（架构文档 §3.2 明确要求
/// 桌面端账号的凭证**每次实时读**）。
///
/// ── 两家桌面态（W4b-T-c2 起）────────────────────────────────
/// 小浣熊（`raccoon-desktop`，读 `~/.box-agent/config/auth.json`）与
/// AutoClaw（`autoclaw-desktop`，读 `%APPDATA%/AutoClaw/auth.json` 的
/// **safeStorage 密文**并走 DPAPI + AES-GCM 解密）都是「记录里不落 token、
/// 凭证实时读」的形态，因此都要在这里填进会话 —— 否则转发链路的
/// `build_chat_request` 从 `auth.accessToken` 取到空串，稳定 401。
/// AutoClaw 的解密结果由 `autoclaw::credentials` 的进程级 mtime 缓存兜住，
/// 每个请求都做一次 DPAPI 是被缓存挡住的那件贵事，不是本函数重复做的。
///
/// CatPaw 不在这里：它的凭证不是 Bearer（Cookie 形态的 `X-Passport-Token` +
/// 独立 uid），会话的 `auth.accessToken` 装不下它，由
/// `catpaw::adapter::forward_conversation` 自己经 `snapshot_for` 取。
///
/// `pub(crate)`：各家的账号公开形态（`raccoon_accounts.rs` /
/// `autoclaw_accounts.rs`）也要用它 —— 桌面端账号的展示字段（token 尾号/过期
/// 时间/能否刷新）必须来自同一份实时值，否则界面与转发看到的就是两个状态。
pub(crate) fn live_desktop_credentials(record: &StoredAccount) -> Option<(String, String, f64)> {
    if record.provider() == super::AUTOCLAW_PROVIDER_ID && record.is_desktop() {
        let credentials =
            crate::server::core::providers::autoclaw::credentials::local_credentials().ok()?;
        return Some((
            credentials.token,
            credentials.refresh_token,
            credentials.expires_at.unwrap_or(0.0),
        ));
    }
    // Cline 桌面端登录态：实时读 `~/.cline/data/settings/providers.json`。
    // 少了这一支，桌面端账号的会话里 `auth.accessToken` 会是空串 ——
    // 记录里**按设计不落 token**（见 `cline_accounts.rs` 的模块头），
    // 于是转发会以「账号缺少 accessToken」401 收场，而账号看着一切正常。
    if super::is_cline_family(&record.provider()) && record.is_desktop() {
        let credentials =
            crate::server::core::providers::cline::credentials::read_desktop_credentials()
                .ok()
                .flatten()?;
        return Some((
            credentials.access_token,
            credentials.refresh_token,
            credentials.expires_at.unwrap_or(0.0),
        ));
    }
    if record.provider() != super::RACCOON_PROVIDER_ID || !record.is_desktop() {
        return None;
    }
    let credentials = crate::server::core::providers::raccoon::credentials::desktop_credentials()
        .ok()?;
    Some((
        credentials.token,
        credentials.refresh_token,
        credentials.expires_at.unwrap_or(0.0),
    ))
}

/// 这条账号记录所属的 provider **是否能承接推理转发**（`ProviderAdapter::supports_chat`）。
///
/// 全局队首要排除「只有账号管理能力」的家（Qoder 在接上推理协议之前就是）：
/// 它会被顶栏当成当前登录态显示，而「退出登录」按队首**删除账号**。
/// 判据问适配器，不写死 id —— 于是某家从「只有账号管理」走到「也能转发」时，
/// 这里一行都不用改；未知 provider id（手改文件塞进来的）按「能转发」处理 ——
/// 与 `public_account` 的兜底口径一致，不让一条陌生记录把队首派生整个清空。
pub(crate) fn forwards_requests(record: &StoredAccount) -> bool {
    crate::server::core::providers::kind_from_id(&record.provider())
        .map(|kind| crate::server::core::providers::adapter::adapter_for(kind).supports_chat())
        .unwrap_or(true)
}

/// 把解析结果拆成 `(proxy, proxyError)`：失败时 proxy 为 null、
/// error 说明原因（调用方据此回退直连并提示）。
///
/// `proxy` 是 JSON（直接进会话对象）；`proxy_error` 是 `Option<String>`，
/// 会话形态里的 `proxyError` 由调用方组装成 null/字符串。
fn split_resolution(resolution: Option<ProxyResolution>) -> (Value, Option<String>) {
    match resolution {
        None => (Value::Null, None),
        Some(ProxyResolution::Resolved(ref proxy)) => {
            (json!(proxy_json(proxy)), None)
        }
        Some(ProxyResolution::Failed(message)) => (Value::Null, Some(message)),
    }
}

/// 出口 → JSON（与 ProxyResolution::to_json 的成功分支同形）
fn proxy_json(proxy: &crate::server::core::proxies::ResolvedProxy) -> Value {
    json!({
        "source": proxy.source,
        "protocol": proxy.protocol,
        "host": proxy.host,
        "port": proxy.port,
        "username": proxy.username,
        "password": proxy.password,
        "label": proxy.label,
    })
}
