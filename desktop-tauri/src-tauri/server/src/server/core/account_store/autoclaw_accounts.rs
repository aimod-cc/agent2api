//! AutoClaw 账号：手动添加、桌面端实时账号、刷新回写、AutoClaw 公开形态
//! （Agent2API 二期 W4b-T-c2；架构文档 §10.2）。
//! 旧数据一次性导入在 `autoclaw_import.rs`（同样是账号存储的方法，只是拆了文件）。
//!
//! ── 与 workbuddy / 小浣熊 / CatPaw 添加路径的关系 ──────────────
//! 四家各有一条添加路径，因为凭证形态各不相同：
//!   - workbuddy：`user-<uid>` + accessToken/refreshToken + edition/endpoint；
//!   - 小浣熊：JWT + refreshToken（可续期）；
//!   - CatPaw：Cookie 形态的 `X-Passport-Token` + 独立 uid，**没有刷新机制**；
//!   - AutoClaw：**JWT（`user_id` / `device_id` 声明）+ refreshToken（可续期）**，
//!     凭证可能是 safeStorage 密文（`enc:` 前缀，见下）。
//! 本文件服务第四条路径（`api::accounts::add_account` 的 autoclaw 分支）。
//!
//! ── 账号字段（对照原项目 `account-store.mjs`，**唯一事实来源**）──
//! ```text
//!   原项目 accounts[i]：{ id, name, userId, deviceId, token, refreshToken,
//!                        tokenTail, tokenExpiresAt, addedAt, updatedAt }
//!   原项目 toPublicAccount()：{ id, name, userId, tokenTail, tokenExpiresAt,
//!                             hasRefreshToken, addedAt, updatedAt, available }
//!   原项目 addAccount 的 id：`user-${credentials.userId}`
//!   原项目账号文件：~/.autoclaw-proxy/accounts.json（createAccountStore 默认目录）
//!   原项目桌面端登录态：%APPDATA%/AutoClaw/auth.json（DESKTOP_ACCOUNT_ID='desktop-auth'）
//! ```
//! 本项目的记录在原字段基础上补 `provider` / `priority` / `enabled` / `source`
//! / `desktop` / `proxy`（与另外三家共用账号存储的通用设施：优先级号段、启用
//! 开关、出网代理、限额冷却）。
//!
//! ── 两处**有意偏离**原项目（键名与桌面端 id）──────────────────
//!   1. **token 键名**：原项目用 `token` / `tokenExpiresAt`，这里落盘用
//!      `accessToken` / `expiresAt` —— 与 W3 为小浣熊确立的口径一致（理由见
//!      `raccoon_accounts.rs` 的模块头：`has_token()` / `access_token()` /
//!      `expires_at()` / `update_account_tokens` 全认后一组键，各认一套会让每条
//!      共用路径都长出 provider 分支）。读取侧兼容旧名（凭证层的
//!      `credentials_from_record` 的候选键、下面 `pick` 的多键取值）。
//!   2. **桌面端账号 id**：原项目用 `desktop-auth`，这里用 `autoclaw-desktop`。
//!      原因很实际：`desktop-auth` 已被 **CatPaw** 的桌面端账号占用
//!      （`catpaw::credentials::DESKTOP_ACCOUNT_ID`，同样照抄原项目），而账号
//!      记录的 id 在整份账号集合里唯一、`remove_account` / `patch_account`
//!      都按裸 id 查找 —— 两家同名会让「删除/改备注」落到另一家身上。
//!      凭证层的 `credentials::DESKTOP_ACCOUNT_ID`（`desktop-auth`）是**凭证对象**
//!      的 id，与账号记录 id 是两回事；`snapshot_for` 认的是记录里的 `desktop`
//!      标记（见那边的说明），因此这里换 id 不影响凭证链。
//!
//! ── `enc:` 密文粘贴（原项目 `parseCredentialsPayload`）────────
//! 用户可以直接粘贴桌面端 `auth.json` 的内容（`token` / `refreshToken` 形如
//! `enc:v10:...`）。本函数用凭证层的 `credentials_from_record` 解析 —— 它会
//! 自动走 DPAPI + AES-GCM 解密链（T-c1 已验证），**落盘存解密后的明文**：
//! 与原项目 `addAccount` 一致（它也是解密后再存），且让账号能随
//! `/api/accounts/export` 迁移到另一台机器。凭证层仍然认 `enc:` 形态（手改
//! 文件塞密文的用户不受影响）。
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
    max_concurrent_public, pick_token, token_tail_of, truncate_chars,
};
use crate::server::core::account_store::{is_autoclaw_family, CredentialWrite, MAX_TOKEN_LENGTH};
use crate::server::core::providers::autoclaw::{credentials, crypto, Region};
use crate::server::logging;

/// 按地区取 provider id（国内版 `autoclaw` / 国际版 `autoclaw-intl`）。
///
/// **不要**再另写一个不带参数的 `autoclaw_id()`：所有判定都必须按地区分派
/// （两个地区的账号集合是分开的），留一个「默认国内版」的便捷函数只会让
/// 新代码顺手用它、然后在国际版上静默失配。函数体走注册表，不存在第二份 id 清单。
fn autoclaw_id_for(region: Region) -> &'static str {
    region.provider_id()
}

/// 桌面端实时登录态账号的固定 id（见模块头的「有意偏离」第 2 条）—— **国内版**。
///
/// 国际版的对应 id 见 [`desktop_account_id`]：两地**必须不同**。那个文件两地
/// 共用（`%APPDATA%/AutoClaw/auth.json`，没有地区标记），但账号记录是两条
/// 独立的记录、各归各的 provider；若用同一个 id，第二次导入会撞上存储层的
/// 跨 provider 保护而报错（「账号 id 已被 xxx 账号占用」），两地也就无法并存。
pub const DESKTOP_ACCOUNT_ID: &str = "autoclaw-desktop";

/// 国际版桌面端账号的固定 id（[`DESKTOP_ACCOUNT_ID`] 的国际版对应值）。
///
/// `pub` 而不是私有：账号迁移的「保留 id」清单要列出它
/// （`account_transfer::identity::RESERVED_DESKTOP_IDS`）——
/// 漏了它，这条桌面端记录就能被当成普通账号导入，然后因为「不落 token」
/// 而永远 `available: false`。
pub const INTL_DESKTOP_ACCOUNT_ID: &str = "autoclaw-intl-desktop";

/// 按地区取桌面端账号的固定 id。
///
/// 国内版保持历史值 `autoclaw-desktop`（存量记录的 id，不能变）；
/// 国际版用 `autoclaw-intl-desktop`。与 [`autoclaw_id_for`] 同一手法 ——
/// 一切按地区分派，不提供「默认国内版」的便捷写法。
fn desktop_account_id(region: Region) -> &'static str {
    match region {
        Region::Cn => DESKTOP_ACCOUNT_ID,
        Region::Intl => INTL_DESKTOP_ACCOUNT_ID,
    }
}

/// 桌面端账号的备注名**是不是自动生成的那一版**（用户没改过）。
///
/// 用来决定「这次导入要不要把名字更新成新的默认值」：
///   - 两个历史/当前的自动名形态：`桌面端登录账号`、`桌面端登录账号（<userId>）`；
///   - 含 `@` 的一律视为自动生成 —— 现在的默认名就是邮箱，而桌面端记录按设计
///     代表「客户端此刻登录的那个号」，用户在客户端换号后名字要跟着走，
///     否则一条记录上会同时出现新旧两个邮箱（名字是旧的、副标题是新的）。
///
/// 用户自己起的备注名（不含 `@`、也不是上面两种形态）一律保留 ——
/// 与另外几家的 `add_*` 路径「不覆盖用户改过的备注名」同一纪律。
fn is_generated_desktop_name(name: &str, user_id: &str) -> bool {
    name == "桌面端登录账号"
        || (!user_id.is_empty() && name == format!("桌面端登录账号（{user_id}）"))
        || name.contains('@')
}

/// 备注名长度上限（原项目 `name.trim().slice(0, 100)`）
const MAX_NAME_LENGTH: usize = 100;

/// uid / deviceId 的长度上限（原项目 `MAX_USER_UID_LENGTH = 256` 的同类防护；
/// 这两个值会进请求体与界面，超长的一律截断）
const MAX_IDENTITY_LENGTH: usize = 256;

impl AccountStore {
    // ─── 读：账号记录 ────────────────────────────────────────

    /// 取 AutoClaw 账号的**原始记录**（含 accessToken；桌面端账号的 token 不在此，
    /// 由调用方实时读 auth.json 并解密）。
    ///
    /// `account_id` 为空 → 取 AutoClaw 组内优先级最小的启用账号（与转发选路同一
    /// 判据，`current_entry_for_provider` 的口径）；找不到返回 None。
    ///
    /// 为什么返回原始 JSON 而不是公开形态：适配器的 `resolve_credentials` 需要
    /// 把记录交给凭证层（`credentials::snapshot_for`），而公开形态按设计只有
    /// `tokenTail`（见 `to_autoclaw_public_account`）。
    pub fn autoclaw_account_record(&self, region: Region, account_id: &str) -> Option<Value> {
        let _guard = self.guard();
        let autoclaw = autoclaw_id_for(region);
        if !account_id.is_empty() {
            // 按 id 直查一行（不读别家、也不读其余 AutoClaw 账号）
            let record = self.record_by_id(&_guard, account_id)?;
            return (record.provider() == autoclaw).then(|| record.to_value());
        }
        let mut candidates: Vec<StoredAccount> = self
            .records_for_provider(&_guard, autoclaw)
            .into_iter()
            .filter(StoredAccount::enabled)
            .collect();
        candidates.sort_by_key(StoredAccount::order_key);
        candidates.into_iter().next().map(|item| item.to_value())
    }

    // ─── 写：手动添加 ────────────────────────────────────────

    /// 添加/更新一个 AutoClaw 账号（`POST /api/accounts` 的 autoclaw 分支）。
    ///
    /// 接受两种 payload（字段名照抄原项目 `parseCredentialsPayload`）：
    ///   1. 扁平形态：`{ token | accessToken | access_token, refreshToken |
    ///      refresh_token, deviceId?, name? }`；
    ///   2. **直接粘贴桌面端 auth.json 内容**：同上（`token` / `refreshToken`
    ///      可能是 `enc:` 密文，自动解密）。
    ///
    /// 校验（照抄原项目）：token 非空、≤ `MAX_TOKEN_LENGTH`（8192）、**必须是
    /// 可解出 `user_id` 的 AutoClaw JWT**（原项目 `缺少 user_id` 那条报错）——
    /// 粘错的字符串在转发时只会换来一个 401，不如在这里就说清楚。
    /// id 生成沿原项目规则：`user-<userId>`。
    ///
    /// 撞 id（本机已有同 id 记录）时：同 provider 就地更新（保留优先级与启用
    /// 状态、沿用用户改过的备注名），撞到**别的 provider** 时报错 —— 绝不覆写
    /// 他人记录（与另外三家同一策略）。
    pub fn add_autoclaw_account(
        &self,
        region: Region,
        payload: &Value,
        name: Option<&str>,
    ) -> Result<Value, AccountStoreError> {
        let autoclaw = autoclaw_id_for(region);
        let Some(object) = payload.as_object() else {
            return Err(AccountStoreError::new("上传内容必须是 JSON 对象", 400));
        };
        // 原项目的取值是「token 非空就用 token，否则 accessToken」——
        // 候选键覆盖原项目名（token）与本项目落盘名（accessToken）以及
        // auth.json 的 snake_case（access_token）。
        let raw_token = pick_token(object, &["token", "accessToken", "access_token"]);
        let raw_refresh = pick_token(object, &["refreshToken", "refresh_token"]);
        if raw_token.is_empty() {
            return Err(AccountStoreError::new(
                "缺少 token（accessToken / access_token）",
                400,
            ));
        }
        if raw_token.chars().count() > MAX_TOKEN_LENGTH
            || raw_refresh.chars().count() > MAX_TOKEN_LENGTH
        {
            return Err(AccountStoreError::new("token 或 refreshToken 过长", 400));
        }
        // 交给凭证层解析：`enc:` 密文在这里被解密，明文 token 原样通过；
        // 失败（解密失败 / token 为空）由它给出可读的中文原因。
        // **状态码统一归到 400**：凭证层是在**转发链路**的语义下写那些错误的
        // （401 = 拿不到可用凭证），而这里是一次「上传账号」的管理动作 ——
        // 失败原因是用户给的内容，400 才与另外三家添加路径一致
        // （400 也让前端的报错提示走「参数问题」而不是「需要重新登录」）。
        let parsed = credentials::credentials_from_record(payload, region)
            .map_err(|error| AccountStoreError::new(error.message, 400))?;
        if parsed.user_id.is_empty() {
            return Err(AccountStoreError::new(
                "token 不是有效的 AutoClaw JWT（缺少 user_id）",
                400,
            ));
        }
        // deviceId 优先用 payload 里的显式值（凭证层已做同一件事：文件字段 →
        // JWT 的 `device_id` 声明），因此这里直接用它算出来的值。
        let user_id = truncate_chars(&parsed.user_id, MAX_IDENTITY_LENGTH);
        let device_id = truncate_chars(&parsed.device_id, MAX_IDENTITY_LENGTH);

        let _guard = self.guard();
        // id 前缀按地区给：国内版保持裸 `user-`（存量账号的 id 就长这样），
        // 国际版带 `intl-` 前缀 —— 两地的 userId 可能撞，前缀让它们天然不相交
        // （完整理由见 `Region::account_id_prefix`）。
        let id = format!("{}{user_id}", region.account_id_prefix());
        // 只读这一行（不再读全量）：既有记录决定「更新还是新建」与多处沿用值
        let existing = self.record_by_id(&_guard, &id);
        if let Some(existing) = existing.as_ref() {
            let existing_provider = existing.provider();
            if existing_provider != autoclaw {
                return Err(AccountStoreError::new(
                    format!(
                        "账号 id「{id}」已被{existing_provider}账号占用，无法添加同一 userId 的\
                         AutoClaw 账号（请先处理那个账号）"
                    ),
                    400,
                ));
            }
        }
        // 备注名兜底：显式传入 → 既有记录里的备注名 → `账号 {userId}`
        // （原项目的缺省是 `账号 ${userId}`；它没有「用户改过的备注名要保留」
        // 这一层，那层是本项目 `add_*` 路径共有的语义）
        let explicit_name = name
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| truncate_chars(value, MAX_NAME_LENGTH));
        let record_name = explicit_name
            .or_else(|| {
                existing
                    .as_ref()
                    .map(StoredAccount::name)
                    .filter(|value| !value.is_empty())
            })
            .unwrap_or_else(|| format!("账号 {user_id}"));
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
        record.insert("provider".to_string(), Value::String(autoclaw.to_string()));
        record.insert("name".to_string(), Value::String(record_name.clone()));
        record.insert("userId".to_string(), Value::String(user_id.clone()));
        if !device_id.is_empty() {
            record.insert("deviceId".to_string(), Value::String(device_id));
        }
        // 邮箱：由调用方在建账号前补进 payload —— 网页登录（OAuth）换码后查一次
        // 用户资料，`providers::autoclaw::profile` 负责查（token 里没有邮箱）。
        // 手填凭证那条链没有这个字段，之后由余额查询顺带回填
        // （见 `set_autoclaw_account_email`）；payload 没带时下面那轮
        // 「未知字段全量保留」会把记录里已有的邮箱留着，不会被这次更新抹掉。
        if let Some(email) = object
            .get("email")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| truncate_chars(value, MAX_IDENTITY_LENGTH))
        {
            record.insert("email".to_string(), Value::String(email));
        }
        record.insert(
            "accessToken".to_string(),
            Value::String(parsed.token.clone()),
        );
        record.insert(
            "refreshToken".to_string(),
            Value::String(if parsed.refresh_token.is_empty() {
                existing
                    .as_ref()
                    .map(StoredAccount::refresh_token)
                    .unwrap_or_default()
            } else {
                parsed.refresh_token.clone()
            }),
        );
        record.insert(
            "tokenTail".to_string(),
            Value::String(token_tail_of(&parsed.token)),
        );
        record.insert(
            "expiresAt".to_string(),
            parsed
                .expires_at
                .map(crate::server::core::account_store::state::json_number)
                .unwrap_or(Value::Null),
        );
        // 手动添加一律 source=manual（导入路径才写 imported / desktop）
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
                    // 粘贴原项目的整条账号记录时沿用它的时间线
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
        // 未知字段全量保留（用户手工加过的字段不能因为一次「更新账号」丢掉）
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
            &format!("✅ AutoClaw 账号已保存: {record_name}（{user_id}，优先级 {priority}）"),
        );
        Ok(self.to_autoclaw_public_account(&saved))
    }

    /// 导入/刷新「桌面端实时登录态」账号（`importDesktop: true` 与启动导入共用）。
    ///
    /// 语义（对齐 raccoon-desktop 模式）：id 固定 [`DESKTOP_ACCOUNT_ID`]
    /// （`autoclaw-desktop`）、`desktop: true`、**账号记录里不落 token** ——
    /// 凭证每次实时读 `%APPDATA%/AutoClaw/auth.json`（DPAPI + AES-GCM 解密，
    /// 带 mtime 缓存），桌面端重新登录后下一次请求即生效。
    /// 已存在时**幂等**：只更新展示字段（userId / deviceId / tokenTail / 过期时间），
    /// 保留优先级、启用状态与用户改过的备注名。
    ///
    /// 读不到登录态时报 400 并把原因说清楚（「请在 AutoClaw 桌面端登录」）——
    /// 这是用户点按钮时的即时反馈，静默建一条空记录只会让人以为成功了。
    pub fn import_autoclaw_desktop_account(
        &self,
        region: Region,
        source: &str,
    ) -> Result<Value, AccountStoreError> {
        let autoclaw = autoclaw_id_for(region);
        // ── 桌面端登录态文件没有地区标记（一次真实的歧义，这里不猜）────
        // `%APPDATA%/AutoClaw/auth.json` 里只有 `{deviceId, updatedAt, userInfo,
        // token, refreshToken}`，**没有**任何地区字段；两个构建的 Electron
        // 应用名都是 `autoclaw`、userData 也是同一个目录（实测 inode 相同）。
        // 也就是说这个文件属于哪个地区，只取决于用户装的是哪个构建。
        //
        // 因此**地区由用户选的那一项决定**，本函数不替他判断：他在「国内版」
        // 区块点导入，就得到一条国内版账号；在国际版区块点，就得到国际版账号。
        // 这是唯一诚实的做法 —— 本机无从判断，而用户自己知道装的是哪个客户端。
        //
        // 猜错的后果是**可见的**而不是静默的：凭证与域名不匹配时上游直接 401，
        // 用户看到明确的失败，换到另一项重新导入即可。反过来，若在这里替用户
        // 拒绝（曾经如此），最需要这条路的人反而被挡住 —— 当时国际版的 OAuth
        // 登录还没接上（那条链路强制风控验证码），「从客户端导入」几乎是 OAuth
        // 用户唯一实用的入口。OAuth 现已接上（见 `providers::autoclaw::oauth`），
        // 但导入照旧两个地区都给：它是一条独立可用的路径（不需要过一次验证码），
        // 没有理由因为「多了 OAuth」就把它收回去。
        // 本机这份 auth.json 正是一个 OAuth 账号（邮箱有值、手机号为空）。
        let summary = credentials::local_summary(region)
            .map_err(|reason| AccountStoreError::new(reason, 400))?;
        let user_id = summary
            .get("userId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        // 邮箱：auth.json 的 `userInfo.email`（明文，本地读，不联网）。
        // 它是这个账号在界面上的主要标识 —— 见 `providers::autoclaw::profile`
        let email = summary
            .get("email")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let _guard = self.guard();
        let id = desktop_account_id(region).to_string();
        let existing = self.record_by_id(&_guard, &id);
        if let Some(existing) = existing.as_ref() {
            let existing_provider = existing.provider();
            if existing_provider != autoclaw {
                return Err(AccountStoreError::new(
                    format!(
                        "账号 id「{id}」已被{existing_provider}账号占用，无法导入 AutoClaw 桌面端登录态"
                    ),
                    400,
                ));
            }
        }
        // ── 同一地区、同一个账号只留一条记录（本次新增的判断）──────────
        // 桌面端记录（固定 id `autoclaw-desktop` / `autoclaw-intl-desktop`）与
        // 手动 / OAuth 账号（id `user-<userId>` / `intl-user-<userId>`）是**两条
        // 不同 id 的记录，却指向同一个上游账号**：两条并存时选路会把它当成两个
        // 账号用（同一个人的额度被并发消耗、报表里也数成两个），用户看着也像
        // 重复。因此新建之前先看本地区有没有别的记录已经是这个 userId：有就明确
        // 拒绝，把「留哪一条」的决定交回用户（删掉旧的再导入，或不导入）。
        //
        // 只在**新建**时查（`existing.is_none()`）：桌面记录已在的那次是幂等
        // 更新（刷新展示字段），不能被挡住 —— 那正是「两条都在」的历史数据
        // 还能继续刷新的唯一路径。userId 为空时（登录态里读不到）也无从比对，
        // 直接放行。
        if existing.is_none() && !user_id.is_empty() {
            let siblings = self.with_conn(&_guard, |conn| sql::load_by_provider(conn, autoclaw))?;
            let duplicate = siblings
                .iter()
                .find(|record| record.id() != id && record.user_id() == user_id);
            if let Some(duplicate) = duplicate {
                return Err(AccountStoreError::new(
                    format!(
                        "账号已存在：同一账号（userId {user_id}）已在列表中（{}「{}」）。\
                         如需改用桌面端实时登录态，请先删除那条账号再导入",
                        duplicate.name(),
                        duplicate.id()
                    ),
                    409,
                ));
            }
        }
        // 默认名用**邮箱**：它才认得出来是谁的号（国际版的 OAuth 账号尤其如此，
        // token 里没有邮箱、上游 user_name 又可能只是昵称）。读不到邮箱时退回
        // 原来的「桌面端登录账号（userId）」。
        let default_name = if !email.is_empty() {
            email.clone()
        } else if user_id.is_empty() {
            "桌面端登录账号".to_string()
        } else {
            format!("桌面端登录账号（{user_id}）")
        };
        // 备注名沿用既有记录 —— 但名字**还是自动生成的那一版**时跟着更新成新的
        // 默认名（老版本的默认名是「桌面端登录账号（userId）」，现在默认名是邮箱；
        // 客户端换了登录账号时记录名也要跟着走，见 `is_generated_desktop_name`）。
        // 用户自己改过的备注名一律保留，与另外几家的 `add_*` 同一纪律。
        let record_name = existing
            .as_ref()
            .map(StoredAccount::name)
            .filter(|value| !value.is_empty())
            .filter(|name| !is_generated_desktop_name(name, &user_id))
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
        record.insert("provider".to_string(), Value::String(autoclaw.to_string()));
        record.insert("name".to_string(), Value::String(record_name.clone()));
        record.insert("userId".to_string(), Value::String(user_id));
        // 邮箱（auth.json 的 `userInfo.email`，明文）：空串不入库 —— 界面按
        // 「有 email 才渲染副标题」判断，存空串只会让那条判断多一次 trim
        if !email.is_empty() {
            record.insert("email".to_string(), Value::String(email.clone()));
        }
        for (target, key) in [
            ("deviceId", "deviceId"),
            ("tokenTail", "tokenTail"),
            ("expiresAt", "tokenExpiresAt"),
            ("desktopSource", "source"),
        ] {
            if let Some(value) = summary.get(key).filter(|value| !value.is_null()) {
                record.insert(target.to_string(), value.clone());
            }
        }
        record.insert("desktop".to_string(), Value::Bool(true));
        record.insert("source".to_string(), Value::String(source.to_string()));
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
                // 桌面端账号的凭证**不落盘**（实时读 auth.json + 解密）：
                // 老记录里若有 token 残留，这里跳过（与另外三家同一纪律）
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
                "{} AutoClaw 桌面端登录态{}: {record_name}",
                if is_new { "✅" } else { "🔄" },
                if is_new { "已导入" } else { "已刷新" },
            ),
        );
        Ok(self.to_autoclaw_public_account(&saved))
    }

    /// 补写账号邮箱（只在记录里**还没有**邮箱时写）—— 返回是否真的写了。
    ///
    /// ── 为什么要有它 ────────────────────────────────────────────
    /// 邮箱是展示字段（界面拿它做账号副标题），而它只能**带着 token 向上游查**
    /// （`providers::autoclaw::profile`）。建账号时能查的两条链（网页登录换码后、
    /// 桌面端导入读本地文件）已经在创建时就写进去了；**此前建的**账号 ——
    /// 网页登录之前建的、手填凭证建的、老版本建的 —— 记录里没有邮箱，
    /// 只能等下一次带着凭证的动作顺手补上：余额查询是最自然的那一处
    /// （它每 10 分钟对每个账号跑一次，且本来就要求凭证可用）。
    ///
    /// ── 三条纪律 ────────────────────────────────────────────────
    ///   1. **只补空**：已有邮箱就不写 —— 不做无谓写盘，也不覆盖任何值；
    ///   2. **只改这一个字段**：读-改-写在账号锁内一次完成，其余字段原样带过
    ///      （`put` 是整行替换，少带一个字段就是一次静默的数据丢失）；
    ///   3. **调用方按 best-effort 处理**：写不进去只是少一行副标题，
    ///      不该让一次余额查询变红（见 `balance::query_usage` 的调用点）。
    pub fn set_autoclaw_account_email(
        &self,
        id: &str,
        email: &str,
    ) -> Result<bool, AccountStoreError> {
        let email = truncate_chars(email.trim(), MAX_IDENTITY_LENGTH);
        if email.is_empty() {
            return Ok(false);
        }
        let _guard = self.guard();
        let Some(mut record) = self.record_by_id(&_guard, id) else {
            return Ok(false);
        };
        // 只认 AutoClaw 系（这个字段是它家的，别家记录不该被它写）
        if !is_autoclaw_family(&record.provider()) {
            return Ok(false);
        }
        let has_email = record
            .get("email")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.trim().is_empty());
        if has_email {
            return Ok(false);
        }
        record
            .fields_mut()
            .insert("email".to_string(), Value::String(email));
        record
            .fields_mut()
            .insert("updatedAt".to_string(), Value::from(logging::now_ms()));
        self.with_conn(&_guard, |conn| sql::put(conn, &record))?;
        Ok(true)
    }

    /// AutoClaw 账号刷新成功后回写新 token（适配器的 `persist_refresh` 调用）。
    ///
    /// ── 比较-再写（本次修复）──────────────────────────────────
    /// 只有当记录里**此刻的** accessToken / refreshToken 仍等于刷新前那份快照时
    /// 才写入；否则返回 [`CredentialWrite::Stale`]。比较与写入在**同一把账号锁内**
    /// 完成（同一个 `_guard` 下 load → 比较 → 改 → save），不存在「检查完释放锁、
    /// 写入时已被别的写者换掉」的窗口。
    ///
    /// 为什么需要它：刷新是网络动作（秒级），期间用户可能重导入账号、换号，
    /// 或另一轮刷新先落地。旧实现无条件覆盖，会把**旧凭证**盖到新凭证上。
    ///
    /// 同时同步 `userId`（新 token 的 JWT 声明是权威的）与 `deviceId`
    /// （刷新响应不会改设备，但 token 里可能带新的 `device_id` 声明），
    /// 并在同一次写入里完成 —— 不再有「先更新 token、再单独加锁补字段」的
    /// 第二次写入。桌面端账号仍然拒绝（它的凭证在 auth.json）。
    pub fn update_autoclaw_account_tokens_if_current(
        &self,
        region: Region,
        id: &str,
        expected_access_token: &str,
        expected_refresh_token: &str,
        access_token: &str,
        refresh_token: &str,
        expires_at: Option<f64>,
        device_id: &str,
    ) -> Result<CredentialWrite, String> {
        let _guard = self.guard();
        // 「比较-再写」只涉及这一行：读它、比它、原地更新它（与 raccoon 同一改造）
        let Some(mut record) = self.record_by_id(&_guard, id) else {
            // 账号已被删除（或 id 被换掉）：刷新结果无处可写，也不该新建记录
            return Ok(CredentialWrite::Stale);
        };
        if record.is_desktop() {
            return Err(
                "桌面端账号的凭证不落盘（实时读 auth.json 并解密），无需回写".to_string(),
            );
        }
        if record.provider() != autoclaw_id_for(region) {
            return Err(format!(
                "账号 {id} 不是 AutoClaw {}账号",
                region.label()
            ));
        }
        // 比较：记录里此刻的凭证必须仍是刷新前那份
        if record.access_token() != expected_access_token
            || record.refresh_token() != expected_refresh_token
        {
            return Ok(CredentialWrite::Stale);
        }
        if !access_token.is_empty() {
            record.set("accessToken", Value::String(access_token.to_string()));
            record.set(
                "tokenTail",
                Value::String(token_tail_of(access_token)),
            );
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
        if let Some(claims) = crypto::decode_jwt_claims(access_token) {
            // `user_id` 声明可能是数字（JWT 不强制字符串），按 JS `String(x)` 取
            let user_id = match claims.get("user_id") {
                Some(Value::String(text)) => text.clone(),
                Some(Value::Number(number)) => number.to_string(),
                _ => String::new(),
            };
            if !user_id.is_empty() {
                record.set(
                    "userId",
                    Value::String(truncate_chars(&user_id, MAX_IDENTITY_LENGTH)),
                );
            }
        }
        if !device_id.trim().is_empty() {
            record.set(
                "deviceId",
                Value::String(truncate_chars(device_id.trim(), MAX_IDENTITY_LENGTH)),
            );
        }
        record.set_updated_at(logging::now_ms());
        self.with_conn(&_guard, |conn| sql::update_in_place(conn, &record))
            .map_err(|error| error.message)?;
        Ok(CredentialWrite::Written)
    }

    // ─── 删除保护 ────────────────────────────────────────────
    //
    // 这里曾有 `is_removal_protected_autoclaw`：AutoClaw 桌面端实时登录态账号
    // （`autoclaw-desktop`）被判定为不可删除，由 `protected_from_removal` 汇总。
    // **现已去掉**，与另外两家（小浣熊 / CatPaw）同一处理，理由也相同：
    //
    //   桌面端账号是「导入桌面端登录态」建出来的**一条账号记录**，记录里不落
    //   token（凭证每次实时读 `%APPDATA%/AutoClaw/auth.json` 并走 DPAPI +
    //   AES-GCM 解密），语义是「我要用这个客户端当前的登录态」—— 用户可能想
    //   撤销这个选择，而旧实现只能禁用，禁用后这条记录仍占着列表与优先级序号。
    //   删除只作用于这条记录：客户端的 auth.json 我们从不写、也不会删，所以
    //   安全且可逆（再点一次「导入桌面端登录态」即可加回来）；想临时停用仍有
    //   「禁用」这个更轻的动作。
    //
    // 删除后的收尾不走 CatPaw 那条会话作废路径（AutoClaw 没有会话注册表），
    // `remove_account` 的 `invalidate_catpaw_sessions` 按 provider 判定，
    // 非 catpaw 直接返回 0，无副作用。

    // ─── 公开形态（AutoClaw）──────────────────────────────────

    /// AutoClaw 账号的公开形态（架构文档 §5 与 §10.2；对照原项目 `toPublicAccount`）。
    ///
    /// 字段：`id` / `provider` / `name` / `userId` / `email` / `deviceId` /
    /// `tokenTail` / `tokenExpiresAt` / `hasRefreshToken` / `desktop` / `source` /
    /// `priority` / `enabled` / `addedAt` / `updatedAt` / `proxy` / `rateLimits` /
    /// `available`。
    ///
    /// `email` 是**展示用**字段（界面拿它当副标题，见 `providers::autoclaw::profile`）：
    /// 桌面端记录实时取 auth.json 的 `userInfo.email`，其余取记录里存的那份；
    /// 两处都读不到时**不写这个键**（而不是写空串）。
    ///
    /// `deviceId` 是 **AutoClaw 特有字段**（刷新接口的 `device_id` 与上游的设备
    /// 维度都靠它），因此公开形态带出 —— 前端展示与排障都要能看见它。
    /// **不显示积分/余额字段**（原项目 `account-balance.mjs` 那套明确不迁移，
    /// 与 CatPaw 的 `balanceCookie` 同一取舍：网关不消费余额）。
    ///
    /// ── 桌面端账号按**实时值**展示 ─────────────────────────────
    /// 这类账号记录里按设计没有 token，`tokenTail` / 过期时间 / `hasRefreshToken`
    /// 若只读记录会永远是空的，界面看起来就是「没有凭证」。因此有桌面端登录态时
    /// 优先用实时摘要（`credentials::local_summary`）；读不到（客户端退出登录、
    /// 文件被删、本机不是 Windows 且没有 openclaw.json）时回落记录里的存量值 ——
    /// 于是「登录态消失」表现为字段变空 + `available: false`，而不是整条记录消失。
    pub fn to_autoclaw_public_account(&self, record: &StoredAccount) -> Value {
        let proxy = crate::server::core::proxies::describe_account_proxy(Some(
            &record.proxy(),
        ));
        let stored_tail = record
            .get("tokenTail")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let stored_expires = record.expires_at().or_else(|| record.token_expires_at());
        // 邮箱：桌面端记录优先用**实时**摘要里的（auth.json 的 `userInfo.email`，
        // 客户端换号后跟着变），否则用记录里存的那一份（网页登录 / 手填凭证
        // 建的账号在创建时或余额查询时补写过，见 `set_autoclaw_account_email`）
        let mut email = record
            .get("email")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let mut user_id = record.user_id();
        let mut device_id = record
            .get("deviceId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let mut available = true;
        let mut reason = String::new();
        // 桌面端实时登录态两个地区都有（那个文件没有地区标记、两地共用，见
        // `providers::autoclaw::credentials::local_credentials`）：只要这条记录
        // 是 AutoClaw 系的桌面账号，就按它自己的 provider 去读实时摘要。
        let (token_tail, expires_at, has_refresh) = if record.is_desktop()
            && is_autoclaw_family(&record.provider())
        {
            // 地区取记录自己的 provider：桌面端登录态文件两地共用，
            // 实时摘要必须按这条记录所属的那一家去解析（否则国际版记录会拿到
            // 国内版域名的凭证 —— 见 credentials.rs 的缓存键说明）
            let record_region = Region::from_provider_id(&record.provider()).unwrap_or(Region::Cn);
            match credentials::local_summary(record_region) {
                Ok(summary) => {
                    let live_user = summary
                        .get("userId")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    if !live_user.is_empty() {
                        user_id = live_user;
                    }
                    let live_device = summary
                        .get("deviceId")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    if !live_device.is_empty() {
                        device_id = live_device;
                    }
                    let live_tail = summary
                        .get("tokenTail")
                        .and_then(Value::as_str)
                        .filter(|value| !value.is_empty())
                        .map(str::to_string)
                        .unwrap_or_else(|| stored_tail.clone());
                    let live_expires = summary
                        .get("tokenExpiresAt")
                        .and_then(Value::as_f64)
                        .filter(|value| *value > 0.0)
                        .or(stored_expires);
                    let can_refresh = summary
                        .get("hasRefreshToken")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    if let Some(live_email) = summary
                        .get("email")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                    {
                        email = live_email.to_string();
                    }
                    (live_tail, live_expires, can_refresh)
                }
                Err(reason_text) => {
                    // 读不到实时登录态：字段回落记录里的存量值，可用性如实报 false
                    available = false;
                    reason = reason_text;
                    (
                        stored_tail,
                        stored_expires,
                        !record.refresh_token().is_empty(),
                    )
                }
            }
        } else {
            (
                stored_tail,
                stored_expires,
                !record.refresh_token().is_empty(),
            )
        };
        let mut public = Map::new();
        public.insert("id".to_string(), Value::String(record.id().to_string()));
        public.insert("provider".to_string(), Value::String(record.provider()));
        public.insert("name".to_string(), Value::String(record.name()));
        public.insert("userId".to_string(), Value::String(user_id));
        // 邮箱：界面拿它做账号的副标题（国际版的 OAuth 账号靠它认人）。
        // 空串**不写这个键**：读不到邮箱的账号（国内版的手机号账号等）界面上
        // 就没有那一行，而不是显示一个空行
        if !email.is_empty() {
            public.insert("email".to_string(), Value::String(email));
        }
        public.insert("deviceId".to_string(), Value::String(device_id));
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
        // 限额冷却标记：选路层从公开形态读它（与另外三家同口径）。
        // AutoClaw 的 429 会写这条标记（适配器按 HTTP 状态码判 QuotaLimited），
        // 少了这个键，429 之后的**下一次请求**仍会选中同一个账号。
        public.insert(
            "rateLimits".to_string(),
            record
                .get("rateLimits")
                .cloned()
                .unwrap_or_else(|| Value::Object(Map::new())),
        );
        public.insert("available".to_string(), Value::Bool(available));
        if !reason.is_empty() {
            public.insert("reason".to_string(), Value::String(reason));
        }
        // 单账号并发上限（所有家通用，兜底共用 `max_concurrent_public`）：
        // 0 = 不限，缺键同样输出 0
        public.insert(
            "maxConcurrent".to_string(),
            Value::from(max_concurrent_public(record.get("maxConcurrent"))),
        );
        Value::Object(public)
    }
}
