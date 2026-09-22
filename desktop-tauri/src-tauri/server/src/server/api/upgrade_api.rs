//! 数据结构升级（旧 JSON/JSONL 文件 → 统一 SQLite 库）的两条端点。
//!
//! ```text
//! GET  /api/upgrade       还有没有待迁移的旧数据（界面据此决定要不要跑）
//! POST /api/upgrade/run   跑一次迁移（幂等；失败项不阻断其它项）
//! ```
//!
//! ── 为什么迁移不在这里自动跑，而由界面在启动时触发 ────────────
//! 这次更新把数据存储从「八个 JSON/JSONL 文件」换成了单个 SQLite 库。
//! 迁移动作本身放在 `POST /api/upgrade/run` 而不是塞进启动流程，是因为它要读
//! 几十 MB 旧文件并写一整批行 —— 让**界面在启动后**发这一枪，用户能看到进度
//! 反馈（toast），也不会拖长进程启动到「窗口可见」的时间。
//!
//! 界面侧（`ui/upgrade-panel.js`）**打开就升级、不弹窗**：升级没有选项、不能
//! 取消（不升级 = 账号与历史记录不可用），弹窗只是一道多余的坎，而它的「稍后」
//! 出口会让账号一直空着看起来像坏了。自动执行**不改变**「旧数据永不删除」这条
//! 硬不变量 —— 旧文件仍只被改名（见下一节）。
//!
//! ── 旧数据为什么永不删除 ────────────────────────────────────
//! 迁移成功后旧文件只被**改名**成 `{原名}.migrated`（见
//! `db::migrate::backup` 的论证：改名是同一文件系统的元数据操作，原子、
//! 不产生第二份数据、也天然保证同一个文件不会被迁两次）。本模块与整个迁移
//! 框架里**没有任何删除用户数据的路径**。用户没点过任何东西就看到程序在搬
//! 数据，所以界面必须在迁移前后各报一次「文件保留在原处」。
//!
//! ── 迁移后要重装哪些内存状态（本模块最容易漏的一处）─────────
//! 各 store 的形状不同，处理也不同，逐个核实过（结论与依据）：
//!   - `config`：**必须** `reload()`。它的快照在 `bootstrap` 里装入，而那时
//!     迁移还没跑，快照是从旧 `config.json` **回落读**出来的；迁移把配置搬进
//!     库之后，库才是真相来源，重装一次让本次运行就用上与库一致的那一份
//!     （论证见 `config::reload`）。指纹脱敏开关也走这份快照（它读
//!     `config::current().sanitize_fingerprints()`），因此随 config 一起生效。
//!   - `AccountStore` / `LogStore` / `RequestStats` / `debug_traffic`：**不需要**
//!     额外处理。四者都持 `Db` 句柄、每次操作现读库（没有全量内存快照），
//!     所以迁移导入的行下一个查询就能看到。
//!   - `settings`（壳侧桌面设置，含端口）：不需要。它的读取是「库 → 旧文件 →
//!     默认值」三级回落（`settings::load`），迁移前后读到同一份值；而它唯一
//!     需要「早期读到」的字段是端口，那个在 `Db::open` 之前就读完了，
//!     迁移改不了它（改了也要重启才生效）。
//!   - `core::model_rules`：不需要。它是 `config::current().raw()` 的**派生**
//!     视图（`ModelRules::from_raw`），每次 `current()` 现算 —— 配置重载之后
//!     它自然跟着变。
//!
//! ── 并发与失败 ──────────────────────────────────────────────
//! 迁移本体在 `spawn_blocking` 里跑：它做的是文件 IO（旧明细可能几十 MB）与
//! 一整批数据库写，不该占住 async worker。同时用一把进程级锁挡住重复点击
//! （迁移项各自幂等，重复跑不会插重复数据，但会白读一遍大文件、并让两次调用的
//! 返回值互相矛盾）。失败项不阻断其它项（框架的形状保证），失败原因由各迁移项
//! 自己写进控制台日志；响应里如实回报**只成功了哪几项**，界面据此提示可重试。

use std::sync::Mutex;

use axum::extract::State;
use axum::response::Response;
use serde_json::json;

use crate::server::errors;
use crate::server::http::ok_json;
use crate::server::logging;
use crate::server::ServerState;

/// 迁移执行中的互斥标记。`true` = 有一次迁移正在跑。
///
/// 用 `std::sync::Mutex` 而不是 tokio 的锁：临界区只有一次布尔赋值，异步锁
/// 没有意义；而它保护的动作（跑迁移）发生在阻塞线程里。
static RUNNING: Mutex<bool> = Mutex::new(false);

/// 取「开始执行」的许可；已经在跑时返回 `false`（调用方给 409）。
fn begin_run() -> bool {
    match RUNNING.lock() {
        Ok(mut guard) => {
            if *guard {
                return false;
            }
            *guard = true;
            true
        }
        // 锁中毒（某次持锁 panic）：接管内部值继续用 —— 与各 store 的 guard()
        // 同一取向，这个标记没有会被 panic 打断的中间态
        Err(poisoned) => {
            let mut guard = poisoned.into_inner();
            if *guard {
                return false;
            }
            *guard = true;
            true
        }
    }
}

/// 释放「开始执行」的许可。**必须**在每条退出路径上调用（用 guard 兜住 panic
/// 之外的所有路径；release 是 panic=abort，没有 unwind 要担心）。
fn end_run() {
    match RUNNING.lock() {
        Ok(mut guard) => *guard = false,
        Err(poisoned) => *poisoned.into_inner() = false,
    }
}

/// GET /api/upgrade —— `{ pending, items }`。
///
/// `items` 是待迁移项的可读名清单（如 `["网关配置","账号","事件日志"]`），
/// 供弹窗列出「将要导入哪些数据」。`pending` 为 false 时 `items` 恒为空。
pub async fn get_upgrade(State(state): State<ServerState>) -> Response {
    let pending = state.upgrade_pending();
    let items = if pending { state.upgrade_items() } else { Vec::new() };
    ok_json(json!({ "pending": pending, "items": items }))
}

/// POST /api/upgrade/run —— 跑一次旧数据迁移。
///
/// 返回 `{ outcomes: [{label, imported}], pending }`：`outcomes` 只含**本次真的
/// 导入了东西**的项（没有旧文件、或库里已有数据的项不在里面，那是正常跳过）；
/// `pending` 是跑完之后重算的结果 —— 仍为 true 说明还有文件没搬成（某一项失败
/// 且旧文件保留在原处），界面据此让用户重试。
///
/// 幂等：重复调用安全（迁移项各自有幂等闸门），所以界面不必做「只许点一次」的
/// 防护；但同一时刻只受理一次（并发跑会白读大文件，且两次返回值会互相矛盾），
/// 重入给 409。
pub async fn run_upgrade(State(state): State<ServerState>) -> Response {
    let Some(db) = state.db().cloned() else {
        // 库不可用：没有可导入的目标。明确报错而不是「跑完 0 项」——
        // 后者会让用户以为升级成功了，实际什么都没发生（与 bootstrap 里
        // 「库不可用就不提示待迁移」同一判据）。
        return errors::management_error(500, "数据库不可用，无法导入旧数据（详见启动日志）");
    };
    if !begin_run() {
        return errors::management_error(409, "数据升级正在执行中，请稍候");
    }

    // 迁移本体放进阻塞线程池：读几十 MB 的旧文件 + 一整批数据库写 + 账号侧的
    // 整队/导入（都要读写库），不该占住 async worker（与改造前 `relocate` 的
    // 处置一致）。
    let config_dir = state.config_dir.clone();
    let store = state.store().clone();
    let request_stats = state.request_stats();
    let result = tokio::task::spawn_blocking(move || {
        let outcomes =
            db.with(|conn| crate::server::db::migrate::run_legacy_migrations(conn, &config_dir));
        // 只有真导入了东西才补做后续动作（一项都没成时库里的配置与账号都没变）
        if matches!(outcomes, Some(ref list) if !list.is_empty()) {
            // 配置快照必须重装：迁移把配置搬进库之后，库才是真相来源（见模块头）。
            crate::server::config::reload();
            // 账号侧的启动期迁移必须**补做一次**（见 `account_bootstrap` 的
            // 模块头）：启动时它们被整块跳过了（那时 `accounts` 表还空着，
            // 跑它们会把 `import_accounts` 的幂等闸门永久关上）。现在账号已经
            // 进库、前提重新成立 —— 不补这一下，用户本次运行看到的是一个没整过
            // 队、没补过 provider、也没导入过三家旧数据的账号列表（要等下次
            // 重启才好）。
            //
            // 顺序：必须排在 `config::reload()` **之后** —— 其中的
            // `migrate_cline_split` 读 `modelRules`，而那个配置项正是这次迁移
            // 刚从旧 `config.json` 搬进来的，快照没重装就读不到它。
            crate::server::account_bootstrap::run(&store);
            // 统计侧的口径订正同理补调（启动时被 upgrade_pending 跳过，
            // 见 `RequestStats::remap_model_dimension_once` 的调用时机说明）：
            // 旧聚合行刚导进来，模型维度还按请求名累计，不重算的话本次运行
            // 的报表会一直把映射别名当成独立模型。
            request_stats.remap_model_dimension_once();
        }
        outcomes
    })
    .await;
    end_run();

    let outcomes = match result {
        Ok(Some(outcomes)) => outcomes,
        // `Db::with` 返回 None = 连接锁中毒（某次持锁 panic 打断了未提交的事务）。
        // 接管连接去读可能读到半截数据，所以它保守回落 —— 这里如实报错。
        Ok(None) => return errors::management_error(500, "数据库连接不可用，迁移未执行"),
        Err(error) => return errors::management_error(500, format!("迁移任务执行失败: {error}")),
    };

    let payload: Vec<_> = outcomes
        .iter()
        .map(|item| json!({ "label": item.label, "imported": item.imported }))
        .collect();
    // 逐项记日志：这次迁移发生在**日志库已就绪之后**（用户点按钮时应用早跑起来了），
    // 所以走 `logging::log` 让它进日志页 —— 用户事后能在「日志」里回看这次升级
    // 导入了什么（与启动期迁移只能走控制台不同，见 `db::migrate` 模块头）。
    if outcomes.is_empty() {
        logging::log("[Storage]", "数据升级：没有可导入的旧数据（可能已升级过）");
    } else {
        for item in &outcomes {
            logging::log(
                "[Storage]",
                &format!("✅ {} 旧数据已导入数据库（{} 条）", item.label, item.imported),
            );
        }
    }

    let pending = state.refresh_upgrade_pending();
    ok_json(json!({ "outcomes": payload, "pending": pending }))
}
