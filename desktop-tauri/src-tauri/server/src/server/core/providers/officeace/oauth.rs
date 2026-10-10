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
    /// 上游给的账号显示名（`id_token` 或 `refresh_token` 里的
    /// `preferred_username`/`name`，都没有再解 `user_profile.account_name`；取不到为空串）。
    /// 拿它落账号名 —— 否则面板只能显示十六进制的 id（与别家显示昵称/邮箱不一致）。
    /// 实测这个上游的名字在 **`refresh_token`** 那一枚里（见 `response_identity`）。
    pub user_name: String,
    /// 上游账号 id（`client-permission-validate` 的 `account_id`）。
    /// 显示名取不到时拿它当兜底名字，好把多个账号区分开。
    pub account_id: String,
    /// IAM **用户级** id（令牌 `user_profile` 的 `principal_id`，取不到退
    /// `validate` 的 `principal_id`）。多账号兜底时它比 `account_id` 更细：
    /// 同一个华为云账号下的不同 IAM 用户共用 `account_id`，而 principal 各不同。
    pub principal_id: String,
    /// 一次性 refresh token（换新的临时 AK/SK 用；上游每轮轮换）。
    /// 落盘后 `hasRefreshToken` 为真 ⇒ 面板出现「刷新 Token」按钮，且续期链可用。
    pub refresh_token: String,
    /// 这次登录用的 DPoP 密钥对（续期签 proof 要用**同一把**私钥，丢了只能重登）。
    pub dpop_key_pair: DpopKeyPair,
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
        let (temporary, expires_at, identity, refresh_token) = self.exchange_code(code).await?;
        let gateway = self.fetch_gateway_credential(&temporary).await?;
        // principal id 两个来源：令牌 `user_profile` 优先（它就在身份本身里），
        // 没有就退 `client-permission-validate` 回的那个
        let principal_id = if identity.principal_id.is_empty() {
            gateway.4
        } else {
            identity.principal_id
        };
        Ok(GatewayCredential {
            base_url: gateway.0,
            model_app_key: gateway.1,
            model_app_secret: gateway.2,
            account_id: gateway.3,
            principal_id,
            access_key_id: temporary.access_key_id,
            secret_access_key: temporary.secret_access_key,
            security_token: temporary.security_token,
            project_id: temporary.project_id,
            expires_at,
            user_name: identity.user_name,
            refresh_token,
            // 私钥随登录一并落盘（续期要用它签 DPoP proof；丢了就只能重新登录）
            dpop_key_pair: self.dpop_key_pair.clone(),
        })
    }

    /// 用一次性 refresh_token 换一套新的临时 AK/SK（面板「刷新 Token」与自动维护走这条）。
    ///
    /// ── 与 `exchange_code` 同一套（表单 + `DPoP:` 头），只换 grant 与表单项 ──
    /// 上游令牌端点两个 grant 共用；`refresh_token` **一次性轮换**，所以返回值里那份
    /// 新的必须落盘（旧的重放会得到 `STS5.1806 the refresh token has been used`）。
    ///
    /// `dpop_key_pair` 必须是**当初登录那把**（proof 的 `jwk` 头要与授权时一致）。
    /// 返回 `(新临时凭据, 到期毫秒, id_token 里的显示名, 轮换后的 refresh_token)` ——
    /// 显示名与 `exchange_code` 同一位置、同一口径（参考实现每次续期都重取它）。
    pub async fn refresh(
        dpop_key_pair: &DpopKeyPair,
        refresh_token: &str,
    ) -> Result<(SigningCredential, i64, String, String), String> {
        if refresh_token.trim().is_empty() {
            return Err("OfficeAce 账号没有 refresh token，只能重新登录授权".to_string());
        }
        let key = DpopKey::from_key_pair(dpop_key_pair)
            .map_err(|error| format!("DPoP 私钥不可用：{error}"))?;
        let proof = key
            .proof("POST", TOKEN_URL, logging::now_ms())
            .map_err(|error| format!("生成 DPoP proof 失败：{error}"))?;
        let form = [
            ("client_id", CLIENT_ID),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.trim()),
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
                format!("续期请求失败：{}", egress::describe_error_detail(&error))
            })?;
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();
        let payload = serde_json::from_str::<Value>(&text).unwrap_or(Value::Null);
        if !(200..300).contains(&status) {
            // 错误体里可能回显发过去的东西（表单里躺着 refresh_token）—— 只取 code/message
            let detail = payload
                .get("error_description")
                .and_then(Value::as_str)
                .or_else(|| payload.get("error_msg").and_then(Value::as_str))
                .or_else(|| payload.get("error").and_then(Value::as_str))
                .unwrap_or("");
            return Err(format!("续期被拒（HTTP {status}）：{detail}"));
        }
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
            return Err("续期响应没有新的 AK/SK".to_string());
        }
        // 显示名也从这一轮的重发里取（参考实现同源：`refresh.mjs` 每次续期都解一遍
        // claims，`accounts.mjs` 的 label 优先级是「用户标签 → 上游显示名 → id」）。
        // 名字要在续期时自愈 —— 登录那一次如果上游没给显示名，账号名就永远停在
        // 那串十六进制上；每轮再问一次才追得回来（`response_identity` 的两枚 token
        // 都看，实测名字就在这一发的 `refresh_token` 里）。
        let user_name = response_identity(&payload).user_name;
        let expires_at = credentials
            .get("expiration")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339_ms)
            .filter(|value| *value > 0)
            .unwrap_or_else(|| logging::now_ms() + 2 * 3600 * 1000);
        // 轮换后的 token：没给就沿用旧的（一次性的，沿用也只是再赌一次）
        let next_refresh = payload
            .get("refresh_token")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(refresh_token)
            .to_string();
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
            next_refresh,
        ))
    }

    /// ⑤ 授权码 → 临时 AK/SK（表单 + `DPoP:` 头，与 CodeArts 同一套；
    /// `send_raw` 只收 JSON 体，所以这里直接用 egress 客户端发表单）。
    ///
    /// 返回 `(临时凭据, 到期时刻毫秒, id_token 里的身份信息, refresh token)`。
    async fn exchange_code(
        &self,
        code: &str,
    ) -> Result<(SigningCredential, i64, TokenIdentity, String), String> {
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
        // 身份信息：这一发里 `id_token` 与 `refresh_token` 都要看（实测名字就在
        // `refresh_token` 的 `user_profile` 里，见 `response_identity`）；取不到给空串，
        // 落账号时退下一级兜底。
        let identity = response_identity(&payload);
        // 一次性 refresh token：落盘后 `hasRefreshToken` 为真（面板出现「刷新 Token」），
        // 也是续期链的唯一钥匙 —— 丢了就只能重新登录。
        let refresh_token = payload
            .get("refresh_token")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
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
            identity,
            refresh_token,
        ))
    }

    /// ⑥ 用临时凭据问云端要网关 Basic 那对（SDK-HMAC-SHA256 签名）。
    ///
    /// 返回 `(网关基址, app key, app secret, account_id, principal_id)` ——
    /// 后两项只用来给账号名兜底。
    async fn fetch_gateway_credential(
        &self,
        temporary: &SigningCredential,
    ) -> Result<(String, String, String, String, String), String> {
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
        let account_id = payload
            .get("account_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        // 用户级 id（参考实现同源读 `principal_id`）：名字兜底时比 account_id 更细，
        // 同一华为云账号下的不同 IAM 用户共用 account_id，principal 各不同
        let principal_id = payload
            .get("principal_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        Ok((base_url, app_key, app_secret, account_id, principal_id))
    }
}

/// 从令牌端点的**响应体**里取身份信息：一发里有两枚 JWT，名字可能装在任意一枚里。
///
/// ── 为什么要看 `refresh_token` ──────────────────────────────
/// 实测（2026-10-11，解本机 dev 库里两枚存量 refresh token 的 claims，零网络）：
/// 这个上游把身份放在 **`refresh_token`** 的 `user_profile` 里 ——
/// `account_name`（IAM 用户名，面板要显示的那个）、`principal_id`、`account_id` 都在。
/// 只读 `id_token` 的话，这两枚 token 的响应里能不能解出名字**从未被观测过**，
/// 结果就是账号名永远退到十六进制兜底（用户报了三轮的那现象）。
///
/// 顺序：`id_token` 优先（它是这次会话的身份快照，上游哪天补上就立刻切回），
/// 它没有名字再退 `refresh_token`。principal_id 同口径，但**不因名字空而被丢弃** ——
/// 只要有一枚给了 principal 就留着，调用方还来得及退 `validate` 那一格。
fn response_identity(payload: &Value) -> TokenIdentity {
    let mut principal_only = TokenIdentity::default();
    for field in ["id_token", "refresh_token"] {
        let identity = jwt_identity(payload.get(field).and_then(Value::as_str).unwrap_or(""));
        if !identity.user_name.is_empty() {
            return identity;
        }
        if principal_only.principal_id.is_empty() && !identity.principal_id.is_empty() {
            principal_only = identity;
        }
    }
    principal_only
}

/// `id_token` / `refresh_token` 的 claims 解出来的身份信息。两项都**可能为空** ——
/// 给不给取决于这个租户的 IAM 形态，取不到不是错误（调用方自己退下一级兜底）。
#[derive(Clone, Default)]
pub struct TokenIdentity {
    pub user_name: String,
    pub principal_id: String,
}

/// 从一枚 JWT（`id_token` 或 `refresh_token`）的 payload 里取显示名与用户级 principal id。
///
/// ── 为什么要比参考实现多解一层 ──────────────────────────────
/// 参考实现只读顶层 `preferred_username || name`（`refresh.mjs` 的 `idTokenClaims`）。
/// **实测这个租户的令牌顶层没有这两个键** —— claims 只有
/// `client_id / cnf / exp / federation / iat / iss / jti / type / user_profile`，
/// 身份装在一个 base64 JSON 串里：
///
/// ```text
/// user_profile → { account_id, account_name, principal_id, principal_urn, ... }
/// ```
///
/// 照参考实现只读顶层的结果就是面板上看到的那串十六进制：显示名永远取不到，
/// 一路退到 `account_id`。所以这里的取法是**顶层优先、内层兜底**：
/// 顶层有就守既有口径（别家租户形态换了也不用改），没有才解 `user_profile`。
///
/// 全程 fail-open：不是三段 JWT / 解不出 base64 / 内层不是 JSON / 字段不是字符串，
/// 都只当没取到，回空串继续退下一级 —— 名字这条链**没有任何失败面**，
/// 拿它报错会把一次已经成功的登录记成失败。
fn jwt_identity(token: &str) -> TokenIdentity {
    let Some(claims) = jwt_claims(token) else {
        return TokenIdentity::default();
    };
    let profile = claims
        .get("user_profile")
        .and_then(Value::as_str)
        .and_then(decode_user_profile);
    let principal_id = profile_text(&profile, "principal_id")
        .or_else(|| text_of(&claims, "principal_id"))
        .unwrap_or_default();
    // 顶层字段优先：上游哪天把名字放回顶层，这里不需要改代码
    for field in ["preferred_username", "name"] {
        if let Some(text) = text_of(&claims, field) {
            return TokenIdentity { user_name: text, principal_id };
        }
    }
    TokenIdentity {
        user_name: profile_text(&profile, "account_name").unwrap_or_default(),
        principal_id,
    }
}

/// 解 JWT 的 payload 段。**不验签** —— 这里只取显示名，不用于任何鉴权判断。
fn jwt_claims(id_token: &str) -> Option<Value> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    let mut parts = id_token.split('.');
    parts.next()?; // header
    let payload = parts.next()?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('=').as_bytes())
        .ok()?;
    serde_json::from_slice::<Value>(&bytes).ok()
}

/// 解 `user_profile`：一段**无填充** base64 的 JSON。两种字母表都试
/// （URL_SAFE 与 STANDARD），谁先解出合法 JSON 算谁 —— 上游在这里没给口径，
/// 而这条链的代价是「取不到名字」，不是「取错名字」。
fn decode_user_profile(text: &str) -> Option<Value> {
    use base64::engine::general_purpose::{STANDARD_NO_PAD, URL_SAFE_NO_PAD};
    use base64::Engine;
    let body = text.trim().trim_end_matches('=');
    if body.is_empty() {
        return None;
    }
    for candidate in [
        URL_SAFE_NO_PAD.decode(body),
        STANDARD_NO_PAD.decode(body),
    ] {
        let Ok(bytes) = candidate else { continue };
        if let Ok(parsed) = serde_json::from_slice::<Value>(&bytes) {
            return Some(parsed);
        }
    }
    None
}

/// 从已解开的 `user_profile` 里取一个非空字符串字段。
fn profile_text(profile: &Option<Value>, field: &str) -> Option<String> {
    profile.as_ref().and_then(|value| text_of(value, field))
}

/// 取一个非空的字符串字段（trim 后为空算没有）。
fn text_of(container: &Value, field: &str) -> Option<String> {
    container
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
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
    use serde_json::json;

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
    fn jwt_identity_reads_the_top_level_claims() {
        let token_of = |claims: &str| format!("header.{}.sig", base64url(claims.as_bytes()));
        let name_of = |jwt: &str| jwt_identity(jwt).user_name;

        assert_eq!(
            "user@example.com",
            name_of(&token_of(r#"{"preferred_username":"user@example.com","sub":"u1"}"#))
        );
        // `preferred_username` 缺 → 退到 `name`
        assert_eq!("张三", name_of(&token_of(r#"{"name":"张三"}"#)));
        // 带 `=` 填充也能解（JWT 通常不带，但别为它挂）
        let padded = format!("{}==", base64url(r#"{"name":"abc"}"#.as_bytes()));
        assert_eq!("abc", name_of(&format!("h.{padded}.s")));

        // 取不到就给空串（调用方退下一级兜底），不 panic
        assert_eq!("", name_of(""));
        assert_eq!("", name_of("not.a.jwt"));
        assert_eq!("", name_of(&token_of(r#"{"sub":"u1"}"#)));
    }

    /// 实测形态（2026-10-11，直接解本机 dev 库里两枚**存量 refresh token** 的 claims，
    /// 零网络、零消耗）：身份不在顶层，而是装在一个 base64 JSON 串（`user_profile`）里，
    /// 键集 `client_id / cnf / exp / federation / iat / iss / jti / type / user_profile`。
    /// 参考实现只读顶层的 `preferred_username`/`name`（`refresh.mjs` 的 `idTokenClaims`），
    /// 所以它这条路也取不到名字 —— 这里多解那一层。
    /// （夹具值全是合成串，不是真账号数据）
    #[test]
    fn identity_reaches_into_the_nested_user_profile() {
        let profile = base64url(
            br#"{"account_id":"acc_0123456789abcdef","account_name":"hid_synthetic-1","principal_id":"prin_89abcdef01234567","principal_urn":"iam::acc_0123456789abcdef:user:hid_synthetic-1"}"#,
        );
        let claims = format!(
            r#"{{"client_id":"pdp5_for_agentarts","type":"app","iat":1,"exp":2,"user_profile":"{profile}"}}"#
        );
        let identity = jwt_identity(&format!("header.{}.sig", base64url(claims.as_bytes())));
        assert_eq!("hid_synthetic-1", identity.user_name, "名字在 user_profile.account_name");
        assert_eq!("prin_89abcdef01234567", identity.principal_id);
    }

    /// 顶层字段仍然优先（上游哪天把名字放回顶层，不用改代码就回到既有口径）。
    #[test]
    fn top_level_claims_still_win_over_the_profile() {
        let profile = base64url(br#"{"account_name":"from_profile","principal_id":"prin_2"}"#);
        let claims = format!(r#"{{"preferred_username":"顶层优先","user_profile":"{profile}"}}"#);
        let identity = jwt_identity(&format!("h.{}.s", base64url(claims.as_bytes())));
        assert_eq!("顶层优先", identity.user_name);
        assert_eq!("prin_2", identity.principal_id, "principal 仍从内层取");
    }

    /// 内层解不开（不是 base64 / 不是 JSON / 字段不是字符串）⇒ 两项都空、不报错：
    /// 名字这条链没有任何失败面，畸形载荷只当没取到。
    #[test]
    fn an_unreadable_profile_yields_no_identity() {
        let blobs = vec![
            "!!!not base64!!!".to_string(),
            base64url(b"plain text, not json"),
            base64url(br#"{"account_name":123}"#),
            base64url(br#"{"other":"x"}"#),
        ];
        for blob in blobs {
            let claims = format!(r#"{{"user_profile":"{blob}"}}"#);
            let identity = jwt_identity(&format!("h.{}.s", base64url(claims.as_bytes())));
            assert!(
                identity.user_name.is_empty() && identity.principal_id.is_empty(),
                "畸形 user_profile 不该产出名字：{blob}"
            );
        }
        let empty = jwt_identity(&format!("h.{}.s", base64url(br#"{"sub":"u1"}"#)));
        assert!(empty.user_name.is_empty() && empty.principal_id.is_empty());
    }

    /// **账号名一直是十六进制的真根因**：上游把这个租户的身份放在
    /// `refresh_token` 的 claims 里（实测两枚存量 refresh token 都带
    /// `user_profile.account_name`），而取名字的代码只看 `id_token` ——
    /// 那一发里压根没有可用的 `id_token`，于是永远退到下一格兜底。
    #[test]
    fn response_identity_falls_back_to_the_refresh_token() {
        let profile = base64url(
            br#"{"account_id":"acc_0123456789abcdef","account_name":"hid_synthetic-1","principal_id":"0123456789abcdef0123456789abcdef"}"#,
        );
        let bearer = format!(
            "h.{}.s",
            base64url(
                format!(
                    r#"{{"client_id":"pdp5_for_agentarts","type":"refreshToken","iat":1,"exp":2,"user_profile":"{profile}"}}"#
                )
                .as_bytes()
            )
        );

        // 只有 refresh_token（登录与续期响应的实测形态）
        let identity = response_identity(&json!({"credentials": {}, "refresh_token": bearer}));
        assert_eq!("hid_synthetic-1", identity.user_name, "名字在 refresh_token 里");
        assert_eq!("0123456789abcdef0123456789abcdef", identity.principal_id);

        // 同一发里 id_token 也带名字 ⇒ 以 id_token 为准（它是这次会话的身份快照）
        let snapshot = format!(
            "h.{}.s",
            base64url(r#"{"preferred_username":"top-level-wins"}"#.as_bytes())
        );
        let identity = response_identity(&json!({
            "id_token": snapshot,
            "refresh_token": bearer,
        }));
        assert_eq!("top-level-wins", identity.user_name);

        // id_token 有但没名字、refresh_token 有名字 ⇒ 仍然取到名字（旧代码正是卡在这）
        let bare = format!("h.{}.s", base64url(br#"{"sub":"u1"}"#));
        let identity = response_identity(&json!({"id_token": bare, "refresh_token": bearer}));
        assert_eq!("hid_synthetic-1", identity.user_name);

        // 两发都没有 ⇒ 空串，调用方继续退下一级兜底
        let identity = response_identity(&json!({"credentials": {}}));
        assert!(identity.user_name.is_empty() && identity.principal_id.is_empty());
    }
}
