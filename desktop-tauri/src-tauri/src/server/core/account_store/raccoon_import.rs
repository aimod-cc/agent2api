//! 小浣熊旧数据一次性导入（Agent2API 改造 W3-T4；架构文档 §3.3）。
//!
//! 从 `raccoon_accounts.rs` 拆出（单文件行数约定）。这里只有「把两处旧数据
//! 变成 raccoon 账号记录」这一件事：
//!   1. `~/.raccoon-proxy/accounts.json` —— 旧网关的多账号文件；
//!   2. `~/.box-agent/config/auth.json` —— 桌面端实时登录态（走
//!      `raccoon_accounts::import_raccoon_desktop_account`，与手动导入同一入口）。
//!
//! ── 为什么必须「一次性」─────────────────────────────────────
//! 旧文件是**另一套字段命名**（`token` / `tokenExpiresAt`），每次启动都扫一遍
//! 会把用户后来在新版里删掉的账号一次次带回来。因此导入只在
//! 「raccoon 账号列表为空 **且** config.json 里没有 `raccoonImported`」时发生，
//! 做完（无论导入了多少条）立刻写标记 —— 用户想再导入就手改 config。
//!
//! ── id 沿用原值 ────────────────────────────────────────────
//! 旧记录的 id 形如 `user-7160938`，与 workbuddy 的 `user-<uid>` **形态相同但
//! 空间独立**（两家各有一套数字账号）。沿用原值让用户能在新界面上对照旧网关
//! 的列表；撞 id（本机已有同 id 记录）时跳过该条并打日志，绝不覆盖。
//!
//! ── 硬约束 ────────────────────────────────────────────────
//! 本文件全是文件读写，**没有任何网络请求**；绝不 unwrap/expect
//! （release 是 panic=abort）。

use std::path::PathBuf;

use serde_json::{json, Map, Value};

use crate::server::core::account_store::priority::next_free_priority;
use crate::server::core::account_store::state::StoredAccount;
use crate::server::core::account_store::store::AccountStore;
use crate::server::core::account_store::store_util::{pick_token, token_tail_of, truncate_chars};
use crate::server::core::account_store::MAX_ACCOUNTS;
use crate::server::core::providers::raccoon::jwt;
use crate::server::core::providers::kind_id;
use crate::server::core::providers::ProviderKind;
use crate::server::logging;

/// 小浣熊的 provider id
fn raccoon_id() -> &'static str {
    kind_id(ProviderKind::Raccoon)
}

/// 旧网关的账号目录（源项目 `~/.raccoon-proxy`）
fn legacy_directory() -> PathBuf {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    home.join(".raccoon-proxy")
}

/// config.json 里的一次性导入标记（架构文档 §3.3）
const IMPORT_FLAG_KEY: &str = "raccoonImported";

/// 身份字段（office_*）的长度上限（源项目 `MAX_IDENTITY_LENGTH`）
const MAX_IDENTITY_LENGTH: usize = 1024;

impl AccountStore {
    /// 启动时的小浣熊旧数据导入。
    ///
    /// 触发条件（两个都要满足）：
    ///   ① `provider="raccoon"` 的账号列表为空；
    ///   ② config.json 里没有 `raccoonImported: true`。
    ///
    /// 来源（各自独立，缺失/损坏只跳过并记日志）：
    ///   1. `~/.raccoon-proxy/accounts.json`（旧网关多账号）→ 逐条转成 raccoon
    ///      账号，**id 保留原值**（如 `user-7160938`，与 workbuddy 的 id 空间
    ///      不冲突，用户可对照旧网关的界面识别）；撞 id 的条目跳过并记日志。
    ///   2. `~/.box-agent/config/auth.json` → 追加 `raccoon-desktop` 实时账号。
    ///
    /// 完成后**无论有没有导入成功都写标记**（两个来源都空也写）：否则每次启动
    /// 都会重扫一遍，而用户的「我不想要这些旧账号」只能靠改 config 表达。
    ///
    /// 优先级一律排在 raccoon 组末尾（`next_free_priority`），不打乱既有顺序。
    pub fn import_legacy_raccoon_data(&self) -> Value {
        let raccoon = raccoon_id();
        {
            let _guard = self.guard();
            let state = self.load(&_guard);
            let has_raccoon = state.accounts.iter().any(|item| item.provider() == raccoon);
            let flagged = crate::server::config::current()
                .raw()
                .get(IMPORT_FLAG_KEY)
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if has_raccoon || flagged {
                return json!({ "skipped": true, "reason": if has_raccoon { "已有小浣熊账号" } else { "已完成过导入" } });
            }
        }
        let mut imported = 0usize;
        let mut skipped = 0usize;
        // 来源 1：旧网关账号文件
        match self.import_legacy_accounts_file() {
            Ok((added, skipped_items)) => {
                imported += added;
                skipped += skipped_items;
                if added > 0 || skipped_items > 0 {
                    logging::log(
                        "[Accounts]",
                        &format!(
                            "📥 已从旧网关导入 {added} 个小浣熊账号{}",
                            if skipped_items > 0 {
                                format!("（{skipped_items} 条因 id 冲突或数据不完整跳过）")
                            } else {
                                String::new()
                            }
                        ),
                    );
                }
            }
            Err(reason) => logging::log(
                "[Accounts]",
                &format!("ℹ️  未导入旧网关账号（{reason}）"),
            ),
        }
        // 来源 2：桌面端登录态
        match self.import_raccoon_desktop_account("imported") {
            Ok(_) => {
                imported += 1;
            }
            Err(error) => logging::verbose(
                "[Accounts]",
                &format!("未导入小浣熊桌面端登录态：{}", error.message),
            ),
        }
        // 标记（无论导入多少条都写）
        crate::server::config::update_raw_field(IMPORT_FLAG_KEY, Value::Bool(true));
        if imported == 0 {
            logging::log(
                "[Accounts]",
                "ℹ️  未发现可导入的小浣熊旧数据（已记录标记，后续启动不再扫描）",
            );
        } else {
            logging::log(
                "[Accounts]",
                &format!("✅ 小浣熊旧数据导入完成：共 {imported} 条（跳过 {skipped} 条）"),
            );
        }
        json!({ "skipped": false, "imported": imported, "failed": skipped })
    }

    /// 读 `~/.raccoon-proxy/accounts.json` 并逐条导入。
    ///
    /// 返回 `(导入条数, 跳过条数)`；文件缺失/损坏返回 Err（原因用于日志）。
    /// 旧文件的形态（源项目 `account-store.mjs` 的 `load`）：
    /// `{ currentAccountId, accounts: [{ id, name, userId, token, refreshToken,
    /// tokenTail, tokenExpiresAt, addedAt, updatedAt, office* }] }`。
    fn import_legacy_accounts_file(&self) -> Result<(usize, usize), String> {
        let path = legacy_directory().join("accounts.json");
        let text = std::fs::read_to_string(&path)
            .map_err(|error| format!("未找到旧网关账号文件 {}（{error}）", path.display()))?;
        let value: Value = serde_json::from_str(&text)
            .map_err(|_| format!("旧网关账号文件无法解析: {}", path.display()))?;
        let items = value
            .get("accounts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if items.is_empty() {
            return Err("旧网关账号文件里没有账号".to_string());
        }
        let raccoon = raccoon_id();
        let _guard = self.guard();
        let mut state = self.load(&_guard);
        let mut added = 0usize;
        let mut skipped = 0usize;
        for item in items {
            let Some(object) = item.as_object() else {
                skipped += 1;
                continue;
            };
            let id = object
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            // 旧文件里的 token 键名是 `token`（源项目命名），新记录统一成 accessToken
            let token = pick_token(object, &["token", "accessToken", "access_token"]);
            let refresh_token = pick_token(object, &["refreshToken", "refresh_token"]);
            if id.is_empty() || token.is_empty() {
                skipped += 1;
                logging::verbose(
                    "[Accounts]",
                    &format!("旧小浣熊账号缺少 id 或 token，已跳过（id={id}）"),
                );
                continue;
            }
            if state.accounts.iter().any(|item| item.id() == id) {
                skipped += 1;
                logging::log(
                    "[Accounts]",
                    &format!("⚠️  旧小浣熊账号 {id} 与本机记录同 id，已跳过（不覆盖现有数据）"),
                );
                continue;
            }
            if state.accounts.len() >= MAX_ACCOUNTS {
                skipped += 1;
                logging::log(
                    "[Accounts]",
                    &format!("⚠️  账号数已达上限 {MAX_ACCOUNTS}，旧小浣熊账号 {id} 未导入"),
                );
                continue;
            }
            let user_id = object
                .get("userId")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .or_else(|| {
                    jwt::decode_jwt_claims(&token).map(|claims| jwt::extract_user_id(&claims))
                })
                .unwrap_or_default();
            let expires_at = object
                .get("tokenExpiresAt")
                .and_then(Value::as_f64)
                .or_else(|| jwt::jwt_expiry_ms(&token));
            let name = object
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| truncate_chars(value, 100))
                .unwrap_or_else(|| format!("账号 {user_id}"));
            let priority = next_free_priority(
                // 号段取**全部**账号：优先级全局唯一（四家共用一条队列），
                // 只看本家会让新账号撞上别家已在用的号（见 priority.rs 模块头）
                &state
                    .accounts
                    .iter()
                    .map(StoredAccount::priority)
                    .collect::<Vec<_>>(),
            );
            let now = logging::now_ms();
            let mut record = Map::new();
            record.insert("id".to_string(), Value::String(id.clone()));
            record.insert("provider".to_string(), Value::String(raccoon.to_string()));
            record.insert("name".to_string(), Value::String(name));
            record.insert("userId".to_string(), Value::String(user_id));
            record.insert("accessToken".to_string(), Value::String(token.clone()));
            record.insert(
                "refreshToken".to_string(),
                Value::String(refresh_token.clone()),
            );
            record.insert(
                "tokenTail".to_string(),
                Value::String(
                    object
                        .get("tokenTail")
                        .and_then(Value::as_str)
                        .filter(|value| !value.is_empty())
                        .map(str::to_string)
                        .unwrap_or_else(|| token_tail_of(&token)),
                ),
            );
            record.insert(
                "expiresAt".to_string(),
                expires_at
                    .map(crate::server::core::account_store::state::json_number)
                    .unwrap_or(Value::Null),
            );
            for (key, source_key) in [
                ("officeIdentity", "officeIdentity"),
                ("officeOrgName", "officeOrgName"),
                ("officeOrgRole", "officeOrgRole"),
            ] {
                if let Some(Value::String(text)) = object.get(source_key) {
                    if !text.trim().is_empty() {
                        record.insert(
                            key.to_string(),
                            Value::String(truncate_chars(text.trim(), MAX_IDENTITY_LENGTH)),
                        );
                    }
                }
            }
            record.insert("source".to_string(), Value::String("imported".to_string()));
            record.insert("priority".to_string(), Value::from(priority));
            record.insert("enabled".to_string(), Value::Bool(true));
            // 时间戳沿用原值（用户能在界面上对照旧网关的时间线），缺失才补现在
            record.insert(
                "addedAt".to_string(),
                object
                    .get("addedAt")
                    .cloned()
                    .filter(|value| value.as_i64().map(|item| item != 0).unwrap_or(false))
                    .unwrap_or_else(|| Value::from(now)),
            );
            record.insert(
                "updatedAt".to_string(),
                object
                    .get("updatedAt")
                    .cloned()
                    .filter(|value| value.as_i64().map(|item| item != 0).unwrap_or(false))
                    .unwrap_or_else(|| Value::from(now)),
            );
            state.accounts.push(StoredAccount::from_map(record));
            added += 1;
        }
        if added > 0 {
            self.save(&state, &_guard)
                .map_err(|error| format!("导入落盘失败: {}", error.message))?;
        }
        Ok((added, skipped))
    }}
