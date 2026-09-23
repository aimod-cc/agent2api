//! 自定义提供商账号：手动添加、级联删除、公开形态的字段来源
//! （架构见 `core::custom_providers` 的模块头）。
//!
//! ── 与各家添加路径的关系 ────────────────────────────────────
//! 内置八家的凭证链路各不相同（JWT / token / apiKey / 桌面端登录态），因此
//! 各有自己的 `add_*_account`。自定义提供商最简单：**用户直接填一把上游
//! API Key**（可选带上 baseUrl 覆盖项），网关不代管任何登录态 —— 所以这里
//! 没有刷新、没有导入、没有桌面端实时读取，只有「写一条记录」。
//!
//! ── 记录形状（与内置家对齐的部分 + 本家特有的键）─────────────
//! ```json
//! {
//!   "id": "custom-acct-8f2c1d4a6b90",  // custom-acct- + 12 位 hex
//!   "provider": "custom-3f2a91b04c7e", // 指向自定义提供商（外键字符串）
//!   "name": "我的网关 A 账号",
//!   "apiKey": "sk-…",                  // 唯一凭证来源（可空：部分上游不需要）
//!   "baseUrl": "https://…",            // 可选覆盖项；不存在则用提供商的 baseUrl
//!   "tokenTail": "…abcd",              // 界面上展示的尾号（apiKey 派生）
//!   "source": "manual",
//!   "priority": 5,                     // 全局一条队列（与各家共用号段）
//!   "enabled": true,
//!   "addedAt": 1730000000000,
//!   "updatedAt": 1730000000000
//! }
//! ```
//!
//! ── 为什么 id 是派生 + 随机的两段式 ─────────────────────────
//! `apiKey` 非空时用它的 SHA-256 前 12 位：**同一把 key 重复添加会合并更新
//! 同一条记录** —— 与各家「同标识合并」（workbuddy 的 `user-<uid>`、Qoder 的
//! `qoder-<region>-<hash>`）语义一致，用户粘错重试不会堆出重复账号。
//! key 为空时退化为随机 12 位（`getrandom`）：没有稳定标识可派生，但 id 的
//! 唯一性不能打折（并发/重装都要安全，理由同 `custom_providers` 的 id）。
//! 前缀 `custom-acct-` 与 provider id 的 `custom-` **刻意不同形**：
//! `is_custom_provider_id` 按 provider 前缀判定，账号 id 若同形会被误认成
//! 提供商 id（两套 id 空间混淆是难查的静默错配）。
//!
//! ── 硬约束 ────────────────────────────────────────────────
//! 本文件全是「读-改-写」文件操作，**没有任何网络请求**（持锁不做网络）。
//! 绝不 unwrap/expect（release 是 panic=abort）。

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::server::core::account_store::priority::next_free_priority;
use crate::server::core::account_store::sql;
use crate::server::core::account_store::state::StoredAccount;
use crate::server::core::account_store::store::{AccountStore, AccountStoreError};
use crate::server::core::account_store::store_util::{token_tail_of, truncate_chars};
use crate::server::core::custom_providers;
use crate::server::core::proxies::{resolve_account_proxy, ResolvedProxy};
use crate::server::logging;

/// 账号 id 前缀。**与 provider id 的 `custom-` 不同形**（理由见模块头）。
pub const CUSTOM_ACCOUNT_ID_PREFIX: &str = "custom-acct-";

/// 备注名长度上限（与另外几家一致）
const MAX_NAME_CHARS: usize = 100;

/// apiKey 长度上限（防御：粘贴进一整段文档时不落盘）。
/// 上游 key 一般 32~256 字符，8192 与 `MAX_TOKEN_LENGTH` 同量级。
const MAX_API_KEY_LENGTH: usize = 8192;

impl AccountStore {
    // ─── 写：手动添加 ────────────────────────────────────────

    /// 添加/更新一个自定义提供商账号（`POST /api/accounts` 的 custom 分支与
    /// `POST /api/custom-providers` 的「首个账号」共用）。
    ///
    /// `provider_id` 必须**以 `custom-` 开头**（调用方已用
    /// `custom_providers::is_custom_provider_id` 校验过它确实存在；这里只做
    /// 前缀防御 —— 存储层不回头查配置，免得账号层与配置层互相依赖）。
    ///
    /// `payload` 认三个键（其余忽略，与各家「未知键不动」口径一致）：
    ///   - `apiKey`：凭证（可空 —— 部分自建上游不需要 key，此时
    ///     `hasCredentials` 为 false，界面据此提示）；
    ///   - `baseUrl`：**可选覆盖项**（缺省不写这条键，转发阶段回落到提供商的
    ///     baseUrl）。校验与提供商侧同一套（`normalize_base_url`）；
    ///   - `name` 由 `name` 参数决定（调用方已从请求体取好，与各家一致）。
    ///
    /// 幂等口径：`apiKey` 非空时同 key 重复添加 = **更新**既有记录（备注名 /
    /// baseUrl 覆盖项随之更新，优先级与启用状态沿用）；没有撞上则新建。
    /// 撞到**其它 provider** 的账号 id（hash 空间巧合）时报 409 而不是覆写。
    pub fn add_custom_account(
        &self,
        provider_id: &str,
        payload: &Value,
        name: Option<&str>,
    ) -> Result<Value, AccountStoreError> {
        let provider_id = provider_id.trim();
        if !provider_id.starts_with(custom_providers::ID_PREFIX) {
            return Err(AccountStoreError::bad_request(format!(
                "不是自定义提供商 id: {provider_id}"
            )));
        }
        let Some(object) = payload.as_object() else {
            return Err(AccountStoreError::bad_request("账号内容必须是 JSON 对象"));
        };
        // apiKey：trim 后落盘（粘贴时常见首尾空白/换行 —— 原样落盘会让
        // Authorization 头带上换行，请求在网络层就失败，用户看不出为什么）
        let api_key = object
            .get("apiKey")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_string();
        if api_key.chars().count() > MAX_API_KEY_LENGTH {
            return Err(AccountStoreError::bad_request("apiKey 过长"));
        }
        // baseUrl 覆盖项：给了就必须合法（与提供商侧同判据）；**没给不写键** ——
        // 「没有覆盖项」与「覆盖项指到空串」在转发阶段是两种语义，形状上要能区分
        let base_url_override = match object
            .get("baseUrl")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            Some(raw) => Some(
                custom_providers::normalize_base_url(raw)
                    .map_err(AccountStoreError::bad_request)?,
            ),
            None => None,
        };

        let _guard = self.guard();
        let id = account_id_for(&api_key)
            .map_err(|error| AccountStoreError::new(error, 500))?;
        // 撞 id 保护（与 cline 同一考虑）：hash 空间巧合或手改数据都会撞上
        // 别家的记录，覆写会把那条记录的凭证一起弄丢
        let existing = self.record_by_id(&_guard, &id);
        if let Some(existing) = existing.as_ref() {
            if existing.provider() != provider_id {
                return Err(AccountStoreError::new(
                    format!(
                        "账号 id「{id}」已被{}账号占用，请先处理那条记录",
                        existing.provider()
                    ),
                    409,
                ));
            }
        }

        // 备注名兜底链：显式传入 → 记录里原有的 → 「账号 + apiKey 尾号」。
        // 尾号对人有参考价值（哪把 key），空 key 时退化成一个稳定的默认名
        let explicit_name = name
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| truncate_chars(value, MAX_NAME_CHARS));
        let record_name = explicit_name
            .or_else(|| {
                existing
                    .as_ref()
                    .map(StoredAccount::name)
                    .filter(|value| !value.is_empty())
            })
            .unwrap_or_else(|| {
                let tail = token_tail_of(&api_key);
                if tail.is_empty() {
                    "自定义账号".to_string()
                } else {
                    format!("账号 {tail}")
                }
            });

        let priority = existing
            .as_ref()
            .map(StoredAccount::priority)
            .unwrap_or_else(|| {
                // 号段取全部账号：优先级全局唯一（所有家共用一条队列，见
                // `account_store` 模块头）。查询失败（库不可用）回落到默认号段，
                // 与 cline 的兜底一致。
                let used = self
                    .with_conn(&_guard, |conn| sql::priorities_all(conn))
                    .unwrap_or_default();
                next_free_priority(&used)
            });

        let now = logging::now_ms();
        // 从既有记录的字段表出发（不变量「未知字段全量保留」：用户手加的备注、
        // 将来版本新增的字段都不能在这一次重建里丢）—— 新建时是空表
        let mut fields: Map<String, Value> = existing
            .as_ref()
            .map(|item| item.fields().clone())
            .unwrap_or_default();
        fields.insert("id".to_string(), Value::String(id));
        fields.insert(
            "provider".to_string(),
            Value::String(provider_id.to_string()),
        );
        fields.insert("name".to_string(), Value::String(record_name.clone()));
        fields.insert("apiKey".to_string(), Value::String(api_key.clone()));
        match base_url_override {
            Some(base_url) => {
                fields.insert("baseUrl".to_string(), Value::String(base_url));
            }
            // 显式传了空 baseUrl（想清掉覆盖项回落到提供商默认值）也要支持：
            // 请求里**带了这个键**但值为空 → 删掉覆盖项；完全没带 → 保留原值。
            // 这是「清空覆盖」唯一可行的表达方式（空串不是合法 URL，不能落盘）。
            None => {
                if object.contains_key("baseUrl") {
                    fields.remove("baseUrl");
                }
            }
        }
        fields.insert(
            "tokenTail".to_string(),
            Value::String(token_tail_of(&api_key)),
        );
        fields.insert(
            "source".to_string(),
            Value::String(
                existing
                    .as_ref()
                    .map(StoredAccount::source)
                    .filter(|value| !value.is_empty())
                    .unwrap_or_else(|| "manual".to_string()),
            ),
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

        let saved = StoredAccount::from_map(fields);
        // 单行落地：`put` = DELETE + INSERT（更新既有记录时它落到列表末尾，
        // 与旧实现 retain + push 的结果一致，见 `sql::put`）
        self.with_conn(&_guard, |conn| sql::put(conn, &saved))?;
        logging::log(
            "[Accounts]",
            &format!(
                "{} 自定义账号{}: {}（{}）",
                if existing.is_some() { "🔄" } else { "✅" },
                if existing.is_some() { "已更新" } else { "已添加" },
                record_name,
                custom_providers::label_of(provider_id)
                    .unwrap_or_else(|| provider_id.to_string()),
            ),
        );
        Ok(self.public_account(&saved))
    }

    /// 删除某个自定义提供商名下的**全部账号**，返回删除条数。
    ///
    /// 只服务级联删除（`custom_providers::remove` 在删掉提供商之前调用它）：
    /// 逐个删而不是一条批量 SQL —— 账号数量在个位数，而 `sql::delete` 是
    /// 既有单行删除路径（含「不存在返回 false」的判据），复用它可以不新增
    /// SQL、也不引入新的事务语义（本操作不需要跨行原子性：删到一半失败时
    /// 剩下的账号仍在原组里，重试一次即可）。
    pub fn remove_custom_accounts(&self, provider_id: &str) -> Result<usize, AccountStoreError> {
        let provider_id = provider_id.trim();
        let _guard = self.guard();
        let records = self.with_conn(&_guard, |conn| sql::load_by_provider(conn, provider_id))?;
        let mut removed = 0usize;
        for record in &records {
            let deleted = self.with_conn(&_guard, |conn| sql::delete(conn, record.id()))?;
            if deleted {
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// 按账号 id 取它的 provider —— **仅当它属于自定义提供商时**返回 Some。
    ///
    /// 消费方是 `api::accounts::refresh_account` 的分支：自定义账号没有可刷新
    /// 的凭证（key 是用户填的），若不拦截会落进 workbuddy 的刷新链路并得到
    /// 一条误导性的「账号不存在」。判据用前缀而不是 `is_custom_provider_id`：
    /// 这里问的是「这条账号记录是不是自定义形态」，与提供商是否还在无关
    /// （提供商被删后残留的账号同样不该走 workbuddy 刷新）。
    pub fn custom_account_provider(&self, id: &str) -> Option<String> {
        let _guard = self.guard();
        let record = self.record_by_id(&_guard, id)?;
        let provider = record.provider();
        provider
            .starts_with(custom_providers::ID_PREFIX)
            .then_some(provider)
    }

    // ─── 读：内部凭证快照（转发 / 拉取模型清单用）────────────────

    /// 读取一条自定义账号的**转发凭证**（`custom_credential_by_id`）。
    ///
    /// 消费方：`providers::custom::forward`（对话转发，出网要 apiKey 与
    /// baseUrl 覆盖项）。**返回值含 apiKey 明文，绝不进任何 HTTP 响应** ——
    /// 公开形态（`to_custom_public_account`）刻意只有 tokenTail，那是给界面的；
    /// 这条是网关自身出网的内部读数，两条管道不能共用一个形状。
    ///
    /// 代理在锁内只做**解析**（读 Clash 快照，本地文件 IO；与
    /// `get_session_by_id` 同一口径），真正的网络动作都在锁外。
    pub fn custom_credential_by_id(&self, account_id: &str) -> Option<CustomCredential> {
        let _guard = self.guard();
        let record = self.record_by_id(&_guard, account_id)?;
        Some(credential_of_record(&record))
    }

    /// 该自定义提供商下第一个「启用且 apiKey 非空」的账号凭证
    /// （`custom_providers::fetch_upstream_models` 的取数口径）。
    ///
    /// 「第一个」= 优先级最小（`order_key` 与选路同一排序）—— 拉清单是
    /// 管理动作，没有逐账号轮换的语义，用队首账号即可；多个账号各有清单
    /// 可见性的场景现实中不存在（同一家上游的清单对每把 key 一致）。
    /// 一个都没有时报错文案由调用方给（「请先添加账号」），这里以 None 表达。
    pub fn first_custom_credential(&self, provider_id: &str) -> Option<CustomCredential> {
        let _guard = self.guard();
        self.records_for_provider(&_guard, provider_id)
            .into_iter()
            .filter(|record| record.enabled() && record.has_api_key())
            .min_by_key(StoredAccount::order_key)
            .map(|record| credential_of_record(&record))
    }
}

/// 自定义账号的**内部凭证快照**（`custom_credential_by_id` /
/// `first_custom_credential` 的返回值）。
///
/// 与公开形态（`to_custom_public_account`）的字段差别只有两个：
/// `apiKey`（明文，出网鉴权用）与代理的**已解析形态**（`ResolvedProxy`，
/// 直接可交给 egress；解析失败按直连兜底 —— 公开形态里那是给用户看的
/// 「解析失败」气泡，这里是转发可执行的出口）。
pub struct CustomCredential {
    /// 账号 id
    pub account_id: String,
    /// 备注名（日志用）
    pub account_name: String,
    /// 上游 API Key（`add_custom_account` 落盘的凭证；非空）
    pub api_key: String,
    /// 账号上的 baseUrl **覆盖项**（None = 未覆盖，转发回落提供商的 baseUrl）
    pub base_url_override: Option<String>,
    /// 出网代理（None = 未配置 / 解析失败 → 直连）
    pub proxy: Option<ResolvedProxy>,
}

/// 一条记录 → 凭证快照（两个入口共用一份取数口径，不会各漂一份）
fn credential_of_record(record: &StoredAccount) -> CustomCredential {
    let fields = record.fields();
    let proxy = match resolve_account_proxy(Some(&record.proxy())) {
        Some(resolution) => resolution.resolved().cloned(),
        // 无代理配置 → 直连（与「解析失败回退直连」在出网行为上等价，
        // 区别只在日志：这里没有请求上下文可挂 notice，静默直连即可）
        None => None,
    };
    CustomCredential {
        account_id: record.id().to_string(),
        account_name: record.name(),
        api_key: fields
            .get("apiKey")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        base_url_override: fields
            .get("baseUrl")
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|text| !text.is_empty()),
        proxy,
    }
}

/// 账号 id：`custom-acct-` + 12 位 hex。
///
/// apiKey 非空 → SHA-256 前 12 位（同 key 合并，见模块头）；
/// 空 key → 随机 12 位（`getrandom`，理由同 `custom_providers` 的 id：时间戳
/// 在卸载重装 / 并发添加下会撞号）。随机源失败时如实报错（本函数返回 Err）：
/// 本模块硬约束「绝不 panic」，宁可让用户重试一次，也不落一条 id 可靠性
/// 没保证的记录。
fn account_id_for(api_key: &str) -> Result<String, String> {
    if !api_key.is_empty() {
        let digest = Sha256::digest(api_key.as_bytes());
        let hex = format!("{digest:x}");
        let short: String = hex.chars().take(12).collect();
        return Ok(format!("{CUSTOM_ACCOUNT_ID_PREFIX}{short}"));
    }
    let mut bytes = [0u8; 6];
    getrandom::getrandom(&mut bytes)
        .map_err(|_| "无法生成安全的随机账号 id（系统随机源不可用），请重试".to_string())?;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!("{CUSTOM_ACCOUNT_ID_PREFIX}{hex}"))
}
