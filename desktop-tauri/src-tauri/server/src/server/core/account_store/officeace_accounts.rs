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

/// 像上游标识、不像人起的名字：整串 32 位十六进制（华为云 `account_id` 的实测形态）。
/// 名字自愈用它判断「这占位名不是用户打的」（见 [`AccountStore::heal_officeace_display_name`]）。
fn looks_like_upstream_identifier(name: &str) -> bool {
    name.len() == 32 && name.bytes().all(|byte| byte.is_ascii_hexdigit())
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

    /// 续期写回：只改**控制面**那半边（临时 AK/SK/STS/project_id + 到期时刻 +
    /// 轮换后的 refresh token），**不碰** baseUrl / modelAppKey / dpopKeyPair。
    ///
    /// 与 CodeArts 的凭据写回同一取舍：转发的网关 Basic 不过期，续期只管控制面；
    /// 一次续期把网关凭据一起重写等于给它一次无谓的写风险。就地更新，不改队列位置。
    #[allow(clippy::too_many_arguments)]
    pub fn update_officeace_control_plane(
        &self,
        account_id: &str,
        access_key_id: &str,
        secret_access_key: &str,
        security_token: &str,
        project_id: &str,
        expires_at: i64,
        refresh_token: &str,
    ) -> Result<(), AccountStoreError> {
        let guard = self.guard();
        let Some(mut record) = self
            .record_by_id(&guard, account_id)
            .filter(|record| record.provider() == OFFICEACE_PROVIDER_ID)
        else {
            return Err(AccountStoreError::new(
                "OfficeAce 账号已不存在，续期结果无处落盘",
                404,
            ));
        };
        let fields = record.fields_mut();
        for (key, value) in [
            ("accessKeyId", access_key_id),
            ("secretAccessKey", secret_access_key),
            ("securityToken", security_token),
            ("projectId", project_id),
        ] {
            if !value.trim().is_empty() {
                fields.insert(key.to_string(), Value::String(value.to_string()));
            }
        }
        if expires_at > 0 {
            fields.insert("expiresAt".to_string(), Value::from(expires_at));
        }
        // refresh token 一次性轮换：新的必须落盘，否则下一轮续期会用废掉的旧串
        if !refresh_token.trim().is_empty() {
            fields.insert(
                "refreshToken".to_string(),
                Value::String(refresh_token.to_string()),
            );
        }
        record.set_updated_at(logging::now_ms());
        self.with_conn(&guard, |conn| sql::update_in_place(conn, &record))?;
        Ok(())
    }

    /// 用上游给的显示名**自愈**账号名（续期链每轮调一次，见 `officeace::refresh_control_plane`）。
    /// 回 `true` = 真的改了名。前提是新名非空；改写条件二选一：
    ///   - 现名不是用户在面板打的（`nameCustom` 不为真）—— 那它就是登录时派生出来的占位名，
    ///     上游后来给了真显示名当然该换；
    ///   - 或现名长得像上游标识（32 位十六进制）—— 这是给**存量**记录留的出路：登录路径曾把
    ///     `account_id` 当名字落库并顺手标了 custom（现在不标了，见 `name_custom`）。
    ///     一串十六进制不会是用户自己起的名字，覆盖它不算越过用户意图。
    ///
    /// 命中时把 `nameCustom` 落回 false：这条名字来自上游，下一轮显示名再变还要能接着改。
    pub fn heal_officeace_display_name(
        &self,
        account_id: &str,
        display_name: &str,
    ) -> Result<bool, AccountStoreError> {
        let display_name = display_name.trim();
        if display_name.is_empty() {
            return Ok(false);
        }
        let guard = self.guard();
        let Some(mut record) = self
            .record_by_id(&guard, account_id)
            .filter(|record| record.provider() == OFFICEACE_PROVIDER_ID)
        else {
            // 账号不在了不算错误（可能刚被删）—— 续期链上只是没东西可自愈
            return Ok(false);
        };
        let current = record.name();
        let custom = record
            .get("nameCustom")
            .is_some_and(|value| matches!(value, Value::Bool(true)));
        let next = truncate_chars(display_name, MAX_NAME_LENGTH);
        if next == current || (custom && !looks_like_upstream_identifier(&current)) {
            return Ok(false);
        }
        let fields = record.fields_mut();
        fields.insert("name".to_string(), Value::String(next));
        fields.insert("nameCustom".to_string(), Value::Bool(false));
        record.set_updated_at(logging::now_ms());
        self.with_conn(&guard, |conn| sql::update_in_place(conn, &record))?;
        Ok(true)
    }

    /// 添加/更新一个 OfficeAce 账号（手工导入与自助 OAuth 共用）。
    ///
    /// payload：
    ///   - `baseUrl` / `api_base`：模型网关基址（必填）；
    ///   - `modelAppKey` / `appKey` 与 `modelAppSecret` / `appSecret`：网关凭据（必填）；
    ///   - `accessKeyId` / `secretAccessKey` / `securityToken` / `projectId`：控制面临时
    ///     凭据（可选，自助 OAuth 会给；没给就沿用既有记录的值，不把 OAuth 换来的抹掉）；
    ///   - `expiresAt`：控制面凭据到期时刻（毫秒，可选）。
    ///
    /// `name` 的两种来路要分清（`name_custom`）：`true` = 用户在面板自己打的（粘贴导入
    /// 那一路），落库标 `nameCustom`，此后重登与续期都不许覆盖；`false` = 登录路径的
    /// **派生名**（上游显示名 → `account_id` → 派生 hash），它只是让多账号暂时分得开，
    /// 上游哪天给了真显示名应由续期链换回来（见 [`Self::heal_officeace_display_name`]）。
    pub fn add_officeace_account(
        &self,
        payload: &Value,
        name: Option<&str>,
        name_custom: bool,
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
        // 派生名（登录那一路，`name_custom=false`）不许冲掉用户自己改过的名字 ——
        // 参考实现的 label 优先级就是「用户标签 → 上游显示名 → id」，用户标签永久赢。
        let existing_name = || {
            existing
                .as_ref()
                .map(StoredAccount::name)
                .filter(|value| !value.is_empty())
        };
        let carried_custom = existing
            .as_ref()
            .and_then(|record| record.get("nameCustom"))
            .is_some_and(|value| matches!(value, Value::Bool(true)));
        let record_name = if carried_custom && !name_custom {
            existing_name().unwrap_or_else(|| "OfficeAce 果办".to_string())
        } else {
            explicit_name
                .or_else(existing_name)
                .unwrap_or_else(|| "OfficeAce 果办".to_string())
        };
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
            name_custom && name.is_some_and(|value| !value.trim().is_empty()),
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
        // 续期材料：一次性 refresh token + 当初登录那把 DPoP 私钥。两者一起落盘才
        // 能续期（私钥丢了签不出与授权一致的 DPoP proof）。给了就存、没给沿用既有
        // 记录 —— 手工导入不带它们，重复导入不该把 OAuth 换来的抹掉。
        let refresh_token = {
            let incoming = pick(payload, &["refreshToken", "refresh_token"]);
            if !incoming.is_empty() {
                incoming
            } else {
                existing
                    .as_ref()
                    .and_then(|record| record.fields().get("refreshToken").cloned())
                    .and_then(|value| value.as_str().map(str::to_string))
                    .unwrap_or_default()
            }
        };
        if !refresh_token.is_empty() {
            record.insert("refreshToken".to_string(), Value::String(refresh_token));
        }
        if let Some(value) = object
            .get("dpopKeyPair")
            .filter(|value| value.is_object())
            .cloned()
            .or_else(|| {
                existing
                    .as_ref()
                    .and_then(|record| record.fields().get("dpopKeyPair").cloned())
                    .filter(|value| value.is_object())
            })
        {
            record.insert("dpopKeyPair".to_string(), value);
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
        // 回**公开形态**而不是整条记录：记录里躺着 modelAppSecret / secretAccessKey /
        // securityToken / refreshToken / DPoP 私钥，而账号列表那一侧走的是
        // `store_view::public_account` 的 officeace 分支（同一个函数）——
        // 添加响应原先直接把原始记录回出去，是全家族唯一的明文出口。
        Ok(self.to_officeace_public_account(&saved))
    }

    /// 账号的公开形态（前端账号列表 / 添加响应 / 会话摘要用；**不含任何凭据**）。
    ///
    /// 字段口径照 kuku 那份（`to_kuku_public_account`）：标识 + 归属 + 队列状态 +
    /// 时间戳 + 代理描述，再加本家界面真正会读的三样 —— `baseUrl`（网关地址，
    /// 面板改凭据时要用）、`expiresAt`（控制面临时凭据到期时刻，域配置里
    /// `expiry:'expiresAt'` 读它）、`hasRefreshToken`（面板「刷新 Token」按钮的开关，
    /// 判据与 workbuddy 那份一致：记录上的 `refreshToken` 非空）。
    pub fn to_officeace_public_account(&self, record: &StoredAccount) -> Value {
        let proxy = crate::server::core::proxies::describe_account_proxy(Some(&record.proxy()));
        let mut public = Map::new();
        public.insert("id".to_string(), Value::String(record.id().to_string()));
        public.insert("provider".to_string(), Value::String(record.provider()));
        public.insert("name".to_string(), Value::String(record.name()));
        public.insert("source".to_string(), Value::String(record.source()));
        public.insert("priority".to_string(), Value::from(record.priority()));
        public.insert("enabled".to_string(), Value::Bool(record.enabled()));
        public.insert("addedAt".to_string(), Value::from(record.added_at()));
        public.insert("updatedAt".to_string(), Value::from(record.updated_at()));
        public.insert("proxy".to_string(), proxy);
        public.insert(
            "baseUrl".to_string(),
            record.get("baseUrl").cloned().unwrap_or(Value::Null),
        );
        public.insert(
            "expiresAt".to_string(),
            record.get("expiresAt").cloned().unwrap_or(Value::Null),
        );
        public.insert(
            "hasRefreshToken".to_string(),
            Value::Bool(
                record
                    .get("refreshToken")
                    .and_then(Value::as_str)
                    .is_some_and(|value| !value.trim().is_empty()),
            ),
        );
        // 这一位决定界面「备注名赢过上游显示名」的口径；账号列表那一侧由
        // `public_account` 末尾统一注入，添加响应不经过那里，所以在这里补齐 ——
        // 两条出口给前端的形状必须一致，否则加完账号要刷新列表才显示对。
        public.insert(
            "nameCustom".to_string(),
            Value::Bool(
                record
                    .get("nameCustom")
                    .is_some_and(|value| matches!(value, Value::Bool(true))),
            ),
        );
        Value::Object(public)
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

    #[test]
    fn the_identifier_look_only_matches_a_bare_hex_account_id() {
        assert!(looks_like_upstream_identifier("0123456789abcdef0123456789abcdef"));
        assert!(!looks_like_upstream_identifier("我的果办号"));
        assert!(!looks_like_upstream_identifier("OfficeAce 果办"));
        // 短一位/长一位都不算：只有上游那串 32 位形态才当它是标识
        assert!(!looks_like_upstream_identifier("0123456789abcdef0123456789abcde"));
        assert!(!looks_like_upstream_identifier("0123456789abcdef0123456789abcdef0"));
    }

    /// 临时库 + 目录守卫：删除挂在 Drop 上（"打开前先删"只保证不复用、不保证不留垃圾，
    /// 目录名带 pid 与序号时上一轮的永远删不到）。
    struct TempStore {
        store: AccountStore,
        dir: std::path::PathBuf,
    }

    impl Drop for TempStore {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn temp_store(tag: &str) -> TempStore {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "officeace-name-{tag}-{}-{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let db = crate::server::db::Db::open(&dir.join("agent2api.db")).expect("临时库该建得起来");
        TempStore { store: AccountStore::with_db(Some(db)), dir }
    }

    /// 落一个 OfficeAce 账号（`name_custom` 区分面板打的与登录派生的），回它的 id。
    fn add_account(store: &AccountStore, key: &str, name: &str, custom: bool) -> String {
        use serde_json::json;
        let payload = json!({
            "baseUrl": "https://gw.example.com/v2",
            "modelAppKey": key,
            "modelAppSecret": "SK",
        });
        let account = store
            .add_officeace_account(&payload, Some(name), custom)
            .expect("导入应当成功");
        account["id"].as_str().unwrap_or("").to_string()
    }

    const HEX_ID: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn a_panel_name_is_custom_but_a_login_name_is_not() {
        let temp = temp_store("flag");
        let panel = add_account(&temp.store, "K_PANEL", "我的果办号", true);
        let login = add_account(&temp.store, "K_LOGIN", HEX_ID, false);
        let record = temp.store.officeace_account_record(&panel).expect("面板那一条");
        assert_eq!(
            Some(&Value::Bool(true)),
            record.get("nameCustom"),
            "用户在面板打的名字要标 custom"
        );
        let record = temp.store.officeace_account_record(&login).expect("登录那一条");
        assert_ne!(
            Some(&Value::Bool(true)),
            record.get("nameCustom"),
            "登录带来的派生名不该标 custom —— 标了就永远锁死在十六进制上"
        );
    }

    #[test]
    fn a_relogin_does_not_overwrite_a_renamed_account() {
        use serde_json::json;
        let temp = temp_store("relogin");
        let id = add_account(&temp.store, "K_SAME", "我的果办号", true);
        // 重登同一套凭据、带着派生名回来：不许冲掉用户改过的名字
        let payload = json!({
            "baseUrl": "https://gw.example.com/v2",
            "modelAppKey": "K_SAME",
            "modelAppSecret": "SK",
        });
        temp.store
            .add_officeace_account(&payload, Some(HEX_ID), false)
            .expect("重登该就地更新同一条");
        let record = temp.store.officeace_account_record(&id).expect("账号还在");
        assert_eq!("我的果办号", record["name"].as_str().unwrap());
    }

    #[test]
    fn the_upstream_display_name_heals_a_derived_name_and_keeps_it_healable() {
        let temp = temp_store("heal");
        let id = add_account(&temp.store, "K_HEAL", HEX_ID, false);
        assert!(
            temp.store
                .heal_officeace_display_name(&id, "zhang-san")
                .expect("改名不该失败"),
            "派生名该被上游显示名换掉"
        );
        let record = temp.store.officeace_account_record(&id).expect("账号还在");
        assert_eq!("zhang-san", record["name"].as_str().unwrap());
        assert_ne!(
            Some(&Value::Bool(true)),
            record.get("nameCustom"),
            "自愈回来的名字仍属上游，下一轮显示名再变还要能接着改"
        );
    }

    #[test]
    fn a_users_own_name_is_never_healed_but_a_stuck_hex_one_is() {
        let temp = temp_store("custom");
        // 存量记录的形态：登录曾把 account_id 落成名字、还标了 custom
        let stuck = add_account(&temp.store, "K_STUCK", HEX_ID, true);
        assert!(
            temp.store
                .heal_officeace_display_name(&stuck, "zhang-san")
                .expect("改名不该失败"),
            "一串十六进制不会是用户起的名字，允许自愈"
        );
        // 用户自己打的名字：永远不动
        let mine = add_account(&temp.store, "K_MINE", "我的果办号", true);
        assert!(
            !temp
                .store
                .heal_officeace_display_name(&mine, "zhang-san")
                .expect("改名不该失败"),
            "用户改过的名不能被上游冲掉"
        );
        let record = temp.store.officeace_account_record(&mine).expect("账号还在");
        assert_eq!("我的果办号", record["name"].as_str().unwrap());
        // 上游没给显示名：一个字都不动，也不算错误
        assert!(!temp.store.heal_officeace_display_name(&mine, "").expect("空名不该失败"));
    }

    /// 添加响应走**公开形态**。审计抓到本家是全家族唯一的明文出口：原先直接回
    /// 整条记录，里面躺着 modelAppSecret / secretAccessKey / securityToken /
    /// refreshToken / DPoP 私钥。这条用例既钉「凭据字段一个都不许出现」，
    /// 也钉界面真读的字段还在（`hasRefreshToken` 之前只是**恰好**从 workbuddy
    /// 兜底形状里漏出来才有的，不是设计）。
    #[test]
    fn neither_the_add_response_nor_the_list_carries_credentials() {
        use serde_json::json;
        let temp = temp_store("public");
        let payload = json!({
            "baseUrl": "https://gw.example.com/v2",
            "modelAppKey": "AK-GATEWAY",
            "modelAppSecret": "SK-GATEWAY",
            "accessKeyId": "AK-CONTROL",
            "secretAccessKey": "SK-CONTROL",
            "securityToken": "STS-TOKEN",
            "refreshToken": "REFRESH-ONCE",
            "expiresAt": 1_800_000_000_000_i64,
        });
        let account = temp
            .store
            .add_officeace_account(&payload, Some("我的果办号"), true)
            .expect("导入应当成功");
        for key in [
            "modelAppKey",
            "modelAppSecret",
            "accessKeyId",
            "secretAccessKey",
            "securityToken",
            "refreshToken",
            "dpopKeyPair",
        ] {
            assert!(account.get(key).is_none(), "{key} 是凭据字段，不许出现在添加响应里");
        }
        assert_eq!("我的果办号", account["name"].as_str().unwrap());
        assert_eq!("https://gw.example.com/v2", account["baseUrl"].as_str().unwrap());
        assert_eq!(
            Some(&Value::Bool(true)),
            account.get("hasRefreshToken"),
            "面板「刷新 Token」按钮读这一位"
        );
        assert_eq!(
            Some(&Value::Bool(true)),
            account.get("nameCustom"),
            "面板打的名字仍按「用户说了算」透出"
        );
        // 列表同口径：officeace 分支接上之后，凭据值不该出现在任何一处出口
        let listed = temp.store.list_accounts().to_string();
        for secret in ["SK-GATEWAY", "AK-GATEWAY", "AK-CONTROL", "SK-CONTROL", "STS-TOKEN", "REFRESH-ONCE"] {
            assert!(!listed.contains(secret), "账号列表里出现了凭据值 {secret}");
        }
    }

    /// 「出站前问一次凭证」那条链：本家没有可刷新的 token，这是**设计上的空操作**，
    /// 必须报成功。原先它恒回一个 401「没有可刷新的 token」，实测每发请求都往日志记一条
    /// 「凭证准备失败（沿用现有 token）」—— 因为我把调用时机读错了（不是只在 401 之后走）。
    #[tokio::test]
    async fn ensuring_the_forward_credential_is_a_silent_no_op() {
        use crate::server::core::providers::ProviderKind;
        use crate::server::core::providers::adapter::adapter_for;
        use serde_json::json;
        let temp = temp_store("ensure");
        let payload = json!({
            "baseUrl": "https://gw.example.com/v2",
            "modelAppKey": "K-ENSURE",
            "modelAppSecret": "S-ENSURE",
        });
        let account = temp
            .store
            .add_officeace_account(&payload, Some("n"), true)
            .expect("导入应当成功");
        let id = account["id"].as_str().unwrap_or("").to_string();
        let adapter = adapter_for(ProviderKind::OfficeAce);
        assert_eq!(
            String::new(),
            adapter
                .ensure_access_token(&temp.store, &id)
                .await
                .expect("齐备的网关凭据不该报错"),
            "空操作回空串，不冒充任何 token"
        );
        assert!(
            adapter.ensure_access_token(&temp.store, "officeace-absent").await.is_err(),
            "账号不存在仍然要报错"
        );
    }
}
