//! Cline 余额与订阅：credit 余额 + 当前套餐，归一成账号页的统一形状。
//!
//! ── 两个上游接口（实测，2026-09）─────────────────────────────
//! ```text
//! GET {apiBase}/users/{userId}/balance
//!   → {"data":{"userId":"usr-...","balance":-17704},"success":true}
//!
//! GET {apiBase}/users/me/plan
//!   → 200 {"data":{...}}                     有订阅时
//!   → 404 {"data":null,"error":"no plan history found for user","success":false}
//!                                            **没订阅是 404，不是空 200**
//! ```
//!
//! ── `balance` 是**微 credit（microcredit）**，要 ÷1e6（关键）────────
//! 实测一个刚用过的账号返回 `balance: -17704`，而 Cline 官方界面（Cline Hub
//! 的余额卡片）对同一账号显示 `-0.0177`。差值恰好是 1e6，且官方前端的格式化
//! 函数就是 `format(e / 1e6)`（实测 `@cline/cli-windows-x64` 的
//! `cline-hub/webview/assets/settings-view-*.js`：`ae=(e,t=2)=>new
//! Intl.NumberFormat(...).format(e/1e6)`）—— 上游返回的整数是**微 credit**，
//! 除以 1e6 才是用户认得的 credit。
//!
//! 负值表示**已欠费**（免费额度用超了）—— 这个符号是用户最需要看到的事实，
//! 如实保留（换算不改变符号）。
//!
//! 因此本模块的归一策略：
//!   - `available` = 上游 `balance` ÷ 1e6（`unit` 为 `"credit"`）；
//!   - 原始微 credit 值放进 `raw.balance`、`raw.balanceMicrocredits`，
//!     排障时能对上上游；
//!   - `unit` 不写「积分」：Cline 的 credit 与另外几家的积分不是一回事，
//!     混用会让用户以为可以跨家比较。
//!
//! 刻度换算只此一处（[`CREDITS_PER_MICRO`]），将来若上游改刻度改这里即可。
//!
//! ── 为什么把 plan 也拉进来 ──────────────────────────────────
//! 本项目的 `subscription` 段就是给「订阅型提供商」准备的（Qoder 填了
//! `expireAt`）。Cline 的套餐信息恰好填充它，而**没有订阅时 404 是正常状态**
//! （免费用户），不能当失败 —— 因此那个请求失败只记空，不影响余额结果。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：绝不 unwrap/expect/panic。

use serde_json::{json, Map, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::core::egress;
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::credentials;
use super::refresh;

/// 余额接口超时（与另外几家同档：不设总超时会让挂住的查询把前端转到天荒地老）
const REQUEST_TIMEOUT_MS: u64 = 20_000;

/// 上游 `balance` 的刻度：1 credit = 1e6 微 credit（见模块头）。
///
/// 出处是 Cline 官方前端自己的换算（`format(e/1e6)`），不是猜的。
const CREDITS_PER_MICRO: f64 = 1_000_000.0;

/// 查余额 + 订阅（归一成账号页的统一形状）。
///
/// ── 鉴权 ────────────────────────────────────────────────────
/// `Authorization: Bearer <token>`，token 含 `workos:` 前缀（`credentials`
/// 的 `bearer_token()` 负责补）。**注意余额接口不需要 `X-CLIENT-TYPE`**
/// （实测只带 Authorization 就 200）—— 那是 LLM 端点的产品面校验，
/// 管理接口没有。这里仍然带上，理由与发送请求时一致：更贴近官方客户端，
/// 且没有代价。
///
/// ── 用户 id 从哪来（三级，尽量省掉那个往返）──────────────────
/// 余额路径是 `/users/{userId}/balance`，`userId` 是 `usr-...` 形态的
/// **Cline 账号 id**（不是 WorkOS 的 `user_...`）。来源顺序：
///   1. 凭证记录里的 `account`（若已是 `usr-` 形态，通常来自 JWT 的
///      `external_id` 声明 —— 见 `credentials::account_id_from_jwt`）；
///   2. 现场从 access token 的 JWT 里解 `external_id`（手填 token 时记录里
///      可能没有 account，但 JWT 里有）；
///   3. 兜底打一次 `/users/me`，用它的 `data.id`。
///
/// 第 3 条是最后手段：它多一个往返、多一处失败面，但必须留着 ——
/// token 不是 JWT（上游将来换格式）时前两条都取不到。
pub async fn query_usage(store: &AccountStore, account_id: &str) -> Result<Value, GatewayError> {
    let credentials = refresh::ensure_fresh(store, account_id, false).await?;
    let token = credentials.bearer_token();
    let user_id = if credentials.account.starts_with("usr-") {
        credentials.account.clone()
    } else if let Some(from_jwt) = credentials::account_id_from_jwt(&credentials.access_token) {
        from_jwt
    } else {
        // 前两条都取不到 → 问上游要一个
        fetch_me(&token).await?
    };
    if user_id.is_empty() {
        return Err(GatewayError::with_status(
            502,
            "Cline 余额查询失败：拿不到账号 id（上游 /users/me 未返回 id）",
        ));
    }
    let balance_raw = fetch_balance(&token, &user_id).await?;
    let plan = fetch_plan(&token).await;
    Ok(normalize(&balance_raw, plan.as_ref(), &user_id))
}

/// `GET /users/me` → `data.id`（`usr-...`）
async fn fetch_me(token: &str) -> Result<String, GatewayError> {
    let payload = get_json(&format!("{}/users/me", credentials::API_BASE_URL), token).await?;
    let id = payload
        .get("data")
        .and_then(|data| data.get("id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .unwrap_or("")
        .to_string();
    if id.is_empty() {
        return Err(GatewayError::with_status(
            502,
            "Cline 账号信息响应缺少 id",
        ));
    }
    Ok(id)
}

/// `GET /users/{id}/balance` → 原始响应（含 `data`）
async fn fetch_balance(token: &str, user_id: &str) -> Result<Value, GatewayError> {
    get_json(
        &format!(
            "{}/users/{}/balance",
            credentials::API_BASE_URL,
            url_encode(user_id)
        ),
        token,
    )
    .await
}

/// `GET /users/me/plan`（**没订阅时 404 是正常状态**，返回 None）。
///
/// 只有 404 与「响应里没有可用套餐」会被当成 None；其余失败（401/5xx/网络）
/// 也返回 None 但记一条 verbose 日志 —— 余额是主结果，套餐是附加信息，
/// 不该因为套餐接口抖动就让整个查询失败。
async fn fetch_plan(token: &str) -> Option<Value> {
    let url = format!("{}/users/me/plan", credentials::API_BASE_URL);
    match get_json_allow_status(&url, token, &[404]).await {
        Ok(Some(payload)) => payload
            .get("data")
            .filter(|data| !data.is_null())
            .cloned(),
        Ok(None) => None, // 404：没有订阅
        Err(error) => {
            logging::verbose(
                "[Cline]",
                &format!("套餐信息查询失败（不影响余额）: {}", error.message),
            );
            None
        }
    }
}

/// 发一次 GET 并解包 `{"data":..,"success":true}` 信封。
async fn get_json(url: &str, token: &str) -> Result<Value, GatewayError> {
    match get_json_allow_status(url, token, &[]).await? {
        Some(payload) => Ok(payload),
        None => Err(GatewayError::with_status(502, "Cline 接口返回了非预期状态码")),
    }
}

/// 发一次 GET；`allowed` 里的状态码返回 `Ok(None)`（调用方按语义处理），
/// 其余非 2xx 转成错误。
async fn get_json_allow_status(
    url: &str,
    token: &str,
    allowed: &[u16],
) -> Result<Option<Value>, GatewayError> {
    let client = egress::client_for(None);
    let response = client
        .get(url)
        .header("Accept", "application/json")
        .header("Authorization", format!("Bearer {token}"))
        .header("X-CLIENT-TYPE", super::adapter::CLIENT_TYPE)
        .timeout(std::time::Duration::from_millis(REQUEST_TIMEOUT_MS))
        .send()
        .await
        .map_err(|error| {
            GatewayError::with_status(
                502,
                format!("Cline 接口请求失败: {}", egress::describe_error_detail(&error)),
            )
        })?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    if allowed.contains(&status) {
        return Ok(None);
    }
    if status == 401 {
        // 原样透出 401：调用方据此走「刷新后重试一次」的既有处置
        return Err(GatewayError::with_status(
            401,
            "Cline 登录态已失效，请刷新凭证后重试",
        ));
    }
    if !(200..300).contains(&status) {
        let payload: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let detail = payload
            .get("error")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                crate::server::core::account_store::store_util::truncate_text(&text, 200)
            });
        return Err(GatewayError::with_status(
            502,
            format!("Cline 接口返回 {status}: {detail}"),
        ));
    }
    let payload: Value = serde_json::from_str(&text)
        .map_err(|error| GatewayError::with_status(502, format!("Cline 接口响应不是 JSON: {error}")))?;
    Ok(Some(payload))
}

/// 归一成账号页的统一形状（契约见 `ProviderAdapter::query_usage` 的文档）。
///
/// ── 关于 `balance` 的刻度（微 credit → credit，见模块头）──────
/// 上游给的整数是**微 credit**：官方前端自己就除以 1e6 再显示
/// （`format(e/1e6)`），实测 `-17704` 在官方界面显示为 `-0.0177`。
/// 所以 `available` 与 `wallets[].balance` 都换算成 credit，
/// 原始微 credit 值留在 `raw` 里备查。
///
/// 换算只此一处：将来上游改刻度（例如改成直接给 credit）只改 `CREDITS_PER_MICRO`。
fn normalize(balance_payload: &Value, plan: Option<&Value>, user_id: &str) -> Value {
    let data = balance_payload.get("data").unwrap_or(balance_payload);
    let raw_balance = number(data.get("balance"));
    // 微 credit → credit。`number()` 已过滤非有限值，除法不会产生 NaN/Inf
    let credits = raw_balance.map(|value| value / CREDITS_PER_MICRO);
    let mut wallets: Vec<Value> = Vec::new();
    if let Some(balance) = credits {
        wallets.push(json!({
            "type": "cline_credit",
            "displayName": if balance < 0.0 { "Cline credit（已欠费）" } else { "Cline credit" },
            "balance": balance,
        }));
    }
    let mut subscription = Map::new();
    if let Some(plan) = plan {
        // 上游 plan 的字段不稳定（实测有 displayName / name / plan 三种命名），
        // 逐个候选取第一个非空的
        for key in ["displayName", "name"] {
            if let Some(name) = plan
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
            {
                subscription.insert("planName".to_string(), Value::String(name.to_string()));
                break;
            }
        }
        // 嵌套形态：{plan:{displayName:...}}
        if !subscription.contains_key("planName") {
            if let Some(name) = plan
                .get("plan")
                .and_then(|inner| inner.get("displayName").or_else(|| inner.get("name")))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
            {
                subscription.insert("planName".to_string(), Value::String(name.to_string()));
            }
        }
        // 到期时间：契约要求 `expireAt` 是**毫秒时间戳**（与 Qoder 的
        // `expireAt` 同口径，前端按数字渲染成日期）。上游给的是 ISO 字符串
        // （`"2026-10-19T00:00:00Z"`），因此这里必须转成毫秒 ——
        // 直接透传字符串会让前端把日期显示成 `Invalid Date`。
        // 转不出来就不填这个键（宁可少一行，不要一行坏数据）。
        for key in ["currentPeriodEnd", "cancelAt"] {
            if let Some(value) = plan.get(key).filter(|value| !value.is_null()) {
                if let Some(ms) = super::refresh::parse_expires_at(Some(value)) {
                    subscription.insert(
                        "expireAt".to_string(),
                        crate::server::core::account_store::state::json_number(ms),
                    );
                }
                break;
            }
        }
        if let Some(status) = plan
            .get("status")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            subscription.insert("status".to_string(), Value::String(status.to_string()));
        }
    }
    json!({
        "available": credits,
        "unit": "credit",
        "wallets": wallets,
        "subscription": subscription,
        // 排障用原始响应（前端默认不展示）：balance 是**上游原值**（微 credit），
        // credits 是换算后的值，两者一起留着才能一眼看出刻度有没有变
        "raw": {
            "userId": user_id,
            "balance": raw_balance,
            "balanceMicrocredits": raw_balance,
            "plan": plan,
        },
    })
}

/// 数字取值（容忍字符串形态）
fn number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse::<f64>().ok().filter(|v| v.is_finite()),
        _ => None,
    }
}

/// 百分号编码（只编码会破坏路径的字符；`usr-...` 形态实际用不到，防御性）
fn url_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}
