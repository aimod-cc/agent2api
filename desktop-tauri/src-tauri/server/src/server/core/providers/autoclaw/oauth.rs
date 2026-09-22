//! AutoClaw **国际版**的 OAuth 网页登录（Zai / Google）—— 逆向自桌面端
//! `app.asar`，端点与请求形状逐条实测确认（2026-09-22）。
//!
//! ── 这条链为什么必须存在（此前是一个已知缺口）─────────────────
//! 国际版客户端的官方主登录方式就是这两个 OAuth（它的登录页**只**渲染
//! Zai / Google 两个按钮，手机验证码表单被死代码消除；见 `login.rs` 模块头）。
//! 此前网关只能走手机验证码 —— 那对「用 Zai / Google 账号注册、没绑手机号」
//! 的用户等于没有登录入口，只能靠「导入桌面端登录态」绕。
//!
//! ── 协议（三步，实测）───────────────────────────────────────
//! ```text
//! ① POST {intl}/userapi/overseasv1/oauth-captcha-config   {}
//!      → data {enabled, region, prefix, scene_id, captcha_supplier}
//!      ★ 这就是「这个构建要不要走 OAuth」的开关：国内版返回 enabled:false
//!        且 prefix/scene_id 为空串；国际版返回 enabled:true + 阿里云场景参数。
//! ② POST {intl}/userapi/overseasv1/{zai|google}-oauth-url
//!      {source_id, device_id, navigate_uri, ali_captcha_verify_param}
//!      → data.oauth_url（官方登录页）
//! ③ 浏览器在登录页完成登录 → 302 到 {navigate_uri}?code=…&state=…
//!    POST {intl}/userapi/overseasv1/{zai|google}-oauth-login
//!      {source_id, device_id, code, state, navigate_uri}
//!      → data {access_token, refresh_token, user_id, user_name, first_login}
//! ```
//!
//! ── 卡点：第 ② 步**强制要求阿里云风控验证码**（本次解决）──────────
//! 不带 `ali_captcha_verify_param` 得到 `631002 当前版本已停止服务`
//! （一个与真实原因无关的误导性错误码 —— 换任何 `X-Version` 都是这个码，
//! 实测 1.18.5 / 1.19.0 / 1.20.0 / 2.0.0 一致）；带一个假值则得到
//! `630014 抱歉,审核失败`。参数校验的顺序也实测过：**先验验证码、后验
//! navigate_uri** —— 传一个完全不合法的 `not-a-url` 仍然只回 630014，
//! 说明验证码这一关在前面，绕不过去。
//!
//! 验证码是**浏览器端 JS SDK**（`o.alicdn.com` 的 `AliyunCaptcha.js`），
//! 逆向它的参数生成算法成本极高且会随上游更新失效。因此这里**不复刻算法**，
//! 而是把官方那套 SDK 原样搬到**主窗口**里跑（主窗口 `csp: null`，没有 CSP
//! 拦截，SDK 可以原封不动地工作）—— 见前端 `ui/autoclaw-oauth.js`，那份代码
//! 是从客户端 `chatStore-*.js` 的验证码实现逐条移植的。
//!
//! 于是本模块只做**服务端到服务端**的那两跳（② 与 ③），前端拿到
//! `ali_captcha_verify_param` 后把它交给 `/api/session/login/oauth/start`。
//!
//! ── 第 ③ 步**不需要**验证码（实测，这条决定了架构）─────────────
//! 用一个假 code 打 `zai-oauth-login` 得到 `631001 User login error`
//! （而不是 630014）—— 说明换码这一跳不要求风控参数，可以完全由网关完成。
//! 因此整条链是「**前端过一次验证码 → 网关拿 URL → 浏览器登录 → 网关换码**」，
//! 网关侧没有浏览器依赖。
//!
//! ── navigate_uri 为什么可以用本网关的 loopback 地址 ─────────────
//! 实测上游对 `navigate_uri` **只做非空校验、不校验形态也不校验主机**：
//! 传 `not-a-url` / `ftp://x/y` / `http://127.0.0.1:1/a` /
//! `https://evil.example.com/...` 得到的都是同一个 630014（缺验证码），
//! 没有一个被更早地拒掉。官方客户端的端口也是**运行时挑的**
//! （`ALL_PORTS` 里第一个能 listen 的，实测就是随机量级），所以「端口必须
//! 是某个固定值」这条约束并不存在。
//!
//! 于是回调可以直接挂在本网关自己的监听端口上（与 CatPaw 同一手法，见
//! `core::login::catpaw` 的模块头）：省掉一个临时监听器，也省掉「临时端口
//! 被占用 / 忘了关」这类故障面。回调地址把 state 放在**路径**里而不是查询串：
//! 上游会在 `navigate_uri` 后面拼 `?code=…&state=…`，路径里带 state 就不必
//! 猜上游是追加 `?` 还是 `&`。
//!
//! ── 安全 ────────────────────────────────────────────────────
//! 回调落在本机 HTTP 端口上，任何本机进程都能伪造一次 GET。安全由**一次性
//! state** 承担：它只在本进程内生成、与登录任务一一对应、回调时逐字比对
//! （`core::login` 的 `finish_autoclaw_oauth_callback`）。与 CatPaw 的
//! loopback 回调同一威胁模型、同一处置。token 只进账号库，不进日志。

use serde_json::{json, Value};

use crate::server::core::auth_http::send_raw;
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::login::new_device_id;
use super::refresh::signed_auth_headers;
use super::region::Region;

/// 验证码配置（同时是「这个构建要不要走 OAuth」的开关）
const CAPTCHA_CONFIG_PATH: &str = "/userapi/overseasv1/oauth-captcha-config";

/// 请求超时：与客户端同档（`OVERSEA_OAUTH_*_TIMEOUT_MS` = 15 秒）。
/// 这两跳都要过一次风控，给得比普通 userapi 调用宽一点。
const REQUEST_TIMEOUT_MS: u64 = 15_000;

/// OAuth 变体（上游按变体分两个端点，字段与流程完全一致）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Vendor {
    /// Zai（智谱自家账号体系）
    Zai,
    /// Google
    Google,
}

impl Vendor {
    /// 两个变体（顺序 = 界面上按钮的顺序：Zai 在前，与客户端一致）
    pub const ALL: [Vendor; 2] = [Vendor::Zai, Vendor::Google];

    /// 请求体 / 回调路径里用的标识（`zai` / `google`）
    pub fn id(self) -> &'static str {
        match self {
            Self::Zai => "zai",
            Self::Google => "google",
        }
    }

    /// 解析回调路径里的标识（不认识返回 None —— 不静默回落到某一个变体）
    pub fn from_id(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|vendor| vendor.id() == value)
    }

    /// 界面上那个按钮的名字
    pub fn label(self) -> &'static str {
        match self {
            Self::Zai => "Zai",
            Self::Google => "Google",
        }
    }

    /// 取授权地址的端点
    fn url_path(self) -> &'static str {
        match self {
            Self::Zai => "/userapi/overseasv1/zai-oauth-url",
            Self::Google => "/userapi/overseasv1/google-oauth-url",
        }
    }

    /// 用授权码换凭证的端点
    fn login_path(self) -> &'static str {
        match self {
            Self::Zai => "/userapi/overseasv1/zai-oauth-login",
            Self::Google => "/userapi/overseasv1/google-oauth-login",
        }
    }
}

/// 回调路径的前缀（**本网关自己的**路由，不是上游的）。
///
/// 形如 `/api/session/login/autoclaw-oauth-callback/{vendor}/{state}`：
/// 变体与 state 都放在路径里，回调处理函数因此不需要再查任何映射表 ——
/// 「这次回调属于哪个变体」这个事实由 URL 自己带着（它是我们当初交给上游的
/// 那个 `navigate_uri` 的一部分，绕一圈回来必须原样）。
///
/// 与 `catpaw::CALLBACK_PATH` 同一手法，但多两段路径参数：CatPaw 的 state
/// 在**表单 body** 里（上游主动 POST），这条是**浏览器 302**（查询串里只有
/// code/state，而 state 已用于任务比对，变体只能另找地方放）。
pub const CALLBACK_PATH_PREFIX: &str = "/api/session/login/autoclaw-oauth-callback";

/// 拼这次登录的回调地址（同时也是交给上游的 `navigate_uri`）。
///
/// `callback_base` 是本网关自己的 loopback 基址（`http://127.0.0.1:<port>`），
/// 由调用方给出 —— 本模块拿不到监听端口（它在 `ServerState` 上）。
pub fn navigate_uri(callback_base: &str, vendor: Vendor, state: &str) -> String {
    format!(
        "{}{}/{}/{}",
        callback_base.trim_end_matches('/'),
        CALLBACK_PATH_PREFIX,
        vendor.id(),
        state
    )
}

/// 带签名的 userapi POST（登录链路上还没有 token，因此走匿名形态）。
///
/// 与 `login.rs` 的同名函数是同一套签名与超时；不复用是因为那条链路把
/// 「业务码 → 人话」的翻译写死在自己的码表里，两处的码表不同（这条会见到
/// 风控码 630014，那条不会）。
async fn post_signed(
    region: Region,
    path: &str,
    body: &Value,
    what: &str,
) -> Result<Value, GatewayError> {
    let url = format!("{}{path}", super::credentials::userapi_base_url(region));
    let headers = signed_auth_headers("");
    let response = send_raw(
        "POST",
        &url,
        Some(body),
        &headers,
        None,
        Some(REQUEST_TIMEOUT_MS),
    )
    .await
    .map_err(|error| {
        if error.is_timeout() {
            GatewayError::with_status(504, format!("{what}超时"))
        } else {
            GatewayError::with_status(502, format!("{what}失败: {error}"))
        }
    })?;
    if !response.ok {
        return Err(GatewayError::with_status(
            response.status as i32,
            format!("{what}返回 HTTP {}", response.status),
        ));
    }
    Ok(response.payload.unwrap_or(Value::Null))
}

/// 业务码 → 用户能看懂的原因（OAuth 这条链自己的码表）。
///
/// ── 为什么 631002 必须改写（别改回直译）──────────────────────
/// 它的原文是「当前版本已停止服务，请前往官网下载最新版」。对网关用户来说
/// 这句话是**错的**：它不是版本问题，而是**缺少风控验证**（实测换任何
/// `X-Version` 都是这个码）。直译会把用户引去「升级客户端」这个死方向，
/// 而真正该做的是「重新过一次验证码」。
///
/// 630014 是同一族的另一半：带了验证码参数但没通过。两条都归到「风控没过」。
fn describe_oauth_error(code: i64, upstream_msg: &str) -> String {
    match code {
        631_002 => {
            "本次登录缺少风控验证（验证码未通过或已过期），请重新点击登录再试一次".to_string()
        }
        630_014 => "风控验证未通过，请重新点击登录再试一次".to_string(),
        // 换码这一跳的失败：授权码一次性 / 已过期 / 与 state 对不上。
        // 最常见的原因是「回调被重复处理」或「用户在上游页面停留太久」。
        631_001 => "授权码无效或已过期，请重新点击登录".to_string(),
        400_001 => "请求参数有误（登录链路内部错误，请重试）".to_string(),
        400_002 => "请求签名校验失败（本机时钟可能有偏差），请校准系统时间后重试".to_string(),
        _ => {
            if upstream_msg.trim().is_empty() {
                format!("登录失败（上游 code={code}）")
            } else {
                format!("登录失败：{}", upstream_msg.trim())
            }
        }
    }
}

/// 取验证码配置（前端据此初始化阿里云 SDK）。
///
/// 返回归一化后的形状 `{enabled, region, prefix, sceneId, supplier}`：
/// 上游用 snake_case（`scene_id` / `captcha_supplier`），前端 camelCase，
/// 在**这一处**翻译完，前端就不必认识两套命名。
///
/// `enabled: false` **不是错误**：国内版就是这个值（它没有 OAuth）。调用方
/// 据此告诉用户「这一家没有这条登录方式」，而不是报一条失败。
pub async fn captcha_config(region: Region) -> Result<Value, GatewayError> {
    let payload = post_signed(region, CAPTCHA_CONFIG_PATH, &json!({}), "获取风控验证配置").await?;
    let code = payload.get("code").and_then(Value::as_i64).unwrap_or(0);
    if code != 0 {
        let upstream_msg = payload.get("msg").and_then(Value::as_str).unwrap_or("");
        return Err(GatewayError::with_status(
            502,
            describe_oauth_error(code, upstream_msg),
        ));
    }
    let data = payload.get("data").cloned().unwrap_or(Value::Null);
    let text = |key: &str| {
        data.get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string()
    };
    Ok(json!({
        "enabled": data.get("enabled").and_then(Value::as_bool).unwrap_or(false),
        "region": text("region"),
        "prefix": text("prefix"),
        "sceneId": text("scene_id"),
        "supplier": text("captcha_supplier"),
    }))
}

/// 取 OAuth 授权地址（**需要验证码参数**，见模块头）。
///
/// `captcha_verify_param` 是前端跑完阿里云 SDK 得到的那个不透明字符串
/// （客户端叫 `captchaVerifyParam`，上游字段叫 `ali_captcha_verify_param`），
/// 网关**不解析它**，只原样转交 —— 它的格式是阿里云与上游之间的契约。
pub async fn request_oauth_url(
    region: Region,
    vendor: Vendor,
    navigate_uri: &str,
    captcha_verify_param: &str,
    device_id: &str,
) -> Result<String, GatewayError> {
    let captcha = captcha_verify_param.trim();
    if captcha.is_empty() {
        // 本地就挡掉：不带它打上游必然得到那条误导性的 631002，
        // 不如在这里说清「验证码这一步没完成」
        return Err(GatewayError::with_status(
            400,
            "缺少风控验证参数，请先完成验证码",
        ));
    }
    let body = json!({
        "source_id": "autoclaw",
        "device_id": device_id,
        "navigate_uri": navigate_uri,
        "ali_captcha_verify_param": captcha,
    });
    let what = format!("获取 {} 授权地址", vendor.label());
    let payload = post_signed(region, vendor.url_path(), &body, &what).await?;
    let code = payload.get("code").and_then(Value::as_i64).unwrap_or(0);
    if code != 0 {
        let upstream_msg = payload.get("msg").and_then(Value::as_str).unwrap_or("");
        let message = describe_oauth_error(code, upstream_msg);
        logging::log(
            "[Login]",
            &format!(
                "❌ AutoClaw {} 授权地址获取失败（code={code}，上游原文「{upstream_msg}」）: {message}",
                vendor.label()
            ),
        );
        return Err(GatewayError::with_status(400, message));
    }
    let oauth_url = payload
        .get("data")
        .and_then(|data| data.get("oauth_url"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if oauth_url.is_empty() {
        return Err(GatewayError::with_status(
            502,
            "上游没有返回授权地址，请重试",
        ));
    }
    Ok(oauth_url)
}

/// 用回调里的一次性授权码换凭证，返回可直接交给 `add_autoclaw_account` 的
/// payload（形状与手机验证码登录那条**逐字一致**：`{token, refreshToken,
/// deviceId}`）。
///
/// ── 为什么两跳要传同一个 device_id ──────────────────────────
/// 客户端的 `device_id` 来自它本机持久化的设备标识，`oauth-url` 与
/// `oauth-login` 两跳用的是**同一个值**。上游有没有把它当会话键没实测到
/// （走完一次真实验证码才能验），但复刻客户端的做法是零成本且无风险的 ——
/// 因此 `device_id` 由调用方（登录任务）持有并在两跳间沿用。
///
/// `navigate_uri` 必须与第 ② 步**逐字相同**（客户端也是这么传的：
/// 回调处理时重新调一次 `getZaiCallbackUri()`，值不变）。
pub async fn exchange_code(
    region: Region,
    vendor: Vendor,
    code: &str,
    state: &str,
    navigate_uri: &str,
    device_id: &str,
) -> Result<Value, GatewayError> {
    let body = json!({
        "source_id": "autoclaw",
        "device_id": device_id,
        "code": code,
        "state": state,
        "navigate_uri": navigate_uri,
    });
    let what = format!("{} 登录换取凭证", vendor.label());
    let payload = post_signed(region, vendor.login_path(), &body, &what).await?;
    let status = payload.get("code").and_then(Value::as_i64).unwrap_or(0);
    if status != 0 {
        let upstream_msg = payload.get("msg").and_then(Value::as_str).unwrap_or("");
        let message = describe_oauth_error(status, upstream_msg);
        logging::log(
            "[Login]",
            &format!(
                "❌ AutoClaw {} 换取凭证失败（code={status}，上游原文「{upstream_msg}」）: {message}",
                vendor.label()
            ),
        );
        return Err(GatewayError::with_status(400, message));
    }
    let data = payload.get("data").cloned().unwrap_or(Value::Null);
    let token = data
        .get("access_token")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if token.is_empty() {
        logging::log(
            "[Login]",
            &format!("❌ AutoClaw {} 登录成功但上游未返回 access_token", vendor.label()),
        );
        return Err(GatewayError::with_status(
            502,
            "登录成功但上游没有返回 access_token",
        ));
    }
    let refresh_token = data
        .get("refresh_token")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let mut credentials = json!({ "token": token, "deviceId": device_id });
    if let Some(object) = credentials.as_object_mut() {
        if !refresh_token.is_empty() {
            object.insert("refreshToken".to_string(), Value::String(refresh_token));
        }
        // 备注名兜底：OAuth 账号有用户名（手机验证码那条没有，用脱敏手机号）。
        // 用户仍可在添加时自己填，这里只提供一个不像乱码的默认值。
        let user_name = data
            .get("user_name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if !user_name.is_empty() {
            object.insert("userName".to_string(), Value::String(user_name));
        }
    }
    Ok(credentials)
}

/// 生成一个设备 id（与 `login.rs` 同一形态：32 字节随机 → 64 位十六进制）。
///
/// 转发一层而不是让调用方各自 import `login::new_device_id`：那个函数是
/// 「手机验证码链路的设备 id」，这里是「OAuth 链路的设备 id」，两处一旦将来
/// 需要不同的形态（比如上游对 OAuth 要求 ed25519 指纹），改这里一处即可。
pub fn new_oauth_device_id() -> String {
    new_device_id()
}
