//! 小浣熊的余额 / 积分与订阅查询（移植来源 `account-balance.mjs` 的
//! `queryPoints` / `querySubscription`）。
//!
//! ── 上游长什么样（逐条对照源实现）─────────────────────────────
//!   - 余额：`GET {mainOrigin}/api/web/points/v1/balance`
//!     头 `Authorization: Bearer <token>`，响应 `{code, message?, data}`
//!     （`code` 非 0 视为失败，与源实现 `requestJson` 的判定一致）；
//!     `data.available_points` 是可用总量，另有四个钱包字段
//!     （`daily_points` / `monthly_points` / `reward_points` / `topup_points`）。
//!   - 订阅：`GET {mainOrigin}/api/web/auth/v1/entitlement_info`
//!     `data.office` / `data.code` 两套权益，各带 `pro_enable` / `active_plan`
//!     / `pro_expired_time`。
//!
//! ── 域名与 `RACCOON_MAIN_SITE_URL`（别和 `RACCOON_AUTH_API_BASE` 搞混）──
//! 源实现读的是 **`RACCOON_MAIN_SITE_URL`**（默认 `https://xiaohuanxiong.com`），
//! 而本项目既有的 `RACCOON_AUTH_API_BASE`（见 `raccoon/mod.rs`）是**鉴权接口**
//! 的完整前缀（`{origin}/api/web/auth/v1`）。两者默认指向同一个站点，但覆盖语义
//! 不同：本文件只复用主站 origin（`RACCOON_MAIN_SITE_URL`），**不从
//! `RACCOON_AUTH_API_BASE` 反推 origin** —— 那样会在用户只改了鉴权域（指向
//! 内网镜像）时把余额请求也一起改道，而这两条链路在源实现里是两个独立配置点。
//!
//! ── 出网代理（核对结论）──────────────────────────────────────
//! 源实现的 `queryPoints` 收到 `client` 参数但**从不使用**它（函数体内直接
//! `fetch`），余额与订阅都是**裸 fetch**，不走它自己的上游客户端。本网关里
//! 「裸 fetch」对应的是直连 —— 即 `send_raw(..., proxy = None, ...)`。
//! 这与刷新鉴权接口的既有取舍同一理由（见 `credentials::call_refresh_api` 的
//! 注释）：账号级代理是给转发（流式长请求）准备的出口，而主站域与 LLM 域是
//! 两个不同的站点。**不做**「顺手挂上账号代理」的改动，那会与源实现的行为分叉
//! （源实现里这条链路是直连的）。
//!
//! ── 超时 ────────────────────────────────────────────────────
//! 20 秒（源实现 `REQUEST_TIMEOUT_MS = 20_000`）。必须显式设：`egress` 的默认
//! read_timeout 是 600 秒（给 SSE 长连接留的），不设总超时的话一个挂住的余额
//! 请求能让前端的批量查询转圈十分钟。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：零 unwrap/expect/panic，取值一律走 Option 链。

use serde_json::{json, Map, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::core::auth_http::send_raw;
use crate::server::errors::GatewayError;

use super::credentials;

/// 余额 / 订阅接口的请求超时（源实现 `REQUEST_TIMEOUT_MS`）
const REQUEST_TIMEOUT_MS: u64 = 20_000;

/// 默认主站 origin（源实现 `apiOrigin()` 的兜底值）
const DEFAULT_MAIN_SITE_URL: &str = "https://xiaohuanxiong.com";

/// 钱包字段 → 展示名（逐字照抄源实现 `WALLET_LABELS`）。
///
/// 顺序即展示顺序：源实现用 `Object.entries(WALLET_LABELS)` 遍历，JS 对象的
/// 字符串键按插入序枚举，所以这里的数组顺序必须与源实现的对象字面量一致。
const WALLET_LABELS: &[(&str, &str)] = &[
    ("daily_points", "每日积分"),
    ("monthly_points", "每月会员积分"),
    ("reward_points", "奖励积分"),
    ("topup_points", "充值积分"),
];

/// 主站 origin（`RACCOON_MAIN_SITE_URL` 可覆盖；去空白与末尾斜杠）。
pub(super) fn main_site_url() -> String {
    std::env::var("RACCOON_MAIN_SITE_URL")
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_MAIN_SITE_URL.to_string())
}

/// 查询某账号的余额 / 积分（归一化形状见 `ProviderAdapter::query_usage` 的文档）。
///
/// ── 订阅是「尽力而为」而不是必需 ──────────────────────────────
/// 源实现把余额与订阅拆成两个接口（`/api/accounts/balances` 与
/// `/api/accounts/subscriptions`），余额那条**只查积分**。这里合成一次调用，
/// 但订阅的失败**不影响余额结果**（`subscription` 给 null 并往 `raw` 里记一条
/// 原因）：订阅是附加信息，为了它把整个面板变成红色的「查询失败」得不偿失 ——
/// 与源实现给 AutoClaw 的 `points/expiring` 兜底同一取向。
pub(super) async fn query_usage(
    store: &AccountStore,
    account_id: &str,
) -> Result<Value, GatewayError> {
    let credentials = credentials::snapshot_for(store, account_id)?;
    if credentials.token.trim().is_empty() {
        return Err(GatewayError::with_status(
            401,
            "该账号没有可用凭证，无法查询积分",
        ));
    }
    let origin = main_site_url();
    let points = request_json(&origin, "/api/web/points/v1/balance", &credentials.token).await?;
    if !points.is_object() {
        return Err(GatewayError::with_status(
            502,
            "积分余额响应结构异常（期望一个 JSON 对象）",
        ));
    }

    let available = to_int(points.get("available_points"));
    let wallets: Vec<Value> = WALLET_LABELS
        .iter()
        .filter_map(|(key, display_name)| {
            let balance = to_int(points.get(*key))?;
            Some(json!({
                "type": key,
                "displayName": display_name,
                "balance": balance,
            }))
        })
        .collect();

    // 订阅失败只记原因，不阻断余额展示（见上）
    let (subscription, subscription_error) =
        match request_json(&origin, "/api/web/auth/v1/entitlement_info", &credentials.token).await {
            Ok(data) => (normalize_subscription(&data), Value::Null),
            Err(error) => (Value::Null, Value::String(error.message)),
        };

    let mut raw = Map::new();
    raw.insert("points".to_string(), points);
    if !subscription_error.is_null() {
        raw.insert("subscriptionError".to_string(), subscription_error);
    }
    Ok(json!({
        "available": available,
        // 小浣熊的积分口径就是「积分」（源实现的中文标签与文案都用它）
        "unit": "积分",
        "wallets": wallets,
        "subscription": subscription,
        "raw": Value::Object(raw),
    }))
}

/// 发一次 GET 并解包 `{code, message, data}`（源实现 `requestJson` 的判定链）。
///
/// 判定顺序逐条照抄：
///   ① 传输失败 → 超时给 504「积分查询超时」，其余给 502（源实现把两者压成
///      同一个 `BalanceError` 的 502，这里沿用本项目计费接口的分档口径）；
///   ② HTTP 401 → 401「登录态已过期」（调用方据此走刷新重试）；
///   ③ 非 2xx → 502 带上游状态码；
///   ④ `code` 存在且非 0 → 502 取 `message`；
///   ⑤ 取 `data ?? payload`（源实现 `payload?.data ?? payload`）。
async fn request_json(origin: &str, path: &str, token: &str) -> Result<Value, GatewayError> {
    request_json_ex(origin, "GET", path, token, &[], REQUEST_TIMEOUT_MS).await
}

/// `request_json` 的通用形态：方法与附加头可指定（桌面登录积分链路需要
/// `X-Client-Platform` / `X-Client-Version` / `X-Raccoon-Language` 三个头，
/// 账单核对需要更长的超时 —— 见 `claim_daily_grant` 的说明）。
///
/// 其余判定链与 `request_json` 完全一致；`method` 目前只有 GET / POST 两种用法
/// （上游的积分域没有其它方法）。
async fn request_json_ex(
    origin: &str,
    method: &str,
    path: &str,
    token: &str,
    extra_headers: &[(String, String)],
    timeout_ms: u64,
) -> Result<Value, GatewayError> {
    let url = format!("{origin}{path}");
    let mut headers: Vec<(String, String)> = vec![
        ("Accept".to_string(), "application/json".to_string()),
        // 源实现 `token.replace(/^Bearer\s+/i, '')` 后再拼 Bearer：
        // 用户粘贴的 token 可能自带前缀，重复拼会得到 `Bearer Bearer xxx`。
        // 复用 `jwt::strip_bearer`（同目录、同一套「按字节前缀比较」的安全实现）——
        // 自己再写一份 `trimmed[..7]` 会在 token 以多字节字符开头时 panic
        (
            "Authorization".to_string(),
            format!("Bearer {}", super::jwt::strip_bearer(token)),
        ),
    ];
    headers.extend(extra_headers.iter().cloned());
    // 直连（源实现这条链路是裸 fetch，见模块头）
    let response =
        send_raw(method, &url, None, &headers, None, Some(timeout_ms)).await.map_err(|error| {
            if error.is_timeout() {
                GatewayError::with_status(504, "积分查询超时")
            } else {
                GatewayError::with_status(502, format!("积分查询失败: {error}"))
            }
        })?;
    if response.status == 401 {
        return Err(GatewayError::with_status(
            401,
            "登录态已过期，无法查询积分",
        ));
    }
    if !response.ok {
        return Err(GatewayError::with_status(
            502,
            format!("积分查询返回 HTTP {}", response.status),
        ));
    }
    let payload = response.payload.unwrap_or(Value::Null);
    if let Some(code) = payload.get("code").and_then(Value::as_i64) {
        if code != 0 {
            let message = payload
                .get("message")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("积分查询返回异常 code={code}"));
            return Err(GatewayError::with_status(502, message));
        }
    }
    // `payload?.data ?? payload`：data 为 null（显式给了 null）时源实现的 `??`
    // 会落到 payload 本身，所以这里只在 data **缺失或为 null** 时回落
    let data = payload.get("data").cloned().unwrap_or(Value::Null);
    Ok(if data.is_null() { payload } else { data })
}

/// 订阅权益 → 统一形状的 `subscription`（源实现 `querySubscription` 的字段）。
///
/// 小浣熊有两套权益（办公端 `office` 与代码端 `code`），源实现两个都返回。
/// 统一形状里只留一组「套餐 / 状态 / 到期时间」，因此**优先取办公端的**
/// （那是小浣熊桌面端的主入口），办公端没有时回落代码端；两套都保留在
/// `raw` 里供排障。
///
/// `expireAt` 是上游给的字符串形态时间（源实现直接透出，不做解析），
/// 这里同样原样保留 —— 前端按字符串展示，转换成时间戳会让「上游给的是
/// 本地时间串还是 ISO」这个不确定性冒到界面上。
fn normalize_subscription(data: &Value) -> Value {
    let office = data.get("office").filter(|value| value.is_object());
    let code = data.get("code").filter(|value| value.is_object());
    let pick = |key: &str| -> Option<Value> {
        office
            .and_then(|value| value.get(key))
            .or_else(|| code.and_then(|value| value.get(key)))
            .filter(|value| !value.is_null())
            .cloned()
    };
    let pro_enabled = office
        .and_then(|value| value.get("pro_enable"))
        .and_then(Value::as_bool)
        .or_else(|| {
            code.and_then(|value| value.get("pro_enable"))
                .and_then(Value::as_bool)
        })
        .unwrap_or(false);
    if office.is_none() && code.is_none() {
        return Value::Null;
    }
    let plan_type = pick("active_plan")
        .and_then(|plan| plan.get("type").cloned())
        .or_else(|| pick("plan_type"))
        .unwrap_or(Value::Null);
    let expired_at = pick("pro_expired_time")
        .or_else(|| {
            pick("active_plan")
                .and_then(|plan| plan.get("subscription_expired_at").cloned())
        })
        .unwrap_or(Value::Null);
    json!({
        "planName": plan_type.as_str().unwrap_or(""),
        // 状态沿用上游语义：开了 pro 是 active，没开是 none
        // （源实现只给布尔 pro_enable，没有更细的状态机）
        "status": if pro_enabled { "active" } else { "none" },
        "expireAt": expired_at,
        "remainQuota": Value::Null,
        "totalQuota": Value::Null,
    })
}

/// 数值取整（源实现 `toInt`：非有限值给 None，有限值取 floor）。
///
/// 返回 `Option<i64>` 而不是 f64：积分是整数口径，源实现也做 `Math.floor`。
fn to_int(value: Option<&Value>) -> Option<i64> {
    let number = match value? {
        Value::Number(number) => number.as_f64()?,
        // 上游偶尔把数字写成字符串（源实现的 `Number(value)` 两种都认）
        Value::String(text) => text.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    if !number.is_finite() {
        return None;
    }
    Some(number.floor() as i64)
}

// ─── 每日「签到」（桌面登录积分发放）─────────────────────────
//
// 小浣熊没有 WorkBuddy 那种「每日签到」接口；它的每日奖励走的是**桌面端
// 登录积分**链路：官方桌面客户端每次启动会打两个接口，服务端据此按天发放
// 每日积分（1.x 源实现 account-balance.mjs / account-routes.mjs 的
// desktop-grant 已验证）。这里复刻同一条链路作为「签到」：
//   ① POST /api/web/desktop/v1/login/points/grant —— 首次桌面登录奖励
//      （一次性，幂等；已发放过时上游返回 granted=false）；
//   ② GET  /api/web/office/v3/setting_info —— **每日积分的发放触发器**
//      （按天幂等），发放通知在响应的 point_grant_popups / point_grant_toast；
//   ③ GET  /api/web/points/v1/bills —— 核对今日实际入账，作为权威结果
//      （账单接口慢，limit=20 实测约 9 秒，用独立超时兜底；失败降级，
//      不影响 ①② 的领取本身）。

/// 账单核对的拉取条数与超时（源实现 GRANT_CHECK_LIMIT / GRANT_CHECK_TIMEOUT_MS）
const GRANT_CHECK_LIMIT: usize = 20;
const GRANT_CHECK_TIMEOUT_MS: u64 = 25_000;

/// 客户端身份头：官方桌面端启动时带的两个头（源实现 grantDesktopLoginPoints）
fn desktop_client_headers() -> Vec<(String, String)> {
    vec![
        ("X-Client-Platform".to_string(), "desktop-windows".to_string()),
        ("X-Client-Version".to_string(), "v1.0.0".to_string()),
    ]
}

/// 小浣熊账号的每日签到（claim 形状与 WorkBuddy 的 claim_daily_checkin 对齐：
/// `{success, msg}` —— billing::checkin 的汇总只认这两个字段）。
///
/// `success` 的口径是「**今日积分确有入账**」（账单核对），而不是某一步的
/// HTTP 成败：发放是服务端在 setting_info 上按天幂等完成的，重复执行
/// 不会重复入账，账单里看得到就是领到了。
pub async fn claim_daily_grant(
    store: &AccountStore,
    account_id: &str,
) -> Result<Value, GatewayError> {
    let credentials = credentials::snapshot_for(store, account_id)?;
    if credentials.token.trim().is_empty() {
        return Err(GatewayError::with_status(
            401,
            "该账号没有可用凭证，无法签到",
        ));
    }
    let origin = main_site_url();

    // ① 桌面登录奖励（一次性）。失败不阻断：它对老账号注定是 granted=false，
    //    真正的每日发放在 ② 里 —— 与 1.x 源实现「单步失败继续走完」一致
    let desktop_grant = match request_json_ex(
        &origin,
        "POST",
        "/api/web/desktop/v1/login/points/grant",
        &credentials.token,
        &desktop_client_headers(),
        REQUEST_TIMEOUT_MS,
    )
    .await
    {
        Ok(data) => {
            json!({ "granted": data.get("granted").and_then(Value::as_bool) == Some(true) })
        }
        Err(error) => json!({ "granted": false, "error": error.message }),
    };

    // ② 每日积分发放触发器（按天幂等）。失败必须体现在结果里 ——
    //    这步不成功当日就没有发放，账单核对会给出「今日未见入账」
    let mut settings_headers = desktop_client_headers();
    settings_headers.push(("X-Raccoon-Language".to_string(), "zh".to_string()));
    let settings = match request_json_ex(
        &origin,
        "GET",
        "/api/web/office/v3/setting_info",
        &credentials.token,
        &settings_headers,
        REQUEST_TIMEOUT_MS,
    )
    .await
    {
        Ok(data) => {
            // 发放通知原样透出（结构与上游一致，界面不深入解析）
            json!({
                "popups": data.get("point_grant_popups").cloned().unwrap_or(Value::Null),
                "toast": data.get("point_grant_toast").cloned().unwrap_or(Value::Null),
            })
        }
        Err(error) => json!({ "popups": Value::Null, "toast": Value::Null, "error": error.message }),
    };

    // ③ 账单核对今日入账（慢接口独立超时；失败降级为 grantsError）
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let bills_path = format!(
        "/api/web/points/v1/bills?paging.limit={GRANT_CHECK_LIMIT}&paging.offset=0"
    );
    let (grants, grants_error) = match request_json_ex(
        &origin,
        "GET",
        &bills_path,
        &credentials.token,
        &[],
        GRANT_CHECK_TIMEOUT_MS,
    )
    .await
    {
        Ok(data) => {
            let items = data.get("items").and_then(Value::as_array);
            let grants: Vec<Value> = items
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| {
                            // points>0 且 created_at 的日期部分 == 本地今天
                            // （源实现直接比对字符串前 10 位，口径一致）
                            let points = to_int(item.get("points"))?;
                            if points <= 0 {
                                return None;
                            }
                            let created_at =
                                item.get("created_at").and_then(Value::as_str).unwrap_or("");
                            if created_at.get(..10) != Some(today.as_str()) {
                                return None;
                            }
                            Some(json!({
                                "name": item.get("event_name").and_then(Value::as_str).unwrap_or("积分发放"),
                                "points": points,
                                "at": created_at.replace('T', " ").get(11..19).unwrap_or(""),
                            }))
                        })
                        .collect()
                })
                .unwrap_or_default();
            (grants, None)
        }
        Err(error) => (Vec::new(), Some(error.message)),
    };

    let granted_today = grants.iter().map(|item| to_int(item.get("points")).unwrap_or(0)).sum::<i64>();
    let mut msg_parts: Vec<String> = Vec::new();
    if desktop_grant.get("granted").and_then(Value::as_bool) == Some(true) {
        msg_parts.push("桌面登录奖励已发放".to_string());
    }
    if granted_today > 0 {
        msg_parts.push(format!("今日积分 +{granted_today}"));
    } else if grants_error.is_some() {
        // 账单核对失败拿不到权威结果，不谎报成功
        msg_parts.push("未能核对今日入账".to_string());
    } else {
        msg_parts.push("今日未见积分入账（可能已领取）".to_string());
    }
    if let Some(error) = grants_error.as_deref() {
        msg_parts.push(format!("账单核对失败：{error}"));
    } else if desktop_grant.get("error").and_then(Value::as_str).is_some() {
        // 首次奖励领取失败对老账号是常态（granted=false），但真错误还是提一句
        msg_parts.push("桌面登录奖励未领取".to_string());
    }

    Ok(json!({
        "success": granted_today > 0,
        "msg": msg_parts.join("；"),
        "grantsToday": grants,
        "desktopGrant": desktop_grant,
        "settings": settings,
        "grantsError": grants_error.map(Value::String).unwrap_or(Value::Null),
    }))
}