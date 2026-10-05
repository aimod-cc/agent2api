//! LobsterAI 的额度（积分）查询（feat/lobster-provider PR-2；移植来源
//! 参考实现 的 `_fetch_credit_data` / `quota_summary`，按本仓 `raccoon::balance`
//! 的契约形状重写）。
//!
//! ── 上游长什么样 ────────────────────────────────────────────
//!   - 主口径：`GET /api/user/profile-summary`，响应 `{code:0, data:{
//!     totalCreditsRemaining, creditItems:[{type,label,creditsRemaining,
//!     expiresAt}], availableResetCount, availablePromoSubscriptionCount}}`
//!     —— 积分**真余额**（含签到/邀请/限时礼的批次明细）。
//!   - 回落：`GET /api/user/quota`（老批次口径）：`data` 里 limit/free/
//!     monthly/daily/creditsLimit 多形态额度分支。分支次序照抄 参考实现
//!     `_normalize_quota`（移植自 App 端 normalizeAuthQuota）。
//!
//! ── 归一形状（`ProviderAdapter::query_usage` 的契约）──────────
//! `available` = credits_remaining（服务端显式给的 Remaining 优先于
//! total-used 推导 —— 积分过期时 remaining=0 而 total-used>0，必须以服务端
//! 为准）；`wallets` = creditItems 批次明细；`raw` 保留上游原始响应。
//!
//! ── 缓存 ────────────────────────────────────────────────────
//! 60 秒 TTL 进程级缓存（参考实现 `_QUOTA_CACHE_TTL_S`）：额度是高频轮询路径
//! （账号页每几秒刷一次），不加缓存会把上游打热。**成功与失败都缓存**：
//! 上游挂了时，接下来 60 秒的轮询不该每次都真打一次。
//!
//! ── 401 不走缓存 ────────────────────────────────────────────
//! 凭证过期是「调用方刷一下就能修」的情形（`query_usage_inner` 的刷新重试
//! 链路），把它缓存 60 秒只会让「本可以马上修好的查询」白等一分钟。
//!
//! ── panic=abort ────────────────────────────────────────────
//! 零 unwrap/expect/panic。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::core::auth_http::send_raw;
use crate::server::errors::GatewayError;

use super::credentials;

/// 额度接口超时（短请求；参考实现 同值 15s）
const REQUEST_TIMEOUT_MS: u64 = 15_000;

/// 额度缓存 TTL（参考实现 `_QUOTA_CACHE_TTL_S`）
const QUOTA_CACHE_TTL_MS: i64 = 60_000;

/// 一次额度请求的失败形态（401 单独一档，见模块头「401 不走缓存」）
enum QueryFailure {
    /// 凭证被上游拒绝（401/403）→ 调用方走刷新重试
    AuthExpired(String),
    /// 其它失败（网络 / 非 2xx / 业务码非 0）
    Failed(String),
}

/// 进程级额度缓存（key = 账号 id；值 = (拉取时刻, Ok(归一结果) | Err(失败文案))）
///
/// 成功与失败都缓存（401 除外，见模块头）；失败也缓存后，被删账号/陈旧账号的
/// 条目靠容量上限兜底（超出即整表重建，额度缓存丢了只是多打一次上游）。
fn quota_cache() -> &'static Mutex<HashMap<String, (i64, Result<Value, String>)>> {
    static CACHE: OnceLock<Mutex<HashMap<String, (i64, Result<Value, String>)>>> =
        OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 缓存容量上限（历史出现过的不同账号 id 数远小于此；超出整表重建）
const QUOTA_CACHE_MAX_ENTRIES: usize = 64;

/// 查询某账号的积分余额（归一化形状见 `ProviderAdapter::query_usage` 的文档）。
///
/// 凭证取 `snapshot_for`（完整凭证但不触发刷新 —— 余额是只读展示动作，
/// 为它消耗一次 refreshToken 轮换不划算；过期让上游 401，由调用方走
/// 刷新重试那条既有链路）。401 原样透出（`query_usage_inner` 据此刷新后重试）。
pub(super) async fn query_usage(
    store: &AccountStore,
    account_id: &str,
) -> Result<Value, GatewayError> {
    let cache_key = if account_id.is_empty() {
        // 没点名账号：用队首记录的 id 做缓存键；连记录都没有时用桌面标记
        store
            .lobster_account_record("")
            .and_then(|record| record.get("id").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_else(|| credentials::DESKTOP_ACCOUNT_ID.to_string())
    } else {
        account_id.to_string()
    };
    {
        let now = crate::server::logging::now_ms();
        let cached = quota_cache().lock().ok().and_then(|guard| {
            guard.get(&cache_key).filter(|(fetched_at, _)| {
                // fetched_at <= now 防时钟回拨:负年龄恒小于 TTL,会把旧值当永新
                *fetched_at <= now && now - *fetched_at < QUOTA_CACHE_TTL_MS
            }).map(|(_, value)| value.clone())
        });
        match cached {
            Some(Ok(value)) => return Ok(value),
            Some(Err(message)) => return Err(GatewayError::with_status(502, message)),
            None => {}
        }
    }
    let credentials = credentials::snapshot_for(store, account_id)?;
    if credentials.access_token.trim().is_empty() {
        return Err(GatewayError::with_status(
            401,
            "该账号没有可用凭证，无法查询积分",
        ));
    }
    let outcome = fetch_credit_data(&credentials.access_token).await;
    // 401 不缓存(模块头契约:调用方要走刷新重试);其余成功/失败都缓存
    if !matches!(outcome, Err(QueryFailure::AuthExpired(_))) {
        let cached = match &outcome {
            Ok(value) => Ok(value.clone()),
            Err(QueryFailure::Failed(message)) => Err(message.clone()),
            Err(QueryFailure::AuthExpired(_)) => unreachable!(),
        };
        if let Ok(mut guard) = quota_cache().lock() {
            if guard.len() >= QUOTA_CACHE_MAX_ENTRIES {
                guard.clear();
            }
            guard.insert(cache_key, (crate::server::logging::now_ms(), cached));
        }
    }
    match outcome {
        Ok(value) => Ok(value),
        Err(QueryFailure::AuthExpired(message)) => Err(GatewayError::with_status(401, message)),
        Err(QueryFailure::Failed(message)) => Err(GatewayError::with_status(502, message)),
    }
}

/// 签到领取成功后失效额度缓存（下次查询拉新余额）
pub(super) fn invalidate_quota_cache(account_id: &str) {
    if let Ok(mut guard) = quota_cache().lock() {
        guard.remove(account_id);
        // 队首路径的缓存键是记录 id，桌面兜底键也一并清（代价为零，漏清代价是
        // 「签到成功但面板 60 秒内还是旧余额」）
        guard.remove(credentials::DESKTOP_ACCOUNT_ID);
    }
}

/// 拉积分：profile-summary 主轨，quota 回落。
async fn fetch_credit_data(token: &str) -> Result<Value, QueryFailure> {
    // 主轨「请求失败」与「响应无可解析余额」统一折成同一档失败再回落 quota
    // (上游第 3 轮·发现 2:解析失败产生在 Ok 分支内部,旧的 match 结构下
    // 不会再进 Err 分支,回落从未真正发生——探针实测只发了一次请求)
    let main_failure: QueryFailure = match request_json("/api/user/profile-summary", token).await {
        Ok(data) => match normalize_profile_summary(&data) {
            Some(value) => return Ok(value),
            None => QueryFailure::Failed("profile-summary 响应无可解析余额".to_string()),
        },
        Err(QueryFailure::AuthExpired(message)) => return Err(QueryFailure::AuthExpired(message)),
        Err(failure) => failure,
    };
    match request_json("/api/user/quota", token).await {
        Ok(data) => Ok(normalize_quota(&data)),
        Err(QueryFailure::AuthExpired(message)) => Err(QueryFailure::AuthExpired(message)),
        Err(_) => Err(main_failure),
    }
}

/// 带鉴权 GET，解包 `{code, message, data}`（`data` 缺失时整个 payload 视作 data）
async fn request_json(path: &str, token: &str) -> Result<Value, QueryFailure> {
    let url = format!("{}{path}", super::DEFAULT_LLM_BASE_URL);
    // 用户粘贴的 token 可能自带 Bearer 前缀，重复拼会得到 `Bearer Bearer xxx`
    let bearer = token
        .trim()
        .trim_start_matches("Bearer ")
        .trim_start_matches("bearer ")
        .trim();
    let headers = vec![
        ("Accept".to_string(), "application/json".to_string()),
        ("Authorization".to_string(), format!("Bearer {bearer}")),
        ("Cache-Control".to_string(), "no-store".to_string()),
    ];
    let response = send_raw("GET", &url, None, &headers, None, Some(REQUEST_TIMEOUT_MS))
        .await
        .map_err(|error| {
            if error.is_timeout() {
                QueryFailure::Failed("积分查询超时".to_string())
            } else {
                QueryFailure::Failed(format!("积分查询失败: {error}"))
            }
        })?;
    if response.status == 401 || response.status == 403 {
        return Err(QueryFailure::AuthExpired(
            "登录态已过期，无法查询积分".to_string(),
        ));
    }
    if !response.ok {
        return Err(QueryFailure::Failed(format!(
            "积分查询返回 HTTP {}",
            response.status
        )));
    }
    let payload = response.payload.unwrap_or(Value::Null);
    if let Some(code) = payload.get("code").and_then(Value::as_i64) {
        if code != 0 {
            let message = payload
                .get("message")
                .or_else(|| payload.get("msg"))
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("积分查询返回异常 code={code}"));
            return Err(QueryFailure::Failed(message));
        }
    }
    let data = payload.get("data").cloned().unwrap_or(Value::Null);
    Ok(if data.is_null() { payload } else { data })
}

/// profile-summary → 归一形状（creditItems 批次明细按剩余量降序）；
/// 总额与批次都解析不出时返回 None（调用方回落 quota，不把「无法解析」报成 0）。
fn normalize_profile_summary(data: &Value) -> Option<Value> {
    let items = normalize_credit_items(data.get("creditItems"));
    let explicit_total = data
        .get("totalCreditsRemaining")
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite());
    if explicit_total.is_none() && items.is_empty() {
        return None;
    }
    let total = explicit_total
        .unwrap_or_else(|| {
            items
                .iter()
                .filter_map(|item| item.get("balance").and_then(Value::as_f64))
                .sum()
        })
        // 与 quota 口径同款钳制:负余额按 0 展示(欠费态由 raw 可辨)
        .max(0.0);
    // 字典序取最小:上游实测给**同一格式**的 ISO-8601 串,此情形下字典序有效;
    // 若上游混用时区/精度/异构格式会得出错值(留观察,见 verify-results 台账)。
    let earliest_expiry = items
        .iter()
        .filter_map(|item| item.get("expiresAt").and_then(Value::as_str))
        .filter(|text| !text.is_empty())
        .min()
        .unwrap_or("");
    Some(json!({
        "available": total,
        "unit": "积分",
        "wallets": items,
        "subscription": Value::Null,
        "source": "profile-summary",
        "planName": "",
        "subscriptionStatus": "",
        "hasPaidCredits": total > 0.0,
        "creditsUsed": 0.0,
        "creditsExpiresAt": earliest_expiry,
        "availableResetCount": data.get("availableResetCount").and_then(Value::as_i64).unwrap_or(0),
        "availablePromoSubscriptionCount": data
            .get("availablePromoSubscriptionCount")
            .and_then(Value::as_i64)
            .unwrap_or(0),
        "raw": data,
    }))
}

/// creditItems → 精简批次明细（creditsRemaining 非数值的条目丢弃，按剩余降序）
fn normalize_credit_items(items: Option<&Value>) -> Vec<Value> {
    let Some(items) = items.and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out: Vec<Value> = Vec::new();
    for item in items {
        let Some(remaining) = item.get("creditsRemaining").and_then(Value::as_f64) else {
            continue;
        };
        let label = item
            .get("label")
            .or_else(|| item.get("labelEn"))
            .and_then(Value::as_str)
            .unwrap_or("");
        out.push(json!({
            "type": item.get("type").and_then(Value::as_str).unwrap_or(""),
            "displayName": label,
            "balance": remaining,
            "expiresAt": item.get("expiresAt").and_then(Value::as_str).unwrap_or(""),
        }));
    }
    out.sort_by(|left, right| {
        let lhs = left.get("balance").and_then(Value::as_f64).unwrap_or(0.0);
        let rhs = right.get("balance").and_then(Value::as_f64).unwrap_or(0.0);
        rhs.partial_cmp(&lhs).unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

/// quota（老批次口径）→ 归一形状。分支次序照抄 参考实现 `_normalize_quota`
/// （limit → free → monthly → daily → creditsLimit）；服务端显式给出的
/// *Remaining 字段优先于 total-used 推导 —— 积分过期时 remaining=0 而
/// total-used>0，必须以服务端为准。
fn normalize_quota(data: &Value) -> Value {
    let mut total = 0.0f64;
    let mut used = 0.0f64;
    let mut remaining: Option<f64> = None;
    let expires_at = data
        .get("freeCreditsExpiresAt")
        .and_then(Value::as_str)
        .unwrap_or("");
    let number =
        |key: &str| data.get(key).and_then(Value::as_f64).filter(|value| value.is_finite());
    if let Some(limit) = number("limit") {
        total = limit;
        used = number("used").unwrap_or(0.0);
    } else if let Some(free_total) = number("freeCreditsTotal") {
        total = free_total;
        used = number("freeCreditsUsed").unwrap_or(0.0);
        remaining = number("freeCreditsRemaining");
    } else if let Some(monthly) = number("monthlyCreditsLimit") {
        total = monthly;
        used = number("monthlyCreditsUsed").unwrap_or(0.0);
        remaining = number("monthlyCreditsRemaining");
    } else if let Some(daily) = number("dailyCreditsLimit") {
        total = daily;
        used = number("dailyCreditsUsed").unwrap_or(0.0);
        remaining = number("dailyCreditsRemaining");
    } else if let Some(credits) = number("creditsLimit") {
        total = credits;
        used = number("creditsUsed").unwrap_or(0.0);
        remaining = number("creditsRemaining");
    }
    // 显式值与推导值统一钳制:负余额按 0 展示(上游审计发现 5 后半)
    let remaining = remaining
        .unwrap_or_else(|| (total - used).max(0.0))
        .max(0.0);
    json!({
        "available": remaining,
        "unit": "积分",
        "wallets": [],
        "subscription": Value::Null,
        "source": "quota",
        "planName": data.get("planName").and_then(Value::as_str).unwrap_or(""),
        "subscriptionStatus": data
            .get("subscriptionStatus")
            .and_then(Value::as_str)
            .unwrap_or(""),
        "hasPaidCredits": data.get("hasPaidCredits") == Some(&Value::Bool(true)),
        "creditsTotal": total,
        "creditsUsed": used,
        "creditsExpiresAt": expires_at,
        "raw": data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 分支次序（limit → free → monthly → daily → creditsLimit）与
    /// 「服务端 Remaining 优先于 total-used 推导」是 参考实现 逐条移植的口径，
    /// 这里各分支各验一条。
    #[test]
    fn quota_normalization_follows_upstream_branch_order() {
        // limit 分支：无 Remaining 字段，remaining = total - used
        let norm = normalize_quota(&json!({ "limit": 100, "used": 30 }));
        assert_eq!(norm.get("available").and_then(Value::as_f64), Some(70.0));
        assert_eq!(norm.get("source").and_then(Value::as_str), Some("quota"));

        // free 分支：服务端显式给 remaining=0（积分过期），不得推导成 total-used
        let norm = normalize_quota(&json!({
            "freeCreditsTotal": 100, "freeCreditsUsed": 20, "freeCreditsRemaining": 0
        }));
        assert_eq!(norm.get("available").and_then(Value::as_f64), Some(0.0));

        // monthly 分支：显式 remaining 优先
        let norm = normalize_quota(&json!({
            "monthlyCreditsLimit": 200, "monthlyCreditsUsed": 50,
            "monthlyCreditsRemaining": 180
        }));
        assert_eq!(norm.get("available").and_then(Value::as_f64), Some(180.0));

        // daily 分支
        let norm = normalize_quota(&json!({ "dailyCreditsLimit": 10, "dailyCreditsUsed": 4 }));
        assert_eq!(norm.get("available").and_then(Value::as_f64), Some(6.0));

        // creditsLimit 分支 + 订阅字段透传
        let norm = normalize_quota(&json!({
            "creditsLimit": 500, "creditsUsed": 100, "creditsRemaining": 400,
            "planName": "Pro", "subscriptionStatus": "active"
        }));
        assert_eq!(norm.get("available").and_then(Value::as_f64), Some(400.0));
        assert_eq!(norm.get("planName").and_then(Value::as_str), Some("Pro"));

        // 空对象：total-used 都为 0，remaining 兜 0
        let norm = normalize_quota(&json!({}));
        assert_eq!(norm.get("available").and_then(Value::as_f64), Some(0.0));
    }

    /// 主轨无可解析数值 → None(回落 quota),不报 0(上游审计发现 5)
    #[test]
    fn profile_summary_unparseable_returns_none() {
        assert!(normalize_profile_summary(&json!({ "totalCreditsRemaining": "broken" })).is_none());
        assert!(normalize_profile_summary(&json!({})).is_none());
        // 有任一可解析来源仍成功
        assert!(normalize_profile_summary(&json!({
            "creditItems": [ { "creditsRemaining": 5.0 } ]
        }))
        .is_some());
    }

    #[test]
    fn profile_summary_uses_total_and_sorts_items() {
        let data = json!({
            "totalCreditsRemaining": 120.0,
            "creditItems": [
                { "type": "daily", "label": "每日积分", "creditsRemaining": 30.0,
                  "expiresAt": "2026-10-05" },
                { "type": "reward", "label": "签到奖励", "creditsRemaining": 90.0,
                  "expiresAt": "2026-10-04" },
                { "type": "broken" },
            ],
            "availableResetCount": 2,
        });
        let norm = normalize_profile_summary(&data).unwrap();
        assert_eq!(norm.get("available").and_then(Value::as_f64), Some(120.0));
        assert_eq!(
            norm.get("source").and_then(Value::as_str),
            Some("profile-summary")
        );
        let wallets = norm.get("wallets").and_then(Value::as_array).cloned().unwrap_or_default();
        // 非数值条目被丢弃，剩下两条按剩余量降序
        assert_eq!(wallets.len(), 2);
        assert_eq!(
            wallets[0].get("displayName").and_then(Value::as_str),
            Some("签到奖励")
        );
        // 最早到期时间取批次里的最小值
        assert_eq!(
            norm.get("creditsExpiresAt").and_then(Value::as_str),
            Some("2026-10-04")
        );
    }
}
