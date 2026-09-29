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

/// 最近一次发出的探针（**两枚**值 + 时刻）——服务器端自持的判据。
///
/// ── 为什么前端配合还不够 ────────────────────────────────────
/// 实测（2026-09-29 两轮）：这个中转对 Set-Cookie / 自定义请求头 / URL
/// 参数的态度无法从服务器侧观测，且浏览器里长期留着早前轮次写下的 90 天
/// 旧探针 —— 页面版本旧、标记被剥、query 被剥，任何一种都会让「前端说了
/// 算」的判定失真。把判据收回服务器：登录 / 刷新时收到的探针 cookie 必须
/// **等于最近发出的那个值且足够新**，才证明「这条通道此刻真的能往返」；
/// 其余一切情况（缺席、旧值、对不上）一律令牌进响应体 —— 宁可多给，不可
/// 错判。直连环境的时序（登录页加载 → 几秒内登录）天然命中；探针超过
/// 窗口后令牌也会进响应体，功能无损（客户端两边都认）。
///
/// ── 为什么是两枚 ───────────────────────────────────────────
/// 登录响应下发的是两条 `Set-Cookie`（access + refresh），探针必须**同形**：
/// 只种一枚的中转会把探针判成「通道完好」，却在真正登录时把两条拼成一条
/// （属性 `Path=/` 因此降级成名为 `path` 的 cookie，新会话不按 `/` 落罐）——
/// 探针通过、会话死循环。两枚都往返才判通，缺一条就判中转。
static LAST_ISSUED_PROBE: std::sync::OnceLock<
    std::sync::Mutex<Option<(String, String, std::time::Instant)>>,
> = std::sync::OnceLock::new();
const PROBE_FRESH_WINDOW: std::time::Duration = std::time::Duration::from_secs(30 * 60);

fn remember_issued_probe(first: &str, second: &str) {
    let Ok(mut slot) = LAST_ISSUED_PROBE
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
    else {
        return;
    };
    *slot = Some((first.to_string(), second.to_string(), std::time::Instant::now()));
}

/// 收到的**两枚**探针 cookie 是否「当前有效」：每一枚都在场、各自等于最近发出的
/// 那个值、且在窗口内。同名多条（罐里有陈值）时按「期望值在不在其中」判，
/// 不看第一条 —— 与 `access::cookie_values` 的全量口径一致。
fn probe_is_currently_valid(first: &[String], second: &[String]) -> bool {
    let Ok(slot) = LAST_ISSUED_PROBE
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
    else {
        return false;
    };
    match &*slot {
        Some((expected_first, expected_second, at)) if at.elapsed() < PROBE_FRESH_WINDOW => {
            first.contains(expected_first) && second.contains(expected_second)
        }
        _ => false,
    }
}

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
    // 探针 cookie 必须是「本服务刚发的那两个值」（见 `probe_is_currently_valid`
    // 的说明）才算 cookie 通道可用 —— 缺席、旧值（早前轮次 / 同宿主其它端口
    // 写下的）、对不上、只回来一条，一律令牌进响应体。这是服务器端自持的判据，
    // 不依赖任何前端标记穿过中转，也不被同宿主旧探针骗过。
    !probe_is_currently_valid(
        &access::cookie_values(headers, access::PROBE_COOKIE),
        &access::cookie_values(headers, access::PROBE_COOKIE_SECOND),
    )
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

/// 探针两枚 cookie 的上行情况（登录日志用）：`两条都在` / `只有一条` / `都没有`。
///
/// 「只有一条」就是合并型中转的指纹 —— 单条 Set-Cookie 透传得动、两条就拼成一条。
/// 这一列出现的意义是把「为什么这次令牌进了响应体」写在同一行里，不必再去翻
/// 探针端点的响应。
fn probe_shape(headers: &HeaderMap) -> &'static str {
    let first = !access::cookie_values(headers, access::PROBE_COOKIE).is_empty();
    let second = !access::cookie_values(headers, access::PROBE_COOKIE_SECOND).is_empty();
    match (first, second) {
        (true, true) => "两条都在",
        (true, false) | (false, true) => "只有一条",
        (false, false) => "都没有",
    }
}

/// `ANY /api/panel/cookie-probe` —— 会话传输探测（登录页加载时连打两发）。
///
/// 挂 public：探测发生在登录之前。第一发**没有**探针 cookie：种**两枚** 90 天
/// 寿命的探针 cookie（`PROBE_COOKIE` + `PROBE_COOKIE_SECOND`，与登录响应的两条
/// `Set-Cookie` 同形）并回 `roundtrip:false`；第二发浏览器若两条都存得下、发
/// 得回且各自值对得上，服务端回 `roundtrip:true` —— 本环境的 cookie 传输（含
/// 多条 Set-Cookie 下发）完好，登录响应不需要令牌进 body。第二发仍 `false`
/// （中转剥 Set-Cookie / 拼成一条 / 浏览器拒存第三方 cookie / 隐私模式）＝ cookie
/// 走不通，前端才带 `x-panel-auth-mode: body` 登录。测的是环境的真实传输，
/// 不猜代理行为：哪天中转把 cookie 修好了，直连形态自动回归。
pub async fn cookie_probe(
    headers: HeaderMap,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Response {
    // ── 值比对：带回的必须是**刚发的那两个值** ───────────────────
    // cookie 按主机算、不分端口（RFC 6265）——fnOS 网页（:5666）与面板
    // 直连（:3065）共用同一份 cookie 罐：用户先直连过一次，浏览器里就
    // 存下了一份长效探针；从 :5666 的中转进来时这份**旧探针**照样会上行。
    // 只判「有没有」会被它骗过（实测 2026-09-29：探针 cookie 在场、通道
    // 实际不通、弹回登录页）。所以第二发必须带 `expect=` / `expect-b=`，
    // 服务器比对两个 cookie 值 —— 旧探针的值对不上，判 false。
    let received_first = access::cookie_values(&headers, access::PROBE_COOKIE);
    let received_second = access::cookie_values(&headers, access::PROBE_COOKIE_SECOND);
    let expected_first = non_empty(query.get("expect"));
    let expected_second = non_empty(query.get("expect-b"));
    if expected_first.is_some() || expected_second.is_some() {
        // 第二发（带 expect）：两枚都必须对得上才算通。**只回一个 expect 的
        // 是旧版页面缓存**（单枚探针那版），它证明不了两条 Set-Cookie 的命运
        // —— 按不通处理，宁可用响应体令牌这条更宽的路。
        let roundtrip = match (&expected_first, &expected_second) {
            (Some(first), Some(second)) => {
                received_first.iter().any(|value| value == first)
                    && received_second.iter().any(|value| value == second)
            }
            _ => false,
        };
        // 不再补发：补发会洗掉浏览器里那份旧 cookie，干扰下一轮判断
        return ok_json(serde_json::json!({ "roundtrip": roundtrip }));
    }
    // 第一发（无 expect）：发两枚新探针，值随响应体带回（匿名随机串，
    // 无会话语义；HttpOnly 本体 JS 读不到，走 body 才比对得了）
    let first = access::random_hex(16);
    let second = access::random_hex(16);
    // 长效（90 天）：它们发下去之后是「这台浏览器的 cookie 通道完好（连两条
    // 都透传）」的持续标志 —— 直连环境的登录 / 刷新请求恒带着它们，响应体与
    // 旧版逐字一致（见 `tokens_in_body`）。
    let cookies = [
        format!(
            "{}={first}; Path=/; Max-Age={}; HttpOnly; SameSite=Lax",
            access::PROBE_COOKIE,
            90 * 24 * 3600
        ),
        format!(
            "{}={second}; Path=/; Max-Age={}; HttpOnly; SameSite=Lax",
            access::PROBE_COOKIE_SECOND,
            90 * 24 * 3600
        ),
    ];
    remember_issued_probe(&first, &second);
    let mut response = ok_json(serde_json::json!({
        "roundtrip": false,
        "token": first,
        "tokenB": second,
    }));
    for cookie in cookies {
        if let Ok(value) = axum::http::HeaderValue::from_str(&cookie) {
            response.headers_mut().append(SET_COOKIE, value);
        }
    }
    response
}

fn non_empty(value: Option<&String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
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
    // 两枚探针各回来了几条，一眼定位是中转剥头还是拼 cookie。
    let body_mode = tokens_in_body(&headers, &query);
    let session = access::IssuedSession::new_session();
    // 指纹是这条日志的全部意义：下一个请求的「面板会话无效」行会打印它**收到的**
    // access cookie 指纹，两行一比对就知道是"新 cookie 没存上"还是"存上了但
    // 服务端不认"——只看 cookie 名单分不出这两种（它们都显示"带 access cookie"）。
    logging::log(
        "[Security]",
        &format!(
            "✅ 面板登录成功（{}）令牌进响应体={} 标记={} 探针={} 本次会话指纹={}",
            addr.ip(),
            body_mode,
            marker_source(&headers, &query),
            probe_shape(&headers),
            access::access_fingerprint(&session),
        ),
    );
    issue_response(session, body_mode)
}

/// `POST /api/panel/refresh` —— 用长效 refresh 轮换出新的双令牌。
///
/// 前端在 access 过期（401）后先静默调这里，成功则原请求重试、用户无感；
/// refresh 也失效（过期 / 重放检测触发）才真正跳登录页。
pub async fn panel_refresh(
    headers: HeaderMap,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let Some(session) = access::rotate_session(&headers) else {
        return management_error(401, "登录已过期，请重新登录");
    };
    issue_response(session, tokens_in_body(&headers, &query))
}

/// `POST /api/panel/logout` —— 撤销当前会话链（双 cookie 一并清除）。
pub async fn panel_logout(headers: HeaderMap) -> Response {
    access::revoke_session(&headers);
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

    /// 探针槽位（`LAST_ISSUED_PROBE`）是进程级的，本模块每条测试都会写它 ——
    /// 并行跑会互踩（A 刚记住的值被 B 覆盖，A 的有效性判据就凭空变 false）。
    /// 用这把锁把「写槽位 + 判槽位」的那几段串起来；跨 `.await` 持有是安全的：
    /// `#[tokio::test]` 每条测试各自一个 current_thread 运行时，只会让**另一个
    /// 线程**上的测试阻塞等待，不存在同线程重入。
    static PROBE_SLOT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn hold_probe_slot() -> std::sync::MutexGuard<'static, ()> {
        PROBE_SLOT_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

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

    /// 两枚探针 cookie 的上行文本。
    fn probe_cookies(first: &str, second: &str) -> String {
        format!("{PROBE}={first}; {SECOND}={second}")
    }

    // 别名而非重新字面量化：探针名字必须与 `access` 里那两位常量同源，
    // 否则改了名字测试还全绿（生产的探针 cookie 就再也没人对得上）。
    const PROBE: &str = access::PROBE_COOKIE;
    const SECOND: &str = access::PROBE_COOKIE_SECOND;

    /// 判据是**两枚都要对得上**（2026-09-29 那台中转的现场）：
    ///   · 两条都在且各自等于刚发的值 → cookie 通道真的通了（含多条 Set-Cookie
    ///     透传），响应体与旧版逐字一致（令牌只在 HttpOnly cookie，JS 读不到）；
    ///   · 只回来一条 → 中转把两条拼成了一条（`Path=/` 降级成名为 `path` 的
    ///     cookie，会话落不进罐）→ 令牌进响应体。**这一格就是旧实现的漏洞**：
    ///     单枚探针那版在这里判的是「通」。
    ///   · 值是同宿主其它来源写下的旧值（cookie 不分端口，fnOS :5666 中转与
    ///     :3065 直连共用一份罐）→ 同样判不通。
    #[test]
    fn both_probe_cookies_must_match_the_recently_issued_values() {
        let _serial = hold_probe_slot();
        remember_issued_probe("fresh-a", "fresh-b");

        let both = headers_with(&[("cookie", probe_cookies("fresh-a", "fresh-b").as_str())]);
        assert!(
            !tokens_in_body(&both, &HashMap::new()),
            "两条探针都对得上时不许把令牌塞进响应体（直连环境的原样行为）"
        );

        // 只回来一枚：合并型中转的指纹
        for only in [
            format!("{PROBE}=fresh-a"),
            format!("{SECOND}=fresh-b"),
        ] {
            let headers = headers_with(&[("cookie", only.as_str())]);
            assert!(
                tokens_in_body(&headers, &HashMap::new()),
                "缺另一枚探针（{only}）必须判不通 —— 单枚探针被骗过的那次弹回就是这么来的"
            );
        }

        // 两条都在，但第二枚是陈值
        let half_stale = headers_with(&[(
            "cookie",
            probe_cookies("fresh-a", "stale-b").as_str(),
        )]);
        assert!(tokens_in_body(&half_stale, &HashMap::new()));

        // 同名多条（罐里留着上一轮的残本）：期望值在其中就算对得上，
        // 与 `access::cookie_values` 的全量口径一致
        let repeated = headers_with(&[(
            "cookie",
            format!("{}=old-a; {PROBE}=fresh-a; {SECOND}=fresh-b", PROBE).as_str(),
        )]);
        assert!(!tokens_in_body(&repeated, &HashMap::new()));
    }

    /// 中转剥了 cookie（探针不在场）→ 令牌进响应体，**哪怕前端标记没穿过
    /// 来** —— 服务器自己能看见 cookie 缺席，不依赖任何请求头穿过中转。
    #[test]
    fn a_cookie_stripping_environment_gets_tokens_in_the_body() {
        let _serial = hold_probe_slot();
        remember_issued_probe("a", "b");
        let headers = headers_with(&[]);
        assert!(tokens_in_body(&headers, &HashMap::new()));
    }

    /// 前端标记走两条通道（头 + URL 参数）：中转剥自定义请求头时，
    /// query 上的标记照样生效 —— 登录请求能到服务器，query 就完整。
    #[test]
    fn the_marker_travels_by_header_or_by_query() {
        let _serial = hold_probe_slot();
        remember_issued_probe("fresh-a", "fresh-b");
        let pair = probe_cookies("fresh-a", "fresh-b");

        // 显式标记**优先于**「探针说通道完好」：前端明确要求走响应体时，
        // 即便两枚探针都对得上也把令牌放进去（两侧客户端都认，功能无损）。
        let headers = headers_with(&[
            ("x-panel-auth-mode", "body"),
            ("cookie", pair.as_str()),
        ]);
        assert!(tokens_in_body(&headers, &HashMap::new()));
        // 参数标记：头被剥了也认
        let headers = headers_with(&[("cookie", pair.as_str())]);
        assert!(tokens_in_body(
            &headers,
            &query_with(&[("auth-mode", "body")])
        ));
        // 认不出的标记值不算数：落到探针有效性判据上 —— 探针都对得上，
        // 于是这里判的是「走 cookie」（与上一条互为对照，证明那个标记
        // 没被当成"任何值都算 body"）
        assert!(
            !tokens_in_body(&headers, &query_with(&[("auth-mode", "cookie")])),
            "未知取值的标记不该改变判据，应回落到探针结论"
        );
        // 探针值对不上时，认不出的标记也不影响「令牌进响应体」这个安全默认
        let stale_pair = probe_cookies("nope-a", "nope-b");
        let headers = headers_with(&[("cookie", stale_pair.as_str())]);
        assert!(tokens_in_body(
            &headers,
            &query_with(&[("auth-mode", "cookie")])
        ));
    }

    /// 探针端点本身的两发（**逐字节检查响应头**：种了几条 Set-Cookie 是这个
    /// 改动的全部立意，只测判据函数测不到它）。
    #[tokio::test]
    // 跨 await 持锁是**这里**故意的：`#[tokio::test]` 每条测试一个 current_thread
    // 运行时，守卫只会让另一个线程上的测试阻塞等待槽位，不存在同线程重入死锁；
    // 换成 async Mutex 反而要让整条测试变成 await 密集型，为一把测试用的锁不值。
    #[allow(clippy::await_holding_lock)]
    async fn the_probe_endpoint_plants_two_cookies_and_needs_both_back() {
        let _serial = hold_probe_slot();

        // 第一发（没有任何探针 cookie）：两条 Set-Cookie + 两个值进响应体
        let response = cookie_probe(HeaderMap::new(), Query(HashMap::new())).await;
        let planted: Vec<String> = response
            .headers()
            .get_all(SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok().map(str::to_string))
            .collect();
        assert_eq!(2, planted.len(), "探针必须下发**两条** Set-Cookie，与登录响应同形");
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("探针响应体可读");
        let payload: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(false, payload["data"]["roundtrip"]);
        let first = payload["data"]["token"].as_str().unwrap().to_string();
        let second = payload["data"]["tokenB"].as_str().unwrap().to_string();
        assert!(
            planted
                .iter()
                .any(|cookie| cookie.starts_with(&format!("{PROBE}={first};"))),
            "第一枚探针的名字与值要下发的是它自己"
        );
        assert!(
            planted
                .iter()
                .any(|cookie| cookie.starts_with(&format!("{SECOND}={second};"))),
            "第二枚探针（那条 -b）缺席就等于回到单枚探针的老毛病"
        );

        // 第二发（直连环境：两条都回来了）→ roundtrip true
        let headers = headers_with(&[("cookie", probe_cookies(&first, &second).as_str())]);
        let response = cookie_probe(
            headers,
            Query(query_with(&[("expect", &first), ("expect-b", &second)])),
        )
        .await;
        let payload: serde_json::Value = payload_of(response).await;
        assert_eq!(true, payload["data"]["roundtrip"]);

        // 第二发（合并型中转：只有第一条回来）→ false —— 这正是旧版探针
        // 判成 true、随后登录弹回的那一格
        let headers = headers_with(&[("cookie", format!("{PROBE}={first}").as_str())]);
        let response = cookie_probe(
            headers,
            Query(query_with(&[("expect", &first), ("expect-b", &second)])),
        )
        .await;
        let payload: serde_json::Value = payload_of(response).await;
        assert_eq!(
            false, payload["data"]["roundtrip"],
            "缺第二枚探针时必须判不通（令牌随后要走响应体那条更宽的路）"
        );

        // 第二发（旧版页面只发 expect）→ 也判不通：它证明不了两条的命运，
        // 而服务端无法区分"旧页面"与"页面新但另一枚被剥"，两者都该退到 body 模式
        let headers = headers_with(&[("cookie", probe_cookies(&first, &second).as_str())]);
        let response = cookie_probe(headers, Query(query_with(&[("expect", &first)]))).await;
        let payload: serde_json::Value = payload_of(response).await;
        assert_eq!(false, payload["data"]["roundtrip"]);
    }

    async fn payload_of(response: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("探针响应体可读");
        serde_json::from_slice(&bytes).unwrap()
    }

    /// 直连环境的**静默续期**不许把裸令牌吐进响应体（2026-09-29 复核时发现的
    /// 一条既有缺陷）：`web_shim` 早先把 `?auth-mode=body` 写成无条件发送，于是
    /// 每两小时一次续期都会让令牌变成 JS 可读、并被落进 localStorage ——
    /// 「直连环境令牌只在 HttpOnly cookie 里」这条性质在一次续期之后就失效了。
    /// 客户端改成只在本地存过令牌时才发标记；这一条钉住服务端那一半：
    /// **没有标记 + 两枚探针都对得上 = cookie 模式 = 响应体里没有令牌**，
    /// 而新令牌仍随两条 `Set-Cookie` 下发（会话照续）。
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn a_direct_session_refreshes_without_putting_tokens_in_the_body() {
        let _serial = hold_probe_slot();
        remember_issued_probe("direct-a", "direct-b");
        let session = access::IssuedSession::new_session();
        let cookie = format!(
            "{PROBE}=direct-a; {SECOND}=direct-b; {}={}",
            access::REFRESH_COOKIE,
            session.refresh_token
        );

        let response = panel_refresh(
            headers_with(&[("cookie", cookie.as_str())]),
            Query(HashMap::new()),
        )
        .await;
        assert!(response.status().is_success(), "cookie 模式的续期应成功");
        let set_cookies: Vec<String> = response
            .headers()
            .get_all(SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok().map(str::to_string))
            .collect();
        let payload = payload_of(response).await;
        assert!(
            payload["data"]["accessToken"].is_null(),
            "直连续期的响应体不许带访问令牌，实得 {:?}",
            payload["data"]
        );
        assert!(payload["data"]["refreshToken"].is_null());
        assert_eq!(2, set_cookies.len(), "新令牌应改为随两条 Set-Cookie 下发");

        // 对照：同一个（已轮换过的）链，客户端**带标记**时令牌必须进响应体 ——
        // 少了这一格，上面的"没有令牌"就可能是「续期根本没发新令牌」
        let rotated_refresh = set_cookies
            .iter()
            .filter_map(|value| value.split(';').next())
            .find_map(|pair| pair.strip_prefix(&format!("{}=", access::REFRESH_COOKIE)))
            .map(str::to_string)
            .expect("Set-Cookie 里应有新的 refresh");
        let marked = format!(
            "{PROBE}=direct-a; {SECOND}=direct-b; {}={rotated_refresh}",
            access::REFRESH_COOKIE
        );
        let response = panel_refresh(
            headers_with(&[
                ("cookie", marked.as_str()),
                ("x-panel-auth-mode", "body"),
            ]),
            Query(HashMap::new()),
        )
        .await;
        assert!(response.status().is_success());
        let payload = payload_of(response).await;
        assert!(
            payload["data"]["accessToken"]
                .as_str()
                .is_some_and(|value| !value.is_empty()),
            "显式标记必须让令牌进响应体（中转环境的唯一通路）"
        );
    }
}
