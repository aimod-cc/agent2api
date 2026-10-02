//! OrcaRouter 账号：**两个认证入口共用一个落账号入口**（本集成的核心不变量）。
//!
//! ── 为什么这一家只需要一个 `add_*` ────────────────────────────
//! 其余各家的两类入口往往对应两份不同的凭证形态（例如 Cline 的设备授权 token
//! 与手填 token、Trae 的登录态与粘贴凭据），因此各写一个 `add_*_account`。
//! OrcaRouter 不是：手填的 `sk-orca-…` 与 PKCE 换回来的 Key **是同一件东西**
//! （都是一把属于用户账号的长期 API Key，见 `providers::orcarouter` 的模块头）。
//! 于是两条入口在这里收敛成同一个函数 —— 转发、目录、模型下拉因此**无从分辨
//! 凭据来源**，这是规范要求的「两种入口最终只向下游产生同一种普通 API Key」。
//!
//! ── 记录形状 ──────────────────────────────────────────────
//! ```json
//! {
//!   "id": "orcarouter-8f2c1d4a6b90",  // key 的 SHA-256 前 12 位（同 key 合并）
//!   "provider": "orcarouter",
//!   "name": "OrcaRouter 账号 abcd",
//!   "apiKey": "sk-orca-…",            // 唯一凭证来源（转发读它、目录读它）
//!   "tokenTail": "…abcd",             // 界面展示尾号（绝不展示全量）
//!   "source": "manual" | "oauth",     // 凭据从哪条入口来（只影响来源标签）
//!   "userId": "12345",                // 换码响应带回（有则存，仅展示）
//!   "scope": "api",                   // 换码响应**实际授予**的范围
//!   "priority": 5,                    // 全局一条队列（与各家共用号段）
//!   "enabled": true,
//!   "addedAt": 1730000000000,
//!   "updatedAt": 1730000000000,
//!   "needsReauth": false,             // 401 后置位（见 `mark_orcarouter_needs_reauth`）
//!   "reauthGeneration": "…"           // 置位时的凭证指纹（防止旧失败污染新凭据）
//! }
//! ```
//!
//! ── 为什么 `needsReauth` 要带一个 generation ──────────────────
//! 「401 标记」与「重新登录成功」是两个并发的动作：一次**旧的**异步 401 可能在
//! 用户刚刚重登成功之后才落地，把刚换上的新凭据误标成需要重新授权。因此置位时
//! 带上**当时那把 key 的指纹**（`credentials::fingerprint`），清除时也只在指纹
//! 一致时清除 —— 新凭据永远不被旧失败污染（规范明确要求这一条）。
//!
//! ── 硬约束 ────────────────────────────────────────────────
//! 本文件全是「读-改-写」文件操作（**没有任何网络请求**，持锁不做网络）；
//! 绝不 unwrap/expect（release 是 panic=abort）。

use serde_json::{Map, Value};

use crate::server::core::account_store::priority::next_free_priority;
use crate::server::core::account_store::sql;
use crate::server::core::account_store::state::StoredAccount;
use crate::server::core::account_store::store::{AccountStore, AccountStoreError};
use crate::server::core::proxies::{resolve_account_proxy, ResolvedProxy};
use crate::server::logging;

use crate::server::core::providers::orcarouter::credentials::{self, Credentials};

/// 账号 id 前缀（与 `custom-acct-` / `qoder-` 同档：**账号** id，不是 provider id）。
pub const ID_PREFIX: &str = credentials::ID_PREFIX;

/// OrcaRouter 的 provider id（从注册表推导，别处不再写这个字面量）。
pub const ORCAROUTER_PROVIDER_ID: &str = crate::server::core::providers::kind_id(
    crate::server::core::providers::ProviderKind::OrcaRouter,
);

/// `needsReauth` 字段名（公开形态与内部判定共用，只在这里写一次）。
pub const NEEDS_REAUTH_FIELD: &str = "needsReauth";

/// 置位 `needsReauth` 时记录的那把 key 的指纹（见模块头「为什么带 generation」）。
pub const REAUTH_GENERATION_FIELD: &str = "reauthGeneration";

/// 一条 OrcaRouter 账号的**内部凭证快照**（转发 / 目录用；含明文 Key）。
///
/// 与公开形态（`to_orcarouter_public_account`）刻意是**两个类型**：公开形态只有
/// `tokenTail`，这条是网关自身出网要用的读数。两条管道共用一个形状是泄露密钥的
/// 最快路径，所以这里显式分开。
pub struct OrcaRouterCredential {
    /// 账号 id
    pub account_id: String,
    /// 备注名（日志用）
    pub account_name: String,
    /// API Key 明文（**绝不进任何 HTTP 响应 / 日志 / 错误**）
    pub api_key: String,
    /// 出网代理（None = 未配置 / 解析失败 → 直连）
    pub proxy: Option<ResolvedProxy>,
}

impl AccountStore {
    // ─── 写：落账号（两个入口共用）────────────────────────────

    /// 添加/更新一个 OrcaRouter 账号。
    ///
    /// `credentials` 由两条入口之一产出（[`Credentials::from_manual`] /
    /// [`Credentials::from_exchange`]），`source` 只影响记录的来源标签。
    /// `name` 为 `None` 时沿用记录里原有的备注名（或由尾号派生兜底名）。
    ///
    /// 幂等口径与自定义账号一致：`account_id()` 由 Key 派生，因此**同一把 Key
    /// 重复添加 = 更新既有记录**（备注名 / userId / scope / source 随之更新，
    /// 优先级与启用状态沿用）。撞到**其它 provider** 的账号 id（hash 空间巧合）
    /// 时报 409 而不是覆写 —— 覆写会把那条记录的凭证一起弄丢。
    pub fn add_orcarouter_account(
        &self,
        credentials: &Credentials,
        name: Option<&str>,
        source: &str,
    ) -> Result<Value, AccountStoreError> {
        let _guard = self.guard();
        let id = credentials.account_id();
        let existing = self.record_by_id(&_guard, &id);
        if let Some(existing) = existing.as_ref() {
            if existing.provider() != crate::server::core::providers::kind_id(
                crate::server::core::providers::ProviderKind::OrcaRouter,
            ) {
                return Err(AccountStoreError::new(
                    format!(
                        "账号 id「{id}」已被{}账号占用，请先处理那条记录",
                        existing.provider()
                    ),
                    409,
                ));
            }
        }
        // 备注名兜底链：显式传入 → 记录里原有的 → 由尾号派生的默认名。
        // 重新登录时响应可能不带名字，此时**不能**把用户改过的备注名洗掉。
        let explicit_name = name
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| value.chars().take(credentials::MAX_NAME_CHARS).collect::<String>());
        let record_name = explicit_name
            .or_else(|| {
                existing
                    .as_ref()
                    .map(StoredAccount::name)
                    .filter(|value| !value.is_empty())
            })
            .unwrap_or_else(|| credentials.default_name());
        // 传入的凭据自己也带一个 name（手填路径从请求体里取的），优先级最低 ——
        // 它是「这次请求里附带的」，不如记录里已存的备注名可靠
        let record_name = if record_name.trim().is_empty() {
            credentials.default_name()
        } else {
            record_name
        };

        let priority = existing
            .as_ref()
            .map(StoredAccount::priority)
            .unwrap_or_else(|| {
                // 号段取全部账号：优先级全局唯一（所有家共用一条队列）
                let used = self
                    .with_conn(&_guard, |conn| sql::priorities_all(conn))
                    .unwrap_or_default();
                next_free_priority(&used)
            });

        let now = logging::now_ms();
        // 从既有字段表出发（不变量「未知字段全量保留」）；新建时是空表
        let mut fields: Map<String, Value> = existing
            .as_ref()
            .map(|item| item.fields().clone())
            .unwrap_or_default();
        fields.insert("id".to_string(), Value::String(id));
        fields.insert("name".to_string(), Value::String(record_name.clone()));
        // 凭据字段（apiKey / tokenTail / source / userId / scope / provider）
        credentials.apply_to_fields(&mut fields, crate::server::core::providers::kind_id(
            crate::server::core::providers::ProviderKind::OrcaRouter,
        ));
        // `source` 以调用方给的为准：凭据自带的来源与「这次是哪条入口」在
        // 正常链路上一致，出现分歧时以调用方为准（它更接近事实）
        fields.insert(
            "source".to_string(),
            Value::String(if source.trim().is_empty() {
                credentials.source.clone()
            } else {
                source.trim().to_string()
            }),
        );
        fields.insert("priority".to_string(), Value::from(priority));
        fields.insert(
            "enabled".to_string(),
            Value::Bool(existing.as_ref().map(StoredAccount::enabled).unwrap_or(true)),
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

        let mut saved = StoredAccount::from_map(fields);
        // 一条**新**凭据落地 = 这条记录的重新授权已完成：清掉旧标记
        //（generation 也一起清，见 `mark_orcarouter_needs_reauth` 的说明）
        self.clear_needs_reauth_locked(&mut saved);
        self.with_conn(&_guard, |conn| sql::put(conn, &saved))?;
        credentials::log_saved(
            source,
            &credentials.token_tail(),
            existing.is_some(),
        );
        Ok(self.public_account(&saved))
    }

    // ─── 读：内部凭证快照（转发 / 目录 / 模型下拉用）────────────

    /// 取一条 OrcaRouter 账号的转发凭证。
    ///
    /// `account_id` 为空串时取该家**队首的可用账号**（优先级最小、启用且有 apiKey）
    /// —— 与转发选路的默认口径一致（见 `providers::adapter` 的 `refresh_models`
    /// 契约：空串 = 该家的默认选取）。点名了 id 但取不到时返回 `Ok(None)`，
    /// **不回落队首** —— 那会变成「选了 A、用的是 B」的静默错误。
    ///
    /// 代理在锁内只做**解析**（读 Clash 快照，本地文件 IO；与
    /// `custom_credential_by_id` 同一口径），真正的网络动作都在锁外。
    pub fn orcarouter_api_key(
        &self,
        account_id: &str,
    ) -> Result<Option<OrcaRouterCredential>, String> {
        let _guard = self.guard();
        let record = if account_id.trim().is_empty() {
            self.records_for_provider(&_guard, ORCAROUTER_PROVIDER_ID)
                .into_iter()
                .filter(|record| record.enabled() && record.has_api_key())
                .min_by_key(StoredAccount::order_key)
        } else {
            let found = self.record_by_id(&_guard, account_id.trim());
            match found {
                Some(record) if record.has_api_key() => Some(record),
                Some(_) => {
                    return Err("该 OrcaRouter 账号没有可用的 API Key（请重新连接或粘贴新 Key）"
                        .to_string())
                }
                None => return Err("OrcaRouter 账号不存在或已被删除".to_string()),
            }
        };
        Ok(record.map(|record| credential_of_record(&record)))
    }

    // ─── 401 的精确账号标记（terminal reauthentication）──────────

    /// 清除「需要重新授权」标记。
    ///
    /// 调用点只有一处：[`AccountStore::add_orcarouter_account`] 在一条**新凭据**
    /// 落地后主动清（那正是「用户已经重新连上了」的定义）。这里不对外开放 ——
    /// 「重登成功」这件事只有落账号那一步知道，多一个入口只会多一种把标记清早了
    /// 的路径（标记清早了，界面就不再提示重连，而凭据其实还是死的）。
    fn clear_needs_reauth_locked(&self, record: &mut StoredAccount) {
        if !record.needs_reauth() {
            return;
        }
        record.remove(NEEDS_REAUTH_FIELD);
        record.remove(REAUTH_GENERATION_FIELD);
    }

    /// 把**这一条账号的这一代凭据**标记成「需要重新授权」。
    ///
    /// ── 为什么必须精确到「账号 + 凭证代」─────────────────────
    /// 一次 401 只说明**被拒的那把 Key** 不能用了（可能已在控制台吊销），
    /// 不能推广成「这台机器上的 OrcaRouter 全都要重登」——其它账号的 Key 可能
    /// 完全正常。因此这里按 `account_id` 定位记录，并且记下**当时那把 key 的
    /// 指纹**（`reauthGeneration`）：用户重新登录后凭据代改变，那条旧的标记
    /// 就不再适用（见 `add_orcarouter_account` 的清标记一步）。
    ///
    /// 账号 id 本身由 Key 派生（`credentials::Credentials::account_id`），所以
    /// 「换了一把新 Key」= 一条新记录，旧记录上的 401 天然污染不到新凭据 ——
    /// 这是本家两道防污染机制里的第一道（第二道就是这里的指纹）。
    ///
    /// 调用点：适配器的 [`super::adapter::OrcaRouterAdapter::refresh_access_token`]
    /// —— 它**只**在编排层已经收到 401 之后被调用，因此「调用 = 这次请求用的
    /// 那把 Key 被拒了」是准确的事实。
    ///
    /// 返回 `true` = 本次真的置位了（调用方可据此打一条日志）。
    pub fn mark_orcarouter_needs_reauth(&self, account_id: &str) -> Result<bool, AccountStoreError> {
        let account_id = account_id.trim();
        if account_id.is_empty() {
            // 编排层拿不到账号 id 时**不猜**：宁可什么都不标，也不能把一条
            // 别人的账号标记成需要重登
            return Ok(false);
        }
        let _guard = self.guard();
        let Some(mut record) = self.record_by_id(&_guard, account_id) else {
            return Ok(false);
        };
        // 只有本家的记录才置位（别家的 401 有自己的处置）
        if record.provider() != ORCAROUTER_PROVIDER_ID {
            return Ok(false);
        }
        let fingerprint = credentials::fingerprint(&record.api_key());
        record.set(NEEDS_REAUTH_FIELD, Value::Bool(true));
        record.set(
            REAUTH_GENERATION_FIELD,
            Value::String(fingerprint),
        );
        record.set_updated_at(logging::now_ms());
        self.with_conn(&_guard, |conn| sql::put(conn, &record))?;
        Ok(true)
    }
}

/// 一条记录 → 内部凭证快照（唯一取数口径，两个入口共用）。
fn credential_of_record(record: &StoredAccount) -> OrcaRouterCredential {
    let proxy = match resolve_account_proxy(Some(&record.proxy())) {
        Some(resolution) => resolution.resolved().cloned(),
        None => None,
    };
    OrcaRouterCredential {
        account_id: record.id().to_string(),
        account_name: record.name(),
        api_key: record.api_key(),
        proxy,
    }
}
