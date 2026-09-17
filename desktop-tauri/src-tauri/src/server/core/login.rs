//! 无头登录（device-flow 风格）与登录任务表。
//!
//! 流程与桌面端 createSession 完全一致（对照 src/workbuddy-auth.mjs 的
//! `loginInteractive`，以及 server.mjs 614-668 行的 startLoginTask /
//! cancelLoginTask / loginTasks）：
//!
//!   ① POST {endpoint}/v2{prefix}/auth/state?platform=workbuddy   （匿名）
//!        → data.authUrl + data.state
//!   ② 用户在浏览器打开 authUrl 完成登录
//!   ③ 每 3 秒 GET {endpoint}/v2{prefix}/auth/token?state=<state> （匿名）
//!        → data.accessToken / refreshToken / expiresIn / refreshExpiresIn
//!        未完成时上游返回 code=11217，需继续轮询
//!   ④ GET {endpoint}/v2{prefix}/login/account?state=<state>     （Bearer）
//!        → 账号详情（失败不影响登录，回退用 ⑤ 的列表）
//!   ⑤ GET {endpoint}/v2{prefix}/accounts                        （Bearer）
//!        → data.accounts[]
//!
//! 登录成功后把会话交给账号存储入库（Node 版 `accountStore.addAccount(session)`）。
//!
//! ── 任务表与取消 ──────────────────────────────────────────
//! 每次登录是一个 `LoginTask`（`Arc<Mutex<...>>`），`/start` 拿到句柄后最多等
//! 15 秒的 authUrl，拿到就把任务按 state 登记进表供 `/wait` 查询；拿不到就回
//! 502（任务本身继续在后台跑，与 Node 版一致）。
//!
//! 取消不走 AbortController 而是置任务上的 `canceled` 标记 —— 轮询循环每一拍
//! 检查一次，语义等价（Node 的 signal.aborted 也是循环里检查）。
//! 任务完成后保留 10 分钟，清理在「取任务时顺手做过期检查」里完成，
//! 不额外起后台定时器。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::{json, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::core::auth::{
    anonymous_headers, context_for_edition, send_public_request, unwrap_public_response, urlencoding,
    with_expires_at, AuthService, WorkBuddyAuthError, SERVER_CODE_RETRY_FETCH_TOKEN,
};
use crate::server::core::endpoints::{resolve_edition, Context, DEFAULT_EDITION};
use crate::server::logging;

/// 登录轮询间隔与总超时（桌面端 SIGN_IN_FETCH_INTERVAL / SIGN_IN_PENDING_TIMEOUT）
pub const LOGIN_POLL_INTERVAL_MS: u64 = 3000;
pub const LOGIN_TIMEOUT_MS: u64 = 5 * 60 * 1000;
/// `/start` 等 authUrl 的上限（对应 server.mjs 的 `Date.now() + 15000`）
pub const AUTH_URL_WAIT_MS: u64 = 15_000;
/// 任务完成后在表里保留 10 分钟（Node 版 setTimeout 同值）
const TASK_RETENTION_MS: i64 = 10 * 60 * 1000;

/// 一次登录任务的状态（对应 Node 版 `loginTasks` 里的 task 对象）。
#[derive(Clone, Debug, Default)]
pub struct LoginTaskState {
    pub state: Option<String>,
    pub auth_url: Option<String>,
    pub done: bool,
    pub error: Option<String>,
    /// 成功后的会话摘要 `{ accountUid, nickname, edition }`
    pub session: Option<Value>,
    pub edition: String,
    pub canceled: bool,
    finished_at: Option<i64>,
}

impl LoginTaskState {
    /// `/api/session/login/wait` 的三分支响应：
    /// `{pending:true}` / `{done:true,error}` / `{done:true,session}`
    pub fn to_wait_response(&self) -> Value {
        if !self.done {
            return json!({ "pending": true });
        }
        if let Some(error) = &self.error {
            return json!({ "done": true, "error": error });
        }
        json!({ "done": true, "session": self.session.clone().unwrap_or(Value::Null) })
    }
}

/// 任务句柄：登录流程与 `/start`、`/wait`、`/cancel` 共享同一个状态。
#[derive(Clone)]
pub struct LoginTaskHandle {
    inner: Arc<Mutex<LoginTaskState>>,
    /// 唯一标识（表里按 state 索引，未拿到 state 前用它做日志与查找兜底）
    ticket: u64,
}

impl LoginTaskHandle {
    fn lock(&self) -> MutexGuard<'_, LoginTaskState> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    pub fn snapshot(&self) -> LoginTaskState {
        self.lock().clone()
    }

    fn update(&self, action: impl FnOnce(&mut LoginTaskState)) {
        let mut guard = self.lock();
        action(&mut guard);
    }

    pub fn ticket(&self) -> u64 {
        self.ticket
    }
}

/// 登录任务表：`state → 任务`。
///
/// 只登记「已拿到 state」的任务 —— 与 Node 版一致（它在 onAuthUrl 回调里
/// 才 `loginTasks.set(state, task)`），所以拿不到 authUrl 的失败任务不会
/// 被 `/wait` 查到，前端会收到 404「登录任务不存在或已过期」。
#[derive(Clone)]
pub struct LoginTasks {
    inner: Arc<Mutex<TaskTable>>,
}

struct TaskTable {
    by_state: HashMap<String, LoginTaskHandle>,
    next_ticket: u64,
}

impl LoginTasks {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(TaskTable { by_state: HashMap::new(), next_ticket: 1 })),
        }
    }

    fn lock(&self) -> MutexGuard<'_, TaskTable> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// 清理过期任务（完成后 10 分钟）。取任务时顺手做，不起后台定时器 ——
    /// 桌面端关掉弹窗后不会再有 /wait 轮询，此时残留的任务对象也只是几十字节，
    /// 下次任何一次取任务都会把它扫掉。
    fn sweep(table: &mut TaskTable) {
        let now = logging::now_ms();
        table.by_state.retain(|_, handle| match handle.lock().finished_at {
            Some(finished) => now - finished < TASK_RETENTION_MS,
            None => true,
        });
    }

    pub fn get(&self, state: &str) -> Option<LoginTaskHandle> {
        if state.is_empty() {
            return None;
        }
        let mut table = self.lock();
        Self::sweep(&mut table);
        table.by_state.get(state).cloned()
    }

    /// 登记任务（`/start` 拿到 state 后调用）
    pub fn register(&self, state: &str, handle: LoginTaskHandle) -> bool {
        if state.is_empty() {
            return false;
        }
        let mut table = self.lock();
        Self::sweep(&mut table);
        table.by_state.insert(state.to_string(), handle);
        true
    }

    /// 取消任务：置 canceled 标记（轮询循环下一拍退出）并**立即从表里移除**。
    ///
    /// 移除是刻意的：Node 版 `cancelLoginTask` 同样 `loginTasks.delete(state)`，
    /// 于是前端紧接着的那次 `/wait` 会拿到 404「登录任务不存在或已过期」——
    /// 壳侧 login.rs 正是靠这条文案判定「用户已放弃」，直接结束等待循环
    /// （见其 `error.contains("登录任务不存在")` 分支）。保留任务反而会让
    /// 那次 /wait 拿到一个 error 字符串，走进「打印错误继续轮询」的分支。
    ///
    /// 返回是否真的取消了。已结束/不存在的任务返回 false。
    pub fn cancel(&self, state: &str) -> bool {
        let Some(handle) = self.get(state) else {
            return false;
        };
        let should_cancel = {
            let guard = handle.lock();
            !guard.done
        };
        if !should_cancel {
            return false;
        }
        handle.update(|task| {
            task.canceled = true;
            task.done = true;
            task.error = Some("登录已取消".to_string());
            task.finished_at = Some(logging::now_ms());
        });
        {
            let mut table = self.lock();
            // 按 state 或按句柄（state 未入表时用 ticket 兜底，两者必居其一）
            let ticket = handle.ticket();
            table
                .by_state
                .retain(|_, item| item.ticket() != ticket);
        }
        logging::log("[Login]", "登录任务已取消（用户放弃等待）");
        true
    }

    /// 占位一个任务号（`/start` 与任务句柄共用，仅用于诊断日志）
    fn next_ticket(table: &mut TaskTable) -> u64 {
        let ticket = table.next_ticket;
        table.next_ticket = table.next_ticket.wrapping_add(1);
        ticket
    }
}

impl Default for LoginTasks {
    fn default() -> Self {
        Self::new()
    }
}

/// 登录服务：把 auth（会话与账号接口）与任务表绑在一起。
#[derive(Clone)]
pub struct LoginService {
    auth: AuthService,
    store: AccountStore,
    tasks: LoginTasks,
}

impl LoginService {
    pub fn new(auth: AuthService, store: AccountStore) -> Self {
        Self { auth, store, tasks: LoginTasks::new() }
    }

    pub fn tasks(&self) -> &LoginTasks {
        &self.tasks
    }

    /// 发起一次登录任务（对应 server.mjs 的 `startLoginTask`）。
    ///
    /// 立刻返回任务句柄，登录流程在后台任务里跑 —— authUrl 与 state 由
    /// 回调写进句柄，调用方（`/api/session/login/start`）负责等它出现。
    pub fn start(&self, edition: Option<&str>) -> LoginTaskHandle {
        let info = resolve_edition(edition.or(Some(DEFAULT_EDITION)));
        let handle = self.new_handle(info);
        self.spawn_login(handle.clone(), info.id.to_string());
        handle
    }

    /// 建一个任务句柄但不启动后台任务（`/auth/login` 的同步登录用它 ——
    /// 那条路径自己 await 登录流程，不能再起一个后台任务重复登录）
    fn new_handle(&self, info: &'static crate::server::core::endpoints::EditionInfo) -> LoginTaskHandle {
        let ticket = {
            let mut guard = self.tasks.lock();
            LoginTasks::next_ticket(&mut guard)
        };
        LoginTaskHandle {
            inner: Arc::new(Mutex::new(LoginTaskState {
                state: None,
                auth_url: None,
                done: false,
                error: None,
                session: None,
                edition: info.id.to_string(),
                canceled: false,
                finished_at: None,
            })),
            ticket,
        }
    }

    /// 等 authUrl（对应 Node 版 `/api/session/login/start` 的 15 秒等待）。
    ///
    /// 返回 `(state, authUrl, edition)`；超时或任务提前失败时返回错误原因。
    /// 任务本身不受影响，仍在后台轮询（与 Node 版一致：返回 502 不代表任务停了）。
    pub async fn wait_for_auth_url(
        &self,
        handle: &LoginTaskHandle,
        timeout: Duration,
    ) -> Result<(String, String, String), String> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let task = handle.snapshot();
            if let (Some(state), Some(url)) = (task.state.clone(), task.auth_url.clone()) {
                // authUrl 拿到即登记进任务表，/wait 才能按 state 查到
                self.tasks.register(&state, handle.clone());
                return Ok((state, url, task.edition));
            }
            if task.done {
                return Err(task
                    .error
                    .clone()
                    .unwrap_or_else(|| "上游登录服务未返回登录链接".to_string()));
            }
            if tokio::time::Instant::now() >= deadline {
                return Err("上游登录服务未返回登录链接".to_string());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// 跑一次完整登录（同步等完成，供 POST /auth/login 用）。
    ///
    /// 与 `start()` 的区别：这里**亲自 await 登录流程**，不起后台任务 ——
    /// 命令行入口要的就是「等登录完成才响应」。任务句柄仍然建一个，
    /// 这样 authUrl 能被回调写进去（日志用它打印链接），登录结果也能
    /// 被 `/api/session/login/wait` 查到（与 Node 版共用 loginTasks 一致）。
    pub async fn run_login(&self, edition: Option<&str>) -> Result<Value, WorkBuddyAuthError> {
        let info = resolve_edition(edition.or(Some(DEFAULT_EDITION)));
        logging::log(
            "[Login]",
            &format!("发起{}登录（{}）…", info.label, info.endpoint),
        );
        let handle = self.new_handle(info);
        let session = self
            .login_interactive(Some(info.id), &handle, |url, _state| {
                logging::log("[Login]", "请在浏览器中打开以下链接并完成登录：");
                logging::console_line("[Login]", &format!("  {url}"));
                logging::log("[Login]", "登录完成后本网关将自动获取并保存 token…");
            })
            .await;
        match &session {
            Ok(value) => {
                if let Some(state) = handle.snapshot().state {
                    self.tasks.register(&state, handle.clone());
                }
                finish_task(&handle, value);
                logging::log(
                    "[Login]",
                    &format!(
                        "✅ 登录完成（{}，账号 {}）",
                        info.label,
                        value
                            .get("account")
                            .and_then(|account| account.get("uid"))
                            .and_then(Value::as_str)
                            .unwrap_or("未知")
                    ),
                );
            }
            Err(error) => {
                let message = error.message.clone();
                finish_task_error(&handle, &message);
                logging::log("[Login]", &format!("❌ 登录失败: {message}"));
            }
        }
        session
    }

    /// 后台起一个登录任务，并把「失败/完成」写回任务句柄。
    fn spawn_login(&self, handle: LoginTaskHandle, edition: String) {
        let this = self.clone();
        tauri::async_runtime::spawn(async move {
            let handle_for_callback = handle.clone();
            let result = this
                .login_interactive(Some(edition.as_str()), &handle, move |url, state| {
                    // 回调发生在轮询任务内：把 authUrl/state 落进句柄，
                    // 让 /start 的等待与 /wait 的轮询都能看到
                    let state = state.map(str::to_string);
                    handle_for_callback.update(|task| {
                        task.auth_url = Some(url.to_string());
                        task.state = state.clone();
                    });
                })
                .await;
            match result {
                Ok(session) => {
                    if let Some(state) = handle.snapshot().state {
                        this.tasks.register(&state, handle.clone());
                    }
                    let account_uid = session
                        .get("account")
                        .and_then(|account| account.get("uid"))
                        .and_then(Value::as_str)
                        .unwrap_or("未知")
                        .to_string();
                    finish_task(&handle, &session);
                    logging::log(
                        "[Login]",
                        &format!("✅ 登录任务完成（{edition}，账号 {account_uid}）"),
                    );
                }
                Err(error) => {
                    let canceled = handle.snapshot().canceled;
                    let message = error.message.clone();
                    finish_task_error(&handle, &message);
                    if canceled {
                        logging::log("[Login]", "登录任务已取消");
                    } else {
                        logging::log("[Login]", &format!("❌ 登录任务失败: {message}"));
                    }
                }
            }
        });
    }

    /// 登录主流程（对照 `loginInteractive`）。
    ///
    /// 每拍轮询前检查任务的 `canceled` 标记 —— 等价 Node 的 `signal.aborted`。
    async fn login_interactive(
        &self,
        edition: Option<&str>,
        handle: &LoginTaskHandle,
        on_auth_url: impl Fn(&str, Option<&str>),
    ) -> Result<Value, WorkBuddyAuthError> {
        let context: Context = context_for_edition(edition, None);
        let headers = anonymous_headers();
        let url = context.auth_url(&format!(
            "/auth/state?platform={}",
            urlencoding(&context.platform)
        ));
        let response = send_public_request("POST", &url, Some(&json!({})), &headers).await?;
        let state_data = unwrap_public_response(&response, "auth/state")?;
        let state = state_data
            .get("state")
            .or_else(|| state_data.get("authState"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let auth_url = state_data
            .get("authUrl")
            .and_then(Value::as_str)
            .map(str::to_string);
        if state.is_none() && auth_url.is_none() {
            return Err(WorkBuddyAuthError::new(
                "auth/state 未返回 state/authUrl，无法发起登录",
            ));
        }
        if let Some(url) = &auth_url {
            on_auth_url(url, state.as_deref());
        }
        let state = state.unwrap_or_default();

        let deadline = tokio::time::Instant::now() + Duration::from_millis(LOGIN_TIMEOUT_MS);
        while tokio::time::Instant::now() < deadline {
            if handle.snapshot().canceled {
                return Err(WorkBuddyAuthError::new("登录已取消"));
            }
            tokio::time::sleep(Duration::from_millis(LOGIN_POLL_INTERVAL_MS)).await;
            if handle.snapshot().canceled {
                return Err(WorkBuddyAuthError::new("登录已取消"));
            }

            let token_url = context.auth_url(&format!("/auth/token?state={}", urlencoding(&state)));
            let token_data = match send_public_request("GET", &token_url, None, &headers).await {
                Ok(response) => match unwrap_public_response(&response, "auth/token") {
                    Ok(data) => data,
                    Err(error) => {
                        // 未完成登录时上游返回 code=11217，轮询期间一律继续等待
                        if error.upstream_code == Some(SERVER_CODE_RETRY_FETCH_TOKEN) {
                            logging::verbose("[Auth]", "等待浏览器登录完成…");
                        } else {
                            logging::verbose("[Auth]", &format!("登录轮询中: {}", error.message));
                        }
                        continue;
                    }
                },
                Err(error) => {
                    logging::verbose("[Auth]", &format!("登录轮询中: {}", error.message));
                    continue;
                }
            };

            let access_token = token_data
                .get("accessToken")
                .and_then(Value::as_str)
                .unwrap_or("");
            if !access_token.is_empty() {
                let session = self
                    .build_session_from_token(&token_data, &state, &context)
                    .await?;
                let uid = session
                    .get("account")
                    .and_then(|account| account.get("uid"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if uid.is_empty() {
                    return Err(WorkBuddyAuthError::new(
                        "登录成功但获取账号信息失败（缺少 uid），请重试",
                    ));
                }
                let saved = self
                    .store
                    .add_account(&session, None)
                    .map_err(|error| {
                        WorkBuddyAuthError::with_status(error.status_code, error.message)
                    })?;
                let name = saved
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                logging::log(
                    "[Auth]",
                    &format!(
                        "✅ 登录成功：{name}（{}…），已加入账号列表",
                        crate::server::core::account_store::store_util::truncate_text(uid, 8)
                    ),
                );
                return Ok(session);
            }
            logging::verbose("[Auth]", "等待浏览器登录完成…");
        }
        Err(WorkBuddyAuthError::new(format!(
            "登录轮询超时（{} 分钟）",
            LOGIN_TIMEOUT_MS / 60000
        )))
    }

    /// 拿到 token 后拉账号：优先 `/login/account?state=`，再回退 `/accounts`
    /// （对照 `buildSessionFromToken`：两步都失败不影响登录本身）。
    async fn build_session_from_token(
        &self,
        auth: &Value,
        state: &str,
        context: &Context,
    ) -> Result<Value, WorkBuddyAuthError> {
        let enriched = with_expires_at(auth.clone());
        let mut account = Value::Object(serde_json::Map::new());
        let mut accounts: Vec<Value> = Vec::new();

        if !state.is_empty() {
            // 登录时机还没有任何账号上下文，因此这里的出口固定为直连
            // （Node 版登录请求同样不传 proxy）
            match self
                .auth
                .fetch_login_account(&enriched, state, context, None)
                .await
            {
                Ok(data) => {
                    if data.is_object() {
                        account = data;
                    }
                }
                Err(error) => logging::verbose(
                    "[Auth]",
                    &format!("login/account 拉取失败: {}", error.message),
                ),
            }
        }
        match self.auth.fetch_accounts(&enriched, context, None).await {
            Ok(list) => accounts = list,
            Err(error) => logging::log(
                "[Auth]",
                &format!("拉取账号列表失败（不影响登录）: {}", error.message),
            ),
        }
        let account_uid = account.get("uid").and_then(Value::as_str).unwrap_or("");
        if account_uid.is_empty() {
            let fallback = accounts
                .iter()
                .find(|item| {
                    item.get("lastLogin")
                        .map(|value| !value.is_null())
                        .unwrap_or(false)
                })
                .or_else(|| accounts.first())
                .cloned()
                .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
            account = fallback;
        }

        Ok(json!({
            "endpoint": context.base_url,
            "prefixPath": context.prefix,
            "platform": context.platform,
            "edition": context.edition,
            "auth": enriched,
            "account": account,
            "accounts": accounts,
            "lastRefreshTime": logging::now_ms(),
        }))
    }
}

/// 标记任务完成并写入会话摘要
fn finish_task(handle: &LoginTaskHandle, session: &Value) {
    let summary = json!({
        "accountUid": session
            .get("account")
            .and_then(|account| account.get("uid"))
            .cloned()
            .unwrap_or(Value::Null),
        "nickname": session
            .get("account")
            .and_then(|account| account.get("nickname"))
            .cloned()
            .unwrap_or(Value::Null),
        "edition": session
            .get("edition")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    });
    handle.update(|task| {
        task.done = true;
        task.session = Some(summary);
        task.finished_at = Some(logging::now_ms());
    });
}

/// 标记任务失败（保留 cancel 与否交给调用方判断日志措辞）
fn finish_task_error(handle: &LoginTaskHandle, message: &str) {
    let message = message.to_string();
    handle.update(|task| {
        task.done = true;
        task.error = Some(message.clone());
        task.finished_at = Some(logging::now_ms());
    });
}
