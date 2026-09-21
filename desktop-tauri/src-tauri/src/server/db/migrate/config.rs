//! `{config_dir}/config.json` → `kv` 表的**逐顶层键**。
//!
//! ── 幂等判据：为什么这一项**需要**标记键（与其余六项都不同）──────
//! 其余项的判据都是「目标数据单元在不在」：
//!   - 独占一张表的（`accounts` / `logs` / `requests` / `request_daily` /
//!     `debug_traffic`）判「表空不空」；
//!   - 独占 `kv` 里一个键的（`desensitize` / `desktopSettings`）判「那个键
//!     在不在」。
//! 本项两条都走不通，原因是**配置项是开放集合**：
//!   - 「表空不空」不行 —— `kv` 是共享表（模块头的幂等原则已论证）；
//!   - 「某个键在不在」不行 —— 没有哪一个键能代表整份配置。用
//!     `apiKeys` 或任何单个键当代表都是错的：那几项完全可能恰好没配过；
//!   - 「`kv` 里有没有非保留键」也不行 —— 运行期写一次配置就会让这类键出现，
//!     而此时迁移可能还没跑（旧文件里其余键还没搬），据此跳过会把用户的旧配置
//!     永久留在文件里（`config::read_raw` 的回落路径会一直生效，看似正常，
//!     但用户改一项就会把没搬过来的那些项永久留在 `.json` 里）。
//! 所以本项**用**一个标记键 `configMigrated`（在 `kv` 里，且登记在
//! `db::schema::RESERVED_KV_KEYS`，因此不会被配置写入当成「已删掉的配置项」
//! 清掉）。
//!
//! ── 为什么这里可以推翻 T6 那条「不用 migratedXxx 标记」的结论 ──
//! `desensitize.rs` 的模块头论证过「不用标记键」，理由是**标记与数据可能不一致**
//! （标记写了但导入只落了一半，于是闸门认为迁过了，缺的那半永远补不上）。
//! 那条理由在本项**不成立**，因为本项把标记与数据放在**同一个事务**里提交
//! （`config::sql::import_conn`）：
//!   - 事务提交 ⇒ 每个配置键 + 标记都在；
//!   - 事务回滚 ⇒ 二者都不在，下次启动干净重试。
//! 「标记写了、数据缺一半」这个中间态在物理上不存在。换言之：T6 反对的是
//! **非原子**的标记，而不是标记本身。这是本项与 `desensitize` 的关键区别，
//! 也是「情况不同可以有不同结论」的那一处。
//!
//! ── 导入为什么不带删除（与运行期整份写的区别）────────────────
//! 运行期 `save_raw` 是**替换**语义（对应文件形态的整文件覆盖）：新配置里没有
//! 的键要删掉，否则「清空一个配置项」在库形态下不生效。迁移是**导入**语义：
//! 只把旧文件里的键搬进去，绝不删除库里已有的键。带上删除会有一条破坏路径
//! —— 迁移项跑在 `bootstrap` 里，万一将来有人在它之前写过配置，删除就会把
//! 那次写入抹掉。不带删除则最坏是「旧文件里的键被并进来」，那正是迁移该做的。
//! 完整论证见 `config::sql::import_conn`。
//!
//! ── 为什么排在注册表**第一位** ──────────────────────────────
//! 其余项要读配置：`import_logs` / `import_requests` / `import_debug` 用
//! `config::storage_dirs()` 找旧文件的自定义目录，`import_logs` 还用
//! `config::retention_settings()` 决定裁剪天数。配置进库之后，这几项读的
//! `config` 内存快照依赖「配置已经从旧文件读进来」——`config::init` 用
//! 回落读文件的方式解决了迁移**之前**的读取（见 `config::read_raw`），
//! 所以本项的先后**不影响正确性**；排第一只是让「先把配置搬好，再搬依赖配置
//! 的那些数据」这个顺序在读注册表时一眼可见，也让「配置已经在库里」这件事
//! 对后续项与运行期同时成立。
//!
//! ── 失败怎么办 ──────────────────────────────────────────────
//! 读文件失败 / 解析失败（整段不是 JSON 对象）/ 写库失败：记一行 ❌ 控制台日志
//! 并返回 `None`，**不备份旧文件**（`backup_legacy_file` 只在成功之后调），
//! 不阻断启动、不 panic。下次启动重试，而旧文件始终留在原处可供人工核对。
//! 记的是 `console_line` 而不是 `logging::log`：迁移发生在
//! `logging::init_store` 之前（见 `ServerState::bootstrap` 的顺序说明），
//! 那时日志库还没装入，`log()` 的入库那一路会静默丢弃 —— 而迁移失败恰恰是
//! 用户最需要看到的东西（与其余六项同一取舍）。

use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde_json::Value;

use super::backup::backup_legacy_file;
use super::LegacyOutcome;
use crate::server::config;
use crate::server::logging;

/// 旧配置文件名（`{config_dir}/config.json`）。
///
/// 字面量在本文件里写一次而不是引 `config::FILE_NAME`（那个常量随本次改造已
/// 不存在）：文件名是历史事实，与 `debug.rs` 的 `DEBUG_FILE_NAME` 同一处理。
const CONFIG_FILE_NAME: &str = "config.json";

/// 迁移项的可读名（`LegacyOutcome.label` 与「待迁移项清单」共用同一个字面量）。
pub(super) const LABEL: &str = "网关配置";

/// 旧文件的位置（不在原处时 `None`）。定位逻辑与 [`import_config`] 同源。
pub(super) fn legacy_file(dir: &Path) -> Option<PathBuf> {
    let path = dir.join(CONFIG_FILE_NAME);
    path.is_file().then_some(path)
}

/// 旧配置 `{config_dir}/config.json` → `kv` 表（逐顶层键一行）。
pub(super) fn import_config(conn: &Connection, dir: &Path) -> Option<LegacyOutcome> {
    let path = legacy_file(dir)?;
    // 已迁过 → 跳过（判据是**标记键存在**，理由见模块头）。检查在读取旧文件
    // 之前：已经迁过时连文件都不必打开。
    match config::sql::marker_present_conn(conn, config::sql::KEY_CONFIG_MIGRATED) {
        Ok(false) => {}
        Ok(true) => return None,
        Err(error) => {
            logging::console_line(
                "[Storage]",
                &format!("❌ 配置迁移失败：读取数据库失败（{error}）"),
            );
            return None;
        }
    }
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => {
            logging::console_line(
                "[Storage]",
                &format!("❌ 配置迁移失败：无法读取 {}（{error}）", path.display()),
            );
            return None;
        }
    };
    // 只认 JSON **对象**：配置的形态就是一个对象（顶层键就是配置项）。
    // 数组 / 标量 / 坏 JSON 都算「不认识这份内容」，跳过并保留原文件 ——
    // 不强写成空配置（那会让用户的旧文件看似被处理过，实际什么都没搬）。
    let raw = match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(map)) => map,
        Ok(_) => {
            logging::console_line(
                "[Storage]",
                &format!("❌ 配置迁移失败：{} 的顶层不是 JSON 对象", path.display()),
            );
            return None;
        }
        Err(error) => {
            logging::console_line(
                "[Storage]",
                &format!("❌ 配置迁移失败：{}（{error}）", path.display()),
            );
            return None;
        }
    };
    // 逐顶层键搬进 `kv`，并在**同一个事务**里落完成标记（原子性论证见模块头）。
    let imported = match config::sql::import_conn(conn, &raw, config::sql::KEY_CONFIG_MIGRATED)
    {
        Ok(count) => count,
        Err(error) => {
            logging::console_line(
                "[Storage]",
                &format!("❌ 配置迁移失败：{}（{error}）", path.display()),
            );
            return None;
        }
    };
    let backup = backup_legacy_file(&path);
    Some(LegacyOutcome {
        label: LABEL,
        source: path,
        // 报的是**顶层键数**：一份配置的规模就是「有多少个配置项」，
        // 日志里「导入 12 条」比「导入 1 条」更能说明迁了什么
        // （与 `desensitize` 项按词条数计数的口径一致）。
        imported,
        backup,
    })
}
