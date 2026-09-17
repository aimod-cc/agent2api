//! 上游管理接口的传输层：请求发送、响应解包与鉴权错误类型。
//!
//! 从 auth.rs 拆出（单文件行数约定）。这一层不认识账号与会话，只负责按
//! Node 版 `api()` + `unwrap()` 的语义「发一次请求并解包出 data」：
//!   - `ApiResponse` 保留原始状态码与解析出的 payload
//!   - `unwrap_response` 把 Node 的四种情形逐条映射成 Result
//!     （非 JSON / HTTP 非 2xx / `code !== 0` / 成功）
//!
//! **所有上游出网都经过 `send_request`**（切片 3 起含计费/签到/活动），
//! 出口由调用方按会话的 `proxy` 字段传入，交给 `core::egress` 取对应 Client。
//! 不传代理 = 直连（`Option<&ResolvedProxy>` 为 None），与 Node 版
//! 「账号无代理 → idleDispatcher」一致。
//!
//! `WorkBuddyAuthError` 也放在这里：登录、账号刷新、会话读取都要用，
//! 是各调用方共同的最小依赖。

use std::time::Duration;

use serde_json::Value;

use crate::server::core::egress;
use crate::server::core::endpoints::RESPONSE_CODE_OK;
use crate::server::core::proxies::ResolvedProxy;
use crate::server::errors::GatewayError;

/// token 临期刷新窗口：过期前 5 分钟视为需要刷新
pub const REFRESH_WINDOW_MS: f64 = 5.0 * 60.0 * 1000.0;
/// 管理接口请求超时（Node 版按 undici 默认给足；这里给 30 秒，
/// 网络异常时不能把管理 API 卡死）
const REQUEST_TIMEOUT_MS: u64 = 30_000;

/// 上游业务码：轮询取 token 时「尚未完成登录」的返回码（RetryFetchToken）
pub const SERVER_CODE_RETRY_FETCH_TOKEN: i64 = 11217;

/// WorkBuddy 鉴权错误（对应 Node 版 WorkBuddyAuthError）。
///
/// `status_code` 为 None 时由路由层按 502 处理 —— 与 Node 版
/// `Number.isInteger(error.statusCode) ? error.statusCode : 502` 一致。
#[derive(Clone, Debug)]
pub struct WorkBuddyAuthError {
    pub message: String,
    pub status_code: Option<i32>,
    pub upstream_code: Option<i64>,
}

impl WorkBuddyAuthError {
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into(), status_code: None, upstream_code: None }
    }

    pub fn with_status(status: i32, message: impl Into<String>) -> Self {
        Self { message: message.into(), status_code: Some(status), upstream_code: None }
    }

    pub fn with_code(message: impl Into<String>, status: Option<i32>, code: Option<i64>) -> Self {
        Self { message: message.into(), status_code: status, upstream_code: code }
    }

    /// 归一后的 HTTP 状态码（无显式状态码 → 502，照抄 Node 版）
    pub fn http_status(&self) -> i32 {
        self.status_code.unwrap_or(502)
    }

    /// 转成统一的网关错误（路由层按管理 API 信封返回）
    pub fn to_gateway_error(&self) -> GatewayError {
        let mut error = GatewayError::with_status(self.http_status(), self.message.clone());
        if let Some(code) = self.upstream_code {
            error = error.upstream_code(code);
        }
        error
    }
}

impl std::fmt::Display for WorkBuddyAuthError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.message)
    }
}

/// 鉴权/账号接口的响应包：`{ code, data, msg?, message? }`
#[derive(Clone, Debug, Default)]
pub struct ApiResponse {
    pub status: u16,
    pub ok: bool,
    pub payload: Option<Value>,
}

/// 发一次上游请求并返回原始响应文本与状态码。
///
/// 直连 + 管理接口默认超时。**登录流程走这里**（登录时机还没有任何账号上下文，
/// 因此没有出口可挂 —— 与 Node 版登录请求 `proxy = null` 一致）。
pub async fn send_request(
    method: &str,
    url: &str,
    body: Option<&Value>,
    headers: &[(String, String)],
) -> Result<ApiResponse, WorkBuddyAuthError> {
    send_request_via(method, url, body, headers, None, Some(REQUEST_TIMEOUT_MS)).await
}

/// 发一次上游请求，可按出口挂代理并指定总超时。
///
/// ── 出口 ──────────────────────────────────────────────────
/// `proxy` 为 `None` 时直连（含「账号没配代理」与「代理解析失败回退直连」两种
/// 情形）；否则交给 `core::egress::client_for` 取该出口的 Client（带连接池缓存）。
///
/// ── 超时 ──────────────────────────────────────────────────
/// `timeout_ms` 是**单请求总超时**，对应 Node 版各模块的
/// `signal: AbortSignal.timeout(N)`（计费 20s / 管理接口 30s）；
/// 与客户端上的 `connect_timeout` / `read_timeout` 是叠加关系（先到者生效）。
/// 传 None 表示不设总超时 —— 只有 SSE 转发这类「长度未知、由调用方决定何时结束」
/// 的请求才该这样（切片 4）。
pub async fn send_request_via(
    method: &str,
    url: &str,
    body: Option<&Value>,
    headers: &[(String, String)],
    proxy: Option<&ResolvedProxy>,
    timeout_ms: Option<u64>,
) -> Result<ApiResponse, WorkBuddyAuthError> {
    send_raw(method, url, body, headers, proxy, timeout_ms)
        .await
        .map_err(|error| WorkBuddyAuthError::new(describe_transport_error(&error)))
}

/// 与 `send_request_via` 同一实现，但把**原始传输错误**交给调用方。
///
/// 只有计费模块用它：Node 版的 `callBilling` 需要区分「超时」（→ 504
/// 「计费接口请求超时」）与「其它失败」（→ 502「计费接口请求失败: <message>」），
/// 而 `WorkBuddyAuthError` 会把这两者压成同一个字符串，信息就丢了。
/// 出网点仍然是 `egress::client_for`，两条路径共用同一个连接池。
pub async fn send_raw(
    method: &str,
    url: &str,
    body: Option<&Value>,
    headers: &[(String, String)],
    proxy: Option<&ResolvedProxy>,
    timeout_ms: Option<u64>,
) -> Result<ApiResponse, reqwest::Error> {
    let client = egress::client_for(proxy);
    let mut builder = match method {
        "POST" => client.post(url),
        _ => client.get(url),
    };
    builder = builder.header("Accept", "application/json");
    for (key, value) in headers {
        builder = builder.header(key, value);
    }
    if let Some(payload) = body {
        builder = builder
            .header("Content-Type", "application/json")
            .body(payload.to_string());
    }
    if let Some(timeout) = timeout_ms {
        builder = builder.timeout(Duration::from_millis(timeout));
    }
    let response = builder.send().await?;
    let status = response.status().as_u16();
    let ok = response.status().is_success();
    let text = response.text().await?;
    let payload = if text.trim().is_empty() {
        None
    } else {
        serde_json::from_str::<Value>(&text).ok()
    };
    Ok(ApiResponse { status, ok, payload })
}

/// 把 reqwest 的传输错误翻译成可读中文（保留原因链，便于排障）
fn describe_transport_error(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "连接上游超时".to_string()
    } else if error.is_connect() {
        format!("无法连接上游: {error}")
    } else {
        format!("请求上游失败: {error}")
    }
}

/// 对照 Node 版 `unwrap(response, apiName)`：
///   - 非 JSON → 抛错（带 HTTP 状态码）
///   - HTTP 非 2xx → 抛错（取 error.message / message / msg 文案）
///   - `code` 为数字且 !== 0 → 抛错（带上游业务码）
///   - 成功 → 返回 `payload.data`
pub fn unwrap_response(response: &ApiResponse, api_name: &str) -> Result<Value, WorkBuddyAuthError> {
    let payload = response.payload.as_ref();
    if payload.is_none() && !response.ok {
        return Err(WorkBuddyAuthError::with_status(
            response.status as i32,
            format!("{api_name} 返回非 JSON 响应（HTTP {}）", response.status),
        ));
    }
    if !response.ok {
        let message = payload
            .and_then(|value| {
                value
                    .get("error")
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
                    .or_else(|| value.get("message").and_then(Value::as_str))
                    .or_else(|| value.get("msg").and_then(Value::as_str))
            })
            .unwrap_or("")
            .to_string();
        let message = if message.is_empty() {
            format!("HTTP {}", response.status)
        } else {
            message
        };
        let code = payload
            .and_then(|value| value.get("code"))
            .and_then(Value::as_i64);
        return Err(WorkBuddyAuthError::with_code(
            format!("{api_name} 失败: {message}"),
            Some(response.status as i32),
            code,
        ));
    }
    if let Some(Value::Number(code)) = payload.and_then(|value| value.get("code")) {
        if let Some(code) = code.as_i64() {
            if code != RESPONSE_CODE_OK {
                let detail = payload
                    .and_then(|value| value.get("message").and_then(Value::as_str))
                    .or_else(|| {
                        payload.and_then(|value| value.get("msg").and_then(Value::as_str))
                    })
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let message = if detail.is_empty() {
                    format!("{api_name} 失败: code={code}")
                } else {
                    format!("{api_name} 失败: code={code} {detail}")
                };
                return Err(WorkBuddyAuthError::with_code(
                    message,
                    Some(response.status as i32),
                    Some(code),
                ));
            }
        }
    }
    Ok(payload
        .and_then(|value| value.get("data"))
        .cloned()
        .unwrap_or(Value::Null))
}

/// 登录流程用的裸请求（不走 AuthService：登录时机还没有任何账号上下文）。
pub async fn send_public_request(
    method: &str,
    url: &str,
    body: Option<&Value>,
    headers: &[(String, String)],
) -> Result<ApiResponse, WorkBuddyAuthError> {
    send_request(method, url, body, headers).await
}

/// 登录流程用的响应解包（同 `unwrap_response`）
pub fn unwrap_public_response(
    response: &ApiResponse,
    api_name: &str,
) -> Result<Value, WorkBuddyAuthError> {
    unwrap_response(response, api_name)
}
