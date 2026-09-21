//! `{config_dir}/desensitize.json` → `kv` 表的 `desensitize` 键。
//!
//! ── 幂等判据：**键存不存在**，不是「表为空」──────────────────
//! 这是本项与其它五个迁移项最重要的区别，务必看清：
//! `accounts` / `logs` / `requests` / `request_daily` / `debug_traffic` 都是
//! **本项独占的表**，「表里有没有行」就是「本项迁过没有」。`kv` 不是 ——
//! 它是**共享表**（账号的 `priorityScope`、日志的 `logsNextId`、桌面设置
//! `desktopSettings` 都在里面，见 `server/db/schema.rs` 的键命名规范），
//! 启动时 `kv` 几乎必然已经有别的键。若照抄「表非空即跳过」，本项会**永远
//! 跳过**（用户的词表配置默默留在旧文件里，界面读到的却是默认词表）。
//! 所以这里判的是**自己那个键在不在**（`desensitize::legacy_present` →
//! `SELECT 1 FROM kv WHERE key = 'desensitize'`）。
//!
//! 判据换了，框架那条「幂等原则」的**目的**没变：重复启动不重复导入。
//! 键一旦写进去就一直在（本模块不删它，`reset_terms` 也只是写一份新值），
//! 于是第二次启动即跳过；旧文件改名之后更是连文件都看不到。
//! 与「表非空」一样，这里也**不用** `kv` 里另记一个 `migratedDesensitize: true`
//! 标记：标记与数据可能不一致（标记写了但写入只落了一半），而「目标键存在」
//! 是数据自己的事实，不会说谎（框架模块头对这条有完整论证）。
//!
//! ── 旧文件在哪：只有配置目录一个候选 ────────────────────────
//! `desensitize.json` **没有**可配置的位置键（`config.rs` 里没有这一类键，
//! 与 `logDir` / `requestStatsDir` / `debugDir` 不同），它从第一天起就固定在
//! `{config_dir}` 里。所以这里不抄 `logs` / `debug` 两项的「候选目录」写法：
//! 那不是谨慎而是**多余的推测路径**（一个永远不会命中的候选），而多出来的
//! 分支会让「到底该去哪个目录找」在阅读时变得不确定。
//!
//! ── 解析与落库都走运行期那条路 ──────────────────────────────
//! 解析用 `desensitize::import_legacy`（内部是运行期 `load` 的同款归一），
//! 它同时负责按 `kv` 的键名落库。这样「迁移进来的状态」与「运行期读到的状态」
//! 由同一份实现保证一致；各写一份解析的后果在 T5 已有先例（snake_case 与
//! camelCase 的差一点就是一份静默的字段丢失）。
//!
//! ── 失败怎么办 ──────────────────────────────────────────────
//! 解析失败（整段不是合法 JSON）与写库失败都走「记一行 ❌ 控制台日志 + 返回
//! `None`」：**不阻断启动、不备份旧文件**（`backup_legacy_file` 只在成功之后
//! 调）。用户下次启动会被重试，而那份文件始终留在原处可供人工核对。
//! 记的是 `console_line` 而不是 `logging::log`：迁移发生在 `logging::init_store`
//! **之前**（见 `ServerState::bootstrap` 的顺序说明），此时日志库还没装入，
//! `log()` 的入库那一路会静默丢弃 —— 而迁移失败恰恰是用户最需要看到的东西。

use std::path::{Path, PathBuf};

use rusqlite::Connection;

use super::backup::backup_legacy_file;
use super::LegacyOutcome;
use crate::server::core::desensitize;
use crate::server::logging;

/// 词表旧文件名（`{config_dir}/desensitize.json`）。
///
/// 字面量在本文件里写一次而不是引 `desensitize::FILE_NAME`：那个常量随本次改造
/// 已经**不存在了**（状态进了数据库），而迁移项处理的正是它 ——
/// 名字是历史事实，不会随代码演进变化（与 `debug.rs` 的 `DEBUG_FILE_NAME` 同一
/// 处理）。
const DESENSITIZE_FILE_NAME: &str = "desensitize.json";

/// 迁移项的可读名（`LegacyOutcome.label` 与「待迁移项清单」共用同一个字面量）。
pub(super) const LABEL: &str = "脱敏词表";

/// 旧文件的位置（不在原处时 `None`）—— 定位器与 [`import_desensitize`] 共用。
///
/// 只有配置目录一个候选：`desensitize.json` **没有**可配置的位置键
/// （`config.rs` 里没有这一类键，与 `logDir` / `requestStatsDir` / `debugDir`
/// 不同），它从第一天起就固定在 `{config_dir}` 里。所以这里不抄 `logs` /
/// `debug` 两项的「候选目录」写法：那不是谨慎而是**多余的推测路径**
/// （一个永远不会命中的候选），而多出来的分支会让「到底该去哪个目录找」
/// 在阅读时变得不确定。
pub(super) fn legacy_file(dir: &Path) -> Option<PathBuf> {
    let path = dir.join(DESENSITIZE_FILE_NAME);
    path.is_file().then_some(path)
}

/// 旧词表 `{config_dir}/desensitize.json` → `kv` 的 `desensitize` 键。
pub(super) fn import_desensitize(conn: &Connection, dir: &Path) -> Option<LegacyOutcome> {
    let path = legacy_file(dir)?;
    // 已迁过 → 跳过（判据是**键存在**而不是表非空，见模块头）。检查在读取
    // 旧文件之前：已经迁过时连文件都不必打开。
    match desensitize::legacy_present(conn) {
        Ok(false) => {}
        Ok(true) => return None,
        Err(error) => {
            logging::console_line(
                "[Storage]",
                &format!("❌ 脱敏词表迁移失败：读取数据库失败（{error}）"),
            );
            return None;
        }
    }
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => {
            logging::console_line(
                "[Storage]",
                &format!("❌ 脱敏词表迁移失败：无法读取 {}（{error}）", path.display()),
            );
            return None;
        }
    };
    // 一整份状态就是一行（`sql::save` 是一条 UPSERT），它本身就是原子的 ——
    // 框架要求的「整批一个事务」在这里天然满足，不需要再开事务。
    let imported = match desensitize::import_legacy(conn, &text) {
        Ok(count) => count,
        Err(error) => {
            logging::console_line(
                "[Storage]",
                &format!("❌ 脱敏词表迁移失败：{}（{error}）", path.display()),
            );
            return None;
        }
    };
    let backup = backup_legacy_file(&path);
    Some(LegacyOutcome {
        label: LABEL,
        source: path,
        // 报的是**词条数**而不是「1 条状态」：这份记录的规模就是词表大小，
        // 日志里「导入 47 条」比「导入 1 条」更能说明迁了什么
        // （与 `logs` / `requests` 两项按各自数据单元计数的口径一致）。
        imported,
        backup,
    })
}
