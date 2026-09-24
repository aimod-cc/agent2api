//! Accio **网页登录**：OAuth 2.0 授权码 + PKCE（S256）。
//!
//! ── 协议（逆向自桌面端 `AuthService`）────────────────────────
//! ```text
//! ① 生成 code_verifier（随机）/ code_challenge（SHA-256 → base64url）
//! ② 打开 {loginBase}/login?return_url=<本机回调>&state=<state>
//!        &code_challenge=<challenge>&code_challenge_method=S256&client_id=accio-work
//! ③ 用户登录完成 → 登录页把 ?code=…&state=… 拼到 return_url 上跳回来
//! ④ POST {gw}/api/oauth/token  {code, codeVerifier, clientId, redirectUri}
//!        → {accessToken, refreshToken, expiresAt}
//! ```
//! `client_id` 是桌面端常量（`accio-work`，两地共用）；`redirectUri` 必须与
//! 第 ② 步的 `return_url` **逐字相同**，因此它跟着 pending 一起存下来原样回传。
//!
//! ── 为什么要 PKCE 与 state（而不是只带一个 code）──────────────
//! 回调落在本机 HTTP 端口上（`http://127.0.0.1:<port>/auth/callback-accio`），
//! 任何本机进程都能伪造一次 GET。两道闸门各挡一类问题：
//!   - **state**：`← 这次回调是不是本进程刚发起的那一轮`。逐字比对，不匹配就
//!     拒绝 —— 否则别人可以拿自己那次的 code 把凭证写进你的账号库；
//!   - **code_verifier**：规范给的防换码能力（授权码被截获也换不出 token，
//!     因为换码请求要带上只有我们知道的那个原像）。
//! 两者都随 pending 表进出：**只在内存**，不落盘（进程重启等于那次登录作废，
//! 与 AutoClaw 的 `PendingOauth` 同一处置）。
//!
//! ── 与桌面端 `accio://` 回调的关系 ──────────────────────────
//! 官方客户端优先用自定义协议（`accio://auth/callback`），退到它自己 local
//! server 的 `/auth/callback`。本网关走**后者同款**的 loopback 形态：
//! Tauri 没有 Electron 的协议接管能力（见 `src-tauri/src/login.rs` 的模块头），
//! 而 loopback 是浏览器导航、内嵌窗口与系统浏览器都天然走得通的那条。
//!
//! ── panic=abort ────────────────────────────────────────────
//! 本文件在登录链路上，绝不 unwrap/expect/panic。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::server::core::proxies::ResolvedProxy;
use crate::server::errors::GatewayError;
use crate::server::logging;
use crate::server::core::upstream::request::new_request_id;

use super::auth;
use super::credentials::{self, Credentials};
use super::endpoints::{self, Region};

/// 本网关 loopback 回调路径（浏览器 302 到这里；与 AutoClaw 的
/// `/auth/callback-{vendor}` 同一约定：一眼能看出是哪一家）。
pub const CALLBACK_PATH: &str = "/auth/callback-accio";

/// pending 表的有效期：与登录任务的 5 分钟超时同一量级
const PENDING_TTL_MS: i64 = 6 * 60 * 1000;

/// 本网关的监听端口（`ServerState::bootstrap` 时写入）。
///
/// ── 为什么要一个进程级常量 ──────────────────────────────────
/// `ProviderAdapter::build_login_url()` 是同步、无参的（trait 契约），拿不到
/// `ServerState`；而授权地址里必须拼上本机的回调地址。端口在进程生命周期内
/// 不变，因此开机写一次、之后只读 —— 与各家 `models` 的进程级缓存同一手法。
static LOOPBACK_PORT: OnceLock<u16> = OnceLock::new();

/// 记录本进程的监听端口（`ServerState::bootstrap` 调一次，重复调用无害）
pub fn set_loopback_port(port: u16) {
    let _ = LOOPBACK_PORT.set(port);
}

/// 本机回调基址（端口未知时 None —— 那时的错误由上层文案说清）
pub fn loopback_base() -> Option<String> {
    LOOPBACK_PORT.get().map(|port| format!("http://127.0.0.1:{port}"))
}

/// 授权页地址（`{loginBase}/login?...`）。
///
/// `return_url` 走 ours：`{loopback}{CALLBACK_PATH}`。桌面端把 trace 参数也塞
/// 进 return_url（`login_trace_id` 等），那是它的埋点需要，本网关不带。
pub fn build_authorize_url(region: Region, state: &str, challenge: &str, return_url: &str) -> String {
    format!(
        "{}/login?return_url={}&state={}&code_challenge={}&code_challenge_method=S256&client_id={}",
        region.login_base(),
        urlencode(return_url),
        urlencode(state),
        urlencode(challenge),
        urlencode(endpoints::CLIENT_ID),
    )
}

/// query 参数编码（与 `core::auth` 里那个同口径：只转义必须转义的字符）
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// 一次待完成登录所需的全部上下文（回调进来时按 state 取回）。
pub struct PendingLogin {
    pub region: Region,
    pub verifier: String,
    /// 与授权时逐字相同的 redirect_uri（换码要原样回传）
    pub redirect_uri: String,
    created_at: i64,
}

fn pending_table() -> &'static Mutex<HashMap<String, PendingLogin>> {
    static TABLE: OnceLock<Mutex<HashMap<String, PendingLogin>>> = OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 登记一轮登录（生成 state + verifier，产出授权地址）。
///
/// 返回 `(auth_url, state)`；`return_url` 由调用方给出（它才知道本机端口）。
pub fn begin_login(region: Region, return_url: &str) -> (String, String) {
    let verifier = random_secret();
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = new_request_id().replace('-', "");
    let auth_url = build_authorize_url(region, &state, &challenge, return_url);
    let mut table = pending_table().lock().unwrap_or_else(|error| error.into_inner());
    sweep(&mut table);
    table.insert(
        state.clone(),
        PendingLogin {
            region,
            verifier,
            redirect_uri: return_url.to_string(),
            created_at: logging::now_ms(),
        },
    );
    (auth_url, state)
}

/// 取走一轮登录（回调进来时调用；取走即失效，重复回调拿不到第二次）
pub fn take_pending(state: &str) -> Option<PendingLogin> {
    let mut table = pending_table().lock().unwrap_or_else(|error| error.into_inner());
    sweep(&mut table);
    table.remove(state)
}

/// 丢弃一轮登录（取消 / 失败收尾）
pub fn drop_pending(state: &str) {
    let mut table = pending_table().lock().unwrap_or_else(|error| error.into_inner());
    table.remove(state);
}

/// 清掉过期的 pending（在每次进出表时顺手做，不另起定时器）
fn sweep(table: &mut HashMap<String, PendingLogin>) {
    let now = logging::now_ms();
    table.retain(|_, pending| now - pending.created_at <= PENDING_TTL_MS);
}

/// 一次性随机串（code_verifier / state 用）。
///
/// 32 字节 → base64url（43 字符），落在 PKCE 要求的 43–128 之间。
fn random_secret() -> String {
    let mut bytes = [0u8; 32];
    if getrandom::getrandom(&mut bytes).is_err() {
        // 随机源不可用时**不能**退化成可预测值（那等于没有 PKCE）：
        // 用 new_request_id 的混合源兜底，它至少混进了 RandomState 的种子。
        return new_request_id().replace('-', "");
    }
    URL_SAFE_NO_PAD.encode(bytes)
}

/// 用授权码换凭证（`POST /api/oauth/token`）。
pub async fn exchange_code(
    region: Region,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
    proxy: Option<&ResolvedProxy>,
) -> Result<Credentials, GatewayError> {
    let body = json!({
        "code": code,
        "codeVerifier": verifier,
        "clientId": endpoints::CLIENT_ID,
        "redirectUri": redirect_uri,
    });
    let response = auth::post_json(region, endpoints::OAUTH_TOKEN_PATH, &body, proxy).await?;
    let data = auth::payload(response, "授权码换令牌").map(auth::unwrap_data)?;
    let access_token = credentials::secret(&data, &["accessToken", "access_token", "token"])?;
    if access_token.is_empty() {
        return Err(GatewayError::with_status(502, "Accio 换码响应缺少 accessToken"));
    }
    let mut credentials = Credentials {
        region,
        access_token,
        refresh_token: credentials::secret(&data, &["refreshToken", "refresh_token"])?,
        expires_at: credentials::timestamp(
            data.get("expiresAt").or_else(|| data.get("expires_at")),
        ),
        user_id: String::new(),
        email: String::new(),
        name: String::new(),
        device_id: String::new(),
    };
    credentials.ensure_identity();
    // 换回来的 token 是不透明串，账号标识只能靠资料接口问一次
    match auth::fetch_profile(&credentials.access_token, region, proxy).await {
        Ok(profile) => auth::apply_profile(&mut credentials, &profile),
        Err(error) => logging::verbose(
            "[Accio]",
            &format!("登录后查资料失败（不影响落账号）: {}", error.message),
        ),
    }
    credentials.ensure_identity();
    Ok(credentials)
}

/// 从回调 URL 里取 `code` / `state`（两个都必需）。
pub fn parse_callback(query: &HashMap<String, String>) -> Result<(String, String), GatewayError> {
    if let Some(error) = query.get("error").filter(|value| !value.trim().is_empty()) {
        return Err(GatewayError::with_status(
            400,
            format!("授权被拒绝（{error}）"),
        ));
    }
    let code = query.get("code").map(String::as_str).unwrap_or("").trim();
    let state = query.get("state").map(String::as_str).unwrap_or("").trim();
    if code.is_empty() {
        return Err(GatewayError::with_status(400, "回调没有携带授权码"));
    }
    if state.is_empty() {
        return Err(GatewayError::with_status(400, "回调没有携带 state，无法确认这次登录归属"));
    }
    if code.len() > 8192 || state.len() > 512 {
        return Err(GatewayError::with_status(400, "回调参数超长，已拒绝"));
    }
    Ok((code.to_string(), state.to_string()))
}
