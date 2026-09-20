//! 数据保存位置（设置页「保存位置」）。
//!
//! ```text
//! GET  /api/storage            两类数据的当前目录、文件与大小
//! POST /api/storage/relocate   迁移到新目录（同步执行；迁移期间可轮询进度）
//! GET  /api/storage/progress   最近一次迁移的进度（无迁移时 active:false）
//! ```
//!
//! ── 与 /api/retention 的关系 ─────────────────────────────────
//! 保留期管「存多久」，这里管「存哪里」。两者都是动用户数据的低频管理接口，
//! 同挂 `protected`；写动作落在 `config::set_storage_dir`（config.json）与
//! 两个存储各自的 `relocate`（真正的搬家）。
//!
//! ── 迁移的执行模型 ──────────────────────────────────────────
//! POST 是**同步**的：handler 把搬迁扔进 `spawn_blocking`，等它跑完才返回 ——
//! 前端在等待期间轮询 `/progress` 画进度条，POST 返回即迁移结束。
//! 迁移本体持存储的 `inner` 锁（见 `LogStore::relocate` / `RequestStats::relocate`），
//! 期间的转发记账 / 写日志会排队等迁移完成。
//!
//! 成功后才写 config.json（目录键），且新目录等于配置目录时把键清掉 ——
//! 「搬回默认位置」不该在配置里固化一份绝对路径。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use axum::body::Bytes;
use axum::extract::State;
use axum::response::Response;
use serde_json::{json, Value};

use crate::server::config::{self, KEY_DEBUG_DIR, KEY_LOG_DIR, KEY_REQUEST_STATS_DIR};
use crate::server::errors;
use crate::server::http::{ok_json, parse_body};
use crate::server::logging;
use crate::server::ServerState;

/// 进行中的迁移进度（进程内唯一一份）。`None` = 当前没有迁移在跑。
///
/// 用 `std::sync::Mutex` 而不是 tokio 的锁：进度回调来自**阻塞线程**
/// （spawn_blocking 里的搬迁本体），临界区只有一次赋值，异步锁没有意义。
static RELOCATE: Mutex<Option<RelocateLive>> = Mutex::new(None);

/// 一条进行中的迁移：目标是哪类数据、当前完成百分比
struct RelocateLive {
    target: &'static str,
    percent: u32,
}

fn set_progress(target: &'static str, percent: u32) {
    if let Ok(mut guard) = RELOCATE.lock() {
        *guard = Some(RelocateLive {
            target,
            percent: percent.min(100),
        });
    }
}

fn clear_progress() {
    if let Ok(mut guard) = RELOCATE.lock() {
        *guard = None;
    }
}

/// 文件大小；不存在（还没写过盘）按 0 算
fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
}

/// GET /api/storage —— 两类数据的当前目录与文件概况（设置页渲染用）。
///
/// `custom` 表示目录是不是用户自定义的（≠ 配置目录）；`bytes` 供界面
/// 显示「迁移多大」；`count` 是条数（日志 500 条上限 / 明细 2 万条上限）。
pub async fn get_storage(State(state): State<ServerState>) -> Response {
    let base = config::config_dir();
    let dirs = config::storage_dirs();

    let log = match logging::store_ref() {
        Some(store) => {
            let file = store.file();
            json!({
                "dir": dirs.log_dir.to_string_lossy(),
                "custom": dirs.log_dir != base,
                "file": file.to_string_lossy(),
                "bytes": file_size(&file),
                "count": store.stats().total,
            })
        }
        None => Value::Null,
    };
    let requests = {
        let requests = state.request_stats();
        let request_file = requests.request_file();
        let daily_file = requests.daily_file();
        let stats = requests.stats();
        json!({
            "dir": dirs.request_stats_dir.to_string_lossy(),
            "custom": dirs.request_stats_dir != base,
            "file": request_file.to_string_lossy(),
            "bytes": file_size(&request_file),
            "dailyFile": daily_file.to_string_lossy(),
            "dailyBytes": file_size(&daily_file),
            "count": stats.get("total").and_then(Value::as_u64).unwrap_or(0),
        })
    };
    // 调试模式的原始报文（只有开着调试模式写过盘时才有内容）
    let debug = {
        let file = crate::server::core::debug_traffic::file();
        json!({
            "dir": dirs.debug_dir.to_string_lossy(),
            "custom": dirs.debug_dir != base,
            "file": file.as_ref().map(|path| path.to_string_lossy().to_string()).unwrap_or_default(),
            "bytes": file.as_deref().map(file_size).unwrap_or(0),
            "count": crate::server::core::debug_traffic::count(),
        })
    };
    ok_json(json!({
        "configDir": base.to_string_lossy(),
        // 键与 relocate 的 target 取值（logs / requests / debug）保持同名：
        // 前端拿同一个 target 既读概况又发迁移请求，两处名字必须一致
        "logs": log,
        "requests": requests,
        "debug": debug,
    }))
}

/// POST /api/storage/relocate —— body `{ target: "logs"|"requests", dir: "..." }`
///
/// 同步执行迁移（阻塞到搬迁结束才返回），成功后写 config.json 的目录键。
/// 迁移期间前端轮询 `/api/storage/progress` 画进度条；返回错误（400/500）
/// 时旧数据原封不动（搬迁失败不切目录，见 `relocate` 的失败语义）。
pub async fn relocate(State(state): State<ServerState>, body: Bytes) -> Response {
    let payload = match parse_body(&body) {
        Ok(value) => value,
        Err(error) => return errors::management_error(400, error.message),
    };
    let target = match payload.get("target").and_then(Value::as_str).unwrap_or("") {
        "logs" => "logs",
        "requests" => "requests",
        "debug" => "debug",
        other => {
            return errors::management_error(
                400,
                format!("target 取值非法: {other}（合法值 logs、requests、debug）"),
            );
        }
    };
    let dir_text = payload
        .get("dir")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if dir_text.is_empty() {
        return errors::management_error(400, "dir 不能为空");
    }
    let dir = PathBuf::from(&dir_text);
    if !dir.is_absolute() {
        return errors::management_error(400, "dir 必须是绝对路径（由目录选择框给出）");
    }

    // 搬迁放进阻塞线程池：持存储锁 + 文件 IO 不该占住 async worker
    set_progress(target, 0);
    let target_for_progress = target;
    let result = tokio::task::spawn_blocking(move || {
        let progress = |written: u64, total: u64| {
            let percent = if total == 0 {
                100
            } else {
                ((written * 100) / total).min(100) as u32
            };
            set_progress(target_for_progress, percent);
        };
        match target {
            "logs" => match logging::store_ref() {
                Some(store) => store.relocate(&dir, progress),
                None => Err("日志模块未启用".to_string()),
            },
            "debug" => {
                // 报文是小文件（上限 500 条），一次性搬完即可，不逐字节报进度
                progress(1, 1);
                crate::server::core::debug_traffic::relocate(&dir)
            }
            _ => state.request_stats().relocate(&dir, progress),
        }
    })
    .await
    .map_err(|error| format!("迁移任务执行失败: {error}"));
    clear_progress();

    match result {
        Ok(Ok(())) => {
            // 成功才写配置：搬回默认配置目录时清掉键（不固化绝对路径）。
            // dir 已随闭包搬进 spawn_blocking，这里按 dir_text 重取一份
            let base = config::config_dir();
            let key = match target {
                "logs" => KEY_LOG_DIR,
                "debug" => KEY_DEBUG_DIR,
                _ => KEY_REQUEST_STATS_DIR,
            };
            let moved_dir = PathBuf::from(&dir_text);
            if !config::set_storage_dir(key, (moved_dir != base).then_some(dir_text.as_str())) {
                logging::console_line(
                    "[Config]",
                    "⚠️ 保存位置写入 config.json 失败，本次运行内仍生效",
                );
            }
            let label = match target {
                "logs" => "事件日志",
                "debug" => "调试报文",
                _ => "请求日志",
            };
            logging::log("[Config]", &format!("✅ {label}已迁移到 {dir_text}"));
            ok_json(json!({ "target": target, "dir": dir_text }))
        }
        // 搬迁本体失败（建目录 / 写文件失败、目录相同）：旧数据原封不动
        Ok(Err(message)) => errors::management_error(500, message),
        Err(message) => errors::management_error(500, message),
    }
}

/// GET /api/storage/progress —— 迁移进度。无迁移进行时 `{ active: false }`。
pub async fn progress() -> Response {
    let payload = {
        let guard = RELOCATE.lock().ok();
        match guard.as_deref().and_then(|live| live.as_ref()) {
            Some(live) => json!({ "active": true, "target": live.target, "percent": live.percent }),
            None => json!({ "active": false }),
        }
    };
    ok_json(payload)
}
