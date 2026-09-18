//! 运行日志路由（对照 src/workbuddy-log-routes.mjs 逐字段实现）。
//!
//!   GET    /api/logs          查询（?limit=&level=&category=&keyword=&sinceId=&start=&end=）
//!   GET    /api/logs/stats    各级别 / 分类计数（导航徽标、筛选下拉）
//!   GET    /api/logs/download 导出 JSONL 附件
//!   DELETE /api/logs          清空
//!
//! 响应里的 `levels` / `categories` / `max` 三个字段由各接口统一附带 ——
//! 前端 logs-panel.js 用它们填筛选下拉与徽标上限，所以每个响应都要带。
//!
//! 与 Node 版的差异：Node 版对未匹配的 /api/logs/* 子路径返回
//! `{success:false, error:"Not found: ..."}`（404），本实现把未知子路径交给
//! 全局 404 兜底（OpenAI 风格 body）。两者都是 404，且前端只访问上面四条路径，
//! 唯一的区别是未知子路径的错误体形状 —— 未知路径不属于契约，取统一兜底更好维护。
//!
//! 日志参数解析是本文件的重点：查询串里的值都是字符串，
//! 与 Node 的 `Number(...)` / `Math.min(Math.max(...))` 语义要一一对上
//! （NaN 视作缺省、limit 夹在 1..MAX_ENTRIES）。
//!
//! `start` / `end`（毫秒时间戳）是**新增的可选**参数，Node 版没有：
//! 日志页的时间筛选需要它。解析规则与报表 API 一致 —— 非法（空串 / 非数字）
//! **忽略而不是报错**（前端可能发空串表示「不限时间」），
//! 不传时的过滤链与以前完全一致。

use std::collections::HashMap;

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Number, Value};

use crate::server::http::{ok_json, parse_query_ms, raw_json};
use crate::server::logging;
use crate::server::logs_store::{self, Query as LogQuery};
use crate::server::ServerState;

/// `/api/logs*` 上未匹配子路径的 404：Node 版这些路由模块自己发的 404
/// 是管理 API 形状（`{success:false,error:"Not found: <METHOD> <path>"}`），
/// 与全局兜底的 OpenAI 形状（`{error:{message}}`）不同 —— 两者都是 404，
/// 但前端与排障脚本按 body 形状区分来源，不能混用。
pub async fn not_found(request: axum::extract::Request) -> Response {
    let method = request.method().as_str().to_string();
    let path = request.uri().path().to_string();
    crate::server::errors::management_error(404, format!("Not found: {method} {path}"))
}

/// 取出全局日志库；未启用时返回与 Node 版一致的 503
fn store_or_error() -> Result<&'static logs_store::LogStore, Response> {
    logging::store_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            raw_json(json!({ "success": false, "error": "日志模块未启用" })),
        )
            .into_response()
    })
}

/// 查询串里只可能有这几项（axum 的 Query 用 HashMap 接收，
/// 比给每个参数定义结构体更贴合 Node 版「有则用、无则忽略」的写法）
type Params = HashMap<String, String>;

/// 把查询串里的字符串按 Node 的 Number() 语义转数字：空串/非数字 → None
fn parse_number(value: Option<&String>) -> Option<f64> {
    let text = value?.trim();
    if text.is_empty() {
        return None;
    }
    text.parse::<f64>().ok().filter(|item| item.is_finite())
}

/// limit 的生效值，逐例对齐 Node 版
/// `Math.min(Math.max(Number(limit) || 200, 1), MAX_ENTRIES)`：
///   - 缺省 / 非数字 / 0 → 200（JS 里 `0 || 200` 得 200，0 不是有效 limit）
///   - 负数 → 1；超上限 → MAX_ENTRIES
fn effective_limit(raw: Option<&String>) -> usize {
    let parsed = parse_number(raw).unwrap_or(0.0);
    let base = if parsed == 0.0 { 200.0 } else { parsed };
    base.max(1.0).min(logs_store::MAX_ENTRIES as f64) as usize
}

/// GET /api/logs
pub async fn query_logs(State(_state): State<ServerState>, Query(params): Query<Params>) -> Response {
    let store = match store_or_error() {
        Ok(store) => store,
        Err(response) => return response,
    };

    let query = LogQuery {
        limit: Some(effective_limit(params.get("limit"))),
        // level/category 的合法性校验在 LogStore 内（未知值不参与过滤）
        level: params.get("level").cloned(),
        category: params.get("category").cloned(),
        keyword: params.get("keyword").cloned(),
        since_id: parse_number(params.get("sinceId")).map(|value| value.max(0.0) as u64),
        // 时间筛选：非法值解析成 None（= 该边界不生效），与 Node 版「有则用、无则忽略」同调
        start: parse_query_ms(params.get("start")),
        end: parse_query_ms(params.get("end")),
    };

    let result = store.query(&query);
    let mut data = match serde_json::to_value(&result) {
        Ok(value) => value,
        Err(error) => {
            logging::log("[Logs]", &format!("❌ 日志查询结果序列化失败: {error}"));
            return raw_json(json!({ "success": false, "error": "日志查询失败" }));
        }
    };
    if let Some(object) = data.as_object_mut() {
        attach_dictionary(object);
    }
    ok_json(data)
}

/// GET /api/logs/stats
pub async fn stats_logs(State(_state): State<ServerState>) -> Response {
    let store = match store_or_error() {
        Ok(store) => store,
        Err(response) => return response,
    };
    let mut data = match serde_json::to_value(store.stats()) {
        Ok(value) => value,
        Err(error) => {
            logging::log("[Logs]", &format!("❌ 日志统计序列化失败: {error}"));
            return raw_json(json!({ "success": false, "error": "日志统计失败" }));
        }
    };
    if let Some(object) = data.as_object_mut() {
        attach_dictionary(object);
    }
    ok_json(data)
}

/// DELETE /api/logs
pub async fn clear_logs(State(_state): State<ServerState>) -> Response {
    let store = match store_or_error() {
        Ok(store) => store,
        Err(response) => return response,
    };
    let stats = store.clear();
    logging::log("[Logs]", "运行日志已清空");
    match serde_json::to_value(stats) {
        Ok(value) => ok_json(value),
        Err(error) => {
            logging::log("[Logs]", &format!("❌ 清空后统计序列化失败: {error}"));
            raw_json(json!({ "success": false, "error": "日志清空失败" }))
        }
    }
}

/// GET /api/logs/download —— 直接回 JSONL 附件
pub async fn download_logs(State(_state): State<ServerState>) -> Response {
    let store = match store_or_error() {
        Ok(store) => store,
        Err(response) => return response,
    };
    let body = store.to_jsonl();
    let filename = format!("workbuddy-logs-{}.jsonl", file_stamp());

    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/x-ndjson; charset=utf-8")
        .body(Body::from(body))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());

    // 文件名/长度用 HeaderValue 构造（含非 ASCII 的兜底不 panic）
    if let Ok(value) = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\"")) {
        response.headers_mut().insert(header::CONTENT_DISPOSITION, value);
    }
    response
}

/// 给响应体补上 levels / categories / max 三个字典字段
/// （对应 Node 版 `{ ...data, levels: LEVELS, categories: CATEGORIES, max: MAX_ENTRIES }`）
fn attach_dictionary(object: &mut serde_json::Map<String, Value>) {
    object.insert(
        "levels".to_string(),
        Value::Array(logs_store::LEVELS.iter().map(|item| Value::String(item.to_string())).collect()),
    );
    let mut categories = serde_json::Map::new();
    for (key, label) in logs_store::CATEGORIES {
        categories.insert(key.to_string(), Value::String(label.to_string()));
    }
    object.insert("categories".to_string(), Value::Object(categories));
    object.insert(
        "max".to_string(),
        Value::Number(Number::from(logs_store::MAX_ENTRIES as u64)),
    );
}

/// 导出文件名的时间戳：对应 Node 版
/// `new Date().toISOString().replace(/[:.]/g, '-')`（把 : 和 . 换成 -），
/// 结果形如 `2026-09-17T13-45-02-123Z` —— 冒号在 Windows 文件名里非法，所以必须换。
fn file_stamp() -> String {
    match chrono::DateTime::from_timestamp_millis(logging::now_ms()) {
        Some(now) => now.format("%Y-%m-%dT%H-%M-%S-%3fZ").to_string(),
        None => "unknown".to_string(),
    }
}
