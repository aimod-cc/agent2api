//! OrcaRouter 的 **OAuth 2.0 授权码 + PKCE（S256）** 登录：授权地址、换码与
//! pending 表。
//!
//! ── 协议（逐条对照规范，不得凭记忆改写）────────────────────────
//! ```text
//! ① 生成 code_verifier（32 字节密码学随机 → base64url 无 padding）
//! ② code_challenge = base64url(sha256(verifier))，method = S256
//! ③ 浏览器打开
//!      {auth}/auth?callback_url=http%3A%2F%2F127.0.0.1%3A<port>%2Fauth%2Fcallback-orcarouter
//!                 &code_challenge=<challenge>&code_challenge_method=S256
//!                 &state=<opaque>&app_name=Agent2API&scope=api
//! ④ 授权页把 ?code=…&state=… 拼到 callback_url 上跳回来
//! ⑤ POST {auth}/api/v1/auth/keys
//!      {"code": …, "code_verifier": …, "code_challenge_method": "S256"}
//!    → 200 {"key": "sk-orca-…", "user_id": "12345", "scope": "api"}
//! ```
//!
//! **换码在 `{auth}/api/v1/auth/keys`，不是 `/v1/auth/keys`。** 认证 origin
//! （`https://www.orcarouter.ai`）与推理 origin（`https://api.orcarouter.ai/v1`）
//! 是两个地址，绝不能靠替换 hostname 或顺手拼 `/v1` 互推 —— 见 `mod.rs` 的
//! [`super::Endpoints`]。
//!
//! ── 为什么 Flow A（loopback）而不是 Flow B（out-of-band）────────
//! 判据是「客户端跑在哪」（规范「Pick your flow」）：本网关**有浏览器**，而且
//! **能**在 `127.0.0.1` 上监听 —— 它本来就是一个本机 HTTP 服务，进程启动时就
//! 知道自己监听哪个端口（`ServerState::bootstrap` → [`set_loopback_port`]），
//! 并且已经有一条同款的 loopback 回调路由（`/auth/callback-accio`）。
//! 于是 Flow A 是最短路径：用户点一下、浏览器自己回来，不需要复制粘贴。
//!
//! **Flow B 的能力并没有被丢掉**：规范指出用户可以在同意页上选
//! 「Show me a code」，那条路给出的就是一个要人肉粘贴的 code —— 它和 Flow A
//! 的 code 走**同一个**换码接口、同一个 pending 表。因此
//! [`super::adapter`] 的换码入口同时接受「回调 URL」与「裸 code」（见
//! `Api::exchange` 的实现），容器 / 远程部署形态照样能用。
//! Flow C（设备授权）本家**没有实现**，PR 正文如实说明。
//!
//! ── 每次都新建 verifier / state（规范里点名的错误）──────────────
//! verifier 必须来自**密码学随机源**、每次尝试都新生成、且**从不**进 URL /
//! 日志 / 遥测；它只在换码请求的 body 里出现一次。state 是 CSRF 闸门：回调进来
//! 时先**常量时间**比对，不匹配就拒绝 —— 否则任何本机进程都能把一个别人的 code
//! 塞进我们的监听端口。
//!
//! ── 这一家没有 refresh grant（不要再造一个）────────────────────
//! 换回来的是**长期 API Key**：不刷新、不轮换，被吊销就只能重新走一遍登录
//! （每用户 24 小时最多签发 10 把 PKCE Key，所以启动时绝不能重新授权一次）。
//! 相关处置见 [`super::adapter`] 的 `refresh_access_token` 与
//! `core::disconnect_guard`。
//!
//! ── 硬约束 ────────────────────────────────────────────────
//! 绝不 unwrap/expect/panic；不持锁穿越 await；verifier / code / key 一律不进
//! 日志与错误文案。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::server::core::egress;
use crate::server::core::proxies::ResolvedProxy;
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::credentials::Credentials;
use super::Endpoints;

/// 本网关的 loopback 回调路径（浏览器 302 到这里；与 `/auth/callback-accio`
/// 同一约定：一眼能看出是哪一家，且别家不会撞）。
pub const CALLBACK_PATH: &str = "/auth/callback-orcarouter";

/// 同意页上显示的应用名（规范里 app_name 是**调用方选定的标签**，
/// 页面会把它作为「谁在请求」的声明展示出来）。
pub const APP_NAME: &str = "Agent2API";

/// 请求的 scope。`api` 是规范里推理用的那一档（也是默认值）。
pub const REQUESTED_SCOPE: &str = "api";

/// pending 表的有效期。规范：授权码 10 分钟 TTL；这里留一点余量让「刚过期的
/// code」给出的是「已过期」而不是「查不到」。
const PENDING_TTL_MS: i64 = 11 * 60 * 1000;

/// pending 表的最大条目数（防御：异常客户端反复开始登录时不能无界增长）。
/// 每用户 24 小时 10 把 Key 的上游限额本身就会拒绝第 11 次，这里只挡本机堆积。
const MAX_PENDING: usize = 64;

/// 换码请求的超时（一次 POST，不该长等）。
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);

/// 本网关的监听端口（`ServerState::bootstrap` 时写入一次）。
///
/// 与 `accio::oauth::set_loopback_port` 同一手法：`build_login_url()` 是同步、
/// 无参的 trait 方法，拿不到 `ServerState`；而授权地址里必须拼上本机回调地址。
/// 端口在进程生命周期内不变，开机写一次、之后只读。
static LOOPBACK_PORT: OnceLock<u16> = OnceLock::new();

/// 记录本进程的监听端口（重复调用无害）。
pub fn set_loopback_port(port: u16) {
    let _ = LOOPBACK_PORT.set(port);
}

/// 本机回调基址（端口未知时 `None`）。
pub fn loopback_base() -> Option<String> {
    LOOPBACK_PORT
        .get()
        .map(|port| format!("http://127.0.0.1:{port}"))
}

/// 本机回调地址（`{loopback}{CALLBACK_PATH}`）。端口未知时 `None`。
pub fn callback_url() -> Option<String> {
    Some(format!("{}{}", loopback_base()?, CALLBACK_PATH))
}

/// 一次待完成登录的全部上下文（回调进来时按 state 取回）。
///
/// **只在内存**，进程重启即作废（与 accio / AutoClaw 的 pending 同一处置）：
/// 把它落盘等于把 verifier 写进磁盘，而它的全部意义就是「不离开本进程」。
pub struct PendingLogin {
    /// 密码学随机的一次性 verifier（base64url 无 padding）
    verifier: String,
    /// 与授权时逐字相同的回调地址（换码时要原样回传 callback 语义）
    callback_url: String,
    /// 创建时刻（毫秒，TTL 判定用）
    created_at: i64,
}

impl PendingLogin {
    /// verifier 的**长度**（诊断用；不暴露内容）。
    pub fn verifier_len(&self) -> usize {
        self.verifier.len()
    }

    /// 回调地址（日志里可以出现：它不是秘密）。
    pub fn callback_url_text(&self) -> &str {
        &self.callback_url
    }
}

fn pending_table() -> &'static Mutex<HashMap<String, PendingLogin>> {
    static TABLE: OnceLock<Mutex<HashMap<String, PendingLogin>>> = OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock_table() -> std::sync::MutexGuard<'static, HashMap<String, PendingLogin>> {
    match pending_table().lock() {
        Ok(guard) => guard,
        // 中毒恢复：pending 表里全是短命的一次性条目，没有跨调用的一致性不变量
        // 需要靠 panic 传播 —— 恢复出来继续用比整个登录功能瘫痪好。
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// base64url 无 padding（PKCE 与 state 的编码口径）。
fn b64url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// 密码学随机字节。系统随机源不可用时返回 `Err`（**绝不**回落到时间戳 /
/// 计数器这类可预测源：那正是规范点名的错误做法）。
fn random_bytes(len: usize) -> Result<Vec<u8>, String> {
    let mut buffer = vec![0u8; len];
    getrandom::getrandom(&mut buffer)
        .map_err(|_| "系统随机源不可用，无法安全地发起登录（请重试）".to_string())?;
    Ok(buffer)
}

/// 新建一次尝试的 verifier 与 state（**每次调用都重新生成**）。
///
/// verifier 用 32 字节（256 bit）随机；state 用 16 字节 —— 两者的用途不同：
/// verifier 是防换码的密钥，state 是防 CSRF 的随机串。
pub fn new_verifier_and_state() -> Result<(String, String), String> {
    let verifier = b64url(&random_bytes(32)?);
    let state = b64url(&random_bytes(16)?);
    Ok((verifier, state))
}

/// verifier → challenge：`base64url(sha256(verifier))`，无 padding（S256）。
pub fn challenge_for(verifier: &str) -> String {
    b64url(Sha256::digest(verifier.as_bytes()).as_slice())
}

/// 常量时间字节比较（长度不同直接 false —— 长度本身不是秘密）。
///
/// 为什么不用 `==`：`==` 会在第一个不同字节处提前返回，调用方是「本机任何进程
/// 都能打进来的回调端点」，逐字节短路把 state 比对变成一个可以按字节暴力猜的
/// 预言机。这里把全部字节异或累积起来，只在最后看是否为零。
fn constant_time_eq(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// 构造授权页地址（`{auth}/auth?...`）。
///
/// `callback_url` 为 `oob` 时是 Flow B 的形态（规范说这个字面量要显式给出，
/// 而不是省略）。本模块默认走 Flow A，oob 作为「浏览器与网关不同机」的兜底
/// 由调用方选择。
pub fn build_authorize_url(
    endpoints: &Endpoints,
    callback: &str,
    challenge: &str,
    state: &str,
) -> String {
    // `prompt=consent` 不在默认链路上：强制每次重新批准会让用户在 24 小时
    // 10 把 Key 的限额下更快撞限；需要时由调用方另行追加。
    format!(
        "{}?callback_url={}&code_challenge={}&code_challenge_method=S256&state={}&app_name={}&scope={}",
        endpoints.authorize_endpoint(),
        urlencode(callback),
        urlencode(challenge),
        urlencode(state),
        urlencode(APP_NAME),
        urlencode(REQUESTED_SCOPE),
    )
}

/// query 参数编码（与项目既有的 `accio::oauth::urlencode` 同口径：
/// 只转义必须转义的字符）。
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

/// 登记一轮登录（生成 state + verifier，产出授权地址）。
///
/// 返回 `None` 表示**端口未知或随机源不可用** —— 上层文案会如实说明「未能生成
/// 授权地址」，而不是给一个必然 404 的地址。
pub fn begin_login(endpoints: &Endpoints, callback: &str) -> Option<(String, String)> {
    let (verifier, state) = match new_verifier_and_state() {
        Ok(pair) => pair,
        Err(reason) => {
            logging::log("[Login]", &format!("❌ OrcaRouter 无法生成 PKCE 参数: {reason}"));
            return None;
        }
    };
    let challenge = challenge_for(&verifier);
    let auth_url = build_authorize_url(endpoints, callback, &challenge, &state);
    {
        let mut table = lock_table();
        purge_expired(&mut table);
        if table.len() >= MAX_PENDING {
            logging::log(
                "[Login]",
                "⚠️ OrcaRouter 待完成的登录过多，已丢弃最旧的一轮（可能是异常客户端反复发起）",
            );
            // 丢最旧的一条（按创建时刻）
            if let Some(oldest) = table
                .iter()
                .min_by_key(|(_, pending)| pending.created_at)
                .map(|(state, _)| state.clone())
            {
                table.remove(&oldest);
            }
        }
        table.insert(
            state.clone(),
            PendingLogin {
                verifier,
                callback_url: callback.to_string(),
                created_at: logging::now_ms(),
            },
        );
    }
    // 日志里只有 state 的长度与回调地址（**不含 verifier**）
    logging::log(
        "[Login]",
        &format!(
            "发起 OrcaRouter 网页登录（PKCE S256，state 长度 {}，回调 {}）",
            state.len(),
            callback
        ),
    );
    Some((auth_url, state))
}

/// 清掉过期的 pending（每次登记/取用时顺带做一次，不另起清理任务）。
fn purge_expired(table: &mut HashMap<String, PendingLogin>) {
    let now = logging::now_ms();
    table.retain(|_, pending| now - pending.created_at <= PENDING_TTL_MS);
}

/// 取走一轮 pending（换码用；**取走即删除** —— 授权码一次性，pending 也一次性）。
///
/// 取不到 = 「不是本进程发起的那一轮」（已取消 / 已过期 / 别人的 state）。
pub fn take_pending(state: &str) -> Option<PendingLogin> {
    let mut table = lock_table();
    let pending = table.remove(state)?;
    (logging::now_ms() - pending.created_at <= PENDING_TTL_MS).then_some(pending)
}

/// 丢弃一轮 pending（回调失败时把这一轮作废，免得留在表里等超时）。
pub fn drop_pending(state: &str) {
    lock_table().remove(state);
}

/// 当前 pending 条数（测试与诊断用）。
pub fn pending_count() -> usize {
    lock_table().len()
}

/// 从回调查询串里取出 `code`/`state`，并**先按常量时间比对 state**。
///
/// 顺序是规范明确要求的：`state` 比对在**动 code 之前**。拒绝、state 不匹配、
/// 上游带 `error` 三种情况分别给出可操作的文案（不挂起、不热循环）。
pub fn parse_callback(
    query: &HashMap<String, String>,
    expected_state: &str,
) -> Result<String, GatewayError> {
    let state = query.get("state").map(String::as_str).unwrap_or("");
    if state.is_empty() {
        return Err(GatewayError::with_status(
            400,
            "回调没有携带 state，无法确认这次授权归属（可能不是本网关发起的那一轮）",
        ));
    }
    if !constant_time_eq(state, expected_state) {
        return Err(GatewayError::with_status(
            403,
            "回调的 state 与本次登录不一致，已拒绝（可能是伪造或串台的授权回调）",
        ));
    }
    if let Some(error) = query
        .get("error")
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Err(GatewayError::with_status(
            400,
            format!("授权被拒绝（{error}），请在同意页确认后再试一次"),
        ));
    }
    let code = query
        .get("code")
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            GatewayError::with_status(400, "回调没有携带授权码（code），请重新发起登录")
        })?;
    Ok(code.to_string())
}

/// 用一次性 `code` + verifier 换回长期 API Key。
///
/// 请求体逐字对照规范：`{code, code_verifier, code_challenge_method: "S256"}`，
/// 打到 `{auth}/api/v1/auth/keys`（**不是** `/v1/auth/keys`）。
///
/// 错误语义（规范表）：
///   · `400` —— `code_challenge_method` 不认，或与授权时发的不一致（降级防御）；
///   · `403` —— code 未知 / 已过期 / 已用过，或 verifier 与存的 challenge 不符；
///   · `429` —— 该用户 24 小时内第 11 次签发（见规范「Persist it」）。
/// 三者都给**终止性**错误，调用方把任务标记为失败（不重试 —— 重试同一个 code
/// 只会一直 403，而重试一轮新登录会更快撞到 429）。
pub async fn exchange_code(
    endpoints: &Endpoints,
    pending: &PendingLogin,
    code: &str,
    proxy: Option<&ResolvedProxy>,
) -> Result<Credentials, GatewayError> {
    let code = code.trim();
    if code.is_empty() {
        return Err(GatewayError::with_status(400, "换码请求缺少授权码"));
    }
    let client = egress::client_for(proxy);
    let body = json!({
        "code": code,
        "code_verifier": pending.verifier,
        "code_challenge_method": "S256",
    });
    let response = client
        .post(endpoints.exchange_endpoint())
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .timeout(EXCHANGE_TIMEOUT)
        .json(&body)
        .send()
        .await
        .map_err(|error| {
            // 网络错误：安全结束（不热循环），给可操作提示。
            // 注意 `describe_error_detail` 只描述传输层，**不包含请求体** ——
            // verifier 因此在错误文案里不会出现。
            GatewayError::with_status(
                502,
                format!(
                    "OrcaRouter 换码请求失败（网络错误）：{}。请检查网络与 auth origin（当前 {}）后重试",
                    egress::describe_error_detail(&error),
                    endpoints.auth_base
                ),
            )
        })?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    if !(200..300).contains(&status) {
        return Err(exchange_error(status, &text, endpoints));
    }
    let payload: Value = serde_json::from_str(&text).map_err(|error| {
        GatewayError::with_status(
            502,
            format!("OrcaRouter 换码响应不是合法 JSON：{error}"),
        )
    })?;
    let credentials = Credentials::from_exchange(&payload, None).map_err(|reason| {
        GatewayError::with_status(502, format!("OrcaRouter 换码响应无法使用：{reason}"))
    })?;
    if !credentials.scope_is_sufficient() {
        // 读到的是**授予**的 scope，不是请求的那个：用户可能被批准了更窄的
        // 授权。这里如实拒绝并说明（规范要求「say so」），而不是假装拿到了 api。
        return Err(GatewayError::with_status(
            403,
            format!(
                "OrcaRouter 只授予了「{}」范围，不足以访问推理接口（需要「{}」或「connector」）。\
                 请让账号管理员调整工作区权限后重新登录，或改用「填写 API Key」",
                credentials.scope, REQUESTED_SCOPE
            ),
        ));
    }
    logging::log(
        "[Login]",
        &format!(
            "✅ OrcaRouter 换码成功（尾号 {}，授予范围 {}）",
            credentials.token_tail(),
            if credentials.scope.is_empty() { "未声明" } else { credentials.scope.as_str() }
        ),
    );
    Ok(credentials)
}

/// 换码失败 → 可操作的错误（状态码语义来自规范的表）。
///
/// 上游的错误体是 `{"error": {...}}` 或普通信封；这里只取一段**截断后的**摘要
/// 用于定位，绝不把整份响应（可能含凭据）灌进错误文案或日志。
fn exchange_error(status: u16, text: &str, endpoints: &Endpoints) -> GatewayError {
    let summary = summarize(text, 300);
    let message = match status {
        400 => format!(
            "OrcaRouter 换码请求被拒（400）：code_challenge_method 不被识别或与授权时不一致。\
             请重新发起登录；若自建实例，请确认 auth origin（当前 {}）使用了 S256 的 PKCE 端点。上游说明：{summary}",
            endpoints.auth_base
        ),
        403 => format!(
            "OrcaRouter 授权码已失效（403）：可能已过期（有效期 10 分钟）、已被用过，或 verifier 与 challenge 不符。\
             请重新发起登录。上游说明：{summary}"
        ),
        429 => format!(
            "OrcaRouter 拒绝了这次签发（429）：同一账号 24 小时内最多签发 10 把登录 Key。\
             请先复用已保存的凭据，或稍后再试。上游说明：{summary}"
        ),
        other => format!(
            "OrcaRouter 换码失败（HTTP {other}）。请稍后重试；持续失败请核对 auth origin（当前 {}）。上游说明：{summary}",
            endpoints.auth_base
        ),
    };
    GatewayError::with_status(status as i32, message)
}

/// 响应摘要（截断）。
fn summarize(text: &str, limit: usize) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return "（空响应）".to_string();
    }
    if trimmed.chars().count() <= limit {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(limit).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_is_unpadded_base64url_sha256() {
        // RFC 7636 附录 B 的官方向量：verifier 对 challenge 的映射必须逐字相同。
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = challenge_for(verifier);
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
        assert!(!challenge.contains('='), "challenge 不得带 padding");
        assert!(!challenge.contains('+') && !challenge.contains('/'), "必须 base64url 字母表");
    }

    #[test]
    fn verifier_and_state_are_fresh_every_attempt() {
        let (v1, s1) = new_verifier_and_state().expect("random source");
        let (v2, s2) = new_verifier_and_state().expect("random source");
        assert_ne!(v1, v2, "verifier 必须每次重新生成");
        assert_ne!(s1, s2, "state 必须每次重新生成");
        // 32 字节 base64url 无 padding = ceil(32*4/3) = 43 字符
        assert_eq!(v1.len(), 43);
        // 16 字节 → 22 字符
        assert_eq!(s1.len(), 22);
    }

    #[test]
    fn authorize_url_uses_auth_origin_and_never_carries_the_verifier() {
        let endpoints = Endpoints::new("https://www.orcarouter.ai", "https://api.orcarouter.ai/v1");
        let (verifier, state) = new_verifier_and_state().expect("random source");
        let challenge = challenge_for(&verifier);
        let url = build_authorize_url(
            &endpoints,
            "http://127.0.0.1:51733/auth/callback-orcarouter",
            &challenge,
            &state,
        );
        assert!(url.starts_with("https://www.orcarouter.ai/auth?"), "授权地址在认证 origin 的 /auth");
        assert!(!url.contains("api.orcarouter.ai"), "授权地址绝不指向推理 origin");
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains(&format!("code_challenge={challenge}")));
        assert!(url.contains(&format!("state={state}")));
        assert!(url.contains("callback_url=http%3A%2F%2F127.0.0.1%3A51733"));
        assert!(!url.contains(&verifier), "verifier 绝不能出现在 URL 里");
    }

    #[test]
    fn exchange_endpoint_is_api_v1_auth_keys_on_the_auth_origin() {
        let endpoints = Endpoints::new("https://www.orcarouter.ai", "https://api.orcarouter.ai/v1");
        assert_eq!(
            endpoints.exchange_endpoint(),
            "https://www.orcarouter.ai/api/v1/auth/keys"
        );
        // 反例：推理 origin 上的 /v1/auth/keys 是 404，本实现永远不得拼出它
        assert_ne!(
            endpoints.exchange_endpoint(),
            format!("{}/auth/keys", endpoints.api_base)
        );
        assert!(!endpoints.exchange_endpoint().contains("api.orcarouter.ai"));
    }

    #[test]
    fn state_mismatch_is_rejected_before_the_code_is_used() {
        let mut query = HashMap::new();
        query.insert("code".to_string(), "one-time-code".to_string());
        query.insert("state".to_string(), "attacker-state".to_string());
        let error = parse_callback(&query, "our-state").expect_err("state 不匹配必须拒绝");
        assert_eq!(error.status_code, 403);
        // 有效 state 时同一个 code 才被接受
        query.insert("state".to_string(), "our-state".to_string());
        assert_eq!(parse_callback(&query, "our-state").unwrap(), "one-time-code");
    }

    #[test]
    fn denial_and_missing_state_are_terminal_with_actionable_text() {
        let mut query = HashMap::new();
        query.insert("error".to_string(), "access_denied".to_string());
        query.insert("state".to_string(), "s".to_string());
        let denied = parse_callback(&query, "s").expect_err("拒绝必须报错");
        assert_eq!(denied.status_code, 400);
        assert!(denied.message.contains("access_denied"), "要原样带出拒绝原因");

        let empty = HashMap::new();
        let missing = parse_callback(&empty, "s").expect_err("缺 state 必须报错");
        assert_eq!(missing.status_code, 400);
    }

    #[test]
    fn pending_is_single_use_and_constant_time_compared() {
        let state = format!("state-{}", logging::now_ms());
        let mut table = lock_table();
        table.insert(
            state.clone(),
            PendingLogin {
                verifier: "v".to_string(),
                callback_url: "http://127.0.0.1/cb".to_string(),
                created_at: logging::now_ms(),
            },
        );
        drop(table);
        assert!(take_pending(&state).is_some(), "第一次取用应当命中");
        assert!(take_pending(&state).is_none(), "取走即删除（授权码一次性）");
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "ab"));
    }

    #[test]
    fn exchange_errors_map_to_actionable_messages() {
        let endpoints = Endpoints::new("https://www.orcarouter.ai", "https://api.orcarouter.ai/v1");
        for status in [400u16, 403, 429, 500] {
            let error = exchange_error(status, "{\"error\":\"x\"}", &endpoints);
            assert_eq!(error.status_code, status as i32);
            assert!(!error.message.is_empty());
        }
        let downgraded = exchange_error(400, "", &endpoints);
        assert!(downgraded.message.contains("code_challenge_method"), "400 要说清降级防御");
        let reused = exchange_error(403, "", &endpoints);
        assert!(reused.message.contains("授权码已失效"));
        let limited = exchange_error(429, "", &endpoints);
        assert!(limited.message.contains("10 把"), "429 要说清 24 小时限额");
    }
}
