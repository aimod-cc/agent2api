//! 小浣熊账号：手动添加、桌面端实时账号、刷新回写、小浣熊公开形态
//! （Agent2API 改造 W3-T4；架构文档 §3.2 与 §5）。
//! 旧数据一次性导入在 `raccoon_import.rs`（同样是账号存储的方法，只是拆了文件）。
//!
//! ── 与 workbuddy 添加路径的关系 ────────────────────────────
//! workbuddy 的 `add_account`（`store_crud.rs`）认的是「会话形态 / 裸凭证形态，
//! 必须有 uid」，登录流程与旧版 auth.json 迁移都走它 —— 那套语义保持不变。
//! 小浣熊的凭证形态不同（JWT + userId，没有 uid 这个概念），因此单独一条添加
//! 路径：
//!   - 手动添加：粘贴 token/refreshToken，或直接粘贴 auth.json 内容
//!     （`access_token` / `refresh_token` 自动识别，对齐源项目 `account-store.mjs`
//!     的 `parseCredentialsPayload`）；
//!   - 桌面端：`importDesktop: true` → 建/更新 `raccoon-desktop` 记录，
//!     **不落 token**（凭证每次实时读 auth.json）。
//!
//! ── 记录里的字段（架构文档 §3.2，沿小浣熊项目命名）──────────
//! `id` / `provider: "raccoon"` / `name` / `userId` / `accessToken` /
//! `refreshToken` / `tokenTail` / `expiresAt` / `source` / `desktop` /
//! `priority` / `enabled` / `addedAt` / `updatedAt`。
//!
//! ── 与契约文档的一处**有意偏离**（键名）─────────────────────
//! 架构文档 §3.2 列的是源项目的 `token` / `tokenExpiresAt`，这里落盘用
//! `accessToken` / `expiresAt`。理由：账号存储的 `has_token()`、`access_token()`、
//! `expires_at()`、公开形态的 `tokenTail` / `hasRefreshToken`、以及刷新回写的
//! `update_account_tokens` 全部认后一组键 —— 换名字就要在这些**共用路径**上
//! 加 provider 分支，正是这次改造要消灭的东西。
//! 数据不丢：旧格式的 `token` / `tokenExpiresAt` 在导入与读取时都被认
//! （`pick_token` 的候选键、公开形态的 `expires_at().or(token_expires_at())`，
//! 见 `state.rs` 与 `raccoon_import.rs`）。
//!
//! ── 硬约束 ────────────────────────────────────────────────
//! 本文件全是「读-改-写」文件操作，**没有任何网络请求**（持锁不做网络）。
//! 绝不 unwrap/expect（release 是 panic=abort）。

use serde_json::{Map, Value};

use crate::server::core::account_store::priority::next_free_priority;
use crate::server::core::account_store::sql;
use crate::server::core::account_store::state::StoredAccount;
use crate::server::core::account_store::store::live_desktop_credentials;
use crate::server::core::account_store::store::{AccountStore, AccountStoreError};
use crate::server::core::account_store::store_util::{
    max_concurrent_public, pick_token, token_tail_of, truncate_chars,
};
use crate::server::core::account_store::{CredentialWrite, MAX_TOKEN_LENGTH};
use crate::server::core::providers::raccoon::{credentials, jwt, models};
use crate::server::core::providers::kind_id;
use crate::server::core::providers::ProviderKind;
use crate::server::logging;

/// 小浣熊的 provider id（`providers::kind_id` 的常量形态，避免每处都调函数）
fn raccoon_id() -> &'static str {
    kind_id(ProviderKind::Raccoon)
}

/// 身份字段（office_*）的长度上限（源项目 `MAX_IDENTITY_LENGTH`）
const MAX_IDENTITY_LENGTH: usize = 1024;

impl AccountStore {
    // ─── 读：账号记录 ────────────────────────────────────────

    /// 取小浣熊账号的**原始记录**（含 accessToken；桌面端账号的 token 不在此，
    /// 由调用方实时读 auth.json）。
    ///
    /// `account_id` 为空 → 取小浣熊组内优先级最小的启用账号（与转发选路同一
    /// 判据，`current_entry_for_provider` 的口径）；找不到返回 None。
    ///
    /// 为什么返回原始 JSON 而不是公开形态：调用方（`raccoon::credentials`）
    /// 需要 accessToken —— 公开形态按设计只有 `tokenTail`。
    pub fn raccoon_account_record(&self, account_id: &str) -> Option<Value> {
        let _guard = self.guard();
        let raccoon = raccoon_id();
        if !account_id.is_empty() {
            // 按 id 直查一行（不读别家、也不读其余小浣熊账号）
            let record = self.record_by_id(&_guard, account_id)?;
            return (record.provider() == raccoon).then(|| record.to_value());
        }
        // 空 id = 取本家队首：只读这一家，排序交给内存（账号最多 20 条）
        let mut candidates: Vec<StoredAccount> = self
            .records_for_provider(&_guard, raccoon)
            .into_iter()
            .filter(StoredAccount::enabled)
            .collect();
        candidates.sort_by_key(StoredAccount::order_key);
        candidates.into_iter().next().map(|item| item.to_value())
    }

    // ─── 写：手动添加 ────────────────────────────────────────

    /// 添加/更新一个小浣熊账号（`POST /api/accounts` 的 raccoon 分支）。
    ///
    /// 接受两种 payload（字段名照抄源项目 `parseCredentialsPayload`）：
    ///   1. `{ token | accessToken | access_token | auth_token, refreshToken |
    ///      refresh_token, name?, officeIdentity?… }`
    ///   2. **直接粘贴 auth.json 内容**：同上（`access_token` 就在候选键里），
    ///      另外 `office_identity` / `office_org_name` / `office_org_role`
    ///      会被保留到账号记录上（源项目同样如此）。
    ///
    /// 校验：token 非空、长度 ≤ `MAX_TOKEN_LENGTH`（8192，源项目同值）、
    /// **必须是可解析的 JWT**（源项目的 `decodeJwtClaims` 失败即报错 ——
    /// 粘错的字符串在转发时只会换来一个 401，不如在这里就说清楚）。
    /// 缺省备注名：`name` → JWT 的 `name` → `账号 {userId}`。
    pub fn add_raccoon_account(
        &self,
        payload: &Value,
        name: Option<&str>,
    ) -> Result<Value, AccountStoreError> {
        let Some(object) = payload.as_object() else {
            return Err(AccountStoreError::new("上传内容必须是 JSON 对象", 400));
        };
        let token = pick_token(
            object,
            &["token", "accessToken", "access_token", "auth_token"],
        );
        let refresh_token = pick_token(object, &["refreshToken", "refresh_token"]);
        if token.is_empty() {
            return Err(AccountStoreError::new(
                "缺少 token（accessToken / access_token）",
                400,
            ));
        }
        if token.chars().count() > MAX_TOKEN_LENGTH
            || refresh_token.chars().count() > MAX_TOKEN_LENGTH
        {
            return Err(AccountStoreError::new("token 或 refreshToken 过长", 400));
        }
        let Some(claims) = jwt::decode_jwt_claims(&token) else {
            return Err(AccountStoreError::new("token 不是有效的 JWT", 400));
        };
        let user_id = {
            let extracted = jwt::extract_user_id(&claims);
            if extracted.is_empty() {
                "unknown".to_string()
            } else {
                extracted
            }
        };
        let expires_at = jwt::jwt_expiry_ms(&token);
        let identity = |keys: &[&str]| -> String {
            for key in keys {
                if let Some(Value::String(text)) = object.get(*key) {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        return truncate_chars(trimmed, MAX_IDENTITY_LENGTH);
                    }
                }
            }
            String::new()
        };
        let office_identity = identity(&["officeIdentity", "office_identity"]);
        let office_org_name = identity(&["officeOrgName", "office_org_name"]);
        let office_org_role = identity(&["officeOrgRole", "office_org_role"]);
        let display = jwt::display_name(&claims);

        let _guard = self.guard();
        let id = format!("user-{user_id}");
        // 只读这一行（不再读全量）：既有记录决定「更新还是新建」与两处沿用值
        let existing = self.record_by_id(&_guard, &id);
        // ── 撞 id 保护（与 `add_account` 同一考虑，方向相反）─────────
        // 小浣熊的 `user-<userId>` 与 workbuddy 的 `user-<uid>` 形态相同但 id 空间
        // 独立，一个数字 uid 可能两家都用。撞上**别的 provider** 的记录时报错，
        // 而不是把它改写成 raccoon（那会连凭证一起丢掉）。
        if let Some(existing) = existing.as_ref() {
            let existing_provider = existing.provider();
            if existing_provider != raccoon_id() {
                return Err(AccountStoreError::new(
                    format!(
                        "账号 id「{id}」已被{existing_provider}账号占用，无法添加同一 userId 的\
                         小浣熊账号（请先处理那个账号）"
                    ),
                    400,
                ));
            }
        }
        let explicit_name = name
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| truncate_chars(value, 100));
        let record_name = explicit_name
            .or_else(|| {
                existing
                    .as_ref()
                    .map(StoredAccount::name)
                    .filter(|value| !value.is_empty())
            })
            .or_else(|| {
                if display.is_empty() {
                    None
                } else {
                    Some(display.clone())
                }
            })
            .unwrap_or_else(|| format!("账号 {user_id}"));
        let merged_identity = |incoming: String, key: &str| -> String {
            if !incoming.is_empty() {
                return incoming;
            }
            existing
                .as_ref()
                .and_then(|item| item.get(key))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
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
        record.insert("provider".to_string(), Value::String(raccoon_id().to_string()));
        record.insert("name".to_string(), Value::String(record_name.clone()));
        record.insert("userId".to_string(), Value::String(user_id.clone()));
        record.insert("accessToken".to_string(), Value::String(token.clone()));
        record.insert(
            "refreshToken".to_string(),
            Value::String(if refresh_token.is_empty() {
                existing
                    .as_ref()
                    .map(StoredAccount::refresh_token)
                    .unwrap_or_default()
            } else {
                refresh_token.clone()
            }),
        );
        record.insert(
            "tokenTail".to_string(),
            Value::String(token_tail_of(&token)),
        );
        record.insert(
            "expiresAt".to_string(),
            expires_at
                .map(crate::server::core::account_store::state::json_number)
                .unwrap_or(Value::Null),
        );
        for (key, value) in [
            ("officeIdentity", merged_identity(office_identity, "officeIdentity")),
            ("officeOrgName", merged_identity(office_org_name, "officeOrgName")),
            ("officeOrgRole", merged_identity(office_org_role, "officeOrgRole")),
        ] {
            if !value.is_empty() {
                record.insert(key.to_string(), Value::String(value));
            }
        }
        // 手动添加的账号一律 source=manual（导入路径才写 imported）
        record.insert("source".to_string(), Value::String("manual".to_string()));
        record.insert("priority".to_string(), Value::from(priority));
        record.insert(
            "enabled".to_string(),
            Value::Bool(
                existing
                    .as_ref()
                    .map(StoredAccount::enabled)
                    .unwrap_or(true),
            ),
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
        // 未知字段全量保留：把既有记录的键补回来（用户手工加过的字段不能丢）
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
        logging::log(
            "[Accounts]",
            &format!(
                "✅ 小浣熊账号已保存: {record_name}（{user_id}，优先级 {priority}）"
            ),
        );
        Ok(self.to_raccoon_public_account(&saved))
    }

    /// 导入/刷新「桌面端实时登录态」账号（`importDesktop: true` 与启动导入共用）。
    ///
    /// 语义（架构文档 §3.2）：id 固定 `raccoon-desktop`、`desktop: true`、
    /// **账号记录里不落 token**（凭证每次实时读 `~/.box-agent/config/auth.json`）。
    /// 已存在时**幂等**：只更新展示字段（userId / tokenTail / 过期时间 / mtime），
    /// 保留优先级、启用状态与用户改过的备注名。
    ///
    /// 读不到登录态时报 400 并把原因说清楚（「请先在小浣熊客户端登录」）——
    /// 这是用户点按钮时的即时反馈，静默建一条空记录只会让人以为成功了。
    pub fn import_raccoon_desktop_account(&self, source: &str) -> Result<Value, AccountStoreError> {
        let summary = credentials::desktop_summary()
            .map_err(|reason| AccountStoreError::new(reason, 400))?;
        let user_id = summary
            .get("userId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let _guard = self.guard();
        let id = credentials::DESKTOP_ACCOUNT_ID.to_string();
        let existing = self.record_by_id(&_guard, &id);
        let default_name = if user_id.is_empty() {
            "桌面端登录账号".to_string()
        } else {
            format!("桌面端登录账号（{user_id}）")
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
        record.insert("provider".to_string(), Value::String(raccoon_id().to_string()));
        record.insert("name".to_string(), Value::String(record_name.clone()));
        record.insert("userId".to_string(), Value::String(user_id.clone()));
        record.insert("desktop".to_string(), Value::Bool(true));
        record.insert("source".to_string(), Value::String(source.to_string()));
        for (target, key) in [
            ("tokenTail", "tokenTail"),
            ("expiresAt", "tokenExpiresAt"),
            ("desktopMtime", "mtime"),
        ] {
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
                // 桌面端账号的凭证**不落盘**：老记录里若有 token 残留，这里跳过
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
        logging::log(
            "[Accounts]",
            &format!(
                "{} 小浣熊桌面端登录态{}: {record_name}",
                if is_new { "✅" } else { "🔄" },
                if is_new { "已导入" } else { "已刷新" },
            ),
        );
        Ok(self.to_raccoon_public_account(&saved))
    }

    /// 小浣熊账号刷新成功后回写新 token（`raccoon::credentials` 的刷新路径调用）。
    ///
    /// ── 比较-再写（本次修复）──────────────────────────────────
    /// 只有当记录里**此刻的** accessToken / refreshToken 仍等于刷新前那份快照时
    /// 才写入；否则返回 [`CredentialWrite::Stale`]，由调用方改用最新快照。
    /// 比较与写入在**同一把账号锁内**完成（同一个 `_guard` 下 load → 比较 → 改
    /// → save），因此不会出现「检查完释放锁、写入时已被别的写者换掉」的窗口。
    ///
    /// 为什么需要它：刷新是网络动作（秒级），期间用户可能重导入账号、换号，
    /// 或者桌面端重新登录、另一轮刷新先落地。旧实现无条件覆盖，会把**旧凭证**
    /// 盖到新凭证上（用户刚换的账号被顶回旧的）。
    ///
    /// 同时拒绝桌面端账号（它的凭证在 auth.json，回写账号记录毫无意义，
    /// 还会把一个本该「实时读盘」的账号变成一份过期副本），并同步小浣熊侧的
    /// `userId`（新 token 的 JWT 声明是权威的）——都在同一次写入里完成，
    /// 不再有「先更新 token、再单独加锁补 userId」的第二次写入。
    pub fn update_raccoon_account_tokens_if_current(
        &self,
        id: &str,
        expected_access_token: &str,
        expected_refresh_token: &str,
        access_token: &str,
        refresh_token: &str,
        expires_at: Option<f64>,
    ) -> Result<CredentialWrite, String> {
        let _guard = self.guard();
        // 「比较-再写」只涉及这一行：读它、比它、原地更新它。
        // 改造前这里要把全部账号读进内存、改一条、再把全部写回去 ——
        // 而刷新是**每个请求都可能触发**的路径。
        let Some(mut record) = self.record_by_id(&_guard, id) else {
            // 账号已被删除（或 id 被换掉）：刷新结果无处可写，也不该新建记录
            return Ok(CredentialWrite::Stale);
        };
        if record.is_desktop() {
            return Err("桌面端账号的凭证不落盘（实时读 auth.json），无需回写".to_string());
        }
        if record.provider() != raccoon_id() {
            return Err(format!("账号 {id} 不是小浣熊账号"));
        }
        // 比较：记录里此刻的凭证必须仍是刷新前那份（换号 / 重导入 / 另一轮刷新
        // 先落地都会在这里被拦住）
        if record.access_token() != expected_access_token
            || record.refresh_token() != expected_refresh_token
        {
            return Ok(CredentialWrite::Stale);
        }
        // 同一把锁内写入（比较与写入之间不可能被别的写者插入）
        if !access_token.is_empty() {
            record.set("accessToken", Value::String(access_token.to_string()));
            record.set("tokenTail", Value::String(token_tail_of(access_token)));
        }
        if !refresh_token.is_empty() {
            record.set("refreshToken", Value::String(refresh_token.to_string()));
        }
        if let Some(value) = expires_at.filter(|value| *value > 0.0) {
            record.set(
                "expiresAt",
                crate::server::core::account_store::state::json_number(value),
            );
        }
        if let Some(claims) = jwt::decode_jwt_claims(access_token) {
            let user_id = jwt::extract_user_id(&claims);
            if !user_id.is_empty() {
                record.set("userId", Value::String(user_id));
            }
        }
        record.set_updated_at(logging::now_ms());
        self.with_conn(&_guard, |conn| sql::update_in_place(conn, &record))
            .map_err(|error| error.message)?;
        Ok(CredentialWrite::Written)
    }

    // ─── 删除保护 ────────────────────────────────────────────

    /// 该账号是否受保护、不可删除。返回 `Some(原因)` 表示应当拒绝删除。
    ///
    /// ── 现在**不保护任何账号**（函数本身刻意保留）────────────────
    /// 三家（小浣熊 / CatPaw / AutoClaw）的桌面端实时登录态账号曾在这里被拒绝
    /// 删除（各家的 `is_removal_protected_*` 判定 + 本函数汇总，W3-T4 /
    /// W5-T-d4 / W4b-T-c2），**现已全部允许**。理由对三家是同一个：
    ///
    ///   桌面端账号是「导入桌面端登录态」这个动作建出来的**一条账号记录**
    ///   （id 形如 `raccoon-desktop` / `desktop-auth` / `autoclaw-desktop`），
    ///   记录里不落 token、每次转发实时读客户端的登录态文件。它的语义是
    ///   「我要用这个客户端当前的登录态」—— 用户完全可能想撤销这个选择
    ///   （不再想用它，或想换成手动粘贴的账号）。旧的「不可删除」把用户锁死：
    ///   只给「禁用」一条路，而禁用后这条记录仍然占着列表、占着优先级序号 ——
    ///   「我不要这条记录」这个正当诉求无处表达。删除把它还给了用户。
    ///
    ///   删除**只作用于这条记录**：客户端的登录态文件我们从不写、也不删，
    ///   所以删掉是安全且可逆的 —— 想再用，点一次「导入桌面端登录态」就能
    ///   加回来（`import_*_desktop_account` 对不存在的记录是新建）。想临时
    ///   停用它，「禁用」仍是更轻的动作，两者都留给用户自己选。
    ///
    /// ── 为什么保留这个恒返回 None 的函数 ────────────────────────
    /// 调用方（`remove_account` / `batch_remove`）只问「能不能删」、不认识
    /// provider 分支，这是一条**明确的扩展点**：将来若真出现「不允许删」的账号
    /// （例如某种不可再生的凭证），判定加回这一处即可，调用方一行不用改。
    /// 删掉函数等于把「哪些账号不可删」重新散布回每个调用点。
    ///
    /// 形参改名 `_id`：本函数不再读它，留着原名会有 unused 警告（调用方不受影响）。
    pub(crate) fn protected_from_removal(&self, _id: &str) -> Option<String> {
        None
    }

    // ─── 公开形态（小浣熊）───────────────────────────────────

    /// 小浣熊账号的公开形态。
    ///
    /// 字段（架构文档 §5 与 §6）：`id` / `provider` / `name` / `userId` /
    /// `tokenTail` / `tokenExpiresAt` / `hasRefreshToken` / `desktop` /
    /// `priority` / `enabled` / `addedAt` / `updatedAt` / `source` /
    /// 出网代理。
    ///
    /// **不显示积分字段**（本期明确不迁移积分/账单），因此这里不放
    /// `credits` / `usage` 之类；界面按 provider 分支渲染时也不会去读。
    /// workbuddy 账号的公开形态仍走 `to_public_account`（字段逐字不变）。
    ///
    /// ── 桌面端账号按**实时值**展示（W3-T4）─────────────────────
    /// 这类账号记录里本来就（按设计）没有 token，`tokenTail` / 过期时间 /
    /// `hasRefreshToken` 若只读记录会永远是空的，界面看起来就是「没有凭证」。
    /// 因此这里在有桌面端登录态时优先用 auth.json 的实时值；读不到（客户端
    /// 退出登录、文件被删）时回落记录里的存量值 —— 于是「登录态消失」表现为
    /// 字段变空，而不是整条记录消失。
    /// `tokenExpiresAt` 还兼容旧数据直接搬过来的 `tokenExpiresAt` 键名。
    pub fn to_raccoon_public_account(&self, record: &StoredAccount) -> Value {
        let proxy = crate::server::core::proxies::describe_account_proxy(Some(
            &record.proxy(),
        ));
        let stored_tail = record
            .get("tokenTail")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let stored_expires = record.expires_at().or_else(|| record.token_expires_at());
        let (token_tail, expires_at, has_refresh) = match live_desktop_credentials(record) {
            Some((token, refresh_token, expires_at)) => (
                if token.is_empty() {
                    stored_tail
                } else {
                    token_tail_of(&token)
                },
                if expires_at > 0.0 { Some(expires_at) } else { stored_expires },
                !refresh_token.is_empty(),
            ),
            None => (
                stored_tail,
                stored_expires,
                !record.refresh_token().is_empty(),
            ),
        };
        let mut public = Map::new();
        public.insert("id".to_string(), Value::String(record.id().to_string()));
        public.insert(
            "provider".to_string(),
            Value::String(record.provider()),
        );
        public.insert("name".to_string(), Value::String(record.name()));
        public.insert("userId".to_string(), Value::String(record.user_id()));
        public.insert("tokenTail".to_string(), Value::String(token_tail));
        public.insert(
            "tokenExpiresAt".to_string(),
            expires_at
                .map(crate::server::core::account_store::state::json_number)
                .unwrap_or(Value::Null),
        );
        public.insert("hasRefreshToken".to_string(), Value::Bool(has_refresh));
        public.insert("desktop".to_string(), Value::Bool(record.is_desktop()));
        public.insert("source".to_string(), Value::String(record.source()));
        public.insert("priority".to_string(), Value::from(record.priority()));
        public.insert("enabled".to_string(), Value::Bool(record.enabled()));
        public.insert("addedAt".to_string(), Value::from(record.added_at()));
        public.insert("updatedAt".to_string(), Value::from(record.updated_at()));
        public.insert("proxy".to_string(), proxy);
        // `rateLimits` 是**限额冷却标记**（账号×模型），不是积分字段：
        // 选路层（`routing::pick_account_by_priority`）正是从公开形态里读它来决定
        // 「这个账号对该模型是否还在冷却期内」。少了它，小浣熊账号在 429 之后的
        // **下一次请求**仍会被选中（同一次请求内的降级靠 tried_ids 兜住了），
        // 于是客户端重试会一直撞在同一个限额账号上，而不是降级到下一个。
        // 默认给 `{}`（与 workbuddy 的公开形态同一兜底口径）。
        public.insert(
            "rateLimits".to_string(),
            record.get("rateLimits").cloned().unwrap_or_else(|| Value::Object(Map::new())),
        );
        // 与 workbuddy 的公开形态同口径：本切片所有账号都视为可用
        public.insert("available".to_string(), Value::Bool(true));
        // 单账号并发上限（所有家通用，兜底共用 `max_concurrent_public`）：
        // 0 = 不限，缺键同样输出 0
        public.insert(
            "maxConcurrent".to_string(),
            Value::from(max_concurrent_public(record.get("maxConcurrent"))),
        );
        Value::Object(public)
    }


    /// `GET /v1/models` 之类探针用的小浣熊模型清单条数（排障口，保留）
    #[allow(dead_code)]
    pub fn raccoon_model_count(&self) -> usize {
        models::count()
    }
}
