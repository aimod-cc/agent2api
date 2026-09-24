//! Accio 凭证格式：`accessToken` / `refreshToken` / `expiresAt` + 账号标识。
//!
//! ── 上游给的东西（桌面端 `/api/oauth/token` 与 `/api/auth/refresh_token`
//!    的返回）──────────────────────────────────────────────────
//! ```jsonc
//! { "accessToken": "…", "refreshToken": "…", "expiresAt": 1789…, "sid": "…", "sgcookie": "…" }
//! ```
//! `sid` / `sgcookie` 是浏览器侧会话的伴随标识，桌面端只用于「账号切换」
//! （`/api/auth/safe/refresh_token` 那条多账号链路）。本网关的续期走普通那条，
//! 因此**不落**这两个字段 —— 少存两个用不上的敏感值。
//!
//! ── 身份用什么标识 ──────────────────────────────────────────
//! Accio 的 token 是**不透明串**（不是 JWT，解不出 userId），因此账号身份由
//! `/api/auth/userinfo` 查回的资料里的 id 决定（见 `auth::fetch_profile`）。
//! 「粘贴凭证」这条路查一次资料拿到 id；查不到时退回按凭证指纹生成 id ——
//! 至少能保证同一串凭证重复添加时命中同一条记录，不会堆出一串重复账号。
//!
//! ── `deviceId`（utdid）是干嘛的 ─────────────────────────────
//! 推理请求要带 `utdid`（设备指纹）。桌面端每台机器一个（装机时生成、落盘）。
//! 本网关按**账号**生成并落进记录：多账号池化时每个账号带自己的指纹，
//! 与「一个账号一台设备」的真实形态一致（同一指纹挂十个账号才是可疑形态）。
//! 它不影响鉴权，缺省也能跑（推理头里 utdid 是可选的），但带上更接近真实客户端。

use serde_json::{json, Value};

use crate::server::core::account_store::MAX_TOKEN_LENGTH;
use crate::server::errors::GatewayError;
use crate::server::logging;
use crate::server::core::upstream::request::new_request_id;

use super::endpoints::Region;

/// 提前多久认为 token 该续期（与另外几家同一量级）
pub const REFRESH_MARGIN_MS: i64 = 5 * 60 * 1000;

#[derive(Clone)]
pub struct Credentials {
    pub region: Region,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: Option<i64>,
    /// 上游账号标识（userinfo 的 id / userId）
    pub user_id: String,
    pub email: String,
    pub name: String,
    /// 推理请求的设备指纹（本网关按账号生成）
    pub device_id: String,
}

impl Credentials {
    /// 从账号记录 / 添加请求解析凭证。
    ///
    /// 接受的键名与其它家保持一致的宽口径（`accessToken` / `access_token` /
    /// `token`），前端「填写凭证」与前端的导入路径都可能给不同形态。
    pub fn from_payload(payload: &Value) -> Result<Self, GatewayError> {
        if !payload.is_object() {
            return Err(GatewayError::with_status(400, "Accio 账号内容必须是 JSON 对象"));
        }
        let region = Region::from_payload(payload)?;
        let access_token = secret(payload, &["accessToken", "access_token", "token"])?;
        if access_token.is_empty() {
            return Err(GatewayError::with_status(400, "缺少 Accio accessToken"));
        }
        let refresh_token = secret(payload, &["refreshToken", "refresh_token"])?;
        let expires_at = ["expiresAt", "expires_at", "expires"]
            .iter()
            .find_map(|key| timestamp(payload.get(*key)));
        Ok(Self {
            region,
            access_token,
            refresh_token,
            expires_at,
            user_id: text(payload, &["userId", "user_id", "uid"]),
            email: text(payload, &["email"]).chars().take(320).collect(),
            name: text(payload, &["nickname", "name"]).chars().take(100).collect(),
            device_id: text(payload, &["deviceId", "device_id"]),
        })
    }

    /// 新账号的兜底标识：没有 userId 时按凭证指纹生成一个稳定串。
    ///
    /// 用 `new_request_id()`（项目里唯一的不可预测随机源，同 `raccoon::oauth`
    /// 的手法）而不是凭证本身的摘要：id 会落进账号 id 与日志，**不该可反推
    /// 凭证**。代价是同一串凭证重复添加会得到不同的兜底 id —— 由
    /// `add_accio_account` 的「同 provider 内按 userId 去重」兜住（userId 缺失
    /// 时它按 accessToken 相等去重，见那边的实现）。
    pub fn ensure_identity(&mut self) {
        if self.device_id.is_empty() {
            self.device_id = format!("desktop-{}", new_request_id().replace('-', ""));
        }
    }

    /// 有刷新凭证 + 有到期时间，且快到点了
    pub fn expiring(&self) -> bool {
        self.can_refresh()
            && self
                .expires_at
                .is_some_and(|expiry| expiry <= logging::now_ms() + REFRESH_MARGIN_MS)
    }

    pub fn can_refresh(&self) -> bool {
        !self.refresh_token.is_empty()
    }

    /// 落进账号记录的形态。空串按 null 落（`add_accio_account` 的合并规则是
    /// 「空值不覆盖」，缺项不该把既有值洗成空）。
    pub fn to_value(&self) -> Value {
        let text_or_null = |value: &str| -> Value {
            if value.is_empty() { Value::Null } else { Value::String(value.to_string()) }
        };
        json!({
            "mode": self.region.edition(),
            "accessToken": text_or_null(&self.access_token),
            "refreshToken": text_or_null(&self.refresh_token),
            "expiresAt": self.expires_at,
            "userId": text_or_null(&self.user_id),
            "email": text_or_null(&self.email),
            "nickname": text_or_null(&self.name),
            "deviceId": text_or_null(&self.device_id),
        })
    }
}

/// 取一个文本字段（空串按缺失处理）
pub fn text(payload: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| match payload.get(*key) {
            Some(Value::String(value)) if !value.trim().is_empty() => {
                Some(value.trim().to_string())
            }
            Some(Value::Number(value)) => Some(value.to_string()),
            _ => None,
        })
        .unwrap_or_default()
}

/// 取一个密钥字段：去 `Bearer ` 前缀、限长、拒绝控制字符。
pub fn secret(payload: &Value, keys: &[&str]) -> Result<String, GatewayError> {
    let mut value = keys
        .iter()
        .find_map(|key| {
            payload
                .get(*key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
        .unwrap_or("");
    if value.get(..7).is_some_and(|prefix| prefix.eq_ignore_ascii_case("Bearer ")) {
        value = value[7..].trim();
    }
    if value.len() > MAX_TOKEN_LENGTH || value.chars().any(char::is_control) {
        return Err(GatewayError::with_status(400, "Accio 凭证过长或包含非法控制字符"));
    }
    Ok(value.to_string())
}

/// 时间戳（毫秒）；接受秒级数字与 RFC3339 字符串。0 / 负数 / 解析不出 → None。
pub fn timestamp(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    let number = value
        .as_i64()
        .or_else(|| value.as_str()?.trim().parse::<i64>().ok());
    if let Some(number) = number.filter(|value| *value > 0) {
        return if number < 100_000_000_000 {
            number.checked_mul(1000)
        } else {
            Some(number)
        };
    }
    chrono::DateTime::parse_from_rfc3339(value.as_str()?)
        .ok()
        .map(|time| time.timestamp_millis())
}
