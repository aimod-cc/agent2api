//! OfficeAce 的订阅 / 额度 / 奖励族接口：控制面 **V11-HMAC-SHA256** 签名。
//!
//! ── 挂在网关根，不是 chat 的 `/v2` ──────────────────────────
//! 账号里存的 `baseUrl` 是 `https://{host}/v2`（见 `oauth::fetch_gateway_credential`），
//! 而 `/v1/subscription` 与 `/v1/subscription/bonus/claim` 都挂在**网关根**上；
//! [`gateway_base`] 去掉尾 `/v2` 再拼路径。
//!
//! ── 为什么不用 `auth_http::send_raw` ─────────────────────────
//! `send_raw` 在 body 非空时会**自己补一个 `Content-Type`**，而签名要求
//! `content-type` 以**同一个值**进签名头集合 —— 两条路一起走会发成两个
//! `Content-Type`。照 `codearts::welfare` 的先例：用 `egress::client_for(None)`
//! 直接发，显式带上签名算出的全套头（`Host` 由签名层从 URL 注入，见 `signer`）。
//!
//! ── 三条实测坑（写在这里，别在别处重推）──────────────────────
//!   1. 领奖励 `POST /v1/subscription/bonus/claim` **必须** body 给 `{}` 且带
//!      `Content-Type: application/json` —— 裸 POST（无体）会 500 `OfficeAce.11020001`。
//!   2. `x-subscription-type: v2` 是 `/v1/subscription` 与 bonus/claim 的必要头
//!      （usage / rates 不带）；它进签名头集合。
//!   3. 一天一个活动只领一次，**重复领不报错**（返回同一份列表）—— 幂等由上游保证。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::{json, Value};

use crate::server::core::egress;
use crate::server::errors::GatewayError;

use super::credentials::OfficeAceCredential;
use super::signer::{self, Algorithm, SignRequest};

/// 订阅族接口总超时（余额/签到是外部 HTTP，必须设总超时：`egress` 默认 read_timeout
/// 600 秒是给 SSE 长连接的，直接用会把一个挂住的查询拖到天荒地老）。
const TIMEOUT_SECS: u64 = 20;

/// 账号里存的 `https://{host}/v2` → 网关根 `https://{host}`。
pub fn gateway_base(base_url: &str) -> String {
    let trimmed = base_url.trim().trim_end_matches('/');
    trimmed
        .strip_suffix("/v2")
        .unwrap_or(trimmed)
        .trim_end_matches('/')
        .to_string()
}

/// 响应体截断（排障用；不打印凭据 —— 这里只截原始响应文本）。
fn excerpt(text: &str) -> String {
    let cleaned = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.chars().count() <= 240 {
        return cleaned;
    }
    cleaned.chars().take(240).collect::<String>() + "…"
}

/// 调一次 V11 签名的订阅族接口，返回解析后的响应体。
///
/// `body`（有体时）与 `extra_headers` 都**进签名**；`Content-Type` 由这里有体时补上。
async fn call(
    credential: &OfficeAceCredential,
    method: &str,
    path: &str,
    body: Option<Value>,
    extra_headers: &[(&str, &str)],
) -> Result<Value, GatewayError> {
    if !credential.has_control_plane() {
        return Err(crate::server::core::providers::adapter::usage_not_configured(
            "OfficeAce 果办",
            "控制面临时凭据（AK/SK）",
        ));
    }
    let base = gateway_base(&credential.base_url);
    if base.is_empty() {
        return Err(GatewayError::with_status(400, "OfficeAce 账号缺少模型网关基址"));
    }
    let url = format!("{base}{path}");
    let signing = credential
        .signing()
        .map_err(|reason| GatewayError::with_status(500, format!("OfficeAce 签名凭据不可用：{reason}")))?;
    let payload = body
        .as_ref()
        .map(|value| value.to_string().into_bytes())
        .unwrap_or_default();
    let mut headers: Vec<(String, String)> = extra_headers
        .iter()
        .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
        .collect();
    if body.is_some() {
        headers.push(("Content-Type".to_string(), "application/json".to_string()));
    }
    let signed = signer::sign(&SignRequest {
        algorithm: Algorithm::V11,
        method,
        url: &url,
        headers: &headers,
        body: &payload,
        credential: &signing,
        region: signer::DEFAULT_REGION,
        date_override: None,
    })
    .map_err(|reason| GatewayError::with_status(500, format!("OfficeAce 订阅请求签名失败：{reason}")))?;
    let built = match method {
        "POST" => egress::client_for(None)
            .post(&url)
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
            .body(payload),
        _ => egress::client_for(None)
            .get(&url)
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS)),
    };
    let mut request = built;
    for (name, value) in signed {
        request = request.header(name.as_str(), value.as_str());
    }
    let response = request.send().await.map_err(|error| {
        GatewayError::with_status(
            502,
            format!("OfficeAce 订阅请求失败：{}", egress::describe_error_detail(&error)),
        )
    })?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    // 401/403 = 控制面临时凭据失效（约 2 小时过期），要用户重新登录，与网络错误区分开
    if status == 401 || status == 403 {
        return Err(GatewayError::with_status(
            i32::from(status),
            "OfficeAce 控制面凭据已失效（临时 AK/SK 会过期）：请重新登录一次",
        ));
    }
    if status != 200 {
        return Err(GatewayError::with_status(
            i32::from(status),
            format!("OfficeAce 订阅接口返回 HTTP {status}：{}", excerpt(&text)),
        ));
    }
    serde_json::from_str::<Value>(&text)
        .map_err(|_| GatewayError::with_status(502, "OfficeAce 订阅接口响应不是合法 JSON"))
}

/// 读订阅快照（套餐 + 积分账本 + 奖励活动）。
pub async fn fetch_subscription(credential: &OfficeAceCredential) -> Result<Value, GatewayError> {
    call(
        credential,
        "GET",
        "/v1/subscription",
        None,
        &[("x-subscription-type", "v2")],
    )
    .await
}

/// 领奖励积分：`POST /v1/subscription/bonus/claim`（body `{}`，幂等）。
///
/// 返回完整响应体（`bonus_skus` 是当前所有奖励活动，含刚领到的）。
pub async fn claim_bonus(credential: &OfficeAceCredential) -> Result<Value, GatewayError> {
    call(
        credential,
        "POST",
        "/v1/subscription/bonus/claim",
        Some(json!({})),
        &[("x-subscription-type", "v2")],
    )
    .await
}

/// 一个奖励活动的归一形态（从 `bonus_skus` 的条目解出）。
pub struct Bonus {
    pub activity_id: String,
    pub name: String,
    pub total: f64,
    pub used: f64,
    pub expires_at: i64,
}

impl Bonus {
    pub fn remaining(&self) -> f64 {
        (self.total - self.used).max(0.0)
    }
}

/// 从 `bonus_skus` 数组解出奖励活动（形状见参考实现的 `summarizeSubscription`）。
pub fn bonuses_of(subscription: &Value) -> Vec<Bonus> {
    subscription
        .get("bonus_skus")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let activity_id = item
                        .get("activity_id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    if activity_id.is_empty() {
                        return None;
                    }
                    Some(Bonus {
                        name: item
                            .get("activity_name")
                            .and_then(Value::as_str)
                            .filter(|name| !name.is_empty())
                            .unwrap_or(&activity_id)
                            .to_string(),
                        total: item.get("points").and_then(Value::as_f64).unwrap_or(0.0),
                        used: item.get("current_value").and_then(Value::as_f64).unwrap_or(0.0),
                        expires_at: parse_ms(item.get("expired_time")),
                        activity_id,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 把「过期时刻」解成毫秒时间戳：上游给 RFC3339 串（`2026-10-10T05:15:00Z`）或数字。
fn parse_ms(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Number(number)) => number.as_i64().unwrap_or(0),
        Some(Value::String(text)) => super::oauth::parse_rfc3339_ms(text).unwrap_or(0),
        _ => 0,
    }
}

/// 把 `/v1/subscription` 归一成 `query_usage` 的形状（见 `adapter::ProviderAdapter`
/// 的 `query_usage` 契约）。
///
/// 只算**未过期**的额度（过期条目不计入，与参考实现的 `summarizeSubscription` 一致）；
/// `available` = 套餐剩余 + 奖励剩余。
pub fn summarize(subscription: &Value) -> Value {
    let now = crate::server::logging::now_ms();
    let mut plan_total = 0.0;
    let mut plan_used = 0.0;
    let mut plan_name = String::new();
    let mut plan_expire = 0i64;
    let mut search_total = 0.0;
    let mut search_used = 0.0;
    for sku in subscription
        .get("skus")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        for quota in sku.get("quotas").and_then(Value::as_array).into_iter().flatten() {
            let expired = parse_ms(quota.get("expired_time"));
            if expired > 0 && expired <= now {
                continue;
            }
            match quota.get("sku_attr_code").and_then(Value::as_str) {
                Some("officeace_points") => {
                    plan_total += quota.get("sku_value").and_then(Value::as_f64).unwrap_or(0.0);
                    plan_used += quota.get("current_value").and_then(Value::as_f64).unwrap_or(0.0);
                }
                Some("online_search_count") => {
                    search_total += quota.get("sku_value").and_then(Value::as_f64).unwrap_or(0.0);
                    search_used += quota.get("current_value").and_then(Value::as_f64).unwrap_or(0.0);
                }
                _ => {}
            }
        }
        if plan_name.is_empty() {
            plan_name = sku
                .get("sku_name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
        }
        let expire = parse_ms(sku.get("expired_time"));
        if expire > 0 && (plan_expire == 0 || expire < plan_expire) {
            plan_expire = expire;
        }
    }
    let bonuses = bonuses_of(subscription);
    let bonus_total: f64 = bonuses
        .iter()
        .filter(|bonus| bonus.expires_at == 0 || bonus.expires_at > now)
        .map(|bonus| bonus.total)
        .sum();
    let bonus_used: f64 = bonuses
        .iter()
        .filter(|bonus| bonus.expires_at == 0 || bonus.expires_at > now)
        .map(|bonus| bonus.used)
        .sum();
    let plan_remaining = (plan_total - plan_used).max(0.0);
    let bonus_remaining = (bonus_total - bonus_used).max(0.0);
    let available = plan_remaining + bonus_remaining;
    let status = subscription
        .get("subscribe_status")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    json!({
        "available": available,
        "unit": "积分",
        "wallets": [
            { "type": "plan", "displayName": plan_name_or_default(&plan_name), "balance": plan_remaining },
            { "type": "bonus", "displayName": "奖励积分", "balance": bonus_remaining },
        ],
        "subscription": {
            "planName": plan_name_or_default(&plan_name),
            "status": status,
            "expireAt": plan_expire,
            "remainQuota": plan_remaining,
            "totalQuota": plan_total,
            "searches": if search_total > 0.0 {
                json!({ "remaining": (search_total - search_used).max(0.0), "total": search_total })
            } else {
                Value::Null
            },
        },
        "raw": subscription,
    })
}

fn plan_name_or_default(name: &str) -> String {
    if name.is_empty() {
        "OfficeAce 果办".to_string()
    } else {
        name.to_string()
    }
}

/// `activity_id → 该活动的剩余量`（两帧对比、任务行渲染都用它）。
pub fn remaining_map(subscription: &Value) -> std::collections::HashMap<String, f64> {
    bonuses_of(subscription)
        .into_iter()
        .map(|bonus| {
            let remaining = bonus.remaining();
            (bonus.activity_id, remaining)
        })
        .collect()
}

/// 两帧订阅之间「这次多了多少」：按每个奖励活动的**余额增量**求和。
///
/// 首次见到的活动（`before` 里没有）退一步用它的 `remaining`（余额），而**不是**
/// `total` —— 后者是**累计发放**，可能早就由别的渠道（桌面端、活动页）发过，算进去
/// 会让读数虚高（参考实现踩过：签到页显示「领到 100」而实际只多出一点）。
pub fn gained_between(before: &Value, after: &Value) -> f64 {
    let previous = remaining_map(before);
    bonuses_of(after)
        .iter()
        .map(|bonus| match previous.get(&bonus.activity_id) {
            Some(prev) => (bonus.remaining() - prev).max(0.0),
            None => bonus.remaining(),
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_base_strips_the_v2_suffix() {
        assert_eq!("https://h", gateway_base("https://h/v2"));
        assert_eq!("https://h", gateway_base("https://h/v2/"));
        assert_eq!("https://h", gateway_base("https://h"));
        assert_eq!("https://h", gateway_base("  https://h/v2  "));
    }

    #[test]
    fn summarize_adds_plan_and_bonus_remaining() {
        let sub = json!({
            "subscribe_status": "SUBSCRIBED",
            "skus": [{
                "sku_name": "标准版",
                "expired_time": "2099-01-01T00:00:00Z",
                "quotas": [
                    { "sku_attr_code": "officeace_points", "sku_value": 500, "current_value": 120 },
                    { "sku_attr_code": "online_search_count", "sku_value": 200, "current_value": 0 }
                ]
            }],
            "bonus_skus": [
                { "activity_id": "newbie", "activity_name": "新人注册礼", "points": 100, "current_value": 0 }
            ]
        });
        let out = summarize(&sub);
        // 500-120 = 380（套餐）+ 100-0 = 100（奖励）= 480
        assert_eq!(480.0, out["available"].as_f64().unwrap());
        assert_eq!("积分", out["unit"]);
        assert_eq!(380.0, out["wallets"][0]["balance"].as_f64().unwrap());
        assert_eq!(100.0, out["wallets"][1]["balance"].as_f64().unwrap());
        assert_eq!(500.0, out["subscription"]["totalQuota"].as_f64().unwrap());
        assert_eq!(200.0, out["subscription"]["searches"]["total"].as_f64().unwrap());
    }

    #[test]
    fn summarize_skips_expired_quotas() {
        let sub = json!({
            "skus": [{
                "quotas": [
                    { "sku_attr_code": "officeace_points", "sku_value": 999, "current_value": 0,
                      "expired_time": "2000-01-01T00:00:00Z" }
                ]
            }],
            "bonus_skus": []
        });
        let out = summarize(&sub);
        assert_eq!(0.0, out["available"].as_f64().unwrap(), "过期的额度不计入");
    }

    #[test]
    fn bonuses_of_ignores_entries_without_activity_id() {
        let sub = json!({ "bonus_skus": [ {"points": 1}, {"activity_id": "a", "points": 5, "current_value": 2} ] });
        let bonuses = bonuses_of(&sub);
        assert_eq!(1, bonuses.len());
        assert_eq!("a", bonuses[0].activity_id);
        assert_eq!(3.0, bonuses[0].remaining());
    }
}
