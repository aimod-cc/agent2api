//! Qoder 凭证续期：同一凭证只刷新一次，保存时确认账号未被重新导入或删除。

use std::sync::OnceLock;

use serde_json::{json, Value};

use crate::server::core::account_store::{AccountStore, CredentialWrite};
use crate::server::core::providers::refresh_flight::{self, Join, Table};
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::auth;
use super::credentials::{self, Credentials};
use super::endpoints;

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
    let mut fresh = if let Some(pat) = credentials.pat() {
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
        fresh.expires_at = Some(credentials::timestamp(data.get("expires_at"))
            .unwrap_or_else(|| logging::now_ms() + 30 * 24 * 60 * 60 * 1000));
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
