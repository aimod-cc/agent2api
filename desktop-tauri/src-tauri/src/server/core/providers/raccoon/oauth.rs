//! 小浣熊**网页登录**（`office-raccoon://` 授权码换凭证）。
//!
//! 移植来源：源项目的 `raccoon-oauth.mjs`（`buildRaccoonAuthorizeUrl` /
//! `isRaccoonAuthCallbackUrl` / `parseRaccoonAuthCallback` /
//! `exchangeRaccoonAuthorizationCode`）。协议本身很短：
//!
//! ```text
//! ① 打开 {mainOrigin}/code/authorize?login_source=desktop&appname=<客户端名>&state=<state>
//! ② 用户登录成功 → 官方页面跳转 office-raccoon://auth/callback?code=…&state=…
//! ③ POST {authApiBase}/login_with_authorization_code  body {"authorization_code": code}
//!      → data.access_token / data.refresh_token / data.office_* （code 200035 = 授权码已失效）
//! ```
//!
//! ── 为什么 state 必须在这里生成、且回调时逐字比对 ────────────
//! `state` 是**一次性随机串**，它回答的是「这次带着 code 跳回来的页面，是不是
//! 由本进程刚刚发起的那次登录」。回调落在自定义协议上，任何本机程序都能构造
//! 一个 `office-raccoon://auth/callback?code=…&state=…` 丢进登录窗口，因此
//! 不校验 state 就等于把「用别人的 code 换来的凭证写进你的账号库」这条口子
//! 敞开着（换来的凭证指向谁，取决于别人手里的 code）。源实现同样逐字比对。
//!
//! ── 为什么 state 用 `new_request_id()` 那个生成方式 ──────────
//! 项目里唯一的「不可预测随机串」来源就是 `upstream::request::new_request_id`
//! （uuid v4 形态，混入 `RandomState` 的随机种子 + 单调计数 + 纳秒时间戳）。
//! workbuddy 登录的 state 是**上游**发的（`auth/state` 接口），这里没有上游
//! 可问，所以复用该项目既有的同一手法，不新引随机数依赖（Cargo.toml 里没有
//! `rand`／`uuid`，为一个 state 引进它们不合算）。
//!
//! ── 落账号为什么不自己写一份 ────────────────────────────────
//! 走 `AccountStore::add_raccoon_account` —— 与「填写凭证 / 粘贴 JSON」两条
//! 路径**同一个入口**：JWT 校验、`user-<userId>` 的 id 生成、撞 id 保护、
//! 优先级分配、字段合并都在那里。另写一份落盘逻辑的唯一后果是两条路逐渐分叉
//! （例如网页登录进来的账号缺 officeIdentity，或者优先级算法不一样）。
//! `source` 因此沿用该函数的既有取值 `manual`（导入路径才写 `imported`），
//! 不新造 `web`：前端账号卡片（`accounts-model.js`）只区分 `imported` 与其余，
//! 新取值不会有任何展示差异，却会让「来源」这一列的语义凭空多出一档。
//!
//! ── panic=abort ────────────────────────────────────────────
//! 本文件在登录链路上，绝不 unwrap/expect/panic：取值走 Option 链与 `unwrap_or`，
//! 所有失败都转成 `GatewayError`。

use serde_json::{json, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::core::auth_http::send_raw;
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::credentials::AUTH_REQUEST_TIMEOUT_MS;
use super::{auth_api_base, DEFAULT_AUTH_ORIGIN};

/// 授权页路径（源实现 `new URL('/code/authorize', mainOrigin)`）
const AUTHORIZE_PATH: &str = "/code/authorize";

/// 登录来源标记：官方页面据此走「桌面端登录」分支（源实现固定 `desktop`）
const LOGIN_SOURCE: &str = "desktop";

/// 申请授权时上报的客户端名（源实现 `DEFAULT_APP_NAME`）
const APP_NAME: &str = "办公小浣熊客户端";

/// 回调的协议 / host / 路径（源实现 `isRaccoonAuthCallbackUrl` 的三个判据）。
///
/// 官方登录成功后**固定**跳这个形态，不能改：它就是小浣熊发起登录时登记的
/// 回调地址，改了官方页面就不会带 code 回来。
const CALLBACK_SCHEME: &str = "office-raccoon";
const CALLBACK_HOST: &str = "auth";
const CALLBACK_PATH: &str = "/callback";

/// state 长度上限（源实现 `MAX_STATE_LENGTH`）：超长一律拒绝，不截断 ——
/// 被截断的 state 已经不可能与发起时一致，报错比默默比对失败更清楚。
const MAX_STATE_LENGTH: usize = 512;

/// 授权码长度上限（源实现 `MAX_CODE_LENGTH`）
const MAX_CODE_LENGTH: usize = 8192;

/// 授权码已失效的上游业务码（源实现 `payload.code === 200035`）。
/// 它值得单独识别：这不是「登录服务坏了」，而是「用户把回调链接放太久、
/// 或者重复点了两次」，文案必须告诉用户重新发起，而不是让他等我们重试。
const CODE_EXPIRED: i64 = 200_035;

/// 主站 Origin（源实现 `mainOrigin`：`RACCOON_MAIN_SITE_URL` 可覆盖）。
///
/// 默认与鉴权 API 同一个站点（`DEFAULT_AUTH_ORIGIN`）—— 两者在生产上就是
/// 同一个域，这里不重复写一遍字面量，避免将来换域名时漏改一处。
fn main_origin() -> String {
    std::env::var("RACCOON_MAIN_SITE_URL")
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_AUTH_ORIGIN.to_string())
}

/// 构造授权地址（源实现 `buildRaccoonAuthorizeUrl`）。
///
/// 用 `url::Url` 拼而不是字符串相加：`appname` 是中文、`state` 是 uuid，
/// 两者都必须正确百分号编码，手拼一个 `&` 或空格就会让官方页面拿到半个参数。
pub fn build_authorize_url(state: &str) -> String {
    let origin = main_origin();
    let Ok(mut url) = url::Url::parse(&origin) else {
        // origin 不可解析（用户把 RACCOON_MAIN_SITE_URL 配成了非法值）：
        // 退回到「把路径拼在后面」的形态，让请求去打上游、由传输层报出可读错误。
        // 这里**不返回错误**是因为本函数在 trait 上返回 Option<String> 形态的
        // (url, state)，没有错误通道；配置错误在 HTTP 层会比在这里更早暴露。
        return format!(
            "{origin}{AUTHORIZE_PATH}?login_source={LOGIN_SOURCE}&appname={APP_NAME}&state={state}"
        );
    };
    url.set_path(AUTHORIZE_PATH);
    url.query_pairs_mut()
        .append_pair("login_source", LOGIN_SOURCE)
        .append_pair("appname", APP_NAME)
        .append_pair("state", state);
    url.to_string()
}

/// 生成一次登录的 `state`（不可预测的 uuid v4 串，理由见模块头）。
pub fn new_login_state() -> String {
    crate::server::core::upstream::request::new_request_id()
}

/// 回调地址 → 授权码（源实现 `isRaccoonAuthCallbackUrl` + `parseRaccoonAuthCallback`）。
///
/// ── 逐项比对而不是前缀匹配（安全）────────────────────────────
/// `office-raccoon://auth/callback@evil.com?…`、`office-raccoon://auth/callback.evil`
/// 这类字符串用 `starts_with` 判会全部放行。这里用 `url` crate 解析后逐项比对
/// scheme / host / path —— 与源实现 `new URL()` + 三个等值判断同一口径。
/// 注意自定义协议下 `//auth/callback` 的 `auth` 是 host（源实现读的是 `hostname`，
/// 同一个意思），path 则是 `/callback`。
pub fn parse_callback_code(raw_url: &str, expected_state: &str) -> Result<String, GatewayError> {
    let url = url::Url::parse(raw_url.trim())
        .map_err(|_| GatewayError::with_status(400, "登录回调地址无效"))?;
    // host 用**不区分大小写**比对：自定义协议在 url crate 里走的是 opaque host 那条
    // 解析路径，不像 http(s) 那样被规范化成小写，`office-raccoon://AUTH/callback`
    // 会原样保留大小写。scheme 由解析器保证小写，path 保持精确（官方给的固定形态）。
    let host_matches = url
        .host_str()
        .map(str::to_ascii_lowercase)
        .is_some_and(|host| host == CALLBACK_HOST);
    let matches = url.scheme() == CALLBACK_SCHEME && host_matches && url.path() == CALLBACK_PATH;
    if !matches {
        return Err(GatewayError::with_status(400, "登录回调地址无效"));
    }
    let param = |name: &str| -> String {
        url.query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.trim().to_string())
            .unwrap_or_default()
    };
    let code = param("code");
    if code.is_empty() {
        return Err(GatewayError::with_status(400, "登录回调里没有授权码"));
    }
    if code.chars().count() > MAX_CODE_LENGTH {
        return Err(GatewayError::with_status(400, "授权码过长"));
    }
    let state = param("state");
    if state.is_empty() {
        return Err(GatewayError::with_status(
            400,
            "登录回调里没有 state，无法确认这次登录由本机发起，请重新发起网页登录",
        ));
    }
    if state.chars().count() > MAX_STATE_LENGTH {
        return Err(GatewayError::with_status(400, "登录回调 state 过长"));
    }
    if state != expected_state {
        return Err(GatewayError::with_status(
            400,
            "登录回调 state 校验失败，请重新发起登录",
        ));
    }
    Ok(code)
}

/// 用一次性授权码换凭证并落账号，返回账号 id
/// （源实现 `exchangeRaccoonAuthorizationCode` + 路由里的 `store.addAccount`）。
///
/// ── 出网为什么不带账号代理 ──────────────────────────────────
/// 与 `credentials::call_refresh_api` 同一取舍：账号级代理是给转发（流式长请求）
/// 准备的出口，而鉴权域是另一个站点；源实现同样用裸 fetch。
///
/// ── 错误语义 ────────────────────────────────────────────────
/// 传输失败 → 502（超时 504）；授权码失效（200035）→ **400** 且文案要求重新发起；
/// 上游 4xx → 原样透出该状态码（多半是 code 不合法）；其余 → 502。
pub async fn exchange_code(
    store: &AccountStore,
    code: &str,
    state: &str,
) -> Result<String, GatewayError> {
    let code = code.trim();
    if code.is_empty() {
        return Err(GatewayError::with_status(400, "缺少授权码"));
    }
    if code.chars().count() > MAX_CODE_LENGTH {
        return Err(GatewayError::with_status(400, "授权码过长"));
    }
    // state 在这里再校验一次（深度防御）：调用链上游（登录接口）已比对过，
    // 但「这个 state 该长什么样」只有实现知道，多一道不会有代价。
    if state.trim().is_empty() {
        return Err(GatewayError::with_status(400, "缺少登录 state"));
    }

    let url = format!("{}/login_with_authorization_code", auth_api_base());
    let body = json!({ "authorization_code": code });
    let headers: Vec<(String, String)> =
        vec![("Content-Type".to_string(), "application/json".to_string())];
    let response = send_raw(
        "POST",
        &url,
        Some(&body),
        &headers,
        None,
        Some(AUTH_REQUEST_TIMEOUT_MS),
    )
    .await
    .map_err(|error| {
        if error.is_timeout() {
            GatewayError::with_status(504, "网页登录换取凭证超时，请重试")
        } else {
            GatewayError::with_status(502, format!("网页登录换取凭证失败: {error}"))
        }
    })?;

    let payload = response.payload.unwrap_or(Value::Null);
    let data = payload.get("data").cloned().unwrap_or(Value::Null);
    let text_of = |key: &str| -> String {
        data.get(key)
            .map(super::jwt::js_text)
            .map(|text| super::jwt::strip_bearer(&text))
            .unwrap_or_default()
    };
    let access_token = text_of("access_token");
    let refresh_token = text_of("refresh_token");
    let upstream_code = payload.get("code").and_then(Value::as_i64);
    if upstream_code == Some(CODE_EXPIRED) {
        return Err(GatewayError::with_status(
            400,
            "授权码已失效，请重新发起网页登录",
        )
        .with_optional_code(upstream_code));
    }
    if !response.ok || access_token.is_empty() || refresh_token.is_empty() {
        let message = payload
            .get("message")
            .or_else(|| payload.get("msg"))
            .map(super::jwt::js_text)
            .filter(|text| !text.is_empty());
        let message = match message.as_deref() {
            // 上游把「内部异常」原样回下来，这句话对用户没有信息量
            Some("internal_server_error") => "登录服务异常，请稍后重试".to_string(),
            Some(text) => text.to_string(),
            None => format!("网页登录换取凭证失败（HTTP {}）", response.status),
        };
        let status = if (400..500).contains(&response.status) {
            response.status
        } else {
            502
        };
        return Err(GatewayError::with_status(status as i32, message).with_optional_code(upstream_code));
    }

    // 落账号：走既有添加路径（理由见模块头）。office_* 用上游返回的下划线键名，
    // `add_raccoon_account` 的取值链里认它们（与粘贴 auth.json 那条路同一份键名）。
    let mut record = serde_json::Map::new();
    record.insert("access_token".to_string(), Value::String(access_token));
    record.insert("refresh_token".to_string(), Value::String(refresh_token));
    for key in ["office_identity", "office_org_name", "office_org_role"] {
        let value = data
            .get(key)
            .map(super::jwt::js_text)
            .map(|text| text.trim().to_string())
            .unwrap_or_default();
        if !value.is_empty() {
            record.insert(key.to_string(), Value::String(value));
        }
    }
    let saved = store
        .add_raccoon_account(&Value::Object(record), None)
        .map_err(|error| GatewayError::with_status(error.status_code, error.message))?;
    let id = saved
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if id.is_empty() {
        return Err(GatewayError::with_status(
            500,
            "网页登录成功但账号未能写入（数据缺少 id），请重试",
        ));
    }
    logging::log(
        "[Raccoon]",
        &format!(
            "✅ 网页登录成功，账号已加入列表: {}（{}）",
            saved.get("name").and_then(Value::as_str).unwrap_or(""),
            id
        ),
    );
    Ok(id)
}
