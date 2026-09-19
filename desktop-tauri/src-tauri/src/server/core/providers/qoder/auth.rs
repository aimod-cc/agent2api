//! Qoder PAT 交换、用户资料与设备令牌请求。错误信息不回显凭证或轮询 URL。

use serde_json::{json, Value};

use crate::server::core::auth_http::{send_raw, ApiResponse};
use crate::server::core::proxies::{resolve_account_proxy, ProxyResolution, ResolvedProxy};
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::credentials::{self, Credentials};
use super::endpoints::{self, Region};

const REQUEST_TIMEOUT_MS: u64 = 15_000;

pub async fn request(
    method: &str,
    url: &str,
    body: Option<&Value>,
    headers: &[(String, String)],
    proxy: Option<&ResolvedProxy>,
) -> Result<ApiResponse, GatewayError> {
    send_raw(method, url, body, headers, proxy, Some(REQUEST_TIMEOUT_MS))
        .await
        .map_err(|error| {
            if error.is_timeout() {
                GatewayError::with_status(504, "Qoder 请求超时，请检查网络后重试").with_code("qoder_transport")
            } else {
                GatewayError::with_status(502, "无法连接 Qoder，请检查网络或账号代理设置").with_code("qoder_transport")
            }
        })
}

pub fn payload(response: ApiResponse, action: &str) -> Result<Value, GatewayError> {
    if !response.ok {
        let hint = match response.status {
            401 | 403 => "，请确认凭证有效且地区选择正确",
            429 => "，请稍后重试",
            _ => "",
        };
        return Err(GatewayError::with_status(
            i32::from(response.status),
            format!("Qoder {action}失败（HTTP {}）{hint}", response.status),
        ));
    }
    response.payload.filter(Value::is_object)
        .ok_or_else(|| GatewayError::with_status(502, format!("Qoder {action}未返回有效 JSON 对象")))
}

pub fn account_proxy(record: &Value) -> Result<Option<ResolvedProxy>, GatewayError> {
    match resolve_account_proxy(record.get("proxy")) {
        Some(ProxyResolution::Resolved(proxy)) => Ok(Some(proxy)),
        Some(ProxyResolution::Failed(reason)) => Err(GatewayError::with_status(400, reason)),
        None => Ok(None),
    }
}

pub async fn fetch_profile(
    token: &str,
    region: Region,
    proxy: Option<&ResolvedProxy>,
) -> Result<Value, GatewayError> {
    let response = request(
        "GET",
        &format!("{}{}", region.open_api(), endpoints::USER_INFO_PATH),
        None,
        &endpoints::open_api_headers(Some(token)),
        proxy,
    ).await?;
    payload(response, "用户信息查询")
}

pub fn apply_profile(credentials: &mut Credentials, profile: &Value) -> Result<(), GatewayError> {
    let user_id = credentials::text(profile, &["id", "userId", "user_id"]);
    if !user_id.is_empty() {
        if !credentials.user_id.is_empty() && credentials.user_id != user_id {
            return Err(GatewayError::with_status(400, "Qoder 凭证中的用户与账号资料不一致"));
        }
        credentials.user_id = user_id;
    }
    let name = credentials::text(profile, &["name", "username"]);
    if !name.is_empty() {
        credentials.name = name.chars().take(100).collect();
    }
    let email = credentials::text(profile, &["email"]);
    if !email.is_empty() {
        credentials.email = email.chars().take(320).collect();
    }
    Ok(())
}

pub async fn exchange_pat(
    pat: &str,
    region: Region,
    proxy: Option<&ResolvedProxy>,
) -> Result<Credentials, GatewayError> {
    let pat = credentials::secret(&json!({ "pat": pat }), &["pat"])?;
    if pat.is_empty() || pat.contains('|') {
        return Err(GatewayError::with_status(400, "请填写有效的 Qoder 个人访问令牌（PAT）"));
    }
    let response = request(
        "POST",
        &format!("{}{}", region.open_api(), endpoints::EXCHANGE_PATH),
        Some(&json!({ "personal_token": pat })),
        &endpoints::open_api_headers(None),
        proxy,
    ).await?;
    let data = payload(response, "PAT 换取令牌")?;
    let token = credentials::secret(&data, &["token"])?;
    let job_refresh = credentials::secret(&data, &["refresh_token"])?;
    if token.is_empty() || job_refresh.contains('|') {
        return Err(GatewayError::with_status(502, "Qoder PAT 响应缺少有效 token 或 refresh_token 格式无效"));
    }
    let profile = fetch_profile(&token, region, proxy).await?;
    let user_id = credentials::text(&profile, &["id"]);
    let machine_id = super::machine::machine_id()?;
    let mut credentials = Credentials::from_payload(&json!({
        "mode": region.id(),
        "accessToken": token,
        "refreshToken": format!("pat|{pat}|{job_refresh}|{user_id}|{machine_id}"),
        "expiresAt": credentials::timestamp(data.get("expires_at"))
            .unwrap_or_else(|| logging::now_ms() + 24 * 60 * 60 * 1000),
        "userId": user_id,
        "machineId": machine_id,
    }))?;
    apply_profile(&mut credentials, &profile)?;
    credentials.complete_identity()?;
    Ok(credentials)
}

pub async fn prepare_account(payload: &Value) -> Result<Credentials, GatewayError> {
    if payload.get("importDesktop").and_then(Value::as_bool) == Some(true) {
        return Err(GatewayError::with_status(400, "Qoder 请使用网页登录或个人访问令牌（PAT）添加"));
    }
    let region = Region::from_payload(payload)?;
    let pat = credentials::secret(payload, &["pat", "personalAccessToken", "personal_token"])?;
    if !pat.is_empty() {
        return exchange_pat(&pat, region, None).await;
    }
    let mut credentials = Credentials::from_payload(payload)?;
    if credentials.user_id.is_empty() {
        let profile = fetch_profile(&credentials.access_token, region, None).await?;
        apply_profile(&mut credentials, &profile)?;
    }
    credentials.complete_identity()?;
    Ok(credentials)
}
