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

/// 后端就绪状态：渲染层可在启动阶段据此显示提示。
///
/// 返回的不只是「就绪与否」，还有**为什么没就绪** —— 端口冲突的详情
/// （性质、占用者、能否靠结束进程解决）都在 `failure` 里。界面据此决定
/// 是显示「网关启动中」还是给出「结束占用进程 / 更换端口」两个出口。
///
/// 之所以做成可反复查询而不是只发一次事件：事件可能在界面订阅之前就发出去了
/// （窗口还在加载），那时用户只能看到一个灰着的状态灯，不知道发生了什么。
#[tauri::command]
pub async fn backend_status(app: AppHandle) -> Result<Value, String> {
    let port = gateway::proxy_port();
    let ready = backend::is_ready(port).await;
    // 失败详情存在 AppState 里（由 ensure_ready 的失败分支写入）；
    // 端口已被显式指定过（环境变量）时，界面不该再提供「更换端口」——
    // 改设置也不会生效，只会让用户白忙一场
    let failure = app
        .state::<AppState>()
        .backend
        .lock()
        .ok()
        .and_then(|guard| guard.failure.clone());
    Ok(json!({
        "ready": ready,
        "port": port,
        "portFromEnv": gateway::port_from_env().is_some(),
        "failure": failure,
    }))
}

/// 重启整个应用。
///
/// ── 为什么是整进程重启，而不是「进程内重建后端」 ──────────────
/// 服务端有一批**进程级单例**（模型目录、定时签到调度、更新管理器），它们用
/// `OnceLock` 装载：第二次 `ServerState::bootstrap` 装不进全局，于是新的
/// ServerState 会拿着一份与全局不同的句柄 —— 停机时 `stop_global()` 停的是
/// 旧那份，新起的调度循环没人管。整进程重启让所有单例从头初始化，不存在这种
/// 分叉；代价是窗口会重新打开（约一秒），换来的是「重启后状态一定是对的」。
///
/// ── 顺序上的两个要点 ────────────────────────────────────────
///   1. **必须先 `begin_exit()`**：否则 `RunEvent::ExitRequested` 会被
///      「关闭到托盘」那条规则拦下（`api.prevent_exit()`），重启请求落空；
///   2. **延迟再退**：让本命令的返回值先回到前端，界面才能提示「正在重启」
///      而不是无声消失。
pub fn schedule_app_restart(app: &AppHandle) {
    app.state::<AppState>().begin_exit();
    let handle = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(400));
        // `restart()` 触发 ExitRequested → Exit（插件在此释放单实例互斥体）
        // → 拉起新进程 → 本进程退出。不返回。
        handle.restart();
    });
}

/// 重启应用（供界面在「结束占用进程」后重试启动，或用户手动触发）。
#[tauri::command]
pub async fn restart_app(app: AppHandle) -> Result<Value, String> {
    let port = gateway::proxy_port();
    schedule_app_restart(&app);
    Ok(json!({ "restarting": true, "port": port }))
}

/// 查占用网关端口的进程。
///
/// 端口没被占用（或查不到，例如系统保留段）时 `occupant` 为 null ——
/// 界面据此只给「更换端口」，不显示一个点了必然失败的「结束进程」。
#[tauri::command]
pub async fn port_occupant() -> Result<Value, String> {
    let port = gateway::proxy_port();
    let occupant = backend::occupant_of(port);
    Ok(json!({ "port": port, "occupant": occupant }))
}

/// 结束占用网关端口的进程，并在宽限期内确认端口真的释放。
///
/// 只结束 `port_occupant` 查到的那个 PID（不做按端口号的无差别清理：
/// 端口在两次查询之间易主时，无差别清理会杀掉无辜进程）。
/// 结束前由界面把进程名与路径显示给用户确认。
///
/// 成功后**不自动重启**：调用方（界面）需要先看到结果，再决定是否重启
/// 让网关重新启动（`restart_app`）。
#[tauri::command]
pub async fn end_port_occupant(app: AppHandle) -> Result<Value, String> {
    let port = gateway::proxy_port();
    let Some(occupant) = backend::occupant_of(port) else {
        return Err(format!(
            "端口 {port} 上没有查到监听进程，无法结束。\
             若该端口属于系统保留段，请改用「更换端口」。"
        ));
    };
    // 结束是阻塞的（OpenProcess + TerminateProcess + 等端口释放），
    // 放到阻塞线程池，别占着异步运行时的 worker
    let result = tauri::async_runtime::spawn_blocking(move || {
        let killed = backend::end_occupant(port, &occupant);
        // 无论结束是否成功都探一次端口现状：成功了要确认释放，
        // 失败了要告诉用户「现在端口上是什么情况」
        let release = backend::wait_port_released(port);
        (killed, release)
    })
    .await
    .map_err(|error| format!("结束进程任务失败: {error}"))?;

    let (killed, release) = result;
    // 结束失败（权限不足 / 是系统进程 / 是本程序自己）原样把说明透给界面
    killed.map_err(|conflict| conflict.message)?;
    let released = release.is_ok();
    if released {
        // 端口已释放：**清掉启动失败记录**。那条记录描述的是「启动时端口被占」，
        // 现在占用者已经不在了，留着会让界面一直显示「端口被占用」——
        // 而用户刚结束完进程，看到这个只会以为没生效。
        // 清掉后界面回到「网关启动中…」，如实反映「端口空着、网关还没起来」。
        if let Ok(mut guard) = app.state::<AppState>().backend.lock() {
            guard.failure = None;
        }
    }
    Ok(json!({ "port": port, "released": released }))
}

/// 校验一个端口能否用于监听（「更换端口」在保存前先试）。
///
/// 用真实的 bind 探测，而不是查 netsh 保留段：实测保留段列表里有些端口
/// 照样能绑上，列表既不充分也不必要（详见 port_conflict 模块头）。
/// 返回 `{ ok, message, conflict }` —— 不抛错，因为「端口不可用」是
/// 一种正常结果而非调用失败，界面要拿它的文案直接显示在输入框旁。
#[tauri::command]
pub async fn check_port(port: u16) -> Result<Value, String> {
    if port == 0 {
        return Ok(json!({ "ok": false, "message": "端口号必须在 1-65535 之间" }));
    }
    // 1024 以下的端口在 Windows 上通常需要管理员权限，直接拒绝能省掉
    // 一次「保存成功但重启后起不来」的往返
    if port < 1024 {
        return Ok(json!({
            "ok": false,
            "message": "端口 1-1023 是系统保留范围，普通程序无法监听，请用 1024 以上的端口",
        }));
    }
    let current = gateway::proxy_port();
    if port == current {
        return Ok(json!({ "ok": true, "message": "与当前端口相同，无需重启", "same": true }));
    }
    match crate::server::probe_port(port) {
        Ok(()) => Ok(json!({ "ok": true, "message": format!("端口 {port} 可用") })),
        Err(conflict) => Ok(json!({
            "ok": false,
            "message": conflict.message.clone(),
            "conflict": conflict,
        })),
    }
}

/// 更换网关端口：校验 → 写设置 → 重启应用。
///
/// 校验放在写盘之前，是为了让「端口不可用」被拦在设置文件之外 —— 否则文件里
/// 会留下一个起不来的端口，下次开机照样起不来。
///
/// 端口在服务端 bind 之后就改不了，所以必须重启（见 `schedule_app_restart`）。
#[tauri::command]
pub async fn change_port(app: AppHandle, port: u16) -> Result<Value, String> {
    if port < 1024 {
        return Err("端口 1-1023 是系统保留范围，请用 1024 以上的端口".to_string());
    }
    // 环境变量显式指定端口时，设置文件里的值不会生效 —— 与其静默失败，
    // 不如明确告诉用户该改哪里
    if gateway::port_from_env().is_some() {
        return Err(
            "当前端口由环境变量 AGENT2API_PROXY_PORT 指定，设置里的端口不会生效。\
             请修改该环境变量后重启程序。"
                .to_string(),
        );
    }
    if port == gateway::proxy_port() {
        return Ok(json!({ "changed": false, "port": port }));
    }
    // 真实 bind 探测：不可用就别写盘（详见 check_port 的说明）
    if let Err(conflict) = crate::server::probe_port(port) {
        return Err(conflict.message);
    }

    let mut current = settings::load();
    current.proxy_port = port;
    settings::save(&current)?;

    // 重启后新端口生效。不在这里等结果：重启会带走本进程，
    // 界面靠重连（页面重载）确认新端口是否可用。
    schedule_app_restart(&app);
    Ok(json!({ "changed": true, "port": port, "restarting": true }))
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
///
/// ── 为什么 `proxyPort` 要从磁盘上带过来 ──────────────────────
/// 本命令的调用方（设置页）只管「关闭到托盘」与「开机自启」两个开关，
/// 它发来的 patch 里没有端口字段 —— 而本命令是**全量覆盖**写入。
/// 若直接落盘，`proxyPort` 会被反序列化成 0（= 未设置），把用户通过
/// 「更换端口」设过的端口悄悄抹掉，下次开机又回到 3065 上撞冲突。
/// 端口不归这个命令管，就原样保留磁盘上的值。
#[tauri::command]
pub fn save_app_settings(app: AppHandle, patch: AppSettings) -> Result<AppSettings, String> {
    let mut saved = patch;
    // 端口由 change_port 专门负责，这里保持磁盘现值不被覆盖
    saved.proxy_port = settings::load().proxy_port;

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

/// 启动维护：让网关刷新一遍临期凭证，结果推给渲染层。
/// 任一环节失败只记日志，不影响窗口使用（与原 Electron 版行为一致）。
///
/// ── 余额查询为什么从这里移走了 ──────────────────────────────
/// 原先这里还会调一次 `GET /api/accounts/usage`。现在余额查询归「定时查询积分」
/// 这条定时任务（`scheduled_tasks` 的 backend 任务，默认每 10 分钟一次），
/// 而它的**首轮在网关 bootstrap 后就立即跑一次**（`seed_schedule` 把首次排到
/// 「现在」）—— 于是启动时该做的这一次查询依旧会发生，只是执行者换成了定时任务。
///
/// 两处各查一遍的代价是每个账号在启动瞬间被打两次上游积分接口：既无收益
/// （同一份数据），又平白多担一次风控风险。更关键的是「关掉定时查询积分 =
/// 启动也不查」这条一致性 —— 与其它定时任务（「关掉任务 = 启动也不刷」）同款，
/// 留着这里这一份会让那条开关变得半失效。
///
/// 界面因此改为读定时任务的结果快照（`/api/accounts/usage/snapshot`），
/// 由 `usage-actions.js` 的 `syncSnapshot` 应用 —— 手动点「查询积分」那条路径
/// 不受影响，它仍走 `GET /api/accounts/usage` 当场取。
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

    let _ = app.emit("accounts:auto-maintained", json!({ "refreshed": refreshed }));
    if !refreshed.is_empty() {
        eprintln!("[startup] 已自动刷新 {} 个临期账号的 Token", refreshed.len());
    }
}
