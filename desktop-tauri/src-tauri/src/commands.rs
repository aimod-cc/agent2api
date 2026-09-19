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

/// 发起登录；阻塞到完成/失败/取消/超时。
///
/// `provider` 是 Option：老版本界面不会传它，缺省（None）按 workbuddy 处理 ——
/// 那条链的行为必须逐字保持。`Option<String>` 在 Tauri 命令里就是「可以不传」，
/// 比给一个空串默认值更贴实地表达「老客户端没有这个概念」。
#[tauri::command]
pub async fn start_login(
    app: AppHandle,
    edition: String,
    mode: String,
    provider: Option<String>,
) -> Result<Value, String> {
    login::start(&app, edition, mode, provider.unwrap_or_default()).await
}

#[tauri::command]
pub fn login_state(app: AppHandle) -> LoginState {
    match login::current_login(&app) {
        Some(active) => LoginState {
            active: true,
            mode: Some(active.mode),
            edition: Some(active.edition),
            provider: Some(active.provider),
        },
        None => LoginState { active: false, mode: None, edition: None, provider: None },
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
///
/// 顺序很关键：必须在启动安装包之前显式回收后端进程。
/// 壳自己退出并不会带走 node 子进程（`exit(0)` 不保证触发 `RunEvent::Exit`），
/// NSIS 覆盖安装时 node.exe 仍占着可执行文件与 3065 端口，复制文件必然失败；
/// `backend::shutdown` 内部是同步的 kill + wait，返回时占用已经释放。
#[tauri::command]
pub fn run_installer(app: AppHandle, path: String, restart: Option<bool>) -> Result<Value, String> {
    let target = crate::update::verify_installer(&path)?;
    // 先让出 node.exe 的文件占用与 3065 端口，再让安装包去覆盖文件
    crate::backend::shutdown(&app.state::<AppState>());
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
///
/// 打开动作复用 login.rs 的实现（Windows 走 ShellExecuteW）：本地原来的
/// `cmd /C start` 会把 URL 里的 `&` 当命令分隔符，带查询串的链接会被截断，
/// 而且控制台窗口会闪一下。
#[tauri::command]
pub fn open_release_page(url: String) -> Result<Value, String> {
    let trimmed = url.trim();
    if !(trimmed.starts_with("https://") || trimmed.starts_with("http://")) {
        return Err("只允许打开 http(s) 链接".to_string());
    }
    crate::login::open_in_browser(trimmed)?;
    Ok(json!({ "url": trimmed }))
}

/// 设置主窗口主题（跟随界面深浅色切换）。
///
/// Windows 上这决定系统标题栏的深浅色（tao 内部走 DWMWA_USE_IMMERSIVE_DARK_MODE）：
/// 不设置时标题栏由操作系统按「系统主题」绘制，于是界面切到深色时标题栏仍是浅色，
/// 顶部就会出现一条刺眼的白带。传入 None 表示交回系统跟随，语义与 Tauri 一致。
#[tauri::command]
pub fn set_window_theme(app: AppHandle, theme: Option<String>) -> Result<(), String> {
    let theme = match theme.as_deref() {
        Some("dark") => Some(tauri::Theme::Dark),
        Some("light") => Some(tauri::Theme::Light),
        _ => None,
    };
    let window = app
        .get_webview_window(crate::MAIN_WINDOW_LABEL)
        .ok_or_else(|| "主窗口不存在".to_string())?;
    window.set_theme(theme).map_err(|error| format!("设置窗口主题失败: {error}"))
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

/// 启动维护：让网关刷新一遍临期凭证，再批量查询余额，结果推给渲染层。
/// 任一环节失败只记日志，不影响窗口使用（与原 Electron 版行为一致）。
pub async fn startup_maintenance(app: AppHandle) {
    // 临期凭证的刷新**交给网关自己**（POST /api/accounts/refresh-expiring）：
    // 「哪个账号该刷」是各家 provider 的知识（过期时间字段名、临期窗口四家
    // 各不相同），壳侧按字段名判断会漏（曾漏掉小浣熊的 `tokenExpiresAt`）。
    // 网关那边同时还有每 10 分钟的周期维护，这里这一次调用是为了让**刚启动的
    // 这一轮**尽快把状态刷对，而不是等第一个周期。
    //
    // 保留 `refreshed` 的语义（本次实际刷新成功的账号 id 列表）：渲染层的
    // `accounts:auto-maintained` 事件按它的长度决定要不要提示用户。
    let refreshed = match gateway::call("POST", "/api/accounts/refresh-expiring", None).await {
        Ok(report) => report
            .get("results")
            .and_then(Value::as_array)
            .map(|results| {
                results
                    .iter()
                    .filter(|item| item.get("status").and_then(Value::as_str) == Some("refreshed"))
                    .filter_map(|item| item.get("id").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect::<Vec<String>>()
            })
            .unwrap_or_default(),
        Err(error) => {
            eprintln!("[startup] 自动刷新临期凭证失败: {error}");
            Vec::new()
        }
    };

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
