//! `{config_dir}/desktop-settings.json` → `kv` 表的 `desktopSettings` 键。
//!
//! ── 幂等判据：键存在 ─────────────────────────────────────────
//! `kv` 是**共享表**（账号的 `priorityScope`、日志的 `logsNextId`、
//! 配置迁移标记 `configMigrated` 都在里面），所以判据是
//! 「**自己那个键在不在**」而不是「表为空」。完整论证见框架模块头
//! 的幂等原则（那一条对写 `kv` 的项都成立）。
//!
//! 本项**不需要** `configMigrated` 那样的标记键：桌面设置是**单个数据单元**
//! （整份设置一个键），「那个键在不在」就是完整的答案 ——
//! 只有配置项那种开放集合才需要标记（论证见 `config.rs` 的模块头）。
//! 注意 `desktopSettings` 也在 `db::schema::RESERVED_KV_KEYS` 里，因此
//! `config::save_raw` 的整份写不会把它当成「已删掉的配置项」清掉。
//!
//! ── 为什么这一项是设置**唯一**的搬迁者（没有并存的运行期路径）─────
//! 桌面壳的 `settings::load()` 确实可能在 `Db::open` **之前**被调用（端口 ——
//! 见桌面 `settings` 模块头的时序论证），早先那里配了一条「读不到库就回落旧
//! 文件，读到就顺手写进库」的惰性迁移来兜底。**那条路径已经删掉**，因为它在
//! 真实启动顺序下必然抢在本项前面（桌面 `lib.rs` 的 setup 第一步就读
//! `closeToTray`，而 `Db::open` 在 `backend::ensure_ready` 里，晚于它），于是
//! 本项每次都被「键已存在」挡掉 —— 后果是旧文件永不改名为 `.migrated`、启动
//! 日志里那句「✅ 桌面设置 旧数据已导入数据库」永不出现，而下面两段（以及
//! `backup_legacy_file` 的调用点清单）恰恰承诺了这两件事。完整论证见桌面
//! `settings` 模块头「为什么没有惰性迁移」。
//!
//! ── 辅助函数为什么在本文件本地实现 ──────────────────────────
//! 拆 crate 前它们住在桌面壳的 `settings` 模块（读旧文件 / 校验解析 / 写库）。
//! 迁移是一次性逻辑，而桌面壳在 headless 形态里根本不参与 —— 把迁移要用的
//! 三个小函数搬过来本文件自持，桌面壳与迁移互不依赖。代价是「这份 JSON 能不
//! 能按设置解析」的判据（[`LegacyAppSettings`]）与桌面的 `AppSettings` 成了
//! **镜像结构**：桌面侧给 `AppSettings` 增删字段时，必须同步这里 —— 两边文件
//! 都有注释指向对方，这是拆 crate 的显式代价。
//!
//! 于是本项现在承担两件事，缺一不可：
//!   - **搬数据**（把旧文件的值写进 `kv`）；
//!   - **收尾**（改名备份 + 进启动日志）—— 这两条只有框架项做得到。
//!
//! ── 备份 ────────────────────────────────────────────────────
//! 成功后把旧文件改名成 `desktop-settings.json.migrated`（与其余项一致）。
//! 改名之后回落读路径自然失效（读不到旧文件），后续启动只会走库那条路。
//!
//! ── 失败怎么办 ──────────────────────────────────────────────
//! 读失败 / 解析失败 / 写库失败：记一行 ❌ 控制台日志并返回 `None`，
//! **不备份旧文件**（那份文件是运行期回落链的最后保障 —— 它一旦被改名，
//! 而库里的值又读不懂，用户的端口设置就真的丢了）。不阻断启动、不 panic。
//! 记 `console_line` 而不是 `logging::log`：迁移发生在日志库装入之前
//! （与其余七项同一取舍，见 `db::migrate` 模块头）。

use std::path::{Path, PathBuf};

use rusqlite::Connection;

use super::backup::backup_legacy_file;
use super::LegacyOutcome;
use crate::server::logging;
use crate::paths;

/// 迁移项的可读名（`LegacyOutcome.label` 与「待迁移项清单」共用同一个字面量）。
pub(super) const LABEL: &str = "桌面设置";

/// 旧文件的位置（不在原处时 `None`）。
///
/// 路径走 `paths::legacy_desktop_settings_file()`（它从 `paths::config_dir()` 派生）
/// 而不是 `dir.join(...)`：`dir` 是调用点传进来的配置目录，两者正常情况下
/// 相同，但「桌面设置在哪」这件事的事实来源只该有一处（`paths` 模块），
/// 各拼一次路径就多一个漂移点。本项不判「配置目录变了没有」，所以直接用
/// paths 的路径 —— 定位器与 [`import_settings`] 共用这一份。
pub(super) fn legacy_file(_dir: &Path) -> Option<PathBuf> {
    let path = paths::legacy_desktop_settings_file();
    path.is_file().then_some(path)
}

/// 桌面设置 `{config_dir}/desktop-settings.json` → `kv` 的 `desktopSettings` 键。
pub(super) fn import_settings(conn: &Connection, dir: &Path) -> Option<LegacyOutcome> {
    // 定位器与「待迁移项清单」共用同一份（见 [`legacy_file`]）：`dir` 只被转交，
    // 本项不判「配置目录变了没有」，所以实际路径由 settings 模块给出。
    let path = legacy_file(dir)?;
    // 已迁过 → 跳过（判据是**键存在**，见模块头）。检查在读取旧文件之前。
    match key_present(conn) {
        Ok(false) => {}
        Ok(true) => return None,
        Err(error) => {
            logging::console_line(
                "[Storage]",
                &format!("❌ 桌面设置迁移失败：读取数据库失败（{error}）"),
            );
            return None;
        }
    }
    let Some(text) = read_legacy_text(&path) else {
        logging::console_line(
            "[Storage]",
            &format!("❌ 桌面设置迁移失败：无法读取 {}", path.display()),
        );
        return None;
    };
    // 解析不过就不搬（**关键**：库里写进一份读不懂的值会让运行期的回落链断掉
    // ——桌面壳的 `settings::load` 命中库里的值就不再回落旧文件，用户会看到
    // 设置全变回默认，而旧文件又被改名了。理由详见 `legacy_parses` 的注释）。
    if !legacy_parses(&text) {
        logging::console_line(
            "[Storage]",
            &format!("❌ 桌面设置迁移失败：{} 不是合法的设置 JSON（文件保留原处）", path.display()),
        );
        return None;
    }
    // 一整份设置就是一行（UPSERT），它本身就是原子的 —— 框架要求的
    // 「整批一个事务」在这里天然满足（单键 UPSERT 本身就是原子的）。
    if let Err(error) = write_migrated(conn, &text) {
        logging::console_line(
            "[Storage]",
            &format!("❌ 桌面设置迁移失败：写入数据库失败（{error}）"),
        );
        return None;
    }
    let backup = backup_legacy_file(&path);
    Some(LegacyOutcome {
        label: LABEL,
        source: path,
        // 桌面设置是单个数据单元（整份一个键），所以这里恒为 1 ——
        // 与 `config` 项按顶层键数计数的区别在于：那一个是「一份里有 N 条」，
        // 这里一份就是一条。
        imported: 1,
        backup,
    })
}

/// `kv` 里桌面设置的键名（与桌面壳 `settings::sql::KEY` 逐字一致；约定见
/// `db::schema` 模块头，且它在 [`crate::server::db::schema::RESERVED_KV_KEYS`]
/// 里 —— 配置写入不会碰它）。
const KEY: &str = "desktopSettings";

/// `kv` 里有没有 `desktopSettings` 键（幂等闸门）。
///
/// 只取 `SELECT 1 ... LIMIT 1` 而不是把值读出来：闸门只关心存在性。
fn key_present(conn: &Connection) -> rusqlite::Result<bool> {
    let found: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM kv WHERE key = ?1 LIMIT 1",
            rusqlite::params![KEY],
            |row| row.get(0),
        )
        .ok();
    Ok(found.is_some())
}

/// 迁移用：读旧文件的原文（**不解析**，保持原样交给迁移）。
///
/// 整份文本搬过去的好处：用户手写的未知字段一并保留（与配置项「全量保留
/// 未知字段」的不变量一致）。
fn read_legacy_text(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// 迁移用：这份旧设置文本能不能按设置解析。
///
/// 为什么要先判一次：把一份读不懂的内容写进库，会让**运行期的回落链断掉**
/// —— 库里有了值（虽然读不懂），桌面壳的 `settings::load` 就不会再回落
/// （命中即返回，解析失败取默认），于是用户看到的是「设置全变回默认」，
/// 而旧文件又被改名成了备份。判一次则宁可让文件留在原处、库里不写
/// （本项跳过 + 保留旧文件，与其它项遇到坏 JSON 时同一处理）。
///
/// [`LegacyAppSettings`] 是桌面壳 `AppSettings` 的**镜像**（见模块头），
/// 桌面侧改字段必须同步这里。
fn legacy_parses(text: &str) -> bool {
    serde_json::from_str::<LegacyAppSettings>(text).is_ok()
}

/// 桌面壳 `AppSettings` 的迁移期镜像：只用于 [`legacy_parses`] 的解析校验。
///
/// 字段名与 serde 属性（`camelCase` + `default`）必须与桌面壳 `AppSettings`
/// 保持一致 —— 本迁移只判「读不读得懂」，不消费字段值（字段都收进 `_` 前缀）。
/// **桌面侧 `AppSettings` 增删字段时同步这里**（两边注释互相指向）。
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct LegacyAppSettings {
    _close_to_tray: bool,
    _autostart: bool,
    _proxy_port: u16,
}

impl Default for LegacyAppSettings {
    fn default() -> Self {
        Self { _close_to_tray: true, _autostart: false, _proxy_port: 0 }
    }
}

/// 迁移用：把一份旧设置文本写进库（**用迁移框架给的连接**）。
///
/// 为什么不走「自开短命连接」的运行期写法：迁移的写入必须与其余七项一样
/// 跑在框架那批连接里，「写不进去时旧文件要不要保留」这个判断要和写入处在
/// 同一个上下文。UPSERT 语句与桌面壳 `settings::sql::write_conn` 逐字一致；
/// 不建表 —— 迁移跑在 `Db::open` 里，schema 已经建好。
fn write_migrated(conn: &Connection, text: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO kv (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![KEY, text],
    )?;
    Ok(())
}
