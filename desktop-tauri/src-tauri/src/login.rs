//! 登录流程：拉起登录窗口（内嵌 WebView 或系统浏览器）并轮询后端直到完成。
//!
//! 后端把登录拆成三步：start 拿 state+authUrl → 用户在页面上完成 →
//! 轮询 wait 拿结果。桌面端只负责托管页面与轮询，不接触任何凭证。

use std::time::Duration;

use serde_json::json;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

use crate::gateway;
use crate::state::ActiveLogin;

pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const LOGIN_WINDOW_LABEL: &str = "login";

/// 登录窗口允许导航的域名，取自两版 cli/product.json 的
/// internalDomain / externalDomain / iOADomain，外加扫码登录所需的微信/QQ 域名。
const ALLOWED_HOSTS: &[&str] = &[
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

fn host_allowed(raw_url: &str) -> bool {
    let Ok(url) = url::Url::parse(raw_url) else {
        return false;
    };
    match url.scheme() {
        "about" | "data" => true,
        "https" | "http" => {
            let Some(host) = url.host_str() else {
                return false;
            };
            let host = host.to_lowercase();
            ALLOWED_HOSTS
                .iter()
                .any(|allowed| host == *allowed || host.ends_with(&format!(".{allowed}")))
        }
        _ => false,
    }
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginState {
    pub active: bool,
    pub mode: Option<String>,
    pub edition: Option<String>,
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

/// 读取后端登录状态：`{ pending: true }` 表示还在等待，
/// 否则可能带 error，或表示成功（无 pending / error）。
async fn poll_once(state: &str) -> Result<Option<String>, String> {
    let path = format!("/api/session/login/wait?state={state}");
    match gateway::call("GET", &path, None).await {
        Ok(value) => {
            if value.get("pending").and_then(serde_json::Value::as_bool) == Some(true) {
                return Ok(None);
            }
            if let Some(error) = value.get("error").and_then(serde_json::Value::as_str) {
                return Err(error.to_string());
            }
            Ok(Some(String::new()))
        }
        Err(error) => Err(error),
    }
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
    emit_login_state(app, LoginState { active: false, mode: None, edition: None });
    Ok(())
}

/// 发起一次登录。阻塞到登录完成、失败、取消或超时。
pub async fn start(app: &AppHandle, edition: String, mode: String) -> Result<serde_json::Value, String> {
    if current_login(app).is_some() {
        return Err("已有登录流程在进行，请先完成当前登录或等待超时".to_string());
    }

    let edition_id = if edition == "intl" { "intl" } else { "cn" };
    let edition_label = if edition_id == "intl" { "国际版" } else { "国内版" };
    let use_external = mode == "external";

    let started = gateway::call(
        "POST",
        "/api/session/login/start",
        Some(&json!({ "edition": edition_id })),
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
        });
    }
    emit_login_state(
        app,
        LoginState {
            active: true,
            mode: Some(if use_external { "external".into() } else { "embedded".into() }),
            edition: Some(edition_id.to_string()),
        },
    );

    let result = if use_external {
        open_external(app, &auth_url, edition_label).await
    } else {
        run_embedded(app, &auth_url, edition_label, &login_state).await
    };

    {
        let state = app.state::<crate::state::AppState>();
        if let Ok(mut guard) = state.login.lock() {
            *guard = None;
        };
    }
    emit_login_state(app, LoginState { active: false, mode: None, edition: None });
    result
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
            Ok(None) => continue,
            Ok(Some(_)) => return Ok(json!({ "ok": true, "external": true })),
            Err(error) => {
                // 登录任务被后端清理（已取消/已过期）时直接结束，不必等满 5 分钟
                if error.contains("登录任务不存在") || error.contains("已过期") {
                    return Ok(json!({ "ok": false, "canceled": true }));
                }
                eprintln!("[login] 查询登录进度失败: {error}");
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
async fn run_embedded(
    app: &AppHandle,
    auth_url: &str,
    edition_label: &str,
    login_state: &str,
) -> Result<serde_json::Value, String> {
    if let Some(existing) = app.get_webview_window(LOGIN_WINDOW_LABEL) {
        let _ = existing.close();
    }
    let url = url::Url::parse(auth_url).map_err(|error| format!("登录链接无效: {error}"))?;

    let window = WebviewWindowBuilder::new(app, LOGIN_WINDOW_LABEL, WebviewUrl::External(url))
        .title(format!("登录 WorkBuddy {edition_label}账号"))
        .inner_size(1100.0, 820.0)
        .min_inner_size(760.0, 560.0)
        .center()
        // 登录页只允许在上游白名单域名之间跳转：登录页会经过 SSO 中转，
        // 若页面被注入任意跳转，凭据可能被带到第三方站点
        .on_navigation(|url| host_allowed(url.as_str()))
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
            Ok(None) => continue,
            Ok(Some(_)) => break Ok(json!({ "ok": true })),
            Err(error) => {
                if error.contains("登录任务不存在") || error.contains("已过期") {
                    break Ok(json!({ "ok": false, "canceled": true }));
                }
                eprintln!("[login] 查询登录进度失败: {error}");
                continue;
            }
        }
    };

    if let Some(window) = app.get_webview_window(LOGIN_WINDOW_LABEL) {
        let _ = window.close();
    }
    outcome
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
