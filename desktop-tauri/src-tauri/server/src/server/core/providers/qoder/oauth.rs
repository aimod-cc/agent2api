//! Qoder 的 PKCE 设备授权（国际版与中国版同构，只有站点主机不同）。
//! verifier 只留在后端，不能进入前端状态或日志。

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::server::errors::GatewayError;
use crate::server::logging;

use super::credentials::{self, Credentials};
use super::{auth, endpoints, machine};

pub struct DeviceLogin {
    pub state: String,
    pub auth_url: String,
    poll_url: String,
    machine_id: String,
    region: endpoints::Region,
}

impl DeviceLogin {
    pub fn new(region: endpoints::Region) -> Result<Self, GatewayError> {
        let verifier = machine::random_secret()?;
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let nonce = machine::random_uuid()?.replace('-', "");
        let machine_id = machine::machine_id()?;
        let mut auth_url = url::Url::parse(&region.device_login_url())
            .map_err(|_| GatewayError::with_status(500, "Qoder 授权地址配置无效"))?;
        auth_url.query_pairs_mut()
            .append_pair("challenge", &challenge)
            .append_pair("challenge_method", "S256")
            .append_pair("machine_id", &machine_id)
            .append_pair("nonce", &nonce);
        let mut poll_url = url::Url::parse(&format!("{}{}", region.open_api(), endpoints::DEVICE_POLL_PATH))
            .map_err(|_| GatewayError::with_status(500, "Qoder 轮询地址配置无效"))?;
        poll_url.query_pairs_mut()
            .append_pair("nonce", &nonce)
            .append_pair("verifier", &verifier)
            .append_pair("challenge_method", "S256");
        Ok(Self {
            state: machine::random_secret()?,
            auth_url: auth_url.to_string(),
            poll_url: poll_url.to_string(),
            machine_id,
            region,
        })
    }

    pub async fn poll(&self) -> Result<Option<Credentials>, GatewayError> {
        let response = auth::request("GET", &self.poll_url, None, &endpoints::open_api_headers(None), None).await?;
        if response.status == 202 || response.status == 404 {
            return Ok(None);
        }
        let data = auth::payload(response, "设备授权")?;
        let token = credentials::secret(&data, &["token"])?;
        if token.is_empty() {
            return Err(GatewayError::with_status(502, "Qoder 设备授权响应缺少 token"));
        }
        let refresh_token = credentials::secret(&data, &["refresh_token"])?;
        if refresh_token.contains('|') {
            return Err(GatewayError::with_status(502, "Qoder 设备授权响应的 refresh_token 格式无效"));
        }
        let user_id = credentials::text(&data, &["user_id"]);
        let mut credentials = Credentials::from_payload(&json!({
            "mode": self.region.id(),
            "accessToken": token,
            "refreshToken": if refresh_token.is_empty() { String::new() }
                else { format!("{}|{}|{}", refresh_token, user_id, self.machine_id) },
            "userId": user_id,
            "machineId": self.machine_id,
            "expiresAt": credentials::timestamp(data.get("expires_at"))
                .unwrap_or_else(|| logging::now_ms() + 30 * 24 * 60 * 60 * 1000),
        }))?;
        match auth::fetch_profile(&credentials.access_token, credentials.region, None).await {
            Ok(profile) => auth::apply_profile(&mut credentials, &profile)?,
            Err(error) if credentials.user_id.is_empty() => return Err(error),
            Err(_) => {}
        }
        credentials.complete_identity()?;
        Ok(Some(credentials))
    }
}
