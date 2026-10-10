//! OfficeAce 凭证：**两层**，别混。
//!
//! ── 两层各是什么（依据：officeace2api 与 opencode-officeace-auth 的实现）──
//!   · **网关凭据**（转发用）：`model_app_key` : `model_app_secret`，拼成一个
//!     `Authorization: Basic <base64>` 发给模型网关。**不过期**。
//!   · **控制面凭据**（问云端 / 续期用）：`HSTA…` 的 AK/SK + `security_token`
//!     + `project_id`。实测活 **2 小时**；用来签 `client-permission-validate`、
//!     额度与签到接口。
//!
//! 上游干活只用网关凭据 —— 所以「导入过一次」就有稳定可用的账号；控制面凭据
//! 过期只影响额度/签到与自动续期，不影响转发。
//!
//! ── 账号记录里怎么存 ────────────────────────────────────────
//! 复用本仓既有的「账号是一个 JSON 对象」约定，字段：
//!
//! ```text
//! baseUrl           模型网关基址（不含 /v2 也要能收，见 `chat::chat_endpoint`）
//! modelAppKey       网关凭据 key（与 modelAppSecret 成对）
//! modelAppSecret
//! accessKeyId       控制面临时 AK（可缺：手工只导入网关凭据时没有）
//! secretAccessKey
//! securityToken
//! projectId
//! expiresAt         控制面凭据的到期时刻（毫秒；缺省 0 = 未知）
//! ```
//!
//! 手工导入那条路会给前三个（`baseUrl` + 两个 key）；自助 OAuth 会给全部。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::Value;

use crate::server::errors::GatewayError;

use super::signer::Credential as SigningCredential;

/// 从账号记录取值：优先 `keys` 里的名字，取第一个非空字符串。
fn pick(record: &Value, keys: &[&str]) -> String {
    for key in keys {
        if let Some(value) = record.get(*key).and_then(Value::as_str) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    String::new()
}

/// 一次转发用的凭证快照。
#[derive(Clone, Debug, Default)]
pub struct OfficeAceCredential {
    /// 账号记录 id（错误归因与冷却键用）
    pub id: String,
    /// 模型网关基址（未规范化，交给 `chat` 去拼端点）
    pub base_url: String,
    /// 网关凭据：转发用的那一对
    pub model_app_key: String,
    pub model_app_secret: String,
    /// 控制面临时凭据（手工只导入网关凭据时全空）
    pub access_key_id: String,
    pub secret_access_key: String,
    pub security_token: String,
    pub project_id: String,
    /// 控制面凭据到期时刻（毫秒；0 = 未知）
    pub expires_at: i64,
}

impl OfficeAceCredential {
    /// 转发可用的最低条件：基址 + 一对网关凭据。
    pub fn can_forward(&self) -> bool {
        !self.base_url.is_empty()
            && !self.model_app_key.is_empty()
            && !self.model_app_secret.is_empty()
    }

    /// 控制面凭据齐不齐（额度 / 签到 / 续期要用）。
    pub fn has_control_plane(&self) -> bool {
        !self.access_key_id.is_empty() && !self.secret_access_key.is_empty()
    }

    /// 控制面临期判定：到期前 [`EXPIRY_MARGIN_MS`] 算临期。
    pub fn control_plane_expiring(&self) -> bool {
        self.expires_at > 0
            && self.expires_at - EXPIRY_MARGIN_MS <= crate::server::logging::now_ms()
    }

    /// 转成签名层要的凭据（控制面请求用；没有临时 AK/SK 时返回 Err）。
    pub fn signing(&self) -> Result<SigningCredential, String> {
        if !self.has_control_plane() {
            return Err("OfficeAce 账号没有控制面凭据（导入时只给了网关 Basic 那一对）".to_string());
        }
        Ok(SigningCredential {
            access_key_id: self.access_key_id.clone(),
            secret_access_key: self.secret_access_key.clone(),
            security_token: self.security_token.clone(),
            project_id: self.project_id.clone(),
        })
    }
}

/// 控制面临期窗口：到期前 30 分钟提示（依据与 officeace2api 的续期提前量同级）。
pub const EXPIRY_MARGIN_MS: i64 = 30 * 60 * 1000;

/// 从账号记录解析凭证（**唯一入口**）。
///
/// 字段兼容三种粘贴形态（`pick` 的候选表）：
///   · 本仓落盘名：`baseUrl` / `modelAppKey` / `modelAppSecret`
///   · 桌面端 `models.json` 的字段：`api_base` / `api_key`
///   · 常见别名：`base_url` / `appKey` / `appSecret`
pub fn from_record(record: Option<&Value>) -> Result<OfficeAceCredential, GatewayError> {
    let Some(record) = record else {
        return Err(GatewayError::with_status(
            503,
            "没有可用的 OfficeAce 账号：请在账号页添加账号",
        ));
    };
    let credential = OfficeAceCredential {
        id: pick(record, &["id", "accountId"]),
        base_url: pick(record, &["baseUrl", "base_url", "api_base", "apiBase"]),
        model_app_key: pick(record, &["modelAppKey", "appKey", "model_app_key"]),
        model_app_secret: pick(record, &["modelAppSecret", "appSecret", "model_app_secret"]),
        access_key_id: pick(record, &["accessKeyId", "access_key_id", "accessKey"]),
        secret_access_key: pick(record, &["secretAccessKey", "secret_access_key", "secretKey"]),
        security_token: pick(record, &["securityToken", "security_token"]),
        project_id: pick(record, &["projectId", "project_id"]),
        expires_at: record.get("expiresAt").and_then(Value::as_i64).unwrap_or(0),
    };
    if !credential.can_forward() {
        return Err(GatewayError::with_status(
            401,
            "OfficeAce 账号缺少网关凭据（需要 baseUrl 与 modelAppKey/modelAppSecret 两项）",
        ));
    }
    Ok(credential)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_the_house_field_names() {
        let record = json!({
            "id": "a1",
            "baseUrl": "https://gw.example.com",
            "modelAppKey": "k",
            "modelAppSecret": "s",
            "accessKeyId": "AK",
            "secretAccessKey": "SK",
            "securityToken": "ST",
            "projectId": "p",
            "expiresAt": 1_791_602_100_000i64
        });
        let credential = from_record(Some(&record)).expect("齐备的记录要能解析");
        assert_eq!("a1", credential.id);
        assert!(credential.can_forward());
        assert!(credential.has_control_plane());
        assert_eq!(Some("k"), Some(credential.model_app_key.as_str()));
        let signing = credential.signing().expect("控制面凭据齐备时能给签名凭据");
        assert_eq!("AK", signing.access_key_id);
    }

    #[test]
    fn accepts_desktop_field_names() {
        // 桌面端 models.json 的写法（api_base + api_key + custom_headers 里的 Basic 由
        // 用户拆出来填；这里只验字段别名能读到）
        let record = json!({
            "api_base": "https://modelgw-0004.example.com/v2",
            "appKey": "k",
            "appSecret": "s",
        });
        let credential = from_record(Some(&record)).expect("别名形态要能解析");
        assert_eq!("https://modelgw-0004.example.com/v2", credential.base_url);
        assert_eq!("k", credential.model_app_key);
        assert_eq!("s", credential.model_app_secret);
        // 只有网关凭据，控制面那一半为空
        assert!(!credential.has_control_plane());
        assert!(credential.signing().is_err(), "没控制面凭据时签名要给错");
    }

    #[test]
    fn rejects_records_without_gateway_credentials() {
        let record = json!({ "baseUrl": "https://gw.example.com" });
        let error = from_record(Some(&record)).expect_err("缺网关凭据要报错");
        assert!(error.to_string().contains("网关凭据"), "{}", error);
        let error = from_record(None).expect_err("没有账号要报错");
        assert!(error.to_string().contains("没有可用的 OfficeAce 账号"), "{error}");
    }

    #[test]
    fn control_plane_expiry_uses_the_margin() {
        let now = crate::server::logging::now_ms();
        let mut credential = OfficeAceCredential {
            access_key_id: "AK".to_string(),
            secret_access_key: "SK".to_string(),
            ..Default::default()
        };
        credential.expires_at = now + 10 * 60 * 1000;
        assert!(credential.control_plane_expiring(), "只剩 10 分钟算临期");
        credential.expires_at = now + 2 * 3600 * 1000;
        assert!(!credential.control_plane_expiring(), "还剩两小时不算临期");
        credential.expires_at = 0;
        assert!(!credential.control_plane_expiring(), "未知到期时刻不报临期");
    }
}
