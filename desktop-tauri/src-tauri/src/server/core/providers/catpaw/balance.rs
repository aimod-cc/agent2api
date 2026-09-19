//! CatPaw 的余额查询（移植来源 `account-balance.mjs` 的 `queryBalance`）。
//!
//! ── 上游长什么样（逐条对照源实现）─────────────────────────────
//!   `GET https://credit.catpaw.meituan.com/api/credit/balance`
//!   响应 `{code, message?, data: {userId, totalCredits, availableCredits,
//!   frozenCredits, expiredCredits}}`，HTTP 401 或 `code === 401` 都表示凭证失效。
//!
//! ── ⚠️ 为什么需要 token2，而不是转发用的登录态（本次移植的关键约束）──
//! 这个接口要的是**网页会话凭证**：源实现注释写明「要求 passport 会话 token
//! 同时出现在多个 cookie 名下（`token2` / `mt_c_token` / `isid` / `oops`，
//! 值相同），只发 `token2` 会 401」。而网关转发用的是 `X-Passport-Token`
//! （见 `conversation::CatPawCredentials`），那是**同一个登录态的另一份投影**
//! —— 名字像、来源像，但在这个接口上不认。原项目因此把它做成**单独配置的一项**
//! （账号文件顶层的 `balanceCookies[id].token2`，带 `token2Tail` / `savedAt`）。
//!
//! 所以本模块的凭证来源有两个，按优先级：
//!   1. 账号记录里的 `balanceToken`（用户在账号设置里填的，见
//!      `account_store::catpaw_accounts`）；
//!   2. 旧数据导入留下的 `balanceCookie.token2`（`catpaw_import.rs` 从原项目的
//!      `balanceCookies` 搬过来的字段）—— 那份数据本来就是为这个功能存的，
//!      现在余额功能接回来了，正好把它用上，导入过旧数据的用户零配置可用。
//! 两者都没有时返回**可识别的「未配置」错误**（见下），而不是「失败」。
//!
//! ── 为什么「未配置」是中性的提示而不是红色错误 ─────────────────
//! 「没填 token2」不是故障：用户什么都没做错，账号本身也完全正常（转发照跑），
//! 只是他没告诉网关「这台机器的网页登录态长什么样」。把它渲染成红色的
//! 「查询失败」会让用户以为账号坏了、去排查一个不存在的故障。因此错误文案由
//! `adapter::usage_not_configured` 统一给出（400 + `usage_not_configured` 标记），
//! 前端据此显示成类似「未配置查询」的中性提示。
//!
//! ── 出网代理（核对结论）──────────────────────────────────────
//! 源实现的 `queryBalance` 是**裸 fetch**（没有任何代理参数、没有 client 注入），
//! 对应本项目的直连：`send_raw(..., proxy = None, ...)`。**不挂账号代理** ——
//! credit 域与 LLM 域是两个站点，账号代理是给转发那条流式长请求准备的出口
//! （与小浣熊余额、两家刷新接口同一取舍）。
//!
//! ── 超时 ────────────────────────────────────────────────────
//! 15 秒（源实现 `REQUEST_TIMEOUT_MS = 15000`）。必须显式设：`egress` 的默认
//! read_timeout 是 600 秒（给 SSE 留的），不设总超时会让挂住的请求把批量查询
//! 拖到前端一直转圈。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：零 unwrap/expect/panic。

use serde_json::{json, Map, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::core::auth_http::send_raw;
use crate::server::core::providers::adapter::usage_not_configured;
use crate::server::errors::GatewayError;

/// 余额接口（源实现 `BALANCE_URL`）
const BALANCE_URL: &str = "https://credit.catpaw.meituan.com/api/credit/balance";

/// 请求超时（源实现 `REQUEST_TIMEOUT_MS`）
const REQUEST_TIMEOUT_MS: u64 = 15_000;

/// 余额接口要求的 cookie 名（源实现 `PASSPORT_COOKIE_NAMES`）。
///
/// **值相同、名字四个都要发**：源实现实测只发 `token2` 会 401。这不是猜测出来的
/// 兼容写法，是原项目踩过坑之后写进注释的结论，照抄。
const PASSPORT_COOKIE_NAMES: &[&str] = &["token2", "mt_c_token", "isid", "oops"];

/// 浏览器 UA（源实现 `BROWSER_UA`）。
///
/// 必须显式带：`egress` 的默认 UA 是 `undici`（给计费接口用的），而 credit 域
/// 是网页接口，非浏览器 UA 会被判为非法请求。
const BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                          (KHTML, like Gecko) Chrome/151.0.0.0 Safari/537.36";

/// 账号记录里的余额凭证字段（用户可在账号设置里填）。
pub const BALANCE_TOKEN_FIELD: &str = "balanceToken";

/// 查询某账号的余额（归一化形状见 `ProviderAdapter::query_usage` 的文档）。
pub(super) async fn query_usage(
    store: &AccountStore,
    account_id: &str,
) -> Result<Value, GatewayError> {
    let record = store
        .catpaw_account_record(account_id)
        .ok_or_else(|| GatewayError::with_status(404, "账号不存在或不属于 CatPaw"))?;
    let Some(token2) = balance_token_of(&record) else {
        return Err(usage_not_configured(
            "CatPaw",
            "余额查询凭证（token2）",
        ));
    };
    // uid 用于 cookie 里的 `u` / `userId`（源实现：`cookie.uid || account.uid`）
    let uid = record
        .get("uid")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            record
                .get("balanceCookie")
                .and_then(|cookie| cookie.get("uid"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
        .unwrap_or("")
        .to_string();

    let mut cookie_parts: Vec<String> = PASSPORT_COOKIE_NAMES
        .iter()
        .map(|name| format!("{name}={token2}"))
        .collect();
    if !uid.is_empty() {
        cookie_parts.push(format!("u={uid}"));
        cookie_parts.push(format!("userId={uid}"));
    }
    let headers: Vec<(String, String)> = vec![
        (
            "Accept".to_string(),
            "application/json, text/plain, */*".to_string(),
        ),
        ("Cookie".to_string(), cookie_parts.join("; ")),
        // Referer 不是可选项：源实现带上它（credit 域的同源页面）
        (
            "Referer".to_string(),
            "https://credit.catpaw.meituan.com/".to_string(),
        ),
        ("User-Agent".to_string(), BROWSER_UA.to_string()),
    ];

    // 直连（源实现是裸 fetch，见模块头）
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
            GatewayError::with_status(504, "余额查询超时")
        } else {
            GatewayError::with_status(502, format!("余额查询请求失败: {error}"))
        }
    })?;

    let payload = response.payload.unwrap_or(Value::Null);
    let code = payload.get("code").and_then(Value::as_i64);
    // HTTP 401 与业务码 401 是两条路径（源实现两个都判）
    if response.status == 401 || code == Some(401) {
        return Err(GatewayError::with_status(
            401,
            "余额查询凭证已过期，请在账号设置里更新 token2",
        ));
    }
    if !response.ok {
        return Err(GatewayError::with_status(
            502,
            format!("余额查询返回 HTTP {}", response.status),
        ));
    }
    let data = payload.get("data").cloned().unwrap_or(Value::Null);
    if code != Some(0) || data.is_null() {
        let message = payload
            .get("message")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                format!(
                    "余额查询返回异常{}",
                    code.map(|value| format!(" code={value}")).unwrap_or_default()
                )
            });
        return Err(GatewayError::with_status(502, message));
    }

    // 明细按「额度状态」列出（这是 CatPaw 侧唯一能拿到的拆分口径：
    // 上游只给总额 / 可用 / 冻结 / 已过期四个数，没有钱包概念）
    let wallets: Vec<Value> = [
        ("total", "总额度", data.get("totalCredits")),
        ("available", "可用", data.get("availableCredits")),
        ("frozen", "冻结", data.get("frozenCredits")),
        ("expired", "已过期", data.get("expiredCredits")),
    ]
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
        // 美团侧的额度单位就是它自己的 credits（源实现所有文案都叫 credits）
        "unit": "credits",
        "wallets": wallets,
        // 这个接口不含订阅信息（源实现只有余额那一路），字段按契约给 null
        "subscription": Value::Null,
        "raw": Value::Object(raw),
    }))
}

/// 取账号记录里的余额凭证：优先用户填的 `balanceToken`，其次旧数据导入留下的
/// `balanceCookie.token2`（字段来源见模块头）。
///
/// 取不到时返回 None（调用方给「未配置」错误，不是失败）。
fn balance_token_of(record: &Value) -> Option<String> {
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

/// 数值字段透传（缺失/非数字给 None —— 前端把它显示成「—」而不是 0）。
///
/// 为什么不用 0 兜底：`frozenCredits` 缺失与「冻结额度是 0」是两件事，
/// 前者是上游没给，后者是明确的零；都显示成 0 会让人以为冻结额度被清空了。
fn number_or_null(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64().filter(|item| item.is_finite()),
        Value::String(text) => text.trim().parse::<f64>().ok().filter(|item| item.is_finite()),
        _ => None,
    }
}