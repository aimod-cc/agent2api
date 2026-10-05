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

use crate::server::core::providers::lobster::credentials::{
    extract_credentials, read_auth_tokens_from_db, snapshot_for_desktop,
};
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

/// token 指纹（最后 4 字符），UI 列展示用
fn token_tail(token: &str) -> String {
    let bytes = token.as_bytes();
    let start = bytes.len().saturating_sub(4);
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

impl AccountStore {
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
}

/// 把一个值塞给单参数函数（避开 `Iterator::pipe` 命名冲突）。
trait Pipe: Sized {
    fn pipe<U, F: FnOnce(Self) -> U>(self, f: F) -> U {
        f(self)
    }
}
impl<T> Pipe for T {}

/// 占位：保留 read_auth_tokens_from_db 与 extract_credentials 引用路径以免编译告警
#[allow(dead_code)]
fn _unused_keep_alive() {
    let _ = read_auth_tokens_from_db;
    let _ = extract_credentials;
}