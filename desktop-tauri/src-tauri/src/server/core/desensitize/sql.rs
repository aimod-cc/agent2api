//! `kv` 表里 `desensitize` 键的**行级 SQL 访问层** —— 本模块里唯一出现 SQL 的地方。
//!
//! ── 为什么单独一层 ──────────────────────────────────────────
//! 改造前状态是 `{config_dir}/desensitize.json` 一整份 JSON 文件：读是一次
//! `read_to_string` + 解析，写是一次「先在内存里拼好缩进、再整文件覆写」。
//! 进数据库后那套「目录存不存在 / 半截文件 / 权限」的顾虑整体消失：一整份状态
//! 就是 `kv` 表的一行，读是一次主键查询、写是一条 UPSERT，原子性与落盘交给
//! SQLite。本文件承接这两条语句，上层（`mod.rs`）只做序列化、归一与降级
//! （与 `logs_store/sql.rs` / `request_stats/sql.rs` / `debug_traffic/sql.rs`
//! 的分工一致）。
//!
//! ── 值为什么还是 JSON 文本 ──────────────────────────────────
//! `kv.value` 里存的就是那段 JSON（值的形态约定见 `server/db/schema.rs` 模块头），
//! 这里不做编码转换、也不拆成多行：状态是「没有固定结构、整份进出」的数据，
//! 拆键会让「一次写入」变成多行更新（徒增事务与冲突面），而保持 JSON 文本让
//! `sqlite3` 直接看库时一眼能读懂词表。
//!
//! ── 解析为什么不在这里 ──────────────────────────────────────
//! 「值解析不出来时回落到哪份状态、要不要打日志」是**上层**的知识（回落默认
//! 词表 + 打一行日志），本层只把原始文本交出去。于是本层也不会因为「值被手改
//! 坏了」而多出一套自己的容错口径。
//!
//! ── 并发：本层不加锁、不打日志 ──────────────────────────────
//! 所有函数取裸 `&Connection`，串行化由 `Db` 那把 Mutex 负责（上层每个操作都在
//! **一次** `Db::with` 调用里跑完）。**硬约束**：持这把锁期间绝不能再调
//! `logging::log` / `logging::verbose` —— 它们要写同一个库，`std::sync::Mutex`
//! 不可重入，会当场死锁（本层所有函数都不打日志，错误一律 `Err` 交回上层）。

use rusqlite::{params, Connection, OptionalExtension};

/// `kv` 里本模块状态的键名（键命名规范见 `server/db/schema.rs` 模块头）。
///
/// 定成常量而不是散落的字面量：写入侧、读取侧、迁移项的幂等闸门三处必须是
/// 同一个字符串 —— 错一个字符的后果是「迁移怎么写都不算迁过」，每次启动都
/// 重导一遍。
pub(super) const KEY: &str = "desensitize";

/// 状态记录的原始 JSON 文本（没有这个键 → `None`）。
///
/// `value` 列虽然声明为 `NOT NULL`，这里仍按 `Option<String>` 读：手工改库
/// 塞进去一个 NULL 是唯一能造出这种行的途径，而那种行按「没有记录」处理才是
/// 安全的（回落默认词表，而不是把 NULL 当成一份空状态解析成别的什么）。
pub(super) fn load_text(conn: &Connection) -> rusqlite::Result<Option<String>> {
    let value: Option<Option<String>> = conn
        .query_row("SELECT value FROM kv WHERE key = ?1", params![KEY], |row| {
            row.get(0)
        })
        .optional()?;
    Ok(value.flatten())
}

/// 有没有本模块的状态记录（迁移项的幂等闸门用）。
///
/// 只取 `SELECT 1` 而不把整行读出来：闸门只关心存在性，词表可能有几百个词、
/// 几十 KB，为了判「迁过没有」把值读进来是白费。
pub(super) fn has_key(conn: &Connection) -> rusqlite::Result<bool> {
    let found: Option<i64> = conn
        .query_row("SELECT 1 FROM kv WHERE key = ?1", params![KEY], |row| {
            row.get(0)
        })
        .optional()?;
    Ok(found.is_some())
}

/// 写入状态（UPSERT）。
///
/// 用 `ON CONFLICT DO UPDATE` 而不是纯 `INSERT`：`kv` 是共享表，这个键可能
/// 已经存在（用户改过配置）也可能不存在（全新用户第一次保存），两条路径
/// 都要能跑，而调用方不该为此分两个方法（与 `account_store::sql` 写
/// `priorityScope`、`logs_store::sql` 写 `logsNextId` 同一写法）。
///
/// **这一条语句本身就是原子的**：本模块的「一批」就是这一行，不需要再包事务 ——
/// 迁移项那条「整批一个事务」的要求由它天然满足。
pub(super) fn save(conn: &Connection, text: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO kv (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![KEY, text],
    )?;
    Ok(())
}
