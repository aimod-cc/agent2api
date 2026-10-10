//! OfficeAce 自助 OAuth：**不需要桌面端**，全链路在网关这一侧跑完。
//!
//! ── 七步（依据：officeace2api 的 `oauth.mjs` 与 opencode-officeace-auth 的
//! `index.mjs`，两处逐条一致）─────────────────────────────────
//! ```text
//! ① POST {cloud}/v1/claw/auth/state                     → state（无鉴权）
//! ② 本地生成 PKCE(S256) 与一对 P-256 DPoP 密钥
//! ③ 拼 authorizeUrl，让用户去浏览器登华为云
//! ④ GET  {cloud}/v1/claw/auth/code?state=<state>        ← 整件事的钥匙
//!      202 = 还没登完；404 = state 不认识/过期；200 = 拿到 authorization code
//! ⑤ POST {sts}/v1/oauth2/tokens（表单 + `DPoP:` 头）    → 临时 AK/SK/STS/project_id
//! ⑥ GET  {cloud}/v1/claw/client-permission-validate（SDK-HMAC 签名）
//!      → model_auth_info.model_app_key / model_app_secret + model_api_url_base
//! ⑦ 入库：网关 Basic 那对**不过期**，临时凭据用来续期与额度/签到
//! ```
//!
//! ── 为什么必须轮询（而不是等回调）────────────────────────────
//! 云端回调页（`/v1/claw/auth/callback`）只是终点页，**不往本机跳** ——
//! 桌面端就是靠轮询 ④ 把授权码取回来的（`officeace2api` 的模块头写得很清楚：
//! 这条从官方包的 `dist/index.js` 里挖出来）。所以本家**不需要桌面壳开回调
//! 监听**，与 ZCode 的「服务端中介轮询」同一形态。
//!
//! ── 三处与 CodeArts 那条的结构差别 ───────────────────────────
//!   · `client_id` = `pdp5_for_agentarts`，`redirect_uri` **指向云端**（不是本机
//!     loopback）—— 授权码要轮询去取，不是等回调；
//!   · 取码失败语义**要读状态码**：202 继续等、404 直接判作废并让用户重来
//!     （不读状态码的实现会把这个失败拖到 10 分钟超时才报，用户看到的是静默挂起）；
//!   · 拿到临时凭据后**还要再打一发** `client-permission-validate` 才是能用的
//!     网关凭据（CodeArts 那一步就是终点了）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use std::time::Duration;

use serde_json::Value;

use crate::server::core::egress;
use crate::server::core::providers::codearts::credentials::DpopKeyPair;
use crate::server::core::providers::codearts::dpop::DpopKey;
use crate::server::core::providers::officeace::signer::{self, Algorithm, Credential as SigningCredential};
use crate::server::logging;

/// 云端总控基址（`/v1/claw/*` 都挂在这里）。
pub const CLOUD_BASE: &str = "https://agentarts.cn-southwest-2.myhuaweicloud.com";
/// 华为云登录页基址。
pub const AUTH_BASE: &str = "https://auth.huaweicloud.com";
/// 固定的 client_id（官方实现同款）。
pub const CLIENT_ID: &str = "pdp5_for_agentarts";
/// 授权回调**指向云端**（不是本机 loopback）。
pub const REDIRECT_URI: &str = "https://agentarts.cn-southwest-2.myhuaweicloud.com/v1/claw/auth/callback";
/// 令牌端点（与 CodeArts 同一个 STS 主机）。
pub const TOKEN_URL: &str = "https://sts.cn-north-4.myhuaweicloud.com/v1/oauth2/tokens";
/// 区域（控制面签名用）。
pub const REGION: &str = "cn-southwest-2";

/// 轮询间隔（官方实现 2.5 秒）。
const POLL_INTERVAL_MS: u64 = 2500;
/// 一轮登录的本地超时（官方 10 分钟）。
pub const LOGIN_TTL_MS: u64 = 10 * 60 * 1000;
/// 单次请求超时。
const REQUEST_TIMEOUT_MS: u64 = 30_000;

/// 一次登录会话：state + PKCE verifier + DPoP 密钥对。
pub struct LoginFlow {
    state: String,
    auth_url: String,
    code_verifier: String,
    dpop_key_pair: DpopKeyPair,
}

/// 登录成功后的凭据（⑦ 的产物）：两层都在。
#[derive(Clone, Debug)]
pub struct GatewayCredential {
    pub base_url: String,
    pub model_app_key: String,
    pub model_app_secret: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub security_token: String,
    pub project_id: String,
    pub expires_at: i64,
    /// 上游给的账号显示名（`id_token` 的 `preferred_username`/`name`；取不到为空串）。
    /// 拿它落账号名 —— 否则面板只能显示代码里的种子名「OfficeAce 果办」，
    /// 看起来就是「只有提供商名」（与别家显示昵称/邮箱不一致）。
    pub user_name: String,
}

fn base64url(bytes: &[u8]) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    URL_SAFE_NO_PAD.encode(bytes)
}

fn random_bytes(count: usize) -> Vec<u8> {
    let mut buffer = vec![0u8; count];
    if getrandom::getrandom(&mut buffer).is_err() {
        // 取不到随机源时退化为时间戳派生（state/verifier 仍在本机唯一）
        let now = logging::now_ms();
        for (index, slot) in buffer.iter_mut().enumerate() {
            *slot = ((now >> (index % 8 * 8)) & 0xff) as u8;
        }
    }
    buffer
}

/// `encodeURIComponent` 口径的百分号编码（PKCE challenge 与 URL 参数都用它）。
pub fn url_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(*byte, b'-' | b'_' | b'.' | b'~') {
            out.push(*byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// 第一步：向云端要一个 state（**无鉴权**）。
async fn fetch_state() -> Result<String, String> {
    let url = format!("{CLOUD_BASE}/v1/claw/auth/state");
    let response = egress::client_for(None)
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .timeout(Duration::from_millis(REQUEST_TIMEOUT_MS))
        .body("{}")
        .send()
        .await
        .map_err(|error| format!("向云端申请 state 失败：{}", egress::describe_error_detail(&error)))?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    let state = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|payload| payload.get("state").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_default();
    if state.trim().is_empty() {
        return Err(format!("云端没给出 state（HTTP {status}）"));
    }
    Ok(state.trim().to_string())
}

/// 第三步：拼授权地址（官方写法：外层 `login.html?service=<内层 authorize>`）。
fn build_authorize_url(state: &str, challenge: &str) -> String {
    let inner = format!(
        "{AUTH_BASE}/authui/v1/oauth2/authorize?client_id={CLIENT_ID}\
         &code_challenge={}&code_challenge_method=SHA-256&state={}\
         &scope=openid&redirect_uri={}&response_type=code",
        url_escape(challenge),
        url_escape(state),
        url_escape(REDIRECT_URI),
    );
    format!("{AUTH_BASE}/authui/login.html?service={}", url_escape(&inner))
}

impl LoginFlow {
    /// 发起一轮登录（①②③）。
    pub async fn start() -> Result<Self, String> {
        let state = fetch_state().await?;
        // PKCE：verifier 是 43–128 位随机串；challenge = base64url(sha256(verifier))
        let code_verifier = base64url(&random_bytes(32));
        let digest = {
            use sha2::{Digest, Sha256};
            Sha256::digest(code_verifier.as_bytes()).to_vec()
        };
        let challenge = base64url(&digest);
        let dpop_key_pair = DpopKey::generate().map_err(|error| format!("生成 DPoP 密钥失败：{error}"))?;
        let auth_url = build_authorize_url(&state, &challenge);
        Ok(Self {
            state,
            auth_url,
            code_verifier,
            dpop_key_pair,
        })
    }

    pub fn state(&self) -> &str {
        &self.state
    }

    pub fn auth_url(&self) -> &str {
        &self.auth_url
    }

    pub fn poll_interval(&self) -> Duration {
        Duration::from_millis(POLL_INTERVAL_MS)
    }

    /// 单次取码。`Ok(None)` = 还没登完（202）；`Err` = 作废（404）或其它失败。
    pub async fn poll_code(&self) -> Result<Option<String>, String> {
        let url = format!(
            "{CLOUD_BASE}/v1/claw/auth/code?state={}",
            url_escape(&self.state)
        );
        let response = egress::client_for(None)
            .get(&url)
            .header("Accept", "application/json")
            .timeout(Duration::from_millis(REQUEST_TIMEOUT_MS))
            .send()
            .await
            .map_err(|error| format!("轮询授权码失败：{}", egress::describe_error_detail(&error)))?;
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();
        match status {
            // 用户还没在浏览器里点完 —— 正常，继续等
            202 => Ok(None),
            // state 不认识或过期：**立即判作废**，别拖到超时
            404 => Err("授权码已失效或 state 不认识，请重新发起登录".to_string()),
            code_status if !(200..300).contains(&code_status) => {
                Err(format!("取授权码返回 HTTP {code_status}"))
            }
            _ => {
                let payload = serde_json::from_str::<Value>(&text).unwrap_or(Value::Null);
                if let Some(error) = payload.get("error").and_then(Value::as_str) {
                    let message = payload
                        .get("error_msg")
                        .and_then(Value::as_str)
                        .unwrap_or(error);
                    return Err(format!("云端拒绝：{message}"));
                }
                let code = payload
                    .get("code")
                    .and_then(Value::as_str)
                    .or_else(|| payload.get("authorization_code").and_then(Value::as_str))
                    .map(str::trim)
                    .unwrap_or("")
                    .to_string();
                if code.is_empty() {
                    return Err("云端说好了，但没给授权码".to_string());
                }
                Ok(Some(code))
            }
        }
    }

    /// ⑤⑥⑦：拿授权码换临时凭据，再用它换网关 Basic 那对。
    pub async fn finish(&self, code: &str) -> Result<GatewayCredential, String> {
        let (temporary, expires_at, user_name) = self.exchange_code(code).await?;
        let gateway = self.fetch_gateway_credential(&temporary).await?;
        Ok(GatewayCredential {
            base_url: gateway.0,
            model_app_key: gateway.1,
            model_app_secret: gateway.2,
            access_key_id: temporary.access_key_id,
            secret_access_key: temporary.secret_access_key,
            security_token: temporary.security_token,
            project_id: temporary.project_id,
            expires_at,
            user_name,
        })
    }

    /// ⑤ 授权码 → 临时 AK/SK（表单 + `DPoP:` 头，与 CodeArts 同一套；
    /// `send_raw` 只收 JSON 体，所以这里直接用 egress 客户端发表单）。
    ///
    /// 返回 `(临时凭据, 到期时刻毫秒)`。
    async fn exchange_code(&self, code: &str) -> Result<(SigningCredential, i64, String), String> {
        let key = DpopKey::from_key_pair(&self.dpop_key_pair)
            .map_err(|error| format!("DPoP 私钥不可用：{error}"))?;
        let proof = key
            .proof("POST", TOKEN_URL, logging::now_ms())
            .map_err(|error| format!("生成 DPoP proof 失败：{error}"))?;
        let form = [
            ("client_id", CLIENT_ID),
            ("code", code.trim()),
            ("code_verifier", self.code_verifier.as_str()),
            ("grant_type", "authorization_code"),
            ("redirect_uri", REDIRECT_URI),
        ]
        .iter()
        .map(|(name, value)| format!("{}={}", url_escape(name), url_escape(value)))
        .collect::<Vec<_>>()
        .join("&");
        let response = egress::client_for(None)
            .post(TOKEN_URL)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .header("DPoP", proof)
            .timeout(Duration::from_millis(REQUEST_TIMEOUT_MS))
            .body(form)
            .send()
            .await
            .map_err(|error| {
                format!("换取临时凭据失败：{}", egress::describe_error_detail(&error))
            })?;
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();
        if !(200..300).contains(&status) {
            return Err(format!("令牌端点被拒（HTTP {status}）"));
        }
        let payload = serde_json::from_str::<Value>(&text).unwrap_or(Value::Null);
        let credentials = payload.get("credentials").unwrap_or(&Value::Null);
        let access_key_id = credentials
            .get("access_key_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let secret_access_key = credentials
            .get("secret_access_key")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if access_key_id.is_empty() || secret_access_key.is_empty() {
            return Err(format!("令牌端点没返回临时凭据（HTTP {status}）"));
        }
        // 到期时刻：优先上游给的 `expiration`，缺省按「现在 + 2 小时」估算
        // （实测值，见模块头 ⑦）
        let expires_at = credentials
            .get("expiration")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339_ms)
            .filter(|value| *value > 0)
            .unwrap_or_else(|| logging::now_ms() + 2 * 3600 * 1000);
        // 用户的显示名从 `id_token`（JWT）的 claims 取（参考实现同源：
        // `preferred_username || name`）；取不到给空串，落账号时退到种子名。
        let user_name = id_token_user_name(
            payload
                .get("id_token")
                .and_then(Value::as_str)
                .unwrap_or(""),
        );
        Ok((
            SigningCredential {
                access_key_id,
                secret_access_key,
                security_token: credentials
                    .get("security_token")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                project_id: credentials
                    .get("project_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            },
            expires_at,
            user_name,
        ))
    }

    /// ⑥ 用临时凭据问云端要网关 Basic 那对（SDK-HMAC-SHA256 签名）。
    async fn fetch_gateway_credential(
        &self,
        temporary: &SigningCredential,
    ) -> Result<(String, String, String), String> {
        let url = format!("{CLOUD_BASE}/v1/claw/client-permission-validate");
        let headers = signer::sign(&signer::SignRequest {
            algorithm: Algorithm::Sdk,
            method: "GET",
            url: &url,
            headers: &[(
                "Content-Type".to_string(),
                "application/json;charset=utf8".to_string(),
            )],
            body: b"",
            credential: temporary,
            region: REGION,
            date_override: None,
        })?;
        let mut request = egress::client_for(None)
            .get(&url)
            .timeout(Duration::from_millis(REQUEST_TIMEOUT_MS));
        for (name, value) in &headers {
            request = request.header(name.as_str(), value.as_str());
        }
        let response = request
            .send()
            .await
            .map_err(|error| format!("开通校验失败：{}", egress::describe_error_detail(&error)))?;
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();
        let payload = serde_json::from_str::<Value>(&text).unwrap_or(Value::Null);
        if !(200..300).contains(&status) {
            let detail = payload
                .get("error_msg")
                .and_then(Value::as_str)
                .or_else(|| payload.get("error_code").and_then(Value::as_str))
                .unwrap_or("");
            return Err(format!("开通校验失败（HTTP {status}）：{detail}"));
        }
        let model_info = payload
            .get("model_info")
            .or_else(|| payload.get("subscription").and_then(|value| value.get("model_info")))
            .or_else(|| payload.get("modelInfo"))
            .unwrap_or(&Value::Null);
        let auth_info = model_info.get("model_auth_info").unwrap_or(&Value::Null);
        let app_key = auth_info
            .get("model_app_key")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let app_secret = auth_info
            .get("model_app_secret")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if app_key.is_empty() {
            let pending = payload
                .get("subscription")
                .and_then(|value| value.get("status"))
                .and_then(Value::as_str)
                == Some("PENDING");
            return Err(if pending {
                "这个账号还没开通服务，需要先在客户端里兑换邀请码；开通后再重新登录一次即可".to_string()
            } else {
                "云端没返回模型网关信息，可能该账号没有可用套餐".to_string()
            });
        }
        let host = model_info
            .get("model_api_url_base")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_end_matches('/')
            .to_string();
        let base_url = if host.is_empty() {
            String::new()
        } else if host.ends_with("/v2") {
            format!("https://{host}")
        } else {
            format!("https://{host}/v2")
        };
        Ok((base_url, app_key, app_secret))
    }
}

/// 从 `id_token`（JWT）的 payload 里取用户显示名（`preferred_username` → `name`）。
///
/// 参考实现同源（`claims.preferred_username || claims.name`）。取不到（没给
/// `id_token` / 不是三段 JWT / 解码失败）给空串 —— 调用方退到种子名，不报错。
fn id_token_user_name(id_token: &str) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    let mut parts = id_token.split('.');
    parts.next(); // header（不验签：这里只取显示名，不用于任何鉴权判断）
    let Some(payload) = parts.next() else {
        return String::new();
    };
    let Ok(bytes) = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=').as_bytes()) else {
        return String::new();
    };
    let Ok(claims) = serde_json::from_slice::<Value>(&bytes) else {
        return String::new();
    };
    for field in ["preferred_username", "name"] {
        if let Some(text) = claims.get(field).and_then(Value::as_str) {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    String::new()
}

/// 把 RFC3339（`2026-10-10T05:15:00Z`）解析成毫秒时间戳。
pub(super) fn parse_rfc3339_ms(input: &str) -> Option<i64> {
    let text = input.trim().trim_end_matches('Z');
    let (date, time) = text.split_once('T')?;
    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: i64 = date_parts.next()?.parse().ok()?;
    let day: i64 = date_parts.next()?.parse().ok()?;
    let mut time_parts = time.split(':');
    let hour: i64 = time_parts.next()?.parse().ok()?;
    let minute: i64 = time_parts.next()?.parse().ok()?;
    let second: i64 = time_parts
        .next()
        .and_then(|value| value.split('.').next())
        .unwrap_or("0")
        .parse()
        .ok()?;
    days_from_civil(year, month, day)
        .map(|days| (days * 86_400 + hour * 3600 + minute * 60 + second) * 1000)
}

/// Howard Hinnant 的 `days_from_civil`（年月日 → epoch 起的天数）。
fn days_from_civil(year: i64, month: i64, day: i64) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let adjusted_year = if month <= 2 { year - 1 } else { year };
    let era = if adjusted_year >= 0 { adjusted_year } else { adjusted_year - 399 } / 400;
    let yoe = adjusted_year - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 授权地址的关键性质：**回调指向云端**（不是本机 loopback）—— 这是与
    /// CodeArts 那条最大的结构差别，写错了整条登录链在 NAS 上跑不通。
    #[test]
    fn authorize_url_has_the_cloud_redirect_not_loopback() {
        let url = build_authorize_url("st123", "ch456");
        assert!(
            url.starts_with("https://auth.huaweicloud.com/authui/login.html?service="),
            "{url}"
        );
        // 内层整体被百分号编码 ⇒ 参数以 `%3D` 形态出现
        assert!(url.contains("client_id%3Dpdp5_for_agentarts"), "{url}");
        assert!(url.contains("state%3Dst123"), "{url}");
        assert!(url.contains("code_challenge%3Dch456"), "{url}");
        // redirect_uri 指向云端
        assert!(
            url.contains("agentarts.cn-southwest-2.myhuaweicloud.com"),
            "{url}"
        );
        assert!(!url.contains("127.0.0.1"), "回调不能是本机 loopback：{url}");
    }

    #[test]
    fn rfc3339_parses_official_shape() {
        // 2026-10-10T03:15:00Z
        assert_eq!(Some(1_791_602_100_000), parse_rfc3339_ms("2026-10-10T03:15:00Z"));
        assert_eq!(Some(0), parse_rfc3339_ms("1970-01-01T00:00:00Z"));
        assert_eq!(None, parse_rfc3339_ms("not a date"));
        assert_eq!(None, parse_rfc3339_ms("2026-13-01T00:00:00Z"));
    }

    #[test]
    fn escape_matches_encode_uri_component() {
        assert_eq!("a%20b", url_escape("a b"));
        assert_eq!("a%2Fb", url_escape("a/b"));
        assert_eq!("-_.~", url_escape("-_.~"));
    }

    #[test]
    fn id_token_user_name_reads_the_claims() {
        let token_of = |claims: &str| format!("header.{}.sig", base64url(claims.as_bytes()));

        assert_eq!(
            "user@example.com",
            id_token_user_name(&token_of(r#"{"preferred_username":"user@example.com","sub":"u1"}"#))
        );
        // `preferred_username` 缺 → 退到 `name`
        assert_eq!("张三", id_token_user_name(&token_of(r#"{"name":"张三"}"#)));
        // 带 `=` 填充也能解（JWT 通常不带，但别为它挂）
        let padded = format!("{}==", base64url(r#"{"name":"abc"}"#.as_bytes()));
        assert_eq!("abc", id_token_user_name(&format!("h.{padded}.s")));

        // 取不到就给空串（调用方退种子名），不 panic
        assert_eq!("", id_token_user_name(""));
        assert_eq!("", id_token_user_name("not.a.jwt"));
        assert_eq!("", id_token_user_name(&token_of(r#"{"sub":"u1"}"#)));
    }
}
