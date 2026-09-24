//! Accio 凭证续期：同一凭证只刷一次，保存时确认账号未被重新导入或删除。
//!
//! 上游：`POST /api/auth/refresh_token`，body `{accessToken, refreshToken}`
//! → `{accessToken, refreshToken, expiresAt}`（桌面端同名链路，见
//! `core::login` 里那套账号切换的安全版是 `/api/auth/safe/refresh_token`，
//! 本网关不做账号切换，走普通那条）。
//!
//! ── 上游没回过期时间怎么办 ──────────────────────────────────
//! `expiresAt` 缺失时给一个保守的默认值（1 小时）：它只影响「下次要不要续期」
//! 的判定，给短了会多刷一次（无害），给长了会让一个已经失效的 token 留着不走
//! 续期路径（有害）。桌面端的 token 有效期在小时级，1 小时是安全的下界。

use std::sync::OnceLock;

use serde_json::Value;

use crate::server::core::account_store::{AccountStore, CredentialWrite};
use crate::server::core::providers::refresh_flight::{self, Join, Table};
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::auth;
use super::credentials::{self, Credentials};
use super::endpoints;

/// 上游没给到期时间时的保守默认：1 小时
const DEFAULT_TTL_MS: i64 = 60 * 60 * 1000;

static FLIGHTS: OnceLock<Table<Credentials>> = OnceLock::new();

/// 取账号记录 + 凭证（不刷新）。
///
/// `region` 必给：两个地区是两家 provider，取记录时按它过滤（见
/// `AccountStore::accio_account_record`）。
pub fn snapshot(
    store: &AccountStore,
    account_id: &str,
    region: super::endpoints::Region,
) -> Result<(Value, Credentials), GatewayError> {
    let record = store
        .accio_account_record(account_id, region.provider_id())
        .ok_or_else(|| GatewayError::with_status(404, "Accio 账号不存在，请先添加账号"))?;
    let credentials = auth::snapshot(&record)?;
    Ok((record, credentials))
}

/// 确保拿到可用凭证（`force = true` 时不看临期窗口，401 之后强制刷一次）。
pub async fn ensure_fresh(
    store: &AccountStore,
    account_id: &str,
    region: super::endpoints::Region,
    force: bool,
) -> Result<Credentials, GatewayError> {
    let (record, credentials) = snapshot(store, account_id, region)?;
    if !force && !credentials.expiring() {
        return Ok(credentials);
    }
    if !credentials.can_refresh() {
        return Err(GatewayError::with_status(
            400,
            "Accio 账号没有 refreshToken，无法续期，请重新登录或粘贴完整凭证",
        ));
    }
    let key = format!(
        "{}:{}:{}:{}:{}",
        store.file_string(),
        account_id,
        credentials.region.edition(),
        refresh_flight::fingerprint(&credentials.access_token),
        refresh_flight::fingerprint(&credentials.refresh_token),
    );
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
    let body = serde_json::json!({
        "accessToken": credentials.access_token,
        "refreshToken": credentials.refresh_token,
    });
    let response = auth::post_json(credentials.region, endpoints::REFRESH_TOKEN_PATH, &body, proxy.as_ref())
        .await?;
    let data = auth::payload(response, "凭证续期").map(auth::unwrap_data)?;
    let token = credentials::secret(&data, &["accessToken", "access_token", "token"])?;
    if token.is_empty() {
        return Err(GatewayError::with_status(502, "Accio 续期响应缺少 accessToken，旧凭证未被覆盖"));
    }
    let mut fresh = credentials.clone();
    fresh.access_token = token;
    let refresh = credentials::secret(&data, &["refreshToken", "refresh_token"])?;
    if !refresh.is_empty() {
        fresh.refresh_token = refresh;
    }
    fresh.expires_at = Some(
        credentials::timestamp(
            data.get("expiresAt").or_else(|| data.get("expires_at")),
        )
        .unwrap_or_else(|| logging::now_ms() + DEFAULT_TTL_MS),
    );
    // 身份/地区不允许被续期结果改掉：那意味着串号（换到了另一个账号的凭证）
    if !credentials.user_id.is_empty() {
        fresh.user_id = credentials.user_id.clone();
    }
    fresh.device_id = credentials.device_id.clone();
    match store
        .update_accio_credentials_if_current(record, &fresh)
        .map_err(|error| GatewayError::with_status(error.status_code, error.message))?
    {
        CredentialWrite::Written => Ok(fresh),
        CredentialWrite::Stale => {
            // 回写被拒（记录已被换掉）：用最新那份，别把旧结果当成功用出去
            let id = record.get("id").and_then(Value::as_str).unwrap_or("");
            snapshot(store, id, credentials.region).map(|(_, credentials)| credentials)
        }
    }
}
