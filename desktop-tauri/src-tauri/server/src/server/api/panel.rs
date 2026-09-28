//! 管理面板的注册 / 登录 / 刷新 / 登出（`/api/panel/*`）。
//!
//! ── 面板认证模型（双令牌，照 OmniProxy 的语义）────────────────
//! 账号密码是「人」的凭证，API Key 是「程序」的凭证，各管一层：
//!   · 登录成功签发双令牌 —— access（2 小时，path=/）+ refresh
//!     （30 天，path=/api/panel）；`/api/*` 由 `http::require_api_key`
//!     认 access 会话或 API Key；
//!   · access 过期后前端调 `POST /api/panel/refresh` 静默换新
//!     （轮换：旧 refresh 作废、新 refresh 同会话链；重放旧 refresh
//!     会被检测为泄露并整链作废）；
//!   · 首次部署（没有任何管理员）时登录页走注册模式：
//!     `POST /api/panel/setup` 创建管理员并直接登录。
//!
//! ── 防爆破 ──────────────────────────────────────────────────
//! 同一来源连续失败 5 次锁 5 分钟（按 IP，不是全局 —— 全局锁会把
//! 「攻击者锁死管理员」变成一种攻击）。登录端点挂 public 组：调用方
//! 还没有任何凭证，安全性由锁定与 bcrypt 的校验成本承担。

use axum::body::Bytes;
use axum::extract::ConnectInfo;
use axum::http::{header::SET_COOKIE, HeaderMap};
use axum::response::Response;
use std::net::SocketAddr;

use crate::server::access;
use crate::server::altcha;
use crate::server::config;
use crate::server::errors;
use crate::server::http::{ok_json, parse_body};
use crate::server::logging;

fn body_str<'a>(payload: &'a serde_json::Value, field: &str) -> String {
    payload
        .get(field)
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

/// 机器人校验（ALTCHA proof-of-work，见 `server::altcha`）：开关开启时，
/// 注册 / 登录的请求体必须带登录页算好的 payload，否则 400。失败**不计入**
/// 登录失败锁定 —— 那把锁针对「密码猜错」，校验没过说明还没走到验密码那步。
fn check_captcha(payload: &serde_json::Value) -> Result<(), Response> {
    if !config::current().captcha_enabled() {
        return Ok(());
    }
    let Some(token) = payload.get("captcha").and_then(serde_json::Value::as_str) else {
        return Err(errors::management_error(400, "请完成人机验证后重试"));
    };
    altcha::verify(token).map_err(|message| errors::management_error(400, &message))
}

/// `GET /api/panel/captcha` —— 签发一道 ALTCHA challenge（登录页加载时领）。
///
/// 挂 public：领题时用户还没有任何凭证。响应是**裸 challenge JSON**
/// （不带管理 API 的 success/data 信封）—— 官方 widget 直接读顶层字段，
/// 包了信封它就解析失败（altcha-lib 的 challengeHandler 同样发裸对象）。
/// 开关关闭时回 400 —— 前端据此隐藏验证行。
pub async fn captcha_challenge() -> Response {
    match altcha::challenge() {
        Some(challenge) => crate::server::http::raw_json(challenge),
        None => errors::management_error(400, "机器人校验未启用"),
    }
}

/// 客户端要求「令牌进响应体」的请求头（值固定 `body`）。服务端不做任何
/// 来源/环境猜测：标记由前端在传输探测（`cookie_probe`）失败后显式给出。
const TOKEN_BODY_MODE: &str = "x-panel-auth-mode";

fn tokens_in_body(headers: &HeaderMap) -> bool {
    headers
        .get(TOKEN_BODY_MODE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("body"))
}

/// `GET /api/panel/cookie-probe` —— 会话传输探测（登录页加载时连打两发）。
///
/// 挂 public：探测发生在登录之前。第一发**没有**探针 cookie：种一枚 60 秒
/// 寿命的 `PROBE_COOKIE` 并回 `roundtrip:false`；第二发浏览器若存得下、发
/// 得回，服务端读到它回 `roundtrip:true` —— 本环境的 cookie 传输（含
/// Set-Cookie 下发）完好，登录响应不需要令牌进 body。第二发仍 `false`
/// （中转剥 Set-Cookie / 浏览器拒存第三方 cookie / 隐私模式）＝ cookie 走
/// 不通，前端才带 `x-panel-auth-mode: body` 登录。测的是环境的真实传输，
/// 不猜代理行为：哪天中转把 cookie 修好了，直连形态自动回归。
pub async fn cookie_probe(headers: HeaderMap) -> Response {
    if access::cookie_value(&headers, access::PROBE_COOKIE).is_some() {
        return ok_json(serde_json::json!({ "roundtrip": true }));
    }
    let cookie = format!(
        "{}={}; Path=/; Max-Age=60; HttpOnly; SameSite=Lax",
        access::PROBE_COOKIE,
        access::random_hex(16)
    );
    let mut response = ok_json(serde_json::json!({ "roundtrip": false }));
    if let Ok(value) = axum::http::HeaderValue::from_str(&cookie) {
        response.headers_mut().append(SET_COOKIE, value);
    }
    response
}

fn issue_response(session: access::IssuedSession, tokens_in_body: bool) -> Response {
    // 令牌进不进响应体由客户端的显式标记决定（见 `tokens_in_body`）：
    // 带 `x-panel-auth-mode: body` 的是「探测到 cookie 走不通」的环境
    // （fnOS docker 管理页这类宿主中转入口），前端把 body 里的令牌存
    // localStorage、后续请求走 `x-panel-token` 头（见 access::session_valid
    // 的说明与 web_shim 的 PANEL_AUTH 段）；不带标记的直连环境与旧版
    // 逐字节一致 —— 令牌只在 HttpOnly cookie 里，JS 读不到。带标记的
    // 环境令牌因此 JS 可读：面板是同源可信代码，用可用性换掉的那部分
    // XSS 面只在「cookie 本来就不可用」的世界里发生。
    let mut payload = serde_json::json!({ "loggedIn": true });
    if tokens_in_body {
        payload["accessToken"] = session.access_token.into();
        payload["refreshToken"] = session.refresh_token.into();
    }
    let mut response = ok_json(payload);
    for cookie in [session.access_cookie, session.refresh_cookie] {
        if let Ok(value) = axum::http::HeaderValue::from_str(&cookie) {
            response.headers_mut().append(SET_COOKIE, value);
        }
    }
    response
}

/// `GET /api/panel/status` —— 登录页用它决定显示「注册」还是「登录」。
///
/// 挂 public：打开登录页时用户还没有任何凭证。
pub async fn panel_status() -> Response {
    ok_json(serde_json::json!({
        "registered": access::admin_registered(),
    }))
}

/// `POST /api/panel/setup` —— 首次注册管理员（只在无人注册时成功）。
///
/// 密码要求：至少 8 位。bcrypt 哈希只落库（`kv` 的 `panelAdmin`），明文
/// 不留痕。注册成功直接签发会话 —— 用户注册完就进面板，不再输一次。
pub async fn panel_setup(
    headers: HeaderMap,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    body: Bytes,
) -> Response {
    if access::admin_registered() {
        return management_error(409, "管理员账号已存在，无需重复注册");
    }
    let payload = parse_body(&body).unwrap_or(serde_json::Value::Null);
    if let Err(response) = check_captcha(&payload) {
        return response;
    }
    let username = body_str(&payload, "username");
    let password = payload
        .get("password")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if username.is_empty() || username.len() > 64 {
        return management_error(400, "请填写管理员账号（64 字符以内）");
    }
    if password.len() < 8 {
        return management_error(400, "密码至少 8 位");
    }
    let hash = match bcrypt::hash(password, 10) {
        Ok(hash) => hash,
        Err(error) => {
            logging::log("[Security]", &format!("❌ 管理员注册失败：{error}"));
            return management_error(500, "密码加密失败，请重试");
        }
    };
    match access::setup_admin(&username, &hash) {
        Ok(true) => {
            logging::log(
                "[Security]",
                &format!("✅ 管理员「{username}」注册完成（{}）", addr.ip()),
            );
            issue_response(access::IssuedSession::new_session(), tokens_in_body(&headers))
        }
        Ok(false) => management_error(409, "管理员账号已存在，无需重复注册"),
        Err(reason) => {
            logging::log("[Security]", &format!("❌ 管理员注册失败：{reason}"));
            management_error(500, &reason)
        }
    }
}

/// `POST /api/panel/login` —— 账号密码换双令牌。
pub async fn panel_login(
    headers: HeaderMap,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    body: Bytes,
) -> Response {
    if !access::admin_registered() {
        return management_error(400, "尚未注册管理员账号：请先在登录页完成首次注册");
    }
    if access::login_locked(addr.ip()) {
        logging::log("[Security]", &format!("❌ 面板登录已锁定（{}）", addr.ip()));
        return management_error(429, "登录失败次数过多，请 5 分钟后再试");
    }
    let payload = parse_body(&body).unwrap_or(serde_json::Value::Null);
    if let Err(response) = check_captcha(&payload) {
        return response;
    }
    let username = body_str(&payload, "username");
    let password = payload
        .get("password")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");

    if !access::verify_login(&username, password) {
        access::record_login_failure(addr.ip());
        logging::log("[Security]", &format!("❌ 面板登录失败（{}）", addr.ip()));
        return management_error(401, "账号或密码不正确");
    }
    access::clear_login_failures(addr.ip());
    logging::log("[Security]", &format!("✅ 面板登录成功（{}）", addr.ip()));
    issue_response(access::IssuedSession::new_session(), tokens_in_body(&headers))
}

/// `POST /api/panel/refresh` —— 用长效 refresh 轮换出新的双令牌。
///
/// 前端在 access 过期（401）后先静默调这里，成功则原请求重试、用户无感；
/// refresh 也失效（过期 / 重放检测触发）才真正跳登录页。
pub async fn panel_refresh(headers: HeaderMap) -> Response {
    let Some(session) = access::rotate_session(access::refresh_token_of(&headers)) else {
        return management_error(401, "登录已过期，请重新登录");
    };
    issue_response(session, tokens_in_body(&headers))
}

/// `POST /api/panel/logout` —— 撤销当前会话链（双 cookie 一并清除）。
pub async fn panel_logout(headers: HeaderMap) -> Response {
    access::revoke_session(access::refresh_token_of(&headers), &headers);
    let mut response = ok_json(serde_json::json!({ "loggedIn": false }));
    for name in [access::ACCESS_COOKIE, access::REFRESH_COOKIE] {
        let clear = format!("{name}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0");
        if let Ok(value) = axum::http::HeaderValue::from_str(&clear) {
            response.headers_mut().append(SET_COOKIE, value);
        }
    }
    response
}

fn management_error(status: i32, message: &str) -> Response {
    errors::management_error(status, message)
}
