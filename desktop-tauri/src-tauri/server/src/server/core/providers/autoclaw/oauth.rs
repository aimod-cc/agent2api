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
//! ── navigate_uri 为什么必须长成客户端那样（本节两处实测，别混）─
//! 上游对 `navigate_uri` 的校验发生在**两个阶段**，口径完全不同：
//!
//!   - **第 ② 步（`oauth-url` 接口）**：只做非空校验，不校验形态也不校验主机。
//!     传 `not-a-url` / `ftp://x/y` / `http://127.0.0.1:1/a` 得到的都是同一个
//!     630014（缺验证码），没有一个被更早地拒掉 —— 因此**这一步不能用来判断
//!     「什么形态能过」**（早期正是把这里的宽松误当成了整体结论）。
//!   - **登录完成后的 authorize 跳转**：Zai 的 OAuth 服务按 client 登记的
//!     白名单逐字校验 `redirect_uri`。名单里只有官方客户端那四个 loopback
//!     端口（`18432 / 19654 / 19723 / 53699`）上的 `http://localhost:<端口>/auth/callback-zai`
//!     —— 端口不是那四个之一（网关自己的端口、`127.0.0.1`、路径不对……）
//!     一律被拒，窗口里直接渲染
//!     `{"detail":"Redirect URI not registered for this client"}`。
//!     **Google 是另一套规则**（RFC 8252 的 loopback 宽松匹配，端口任意），
//!     所以同一份代码「谷歌能登、Zai 登不上」—— 端口那一半由
//!     [`callback_server`](super::callback_server) 解决（登录时临时占用一个
//!     登记端口并把回调转回网关），本模块只负责拼对形态。
//!
//! 于是回调直接挂在本网关自己的监听端口上（与 CatPaw 同一手法，见
//! `core::login::catpaw` 的模块头）：省掉一个临时监听器，也省掉「临时端口
//! 被占用 / 忘了关」这类故障面。回调地址里**不带** state（理由见
//! `CALLBACK_PATH_PREFIX` 的说明）；任务关联由登录服务按变体匹配进行中的
//! 任务完成。
//!
//! ── 安全 ────────────────────────────────────────────────────
//! 回调落在本机 HTTP 端口上，任何本机进程都能伪造一次 GET。安全由**一次性
//! 授权码**与登录任务的校验链承担：code 一次性（重复回调幂等）、任务必须
//! 进行中、换码失败任务即失败 —— 与 CatPaw 的 loopback 回调同一威胁模型。
//! 与旧形态（state 进回调 URL 逐字比对）的差异：伪造回调不再被 state 挡住，
//! 代价是本机进程可以毁掉一场进行中的登录（DoS，重试即可），换不来任何凭证。
//! state 仍照常生成 —— 它是登录任务的内部关联键与去重键，只是不再外露。
//! token 只进账号库，不进日志。

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
/// 形如 `/auth/callback-{vendor}`（`zai` / `google`）—— **与官方客户端逐字同款**
/// （客户端 `getZaiCallbackUri()` / `getGoogleCallbackUri()` 给的就是
/// `http://localhost:<port>/auth/callback-zai|google`）。变体放在路径末段，
/// 回调处理函数据此认出这次回调属于哪个变体。
///
/// ── 为什么形态一个字都不能改（2026-09-22 实测，issue #11 修正）──
/// 上游在第 ② 步（`oauth-url` 接口）对 `navigate_uri` 只做非空校验，但登录
/// 完成、Zai 的 OAuth 服务生成 302 之前会按 client 登记的名单校验
/// `redirect_uri`：只有官方客户端那四个端口上的这个形态能过
/// （`http://localhost:18432|19654|19723|53699/auth/callback-zai`），
/// 别的（网关自己的端口、`127.0.0.1`、路径不对）得到
/// `{"detail":"Redirect URI not registered for this client"}`。
/// Google 的名单是宽松匹配（端口任意），因此它一直能过 —— 别把 Google 的表现
/// 当成两家的共同规则。
///
/// **端口由调用方给**：`callback_base` 是「这一轮的回调落点」，正常是登录时
/// 借到的登记端口（见 `super::callback_server`），抢不到时才回落到网关自己的
/// 端口（那时 Zai 这条路走不通，Google 不受影响）。
///
/// 与 CatPaw 的 loopback 回调同一手法，但这里没有把 state 拼进 URL：
/// 放查询串会与上游 302 带回的 `state`（上游自己生成的另一个值，见
/// `core::login::autoclaw::finish_autoclaw_oauth_callback`）撞参数名，放子路径
/// 又偏离了客户端形态 —— 任务关联改由登录服务按「变体匹配的进行中任务」
/// 完成（见 `CALLBACK` 安全说明）。
pub const CALLBACK_PATH_PREFIX: &str = "/auth/callback-";

/// 拼这次登录的回调地址（同时也是交给上游的 `navigate_uri`）。
///
/// `callback_base` 是这一轮的**回调落点基址**（`http://localhost:<端口>`），
/// 由调用方给出（`core::login::autoclaw` 的 `callback_endpoint` 决定用哪个
/// 端口，本模块拿不到）。host 必须是 `localhost`、端口必须是登记过的那四个
/// 之一（理由见 [`CALLBACK_PATH_PREFIX`] 的说明）。
pub fn navigate_uri(callback_base: &str, vendor: Vendor) -> String {
    format!(
        "{}{}{}",
        callback_base.trim_end_matches('/'),
        CALLBACK_PATH_PREFIX,
        vendor.id()
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
