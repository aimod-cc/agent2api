//! 账号存储句柄：CRUD、当前账号派生、凭证/会话查询、限额标记、启动迁移。
//!
//! 对照 workbuddy-account-store.mjs 的 `createAccountStore` 返回值逐条移植。
//! 三条硬约束写在 `super::mod` 的头部：未知字段全量保留、优先级**在 provider 内**
//! 唯一、provider 字段缺失时按 workbuddy 兜底。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::{json, Map, Value};

use crate::server::core::account_store::state::{
    AccountState, CredentialsById, CurrentEntry, SessionById, StoredAccount,
};
use crate::server::core::account_store::store_util::{js_truthy, value_or, value_or_nullish};
use crate::server::core::endpoints::{resolve_edition, EditionInfo};
use crate::server::core::providers::DEFAULT_PROVIDER_ID;
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

    /// 使用全局配置目录（`~/.agent2api`）
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
        let priority_scope = value
            .get("priorityScope")
            .and_then(Value::as_str)
            .map(str::to_string);
        AccountState { accounts, priority_scope }
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
        let mut payload = json!({
            "accounts": state.accounts.iter().map(StoredAccount::to_value).collect::<Vec<_>>(),
        });
        if let (Some(scope), Some(object)) = (&state.priority_scope, payload.as_object_mut()) {
            object.insert("priorityScope".to_string(), Value::String(scope.clone()));
        }
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

    /// 指定 provider 的「当前账号」= **该家**转发顺序里第一个「已启用且有可用凭证」的账号。
    ///
    /// 判据与转发层逐条相同（也是 `current_entry_for_provider` 的内核）：启用 +
    /// [`StoredAccount::has_credentials`]（桌面端实时账号记录里按设计没有 token，
    /// 但凭证在客户端登录态文件里，同样算「有凭证」）+ 按 (优先级, 加入时间) 取队首。
    /// 没有可用账号时返回 None（而不是回落到某个账号）—— 此时这一家转发确实拿不到
    /// 登录态，界面就不该给它的任何账号打「当前」标记。
    ///
    /// ── 作用域：这一家的队首 ──────────────────────────────────
    /// 全局队列之后转发不再逐家排队，本函数只剩「没有账号记录时回落到该家默认登录态」
    /// 与快照里 `currentAccountIds` 的参考用途。判据与全局的 `pick_current` 相同，
    /// 只是先缩到该 provider。历史上这里曾出现两个分歧（现已统一）：
    ///   1. 桌面端账号（raccoon/catpaw/autoclaw 的 `desktop: true`）被整体跳过 ——
    ///      转发明明会选它，界面却拿不到「它是当前账号」这个事实；
    ///   2. 只有小浣熊账号的机器上，workbuddy 的「当前账号」会是小浣熊的账号。
    /// 跨家展示（账号页每组卡片上的 ★）由调用方**逐家调用本函数**得到
    /// （见 `snapshot` 的 `currentAccountIds`），不要把不同家的结果塞进同一个字段。
    pub(crate) fn pick_current_for_provider(
        accounts: &[StoredAccount],
        provider: &str,
    ) -> Option<StoredAccount> {
        let mut candidates: Vec<StoredAccount> = accounts
            .iter()
            .filter(|item| item.provider() == provider && item.enabled() && item.has_credentials())
            .cloned()
            .collect();
        candidates.sort_by_key(StoredAccount::order_key);
        candidates.into_iter().next()
    }

    /// **全局**队首：所有提供商的账号排在同一条队列里，取第一个「已启用且有可用
    /// 凭证」的账号。这是账号页 ★ / 「首选」与 `/api/session` 的 `currentAccountId`
    /// 的数据源。对某个具体模型实际先用谁还要看它是否支持该模型、是否限流中，
    /// 那是转发层按请求逐次判定的（`routing::pick_account_by_priority`）。
    ///
    /// ── 为什么排除**没有转发能力**的家（历史用法，五家现已都能转发）──
    /// 这个值的消费方都把它当「会承接请求的那个账号」用：`/api/session` 的
    /// `currentAccountId` 决定顶栏显示的昵称/过期时间，`clear_session`（退出登录）
    /// 按它**删除账号**，界面的 ★ / 「设为首选」也按它渲染。Qoder 只有账号管理
    /// 能力那会儿（转发返回 501）必须排除：排进队首会让顶栏把它显示成当前登录态，
    /// 而「退出登录」会把它删掉。它接上推理协议后，这条过滤对它就自然失效了 ——
    /// 判据取自适配器的**恒定能力声明**（`supports_chat`），不写死 provider id，
    /// 因此两家各自的接线时点都不需要改这里。
    pub(crate) fn pick_current(accounts: &[StoredAccount]) -> Option<StoredAccount> {
        let mut candidates: Vec<StoredAccount> = accounts
            .iter()
            .filter(|item| item.enabled() && item.has_credentials() && forwards_requests(item))
            .cloned()
            .collect();
        candidates.sort_by_key(StoredAccount::order_key);
        candidates.into_iter().next()
    }

    /// 全局「当前账号」id（无账号或全部不可用时为 None）—— 全局队首，见 `pick_current`。
    pub fn current_account_id(&self) -> Option<String> {
        let _guard = self.guard();
        let state = self.load(&_guard);
        Self::pick_current(&state.accounts).map(|record| record.id().to_string())
    }

    /// workbuddy 默认登录态的凭证与会话（供 workbuddy 专属链路实时取用：
    /// 模型目录拉取、登录态刷新等）；无可用账号时 None。
    ///
    /// **workbuddy 语义**（收窄到默认 provider）。转发链路不用它 ——
    /// 转发按全局队列选账号，只有「没有账号记录」时才回落到各家的默认登录态。
    pub fn get_current_entry(&self) -> Option<CurrentEntry> {
        self.current_entry_for_provider(DEFAULT_PROVIDER_ID)
    }

    /// 实际生效的账号（与 get_current_entry 同一套判据，独立入口只为语义清晰）
    pub fn get_active_entry(&self) -> Option<CurrentEntry> {
        self.get_current_entry()
    }

    /// 指定 provider 的「当前账号」凭证与会话（Agent2API W2b-T3）。
    ///
    /// 判据与 `pick_current_for_provider` 逐条相同（启用 + 有凭证 + 按 (优先级, 加入时间)
    /// 取队首），只是先在**该 provider 的账号组**里缩一遍。转发层逐家尝试时用它拿
    /// 「这一家的默认账号」，与 `accounts_for_provider` 的选路口径一致
    /// （后者用于显式指定的账号链，这里用于「没有账号记录/未登录」时的兜底会话）。
    ///
    /// ── 小浣熊的桌面端实时账号（W3-T4）─────────────────────────
    /// 它**记录里本来就没有 accessToken**（凭证在 `~/.box-agent/config/auth.json`），
    /// 因此判据从 `has_token()` 放宽成 `has_credentials()`：带 `desktop: true`
    /// 标记的 raccoon 记录也算「有凭证」，实际 token 由会话构造时实时读入
    /// （见 `session_from_record`）。只认 `has_token()` 会让这台机器上
    /// 小浣熊永远选不中账号，转发直接 401。
    ///
    /// 返回 None 表示该 provider 在账号文件里没有可用账号（此时
    /// `auth::get_current_session_for` 会尝试该 provider 的环境变量旁路）。
    pub fn current_entry_for_provider(&self, provider: &str) -> Option<CurrentEntry> {
        let _guard = self.guard();
        let state = self.load(&_guard);
        let mut candidates: Vec<StoredAccount> = state
            .accounts
            .iter()
            .filter(|item| item.provider() == provider && item.enabled() && item.has_credentials())
            .cloned()
            .collect();
        candidates.sort_by_key(StoredAccount::order_key);
        let record = candidates.into_iter().next()?;
        Some(CurrentEntry {
            id: record.id().to_string(),
            session: self.session_from_record(&record),
        })
    }

    /// 账号记录 → auth 模块的会话形态（端点/prefixPath/platform 按 edition 兜底）。
    ///
    /// `proxy` 是该账号解析出的出口（null = 直连），计费/签到等「拿着 session
    /// 直接发请求」的调用方无需再单独传代理参数；`proxyError` 非空表示配置的
    /// 代理解析失败，调用方应回退直连并提示。
    ///
    /// ── 小浣熊桌面端账号的凭证（W3-T4）─────────────────────────
    /// 这类记录**故意不落 token**（凭证在 `~/.box-agent/config/auth.json`，
    /// 客户端重新登录后下次请求即生效）。会话形态是「拿着就能发请求」的形状，
    /// 因此这里把 auth.json 的实时值填进 `auth` —— 否则转发链路的
    /// `has_access_token` 判定会把它当成没有登录态（401），而小浣熊适配器
    /// 也正是从 `auth.accessToken` 取 Authorization 头的。
    /// 读盘失败（文件缺失/损坏）时退化成空 token：让上层的 401 文案去说明原因，
    /// 本函数不制造错误（它是被 CRUD 与选路大量复用的纯取值路径）。
    fn session_from_record(&self, record: &StoredAccount) -> Value {
        let edition = resolve_edition(record.edition().as_deref());
        let resolution = resolve_account_proxy(Some(&record.proxy()));
        let (proxy, proxy_error) = split_resolution(resolution);
        let proxy_error_value = proxy_error
            .clone()
            .map(Value::String)
            .unwrap_or(Value::Null);
        let live = live_desktop_credentials(record);
        let (access_token, refresh_token, expires_at) = match live {
            Some(credentials) => credentials,
            None => (
                record.access_token(),
                record.refresh_token(),
                record.expires_at().unwrap_or(0.0),
            ),
        };
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
                "accessToken": access_token,
                "refreshToken": refresh_token,
                "tokenType": "Bearer",
                "expiresAt": expires_at,
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
    ///
    /// 小浣熊桌面端账号同样把 auth.json 的实时值填进 `access_token` /
    /// `refresh_token` / `expires_at`（理由见 `session_from_record`）——
    /// 「有没有可用凭证」在全仓是一条判据，不能因为凭证存在别处就分叉。
    pub fn get_credentials_by_id(&self, id: &str) -> Option<CredentialsById> {
        let _guard = self.guard();
        let state = self.load(&_guard);
        let record = state.accounts.iter().find(|item| item.id() == id)?;
        let live = live_desktop_credentials(record);
        let (access_token, refresh_token, expires_at) = match live {
            Some(credentials) => credentials,
            None => (
                record.access_token(),
                record.refresh_token(),
                record.expires_at().unwrap_or(0.0),
            ),
        };
        if access_token.is_empty() {
            return None;
        }
        let edition = resolve_edition(record.edition().as_deref());
        let resolution = resolve_account_proxy(Some(&record.proxy()));
        let (proxy, proxy_error) = split_resolution(resolution);
        Some(CredentialsById {
            id: record.id().to_string(),
            name: record.name(),
            uid: record.uid(),
            access_token,
            refresh_token,
            expires_at: if expires_at > 0.0 { Some(expires_at) } else { None },
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
        if !record.has_credentials() {
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
        // 所属提供商（Agent2API 改造新增字段）：缺失时按 workbuddy 兜底，
        // 保证界面拿到的每条账号都能直接分组，不必自己判断「没有 provider = 老数据」
        public.insert(
            "provider".to_string(),
            Value::String(record.provider()),
        );
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

    /// 账号列表快照 `{ currentAccountId, currentAccountIds, accounts: [...], providers: [...] }`。
    ///
    /// accounts 按**文件顺序**返回（与 Node 版一致）—— 界面自己按优先级排序，
    /// 不要在这里改成优先级序，否则与 Node 版的行为就分叉了。
    ///
    /// ── 「当前账号」的两个字段 ──────────────────────────────
    ///   · `currentAccountId`：**全局队首**（`pick_current`：启用 + 有可用凭证 +
    ///     优先级序），与 `/api/session` 的 `session.currentAccountId` 同源。
    ///     账号页的 ★ / 「首选」读这个。
    ///   · `currentAccountIds`：provider → 该家队首账号 id（逐家派生，
    ///     `pick_current_for_provider`）。全局队列之后它只剩「这一家有没有可用账号」
    ///     的参考意义，保留是为了不破坏读它的旧客户端。
    ///
    /// `providers` 是 provider 摘要（`{id,label,count}`）：
    /// 界面用它渲染账号页的分组标题与「作用提供商」多选，因此**必须在列表接口
    /// 上就给出**，前端不必再发第二个请求。计数是各 provider 的账号**总数**
    /// （含禁用账号）—— 与分组标题显示的「N 个账号」口径一致。
    pub fn list_accounts(&self) -> Value {
        let _guard = self.guard();
        let state = self.load(&_guard);
        self.snapshot(&state)
    }

    /// 已持锁时的列表快照（CRUD 内部要在同一次锁里连做「写盘 + 取快照」）
    pub(crate) fn snapshot(&self, state: &AccountState) -> Value {
        // 逐家派生队首：provider 取值的来源就是账号文件里出现过的家
        // （含手工塞进来的未知 id —— 它们也有自己的队首，不该被静默忽略）
        let mut current_ids = Map::new();
        for record in &state.accounts {
            let provider = record.provider();
            if current_ids.contains_key(&provider) {
                continue;
            }
            let head = Self::pick_current_for_provider(&state.accounts, &provider)
                .map(|record| Value::String(record.id().to_string()))
                .unwrap_or(Value::Null);
            current_ids.insert(provider, head);
        }
        let global_current = Self::pick_current(&state.accounts)
            .map(|record| Value::String(record.id().to_string()))
            .unwrap_or(Value::Null);
        json!({
            "currentAccountId": global_current,
            "currentAccountIds": Value::Object(current_ids),
            "accounts": state
                .accounts
                .iter()
                .map(|record| self.public_account(record))
                .collect::<Vec<_>>(),
            "providers": self.provider_summary(state),
        })
    }

    /// 记录 → 公开形态（**按 provider 分派**）。
    ///
    /// 各家的公开字段集不同：workbuddy 有 uid/nickname/edition/enterprise…，
    /// 小浣熊有 userId/tokenExpiresAt/desktop 且**没有**积分字段（架构文档 §5/§6），
    /// CatPaw 有 uid/loginName/tokenTail/desktop 且**没有**积分/签到（W5-T-d4）。
    /// 分派点放在这里，调用方（列表快照、批量目标解析、限额事件日志）
    /// 一行都不用改 —— 它们拿到的就是各自 provider 该有的形状。
    ///
    /// 兜底分支给的是 **workbuddy 形状**，这对**未知** provider id 是刻意的
    /// （与 `StoredAccount::provider()` 的容错口径一致：手改文件塞进来的陌生 id
    /// 至少还能在界面上显示出来）。AutoClaw 现在**加不了账号**
    /// （`api::accounts::add_account` 显式 400），将来它的公开形态在适配器接线
    /// 波次里补一个分支即可 —— 那条分支落地前，万一有手改的账号记录落进这里，
    /// 走 workbuddy 形状总比整条列表报错好。
    ///
    /// ── `hasCredentials` / `chatSupported`：统一注入的**跨家事实** ──────
    /// 在这里**统一注入**而不是改各家的公开形态函数：判据只有一条
    /// （`has_credentials` / 适配器的 `supports_chat`），而「谁有凭证」「谁能转发」
    /// 正是转发选路与「当前账号」派生共用的那两道闸门。前端要按模型
    /// 自行推算「这一家此刻会走谁」时（后端只给不限模型的队首），必须拿得到同一
    /// 事实，否则会出现「界面标 ★ 的账号其实转发时会因无凭证被跳过」的分歧。
    /// 放在分派点让各家形状**同字段名、同语义**，前端不必按 provider 查表。
    ///
    /// 纯新增字段：各家的既有字段一个不动，旧客户端忽略它即可。
    pub(crate) fn public_account(&self, record: &StoredAccount) -> Value {
        let shaped = if record.provider() == super::RACCOON_PROVIDER_ID {
            self.to_raccoon_public_account(record)
        } else if record.provider() == super::CATPAW_PROVIDER_ID {
            self.to_catpaw_public_account(record)
        } else if record.provider() == super::AUTOCLAW_PROVIDER_ID {
            self.to_autoclaw_public_account(record)
        } else if record.provider() == super::QODER_PROVIDER_ID {
            self.to_qoder_public_account(record)
        } else {
            self.to_public_account(record)
        };
        match shaped {
            Value::Object(mut fields) => {
                fields.insert(
                    "hasCredentials".to_string(),
                    Value::Bool(record.has_credentials()),
                );
                // 没有转发能力的家：界面据此说明「启用了也不会被转发」，
                // 而不是把一个失效的启用开关当成正常账号展示。
                // 五家现在都能转发，所以正常配置下这里恒为 true ——
                // 保留这个字段是因为「能用账号管理、但转发还没接上」这种过渡期
                // 状态将来还会出现，而界面需要有办法如实说出来。
                fields.insert(
                    "chatSupported".to_string(),
                    Value::Bool(forwards_requests(record)),
                );
                Value::Object(fields)
            }
            // 各家形状恒为对象；真出现异常形态时原样透出，不在这里改语义
            other => other,
        }
    }

    /// provider 摘要：注册表顺序 + 各 provider 的账号总数。
    ///
    /// 计数函数在内存快照上跑（不读盘、不再取锁）—— `snapshot` 的调用方已经持锁。
    fn provider_summary(&self, state: &AccountState) -> Value {
        let counts: Vec<(String, usize)> = state
            .accounts
            .iter()
            .fold(Vec::new(), |mut acc, record| {
                let provider = record.provider();
                match acc.iter_mut().find(|(id, _)| *id == provider) {
                    Some((_, count)) => *count += 1,
                    None => acc.push((provider, 1)),
                }
                acc
            });
        let value = crate::server::core::providers::summary_json(|id| {
            counts
                .iter()
                .find(|(known, _)| known == id)
                .map(|(_, count)| *count)
                .unwrap_or(0)
        });
        Value::Array(value)
    }

    // ─── 按 provider 查询 ────────────────────────────────────

    /// 指定 provider 的启用账号（公开形态，按 priority、addedAt 升序）。
    ///
    /// 消费方：聚合目录判断「这一家现在有没有可用登录态」
    /// （`catalog::provider_available`）—— 直接调适配器会让「有清单但没账号」
    /// 的家被广告给客户端，所以聚合层必须能按 provider 查可用性。
    ///
    /// 转发选路**不用**本函数：`rotate::provider_accounts` 走的是公开快照
    /// 过滤（那份要**看得见禁用账号**，第三级「全禁用 → 503」的文案依赖它）。
    ///
    /// 「启用」口径与选路一致：`enabled !== false`**且**有凭证
    /// （`has_credentials`：小浣熊的桌面端实时账号记录里没有 token，但凭证在
    /// auth.json，同样算「有凭证」，见 `state.rs` 的说明）。返回值是**排序后**的：
    /// 调用方按序尝试即可，不必自己再排一遍 —— 优先级的相对顺序就是主备顺序。
    pub fn accounts_for_provider(&self, provider: &str) -> Vec<Value> {
        let _guard = self.guard();
        let state = self.load(&_guard);
        let mut records: Vec<StoredAccount> = state
            .accounts
            .iter()
            .filter(|record| {
                record.provider() == provider && record.enabled() && record.has_credentials()
            })
            .cloned()
            .collect();
        records.sort_by_key(StoredAccount::order_key);
        records
            .iter()
            .map(|record| self.public_account(record))
            .collect()
    }
}

/// 桌面端实时账号的凭证（读取那一刻的值）；不是桌面端账号或读不到时 None。
///
/// 为什么不缓存到记录里：auth.json 是小浣熊生态的共享登录态，客户端/box-agent
/// 会在 token 临期时自行刷新并回写；把值缓存进账号记录只会得到一份过期副本，
/// 还会与「刷新成功后回写 auth.json」的路径打架（架构文档 §3.2 明确要求
/// 桌面端账号的凭证**每次实时读**）。
///
/// ── 两家桌面态（W4b-T-c2 起）────────────────────────────────
/// 小浣熊（`raccoon-desktop`，读 `~/.box-agent/config/auth.json`）与
/// AutoClaw（`autoclaw-desktop`，读 `%APPDATA%/AutoClaw/auth.json` 的
/// **safeStorage 密文**并走 DPAPI + AES-GCM 解密）都是「记录里不落 token、
/// 凭证实时读」的形态，因此都要在这里填进会话 —— 否则转发链路的
/// `build_chat_request` 从 `auth.accessToken` 取到空串，稳定 401。
/// AutoClaw 的解密结果由 `autoclaw::credentials` 的进程级 mtime 缓存兜住，
/// 每个请求都做一次 DPAPI 是被缓存挡住的那件贵事，不是本函数重复做的。
///
/// CatPaw 不在这里：它的凭证不是 Bearer（Cookie 形态的 `X-Passport-Token` +
/// 独立 uid），会话的 `auth.accessToken` 装不下它，由
/// `catpaw::adapter::forward_conversation` 自己经 `snapshot_for` 取。
///
/// `pub(crate)`：各家的账号公开形态（`raccoon_accounts.rs` /
/// `autoclaw_accounts.rs`）也要用它 —— 桌面端账号的展示字段（token 尾号/过期
/// 时间/能否刷新）必须来自同一份实时值，否则界面与转发看到的就是两个状态。
pub(crate) fn live_desktop_credentials(record: &StoredAccount) -> Option<(String, String, f64)> {
    if record.provider() == super::AUTOCLAW_PROVIDER_ID && record.is_desktop() {
        let credentials =
            crate::server::core::providers::autoclaw::credentials::local_credentials().ok()?;
        return Some((
            credentials.token,
            credentials.refresh_token,
            credentials.expires_at.unwrap_or(0.0),
        ));
    }
    if record.provider() != super::RACCOON_PROVIDER_ID || !record.is_desktop() {
        return None;
    }
    let credentials = crate::server::core::providers::raccoon::credentials::desktop_credentials()
        .ok()?;
    Some((
        credentials.token,
        credentials.refresh_token,
        credentials.expires_at.unwrap_or(0.0),
    ))
}

/// 这条账号记录所属的 provider **是否能承接推理转发**（`ProviderAdapter::supports_chat`）。
///
/// 全局队首要排除「只有账号管理能力」的家（Qoder 在接上推理协议之前就是）：
/// 它会被顶栏当成当前登录态显示，而「退出登录」按队首**删除账号**。
/// 判据问适配器，不写死 id —— 于是某家从「只有账号管理」走到「也能转发」时，
/// 这里一行都不用改；未知 provider id（手改文件塞进来的）按「能转发」处理 ——
/// 与 `public_account` 的兜底口径一致，不让一条陌生记录把队首派生整个清空。
pub(crate) fn forwards_requests(record: &StoredAccount) -> bool {
    crate::server::core::providers::kind_from_id(&record.provider())
        .map(|kind| crate::server::core::providers::adapter::adapter_for(kind).supports_chat())
        .unwrap_or(true)
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
