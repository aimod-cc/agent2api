//! Accio 账号（两个地区各一家 provider）：添加、凭证刷新回写、公开形态。
//!
//! ── 身份怎么认 ──────────────────────────────────────────────
//! Accio 的 accessToken 是**不透明串**（不是 JWT），解不出 userId，因此身份
//! 由凭证里的 `userId`（网页登录 / 查资料拿到）决定。查不到 userId 时退回
//! **按 accessToken 相等**去重 —— 同一串凭证重复添加命中同一条记录，不会
//! 堆出一串重复账号。
//!
//! ── 两个地区为什么共用这一份实现 ─────────────────────────────
//! 账号字段、续期链路、公开形态两地完全一致，差别只在域名与 `x-package-region`
//! （那是转发与凭证层的事，账号层不体现）。`provider` 字段区分两地：
//! `accio` / `accio-cn`，因此**记录 id 也带地区前缀**，两条记录不会撞 id。

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::server::core::providers::accio::credentials::Credentials;
use crate::server::core::providers::accio::endpoints::Region;
use crate::server::logging;

use super::priority::next_free_priority;
use super::sql;
use super::state::StoredAccount;
use super::store::{AccountStore, AccountStoreError};
use super::store_util::{max_concurrent_public, token_tail_of, truncate_chars};
use super::CredentialWrite;

impl AccountStore {
    /// 取一条 Accio 账号记录。
    ///
    /// `provider_id` 是 `accio` / `accio-cn` 之一 —— **必须给**：两个地区是两家
    /// provider，国际版的凭证打不了国内版的站点（反之亦然），混着挑会让转发
    /// 拿一个必然 401 的账号去试。
    ///
    /// `account_id` 为空时给「这一家的队首可用账号」（与转发默认选路同一口径）。
    pub fn accio_account_record(&self, account_id: &str, provider_id: &str) -> Option<Value> {
        let guard = self.guard();
        if !account_id.is_empty() {
            let record = self.record_by_id(&guard, account_id)?;
            return (record.provider() == provider_id).then(|| record.to_value());
        }
        self.records_for_provider(&guard, provider_id)
            .into_iter()
            .filter(|record| record.enabled() && record.has_token())
            .min_by_key(|record| record.order_key())
            .map(|record| record.to_value())
    }

    /// 添加 / 更新一条 Accio 账号（手动粘贴与网页登录共用这一个入口）。
    pub fn add_accio_account(
        &self,
        credentials: &Credentials,
        name: Option<&str>,
        source: &str,
    ) -> Result<Value, AccountStoreError> {
        let mut credentials = credentials.clone();
        credentials.ensure_identity();
        let region = credentials.region;
        let provider = region.provider_id();
        let guard = self.guard();
        // 去重：优先按 userId（同一账号重复登录）；没有 userId 时按 accessToken 相等
        let existing = self
            .records_for_provider(&guard, provider)
            .into_iter()
            .find(|record| {
                let record_region = accio_region_of(record);
                if record_region != Some(region) {
                    return false;
                }
                let same_user = !credentials.user_id.is_empty()
                    && record.user_id() == credentials.user_id;
                let same_token = record
                    .get("accessToken")
                    .and_then(Value::as_str)
                    .is_some_and(|value| value == credentials.access_token);
                same_user || same_token
            });
        let id = existing
            .as_ref()
            .map(|record| record.id().to_string())
            .unwrap_or_else(|| {
                let seed = if credentials.user_id.is_empty() {
                    credentials.access_token.clone()
                } else {
                    format!("{}-{}", region.edition(), credentials.user_id)
                };
                format!(
                    "accio-{}-{:x}",
                    region.edition(),
                    Sha256::digest(seed.as_bytes())
                )
            });
        if existing.is_none() && self.record_by_id(&guard, &id).is_some() {
            return Err(AccountStoreError::new(
                "Accio 账号 ID 已被其它账号占用，请先核对账号记录",
                409,
            ));
        }
        let record_name = name
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .or_else(|| existing.as_ref().map(StoredAccount::name).filter(|value| !value.is_empty()))
            .unwrap_or_else(|| {
                if !credentials.name.is_empty() {
                    credentials.name.clone()
                } else if !credentials.email.is_empty() {
                    credentials.email.clone()
                } else if !credentials.user_id.is_empty() {
                    format!("Accio {}", credentials.user_id)
                } else {
                    format!("Accio {}账号", region.label())
                }
            });
        let mut fields = existing
            .as_ref()
            .map(|record| record.fields().clone())
            .unwrap_or_default();
        if let Value::Object(values) = credentials.to_value() {
            for (key, value) in values {
                // 空值不覆盖既有内容（重新登录时只给了一个 accessToken，
                // 不该把上次的 refreshToken / expiresAt 洗掉）
                if value.is_null() {
                    continue;
                }
                if matches!(&value, Value::String(text) if text.is_empty()) {
                    continue;
                }
                fields.insert(key, value);
            }
        }
        let priority = match existing.as_ref() {
            Some(record) => record.priority(),
            None => {
                let used = self
                    .with_conn(&guard, |conn| sql::priorities_all(conn))
                    .unwrap_or_default();
                next_free_priority(&used)
            }
        };
        fields.insert("id".to_string(), Value::String(id.clone()));
        fields.insert("provider".to_string(), Value::String(provider.to_string()));
        fields.insert("name".to_string(), Value::String(truncate_chars(&record_name, 100)));
        fields.insert(
            "tokenTail".to_string(),
            Value::String(token_tail_of(&credentials.access_token)),
        );
        fields.insert("priority".to_string(), Value::from(priority));
        fields.insert(
            "enabled".to_string(),
            Value::Bool(existing.as_ref().map(StoredAccount::enabled).unwrap_or(true)),
        );
        fields.insert("desktop".to_string(), Value::Bool(false));
        fields.insert("source".to_string(), Value::String(source.to_string()));
        fields.insert(
            "addedAt".to_string(),
            Value::from(existing.as_ref().map(StoredAccount::added_at).unwrap_or_else(logging::now_ms)),
        );
        fields.insert("updatedAt".to_string(), Value::from(logging::now_ms()));
        fields.insert("rateLimits".to_string(), json!({}));
        for key in ["edition", "endpoint", "prefixPath", "platform", "access", "refresh", "expires", "pat"] {
            fields.remove(key);
        }
        let record = StoredAccount::from_map(fields);
        self.with_conn(&guard, |conn| sql::put(conn, &record))?;
        logging::log(
            "[Accounts]",
            &format!("✅ Accio {}账号已保存（优先级 {priority}）", region.label()),
        );
        Ok(self.public_account(&record))
    }

    /// 刷新结果回写（比较-再写：记录里的凭证仍是刷新前那份才写）
    pub fn update_accio_credentials_if_current(
        &self,
        expected: &Value,
        credentials: &Credentials,
    ) -> Result<CredentialWrite, AccountStoreError> {
        let id = expected.get("id").and_then(Value::as_str).unwrap_or("");
        let guard = self.guard();
        let Some(mut record) = self
            .record_by_id(&guard, id)
            .filter(|record| accio_region_of(record).is_some())
        else {
            return Ok(CredentialWrite::Stale);
        };
        for key in ["accessToken", "refreshToken", "userId", "deviceId", "addedAt"] {
            if record.get(key) != expected.get(key) {
                return Ok(CredentialWrite::Stale);
            }
        }
        if accio_region_of(&record) != Some(credentials.region) {
            return Err(AccountStoreError::bad_request("Accio 刷新结果与原账号地区不一致"));
        }
        if !credentials.user_id.is_empty() && record.user_id() != credentials.user_id {
            return Err(AccountStoreError::bad_request("Accio 刷新结果与原账号身份不一致"));
        }
        if let Value::Object(values) = credentials.to_value() {
            for (key, value) in values {
                if value.is_null() || matches!(&value, Value::String(text) if text.is_empty()) {
                    continue;
                }
                record.fields_mut().insert(key, value);
            }
        }
        record.set("tokenTail", Value::String(token_tail_of(&credentials.access_token)));
        record.set_updated_at(logging::now_ms());
        self.with_conn(&guard, |conn| sql::update_in_place(conn, &record))?;
        Ok(CredentialWrite::Written)
    }

    /// 公开形态（两个地区共用；`edition` / `editionLabel` 体现差异）
    pub fn to_accio_public_account(&self, record: &StoredAccount) -> Value {
        let region = accio_region_of(record);
        let available = record.has_token() && region.is_some();
        let can_refresh = Credentials::from_payload(&record.to_value())
            .map(|credentials| credentials.can_refresh())
            .unwrap_or(false);
        let mut public = Map::new();
        for key in ["id", "provider", "name", "userId", "email", "nickname", "source", "tokenTail", "expiresAt"] {
            public.insert(key.to_string(), record.get(key).cloned().unwrap_or(Value::Null));
        }
        public.insert(
            "edition".to_string(),
            region
                .map(|value| Value::String(value.edition().to_string()))
                .unwrap_or(Value::Null),
        );
        public.insert(
            "editionLabel".to_string(),
            region
                .map(|value| Value::String(value.label().to_string()))
                .unwrap_or(Value::Null),
        );
        public.insert("hasRefreshToken".to_string(), Value::Bool(can_refresh));
        public.insert("priority".to_string(), Value::from(record.priority()));
        public.insert("enabled".to_string(), Value::Bool(record.enabled()));
        public.insert("addedAt".to_string(), Value::from(record.added_at()));
        public.insert("updatedAt".to_string(), Value::from(record.updated_at()));
        public.insert(
            "proxy".to_string(),
            crate::server::core::proxies::describe_account_proxy(Some(&record.proxy())),
        );
        public.insert(
            "rateLimits".to_string(),
            record.get("rateLimits").cloned().unwrap_or_else(|| json!({})),
        );
        public.insert("desktop".to_string(), Value::Bool(false));
        public.insert("available".to_string(), Value::Bool(available));
        public.insert(
            "maxConcurrent".to_string(),
            Value::from(max_concurrent_public(record.get("maxConcurrent"))),
        );
        Value::Object(public)
    }
}

/// 记录属于哪个 Accio 地区（不是 Accio 系 → None）。
///
/// 判据是 `provider` 字段（落盘契约），不是记录里的 `mode` —— 后者是凭证的
/// 冗余信息，改错了不该影响分派。
fn accio_region_of(record: &StoredAccount) -> Option<Region> {
    Region::from_provider_id(&record.provider())
}
