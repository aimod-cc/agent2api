//! Cline 账号：手动添加、桌面端实时账号、续期回写、公开形态。
//!
//! ── 与其他五家的关系 ────────────────────────────────────────
//! 各家各有一条添加路径，因为凭证形态各不相同。Cline 这条是**最简单**的一种：
//! 凭证就是一个 WorkOS access token（带 `workos:` 前缀）+ refresh token +
//! 过期时间，没有 JWT 声明要校验（不像 AutoClaw 要解 `user_id`）、没有第二凭证
//! （不像 CatPaw 的 `token2`）。因此本文件的校验比另外几家都薄 —— 只检查
//! 「token 非空 + 长度合理」，剩下交给凭证层（转发时 401 会如实反映）。
//!
//! ── 两个池 = 两个 provider（本文件的参数化方式）──────────────
//! Cline 按额度池拆成了 `cline-free` / `cline-pass` 两家（理由见
//! `providers::cline::models` 的模块头）。账号形态**完全一样**，差别只有
//! `provider` 字段的值与账号 id 的前缀 —— 因此本文件的每个入口都收一个
//! `provider: &str`（调用方传 `providers::kind_id(kind)`），内部不做任何
//! 池特有的分支。
//!
//! **账号 id 带 provider 前缀**（`cline-free-usr-…` / `cline-pass-usr-…`）：
//! 同一个 Cline 账号两个池都能用，用户完全可以把它加两次（各服务一个池）。
//! id 若只带账号标识，两次添加会撞 id 而被当成「更新同一条」，第二个池就
//! 永远加不进去 —— 前缀正是为了让「同一个账号的两条记录」能并存。
//!
//! ── 账号字段 ────────────────────────────────────────────────
//! ```text
//!   { id, provider: "cline-free" | "cline-pass", name, account,
//!     accessToken, refreshToken, tokenTail, expiresAt, priority, enabled,
//!     desktop, source, addedAt, updatedAt, rateLimits }
//! ```
//! 与另外几家共用账号存储的通用设施（优先级号段、启用开关、出网代理、
//! 限额冷却）。键名沿用 W3 为小浣熊确立的口径（`accessToken` /
//! `refreshToken` / `expiresAt`）。
//!
//! **不再有 `pool` 字段**（拆分前有）：池已经是 provider 身份的一部分，
//! 留着它只会变成第二处事实。存量记录里的这个字段由
//! `store_admin::migrate_startup` 在启动时读掉并删除（那是**唯一**还认识它的
//! 地方）。
//!
//! ── 桌面端账号的凭证**不落盘** ──────────────────────────────
//! 与另外几家的「导入桌面端登录态」同一纪律：记录里只有 `desktop: true`，
//! 真正的 token 每次实时读 `~/.cline/data/settings/providers.json`
//! （见 `providers::cline::refresh::snapshot_for`）。这样 Cline 客户端自己
//! 续期后网关立刻跟上，也让「删掉这条记录不影响客户端登录态」成立。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! 本文件全是「读-改-写」文件操作，**没有任何网络请求**（持锁不做网络）。
//! 绝不 unwrap/expect（release 是 panic=abort）。

use serde_json::{Map, Value};

use crate::server::core::account_store::priority::next_free_priority;
use crate::server::core::account_store::state::StoredAccount;
use crate::server::core::account_store::store::{AccountStore, AccountStoreError};
use crate::server::core::account_store::store_util::{token_tail_of, truncate_chars};
use crate::server::core::account_store::{
    is_cline_family, CredentialWrite, MAX_ACCOUNTS, MAX_TOKEN_LENGTH,
};
use crate::server::core::providers::cline::credentials;
use crate::server::logging;

/// 备注名长度上限（与另外几家一致）
const MAX_NAME_LENGTH: usize = 100;

/// 账号标识长度上限（email / usr- id 会进界面）
const MAX_IDENTITY_LENGTH: usize = 256;

/// 拼一个 Cline 账号的记录 id：`<provider>-<账号标识>`。
///
/// 前缀必须带（理由见模块头：同一个 Cline 账号可以两个池各加一条），
/// 而标识为空时由调用方给出 token 指纹兜底。
fn record_id(provider: &str, key: &str) -> String {
    format!("{provider}-{}", truncate_chars(key, MAX_IDENTITY_LENGTH))
}

impl AccountStore {
    // ─── 读：账号记录 ────────────────────────────────────────

    /// 取某个池的 Cline 账号的**原始记录**（含 accessToken；桌面端账号的
    /// token 不在此，由调用方实时读 providers.json）。
    ///
    /// `account_id` 为空 → 取该 provider 组内优先级最小的启用账号；找不到返回 None。
    ///
    /// `provider` 必须是两家之一（`cline-free` / `cline-pass`）—— 它是
    /// **定位记录的必要条件**，因为同一个账号可能在两个池各有一条记录。
    pub fn cline_account_record(&self, provider: &str, account_id: &str) -> Option<Value> {
        if !is_cline_family(provider) {
            return None;
        }
        let guard = self.guard();
        let state = self.load(&guard);
        if !account_id.is_empty() {
            return state
                .accounts
                .iter()
                .find(|item| item.id() == account_id && item.provider() == provider)
                .map(StoredAccount::to_value);
        }
        let mut candidates: Vec<StoredAccount> = state
            .accounts
            .iter()
            .filter(|item| item.provider() == provider && item.enabled())
            .cloned()
            .collect();
        candidates.sort_by_key(StoredAccount::order_key);
        candidates.into_iter().next().map(|item| item.to_value())
    }

    /// 取**任意一个池**的 Cline 账号记录（按 id 找；id 为空时取两个池里
    /// 优先级最小的启用账号）。
    ///
    /// ── 什么时候用它（而不是 `cline_account_record`）────────────
    /// 凭证层（续期、余额、维护任务）只拿到一个**账号 id**，不知道也不关心它
    /// 属于哪个池：池只影响发上游时的模型名前缀，凭证与续期两个池完全一样
    /// （见 `providers::cline::refresh`）。因此那条链走本函数。
    ///
    /// 而「按池定位账号」的调用方（登录落账号、按 provider 列账号）走
    /// `cline_account_record` —— 它必须带池，因为同一个 Cline 账号在两个池
    /// 各可能有一条记录，只给 id 无法区分。
    pub fn cline_any_account_record(&self, account_id: &str) -> Option<Value> {
        let guard = self.guard();
        let state = self.load(&guard);
        if !account_id.is_empty() {
            return state
                .accounts
                .iter()
                .find(|item| item.id() == account_id && is_cline_family(&item.provider()))
                .map(StoredAccount::to_value);
        }
        let mut candidates: Vec<StoredAccount> = state
            .accounts
            .iter()
            .filter(|item| is_cline_family(&item.provider()) && item.enabled())
            .cloned()
            .collect();
        candidates.sort_by_key(StoredAccount::order_key);
        candidates.into_iter().next().map(|item| item.to_value())
    }

    /// 这条记录是不是 Cline 系的某个池（适配器按 id 找人时用）。
    ///
    /// **不看池**：转发链路只拿到一个账号 id，想知道它属于本家，判据是
    /// 「它属于 Cline 系」再由 `accounts_for_provider` 的清单决定谁能承接；
    /// 池的归属在选路那一步已经由 provider 身份定好了。
    pub fn cline_account_provider(&self, account_id: &str) -> Option<String> {
        if account_id.is_empty() {
            return None;
        }
        let guard = self.guard();
        let state = self.load(&guard);
        state
            .accounts
            .iter()
            .find(|item| item.id() == account_id)
            .map(StoredAccount::provider)
            .filter(|provider| is_cline_family(provider))
    }

    /// 这条记录是不是**桌面端实时登录态**（凭证在 Cline 客户端自己的文件里）。
    ///
    /// 续期回写路径用它做早退（见 `cline::refresh::persist_refresh`）：桌面端
    /// 账号的凭证网关只读不写，否则会与客户端自己的续期互相顶掉。
    /// 判据读取记录上的 `desktop` 标记 + 属于 Cline 系 —— **不认 id 字面量**：
    /// 拆分后桌面端账号的 id 带池前缀（`cline-free-desktop` 这类），
    /// 写死任何一个都会漏判。
    pub fn cline_is_desktop_account(&self, account_id: &str) -> bool {
        if account_id.is_empty() {
            return false;
        }
        let guard = self.guard();
        let state = self.load(&guard);
        state
            .accounts
            .iter()
            .find(|item| item.id() == account_id)
            .map(|item| item.is_desktop() && is_cline_family(&item.provider()))
            .unwrap_or(false)
    }

    // ─── 写：手动添加 / 登录落账号 ────────────────────────────

    /// 添加/更新一个 Cline 账号（`POST /api/accounts` 的 cline 分支，
    /// 也是设备授权登录成功后的落账号入口）。
    ///
    /// `provider` 决定进哪个池那一家（`cline-free` / `cline-pass`），
    /// 目标是**那个池**；同一个账号可以两个池各加一份。
    ///
    /// 接受的 payload：
    ///   - `accessToken` / `access_token` / `token`（任一非空即可）；
    ///   - `refreshToken` / `refresh_token`（可选，没有则无法自动续期）；
    ///   - `expiresAt`（可选，缺失时从 JWT 的 `exp` 推）；
    ///   - `account` / `userId`（可选，展示用；缺失时从 JWT 的 email 推）；
    ///   - `name`（可选，备注名）。
    ///
    /// ── 校验口径（刻意薄）─────────────────────────────────────
    /// 只做「token 非空 + 长度合理」。为什么不校验 token 是不是合法 JWT：
    ///   - Cline 的 token 是 WorkOS 签发的，**离线验签做不到**（要 WorkOS 的公钥）；
    ///   - 形状检查（三段 JWT）会误伤 —— 上游将来换令牌格式时，形状检查
    ///     会把能用的凭证挡在门外，而真正的判定只有「打一次上游」才算数，
    ///     那件事由转发时的 401 如实反映。
    /// 与 AutoClaw 的「必须解出 user_id」不同：那个的 JWT 是自签的、声明是
    /// 必需的业务字段；这里没有这样的字段（account 只是展示用）。
    ///
    /// 撞 id（本机已有同 id 记录）时：同 provider 就地更新（保留优先级与启用
    /// 状态、沿用用户改过的备注名），撞到**别的 provider** 时报错。
    pub fn add_cline_account(
        &self,
        provider: &str,
        payload: &Value,
        name: Option<&str>,
    ) -> Result<Value, AccountStoreError> {
        if !is_cline_family(provider) {
            return Err(AccountStoreError::new(
                format!("{provider} 不是 Cline 系的提供商"),
                400,
            ));
        }
        let Some(object) = payload.as_object() else {
            return Err(AccountStoreError::new("上传内容必须是 JSON 对象", 400));
        };
        let access = pick(object, &["accessToken", "access_token", "token"]);
        if access.is_empty() {
            return Err(AccountStoreError::new(
                "缺少 token（accessToken / access_token）",
                400,
            ));
        }
        let refresh = pick(object, &["refreshToken", "refresh_token"]);
        if access.chars().count() > MAX_TOKEN_LENGTH || refresh.chars().count() > MAX_TOKEN_LENGTH {
            return Err(AccountStoreError::new("token 或 refreshToken 过长", 400));
        }
        // 规范化：补 workos: 前缀（幂等），算过期时间（显式值优先，其次 JWT 的 exp）
        let access = credentials::ensure_token_prefix(&access);
        let expires_at = object
            .get("expiresAt")
            .and_then(number)
            .or_else(|| credentials::expires_at_from_jwt(&access));
        // 展示名（姓名优先，见 `identity_from_jwt`）与**账号 id**（usr-…）分开：
        // 前者给人看，后者进 URL（`PATCH /api/accounts/<id>`）。
        let (display, jwt_account) = credentials::identity_from_jwt(&access);
        // 账号标识的来源顺序：payload 显式给的 → JWT 的 external_id（`usr-…`）
        // → 展示名。**优先 usr- 形态**是刻意的：它是 URL 安全的、也是上游
        // 余额接口认的那个标识，而 email 里的 `@` 虽然前端会 encode，但让 id
        // 带上它会让日志、导出文件与人工核对都更别扭。
        let account = {
            let given = pick(object, &["account", "userId", "accountId"]);
            if !given.is_empty() {
                given.to_string()
            } else if !jwt_account.is_empty() {
                jwt_account
            } else {
                display.clone()
            }
        };
        // id：优先用账号标识（同池重复添加时合并），拿不到就按 token 指纹
        // 生成一个稳定 id
        let id = {
            let key = if !account.is_empty() {
                account.clone()
            } else {
                format!("token-{}", fingerprint(&access))
            };
            record_id(provider, &key)
        };

        let _guard = self.guard();
        let mut state = self.load(&_guard);
        let existing = state.accounts.iter().find(|item| item.id() == id).cloned();
        if existing.is_none() && state.accounts.len() >= MAX_ACCOUNTS {
            return Err(AccountStoreError::new(
                format!("最多保存 {MAX_ACCOUNTS} 个账号"),
                400,
            ));
        }
        if let Some(existing) = existing.as_ref() {
            if existing.provider() != provider {
                return Err(AccountStoreError::new(
                    format!("账号 ID {id} 已被其它提供商的账号占用，请改用其它标识"),
                    409,
                ));
            }
        }
        let now = logging::now_ms();
        // 备注名的兜底链：调用方给的 → 记录里原有的 → **姓名** → account id
        // → token 尾号。姓名排在 account 前面：`usr-…` 那串 id 对人不友好，
        // 而姓名是这一家唯一「用户认得出」的东西（见 credentials 的说明）。
        let record_name = name
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .or_else(|| {
                existing
                    .as_ref()
                    .map(StoredAccount::name)
                    .filter(|value| !value.is_empty())
            })
            .unwrap_or_else(|| {
                if !display.is_empty() {
                    display.clone()
                } else if !account.is_empty() {
                    account.clone()
                } else {
                    format!("Cline {}", token_tail_of(&access))
                }
            });
        let priority = existing
            .as_ref()
            .map(StoredAccount::priority)
            .unwrap_or_else(|| {
                next_free_priority(
                    &state
                        .accounts
                        .iter()
                        .map(StoredAccount::priority)
                        .collect::<Vec<_>>(),
                )
            });
        let mut fields: Map<String, Value> = existing
            .as_ref()
            .map(|item| item.fields().clone())
            .unwrap_or_default();
        fields.insert("id".to_string(), Value::String(id));
        fields.insert("provider".to_string(), Value::String(provider.to_string()));
        fields.insert(
            "name".to_string(),
            Value::String(truncate_chars(&record_name, MAX_NAME_LENGTH)),
        );
        if !account.is_empty() {
            fields.insert(
                "account".to_string(),
                Value::String(truncate_chars(&account, MAX_IDENTITY_LENGTH)),
            );
        }
        // `displayName` 落盘：桌面端账号**不落 token**（凭证实时读客户端文件），
        // 而展示名要从 JWT / userInfo 里解 —— 不存下来的话，重新读账号文件时
        // 就没有任何地方能拿到姓名了（`credentials_from_record` 读的正是它）。
        if !display.is_empty() {
            fields.insert(
                "displayName".to_string(),
                Value::String(truncate_chars(&display, MAX_IDENTITY_LENGTH)),
            );
        }
        fields.insert("accessToken".to_string(), Value::String(access.clone()));
        if !refresh.is_empty() {
            fields.insert("refreshToken".to_string(), Value::String(refresh.clone()));
        }
        fields.insert(
            "tokenTail".to_string(),
            Value::String(token_tail_of(&access)),
        );
        if let Some(expires) = expires_at.filter(|value| *value > 0.0) {
            fields.insert(
                "expiresAt".to_string(),
                crate::server::core::account_store::state::json_number(expires),
            );
        }
        // 池由 provider 身份决定，记录里**不写** `pool`（旧记录里的那个字段
        // 由启动迁移读掉并删除，见模块头）
        fields.remove("pool");
        fields.insert("priority".to_string(), Value::from(priority));
        fields.insert(
            "enabled".to_string(),
            Value::Bool(existing.as_ref().map(StoredAccount::enabled).unwrap_or(true)),
        );
        fields.insert("desktop".to_string(), Value::Bool(false));
        fields.insert(
            "source".to_string(),
            Value::String(
                existing
                    .as_ref()
                    .map(|item| item.source())
                    .unwrap_or_else(|| "manual".to_string()),
            ),
        );
        fields.insert(
            "addedAt".to_string(),
            Value::from(
                existing
                    .as_ref()
                    .map(StoredAccount::added_at)
                    .filter(|value| *value != 0)
                    .unwrap_or(now),
            ),
        );
        fields.insert("updatedAt".to_string(), Value::from(now));
        // 清掉可能来自别家的残留字段（同一个 id 曾被别家占用过时）
        for key in ["edition", "endpoint", "prefixPath", "platform", "uid", "deviceId"] {
            fields.remove(key);
        }
        let saved = StoredAccount::from_map(fields);
        let is_new = existing.is_none();
        state.accounts.retain(|item| item.id() != saved.id());
        state.accounts.push(saved.clone());
        self.save(&state, &_guard)?;
        logging::log(
            "[Accounts]",
            &format!(
                "{} Cline 账号{}: {}（{}）",
                if is_new { "✅" } else { "🔄" },
                if is_new { "已添加" } else { "已更新" },
                saved.name(),
                crate::server::core::providers::label_of(provider),
            ),
        );
        Ok(self.to_cline_public_account(&saved))
    }

    /// 导入桌面端实时登录态（建一条 `desktop: true` 的账号记录，凭证不落盘）。
    ///
    /// `provider` 决定进哪个池那一家。**同一个 Cline 桌面登录态两个池各能导入
    /// 一份**：桌面登录态不属于任何池（池只是发上游时的通道选择器），所以
    /// 「我要用免费池」和「我要用订阅池」是两次独立的导入，各得一条记录。
    pub fn import_cline_desktop_account(
        &self,
        provider: &str,
        name: Option<&str>,
    ) -> Result<Value, AccountStoreError> {
        if !is_cline_family(provider) {
            return Err(AccountStoreError::new(
                format!("{provider} 不是 Cline 系的提供商"),
                400,
            ));
        }
        let credentials = credentials::read_desktop_credentials()
            .map_err(|error| AccountStoreError::new(error.message, error.status_code))?
            .ok_or_else(|| {
                AccountStoreError::new(
                    "本机没有找到 Cline 登录态（请先在 Cline 客户端登录）",
                    400,
                )
            })?;
        let id = credentials::desktop_account_id(provider);
        let _guard = self.guard();
        let mut state = self.load(&_guard);
        let existing = state.accounts.iter().find(|item| item.id() == id).cloned();
        if let Some(existing) = existing.as_ref() {
            if existing.provider() != provider {
                return Err(AccountStoreError::new(
                    format!("账号 ID {id} 已被其它提供商的账号占用"),
                    409,
                ));
            }
        }
        if existing.is_none() && state.accounts.len() >= MAX_ACCOUNTS {
            return Err(AccountStoreError::new(
                format!("最多保存 {MAX_ACCOUNTS} 个账号"),
                400,
            ));
        }
        let now = logging::now_ms();
        // 备注名兜底链：调用方给的 → 记录里原有的 → **姓名**（`credentials.name`
        // 已按「用户名优先」解析，见 credentials 模块）→ 账号标识。
        let record_name = name
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .or_else(|| {
                existing
                    .as_ref()
                    .map(StoredAccount::name)
                    .filter(|value| !value.is_empty())
            })
            .unwrap_or_else(|| {
                if !credentials.name.is_empty() {
                    credentials.name.clone()
                } else if !credentials.account.is_empty() {
                    credentials.account.clone()
                } else {
                    "Cline 桌面端登录态".to_string()
                }
            });
        let priority = existing
            .as_ref()
            .map(StoredAccount::priority)
            .unwrap_or_else(|| {
                next_free_priority(
                    &state
                        .accounts
                        .iter()
                        .map(StoredAccount::priority)
                        .collect::<Vec<_>>(),
                )
            });
        // 桌面端账号**不落 token**（实时读 providers.json）：只写展示需要的最小
        // 事实 + 一个用于界面显示的有效期（下次读文件会刷新它）
        let mut fields: Map<String, Value> = existing
            .as_ref()
            .map(|item| item.fields().clone())
            .unwrap_or_default();
        fields.insert("id".to_string(), Value::String(id.to_string()));
        fields.insert("provider".to_string(), Value::String(provider.to_string()));
        fields.insert(
            "name".to_string(),
            Value::String(truncate_chars(&record_name, MAX_NAME_LENGTH)),
        );
        if !credentials.account.is_empty() {
            fields.insert(
                "account".to_string(),
                Value::String(truncate_chars(&credentials.account, MAX_IDENTITY_LENGTH)),
            );
        }
        // 姓名落盘（同 `add_cline_account`）：桌面端账号不落 token，
        // 展示名是唯一能从记录里读出「这是谁」的字段
        if !credentials.name.is_empty() {
            fields.insert(
                "displayName".to_string(),
                Value::String(truncate_chars(&credentials.name, MAX_IDENTITY_LENGTH)),
            );
        }
        fields.insert(
            "tokenTail".to_string(),
            Value::String(token_tail_of(&credentials.access_token)),
        );
        if let Some(expires) = credentials.expires_at.filter(|value| *value > 0.0) {
            fields.insert(
                "expiresAt".to_string(),
                crate::server::core::account_store::state::json_number(expires),
            );
        }
        fields.insert(
            "hasRefreshToken".to_string(),
            Value::Bool(credentials.can_refresh()),
        );
        // 池由 provider 身份决定，记录里不写 `pool`（旧字段由启动迁移清掉）
        fields.remove("pool");
        fields.insert("priority".to_string(), Value::from(priority));
        fields.insert(
            "enabled".to_string(),
            Value::Bool(existing.as_ref().map(StoredAccount::enabled).unwrap_or(true)),
        );
        fields.insert("desktop".to_string(), Value::Bool(true));
        fields.insert("source".to_string(), Value::String("imported".to_string()));
        fields.insert(
            "addedAt".to_string(),
            Value::from(
                existing
                    .as_ref()
                    .map(StoredAccount::added_at)
                    .filter(|value| *value != 0)
                    .unwrap_or(now),
            ),
        );
        fields.insert("updatedAt".to_string(), Value::from(now));
        // 桌面端账号不落凭证（老记录里若有残留一并清掉）
        fields.remove("accessToken");
        fields.remove("refreshToken");
        let saved = StoredAccount::from_map(fields);
        let is_new = existing.is_none();
        state.accounts.retain(|item| item.id() != id);
        state.accounts.push(saved.clone());
        self.save(&state, &_guard)?;
        logging::log(
            "[Accounts]",
            &format!(
                "{} Cline 桌面端登录态{}: {record_name}",
                if is_new { "✅" } else { "🔄" },
                if is_new { "已导入" } else { "已刷新" },
            ),
        );
        Ok(self.to_cline_public_account(&saved))
    }

    /// Cline 账号续期成功后回写新 token（适配器的 `persist_refresh` 调用）。
    ///
    /// ── 比较-再写（与另外几家同一纪律）─────────────────────────
    /// 只当记录里**此刻的**凭证仍等于刷新前那份快照时才写入，否则返回
    /// [`CredentialWrite::Stale`]。比较与写入在同一把账号锁内完成。
    ///
    /// 为什么需要它：刷新是秒级的网络动作，期间用户可能重导入 / 换号，
    /// 或另一轮刷新先落地 —— 无条件覆盖会把**旧凭证**盖到新凭证上。
    ///
    /// 桌面端账号拒绝（它的凭证在客户端自己的文件里，见模块头）。
    pub fn update_cline_account_tokens_if_current(
        &self,
        id: &str,
        expected_access_token: &str,
        expected_refresh_token: &str,
        access_token: &str,
        refresh_token: &str,
        expires_at: Option<f64>,
    ) -> Result<CredentialWrite, String> {
        let _guard = self.guard();
        let mut state = self.load(&_guard);
        let Some(index) = state.accounts.iter().position(|item| item.id() == id) else {
            // 账号已被删除：结果无处可写，也不该新建记录
            return Ok(CredentialWrite::Stale);
        };
        if state.accounts[index].is_desktop() {
            return Err(
                "桌面端账号的凭证不落盘（实时读 Cline 的 providers.json），无需回写".to_string(),
            );
        }
        // 「是不是 Cline 账号」按**系**判（两个池都算），不认具体的池 ——
        // 回写只关心「这条记录的凭证格式归不归我管」，池在写入时原样保留
        if !is_cline_family(&state.accounts[index].provider()) {
            return Err(format!("账号 {id} 不是 Cline 账号"));
        }
        if state.accounts[index].access_token() != expected_access_token
            || state.accounts[index].refresh_token() != expected_refresh_token
        {
            return Ok(CredentialWrite::Stale);
        }
        if !access_token.is_empty() {
            state.accounts[index].set("accessToken", Value::String(access_token.to_string()));
            state.accounts[index].set("tokenTail", Value::String(token_tail_of(access_token)));
        }
        if !refresh_token.is_empty() {
            state.accounts[index].set("refreshToken", Value::String(refresh_token.to_string()));
        }
        if let Some(value) = expires_at.filter(|value| *value > 0.0) {
            state.accounts[index].set(
                "expiresAt",
                crate::server::core::account_store::state::json_number(value),
            );
        }
        state.accounts[index].set_updated_at(logging::now_ms());
        self.save(&state, &_guard).map_err(|error| error.message)?;
        Ok(CredentialWrite::Written)
    }

    /// Cline 账号的公开形态（**不含 token**）。
    ///
    /// 字段与另外几家同构。**没有 `pool` 字段**（拆分前有）：池已经由
    /// `provider` 表达 —— 前端按 provider id 分组显示，本来就知道这条属于
    /// 哪个池，再回一个 `pool` 只会变成第二处事实（两处不一致时界面信哪个？）。
    ///
    /// ── 账号名为什么可能是「推导」出来的 ─────────────────────────
    /// 早先的版本把账号名直接写成 `usr-…` 那串 id（用户完全认不出是谁的号）。
    /// 现在**在读取时补一次推导**，于是老记录不必重新添加就能显示成人名：
    ///   - `displayName`：记录里存的 → 从 JWT 现解（姓名 → email → 账号 id）；
    ///   - 公开形态的 `name`：记录名与 `account` **逐字相同**（即当初是自动
    ///     生成的、用户没改过）时，改用 `displayName`；用户改过备注名就尊重它。
    /// 只影响展示，不改盘 —— 写入仍只发生在添加 / 续期那两条既有路径上。
    pub fn to_cline_public_account(&self, record: &StoredAccount) -> Value {
        let value = record.to_value();
        let mut out = Map::new();
        out.insert("id".to_string(), Value::String(record.id().to_string()));
        // provider 取**记录自己的**（不是某个写死的值）：两个池共用这一份
        // 公开形态，回错池会让账号显示在另一个分组里
        out.insert(
            "provider".to_string(),
            Value::String(record.provider()),
        );
        let account = value
            .get("account")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        // 姓名：存的优先，没有就从 token 现解（老记录 / 手填 token 的账号）
        let display = {
            let stored = value
                .get("displayName")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            if !stored.is_empty() {
                stored
            } else {
                value
                    .get("accessToken")
                    .and_then(Value::as_str)
                    .map(|token| credentials::identity_from_jwt(token).0)
                    .unwrap_or_default()
            }
        };
        // 账号名：自动生成的那个（与 account 同字）换成姓名
        let stored_name = record.name();
        let name = if !display.is_empty()
            && (stored_name.trim().is_empty() || stored_name.trim() == account)
        {
            display.clone()
        } else {
            stored_name
        };
        out.insert("name".to_string(), Value::String(name));
        if !account.is_empty() {
            out.insert("account".to_string(), Value::String(account));
        }
        // 姓名单独一个字段：界面拿它做展示（账号名之外的第二标识），
        // 也让「备注名被用户改过」时仍能看到这到底是谁的账号
        if !display.is_empty() {
            out.insert("displayName".to_string(), Value::String(display));
        }
        out.insert("tokenTail".to_string(), Value::String(token_tail(&value)));
        out.insert(
            "hasRefreshToken".to_string(),
            Value::Bool(
                value
                    .get("refreshToken")
                    .and_then(Value::as_str)
                    .map(|text| !text.trim().is_empty())
                    .unwrap_or_else(|| {
                        // 桌面端账号不落 refreshToken（凭证在客户端文件里），
                        // 用导入时记下的标记兜底
                        value
                            .get("hasRefreshToken")
                            .and_then(Value::as_bool)
                            .unwrap_or(false)
                    }),
            ),
        );
        out.insert("priority".to_string(), Value::from(record.priority()));
        out.insert("enabled".to_string(), Value::Bool(record.enabled()));
        out.insert("desktop".to_string(), Value::Bool(record.is_desktop()));
        out.insert("source".to_string(), Value::String(record.source()));
        out.insert("addedAt".to_string(), Value::from(record.added_at()));
        out.insert("updatedAt".to_string(), Value::from(record.updated_at()));
        out.insert(
            "available".to_string(),
            // 用 `has_credentials()` 而不是 `has_token()`：桌面端账号按设计不落
            // token（凭证实时读 providers.json），只认 has_token 会把它标成不可用
            Value::Bool(record.enabled() && record.has_credentials()),
        );
        if let Some(expires) = value.get("expiresAt").filter(|value| !value.is_null()) {
            out.insert("expiresAt".to_string(), expires.clone());
        }
        Value::Object(out)
    }
}

/// 取候选键里第一个非空字符串（去空白）
fn pick(object: &Map<String, Value>, keys: &[&str]) -> String {
    for key in keys {
        if let Some(text) = object.get(*key).and_then(Value::as_str) {
            let text = text.trim();
            if !text.is_empty() {
                return text.to_string();
            }
        }
    }
    String::new()
}

/// 数字取值（容忍字符串形态）
fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse::<f64>().ok().filter(|v| v.is_finite()),
        _ => None,
    }
}

/// 记录里的 `tokenTail`（公开形态照搬；缺失时给空串）
fn token_tail(value: &Value) -> String {
    value
        .get("tokenTail")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// token 的短指纹（生成稳定 id 用）。
///
/// 用 FNV-1a 而不是 crypto 哈希：这里只要「同一 token 得到同一个 id」，
/// 不需要抗碰撞（id 还会先被 `truncate_chars` 截断，且撞了也只是两条记录
/// 合并成一条的下场，不是安全问题）。不引 sha2 是为了与另外几家的用量
/// 对齐 —— 那几家用 sha2 是因为它们的 id 要进 URL 或要抗用户猜测。
fn fingerprint(token: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in token.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}
