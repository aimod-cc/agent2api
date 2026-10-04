//! LobsterAI 账号管理（feat/lobster-provider PR-1）。
//!
//! ── 本模块负责什么（PR-1 范围）─────────────────────────────────
//!   - `add_lobster_account`：手动粘贴 accessToken + refreshToken 落库。
//!   - `import_lobster_desktop_account`：从本机 LobsterAI App 的 sqlite
//!     实时读出凭证，落到与手动账号同一份存储里。
//!   - **不实现**（PR-2 接）：额度查询 / 模型目录 / 刷新回写 / 守护签到。
//!
//! ── 字段约定 ─────────────────────────────────────────────
//!   - `accessToken` / `refreshToken`：与 raccoon / workbuddy 同名（统一 JSON
//!     字段键名，UI 表单与代填脚本都按这套走）。
//!   - `accountId`：`lobster-{jwtExp 或 tokenTail}`，去重键；userId 暂取 JWT
//!     payload 的 `exp`（LobsterAI JWT 中 exp 即客户端身份标识）。
//!   - `path`：sqlite 文件路径，记录展示用。
//!   - `desktop`：手动 = false，桌面端导入 = true（与 autoclaw 同语义）。
//!
//! ── panic=abort ────────────────────────────────────────────
//! 本文件在对话链路上，绝不 unwrap/expect/panic：取值走 Option 链与
//! `unwrap_or`，SQL/JSON 失败一律转成 AccountStoreError。

use serde_json::{json, Map, Value};

use crate::server::core::account_store::store::live_desktop_credentials;
use crate::server::core::providers::lobster::credentials::snapshot_for_desktop;
use crate::server::core::providers::{kind_id, ProviderKind};
use crate::server::logging;

use super::priority::next_free_priority;
use super::sql;
use super::state::StoredAccount;
use super::store::{AccountStore, AccountStoreError};
use super::store_util::truncate_chars;

/// provider id 常量（与 `kind_id(ProviderKind::Lobster)` 等价）
pub fn lobster_id() -> &'static str {
    kind_id(ProviderKind::Lobster)
}

/// 从 payload 挑 accessToken / refreshToken（与 raccoon 一致的宽松别名）
fn pick_token(object: &Map<String, Value>, keys: &[&str]) -> String {
    for key in keys {
        if let Some(Value::String(text)) = object.get(*key) {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return truncate_chars(trimmed, 4096);
            }
        }
    }
    String::new()
}

/// token 指纹（最后 4 个字符），UI 列展示用（按 char 切,与 raccoon 同款）
fn token_tail(token: &str) -> String {
    let chars: Vec<char> = token.chars().collect();
    let start = chars.len().saturating_sub(4);
    chars[start..].iter().collect::<String>()
}

impl AccountStore {
    /// 取 LobsterAI 的账号记录（`raccoon_account_record` 同构）：
    /// `account_id` 非空按 id 直取（provider 必须是 lobster）；空则取本家队首
    /// （仅启用账号，排序交给内存）。没有记录返回 None（调用方回落桌面 sqlite）。
    pub fn lobster_account_record(&self, account_id: &str) -> Option<Value> {
        let _guard = self.guard();
        let lobster = lobster_id();
        if !account_id.is_empty() {
            let record = self.record_by_id(&_guard, account_id)?;
            return (record.provider() == lobster).then(|| record.to_value());
        }
        let mut candidates: Vec<StoredAccount> = self
            .records_for_provider(&_guard, lobster)
            .into_iter()
            .filter(StoredAccount::enabled)
            .collect();
        candidates.sort_by_key(StoredAccount::order_key);
        candidates.into_iter().next().map(|item| item.to_value())
    }

    /// 刷新后的凭证回写账号记录（**比较-再写**：记录里仍是刷新前那份凭证时才写，
    /// `raccoon` 的 `update_raccoon_account_tokens_if_current` 同一语义）。
    ///
    /// 返回 `Ok(true)` = 已写入；`Ok(false)` = 记录里的凭证已被更换（用户重导/
    /// 换号），本次结果不落盘（调用方改用最新快照）；`Err` = 记录不存在或写失败。
    pub fn update_lobster_account_tokens_if_current(
        &self,
        account_id: &str,
        previous_access: &str,
        previous_refresh: &str,
        next_access: &str,
        next_refresh: &str,
        expires_at: Option<i64>,
    ) -> Result<bool, AccountStoreError> {
        let guard = self.guard();
        let Some(mut record) = self.record_by_id(&guard, account_id) else {
            // 记录已删除:返回 Stale(Ok(false))而不是 Err——调用方的三态收口里,
            // Stale 会走 latest_credentials 重读,而 latest 对已删记录给**终态 401**;
            // 若走 Err 分支会被当成「回写失败但刷新成功」,仍返回新 token——
            // 账号都没了还发新凭证,是把删除当成了临时故障(上游第 2 轮探针发现)
            return Ok(false);
        };
        if record.provider() != lobster_id() {
            return Err(AccountStoreError::new(
                format!("账号 {account_id} 不是 LobsterAI 账号"),
                400,
            ));
        }
        let stored_access = record
            .fields()
            .get("accessToken")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let stored_refresh = record
            .fields()
            .get("refreshToken")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if stored_access != previous_access || stored_refresh != previous_refresh {
            return Ok(false);
        }
        let fields = record.fields_mut();
        fields.insert(
            "accessToken".to_string(),
            Value::String(next_access.to_string()),
        );
        fields.insert(
            "refreshToken".to_string(),
            Value::String(next_refresh.to_string()),
        );
        fields.insert("tokenTail".to_string(), Value::String(token_tail(next_access)));
        match expires_at {
            Some(exp) => {
                fields.insert("jwtExpiresAt".to_string(), json!(exp));
            }
            None => {
                fields.remove("jwtExpiresAt");
            }
        }
        record.set_updated_at(logging::now_ms());
        let updated = record.clone();
        self.with_conn(&guard, |conn| sql::put(conn, &updated))?;
        Ok(true)
    }

    /// 手动添加 LobsterAI 账号（accessToken + refreshToken 必填）。
    pub fn add_lobster_account(
        &self,
        payload: &Value,
        name: Option<&str>,
    ) -> Result<Value, AccountStoreError> {
        let Some(object) = payload.as_object() else {
            return Err(AccountStoreError::new("上传内容必须是 JSON 对象", 400));
        };
        let access_token = pick_token(object, &["accessToken", "access_token", "token"]);
        let refresh_token = pick_token(object, &["refreshToken", "refresh_token"]);
        if access_token.is_empty() {
            return Err(AccountStoreError::new("缺少 accessToken", 400));
        }
        if refresh_token.is_empty() {
            return Err(AccountStoreError::new("缺少 refreshToken", 400));
        }
        let path = object
            .get("path")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_default();

        let jwt_exp = crate::server::core::providers::lobster::credentials::jwt_exp_seconds(
            &access_token,
        );
        let id = format!(
            "lobster-{}",
            jwt_exp
                .map(|e| e.to_string())
                .unwrap_or_else(|| format!("tok-{}", token_tail(&access_token)))
        );
        let display_name = name
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("LobsterAI-{}", token_tail(&access_token)));

        let guard = self.guard();
        let existing = self.record_by_id(&guard, &id);
        if let Some(existing) = existing.as_ref() {
            if existing.provider() != lobster_id() {
                return Err(AccountStoreError::new(
                    format!(
                        "账号 id「{id}」已被 {} 账号占用，无法添加同一 id 的 LobsterAI 账号",
                        existing.provider()
                    ),
                    409,
                ));
            }
        }

        let record_name = existing
            .as_ref()
            .map(StoredAccount::name)
            .filter(|s| !s.is_empty())
            .unwrap_or(display_name.clone());
        let priority = match existing.as_ref() {
            Some(record) => record.priority(),
            None => self
                .with_conn(&guard, |conn| sql::priorities_all(conn))
                .unwrap_or_default()
                .pipe(|used| next_free_priority(&used)),
        };

        let mut fields: Map<String, Value> = existing
            .as_ref()
            .map(|record| record.fields().clone())
            .unwrap_or_default();
        // 标准字段（与 trae / raccoon 对齐）
        fields.insert("id".to_string(), Value::String(id.clone()));
        fields.insert(
            "provider".to_string(),
            Value::String(lobster_id().to_string()),
        );
        fields.insert(
            "name".to_string(),
            Value::String(truncate_chars(&record_name, 100)),
        );
        fields.insert(
            "tokenTail".to_string(),
            Value::String(token_tail(&access_token)),
        );
        fields.insert("priority".to_string(), Value::from(priority));
        fields.insert(
            "enabled".to_string(),
            Value::Bool(existing.as_ref().map(StoredAccount::enabled).unwrap_or(true)),
        );
        // 凭证三件套
        fields.insert(
            "accessToken".to_string(),
            Value::String(access_token.clone()),
        );
        fields.insert(
            "refreshToken".to_string(),
            Value::String(refresh_token.clone()),
        );
        if !path.is_empty() {
            fields.insert("path".to_string(), Value::String(path.clone()));
        }
        if let Some(exp) = jwt_exp {
            fields.insert("jwtExpiresAt".to_string(), json!(exp));
        }
        // desktop=false（手动添加）；import 路径会覆盖为 true
        fields.insert("desktop".to_string(), Value::Bool(false));
        fields.insert("source".to_string(), Value::String("manual".to_string()));
        fields.insert(
            "addedAt".to_string(),
            Value::from(
                existing
                    .as_ref()
                    .map(StoredAccount::added_at)
                    .unwrap_or_else(logging::now_ms),
            ),
        );
        fields.insert("updatedAt".to_string(), Value::from(logging::now_ms()));

        let record = StoredAccount::from_map(fields);
        self.with_conn(&guard, |conn| sql::put(conn, &record))?;
        logging::log(
            "[Accounts]",
            &format!("✅ LobsterAI 手动账号已添加: {display_name}"),
        );

        Ok(json!({
            "id": id,
            "provider": lobster_id(),
            "name": display_name,
            "priority": priority,
            "enabled": true,
            "source": "manual",
        }))
    }

    /// 桌面端实时登录态导入：从本机 sqlite 读凭证，落到同一份存储。
    pub fn import_lobster_desktop_account(
        &self,
        _source: &str,
    ) -> Result<Value, AccountStoreError> {
        let (access, refresh) = snapshot_for_desktop().map_err(|e| {
            AccountStoreError::new(
                format!("LobsterAI 桌面端导入失败: {}", e.message),
                401,
            )
        })?;
        let payload = json!({
            "accessToken": access,
            "refreshToken": refresh,
        });
        let account = self.add_lobster_account(&payload, None)?;
        // 把 source 标记为 desktop，add_lobster_account 里默认是 manual
        let id = account
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| AccountStoreError::new("导入返回缺少 id", 500))?
            .to_string();
        let guard = self.guard();
        if let Some(mut record) = self.record_by_id(&guard, &id) {
            record
                .fields_mut()
                .insert("source".to_string(), Value::String("desktop".to_string()));
            record
                .fields_mut()
                .insert("desktop".to_string(), Value::Bool(true));
            record
                .fields_mut()
                .insert("updatedAt".to_string(), Value::from(logging::now_ms()));
            let updated = record.clone();
            self.with_conn(&guard, |conn| sql::put(conn, &updated))?;
        }
        logging::log(
            "[Accounts]",
            &format!(
                "✅ LobsterAI 桌面端登录态已导入: {}",
                account.get("name").and_then(Value::as_str).unwrap_or("(unnamed)")
            )
        );
        Ok(account)
    }

    /// 公开形态（进面板账号列表与 `/api/accounts`；仿 `to_raccoon_public_account`）。
    ///
    /// ── 桌面端账号的实时值 ─────────────────────────────────────
    /// 桌面账号的真值活在 App 的 sqlite 里（记录里的副本是导入那一刻的快照），
    /// `tokenTail` / `jwtExpiresAt` / `hasRefreshToken` 优先用实时值 —— 与
    /// raccoon 的公开形态同一取向：登录态消失表现为字段变空，而不是整条
    /// 记录看起来还是好的。
    ///
    /// ── `jwtExpiresAt` 的口径 ───────────────────────────────────
    /// 落盘记录里存的是 JWT 的 exp（**秒**），公开形态统一换算成**毫秒** ——
    /// 全仓的过期时间口径（`tokenExpiresAt` / `expiresAt`）都是毫秒，前端
    /// `tokenExpiryOf` 按毫秒渲染，秒值会显示成 1970 年。
    pub fn to_lobster_public_account(&self, record: &StoredAccount) -> Value {
        let proxy = crate::server::core::proxies::describe_account_proxy(Some(&record.proxy()));
        let stored_tail = record
            .get("tokenTail")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let stored_expires_ms = record
            .get("jwtExpiresAt")
            .and_then(Value::as_i64)
            .filter(|seconds| *seconds > 0)
            .map(|seconds| seconds as f64 * 1000.0);
        let live = if record.is_desktop() {
            live_desktop_credentials(record)
        } else {
            None
        };
        let (token_tail, expires_ms, has_refresh, live_missing) = match live {
            // `live_desktop_credentials` 的 expires_at 已是毫秒（f64）。
            // 桌面账号读不到实时登录态时（live 为空对）：字段全部清空 +
            // available=false —— 「App 已登出」要让面板与转发看到同一个事实，
            // 而不是回落到导入时的旧副本装作正常（上游审计发现 8）。
            Some((token, refresh_token, expires_at_ms)) if !token.is_empty() => (
                token_tail(&token),
                (expires_at_ms > 0.0).then_some(expires_at_ms).or(stored_expires_ms),
                !refresh_token.is_empty(),
                false,
            ),
            Some((_, _, _)) => (String::new(), None, false, true),
            None => (
                stored_tail,
                stored_expires_ms,
                !record
                    .get("refreshToken")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .is_empty(),
                false,
            ),
        };
        let mut public = Map::new();
        public.insert("id".to_string(), Value::String(record.id().to_string()));
        public.insert(
            "provider".to_string(),
            Value::String(record.provider()),
        );
        public.insert("name".to_string(), Value::String(record.name()));
        public.insert("tokenTail".to_string(), Value::String(token_tail));
        public.insert(
            "jwtExpiresAt".to_string(),
            expires_ms
                .map(crate::server::core::account_store::state::json_number)
                .unwrap_or(Value::Null),
        );
        public.insert("hasRefreshToken".to_string(), Value::Bool(has_refresh));
        public.insert("desktop".to_string(), Value::Bool(record.is_desktop()));
        public.insert(
            "source".to_string(),
            Value::String(
                record
                    .get("source")
                    .and_then(Value::as_str)
                    .unwrap_or("manual")
                    .to_string(),
            ),
        );
        if let Some(path) = record.get("path").and_then(Value::as_str) {
            if !path.is_empty() {
                public.insert("path".to_string(), Value::String(path.to_string()));
            }
        }
        public.insert("priority".to_string(), Value::from(record.priority()));
        public.insert("enabled".to_string(), Value::Bool(record.enabled()));
        public.insert("addedAt".to_string(), Value::from(record.added_at()));
        public.insert("updatedAt".to_string(), Value::from(record.updated_at()));
        public.insert("proxy".to_string(), proxy);
        // 限额冷却标记（选路层读它决定「该账号对该模型是否在冷却期」，
        // 与 raccoon 公开形态的注释同一理由）；默认空对象
        public.insert(
            "rateLimits".to_string(),
            record.get("rateLimits").cloned().unwrap_or_else(|| Value::Object(Map::new())),
        );
        // 桌面账号实时登录态缺失（App 登出）→ available=false,与转发侧的
        // 「缺少 accessToken」401 是同一个事实的两面
        public.insert("available".to_string(), Value::Bool(!live_missing));
        Value::Object(public)
    }
}

/// 把一个值塞给单参数函数（避开 `Iterator::pipe` 命名冲突）。
trait Pipe: Sized {
    fn pipe<U, F: FnOnce(Self) -> U>(self, f: F) -> U {
        f(self)
    }
}
impl<T> Pipe for T {}
#[cfg(test)]
mod tests {
    use super::*;

    /// 删除场景的三态语义(上游第 2 轮探针发现):记录不存在必须返回
    /// Ok(false)(Stale),让调用方走 latest_credentials 的终态分支;
    /// 返回 Err 会被当成「回写失败但刷新成功」,账号删了还发新 token。
    #[test]
    fn missing_record_reports_stale_not_failure() {
        let (db, guard) = crate::server::db::test_temp::TempDb::open("lobster-accounts-missing");
        let store = AccountStore::with_db(Some(db));
        let _ = guard; // 删文件的守卫:测试结束清理临时库
        let outcome = store.update_lobster_account_tokens_if_current(
            "lobster-nonexistent",
            "old-at",
            "old-rt",
            "new-at",
            "new-rt",
            None,
        );
        assert!(
            matches!(outcome, Ok(false)),
            "记录不存在必须是 Ok(false)(Stale),实际: {:?}",
            outcome.map(|v| v.to_string())
        );
    }
}
