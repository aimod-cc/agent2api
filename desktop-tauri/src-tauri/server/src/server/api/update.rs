//! 软件更新路由（对照 server.mjs 749-773 行）。
//!
//!   GET  /api/update/check?current=1.0.0   检查新版本（返回里带最新版的发布说明）
//!   POST /api/update/download              开始下载 { url, name }
//!   GET  /api/update/progress              下载进度
//!   POST /api/update/cancel                取消下载
//!
//! ── 路由挂法 ────────────────────────────────────────────────
//! Node 是 `path.startsWith('/api/update/')` 的前缀判定：命中前缀后逐条判
//! 方法与路径，都不匹配则落到最外层 404（**不带** `success` 信封的
//! `{"error":{"message":"Not found: <METHOD> <path>"}}`，实测如此）。
//! 所以这里用一条 `{*rest}` 通配入口 + dispatch：拆成独立 axum 路由会让
//! `/api/update/zzz` 这类未注册子路径落到全局兜底 —— 那其实也是同一个 404 形状，
//! 但 `DELETE /api/update/check` 这类「已注册路径 + 未注册方法」会变成 405 兜底，
//! 与 Node 的 404 分叉（见 http.rs 的 method_not_allowed 说明）。
//!
//! ── 错误信封 ────────────────────────────────────────────────
//! Node 的四条都在最外层大 try 里 → 失败走 errorPayload 的 **OpenAI 风格**
//! body，状态码取 UpdateError 自带的值（400 参数/域名非法、502 网络与 GitHub 侧）。
//! 实测：`{"url":"http://evil.com/a.exe"}` → 400「下载地址必须是 https」。
//! 后加的 `/releases` 沿用同一个信封（它同样返回 UpdateError）。

use axum::body::Bytes;
use axum::extract::State;
use axum::http::Method;
use axum::response::{IntoResponse, Response};
use serde_json::Value;

use crate::server::core::update::UpdateError;
use crate::server::errors::{management_error, GatewayError};
use crate::server::http::{ok_json, parse_body};
use crate::server::logging;
use crate::server::ServerState;

/// 更新错误 → 响应（OpenAI 风格 body，状态码保留）
fn update_error(error: UpdateError) -> Response {
    logging::log("[Update]", &format!("❌ {}", error.message));
    error.to_gateway_error().into_response()
}

/// `/api/update/{*rest}` 的入口（任意方法、任意子路径）。
pub async fn entry(State(state): State<ServerState>, request: axum::extract::Request) -> Response {
    let method = request.method().clone();
    let full_path = request.uri().path().to_string();
    let query = request.uri().query().unwrap_or("").to_string();
    let body = match axum::body::to_bytes(request.into_body(), crate::server::http::MAX_BODY_SIZE)
        .await
    {
        Ok(bytes) => bytes,
        Err(error) => return management_error(413, format!("请求体读取失败或过大: {error}")),
    };
    dispatch(&state, method, &full_path, &query, &body).await
}

/// 路径分发（判定顺序按既有四条 if 的写法顺延）
async fn dispatch(
    state: &ServerState,
    method: Method,
    full_path: &str,
    query: &str,
    body: &Bytes,
) -> Response {
    if method == Method::GET && full_path == "/api/update/check" {
        // `url.searchParams.get('current') || ''`：缺省是空串（不是 null）
        let current = query
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .find(|(key, _)| *key == "current")
            .map(|(_, value)| percent_decode(value))
            .unwrap_or_default();
        return match state.update().check(&current).await {
            Ok(data) => ok_json(data),
            Err(error) => update_error(error),
        };
    }

    if method == Method::GET && full_path == "/api/update/status" {
        // 最近一次「检查更新」的结果（定时任务与壳命令写入，前端 60 秒轮询
        // 读这里亮侧栏徽标，自己不再打 GitHub —— 匿名限额 60 次/小时）。
        // 本进程还没查过时是 {checked:false}，前端按「未知」处理，不亮标。
        return ok_json(state.update().last_check());
    }

    if method == Method::POST && full_path == "/api/update/download" {
        // 非法 JSON 在 Node 里是 JSON.parse 抛错 → errorPayload 500 proxy_error
        // （与 /api/auto-checkin 同一条路径），这里保持一致
        let payload = match parse_body(body) {
            Ok(value) => value,
            Err(error) => return GatewayError::new(error.message).into_response(),
        };
        let text = |key: &str| -> String {
            payload
                .get(key)
                .filter(|value| !value.is_null())
                .map(js_string)
                .unwrap_or_default()
        };
        return match state.update().start_download(&text("url"), &text("name")) {
            Ok(data) => ok_json(data),
            Err(error) => update_error(error),
        };
    }

    if method == Method::GET && full_path == "/api/update/progress" {
        return ok_json(state.update().get_progress());
    }

    if method == Method::POST && full_path == "/api/update/cancel" {
        return ok_json(state.update().cancel_download());
    }

    // 未命中任何一条 → 全局 404 的文案（Node 落到最外层 `sendJson(res, 404, …)`），
    // 注意这个 body **没有 success 字段**
    crate::server::errors::not_found_response(method.as_str(), full_path)
}

/// JS `String(value)`：非字符串也照转（手改的请求体里 name 可能是数字）。
/// 对象/数组形态只会来自构造出来的脏请求，这里给 JSON 文本即可 ——
/// 它最终会被 safe_file_name 清成安全文件名。
fn js_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// 查询串的百分号解码（`current=1.0.0-beta%2B1` 这类形态）。
/// 失败时原样返回（不是致命错误，交给版本比较处理）。
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3])
                    .ok()
                    .and_then(|text| u8::from_str_radix(text, 16).ok());
                match hex {
                    Some(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    None => {
                        out.push(b'%');
                        index += 1;
                    }
                }
            }
            // `+` 在查询串里代表空格（与 URLSearchParams 一致）
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}
