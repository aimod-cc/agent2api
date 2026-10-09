//! Loomy 凭证：**只有一条来源** —— 账号记录里的登录 session。
//!
//! ── 与别家最大的不同：没有续期，也没有桌面端导入 ─────────────
//! 客户端实测（`electron/xfyun/account-service.js` + 全包检索）：
//!   - 登录接口的 `expire` 是 14 天，**没有任何 refresh / renew 接口**；
//!     过期（业务码 `020002` / `100002`）只能重新走短信登录。
//!   - 登录态只存在于客户端自己的存储里（Electron 会话 / 加密区），
//!     没有 `auth.json` 那种稳定可读的文件形态 —— 因此不提供「导入桌面端
//!     登录态」入口（与 Accio / ZCode 同一处境，不要照抄 AutoClaw 那边）。
//!
//! 所以本模块比 `autoclaw::credentials` 薄得多：从账号记录取 session、判临期。
//! 临期判定用**登录时间 + 14 天**估算（上游不返回明确过期时刻），提前 1 天报临期。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::Value;

use crate::server::errors::GatewayError;

/// 会话有效期（登录请求里的 `expire`，单位秒 → 毫秒）
pub const SESSION_TTL_MS: i64 = 14 * 24 * 3600 * 1000;

/// 临期窗口：到期前 1 天提示重新登录（上游无续期，只能重登）
pub const EXPIRY_MARGIN_MS: i64 = 24 * 3600 * 1000;

/// 一次转发 / 查询用的 Loomy 凭证快照。
#[derive(Clone, Debug)]
pub struct LoomyCredentials {
    /// 账号记录 id
    pub id: String,
    /// 登录 session（**同时是**模型网关的 `token`）
    pub session: String,
    /// 讯飞侧 userid
    pub userid: String,
    /// 手机号（可选，展示用）
    pub phone: String,
    /// 估算的过期时刻（毫秒；来自记录 `expiresAt`，缺失时用 `addedAt + 14 天`）
    pub expires_at: i64,
    /// 展示名（记录的备注名）
    pub name: String,
}

impl LoomyCredentials {
    /// 是否已过期或进入临期窗口（上游无续期手段，这个判定只用于提示重登）
    pub fn expiring(&self) -> bool {
        self.expires_at > 0 && self.expires_at - EXPIRY_MARGIN_MS <= crate::server::logging::now_ms()
    }
}

/// 从一条取值链里挑第一个非空字符串（记录可能被手工编辑过，宽容读取）
fn pick<'a>(record: &'a Value, keys: &[&str]) -> String {
    for key in keys {
        let value = record
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if !value.is_empty() {
            return value.to_string();
        }
    }
    String::new()
}

/// 从账号记录解析凭证（**唯一入口**；`record` 为 None 时报「没有可用账号」）。
///
/// 字段兼容：`accessToken`（本项目落盘名）/ `token` / `session`（粘贴形态）。
pub fn from_record(record: Option<&Value>) -> Result<LoomyCredentials, GatewayError> {
    let Some(record) = record else {
        return Err(GatewayError::with_status(
            401,
            "没有可用的 Loomy 账号：请在账号页用手机号登录或粘贴 session 添加",
        ));
    };
    let session = pick(record, &["accessToken", "token", "session"]);
    if session.is_empty() {
        return Err(GatewayError::with_status(
            401,
            "Loomy 账号缺少 session，请重新登录或粘贴凭证",
        ));
    }
    let userid = pick(record, &["userId", "userid", "user_id"]);
    let phone = pick(record, &["phone"]);
    let name = pick(record, &["name"]);
    let id = pick(record, &["id"]);
    let explicit_expiry = record.get("expiresAt").and_then(Value::as_i64).unwrap_or(0);
    let added_at = record.get("addedAt").and_then(Value::as_i64).unwrap_or(0);
    let expires_at = if explicit_expiry > 0 {
        explicit_expiry
    } else if added_at > 0 {
        added_at + SESSION_TTL_MS
    } else {
        0
    };
    Ok(LoomyCredentials {
        id,
        session,
        userid,
        phone,
        expires_at,
        name,
    })
}

/// 手机号脱敏（保留前 3 后 4；与 `autoclaw::login` 的同名函数同口径）
///
/// ── 为什么按 `char` 而不按字节切 ────────────────────────────────
/// `POST /api/accounts` 的 `phone` 是**请求体原样**进来的（`account_store` 侧只
/// `trim` + 按字符数截断，不校验格式），所以它完全可能不是 11 位 ASCII 数字。
/// 按字节回切 `&phone[phone.len() - 4..]` 就会落在多字节字符中间 —— release 是
/// `panic = "abort"`，一个带汉字的 `phone` 就能让网关整体退出。按字符取则与
/// 内容无关地安全（本仓库同款安全写法：`store_util.rs` 的 `token_tail`）。
/// 截断口径不变：仍保留前 3 个字符与后 4 个字符。
pub fn mask_phone(phone: &str) -> String {
    let chars: Vec<char> = phone.chars().collect();
    if chars.len() < 7 {
        return phone.to_string();
    }
    let head: String = chars[..3].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}****{tail}")
}

#[cfg(test)]
mod tests {
    use super::mask_phone;

    /// 回归：非 ASCII 的 `phone` 不得 panic。
    ///
    /// `phone` 来自 `POST /api/accounts` 的请求体（`account_store` 只 `trim` + 按
    /// 字符数截断，不校验格式），所以「11 位数字 + 汉字」是合法输入。旧实现按字节
    /// 回切 `&phone[phone.len() - 4..]`，第 16 字节落在 `你` 的续字节（0xa0）上
    /// → `panic: not a char boundary` → `panic = "abort"` 整进程退出。
    #[test]
    fn mask_phone_survives_multibyte_input() {
        // 20 字节 / 14 字符：11 个 ASCII 数字 + 3 个汉字。
        // 旧实现切在第 16 字节（第二个 `你` 的续字节 0xa0）→ panic。
        // 修复后按字符取：head = 前 3 个 `1`，tail = 第 11 个 `1` + 三个汉字。
        assert_eq!(mask_phone("11111111111你你你"), "111****1你你你");
        // 23 字节 / 15 字符：11 个 ASCII 数字 + 4 个汉字。
        // 旧实现切在第 19 字节（第四个 `你` 的续字节）→ panic。
        assert_eq!(mask_phone("11111111111你你你你"), "111****你你你你");
    }

    /// 正常手机号口径不变：保留前 3 后 4。
    #[test]
    fn mask_phone_keeps_three_and_four_for_ascii() {
        assert_eq!(mask_phone("13800138000"), "138****8000");
    }

    /// 少于 7 个**字符**（不是字节）时原样返回，且不得因字节数够而误切。
    #[test]
    fn mask_phone_returns_short_input_unchanged() {
        assert_eq!(mask_phone("123456"), "123456");
        // 6 个字符但 18 字节：按字节判会误以为「够长」而去切
        assert_eq!(mask_phone("你你你你你你"), "你你你你你你");
    }
}
