//! Accio 的请求封装与用户资料查询。
//!
//! ── 与另外几家的差别：token 不进请求头 ──────────────────────
//! 桌面端的业务接口**不走 Authorization**：GET 把 `accessToken` 拼进 query
//! （`GET /api/entitlement/quota?accessToken=…`），POST 放进 JSON body
//! （`POST /api/auth/refresh_token {accessToken, refreshToken}`）。本模块把
//! 这个约定收在两处构造里（[`get_with_token`] / [`post_json`]），别处不再各写。

use serde_json::{json, Value};

use crate::server::core::auth_http::{send_raw, ApiResponse};
use crate::server::core::proxies::{resolve_account_proxy, ProxyResolution, ResolvedProxy};
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::credentials::{self, Credentials};
use super::endpoints::{self, Region};

/// 业务接口(非转发)的请求超时：与另外几家的 15 秒同量级
pub const REQUEST_TIMEOUT_MS: u64 = 15_000;

/// 一次出网请求（错误文案带地区名，便于排障时区分两站）
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
                GatewayError::with_status(504, "Accio 请求超时，请检查网络后重试")
                    .with_code("accio_transport")
            } else {
                GatewayError::with_status(502, "无法连接 Accio，请检查网络或账号代理设置")
                    .with_code("accio_transport")
            }
        })
}

/// GET + token 进 query（业务接口的约定之一）
pub async fn get_with_token(
    region: Region,
    path: &str,
    token: &str,
    query: &[(&str, &str)],
    proxy: Option<&ResolvedProxy>,
) -> Result<ApiResponse, GatewayError> {
    let mut url = url::Url::parse(&format!("{}{}", endpoints::gateway_base(), path))
        .map_err(|_| GatewayError::with_status(500, "Accio 请求地址无效"))?;
    {
        let mut pairs = url.query_pairs_mut();
        for (key, value) in query {
            pairs.append_pair(key, value);
        }
        if !token.is_empty() {
            pairs.append_pair("accessToken", token);
        }
    }
    request("GET", url.as_str(), None, &endpoints::api_headers(region), proxy).await
}

/// POST + JSON body（业务接口的约定之二）
pub async fn post_json(
    region: Region,
    path: &str,
    body: &Value,
    proxy: Option<&ResolvedProxy>,
) -> Result<ApiResponse, GatewayError> {
    let url = format!("{}{}", endpoints::gateway_base(), path);
    request("POST", &url, Some(body), &endpoints::api_headers(region), proxy).await
}

/// 把响应翻成「有效 JSON 对象」，失败时给出人话。
pub fn payload(response: ApiResponse, action: &str) -> Result<Value, GatewayError> {
    if !response.ok {
        let hint = match response.status {
            401 | 403 => "，请确认凭证有效或重新登录",
            429 => "，请稍后重试",
            _ => "",
        };
        return Err(GatewayError::with_status(
            i32::from(response.status),
            format!("Accio {action}失败（HTTP {}）{hint}", response.status),
        ));
    }
    response
        .payload
        .filter(Value::is_object)
        .ok_or_else(|| GatewayError::with_status(502, format!("Accio {action}未返回有效 JSON 对象")))
}

/// 账号级出口代理（解析失败按 400 报，不静默直连）
pub fn account_proxy(record: &Value) -> Result<Option<ResolvedProxy>, GatewayError> {
    match resolve_account_proxy(record.get("proxy")) {
        Some(ProxyResolution::Resolved(proxy)) => Ok(Some(proxy)),
        Some(ProxyResolution::Failed(reason)) => Err(GatewayError::with_status(400, reason)),
        None => Ok(None),
    }
}

/// 查用户资料（`GET /api/auth/userinfo?accessToken=…`）。
///
/// 上游把资料放在 `data` 里还是顶层，两种形态都见过（桌面端读的是
/// `data.user` / 顶层混合），这里都接受。
pub async fn fetch_profile(
    token: &str,
    region: Region,
    proxy: Option<&ResolvedProxy>,
) -> Result<Value, GatewayError> {
    let response = get_with_token(region, endpoints::USER_INFO_PATH, token, &[], proxy).await?;
    let payload = payload(response, "用户信息查询")?;
    Ok(unwrap_data(payload))
}

/// 上游响应常见两层信封（`{success, data:{…}}`）；取内层
pub fn unwrap_data(payload: Value) -> Value {
    match payload.get("data") {
        Some(Value::Object(_)) => payload.get("data").cloned().unwrap_or(payload),
        _ => payload,
    }
}

/// 把资料里的身份/名字搬进凭证。
pub fn apply_profile(credentials: &mut Credentials, profile: &Value) {
    let user_id = credentials::text(profile, &["id", "userId", "user_id", "uid"]);
    if !user_id.is_empty() {
        credentials.user_id = user_id;
    }
    let name = credentials::text(profile, &["nickname", "name", "displayName", "userName"]);
    if !name.is_empty() {
        credentials.name = name.chars().take(100).collect();
    }
    let email = credentials::text(profile, &["email", "emailAddress"]);
    if !email.is_empty() {
        credentials.email = email.chars().take(320).collect();
    }
    let device_id = credentials::text(profile, &["deviceId", "device_id"]);
    if !device_id.is_empty() && credentials.device_id.is_empty() {
        credentials.device_id = device_id;
    }
}

/// 「填写凭证」路径的入口：解析 payload → 补资料 → 补齐设备指纹。
///
/// ── 为什么要查一次资料（多一次往返）────────────────────────
/// token 是不透明串，账号列表上总得显示「这是谁」；查一次资料就有了 userId /
/// 昵称 / 邮箱三样。查询失败**不算失败**：拿不到资料照常建账号（id 走凭证
/// 指纹兜底、名字走备注名），只是少一行副标题 —— 与 AutoClaw 的
/// `email_or_empty` 同一取舍。
pub async fn prepare_account(payload: &Value) -> Result<Credentials, GatewayError> {
    let mut credentials = Credentials::from_payload(payload)?;
    credentials.ensure_identity();
    if credentials.user_id.is_empty() || credentials.name.is_empty() {
        match fetch_profile(&credentials.access_token, credentials.region, None).await {
            Ok(profile) => apply_profile(&mut credentials, &profile),
            Err(error) => logging::verbose(
                "[Accio]",
                &format!("用户资料查询失败（不影响添加）: {}", error.message),
            ),
        }
    }
    credentials.ensure_identity();
    Ok(credentials)
}

/// 额度查询用的「只读快照」：不触发续期（余额是展示动作，烧一次 refreshToken
/// 不划算 —— 过期就让上游回 401，由调用方的刷新重试那条既有链路处置）。
pub fn snapshot(record: &Value) -> Result<Credentials, GatewayError> {
    let mut credentials = Credentials::from_payload(record)?;
    credentials.ensure_identity();
    Ok(credentials)
}

/// 一个便于日志的账号摘要（绝不回显 token）
pub fn describe(credentials: &Credentials) -> String {
    let who = if !credentials.user_id.is_empty() {
        credentials.user_id.clone()
    } else if !credentials.email.is_empty() {
        credentials.email.clone()
    } else {
        "未知账号".to_string()
    };
    format!("{}（{}）", who, credentials.region.label())
}

/// 空 body 的占位（部分接口要求 POST 但不需要体）
pub fn empty_body() -> Value {
    json!({})
}
