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

mod autoclaw;
mod catpaw;
mod qoder;

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
use crate::server::core::providers::adapter::adapter_for;
use crate::server::core::providers::raccoon::oauth;
use crate::server::core::providers::{kind_from_id, kind_id, ProviderKind, DEFAULT_PROVIDER_ID};
use crate::server::errors::GatewayError;
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
    /// 这次登录属于哪一家（provider id）。
    ///
    /// 为什么必须记在任务上：`/wait` 只按 state 查任务，而**回调换凭证**那一步
    /// （`submit_login_callback`）必须知道该调哪家的 `exchange_login_code`。
    /// 换凭证是网络动作，不能靠调用方再传一次 provider —— 那等于让客户端
    /// 决定「用哪家的协议解释这个 state」，把一条内部契约暴露成入参。
    ///
    /// 缺省（default）为空串，`new_handle_for_provider` 会写上一家；
    /// 空串在回调侧按「不认识」拒绝，不会静默落到某一家。
    pub provider: String,
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
        {
            let mut task = handle.lock();
            if task.done {
                return false;
            }
            task.canceled = true;
            task.done = true;
            task.error = Some("登录已取消".to_string());
            task.finished_at = Some(logging::now_ms());
        }
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
    /// AutoClaw OAuth 登录的额外状态（`state → PendingOauth`）。
    ///
    /// ── 为什么要单独一张表（不能塞进 LoginTaskState）────────────
    /// 那个结构是所有 provider 共用的、`/wait` 直接序列化它，往里加
    /// AutoClaw 专属字段会让每次 `/wait` 都多带一份别人用不到的负载，
    /// 也让「哪些字段是这家独有的」从类型上看不出来。见
    /// `login/autoclaw.rs` 的 `PendingOauth`。
    ///
    /// 生命周期与任务表一致（收尾时清掉）；进程重启后自然为空，那时回调
    /// 会被 `finish_autoclaw_oauth_callback` 判成「登录上下文已丢失」。
    autoclaw_oauth: Arc<Mutex<HashMap<String, autoclaw::PendingOauth>>>,
}

impl LoginService {
    pub fn new(auth: AuthService, store: AccountStore) -> Self {
        Self {
            auth,
            store,
            tasks: LoginTasks::new(),
            autoclaw_oauth: Arc::new(Mutex::new(HashMap::new())),
        }
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
        self.new_handle_for_provider(info, DEFAULT_PROVIDER_ID)
    }

    /// 同上，但显式指定 provider（网页登录的任务表要记住它，见 `LoginTaskState::provider`）。
    fn new_handle_for_provider(
        &self,
        info: &'static crate::server::core::endpoints::EditionInfo,
        provider: &str,
    ) -> LoginTaskHandle {
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
                provider: provider.to_string(),
                canceled: false,
                finished_at: None,
            })),
            ticket,
        }
    }

    /// 发起一次「网页登录」任务（trait 扩展 7 的入口，小浣熊走这条）。
    ///
    /// 与 [`Self::start`] 的区别：这条**不碰上游** —— authUrl 与 state 都由
    /// provider 适配器自己拼（小浣熊的授权页地址是静态的，state 本地生成），
    /// 拿到回调里的 code 之前没有任何网络动作。因此没有后台任务、没有轮询，
    /// 任务句柄登记进表后等着 `submit_login_callback` 来收尾（或等 `cancel`）。
    ///
    /// 返回 `Err(原因)` 表示这家不支持网页登录（调用方据此报 400，文案原样透出）。
    pub fn start_web_login(&self, kind: ProviderKind) -> Result<LoginTaskHandle, String> {
        let label = crate::server::core::providers::meta(kind).label;
        let adapter = adapter_for(kind);
        // 先问能力再问地址：两个方法各有分工（`supports_web_login` 是恒定能力，
        // `build_login_url` 是这次的地址）。分开问才能把「这家没有这条协议」
        // 与「这家有协议但这次拼不出地址」在日志和文案里区分开。
        if !adapter.supports_web_login() {
            return Err(format!(
                "{label}不支持网页登录，请改用「填写凭证」或「导入桌面端登录态」添加账号"
            ));
        }
        let Some((auth_url, state)) = adapter.build_login_url() else {
            return Err(format!("{label}未能生成网页登录授权地址，请重试"));
        };
        let info = resolve_edition(Some(DEFAULT_EDITION));
        let handle = self.new_handle_for_provider(info, kind_id(kind));
        handle.update(|task| {
            task.state = Some(state.clone());
            task.auth_url = Some(auth_url);
        });
        self.tasks.register(&state, handle.clone());
        logging::log("[Login]", &format!("发起{label}网页登录（等待浏览器回调…）"));
        Ok(handle)
    }

    /// 发起一次 **Cline 设备授权登录**（WorkOS RFC 8628）。
    ///
    /// ── 与前两条链路的区别（为什么是第三种形态）─────────────────
    ///   - `start`（workbuddy）：起后台任务问上游要 state/authUrl，**上游推**
    ///     authUrl 过来；
    ///   - `start_web_login`（小浣熊）：本地拼授权地址，等**浏览器回调**带 code；
    ///   - 本方法（Cline）：**同步问上游要 user_code 与授权页地址**（一次
    ///     POST），然后把地址回给界面；用户确认后由后台任务**轮询**换令牌。
    ///
    /// 三种形态的共同点是「前端只认 `{state, authUrl}` 这两个键」，因此对界面
    /// 而言它们是同一个东西：给一个 URL 去打开、等 `/wait` 返回结果。
    /// Cline 的 `authUrl` 用上游给的 `verification_uri_complete`（已带 user_code，
    /// 用户点开就免手输），`state` 用设备授权返回的 `deviceCode` 指纹 ——
    /// 它既是任务的表键，也是轮询时认这一轮的凭据。
    ///
    /// ── 后台任务为什么必须有 ────────────────────────────────────
    /// 用户确认是在浏览器里发生的，网关这边只能轮询。所以拿完 user_code 就要
    /// 起任务去轮询（`poll_and_register` 会一直等到确认 / 超时 / 取消），
    /// 结果写回任务句柄，前端照 `/wait` 的既有协议取。
    ///
    /// ── `provider` 传什么 ───────────────────────────────────────
    /// `"cline-free"` / `"cline-pass"` —— 登录落账号时进哪一家。
    /// **登录流程本身与池无关**（只有 api.cline.bot 一台站点、一套设备授权），
    /// 池只决定落的账号记录属于哪家、能广告哪批模型。
    pub async fn start_cline_device_login(
        &self,
        provider: &str,
        name: Option<String>,
    ) -> Result<LoginTaskHandle, String> {
        let label = crate::server::core::providers::label_of(provider);
        // 第一步要拿 user_code：这一步是同步等待的（一次 POST，30 秒超时在
        // `cline::login` 里），因为它决定 authUrl —— 拿不到就没有页面可给用户开。
        let start = crate::server::core::providers::cline::login::start()
            .await
            .map_err(|error| error.message)?;
        // state 用 device_code 的前缀：够稳定（同一轮设备授权唯一）、够短
        // （进表键与日志），且不把完整的 device_code 暴露到界面上
        let state = format!(
            "cline-{}",
            crate::server::core::account_store::store_util::truncate_text(&start.device_code, 12)
        );
        let auth_url = start
            .verification_uri_complete
            .clone()
            .unwrap_or_else(|| start.verification_uri.clone());
        let info = resolve_edition(Some(DEFAULT_EDITION));
        let handle = self.new_handle_for_provider(info, provider);
        handle.update(|task| {
            task.state = Some(state.clone());
            task.auth_url = Some(auth_url.clone());
        });
        self.tasks.register(&state, handle.clone());
        logging::log(
            "[Login]",
            &format!("发起{label}设备授权登录（授权码 {}）", start.user_code),
        );
        // 后台轮询：用户确认后换令牌、落账号、写回句柄
        let service = self.clone();
        let task_state = state.clone();
        let task_provider = provider.to_string();
        tokio::spawn(async move {
            let store = service.store.clone();
            let result = crate::server::core::providers::cline::login::poll_and_register(
                &store,
                &start,
                &task_provider,
                name.as_deref(),
            )
            .await;
            let Some(handle) = service.tasks.get(&task_state) else {
                // 任务已被 cancel / 过期回收：结果无处可写（账号已经落地，
                // 用户下次刷新列表就能看到 —— 这里只记一条日志）
                logging::verbose(
                    "[Login]",
                    "Cline 登录任务已不在表中，结果未写回（账号仍已保存）",
                );
                return;
            };
            match result {
                Ok(account_id) => {
                    let account = store
                        .cline_account_record(&task_provider, &account_id)
                        .and_then(|record| {
                            record
                                .get("account")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                        })
                        .unwrap_or_default();
                    // 复用 workbuddy 那条链路的会话摘要形状，前端不需要新分支
                    finish_task(
                        &handle,
                        &json!({
                            "account": { "uid": account_id, "nickname": account },
                            "edition": "",
                        }),
                    );
                }
                Err(error) => finish_task_error(&handle, &error.message),
            }
        });
        Ok(handle)
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

    /// 收一次**网页登录回调**：校验 state → 换凭证 → 落账号 → 标记任务完成。
    ///
    /// ── 为什么回调要由登录窗口提交进来 ──────────────────────────
    /// 小浣熊的官方回调是自定义协议 `office-raccoon://auth/callback`。Electron
    /// 版能在会话内 `protocol.handle('office-raccoon', …)` 接管它，Tauri/WebView2
    /// **没有等价物**：WebView2 的 NavigationStarting 只是「导航前问一句要不要
    /// 拦」，而自定义协议导航一旦放行就会交给系统处理（本机没注册该协议时是
    /// 一个失败页）。因此壳侧的做法是：**在导航拦截里认出回调 URL、拦下导航、
    /// 把 URL 原样 POST 到本接口**（见 `src-tauri/src/login.rs`）。
    ///
    /// ── state 校验（安全口径）───────────────────────────────────
    /// 这一步是整条链路上**唯一**能确认「这个 code 是本次登录的回调」的地方：
    /// 任务表按 state 索引，且 state 是本进程刚生成的不可预测随机串。少了它，
    /// 任何本机程序都能构造一个回调 URL 让网关用**别人的** code 换凭证并落进
    /// 本机账号库（等于把陌生人的登录态塞给用户）。因此：
    ///   1. 任务必须存在（不存在 → 404「请重新发起」）；
    ///   2. 回调里的 state 必须与任务里的逐字一致（`parse_callback_code`）；
    ///   3. 任务必须还没结束（重复提交同一个回调 → 直接返回成功，不重复换码，
    ///      因为授权码是一次性的：重复调用只会拿到 200035「已失效」的错误，
    ///      把一次成功登录变成一次失败）。
    ///
    /// 返回成功时的账号 id（前端不用它，留给日志与将来可能的「登录后高亮新账号」）。
    pub async fn submit_login_callback(
        &self,
        state: &str,
        callback_url: &str,
    ) -> Result<String, GatewayError> {
        let state = state.trim();
        if state.is_empty() {
            return Err(GatewayError::with_status(400, "缺少 state，无法确认这次回调归属"));
        }
        let Some(handle) = self.tasks.get(state) else {
            return Err(GatewayError::with_status(
                404,
                "登录任务不存在或已过期，请重新发起网页登录",
            ));
        };
        let task = handle.snapshot();
        if task.canceled {
            return Err(GatewayError::with_status(400, "登录已取消，请重新发起"));
        }
        if task.done {
            // 幂等：同一个回调被送来两次（深链 + 导航各触发一次）不是错误
            return Ok(String::new());
        }
        // provider 由任务记着，不由调用方决定（见 LoginTaskState::provider）
        let Some(kind) = kind_from_id(&task.provider) else {
            return Err(GatewayError::with_status(
                500,
                format!("登录任务记录的提供商「{}」无法识别", task.provider),
            ));
        };
        if kind != ProviderKind::Raccoon {
            return Err(GatewayError::with_status(400, "该登录任务不接收授权码回调"));
        }
        let code = match oauth::parse_callback_code(callback_url, state) {
            Ok(code) => code,
            Err(error) => {
                // 校验失败也要落定任务：否则前端会一直等到 5 分钟超时
                finish_task_error(&handle, &error.message);
                logging::log("[Login]", &format!("❌ 网页登录回调校验失败: {}", error.message));
                return Err(error);
            }
        };
        match adapter_for(kind).exchange_login_code(&self.store, &code, state).await {
            Ok(account_id) => {
                let session = json!({
                    "accountUid": account_id,
                    "nickname": Value::Null,
                    "edition": task.edition,
                    "provider": task.provider,
                });
                handle.update(|task| {
                    task.done = true;
                    task.session = Some(session);
                    task.finished_at = Some(logging::now_ms());
                });
                Ok(account_id)
            }
            Err(error) => {
                finish_task_error(&handle, &error.message);
                logging::log("[Login]", &format!("❌ 网页登录换取凭证失败: {}", error.message));
                Err(error)
            }
        }
    }

    /// 后台起一个登录任务，并把「失败/完成」写回任务句柄。
    fn spawn_login(&self, handle: LoginTaskHandle, edition: String) {
        let this = self.clone();
        crate::spawn_task(async move {
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
