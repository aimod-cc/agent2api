//! OfficeAce 账号：手工导入（网关 Basic 凭据）与自助 OAuth 落账号共用入口。
//!
//! ── 账号字段 ────────────────────────────────────────────────
//! ```text
//!   { id, provider:"officeace", name,
//!     baseUrl,                                  模型网关基址
//!     modelAppKey, modelAppSecret,              网关凭据（转发用，不过期）
//!     accessKeyId, secretAccessKey,             控制面临时凭据（可缺）
//!     securityToken, projectId, expiresAt,
//!     addedAt, updatedAt, source, priority, enabled }
//! ```
//! 前三个（`baseUrl` + 两个 key）是转发的最低要求；控制面那一半只有自助 OAuth
//! 会给（手工导入若只给网关凭据，额度/签到与自动续期就不可用 —— 这是允许的，
//! 不影响转发）。
//!
//! ── id 形态 ────────────────────────────────────────────────
//! `officeace-<hash>`：hash 取（规范化基址 + app key）的 FNV-1a 64 位前 12 位 ——
//! 同一套凭据重复导入落同一条 id（就地更新），换 key 或换基址就是新账号。
//!
//! ── 保存路径 ────────────────────────────────────────────────
//! 走 `StoredAccount::from_map` + `sql::put`（与 loomy 同款），**未知字段全量保留**
//! —— 用户手工加过的字段不能因为一次「更新账号」丢掉。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::{Map, Value};

use crate::server::core::account_store::priority::next_free_priority;
use crate::server::core::account_store::sql;
use crate::server::core::account_store::state::{mark_name_custom, StoredAccount};
use crate::server::core::account_store::{AccountStore, AccountStoreError, OFFICEACE_PROVIDER_ID};
use crate::server::logging;

/// 备注名长度上限（与别家同一口径）
const MAX_NAME_LENGTH: usize = 100;
/// 单个字段长度上限（URL 与两个 key 都用这个）
const MAX_FIELD_LENGTH: usize = 8192;

fn truncate_chars(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    value.chars().take(max).collect()
}

fn pick(payload: &Value, keys: &[&str]) -> String {
    for key in keys {
        if let Some(value) = payload.get(*key).and_then(Value::as_str) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    String::new()
}

/// FNV-1a 64 位（`&str` → 16 位 hex，取前 12 位）。
fn short_hash(input: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")[..12].to_string()
}

/// 「网关基址 + app key」→ 账号 id（同一套凭据重复导入落同一条）。
pub fn account_id_for(base_url: &str, app_key: &str) -> String {
    format!(
        "officeace-{}",
        short_hash(&format!(
            "{}|{}",
            crate::server::core::providers::officeace::chat::normalize_base(base_url),
            app_key.trim()
        ))
    )
}

impl AccountStore {
    /// 取一条 OfficeAce 账号记录（公开形态的原始 JSON）。
    ///
    /// `account_id` 非空 → 按 id 直查（不属于本家返回 None）；
    /// 为空 → 本家**启用中**账号里优先级最靠前的一条（转发默认用队首账号）。
    pub fn officeace_account_record(&self, account_id: &str) -> Option<Value> {
        let guard = self.guard();
        if !account_id.is_empty() {
            let record = self.record_by_id(&guard, account_id)?;
            return (record.provider() == OFFICEACE_PROVIDER_ID).then(|| record.to_value());
        }
        let mut candidates: Vec<StoredAccount> = self
            .records_for_provider(&guard, OFFICEACE_PROVIDER_ID)
            .into_iter()
            .filter(StoredAccount::enabled)
            .collect();
        candidates.sort_by_key(StoredAccount::order_key);
        candidates.into_iter().next().map(|item| item.to_value())
    }

    /// 写回「奖励领取台账」（存在账号记录的 `bonusClaims` 字段里，整份覆盖）。
    ///
    /// 与 CodeArts 的 `put_codearts_welfare_ledger` 同一形态：只写自己这一个命名
    /// 空间，且记录**在锁内现读** —— 既洗不掉凭据等别的字段，也不存在「快照过期」
    /// 那类问题（上一版 CodeArts 曾复用凭据写回的比对，代价见那里的注释）。
    pub fn put_officeace_bonus_ledger(
        &self,
        account_id: &str,
        ledger: &Value,
    ) -> Result<(), AccountStoreError> {
        let guard = self.guard();
        let Some(mut record) = self
            .record_by_id(&guard, account_id)
            .filter(|record| record.provider() == OFFICEACE_PROVIDER_ID)
        else {
            return Err(AccountStoreError::new(
                "OfficeAce 账号已不存在，领取台账无处落盘",
                404,
            ));
        };
        record
            .fields_mut()
            .insert("bonusClaims".to_string(), ledger.clone());
        record.set_updated_at(logging::now_ms());
        self.with_conn(&guard, |conn| sql::update_in_place(conn, &record))?;
        Ok(())
    }

    /// 台账的读侧（没有台账返回 `None`）。
    pub fn officeace_bonus_ledger(&self, account_id: &str) -> Option<Value> {
        self.officeace_account_record(account_id)
            .and_then(|record| record.get("bonusClaims").cloned())
    }

    /// 添加/更新一个 OfficeAce 账号（手工导入与自助 OAuth 共用）。
    ///
    /// payload：
    ///   - `baseUrl` / `api_base`：模型网关基址（必填）；
    ///   - `modelAppKey` / `appKey` 与 `modelAppSecret` / `appSecret`：网关凭据（必填）；
    ///   - `accessKeyId` / `secretAccessKey` / `securityToken` / `projectId`：控制面临时
    ///     凭据（可选，自助 OAuth 会给；没给就沿用既有记录的值，不把 OAuth 换来的抹掉）；
    ///   - `expiresAt`：控制面凭据到期时刻（毫秒，可选）。
    pub fn add_officeace_account(
        &self,
        payload: &Value,
        name: Option<&str>,
    ) -> Result<Value, AccountStoreError> {
        let Some(object) = payload.as_object() else {
            return Err(AccountStoreError::new("上传内容必须是 JSON 对象", 400));
        };
        let base_url = pick(payload, &["baseUrl", "base_url", "api_base", "apiBase"]);
        if base_url.is_empty() {
            return Err(AccountStoreError::new(
                "缺少模型网关基址（baseUrl / api_base）",
                400,
            ));
        }
        let app_key = pick(payload, &["modelAppKey", "appKey", "model_app_key"]);
        let app_secret = pick(payload, &["modelAppSecret", "appSecret", "model_app_secret"]);
        if app_key.is_empty() || app_secret.is_empty() {
            return Err(AccountStoreError::new(
                "缺少网关凭据（modelAppKey 与 modelAppSecret 两项都要）",
                400,
            ));
        }
        for (label, value) in [
            ("基址", &base_url),
            ("网关 key", &app_key),
            ("网关 secret", &app_secret),
        ] {
            if value.chars().count() > MAX_FIELD_LENGTH {
                return Err(AccountStoreError::new(format!("{label}过长"), 400));
            }
        }
        let id = account_id_for(&base_url, &app_key);

        let guard = self.guard();
        let existing = self.record_by_id(&guard, &id);
        if let Some(existing) = existing.as_ref() {
            let existing_provider = existing.provider();
            if existing_provider != OFFICEACE_PROVIDER_ID {
                return Err(AccountStoreError::new(
                    format!(
                        "账号 id「{id}」已被{existing_provider}账号占用，无法添加同一凭据的\
                         OfficeAce 账号（请先处理那个账号）"
                    ),
                    400,
                ));
            }
        }
        let explicit_name = name
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| truncate_chars(value, MAX_NAME_LENGTH));
        let record_name = explicit_name
            .or_else(|| {
                existing
                    .as_ref()
                    .map(StoredAccount::name)
                    .filter(|value| !value.is_empty())
            })
            .unwrap_or_else(|| "OfficeAce 果办".to_string());
        let priority = match existing.as_ref() {
            Some(record) => record.priority(),
            None => {
                let used = self.with_conn(&guard, |conn| sql::priorities_all(conn))?;
                next_free_priority(&used)
            }
        };
        let now = logging::now_ms();

        let mut record = Map::new();
        record.insert("id".to_string(), Value::String(id.clone()));
        record.insert(
            "provider".to_string(),
            Value::String(OFFICEACE_PROVIDER_ID.to_string()),
        );
        record.insert("name".to_string(), Value::String(record_name.clone()));
        mark_name_custom(
            &mut record,
            name.is_some_and(|value| !value.trim().is_empty()),
            existing.as_ref(),
        );
        record.insert("baseUrl".to_string(), Value::String(base_url));
        record.insert("modelAppKey".to_string(), Value::String(app_key));
        record.insert("modelAppSecret".to_string(), Value::String(app_secret));
        // 控制面那一半：给了就存，没给沿用既有记录（重复导入不该抹掉 OAuth 的成果）
        let carry = |key: &str, candidates: &[&str]| -> Option<String> {
            let value = pick(payload, candidates);
            if !value.is_empty() {
                return Some(value);
            }
            existing
                .as_ref()
                .and_then(|record| record.fields().get(key).cloned())
                .and_then(|value| value.as_str().map(str::to_string))
        };
        for (key, candidates) in [
            ("accessKeyId", &["accessKeyId", "access_key_id", "accessKey"][..]),
            (
                "secretAccessKey",
                &["secretAccessKey", "secret_access_key", "secretKey"][..],
            ),
            ("securityToken", &["securityToken", "security_token"][..]),
            ("projectId", &["projectId", "project_id"][..]),
        ] {
            if let Some(value) = carry(key, candidates) {
                record.insert(key.to_string(), Value::String(value));
            }
        }
        let expires_at = object
            .get("expiresAt")
            .and_then(Value::as_i64)
            .filter(|value| *value > 0)
            .or_else(|| {
                existing
                    .as_ref()
                    .and_then(|record| record.fields().get("expiresAt").and_then(Value::as_i64))
                    .filter(|value| *value > 0)
            })
            .unwrap_or(0);
        if expires_at > 0 {
            record.insert("expiresAt".to_string(), Value::from(expires_at));
        }
        // 来源：新记录记 import（OAuth 那条路会自己覆写），既有记录沿用
        record.insert(
            "source".to_string(),
            Value::String(
                existing
                    .as_ref()
                    .and_then(|record| record.fields().get("source").cloned())
                    .and_then(|value| value.as_str().map(str::to_string))
                    .unwrap_or_else(|| "import".to_string()),
            ),
        );
        record.insert("priority".to_string(), Value::from(priority));
        record.insert(
            "enabled".to_string(),
            Value::Bool(existing.as_ref().map(StoredAccount::enabled).unwrap_or(true)),
        );
        record.insert(
            "addedAt".to_string(),
            Value::from(
                existing
                    .as_ref()
                    .map(StoredAccount::added_at)
                    .filter(|value| *value != 0)
                    .unwrap_or(now),
            ),
        );
        record.insert("updatedAt".to_string(), Value::from(now));
        // 未知字段全量保留（用户手工加过的字段不能因为一次「更新账号」丢掉）
        let mut merged = record;
        if let Some(existing) = existing.as_ref() {
            for (key, value) in existing.fields() {
                merged.entry(key.clone()).or_insert_with(|| value.clone());
            }
        }
        let saved = StoredAccount::from_map(merged);
        self.with_conn(&guard, |conn| sql::put(conn, &saved))?;
        logging::log(
            "[Accounts]",
            &format!("✅ OfficeAce 账号已保存: {record_name}（优先级 {priority}）"),
        );
        Ok(saved.to_value())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_stable_for_the_same_credentials() {
        let a = account_id_for("https://gw.example.com", "K1");
        let b = account_id_for("https://gw.example.com/v2/", "K1");
        assert_eq!(a, b, "同一套凭据（含基址写法差异）落同一条 id");
        assert!(a.starts_with("officeace-"));
        assert_ne!(a, account_id_for("https://gw.example.com", "K2"));
        assert_ne!(a, account_id_for("https://other.example.com", "K1"));
    }

    #[test]
    fn short_hash_is_deterministic_and_matches_the_fnv_vector() {
        let hash = short_hash("hello");
        assert_eq!(hash, short_hash("hello"));
        assert_eq!(12, hash.len());
        // 已知的 FNV-1a 64 向量："hello" → 0xa430d84680aabd0b
        assert_eq!("a430d84680aa", hash);
    }
}
