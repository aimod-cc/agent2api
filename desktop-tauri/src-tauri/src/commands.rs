//! 暴露给渲染层的命令。
//!
//! 设计取向：命令层只做「HTTP 转发 + 输入整形」，不做业务校验 ——
//! 与原 Electron 版一致（校验统一在后端 HTTP 层，避免同一套规则两处维护）。
//! 前端把各功能映射成 `api_request(method, path, body)` 调用，
//! 路径与入参整形集中在 bridge.js 里，便于对照排查。

use serde::Deserialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_autostart::ManagerExt as AutostartExt;
use tauri_plugin_dialog::DialogExt;

use crate::backend;
use crate::gateway;
use crate::login::{self, LoginState};
use crate::settings::{self, AppSettings};
use crate::state::AppState;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiRequest {
    pub method: String,
    pub path: String,
    #[serde(default)]
    pub body: Option<Value>,
}

/// 统一的管理 API 入口：返回后端 data，出错时返回可读消息。
#[tauri::command]
pub async fn api_request(request: ApiRequest) -> Result<Value, String> {
    gateway::call(&request.method, &request.path, request.body.as_ref()).await
}

/// 取原始文本（导出日志用）
#[tauri::command]
pub async fn api_request_text(method: String, path: String) -> Result<String, String> {
    gateway::call_text(&method, &path).await
}

/// 后端就绪状态：渲染层可在启动阶段据此显示提示
#[tauri::command]
pub async fn backend_status() -> Result<Value, String> {
    let port = gateway::proxy_port();
    let ready = backend::is_ready(port).await;
    Ok(json!({ "ready": ready, "port": port }))
}

/// 发起登录；阻塞到完成/失败/取消/超时
#[tauri::command]
pub async fn start_login(app: AppHandle, edition: String, mode: String) -> Result<Value, String> {
    login::start(&app, edition, mode).await
}

#[tauri::command]
pub fn login_state(app: AppHandle) -> LoginState {
    match login::current_login(&app) {
        Some(active) => LoginState {
            active: true,
            mode: Some(active.mode),
            edition: Some(active.edition),
        },
        None => LoginState { active: false, mode: None, edition: None },
    }
}

#[tauri::command]
pub async fn cancel_login(app: AppHandle) -> Result<Value, String> {
    login::cancel(&app).await?;
    Ok(json!({ "canceled": true }))
}

/// 导出运行日志：拉取 JSONL 原文，弹系统保存框落盘。
#[tauri::command]
pub async fn export_logs(app: AppHandle) -> Result<Value, String> {
    let text = gateway::call_text("GET", "/api/logs/download").await?;
    let count = text.lines().filter(|line| !line.trim().is_empty()).count();
    if count == 0 {
        return Ok(json!({ "count": 0 }));
    }

    let stamp = timestamp_for_filename();
    let default_name = format!("workbuddy-logs-{stamp}.jsonl");
    let file = tauri_plugin_dialog::DialogExt::dialog(&app)
        .file()
        .set_title("导出运行日志")
        .set_file_name(&default_name)
        .add_filter("JSON Lines", &["jsonl"])
        .add_filter("全部文件", &["*"])
        .blocking_save_file();

    let Some(target) = file else {
        return Ok(json!({ "canceled": true, "count": count }));
    };
    let path = target
        .into_path()
        .map_err(|error| format!("保存路径无效: {error}"))?;
    std::fs::write(&path, text.as_bytes()).map_err(|error| format!("写入日志文件失败: {error}"))?;
    Ok(json!({ "count": count, "file": path.to_string_lossy() }))
}

/// 读取应用设置。
///
/// `autostart` 以系统注册表的实际状态为准，而不是设置文件里的值：
/// 用户可能在任务管理器的「启动」页里单独禁用了本应用，
/// 那种情况下配置文件仍是 true，界面会显示成「已开启」，与实际不符。
#[tauri::command]
pub fn get_app_settings(app: AppHandle) -> AppSettings {
    let mut current = settings::load();
    if let Ok(enabled) = app.autolaunch().is_enabled() {
        current.autostart = enabled;
    }
    // 顺手把缓存与磁盘对齐：拦截关窗时要用到最新值
    app.state::<AppState>().window.set_close_to_tray(current.close_to_tray);
    current
}

/// 覆盖保存应用设置，返回保存后的结果。
///
/// `autostart` 变化时同步系统自启动登记；返回值里的 `autostart` 取插件
/// 反馈的实际结果，避免出现「界面显示已开启但注册表没写进去」。
#[tauri::command]
pub fn save_app_settings(app: AppHandle, patch: AppSettings) -> Result<AppSettings, String> {
    let mut saved = patch;

    let autolaunch = app.autolaunch();
    let currently_enabled = autolaunch.is_enabled().unwrap_or(false);
    if saved.autostart != currently_enabled {
        let result = if saved.autostart {
            autolaunch.enable()
        } else {
            autolaunch.disable()
        };
        if let Err(error) = result {
            return Err(format!("更新开机自启动失败: {error}"));
        }
        // 以系统实际状态回填：写注册表成功但被策略拦下时，这里能如实反映
        saved.autostart = autolaunch.is_enabled().unwrap_or(saved.autostart);
    }

    settings::save(&saved)?;
    // 立即生效：配置改完不用重启，下一次关窗就走新行为
    app.state::<AppState>().window.set_close_to_tray(saved.close_to_tray);
    Ok(saved)
}

/// 导出账号：拉取导出数据，弹系统保存框落盘。
///
/// 账号数为 0 时直接返回，不弹保存框 —— 让用户选完路径再被告知「没东西可存」
/// 是纯打扰。
#[tauri::command]
pub async fn export_accounts(app: AppHandle) -> Result<Value, String> {
    let data = gateway::call("GET", "/api/accounts/export", None).await?;
    let accounts = data
        .get("accounts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if accounts.is_empty() {
        return Ok(json!({ "count": 0 }));
    }

    let stamp = timestamp_for_filename();
    let default_name = format!("workbuddy-accounts-{stamp}.json");
    let file = app
        .dialog()
        .file()
        .set_title("导出账号")
        .set_file_name(&default_name)
        .add_filter("JSON", &["json"])
        .add_filter("全部文件", &["*"])
        .blocking_save_file();

    let Some(target) = file else {
        return Ok(json!({ "canceled": true }));
    };
    let path = target
        .into_path()
        .map_err(|error| format!("保存路径无效: {error}"))?;
    let text = serde_json::to_string_pretty(&data)
        .map_err(|error| format!("导出内容序列化失败: {error}"))?;
    std::fs::write(&path, text.as_bytes()).map_err(|error| format!("写入账号文件失败: {error}"))?;
    Ok(json!({ "count": accounts.len(), "file": path.to_string_lossy() }))
}

/// 从文件导入账号（merge 语义：按 uid 匹配，命中更新、未命中追加）。
///
/// 兼容两种形态：整体导出文件 `{ version, exportedAt, accounts }`
/// 与直接的账号数组 `[...]`，统一取成 accounts 数组再提交给后端。
#[tauri::command]
pub async fn import_accounts(app: AppHandle) -> Result<Value, String> {
    let file = app
        .dialog()
        .file()
        .set_title("导入账号")
        .add_filter("JSON", &["json"])
        .add_filter("全部文件", &["*"])
        .blocking_pick_file();

    let Some(target) = file else {
        return Ok(json!({ "canceled": true }));
    };
    let path = target
        .into_path()
        .map_err(|error| format!("文件路径无效: {error}"))?;
    let text = std::fs::read_to_string(&path)
        .map_err(|error| format!("读取文件失败: {error}"))?;

    let parsed: Value = serde_json::from_str(&text)
        .map_err(|error| format!("文件不是有效 JSON: {error}"))?;
    let accounts = match parsed {
        // 整体导出文件
        Value::Object(ref map) => map
            .get("accounts")
            .and_then(Value::as_array)
            .cloned()
            .ok_or("文件中缺少 accounts 字段")?,
        // 直接就是账号数组
        Value::Array(items) => items,
        _ => return Err("文件内容既不是导出文件也不是账号数组".to_string()),
    };

    gateway::call(
        "POST",
        "/api/accounts/import",
        Some(&json!({ "accounts": accounts, "mode": "merge" })),
    )
    .await
}

/// 检查新版本：把本应用版本作为 query 参数交给后端比较。
///
/// 版本号必须由壳提供 —— 后端以独立进程运行，不知道自己被哪个壳打包，
/// 缺了它后端只能回报「最新版本是多少」，无法判断「是否有更新」。
/// 取值用运行时的 package_info（打包配置里的版本），而不是编译期常量：
/// 版本号在 Cargo.toml 与 tauri.conf.json 各有一份，前者可能与实际安装包不一致。
#[tauri::command]
pub async fn check_update(app: AppHandle) -> Result<Value, String> {
    let current = app.package_info().version.to_string();
    let path = format!("/api/update/check?current={}", urlencoding(&current));
    gateway::call("GET", &path, None).await
}

/// 下载安装包（后端负责联网与落盘，这里只转发参数）
#[tauri::command]
pub async fn download_update(url: String, name: Option<String>) -> Result<Value, String> {
    let payload = json!({ "url": url, "name": name.unwrap_or_default() });
    gateway::call("POST", "/api/update/download", Some(&payload)).await
}

/// 下载进度（前端轮询）
#[tauri::command]
pub async fn update_progress() -> Result<Value, String> {
    gateway::call("GET", "/api/update/progress", None).await
}

/// 取消下载
#[tauri::command]
pub async fn cancel_update() -> Result<Value, String> {
    gateway::call("POST", "/api/update/cancel", Some(&json!({}))).await
}

/// 运行已下载的安装包。
///
/// 路径必须通过 `update::verify_installer` 的校验（存在、.exe、位于下载目录内），
/// 否则这个命令就成了「执行任意程序」的入口。
///
/// `restart` 为 true 时：先把退出标志置位再退出，让出安装包要覆盖的文件占用
/// （不置位的话关窗逻辑会把退出拦成「最小化到托盘」，安装程序会卡在文件占用上）。
#[tauri::command]
pub fn run_installer(app: AppHandle, path: String, restart: Option<bool>) -> Result<Value, String> {
    let target = crate::update::verify_installer(&path)?;
    crate::update::launch_installer(&target, true)?;

    if restart.unwrap_or(true) {
        let state = app.state::<AppState>();
        state.begin_exit();
        // 稍留一点时间让命令的返回值先回到前端，再退出
        let handle = app.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(400));
            handle.exit(0);
        });
    }
    Ok(json!({ "launched": true, "path": target.to_string_lossy(), "restart": restart.unwrap_or(true) }))
}

/// 用系统默认浏览器打开 Release 页面。
///
/// 只允许 http(s)：直接交给系统打开器时，file:// 会变成「用默认程序打开本地文件」，
/// 等于给渲染层留了一个执行本机文件的口子。
#[tauri::command]
pub fn open_release_page(url: String) -> Result<Value, String> {
    let trimmed = url.trim();
    if !(trimmed.starts_with("https://") || trimmed.starts_with("http://")) {
        return Err("只允许打开 http(s) 链接".to_string());
    }
    open_in_browser(trimmed)?;
    Ok(json!({ "url": trimmed }))
}

/// 交给系统默认浏览器处理。
/// Windows 用 `cmd /C start`，注意 start 会把第一个带引号的参数当窗口标题，
/// 因此必须补一个空标题占位。
#[cfg(windows)]
fn open_in_browser(url: &str) -> Result<(), String> {
    std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("打开浏览器失败: {error}"))
}

#[cfg(not(windows))]
fn open_in_browser(url: &str) -> Result<(), String> {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    std::process::Command::new(opener)
        .arg(url)
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("打开浏览器失败: {error}"))
}

/// 最小化的 query 转义：版本号只含数字与点，做一层保险即可
fn urlencoding(text: &str) -> String {
    text.chars()
        .map(|ch| match ch {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => ch.to_string(),
            other => format!("%{:02X}", other as u32 & 0xFF),
        })
        .collect()
}

/// 本地时间戳（yyyy-MM-dd-HH-mm-ss），避免引入时间格式化依赖
fn timestamp_for_filename() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // 从 UTC 秒换算本地日期时间：这里只用于文件名，用 UTC+8 近似即可
    let secs = now + 8 * 3600;
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    // civil_from_days：把 1970-01-01 起的天数换算成年月日
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };

    format!("{year:04}-{month:02}-{day:02}-{hour:02}-{minute:02}-{second:02}")
}

/// 启动维护：临期 token 自动刷新一次，再批量查询余额，结果推给渲染层。
/// 任一环节失败只记日志，不影响窗口使用（与原 Electron 版行为一致）。
pub async fn startup_maintenance(app: AppHandle) {
    /// 与后端 PROACTIVE_REFRESH_MARGIN_MS 对齐：5 分钟内到期即视为临期
    const REFRESH_MARGIN_MS: f64 = 5.0 * 60.0 * 1000.0;

    let snapshot = match gateway::call("GET", "/api/accounts", None).await {
        Ok(value) => value,
        Err(error) => {
            eprintln!("[startup] 读取账号列表失败: {error}");
            return;
        }
    };
    let accounts = snapshot
        .get("accounts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0);

    let mut refreshed = Vec::new();
    for account in &accounts {
        let available = account.get("available").and_then(Value::as_bool).unwrap_or(true);
        let has_refresh = account.get("hasRefreshToken").and_then(Value::as_bool).unwrap_or(false);
        let expires_at = account.get("expiresAt").and_then(Value::as_f64).unwrap_or(0.0);
        if !available || !has_refresh || expires_at <= 0.0 {
            continue;
        }
        if expires_at - now_ms >= REFRESH_MARGIN_MS {
            continue;
        }
        let Some(id) = account.get("id").and_then(Value::as_str) else { continue };
        match gateway::call("POST", "/api/accounts/refresh", Some(&json!({ "id": id }))).await {
            Ok(_) => refreshed.push(id.to_string()),
            Err(error) => eprintln!("[startup] 自动刷新 token 失败 {id}: {error}"),
        }
    }

    let balances = match gateway::call("GET", "/api/accounts/usage", None).await {
        Ok(value) => Some(value),
        Err(error) => {
            eprintln!("[startup] 自动查询余额失败: {error}");
            None
        }
    };

    let _ = app.emit("accounts:auto-maintained", json!({ "refreshed": refreshed, "balances": balances }));
    if !refreshed.is_empty() {
        eprintln!("[startup] 已自动刷新 {} 个临期账号的 Token", refreshed.len());
    }
}
