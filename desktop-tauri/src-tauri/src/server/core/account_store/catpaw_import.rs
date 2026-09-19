//! CatPaw 旧数据一次性导入（Agent2API 二期 W5-T-d4；对照架构文档 §3.3 的模式）。
//!
//! 从 `catpaw_accounts.rs` 拆出（单文件行数约定）。这里只有「把两处旧数据
//! 变成 catpaw 账号记录」这一件事：
//!   1. `~/.meituan-catpaw/catpaw-proxy-accounts.json` —— 原项目的多账号文件
//!      （`account-store.mjs` 的 `createAccountStore` 默认路径，**唯一事实来源**）；
//!   2. `~/.meituan-catpaw/auth.json` —— 桌面端实时登录态（走
//!      `catpaw_accounts::import_catpaw_desktop_account`，与手动导入同一入口）。
//!
//! ── 为什么必须「一次性」─────────────────────────────────────
//! 旧文件是原项目的持久化格式，每次启动都扫一遍会把用户后来在新版里删掉的账号
//! 一次次带回来。因此导入只在「catpaw 账号列表为空 **且** config.json 里没有
//! `catpawImported`」时发生，做完（无论导入了多少条）立刻写标记 ——
//! 用户想再导入就手改 config。这与小浣熊的 `raccoonImported` 逐条同构。
//!
//! ── 原文件格式（`account-store.mjs` 的 `load()`）──────────────
//! ```json
//! {
//!   "currentAccountId": "12345" | null,
//!   "accounts": [{ "id", "name", "loginName", "uid", "tokenTail",
//!                  "accessToken", "addedAt", "updatedAt" }],
//!   "balanceCookies": { "<id>": { "token2", "token2Tail", "uid", "savedAt" } }
//! }
//! ```
//! `accounts[i].id` 是 `uid || loginName`（原项目 `addAccount` 就是这么生成的），
//! 本模块**沿用原 id**（不加前缀）—— 用户能在新旧界面上对照同一条记录。
//! `balanceCookies` 明确不消费（本项目不迁移余额功能，见 `catpaw_accounts` 的
//! 模块头），但**逐账号搬到记录的 `balanceCookie` 字段**上：一条 import 不丢数据，
//! 将来要接回余额功能时有原始凭证可用。
//!
//! ── 硬约束 ────────────────────────────────────────────────
//! 本文件全是文件读写，**没有任何网络请求**；绝不 unwrap/expect
//! （release 是 panic=abort）。

use serde_json::{json, Map, Value};

use crate::server::core::account_store::priority::next_free_priority;
use crate::server::core::account_store::state::StoredAccount;
use crate::server::core::account_store::store::AccountStore;
use crate::server::core::account_store::store_util::{strip_bearer_prefix, token_tail_of, truncate_chars};
use crate::server::core::account_store::MAX_ACCOUNTS;
use crate::server::core::providers::catpaw::credentials;
use crate::server::core::providers::{kind_id, ProviderKind};
use crate::server::logging;

/// CatPaw 的 provider id
fn catpaw_id() -> &'static str {
    kind_id(ProviderKind::CatPaw)
}

/// config.json 里的一次性导入标记（与小浣熊的 `raccoonImported` 同构）
const IMPORT_FLAG_KEY: &str = "catpawImported";

/// uid / loginName 长度上限（原项目 `MAX_USER_UID_LENGTH`）
const MAX_USER_UID_LENGTH: usize = 256;

impl AccountStore {
    /// 启动时的 CatPaw 旧数据导入（架构文档 §9 的导入挂点）。
    ///
    /// 触发条件（两个都要满足）：
    ///   ① `provider="catpaw"` 的账号列表为空；
    ///   ② config.json 里没有 `catpawImported: true`。
    ///
    /// 来源（各自独立，缺失/损坏只跳过并记日志）：
    ///   1. `~/.meituan-catpaw/catpaw-proxy-accounts.json`（原项目多账号）→
    ///      逐条转成 catpaw 账号，**id 沿用原值**；撞 id 的条目跳过并记日志；
    ///   2. `~/.meituan-catpaw/auth.json` → 追加 `desktop-auth` 实时账号
    ///      （只要文件在就导入）。
    ///
    /// 完成后**无论有没有导入成功都写标记**（两个来源都空也写）：否则每次启动
    /// 都会重扫一遍，而用户的「我不想要这些旧账号」只能靠改 config 表达。
    ///
    /// 优先级一律排在 catpaw 组末尾（`next_free_priority`），不打乱既有顺序。
    pub fn import_legacy_catpaw_data(&self) -> Value {
        let catpaw = catpaw_id();
        {
            let _guard = self.guard();
            let state = self.load(&_guard);
            let has_catpaw = state.accounts.iter().any(|item| item.provider() == catpaw);
            let flagged = crate::server::config::current()
                .raw()
                .get(IMPORT_FLAG_KEY)
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if has_catpaw || flagged {
                return json!({
                    "skipped": true,
                    "reason": if has_catpaw { "已有 CatPaw 账号" } else { "已完成过导入" },
                });
            }
        }
        let mut imported = 0usize;
        let mut skipped = 0usize;
        // 来源 1：原项目的多账号文件
        match self.import_legacy_catpaw_accounts_file() {
            Ok((added, skipped_items)) => {
                imported += added;
                skipped += skipped_items;
                if added > 0 || skipped_items > 0 {
                    logging::log(
                        "[Accounts]",
                        &format!(
                            "📥 已从 CatPaw 旧代理导入 {added} 个账号{}",
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
                &format!("ℹ️  未导入 CatPaw 旧代理账号（{reason}）"),
            ),
        }
        // 来源 2：桌面端登录态（auth.json 存在即导入）
        match self.import_catpaw_desktop_account("imported") {
            Ok(_) => {
                imported += 1;
            }
            Err(error) => logging::verbose(
                "[Accounts]",
                &format!("未导入 CatPaw 桌面端登录态：{}", error.message),
            ),
        }
        // 标记（无论导入多少条都写）
        crate::server::config::update_raw_field(IMPORT_FLAG_KEY, Value::Bool(true));
        if imported == 0 {
            logging::log(
                "[Accounts]",
                "ℹ️  未发现可导入的 CatPaw 旧数据（已记录标记，后续启动不再扫描）",
            );
        } else {
            logging::log(
                "[Accounts]",
                &format!("✅ CatPaw 旧数据导入完成：共 {imported} 条（跳过 {skipped} 条）"),
            );
        }
        json!({ "skipped": false, "imported": imported, "failed": skipped })
    }

    /// 读原项目的账号文件并逐条导入。
    ///
    /// 返回 `(导入条数, 跳过条数)`；文件缺失/损坏返回 Err（原因用于日志）。
    /// 每条记录转成 catpaw 账号：id 沿用原值、`accessToken` 与 uid/loginName
    /// 保留、`balanceCookies[id]` 搬到记录的 `balanceCookie`（保留不消费）。
    fn import_legacy_catpaw_accounts_file(&self) -> Result<(usize, usize), String> {
        let path = credentials::catpaw_home().join("catpaw-proxy-accounts.json");
        let text = std::fs::read_to_string(&path)
            .map_err(|error| format!("未找到 CatPaw 旧代理账号文件 {}（{error}）", path.display()))?;
        let value: Value = serde_json::from_str(&text)
            .map_err(|_| format!("CatPaw 旧代理账号文件无法解析: {}", path.display()))?;
        let items = value
            .get("accounts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if items.is_empty() {
            return Err("CatPaw 旧代理账号文件里没有账号".to_string());
        }
        // 顶层 balanceCookies：按 id 索引的余额查询凭证（保留不消费）
        let balance_cookies = value
            .get("balanceCookies")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let catpaw = catpaw_id();
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
            // 原项目的键名是 `accessToken`；`token` / `access_token` 只是容错候选
            let token = ["accessToken", "token", "access_token"]
                .iter()
                .find_map(|key| object.get(*key).and_then(Value::as_str))
                .map(strip_bearer_prefix)
                .map(|value| value.trim().to_string())
                .unwrap_or_default();
            if id.is_empty() || token.is_empty() || !valid_field(&id, MAX_USER_UID_LENGTH) {
                skipped += 1;
                logging::verbose(
                    "[Accounts]",
                    &format!("CatPaw 旧账号缺少 id 或 token，已跳过（id={id}）"),
                );
                continue;
            }
            if state.accounts.iter().any(|item| item.id() == id) {
                skipped += 1;
                logging::log(
                    "[Accounts]",
                    &format!("⚠️  CatPaw 旧账号 {id} 与本机记录同 id，已跳过（不覆盖现有数据）"),
                );
                continue;
            }
            if state.accounts.len() >= MAX_ACCOUNTS {
                skipped += 1;
                logging::log(
                    "[Accounts]",
                    &format!("⚠️  账号数已达上限 {MAX_ACCOUNTS}，CatPaw 旧账号 {id} 未导入"),
                );
                continue;
            }
            let uid = object
                .get("uid")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| valid_field(value, MAX_USER_UID_LENGTH))
                .unwrap_or("")
                .to_string();
            let login_name = object
                .get("loginName")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| valid_field(value, MAX_USER_UID_LENGTH))
                .unwrap_or("")
                .to_string();
            let name = object
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| truncate_chars(value, 100))
                .unwrap_or_else(|| format!("账号 {id}"));
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
            record.insert("provider".to_string(), Value::String(catpaw.to_string()));
            record.insert("name".to_string(), Value::String(name));
            record.insert("uid".to_string(), Value::String(uid));
            record.insert("loginName".to_string(), Value::String(login_name));
            record.insert("accessToken".to_string(), Value::String(token.clone()));
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
            // 余额查询凭证：保留不消费（见模块头）。原项目按 id 索引，
            // 本记录就是那个 id 对应的账号，于是搬到记录内一个单数键上。
            if let Some(cookie) = balance_cookies.get(&id) {
                if !cookie.is_null() {
                    record.insert("balanceCookie".to_string(), cookie.clone());
                }
            }
            record.insert("source".to_string(), Value::String("imported".to_string()));
            record.insert("priority".to_string(), Value::from(priority));
            record.insert("enabled".to_string(), Value::Bool(true));
            // 时间戳沿用原值（用户能在界面上对照旧代理的时间线），缺失才补现在
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
    }
}

/// 字段是否可用（原项目 `safeString` 的口径：非空、不超长、不含 `[\r\n;]`）
///
/// 导入路径上的失败**只跳过该条**（不报错）：旧文件是别人写的，一条坏记录
/// 不该让整份导入失败 —— 与 `raccoon_import` 同一取向。
fn valid_field(value: &str, max_length: usize) -> bool {
    !value.trim().is_empty()
        && value.chars().count() <= max_length
        && !value.contains(['\r', '\n', ';'])
}
