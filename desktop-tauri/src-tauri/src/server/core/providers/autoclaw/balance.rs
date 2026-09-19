//! AutoClaw 的积分与订阅查询（移植来源 `account-balance.mjs` 的 `queryPoints` /
//! `querySubscription`）。
//!
//! ── 上游长什么样（逐条对照源实现）─────────────────────────────
//! 两条链路**都走 userapi 域**（`AUTOCLAW_USERAPI_BASE_URL`，见
//! `credentials::userapi_base_url`），即源实现里 `client.userapiGetFor` /
//! `userapiPostFor` 的对等物：
//!   - 积分：`GET {userapi}/agent-assetmgr/api/v2/wallets?biz_app_id=autoclaw`
//!     （主链路）；`code !== 0` 或响应里没有 `wallets` 数组时降级到 v1
//!     `/agent-assetmgr/api/v1/wallet-instances?wallet_type=all&wallet_scope=all`；
//!     另外 `GET /agent-assetmgr/api/v1/points/expiring?biz_app_id=autoclaw`
//!     查「即将过期」（失败不阻塞余额展示）。
//!   - 订阅：`POST {userapi}/agentpay/v1/assistant/subscribe-info`
//!     body `{source_id: "autoclaw", device_id?}`。
//!
//! ── 鉴权：请求签名 + Bearer（复用既有实现，不重写）─────────────
//! 两条链路都要那套「客户端指纹」头：`X-Auth-Appid` / `X-Auth-TimeStamp`
//! （秒）/ `X-Auth-Sign = MD5("{appId}&{ts}&{appKey}")`，外加品牌头与
//! `authorization: Bearer <token>`。这份签名**不在这里重写**：复用
//! `refresh::signed_auth_headers`（它是全仓唯一一份签名实现，与刷新接口共用，
//! 两处各写一份必然会在 appId/appKey 或时间戳单位上慢慢分叉）。
//!
//! ── 出网代理（核对结论）──────────────────────────────────────
//! 源实现的 `userapiGetFor` / `userapiPostFor` 是**裸 fetch**（只带超时，
//! 没有任何代理参数），对应本项目的直连：`send_raw(..., proxy = None, ...)`。
//! 账号级代理是给转发那条流式长请求准备的出口，而 userapi 与 LLM 代理域是两个
//! 站点（与小浣熊余额、两家刷新接口同一取舍）。
//!
//! ── 超时 ────────────────────────────────────────────────────
//! 20 秒（源实现 `REQUEST_TIMEOUT_MS = 20_000`）。必须显式设：`egress` 默认的
//! read_timeout 是 600 秒（留给 SSE 长连接），不设总超时会让一个挂住的余额请求
//! 把前端的批量查询拖到一直转圈。
//!
//! ── 为什么订阅查询与积分查询共用一次调用 ───────────────────────
//! 源实现把它们拆成两个接口（`/api/accounts/balances` 与
//! `/api/accounts/subscriptions`），本网关的 `/api/accounts/usage` 只有一条，
//! 所以合成一次：**订阅失败不影响积分结果**（`subscription` 给 null 并把原因
//! 写进 `raw`）—— 订阅是附加信息，为了它把整个面板变成红色的「查询失败」
//! 得不偿失（与源实现给 `points/expiring` 的兜底同一取向）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：零 unwrap/expect/panic。

use serde_json::{json, Map, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::core::auth_http::send_raw;
use crate::server::errors::GatewayError;

use super::credentials::{self, AutoClawCredentials};
use super::refresh::signed_auth_headers;

/// 积分 / 订阅接口的请求超时（源实现 `REQUEST_TIMEOUT_MS`）
const REQUEST_TIMEOUT_MS: u64 = 20_000;

/// 积分钱包路径（源实现 `POINTS_WALLET_PATH`，v2 主链路）
const POINTS_WALLET_PATH: &str = "/agent-assetmgr/api/v2/wallets?biz_app_id=autoclaw";
/// 旧版钱包路径（源实现 `POINTS_WALLET_LEGACY_PATH`，v2 拿不到时的兜底）
const POINTS_WALLET_LEGACY_PATH: &str =
    "/agent-assetmgr/api/v1/wallet-instances?wallet_type=all&wallet_scope=all";
/// 即将过期积分路径（源实现 `POINTS_EXPIRING_PATH`）
const POINTS_EXPIRING_PATH: &str = "/agent-assetmgr/api/v1/points/expiring?biz_app_id=autoclaw";
/// 订阅信息路径（源实现 `querySubscription` 的 path）
const SUBSCRIBE_INFO_PATH: &str = "/agentpay/v1/assistant/subscribe-info";

/// 登录态失效的业务码（源实现的两处判定合并：`41e4` === 410000，与 400000）。
///
/// 上游在 HTTP 200 的响应体里用业务码表达「token 不认」，只判 HTTP 状态码会
/// 把一次登录态过期当成「积分查询返回异常」报给用户（他不会知道要去重新登录）。
const AUTH_EXPIRED_CODES: &[i64] = &[410_000, 400_000];

/// v1 钱包实例的 scope → 展示名（源实现 `WALLET_SCOPE_LABELS`，逐字照抄）。
const WALLET_SCOPE_LABELS: &[(&str, &str)] = &[
    ("daily", "赠送积分"),
    ("monthly", "订阅积分"),
    ("long_term", "通用积分"),
    ("annual", "通用积分"),
    ("campaign", "活动积分"),
];

/// 查询某账号的积分 / 订阅（归一化形状见 `ProviderAdapter::query_usage` 的文档）。
pub(super) async fn query_usage(
    store: &AccountStore,
    account_id: &str,
) -> Result<Value, GatewayError> {
    let record = store.autoclaw_account_record(account_id);
    if !account_id.is_empty() && record.is_none() {
        return Err(GatewayError::with_status(
            404,
            format!("AutoClaw 账号 {account_id} 不存在或不属于 AutoClaw"),
        ));
    }
    // 与适配器的 `resolve_credentials` 同一条链（账号记录 → 桌面端实时登录态 →
    // 环境变量），但这里只取快照、**不触发刷新**：余额查询是只读展示动作，
    // 过期就让上游回 401，由调用方走「刷新后重试一次」那条既有链路。
    let credentials = credentials::snapshot_for(record.as_ref())?;
    if credentials.token.trim().is_empty() {
        return Err(GatewayError::with_status(
            401,
            "该账号没有可用凭证，无法查询积分",
        ));
    }
    let points = query_points(&credentials).await?;
    let (subscription, subscription_error) = match query_subscription(&credentials).await {
        Ok(value) => (value, Value::Null),
        Err(error) => (Value::Null, Value::String(error.message)),
    };

    let available = points.get("totalBalance").cloned().unwrap_or(Value::Null);
    let wallets = points
        .get("wallets")
        .cloned()
        .unwrap_or_else(|| Value::Array(Vec::new()));
    let mut raw = Map::new();
    raw.insert(
        "points".to_string(),
        points.get("raw").cloned().unwrap_or(Value::Null),
    );
    if let Some(expiring) = points.get("expiring") {
        if !expiring.is_null() {
            raw.insert("expiring".to_string(), expiring.clone());
        }
    }
    if !subscription_error.is_null() {
        raw.insert("subscriptionError".to_string(), subscription_error);
    }
    Ok(json!({
        "available": available,
        // AutoClaw 的资产口径就是它自己的「积分」
        "unit": "积分",
        "wallets": wallets,
        "subscription": subscription,
        "raw": Value::Object(raw),
    }))
}

/// 查询积分钱包（源实现 `queryPoints`：v2 主链路 → v1 兜底 → expiring 附加）。
///
/// 返回 `{totalBalance, wallets, expiring, raw}`。
async fn query_points(credentials: &AutoClawCredentials) -> Result<Value, GatewayError> {
    let wallet_payload = userapi_get(credentials, POINTS_WALLET_PATH).await?;
    if let Some(code) = wallet_payload.get("code").and_then(Value::as_i64) {
        if AUTH_EXPIRED_CODES.contains(&code) {
            return Err(GatewayError::with_status(
                401,
                "登录态已过期，无法查询积分",
            ));
        }
    }

    let data = wallet_payload.get("data").cloned().unwrap_or(Value::Null);
    let code_ok = wallet_payload.get("code").and_then(Value::as_i64) == Some(0);
    let (total_balance, wallets, raw) = if code_ok
        && data.get("wallets").and_then(Value::as_array).is_some()
    {
        let wallets = data
            .get("wallets")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        // 源实现按 `priority` 升序（缺失视为 MAX_SAFE_INTEGER，即排在最后）。
        // 先把 priority 提到结构里，再排序，避免排序闭包每次都回原数组里找一遍。
        let mut mapped: Vec<(i64, Value)> = wallets
            .iter()
            // 源实现 `filter(wallet => wallet?.display)`：`display` 为假值的不展示
            .filter(|wallet| js_truthy(wallet.get("display")))
            .map(|wallet| {
                let priority = wallet
                    .get("priority")
                    .and_then(Value::as_i64)
                    .unwrap_or(i64::MAX);
                let entry = json!({
                    "type": wallet
                        .get("public_wallet_type")
                        .cloned()
                        .unwrap_or(Value::Null),
                    "displayName": wallet
                        .get("display_name")
                        .and_then(Value::as_str)
                        .filter(|text| !text.is_empty())
                        .map(str::to_string)
                        .or_else(|| {
                            wallet
                                .get("public_wallet_type")
                                .and_then(Value::as_str)
                                .filter(|text| !text.is_empty())
                                .map(str::to_string)
                        })
                        .unwrap_or_else(|| "积分".to_string()),
                    "balance": number_or_null(wallet.get("balance")),
                    // 上游给的展示串（带千分位/单位这类格式）；缺失时按数值拼
                    "balanceView": wallet
                        .get("balance_view")
                        .and_then(Value::as_str)
                        .filter(|text| !text.is_empty())
                        .map(str::to_string)
                        .or_else(|| {
                            wallet
                                .get("balance")
                                .and_then(Value::as_f64)
                                .map(json_number_text)
                        })
                        .unwrap_or_default(),
                });
                (priority, entry)
            })
            .collect();
        mapped.sort_by_key(|(priority, _)| *priority);
        (
            data.get("total_balance").cloned().unwrap_or(Value::Null),
            mapped.into_iter().map(|(_, entry)| entry).collect(),
            data.clone(),
        )
    } else {
        // v2 拿不到（code 非 0 或没有 wallets 数组）→ v1 兜底
        let legacy = userapi_get(credentials, POINTS_WALLET_LEGACY_PATH).await?;
        if let Some(code) = legacy.get("code").and_then(Value::as_i64) {
            if AUTH_EXPIRED_CODES.contains(&code) {
                return Err(GatewayError::with_status(
                    401,
                    "登录态已过期，无法查询积分",
                ));
            }
        }
        let legacy_data = legacy.get("data").cloned().unwrap_or(Value::Null);
        if legacy.get("code").and_then(Value::as_i64) != Some(0) || legacy_data.is_null() {
            let message = legacy
                .get("msg")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| {
                    format!(
                        "积分查询返回异常 code={}",
                        legacy
                            .get("code")
                            .and_then(Value::as_i64)
                            .or_else(|| wallet_payload.get("code").and_then(Value::as_i64))
                            .map(|value| value.to_string())
                            .unwrap_or_else(|| "null".to_string())
                    )
                });
            return Err(GatewayError::with_status(502, message));
        }
        let instances = legacy_data
            .get("wallet_instances")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        (
            legacy_data.get("total_balance").cloned().unwrap_or(Value::Null),
            normalize_legacy_wallets(&instances),
            legacy_data,
        )
    };

    // 「即将过期」是附加信息：失败不阻塞余额展示（源实现把这条调用包在
    // 自己的 try/catch 里，注释写明「即将过期查询失败不阻塞余额展示」）
    let expiring = match userapi_get(credentials, POINTS_EXPIRING_PATH).await {
        Ok(payload) => {
            let data = payload.get("data").cloned().unwrap_or(Value::Null);
            if payload.get("code").and_then(Value::as_i64) == Some(0) && !data.is_null() {
                json!({
                    "points": data.get("expiring_points").cloned().unwrap_or(Value::Null),
                    "text": data.get("expiring_points_text").cloned().unwrap_or(Value::Null),
                    "totalPoints": data.get("total_points").cloned().unwrap_or(Value::Null),
                    "expireTimeValue": data.get("expire_time_value").cloned().unwrap_or(Value::Null),
                    "expireTimeType": data.get("expire_time_type").cloned().unwrap_or(Value::Null),
                })
            } else {
                Value::Null
            }
        }
        Err(_) => Value::Null,
    };

    Ok(json!({
        "totalBalance": total_balance,
        "wallets": wallets,
        "expiring": expiring,
        "raw": raw,
    }))
}

/// v1 钱包实例按 `wallet_scope` 归类（源实现 `normalizeLegacyWallets`）。
///
/// 源实现的判定逐条照抄：`status` 必须是 `active`（大小写不敏感）、`display`
/// 必须为真值、`balance` 累加；scope 去掉 `wallet_scope_` 前缀，取不到归 `other`。
fn normalize_legacy_wallets(instances: &[Value]) -> Vec<Value> {
    // 用一个保持首次出现顺序的表（源实现用 Map：插入序 = 遍历序）
    let mut merged: Vec<(String, f64)> = Vec::new();
    for instance in instances {
        let status = instance
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_lowercase();
        if status != "active" || !js_truthy(instance.get("display")) {
            continue;
        }
        let scope = instance
            .get("wallet_scope")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_lowercase()
            .replace("wallet_scope_", "");
        let scope = if scope.is_empty() { "other".to_string() } else { scope };
        let balance = number_or_null(instance.get("balance")).unwrap_or(0.0);
        match merged.iter_mut().find(|(kind, _)| kind == &scope) {
            Some((_, total)) => *total += balance,
            None => merged.push((scope, balance)),
        }
    }
    merged
        .into_iter()
        .map(|(kind, balance)| {
            let display_name = WALLET_SCOPE_LABELS
                .iter()
                .find(|(scope, _)| *scope == kind)
                .map(|(_, label)| (*label).to_string())
                .unwrap_or_else(|| "其他积分".to_string());
            json!({
                "type": kind,
                "displayName": display_name,
                "balance": balance,
                "balanceView": json_number_text(balance),
            })
        })
        .collect()
}

/// 查询订阅信息（源实现 `querySubscription`）。
async fn query_subscription(credentials: &AutoClawCredentials) -> Result<Value, GatewayError> {
    let mut body = json!({ "source_id": "autoclaw" });
    if !credentials.device_id.is_empty() {
        if let Some(map) = body.as_object_mut() {
            map.insert(
                "device_id".to_string(),
                Value::String(credentials.device_id.clone()),
            );
        }
    }
    let payload = userapi_post(credentials, SUBSCRIBE_INFO_PATH, &body).await?;
    if let Some(code) = payload.get("code").and_then(Value::as_i64) {
        if AUTH_EXPIRED_CODES.contains(&code) {
            return Err(GatewayError::with_status(
                401,
                "登录态已过期，无法查询订阅",
            ));
        }
        if code != 0 {
            let message = payload
                .get("msg")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("订阅查询返回异常 code={code}"));
            return Err(GatewayError::with_status(502, message));
        }
    }
    let data = payload.get("data").cloned().unwrap_or(Value::Null);
    if data.is_null() {
        return Ok(Value::Null);
    }
    Ok(normalize_subscription(&data))
}

/// 订阅信息 → 统一形状的 `subscription`（源实现 `normalizeSubscription`）。
///
/// 上游的订阅结构**没有稳定形状**：源实现写了一段「在若干候选键里深搜第一个非空值」
/// 的逻辑（`findDeep`，候选容器 `subscribeInfo` / `subscribe` / `data` /
/// `memberInfo` / `vipInfo`）。这里照搬同一策略 —— 上游字段名一变，按固定路径
/// 取值就会静默变成一片 null，而深搜至少还能捞到。
fn normalize_subscription(data: &Value) -> Value {
    // 根 + 第一个候选容器（源实现 `findDeep` 的 `queue = [root, nested]`）。
    // 两个根在，因此每个键都有两次命中机会：先根后容器（源实现的遍历顺序）。
    let nested = ["subscribeInfo", "subscribe", "data", "memberInfo", "vipInfo"]
        .iter()
        .filter_map(|key| data.get(*key))
        .find(|value| value.is_object());
    let mut roots: Vec<&Value> = vec![data];
    if let Some(nested) = nested {
        roots.push(nested);
    }
    /// 在若干候选根里深度优先找第一个键命中且非空的值。
    fn find_deep(roots: &[&Value], keys: &[&str]) -> Option<Value> {
        let mut pending: Vec<&Value> = roots.to_vec();
        // 广度上限：上游若给了一个巨大的对象图，深搜不能无限展开
        // （release 是 panic=abort，这里不 panic、只是到此为止）
        let mut visited = 0usize;
        while let Some(current) = pending.pop() {
            visited += 1;
            if visited > 200 {
                break;
            }
            let Some(object) = current.as_object() else {
                continue;
            };
            for (key, value) in object {
                if keys.contains(&key.as_str()) && !is_blank(value) {
                    return Some(value.clone());
                }
                if value.is_object() {
                    pending.push(value);
                }
            }
        }
        None
    }
    let text_of = |value: Option<Value>| -> String {
        match value {
            Some(Value::String(text)) if !text.trim().is_empty() => text.trim().to_string(),
            Some(Value::Number(number)) => number.to_string(),
            _ => String::new(),
        }
    };
    // 额度字段：数值透传；上游偶尔给字符串（源实现的 `pickNumber` 只认数字，
    // 这里多认一种字符串形态 —— 显示「1000」比显示「—」更接近事实）
    let quota_of = |keys: &[&str]| -> Value {
        find_deep(&roots, keys)
            .and_then(|value| match value {
                Value::Number(number) => number.as_f64(),
                Value::String(text) => text.trim().parse::<f64>().ok(),
                _ => None,
            })
            .filter(|value| value.is_finite())
            .map(|value| json!(value))
            .unwrap_or(Value::Null)
    };
    json!({
        "planName": text_of(find_deep(&roots, &[
            "productName", "planName", "memberName", "subscribeName", "vipName",
        ])),
        "status": text_of(find_deep(&roots, &["status", "subscribeStatus", "memberStatus"])),
        // expireAt 上游既可能是字符串也可能是数字（源实现两种都接）
        "expireAt": find_deep(&roots, &[
            "expireAt", "expireTime", "expiredAt", "endTime", "validEndTime",
        ])
        .unwrap_or(Value::Null),
        "remainQuota": quota_of(&[
            "remainQuota", "remainingQuota", "remainCredits", "availableCredits", "leftQuota",
        ]),
        "totalQuota": quota_of(&["totalQuota", "credits", "totalCredits"]),
    })
}

/// 发一次带签名的 userapi GET（源实现 `userapiGetFor` 的对等物）。
async fn userapi_get(
    credentials: &AutoClawCredentials,
    path: &str,
) -> Result<Value, GatewayError> {
    let url = format!("{}{path}", credentials::userapi_base_url());
    let headers = signed_auth_headers(&credentials.token);
    let response = send_raw("GET", &url, None, &headers, None, Some(REQUEST_TIMEOUT_MS))
        .await
        .map_err(|error| describe_transport("积分查询", &error))?;
    if response.status == 401 {
        return Err(GatewayError::with_status(
            401,
            "登录态已过期，无法查询积分",
        ));
    }
    if !response.ok {
        return Err(GatewayError::with_status(
            response.status as i32,
            format!("积分查询返回 HTTP {}", response.status),
        ));
    }
    Ok(response.payload.unwrap_or(Value::Null))
}

/// 发一次带签名的 userapi POST（源实现 `userapiPostFor` 的对等物）。
async fn userapi_post(
    credentials: &AutoClawCredentials,
    path: &str,
    body: &Value,
) -> Result<Value, GatewayError> {
    let url = format!("{}{path}", credentials::userapi_base_url());
    let headers = signed_auth_headers(&credentials.token);
    let response = send_raw(
        "POST",
        &url,
        Some(body),
        &headers,
        None,
        Some(REQUEST_TIMEOUT_MS),
    )
    .await
    .map_err(|error| describe_transport("订阅查询", &error))?;
    if response.status == 401 {
        return Err(GatewayError::with_status(
            401,
            "登录态已过期，无法查询订阅",
        ));
    }
    if !response.ok {
        return Err(GatewayError::with_status(
            response.status as i32,
            format!("订阅查询返回 HTTP {}", response.status),
        ));
    }
    Ok(response.payload.unwrap_or(Value::Null))
}

/// 传输错误 → 可读的网关错误（超时给 504，与既有计费接口的分档口径一致）。
fn describe_transport(what: &str, error: &reqwest::Error) -> GatewayError {
    if error.is_timeout() {
        GatewayError::with_status(504, format!("{what}超时"))
    } else {
        GatewayError::with_status(502, format!("{what}失败: {error}"))
    }
}

/// 值是否为空（`null` / 空串；源实现 `value !== null && value !== undefined && value !== ''`）
fn is_blank(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(text) => text.is_empty(),
        _ => false,
    }
}

/// JS 真值判定（`Boolean(x)`）：null/false/0/""/空数组 为假。
fn js_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64().map(|item| item != 0.0).unwrap_or(false),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(_)) => true,
    }
}

/// 数值透传（缺失/非数字给 null；前端显示成「—」而不是 0）
fn number_or_null(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64().filter(|item| item.is_finite()),
        Value::String(text) => text
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|item| item.is_finite()),
        _ => None,
    }
}

/// 数值 → 展示串（源实现 `String(balance)`：整数不带小数点）。
fn json_number_text(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 9.007_199_254_740_992e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}