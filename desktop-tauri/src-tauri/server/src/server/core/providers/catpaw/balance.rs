//! CatPaw 的余额 / 积分查询（**改走客户端自己的网关 API**）。
//!
//! ── 上游长什么样 ─────────────────────────────────────────────
//! ```text
//!   GET https://catx.nocode.cn/api/gateway/credit/balance
//!   X-Auth-Token: <auth.json 的 auth.accessToken>
//!   响应 {code, message, data: {availableCredits, userPlan{…}}, errorCode}
//! ```
//! `availableCredits` 是**数字字符串**（如 `"0.00"`），`userPlan` 带套餐名 /
//! 是否专业版 / 到期时间。没有 `totalCredits`／`frozenCredits`／`expiredCredits`
//! —— 那是下面那个网页接口才有的口径。
//!
//! ── ⚠️ 为什么从 `credit.catpaw.meituan.com` 换到这里（本次修正）──
//! 原实现（移植自 `catpaw-local-proxy/account-balance.mjs`）打的是网页积分中心
//! `credit.catpaw.meituan.com/api/credit/balance`，并据此要求用户**手填**
//! 一份 `token2` 网页会话凭证。本地实测（2026-09，逐条复现）表明这套归因是错的：
//!
//!   1. **真正生效的 cookie 是 `mt_c_token`，不是 `token2`** —— 单发
//!      `Cookie: mt_c_token=<凭证>` 就 200；单发 `token2=<同一个值>` 反而 401。
//!      原注释「必须四个 cookie 名同时出现」不成立（四个同值时当然也 200，
//!      所以原实现能用，只是把结论归错了因）。
//!   2. **`token2` 的值就是转发用的那个 `accessToken`** —— 客户端登录后把同一个
//!      token 批量写进 `.meituan.com` / `.sankuai.com` 域的多个 cookie 名
//!      （`mt_c_token` / `pay_param_token` / `token2` / `oops`），那是客户端自己的
//!      行为，不是服务端要求。所以「另配一份凭证」这个前提本身就不存在。
//!   3. `catx.nocode.cn` 这个网关 API 只认 **`X-Auth-Token`** 头（`X-Passport-Token` /
//!      `Cookie` / `Authorization` 全部 401），且它的凭证同样是那个 `accessToken`。
//!
//! 于是本模块现在**不需要任何额外配置**：凭证直接复用转发那条链的
//! [`catpaw::credentials::snapshot_for`]（账号记录 / 桌面端实时登录态 / 环境变量
//! 三处来源一次覆盖），用户填不填 `token2` 都能查。旧字段仍然保留兼容
//! （见下），但不再是查询的前提。
//!
//! ── 兼容旧的 `balanceToken` / `balanceCookie.token2` ──────────
//! 老用户可能已经填过 `balanceToken`，原项目导入的数据里也可能有
//! `balanceCookie.token2`。两者现在都只当作**凭证来源的一个补充**：转发凭证
//! 拿不到时（例如账号记录里没有 token）才回退到它们，而不是作为前置要求。
//! 这样「填过的人不受影响、没填的人也不用再填」。
//!
//! ── 出网代理 ────────────────────────────────────────────────
//! 直连（`proxy = None`）：credit 域与 LLM 域是两个站点，账号代理是给转发那条
//! 流式长请求准备的出口（与小浣熊余额、两家刷新接口同一取舍）。
//!
//! ── 超时 ────────────────────────────────────────────────────
//! 15 秒。必须显式设：`egress` 的默认 read_timeout 是 600 秒（给 SSE 留的），
//! 不设总超时会让挂住的请求把批量查询拖到前端一直转圈。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：零 unwrap/expect/panic。

use serde_json::{json, Map, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::core::auth_http::send_raw;
use crate::server::core::providers::adapter::usage_not_configured;
use crate::server::errors::GatewayError;

/// 积分查询接口（客户端自己的网关 API，凭证与转发同一个 `accessToken`）
const BALANCE_URL: &str = "https://catx.nocode.cn/api/gateway/credit/balance";

/// 请求超时
const REQUEST_TIMEOUT_MS: u64 = 15_000;

/// 鉴权头名。**只认这一个**：实测 `X-Passport-Token` / `Cookie` / `Authorization`
/// 在这个域名下全部 401（见模块头）。
const AUTH_HEADER: &str = "X-Auth-Token";

/// 客户端自己会带的标记头（`gray-set: new-agent-sdk`）。
///
/// 实测**非必需**（只带 `X-Auth-Token` 就 200），这里仍然带上：它是客户端请求的
/// 真实形态，上游若哪天按灰度分组收紧策略，带上它更接近官方调用。
const GRAY_SET: (&str, &str) = ("gray-set", "new-agent-sdk");

/// 账号记录里的旧余额凭证字段（用户可在账号设置里填）。
///
/// 保留为**回退来源**（见模块头）：新装用户不必填，已填的用户也不用清理。
pub const BALANCE_TOKEN_FIELD: &str = "balanceToken";

/// 查询某账号的余额 / 积分（归一化形状见 `ProviderAdapter::query_usage` 的文档）。
pub(super) async fn query_usage(
    store: &AccountStore,
    account_id: &str,
) -> Result<Value, GatewayError> {
    let record = store.catpaw_account_record(account_id);
    let token = resolve_token(store, account_id, record.as_ref())?;

    let headers: Vec<(String, String)> = vec![
        ("Accept".to_string(), "application/json".to_string()),
        (AUTH_HEADER.to_string(), token),
        (GRAY_SET.0.to_string(), GRAY_SET.1.to_string()),
    ];

    let response = send_raw(
        "GET",
        BALANCE_URL,
        None,
        &headers,
        None,
        Some(REQUEST_TIMEOUT_MS),
    )
    .await
    .map_err(|error| {
        if error.is_timeout() {
            GatewayError::with_status(504, "积分查询超时")
        } else {
            GatewayError::with_status(502, format!("积分查询请求失败: {error}"))
        }
    })?;

    let payload = response.payload.unwrap_or(Value::Null);
    let code = payload.get("code").and_then(Value::as_i64);
    // 这个接口的两条凭证失效路径（实测）：4010 = 没带 token，4011 = token 无效。
    // HTTP 401 也同样处理（两条路径都要判，源实现同此口径）。
    let unauthorized = response.status == 401 || matches!(code, Some(4010) | Some(4011));
    if unauthorized {
        return Err(GatewayError::with_status(
            401,
            "积分查询凭证已失效，请在客户端重新登录后重新导入登录态",
        ));
    }
    if !response.ok {
        return Err(GatewayError::with_status(
            502,
            format!("积分查询返回 HTTP {}", response.status),
        ));
    }
    let data = payload.get("data").cloned().unwrap_or(Value::Null);
    if code != Some(0) || data.is_null() {
        return Err(GatewayError::with_status(502, error_message(&payload)));
    }

    let user_plan = data.get("userPlan").cloned().unwrap_or(Value::Null);
    // 只有「可用」一个数：没有钱包概念，因此不给 total/frozen/expired 造零值
    // （缺失就是缺失，前端显示成「—」，见 number_or_null 的说明）。
    let wallets: Vec<Value> = [("available", "可用积分", data.get("availableCredits"))]
        .into_iter()
        .filter_map(|(kind, display_name, value)| {
            let balance = number_or_null(value)?;
            Some(json!({ "type": kind, "displayName": display_name, "balance": balance }))
        })
        .collect();

    let mut raw = Map::new();
    raw.insert("balance".to_string(), data.clone());
    Ok(json!({
        "available": number_or_null(data.get("availableCredits")),
        // 美团侧的额度单位就是它自己的 credits
        "unit": "credits",
        "wallets": wallets,
        // 这个接口没有订阅对象，但套餐信息在 `userPlan` 里 —— 打包成前端已认得的
        // 订阅形状（`planName` / `expireTime`），免得为一家新造一个展示分支。
        "subscription": subscription_of(&user_plan),
        "raw": Value::Object(raw),
    }))
}

/// 解析这次查询要用的 token。
///
/// 顺序：
///   1. **转发链路的凭证**（`snapshot_for`：账号记录 / 桌面端实时登录态 /
///      环境变量旁路都覆盖）—— 新装与旧装用户都能直接命中，无需任何配置；
///   2. 旧字段回退：账号记录里的 `balanceToken`，
///      或原项目导入留下的 `balanceCookie.token2`（见模块头）。
///
/// 两处都没有才报「未配置」——那时账号本身多半也是没有凭证的。
fn resolve_token(
    store: &AccountStore,
    account_id: &str,
    record: Option<&Value>,
) -> Result<String, GatewayError> {
    if let Ok(credentials) = super::credentials::snapshot_for(store, account_id) {
        if !credentials.token.is_empty() {
            return Ok(credentials.token);
        }
    }
    if let Some(record) = record {
        if let Some(token) = legacy_balance_token(record) {
            return Ok(token);
        }
    }
    Err(usage_not_configured("CatPaw", "登录凭证"))
}

/// 旧数据里的余额凭证：`balanceToken`，或原项目导入留下的 `balanceCookie.token2`。
fn legacy_balance_token(record: &Value) -> Option<String> {
    let direct = record
        .get(BALANCE_TOKEN_FIELD)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    direct.or_else(|| {
        record
            .get("balanceCookie")
            .and_then(|cookie| cookie.get("token2"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

/// `userPlan` → 前端已认得的订阅形状（`null` 表示这个账号没有套餐信息）。
fn subscription_of(user_plan: &Value) -> Value {
    if !user_plan.is_object() {
        return Value::Null;
    }
    let name = ["planName", "planId"]
        .iter()
        .find_map(|key| user_plan.get(*key).and_then(Value::as_str))
        .unwrap_or("");
    json!({
        "name": name,
        "expireAt": user_plan.get("expireTime").cloned().unwrap_or(Value::Null),
        // 上游字段名照实带上：前端若要区分「专业版」都够用，不必在这里改名
        "autoRenew": user_plan.get("autoRenew").cloned().unwrap_or(Value::Null),
        "pro": user_plan.get("pro").cloned().unwrap_or(Value::Null),
    })
}

/// 上游的业务错误文案（`message` 为空时给一个带 code 的兜底）。
fn error_message(payload: &Value) -> String {
    payload
        .get("message")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            format!(
                "积分查询返回异常{}",
                payload
                    .get("code")
                    .and_then(Value::as_i64)
                    .map(|value| format!(" code={value}"))
                    .unwrap_or_default()
            )
        })
}

/// 数值字段透传（缺失/非数字给 None —— 前端把它显示成「—」而不是 0）。
///
/// 上游把 `availableCredits` 给成**字符串**（`"0.00"`），因此这里必须同时接受
/// 字符串与数字两种形态。
fn number_or_null(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64().filter(|item| item.is_finite()),
        Value::String(text) => text.trim().parse::<f64>().ok().filter(|item| item.is_finite()),
        _ => None,
    }
}
