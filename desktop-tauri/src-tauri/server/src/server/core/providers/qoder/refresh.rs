//! Qoder 凭证续期：同一凭证只刷新一次，保存时确认账号未被重新导入或删除。
//!
//! 续期端点按**凭证族**分流（`drt-` 设备流 / `pat|` 个人访问令牌 / 其余走旧路径），
//! 不按「有没有 PAT」分流 —— 见 `Credentials::device_refresh`。

use std::sync::OnceLock;

use serde_json::{json, Value};

use crate::server::core::account_store::{AccountStore, CredentialWrite};
use crate::server::core::providers::refresh_flight::{self, Join, Table};
use crate::server::core::proxies::ResolvedProxy;
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::auth;
use super::credentials::{self, Credentials};
use super::endpoints;

/// 上游没给到期时间时，按观测到的 access 令牌寿命兜的有效期。
const ACCESS_LIFETIME_MS: i64 = 30 * 24 * 60 * 60 * 1000;

static FLIGHTS: OnceLock<Table<Credentials>> = OnceLock::new();

pub fn snapshot(store: &AccountStore, account_id: &str) -> Result<(Value, Credentials), GatewayError> {
    let record = store.qoder_account_record(account_id)
        .ok_or_else(|| GatewayError::with_status(404, "Qoder 账号不存在，请先添加账号"))?;
    let mut credentials = Credentials::from_payload(&record)?;
    credentials.complete_identity()?;
    Ok((record, credentials))
}

pub async fn ensure_fresh(
    store: &AccountStore,
    account_id: &str,
    force: bool,
) -> Result<Credentials, GatewayError> {
    let (record, credentials) = snapshot(store, account_id)?;
    if !force && !credentials.expiring() {
        return Ok(credentials);
    }
    if !credentials.can_refresh() {
        return Err(GatewayError::with_status(400, "Qoder 账号没有刷新凭证，请重新登录或添加 PAT"));
    }
    let key = format!("{}:{}:{}:{}:{}", store.file_string(), account_id, credentials.region.id(),
        refresh_flight::fingerprint(&credentials.access_token), refresh_flight::fingerprint(&credentials.refresh_token));
    match FLIGHTS.get_or_init(Table::new).join(&key) {
        Join::Waiter(waiter) => waiter.wait().await,
        Join::Leader(leader) => {
            let result = refresh_and_save(store, &record, &credentials).await;
            leader.finish(result.clone());
            result
        }
    }
}

async fn refresh_and_save(
    store: &AccountStore,
    record: &Value,
    credentials: &Credentials,
) -> Result<Credentials, GatewayError> {
    let proxy = auth::account_proxy(record)?;
    let mut fresh = if let Some(device_token) = credentials.device_refresh() {
        refresh_device(credentials, device_token, proxy.as_ref()).await?
    } else if let Some(pat) = credentials.pat() {
        let mut fresh = auth::exchange_pat(pat, credentials.region, proxy.as_ref()).await?;
        fresh.machine_id = credentials.machine_id.clone();
        fresh.complete_identity()?;
        fresh
    } else {
        let response = auth::request(
            "POST",
            &format!("{}{}", credentials.region.center(), endpoints::REFRESH_PATH),
            Some(&json!({ "refreshToken": credentials.oauth_refresh() })),
            &endpoints::open_api_headers(Some(&credentials.access_token)),
            proxy.as_ref(),
        ).await?;
        let data = auth::payload(response, "凭证续期")?;
        let token = credentials::secret(&data, &["token"])?;
        if token.is_empty() {
            return Err(GatewayError::with_status(502, "Qoder 续期响应缺少 token，旧凭证未被覆盖"));
        }
        let refresh_token = credentials::secret(&data, &["refresh_token"])?;
        if refresh_token.contains('|') {
            return Err(GatewayError::with_status(502, "Qoder 续期响应的 refresh_token 格式无效"));
        }
        let mut fresh = credentials.clone();
        fresh.access_token = token;
        fresh.refresh_token = format!("{}|{}|{}",
            if refresh_token.is_empty() { credentials.oauth_refresh() } else { &refresh_token },
            credentials.user_id, credentials.machine_id);
        fresh.expires_at = Some(access_expiry(&data));
        fresh
    };
    fresh.complete_identity()?;
    if fresh.user_id != credentials.user_id || fresh.region != credentials.region {
        return Err(GatewayError::with_status(400, "Qoder 续期返回了不同账号，旧凭证未被覆盖"));
    }
    match store.update_qoder_credentials_if_current(record, &fresh)
        .map_err(|error| GatewayError::with_status(error.status_code, error.message))?
    {
        CredentialWrite::Written => Ok(fresh),
        CredentialWrite::Stale => {
            let id = record.get("id").and_then(Value::as_str).unwrap_or("");
            snapshot(store, id).map(|(_, credentials)| credentials)
        }
    }
}

/// 设备流（`drt-`）续期：openapi 主机的 `deviceToken/refresh`。
///
/// 这条路与 `center()` 那条不是一台主机、也不是一套字段名：请求体是 snake_case
/// 的 `refresh_token`（发 `refreshToken` 会被 400 `DeviceRefreshTokenRequired`
/// 拒掉），且**不带 `Authorization`** —— 访问令牌这时候通常已经到期，鉴权由
/// `drt-` 本身承担，与登录时的 `deviceToken/poll` 同一形状。
async fn refresh_device(
    credentials: &Credentials,
    device_token: &str,
    proxy: Option<&ResolvedProxy>,
) -> Result<Credentials, GatewayError> {
    let response = auth::request(
        "POST",
        &format!("{}{}", credentials.region.open_api(), endpoints::DEVICE_REFRESH_PATH),
        Some(&json!({ "refresh_token": device_token })),
        &endpoints::open_api_headers(None),
        proxy,
    ).await?;
    let data = auth::payload(response, "凭证续期")?;
    // 访问令牌落在哪个键：登录轮询回 `token`，续期回 `device_token`。
    // 顺序按官方客户端来（它先读 `device_token`，`token` 兜底），两个都接。
    let token = credentials::secret(&data, &["device_token", "token"])?;
    if token.is_empty() {
        return Err(GatewayError::with_status(502, "Qoder 续期响应缺少 token，旧凭证未被覆盖"));
    }
    // `drt-` 是轮换型的：不给新串就说明这次续期不完整，留着旧串只会在下一次
    // 撞上「已使用」。所以这里报错、不覆盖旧凭证（另一族可以沿用，见上）。
    let refresh_token = credentials::secret(&data, &["refresh_token"])?;
    if refresh_token.is_empty() {
        return Err(GatewayError::with_status(502, "Qoder 续期响应缺少 refresh_token，旧凭证未被覆盖"));
    }
    if refresh_token.contains('|') {
        return Err(GatewayError::with_status(502, "Qoder 续期响应的 refresh_token 格式无效"));
    }
    let mut fresh = credentials.clone();
    fresh.access_token = token;
    fresh.refresh_token = format!("{}|{}|{}", refresh_token, credentials.user_id, credentials.machine_id);
    fresh.expires_at = Some(access_expiry(&data));
    Ok(fresh)
}

/// 续期响应里的 access 令牌到期时刻；两个字段形状不同，都要接住。
///
/// - `expires_at`：设备流**续期**只回这个，值是 RFC3339（[`credentials::timestamp`]
///   认，epoch 秒 / 毫秒也认）
/// - `expires_in`：同族端点里的「时长」形态（登录轮询 `deviceToken/poll` 只回这个），
///   单位是**毫秒时长**不是时刻 —— 不能顺手交给 `timestamp()`：它的
///   「小于 100_000_000_000 就当秒」分支会把 30 天（2_592_000_000 ms）乘一千
///   当成时刻，算出 2052 年，于是这个账号永远不会再续期。所以这里自己按时长加。
/// - 两个都没有：按观测到的 `dt-` 寿命兜 30 天
fn access_expiry(data: &Value) -> i64 {
    if let Some(expiry) = credentials::timestamp(data.get("expires_at")) {
        return expiry;
    }
    if let Some(millis) = data.get("expires_in").and_then(Value::as_i64).filter(|value| *value > 0) {
        return logging::now_ms() + millis;
    }
    logging::now_ms() + ACCESS_LIFETIME_MS
}
