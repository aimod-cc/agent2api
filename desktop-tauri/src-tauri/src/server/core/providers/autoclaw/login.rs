//! AutoClaw 手机号验证码登录（**逆向**：老项目没有，接口从 AutoClaw 桌面端的
//! `app.asar` 里读出并实测确认）。
//!
//! ── 为什么不是「网页登录」────────────────────────────────────
//! 另外两家（小浣熊 / Qoder）的网页登录形态是「打开官方登录页 → 用户登录 →
//! 回调带一次性授权码 → 网关换凭证」。AutoClaw **没有这条路**：
//!
//!   - 桌面端没有公网部署的 Web 应用（网页版是本地 29100 端口的自托管 shell，
//!     与账号登录无关）；
//!   - 账号体系里**没有授权码 / device code / grant_type 这套东西**
//!     （在 `out/` 里搜 `authorization_code` / `device_code` / `code_verifier`
//!     的命中全部落在第三方库 node_modules 里）；
//!   - 自定义协议 `autoclaw://` 只服务**海外版** OAuth（`auth/callback-google`
//!     / `-zai`），而国内构建里 `isOversea = false`，登录页不渲染 OAuth 按钮。
//!
//! 国内版唯一的登录方式就是**手机号 + 短信验证码**，两个接口：
//!
//! ```text
//! POST {userapi}/userapi/v1/agent-send-code   {"phone","source_id","device_id"}
//! POST {userapi}/userapi/v1/agent-login/      {"phone","code","platform","source_id","device_id"}
//! ```
//!
//! `agent-login` **直接返回 token 对**（不经授权码），`platform: "web"` 是网页端
//! 与桌面端唯一的差别（源实现 `loginWebWithPhoneCode`）。因此这条链路在网关上就是
//! 「代收验证码 → 换 token → 落账号」，没有回调窗口、没有轮询。
//!
//! ── 实测确认（2026-09-19）────────────────────────────────────
//! 签名头与积分 / 签到链路**完全一致**（`X-Auth-Sign = MD5(appId&秒级ts&appKey)`），
//! 复用 `refresh::signed_auth_headers`；接口可达，业务错误以 HTTP 200 +
//! `code != 0` 表达（错误验证码实测返回 `{"code":400001,"msg":"请求数据有问题,
//! 请检查后重试"}`）。因此这里**每个业务码都要单独翻成人话** —— 上游的 msg 对
//! 用户没有指导意义。
//!
//! ── deviceId 从哪来 ─────────────────────────────────────────
//! 官方客户端是「ed25519 公钥的 SHA-256」（`fingerprintPublicKey`），但**服务端
//! 只把它当一个不透明的 64 位十六进制串**（登录请求里就是个 `device_id` 字段，
//! 刷新接口同样只回传它）。因此这里生成一个密码学随机的 32 字节十六进制串 ——
//! 不需要为「复刻一个客户端指纹」引进 ed25519 依赖，而它承担的全部职责就是
//! 「区分不同设备」。

use serde_json::{json, Value};

use crate::server::core::auth_http::send_raw;
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::credentials;
use super::refresh::signed_auth_headers;

/// 发验证码路径（源实现 `sendCode`）
const SEND_CODE_PATH: &str = "/userapi/v1/agent-send-code";
/// 登录路径（源实现 `phoneCodeLogin`，注意结尾的斜杠是上游要求的）
const LOGIN_PATH: &str = "/userapi/v1/agent-login/";

/// 登录接口的请求超时（与积分 / 签到同一档；验证码接口偶发慢，给到 20 秒）
const REQUEST_TIMEOUT_MS: u64 = 20_000;

/// 中国大陆手机号（源实现的 `normalizePhone` 正则等价物：`1[2-9]` 开头的 11 位）
fn normalize_phone(raw: &str) -> Option<String> {
    let trimmed: String = raw.chars().filter(|ch| !ch.is_whitespace() && *ch != '-').collect();
    let digits = trimmed.strip_prefix("+86").unwrap_or(&trimmed);
    let digits = digits.strip_prefix("86").filter(|rest| rest.len() == 11).unwrap_or(digits);
    if digits.len() == 11
        && digits.starts_with('1')
        && digits.chars().nth(1).is_some_and(|ch| ('2'..='9').contains(&ch))
        && digits.chars().all(|ch| ch.is_ascii_digit())
    {
        Some(digits.to_string())
    } else {
        None
    }
}

/// 6 位数字验证码（源实现 `normalizeCode`）。
///
/// ── 为什么返回**数字**而不是字符串（实测踩过的坑，别改回去）──────
/// 上游要求 body 里的 `code` 是 **JSON 数字**，传字符串会得到
/// `400001 / 请求数据有问题,请检查后重试` —— 一个与「验证码错误」完全无关的
/// 参数错误。实测（2026-09-19，同一个手机号同一时刻）：
///
/// ```text
/// code 字符串 "123456"  → 400001 请求数据有问题,请检查后重试
/// code 数字   123456    → 630202 抱歉,验证码错误，请输入正确验证码！
/// ```
///
/// 源实现的 `normalizeCode` 最后一步是 `return parsed`（`Number(...)` 的结果），
/// 正是这个原因 —— 它不是顺手把字符串转成数字，而是**接口契约要求数字**。
/// 曾经按「验证码看起来像字符串」处理，结果是用户拿着正确验证码也永远登不上，
/// 且错误文案还被误译成「验证码不正确」，把排查方向带偏。
///
/// 前导零也无所谓：`"000000"` 与 `0` 等价（都是同一个错误码），
/// 因为服务端只看数值。
fn normalize_code(raw: &str) -> Option<u32> {
    let trimmed = raw.trim();
    if trimmed.len() == 6 && trimmed.chars().all(|ch| ch.is_ascii_digit()) {
        trimmed.parse::<u32>().ok()
    } else {
        None
    }
}

/// 生成一个设备 id（32 字节随机 → 64 位十六进制，与官方客户端的形态一致）。
fn new_device_id() -> String {
    let mut bytes = [0u8; 32];
    if getrandom::getrandom(&mut bytes).is_err() {
        // 随机源不可用是极罕见的系统级故障。退化成「纳秒时钟 + 进程内计数」仍然
        // 满足它唯一的用途（让本机不同设备在服务端可区分）—— device_id 不是
        // 安全凭证，官方客户端也只是把它当一个不透明的标识串。
        use std::sync::atomic::{AtomicU64, Ordering};
        static FALLBACK_COUNTER: AtomicU64 = AtomicU64::new(1);
        let counter = FALLBACK_COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos() as u64)
            .unwrap_or(0);
        let mixed = nanos ^ counter.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        for (index, slot) in bytes.iter_mut().enumerate() {
            *slot = (mixed >> ((index % 8) * 8)) as u8;
        }
    }
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// 发一次带签名的 userapi POST。
///
/// 与 `balance.rs` 的同名函数是**同一套签名与超时**，但那条链路的凭证是
/// `AutoClawCredentials`（要求已有 token），而登录时**还没有 token** ——
/// 这正是不能复用的原因（用空 token 调 `signed_auth_headers` 恰好可行：
/// 它只在 token 非空时才加 authorization 头）。
async fn post_unauthenticated(path: &str, body: &Value, what: &str) -> Result<Value, GatewayError> {
    let url = format!("{}{path}", credentials::userapi_base_url());
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

/// 业务码 → 用户能看懂的原因。
///
/// ── 为什么不能直接透出上游 msg ──────────────────────────────
/// 上游的参数类错误一律给 `400001 / 请求数据有问题,请检查后重试` —— 它对用户
/// 没有指导意义，也**分不清**到底是哪个字段不对。这里按码翻成「该做什么」，
/// 未知码才回落到上游文案。
///
/// ── 码表的口径（实测 + 客户端源码对照）──────────────────────
///   400001：**参数错误**（客户端源码里对该码的注释就是「参数错误」）。注意它
///     **不是**「验证码不正确」—— 把两者混为一谈会把排查方向带偏：真凶可能
///     是字段类型不对（我们踩过：`code` 传字符串就稳定得这个码，见
///     `normalize_code` 的说明），而用户会一直以为自己验证码输了。
///   630202：验证码错误（这个才是真的「码不对」）。
///   630101：发码过于频繁（发码接口的码，登录接口见不到）。
///   400002：签名校验失败（本机时钟漂移）。400000 / 410000：登录态失效。
fn describe_login_error(code: i64, upstream_msg: &str) -> String {
    match code {
        400_001 => "请求参数有误，请检查手机号与验证码格式后重试".to_string(),
        630_202 => "验证码不正确，请检查后重试".to_string(),
        630_101 => "获取验证码过于频繁，请稍后再试".to_string(),
        410_000 | 400_000 => "登录态已失效，请重新登录".to_string(),
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

/// 发送短信验证码。
///
/// `device_id` 由调用方持有并在随后的 `login` 里沿用 —— 上游把设备与登录会话
/// 关联，两次调用用不同的 device_id 会让「刚发的码」配不上「正在登录的设备」。
pub async fn send_code(phone: &str) -> Result<Value, GatewayError> {
    let Some(phone) = normalize_phone(phone) else {
        return Err(GatewayError::with_status(
            400,
            "请填写 11 位中国大陆手机号",
        ));
    };
    let device_id = new_device_id();
    let body = json!({
        "phone": phone,
        "source_id": "autoclaw",
        "device_id": device_id,
    });
    let payload = post_unauthenticated(SEND_CODE_PATH, &body, "发送验证码").await?;
    let code = payload.get("code").and_then(Value::as_i64).unwrap_or(0);
    if code != 0 {
        let upstream_msg = payload.get("msg").and_then(Value::as_str).unwrap_or("");
        let message = describe_login_error(code, upstream_msg);
        logging::log("[Login]", &format!("❌ AutoClaw 验证码发送失败（{code}）: {message}"));
        return Err(GatewayError::with_status(400, message));
    }
    // 源实现还要求 `data.result` 为真（`code === 0` 但 result 假也算失败）
    let delivered = payload
        .get("data")
        .and_then(|data| data.get("result"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !delivered {
        logging::log("[Login]", "❌ AutoClaw 验证码发送失败：上游返回 result=false");
        return Err(GatewayError::with_status(502, "验证码发送失败，请稍后重试"));
    }
    // 不打手机号（它是个人信息）：只留下「发给了尾号 xxxx」这种可核对但不泄露的痕迹
    logging::log(
        "[Login]",
        &format!("📱 AutoClaw 验证码已发送（{}）", mask_phone(&phone)),
    );
    Ok(json!({ "deviceId": device_id }))
}

/// 用手机号 + 验证码登录，返回可直接交给 `add_autoclaw_account` 的凭证对象。
///
/// 返回形状与「粘贴 token 添加」的 payload **逐字一致**
/// （`{token, refreshToken, deviceId}`）—— 登录只是另一种拿到凭证的方式，
/// 落盘、命名、去重、优先级分配全部复用既有的添加路径，不新开一条写账号的通路。
pub async fn login_with_code(
    phone: &str,
    code: &str,
    device_id: Option<&str>,
) -> Result<Value, GatewayError> {
    let Some(phone) = normalize_phone(phone) else {
        return Err(GatewayError::with_status(
            400,
            "请填写 11 位中国大陆手机号",
        ));
    };
    let Some(code) = normalize_code(code) else {
        return Err(GatewayError::with_status(400, "请填写 6 位数字验证码"));
    };
    // 沿用发码时那个 device_id（调用方从 send_code 的响应里带回）；
    // 没带上就现生成一个 —— 上游接受这种「新设备直接登录」的形态
    let device_id = match device_id.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => value.to_string(),
        None => new_device_id(),
    };
    let body = json!({
        "phone": phone,
        "code": code,
        // 网页端与桌面端**唯一**的差别（源实现 `loginWebWithPhoneCode`）
        "platform": "web",
        "source_id": "autoclaw",
        "device_id": device_id,
    });
    let payload = post_unauthenticated(LOGIN_PATH, &body, "登录").await?;
    let status = payload.get("code").and_then(Value::as_i64).unwrap_or(0);
    if status != 0 {
        let upstream_msg = payload.get("msg").and_then(Value::as_str).unwrap_or("");
        let message = describe_login_error(status, upstream_msg);
        // 上游原始 msg 一起记进日志：翻译后的文案给用户看，原文给排障用
        // （上游偶发改码表时，只有原文能看出到底变了什么）
        logging::log(
            "[Login]",
            &format!(
                "❌ AutoClaw 验证码登录失败（code={status}，上游原文「{upstream_msg}」）: {message}"
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
        logging::log("[Login]", "❌ AutoClaw 登录成功但上游未返回 access_token");
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
    if !refresh_token.is_empty() {
        if let Some(object) = credentials.as_object_mut() {
            object.insert("refreshToken".to_string(), Value::String(refresh_token));
        }
    }
    // 手机号脱敏后随响应回给界面当备注名兜底（源实现用 `maskPhone` 展示）；
    // 界面仍可让用户自己填备注名，这里只提供一个不像乱码的默认值
    if let Some(object) = credentials.as_object_mut() {
        object.insert("phoneTail".to_string(), Value::String(mask_phone(&phone)));
    }
    Ok(credentials)
}

/// 手机号脱敏（源实现 `maskPhone`：保留前 3 后 4）。
fn mask_phone(phone: &str) -> String {
    if phone.len() < 7 {
        return phone.to_string();
    }
    format!("{}****{}", &phone[..3], &phone[phone.len() - 4..])
}
