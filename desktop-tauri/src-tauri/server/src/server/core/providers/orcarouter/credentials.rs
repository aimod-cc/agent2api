//! OrcaRouter 账号记录的**读写形状**：一把 API Key + 几个展示字段。
//!
//! ── 为什么这一家这么薄 ──────────────────────────────────────
//! 其余各家的凭证是一整套生命周期（access/refresh 对、JWT 里的 exp、设备绑定），
//! 因此各自有 credentials / refresh / auth 三个模块。OrcaRouter 只有**一把长期
//! API Key**：没有 refresh grant、没有过期时间、没有设备指纹。于是这里只回答
//! 四个问题：怎么校验、怎么归一、怎么从上游响应里取出、公开形态给哪些键。
//!
//! ── 账号记录形状 ───────────────────────────────────────────
//! ```json
//! {
//!   "id": "orcarouter-8f2c1d4a6b90",   // key 的 SHA-256 前 12 位（同 key 合并）
//!   "provider": "orcarouter",
//!   "name": "OrcaRouter 账号 abcd",     // 备注名
//!   "apiKey": "sk-orca-…",             // 唯一凭证来源（两条入口都落这里）
//!   "tokenTail": "…abcd",              // 界面展示尾号
//!   "source": "manual" | "oauth",       // 手填 / PKCE 网页登录（只影响来源标签）
//!   "userId": "12345",                 // 换码响应里的 user_id（有则存，仅展示）
//!   "scope": "api",                    // 换码响应里**实际授予**的 scope
//!   "priority": 5,
//!   "enabled": true,
//!   "addedAt": 1730000000000,
//!   "updatedAt": 1730000000000
//! }
//! ```
//!
//! ── 硬约束 ────────────────────────────────────────────────
//! 绝不 unwrap/expect/panic；绝不把 Key 写进日志或错误（错误里只说尾号）。

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::server::logging;
use crate::server::core::account_store::store_util::token_tail_of;

use super::{API_KEY_PREFIX, SOURCE_MANUAL, SOURCE_PKCE};

/// Key 的长度上限（防御：粘贴进一整段文档时不落盘）。
/// 上游 Key 一般 40~120 字符，8192 与 `custom_accounts::MAX_API_KEY_LENGTH` 同量级。
pub const MAX_API_KEY_LENGTH: usize = 8192;

/// 备注名长度上限（与各家一致）。
pub const MAX_NAME_CHARS: usize = 100;

/// 一次凭据校验的产物：归一后的 Key 与它派生的持久身份。
///
/// **两条入口共用这一个类型**：手填（`from_manual`）与 PKCE（`from_exchange`）
/// 都产出 [`Credentials`]，落账号的是同一个函数 —— 于是下游（转发 / 目录 /
/// 模型下拉）无从分辨凭据来源，这是本集成要求的不变量。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Credentials {
    /// 归一后的 Key（trim 过；调用方保证非空）
    pub api_key: String,
    /// 备注名（可为空 —— 调用方给兜底名）
    pub name: String,
    /// 上游换码响应里的 `user_id`（手填路径为空）
    pub user_id: String,
    /// 换码响应里**实际授予**的 scope（手填路径为空）
    pub scope: String,
    /// 凭据获取方式（[`SOURCE_MANUAL`] / [`SOURCE_PKCE`]）
    pub source: String,
}

impl Credentials {
    /// 手填路径：从请求体里取 `apiKey`。
    ///
    /// 只做**形状**校验（非空 / 长度 / 去空白）。规范明确说 `sk-orca-…` 前缀不构成
    /// 「这把 Key 能用」的证明，而且上游**没有**稳定、不计费的校验接口 ——
    /// 因此这里如实把「有效性未知」留给第一次真实请求，绝不为让表单显示
    /// 「有效」而发一次计费推理。前缀只用来挡「把整段文档粘进来」这类手滑。
    pub fn from_manual(payload: &Value) -> Result<Self, String> {
        let object = payload
            .as_object()
            .ok_or_else(|| "账号内容必须是 JSON 对象".to_string())?;
        let raw = object
            .get("apiKey")
            .or_else(|| object.get("api_key"))
            .or_else(|| object.get("key"))
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if raw.is_empty() {
            return Err("请填写 OrcaRouter API Key（sk-orca-…）".to_string());
        }
        if raw.chars().count() > MAX_API_KEY_LENGTH {
            return Err("OrcaRouter API Key 过长".to_string());
        }
        let name = object
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("")
            .to_string();
        Ok(Self {
            api_key: raw.to_string(),
            name,
            user_id: String::new(),
            scope: String::new(),
            source: SOURCE_MANUAL.to_string(),
        })
    }

    /// PKCE 路径：从换码响应里取 Key、`user_id`、`scope`。
    ///
    /// `scope` 是**授予**而不是请求的那个值（规范「Read `scope` back」）：
    /// 用户可能只被批准了比请求更窄的授权，因此把响应里的值存下来，
    /// 由调用方比对是否需要提示（见 [`scope_is_sufficient`]）。
    pub fn from_exchange(payload: &Value, name: Option<&str>) -> Result<Self, String> {
        let object = payload
            .as_object()
            .ok_or_else(|| "换码响应不是 JSON 对象".to_string())?;
        let key = object
            .get("key")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if key.is_empty() {
            return Err("换码响应里没有 key：授权可能未完成，请重新发起登录".to_string());
        }
        if key.chars().count() > MAX_API_KEY_LENGTH {
            return Err("换码响应里的 key 过长，已拒绝落盘".to_string());
        }
        Ok(Self {
            api_key: key.to_string(),
            name: name
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("")
                .to_string(),
            user_id: object
                .get("user_id")
                .map(|value| match value {
                    Value::String(text) => text.trim().to_string(),
                    other => other.to_string(),
                })
                .filter(|text| !text.is_empty() && text != "null")
                .unwrap_or_default(),
            scope: object
                .get("scope")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("")
                .to_string(),
            source: SOURCE_PKCE.to_string(),
        })
    }

    /// 实际授予的 scope 是否够本网关用（推理 + 模型目录）。
    ///
    /// `api` 是规范里推理用的那一档；`connector` 是更宽的一档（也覆盖推理）。
    /// 响应里**没有** `scope` 字段时视为未声明（不拦，由第一次请求决定）——
    /// 上游的公开契约里这个字段一直在，真缺失说明对方版本不同，此时报错会把
    /// 一个本来能用的登录挡在门外。
    pub fn scope_is_sufficient(&self) -> bool {
        if self.scope.is_empty() {
            return true;
        }
        matches!(self.scope.as_str(), "api" | "connector")
    }

    /// 账号 id：`orcarouter-` + Key 的 SHA-256 前 12 位。
    ///
    /// 同 Key 重复添加**合并**成一条记录（与自定义账号的 `custom-acct-`、
    /// Qoder 的 `qoder-<region>-<hash>` 同一语义）：用户粘错重试、或先手填后
    /// 又走了一遍 PKCE 拿到同一把 Key（不可能——每次授权签发新 Key），都不会
    /// 堆出重复账号。
    pub fn account_id(&self) -> String {
        let digest = Sha256::digest(self.api_key.as_bytes());
        let hex = format!("{digest:x}");
        let short: String = hex.chars().take(12).collect();
        format!("{}{short}", super::credentials::ID_PREFIX)
    }

    /// Key 尾号（界面展示用；绝不展示全量）。
    pub fn token_tail(&self) -> String {
        token_tail_of(&self.api_key)
    }

    /// 兜底备注名（调用方没给名字时用）。
    pub fn default_name(&self) -> String {
        let tail = self.token_tail();
        if tail.is_empty() {
            "OrcaRouter 账号".to_string()
        } else {
            format!("OrcaRouter {tail}")
        }
    }

    /// 把凭据写进账号记录字段表（**在调用方给的既有字段表上原地合并**，
    /// 保持项目「未知字段全量保留」的不变量）。
    ///
    /// 空值不覆盖既有内容：重新登录时响应可能只带部分字段，把上次的备注名 /
    /// `userId` 洗掉会让用户在账号列表里认不出自己那条。
    pub fn apply_to_fields(&self, fields: &mut Map<String, Value>, provider_id: &str) {
        for (key, value) in [
            ("apiKey", Value::String(self.api_key.clone())),
            ("tokenTail", Value::String(self.token_tail())),
            ("source", Value::String(self.source.clone())),
        ] {
            fields.insert(key.to_string(), value);
        }
        if !self.user_id.is_empty() {
            fields.insert("userId".to_string(), Value::String(self.user_id.clone()));
        }
        if !self.scope.is_empty() {
            fields.insert("scope".to_string(), Value::String(self.scope.clone()));
        }
        fields.insert("provider".to_string(), Value::String(provider_id.to_string()));
    }

    /// 归一 Key 的形态描述（日志用：**只出现尾号与长度**，绝不出现 Key 本身）。
    pub fn describe(&self) -> String {
        let prefix_ok = self.api_key.starts_with(API_KEY_PREFIX);
        format!(
            "key{}，长度 {}，尾号 {}",
            if prefix_ok { "" } else { "（不以 sk-orca- 开头，可能是自建实例的 Key）" },
            self.api_key.chars().count(),
            self.token_tail()
        )
    }
}

/// 账号 id 前缀（与 `custom-acct-`、`qoder-` 等同档：**账号** id，不是 provider id）。
pub const ID_PREFIX: &str = "orcarouter-";

/// 归一 Key 时统一走这里，避免各处各写一遍 trim / 长度判定。
pub fn normalize_api_key(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("OrcaRouter API Key 为空".to_string());
    }
    if trimmed.chars().count() > MAX_API_KEY_LENGTH {
        return Err("OrcaRouter API Key 过长".to_string());
    }
    Ok(trimmed.to_string())
}

/// Key 的**非敏感**指纹（日志与请求日志关联同一把 Key 时用；不可反推）。
pub fn fingerprint(api_key: &str) -> String {
    let digest = Sha256::digest(api_key.as_bytes());
    let hex = format!("{digest:x}");
    hex.chars().take(8).collect()
}

/// 记录一条凭据落地日志（**只输出尾号与来源**）。
pub fn log_saved(source: &str, key_tail: &str, is_update: bool) {
    logging::log(
        "[Accounts]",
        &format!(
            "{} OrcaRouter 账号（{}，尾号 {}）",
            if is_update { "🔄 已更新" } else { "✅ 已添加" },
            if source == SOURCE_PKCE { "网页登录" } else { "手填 Key" },
            key_tail
        ),
    );
}

#[cfg(test)]
mod tests {
    //! 凭据形状的验收测试。本集成的核心不变量在这里被逐条钉住：
    //! **两条入口产出同一种 `Credentials`**，下游无从分辨来源。
    use super::*;
    use serde_json::json;

    /// 测试用假 Key（**绝不是真 Key**；前缀只是让形状校验走通）。
    const FAKE_KEY: &str = "sk-orca-0000000000000000000000000000000000test";

    #[test]
    fn manual_and_exchange_produce_the_same_credential_shape() {
        let manual = Credentials::from_manual(&json!({ "apiKey": FAKE_KEY })).expect("手填");
        let exchanged = Credentials::from_exchange(
            &json!({ "key": FAKE_KEY, "user_id": "12345", "scope": "api" }),
            None,
        )
        .expect("换码");
        // 同一把 Key → 同一个账号 id（同 Key 合并成一条记录）
        assert_eq!(manual.account_id(), exchanged.account_id());
        assert_eq!(manual.api_key, exchanged.api_key);
        assert_eq!(manual.token_tail(), exchanged.token_tail());
        // 只有「来源」这一个字段不同：它只影响界面上的来源标签
        assert_eq!(manual.source, SOURCE_MANUAL);
        assert_eq!(exchanged.source, SOURCE_PKCE);
        assert_ne!(manual.user_id, exchanged.user_id);
    }

    #[test]
    fn both_entry_points_write_the_same_credential_keys() {
        let manual = Credentials::from_manual(&json!({ "apiKey": FAKE_KEY })).expect("手填");
        let exchanged = Credentials::from_exchange(&json!({ "key": FAKE_KEY }), None).expect("换码");
        let mut left = Map::new();
        let mut right = Map::new();
        manual.apply_to_fields(&mut left, "orcarouter");
        exchanged.apply_to_fields(&mut right, "orcarouter");
        // 凭据键逐字相同（source 是唯一的例外，见上一条）
        for key in ["apiKey", "tokenTail", "provider"] {
            assert_eq!(left.get(key), right.get(key), "{key} 必须同形");
        }
        assert_eq!(left.get("apiKey").and_then(Value::as_str), Some(FAKE_KEY));
        assert_eq!(left.get("source").and_then(Value::as_str), Some(SOURCE_MANUAL));
        assert_eq!(right.get("source").and_then(Value::as_str), Some(SOURCE_PKCE));
    }

    #[test]
    fn manual_accepts_the_documented_spellings_and_rejects_broken_input() {
        for payload in [
            json!({ "apiKey": FAKE_KEY }),
            json!({ "api_key": FAKE_KEY }),
            json!({ "key": FAKE_KEY }),
            json!({ "apiKey": format!("  {FAKE_KEY}  ") }),
        ] {
            let credentials = Credentials::from_manual(&payload).expect("应当接受");
            assert_eq!(credentials.api_key, FAKE_KEY, "必须 trim 归一");
        }
        assert!(Credentials::from_manual(&json!({ "apiKey": "   " })).is_err());
        assert!(Credentials::from_manual(&json!({ "name": "x" })).is_err());
        assert!(Credentials::from_manual(&json!("not an object")).is_err());
        assert!(Credentials::from_manual(&json!({ "apiKey": "x".repeat(MAX_API_KEY_LENGTH + 1) })).is_err());
    }

    #[test]
    fn exchange_requires_a_key_and_reads_the_granted_scope() {
        assert!(Credentials::from_exchange(&json!({}), None).is_err());
        assert!(Credentials::from_exchange(&json!({ "key": "  " }), None).is_err());
        let credentials = Credentials::from_exchange(
            &json!({ "key": FAKE_KEY, "user_id": 12345, "scope": "connector" }),
            Some("工作区 A"),
        )
        .expect("换码");
        assert_eq!(credentials.name, "工作区 A");
        // user_id 是数字形态也照收（规范化成字符串，仅展示）
        assert_eq!(credentials.user_id, "12345");
        assert_eq!(credentials.scope, "connector");
        assert!(credentials.scope_is_sufficient());
    }

    #[test]
    fn scope_downgrade_is_detected_but_missing_scope_is_not_blocking() {
        let sufficient = |scope: &str| {
            Credentials::from_exchange(&json!({ "key": FAKE_KEY, "scope": scope }), None)
                .expect("换码")
                .scope_is_sufficient()
        };
        assert!(sufficient("api"));
        assert!(sufficient("connector"));
        assert!(sufficient(""), "响应未声明 scope 时不拦（由第一次真实请求定论）");
        // 窄授权：用户被批了比请求更小的范围，必须如实拒绝
        assert!(!sufficient("chat"));
        assert!(!sufficient("models:read"));
        assert!(!sufficient("api.chat"));
    }

    #[test]
    fn account_id_is_derived_from_the_key_and_merges_repeats() {
        let first = Credentials::from_manual(&json!({ "apiKey": FAKE_KEY })).expect("手填");
        let again = Credentials::from_manual(&json!({ "apiKey": FAKE_KEY })).expect("手填");
        assert_eq!(first.account_id(), again.account_id(), "同一把 Key 必须合并成一条");
        assert!(first.account_id().starts_with(ID_PREFIX));
        assert_eq!(first.account_id().len(), ID_PREFIX.len() + 12);
        let other = Credentials::from_manual(&json!({ "apiKey": "sk-orca-other" })).expect("手填");
        assert_ne!(first.account_id(), other.account_id());
        // id 里不得出现 Key 的任何片段
        assert!(!first.account_id().contains("test"));
    }

    #[test]
    fn masking_never_exposes_the_key() {
        let credentials = Credentials::from_manual(&json!({ "apiKey": FAKE_KEY })).expect("手填");
        let description = credentials.describe();
        assert!(!description.contains(FAKE_KEY), "形态描述不得含明文 Key");
        assert!(!description.contains("000000"), "连中段也不行");
        assert!(description.contains(credentials.token_tail().as_str()));
        // 尾号本身不是全量
        assert_ne!(credentials.token_tail(), FAKE_KEY);
        assert!(FAKE_KEY.ends_with(&credentials.token_tail()));

        let default_name = credentials.default_name();
        assert!(!default_name.contains(FAKE_KEY));
        // 指纹不可反推：长度固定、且与 Key 不同
        let digest = fingerprint(FAKE_KEY);
        assert_eq!(digest.len(), 8);
        assert_ne!(digest, FAKE_KEY);
        assert_eq!(digest, fingerprint(FAKE_KEY), "同一把 Key 的指纹必须稳定");
        assert_ne!(digest, fingerprint("sk-orca-other"));
    }

    #[test]
    fn apply_to_fields_keeps_previous_values_when_the_new_response_is_sparse() {
        let mut fields = Map::new();
        fields.insert("name".to_string(), Value::String("我自己起的名字".to_string()));
        fields.insert("userId".to_string(), Value::String("999".to_string()));
        let credentials = Credentials::from_exchange(&json!({ "key": FAKE_KEY }), None).expect("换码");
        credentials.apply_to_fields(&mut fields, "orcarouter");
        assert_eq!(fields.get("name").and_then(Value::as_str), Some("我自己起的名字"), "空值不得洗掉既有内容");
        assert_eq!(fields.get("userId").and_then(Value::as_str), Some("999"));
        assert_eq!(fields.get("apiKey").and_then(Value::as_str), Some(FAKE_KEY));
    }

    #[test]
    fn normalize_api_key_trims_and_bounds() {
        assert_eq!(normalize_api_key("  sk-orca-x  ").expect("合法"), "sk-orca-x");
        assert!(normalize_api_key("  ").is_err());
        assert!(normalize_api_key(&"x".repeat(MAX_API_KEY_LENGTH + 1)).is_err());
    }
}
