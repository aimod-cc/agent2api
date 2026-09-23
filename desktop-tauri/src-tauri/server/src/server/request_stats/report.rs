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
use super::record::{AccountAccum, DailyEntry, ModelAccum, ProviderAccum, RequestEntry};

/// 未知 provider 的展示名（`id` 为空串的那一组：旧版本写出的明细、
/// 以及「一次都没发出去就失败」的请求）。前端也会用同样的文案做降级显示。
pub(super) const UNKNOWN_PROVIDER_LABEL: &str = "未知";

/// 未知账号的展示名（没有账号身份的那一组）。
///
/// 与 provider 那条是**两个独立的常量**而不是共用一个：两者的「未知」
/// 成因不同（前者是没记下承载的家，后者是没走账号列表），文案将来若分叉，
/// 共用会让改一处等于改两处。
pub(super) const UNKNOWN_ACCOUNT_LABEL: &str = "未知账号";

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

/// 区间内的总量与按模型 / 按 provider / 按账号累计（overview 的原料）
pub(super) struct RangeTotals {
    pub requests: i64,
    pub successful: i64,
    pub tokens: i64,
    pub active_days: i64,
    pub model_totals: Vec<ModelAccum>,
    pub provider_totals: Vec<ProviderAccum>,
    pub account_totals: Vec<AccountAccum>,
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
        provider_totals: Vec::new(),
        account_totals: Vec::new(),
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
        for acc in &day.provider_stats {
            push_provider_accum(
                &mut totals.provider_totals,
                &acc.provider,
                acc.requests,
                acc.successful,
                acc.tokens,
            );
        }
        for acc in &day.account_stats {
            push_account_accum(
                &mut totals.account_totals,
                &acc.account_id,
                &acc.account_name,
                acc.requests,
                acc.successful,
                acc.tokens,
            );
        }
    }

    // ── 旧聚合行的余量归属 ──────────────────────────────────────
    // 升级前写出的聚合行没有 `providerStats` 键（读入后是空表），那些天的请求
    // 因此没有任何 provider 组覆盖。这里把「未被任何组覆盖的余量」补进空 id 组：
    //   ① 各 provider 之和恒等于区间总量 —— 报表之间能对账，前端把
    //      `providers` 与 `overview.requests` 并排显示时不会出现对不上的数；
    //   ② 旧数据落进「未知」组而不是被静默丢掉 —— 契约要求不得给旧数据
    //      **猜**值（不许补 workbuddy），但「未知」正是它的真实归属，不是猜。
    // 先求和再借用 `&mut`：闭包持着不可变借用时不能同时改这张表。
    let covered_requests: i64 = totals.provider_totals.iter().map(|item| item.requests).sum();
    let covered_successful: i64 = totals.provider_totals.iter().map(|item| item.successful).sum();
    let covered_tokens: i64 = totals.provider_totals.iter().map(|item| item.tokens).sum();
    // `max(0)` 兜住手改文件造成的「拆分比总量还多」；余量全零时这里仍会建出
    // 一个空组，但 `build_providers` 会把全零的组滤掉（见那边的 `requests > 0
    // || tokens > 0`），所以不会凭空冒出空的「未知」项
    push_provider_accum(
        &mut totals.provider_totals,
        "",
        (totals.requests - covered_requests).max(0),
        (totals.successful - covered_successful).max(0),
        (totals.tokens - covered_tokens).max(0),
    );

    // ── 账号维度同理 ────────────────────────────────────────────
    // `accountStats` 是本次新增的键，**所有**已有聚合行都没有它，所以这一段
    // 在这里比 provider 那一段更常命中：升级后第一次打开报表，区间内几乎全部
    // 请求都会落进「未知账号」组。这是如实反映「那些行没记账号身份」，
    // 而不是我们丢数据 —— 明细里其实有，但聚合要跨过年份，只能按聚合算。
    let covered_requests: i64 = totals.account_totals.iter().map(|item| item.requests).sum();
    let covered_successful: i64 = totals.account_totals.iter().map(|item| item.successful).sum();
    let covered_tokens: i64 = totals.account_totals.iter().map(|item| item.tokens).sum();
    push_account_accum(
        &mut totals.account_totals,
        "",
        "",
        (totals.requests - covered_requests).max(0),
        (totals.successful - covered_successful).max(0),
        (totals.tokens - covered_tokens).max(0),
    );
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

/// 把一批（一条或一天的）请求并进按 provider 的累计表。
///
/// 与 `push_model_accum` 同一取舍：provider 数量是个位数，线性查找比 HashMap
/// 更快也更省（不必分配键）。`provider` 为空串时**照常建组**，不丢弃 ——
/// 旧数据与「转发前失败」的请求都落在这一组里，丢掉它们会让
/// 「各 provider 之和」对不上区间总量。
pub(super) fn push_provider_accum(
    list: &mut Vec<ProviderAccum>,
    provider: &str,
    requests: i64,
    successful: i64,
    tokens: i64,
) {
    match list.iter_mut().find(|item| item.provider == provider) {
        Some(item) => {
            item.requests += requests;
            item.successful += successful;
            item.tokens += tokens;
        }
        None => list.push(ProviderAccum {
            provider: provider.to_string(),
            requests,
            successful,
            tokens,
        }),
    }
}

/// 把一批（一条或一天的）请求并进按账号的累计表。
///
/// ── 身份只认 account_id ─────────────────────────────────────
/// 匹配按 `account_id` 走，**名字不参与判等**：账号可以在账号页改名，
/// 按名字匹配会让同一个账号在改名前后的两条聚合行分家，报表里出现两个半份的
/// 「同一账号」。名字在这里只承担一件事：**首次建组时留下的展示名快照**。
/// 后续同一账号的累计里若带了新名字，就更新快照（账号改名后报表跟着显示新名字，
/// 这也正是账号页的观感），而不是保留旧名。
///
/// ── 空 id 的处理 ─────────────────────────────────────────────
/// 与 `push_provider_accum` 同一取舍：`account_id` 为空串时**照常建组**，不丢弃 ——
/// 走默认登录态转发的请求、以及旧版本写出的行都落在这一组里，
/// 丢掉它们会让「各账号之和」对不上区间总量。
pub(super) fn push_account_accum(
    list: &mut Vec<AccountAccum>,
    account_id: &str,
    account_name: &str,
    requests: i64,
    successful: i64,
    tokens: i64,
) {
    match list.iter_mut().find(|item| item.account_id == account_id) {
        Some(item) => {
            item.requests += requests;
            item.successful += successful;
            item.tokens += tokens;
            // 名字只做「快照刷新」：非空才覆盖，绝不清空已有的名字
            // （某些请求拿不到名字而另一些拿到了，取有名字的那一份）
            if !account_name.is_empty() {
                item.account_name = account_name.to_string();
            }
        }
        None => list.push(AccountAccum {
            account_id: account_id.to_string(),
            account_name: account_name.to_string(),
            requests,
            successful,
            tokens,
        }),
    }
}

/// provider id → 展示 label（中文名）。
///
/// 走 `providers::meta` 的注册表换算，**不在这里维护第二份 id→label 映射**：
/// 注册表是 provider 身份的唯一事实来源，两处映射迟早漂移，而这类漂移不会报错，
/// 只会让报表里的名字与账号页对不上。
///
/// 未注册的 id（前端比后端新、或手改文件塞进来的值）**原样返回**：
/// 显示一个陌生的英文 id 好过显示空白，至少能提示「这是谁」。
/// 空串同样原样返回（空 label），由展示层决定用什么占位（当前约定是「—」）——
/// 这一层只回答「id 对应什么名字」，不做展示决策。
pub(super) fn provider_label(id: &str) -> String {
    if id.is_empty() {
        return String::new();
    }
    match crate::server::core::providers::kind_from_id(id) {
        Some(kind) => crate::server::core::providers::meta(kind).label.to_string(),
        None => id.to_string(),
    }
}

/// 区间级的按 provider 汇总 → `/api/stats/summary` 的 `providers` 数组。
///
/// 排序：`requests` 降序（契约要求）；并列时按 `totalTokens` 降序、
/// 再并列按 id 升序 —— 后两级纯粹为了让结果稳定（同一份数据多次调用顺序一致），
/// 前端做「只显示前 N 家」的截断时不会每次截到不同的行。
///
/// 空 id 那一组的 label 用「未知」而不是空串：这份数组是**给图表用的**，
/// 每段都要有可显示的图例名；空 label 在图例里就是个看不见的空白项。
/// （明细行那边的空 provider 反而保持空 label，好让展示层用「—」占位 ——
/// 两边形态不同是因为用途不同，不是遗漏。）
pub(super) fn build_providers(totals: &[ProviderAccum]) -> Vec<Value> {
    // 全零的组不返回：它只会出现在手改过的文件里，展示一个「0 次请求」的
    // 图例项没有意义（正常写入路径不会造出这种组）
    let mut rows: Vec<&ProviderAccum> = totals
        .iter()
        .filter(|item| item.requests > 0 || item.tokens > 0)
        .collect();
    rows.sort_by(|left, right| {
        right
            .requests
            .cmp(&left.requests)
            .then(right.tokens.cmp(&left.tokens))
            .then(left.provider.cmp(&right.provider))
    });
    rows.into_iter()
        .map(|item| {
            json!({
                "id": item.provider,
                "label": if item.provider.is_empty() {
                    UNKNOWN_PROVIDER_LABEL.to_string()
                } else {
                    provider_label(&item.provider)
                },
                "requests": item.requests,
                // failures 由减法得出而不是另存一列：见 record::ProviderAccum 的注释。
                // max(0) 兜住手改文件造成的负数
                "success": item.successful,
                "failures": (item.requests - item.successful).max(0),
                "totalTokens": item.tokens,
            })
        })
        .collect()
}

/// 未知模型的展示名（模型名为空串的那一组）。
///
/// 与 provider / 账号那两条是**三个独立的常量**：三者的「未知」成因各不相同
/// （没记下承载的家 / 没走账号列表 / 没记下模型名），文案将来若分叉，
/// 共用会让改一处等于改两处。
pub(super) const UNKNOWN_MODEL_LABEL: &str = "未知模型";

/// 区间级的按模型汇总 → `/api/stats/summary` 的 `models` 数组。
///
/// 与 `build_providers` / `build_accounts` 同形同序（requests 降序 → tokens 降序
/// → 名字升序）。模型维度比那两维更早存在（`model_totals` 一直是
/// `range_totals` 的一部分，`topModel` 就是从它算出来的），但此前只对外暴露了
/// 冠军一个 —— 这里把整张表给出去，供报表的「模型用量」环形图使用。
///
/// ── 与 providers 的两处差异 ─────────────────────────────────
///   1. **空模型名的组照常返回**（label 为「未知模型」）：`build_top_model` 特意
///      把空名字排除在冠军评选之外（避免标题栏显示空白），但这里是要画一张
///      「用量都花在哪」的图，那一组代表的真实用量不该从图上消失 ——
///      它会让各扇区之和对不上 `overview.tokens`。
///   2. 没有 `label` 与 `id` 的区分：模型名既是身份也是展示名，一个字段就够。
///
/// 全零的组仍然滤掉（与另两维一致）：只可能来自手改过的数据，
/// 画出来是一段永远为 0 的扇区。
pub(super) fn build_models(totals: &[ModelAccum]) -> Vec<Value> {
    let mut rows: Vec<&ModelAccum> = totals
        .iter()
        .filter(|item| item.requests > 0 || item.tokens > 0)
        .collect();
    rows.sort_by(|left, right| {
        right
            .requests
            .cmp(&left.requests)
            .then(right.tokens.cmp(&left.tokens))
            .then(left.model.cmp(&right.model))
    });
    rows.into_iter()
        .map(|item| {
            json!({
                "model": item.model,
                "label": if item.model.is_empty() {
                    UNKNOWN_MODEL_LABEL.to_string()
                } else {
                    item.model.clone()
                },
                "requests": item.requests,
                "totalTokens": item.tokens,
            })
        })
        .collect()
}

/// 区间级的按账号汇总 → `/api/stats/summary` 的 `accounts` 数组。
///
/// 与 `build_providers` 同形同序（requests 降序 → tokens 降序 → 身份升序）：
/// 两张排行并排显示时，排序口径一致才不会被误读成「一个按请求数、一个按别的」。
///
/// ── 与 providers 的两处差异 ─────────────────────────────────
///   1. 身份字段是 `id` + `label` 两个：provider 的 `label` 由注册表**现算**，
///      而账号的展示名是**聚合时留下的快照**（账号可能已被删除，注册表里查不到）。
///      取名字的优先级是「快照名 → id」，两者都空的那一组用「未知账号」。
///   2. 没有 `providerLabel` 那种「id → 现算 label」的派生：账号名不是派生值，
///      就是数据本身，所以直接原样给出。
pub(super) fn build_accounts(totals: &[AccountAccum]) -> Vec<Value> {
    // 与 build_providers 同一取舍：全零的组不返回
    let mut rows: Vec<&AccountAccum> = totals
        .iter()
        .filter(|item| item.requests > 0 || item.tokens > 0)
        .collect();
    rows.sort_by(|left, right| {
        right
            .requests
            .cmp(&left.requests)
            .then(right.tokens.cmp(&left.tokens))
            .then(left.account_id.cmp(&right.account_id))
    });
    rows.into_iter()
        .map(|item| {
            // 展示名取「快照名 → id」；两者都空才是真正的未知（默认登录态转发）
            let label = if !item.account_name.is_empty() {
                item.account_name.clone()
            } else if !item.account_id.is_empty() {
                item.account_id.clone()
            } else {
                UNKNOWN_ACCOUNT_LABEL.to_string()
            };
            json!({
                "id": item.account_id,
                "label": label,
                "requests": item.requests,
                "success": item.successful,
                "failures": (item.requests - item.successful).max(0),
                "totalTokens": item.tokens,
            })
        })
        .collect()
}

/// 明细行 → 响应 JSON：在序列化结果上补一个**派生字段** `providerLabel`。
///
/// 为什么不在 `RequestEntry` 上加一个字段：契约里 `providerLabel` 是给前端直接
/// 显示的（§6「记账展示的 provider 用 label」），它由 id 换算而来、会随注册表
/// 变化，而 `RequestEntry` 是**落盘格式**——把派生值写进文件会让
/// 「以后改了 label，历史行仍是旧名字」。
///
/// 序列化失败理论上不可能（字段全是 i64 / String / Option<String>），但仍然
/// 不做 unwrap（panic=abort 下会带走整个应用）：退化成一个空对象，
/// 前端那一行显示空白，比进程消失好。
pub(super) fn entry_json(entry: &RequestEntry) -> Value {
    let Ok(mut value) = serde_json::to_value(entry) else {
        return Value::Object(serde_json::Map::new());
    };
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "providerLabel".to_string(),
            Value::String(provider_label(&entry.provider)),
        );
    }
    value
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

/// 近 24 个**本地整点**的命中率与用量趋势（无数据的整点补零）。
///
/// 按本地时区切整点：先把时间戳还原成 `DateTime<Local>` 再取 `%H`，
/// 于是「14 点」就是用户时钟上的 14 点，不是 UTC 的 14 点。
///
/// 每个整点同时给出 `totalTokens`：前端那一张图要画**两条线**
/// （命中率 + 总 Token），两段数据必须同源同桶 —— 分成两次请求会让两条线
/// 在「这一小时算不算」上出现分歧，看起来像其中一条错位了一格。
pub(super) fn cache_trend_24h(entries: &[RequestEntry], now: i64) -> Vec<Value> {
    let start = hour_floor(ms_to_local(now)) - ChronoDuration::hours(23);
    let start_ms = start.timestamp_millis();

    // 先把窗口内的条目按整点键归桶，再按 24 个整点取值 ——
    // 逐个整点扫一遍明细是 24×N，这样只需一次遍历
    let mut buckets: HashMap<String, (i64, i64, i64)> = HashMap::new();
    for item in entries {
        if item.ts < start_ms || item.ts > now {
            continue;
        }
        let slot = buckets.entry(hour_key(ms_to_local(item.ts))).or_insert((0, 0, 0));
        slot.0 += item.cache_read_tokens;
        slot.1 += item.prompt_tokens;
        slot.2 += item.total_tokens;
    }

    let mut out = Vec::with_capacity(24);
    for step in 0..24 {
        let key = hour_key(start + ChronoDuration::hours(step));
        let (hit, input, total) = buckets.get(&key).copied().unwrap_or((0, 0, 0));
        out.push(json!({
            "hour": key,
            "hitTokens": hit,
            "inputTokens": input,
            "totalTokens": total,
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

/// status 过滤归一的**结果**：三种取值各自对应一个 SQL 条件（见
/// `sql::FilterPlan::of` 里逐个的写法）。
///
/// 为什么从「`Option<bool>`」扩成枚举：原来只有 ok / error 两个值，bool 够用；
/// 「进行中」（running，status=0）加入后是第三种互斥状态 —— 硬塞 bool 要么
/// 让调用方先判字符串再判 bool，要么把 running 混进 error 的取反里。
/// 三分支枚举让「漏处理新分支」在编译期就暴露。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StatusFilter {
    /// 只看成功（2xx 且无错误摘要）
    Ok,
    /// 只看失败（成功条件的取反；**不含**进行中行，见 `sql::FilterPlan::of`）
    Error,
    /// 只看进行中（status = 0，见 `request_stats::record_started`）
    Running,
}

/// status 过滤归一：`"ok"` → 成功，`"error"` → 失败，`"running"` → 进行中，
/// 其余（含 None 与拼错的值）不过滤。大小写不敏感、容忍前后空白。
pub(super) fn normalize_status_filter(value: Option<&str>) -> Option<StatusFilter> {
    match value.map(str::trim).map(str::to_lowercase).as_deref() {
        Some("ok") => Some(StatusFilter::Ok),
        Some("error") => Some(StatusFilter::Error),
        Some("running") => Some(StatusFilter::Running),
        _ => None,
    }
}

/// `YYYY-MM-DD` → NaiveDate（手改文件里的坏值返回 None，调用方各自兜底）
fn parse_key(text: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(text.trim(), "%Y-%m-%d").ok()
}
