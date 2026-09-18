//! 报表计算 —— 对内存快照的**纯函数**变换，不碰磁盘、不持锁。
//!
//! 为什么单独一层：报表口径是本切片里最容易出细节错的部分（零除、补零、
//! 连续天数、并列比较、时区边界），集中在一起才能一眼看清「同一天/同一小时」
//! 的口径是否处处一致；也便于后续改动时只碰这一处。
//!
//! ── 数据来源的分工 ──────────────────────────────────────────
//!   - `overview` / `dailyTrend` / `heatmap` / `streak` 走**聚合**：
//!     这些口径要跨年（热力图固定 365 天、`all` 可能一年以上），
//!     而明细只留 30 天，靠明细算会越算越少。
//!   - `cacheRates` / `cacheTrend24h` 走**明细**：窗口 ≤7 天，
//!     且要精确到分钟/整点，按天的聚合行给不出这种精度。

use std::collections::{BTreeMap, HashMap};

use chrono::{Datelike, Duration as ChronoDuration, NaiveDate};
use serde_json::{json, Value};

use super::clock::{date_key, hour_floor, hour_key, ms_to_local};
use super::record::{DailyEntry, ModelAccum, RequestEntry};

/// 热力图固定返回的天数（与 range 解耦）
pub(super) const HEATMAP_DAYS: i64 = 365;

/// 逐天序列 / 连续天数的循环上限（兜底手改出来的超长区间）
const MAX_TREND_DAYS: i64 = 4000;

/// 合法的时间区间取值
const RANGE_TODAY: &str = "today";
const RANGE_7: &str = "7";
const RANGE_30: &str = "30";
const RANGE_MONTH: &str = "month";
const RANGE_ALL: &str = "all";

/// range 归一：非法值按 `"7"`（取舍见 `usage_summary` 的注释）
pub(super) fn normalize_range(range: &str) -> &'static str {
    match range.trim() {
        RANGE_TODAY => RANGE_TODAY,
        RANGE_30 => RANGE_30,
        RANGE_MONTH => RANGE_MONTH,
        RANGE_ALL => RANGE_ALL,
        _ => RANGE_7,
    }
}

/// 区间边界（闭合的本地日期键 `[startDate, endDate]`）。
///
/// - `today` 就是今天；`7` / `30` 从今天往前推（含今天，所以减 days-1）
/// - `month` 为本月 1 号到今天（本地时区）
/// - `all` 取聚合里最早的一天；**完全没有数据**时给「今天往前 29 天」，
///   让前端拿到一个合法区间而不是空串或 null
pub(super) fn range_bounds(
    range: &str,
    daily: &BTreeMap<String, DailyEntry>,
    today: NaiveDate,
) -> (String, String) {
    let end = today;
    let start = match range {
        RANGE_TODAY => today,
        RANGE_30 => today - ChronoDuration::days(29),
        RANGE_MONTH => today.with_day(1).unwrap_or(today),
        RANGE_ALL => daily
            .keys()
            .next()
            .and_then(|key| NaiveDate::parse_from_str(key, "%Y-%m-%d").ok())
            .unwrap_or_else(|| today - ChronoDuration::days(29)),
        // RANGE_7 及任何落到这里的值
        _ => today - ChronoDuration::days(6),
    };
    // 极端情况：聚合里有「未来」的行（系统时钟被往前调过），start 不能晚于 end，
    // 否则区间是反向的，前端画图会出现负长度时间轴
    let start = if start > end { end } else { start };
    (date_key(start), date_key(end))
}

/// 区间内的总量与按模型累计（overview 的原料）
pub(super) struct RangeTotals {
    pub requests: i64,
    pub successful: i64,
    pub tokens: i64,
    pub active_days: i64,
    pub model_totals: Vec<ModelAccum>,
}

/// 按区间累计聚合行。`BTreeMap::range` 直接按日期键取闭区间 ——
/// 定长日期串的字典序即时间序，这是选 `BTreeMap` 的直接收益。
pub(super) fn range_totals(
    daily: &BTreeMap<String, DailyEntry>,
    start: &str,
    end: &str,
) -> RangeTotals {
    let mut totals = RangeTotals {
        requests: 0,
        successful: 0,
        tokens: 0,
        active_days: 0,
        model_totals: Vec::new(),
    };
    // 闭区间 [start, end]：定长日期串的字典序即时间序，所以直接按键取范围。
    // 这里构造两个 String 边界是有意的取舍 —— 报表调用频率是「用户点一下」级别，
    // 两次小分配远不如让 `range` 的边界类型一目了然重要。
    for (_, day) in daily.range(start.to_string()..=end.to_string()) {
        totals.requests += day.requests;
        totals.successful += day.successful;
        totals.tokens += day.tokens;
        // activeDays 只数**区间内**有请求的天数（补零的日子不算「活跃」）
        if day.requests > 0 {
            totals.active_days += 1;
        }
        for acc in &day.model_tokens {
            push_model_accum(&mut totals.model_totals, &acc.model, acc.tokens, acc.requests);
        }
    }
    totals
}

/// 把一次请求（或一天的累计）并进按模型的累计表。
/// 线性查找即可：模型数量是个位到十位级，建 HashMap 反而更慢。
pub(super) fn push_model_accum(list: &mut Vec<ModelAccum>, model: &str, tokens: i64, requests: i64) {
    match list.iter_mut().find(|item| item.model == model) {
        Some(item) => {
            item.tokens += tokens;
            item.requests += requests;
        }
        None => list.push(ModelAccum {
            model: model.to_string(),
            requests,
            tokens,
        }),
    }
}

/// 区间级 topModel：按 `tokens` 降序，并列比 `requests`，再并列比名字
/// （名字兜底是为了结果稳定，不受聚合行内部顺序影响）。
/// 无数据或所有记录都没带模型名时返回 `null`。
pub(super) fn build_top_model(totals: &[ModelAccum], range_tokens: i64) -> Value {
    let best = totals
        .iter()
        // 空模型名不参与评选：否则会冒出个「空名字冠军」，标题栏显示成空白，
        // 比不给结果更像故障
        .filter(|item| !item.model.is_empty() && (item.tokens > 0 || item.requests > 0))
        .max_by(|left, right| {
            left.tokens
                .cmp(&right.tokens)
                .then(left.requests.cmp(&right.requests))
                // 取反：名字小的排前面，保证并列时结果确定
                .then(right.model.cmp(&left.model))
        });
    let Some(best) = best else {
        return Value::Null;
    };
    // percentage = 该模型 tokens / 区间总 tokens；总为 0 时给 0.0，
    // 绝不产生 NaN/Infinity（serde_json 序列化非有限浮点会 panic 或产出 null）
    let percentage = if range_tokens > 0 {
        best.tokens as f64 / range_tokens as f64
    } else {
        0.0
    };
    json!({
        "model": best.model,
        "tokens": best.tokens,
        "requests": best.requests,
        "percentage": percentage,
    })
}

/// 范围内的逐天序列（缺失日期补零，升序）。
///
/// 前端按天画柱状图，缺的那天必须占位（补 0）而不是不返回 ——
/// 少一天会让整条曲线在视觉上被压缩，趋势看起来是错的。
pub(super) fn daily_trend(daily: &BTreeMap<String, DailyEntry>, start: &str, end: &str) -> Vec<Value> {
    let (Some(start_date), Some(end_date)) = (parse_key(start), parse_key(end)) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cursor = start_date;
    // 上限兜底：手改文件把区间撑到十年也不会把响应撑爆
    while cursor <= end_date && out.len() < MAX_TREND_DAYS as usize {
        let key = date_key(cursor);
        let (tokens, requests) = daily
            .get(&key)
            .map(|day| (day.tokens, day.requests))
            .unwrap_or((0, 0));
        out.push(json!({ "date": key, "tokens": tokens, "requests": requests }));
        cursor += ChronoDuration::days(1);
    }
    out
}

/// 热力图：**固定 365 天**（含今天），与 range 解耦，无数据的日期补零。
/// 升序返回，前端按「一年 = 若干列周」排布。
pub(super) fn heatmap(daily: &BTreeMap<String, DailyEntry>, today: NaiveDate) -> Vec<Value> {
    let start = today - ChronoDuration::days(HEATMAP_DAYS - 1);
    let mut out = Vec::with_capacity(HEATMAP_DAYS as usize);
    let mut cursor = start;
    while cursor <= today {
        let key = date_key(cursor);
        let (requests, tokens) = daily
            .get(&key)
            .map(|day| (day.requests, day.tokens))
            .unwrap_or((0, 0));
        out.push(json!({ "date": key, "requests": requests, "tokens": tokens }));
        cursor += ChronoDuration::days(1);
    }
    out
}

/// 连续有请求的天数：从**本地今天**往前数。
///
/// 「今天还没有请求不算断」—— 凌晨打开报表时今天通常还是空的，
/// 若把今天算成断，连续天数每天都要先归零一次，与用户直觉相反。
/// 所以今天没记录时从昨天起算。
pub(super) fn streak(daily: &BTreeMap<String, DailyEntry>, today: NaiveDate) -> i64 {
    let has = |day: NaiveDate| -> bool {
        daily
            .get(&date_key(day))
            .is_some_and(|item| item.requests > 0)
    };
    let mut cursor = today;
    if !has(cursor) {
        cursor -= ChronoDuration::days(1);
    }
    let mut count = 0i64;
    // 上限兜底：手改数据造出「万年连续」也不会死循环
    while count < MAX_TREND_DAYS && has(cursor) {
        count += 1;
        cursor -= ChronoDuration::days(1);
    }
    count
}

// ─── 缓存命中率（窗口都很短，必须从明细算）────────────────────

/// 四档缓存命中率
pub(super) fn cache_rates(entries: &[RequestEntry], now: i64) -> Value {
    const MINUTE: i64 = 60_000;
    json!({
        "last10m": cache_rate(entries, now - 10 * MINUTE, now),
        "last1h": cache_rate(entries, now - 60 * MINUTE, now),
        "last24h": cache_rate(entries, now - 24 * 60 * MINUTE, now),
        "last7d": cache_rate(entries, now - 7 * 24 * 60 * MINUTE, now),
    })
}

/// 单窗口的命中率（`[since_ms, now]` 闭区间）
fn cache_rate(entries: &[RequestEntry], since_ms: i64, now: i64) -> Value {
    let mut hit: i64 = 0;
    let mut input: i64 = 0;
    for item in entries {
        if item.ts >= since_ms && item.ts <= now {
            hit += item.cache_read_tokens;
            input += item.prompt_tokens;
        }
    }
    json!({ "hitTokens": hit, "inputTokens": input, "rate": safe_rate(hit, input) })
}

/// 近 24 个**本地整点**的命中率趋势（无数据的整点补零）。
///
/// 按本地时区切整点：先把时间戳还原成 `DateTime<Local>` 再取 `%H`，
/// 于是「14 点」就是用户时钟上的 14 点，不是 UTC 的 14 点。
pub(super) fn cache_trend_24h(entries: &[RequestEntry], now: i64) -> Vec<Value> {
    let start = hour_floor(ms_to_local(now)) - ChronoDuration::hours(23);
    let start_ms = start.timestamp_millis();

    // 先把窗口内的条目按整点键归桶，再按 24 个整点取值 ——
    // 逐个整点扫一遍明细是 24×N，这样只需一次遍历
    let mut buckets: HashMap<String, (i64, i64)> = HashMap::new();
    for item in entries {
        if item.ts < start_ms || item.ts > now {
            continue;
        }
        let slot = buckets.entry(hour_key(ms_to_local(item.ts))).or_insert((0, 0));
        slot.0 += item.cache_read_tokens;
        slot.1 += item.prompt_tokens;
    }

    let mut out = Vec::with_capacity(24);
    for step in 0..24 {
        let key = hour_key(start + ChronoDuration::hours(step));
        let (hit, input) = buckets.get(&key).copied().unwrap_or((0, 0));
        out.push(json!({
            "hour": key,
            "hitTokens": hit,
            "inputTokens": input,
            "rate": safe_rate(hit, input),
        }));
    }
    out
}

/// 命中率：`hit / input`，分母为 0（或无命中）时返回 0.0。
///
/// **必须挡住零除**：0/0 在浮点下是 NaN，`serde_json` 序列化 NaN 时
/// 要么 panic 要么产出 `null`，前端拿到非数字后图表直接空白。
/// 这里也挡住负数（手改文件可能塞进负值）——「命中」为负没有物理意义，
/// 直接按 0 处理。
///
/// 注意**不把结果夹到 1.0**：上游有些接口把 cacheRead 与 promptTokens
/// 分开报（promptTokens 不含缓存部分），此时命中率合法地会超过 100%。
/// 夹一下确实更好看，但那是拿数据真实性换观感 —— 报表要反映真实比值，
/// 越界与否交给前端展示层决定。
pub(super) fn safe_rate(hit: i64, input: i64) -> f64 {
    if input <= 0 || hit <= 0 {
        return 0.0;
    }
    let rate = hit as f64 / input as f64;
    if rate.is_finite() {
        rate
    } else {
        0.0
    }
}

/// status 过滤归一：`"ok"` → 只看 2xx，`"error"` → 只看非 2xx，
/// 其余（含 None 与拼错的值）不过滤。大小写不敏感、容忍前后空白。
pub(super) fn normalize_status_filter(value: Option<&str>) -> Option<bool> {
    match value.map(str::trim).map(str::to_lowercase).as_deref() {
        Some("ok") => Some(true),
        Some("error") => Some(false),
        _ => None,
    }
}

/// `YYYY-MM-DD` → NaiveDate（手改文件里的坏值返回 None，调用方各自兜底）
fn parse_key(text: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(text.trim(), "%Y-%m-%d").ok()
}
