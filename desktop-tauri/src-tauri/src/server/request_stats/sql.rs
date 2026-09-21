//! 请求统计两张表的**行级 SQL 访问层** —— 本模块里唯一出现 SQL 的地方。
//!
//! ── 为什么单独一层 ──────────────────────────────────────────
//! 改造前明细是内存里一个按 ts 升序的 `Vec`（`insert_sorted` 维护），聚合是
//! 一个 `BTreeMap<String, DailyEntry>`，两个文件各有自己的落盘策略（明细
//! 「追加为主 + 攒够 `COMPACT_STEP` 次才整文件重写」，聚合「变更满 50 次或
//! 距上次落盘超 60 秒才重写」）。那套「内存快照 + 落盘镜像」的双份状态整体
//! 消失之后：明细是一行 `INSERT`、聚合是一行 UPSERT，报表口径仍由
//! `report.rs` 的纯函数算 —— 本文件承接这些语句，上层（`request_stats.rs`）
//! 只做归一、降级与 API 形状。
//!
//! ── `row_id` 与 `id` 请务必分清（`db/schema.rs` 的同名注释是权威）──
//!   - `row_id`：`INTEGER PRIMARY KEY AUTOINCREMENT`，**物理行号**，排序用它。
//!     显式 AUTOINCREMENT 保证**不复用**（删掉最大行后新行不会拿到同一个号）——
//!     翻页游标一旦复用就会漏行 / 重行。
//!   - `id`：请求的关联键（与调试报文同值），**可以重复、可以是空串**，
//!     所以它既不是主键也不参与排序（同 ts 的次序靠 `row_id`）。
//! 旧实现的「明细按 ts 升序、同毫秒先写入的在前」对应 `ORDER BY ts, row_id`；
//! 「新在前」对应 `ORDER BY ts DESC, row_id DESC`（同 ts 时 row_id 大的就是后
//! 写入的那条，与原实现「升序数组反转」的次序逐位一致）。
//!
//! ── 过滤条件为什么能全部下推给 SQL ──────────────────────────
//! 这里没有「只能在 Rust 侧算」的条件：`model` 是精确匹配、`status` 是两列的
//! 复合判定、`start`/`end` 是数值区间，都能表达成 WHERE。`query_requests` 与
//! `clear_where` 共用同一份 [`FilterPlan`] ——「页面上筛出来的 N 条」与「清空
//! 删掉的那批」因此必然是同一个集合（事件日志那边的 keyword 不能下推是因为
//! SQLite 的 LIKE 只折叠 ASCII 大小写；本模块没有关键词过滤，也就没有那个约束）。
//!
//! ── 聚合行的读-改-写 ────────────────────────────────────────
//! `request_daily` 一天一行，三个 `*Stats` 列是 JSON 数组文本。累计它们必须
//! **先读出整行、在 Rust 里累加、再整行写回**：总量列与三个维度列是同一批
//! 字段，用 `UPDATE ... SET requests = requests + 1` 只推进总量、维度另算，
//! 会让「各维之和 = 当天总量」这条对账前提变成两处维护（见 `fold_into_daily`）。
//! 一天只有一行、三个数组通常几十个条目，读-改-写在**一个事务**里完成，代价可接受
//! （这是连接池 / 增量 UPDATE 都换不来的东西：口径只有一处）。
//!
//! ── 并发：本层不加锁 ────────────────────────────────────────
//! 所有函数取裸 `&Connection`，串行化由 `Db` 那把 Mutex 负责（上层每个公开方法
//! 都在**一次** `Db::with` / `with_mut` 调用里跑完，多语句操作用事务包住）。
//! **硬约束**：持这把锁期间绝不能再调 `logging::log` —— 日志要写同一个库，
//! `std::sync::Mutex` 不可重入，会当场死锁（`request_stats.rs` 模块头也记了这条）。

use std::collections::BTreeMap;

use rusqlite::types::Value as SqlValue;
use rusqlite::{params, params_from_iter, Connection};

use super::record::{
    AccountAccum, AttemptDetail, DailyEntry, ModelAccum, ProviderAccum, RequestEntry, RequestQuery,
    SensitiveHit,
};
use super::report::normalize_status_filter;

/// `requests` 的列顺序（所有 SELECT 都按这个顺序取，`decode_request` 依赖它；
/// 不写 `SELECT *` 是为了「加列时读侧要显式跟上」这件事在 diff 里可见）
///
/// 末尾两列（`attempt_details` / `sensitive_hits`）是 schema v2 新增的 JSON 文本列
/// （见 `db/schema.rs` 的 `V2_SCHEMA`）。它们必须排在**最后**：`decode_request`
/// 按序号取值，而「加列」这件事只有在末尾才不牵动前面的序号 —— 中间插一列会让
/// 所有后续字段的序号整体挪一位，那种改动在整个文件里看不出错，只会静默取错值。
const REQUEST_COLUMNS: &str = "id, ts, model, account_id, account_name, status, duration_ms, \
     first_response_ms, attempts, error, prompt_tokens, completion_tokens, total_tokens, \
     cache_read_tokens, provider, client_model, upstream_model, attempt_details, sensitive_hits";

/// `request_daily` 的列顺序（同上）
const DAILY_COLUMNS: &str = "date, requests, successful, tokens, cache_hit_tokens, \
     cache_input_tokens, model_tokens, provider_stats, account_stats";

// ─── 行解码 ─────────────────────────────────────────────────

/// 行 → `RequestEntry`。
///
/// 列与字段一一对应，**不做任何归一**（不重新夹数值、不重算 `is_success`）：
/// 库里的值都是写入侧 `NewRequestEntry::normalize` 归一过的，读侧再归一既多余，
/// 也会掩盖「有人绕过写入侧直接改库」这件事。可空的两列（`error` /
/// `first_response_ms`）原样读成 `Option` ——「没有错误」与「空串错误」在写入侧
/// 已经收敛成同一个 `None`，读侧不必再分。
///
/// 末尾两个 JSON 列走 [`decode_json_list`]：坏值退化成空表而不是让整条明细读不出来
/// （理由见那个函数）。
fn decode_request(row: &rusqlite::Row<'_>) -> rusqlite::Result<RequestEntry> {
    let attempt_details: String = row.get(17)?;
    let sensitive_hits: String = row.get(18)?;
    Ok(RequestEntry {
        id: row.get(0)?,
        ts: row.get(1)?,
        model: row.get(2)?,
        account_id: row.get(3)?,
        account_name: row.get(4)?,
        status: row.get(5)?,
        duration_ms: row.get(6)?,
        first_response_ms: row.get(7)?,
        attempts: row.get(8)?,
        error: row.get(9)?,
        prompt_tokens: row.get(10)?,
        completion_tokens: row.get(11)?,
        total_tokens: row.get(12)?,
        cache_read_tokens: row.get(13)?,
        provider: row.get(14)?,
        client_model: row.get(15)?,
        upstream_model: row.get(16)?,
        attempt_details: decode_json_list::<AttemptDetail>(&attempt_details),
        sensitive_hits: decode_json_list::<SensitiveHit>(&sensitive_hits),
    })
}

/// JSON 数组文本 → `Vec<T>`（明细行的两个附属列）。解析失败或内容不是数组时
/// 退化成空表。
///
/// 与 `decode_accum`（聚合行那三个列）同一取向：这两个列是**补充信息**，
/// 一个坏值不该让整条请求明细读不出来 —— 退化成空表只是少了重试链 / 命中表，
/// 而状态、模型、用量、错误全都还在。反过来，若在这里报错，一条手改坏的
/// JSON 就能让请求日志整页拉不出来（`select_page_desc` 会因为一个 `?` 直接失败）。
fn decode_json_list<T: serde::de::DeserializeOwned>(text: &str) -> Vec<T> {
    serde_json::from_str(text).unwrap_or_default()
}

/// JSON 数组 ← `Vec<T>`。序列化失败给 `'[]'`（与 DDL 的默认值同一形态，
/// 于是「这一列没有数据」在库里只有一种表示）。
fn encode_json_list<T: serde::Serialize>(items: &[T]) -> String {
    serde_json::to_string(items).unwrap_or_else(|_| "[]".to_string())
}

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

// ─── 过滤条件 → SQL ─────────────────────────────────────────

/// 一次查询的**筛选计划**：WHERE 片段 + 绑定值。
///
/// `query_requests`（分页查）与 `clear_where`（按条件删 + 重算）都先用它编译条件 ——
/// 旧实现是两处共用同一个 `matches_filter` 函数，现在共用的层次更靠下：连 SQL 的
/// WHERE 片段都是同一份，两处不可能漂移。
/// `offset` / `limit` 是分页参数、不是筛选条件，所以不在这里。
pub(super) struct FilterPlan {
    /// `WHERE ...` 片段；无条件时为空串（直接拼在 FROM 后面）
    where_sql: String,
    /// 与 `where_sql` 里的 `?` 一一对应的绑定值
    binds: Vec<SqlValue>,
}

impl FilterPlan {
    /// 按旧 `matches_filter` 的条件逐条编译（顺序无关，SQL 只做 AND 连接）。
    pub(super) fn of(filter: &RequestQuery) -> Self {
        let mut fragments: Vec<String> = Vec::new();
        let mut binds: Vec<SqlValue> = Vec::new();

        // ① 模型名精确匹配；空串（输入框清空）当没筛
        if let Some(want) = filter.model.as_deref().filter(|text| !text.is_empty()) {
            fragments.push("model = ?".to_string());
            binds.push(SqlValue::Text(want.to_string()));
        }

        // ② 成功 / 失败：与 `RequestEntry::is_success` **逐字等价**的复合条件。
        //
        // 成功的判据是「2xx **且**没有错误摘要」（两列合起来才算一个条件，见
        // record.rs 里那条注释：流式请求的 200 是响应头阶段就发出去的，之后
        // 上游断流只能靠 error 表达）。所以：
        //   成功 → `status >= 200 AND status < 300 AND error IS NULL`
        //   失败 → 上面整条的取反（**不是** `status NOT BETWEEN`，那会漏掉
        //          「2xx 但带错误摘要」这一类 —— 它们必须是失败）
        //
        // `error IS NULL` 与 `is_none()` 的等价性对**两种写入路径**都成立：
        //   - 运行期写入的行走 `normalize`，它把空串摘要收敛成 `None`
        //     （`error.filter(|text| !text.is_empty())`）→ 库里是 NULL；
        //   - 从旧文件导入的行**原样搬**（不过 `normalize`），旧文件里写了
        //     `"error": ""` 的行会落成空串。此时 SQL 判它是「有错误」（`IS NULL`
        //     为假）、Rust 的 `is_success()` 也判它有错误（`Some("")` 不是 `None`）
        //     —— 两边仍然一致。所以这个条件不需要额外的 `error <> ''` 分支。
        if let Some(want_ok) = normalize_status_filter(filter.status.as_deref()) {
            let success = "status >= 200 AND status < 300 AND error IS NULL";
            fragments.push(if want_ok {
                format!("({success})")
            } else {
                format!("NOT ({success})")
            });
        }

        // ③ / ④ 时间区间：闭开 [start, end)。`end` 是开区间，与分页口径一致
        //（上一页最后一条的 ts 可以直接当下一页的 end，不会重复取到同一条）。
        if let Some(from) = filter.start {
            fragments.push("ts >= ?".to_string());
            binds.push(SqlValue::Integer(from));
        }
        if let Some(to) = filter.end {
            fragments.push("ts < ?".to_string());
            binds.push(SqlValue::Integer(to));
        }

        let where_sql = if fragments.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", fragments.join(" AND "))
        };
        Self { where_sql, binds }
    }
}

// ─── 明细：计数与读取 ───────────────────────────────────────

/// 明细总条数（`QueryResult.total` 与容量判定都用它）
pub(super) fn count_all(conn: &Connection) -> rusqlite::Result<usize> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM requests", [], |row| row.get(0))?;
    Ok(count.max(0) as usize)
}

/// 命中筛选的条数（`QueryResult.matched`）—— 与 `limit` 无关，
/// 前端据此算总页数（见 `ui/requests-panel.js` 的 `renderPager`）
pub(super) fn count_matching(
    conn: &Connection,
    plan: &FilterPlan,
) -> rusqlite::Result<usize> {
    let sql = format!("SELECT COUNT(*) FROM requests{}", plan.where_sql);
    let count: i64 = conn.query_row(&sql, params_from_iter(plan.binds.iter()), |row| row.get(0))?;
    Ok(count.max(0) as usize)
}

/// 最早 / 最晚的明细时间戳（空表为 `None`）—— `stats()` 的 `firstTs` / `lastTs`。
///
/// 旧实现取的是内存数组（恒按 ts 升序）的首尾元素的 ts，与 `MIN` / `MAX` 等价。
pub(super) fn min_ts(conn: &Connection) -> rusqlite::Result<Option<i64>> {
    conn.query_row("SELECT MIN(ts) FROM requests", [], |row| row.get(0))
}

pub(super) fn max_ts(conn: &Connection) -> rusqlite::Result<Option<i64>> {
    conn.query_row("SELECT MAX(ts) FROM requests", [], |row| row.get(0))
}

/// 分页取明细，**新在前**（`ORDER BY ts DESC, row_id DESC`）。
///
/// 旧实现是「升序数组反转后 `skip(offset).take(limit)`」：反转后同 ts 的次序
/// 是后写入的在前，正是 `row_id DESC` 的效果。`offset` 从 0 起（路由层的
/// 解析保证非负）。
pub(super) fn select_page_desc(
    conn: &Connection,
    plan: &FilterPlan,
    offset: usize,
    limit: usize,
) -> rusqlite::Result<Vec<RequestEntry>> {
    let sql = format!(
        "SELECT {REQUEST_COLUMNS} FROM requests{} ORDER BY ts DESC, row_id DESC LIMIT ? OFFSET ?",
        plan.where_sql
    );
    let mut binds = plan.binds.clone();
    binds.push(SqlValue::Integer(limit as i64));
    binds.push(SqlValue::Integer(offset as i64));
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params_from_iter(binds.iter()))?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(decode_request(row)?);
    }
    Ok(out)
}

/// 取区间 `[from_ms, to_ms]` 内的明细，按**写入顺序**（ts 升序、同 ts 按 row_id）。
///
/// 两个用途，都是「要按时间顺序逐条过一遍」的场景：
///   - 报表的缓存窗口（10 分钟 / 1 小时 / 24 小时 / 7 天）与近 24 小时趋势；
///   - `clear_where` 重算某天聚合时取那一天的剩余明细。
/// 顺序取升序（而不是报表本身需要的顺序）是为了让重算与 `record` 的累加次序
/// 一致 —— 三个维度数组里条目的先后只影响视觉，但没必要制造差异。
pub(super) fn select_between(
    conn: &Connection,
    from_ms: i64,
    to_ms: i64,
) -> rusqlite::Result<Vec<RequestEntry>> {
    let sql = format!(
        "SELECT {REQUEST_COLUMNS} FROM requests WHERE ts >= ?1 AND ts <= ?2 \
         ORDER BY ts ASC, row_id ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![from_ms, to_ms])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(decode_request(row)?);
    }
    Ok(out)
}

/// 命中筛选的时间戳（`clear_where` 用它反推「哪些天的聚合要重算」）。
///
/// 为什么不在 SQL 里 `SELECT DISTINCT date(ts)`：SQLite 的 `date()` 默认按
/// **UTC** 切分，而本模块的「一天」是**本地时区**的自然日（`clock::day_of` 是
/// 全模块唯一的切分口径，`schema.rs` 也明确写了聚合表的 date 是本地时区）。
/// 用 SQL 的 date() 会让 UTC+8 的凌晨落进前一天，重算的日期集合整体偏一格。
/// 所以这里只取回数值，日期键一律在 Rust 侧用 `date_key(day_of(_))` 算。
pub(super) fn select_matching_ts(
    conn: &Connection,
    plan: &FilterPlan,
) -> rusqlite::Result<Vec<i64>> {
    let sql = format!("SELECT ts FROM requests{}", plan.where_sql);
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params_from_iter(plan.binds.iter()))?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(row.get(0)?);
    }
    Ok(out)
}

/// 插入一条明细（`row_id` 由库自增，调用方不必也不该给）
///
/// 末尾两个 JSON 列由 [`encode_json_list`] 编码；空表写成 `'[]'`，与 DDL 的
/// 默认值同形（于是「没有数据」在库里只有一种表示，读侧不必分「NULL 还是空数组」）。
pub(super) fn insert_request(conn: &Connection, entry: &RequestEntry) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO requests (id, ts, model, account_id, account_name, status, duration_ms, \
         first_response_ms, attempts, error, prompt_tokens, completion_tokens, total_tokens, \
         cache_read_tokens, provider, client_model, upstream_model, attempt_details, \
         sensitive_hits) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, \
         ?18, ?19)",
        params![
            entry.id,
            entry.ts,
            entry.model,
            entry.account_id,
            entry.account_name,
            entry.status,
            entry.duration_ms,
            entry.first_response_ms,
            entry.attempts,
            entry.error,
            entry.prompt_tokens,
            entry.completion_tokens,
            entry.total_tokens,
            entry.cache_read_tokens,
            entry.provider,
            entry.client_model,
            entry.upstream_model,
            encode_json_list(&entry.attempt_details),
            encode_json_list(&entry.sensitive_hits),
        ],
    )?;
    Ok(())
}

// ─── 明细：删除与裁剪 ───────────────────────────────────────

/// 删除命中筛选的明细，返回删除条数（`clear_where` 的 `removed`）
pub(super) fn delete_matching(
    conn: &Connection,
    plan: &FilterPlan,
) -> rusqlite::Result<usize> {
    let sql = format!("DELETE FROM requests{}", plan.where_sql);
    conn.execute(&sql, params_from_iter(plan.binds.iter()))
}

/// 清空全部明细（`clear()` 用）
pub(super) fn delete_all_requests(conn: &Connection) -> rusqlite::Result<usize> {
    conn.execute("DELETE FROM requests", [])
}

/// 删掉 `ts < cutoff` 的明细（时间维度保留）
pub(super) fn delete_expired_requests(conn: &Connection, cutoff: i64) -> rusqlite::Result<usize> {
    conn.execute("DELETE FROM requests WHERE ts < ?1", params![cutoff])
}

/// 容量裁剪：只保留 **ts 最新的** `max` 条，返回删除条数。
///
/// 按 ts 而不是 row_id 取「最新」：旧实现是「升序数组从头部 drain 掉溢出部分」，
/// 头部就是 ts 最小的那些；`ts` 允许补写历史（记账点传入的 ts 可能偏早），
/// 按 row_id 裁会把刚补写的旧请求当成最新而保住它、反而裁掉真正的新请求。
///
/// ── 为什么先判条数再发 DELETE（这是热路径）────────────────────
/// `record` 每次记账都调它，而上面那条 DELETE 在**不需要裁剪时也不是空转**：
/// `NOT IN (子查询)` 要让 SQLite 把子查询结果物化出来再逐行比对（子查询的
/// `ORDER BY ts DESC` 能走 `idx_requests_ts`，但仍要取回最多 `max` 个 row_id 建表）。
/// 条数未达上限时这次比对必然一行都不删，纯属白烧 —— 而记账是每个请求一次的路径。
/// 所以先 `COUNT(*)`（走最小的索引，比物化子查询便宜得多），未超限直接返回 0。
/// **与改造前的判定同形**：旧 `insert_sorted` 也是 `if entries.len() > MAX_ENTRIES`
/// 才 drain。
pub(super) fn trim_requests_capacity(conn: &Connection, max: usize) -> rusqlite::Result<usize> {
    if count_all(conn)? <= max {
        return Ok(0);
    }
    conn.execute(
        "DELETE FROM requests WHERE row_id NOT IN \
         (SELECT row_id FROM requests ORDER BY ts DESC, row_id DESC LIMIT ?1)",
        params![max as i64],
    )
}

// ─── 聚合：读-改-写 ─────────────────────────────────────────

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

/// 删掉某一天的聚合行（`clear_where` 重算后发现那天一条明细都不剩）
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

/// 只保留日期最新的 `max` 行（先判行数再删，理由见 [`trim_requests_capacity`]）
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
