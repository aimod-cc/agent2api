//! 账号存储句柄：CRUD、当前账号派生、凭证/会话查询、限额标记、启动迁移。
//!
//! 对照 workbuddy-account-store.mjs 的 `createAccountStore` 返回值逐条移植。
//! 两条硬约束写在 `super::mod` 的头部：未知字段全量保留、优先级全局唯一。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::{json, Map, Value};

use crate::server::core::account_store::state::{
    AccountState, CredentialsById, CurrentEntry, SessionById, StoredAccount,
};
use crate::server::core::account_store::store_util::{js_truthy, value_or, value_or_nullish};
use crate::server::core::endpoints::{resolve_edition, EditionInfo};
use crate::server::core::proxies::{describe_account_proxy, resolve_account_proxy, ProxyResolution};

/// 账号存储错误（对应 Node 版 AccountStoreError，带状态码 → 路由层直接用）。
#[derive(Clone, Debug)]
pub struct AccountStoreError {
    pub message: String,
    pub status_code: i32,
}

impl AccountStoreError {
    pub fn new(message: impl Into<String>, status_code: i32) -> Self {
        Self { message: message.into(), status_code }
    }

    pub(crate) fn bad_request(message: impl Into<String>) -> Self {
        Self::new(message, 400)
    }

    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        Self::new(message, 404)
    }
}

impl std::fmt::Display for AccountStoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.message)
    }
}

/// 账号文件名（Node 版 `join(directory, 'accounts.json')`）
const FILE_NAME: &str = "accounts.json";

/// 共享的账号存储。
///
/// 用 `Arc<Inner>` 做轻量句柄：`ServerState` 可径直 clone，各 handler 拿到的是
/// 同一个 store。锁是**一把粗粒度 Mutex** —— Node 版是单线程事件循环，
/// 这里用一把锁换取与它等价的串行语义，锁粒度不做精细拆分。
/// 硬约束：持锁期间绝不做网络请求（见 super::mod 头部说明）。
#[derive(Clone)]
pub struct AccountStore {
    inner: Arc<Inner>,
}

struct Inner {
    file_path: PathBuf,
    directory: PathBuf,
    /// 串行化「读-改-写」整个周期；只保护文件读写与内存构造，不保护网络请求
    lock: Mutex<()>,
}

impl AccountStore {
    /// 构造存储句柄（不读盘；首次访问才落 IO）
    pub fn new(directory: PathBuf) -> Self {
        let file_path = directory.join(FILE_NAME);
        Self {
            inner: Arc::new(Inner { file_path, directory, lock: Mutex::new(()) }),
        }
    }

    /// 使用全局配置目录（`~/.workbuddy-proxy`）
    pub fn with_config_dir() -> Self {
        Self::new(crate::server::config::config_dir())
    }

    /// accounts.json 的完整路径（`/api/session` 的 authFile 字段用它）
    pub fn file(&self) -> &Path {
        &self.inner.file_path
    }

    pub fn file_string(&self) -> String {
        self.inner.file_path.to_string_lossy().to_string()
    }

    /// 文件是否已存在（Node 版账号存储导出的 `existsSync: () => existsSync(filePath)`）。
    /// 当前管理 API 未调用，保留作为该导出的对等物，便于后续「账号文件是否落盘」判定。
    #[allow(dead_code)]
    pub fn exists(&self) -> bool {
        self.inner.file_path.exists()
    }

    /// 取锁；锁中毒（某次持锁 panic）不致命：直接接管内部数据继续用，
    /// 总好过让所有管理 API 永久 500。
    pub(crate) fn guard(&self) -> MutexGuard<'_, ()> {
        match self.inner.lock.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// 读磁盘（缺失/损坏都当空列表，对应 Node 版 load 的 catch 分支）
    pub(crate) fn load(&self, _guard: &MutexGuard<'_, ()>) -> AccountState {
        let Ok(text) = std::fs::read_to_string(&self.inner.file_path) else {
            return AccountState::default();
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            return AccountState::default();
        };
        // 根必须是对象（数组/标量都当损坏，与 Node 的 `Array.isArray(json)` 判断一致）
        if !value.is_object() {
            return AccountState::default();
        }
        let accounts = value
            .get("accounts")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| StoredAccount::from_value(item.clone()))
                    .collect()
            })
            .unwrap_or_default();
        AccountState { accounts }
    }

    /// 写磁盘：缩进 2 空格（与 Node 的 `JSON.stringify(state, null, 2)` 一致），
    /// 目录不存在时自动创建。
    pub(crate) fn save(&self, state: &AccountState, _guard: &MutexGuard<'_, ()>) -> Result<(), AccountStoreError> {
        if let Err(error) = std::fs::create_dir_all(&self.inner.directory) {
            return Err(AccountStoreError::new(
                format!("账号文件保存失败: {error}"),
                500,
            ));
        }
        let payload = json!({
            "accounts": state.accounts.iter().map(StoredAccount::to_value).collect::<Vec<_>>(),
        });
        let text = match serde_json::to_string_pretty(&payload) {
            Ok(text) => text,
            Err(error) => {
                return Err(AccountStoreError::new(
                    format!("账号文件保存失败: {error}"),
                    500,
                ))
            }
        };
        if let Err(error) = std::fs::write(&self.inner.file_path, text) {
            return Err(AccountStoreError::new(
                format!("账号文件保存失败: {error}"),
                500,
            ));
        }
        Ok(())
    }

    /// 供导入导出子模块复用：在已持锁的前提下读盘
    pub fn load_locked(&self, guard: &MutexGuard<'_, ()>) -> AccountState {
        self.load(guard)
    }

    /// 供导入导出子模块复用：在已持锁的前提下写盘
    pub fn save_locked(
        &self,
        state: &AccountState,
        guard: &MutexGuard<'_, ()>,
    ) -> Result<(), AccountStoreError> {
        self.save(state, guard)
    }

    /// 供导入导出子模块复用：加锁后执行一个「读-改-写」周期
    pub fn with_lock<T>(&self, action: impl FnOnce(&MutexGuard<'_, ()>) -> T) -> T {
        let guard = self.guard();
        action(&guard)
    }

    // ─── 派生：当前账号 ──────────────────────────────────────

    /// 当前账号 = 转发顺序里第一个「已启用且有凭证」的账号。
    ///
    /// 与转发选路共用同一套判据，所以界面标注的「当前账号」就是网关默认使用的
    /// 账号。没有可用账号时返回 None（而不是回落到某个账号）—— 此时转发确实
    /// 不可用，界面就不该给任何账号打「当前」标记。
    pub(crate) fn pick_current(accounts: &[StoredAccount]) -> Option<StoredAccount> {
        let mut candidates: Vec<StoredAccount> = accounts
            .iter()
            .filter(|item| item.has_token() && item.enabled())
            .cloned()
            .collect();
        candidates.sort_by_key(|item| item.order_key());
        candidates.into_iter().next()
    }

    /// 当前账号 id（无账号或全部不可用时为 None）
    pub fn current_account_id(&self) -> Option<String> {
        let _guard = self.guard();
        let state = self.load(&_guard);
        Self::pick_current(&state.accounts).map(|record| record.id().to_string())
    }

    /// 当前账号的凭证与会话（供请求实时取用）；无可用账号时 None。
    pub fn get_current_entry(&self) -> Option<CurrentEntry> {
        let _guard = self.guard();
        let state = self.load(&_guard);
        let record = Self::pick_current(&state.accounts)?;
        if !record.has_token() {
            return None;
        }
        Some(CurrentEntry { id: record.id().to_string(), session: self.session_from_record(&record) })
    }

    /// 实际生效的账号（与 get_current_entry 同一套判据，独立入口只为语义清晰）
    pub fn get_active_entry(&self) -> Option<CurrentEntry> {
        self.get_current_entry()
    }

    /// 账号记录 → auth 模块的会话形态（端点/prefixPath/platform 按 edition 兜底）。
    ///
    /// `proxy` 是该账号解析出的出口（null = 直连），计费/签到等「拿着 session
    /// 直接发请求」的调用方无需再单独传代理参数；`proxyError` 非空表示配置的
    /// 代理解析失败，调用方应回退直连并提示。
    fn session_from_record(&self, record: &StoredAccount) -> Value {
        let edition = resolve_edition(record.edition().as_deref());
        let resolution = resolve_account_proxy(Some(&record.proxy()));
        let (proxy, proxy_error) = split_resolution(resolution);
        let proxy_error_value = proxy_error
            .clone()
            .map(Value::String)
            .unwrap_or(Value::Null);
        json!({
            "endpoint": record.endpoint().unwrap_or_else(|| edition.endpoint.to_string()),
            "prefixPath": record
                .prefix_path()
                .unwrap_or_else(|| edition.prefix_path.to_string()),
            "platform": record.platform().unwrap_or_else(|| edition.platform.to_string()),
            "edition": edition.id,
            "proxy": proxy,
            "proxyError": proxy_error_value,
            "auth": {
                "accessToken": record.access_token(),
                "refreshToken": record.refresh_token(),
                "tokenType": "Bearer",
                "expiresAt": record.expires_at().unwrap_or(0.0),
                "refreshExpiresAt": record.refresh_expires_at().unwrap_or(0.0),
                "domain": record.domain(),
            },
            "account": {
                "uid": record.uid(),
                "nickname": record.nickname(),
                "type": record.account_type(),
                "enterpriseId": record.enterprise_id(),
                "enterpriseName": record.enterprise_name(),
            },
        })
    }

    /// 指定账号的凭证（Node 版 getCredentialsById）；无凭证/不存在时 None
    pub fn get_credentials_by_id(&self, id: &str) -> Option<CredentialsById> {
        let _guard = self.guard();
        let state = self.load(&_guard);
        let record = state.accounts.iter().find(|item| item.id() == id)?;
        if !record.has_token() {
            return None;
        }
        let edition = resolve_edition(record.edition().as_deref());
        let resolution = resolve_account_proxy(Some(&record.proxy()));
        let (proxy, proxy_error) = split_resolution(resolution);
        Some(CredentialsById {
            id: record.id().to_string(),
            name: record.name(),
            uid: record.uid(),
            access_token: record.access_token(),
            refresh_token: record.refresh_token(),
            expires_at: record.expires_at(),
            endpoint: record.endpoint().unwrap_or_else(|| edition.endpoint.to_string()),
            prefix_path: record
                .prefix_path()
                .unwrap_or_else(|| edition.prefix_path.to_string()),
            platform: record.platform().unwrap_or_else(|| edition.platform.to_string()),
            edition: edition.id.to_string(),
            priority: record.priority(),
            enabled: record.enabled(),
            proxy,
            proxy_error,
        })
    }

    /// 指定账号的完整会话形态（Node 版 getSessionById）；不存在/无凭证时 None
    pub fn get_session_by_id(&self, id: &str) -> Option<SessionById> {
        let _guard = self.guard();
        let state = self.load(&_guard);
        let record = state.accounts.iter().find(|item| item.id() == id)?;
        if !record.has_token() {
            return None;
        }
        let resolution = resolve_account_proxy(Some(&record.proxy()));
        let (proxy, proxy_error) = split_resolution(resolution);
        Some(SessionById {
            id: record.id().to_string(),
            session: self.session_from_record(record),
            proxy,
            proxy_error,
        })
    }

    // ─── 公开形态 ────────────────────────────────────────────

    /// 记录 → 公开形态（对照 Node 版 `toPublicAccount`，字段逐个对齐）。
    ///
    /// 注意几处 `||` / `??` 的差别：`prefixPath` 是 `??`（空串有效），
    /// `endpoint` 是 `||`（空串回落版本默认值）；`name` 在 Node 里没有兜底，
    /// 因此这里也是「缺失则不出键」而不是补空串。
    pub(crate) fn to_public_account(&self, record: &StoredAccount) -> Value {
        let edition: &'static EditionInfo = resolve_edition(record.edition().as_deref());
        let fields = record.fields();
        let mut public = Map::new();
        public.insert("id".to_string(), Value::String(record.id().to_string()));
        // name 无兜底：原样透出（含非字符串的脏值），缺失时不出现该键
        if let Some(value) = fields.get("name") {
            public.insert("name".to_string(), value.clone());
        }
        public.insert(
            "uid".to_string(),
            value_or(fields.get("uid"), Value::String(String::new())),
        );
        public.insert(
            "nickname".to_string(),
            value_or(fields.get("nickname"), Value::String(String::new())),
        );
        public.insert(
            "type".to_string(),
            value_or(fields.get("type"), Value::String("personal".to_string())),
        );
        public.insert(
            "enterpriseId".to_string(),
            value_or(fields.get("enterpriseId"), Value::String(String::new())),
        );
        public.insert(
            "enterpriseName".to_string(),
            value_or(fields.get("enterpriseName"), Value::String(String::new())),
        );
        public.insert(
            "tokenTail".to_string(),
            value_or(fields.get("tokenTail"), Value::String(String::new())),
        );
        public.insert(
            "expiresAt".to_string(),
            value_or(fields.get("expiresAt"), Value::Null),
        );
        public.insert(
            "hasRefreshToken".to_string(),
            Value::Bool(
                fields
                    .get("refreshToken")
                    .map(js_truthy)
                    .unwrap_or(false),
            ),
        );
        public.insert(
            "prefixPath".to_string(),
            value_or_nullish(
                fields.get("prefixPath"),
                Value::String(edition.prefix_path.to_string()),
            ),
        );
        public.insert(
            "endpoint".to_string(),
            value_or(
                fields.get("endpoint"),
                Value::String(edition.endpoint.to_string()),
            ),
        );
        public.insert("edition".to_string(), Value::String(edition.id.to_string()));
        public.insert(
            "editionLabel".to_string(),
            Value::String(edition.label.to_string()),
        );
        public.insert("priority".to_string(), Value::from(record.priority()));
        public.insert("enabled".to_string(), Value::Bool(record.enabled()));
        public.insert("addedAt".to_string(), Value::from(record.added_at()));
        public.insert("updatedAt".to_string(), Value::from(record.updated_at()));
        public.insert(
            "proxy".to_string(),
            describe_account_proxy(Some(&value_or(fields.get("proxy"), Value::Null))),
        );
        // Node 是 `record.rateLimits || {}`：任何真值都原样透出
        // （手工写成数组/字符串时也照透，前端按对象读会得到 undefined，
        // 与 Node 的行为保持一致比「顺手修正」重要）
        public.insert(
            "rateLimits".to_string(),
            value_or(fields.get("rateLimits"), Value::Object(Map::new())),
        );
        // 本切片所有账号都视为可用（限额/可用性判定属切片 3 的转发层）
        public.insert("available".to_string(), Value::Bool(true));
        Value::Object(public)
    }

    /// 账号列表快照 `{ currentAccountId, accounts: [...] }`。
    ///
    /// accounts 按**文件顺序**返回（与 Node 版一致）—— 界面自己按优先级排序，
    /// 不要在这里改成优先级序，否则与 Node 版的行为就分叉了。
    pub fn list_accounts(&self) -> Value {
        let _guard = self.guard();
        let state = self.load(&_guard);
        self.snapshot(&state)
    }

    /// 已持锁时的列表快照（CRUD 内部要在同一次锁里连做「写盘 + 取快照」）
    pub(crate) fn snapshot(&self, state: &AccountState) -> Value {
        let current = Self::pick_current(&state.accounts);
        json!({
            "currentAccountId": current.map(|record| Value::String(record.id().to_string()))
                .unwrap_or(Value::Null),
            "accounts": state
                .accounts
                .iter()
                .map(|record| self.to_public_account(record))
                .collect::<Vec<_>>(),
        })
    }
}

/// 把解析结果拆成 `(proxy, proxyError)`：失败时 proxy 为 null、
/// error 说明原因（调用方据此回退直连并提示）。
///
/// `proxy` 是 JSON（直接进会话对象）；`proxy_error` 是 `Option<String>`，
/// 会话形态里的 `proxyError` 由调用方组装成 null/字符串。
fn split_resolution(resolution: Option<ProxyResolution>) -> (Value, Option<String>) {
    match resolution {
        None => (Value::Null, None),
        Some(ProxyResolution::Resolved(ref proxy)) => {
            (json!(proxy_json(proxy)), None)
        }
        Some(ProxyResolution::Failed(message)) => (Value::Null, Some(message)),
    }
}

/// 出口 → JSON（与 ProxyResolution::to_json 的成功分支同形）
fn proxy_json(proxy: &crate::server::core::proxies::ResolvedProxy) -> Value {
    json!({
        "source": proxy.source,
        "protocol": proxy.protocol,
        "host": proxy.host,
        "port": proxy.port,
        "username": proxy.username,
        "password": proxy.password,
        "label": proxy.label,
    })
}
