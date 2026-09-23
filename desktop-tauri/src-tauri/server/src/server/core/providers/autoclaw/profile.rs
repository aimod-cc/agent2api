//! AutoClaw 用户资料（邮箱 / 用户名）—— 只为一件事：让账号在界面上认得出是谁。
//!
//! ── 上游接口（从客户端产物里核对出来的）─────────────────────────
//! ```text
//! POST {userapi}/userapi/v1/user-profile
//! body { source_id: "autotyper", device_id }        ← 客户端 withWebInfo 的形状
//!   → { code, msg, data: { user_id, user_name, email, user_phone, create_at, … } }
//! ```
//! 客户端 `getUserInfo()` 就是这一条（`api.post("/userapi/v1/user-profile",
//! withWebInfo({}))`），它拿到的 `data` 原样存进 auth.json 的 `userInfo` ——
//! 也就是说本地那份 `userInfo.email` 与这里查出来的是同一个东西，只是一个
//! 来自客户端缓存、一个来自实时查询。
//!
//! ── 为什么需要这条链路 ─────────────────────────────────────────
//! 国际版的官方主登录方式是 Zai / Google OAuth，而换码拿回来的 token 里
//! **没有邮箱**（JWT 只有 `user_id` / `device_id` / `guid` / `jti`…，实测）。
//! 于是网页登录建出来的账号在列表上只剩上游 `user_name`（如 `Lucas Ou`）——
//! 同一个人在两地的账号、或几个同类账号之间分不清。桌面端登录态那条来源
//! 不受影响：它的 auth.json 里直接有 `userInfo.email`（本地读，不联网）。
//!
//! 因此本模块只服务两类调用：
//!   1. 网页登录（OAuth）换码成功后补一次 —— 建账号时就带上邮箱；
//!   2. 余额查询顺带回填 —— 补上此前建的、记录里还没有邮箱的账号
//!      （网页登录之前建的、手填凭证建的、老版本建的）。
//!
//! ── 失败一律不致命（这是它与积分查询的关键差别）─────────────────
//! 邮箱是**展示信息**，不是凭证：查不到时账号照常能用，只是副标题少一行。
//! 所以两个调用点都按 best-effort 处理（记 verbose 日志、返回空串），
//! 绝不因为上游这次抖动就让「登录失败」或「余额查询失败」。
//!
//! 请求走 [`balance::userapi_post`]：签名头（`X-Auth-Sign`）与超时在全仓
//! 只有那一份实现，两处各写一份必然会在 appId/appKey 或时间戳单位上分叉。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::{json, Value};

use crate::server::errors::GatewayError;
use crate::server::logging;

use super::balance::{is_auth_expired_code, userapi_post};
use super::credentials::AutoClawCredentials;

/// 用户资料路径（客户端 `getUserInfo` 逐字；两地同一个路径，域名由地区决定）
const PROFILE_PATH: &str = "/userapi/v1/user-profile";

/// `source_id` 的取值（客户端 `withWebInfo` 写死的那个）
const SOURCE_ID: &str = "autotyper";

/// 一次资料查询的结果 —— 只取展示要用的邮箱。
///
/// `data` 里还有 `user_name` / `user_phone` / `create_at` 等字段，本模块**不取**：
/// 用户名在网页登录那条链已经由换码响应给过（`oauth::exchange_code` 的
/// `userName`），手机号是脱敏展示用的、本网关不用它做任何判断。
pub(crate) struct Profile {
    pub email: String,
}

/// 查一次用户资料（失败返回 Err，由调用方决定要不要吞掉）。
///
/// 可见性是 `pub(crate)` 而不是 `pub(super)`：调用方之一在 `core::login`
/// （网页登录换码之后那一步），它不在本模块的父模块之下。
pub(crate) async fn fetch(credentials: &AutoClawCredentials) -> Result<Profile, GatewayError> {
    let body = json!({
        "source_id": SOURCE_ID,
        "device_id": credentials.device_id,
    });
    let payload = userapi_post(credentials, PROFILE_PATH, &body, "用户资料查询").await?;
    if let Some(code) = payload.get("code").and_then(Value::as_i64) {
        if is_auth_expired_code(code) {
            return Err(GatewayError::with_status(
                401,
                "登录态已过期，无法查询用户资料",
            ));
        }
        if code != 0 {
            let message = payload
                .get("msg")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            return Err(GatewayError::with_status(
                502,
                if message.is_empty() {
                    format!("用户资料查询失败（上游业务码 {code}）")
                } else {
                    format!("用户资料查询失败：{message}")
                },
            ));
        }
    }
    let data = payload.get("data").cloned().unwrap_or(Value::Null);
    Ok(Profile {
        email: data
            .get("email")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string(),
    })
}

/// 取邮箱的 best-effort 版本：任何失败都只记 verbose，返回空串。
///
/// 调用点（网页登录 / 余额查询回填）都不该因为「邮箱没查到」而失败 ——
/// 见模块头「失败一律不致命」。
pub(crate) async fn email_or_empty(credentials: &AutoClawCredentials, what: &str) -> String {
    match fetch(credentials).await {
        Ok(profile) => profile.email,
        Err(error) => {
            logging::verbose(
                "[Accounts]",
                &format!("ℹ️  {what}未取到邮箱（{}）", error.message),
            );
            String::new()
        }
    }
}
