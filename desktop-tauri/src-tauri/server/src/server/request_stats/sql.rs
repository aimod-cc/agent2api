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
//! ── request_raw 表与「进行中」行（本次新增的两件事）────────────
//!   - `request_raw`：下游原始正文（预览对话的地基），与 requests 同 id 关联。
//!     它的写入（`upsert_raw`）由上层在明细记账后单独调；**删除必须挂在
//!     requests 明细的每一个删除点之后/之前同步做**（见各删除函数的注释），
//!     否则明细没了正文还在，既占容量闸的名额又永远读不出来。
//!   - status=0 的行：转发开始前由 `insert_started_request` 先插的「进行中」
//!     行，收尾时由 `update_running_request` 补全终态字段（先 UPDATE 后
//!     INSERT 的理由见 `update_running_request`）。这类行**不进日报**
//!     （`fold_into_daily` 按 status=0 跳过），且会被 `delete_stale_running`
//!     按时间清理（网关崩溃留下的僵尸行）。
//!
//! ── 过滤条件为什么能全部下推给 SQL ──────────────────────────
//! 这里没有「只能在 Rust 侧算」的条件：`model` 是精确匹配、`status` 是两列的
//! 复合判定、`start`/`end` 是数值区间，都能表达成 WHERE。`query_requests` 与
//! 清理（`clear_where` / `clear_raw_where` / 预览计数）共用同一份
//! [`FilterPlan`] ——「页面上筛出来的 N 条」与「清空删掉的那批」因此必然是
//! 同一个集合（事件日志那边的 keyword 不能下推是因为 SQLite 的 LIKE 只折叠
//! ASCII 大小写；本模块没有关键词过滤，也就没有那个约束）。
//!
//! ── 聚合行的读-改-写 ────────────────────────────────────────
//! `request_daily` 一天一行，三个 `*Stats` 列是 JSON 数组文本。累计它们必须
//! **先读出整行、在 Rust 里累加、再整行写回**：总量列与三个维度列是同一批
//! 字段，用 `UPDATE ... SET requests = requests + 1` 只推进总量、维度另算，
//! 会让「各维之和 = 当天总量」这条对账前提变成两处维护（见 `fold_into_daily`）。
//! 一天只有一行、三个数组通常几十个条目，读-改-写在**一个事务**里完成，代价可接受
//! （这是连接池 / 增量 UPDATE 都换不来的东西：口径只有一处）。
//! 这一支的语句（含列常量与编解码）住在 `daily.rs` —— 两张表分文件后各自
//! 演化，本文件专注 `requests` 明细表与跨表共用的 [`FilterPlan`]。
//!
//! ── 并发：本层不加锁 ────────────────────────────────────────
//! 所有函数取裸 `&Connection`，串行化由 `Db` 那把 Mutex 负责（上层每个公开方法
//! 都在**一次** `Db::with` / `with_mut` 调用里跑完，多语句操作用事务包住）。
//! **硬约束**：持这把锁期间绝不能再调 `logging::log` —— 日志要写同一个库，
//! `std::sync::Mutex` 不可重入，会当场死锁（`request_stats.rs` 模块头也记了这条）。

use rusqlite::types::Value as SqlValue;
use rusqlite::{params, params_from_iter, Connection};

use super::record::{AttemptDetail, RequestEntry, RequestQuery, RunningProgress, SensitiveHit};
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

// `request_daily`（按天聚合）那一支的列常量、编解码与读-改-写语句在
// `daily.rs` —— 两张表的语句分文件后各自独立演化。

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
/// 与 `decode_accum`（聚合行那三个列，已拆到 `daily.rs`）同一取向：这两个列是
/// **补充信息**，一个坏值不该让整条明细读不出来 —— 退化成空表只是少了重试链 /
/// 命中表，而状态、模型、用量、错误全都还在。反过来，若在这里报错，一条手改坏的
/// JSON 就能让请求日志整页拉不出来（`select_page_desc` 会因为一个 `?` 直接失败）。
fn decode_json_list<T: serde::de::DeserializeOwned>(text: &str) -> Vec<T> {
    serde_json::from_str(text).unwrap_or_default()
}

/// JSON 数组文本 ← `Vec<T>`。序列化失败给 `'[]'`（与 DDL 的默认值同一形态，
/// 于是「这一列没有数据」在库里只有一种表示）。
fn encode_json_list<T: serde::Serialize>(items: &[T]) -> String {
    serde_json::to_string(items).unwrap_or_else(|_| "[]".to_string())
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

        // ① 模型名精确匹配；空串（输入框清空）当没筛。
        //    匹配**两个**名字列：`model`（请求侧解析名）与 `upstream_model`
        //    （实际发给上游的名字）。报表的模型维度按上游真名聚合（见
        //    `fold_into_daily` 的 `model_stat_key`），而明细的 `model` 列存的是
        //    请求侧解析名 —— 只匹配一列时，映射请求在报表里显示的真名
        //    （只落在 upstream_model 列）会筛出 0 条。两个名字都该能筛到，
        //    条件因此取并集；与下拉候选（`select_model_options`）同一口径。
        if let Some(want) = filter.model.as_deref().filter(|text| !text.is_empty()) {
            fragments.push("(model = ? OR upstream_model = ?)".to_string());
            binds.push(SqlValue::Text(want.to_string()));
            binds.push(SqlValue::Text(want.to_string()));
        }

        // ①′ provider id 精确匹配（同一口径：空串当没筛）。
        //
        // 按 `provider` 列而不是「账号属于哪一家」判：那一列记的是**实际承载本次
        // 请求的那一家**（备援换号后是最后扛下来的那家），与列表里显示的提供商
        // 是同一个值 —— 筛选结果与肉眼看到的行必然一致。
        // 空 id 的行（一次都没发出去就失败）不会被任何非空 id 命中，这是有意的：
        // 它们不属于任何一家，用 status=error 看更直接（见 RequestQuery::provider）。
        if let Some(want) = filter.provider.as_deref().filter(|text| !text.is_empty()) {
            fragments.push("provider = ?".to_string());
            binds.push(SqlValue::Text(want.to_string()));
        }

        // ② 成功 / 失败 / 进行中：与 `RequestEntry::is_success` **逐字等价**的
        // 复合条件（成功与失败），加上进行中行的专属条件。
        //
        // 成功的判据是「2xx **且**没有错误摘要」（两列合起来才算一个条件，见
        // record.rs 里那条注释：流式请求的 200 是响应头阶段就发出去的，之后
        // 上游断流只能靠 error 表达）。所以：
        //   成功   → `status >= 200 AND status < 300 AND error IS NULL`
        //   失败   → 上面整条的取反（**不是** `status NOT BETWEEN`，那会漏掉
        //            「2xx 但带错误摘要」这一类 —— 它们必须是失败）
        //   进行中 → `status = 0`（转发开始时插的行，还没收尾；见
        //            `insert_started_request`）
        //
        // 失败条件**额外排除** status=0：取反会把「还没收尾」的进行中行也判成
        // 失败 —— 那是种草效应最差的一类误报（用户看到一排失败，其实只是
        // 还在跑）。status=0 在旧数据里从未被用过（schema 注释），这个排除
        // 只影响新语义的行。
        //
        // `error IS NULL` 与 `is_none()` 的等价性对**两种写入路径**都成立：
        //   - 运行期写入的行走 `normalize`，它把空串摘要收敛成 `None`
        //     （`error.filter(|text| !text.is_empty())`）→ 库里是 NULL；
        //   - 从旧文件导入的行**原样搬**（不过 `normalize`），旧文件里写了
        //     `"error": ""` 的行会落成空串。此时 SQL 判它是「有错误」（`IS NULL`
        //     为假）、Rust 的 `is_success()` 也判它有错误（`Some("")` 不是 `None`）
        //     —— 两边仍然一致。所以这个条件不需要额外的 `error <> ''` 分支。
        if let Some(want) = normalize_status_filter(filter.status.as_deref()) {
            let success = "status >= 200 AND status < 300 AND error IS NULL";
            let fragment = match want {
                super::report::StatusFilter::Ok => format!("({success})"),
                super::report::StatusFilter::Error => format!("(NOT ({success}) AND status <> 0)"),
                super::report::StatusFilter::Running => "status = 0".to_string(),
            };
            fragments.push(fragment);
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

/// 命中筛选条件的**进行中**行数（`QueryResult.running`）。
///
/// 一条 COUNT：在筛选计划上追加 `status = 0` —— 与列表共用同一份 WHERE，
/// 「这批筛选里还有几条没跑完」才是列表页徽标要的口径。
pub(super) fn count_running(
    conn: &Connection,
    plan: &FilterPlan,
) -> rusqlite::Result<usize> {
    let sql = if plan.where_sql.is_empty() {
        "SELECT COUNT(*) FROM requests WHERE status = 0".to_string()
    } else {
        format!("SELECT COUNT(*) FROM requests{} AND status = 0", plan.where_sql)
    };
    let count: i64 = conn.query_row(&sql, params_from_iter(plan.binds.iter()), |row| row.get(0))?;
    Ok(count.max(0) as usize)
}

/// 筛选下拉的候选清单：明细里出现过的模型名与 provider id（各按出现次数降序）。
///
/// ── 为什么不按当前时间档位过滤 ──────────────────────────────
/// 清单回答的是「这份日志里出现过什么」，与界面上的时间档位是两个独立维度：
/// 跟着档位过滤的话，切一次档位就要重拉一次清单，而下拉的内容还会在用户正要选
/// 的时候整体换掉（选中项可能凭空消失）。全量清单稳定得多，代价是切到「今天」
/// 档位后可能列出一个只在 30 天前出现过的模型 —— 选中它得到空结果，
/// 而原因一眼可见（时间档位那一栏还写着「今天」）。
///
/// ── 上限只截断展示 ──────────────────────────────────────────
/// 下拉是给人扫读的，几百项已超出可读范围；而且这份清单按出现次数排序，
/// 尾部那些只出现一两次的值价值极低。截断的是**候选**，不是筛选能力：
/// 前端仍会把当前选中的值保留在列表里（见 ui/requests-panel.js 的
/// `fillFilterSelect`），所以「筛了某值 → 清单里没有它」不会发生。
pub(super) fn select_filter_options(
    conn: &Connection,
    max: usize,
) -> rusqlite::Result<(Vec<String>, Vec<String>)> {
    Ok((
        select_model_options(conn, max)?,
        select_distinct_ordered(conn, "provider", max)?,
    ))
}

/// 模型筛选候选：`model` 与 `upstream_model` 两列非空值的并集。
///
/// 与 [`FilterPlan`] 的模型条件同一口径（两个名字都该能筛到，理由见那边的
/// 注释）：候选清单若只取 `model` 列，报表按上游真名聚合后，用户拿着真名
/// 来明细里筛时下拉里没有那个选项 —— 正是「列表里有这一行、下拉里却没有
/// 这个选项」的不一致（见 `stats_request_filters` 的说明）。非映射请求两个
/// 名字相同，在并集里自然合成一项，计数按两列出现次数合计。
///
/// 不复用 [`select_distinct_ordered`]：那个函数按单列分组，这里是两列
/// `UNION ALL` 后再分组，硬套会让列名参数变成两段 SQL 拼接，可读性反而差。
fn select_model_options(conn: &Connection, max: usize) -> rusqlite::Result<Vec<String>> {
    let sql = "SELECT name FROM (
         SELECT model AS name FROM requests WHERE model <> ''
         UNION ALL
         SELECT upstream_model AS name FROM requests WHERE upstream_model <> ''
       ) GROUP BY name ORDER BY COUNT(*) DESC, name ASC LIMIT ?1";
    let mut stmt = conn.prepare(sql)?;
    let mut rows = stmt.query(params![max as i64])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(row.get(0)?);
    }
    Ok(out)
}

/// 取某一列的非空值，按出现次数降序、同次数按值升序（最多 `max` 个）。
///
/// 同次数时**必须**有第二个排序键：只按 `COUNT(*)` 排序时同次数的值顺序由
/// SQLite 的扫描顺序决定，两次调用可能给出不同的顺序 —— 下拉里的项会无理由地
/// 换位置（用户刚要点的那一项可能正好跳走）。
///
/// 列名由调用方以字面量给出（不接收任何用户输入），所以直接拼进 SQL 是安全的；
/// 上限值仍走绑定参数，与其它查询保持同一种写法。
fn select_distinct_ordered(
    conn: &Connection,
    column: &str,
    max: usize,
) -> rusqlite::Result<Vec<String>> {
    let sql = format!(
        "SELECT {column} FROM requests WHERE {column} <> '' \
         GROUP BY {column} ORDER BY COUNT(*) DESC, {column} ASC LIMIT ?1"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![max as i64])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(row.get(0)?);
    }
    Ok(out)
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
///   - `rebuild_day` 重算某天聚合时取那一天的剩余明细。
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

/// 插一条**进行中**的明细（status=0，转发开始前调用；见 `RequestStats::record_started`）。
///
/// 幂等：同 id 已有进行中行时跳过（重复调用不产生第二行 —— 收尾的
/// `update_running_request` 只按「id + status=0」匹配，多一行会让终态字段
/// 补到错误的行上）。「已有终态行」不算重复：同 id 多行记账是存量语义
/// （重试、去重排队），新一次转发仍该有自己的进行中行。
///
/// 只写四列，其余列走 DDL 的 DEFAULT：与「验收路径不预填字段」的取向一致 ——
/// 进行中行只承诺「这条请求开始了、它叫什么名字」，终态字段一律等收尾时补。
/// 返回是否真的插入了新行（调用方目前不需要区分，但测试与日志可能想看）。
pub(super) fn insert_started_request(
    conn: &Connection,
    id: &str,
    ts: i64,
    model: &str,
    client_model: &str,
) -> rusqlite::Result<bool> {
    let existing: i64 = conn.query_row(
        "SELECT COUNT(*) FROM requests WHERE id = ?1 AND status = 0",
        params![id],
        |row| row.get(0),
    )?;
    if existing > 0 {
        return Ok(false);
    }
    conn.execute(
        "INSERT INTO requests (id, ts, model, client_model, status) VALUES (?1, ?2, ?3, ?4, 0)",
        params![id, ts, model, client_model],
    )?;
    Ok(true)
}

/// 收尾**陈旧的进行中行**：status=0 且开始时刻早于 cutoff 的行没有机会再收到
/// 收尾 UPDATE（进程崩溃 / 流任务泄漏），把它就地补一个明确的失败终态。
///
/// 为什么是「补终态」而不是删除：界面上的进行中 → 结束是用户会一直盯着看的状态
/// 流转。删掉一行等于让请求凭空消失，用户会以为平台漏记；补 408 则如实回答
/// 「这次没跑完」。OmniProxy 的 `applyInterruptedLogs`（requestLogLifecycle.ts）
/// 走的是同一条语义。`duration_ms` 与 `ts` 一并补齐，避免列出 0 耗时的假象。
pub(super) fn finish_stale_running(
    conn: &Connection,
    cutoff: i64,
    now: i64,
) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE requests SET status = 408, error = '网关重启或流中断，请求未能完成', \
         duration_ms = MAX(?2 - ts, 0), attempts = 1, \
         attempt_details = '[]', sensitive_hits = '[]' \
         WHERE status = 0 AND ts < ?1",
        params![cutoff, now],
    )
}

/// 回写一条**进行中**明细的**在途**字段（转发期间每次状态真的变化时调一次；
/// 见 `RequestStats::update_running`）。
///
/// ── 匹配条件与收尾**完全一致**（`id + status = 0`）────────────
/// 收尾之后这一条必然打空 —— 那正是想要的：终态字段由收尾统一写，在途回写
/// 晚到一步不该把终态覆盖回去（把一个已经收尾的行改回「还在飞」的样子）。
/// 同一条请求写多少次都只是覆盖同一行的同几列，所以不需要幂等之外的判据。
///
/// ── 为什么不动这几列 ────────────────────────────────────────
///   - `status` / `error`：它们决定前端认不认这行是「进行中」（有 error 就不再是，
///     见 `ui/requests-panel.js` 的 isRunning）—— 转发中途的失败可能只是换号前的
///     一次尝试失败，不该让列表里的行提前变成失败；
///   - `ts` / `model` / `client_model`：请求发起时就定稿了，在途没有新值；
///   - `duration_ms` / 四个 token 列：只有收尾才有值（用量在途中不显示）。
pub(super) fn update_running_progress(
    conn: &Connection,
    id: &str,
    progress: &RunningProgress,
) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE requests SET provider = ?2, account_id = ?3, account_name = ?4, \
         upstream_model = ?5, attempts = ?6, first_response_ms = ?7, attempt_details = ?8, \
         sensitive_hits = ?9 WHERE id = ?1 AND status = 0",
        params![
            id,
            progress.provider,
            progress.account_id,
            progress.account_name,
            progress.upstream_model,
            progress.attempts,
            progress.first_response_ms,
            encode_json_list(&progress.attempt_details),
            encode_json_list(&progress.sensitive_hits),
        ],
    )
}

/// 收尾一条进行中的明细：按 `id + status=0` 匹配，**补齐全部终态字段**。
///
/// ── 为什么是「先 UPDATE 后 INSERT」而不是唯一索引 + UPSERT ──────
/// `requests.id` 上**不能**建唯一索引：存量数据里同 id 可能有多行
/// （重试 / 去重排队等路径产生的重复记账，见 schema 的 id 列注释），
/// 建索引会直接撞唯一约束、丢数据。而没有唯一约束就没有 UPSERT 的冲突目标
/// —— `ON CONFLICT(id) DO UPDATE` 在没有对应唯一索引时会报语法错误。
/// 所以收尾走两步：先 UPDATE 进行中行（有就补全），影响 0 行再 INSERT
/// （转发前就失败等没有进行中行的路径维持原样）。两步在同一个事务里，
/// 不存在「UPDATE 完、INSERT 前」被读到的中间态。
///
/// 更新**不动 row_id**：进行中行已经占住的物理行号不变，列表顺序稳定
/// （行在「进行中」期间就出现在页面上，收尾后不该跳到别处）。`id` 为空的行
/// 直接返回 0（不 UPDATE 任何行）：空 id 的 started 行不存在，早期失败路径
/// 也没有 id 可匹配 —— 交给 INSERT。
pub(super) fn update_running_request(
    conn: &Connection,
    entry: &RequestEntry,
) -> rusqlite::Result<usize> {
    if entry.id.is_empty() {
        return Ok(0);
    }
    conn.execute(
        "UPDATE requests SET ts = ?2, model = ?3, account_id = ?4, account_name = ?5, status = ?6, \
         duration_ms = ?7, first_response_ms = ?8, attempts = ?9, error = ?10, prompt_tokens = ?11, \
         completion_tokens = ?12, total_tokens = ?13, cache_read_tokens = ?14, provider = ?15, \
         client_model = ?16, upstream_model = ?17, attempt_details = ?18, sensitive_hits = ?19 \
         WHERE id = ?1 AND status = 0",
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
    )
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

/// 删除命中筛选的明细，返回删除条数（`clear_where` 的 `removed`）。
///
/// **调用方必须先删对应的 request_raw 行**（`delete_raw_matching`，同一份
/// 筛选计划）—— 明细删完就再也找不到要陪葬的正文 id 了。
pub(super) fn delete_matching(
    conn: &Connection,
    plan: &FilterPlan,
) -> rusqlite::Result<usize> {
    let sql = format!("DELETE FROM requests{}", plan.where_sql);
    conn.execute(&sql, params_from_iter(plan.binds.iter()))
}

/// 按筛选条件删除 request_raw 中「命中的明细 id」对应的正文行。
///
/// 同一个函数服务两个调用方，语义不同：
///   - `mode=raw` 清理：它就是**全部动作**（只删正文，明细与日报不动）；
///   - `mode=all` 带筛选的清理：它在 `delete_matching` **之前**跑（先删正文
///     再删明细，顺序不能反）。
///
/// `IN (SELECT id FROM requests …)` 直接下推给 SQLite：不把命中 id 拉回
/// Rust 再逐个绑定（筛选命中的可能是几千行，两万参数的 SQL 既慢又难看）。
pub(super) fn delete_raw_matching(
    conn: &Connection,
    plan: &FilterPlan,
) -> rusqlite::Result<usize> {
    let sql = format!(
        "DELETE FROM request_raw WHERE id IN (SELECT id FROM requests{})",
        plan.where_sql
    );
    conn.execute(&sql, params_from_iter(plan.binds.iter()))
}

/// 清空全部明细（`clear()` 用）。调用方必须同时清 request_raw（`delete_all_raw`）。
pub(super) fn delete_all_requests(conn: &Connection) -> rusqlite::Result<usize> {
    conn.execute("DELETE FROM requests", [])
}

/// 删掉 `ts < cutoff` 的明细（时间维度保留）。
///
/// **先删**这批明细对应的 request_raw 行再删明细本身（顺序理由同上）：
/// 过期行通常为 0～少量，`ts` 索引让子查询的代价可以忽略 —— 记账热路径上
/// 多一条带子查询的 DELETE，与原有的逐次裁剪同量级。
pub(super) fn delete_expired_requests(conn: &Connection, cutoff: i64) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM request_raw WHERE id IN \
         (SELECT id FROM requests WHERE ts < ?1 AND id <> '')",
        params![cutoff],
    )?;
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
///
/// ── request_raw 的同步删除（淘汰点接上）──────────────────────
/// 超限时**先**删将被淘汰明细的正文、再删明细：正文与明细同 id 关联，
/// 明细一删，`id IN (SELECT ...)` 就再也找不到要删的正文行了。子查询与
/// 下面的明细 DELETE 用同一个「保留集合」，两步必然删中同一批 id。
/// 同 id 多行的边缘情形（淘汰其一、留下其一）：正文只有一行（UPSERT 覆盖），
/// 删掉它会让留下的那条明细失去详情正文 —— 两个明细行共享一个 id 本来就是
/// 模糊地带，这里取「正文跟着被淘汰的行走」，不再为它单独记状态。
pub(super) fn trim_requests_capacity(conn: &Connection, max: usize) -> rusqlite::Result<usize> {
    if count_all(conn)? <= max {
        return Ok(0);
    }
    conn.execute(
        "DELETE FROM request_raw WHERE id IN (\
           SELECT id FROM requests WHERE row_id NOT IN \
           (SELECT row_id FROM requests ORDER BY ts DESC, row_id DESC LIMIT ?1) AND id <> '')",
        params![max as i64],
    )?;
    conn.execute(
        "DELETE FROM requests WHERE row_id NOT IN \
         (SELECT row_id FROM requests ORDER BY ts DESC, row_id DESC LIMIT ?1)",
        params![max as i64],
    )
}

// ─── 原始报文：request_raw ──────────────────────────────────

/// 写一条原始正文（存在即覆盖；`id` 为空由调用方拦下）。
///
/// `size` 是**两侧字节长度合计**（截断后的落库体积，与 debug_traffic 的
/// size 口径同理：闸门守的是「这张表实际占多大」）。UPSERT 而不是先查后写：
/// `id` 是主键，`ON CONFLICT` 一条语句完成「新建 / 覆盖」，同 id 重写
/// （理论上不该发生，防手改库与重复请求）不会撞约束。
pub(super) fn upsert_raw(
    conn: &Connection,
    id: &str,
    ts: i64,
    request_body: &str,
    response_body: &str,
    size: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO request_raw (id, ts, request_body, response_body, size) \
         VALUES (?1, ?2, ?3, ?4, ?5) \
         ON CONFLICT(id) DO UPDATE SET ts = excluded.ts, request_body = excluded.request_body, \
         response_body = excluded.response_body, size = excluded.size",
        params![id, ts, request_body, response_body, size],
    )?;
    Ok(())
}

/// 按 id 取一条原始正文（详情弹窗用）。无行返回 None（调用方给 404）。
pub(super) fn select_raw(
    conn: &Connection,
    id: &str,
) -> rusqlite::Result<Option<(String, String, String)>> {
    let mut stmt = conn.prepare("SELECT id, request_body, response_body FROM request_raw WHERE id = ?1")?;
    let mut rows = stmt.query(params![id])?;
    match rows.next()? {
        Some(row) => Ok(Some((row.get(0)?, row.get(1)?, row.get(2)?))),
        None => Ok(None),
    }
}

/// request_raw 行数（容量判定）
pub(super) fn count_raw(conn: &Connection) -> rusqlite::Result<usize> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM request_raw", [], |row| row.get(0))?;
    Ok(count.max(0) as usize)
}

/// 容量裁剪：只保留 **ts 最新的** `max` 行（先判条数再删，理由同
/// [`trim_requests_capacity`]）。
///
/// 按 ts 丢最旧：正文的价值随时间衰减（「预览最近的对话」），明细行的
/// 淘汰另有自己的口径 —— 两边各自守闸，孤儿行（明细已删、正文还在）由
/// 各删除点的同步清理兜住。
pub(super) fn trim_raw_capacity(conn: &Connection, max: usize) -> rusqlite::Result<usize> {
    if count_raw(conn)? <= max {
        return Ok(0);
    }
    conn.execute(
        "DELETE FROM request_raw WHERE id NOT IN \
         (SELECT id FROM request_raw ORDER BY ts DESC LIMIT ?1)",
        params![max as i64],
    )
}

/// 命中筛选的明细里**仍带正文**的条数（清理预览的 `raw`）。
///
/// 「带正文」= request_raw 里有对应行 —— 表的约定是「有行 ⇔ 有正文」
/// （写入侧两侧全空时不写行），所以不需要逐列判空。
pub(super) fn count_raw_matching(
    conn: &Connection,
    plan: &FilterPlan,
) -> rusqlite::Result<usize> {
    let sql = format!(
        "SELECT COUNT(*) FROM request_raw WHERE id IN (SELECT id FROM requests{})",
        plan.where_sql
    );
    let count: i64 = conn.query_row(&sql, params_from_iter(plan.binds.iter()), |row| row.get(0))?;
    Ok(count.max(0) as usize)
}

/// 清空全部原始正文（`clear()` / `mode=all` 全清时与明细一起清）
pub(super) fn delete_all_raw(conn: &Connection) -> rusqlite::Result<usize> {
    conn.execute("DELETE FROM request_raw", [])
}

// ─── 聚合：读-改-写 ─────────────────────────────────────────
//
// `request_daily` 的全部语句已拆到 `daily.rs`（列常量、编解码与
// select / upsert / delete / trim），拆分理由见该文件的模块头。
