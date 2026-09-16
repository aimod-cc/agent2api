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

/// 用系统默认浏览器打开链接
fn open_in_browser(url: &str) -> Result<(), String> {
    #[cfg(windows)]
    {
        // cmd /c start：空字符串是窗口标题占位，否则 start 会把带引号的 URL 当标题
        std::process::Command::new("cmd")
            .args(["/c", "start", "", url])
            .spawn()
            .map_err(|error| format!("打开系统浏览器失败: {error}"))?;
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(url)
            .spawn()
            .map_err(|error| format!("打开系统浏览器失败: {error}"))?;
        return Ok(());
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open")
            .arg(url)
            .spawn()
            .map_err(|error| format!("打开系统浏览器失败: {error}"))?;
        return Ok(());
    }
    #[allow(unreachable_code)]
    Err("当前平台不支持打开系统浏览器".to_string())
}
