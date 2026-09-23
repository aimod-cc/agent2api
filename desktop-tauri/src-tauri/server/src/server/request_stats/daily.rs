//! `request_daily` 表（按天聚合）的**行级 SQL** —— 从 `sql.rs` 拆出的一支。
//!
//! ── 为什么独立成文件 ────────────────────────────────────────
//! `sql.rs` 同时装明细（`requests`）与聚合（`request_daily`）两张表的语句，
//! 「进行中行」与「原始正文」两轮改造后行数越过 800。聚合那一支天然自洽：
//! 一套列顺序常量、一对编解码、一组读-改-写语句，与明细侧唯一的耦合是
//! 「都在 `Db` 那把锁里跑」。拆开后 `sql.rs` 专注明细，本文件专注聚合 ——
//! 「聚合行的每个字段只由 `fold_into_daily` 累加而来」这条不变式的语句侧
//! 全部落点就在这一个文件（可见性同为 `pub(super)`，调用方与拆分前相同）。
//!
//! ── 三个 JSON 列为什么整体读写 ──────────────────────────────
//! `model_tokens` / `provider_stats` / `account_stats` 是 JSON 数组文本，
//! 累计必须**先读出整行、在 Rust 里累加、再整行写回**（见 `sql.rs` 模块头的
//! 「聚合行的读-改-写」）：总量列与三个维度列是同一批字段，增量 UPDATE 只
//! 推进总量会让「各维之和 = 当天总量」变成两处维护。一天只有一行、数组通常
//! 几十条，一个事务里读-改-写的代价可以接受。

use std::collections::BTreeMap;

use rusqlite::{params, Connection};

use super::record::{AccountAccum, DailyEntry, ModelAccum, ProviderAccum};

/// `request_daily` 的列顺序（所有 SELECT 都按这个顺序取，`decode_daily`
/// 依赖它；不写 `SELECT *` 的理由同 `sql::REQUEST_COLUMNS`）
const DAILY_COLUMNS: &str = "date, requests, successful, tokens, cache_hit_tokens, \
     cache_input_tokens, model_tokens, provider_stats, account_stats";

/// JSON 数组列 → `Vec<T>`。解析失败或内容不是数组时退化成空表。
///
/// 与旧实现读文件时的取向一致（坏行跳过、坏字段回落）：这三个列是**整体读写**
/// 的补充维度，一个坏值不该让整天的报表读不出来 —— 退化成空表只是那一维少一段，
/// 而总量列还在。
fn decode_accum<T: serde::de::DeserializeOwned>(text: &str) -> Vec<T> {
    serde_json::from_str(text).unwrap_or_default()
}

/// JSON 数组列 ← `Vec<T>`。序列化失败给 `'[]'`（与 DDL 的默认值同一形态，
/// 于是「这一列没有数据」在库里只有一种表示）。
fn encode_accum<T: serde::Serialize>(items: &[T]) -> String {
    serde_json::to_string(items).unwrap_or_else(|_| "[]".to_string())
}

/// 行 → `DailyEntry`
fn decode_daily(row: &rusqlite::Row<'_>) -> rusqlite::Result<DailyEntry> {
    let model_tokens: String = row.get(6)?;
    let provider_stats: String = row.get(7)?;
    let account_stats: String = row.get(8)?;
    Ok(DailyEntry {
        date: row.get(0)?,
        requests: row.get(1)?,
        successful: row.get(2)?,
        tokens: row.get(3)?,
        cache_hit_tokens: row.get(4)?,
        cache_input_tokens: row.get(5)?,
        model_tokens: decode_accum::<ModelAccum>(&model_tokens),
        provider_stats: decode_accum::<ProviderAccum>(&provider_stats),
        account_stats: decode_accum::<AccountAccum>(&account_stats),
    })
}

/// 全部聚合行 → `BTreeMap`（报表的纯函数要的就是这个形状）。
///
/// 「聚合寿命独立于明细」这条契约在这里成立：报表的区间统计只读这张表，
/// 明细被保留期裁掉之后历史曲线不会出现空洞。
pub(super) fn select_daily_map(conn: &Connection) -> rusqlite::Result<BTreeMap<String, DailyEntry>> {
    let sql = format!("SELECT {DAILY_COLUMNS} FROM request_daily ORDER BY date ASC");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([])?;
    let mut out = BTreeMap::new();
    while let Some(row) = rows.next()? {
        let day = decode_daily(row)?;
        out.insert(day.date.clone(), day);
    }
    Ok(out)
}

/// 取某一天的聚合行（`record` 的读-改-写的「读」）
pub(super) fn select_daily_row(
    conn: &Connection,
    date: &str,
) -> rusqlite::Result<Option<DailyEntry>> {
    let sql = format!("SELECT {DAILY_COLUMNS} FROM request_daily WHERE date = ?1");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![date])?;
    match rows.next()? {
        Some(row) => Ok(Some(decode_daily(row)?)),
        None => Ok(None),
    }
}

/// 整行写入（存在即覆盖）。
///
/// `date` 是主键，所以「同一天重复记账」天然是覆盖而不是插入两行 ——
/// 这正是选它当主键的理由（见 `schema.rs`）。三个 JSON 列由
/// [`encode_accum`] 编码，总量列直接取 `DailyEntry` 的字段值。
pub(super) fn upsert_daily(conn: &Connection, day: &DailyEntry) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO request_daily (date, requests, successful, tokens, cache_hit_tokens, \
         cache_input_tokens, model_tokens, provider_stats, account_stats) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
         ON CONFLICT(date) DO UPDATE SET requests = excluded.requests, \
         successful = excluded.successful, tokens = excluded.tokens, \
         cache_hit_tokens = excluded.cache_hit_tokens, \
         cache_input_tokens = excluded.cache_input_tokens, \
         model_tokens = excluded.model_tokens, provider_stats = excluded.provider_stats, \
         account_stats = excluded.account_stats",
        params![
            day.date,
            day.requests,
            day.successful,
            day.tokens,
            day.cache_hit_tokens,
            day.cache_input_tokens,
            encode_accum(&day.model_tokens),
            encode_accum(&day.provider_stats),
            encode_accum(&day.account_stats),
        ],
    )?;
    Ok(())
}

/// 删掉某一天的聚合行（重算后发现那天一条明细都不剩）
pub(super) fn delete_daily(conn: &Connection, date: &str) -> rusqlite::Result<usize> {
    conn.execute("DELETE FROM request_daily WHERE date = ?1", params![date])
}

/// 删掉 `date < cutoff_key` 的聚合行（时间维度保留；定长日期串字典序即时间序）
pub(super) fn delete_daily_before(conn: &Connection, cutoff_key: &str) -> rusqlite::Result<usize> {
    conn.execute("DELETE FROM request_daily WHERE date < ?1", params![cutoff_key])
}

/// 聚合行数与容量裁剪（兜底：手改库塞进十万行时不至于把报表撑爆）
pub(super) fn count_daily(conn: &Connection) -> rusqlite::Result<usize> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM request_daily", [], |row| row.get(0))?;
    Ok(count.max(0) as usize)
}

/// 只保留日期最新的 `max` 行（先判行数再删，理由见 `sql.rs` 的 `trim_requests_capacity`）
pub(super) fn trim_daily_capacity(conn: &Connection, max: usize) -> rusqlite::Result<usize> {
    if count_daily(conn)? <= max {
        return Ok(0);
    }
    conn.execute(
        "DELETE FROM request_daily WHERE date NOT IN \
         (SELECT date FROM request_daily ORDER BY date DESC LIMIT ?1)",
        params![max as i64],
    )
}

/// 清空全部聚合行（`clear()` 用）
pub(super) fn delete_all_daily(conn: &Connection) -> rusqlite::Result<usize> {
    conn.execute("DELETE FROM request_daily", [])
}
