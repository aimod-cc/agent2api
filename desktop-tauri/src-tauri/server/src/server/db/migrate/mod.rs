//! 旧文件 → 数据库的**一次性迁移框架**（注册表 + 公共备份工具）。
//!
//! ── 本文件为什么先于具体迁移存在 ─────────────────────────────
//! 改造要迁移的旧文件有七个：`config.json`、`accounts.json`、`logs.jsonl`、
//! `requests.jsonl`、`request-daily.jsonl`、`debug-traffic.jsonl`、
//! `desktop-settings.json`。它们由**不同的切片**逐个
//! 接入，但「怎么判断已经迁过」「失败怎么办」「旧文件留不留」这三件事必须
//! 一开始就统一 —— 各切片各写一套的话，迁移策略会随切片顺序漂移，而且
//! 「重跑会不会重复导入」这个最要命的问题会变成八个不同的答案。
//! 所以先把骨架、幂等原则与备份工具定下来，后续切片只填「读文件 → 插库」。
//!
//! （第八个 `desensitize.json` 曾在这份清单里：敏感词脱敏的旧词表文件。
//! 那个功能已随规则集换成硬编码指纹脱敏而整体删除，对应的迁移项与模块
//! 一并移除 —— 旧文件留在原处不再导入，库里的 `desensitize` 键也只是
//! 留着的遗留数据，见 `db::schema` 的保留键清单。）
//!
//! ── 幂等原则（后续每个迁移项都必须遵守）──────────────────────
//! 迁移**只在目标数据单元为空时执行**：每个迁移项进来的第一件事是判「本项的
//! 数据在库里有没有」——**独占一张表的项**判 `SELECT COUNT(*) FROM 目标表`，
//! **写 `kv` 的项**判「自己那个键在不在」（`kv` 是共享表，表非空与它迁过没有
//! 毫无关系，见 `settings.rs` 的模块头）。非 0 / 已存在就直接返回 `None`。
//! 于是：
//!   1. 重复启动不会重复导入 —— 第一次导入完数据就在那里了；
//!   2. 即使旧文件没被改名（改名失败、用户在迁移后手工把文件放回来），
//!      第二次启动也只是跳过，不会把同一批数据插两遍；
//!   3. 旧文件改名之后第二次启动连文件都看不到了，天然幂等 ——
//!      所以「目标数据非空」这条检查是第二道保险，不是唯一依据。
//! 为什么用「目标数据有没有」而不是在 `kv` 里记一个 `migratedXxx: true` 标记：
//! 标记与数据可能不一致（标记写了但导入只写了一半就被中断 —— 那时必须重跑，
//! 而标记会阻止重跑；反之标记没写但数据进去了，重跑就插重复行）。
//! 「数据自己有没有」是数据自己的事实，不会说谎。注意写 `kv` 的项自己那个
//! 数据单元（如 `kv.desktopSettings` 键）**本身就是**这个原则的实例：
//! 它要么完整地在那里、要么不在，不存在「迁了一半」的中间态。
//!
//! ── 失败为什么不阻断启动 ────────────────────────────────────
//! 迁移是**旧数据的一次性搬运**，它的失败不该让网关起不来：迁移前的老版本
//! 二进制照样能读那些旧文件（本任务不动任何现有 store 的读写逻辑），所以
//! 「这次迁移失败 → 下次启动再试」是安全的降级路径。于是注册表里的每一项
//! 都是「尽力而为」：失败只记一行日志、返回 `None`，后续项照跑。
//!
//! 日志走 [`crate::server::logging::console_line`] 而**不是** `logging::log`：
//! 迁移发生在 `logging::init_store` **之前**（数据库必须先于日志库就绪，
//! 见 `ServerState::bootstrap` 的顺序说明），此时日志库还没装入，`log()`
//! 的入库那一路会静默丢弃 —— 而迁移结果恰恰是用户最需要在控制台看到的东西。
//! `console_line` 只写控制台，不依赖任何初始化状态。
//!
//! ── 文件布局 ────────────────────────────────────────────────
//! ```text
//! server/db/
//!   migrate/
//!     mod.rs      框架本体：注册表 + 遍历执行（本文件）
//!     backup.rs   公共工具：旧文件改名备份（所有迁移项共用）
//!     accounts.rs accounts.json → accounts 表
//!     logs.rs     logs.jsonl    → logs 表
//!     requests.rs requests.jsonl + request-daily.jsonl → requests / request_daily 表
//!     debug.rs    debug-traffic.jsonl → debug_traffic 表
//!     config.rs   config.json → kv 表（**每个顶层键一行**）
//!     settings.rs desktop-settings.json → kv 表的 `desktopSettings` 键
//! ```
//! 每个迁移项一个文件：**它们各自独立**（读不同文件、写不同表/键、互不依赖），
//! 合在一个文件里只会让「这一项要迁移什么」需要翻很久才能看清；
//! 而几项加起来的注释量远超 800 行，拆开也符合本项目的单文件行数约定。
//! 框架（注册表、`LegacyOutcome`、备份工具）留在 `mod.rs` 与 `backup.rs`：
//! 那是**所有项都要遵守**的东西，改一次必须让各项同时生效。
//!
//! ── 三种幂等判据（看注册表时先分清自己在看哪一类）─────────────
//!   - **独占一张表**（账号 / 事件日志 / 请求明细 / 请求聚合 / 调试报文）：
//!     判「表空不空」；
//!   - **独占 `kv` 的一个键**（桌面设置）：判「自己那个键在不在」
//!     —— 不能判表空不空，`kv` 是共享表（见 `settings.rs` 模块头）；
//!   - **写 `kv` 的开放集合**（网关配置）：用**标记键** —— 配置项没有哪个键能
//!     代表整份配置（见 `config.rs` 模块头那一整段论证，它同时也回应了
//!     「T6 为什么反对标记键」这个看起来矛盾的地方：反对的是非原子的标记）。
//!
//! ── 迁移项怎么写（给后续切片的模板）─────────────────────────
//! 注册表项是一个 [`LegacyMigration`]，由三样东西组成：可读名、**旧文件定位器**、
//! 导入函数：
//! ```text
//! fn import_accounts(conn: &Connection, dir: &Path) -> Option<LegacyOutcome> {
//!     let path = legacy_file(dir)?;                        // 定位器（与下面同源）
//!     if count(conn, "accounts") > 0 { return None; }      // 表非空 → 已迁过
//!     let outcome = match 真的导入(conn, &path) {          // 整批一个事务
//!         Ok(n) => LegacyOutcome { label: LABEL, source: path.clone(), imported: n, backup: backup_legacy_file(&path) },
//!         Err(error) => { console_line("[Storage]", &format!("❌ 账号迁移失败: {error}")); return None; }
//!     };
//!     Some(outcome)
//! }
//! ```
//! 三条注意事项：
//!   - **整批一个事务**：导入 N 行必须原子提交（中断后表要么空、要么完整），
//!     否则「表非空」这道幂等闸门会被半截数据关上。这里拿到的是 `&Connection`，
//!     事务用 `unchecked_transaction()`（`transaction()` 要 `&mut`）——
//!     调用点是 `Db::with`，全程只有一把锁、没有第二个访问者，那条
//!     「同连接不得嵌套事务」的约束由调用点保证。
//!   - **失败要自己记日志**：`Option` 表达不了「没有旧文件」与「导入失败」的区别
//!     （都是 `None`），所以失败必须由迁移项自己用 `console_line` 讲清楚，
//!     本框架不替它猜（见 [`run_legacy_migrations`]）。
//!   - **不得 panic**：release 构建是 `panic = abort`，没有 unwind 路径可以兜住
//!     迁移里的 panic（`catch_unwind` 在 abort 策略下也不会返回），一次越界
//!     索引就会带走整个应用。所以解析旧文件一律用 `Option`/`Result` 兜底，
//!     不要把「旧数据一定长这样」当成前提。
//!
//! ── 定位器为什么和导入函数放在同一个注册表项里 ────────────────
//! 「有没有待迁移的旧文件」要由界面（升级弹窗）在**迁移跑之前**问出来，
//! 而它问的正是「注册表里每一项的旧文件还在不在」。若把定位逻辑另写一份清单，
//! 两份清单迟早分叉（新增一项只改了注册表，界面就永远说「没有待迁移数据」；
//! 或者某个模块改了旧文件位置，判定与导入对不上）。放进同一个结构体之后，
//! 加一项只改一处，[`pending_items`] 与 [`run_legacy_migrations`] 必然一致。
//! 各项的定位器**由各迁移项自己提供**（文件名常量与候选目录的知识都留在那边），
//! 本框架只负责遍历。

mod accounts;
mod backup;
mod config;
mod debug;
mod logs;
mod requests;
mod settings;

use std::path::{Path, PathBuf};

use rusqlite::Connection;

/// 单项旧数据迁移的结果。**这是后续所有迁移项的统一返回形态**。
///
/// 字段含义：`label` 是给人看的迁移项名（启动日志的
/// `✅ {label} 旧数据已导入数据库（{imported} 条）`）；`source` 是旧文件路径；
/// `imported` 是本次导入的条数；`backup` 是旧文件改名后的备份路径
/// （改名失败时为 `None` —— 数据仍留在原处，表已导入完成）。
///
/// `source` 与 `backup` 目前**没有读取方**（启动日志只用 `label` / `imported`，
/// 见 `ServerState::bootstrap`），因此各自标注 `#[allow(dead_code)]`：它们是
/// 「迁移结果」这份契约的一部分，后续「存储」页要展示「从哪个文件导入、备份在
/// 哪」，删掉会让那一批切片回来改这个结构体（而那时每个迁移项都在用它）。
/// 按本项目「有意保留的设施逐个标注并写明理由」的做法（见 `server/mod.rs` 的
/// dead_code 一节）处理。
pub struct LegacyOutcome {
    /// 迁移项名（中文，进启动日志）
    pub label: &'static str,
    /// 旧文件路径
    #[allow(dead_code)]
    pub source: PathBuf,
    /// 本次导入的条数
    pub imported: usize,
    /// 旧文件改名后的备份路径（`None` = 没改名成功或本来就没有旧文件）
    #[allow(dead_code)]
    pub backup: Option<PathBuf>,
}

/// 一个迁移项：可读名 + 旧文件定位 + 导入。
///
/// 三样东西放在一起而不是拆成两个并行清单（理由见模块头「定位器为什么和导入
/// 函数放在同一个注册表项里」）。`label` 与导入函数写进 `LegacyOutcome` 的
/// 是**同一个常量**（各模块的 `LABEL`），所以界面列出的清单与启动日志里的
/// 名字不会出现两套说法。
pub struct LegacyMigration {
    /// 迁移项名（中文；界面列「待迁移数据」与启动日志共用）
    pub label: &'static str,
    /// 旧文件还在不在（`Some` = 待迁移）。找不到时返回 `None`。
    pub locate: fn(&Path) -> Option<PathBuf>,
    /// 真正导入（幂等、失败自记日志，见模块头）
    pub import: fn(&Connection, &Path) -> Option<LegacyOutcome>,
}

/// 迁移注册表：**逐项接入**，当前八项（网关配置、桌面设置、账号、事件日志、
/// 请求明细、请求聚合、调试报文、脱敏词表）。
///
/// 用 `const` 数组而不是运行期注册（`OnceLock<Vec<...>>` / 全局可变表）：
///   1. 顺序即执行顺序，写在源码里一眼可见 —— 迁移之间**有依赖**
///      （「账号」要先于「请求统计」导入，后者的 account_id 才有名字可查），
///      可变注册表会让这个顺序散落在各个模块的初始化时机上；
///   2. 没有初始化时机问题：`Db::open` 之后随时可读，不必关心谁先注册；
///   3. 每加一项就是加一个元素，没有别的登记动作要记得做。
///
/// 加项时注意：**同一项只能出现一次**（框架没有去重），且三项都是 `fn`
/// （不能是闭包 —— 闭包要捕获环境，那就得改类型为 `&dyn Fn` 并引入
/// 生命周期，而迁移只依赖「库连接 + 目录」两个参数，没有捕获的需求）。
///
/// ── 各项之间的顺序（有一处**真实依赖**，别随手调）───────────────
/// `import_daily` 必须排在 `import_requests` **之后**：聚合项导入完旧聚合行
/// 之后要做一次**口径回填**（用明细重算那些缺账号维度的历史日子，见
/// `backfill` 的模块头），而它读的正是刚导进来的 `requests` 表 ——
/// 顺序反了，回填会因为「明细为空」而一天都算不上，那段历史就永久留在
/// 「未知账号」组里（安全边界「拿不准就不动」会保护数据，但也救不回来）。
/// 其余项之间没有依赖：各写自己的表/键、各自判自己的数据单元。
/// 顺序：网关配置 → 桌面设置 → 账号 → 事件日志 → 请求明细 → 请求聚合
/// → 调试报文 → 脱敏词表。
///
/// 领头的 `import_config` / `import_settings` 排最前是**可读性**而非正确性
/// 要求：`import_logs` / `import_requests` / `import_debug` 要读
/// `config::storage_dirs()`（旧文件的自定义目录）与
/// `config::retention_settings()`（日志裁剪天数），而配置进库之后那些读取
/// 依赖「配置已经从旧文件读进来」—— 这一点由 `config::init` 的**回落读文件**
/// 保证（见 `config::read_raw` 的时序论证），所以本项排在哪都不影响结果。
/// 排最前让「先把配置搬好、再搬依赖配置的数据」这个顺序在注册表上一眼可见。
///
/// 排在最后的 `import_debug` 与请求明细**无关联依赖**（它只写 `debug_traffic`，
/// 不读明细也不被明细读）—— 报文与明细靠 id 关联，但那是**读取期**的事
/// （用户在详情弹窗里按下按钮才查），迁移期谁先谁后都不影响结果。
/// 排在末尾只是按「数据量从大到小」的习惯：报文单条可达数 MB，放最后不会让
/// 前面几项的日志被大文件的读取拖在后面。
///
/// `import_settings` 同理无依赖（只写 `kv` 里自己的键，与账号、日志、统计都
/// 不相干）—— 它的**判据与独占一张表的项不同**（判「自己那个键在不在」而不是
/// 「表空不空」，因为 `kv` 是共享表）；`import_config` 的判据是**第三种**
/// （标记键，因为配置项是开放集合）—— 两种判据的完整论证分别见
/// `settings.rs` / `config.rs` 的模块头。
pub const LEGACY_MIGRATIONS: &[LegacyMigration] = &[
    LegacyMigration { label: config::LABEL, locate: config::legacy_file, import: config::import_config },
    LegacyMigration { label: settings::LABEL, locate: settings::legacy_file, import: settings::import_settings },
    LegacyMigration { label: accounts::LABEL, locate: accounts::legacy_file, import: accounts::import_accounts },
    LegacyMigration { label: logs::LABEL, locate: logs::legacy_file, import: logs::import_logs },
    LegacyMigration { label: requests::LABEL, locate: requests::requests_file, import: requests::import_requests },
    LegacyMigration { label: requests::DAILY_LABEL, locate: requests::daily_file, import: requests::import_daily },
    LegacyMigration { label: debug::LABEL, locate: debug::legacy_file, import: debug::import_debug },
];

/// 还在原处的旧文件对应的迁移项名（**界面列「待迁移数据」用**）。
///
/// 判据就是「旧文件还在不在」：迁移成功后它们被改名为 `*.migrated`，
/// 所以「文件存在」与「这项还没迁」是同一件事，不需要另造一个标记键
/// —— 标记与数据可能不一致，而文件在不在是磁盘自己的事实（与模块头
/// 「幂等原则」同一取舍）。
///
/// 返回的清单**只用于展示**：真正的执行走 [`run_legacy_migrations`]，
/// 而每一项自己还会再判一次幂等闸门（表空不空 / 键在不在），所以这里
/// 判「有」而实际什么都没导入是正常结果（库里已有数据），不是 bug。
pub fn pending_items(dir: &Path) -> Vec<&'static str> {
    LEGACY_MIGRATIONS
        .iter()
        .filter(|migration| (migration.locate)(dir).is_some())
        .map(|migration| migration.label)
        .collect()
}

/// 有没有待迁移的旧文件（[`pending_items`] 的布尔形态）。
///
/// 单独给一个布尔入口而不是让调用方 `!pending_items(dir).is_empty()`：
/// 启动路径（`ServerState::bootstrap`）只需要这个布尔值，而它每次启动都要问一次
/// —— 少构造一个 `Vec` 是有意义的（八项定位器各自要做一次 `is_file()`）。
pub fn has_pending(dir: &Path) -> bool {
    LEGACY_MIGRATIONS
        .iter()
        .any(|migration| (migration.locate)(dir).is_some())
}

/// 遍历注册表执行每一项，返回**成功项**的结果（失败/跳过的项不在返回里）。
///
/// 「不阻断」这条保证落在本函数的形状上：循环体里没有 `?`、没有 `break`，
/// 每一项的返回值只决定「要不要收集」，不会影响后面任何一项 —— 前面那项
/// 失败也好、没旧文件也好，后面每一项都照样被调用。
///
/// 为什么本函数不为失败的项记日志：`Option` 把「没有旧文件」与「导入失败」
/// 归成了同一个 `None`（这是刻意的 —— 框架不该要求每个迁移项自造一套错误
/// 类型，见模块头的取舍），所以框架**无从知道**某一次 `None` 是正常跳过
/// 还是真出了错，去记一条「迁移失败」只会把正常情况误报成失败。失败日志
/// 由最清楚发生了什么的那一层（迁移项自己）用 `console_line` 写。
pub fn run_legacy_migrations(conn: &Connection, dir: &Path) -> Vec<LegacyOutcome> {
    let mut outcomes = Vec::new();
    for migration in LEGACY_MIGRATIONS {
        if let Some(outcome) = (migration.import)(conn, dir) {
            outcomes.push(outcome);
        }
    }
    outcomes
}
