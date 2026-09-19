//! 定时任务路由（设置页「定时任务」页的数据源）。
//!
//!   GET   /api/scheduled-tasks             读取四条间隔型任务的开关、间隔与运行状态
//!   PATCH /api/scheduled-tasks/{id}        改一条任务（`{enabled?, interval?}`）
//!   POST  /api/scheduled-tasks/{id}/run    立即执行一条**后端**任务
//!
//! ── 为什么与 /api/auto-checkin 是两组接口 ─────────────────────
//! 自动签到是**每天定点**型（时刻字符串 + 当天去重 + 启动补签），间隔型任务是
//! 「等间隔重复」，两者的配置形状与调度语义都不同（理由详见
//! `core::scheduled_tasks` 的模块头）。界面上它们同在「定时任务」页展示，
//! 但读写各走各的接口 —— 强行合并成一套形状会让两边都长出一堆「本类型不适用」
//! 的字段。前端因此在这一页调两组接口。
//!
//! ── 路由挂法 ────────────────────────────────────────────────
//! 三条入口（无尾段 / 尾斜杠 / 通配尾段）都是 `any(...)`，方法判定收在本文件的
//! `dispatch` 里 —— 与 `/api/accounts`、`/api/desensitize` 同一取舍：
//! 拆成独立 axum 路由的话，「已注册路径 + 未注册方法」会落到 405 兜底，
//! 而这里希望统一给管理信封的 404。方法判定本身很简单（只有两条带 id 的路径），
//! 顺带也解决了「`PATCH` 不在 CORS 允许方法里」的浏览器预检问题：POST 同样受理。
//!
//! ── 错误信封 ────────────────────────────────────────────────
//! 参数校验失败（未知 id / 间隔越界 / 没有需要更新的字段）返回**管理信封**的
//! 400 `{success:false,error:"…"}`，而不是 auto-checkin 那套 OpenAI 风格 body。
//! 理由：那套形状是 Node 版 auto-checkin 的历史契约（逐字对齐，见
//! `api::auto_checkin` 的模块头），本接口是纯新增的管理 API，没有需要对齐的
//! 旧客户端 —— 与 `/api/retention` 同一口径（那里校验失败也是管理信封 400）。
//!
//! 「正在执行中」（run 撞上同一任务已在跑）同样是 400 管理信封：它是**并发
//! 状态**而不是请求格式问题，但既有 auto-checkin 对同一情形也用 400，
//! 这里保持一致的状态码，前端能按一个套路处理。

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::server::config::IntervalTaskPatch;
use crate::server::core::scheduled_tasks;
use crate::server::errors::not_found_response;
use crate::server::http::{ok_json, parse_body, MAX_BODY_SIZE};
use crate::server::logging;
use crate::server::ServerState;

/// `/api/scheduled-tasks` 三条入口的公共处理（任意方法、任意子路径）。
pub async fn entry(State(state): State<ServerState>, request: axum::extract::Request) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let body = match axum::body::to_bytes(request.into_body(), MAX_BODY_SIZE).await {
        Ok(bytes) => bytes,
        Err(error) => return bad_request(format!("请求体读取失败或过大: {error}")),
    };
    dispatch(&state, method, &path, &body).await
}

/// 路径分发。
///
/// 两条带 id 的路径用**手工切分**而不是 axum 的 `Path<String>`：路由注册成
/// `any({*rest})` 之后拿不到具名参数，而这里的形态很规整（前缀 + 一段 id +
/// 可选的动作段），切分比再注册一层路由更直白。
async fn dispatch(state: &ServerState, method: Method, path: &str, body: &Bytes) -> Response {
    // 前缀之后的部分：`""`（无尾段）、`"credentialMaintenance"`、`"…/run"`
    let rest = path
        .strip_prefix("/api/scheduled-tasks")
        .unwrap_or_default()
        .trim_matches('/');

    if rest.is_empty() {
        if method == Method::GET {
            return ok_json(scheduled_tasks::list());
        }
        return not_found(method.as_str(), path);
    }

    // 只剩两种形态：`{id}` 与 `{id}/run`
    let mut segments = rest.splitn(2, '/');
    let id = segments.next().unwrap_or_default();
    let action = segments.next();

    match (method.clone(), action) {
        // PATCH 是规范写法；POST 是别名 —— CORS 的允许方法里没有 PATCH
        // （沿袭 Node 版，见 http.rs 的 CORS_METHODS），浏览器直连时预检会拦下
        // PATCH，走 Tauri 桥的正式前端两种都能用。
        (Method::PATCH, None) | (Method::POST, None) => configure(id, body),
        (Method::POST, Some("run")) => run_now(state, id).await,
        _ => not_found(method.as_str(), path),
    }
}

/// 改一条任务（`{enabled?}` / `{interval?}`，只改传进来的字段）
fn configure(id: &str, body: &Bytes) -> Response {
    // 空 body 视为 `{}` → 「没有需要更新的字段」的 400（与 auto-checkin 一致）
    let payload = match parse_body(body) {
        Ok(value) => value,
        Err(error) => return bad_request(error.message),
    };
    let mut patch = IntervalTaskPatch::default();
    if let Some(object) = payload.as_object() {
        if let Some(value) = object.get("enabled") {
            // 与 auto_checkin 的 configure 同一口径：`enabled !== undefined` 时
            // 只认严格 true，其余（null / 字符串 / 0）一律置为 false，而不是忽略。
            // 前端传的是 input.checked，正常永远是布尔。
            patch.enabled = Some(value == &Value::Bool(true));
        }
        if let Some(value) = object.get("interval") {
            match value.as_i64().or_else(|| {
                value
                    .as_f64()
                    .filter(|raw| raw.is_finite() && raw.fract() == 0.0)
                    .map(|raw| raw as i64)
            }) {
                Some(interval) => patch.interval = Some(interval),
                // 非数字（含 "abc" / null / 小数）直接 400：静默忽略会让界面显示
                // 「保存成功」而值没变，比报错更难排查
                None => return bad_request(format!("「{id}」的间隔必须是整数")),
            }
        }
    }
    match scheduled_tasks::configure(id, patch) {
        Ok(task) => ok_json(task),
        Err(message) => bad_request(message),
    }
}

/// 立即执行一条后端任务
///
/// 只对 `runner = backend` 的任务有效（前端两个刷新任务的定时器长在页面上，
/// 后端跑不了它们）—— 那种情况返回 400 并说明原因。
///
/// 响应里带**刷新后的那条任务**（含新的 `lastRunAt` / `lastResult` /
/// `nextRunAt`）：界面点完按钮就能就地重绘状态栏，不必再跑一趟 GET
/// （与 `/api/models/refresh` 带着清单返回同一个理由）。
async fn run_now(state: &ServerState, id: &str) -> Response {
    let store = state.store().clone();
    match scheduled_tasks::run_now(&store, id).await {
        Ok(summary) => ok_json(json!({
            "summary": summary,
            "task": scheduled_tasks::task_by_id(id),
        })),
        Err(message) => bad_request(message),
    }
}

/// 管理信封的 400（`{success:false,error}`），并打一行日志。
///
/// 不走 `errors::GatewayError::bad_request`：那会得到 OpenAI 风格 body，
/// 与 `/api/retention` 的管理信封不一致 —— 这里保持「管理 API 的 400 都是
/// 同一个形状」。
fn bad_request(message: impl Into<String>) -> Response {
    let message = message.into();
    logging::log("[Tasks]", &format!("❌ {message}"));
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "success": false, "error": message })),
    )
        .into_response()
}

/// 未命中任何一条：与全局兜底同文案（`Not found: <METHOD> <path>`）。
/// 本前缀下没有需要区分「路径错」与「方法错」的契约，两者给同一个 404 更简单。
fn not_found(method: &str, path: &str) -> Response {
    not_found_response(method, path)
}
