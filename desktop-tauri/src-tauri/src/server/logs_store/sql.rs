//! 日志表的**行级 SQL 访问层** —— 事件日志存储里唯一出现 SQL 的地方。
//!
//! ── 为什么单独一层 ──────────────────────────────────────────
//! 改造前日志在 `{log_dir}/logs.jsonl`：内存里持一份 `Vec<LogEntry>` 当**有效
//! 全集**，文件只是它的落盘镜像，于是要有 `dirty` / `appends_since_compact` /
//! `COMPACT_STEP` 这套「攒够一批才整文件重写」的机制去维持两者一致。
//! 进数据库之后这份双份状态整体消失：库就是全集，`append` 是一条 `INSERT`，
//! 裁剪是一条 `DELETE`，没有「内存比文件多/少」的对齐问题，也没有整文件重写。
//! 本文件承接这些语句，上层（`logs_store.rs`）只做归一、降级与 API 形状。
//!
//! ── `id` 的分配：为什么用一个 kv 计数器而不是让它自增 ────────
//! `logs.id` 是 `INTEGER PRIMARY KEY`（rowid 别名）。不显式给 id 时 SQLite 取
//! `MAX(rowid) + 1`，看上去天然单调 —— **但它会复用**：删掉当前最大 id 的那一行
//! 之后，下一条插入会拿到同一个号。已用探针实测确认（删 id=3 → 下一条仍是 3）。
//! 而本项目的「id 单调递增」是一条**有消费方依赖的不变量**：
//!   - 导航徽标把 `stats.lastId` 当已读水位存进 localStorage，再用
//!     `GET /api/logs?level=error&sinceId=<水位>` 数未读错误（`app.js`
//!     `updateLogsBadge` / `refreshUnreadErrors`）；
//!   - 前端专门有一条「`lastId < 水位` ⇒ 被清空过，水位回落」的分支。
//! 复用会让新日志的 id **等于**旧水位，`id > 水位` 从此筛不到它 —— 新错误
//! 永远不亮徽标。删掉最大 id 是可达的：`clear_where` 按条件删（可能命中最新
//! 那条），`prune` 按 `ts` 删（`ts` 允许补写历史所以可能是最新那条）。
//! 因此这里把「下一个 id」记在 `kv` 表的 `logsNextId` 键上（`kv` 正是
//! schema 里给「各存储的元数据」准备的落点）：它是**只增不减的高水位**，
//! 删行不影响它。取值时与 `MAX(id) + 1` 取较大者 —— 于是计数器丢了、库被人
//! 用 sqlite3 手工改过、或旧版本写的库第一次升级上来，都能自愈，不会撞主键。
//!
//! ── 顺序：`ORDER BY id` ────────────────────────────────────
//! 旧实现里 `id` 与数组下标同序（`push` 出来的），所以「按 id 排序」与
//! 「按写入顺序」是同一件事。库这边同样成立：`id` 是分配即递增的高水位。
//!
//! ── 并发：本层不做任何加锁 ──────────────────────────────────
//! 所有函数都取裸 `&Connection`，串行化由 `Db` 的那把 Mutex 负责
//! （`LogStore` 的每个操作都在**一次** `Db::with` / `with_mut` 调用里跑完，
//! 多语句操作用事务包住）。**硬约束**：持这把锁期间绝不能再调
//! `logging::log` 一类的写日志函数 —— 日志本身要写同一个库，`std::sync::Mutex`
//! 不可重入，会当场死锁（`logs_store.rs` 模块头也记了这条）。

use rusqlite::types::Value as SqlValue;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};

use super::{level_rank, LogEntry, Query, CATEGORIES, LEVELS};

/// `kv` 里「下一个日志 id」的键名（键命名规范见 `server/db/schema.rs` 模块头）
pub(super) const NEXT_ID_KEY: &str = "logsNextId";

/// 一行的列顺序（所有 SELECT 都按这个顺序取，`decode_row` 依赖它）
const COLUMNS: &str = "id, ts, level, category, message, data";

/// 行 → `LogEntry`。
///
/// 这里**不做归一**（不调 `normalize_level` / `normalize_category` /
/// `normalize_data`）：库里的值全部由写入侧归一过（`append` 与
/// `db::migrate::import_logs` 都走同一套归一函数），读侧再归一既多余，
/// 也会掩盖「有人绕过写入侧直接往库里塞脏值」这件事。
/// 只有 `data` 例外 —— 它是 JSON 文本，解析失败时退化成 `None`
/// （与旧实现「读文件时坏行跳过、坏字段回落」的容错取向一致）。
fn decode_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<LogEntry> {
    let id: i64 = row.get(0)?;
    let ts: i64 = row.get(1)?;
    let level: String = row.get(2)?;
    let category: String = row.get(3)?;
    let message: String = row.get(4)?;
    let data: Option<String> = row.get(5)?;
    Ok(LogEntry {
        // id 是分配出来的正整数（见模块头），负数不可能出现
        id: id.max(0) as u64,
        ts,
        level,
        category,
        message,
        data: data.and_then(|text| serde_json::from_str(&text).ok()),
    })
}

/// `data` 列的文本形态（`None` → SQL NULL）
fn encode_data(value: Option<&serde_json::Value>) -> Option<String> {
    value.map(|item| item.to_string())
}

// ─── 计数与读取 ─────────────────────────────────────────────

/// 库里的日志总数（`QueryResult.total` 与容量判定都用它）
pub(super) fn count_all(conn: &Connection) -> rusqlite::Result<usize> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM logs", [], |row| row.get(0))?;
    Ok(count.max(0) as usize)
}

/// 当前最大 id（空表为 0）—— `Stats.lastId`（已读水位）的数据源。
///
/// 与旧实现 `guard.entries.last().map(|item| item.id)` 等价：`id` 单调递增，
/// 所以数组最后一条就是 id 最大的那条。
pub(super) fn max_id(conn: &Connection) -> rusqlite::Result<u64> {
    let value: Option<i64> = conn.query_row("SELECT MAX(id) FROM logs", [], |row| row.get(0))?;
    Ok(value.unwrap_or(0).max(0) as u64)
}

/// 每级别计数（`(level, count)`；空表返回空表）。
///
/// 不在这里补齐「计数为 0 的已知级别」—— 那是 `Stats` 的形状问题（要保证
/// 四个级别键恒定存在），由上层拼装；本层只如实回报库里有什么。
pub(super) fn level_counts(conn: &Connection) -> rusqlite::Result<Vec<(String, i64)>> {
    group_counts(conn, "SELECT level, COUNT(*) FROM logs GROUP BY level")
}

/// 每分类计数（同上）
pub(super) fn category_counts(conn: &Connection) -> rusqlite::Result<Vec<(String, i64)>> {
    group_counts(conn, "SELECT category, COUNT(*) FROM logs GROUP BY category")
}

/// 分组计数的公共执行体。SQL 是**两条写死的语句**（不给列名做字符串插值）：
/// 本层不接受任何来自调用方的 SQL 片段。
fn group_counts(conn: &Connection, sql: &str) -> rusqlite::Result<Vec<(String, i64)>> {
    let mut stmt = conn.prepare(sql)?;
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push((row.get(0)?, row.get(1)?));
    }
    Ok(out)
}

/// 全部日志，按 id 升序 —— `to_jsonl` 的导出顺序（旧的「文件顺序」）。
pub(super) fn select_all_asc(conn: &Connection) -> rusqlite::Result<Vec<LogEntry>> {
    let sql = format!("SELECT {COLUMNS} FROM logs ORDER BY id ASC");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(decode_row(row)?);
    }
    Ok(out)
}

// ─── 筛选条件 → SQL ─────────────────────────────────────────

/// 一次查询的**筛选计划**：能下推给 SQL 的条件进 `WHERE`，不能的在 Rust 侧。
///
/// ── 为什么关键词必须留在 Rust 侧（这是本层最需要解释的取舍）────
/// 现在用的是 `message.to_lowercase().contains(keyword)`（先 `to_lowercase` 再
/// 子串匹配，两边都按 **Unicode** 折叠大小写）。SQLite 的 `LIKE` 默认
/// **只对 ASCII 大小写不敏感**（`A`–`Z`），对 `İ` / `Σ` 这类非 ASCII 字符
/// 的行为与 Rust 的 `to_lowercase` 不同；`instr()` 更是完全区分大小写。
/// 用 `LIKE` 会让「带非 ASCII 大小写的日志搜不出来/搜错」，
/// 这是**可观察的行为差异**，所以关键词一律取回来在 Rust 里过滤。
/// 代价是 `LIMIT` 没法下推（必须「先过滤再取最新 N 条」），见
/// [`select_matching_desc`] 的说明 —— 表上限 500 行，这个代价可以忽略。
///
/// `Query::matches`（旧实现在内存里跑的那条过滤链）被本结构取代：
/// `query` 与 `clear_where` **共用同一份**计划（都调 [`select_matching_desc`]），
/// 于是「查询里看到的 N 条」与「删除时删掉的那 N 条」必然是同一批 ——
/// 这正是旧注释强调的「两处手写必然漂移」，现在由结构保证而不是靠纪律。
pub(super) struct FilterPlan {
    /// `WHERE ...` 片段；无条件时为空串
    where_sql: String,
    /// 与 `where_sql` 里的 `?` 一一对应的绑定值
    binds: Vec<SqlValue>,
    /// 关键词（已 trim + `to_lowercase`）；`None` = 该条件不生效
    keyword: Option<String>,
}

impl FilterPlan {
    /// 按旧 `Query::matches` 的条件逐条编译（顺序无关，SQL 只做 AND 连接）。
    ///
    /// 三条「条件非法即不生效」的口径逐字保留：`level` / `category` 只有取值
    /// 在字典里才参与过滤，否则整条条件被忽略（前端可能传空串或陌生值）。
    pub(super) fn of(query: &Query) -> Self {
        let mut fragments: Vec<String> = Vec::new();
        let mut binds: Vec<SqlValue> = Vec::new();

        // ① 级别下限：debug < info < warn < error，「该级别及以上」即
        //    LEVELS[rank..]。库里的 level 恒为 LEVELS 之一（写入侧归一过），
        //    所以 IN 列表与内存里的 `level_rank(item.level) >= rank` 完全等价。
        if let Some(rank) = query
            .level
            .as_deref()
            .map(|value| value.trim().to_lowercase())
            .filter(|value| LEVELS.contains(&value.as_str()))
            .map(|value| level_rank(&value))
        {
            let included: Vec<&str> = LEVELS[rank..].to_vec();
            let placeholders = vec!["?"; included.len()].join(", ");
            fragments.push(format!("level IN ({placeholders})"));
            for level in included {
                binds.push(SqlValue::Text(level.to_string()));
            }
        }

        // ② 分类：同样要求取值是已知分类才过滤
        if let Some(want) = query
            .category
            .as_deref()
            .map(|value| value.trim().to_lowercase())
            .filter(|value| CATEGORIES.iter().any(|(key, _)| *key == *value))
        {
            fragments.push("category = ?".to_string());
            binds.push(SqlValue::Text(want));
        }

        // ③ 起始 id：`id > since_id`（未读水位的查询靠它）
        if let Some(from) = query.since_id {
            // id 是库里的正整数主键；`from` 超出 i64 时不可能有行满足
            // `id > from`，夹到 i64::MAX 即可 —— SQL 里自然查不到行，
            // 与旧实现在 u64 上比较的结果一致（不会溢出成负数而误命中）
            fragments.push("id > ?".to_string());
            binds.push(SqlValue::Integer(from.min(i64::MAX as u64) as i64));
        }

        // ④ / ⑤ 时间区间：闭开 [start, end)
        if let Some(from) = query.start {
            fragments.push("ts >= ?".to_string());
            binds.push(SqlValue::Integer(from));
        }
        if let Some(to) = query.end {
            fragments.push("ts < ?".to_string());
            binds.push(SqlValue::Integer(to));
        }

        let where_sql = if fragments.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", fragments.join(" AND "))
        };

        // ⑥ 关键词：见类型头部的说明，只能在 Rust 侧过滤
        let keyword = query
            .keyword
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_lowercase);

        Self { where_sql, binds, keyword }
    }

    /// 这条消息是否命中关键词（`None` = 该条件不生效，恒真）。
    ///
    /// 与旧实现逐字相同：`message.to_lowercase().contains(&关键词小写形态)`。
    fn matches_keyword(&self, message: &str) -> bool {
        match &self.keyword {
            Some(want) => message.to_lowercase().contains(want),
            None => true,
        }
    }
}

/// 命中筛选的日志，按 id **降序**（最新在前）。
///
/// ── 为什么不在 SQL 里 LIMIT ─────────────────────────────────
/// 两个原因，都是「与旧实现逐字一致」要求的：
///   1. 关键词只能在 Rust 侧过滤（`FilterPlan` 的类型注释）；
///   2. `QueryResult.matched` 要的是**命中总数**，与 limit 无关 —— 本来就得把
///      命中行全数出来。
/// 表上限 `MAX_ENTRIES = 500` 行，全量取出后由调用方切片。旧实现在内存里
/// 也是对 500 条跑一遍过滤链，扫描量没有变化。
pub(super) fn select_matching_desc(
    conn: &Connection,
    plan: &FilterPlan,
) -> rusqlite::Result<Vec<LogEntry>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM logs{} ORDER BY id DESC",
        plan.where_sql
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params_from_iter(plan.binds.iter()))?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let entry = decode_row(row)?;
        if plan.matches_keyword(&entry.message) {
            out.push(entry);
        }
    }
    Ok(out)
}

// ─── 写入与删除 ─────────────────────────────────────────────

/// 插入一条日志（id 由调用方先用 [`allocate_next_id`] 取好）。
pub(super) fn insert(conn: &Connection, entry: &LogEntry) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO logs (id, ts, level, category, message, data) VALUES (?, ?, ?, ?, ?, ?)",
        params![
            entry.id as i64,
            entry.ts,
            entry.level,
            entry.category,
            entry.message,
            encode_data(entry.data.as_ref()),
        ],
    )?;
    Ok(())
}

/// 取下一个可用 id，并把 `kv` 里的高水位推进到 `id + 1`。
///
/// `max(计数器, MAX(id) + 1)` 的由来见模块头：计数器是权威的单调水位，
/// 与「当前最大行」取较大者是为了在计数器丢失/被手工改过/从旧库升级上来时
/// **自愈**（否则会撞主键，那条日志就丢了）。空表时两者都得 1。
pub(super) fn allocate_next_id(conn: &Connection) -> rusqlite::Result<u64> {
    let stored: Option<String> = conn
        .query_row(
            "SELECT value FROM kv WHERE key = ?1",
            params![NEXT_ID_KEY],
            |row| row.get(0),
        )
        .optional()?;
    let stored = stored
        .as_deref()
        .map(str::trim)
        .and_then(|text| text.parse::<u64>().ok());
    let next = stored.unwrap_or(1).max(max_id(conn)? + 1).max(1);
    set_next_id(conn, next + 1)?;
    Ok(next)
}

/// 写 `kv` 的高水位（幂等 upsert）
fn set_next_id(conn: &Connection, value: u64) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO kv (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![NEXT_ID_KEY, value.to_string()],
    )?;
    Ok(())
}

/// 把高水位重置到 1（只有 `clear()` 用）。
///
/// `clear()` 的语义是「id 重新从 1 数起」，前端有配套分支（`app.js`：
/// `lastId < 水位` ⇒ 认定被清空、水位回落），所以这里必须真的回退 ——
/// 与 `clear_where`（只删行、**不回退**）是有意的两种行为。
pub(super) fn reset_next_id(conn: &Connection) -> rusqlite::Result<()> {
    set_next_id(conn, 1)
}

/// 把高水位推到至少 `value`（旧文件导入后调用，保证后续 id 不与历史冲突）
pub(super) fn raise_next_id(conn: &Connection, value: u64) -> rusqlite::Result<()> {
    let stored: Option<String> = conn
        .query_row(
            "SELECT value FROM kv WHERE key = ?1",
            params![NEXT_ID_KEY],
            |row| row.get(0),
        )
        .optional()?;
    let stored = stored
        .as_deref()
        .map(str::trim)
        .and_then(|text| text.parse::<u64>().ok())
        .unwrap_or(1);
    if value > stored {
        set_next_id(conn, value)?;
    }
    Ok(())
}

/// 按 id 删除一批行，返回实际删掉的条数。
///
/// 占位符按 id 个数生成（不是把 id 拼进 SQL）—— 列表长度由命中行数决定，
/// 值本身永远走绑定参数。
pub(super) fn delete_by_ids(conn: &Connection, ids: &[u64]) -> rusqlite::Result<usize> {
    if ids.is_empty() {
        return Ok(0);
    }
    let placeholders = vec!["?"; ids.len()].join(", ");
    let sql = format!("DELETE FROM logs WHERE id IN ({placeholders})");
    let binds: Vec<SqlValue> = ids
        .iter()
        .map(|id| SqlValue::Integer(*id as i64))
        .collect();
    conn.execute(&sql, params_from_iter(binds.iter()))
}

/// 删掉 `ts < cutoff` 的行，返回删除条数。
///
/// `keep_id` 是「即使超期也必须留下」的那一条（`append` 传刚写入的 id）：
/// `append` 的契约是「返回写入的条目」，不能返回一条当场被删掉的记录。
/// 传 `None`（`prune` 的路径）表示不做例外。
pub(super) fn delete_expired(
    conn: &Connection,
    cutoff: i64,
    keep_id: Option<u64>,
) -> rusqlite::Result<usize> {
    match keep_id {
        Some(id) => conn.execute(
            "DELETE FROM logs WHERE ts < ?1 AND id <> ?2",
            params![cutoff, id as i64],
        ),
        None => conn.execute("DELETE FROM logs WHERE ts < ?1", params![cutoff]),
    }
}

/// 容量裁剪：只保留 id 最大的 `max` 条，返回删除条数。
///
/// 按 **id** 而不是 ts 取「最新 N 条」：`ts` 允许补写历史（`NewEntry::ts`），
/// 用它排序会裁掉真正的新日志。旧实现是 `entries.drain(0..溢出)`，
/// 而数组顺序就是 id 顺序，两者等价。
pub(super) fn trim_to_capacity(conn: &Connection, max: usize) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM logs WHERE id NOT IN (SELECT id FROM logs ORDER BY id DESC LIMIT ?1)",
        params![max as i64],
    )
}

/// 清空全部日志（**不动**高水位；`clear()` 另调 `reset_next_id`）
pub(super) fn delete_all(conn: &Connection) -> rusqlite::Result<usize> {
    conn.execute("DELETE FROM logs", [])
}
