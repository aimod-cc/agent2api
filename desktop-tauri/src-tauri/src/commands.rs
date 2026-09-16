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
