//! Qoder 账号以地区 + userId 识别；凭证与其它提供商共享存储键名。

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::server::core::providers::qoder::credentials::Credentials;
use crate::server::core::providers::qoder::endpoints::Region;
use crate::server::core::providers::{kind_id, ProviderKind};
use crate::server::logging;

use super::priority::next_free_priority;
use super::sql;
use super::state::StoredAccount;
use super::store::{AccountStore, AccountStoreError};
use super::store_util::{max_concurrent_public, token_tail_of, truncate_chars};
use super::CredentialWrite;

const PROVIDER: &str = kind_id(ProviderKind::Qoder);

impl AccountStore {
    pub fn qoder_account_record(&self, account_id: &str) -> Option<Value> {
        let guard = self.guard();
        if !account_id.is_empty() {
            // 按 id 直查一行后确认归属（同 id 只会有一条，provider 判定是保险）
            let record = self.record_by_id(&guard, account_id)?;
            return (record.provider() == PROVIDER).then(|| record.to_value());
        }
        self.records_for_provider(&guard, PROVIDER)
            .into_iter()
            .filter(|record| record.enabled() && record.has_token())
            .min_by_key(|record| record.order_key())
            .map(|record| record.to_value())
    }

    pub fn add_qoder_account(
        &self,
        credentials: &Credentials,
        name: Option<&str>,
        source: &str,
    ) -> Result<Value, AccountStoreError> {
        let mut credentials = credentials.clone();
        credentials.complete_identity()
            .map_err(|error| AccountStoreError::new(error.message, error.status_code))?;
        let guard = self.guard();
        // 身份匹配要「地区 + userId」两段信息，而地区存在记录 JSON 里（不是投影列），
        // 所以这里读**本家那一组**（而不是全量）逐条比 —— Qoder 账号通常只有
        // 一两条，按 provider 收窄已经把别家的记录挡在外面了。
        let existing = self
            .records_for_provider(&guard, PROVIDER)
            .into_iter()
            .find(|record| {
                record.user_id() == credentials.user_id
                    && Region::from_payload(&record.to_value()).ok() == Some(credentials.region)
            });
        let id = existing.as_ref().map(|record| record.id().to_string()).unwrap_or_else(|| {
            format!("qoder-{}-{:x}", credentials.region.id(), Sha256::digest(credentials.user_id.as_bytes()))
        });
        // 新建时确认这个 id 没被任何人（含别家）占用 —— 主键查询，只读一行
        if existing.is_none() && self.record_by_id(&guard, &id).is_some() {
            return Err(AccountStoreError::new("Qoder 账号 ID 已被其它账号占用，请先核对账号记录", 409));
        }
        let record_name = name.map(str::trim).filter(|value| !value.is_empty()).map(str::to_string)
            .or_else(|| existing.as_ref().map(StoredAccount::name).filter(|value| !value.is_empty()))
            .unwrap_or_else(|| {
                if !credentials.name.is_empty() { credentials.name.clone() }
                else if !credentials.email.is_empty() { credentials.email.clone() }
                else { format!("Qoder {}", credentials.user_id) }
            });
        let mut fields = existing.as_ref().map(|record| record.fields().clone()).unwrap_or_default();
        if let Value::Object(values) = credentials.to_value() {
            for (key, value) in values {
                // 空值不覆盖既有内容：重新添加时只给了一个 access token（或过期
                // 时间缺失）不该把上次的 refreshToken / expiresAt 洗掉 —— 那会让
                // 一个本来能自动续期的账号在下次导入后变成只能等过期。
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
                // 号段取全部账号（优先级全局唯一）；只取投影列一列数值。
                // `unwrap_or_default` 兜底到 `next_free_priority(&[])` 的默认号，
                // 与旧实现在空账号列表上的行为一致。
                let used = self
                    .with_conn(&guard, |conn| sql::priorities_all(conn))
                    .unwrap_or_default();
                next_free_priority(&used)
            }
        };
        fields.insert("id".to_string(), Value::String(id.clone()));
        fields.insert("provider".to_string(), Value::String(PROVIDER.to_string()));
        fields.insert("name".to_string(), Value::String(truncate_chars(&record_name, 100)));
        fields.insert("tokenTail".to_string(), Value::String(token_tail_of(&credentials.access_token)));
        fields.insert("priority".to_string(), Value::from(priority));
        fields.insert("enabled".to_string(), Value::Bool(existing.as_ref().map(StoredAccount::enabled).unwrap_or(true)));
        fields.insert("desktop".to_string(), Value::Bool(false));
        fields.insert("source".to_string(), Value::String(source.to_string()));
        fields.insert("addedAt".to_string(), Value::from(existing.as_ref().map(StoredAccount::added_at).unwrap_or_else(logging::now_ms)));
        fields.insert("updatedAt".to_string(), Value::from(logging::now_ms()));
        fields.insert("rateLimits".to_string(), json!({}));
        for key in ["edition", "endpoint", "prefixPath", "platform", "access", "refresh", "expires", "pat"] {
            fields.remove(key);
        }
        let record = StoredAccount::from_map(fields);
        // 单行落地：`put` = DELETE + INSERT。旧实现在「命中既有记录」时是
        // `state.accounts[index] = record`（**保持原位**），而 `put` 会把它挪到
        // 列表末尾 —— 这一处差异不会被任何消费方观察到：列表顺序无消费方
        // （界面与选路都按优先级排，见 `sql.rs` 模块头「顺序」一节）。
        self.with_conn(&guard, |conn| sql::put(conn, &record))?;
        logging::log("[Accounts]", &format!("✅ Qoder {}账号已保存（优先级 {priority}）", credentials.region.label()));
        Ok(self.public_account(&record))
    }

    pub fn update_qoder_credentials_if_current(
        &self,
        expected: &Value,
        credentials: &Credentials,
    ) -> Result<CredentialWrite, AccountStoreError> {
        let id = expected.get("id").and_then(Value::as_str).unwrap_or("");
        let guard = self.guard();
        let Some(mut record) = self
            .record_by_id(&guard, id)
            .filter(|record| record.provider() == PROVIDER)
        else {
            return Ok(CredentialWrite::Stale);
        };
        for key in ["accessToken", "refreshToken", "mode", "userId", "machineId", "addedAt"] {
            if record.get(key) != expected.get(key) {
                return Ok(CredentialWrite::Stale);
            }
        }
        if record.user_id() != credentials.user_id
            || Region::from_payload(&record.to_value()).ok() != Some(credentials.region)
        {
            return Err(AccountStoreError::bad_request("Qoder 刷新结果与原账号身份不一致"));
        }
        if let Value::Object(values) = credentials.to_value() {
            for (key, value) in values {
                // 空值不覆盖既有内容：刷新结果里缺某一项（上游没回过期时间）时
                // 不该把记录里仍然有效的值洗成空。
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

    pub fn to_qoder_public_account(&self, record: &StoredAccount) -> Value {
        let region = Region::from_payload(&record.to_value()).ok();
        let available = record.has_token() && region.is_some();
        let can_refresh = Credentials::from_payload(&record.to_value())
            .map(|credentials| credentials.can_refresh()).unwrap_or(false);
        let mut public = Map::new();
        for key in ["id", "provider", "name", "userId", "email", "nickname", "mode", "source", "tokenTail", "expiresAt"] {
            public.insert(key.to_string(), record.get(key).cloned().unwrap_or(Value::Null));
        }
        public.insert("edition".to_string(), region.map(|value| Value::String(value.edition().to_string())).unwrap_or(Value::Null));
        public.insert("editionLabel".to_string(), region.map(|value| Value::String(value.label().to_string())).unwrap_or(Value::Null));
        public.insert("hasRefreshToken".to_string(), Value::Bool(can_refresh));
        public.insert("priority".to_string(), Value::from(record.priority()));
        public.insert("enabled".to_string(), Value::Bool(record.enabled()));
        public.insert("addedAt".to_string(), Value::from(record.added_at()));
        public.insert("updatedAt".to_string(), Value::from(record.updated_at()));
        public.insert("proxy".to_string(), crate::server::core::proxies::describe_account_proxy(Some(&record.proxy())));
        public.insert("rateLimits".to_string(), record.get("rateLimits").cloned().unwrap_or_else(|| json!({})));
        public.insert("desktop".to_string(), Value::Bool(false));
        public.insert("available".to_string(), Value::Bool(available));
        // 单账号并发上限（所有家通用，兜底共用 `max_concurrent_public`）：
        // 0 = 不限，缺键同样输出 0
        public.insert(
            "maxConcurrent".to_string(),
            Value::from(max_concurrent_public(record.get("maxConcurrent"))),
        );
        // `chatSupported` **刻意不在这里写**：它是跨家的统一事实，由
        // `store.rs::public_account` 在分派点按适配器的 `supports_chat()` 注入。
        // 本函数早先硬编码过 `false`（Qoder 只有账号管理能力那会儿），
        // 而那个值会被分派点的注入覆盖 —— 留着它只会在下一次读这段代码时
        // 误导「这家不能转发」，且一旦注入被挪走就会静默失真。
        Value::Object(public)
    }
}
