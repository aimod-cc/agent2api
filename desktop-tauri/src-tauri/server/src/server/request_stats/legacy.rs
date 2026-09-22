//! 旧文件导入：给 `db::migrate` 的两个迁移项用的入口与解析器。
//!
//! ── 为什么这些函数在本模块而不是搬进迁移项 ────────────────────
//! 它们是「读旧文件、写新表」的那一段，看着像迁移项自己的事，但：
//!   1. **解析口径是数据契约的一部分**：`parse_requests_jsonl` /
//!      `parse_daily_jsonl` 必须与 `record.rs` 的 serde 注解**共用同一份**
//!      实现 —— 迁移项只负责「文件在哪、什么时候导、导完备份谁」；
//!   2. **写库口径是存储规则的一部分**：导入后的裁剪边界（保留期）与裁剪语句
//!      必须与运行期**同一套** —— 迁移期没有 `RequestStats` 实例（它由数据库
//!      就绪之后才构造），所以把「从配置取保留期 + 算边界」这一步在这里暴露出来
//!      （`legacy_bounds`），边界计算仍复用 `RetentionBounds::of`；
//!   3. **回填需要 `backfill` 与 `fold_into_daily`**（都是私有子模块/私有函数），
//!      迁移项够不到它们。
//! 所以这里只暴露**入口**（`parse_*` / `legacy_*` / `import_legacy_*`），
//! 迁移项拿到手就只管调度与日志。
//!
//! ── 这些函数只在启动迁移时被调用 ─────────────────────────────
//! 它们全是 `pub(crate)`：唯一的调用方是 `db::migrate::requests` 的两个迁移项。
//! 运行期一次都不会走到（库里已有数据、旧文件已被改名）。
//! 用 `pub(crate)` 而不是 `pub(super)` 是因为迁移项在 `db::migrate` 里，
//! 与 `request_stats` 不是父子模块。

use std::collections::BTreeMap;

use super::record::{DailyEntry, RequestEntry, Retention, MAX_DAILY_DAYS, MAX_ENTRIES};
use super::report::{push_account_accum, push_model_accum, push_provider_accum};
use super::{backfill, sql, RetentionBounds};

/// 逐行解析明细旧文件（`requests.jsonl`），坏行跳过。
///
/// **口径与旧 `load_requests` 逐字一致**：先 trim、空行跳过、解析失败跳过、
/// 字段缺失走 `#[serde(default)]`（`id` 空串、`attempts` 1、token 0…）。
/// 这套解析因此留在本模块而不是搬进迁移项：它是**数据契约**的一部分
/// （`record.rs` 的 serde 注解就是契约），迁移项只负责「文件在哪、什么时候导」。
pub(crate) fn parse_requests_jsonl(text: &str) -> Vec<RequestEntry> {
    let mut out = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(item) = serde_json::from_str::<RequestEntry>(trimmed) {
            out.push(item);
        }
    }
    out
}

/// 逐行解析聚合旧文件（`request-daily.jsonl`），坏行跳过。
///
/// **口径与旧 `load_daily` 逐字一致**，包括那条不显眼但重要的规则：
/// 同一天出现多行（手工合并文件、异常退出留下的重复行）时按行**合并**，
/// 而不是「后者覆盖前者」—— 覆盖会静默吞掉前一份数据。合并走
/// `push_*_accum`，所以三个维度与总量一起合上，不会出现
/// 「总量加了两遍、维度只加了一遍」这种对不上账的库。
/// 日期为空的行丢掉（它进不了按天查询的语义）。
pub(crate) fn parse_daily_jsonl(text: &str) -> BTreeMap<String, DailyEntry> {
    let mut out: BTreeMap<String, DailyEntry> = BTreeMap::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(item) = serde_json::from_str::<DailyEntry>(trimmed) else {
            continue;
        };
        if item.date.is_empty() {
            continue;
        }
        match out.get_mut(&item.date) {
            Some(existing) => {
                existing.requests += item.requests;
                existing.successful += item.successful;
                existing.tokens += item.tokens;
                existing.cache_hit_tokens += item.cache_hit_tokens;
                existing.cache_input_tokens += item.cache_input_tokens;
                for acc in item.model_tokens {
                    push_model_accum(&mut existing.model_tokens, &acc.model, acc.tokens, acc.requests);
                }
                // provider 维度同理逐行合并（同一天多行时两组都要合上，
                // 否则「按 provider 之和 = requests」这条对账关系会破）
                for acc in item.provider_stats {
                    push_provider_accum(
                        &mut existing.provider_stats,
                        &acc.provider,
                        acc.requests,
                        acc.successful,
                        acc.tokens,
                    );
                }
                // 账号维度同理（理由与 provider 那条一致：合并要不全做、要不全不做）
                for acc in item.account_stats {
                    push_account_accum(
                        &mut existing.account_stats,
                        &acc.account_id,
                        &acc.account_name,
                        acc.requests,
                        acc.successful,
                        acc.tokens,
                    );
                }
            }
            None => {
                out.insert(item.date.clone(), item);
            }
        }
    }
    out
}

/// 迁移项用的保留边界：读配置的内存快照，按与运行期**同一套**归一化算边界。
///
/// 为什么不让迁移项自己调 `config::retention_settings()` 再算：那会把「保留 N 天
/// 是含今天在内的 N 个自然日」这条口径复制到第二处。运行期走
/// `RequestStats::retention_bounds`（保留期来自回调），迁移期没有实例，所以
/// 这里把「从配置取保留期」这一步单独暴露出来，边界计算仍共用
/// [`RetentionBounds::of`]。
pub(crate) fn legacy_bounds() -> RetentionBounds {
    let settings = crate::server::config::retention_settings();
    RetentionBounds::of(Retention {
        request_days: settings.request_days,
        daily_days: settings.daily_days,
    })
}

// 这里原本有一个 `legacy_counts`（返回明细与聚合两张表的行数，给「表非空
// 即跳过」当幂等闸门）。**已删除**：T11 把迁移改成用户点「升级」触发之后，
// 那个判据不成立了 —— 两张表在点击前就可能被运行期写过（用户先发过请求），
// 于是本项被永远判成「已迁过」，旧文件里的历史明细再也进不来
// （`logs` 项已线上复现同类死循环）。现在靠标记键 + 同事务幂等
// （见 `MARKER_REQUESTS`），不需要「表空不空」这个查询。

/// 明细迁移的完成标记键（`kv` 表）。
///
/// ── 为什么这两项需要标记键，而 `logs` / `accounts` / `debug` 不需要 ──
/// 那三项的主键就是「这条记录是谁」：`logs.id` 是历史行号、`accounts.id` 是
/// 记录 uuid、`debug_traffic.id` 是请求关联 id —— 于是「已存在」可以直接由
/// 主键判断（`INSERT OR IGNORE`）。而 `requests` / `request_daily` 没有可用的
/// 业务主键：
///   - `requests` 的主键是 `row_id INTEGER PRIMARY KEY AUTOINCREMENT`（自增，
///     与内容无关），而 `id` 列是**请求关联 id、旧行可能是空串**（见
///     `record.rs` 的说明），当不了唯一约束；
///   - `request_daily` 的主键 `date` 倒是稳定，但它**会被运行期 UPSERT 改写**
///     （当天的新请求会累加进同一行）—— 用「date 已存在」判断「导过了」，
///     会把「今天已经转发过请求」误判成「旧聚合已导入」。
/// 所以这两项用标记键。它是安全的，因为**标记与数据在同一个事务里提交**
/// （完整论证见 `db::migrate::config` 的模块头：那条反对标记的理由针对的是
/// **非原子**的标记；事务提交 ⇒ 标记与数据都在，回滚 ⇒ 都不在，
/// 「标记写了数据缺一半」这个中间态在物理上不存在）。
pub(crate) const MARKER_REQUESTS: &str = "requestsMigrated";
/// 聚合迁移的完成标记键（理由同 [`MARKER_REQUESTS`]）
pub(crate) const MARKER_DAILY: &str = "dailyMigrated";

/// 某个标记键在不在（迁移项的幂等闸门）。
pub(crate) fn marker_present(conn: &rusqlite::Connection, key: &str) -> rusqlite::Result<bool> {
    let found: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM kv WHERE key = ?1 LIMIT 1",
            rusqlite::params![key],
            |row| row.get(0),
        )
        .ok();
    Ok(found.is_some())
}

/// 写标记键（**必须在迁移自己的事务里调**，见 [`MARKER_REQUESTS`] 的说明）。
fn write_marker(conn: &rusqlite::Connection, key: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO kv (key, value) VALUES (?1, 'true')
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![key],
    )?;
    Ok(())
}

/// 迁移项用：[`backfill`] 的日志文案（回填发生在迁移里，那份文案的措辞与取舍
/// 写在 `backfill::report_line` 的注释里；这里只做一层转发，
/// 因为 `backfill` 是私有子模块，迁移项拿不到它）。
pub(crate) fn backfill_report_line(changed: &[String]) -> String {
    backfill::report_line(changed)
}

/// 迁移项用：把旧明细整批写进 `requests` 表（**一个事务**，含导入后的裁剪）。
///
/// 为什么导入后要裁一遍：旧实现只在**启动载入**时把内存裁到合规，盘上的文件
/// 下一次写入才收敛 —— 所以旧文件里留着超期行、超容量行是常态（见
/// `db/migrate.rs` 模块头对同类问题的说明）。不裁的话迁移完的库立刻就超限。
/// 裁法与运行期同源：时间用 `delete_expired_requests`、容量用
/// `trim_requests_capacity`（同一组函数，口径不可能漂）。
///
/// 用 `unchecked_transaction` 而不是 `transaction`：迁移框架给的签名是
/// `&Connection`（`transaction()` 要 `&mut`），调用点 `Db::with` 全程只有一把锁、
/// 没有第二个访问者 —— 那条「同连接不得嵌套事务」的约束由调用点保证
/// （与 `db::migrate` 里其它写入函数同一做法）。
///
/// ── 幂等：标记键与数据**同事务**（T11 之后不能靠「表非空即跳过」）──
/// 明细表会被运行期写入（用户点「升级」之前可能已经发过请求），所以判「表空
/// 不空」会让旧文件永远导不进来（`logs` 项已线上复现该死循环）。
/// 本项改用标记键 [`MARKER_REQUESTS`]，**在同一个事务里**与数据一起提交：
///   - 提交 ⇒ 数据 + 标记都在，重复点击时 `marker_present` 命中 → 直接跳过；
///   - 回滚 ⇒ 二者都不在，下次点击干净重试。
/// 所以纯 INSERT 在这里是安全的（`requests` 没有可用的业务唯一键来做
/// `INSERT OR IGNORE`，理由见 [`MARKER_REQUESTS`] 的说明）。
pub(crate) fn import_legacy_requests(
    conn: &rusqlite::Connection,
    entries: &[RequestEntry],
    bounds: &RetentionBounds,
) -> rusqlite::Result<usize> {
    let tx = conn.unchecked_transaction()?;
    for entry in entries {
        sql::insert_request(&tx, entry)?;
    }
    sql::delete_expired_requests(&tx, bounds.requests_ms)?;
    sql::trim_requests_capacity(&tx, MAX_ENTRIES)?;
    // 标记与数据同事务提交（本项幂等的全部依据）
    write_marker(&tx, MARKER_REQUESTS)?;
    tx.commit()?;
    Ok(entries.len())
}

/// 迁移项用：把旧聚合整批写进 `request_daily` 表，再做一次**口径回填**
/// （**一个事务**，含导入后的裁剪与回填写回）。
///
/// 返回（导入的天数, 被回填改写的日期）。调用方把后者记一行控制台日志 ——
/// 那几天的数字与用户昨天看到的可能不同，出问题时日志里要有线索。
///
/// ── 回填为什么在这里而不是运行期 ────────────────────────────
/// 见 `backfill` 模块头：`accountStats` 是后加的维度，旧聚合行没有它，
/// 报表只走聚合（跨年区间超出明细保留期），那段历史会整段落进「未知账号」。
/// 回填需要「缺维度的聚合行 + 足够的明细」同时在场，而这个组合**只可能出现在
/// 刚导入完旧数据的这一刻**：新代码写出的行三个维度恒齐（`fold_into_daily`
/// 恒建组），库里的行不会再出现「缺维度」状态。所以它是**一次性的升级动作**，
/// 归迁移所有；运行期不再有调用点（旧实现的 `load()` 里那次已随本切片消失）。
/// 安全边界「明细条数 >= 聚合行 requests 才重算」原样保留在 `backfill` 里 ——
/// 明细已被保留期裁掉的日子没有重算依据，继续留在未知组比缩水好。
///
/// ── 幂等：标记键与数据**同事务**（理由同 [`MARKER_DAILY`]）──────
/// 聚合表同样会被运行期写入（用户点「升级」之前发过请求 → 当天那行已被
/// UPSERT 累加过），所以「表非空即跳过」会让旧聚合永远导不进来。
/// 用标记键 [`MARKER_DAILY`]，在**同一个事务**里与数据一起提交。
///
/// 注意 `upsert_daily` 在这里是**必要的**（不是「顺手复用」）：旧聚合行里的
/// 日期可能与运行期已经累加过的当天重合，而 UPSERT 的语义是「用旧文件那份
/// 覆盖」—— 这**看起来**会丢掉今天已记录的量，但那正是本项要做的事：
/// 紧接着的回填会用明细（含刚导入的历史明细）重算这些日子，把覆盖掉的量
/// 按同一套口径加回来（`rebuild_legacy_days` 只重算「明细条数 ≥ 聚合行
/// requests」的日子，判据保证不会缩水）。
pub(crate) fn import_legacy_daily(
    conn: &rusqlite::Connection,
    daily: &BTreeMap<String, DailyEntry>,
    bounds: &RetentionBounds,
) -> rusqlite::Result<(usize, Vec<String>)> {
    let tx = conn.unchecked_transaction()?;
    for day in daily.values() {
        sql::upsert_daily(&tx, day)?;
    }
    sql::delete_daily_before(&tx, &bounds.daily_key)?;
    // 聚合的天数上限（与运行期同一个函数 —— 它自己先判行数再决定删不删）。
    // 明细的保留期与容量不在这里管：那是 `import_legacy_requests` 的事，
    // 而它按注册表顺序排在前面 —— 这边只是读它导进来的明细做回填。
    sql::trim_daily_capacity(&tx, MAX_DAILY_DAYS)?;
    // 裁剪之后再回填：已经超期被删掉的日子不必再花一次重算
    //（顺序与旧 `load()` 一致：先裁聚合、再回填）
    let mut stored = sql::select_daily_map(&tx)?;
    let entries = match (sql::min_ts(&tx)?, sql::max_ts(&tx)?) {
        // 明细在这两个时间之间全取 —— 回填的判据要看「某天有多少条明细」，
        // 只取候选日会需要先知道候选日（那正是 backfill 内部算的），
        // 全取一次更简单；上限 2 万条，一次性迁移可以接受
        (Some(min), Some(max)) => sql::select_between(&tx, min, max)?,
        _ => Vec::new(),
    };
    let changed = backfill::rebuild_legacy_days(&mut stored, &entries);
    for key in &changed {
        if let Some(day) = stored.get(key) {
            sql::upsert_daily(&tx, day)?;
        }
    }
    // 标记与数据同事务提交（本项幂等的全部依据）
    write_marker(&tx, MARKER_DAILY)?;
    tx.commit()?;
    Ok((daily.len(), changed))
}
