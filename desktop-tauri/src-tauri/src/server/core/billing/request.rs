//! 计费请求的构造素材：端点表、调用选项、请求头与 JS 语义工具。
//!
//! 从 billing/mod.rs 拆出（单文件行数约定）。这里的东西**只被同层的
//! usage.rs / activity.rs / mod.rs 用**，不对外暴露。
//!
//!   - `BillingSpec` / 各端点常量：URL、方法、固定请求体、是否要白名单头
//!   - `CallOptions` / `BillingCall`：callBilling 的入参与返回
//!   - `build_headers` / `whitelist_headers`：计费接口的头集合
//!   - 一串 JS 语义工具（真值判定、Number()、parseTime 的时区语义…）

use serde_json::{json, Map, Value};

use crate::server::core::endpoints::{resolve_edition, user_agent_for_edition};

// ─── 请求描述 ───────────────────────────────────────────────

/// 一个计费端点的描述（对应 Node 版 BILLING / ACTIVITY 表里的条目）
pub(super) struct BillingSpec {
    pub(super) method: &'static str,
    pub(super) path: &'static str,
    /// 固定请求体（Node 的 `spec.body`；userResource 是那个 PageNumber 常量体）。
    /// 用函数而不是常量值：`json!` 无法在 const 上下文求值。
    pub(super) body: fn() -> Value,
    /// 是否需要客户端白名单头（只有 banner 需要）
    pub(super) whitelist_headers: bool,
}

impl BillingSpec {
    pub(super) fn body(&self) -> Value {
        (self.body)()
    }
}

/// 没有固定请求体的端点：Node 里 `spec.body` 缺省 → `{}`
fn empty_body() -> Value {
    json!({})
}

/// 个人积分包的固定请求体（对照 workbuddy-endpoints.mjs 的 BILLING.userResource.body，
/// ProductCode=p_tcaca 为 WorkBuddy 产品码）
fn user_resource_body() -> Value {
    json!({
        "PageNumber": 1,
        "PageSize": 100,
        "ProductCode": "p_tcaca",
        "Status": [0, 3],
        "OnlyValidPeriod": true,
    })
}

/// 调用选项：会话、请求体、查询串、是否要求 code===0、语言
pub(super) struct CallOptions<'a> {
    pub(super) session: Option<&'a Value>,
    pub(super) body: Option<&'a Value>,
    pub(super) query: Option<&'a str>,
    /// false 时非 0 code 也返回（签到重复领取要读 msg）
    pub(super) expect_code_ok: bool,
    pub(super) locale: Option<&'a str>,
}

impl Default for CallOptions<'_> {
    fn default() -> Self {
        Self {
            session: None,
            body: None,
            query: None,
            // Node 的默认值是 true（`expectCodeOk = true`）
            expect_code_ok: true,
            locale: None,
        }
    }
}

/// callBilling 的返回（对应 Node 的 `{ data, code, msg, requestId, raw }`）
pub(super) struct BillingCall {
    pub(super) code: Option<i64>,
    pub(super) msg: Option<String>,
    /// 上游 requestId；None 表示上游没给（签到的失败分支据此不出这个键）
    pub(super) request_id: Option<Value>,
    pub(super) data: Value,
    /// 原始 payload（企业额度那条路径要遍历 data.data 等嵌套字段）
    pub(super) raw: Option<Value>,
}

// 端点表（对照 workbuddy-endpoints.mjs 的 BILLING / ACTIVITY；不带 prefixPath）

pub(super) const BILLING_CHECKIN_STATUS: BillingSpec = BillingSpec {
    method: "POST",
    path: "/v2/billing/meter/checkin-activity-status",
    body: empty_body,
    whitelist_headers: false,
};

pub(super) const BILLING_DAILY_CHECKIN: BillingSpec = BillingSpec {
    method: "POST",
    path: "/v2/billing/meter/daily-checkin",
    body: empty_body,
    whitelist_headers: false,
};

pub(super) const BILLING_USER_RESOURCE: BillingSpec = BillingSpec {
    method: "POST",
    path: "/v2/billing/meter/get-user-resource",
    body: user_resource_body,
    whitelist_headers: false,
};

pub(super) const BILLING_ENTERPRISE_USAGE: BillingSpec = BillingSpec {
    method: "POST",
    path: "/v2/billing/meter/get-enterprise-user-usage",
    body: empty_body,
    whitelist_headers: false,
};

/// 用量提示端点（Node 版 BILLING.dosageNotify 的对等物）。
/// 由 `usage.rs` 的 `get_dosage_notify` 使用（该方法当前无生产调用点）。
pub(super) const BILLING_DOSAGE_NOTIFY: BillingSpec = BillingSpec {
    method: "POST",
    path: "/v2/billing/meter/get-dosage-notify",
    body: empty_body,
    whitelist_headers: false,
};

pub(super) const ACTIVITY_BANNER: BillingSpec = BillingSpec {
    method: "GET",
    path: "/v2/activity/workbuddy/banner",
    body: empty_body,
    whitelist_headers: true,
};

pub(super) const ACTIVITY_AMBASSADOR: BillingSpec = BillingSpec {
    method: "GET",
    path: "/v2/activity/ambassador/status",
    body: empty_body,
    whitelist_headers: false,
};

// ─── 请求头 ─────────────────────────────────────────────────

/// AuthService.buildHeaders(session) 的等价实现 + extra 条件头。
///
/// 与 `auth::build_auth_headers` 的差别（注意，两者**不能互相替换**）：
///   - 计费版固定带 `Content-Type: application/json` 与 `Accept: application/json`
///   - 计费版**不**带 `X-Department-Info`（转发需要，计费不需要）
///   - 计费版 `X-User-Id` 缺失时给空串（转发版直接不给这个头）
///   - 计费版无 Authorization 时也会给 `Bearer `（转发版跳过）
///
/// 这些差别是 Node 版两个 buildHeaders 各自的实现决定的，逐条保留。
pub(super) fn build_headers(session: &Value, extra: &[(String, String)]) -> Vec<(String, String)> {
    let auth = session.get("auth").cloned().unwrap_or(Value::Null);
    let account = session.get("account").cloned().unwrap_or(Value::Null);
    let access_token = auth
        .get("accessToken")
        .and_then(Value::as_str)
        .unwrap_or("");

    let mut headers: Vec<(String, String)> = vec![
        ("Accept".to_string(), "application/json".to_string()),
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Authorization".to_string(), format!("Bearer {access_token}")),
        (
            "X-User-Id".to_string(),
            account.get("uid").and_then(Value::as_str).unwrap_or("").to_string(),
        ),
    ];
    // extra 在 Node 里是 `...extra` 插在 X-User-Id 之后的展开，
    // 因此同名的情况不存在（extra 只有 Accept-Language 与白名单头）
    for (key, value) in extra {
        headers.push((key.clone(), value.clone()));
    }
    if let Some(enterprise_id) = account
        .get("enterpriseId")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    {
        headers.push(("X-Enterprise-Id".to_string(), enterprise_id.to_string()));
        headers.push(("X-Tenant-Id".to_string(), enterprise_id.to_string()));
    }
    if let Some(domain) = auth
        .get("domain")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    {
        headers.push(("X-Domain".to_string(), domain.to_string()));
    }
    headers
}

/// 运营活动接口需要的客户端白名单头（getActivityBanner 专用）；
/// 按账号版本区分身份 —— 国际版与国内版的 productName / 版本号不同。
pub(super) fn whitelist_headers(session: &Value) -> Vec<(String, String)> {
    let edition = session.get("edition").and_then(Value::as_str);
    let info = resolve_edition(edition);
    vec![
        ("User-Agent".to_string(), user_agent_for_edition(edition)),
        ("X-IDE-Type".to_string(), info.ua_platform.to_string()),
        ("X-IDE-Name".to_string(), info.product_name.to_string()),
        ("X-IDE-Version".to_string(), info.client_version.to_string()),
        ("X-Product".to_string(), info.product_name.to_string()),
    ]
}

// ─── 工具函数 ───────────────────────────────────────────────

/// JS 真值判定（`Boolean(x)`）：null/false/0/"" 为假，空数组/空对象为真
pub(super) fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().map(|item| item != 0.0).unwrap_or(false),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// JS `Number(x) || 0`：非数字/NaN/0/缺失都得到 0
pub(super) fn number_or_zero(value: Option<&Value>) -> f64 {
    let number = match value {
        Some(Value::Number(number)) => number.as_f64(),
        Some(Value::String(text)) => text.trim().parse::<f64>().ok(),
        _ => None,
    };
    match number {
        Some(number) if number.is_finite() => number,
        _ => 0.0,
    }
}

/// JS `toInt(value)`：`Math.floor(Number(value))`，NaN → null
pub(super) fn to_int(value: &Value) -> Option<i64> {
    let number = match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    };
    match number {
        Some(number) if number.is_finite() => Some(number.floor() as i64),
        _ => None,
    }
}

/// 解析时间字段为毫秒时间戳（对照 Node 版 `parseTime` 的**原始返回值**）。
///
/// 语义逐条对齐：
///   - 缺失 / null / 空串 → 0（不是 None）
///   - 纯数字字符串（`/^\d+$/`）→ 数字本身（上游有时把时间戳给成字符串）
///   - 其余交给 `new Date(x).getTime()`；解析不出 → 0
///
/// ── 时区语义必须逐字对齐（实测踩过）────────────────────────
/// JS 的 `new Date(str)` 对**无时区**的格式分两类，差 8 小时（东八区实测）：
///   `"2026-09-30"`（仅日期）      → **UTC** 零点（ECMAScript 规范规定）
///   `"2026-09-30 23:59:59"`       → **本地时区**（实现相关的宽松解析）
///   `"2026-09-30T23:59:59"`       → **本地时区**
/// 上游的 `CycleEndTime` / `CycleStartTime` 正是 `"YYYY-MM-DD HH:MM:SS"` 形态，
/// 若在这里按 UTC 解析，`refreshAt` 会整体偏 8 小时，前端显示的「额度刷新时间」
/// 就与 Node 版对不上。因此这里对「无时区的日期时间」用 `Local` 解析，
/// 「仅日期」用 UTC。
///
/// 返回 i64 而不是 Option：调用方需要的正是「0 为失败哨兵」这件事，
/// 好让 `|| 0` / `|| null` 这些 JS 假值判断在 Rust 侧一目了然。
pub(super) fn parse_time(value: Option<&Value>) -> i64 {
    time_or_null(value).unwrap_or(0)
}

/// `parseTime(x) || null` 的等价物：解析不出（得到 0）时给 None。
///
/// 单独一个函数是因为 Node 里这两种写法混用：`parseTime(x) || parseTime(y)`
/// 会跳过第一个解不出的，而 `parseTime(x) || 0` 会退化成 0。把「0 = 失败」
/// 显式化成 None 之后，Rust 侧的 `or_else` / `unwrap_or` 就能一一对应上去。
pub(super) fn time_or_null(value: Option<&Value>) -> Option<i64> {
    let text = match value {
        None | Some(Value::Null) => return None,
        Some(Value::Number(number)) => {
            let parsed = number.as_f64()? as i64;
            return if parsed == 0 { None } else { Some(parsed) };
        }
        Some(Value::String(text)) => text.trim().to_string(),
        Some(_) => return None,
    };
    if text.is_empty() {
        return None;
    }
    // `/^\d+$/` 的等价判断：全数字按时间戳读
    let parsed = if text.chars().all(|ch| ch.is_ascii_digit()) {
        text.parse::<i64>().ok()
    } else {
        parse_date_text(&text)
    };
    // 0 是「解不出」的哨兵值（与 parseTime 返回 0 等价）
    parsed.filter(|value| *value != 0)
}

/// 日期文本 → 毫秒时间戳，时区语义与 JS 的 `new Date(str)` 对齐（见 `parse_time`）。
///
/// 只覆盖上游真会给到的几种形态；认不出的一律 None（JS 会得到 NaN → 0）。
fn parse_date_text(text: &str) -> Option<i64> {
    use chrono::{Local, NaiveDate, NaiveDateTime, TimeZone, Utc};

    // 带时区偏移：交给 chrono 直接按绝对时间解析（与 JS 一致）
    if let Ok(value) = chrono::DateTime::parse_from_rfc3339(text) {
        return Some(value.timestamp_millis());
    }
    // 仅日期：JS 规范规定按 **UTC** 解读（不是本地！）
    if let Ok(date) = NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        return date
            .and_hms_opt(0, 0, 0)
            .map(|value| value.and_utc().timestamp_millis());
    }
    // 无时区的日期时间：按**本地时区**解读（"YYYY-MM-DD HH:MM:SS" 与 ISO 的 T 形态）
    let naive = NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S")
        .or_else(|_| NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S"))
        .ok()?;
    // 本地时刻可能因夏令时不存在（春季跳变）或重复（秋季回落）：
    // 不存在时退化为 UTC 解读（宁可偏一点，也不要整条链路上报错）
    Local
        .from_local_datetime(&naive)
        .single()
        .map(|value| value.timestamp_millis())
        .or_else(|| Some(Utc.from_utc_datetime(&naive).timestamp_millis()))
}

/// 时间戳 → JSON（None → null；整数形态）
pub(super) fn timestamp_json(value: Option<i64>) -> Value {
    value.map(Value::from).unwrap_or(Value::Null)
}

/// JS `String(Math.floor(n))` 的近似：整数输出整数形态，
/// 浮点（理论上是小数额度）保留一位小数 —— 上游额度是整数，
/// 这里只是不让 `0.0` 这种形态漏到界面上。
pub(super) fn js_int_string(value: f64) -> String {
    if !value.is_finite() {
        return "0".to_string();
    }
    if value.fract() == 0.0 {
        format!("{}", value as i64)
    } else {
        let text = format!("{value}");
        text.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// 签到结果归一化。上游字段随活动配置变化，这里做「宽进」处理：
/// 保留原始 data，同时尽力抽出常见字段供 UI 直接展示。
///
/// ── 一个容易踩的细节：undefined 的键要**整个丢掉** ──────────
/// Node 的 `pick(...)` 全都找不到时返回 `undefined`，而 `JSON.stringify`
/// 会丢掉值为 undefined 的键。所以真实响应里 `checkedIn` / `online` 在
/// 上游没这些字段时**根本不出现在 JSON 里**（而不是 null）。前端用
/// `'checkedIn' in result` 之类的判空会因此分叉，所以这里也只在该键有值时放进去。
///
/// 相反，`toInt(pick(...))` 那条路径返回的是 `null`
/// （`Number.isFinite(NaN)` 为假 → 显式 null），因此天数/积分这类**数值键
/// 永远存在**，值可能是 null。两种语义不能混。
pub(super) fn normalize_checkin(data: &Value) -> Value {
    if !data.is_object() {
        return data.clone();
    }
    // `pick(...)` 的等价物：按候选键名取第一个**存在且非 null** 的值
    let pick = |keys: &[&str]| -> Option<Value> {
        for key in keys {
            if let Some(value) = data.get(*key) {
                if !value.is_null() {
                    return Some(value.clone());
                }
            }
        }
        None
    };
    // `toInt(pick(...))`：解不出时是 null（键仍然存在）
    let int_of = |keys: &[&str]| -> Value {
        pick(keys)
            .and_then(|value| to_int(&value))
            .map(Value::from)
            .unwrap_or(Value::Null)
    };

    let mut result = Map::new();
    // 今日是否已签到（pick 失败 → 不出键）
    if let Some(value) = pick(&[
        "checked_in",
        "checkedIn",
        "is_checked_in",
        "isCheckedIn",
        "signed",
        "is_signed",
    ]) {
        result.insert("checkedIn".to_string(), value);
    }
    // 连续签到天数（toInt 失败 → null，键保留）
    result.insert(
        "continuousDays".to_string(),
        int_of(&[
            "continuous_days",
            "continuousDays",
            "continuous_checkin_days",
            "streak",
            "serial_days",
        ]),
    );
    // 累计签到天数
    result.insert(
        "totalDays".to_string(),
        int_of(&["total_days", "totalDays", "total_checkin_days", "accumulate_days"]),
    );
    // 本次/今日可领积分
    result.insert(
        "points".to_string(),
        int_of(&["points", "credit", "reward_points", "rewardPoints", "daily_points"]),
    );
    // 活动周期 / 活动是否在线：同 checkedIn，取不到就不出键
    if let Some(value) = pick(&["start_time", "startTime"]) {
        result.insert("startTime".to_string(), value);
    }
    if let Some(value) = pick(&["end_time", "endTime"]) {
        result.insert("endTime".to_string(), value);
    }
    if let Some(value) = pick(&["activity_online_status", "activityOnlineStatus", "online"]) {
        result.insert("online".to_string(), value);
    }
    result.insert("raw".to_string(), data.clone());
    Value::Object(result)
}
