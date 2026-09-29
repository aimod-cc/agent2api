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
use axum::extract::{ConnectInfo, Query};
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

fn tokens_in_body(
    headers: &HeaderMap,
    query: &std::collections::HashMap<String, String>,
) -> bool {
    // 显式标记（前端探测到 cookie 走不通后自己要求的），走**两条通道**：
    // 自定义请求头 + URL 参数 —— 有的中转剥自定义请求头，query 剥不掉
    // （登录请求能到服务器，它的 query 就完整）。
    let header_marked = headers
        .get(TOKEN_BODY_MODE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("body"));
    let query_marked = query
        .get("auth-mode")
        .map(|value| value.eq_ignore_ascii_case("body"))
        .unwrap_or(false);
    if header_marked || query_marked {
        return true;
    }
    // 探针 cookie 不在场 = 这个环境的 cookie 传输已被掐掉（宿主中转剥
    // Set-Cookie / Cookie 任一侧都会走到这里）。前端标记要穿过中转才能到
    // 服务器 —— 中转连自定义请求头一起剥时，标记就哑了；探针 cookie 的
    // 缺席是**服务器自己能看见**的同一事实，不依赖任何请求头穿过中转。
    // 直连环境探针 cookie 长效（90 天）恒在场，响应体保持与旧版逐字一致。
    access::cookie_value(headers, access::PROBE_COOKIE).is_none()
}

/// 判定依据的可读形态（登录日志用）：标记走了哪条通道。
fn marker_source(headers: &HeaderMap, query: &std::collections::HashMap<String, String>) -> &'static str {
    if headers
        .get(TOKEN_BODY_MODE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("body"))
    {
        "头"
    } else if query
        .get("auth-mode")
        .map(|value| value.eq_ignore_ascii_case("body"))
        .unwrap_or(false)
    {
        "参数"
    } else {
        "无"
    }
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
pub async fn cookie_probe(
    headers: HeaderMap,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Response {
    // ── 值比对：带回的必须是**刚发的那个值** ───────────────────
    // cookie 按主机算、不分端口（RFC 6265）——fnOS 网页（:5666）与面板
    // 直连（:3065）共用同一份 cookie 罐：用户先直连过一次，浏览器里就
    // 存下了一份长效探针；从 :5666 的中转进来时这份**旧探针**照样会上行。
    // 只判「有没有」会被它骗过（实测 2026-09-29：探针 cookie 在场、通道
    // 实际不通、弹回登录页）。所以第二发必须带 `expect=<刚发的值>`，
    // 服务器比对 cookie 值 —— 旧探针的值对不上，判 false。
    let received = access::cookie_value(&headers, access::PROBE_COOKIE);
    let expected = query
        .get("expect")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    if let (Some(received), Some(expected)) = (&received, &expected) {
        if *received == *expected {
            return ok_json(serde_json::json!({ "roundtrip": true }));
        }
        // 带了 expect 却对不上（旧探针 / Set-Cookie 被剥）：如实 false，
        // 不再补发 —— 补发会洗掉浏览器里那份旧 cookie，干扰下一轮判断
        return ok_json(serde_json::json!({ "roundtrip": false }));
    }
    // 第一发（无 expect）：发一枚新探针，值随响应体带回（匿名随机串，
    // 无会话语义；HttpOnly 本体 JS 读不到，走 body 才比对得了）
    let token = access::random_hex(16);
    // 长效（90 天）：它发下去之后是「这台浏览器的 cookie 通道完好」的
    // 持续标志 —— 直连环境的登录 / 刷新请求恒带着它，响应体与旧版逐字
    // 一致（见 `tokens_in_body`）。
    let cookie = format!(
        "{}={token}; Path=/; Max-Age={}; HttpOnly; SameSite=Lax",
        access::PROBE_COOKIE,
        90 * 24 * 3600
    );
    let mut response = ok_json(serde_json::json!({ "roundtrip": false, "token": token }));
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
    Query(query): Query<std::collections::HashMap<String, String>>,
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
            issue_response(access::IssuedSession::new_session(), tokens_in_body(&headers, &query))
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
    Query(query): Query<std::collections::HashMap<String, String>>,
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
    // 判定依据跟着日志走：中转环境下「令牌到底走没走响应体」是排查
    // 「登录成功却被弹回登录页」的第一个分叉点 —— 标记头有没有穿过来、
    // 探针 cookie 在不在场，一眼定位是中转剥头还是剥 cookie。
    let probe_present = access::cookie_value(&headers, access::PROBE_COOKIE).is_some();
    logging::log(
        "[Security]",
        &format!(
            "✅ 面板登录成功（{}）令牌进响应体={} 标记={} 探针cookie={}",
            addr.ip(),
            tokens_in_body(&headers, &query),
            marker_source(&headers, &query),
            if probe_present { "有" } else { "无" },
        ),
    );
    issue_response(access::IssuedSession::new_session(), tokens_in_body(&headers, &query))
}

/// `POST /api/panel/refresh` —— 用长效 refresh 轮换出新的双令牌。
///
/// 前端在 access 过期（401）后先静默调这里，成功则原请求重试、用户无感；
/// refresh 也失效（过期 / 重放检测触发）才真正跳登录页。
pub async fn panel_refresh(
    headers: HeaderMap,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let Some(session) = access::rotate_session(access::refresh_token_of(&headers)) else {
        return management_error(401, "登录已过期，请重新登录");
    };
    issue_response(session, tokens_in_body(&headers, &query))
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

#[cfg(test)]
mod tokens_in_body_tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderName, HeaderValue};
    use std::collections::HashMap;

    fn headers_with(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
    }

    fn query_with(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// 直连环境（探针 cookie 在场、无标记）：响应体与旧版逐字一致，
    /// 令牌只存在于 HttpOnly cookie，JS 读不到。
    #[test]
    fn a_direct_environment_keeps_tokens_out_of_the_body() {
        let headers = headers_with(&[
            ("cookie", "agent2api-panel-probe=abc; other=x"),
        ]);
        assert!(!tokens_in_body(&headers, &HashMap::new()));
    }

    /// 中转剥了 cookie（探针不在场）→ 令牌进响应体，**哪怕前端标记没穿过
    /// 来** —— 服务器自己能看见 cookie 缺席，不依赖任何请求头穿过中转。
    #[test]
    fn a_cookie_stripping_environment_gets_tokens_in_the_body() {
        let headers = headers_with(&[]);
        assert!(tokens_in_body(&headers, &HashMap::new()));
    }

    /// 前端标记走两条通道（头 + URL 参数）：中转剥自定义请求头时，
    /// query 上的标记照样生效 —— 登录请求能到服务器，query 就完整。
    #[test]
    fn the_marker_travels_by_header_or_by_query() {
        // 头标记（探针在场也不影响：显式标记恒生效）
        let headers = headers_with(&[
            ("x-panel-auth-mode", "body"),
            ("cookie", "agent2api-panel-probe=abc"),
        ]);
        assert!(tokens_in_body(&headers, &HashMap::new()));
        // 参数标记：头被剥了也认
        let headers = headers_with(&[("cookie", "agent2api-panel-probe=abc")]);
        assert!(tokens_in_body(
            &headers,
            &query_with(&[("auth-mode", "body")])
        ));
        // 认不出的标记值不算数
        assert!(!tokens_in_body(
            &headers,
            &query_with(&[("auth-mode", "cookie")])
        ));
    }
}
