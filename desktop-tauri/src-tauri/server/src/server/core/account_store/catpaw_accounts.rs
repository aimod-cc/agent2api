//! CatPaw 账号：手动添加、桌面端实时账号、公开形态（Agent2API 二期 W5-T-d4）。
//! 旧数据一次性导入在 `catpaw_import.rs`（同样是账号存储的方法，只是拆了文件）。
//!
//! ── 与 workbuddy / 小浣熊添加路径的关系 ──────────────────────
//! 三家各有一条添加路径，因为凭证形态完全不同：
//!   - workbuddy：`user-<uid>` + accessToken/refreshToken + edition/endpoint；
//!   - 小浣熊：JWT + refreshToken（可续期）；
//!   - CatPaw：**Cookie 形态**的 `X-Passport-Token` + 独立的 `uid`，且**没有
//!     刷新机制**（§9.1：token 过期只能在桌面端重新登录）。
//! 本文件服务第三条路径（`api::accounts::add_account` 的 catpaw 分支）。
//!
//! ── 记录里的字段（对照原项目 `account-store.mjs`，**唯一事实来源**）──
//! ```text
//!   原项目 accounts[i]：{ id, name, loginName, uid, tokenTail, accessToken,
//!                        addedAt, updatedAt }
//!   原项目 DESKTOP_ACCOUNT_ID = 'desktop-auth'（列表第一项，不可删除）
//!   原项目账号文件：~/.meituan-catpaw/catpaw-proxy-accounts.json
//!   原项目桌面端登录态：~/.meituan-catpaw/auth.json（auth.accessToken /
//!                      account.uid / account.loginName）
//! ```
//! 本项目的记录在原字段基础上补 `provider` / `priority` / `enabled` / `source`
//! / `desktop` / `proxy`（与另外两家共用账号存储的通用设施：优先级号段、启用
//! 开关、出网代理、限额冷却）。
//!
//! ── balanceCookies（原项目的余额查询凭证）**保留但不消费** ─────
//! 原项目账号文件顶层有 `balanceCookies: { [id]: { token2, token2Tail, uid,
//! savedAt } }`，服务「查询余额」功能；本项目**明确不迁移余额**（§9.4 的同类
//! 判断：网关不消费它）。但导入时**不丢**这条数据：逐账号落到记录的
//! `balanceCookie` 字段上（键名从复数改成单数，因为它现在是**这一条账号**的
//! 属性），用户将来想接回余额功能时有原始数据可用。本文件的任何取值路径都不读它。
//!
//! ── 硬约束 ────────────────────────────────────────────────
//! 本文件全是「读-改-写」文件操作，**没有任何网络请求**（持锁不做网络）。
//! 绝不 unwrap/expect（release 是 panic=abort）。

use serde_json::{Map, Value};

use crate::server::core::account_store::priority::next_free_priority;
use crate::server::core::account_store::sql;
use crate::server::core::account_store::state::StoredAccount;
use crate::server::core::account_store::store::{AccountStore, AccountStoreError};
use crate::server::core::account_store::store_util::{
    js_string, max_concurrent_public, object_or_empty, strip_bearer_prefix, token_tail_of,
    truncate_chars,
};
use crate::server::core::account_store::MAX_TOKEN_LENGTH;
/// 余额凭证字段名（定义在 `providers::catpaw::balance`：那里是消费方，
/// 字段名只该有一份，账号层引用它而不是另写一个字符串字面量）
use crate::server::core::providers::catpaw::balance::BALANCE_TOKEN_FIELD;
use crate::server::core::providers::catpaw::credentials;
use crate::server::core::providers::{kind_id, ProviderKind};
use crate::server::logging;

/// CatPaw 的 provider id（`providers::kind_id` 的常量形态）
fn catpaw_id() -> &'static str {
    kind_id(ProviderKind::CatPaw)
}

/// 备注名长度上限（原项目 `name.trim().slice(0, 100)`）
const MAX_NAME_LENGTH: usize = 100;

/// uid / loginName 的长度上限（原项目 `MAX_USER_UID_LENGTH = 256`）
const MAX_USER_UID_LENGTH: usize = 256;

impl AccountStore {
    // ─── 读：账号记录 ────────────────────────────────────────

    /// 取 CatPaw 账号的**原始记录**（含 accessToken）。
    ///
    /// `account_id` 为空 → 取 CatPaw 组内优先级最小的启用账号（与转发选路同一
    /// 判据）；找不到返回 None（此时由调用方回落到桌面端实时登录态 / 环境变量）。
    ///
    /// 为什么返回原始 JSON 而不是公开形态：适配器需要 accessToken 与 uid ——
    /// 公开形态按设计只有 `tokenTail`（见 `to_catpaw_public_account`）。
    pub fn catpaw_account_record(&self, account_id: &str) -> Option<Value> {
        let _guard = self.guard();
        let catpaw = catpaw_id();
        if !account_id.is_empty() {
            // 按 id 直查一行（不读别家、也不读其余 CatPaw 账号）
            let record = self.record_by_id(&_guard, account_id)?;
            return (record.provider() == catpaw).then(|| record.to_value());
        }
        let mut candidates: Vec<StoredAccount> = self
            .records_for_provider(&_guard, catpaw)
            .into_iter()
            .filter(StoredAccount::enabled)
            .collect();
        candidates.sort_by_key(StoredAccount::order_key);
        candidates.into_iter().next().map(|item| item.to_value())
    }

    // ─── 写：手动添加 ────────────────────────────────────────

    /// 添加/更新一个 CatPaw 账号（`POST /api/accounts` 的 catpaw 分支）。
    ///
    /// 接受三种 payload 形态（字段名对照原项目 `account-store.mjs` 与
    /// `catpaw-local-auth.mjs`，**不做猜测性的字段扩张**）：
    ///   1. 扁平形态：`{ token | accessToken | access_token, uid | userId,
    ///      loginName?, name? }`；
    ///   2. **粘贴原项目的账号记录**（`accounts[i]` 整条）：除了上面的字段，
    ///      还带 `id` / `tokenTail` / `addedAt` / `updatedAt` —— 少给 uid 时
    ///      用 `id` 兜底（原项目的 id 就是 uid 或 loginName）；
    ///   3. **粘贴桌面端 auth.json 内容**：`{ auth: { loginType, accessToken },
    ///      account: { uid, loginName } }`。
    ///
    /// 校验（照抄原项目 `safeString` 的口径）：
    ///   - token 非空、≤ `MAX_TOKEN_LENGTH`、**不含 `[\r\n;]`**
    ///     （这些值最终进 Cookie / user-uid 头，换行是头注入、分号会截断 Cookie）；
    ///   - `auth.loginType` 非空且不是 `passport` → 拒绝（不支持的登录方式）；
    ///   - uid / loginName 至少有一个（否则这条账号无法标识，也拼不出 user-uid）；
    ///   - uid / loginName ≤ 256、备注名 ≤ 100（原项目同值）。
    ///
    /// id 的生成沿原项目规则：`uid || loginName`（**不加 `user-` 前缀** ——
    /// 那是 workbuddy 的 id 形态，两家 id 空间独立且原项目的账号就是按 uid
    /// 寻址的）。撞到**别的 provider** 的 id 时报错，不覆写他人记录。
    pub fn add_catpaw_account(
        &self,
        payload: &Value,
        name: Option<&str>,
    ) -> Result<Value, AccountStoreError> {
        let Some(object) = payload.as_object() else {
            return Err(AccountStoreError::new("上传内容必须是 JSON 对象", 400));
        };
        let auth = object_or_empty(object.get("auth"));
        let account = object_or_empty(object.get("account"));
        // 登录方式：auth.json 形态下必查（原项目同）；扁平形态通常没有这个字段
        let login_type = auth
            .get("loginType")
            .or_else(|| object.get("loginType"))
            .map(js_string)
            .unwrap_or_default()
            .trim()
            .to_lowercase();
        if !login_type.is_empty() && login_type != "passport" {
            return Err(AccountStoreError::new(
                format!("暂不支持 CatPaw 登录方式 {login_type}（仅支持 passport）"),
                400,
            ));
        }
        let token = pick_first_text(
            &[
                object.get("token"),
                object.get("accessToken"),
                object.get("access_token"),
                object.get("auth_token"),
                auth.get("accessToken"),
                auth.get("token"),
                auth.get("access_token"),
            ],
            &["token", "accessToken", "access_token"],
        )?;
        let uid_raw = pick_first_optional(&[
            object.get("uid"),
            object.get("userId"),
            account.get("uid"),
            account.get("userId"),
            // 原项目的账号记录：id 就是 uid 或 loginName（uid 缺失时才用它兜底）
            object.get("id"),
        ]);
        let login_name = pick_first_optional(&[
            object.get("loginName"),
            account.get("loginName"),
        ]);
        let user_key = if !uid_raw.trim().is_empty() {
            uid_raw.trim().to_string()
        } else {
            login_name.trim().to_string()
        };
        if user_key.is_empty() {
            return Err(AccountStoreError::new(
                "CatPaw 登录态缺少 uid 或 loginName（无法标识账号）",
                400,
            ));
        }
        let user_key = safe_field(user_key, "uid", MAX_USER_UID_LENGTH)?;
        let uid = if uid_raw.trim().is_empty() {
            String::new()
        } else {
            safe_field(uid_raw, "uid", MAX_USER_UID_LENGTH)?
        };
        let login_name = if login_name.trim().is_empty() {
            String::new()
        } else {
            safe_field(login_name, "loginName", MAX_USER_UID_LENGTH)?
        };

        let _guard = self.guard();
        let id = user_key;
        // 只读这一行（不再读全量）：既有记录决定「更新还是新建」与多处沿用值
        let existing = self.record_by_id(&_guard, &id);
        // 撞 id 保护：id 空间与其他 provider 独立，但同 id 会被整条覆写 ——
        // 撞到别家时报错而不是改写（那会连凭证一起丢），与另外两家同一策略
        if let Some(existing) = existing.as_ref() {
            let existing_provider = existing.provider();
            if existing_provider != catpaw_id() {
                return Err(AccountStoreError::new(
                    format!(
                        "账号 id「{id}」已被{existing_provider}账号占用，无法添加同一标识的\
                         CatPaw 账号（请先处理那个账号）"
                    ),
                    400,
                ));
            }
        }
        let explicit_name = name
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| truncate_chars(value, MAX_NAME_LENGTH));
        // 备注名兜底照抄原项目：显式传入 → 记录里的 name → loginName → uid
        let record_name = explicit_name
            .or_else(|| {
                existing
                    .as_ref()
                    .map(StoredAccount::name)
                    .filter(|value| !value.is_empty())
            })
            .or_else(|| {
                pick_first_optional(&[object.get("name"), account.get("name")])
                    .trim()
                    .to_string()
                    .into_non_empty()
            })
            .or_else(|| login_name.clone().into_non_empty())
            .unwrap_or_else(|| format!("账号 {id}"));
        let priority = match existing.as_ref() {
            Some(record) => record.priority(),
            None => {
                // 号段取**全部**账号：优先级全局唯一（四家共用一条队列），
                // 只看本家会让新账号撞上别家已在用的号（见 priority.rs 模块头）。
                // 改造后直接在投影列上取一列数值，不解析任何记录的 JSON。
                let used = self.with_conn(&_guard, |conn| sql::priorities_all(conn))?;
                next_free_priority(&used)
            }
        };
        let now = logging::now_ms();
        let mut record = Map::new();
        record.insert("id".to_string(), Value::String(id.clone()));
        record.insert("provider".to_string(), Value::String(catpaw_id().to_string()));
        record.insert("name".to_string(), Value::String(record_name.clone()));
        record.insert("uid".to_string(), Value::String(uid));
        record.insert("loginName".to_string(), Value::String(login_name));
        record.insert("accessToken".to_string(), Value::String(token.clone()));
        record.insert("tokenTail".to_string(), Value::String(token_tail_of(&token)));
        // 余额查询凭证（`balanceToken`，可选）：网页会话凭证 token2，与转发用的
        // accessToken **不是同一个东西**（见 `providers/catpaw/balance.rs` 模块头）。
        // 手动添加时给两处来源：payload 里的 `balanceToken` / `token2`（用户直接填），
        // 以及粘贴原项目账号记录时同一层的 `balanceCookie.token2`（旧数据的形态）。
        // 取不到就不出键 —— 「未配置」是有意义的状态（前端据此提示去配置），
        // 写一个空串会让「配置了但填错」与「没配置」不可区分。
        if let Some(balance_token) = pick_balance_token(object, &auth) {
            record.insert(
                BALANCE_TOKEN_FIELD.to_string(),
                Value::String(balance_token),
            );
        }
        // 手动添加一律 source=manual（导入路径才写 imported / desktop）
        record.insert("source".to_string(), Value::String("manual".to_string()));
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
                    // 原项目的账号记录带 addedAt：粘贴整条记录时沿用它的时间线
                    .or_else(|| {
                        object
                            .get("addedAt")
                            .and_then(Value::as_i64)
                            .filter(|value| *value != 0)
                    })
                    .unwrap_or(now),
            ),
        );
        record.insert("updatedAt".to_string(), Value::from(now));
        // 未知字段全量保留（用户手工加过的字段、原项目账号记录里的 balanceCookie
        // 之类的附加数据都不能因为一次「更新账号」丢掉）
        let mut merged = record;
        if let Some(existing) = existing.as_ref() {
            for (key, value) in existing.fields() {
                merged.entry(key.clone()).or_insert_with(|| value.clone());
            }
        }
        let saved = StoredAccount::from_map(merged);
        // 单行落地：`put` = DELETE + INSERT，于是「更新既有记录时它在列表里
        // 往后挪」的旧行为（retain + push）保持不变（见 `sql::put`）
        self.with_conn(&_guard, |conn| sql::put(conn, &saved))?;
        // 更新既有记录 = **换了（或刷新了）登录态**：注册表里属于它的
        // conversationId 建立在上一次凭证的账号上下文里，必须作废
        // （见 `invalidate_catpaw_sessions`）。新建时注册表里本来就没有它，无害。
        let replaced = existing.is_some();
        drop(_guard);
        if replaced {
            self.invalidate_catpaw_sessions(&id, catpaw_id());
        }
        logging::log(
            "[Accounts]",
            &format!("✅ CatPaw 账号已保存: {record_name}（{id}，优先级 {priority}）"),
        );
        Ok(self.to_catpaw_public_account(&saved))
    }

    /// 导入/刷新「桌面端实时登录态」账号（`importDesktop: true` 与启动导入共用）。
    ///
    /// 语义（对照原项目 `account-store.mjs` 的 `desktopEntry()`）：id 固定
    /// [`credentials::DESKTOP_ACCOUNT_ID`]（`desktop-auth`）、`desktop: true`、
    /// **账号记录里不落 token**（凭证每次实时读 `~/.meituan-catpaw/auth.json`）。
    /// 已存在时**幂等**：只更新展示字段（uid / loginName / tokenTail / mtime），
    /// 保留优先级、启用状态与用户改过的备注名。
    ///
    /// 读不到登录态时报 400 并把原因说清楚（原项目 `LocalAuthError` 的文案：
    /// 「请先在 CatPaw 桌面端登录」）—— 这是用户点按钮时的即时反馈，
    /// 静默建一条空记录只会让人以为成功了。
    pub fn import_catpaw_desktop_account(
        &self,
        source: &str,
    ) -> Result<Value, AccountStoreError> {
        let summary =
            credentials::desktop_summary().map_err(|reason| AccountStoreError::new(reason, 400))?;
        let uid = summary
            .get("uid")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let login_name = summary
            .get("loginName")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let _guard = self.guard();
        let id = credentials::DESKTOP_ACCOUNT_ID.to_string();
        let existing = self.record_by_id(&_guard, &id);
        if let Some(existing) = existing.as_ref() {
            let existing_provider = existing.provider();
            if existing_provider != catpaw_id() {
                return Err(AccountStoreError::new(
                    format!(
                        "账号 id「{id}」已被{existing_provider}账号占用，无法导入 CatPaw 桌面端登录态"
                    ),
                    400,
                ));
            }
        }
        let default_name = if login_name.is_empty() {
            if uid.is_empty() {
                "桌面端登录账号".to_string()
            } else {
                format!("桌面端登录账号（{uid}）")
            }
        } else {
            format!("桌面端登录账号（{login_name}）")
        };
        let record_name = existing
            .as_ref()
            .map(StoredAccount::name)
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| default_name.clone());
        let priority = match existing.as_ref() {
            Some(record) => record.priority(),
            None => {
                // 号段取**全部**账号：优先级全局唯一（四家共用一条队列），
                // 只看本家会让新账号撞上别家已在用的号（见 priority.rs 模块头）。
                // 改造后直接在投影列上取一列数值，不解析任何记录的 JSON。
                let used = self.with_conn(&_guard, |conn| sql::priorities_all(conn))?;
                next_free_priority(&used)
            }
        };
        let now = logging::now_ms();
        let mut record = Map::new();
        record.insert("id".to_string(), Value::String(id.clone()));
        record.insert("provider".to_string(), Value::String(catpaw_id().to_string()));
        record.insert("name".to_string(), Value::String(record_name.clone()));
        record.insert("uid".to_string(), Value::String(uid));
        record.insert("loginName".to_string(), Value::String(login_name));
        record.insert("desktop".to_string(), Value::Bool(true));
        record.insert("source".to_string(), Value::String(source.to_string()));
        for (target, key) in [("tokenTail", "tokenTail"), ("desktopMtime", "modifiedAt")] {
            record.insert(
                target.to_string(),
                summary.get(key).cloned().unwrap_or(Value::Null),
            );
        }
        record.insert("priority".to_string(), Value::from(priority));
        record.insert(
            "enabled".to_string(),
            Value::Bool(existing.as_ref().map(StoredAccount::enabled).unwrap_or(true)),
        );
        let is_new = existing.is_none();
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
        let mut merged = record;
        if let Some(existing) = existing.as_ref() {
            for (key, value) in existing.fields() {
                // 桌面端账号的凭证**不落盘**（实时读 auth.json）：
                // 老记录里若有 token 残留，这里跳过（与另外两家同一纪律）
                if key == "accessToken" || key == "refreshToken" {
                    continue;
                }
                merged.entry(key.clone()).or_insert_with(|| value.clone());
            }
        }
        let saved = StoredAccount::from_map(merged);
        // 单行落地：`put` = DELETE + INSERT，于是「更新既有记录时它在列表里
        // 往后挪」的旧行为（retain + push）保持不变（见 `sql::put`）
        self.with_conn(&_guard, |conn| sql::put(conn, &saved))?;
        // 桌面端重新导入 = 用户可能在桌面端换了账号：实时登录态变了，
        // 旧 conversationId 属于上一个登录态的上游上下文，作废（见上一条注释）
        let replaced = existing.is_some();
        drop(_guard);
        if replaced {
            self.invalidate_catpaw_sessions(&id, catpaw_id());
        }
        logging::log(
            "[Accounts]",
            &format!(
                "{} CatPaw 桌面端登录态{}: {record_name}",
                if is_new { "✅" } else { "🔄" },
                if is_new { "已导入" } else { "已刷新" },
            ),
        );
        Ok(self.to_catpaw_public_account(&saved))
    }

    // ─── 会话失效（账号切换语义，架构文档 §9.3）──────────────────

    /// 作废某账号在 CatPaw **会话注册表**里的全部记录。
    ///
    /// ── 为什么必须做（与事件流的关系）────────────────────────────
    /// `conversationId` 是**上游账号上下文里**的对象：换账号之后旧 id 要么不存在、
    /// 要么属于另一个用户，续接必然失败（轻则报错，重则把两个账号的会话搅在一起）。
    /// 原项目在账号切换时整表作废（`account-routes.mjs` 的 `notifySwitch` →
    /// `clearClientToolSessions()`），这里是多账号版：**只作废属于该账号的记录**
    /// （`SessionRegistry::clear_account`），其他账号的会话不受影响。
    ///
    /// ── 什么时候调（三个入口，全部是「凭证身份变了」的场景）──────
    ///   1. 账号被**删除**（`remove_account` / `batch_remove`）；
    ///   2. 账号被**禁用**（`update_account` / `batch_update` 的 `enabled: false`）；
    ///   3. 账号凭证被**重新导入**（`add_catpaw_account` 更新既有记录、
    ///      桌面端账号重新导入）—— 同 id 换了别的登录态时，注册表按 account_id
    ///      根本看不出身份变了，必须由导入方显式作废。
    ///
    /// ── 为什么 `provider` 要由调用方传 ──────────────────────────
    /// 删除路径上记录**已经不在文件里**了（本函数在落盘之后才调），
    /// 此刻再回读只能得到 None。因此调用方在改动之前取好 provider；
    /// 非 CatPaw 账号直接返回 0（不做无谓的注册表扫描）——
    /// 注册表里只有 CatPaw 的会话，这条判定只是省一次加锁。
    ///
    /// 返回被作废的记录条数（供日志）。
    pub(crate) fn invalidate_catpaw_sessions(&self, account_id: &str, provider: &str) -> usize {
        if account_id.is_empty() || provider != catpaw_id() {
            return 0;
        }
        let count =
            crate::server::core::providers::catpaw::conversation::session_registry()
                .clear_account(account_id);
        if count > 0 {
            logging::verbose(
                "[Accounts]",
                &format!("CatPaw 账号 {account_id} 的会话映射已作废（{count} 条）"),
            );
        }
        count
    }

    // ─── 公开形态（CatPaw）──────────────────────────────────

    /// CatPaw 账号的公开形态（架构文档 §5；对照原项目 `toPublicAccount`）。
    ///
    /// 字段：`id` / `provider` / `name` / `uid` / `loginName` / `tokenTail` /
    /// `desktop` / `source` / `priority` / `enabled` / `addedAt` / `updatedAt` /
    /// `proxy` / `rateLimits` / `available`。
    ///
    /// **没有积分/签到字段**（原项目那套余额功能明确不迁移）；`hasBalanceCookie`
    /// 之类的展示位也不给 —— 前端按 provider 分支渲染时不会去读。
    /// workbuddy / 小浣熊账号的公开形态仍走各自的分支（字段逐字不变）。
    ///
    /// ── 桌面端账号按**实时值**展示（与原项目 `desktopEntry()` 同）───
    /// 这类账号记录里按设计没有 token，`tokenTail` 若只读记录会永远是空的，
    /// 界面看起来就是「没有凭证」。因此有桌面端登录态时优先用 auth.json 的
    /// 实时值；读不到（客户端退出登录、文件被删）时回落记录里的存量值 ——
    /// 于是「登录态消失」表现为字段变空，而不是整条记录消失。
    /// `available` 也据此判定：桌面端账号读不到登录态时如实报 false
    /// （原项目同样把 `available: false` 与 `reason` 一起给出）。
    pub fn to_catpaw_public_account(&self, record: &StoredAccount) -> Value {
        let proxy = crate::server::core::proxies::describe_account_proxy(Some(&record.proxy()));
        let stored_tail = record
            .get("tokenTail")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let mut available = true;
        let mut reason = String::new();
        let mut token_tail = stored_tail;
        let mut uid = record.get("uid").and_then(Value::as_str).unwrap_or("").to_string();
        let mut login_name = record
            .get("loginName")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if record.is_desktop() && record.provider() == catpaw_id() {
            match credentials::read_desktop_login() {
                Ok(login) => {
                    token_tail = login.token_tail();
                    if !login.uid.is_empty() {
                        uid = login.uid.clone();
                    }
                    if !login.login_name.is_empty() {
                        login_name = login.login_name.clone();
                    }
                }
                Err(reason_text) => {
                    // 读不到实时登录态：字段回落记录里的存量值，可用性如实报 false
                    available = false;
                    reason = reason_text;
                }
            }
        }
        let mut public = Map::new();
        public.insert("id".to_string(), Value::String(record.id().to_string()));
        public.insert("provider".to_string(), Value::String(record.provider()));
        public.insert("name".to_string(), Value::String(record.name()));
        public.insert("uid".to_string(), Value::String(uid));
        public.insert("loginName".to_string(), Value::String(login_name));
        public.insert("tokenTail".to_string(), Value::String(token_tail));
        public.insert("desktop".to_string(), Value::Bool(record.is_desktop()));
        public.insert("source".to_string(), Value::String(record.source()));
        public.insert("priority".to_string(), Value::from(record.priority()));
        public.insert("enabled".to_string(), Value::Bool(record.enabled()));
        public.insert("addedAt".to_string(), Value::from(record.added_at()));
        public.insert("updatedAt".to_string(), Value::from(record.updated_at()));
        public.insert("proxy".to_string(), proxy);
        // 限额冷却标记：选路层从公开形态读它（与另外两家同口径）。
        // CatPaw 当前不产生限额标记（原项目没有多账号轮换，见 catpaw/adapter.rs），
        // 但字段照样透出 —— 少一个键会让通用代码在这条分支上多一次特判
        public.insert(
            "rateLimits".to_string(),
            record.get("rateLimits").cloned().unwrap_or_else(|| Value::Object(Map::new())),
        );
        public.insert("available".to_string(), Value::Bool(available));
        if !reason.is_empty() {
            public.insert("reason".to_string(), Value::String(reason));
        }
        // 余额查询凭证**只透出「配没配」**，绝不透出值本身（凭证不进前端：
        // 账号列表是会被截图/贴出来排障的界面，而这一项是完整的会话凭证）。
        // 前端据它决定账号设置弹窗里那个输入框的占位文案。
        public.insert(
            "hasBalanceToken".to_string(),
            Value::Bool(
                record
                    .get(BALANCE_TOKEN_FIELD)
                    .and_then(Value::as_str)
                    .map(|value| !value.trim().is_empty())
                    .unwrap_or(false),
            ),
        );
        // 单账号并发上限（所有家通用，兜底共用 `max_concurrent_public`）：
        // 0 = 不限，缺键同样输出 0
        public.insert(
            "maxConcurrent".to_string(),
            Value::from(max_concurrent_public(record.get("maxConcurrent"))),
        );
        Value::Object(public)
    }

    // ─── 余额查询凭证（balanceToken）────────────────────────────

    /// 设置 / 清除某 CatPaw 账号的**余额查询凭证**（`balanceToken`）。
    ///
    /// ── 为什么单独一个方法，而不是加进通用的 `apply_patch` ─────────
    /// 通用 patch 处理的是四家**共有**的运营属性（备注名 / 优先级 / 启用 / 代理），
    /// 而 `balanceToken` 是 CatPaw 独有的余额查询凭证（别的家的余额凭证来自账号
    /// 本身的登录态）。把它塞进 `apply_patch` 只会让那个四家共用的函数多一个
    /// provider 分支，而它唯一的作用是给 CatPaw 存一个字符串。校验（长度上限、
    /// 禁 CR/LF/分号）与 `accessToken` 同源 —— 这个值最终会拼进 `Cookie` 头。
    ///
    /// `value` 语义：字符串（去空白后非空）= 设置；`null` / 空串 = **清除**。
    /// 返回变化描述（与 `update_account` 的 changes 同形）。
    ///
    /// 404 同时覆盖「账号不存在」与「账号不是 CatPaw」（判据是 `id + provider`
    /// 的组合），接口层按「是不是 CatPaw 账号」给更准的提示。
    pub fn update_catpaw_balance_token(
        &self,
        id: &str,
        value: &Value,
    ) -> Result<Vec<String>, AccountStoreError> {
        let _guard = self.guard();
        // 只读目标那一行 + 一次归属判定（同一个 id 只可能有一条记录）
        let mut record = self
            .record_by_id(&_guard, id)
            .filter(|record| record.provider() == catpaw_id())
            .ok_or_else(|| AccountStoreError::not_found("账号不存在或不属于 CatPaw"))?;
        let next: Option<String> = match value {
            Value::Null => None,
            other => {
                let text = js_string(other).trim().to_string();
                if text.is_empty() {
                    None
                } else {
                    Some(safe_field(
                        strip_bearer_prefix(&text).to_string(),
                        "balanceToken",
                        MAX_TOKEN_LENGTH,
                    )?)
                }
            }
        };
        let current = record
            .get(BALANCE_TOKEN_FIELD)
            .and_then(Value::as_str)
            .map(str::to_string);
        if current == next {
            return Ok(Vec::new());
        }
        let mut changes = Vec::new();
        match &next {
            Some(token) => {
                record.set(BALANCE_TOKEN_FIELD, Value::String(token.clone()));
                // 日志只说「已更新」，**不打印凭证本身**（与全仓的 token 处理一致）
                changes.push("余额查询凭证已更新".to_string());
            }
            None => {
                record.remove(BALANCE_TOKEN_FIELD);
                changes.push("余额查询凭证已清除".to_string());
            }
        }
        record.set_updated_at(logging::now_ms());
        let name = record.name();
        self.with_conn(&_guard, |conn| sql::update_in_place(conn, &record))?;
        logging::log(
            "[Accounts]",
            &format!("✏️  CatPaw 账号已更新: {name}（{}）", changes.join("，")),
        );
        Ok(changes)
    }

    // ─── 删除保护 ────────────────────────────────────────────
    //
    // 这里曾有 `is_removal_protected_catpaw`：CatPaw 桌面端实时登录态账号
    // （`desktop-auth`）被判定为不可删除，由 `protected_from_removal` 汇总。
    // **现已去掉**，与另外两家（小浣熊 / AutoClaw）同一处理，理由也相同：
    //
    //   桌面端账号是「导入桌面端登录态」建出来的**一条账号记录**，记录里不落
    //   token（凭证每次实时读 `~/.meituan-catpaw/auth.json`），语义是「我要用
    //   这个客户端当前的登录态」—— 用户可能想撤销这个选择，而旧实现只能禁用，
    //   禁用后这条记录仍占着列表与优先级序号。删除只作用于这条记录：客户端的
    //   auth.json 我们从不去写、也不会删，所以安全且可逆（再点一次
    //   「导入桌面端登录态」即可加回来）；想临时停用仍有「禁用」这个更轻的动作。
    //
    // 删除桌面端账号的会话作废路径不受影响：`remove_account` / `batch_remove`
    // 按**记录里取到的 provider** 调 `invalidate_catpaw_sessions`，`desktop-auth`
    // 属于 catpaw 时同样走这条（判据是 provider，不是 id 形态）。
}

/// 取第一个非空文本（按候选顺序；先去 `Bearer ` 前缀、再去空白）。
///
/// 为什么不像 workbuddy 那样直接用 `pick_token`：那个函数只认顶层键，
/// 而 CatPaw 的登录态可能是嵌套的 `auth.accessToken` 形态（粘贴 auth.json）
/// 或原项目的 `access_token` 形态 —— 顶层/嵌套两种布局都要认，所以这里把
/// 「取值」与「校验」分开：本函数只取值，`safe_field` 负责校验。
fn pick_first_text(candidates: &[Option<&Value>], field_names: &[&str]) -> Result<String, AccountStoreError> {
    for value in candidates.iter().flatten() {
        let text = js_string(value);
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }
        return safe_field(
            crate::server::core::account_store::store_util::strip_bearer_prefix(trimmed),
            field_names.first().copied().unwrap_or("token"),
            MAX_TOKEN_LENGTH,
        );
    }
    Err(AccountStoreError::new(
        format!(
            "缺少 CatPaw 登录凭证（{}）",
            field_names.join(" / ")
        ),
        400,
    ))
}

/// 取第一个非空文本（不校验，仅用于 uid / loginName / name 这类可选字段）
fn pick_first_optional(candidates: &[Option<&Value>]) -> String {
    for value in candidates.iter().flatten() {
        if value.is_null() {
            continue;
        }
        let text = js_string(value).trim().to_string();
        if !text.is_empty() {
            return text;
        }
    }
    String::new()
}

/// 从添加 payload 里取余额查询凭证（`balanceToken`，可选）。
///
/// ── 为什么允许三种键名 ────────────────────────────────────────
///   1. `balanceToken`：本项目自己的字段名（账号设置弹窗里手填的那一项）；
///   2. `token2`：CatPaw 上游的 cookie 名（源实现 `PASSPORT_COOKIE_NAMES` 的
///      第一个），用户从浏览器里复制时看到的多半是这个键名；
///   3. `balanceCookie.token2`：粘贴**原项目账号记录**时的形态（原项目按 id
///      索引存在账号文件顶层，旧数据导入时搬到记录的 `balanceCookie`）。
/// 顺序即优先级：显式填的胜过随记录一起粘进来的。取不到返回 None
/// （「未配置」是合法状态，调用方据此不出键，前端提示去配置）。
///
/// 校验走 `safe_field`（与 accessToken 同一套：长度上限 + 禁 CR/LF/分号）——
/// 这个值最终会拼进 `Cookie` 头，换行是头注入、分号会截断 cookie 串。
fn pick_balance_token(
    object: &Map<String, Value>,
    auth: &Map<String, Value>,
) -> Option<String> {
    let candidates: Vec<Option<&Value>> = vec![
        object.get("balanceToken"),
        object.get("token2"),
        auth.get("token2"),
        object
            .get("balanceCookie")
            .and_then(|cookie| cookie.get("token2")),
    ];
    for value in candidates.into_iter().flatten() {
        let text = js_string(value).trim().to_string();
        if text.is_empty() {
            continue;
        }
        // 校验失败（超长 / 含分隔符）时**跳过这一个候选**而不是让整条添加失败：
        // 余额凭证是可选字段，填错它不该把「添加账号」这个主流程堵死。
        let trimmed = strip_bearer_prefix(&text);
        if let Ok(safe) = safe_field(trimmed.to_string(), "balanceToken", MAX_TOKEN_LENGTH) {
            return Some(safe);
        }
        logging::verbose(
            "[Accounts]",
            "CatPaw 余额查询凭证格式无效（超长或含 CR/LF/分号），已忽略",
        );
    }
    None
}

/// 字段校验（原项目 `safeString`）：非空、不超长、不含 CR/LF/分号。
///
/// 为什么要拦 `[\r\n;]`：这些值最终进 HTTP 头（Cookie / user-uid），换行是头注入，
/// 分号会让 Cookie 串被解析成另一段 —— 原项目因此在这里直接拒绝。
fn safe_field(value: String, field: &str, max_length: usize) -> Result<String, AccountStoreError> {
    let trimmed = value.trim().to_string();
    if trimmed.is_empty() {
        return Err(AccountStoreError::new(
            format!("CatPaw 登录态缺少 {field}"),
            400,
        ));
    }
    if trimmed.chars().count() > max_length || trimmed.contains(['\r', '\n', ';']) {
        return Err(AccountStoreError::new(
            format!("CatPaw 登录态中的 {field} 格式无效"),
            400,
        ));
    }
    Ok(trimmed)
}

/// `String::new().into_non_empty()` 的小工具（链式兜底里省一层 `filter`）
trait IntoNonEmpty {
    fn into_non_empty(self) -> Option<String>;
}

impl IntoNonEmpty for String {
    fn into_non_empty(self) -> Option<String> {
        if self.is_empty() { None } else { Some(self) }
    }
}
