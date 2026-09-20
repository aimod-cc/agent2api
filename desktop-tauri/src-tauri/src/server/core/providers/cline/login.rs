//! Cline **设备授权登录**（WorkOS 的 RFC 8628 实现）。
//!
//! ── 协议（实测核对，2026-09）─────────────────────────────────
//! ```text
//! ① POST https://api.workos.com/user_management/authorize/device
//!      Content-Type: application/x-www-form-urlencoded
//!      body: client_id=<WORKOS_CLIENT_ID>
//!    → 200 {"device_code":"64 字符","user_code":"PXQF-MWRC",
//!           "verification_uri":"https://authkit.cline.bot/device",
//!           "verification_uri_complete":"https://authkit.cline.bot/device?user_code=PXQF-MWRC",
//!           "expires_in":300,"interval":5}
//!
//! ② 用户在浏览器里打开 verification_uri_complete 并确认
//!
//! ③ POST https://api.workos.com/user_management/authenticate   （轮询，每 interval 秒）
//!      body: grant_type=urn:ietf:params:oauth:grant-type:device_code
//!            &device_code=<device_code>&client_id=<client_id>
//!    → 200 {access_token, refresh_token}            （用户已确认）
//!    → 400 {error:"authorization_pending"}          （还没确认，继续轮询）
//!    → 400 {error:"slow_down"}                      （轮太快，拉长间隔）
//!    → 400 {error:"expired_token"} / access_denied  （终止）
//!
//! ④ POST {apiBase}/auth/register   {"accessToken":...,"refreshToken":...}
//!    → {"data":{accessToken, refreshToken, expiresAt, userInfo, accountId}, "success":true}
//!    ↑ **这一步不能省**：WorkOS 给的令牌是「WorkOS 身份令牌」，
//!      还要经 Cline 自己的 `/auth/register` 换成**它自己的会话令牌**
//!      （带 `workos:` 前缀、带 `accountId`），才是能打 LLM 端点的那个。
//! ```
//!
//! ── 为什么这条链路值得做（与另外几家的对照）───────────────────
//! 这是**标准协议**（WorkOS 是 AuthKit 的商业实现，Cline 只是它的客户），
//! 因此比 CatPaw 的「上游往本机推 token」与 Qoder 的 PKCE 自签名都稳：
//! 不需要回调端口、不需要拦截域名、不需要自定义协议 —— 用户在任何设备上
//! 打开授权页都行，网关这边只是轮询。
//!
//! ── 本地回调服务器的缺席是有意的 ─────────────────────────────
//! 源实现（`@cline/core` 的 `loginClineOAuth`）会起一个**本地回调服务器**
//! （端口 48801..48811），那是给「浏览器重定向回来」用的；但设备授权这条
//! 路径**不需要回调**（用户是手动输码 / 点链接确认的），源实现也只是把它
//! 起在那里备用（`callbackUrl` 最终没进请求）。本实现**不起服务器** ——
//! 少一个端口占用、少一处失败面，而流程完全一样。
//!
//! ── 落账号走既有入口 ────────────────────────────────────────
//! 与另外几家同一纪律：登录成功后调 `AccountStore::add_cline_account`，
//! 不另写一份落盘逻辑（否则「手动添加」与「登录」两条路会在校验、id 生成、
//! 优先级分配上慢慢分叉）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：绝不 unwrap/expect/panic；所有失败转 `GatewayError`。

use std::time::Duration;

use serde_json::{json, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::core::egress;
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::credentials::{self, WORKOS_BASE_URL, WORKOS_CLIENT_ID};

/// 单次 HTTP 超时（授权接口是交互式的，给短一些）
const REQUEST_TIMEOUT_MS: u64 = 30_000;

/// 轮询上限：`expires_in` 是 300 秒，这里给同等上限
const MAX_POLL_MS: u64 = 300_000;

/// 默认轮询间隔（上游返回 `interval`，缺失时用它）
const DEFAULT_INTERVAL_SECONDS: u64 = 5;

/// 设备授权的第一步产物（给界面展示「去哪儿输码」用）。
#[derive(Clone, Debug)]
pub struct DeviceAuthStart {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    pub expires_in_seconds: u64,
    pub interval_seconds: u64,
}

/// 发起设备授权（第一步）。
pub async fn start() -> Result<DeviceAuthStart, GatewayError> {
    let client = egress::client_for(None);
    let response = client
        .post(format!(
            "{WORKOS_BASE_URL}/user_management/authorize/device"
        ))
        .header(
            "Content-Type",
            "application/x-www-form-urlencoded",
        )
        .header("Accept", "application/json")
        .timeout(Duration::from_millis(REQUEST_TIMEOUT_MS))
        .body(format!("client_id={WORKOS_CLIENT_ID}"))
        .send()
        .await
        .map_err(|error| {
            GatewayError::with_status(
                502,
                format!(
                    "Cline 设备授权请求失败: {}",
                    egress::describe_error_detail(&error)
                ),
            )
        })?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    let payload: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if !(200..300).contains(&status) {
        let detail = payload
            .get("error_description")
            .or_else(|| payload.get("error"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let detail = if detail.is_empty() {
            crate::server::core::account_store::store_util::truncate_text(&text, 200)
        } else {
            detail.to_string()
        };
        return Err(GatewayError::with_status(
            502,
            format!("Cline 设备授权失败（{status}）: {detail}"),
        ));
    }
    let text_field = |key: &str| {
        payload
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    let Some(device_code) = text_field("device_code") else {
        return Err(GatewayError::with_status(
            502,
            "Cline 设备授权响应缺少 device_code",
        ));
    };
    let Some(user_code) = text_field("user_code") else {
        return Err(GatewayError::with_status(
            502,
            "Cline 设备授权响应缺少 user_code",
        ));
    };
    let verification_uri = text_field("verification_uri")
        .unwrap_or_else(|| credentials::DEVICE_VERIFY_FALLBACK.to_string());
    Ok(DeviceAuthStart {
        device_code,
        user_code,
        verification_uri,
        verification_uri_complete: text_field("verification_uri_complete"),
        expires_in_seconds: payload
            .get("expires_in")
            .and_then(Value::as_u64)
            .unwrap_or(300)
            .min(MAX_POLL_MS / 1000),
        interval_seconds: payload
            .get("interval")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_INTERVAL_SECONDS)
            .max(1),
    })
}

/// 轮询直到用户确认（第三步 + 第四步），成功后**落账号**并返回账号 id。
///
/// ── 这个函数会阻塞到「确认 / 超时 / 取消」三者之一 ────────────
/// 调用方（登录路由）本来就在等一个异步结果，因此这里是 await 循环而不是
/// 「前端反复来问」—— 少一套状态管理，且超时由本函数统一兜住。
pub async fn poll_and_register(
    store: &AccountStore,
    start: &DeviceAuthStart,
    provider: &str,
    name: Option<&str>,
) -> Result<String, GatewayError> {
    let workos_tokens = poll_tokens(start).await?;
    let session = register_session(&workos_tokens).await?;
    // 落账号走既有入口（与「填写凭证」同一条路径）。池由 provider 决定，
    // payload 里不再带 `pool`（那是拆分前的字段，账号层已不写它）。
    let payload = json!({
        "accessToken": session.access_token,
        "refreshToken": session.refresh_token,
    });
    let account = store
        .add_cline_account(provider, &payload, name)
        .map_err(|error| GatewayError::with_status(error.status_code, error.message))?;
    let id = account
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    logging::log(
        "[Login]",
        &format!(
            "✅ {} 登录成功，账号已加入列表",
            crate::server::core::providers::label_of(provider)
        ),
    );
    Ok(id)
}

/// WorkOS 令牌对
struct WorkosTokens {
    access_token: String,
    refresh_token: String,
}

/// 轮询 `/user_management/authenticate`（RFC 8628 的 token 端点语义）。
async fn poll_tokens(start: &DeviceAuthStart) -> Result<WorkosTokens, GatewayError> {
    let started = std::time::Instant::now();
    let mut interval = start.interval_seconds.max(1);
    // 用上游给的过期时间做上限（默认 300 秒）
    let deadline = Duration::from_secs(start.expires_in_seconds.max(1));
    loop {
        if started.elapsed() >= deadline {
            return Err(GatewayError::with_status(
                408,
                "Cline 登录超时（授权码已过期），请重新发起登录",
            ));
        }
        tokio::time::sleep(Duration::from_secs(interval)).await;
        let client = egress::client_for(None);
        let response = client
            .post(format!("{WORKOS_BASE_URL}/user_management/authenticate"))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .timeout(Duration::from_millis(REQUEST_TIMEOUT_MS))
            .body(format!(
                "grant_type=urn:ietf:params:oauth:grant-type:device_code&device_code={}&client_id={WORKOS_CLIENT_ID}",
                start.device_code
            ))
            .send()
            .await
            .map_err(|error| {
                GatewayError::with_status(
                    502,
                    format!(
                        "Cline 登录轮询失败: {}",
                        egress::describe_error_detail(&error)
                    ),
                )
            })?;
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();
        let payload: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if (200..300).contains(&status) {
            let access = payload
                .get("access_token")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty());
            let refresh = payload
                .get("refresh_token")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty());
            match (access, refresh) {
                (Some(access), Some(refresh)) => {
                    return Ok(WorkosTokens {
                        access_token: access.to_string(),
                        refresh_token: refresh.to_string(),
                    })
                }
                _ => {
                    return Err(GatewayError::with_status(
                        502,
                        "Cline 登录响应缺少令牌（access_token / refresh_token）",
                    ))
                }
            }
        }
        // 非 2xx：按 RFC 8628 的 error 码决定继续还是终止
        let error_code = payload
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        match error_code.as_str() {
            // 用户还没确认：继续等
            "authorization_pending" => continue,
            // 轮太快：拉长间隔（RFC 要求至少 +1 秒）
            "slow_down" => {
                interval = interval.saturating_add(1).min(30);
                continue;
            }
            "expired_token" => {
                return Err(GatewayError::with_status(
                    408,
                    "Cline 登录超时（授权码已过期），请重新发起登录",
                ))
            }
            "access_denied" => {
                return Err(GatewayError::with_status(
                    400,
                    "Cline 登录被拒绝（用户在授权页取消了）",
                ))
            }
            _ => {
                let detail = payload
                    .get("error_description")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| {
                        crate::server::core::account_store::store_util::truncate_text(&text, 200)
                    });
                return Err(GatewayError::with_status(
                    502,
                    format!("Cline 登录失败（{status}）: {detail}"),
                ));
            }
        }
    }
}

/// Cline 自己的会话令牌（经 `/auth/register` 换来的）
struct ClineSession {
    access_token: String,
    refresh_token: String,
}

/// 把 WorkOS 令牌换成 Cline 会话令牌（第四步，**不能省**）。
async fn register_session(tokens: &WorkosTokens) -> Result<ClineSession, GatewayError> {
    let client = egress::client_for(None);
    let response = client
        .post(format!("{}/auth/register", credentials::API_BASE_URL))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header("X-CLIENT-TYPE", super::adapter::CLIENT_TYPE)
        .timeout(Duration::from_millis(REQUEST_TIMEOUT_MS))
        .json(&json!({
            "accessToken": tokens.access_token,
            "refreshToken": tokens.refresh_token,
        }))
        .send()
        .await
        .map_err(|error| {
            GatewayError::with_status(
                502,
                format!(
                    "Cline 令牌登记失败: {}",
                    egress::describe_error_detail(&error)
                ),
            )
        })?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    let payload: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if !(200..300).contains(&status) {
        let detail = payload
            .get("error")
            .and_then(|error| {
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .or_else(|| error.as_str())
            })
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                crate::server::core::account_store::store_util::truncate_text(&text, 200)
            });
        return Err(GatewayError::with_status(
            502,
            format!("Cline 令牌登记失败（{status}）: {detail}"),
        ));
    }
    let data = payload.get("data").unwrap_or(&payload);
    let access = data
        .get("accessToken")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| {
            GatewayError::with_status(502, "Cline 令牌登记响应缺少 accessToken")
        })?;
    let refresh = data
        .get("refreshToken")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .unwrap_or(&tokens.refresh_token)
        .to_string();
    Ok(ClineSession {
        access_token: access_token_with_prefix(access),
        refresh_token: refresh,
    })
}

/// 登录返回的 accessToken 可能带也可能不带 `workos:` 前缀（实测带），统一补上
fn access_token_with_prefix(token: &str) -> String {
    // 复用凭证层的幂等实现（登录与续期两条路必须同源）
    super::credentials::ensure_token_prefix(token)
}
