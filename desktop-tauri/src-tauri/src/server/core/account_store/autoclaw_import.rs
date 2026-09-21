//! AutoClaw 旧数据一次性导入（Agent2API 二期 W4b-T-c2；架构文档 §10.2 末条）。
//!
//! 从 `autoclaw_accounts.rs` 拆出（单文件行数约定）。这里只有「把两处旧数据
//! 变成 autoclaw 账号记录」这一件事：
//!   1. `~/.autoclaw-proxy/accounts.json` —— 原项目的多账号文件
//!      （`account-store.mjs` 的 `createAccountStore` 默认目录 `join(homedir(),
//!      '.autoclaw-proxy')`，**唯一事实来源**）；
//!   2. `%APPDATA%/AutoClaw/auth.json`（+ 备来源 `~/.openclaw-autoclaw/openclaw.json`）
//!      —— 桌面端实时登录态（走 `autoclaw_accounts::import_autoclaw_desktop_account`，
//!      与手动导入同一入口）。
//!
//! ── 为什么必须「一次性」─────────────────────────────────────
//! 旧文件是原项目的持久化格式，每次启动都扫一遍会把用户后来在新版里删掉的账号
//! 一次次带回来。因此导入只在「autoclaw 账号列表为空 **且** config.json 里没有
//! `autoclawImported`」时发生，做完（无论导入了多少条）立刻写标记 ——
//! 用户想再导入就手改 config。这与小浣熊的 `raccoonImported`、CatPaw 的
//! `catpawImported` 逐条同构，三个标记互不影响（各挂各的启动导入调用）。
//!
//! ── 原文件格式（`account-store.mjs` 的 `load()`）──────────────
//! ```json
//! {
//!   "currentAccountId": "user-12345" | null,
//!   "accounts": [{ "id", "name", "userId", "deviceId", "token", "refreshToken",
//!                  "tokenTail", "tokenExpiresAt", "addedAt", "updatedAt" }]
//! }
//! ```
//! 三条转换口径（**键名对照**）：
//!   - `token` → `accessToken`、`tokenExpiresAt` → `expiresAt`（本项目落盘口径，
//!     见 `autoclaw_accounts.rs` 模块头的「有意偏离」第 1 条）；
//!   - `id` **沿用原值**（原项目 `addAccount` 生成的是 `user-<userId>`）——
//!     用户能在新旧界面上对照同一条记录；撞 id 的条目跳过并记日志，绝不覆盖；
//!   - `deviceId` / `userId` 原样搬（前者是 AutoClaw 特有字段，公开形态要带出）。
//!
//! ── 与另外两家导入的一处**有意差异**：密文保留原样 ─────────────
//! 旧文件里的 `token` 是**明文**（原项目 `addAccount` 解密后才落盘），所以这里
//! 不做解密。但用户手工把 `enc:` 密文塞进旧文件也是可能的 —— 那种记录**直接
//! 导入**（不做二次解密验证）：凭证层每次取用时都会走解密链，解密不了会在转发
//! 时报出可读的中文错误，而导入期报错会让整条记录被跳过、用户还得手工排查。
//! 取舍与 `raccoon_import` 的「一条坏记录不该让整份导入失败」同一取向。
//!
//! ── 硬约束 ────────────────────────────────────────────────
//! 本文件只做「读旧文件 + 写数据库」，**没有任何网络请求**；绝不 unwrap/expect
//! （release 是 panic=abort）。

use std::path::PathBuf;

use serde_json::{json, Map, Value};

use crate::server::core::account_store::priority::next_free_priority;
use crate::server::core::account_store::sql;
use crate::server::core::account_store::state::StoredAccount;
use crate::server::core::account_store::store::AccountStore;
use crate::server::core::account_store::store_util::{strip_bearer_prefix, token_tail_of, truncate_chars};
use crate::server::core::account_store::MAX_ACCOUNTS;
use crate::server::core::providers::{kind_id, ProviderKind};
use crate::server::logging;

/// AutoClaw 的 provider id（`providers::kind_id` 的常量形态）
fn autoclaw_id() -> &'static str {
    kind_id(ProviderKind::AutoClaw)
}

/// 旧代理的账号目录（原项目 `~/.autoclaw-proxy`）
fn legacy_directory() -> PathBuf {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    home.join(".autoclaw-proxy")
}

/// config.json 里的一次性导入标记（与小浣熊的 `raccoonImported` 同构）
const IMPORT_FLAG_KEY: &str = "autoclawImported";

/// userId / deviceId 的长度上限（与 `autoclaw_accounts` 同一防护口径）
const MAX_IDENTITY_LENGTH: usize = 256;

impl AccountStore {
    /// 启动时的 AutoClaw 旧数据导入（架构文档 §10.2 末条的导入挂点）。
    ///
    /// 触发条件（两个都要满足）：
    ///   ① `provider="autoclaw"` 的账号列表为空；
    ///   ② config.json 里没有 `autoclawImported: true`。
    ///
    /// 来源（各自独立，缺失/损坏只跳过并记日志）：
    ///   1. `~/.autoclaw-proxy/accounts.json`（原项目多账号）→ 逐条转成 autoclaw
    ///      账号，**id 沿用原值**；撞 id 的条目跳过并记日志；
    ///   2. 桌面端实时登录态（`%APPDATA%/AutoClaw/auth.json`，备来源
    ///      `~/.openclaw-autoclaw/openclaw.json`）→ 追加 `autoclaw-desktop` 实时账号。
    ///
    /// 完成后**无论有没有导入成功都写标记**（两个来源都空也写）：否则每次启动
    /// 都会重扫一遍，而用户的「我不想要这些旧账号」只能靠改 config 表达。
    ///
    /// 优先级一律排在 autoclaw 组末尾（`next_free_priority`），不打乱既有顺序。
    pub fn import_legacy_autoclaw_data(&self) -> Value {
        let autoclaw = autoclaw_id();
        {
            let _guard = self.guard();
            // 本家账号数为 0（投影列 COUNT）—— 触发条件之一，只问「有没有」。
            // 改造前这里要读全量再 any() 一遍，等于为了一个布尔值解析 20 条记录。
            // 查询失败（库不可用）按「有账号」处理并跳过：本函数是启动期的
            // **一次性**导入，库不可用时顺势去写只会再失败一次并刷屏；
            // 账号功能整体已不可用这件事由别处的 ❌ 日志说明。
            let has_autoclaw = match self
                .with_conn(&_guard, |conn| sql::count_by_provider(conn, autoclaw))
            {
                Ok(count) => count > 0,
                Err(error) => {
                    logging::log(
                        "[Accounts]",
                        &format!("⚠️  读取AutoClaw账号数失败，跳过旧数据导入: {error}"),
                    );
                    true
                }
            };
            let flagged = crate::server::config::current()
                .raw()
                .get(IMPORT_FLAG_KEY)
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if has_autoclaw || flagged {
                return json!({
                    "skipped": true,
                    "reason": if has_autoclaw { "已有 AutoClaw 账号" } else { "已完成过导入" },
                });
            }
        }
        let mut imported = 0usize;
        let mut skipped = 0usize;
        // 来源 1：原项目的多账号文件
        match self.import_legacy_autoclaw_accounts_file() {
            Ok((added, skipped_items)) => {
                imported += added;
                skipped += skipped_items;
                if added > 0 || skipped_items > 0 {
                    logging::log(
                        "[Accounts]",
                        &format!(
                            "📥 已从 AutoClaw 旧代理导入 {added} 个账号{}",
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
                &format!("ℹ️  未导入 AutoClaw 旧代理账号（{reason}）"),
            ),
        }
        // 来源 2：桌面端登录态（实时解密链可用即导入；读不到只记 verbose）
        match self.import_autoclaw_desktop_account("imported") {
            Ok(_) => {
                imported += 1;
            }
            Err(error) => logging::verbose(
                "[Accounts]",
                &format!("未导入 AutoClaw 桌面端登录态：{}", error.message),
            ),
        }
        // 标记（无论导入多少条都写）
        crate::server::config::update_raw_field(IMPORT_FLAG_KEY, Value::Bool(true));
        if imported == 0 {
            logging::log(
                "[Accounts]",
                "ℹ️  未发现可导入的 AutoClaw 旧数据（已记录标记，后续启动不再扫描）",
            );
        } else {
            logging::log(
                "[Accounts]",
                &format!("✅ AutoClaw 旧数据导入完成：共 {imported} 条（跳过 {skipped} 条）"),
            );
        }
        json!({ "skipped": false, "imported": imported, "failed": skipped })
    }

    /// 读原项目的账号文件并逐条导入。
    ///
    /// 返回 `(导入条数, 跳过条数)`；文件缺失/损坏返回 Err（原因用于日志）。
    /// 键名对照见模块头：`token` → `accessToken`、`tokenExpiresAt` → `expiresAt`、
    /// `id` 沿用原值、`userId` / `deviceId` 原样搬。
    fn import_legacy_autoclaw_accounts_file(&self) -> Result<(usize, usize), String> {
        let path = legacy_directory().join("accounts.json");
        let text = std::fs::read_to_string(&path)
            .map_err(|error| format!("未找到旧代理账号文件 {}（{error}）", path.display()))?;
        let value: Value = serde_json::from_str(&text)
            .map_err(|_| format!("AutoClaw 旧代理账号文件无法解析: {}", path.display()))?;
        let items = value
            .get("accounts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if items.is_empty() {
            return Err("AutoClaw 旧代理账号文件里没有账号".to_string());
        }
        let autoclaw = autoclaw_id();
        let _guard = self.guard();
        // 导入前把「已占用的 id」与「已占用的优先级」各取一次（各一列，
        // 不解析记录），循环里在内存中增量维护 —— 同批新加的账号也必须参与
        // 去重与号段判定，否则同一批里的重复 id / 重号会落库。
        // 改造前这两件事都靠「读全量 state 再遍历」实现。
        let mut taken_ids: std::collections::HashSet<String> = self
            .with_conn(&_guard, |conn| sql::all_ids(conn))
            .map_err(|error| error.message)?
            .into_iter()
            .collect();
        let mut used_priorities: Vec<i64> = self
            .with_conn(&_guard, |conn| sql::priorities_all(conn))
            .map_err(|error| error.message)?;
        let mut added = 0usize;
        let mut skipped = 0usize;
        // 本批要写入的记录：循环里只组装，最后一次性进事务（见循环后的说明）
        let mut fresh: Vec<StoredAccount> = Vec::new();
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
            // 原项目的键名是 `token`（明文；`accessToken` / `access_token` 只是容错候选）
            let token = ["token", "accessToken", "access_token"]
                .iter()
                .find_map(|key| object.get(*key).and_then(Value::as_str))
                .map(strip_bearer_prefix)
                .map(|value| value.trim().to_string())
                .unwrap_or_default();
            if id.is_empty() || token.is_empty() {
                skipped += 1;
                logging::verbose(
                    "[Accounts]",
                    &format!("AutoClaw 旧账号缺少 id 或 token，已跳过（id={id}）"),
                );
                continue;
            }
            if taken_ids.contains(&id) {
                skipped += 1;
                logging::log(
                    "[Accounts]",
                    &format!("⚠️  AutoClaw 旧账号 {id} 与本机记录同 id，已跳过（不覆盖现有数据）"),
                );
                continue;
            }
            // 账号总数上限：已存在的 + 本批已收下的
            if taken_ids.len() >= MAX_ACCOUNTS {
                skipped += 1;
                logging::log(
                    "[Accounts]",
                    &format!("⚠️  账号数已达上限 {MAX_ACCOUNTS}，AutoClaw 旧账号 {id} 未导入"),
                );
                continue;
            }
            let field = |key: &str| -> String {
                object
                    .get(key)
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(|value| truncate_chars(value, MAX_IDENTITY_LENGTH))
                    .unwrap_or_default()
            };
            let user_id = field("userId");
            let device_id = field("deviceId");
            let refresh_token = ["refreshToken", "refresh_token"]
                .iter()
                .find_map(|key| object.get(*key).and_then(Value::as_str))
                .map(strip_bearer_prefix)
                .map(|value| value.trim().to_string())
                .unwrap_or_default();
            let name = object
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| truncate_chars(value, 100))
                .unwrap_or_else(|| {
                    if user_id.is_empty() {
                        format!("账号 {id}")
                    } else {
                        format!("账号 {user_id}")
                    }
                });
            // 号段取**全部**账号：优先级全局唯一（四家共用一条队列），
            // 只看本家会让新账号撞上别家已在用的号（见 priority.rs 模块头）。
            // `used_priorities` 随本批的每次分配增量更新，所以同批内不会重号。
            let priority = next_free_priority(&used_priorities);
            let now = logging::now_ms();
            let mut record = Map::new();
            record.insert("id".to_string(), Value::String(id.clone()));
            record.insert("provider".to_string(), Value::String(autoclaw.to_string()));
            record.insert("name".to_string(), Value::String(name));
            record.insert("userId".to_string(), Value::String(user_id));
            if !device_id.is_empty() {
                record.insert("deviceId".to_string(), Value::String(device_id));
            }
            record.insert("accessToken".to_string(), Value::String(token.clone()));
            if !refresh_token.is_empty() {
                record.insert(
                    "refreshToken".to_string(),
                    Value::String(refresh_token),
                );
            }
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
            // `tokenExpiresAt` 是原项目的键名；新旧两种都认（旧文件用前者）
            let expires_at = object
                .get("tokenExpiresAt")
                .and_then(Value::as_f64)
                .or_else(|| object.get("expiresAt").and_then(Value::as_f64));
            if let Some(expires_at) = expires_at.filter(|value| value.is_finite() && *value > 0.0) {
                record.insert(
                    "expiresAt".to_string(),
                    crate::server::core::account_store::state::json_number(expires_at),
                );
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
            taken_ids.insert(id.clone());
            used_priorities.push(priority);
            fresh.push(StoredAccount::from_map(record));
            added += 1;
        }
        if !fresh.is_empty() {
            // 整批一个事务：要么全部导入、要么一条都不导入。中断留下「部分导入」
            // 会让「本家账号列表为空」这个触发条件永远不再成立（下一次启动看到
            // 有账号就跳过导入），于是剩下的旧账号再也没机会进来。
            // 逐条 `put` 而不是整份状态差异写：本批全是新增，而库里别的记录
            // 这一轮没有被碰过 —— 重写它们纯属多余（改造前正是整份重写）。
            self.with_conn_mut(&_guard, |conn| {
                let tx = conn.transaction()?;
                for record in &fresh {
                    sql::put(&tx, record)?;
                }
                tx.commit()
            })
            .map_err(|error| format!("导入落盘失败: {}", error.message))?;
        }
        Ok((added, skipped))
    }
}
