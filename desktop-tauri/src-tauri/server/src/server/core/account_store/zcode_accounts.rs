//! ZCode 账号以「地区 + userId」识别；凭证（访问令牌 + 套餐 JWT + 设备标识）
//! 与其它提供商共享存储键名。
//!
//! ── 地区为什么不落进记录 JSON ────────────────────────────────
//! 本家的地区**已经编码在 provider id 里**（`zcode` / `zcode-intl`），
//! 而记录自带 `provider` 字段 —— 因此地区可由 `Region::from_provider_id`
//! 直接还原，不需要再存一份 region 字段（存了就有两处事实来源，漂移时
//! 「provider 说是国内版、字段说是国际版」会静默打到错误的推理域名）。
//! 这与 Qoder / AutoClaw 那两家不同：它们的两个地区**共用**一套 provider id
//! 家族的判定方式不同，Qoder 把 region 存进 JSON 是因为它的 provider id
//! 只有一个（`qoder`），地区只能落在记录里。
//!
//! ── 三个凭证字段缺一不可（见 `zcode::credentials` 的模块头）──
//! `accessToken` 转发用、`jwt` 领取用、`deviceMid` 领取时上游要求。
//! 落盘时**空值不覆盖既有内容**（与 Qoder 同一条规则）：重新登录只拿到
//! 访问令牌时，不该把上次的设备标识洗掉 —— 那会让下一次领取稳定命中 3001。

use serde_json::{json, Map, Value};

use crate::server::core::providers::zcode::credentials::ZcodeCredentials;
use crate::server::core::providers::zcode::region::Region;
use crate::server::core::providers::{kind_id, ProviderKind};
use crate::server::logging;

use super::priority::next_free_priority;
use super::sql;
use super::state::StoredAccount;
use super::store::{AccountStore, AccountStoreError};
use super::store_util::{max_concurrent_public, token_tail_of, truncate_chars};

impl AccountStore {
    /// 显示用的一条 ZCode 账号（`account_id` 为空时取本地区组内优先级最高的那条）
    pub fn zcode_account_record(&self, account_id: &str) -> Option<Value> {
        let guard = self.guard();
        if !account_id.is_empty() {
            let record = self.record_by_id(&guard, account_id)?;
            return (Region::from_provider_id(&record.provider()).is_some())
                .then(|| record.to_value());
        }
        // 不带 id 时按**两家一起**找：调用方（领取任务的「有没有账号可用」判定）
        // 不关心地区，只要有任意一家可用即可
        Region::ALL
            .into_iter()
            .filter_map(|region| {
                self.records_for_provider(&guard, region.provider_id())
                    .into_iter()
                    .filter(|record| record.enabled() && record.has_token())
                    .min_by_key(|record| record.order_key())
            })
            .min_by_key(|record| record.order_key())
            .map(|record| record.to_value())
    }

    /// 落一条 ZCode 账号（登录完成 / 粘贴凭证都走这里）。
    ///
    /// 身份匹配用「地区 + userId」两段：地区由 provider id 体现（只查本家那一组），
    /// userId 逐条比 —— 同一个人的两个地区账号因此是两条记录（这正是两个
    /// provider 建模的目的）。
    ///
    /// ── 空 userId 不去重（这是本函数最容易写错的一处）──────────
    /// 「填凭证」那条路把 userId 标成可选，因此空值是常态。若照直比
    /// `record.user_id() == ""`，**所有**没有 userId 的账号都算同一个人：
    /// 加第二个就把第一个覆盖掉，而用户看到的是「上一个账号凭空消失」
    /// （`account_id()` 对空 userId 也回同一个固定串，撞 id 又恰好落进
    /// 「同标识合并」的语义里，静默得很）。所以这里只在 userId 非空时才
    /// 认为它是同一个账号，空 userId 一律当新账号、id 用随机段
    /// （与 `custom_accounts::account_id_for` 同一处置）。
    pub fn add_zcode_account(
        &self,
        credentials: &ZcodeCredentials,
        name: Option<&str>,
        source: &str,
    ) -> Result<Value, AccountStoreError> {
        let provider_id = credentials.region.provider_id();
        let user_id = credentials.user_id.trim();
        let guard = self.guard();
        let existing = if user_id.is_empty() {
            None
        } else {
            self.records_for_provider(&guard, provider_id)
                .into_iter()
                .find(|record| record.user_id() == user_id)
        };
        let id = match existing.as_ref() {
            Some(record) => record.id().to_string(),
            None if user_id.is_empty() => anonymous_account_id(credentials.region)?,
            None => credentials.account_id(),
        };
        // 新建时确认这个 id 没被任何人（含别家）占用 —— 主键查询，只读一行
        if existing.is_none() && self.record_by_id(&guard, &id).is_some() {
            return Err(AccountStoreError::new(
                "ZCode 账号 ID 已被其它账号占用，请先核对账号记录",
                409,
            ));
        }
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
            .unwrap_or_else(|| credentials.display_name());

        let mut fields = existing
            .as_ref()
            .map(|record| record.fields().clone())
            .unwrap_or_default();
        // 凭证三件套：空值不覆盖既有内容（理由见模块头）
        for (key, value) in [
            ("accessToken", credentials.access_token.trim()),
            // jwt 是领取的必要条件，但登录响应里**可能没有**（例如只走 API Key
            // 的账号），此时不该把上一次的 jwt 洗掉
            ("jwt", credentials.jwt.trim()),
            ("deviceMid", credentials.device_mid.trim()),
            ("userId", credentials.user_id.trim()),
        ] {
            if value.is_empty() {
                continue;
            }
            fields.insert(key.to_string(), Value::String(value.to_string()));
        }

        let priority = match existing.as_ref() {
            Some(record) => record.priority(),
            None => {
                let used = self
                    .with_conn(&guard, |conn| sql::priorities_all(conn))
                    .unwrap_or_default();
                next_free_priority(&used)
            }
        };
        fields.insert("id".to_string(), Value::String(id.clone()));
        fields.insert("provider".to_string(), Value::String(provider_id.to_string()));
        fields.insert(
            "name".to_string(),
            Value::String(truncate_chars(&record_name, 100)),
        );
        fields.insert(
            "tokenTail".to_string(),
            Value::String(token_tail_of(&credentials.access_token)),
        );
        fields.insert("priority".to_string(), Value::from(priority));
        fields.insert(
            "enabled".to_string(),
            Value::Bool(existing.as_ref().map(StoredAccount::enabled).unwrap_or(true)),
        );
        // 本家没有「从桌面端导入登录态」这条路（ZCode 客户端的凭证在它自己的
        // 加密存储里，没有 auth.json 那种稳定可读的形态），因此恒为 false
        fields.insert("desktop".to_string(), Value::Bool(false));
        fields.insert("source".to_string(), Value::String(source.to_string()));
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
        fields.insert("rateLimits".to_string(), json!({}));
        // 有效期（账号页「有效期」列读它，键名见 `ui/accounts-groups.js` 给本家
        // 登记的那一行：`expiry: 'expiresAt'`）。解不出就**不写** —— 保留既有值
        // （重新添加时不该把上次的时间洗掉），也从编一个假时间。
        if let Some(expires_at) = crate::server::core::providers::zcode::credentials::expires_at_ms(
            credentials,
        ) {
            fields.insert("expiresAt".to_string(), Value::from(expires_at));
        }
        // 清掉别家形状的遗留键（同一条记录被换家复用时才会存在）
        for key in ["edition", "endpoint", "prefixPath", "platform", "access", "refresh", "expires", "pat"] {
            fields.remove(key);
        }
        let record = StoredAccount::from_map(fields);
        self.with_conn(&guard, |conn| sql::put(conn, &record))?;
        logging::log(
            "[Accounts]",
            &format!(
                "✅ ZCode {}账号已保存（优先级 {priority}）",
                credentials.region.label()
            ),
        );
        Ok(self.public_account(&record))
    }

    /// ZCode 账号的公开形态（界面读它）。
    ///
    /// 与其余各家**同字段名、同语义**，前端不必按 provider 查表：
    /// `edition` 放地区标识、`editionLabel` 放中文标签，于是界面上
    /// 「ZCode 国内版 / 国际版」的显示复用既有那一列。
    pub fn to_zcode_public_account(&self, record: &StoredAccount) -> Value {
        let region = Region::from_provider_id(&record.provider());
        let available = record.has_token() && region.is_some();
        let jwt_present = record
            .get("jwt")
            .and_then(Value::as_str)
            .map(str::trim)
            .is_some_and(|value| !value.is_empty());
        let mut public = Map::new();
        for key in ["id", "provider", "name", "userId", "source", "tokenTail"] {
            public.insert(
                key.to_string(),
                record.get(key).cloned().unwrap_or(Value::Null),
            );
        }
        public.insert(
            "edition".to_string(),
            region
                .map(|value| Value::String(value.provider_id().to_string()))
                .unwrap_or(Value::Null),
        );
        public.insert(
            "editionLabel".to_string(),
            region
                .map(|value| Value::String(value.label().to_string()))
                .unwrap_or(Value::Null),
        );
        // 本家没有续期协议（见 `zcode::adapter` 的模块头），因此恒 false ——
        // 前端据此不给这个账号显示「续期」类按钮，而不是点了才报错
        public.insert("hasRefreshToken".to_string(), Value::Bool(false));
        // 「能不能领取套餐」是**跨地区同语义**的能力位：没有 jwt 时界面上
        // 的领取按钮应当不可点，而不是点了才报「缺少登录态」
        public.insert("canClaim".to_string(), Value::Bool(jwt_present));
        public.insert("priority".to_string(), Value::from(record.priority()));
        public.insert("enabled".to_string(), Value::Bool(record.enabled()));
        public.insert("addedAt".to_string(), Value::from(record.added_at()));
        public.insert("updatedAt".to_string(), Value::from(record.updated_at()));
        public.insert(
            "proxy".to_string(),
            crate::server::core::proxies::describe_account_proxy(Some(&record.proxy())),
        );
        public.insert(
            "rateLimits".to_string(),
            record.get("rateLimits").cloned().unwrap_or_else(|| json!({})),
        );
        public.insert("desktop".to_string(), Value::Bool(false));
        public.insert("available".to_string(), Value::Bool(available));
        public.insert(
            "maxConcurrent".to_string(),
            Value::from(max_concurrent_public(record.get("maxConcurrent"))),
        );
        // `chatSupported` 不在这里写：它是跨家统一事实，由 `store.rs::public_account`
        // 按适配器的 `supports_chat()` 注入（与 Qoder 那份同样的处置）
        Value::Object(public)
    }
}

/// 本家的 provider id 常量（别处按它判「是不是 ZCode」时用注册表，不写字面量）
pub(crate) const _ZCODE_PROVIDER_ID: &str = kind_id(ProviderKind::Zcode);

/// 没有上游 userId 时的账号 id：`{地区前缀}anon-` + 12 位 hex。
///
/// 这类账号（「填凭证」不填 userId 的那种）没有任何稳定标识可派生，而 id
/// 的唯一性不能打折 —— 用固定串（原先的 `unknown`）会让第二次添加撞上第一
/// 条的 id，进而被当成同一个账号合并掉（理由见 `add_zcode_account`）。
/// 随机源失败时如实报错，不落一条 id 可靠性没保证的记录（与
/// `custom_accounts::account_id_for` 同一口径：宁可让用户重试一次）。
fn anonymous_account_id(region: Region) -> Result<String, AccountStoreError> {
    let mut bytes = [0u8; 6];
    getrandom::getrandom(&mut bytes).map_err(|_| {
        AccountStoreError::new(
            "无法生成安全的随机账号 id（系统随机源不可用），请重试",
            500,
        )
    })?;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!("{}anon-{hex}", region.account_id_prefix()))
}
