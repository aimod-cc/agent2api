//! 登录流程：拉起登录窗口（内嵌 WebView 或系统浏览器）并轮询后端直到完成。
//!
//! 后端把登录拆成三步：start 拿 state+authUrl → 用户在页面上完成 →
//! 轮询 wait 拿结果。桌面端只负责托管页面与轮询，不接触任何凭证。
//!
//! ── 两家 provider 的两条链（为什么这里要分流）────────────────
//!   - **workbuddy**：后端向自己上游要 state/authUrl，凭证由后端轮询上游拿回，
//!     桌面端**完全没有回调要处理**。系统浏览器模式（external）也能用，
//!     因为用户在哪登录都行 —— 判定归后端。
//!   - **小浣熊**：官方登录页在登录成功后跳转自定义协议
//!     `office-raccoon://auth/callback?code=…&state=…`，那个 code 必须由网关
//!     换成凭证（code 是给服务端的，不是给用户的）。**难点在于谁能拿到那个 URL**：
//!     Electron 版能在会话里 `session.protocol.handle('office-raccoon', …)` 接管
//!     自定义协议，Tauri 没有这个能力（Tauri 的 custom-protocol 是给自己 webview
//!     加载前端资源用的，不是系统级 URL scheme 注册），WebView2 的
//!     NavigationStarting 也只在**导航发生前**问一句要不要拦。因此这里改成：
//!     在导航拦截里认出回调 URL → **拦下导航** → 把 URL 原样 POST 给后端
//!     （`/api/session/login/callback`），由此完成「窗口自己捕获回调」。
//!     也正因如此，小浣熊**只提供内嵌窗口**：系统浏览器模式下那个深链要靠
//!     系统注册 `office-raccoon://` 才回得来（那是官方客户端注册的，装了才有），
//!     给用户一个「大概率永远收不到回调」的选项只会制造难查的卡死。
//!
//! ── User-Agent 为什么**不需要**清洗（与原 Electron 版的差别）─────
//! Electron 版的登录窗口带 `Electron/xx` 段，源实现特意把它抹掉（有些登录页会
//! 按 UA 拦非浏览器客户端）。Tauri/WebView2 这边没有这个问题：壳没有覆盖 UA
//! （`WebviewWindowBuilder::user_agent` 没被调用，wry 也不追加任何壳标识），
//! 于是登录窗口用的是 **WebView2 自己的标准 Edge UA**
//! （`Mozilla/5.0 … Chrome/… Edg/…`），本来就是一个普通浏览器的形态。
//! 主动去改它反而有两个代价：一是要自己拼一份 UA（版本号会过期），
//! 二是「UA 与 CH-UA 头不一致」在部分站点的风控里比「多一个 Electron 段」更显眼。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

use crate::gateway;
use crate::state::ActiveLogin;

pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const LOGIN_WINDOW_LABEL: &str = "login";

/// workbuddy 登录窗口允许导航的域名，取自两版 cli/product.json 的
/// internalDomain / externalDomain / iOADomain，外加扫码登录所需的微信/QQ 域名。
const WORKBUDDY_ALLOWED_HOSTS: &[&str] = &[
    "copilot.tencent.com",
    "staging-copilot.tencent.com",
    "codebuddy.cn",
    "workbuddy.cn",
    "codebuddy.ai",
    "staging-codebuddy.tencent.com",
    "workbuddy.ai",
    "staging.workbuddy.ai",
    "tencent.com",
    "qq.com",
    "wechat.com",
    "weixin.qq.com",
    "tenpay.com",
];

/// 小浣熊登录窗口允许导航的域名。
///
/// ── 为什么不是「只有 xiaohuanxiong.com」──────────────────────
/// 登录页本身在 `https://xiaohuanxiong.com/code/authorize`，但它把验证码 / 滑块
/// 之类的交互托管在第三方（阿里云、腾讯、百度都有），跳过去再跳回来是正常路径。
/// 所以这份清单**逐字取自原项目的 `LOGIN_ALLOWED_HOSTS`**（`desktop/main.cjs`）——
/// 那是同一条登录链路上经过实际使用验证的白名单，不是猜的；凭「同域就够」的
/// 判断砍成一个域名，会让「验证码加载不出来、登录窗口停在白屏」变成一个
/// 只有用户能碰到的新故障。
///
/// `RACCOON_MAIN_SITE_URL` 指向别处（测试环境）时这份清单**不跟着变**：
/// 一处能改导航白名单的配置项等于没有白名单，测试环境请改这里并重新编译。
const RACCOON_ALLOWED_HOSTS: &[&str] = &[
    "xiaohuanxiong.com",
    "sensetime.com",
    "aliyun.com",
    "aliyuncs.com",
    "alicdn.com",
    "qq.com",
    "baidu.com",
];

/// 小浣熊回调的形态（`office-raccoon://auth/callback`），与后端
/// `raccoon::oauth` 里的三个常量必须一致（那边负责最终校验，这里只做识别）。
const RACCOON_CALLBACK_SCHEME: &str = "office-raccoon";
const RACCOON_CALLBACK_HOST: &str = "auth";
const RACCOON_CALLBACK_PATH: &str = "/callback";

fn allowed_hosts(provider: &str) -> &'static [&'static str] {
    if provider == "raccoon" {
        RACCOON_ALLOWED_HOSTS
    } else {
        WORKBUDDY_ALLOWED_HOSTS
    }
}

/// 该 URL 是否允许在登录窗口里导航。
///
/// 注意 `office-raccoon:` 这类自定义协议在这里**一律返回 false**，所以导航拦截里
/// 必须先判回调再判白名单 —— 顺序反了会把回调当成非法导航拒掉，而那种拒绝
/// 和「拦下回调」在 WebView2 眼里是同一个动作，日志里看不出区别，
/// 最终表现为「用户登录成功了但网关一直没拿到 code」。
fn host_allowed(url: &url::Url, provider: &str) -> bool {
    match url.scheme() {
        "about" | "data" => true,
        "https" | "http" => {
            let Some(host) = url.host_str() else {
                return false;
            };
            let host = host.to_lowercase();
            allowed_hosts(provider)
                .iter()
                .any(|allowed| host == *allowed || host.ends_with(&format!(".{allowed}")))
        }
        _ => false,
    }
}

/// 这个 URL 是不是本 provider 的登录回调（逐项比对 scheme/host/path）。
///
/// host 不区分大小写：自定义协议在 `url` crate 里走 opaque host 解析，不像
/// http(s) 那样被规范化成小写，`office-raccoon://AUTH/callback` 会原样保留。
/// 这里只是**识别**（后端 `raccoon::oauth::parse_callback_code` 才是最终校验，
/// 它有一模一样的口径）；两边都宽一点，避免同一个 URL 在壳与后端得到不同结论。
fn is_login_callback(url: &url::Url, provider: &str) -> bool {
    if provider != "raccoon" {
        // workbuddy 的登录没有自定义协议回调（凭证由后端轮询上游取回），
        // 所以它这里恒为 false —— 别把「没有回调」误写成「什么都算回调」。
        return false;
    }
    let host_matches = url
        .host_str()
        .map(str::to_ascii_lowercase)
        .is_some_and(|host| host == RACCOON_CALLBACK_HOST);
    url.scheme() == RACCOON_CALLBACK_SCHEME && host_matches && url.path() == RACCOON_CALLBACK_PATH
}

/// 归一化前端传来的 provider id（缺省 workbuddy，只认这两家）。
fn normalize_provider(provider: &str) -> Result<&'static str, String> {
    match provider.trim() {
        "" | "workbuddy" => Ok("workbuddy"),
        "raccoon" => Ok("raccoon"),
        other => Err(format!("不支持网页登录的提供商：{other}")),
    }
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginState {
    pub active: bool,
    pub mode: Option<String>,
    pub edition: Option<String>,
    /// 进行中登录的 provider（前端据此复位正确的按钮，见 applyLoginState 的调用点）
    pub provider: Option<String>,
}

pub fn current_login(app: &AppHandle) -> Option<ActiveLogin> {
    app.state::<crate::state::AppState>()
        .login
        .lock()
        .ok()
        .and_then(|guard| guard.clone())
}

/// 把登录进行状态推给渲染层（弹窗按钮据此启用/禁用与显示取消）
fn emit_login_state(app: &AppHandle, state: LoginState) {
    let _ = app.emit("login:state", state);
}

/// 一次 `/wait` 的结果分类。
///
/// ── 为什么要把「任务失败」与「本地网关暂时读不到」分开 ────────
/// 原来的实现把两者都当成「打印一行、继续轮询」，于是**任何**失败都要等到
/// 5 分钟超时才反馈给用户，而且文案是「等待超时」——用户看不到真实原因
/// （小浣熊那边尤其明显：state 校验失败、授权码已失效都会被这条超时盖掉）。
/// 现在：任务已落定（done + error）就立刻结束等待并透出真实文案；
/// 只有传输层错误（本地网关瞬时不可达）才继续重试 —— 那类错误下一拍自愈，
/// 提前放弃反而会把一次正常的登录掐断。
enum PollOutcome {
    /// `{pending:true}`
    Pending,
    /// `{done:true, session:…}`
    Done,
    /// `{done:true, error:…}` —— 终态，文案原样给用户
    Failed(String),
    /// 读不到结果：本地网关的传输层错误（连接失败 / 非 2xx 信封）。
    ///
    /// 仍然带文案，因为其中一类**不是**「稍后会自愈」：用户取消或任务过期时
    /// 后端把任务从表里删掉，`/wait` 回 404「登录任务不存在或已过期」。
    /// 那种情况必须结束等待（见 `task_gone`），否则关掉弹窗后这个循环会一直
    /// 转到 5 分钟超时 —— 表现为「取消后要等 5 分钟按钮才恢复」。
    Unreachable(String),
}

/// 读一次后端登录状态。
async fn poll_once(state: &str) -> PollOutcome {
    let path = format!("/api/session/login/wait?state={state}");
    let value = match gateway::call("GET", &path, None).await {
        Ok(value) => value,
        Err(error) => return PollOutcome::Unreachable(error),
    };
    if value.get("pending").and_then(serde_json::Value::as_bool) == Some(true) {
        return PollOutcome::Pending;
    }
    if let Some(error) = value.get("error").and_then(serde_json::Value::as_str) {
        return PollOutcome::Failed(error.to_string());
    }
    PollOutcome::Done
}

/// 判断一条等待期错误是不是「任务已被后端清理」（取消 / 过期）。
/// 这两种情况下后端把任务从表里删掉了，`/wait` 回 404「登录任务不存在或已过期」。
fn task_gone(message: &str) -> bool {
    message.contains("登录任务不存在") || message.contains("已过期")
}

/// 通知后端中止轮询；用于用户关窗/点取消
pub async fn cancel(app: &AppHandle) -> Result<(), String> {
    let active = {
        let state = app.state::<crate::state::AppState>();
        let mut guard = state.login.lock().map_err(|_| "登录状态锁不可用")?;
        guard.take()
    };
    if let Some(active) = active {
        let _ = gateway::call(
            "POST",
            "/api/session/login/cancel",
            Some(&json!({ "state": active.state })),
        )
        .await;
    }
    emit_login_state(app, LoginState { active: false, mode: None, edition: None, provider: None });
    Ok(())
}

/// 发起一次登录。阻塞到登录完成、失败、取消或超时。
///
/// `provider` 缺省（空串）按 workbuddy 处理 —— 老版本界面不会传它，
/// 那条链的行为必须逐字保持。
pub async fn start(
    app: &AppHandle,
    edition: String,
    mode: String,
    provider: String,
) -> Result<serde_json::Value, String> {
    if current_login(app).is_some() {
        return Err("已有登录流程在进行，请先完成当前登录或等待超时".to_string());
    }
    let provider = normalize_provider(&provider)?;

    // 小浣熊：授权地址由**后端适配器**生成（静态地址 + 本地生成的 state），
    // 所以请求体里只带 provider，不带 edition（那是 workbuddy 的端点维度）。
    if provider == "raccoon" {
        return start_raccoon(app).await;
    }

    let edition_id = if edition == "intl" { "intl" } else { "cn" };
    let edition_label = if edition_id == "intl" { "国际版" } else { "国内版" };
    let use_external = mode == "external";

    let started = gateway::call(
        "POST",
        "/api/session/login/start",
        Some(&json!({ "edition": edition_id, "provider": provider })),
    )
    .await?;
    let login_state = started
        .get("state")
        .and_then(serde_json::Value::as_str)
        .ok_or("后端未返回登录状态")?
        .to_string();
    let auth_url = started
        .get("authUrl")
        .and_then(serde_json::Value::as_str)
        .ok_or("后端未返回登录链接，请检查网络")?
        .to_string();

    {
        let state = app.state::<crate::state::AppState>();
        let mut guard = state.login.lock().map_err(|_| "登录状态锁不可用")?;
        *guard = Some(ActiveLogin {
            state: login_state.clone(),
            edition: edition_id.to_string(),
            mode: if use_external { "external".into() } else { "embedded".into() },
            provider: provider.to_string(),
        });
    }
    emit_login_state(
        app,
        LoginState {
            active: true,
            mode: Some(if use_external { "external".into() } else { "embedded".into() }),
            edition: Some(edition_id.to_string()),
            provider: Some(provider.to_string()),
        },
    );

    let title = format!("登录 WorkBuddy {edition_label}账号");
    let result = if use_external {
        open_external(app, &auth_url, edition_label).await
    } else {
        run_embedded(app, provider, &auth_url, &title, &login_state).await
    };

    clear_active_login(app);
    result
}

/// 小浣熊网页登录：内嵌窗口 + 回调由窗口自己捕获提交（见模块头）。
///
/// 只给内嵌窗口一种方式，理由见模块头（系统浏览器模式下自定义协议深链
/// 需要系统注册 `office-raccoon://`，那是官方客户端装的，装了才有）。
async fn start_raccoon(app: &AppHandle) -> Result<serde_json::Value, String> {
    let started = gateway::call(
        "POST",
        "/api/session/login/start",
        Some(&json!({ "provider": "raccoon" })),
    )
    .await?;
    let login_state = started
        .get("state")
        .and_then(serde_json::Value::as_str)
        .ok_or("后端未返回登录状态")?
        .to_string();
    let auth_url = started
        .get("authUrl")
        .and_then(serde_json::Value::as_str)
        .ok_or("后端未返回登录链接，请检查网络")?
        .to_string();

    {
        let state = app.state::<crate::state::AppState>();
        let mut guard = state.login.lock().map_err(|_| "登录状态锁不可用")?;
        *guard = Some(ActiveLogin {
            state: login_state.clone(),
            edition: String::new(),
            mode: "embedded".into(),
            provider: "raccoon".to_string(),
        });
    }
    emit_login_state(
        app,
        LoginState {
            active: true,
            mode: Some("embedded".into()),
            edition: None,
            provider: Some("raccoon".to_string()),
        },
    );

    let result = run_embedded(app, "raccoon", &auth_url, "登录小浣熊账号", &login_state).await;
    clear_active_login(app);
    result
}

/// 清掉进行中的登录并推状态（成功/失败/取消三条出口共用）
fn clear_active_login(app: &AppHandle) {
    {
        let state = app.state::<crate::state::AppState>();
        if let Ok(mut guard) = state.login.lock() {
            *guard = None;
        };
    }
    emit_login_state(app, LoginState { active: false, mode: None, edition: None, provider: None });
}

/// 系统浏览器模式：浏览器与登录页共享登录态，完成后仍由后端轮询判定。
/// 用户关掉浏览器不影响等待（登录可能已完成）。
async fn open_external(
    app: &AppHandle,
    auth_url: &str,
    edition_label: &str,
) -> Result<serde_json::Value, String> {
    if std::env::var("WORKBUDDY_SKIP_OPEN_BROWSER").as_deref() != Ok("1") {
        open_in_browser(auth_url)?;
    }
    let deadline = tokio::time::Instant::now() + LOGIN_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(POLL_INTERVAL).await;
        // 已被取消（关弹窗/点取消）：停止轮询，不当作错误
        if current_login(app).is_none() {
            return Ok(json!({ "ok": false, "canceled": true }));
        }
        match poll_once(&login_state_of(app)?.as_str()).await {
            PollOutcome::Pending => continue,
            PollOutcome::Done => return Ok(json!({ "ok": true, "external": true })),
            PollOutcome::Failed(error) => {
                // 任务已落定失败（后端写进任务的终态错误）：立刻透出真实原因，
                // 不再把它盖成「等待超时」——小浣熊那条链上的「授权码已失效 /
                // state 校验失败」都是这一类。
                return Err(error);
            }
            PollOutcome::Unreachable(error) => {
                // 任务被后端清理（已取消 / 已过期，`/wait` 回 404）时直接结束，
                // 不必等满 5 分钟；其余传输层错误下一拍可能自愈，继续等。
                if task_gone(&error) {
                    return Ok(json!({ "ok": false, "canceled": true }));
                }
                eprintln!("[login] 读取登录进度失败（继续等待）: {error}");
                continue;
            }
        }
    }
    Err(format!("已打开系统浏览器完成{edition_label}登录，但等待超时（5 分钟），请重试"))
}

fn login_state_of(app: &AppHandle) -> Result<String, String> {
    current_login(app)
        .map(|active| active.state)
        .ok_or_else(|| "登录已取消".to_string())
}

/// 内嵌 WebView 模式：建独立窗口加载登录页，同时轮询后端。
///
/// `provider` 决定白名单与回调识别（小浣熊要捕获 `office-raccoon://` 深链）；
/// `title` 由调用方给出（两家窗口标题不同）。
async fn run_embedded(
    app: &AppHandle,
    provider: &'static str,
    auth_url: &str,
    title: &str,
    login_state: &str,
) -> Result<serde_json::Value, String> {
    if let Some(existing) = app.get_webview_window(LOGIN_WINDOW_LABEL) {
        let _ = existing.close();
    }
    let url = url::Url::parse(auth_url).map_err(|error| format!("登录链接无效: {error}"))?;

    // ── 回调只处理一次 ────────────────────────────────────────
    // 同一个回调 URL 可能从多个来源到达（WebView2 对「导航到自定义协议」在
    // 某些路径上会先触发 NavigationStarting 再派生新窗口请求，而放行后系统
    // 处理失败还可能再来一次）。授权码是**一次性**的：第二次提交只会从上游
    // 换来 200035「已失效」，把一次成功登录变成一次失败。这个标志与源项目
    // Electron 版的 `callbackSeen` 同一用途。
    let callback_seen = Arc::new(AtomicBool::new(false));
    let login_state_owned = login_state.to_string();

    // 导航拦截与弹窗拦截**共用同一个标志**：两种入口收到的可能是同一个回调
    // （页面先改 location、再被 WebView2 派生出一条弹窗请求），分开各一个标志
    // 就会提交两次，第二次拿到的必然是「授权码已失效」。
    let seen_for_nav = callback_seen.clone();
    let state_for_nav = login_state_owned.clone();
    let seen_for_window = callback_seen.clone();
    let state_for_window = login_state_owned.clone();

    let window = WebviewWindowBuilder::new(app, LOGIN_WINDOW_LABEL, WebviewUrl::External(url))
        .title(title.to_string())
        .inner_size(1100.0, 820.0)
        .min_inner_size(760.0, 560.0)
        .center()
        // 登录页只允许在上游白名单域名之间跳转：登录页会经过 SSO 中转，
        // 若页面被注入任意跳转，凭据可能被带到第三方站点。
        //
        // ── 为什么回调必须先于白名单判定 ────────────────────────
        // `host_allowed` 对自定义协议一律 false，先判白名单就会把回调当成
        // 「非法导航」拦掉 —— 拦掉的动作与「捕获回调」在 WebView2 眼里完全相同，
        // 症状是用户明明登录成功、网关却永远等不到 code。
        .on_navigation(move |url| {
            if is_login_callback(url, provider) {
                // 返回 false 即阻止这次导航（否则 WebView2 会把它交给系统：
                // 本机没注册 office-raccoon:// 时是一个错误页）。
                // 回调处理是网络动作，而本回调是同步的，因此 spawn 出去做。
                if !seen_for_nav.swap(true, Ordering::SeqCst) {
                    let login_state = state_for_nav.clone();
                    let callback_url = url.as_str().to_string();
                    tauri::async_runtime::spawn(async move {
                        submit_callback(&login_state, &callback_url).await;
                    });
                }
                return false;
            }
            host_allowed(url, provider)
        })
        // 弹出窗口（window.open）也是回调的可能入口：有的登录页在拿到授权码后会用
        // `window.open('office-raccoon://…')` 而不是直接改 location —— 那样它就只
        // 走 NewWindowRequested，导航拦截根本看不到，用户会看到「登录成功了但网关
        // 没反应」。这里同样**先判回调再统一拒绝**：
        //   - 是我们的回调 → 拦下并提交（去重标志共用，见上）；
        //   - 其余一律 Deny —— 这与不注册本回调时的默认行为**完全一致**
        //     （wry 在没有 handler 时对每个 NewWindowRequested 都 SetHandled(true)），
        //     所以这不是「放开弹窗」，只是把回调那一种从被静默丢弃变成被接住。
        .on_new_window(move |url, _features| {
            if is_login_callback(&url, provider) && !seen_for_window.swap(true, Ordering::SeqCst) {
                let login_state = state_for_window.clone();
                let callback_url = url.as_str().to_string();
                tauri::async_runtime::spawn(async move {
                    submit_callback(&login_state, &callback_url).await;
                });
            }
            tauri::webview::NewWindowResponse::Deny
        })
        .build()
        .map_err(|error| format!("打开登录窗口失败: {error}"))?;

    // 关窗即视为放弃登录：通知后端中止轮询，并让等待循环退出
    let handle = app.clone();
    let window_for_event = window.clone();
    // Tauri 的关闭事件无法直接 await 异步逻辑，用阻塞式的取消请求；
    // 这里只是一次本地 HTTP 调用，开销极小。
    window.on_window_event(move |event| {
        if let tauri::WindowEvent::CloseRequested { .. } = event {
            let app = handle.clone();
            let state_handle = app.state::<crate::state::AppState>();
            let taken = state_handle
                .login
                .lock()
                .ok()
                .and_then(|mut guard| guard.take());
            if let Some(active) = taken {
                let payload = json!({ "state": active.state });
                tauri::async_runtime::spawn(async move {
                    let _ = gateway::call("POST", "/api/session/login/cancel", Some(&payload)).await;
                });
            }
            let _ = window_for_event.close();
        }
    });

    let deadline = tokio::time::Instant::now() + LOGIN_TIMEOUT;
    let outcome = loop {
        if tokio::time::Instant::now() >= deadline {
            break Err("网页登录等待超时（5 分钟），请重试".to_string());
        }
        tokio::time::sleep(POLL_INTERVAL).await;

        // 窗口被关掉（或已取消）：结束等待
        if app.get_webview_window(LOGIN_WINDOW_LABEL).is_none() || current_login(app).is_none() {
            break Ok(json!({ "ok": false, "canceled": true }));
        }
        match poll_once(login_state).await {
            PollOutcome::Pending => continue,
            PollOutcome::Done => break Ok(json!({ "ok": true })),
            // 任务已落定失败（state 校验失败 / 授权码已失效 / 换取凭证失败）：
            // 立刻透出真实文案。继续轮询只会把它盖成 5 分钟后的「等待超时」，
            // 用户看不到任何可行动的原因。
            PollOutcome::Failed(error) => break Err(error),
            PollOutcome::Unreachable(error) => {
                // 任务已被后端清理（用户取消 / 过期）→ 与关窗同一种结局
                if task_gone(&error) {
                    break Ok(json!({ "ok": false, "canceled": true }));
                }
                eprintln!("[login] 读取登录进度失败（继续等待）: {error}");
                continue;
            }
        }
    };

    if let Some(window) = app.get_webview_window(LOGIN_WINDOW_LABEL) {
        let _ = window.close();
    }
    outcome
}

/// 把窗口捕获到的回调 URL 交给后端换凭证。
///
/// 失败只打日志：真正的错误文案由后端写进登录任务，随下一次 `/wait` 返回，
/// 前端与等待循环都从那里读（同一个出口，不必在这里造第二份文案）。
/// 等待循环每 2 秒看一次 `/wait`，回调整体是一次本地 HTTP 调用（毫秒级），
/// 因此不需要额外唤醒机制。
async fn submit_callback(login_state: &str, callback_url: &str) {
    let payload = json!({ "state": login_state, "callbackUrl": callback_url });
    match gateway::call("POST", "/api/session/login/callback", Some(&payload)).await {
        Ok(_) => eprintln!("[login] 已捕获登录回调并提交给网关"),
        Err(error) => eprintln!("[login] 提交登录回调失败: {error}"),
    }
}

/// 用系统默认浏览器打开链接。
///
/// 公开给 commands.rs 复用（打开 Release 页面）：它原来的实现是同一份
/// `cmd /C start`，有下面这个同样的 `&` 截断缺陷 —— 更新说明链接虽然大多
/// 不含查询串，但没有理由留着两份行为不一致的实现。
///
/// ── 为什么不用 `cmd /C start`（曾经的实现，两个真实故障的来源）──
/// 登录 URL 形如 `https://.../login?platform=workbuddy&state=<uuid>`，
/// 而 `cmd` 把 `&` 当**命令分隔符**：实际执行变成
///   ① `start "" https://.../login?platform=workbuddy`
///   ② `state=<uuid>`（一条不存在的命令）
/// 后果有两个，且都很难从现象反推：
///   1. 浏览器打开的登录页**没有 state** —— 用户能正常登录，但上游无法把
///      这次登录与本地发起的任务绑定，后端轮询 `auth/token` 永远返回
///      11217（登录中），前端一直卡在「等待登录完成」直到 5 分钟超时；
///   2. ② 那条命令让 cmd 报错，黑窗口一闪而过。
///
/// 因此改用 `ShellExecuteW`：直接交给 shell 打开 URL，不经过命令解释器，
/// 既不解析 `&`，也不创建控制台窗口。`SW_SHOWNORMAL` 让浏览器正常前台打开。
#[cfg(windows)]
pub fn open_in_browser(url: &str) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;

    const SW_SHOWNORMAL: i32 = 1;
    #[link(name = "shell32")]
    extern "system" {
        fn ShellExecuteW(
            hwnd: *mut std::ffi::c_void,
            operation: *const u16,
            file: *const u16,
            parameters: *const u16,
            directory: *const u16,
            show_cmd: i32,
        ) -> *mut std::ffi::c_void;
    }

    let to_wide = |text: &str| -> Vec<u16> {
        std::ffi::OsStr::new(text).encode_wide().chain(std::iter::once(0)).collect()
    };
    let operation = to_wide("open");
    let file = to_wide(url);

    // ShellExecuteW 的返回值 <= 32 表示失败（这是 Win32 的历史约定）
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            operation.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    if result as isize <= 32 {
        return Err(format!("打开系统浏览器失败（ShellExecute 返回 {result:?}）"));
    }
    Ok(())
}

/// 非 Windows 平台的等价实现（本项目的打包目标只有 Windows，
/// 保留分支是为了 `cargo check` 在其它平台也能过）
#[cfg(not(windows))]
pub fn open_in_browser(url: &str) -> Result<(), String> {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    // URL 作为独立 argv 传入，不经过 shell，`&` 不会被解释
    std::process::Command::new(opener)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("打开系统浏览器失败: {error}"))
}
