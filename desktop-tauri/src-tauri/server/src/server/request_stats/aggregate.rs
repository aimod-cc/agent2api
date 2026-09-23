//! 请求统计的**聚合计算**：日报的折算、整行重算与报表装配。
//!
//! ── 为什么单独一层 ──────────────────────────────────────────
//! `request_daily` 那一行数据的每一条不变式都靠「同一份累加代码」支撑：
//! 记账（`record`）、整行重算（`rebuild_day`）与报表装配（`summarize`）
//! 必须读同一份字段口径，聚合行才不会因为某条路径少算一维而跟明细对不上
//! 账（「各维之和 = 当天总量」）。这些自由函数与 `request_stats.rs` 里的
//! 方法们（记账 / 查询 / 存储概况）关注点不同，拆出来后这条不变式写在
//! 一个文件里，改口径的人一眼能看到全部落点。函数仍是 `request_stats`
//! 模块树的一部分（`pub(super)`），对外的 API 形状与拆分前逐字相同。
//!
//! ── 「进行中行不进日报」的口径 ────────────────────────────────
//! status=0 的行（转发开始时插的，见 `RequestStats::record_started`）没有
//! 任何终态值，折算进聚合会让报表凭空多一次请求而 token 是 0。这条拦截在
//! [`fold_into_daily`] —— 唯一的累加入口，记账 / 重算两条路径都被它覆盖。

use std::collections::{BTreeMap, BTreeSet};

use chrono::{Duration as ChronoDuration, NaiveDate};
use serde_json::{json, Value};

use super::clock::{date_key, day_of, local_midnight_ms};
use super::record::{DailyEntry, RequestEntry};
use super::report::{
    build_accounts, build_models, build_providers, build_top_model, cache_rates, cache_trend_24h,
    daily_trend, heatmap, normalize_range, push_account_accum, push_model_accum,
    push_provider_accum, range_bounds, range_totals, streak,
};
use super::{daily, sql};
use super::legacy;

/// 模型用量口径订正的完成标记键（`kv` 表）。
///
/// 与 `legacy::MARKER_*` 同一机制与写法：存在即视为已订正，值恒为 `'true'`。
/// 为什么需要显式标记（而不是像账号维度回填那样从数据本身检测），见
/// `RequestStats::remap_model_dimension_once` 的说明。
const MODEL_UPSTREAM_MARKER: &str = "modelUsageUpstreamRemapped";

/// [`RequestStats::remap_model_dimension_once`] 的库操作体：单事务完成
/// 「读标记 → 找候选日 → 逐日重算 → 写标记」。
///
/// 重算走 [`rebuild_day`]（与记账 / 清理重算同一段累加代码，
/// 口径天然一致）；返回真正重算的日期（升序，供日志列出）。
pub(super) fn remap_model_days(conn: &mut rusqlite::Connection) -> rusqlite::Result<Vec<String>> {
    if legacy::marker_present(conn, MODEL_UPSTREAM_MARKER)? {
        return Ok(Vec::new());
    }
    let tx = conn.unchecked_transaction()?;
    // 明细覆盖到的本地日期（去重升序）。DISTINCT ts 走 ts 索引，
    // 明细有 2 万行的容量闸，全扫代价可忽略。
    let mut days: BTreeSet<NaiveDate> = BTreeSet::new();
    {
        let mut stmt = tx.prepare("SELECT DISTINCT ts FROM requests")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let ts: i64 = row.get(0)?;
            days.insert(day_of(ts));
        }
    }
    let mut changed = Vec::new();
    for day in days {
        let key = date_key(day);
        let start = local_midnight_ms(day);
        // 开区间上界减 1 毫秒：与 rebuild_day 的取法一致（见那里的边界说明）
        let end = local_midnight_ms(day + ChronoDuration::days(1)).saturating_sub(1);
        let detail_count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM requests WHERE ts >= ?1 AND ts <= ?2",
            rusqlite::params![start, end],
            |row| row.get(0),
        )?;
        // 聚合行的当天请求数。读不出（行不存在或读失败）当 0 处理：
        // 重算是按明细整行重写的安全动作，聚合行缺失时重算顺带把它建全。
        let aggregate_requests: i64 = tx
            .query_row(
                "SELECT requests FROM request_daily WHERE date = ?1",
                rusqlite::params![key],
                |row| row.get(0),
            )
            .unwrap_or(0);
        // backfill 同一条判据：明细条数 ≥ 聚合行请求数才说明明细足以代表
        // 当天（没被保留期/容量裁过），才重算 —— 拿残缺明细重算会让当天
        // 数字凭空缩水（见 `backfill` 模块头的「拿不准就不动」）
        if detail_count >= aggregate_requests {
            rebuild_day(&tx, day)?;
            changed.push(key);
        }
    }
    // 标记与数据同事务提交：中断（断电 / 崩溃）后标记必然没写，下次启动
    // 整批重跑 —— 重算本身幂等，代价只是再扫一遍
    tx.execute(
        "INSERT INTO kv (key, value) VALUES (?1, 'true')
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![MODEL_UPSTREAM_MARKER],
    )?;
    tx.commit()?;
    Ok(changed)
}

/// 重算某一天的聚合行：从**剩余明细**整行重算；一条不剩就删掉那一行。
///
/// `day` 是本地自然日，区间按 `[当天零点, 次日零点)` 取 —— 与 `date_key(day_of(ts))`
/// 的归档口径同源（都用 `clock` 的本地时区工具），所以「某条明细属于哪一天」
/// 与「重算时按哪个区间取它」不可能对不上。
fn rebuild_day(conn: &rusqlite::Connection, day: NaiveDate) -> rusqlite::Result<()> {
    let key = date_key(day);
    let start = local_midnight_ms(day);
    // 开区间上界减 1 毫秒，等价于 `[start, next_start)`；用 1ms 而不是直接取
    // `next_start - 1` 是为了避开「夏令时切换当天次日零点不存在」这类边界
    // （`local_midnight_ms` 有兜底，但让它只负责一个方向更简单）
    let end = local_midnight_ms(day + ChronoDuration::days(1)).saturating_sub(1);
    let entries = sql::select_between(conn, start, end)?;
    if entries.is_empty() {
        daily::delete_daily(conn, &key)?;
        return Ok(());
    }
    let mut rebuilt = DailyEntry::new(key);
    for item in &entries {
        fold_into_daily(&mut rebuilt, item);
    }
    daily::upsert_daily(conn, &rebuilt)
}

/// 报表装配：把两张表的读数交给 `report` 的纯函数，拼出 `/api/stats/summary` 的响应。
///
/// 为什么把这段从 `usage_summary` 里抽出来：库不可用时的降级值是「按空库算同一份
/// 结果」（见该方法的说明），抽成纯函数后两条路径**必然同形** ——
/// 手写一份全零的 JSON 迟早会与正常路径的形状漂开，而形状漂开是静默的
/// （前端只会显示空白或 0，不会报错）。
pub(super) fn summarize(
    daily: &BTreeMap<String, DailyEntry>,
    entries: &[RequestEntry],
    range: &str,
    now: i64,
) -> Value {
    let now_day = super::clock::today();
    let range = normalize_range(range);
    let (start_date, end_date) = range_bounds(range, daily, now_day);

    // overview / dailyTrend / heatmap 走聚合（跨年；明细只有请求天数）
    let totals = range_totals(daily, &start_date, &end_date);
    let top_model = build_top_model(&totals.model_totals, totals.tokens);
    // providers 与 topModel **同源同区间**：都从这次的 `range_totals` 出，
    // 于是「按 provider 的请求数之和」必然等于 overview.requests，
    // 前端把它们并排显示时不会出现互相对不上的数
    let providers = build_providers(&totals.provider_totals);
    // accounts 与 providers / topModel **同源同区间**（同上）：账号排行的
    // 请求数之和也等于 overview.requests
    let accounts = build_accounts(&totals.account_totals);
    // models 同样与 topModel 同源（`topModel` 就是这张表的冠军）：
    // 报表的「模型用量」环形图读它，各扇区之和等于 overview.tokens
    let models = build_models(&totals.model_totals);
    let trend = daily_trend(daily, &start_date, &end_date);
    let map = heatmap(daily, now_day);
    let consecutive = streak(daily, now_day);

    // cacheRates / cacheTrend24h 走明细（窗口 ≤7 天，明细够用）
    let rates = cache_rates(entries, now);
    let cache_trend = cache_trend_24h(entries, now);

    json!({
        "range": range,
        "startDate": start_date,
        "endDate": end_date,
        "overview": {
            "requests": totals.requests,
            "successful": totals.successful,
            "tokens": totals.tokens,
            "activeDays": totals.active_days,
            "streak": consecutive,
            "topModel": top_model,
        },
        // 按 provider 维度的区间汇总（**新增字段，不改既有字段**）。
        // 前端按「存在则展示、缺失则隐藏」消费，所以旧前端拿到它只会忽略。
        // 恒为数组（无数据时是空数组而不是 null）：前端不必判两种空形态
        "providers": providers,
        // 按账号维度的区间汇总（**新增字段，不改既有字段**，与 providers 同形态）。
        // 账号是比 provider 更细的一维（一家可挂多个账号），所以这张表回答的是
        // 「具体哪个登录态在出力」——同一家的多个账号会各占一行。
        "accounts": accounts,
        // 按模型维度的区间汇总（**新增字段，不改既有字段**，与上两维同形态）。
        // 模型比 provider 更细：一家可以承载多个模型，所以这张表回答的是
        // 「用量花在哪个模型上」——报表的「模型用量」环形图直接读它
        "models": models,
        "heatmap": map,
        "cacheRates": rates,
        "cacheTrend24h": cache_trend,
        "dailyTrend": trend,
    })
}

/// 把一条明细累加进当天的聚合行。
///
/// `record` 的记账与 `rebuild_day` 的重算（还有迁移的口径回填）共用这一段：
/// 聚合行的每个字段都**只**由明细逐条累加而来，几条路径手写几遍必然漂移
/// （对不上账）。三个维度（模型 / provider / 账号）与总量并列累计，
/// 各维求和都等于当天总量，这是报表之间能对账的前提。
///
/// **进行中行（status=0）在这里被拦下**：日报只折算终态。记账路径送进来的
/// 终态行不会有 status=0（错误至少是 4xx/5xx，见 `GatewayError` 的状态码），
/// 这条判断实际拦的是 `rebuild_day` / 迁移回填**从库里拉行**时混进来的
/// 进行中行 —— 没有它，重算会把一条「还没跑完」的请求凭空加进当天请求数
/// （token 还是 0），日报与明细口径立刻分裂。
pub(super) fn fold_into_daily(day: &mut DailyEntry, item: &RequestEntry) {
    if item.status == 0 {
        return;
    }
    let success = item.is_success();
    day.requests += 1;
    if success {
        day.successful += 1;
    }
    day.tokens += item.total_tokens;
    day.cache_hit_tokens += item.cache_read_tokens;
    day.cache_input_tokens += item.prompt_tokens;
    // 模型维度用「上游真名」当统计键（见 `model_stat_key`）：映射只是代名，
    // 实际请求的仍是上游那一个模型，报表要按它归组
    push_model_accum(&mut day.model_tokens, model_stat_key(item), item.total_tokens, 1);
    // 空 provider 也建组（见 push_provider_accum 的注释）
    push_provider_accum(
        &mut day.provider_stats,
        &item.provider,
        1,
        i64::from(success),
        item.total_tokens,
    );
    push_account_accum(
        &mut day.account_stats,
        &item.account_id,
        &item.account_name,
        1,
        i64::from(success),
        item.total_tokens,
    );
}

/// 报表按模型聚合用的**统计键**：上游真名优先，请求名回落。
///
/// ── 为什么不能直接用 `model` ────────────────────────────────
/// 映射语义重做后（见 `pipeline::resolve_model` 的说明），请求名全程保持
/// 客户端原值，改写下沉到发送侧按家进行（`payload::send_body` →
/// `catalog::wire_target_for_provider`）。于是 `model`（请求侧解析名）在
/// 客户端点名映射别名时就是**别名本身** —— 报表若按它聚合，同一个上游模型
/// 会被拆成「真名 + 各家别名」好几行。而映射只是代名，实际请求的仍是上游
/// 那一个模型：`upstream_model`（telemetry 在发送侧采集的最终下发名）才是
/// 「用量花在哪个模型上」的正确答案。
///
/// ── 回落 ──────────────────────────────────────────────────
/// 转发前就失败的请求一次都没发出去，`upstream_model` 为空串 —— 请求名是
/// 此时我们所知的最好信息（token 已清零，只影响请求数维度）。还没有
/// `upstreamModel` 键的旧明细同样走回落：这不是「给旧数据猜值」，空串回落
/// 保留的是明细里本来就有的信息。
fn model_stat_key(item: &RequestEntry) -> &str {
    if item.upstream_model.is_empty() {
        &item.model
    } else {
        &item.upstream_model
    }
}
