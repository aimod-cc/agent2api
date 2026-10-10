//! OfficeAce（华为云果办 / OfficeClaw / jiuwenclaw）接入。
//!
//! ── 这条通道长什么样 ─────────────────────────────────────────
//! 上游**本身就是 OpenAI 兼容**：chat 打 `{model_api_url_base}/v2/chat/completions`，
//! 鉴权是一个 Basic 头（`model_app_key`:`model_app_secret`）。桌面端只是把这张表
//! 落在本机 `~/.office-claw/.jiuwenclaw/config/routing_state/users/<账号ID>/models.json`
//! —— 读文件，不是中间人抓包。
//!
//! 凭据**分两层**（别混）：
//!   · **网关凭据**：`model_app_key` / `model_app_secret`，给 chat 用，**不过期**；
//!   · **临时凭据**：`HSTA…` 的 AK/SK + `security_token` + `project_id`，**实测 2 小时**，
//!     只用来签控制面（问云端要模型表、续期）。
//!
//! 拿网关凭据的路有两条：自助 OAuth（PKCE + P-256 DPoP → 轮询取授权码 → STS 换临时
//! 凭据 → `client-permission-validate` 换网关凭据），或用户直接从别的机器导入
//! `api_base` + `Authorization`。
//!
//! ── 与 CodeArts 的关系 ───────────────────────────────────────
//! 同属华为云身份栈（OAuth PKCE + DPoP + SDK-HMAC-SHA256 + IAM v3），但**不是同一套**：
//! 签名规范化的三处差异见 [`signer`] 的模块头；上游 chat 面无状态（不需要 CodeArts
//! 那种并发会话闸门）。

pub mod adapter;
pub mod balance;
pub mod chat;
pub mod checkin;
pub mod credentials;
pub mod models;
pub mod oauth;
pub mod onboarding;
pub mod probe;
pub mod signer;
pub mod subscription;

use serde_json::Value;

use crate::server::core::account_store::AccountStore;
use crate::server::errors::GatewayError;

/// 用落盘的 refresh token + DPoP 私钥续期控制面临时凭据，并把新凭据写回账号。
///
/// 返回新的 `access_key_id`（适配器 `refresh_access_token` 契约要一个非空串表成功）。
///
/// ── 为什么只写控制面那半边 ──────────────────────────────────
/// 续期换的是**控制面**的临时 AK/SK/STS（额度/签到用）；转发的网关 Basic 那对
/// **不过期**，不动。写回走 `update_officeace_control_plane`（只改这四个字段 +
/// 到期时刻 + 轮换后的 refresh token），不碰 baseUrl / modelAppKey / dpopKeyPair。
pub async fn refresh_control_plane(
    store: &AccountStore,
    account_id: &str,
) -> Result<String, GatewayError> {
    let record = store
        .officeace_account_record(account_id)
        .ok_or_else(|| GatewayError::with_status(404, "OfficeAce 账号不存在或不可用"))?;
    let refresh_token = record
        .get("refreshToken")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if refresh_token.is_empty() {
        return Err(GatewayError::with_status(
            400,
            "OfficeAce 账号没有 refresh token，无法续期：请重新登录一次",
        ));
    }
    let dpop = record
        .get("dpopKeyPair")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .ok_or_else(|| {
            GatewayError::with_status(
                400,
                "OfficeAce 账号缺少 DPoP 私钥上下文，无法续期：请重新登录一次",
            )
        })?;
    let (credential, expires_at, user_name, next_refresh) =
        oauth::LoginFlow::refresh(&dpop, &refresh_token)
            .await
            .map_err(|reason| GatewayError::with_status(502, format!("OfficeAce 续期失败：{reason}")))?;
    store
        .update_officeace_control_plane(
            account_id,
            &credential.access_key_id,
            &credential.secret_access_key,
            &credential.security_token,
            &credential.project_id,
            expires_at,
            &next_refresh,
        )
        .map_err(|error| GatewayError::with_status(500, error.message))?;
    // 名字自愈：参考实现每次续期都从重发令牌里再取一次 userName（`accounts.mjs` 的
    // label 优先级是「用户标签 → 上游显示名 → id」）。这里非对称地补那一课 ——
    // 登录那一刻可能一枚令牌都没给名字（那时账号名停在十六进制的 principal_id 或
    // account_id 上），每续一次就问一次，上游给了名字就换回来，用户自己改过的名不动。
    if store
        .heal_officeace_display_name(account_id, &user_name)
        .map_err(|error| GatewayError::with_status(500, error.message))?
    {
        crate::server::logging::log(
            "[Accounts]",
            &format!("OfficeAce 账号名已按上游显示名更新: {account_id}"),
        );
    }
    crate::server::logging::log(
        "[Accounts]",
        &format!("✅ OfficeAce 账号控制面凭据已续期: {account_id}"),
    );
    Ok(credential.access_key_id)
}
