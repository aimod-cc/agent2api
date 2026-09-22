//! 网关错误类型与 OpenAI 风格错误 payload。
//!
//! 对照 Node 版 server.mjs 313-332 行的 `errorPayload()`：
//! 上游/业务错误统一带 `statusCode`（可选 `upstreamCode`），
//! 由本模块翻译成给客户端看的 `{ error: { message, type, ... } }`。
//!
//! 分类规则（照抄，不做「顺手修复」）：
//!   - 429 或上游码 6004            → rate_limit_exceeded（并附 reset_at 恢复时间）
//!   - 401                          → authentication_error
//!   - statusCode < 500             → invalid_request_error
//!   - 其余（含 5xx 与非法 status）  → proxy_error
//!
//! 默认状态码 500：Node 版用 `Number.isInteger(statusCode) ? statusCode : 500`，
//! 这里用 `Option<i32>` 表达同一语义。
//!
//! 注意：项目 release profile 是 `panic = "abort"`，handler 里任何 panic
//! 都会直接带走整个桌面应用，所以错误一律走 Result + 本模块的类型，
//! 不用 unwrap/expect。

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::TimeZone;
use serde_json::{json, Map, Value};

/// 账号限额的上游业务码（Node 版 QUOTA_LIMIT_CODE）
pub const QUOTA_LIMIT_CODE: i64 = 6004;

/// 统一的网关错误。
///
/// `status_code` 对应 Node 版错误对象上的 `statusCode`，
/// `upstream_code` 对应各模块的 `upstreamCode`（带上就会出现在响应里，
/// 客户端据此识别腾讯侧的业务码），
/// `code` 对应 Node 版手写 body 里的 `code` 字段（目前只有模型不存在用：
/// `code: 'model_not_found'`）。
#[derive(Clone, Debug)]
pub struct GatewayError {
    pub message: String,
    pub status_code: i32,
    pub upstream_code: Option<i64>,
    pub code: Option<String>,
}

impl GatewayError {
    /// 通用构造：默认 500（对应 Node 版「非整数状态码回落 500」）
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            status_code: 500,
            upstream_code: None,
            code: None,
        }
    }

    /// 指定状态码
    pub fn with_status(status: i32, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            status_code: status,
            upstream_code: None,
            code: None,
        }
    }

    /// 400 参数错误（后续切片的入参校验用）
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::with_status(400, message)
    }

    /// 挂上错误码字符串（Node 版手写 body 里的 `code` 字段）
    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }

    /// 挂上上游业务码（如 6004 限额）
    pub fn upstream_code(mut self, code: i64) -> Self {
        self.upstream_code = Some(code);
        self
    }

    /// 挂上游业务码（Option 形态；None 时保持原值不变）——
    /// 转发链路里上游码本来就是 Option，用这个可以省掉一次分支。
    pub fn with_optional_code(mut self, code: Option<i64>) -> Self {
        if let Some(code) = code {
            self.upstream_code = Some(code);
        }
        self
    }

    /// 是否为账号限额错误（HTTP 429 或上游 code 6004）
    pub fn is_quota_limit(&self) -> bool {
        self.status_code == 429 || self.upstream_code == Some(QUOTA_LIMIT_CODE)
    }

    /// 归一后的状态码：非 100..600 的取值一律当 500（防止把非法码塞进 HTTP 响应）
    fn safe_status(&self) -> i32 {
        if (100..600).contains(&self.status_code) {
            self.status_code
        } else {
            500
        }
    }

    /// 错误类型串，照抄 Node 版 errorPayload 的判定顺序
    fn error_type(&self) -> &'static str {
        if self.is_quota_limit() {
            "rate_limit_exceeded"
        } else if self.status_code == 401 {
            "authentication_error"
        } else if self.status_code < 500 {
            "invalid_request_error"
        } else {
            "proxy_error"
        }
    }

    /// 构造 OpenAI 风格错误 body（对应 Node 版 errorPayload().body）。
    ///
    /// 字段顺序与 Node 版一致：message → type → upstream_code（可选）→
    /// `code`（可选，手写 body 用）→ reset_at（可选）。
    /// `reset_at` / `reset_at_text` 只在成功解析出恢复时间时出现（Node 版同理）。
    pub fn payload(&self) -> Value {
        let mut error = Map::new();
        error.insert("message".to_string(), Value::String(self.message.clone()));
        error.insert("type".to_string(), Value::String(self.error_type().to_string()));
        if let Some(code) = &self.code {
            error.insert("code".to_string(), Value::String(code.clone()));
        }
        if let Some(code) = self.upstream_code {
            error.insert("upstream_code".to_string(), Value::from(code));
        }
        if self.is_quota_limit() {
            let reset_at = parse_quota_reset_at(&self.message);
            if reset_at > 0 {
                error.insert("reset_at".to_string(), Value::from(reset_at));
                error.insert(
                    "reset_at_text".to_string(),
                    Value::String(format_reset_at_text(reset_at)),
                );
            }
        }
        json!({ "error": Value::Object(error) })
    }

    /// 直接转成 HTTP 响应（状态码 + OpenAI 风格 body）。
    ///
    /// 与 `IntoResponse` 等价，区别只是调用点写起来更明确 ——
    /// 转发链路里 `Err(error) => error.payload_response()` 比
    /// `IntoResponse::into_response(error)` 可读。
    pub fn payload_response(self) -> Response {
        let status = self.http_status();
        (status, Json(self.payload())).into_response()
    }

    /// HTTP 状态码
    pub fn http_status(&self) -> StatusCode {
        StatusCode::from_u16(self.safe_status() as u16).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
    }
}

impl std::fmt::Display for GatewayError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.message)
    }
}

impl std::error::Error for GatewayError {}

/// 让 handler 可以直接 `Result<T, GatewayError>` 返回，错误自动变成
/// 状态码 + OpenAI 风格 body（无需每个 handler 手写转换）。
impl IntoResponse for GatewayError {
    fn into_response(self) -> Response {
        let status = self.http_status();
        (status, Json(self.payload())).into_response()
    }
}

/// 从 `std::io::Error` / `serde_json::Error` 这类常见错误快速转成 500。
impl From<std::io::Error> for GatewayError {
    fn from(error: std::io::Error) -> Self {
        Self::new(format!("文件操作失败: {error}"))
    }
}

impl From<serde_json::Error> for GatewayError {
    fn from(error: serde_json::Error) -> Self {
        Self::new(format!("JSON 解析失败: {error}"))
    }
}

/// 401 未授权响应。
///
/// 文案与 type 都照抄 Node 版 unauthorized()：
/// `{error:{message:"Unauthorized: invalid or missing API key", type:"invalid_api_key"}}`
///
/// 注意 type 是 `invalid_api_key` 而**不是** errorPayload() 给 401 用的
/// `authentication_error` —— 因为 Node 的 unauthorized() 是自己拼的 body，
/// 没走 errorPayload()。两者不要混用。
pub fn unauthorized_response() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({
            "error": {
                "message": "Unauthorized: invalid or missing API key",
                "type": "invalid_api_key",
            }
        })),
    )
        .into_response()
}

/// 401：面板登录缺失（web_shim 据此弹「账号密码」框而不是 Key 框）。
///
/// `type` 用 `panel_login_required` 与普通 Key 鉴权失败（`invalid_api_key`）
/// 区分 —— 两者都是 401，但浏览器面板对它们的反应不同。
pub fn panel_login_required_response() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({
            "error": {
                "message": "请先登录管理面板",
                "type": "panel_login_required",
            }
        })),
    )
        .into_response()
}

/// 503：headless 公网形态未配置任何 API Key 时，`/v1/*` 的 fail-closed 响应。
///
/// 与 401 不同：这不是「Key 错了」，而是「还没有任何 Key 可校验」——
/// 引导用户去面板创建第一把（面板本身经入口认证，不需要 Key）。
pub fn v1_fail_closed_response() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({
            "error": {
                "message": "网关尚未配置任何 API Key：请登录管理面板，在「网关 Key」页创建一把",
                "type": "api_key_not_configured",
            }
        })),
    )
        .into_response()
}

/// 404 兜底响应。
///
/// 注意这里**不带 type 字段** —— Node 版 404 分支发的是
/// `{ error: { message: "Not found: <METHOD> <path>" } }`，
/// 没有经过 errorPayload()，因此不会有 type/upstream_code。
/// 单独出一个构造函数而不是复用 GatewayError，就是为了不「顺手」补上 type。
pub fn not_found_response(method: &str, path: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": { "message": format!("Not found: {method} {path}") } })),
    )
        .into_response()
}

/// 管理 API 的错误信封 `{ success: false, error: "..." }`。
/// 与 OpenAI 风格 payload 不同 —— 管理接口（/api/*）走这个形状，
/// 对应 Node 版各 route 模块里的 `sendJson(res, status, { success:false, error })`。
pub fn management_error(status: i32, message: impl Into<String>) -> Response {
    let status = StatusCode::from_u16(status as u16).unwrap_or(StatusCode::BAD_REQUEST);
    (status, Json(json!({ "success": false, "error": message.into() }))).into_response()
}

/// 解析上游限额恢复时间：`YYYY-MM-DD HH:MM:SS UTC+8` → 毫秒时间戳。
///
/// 对应 Node 版 parseQuotaResetAt（workbuddy-upstream-client.mjs 82-87 行）：
/// ```js
/// /(\d{4})-(\d{2})-(\d{2})\s+(\d{2}):(\d{2}):(\d{2})\s*UTC\+8/
/// ```
/// 解析不出时返回 0（Node 版同样返回 0，调用方据此决定要不要带 reset_at 字段），
/// 而不是报错 —— 这条路径在处理上游错误，不能再制造新错误。
///
/// ── 为什么改写成「按字节扫描 + 数值解析」（切片 4 发现并修复）────
/// 上一版是「滑动窗口 + `&text[start..start+19]` 切片」，在**中文错误文案**上
/// 会直接 panic（release 是 panic=abort，会带走整个桌面应用）：
/// 上游 429 的 msg 是「您的使用量已超出频率限制，将在 2026-09-11 19:43:46
/// UTC+8 重置…」，窗口起点一旦落在汉字中间（如「您」的第 2、3 字节），
/// 切片就在非字符边界上，`is_char_boundary` 的检查写在切片**之后**已经来不及。
/// 实测这条路径在真实 429 响应上 100% 触发。
///
/// 现在全程按 `&[u8]` 判定：只有当某个位置起确实是 `dddd-dd-dd` 形态
/// （全是 ASCII 数字与 `-`）才继续，而 ASCII 字节不可能是多字节字符的
/// 续字节，所以后续任何切片都天然落在字符边界上。顺带把 Node 正则里
/// 的 `\s+`（日期与时间之间至少一个空白）与 `\s*UTC+8` 后缀也补齐了 ——
/// 上一版只认「一个空格 + 19 字符定长窗口」且不校验 UTC+8。
pub fn parse_quota_reset_at(text: &str) -> i64 {
    let bytes = text.as_bytes();
    // 空白判定：`\s` 在 Node 正则里含空格/制表/换行（字节层面判 ASCII 空白即可）
    let space = |index: usize| -> bool {
        bytes
            .get(index)
            .map(|byte| byte.is_ascii_whitespace())
            .unwrap_or(false)
    };
    /// 取 `width` 位十进制数字（不足或非数字返回 None）
    fn number(bytes: &[u8], start: usize, width: usize) -> Option<u32> {
        let mut value = 0u32;
        for offset in 0..width {
            let byte = bytes.get(start + offset)?;
            if !byte.is_ascii_digit() {
                return None;
            }
            value = value * 10 + (byte - b'0') as u32;
        }
        Some(value)
    }

    let mut index = 0usize;
    while index < bytes.len() {
        // ① dddd-dd-dd
        let date = (
            number(bytes, index, 4),
            bytes.get(index + 4),
            number(bytes, index + 5, 2),
            bytes.get(index + 7),
            number(bytes, index + 8, 2),
        );
        if let (Some(year), Some(b'-'), Some(month), Some(b'-'), Some(day)) = date {
            // ② \s+ 分隔
            let mut cursor = index + 10;
            let space_start = cursor;
            while space(cursor) {
                cursor += 1;
            }
            if cursor > space_start {
                // ③ dd:dd:dd
                let time = (
                    number(bytes, cursor, 2),
                    bytes.get(cursor + 2),
                    number(bytes, cursor + 3, 2),
                    bytes.get(cursor + 5),
                    number(bytes, cursor + 6, 2),
                );
                if let (Some(hour), Some(b':'), Some(minute), Some(b':'), Some(second)) = time {
                    // ④ \s*UTC+8 —— 必须命中，否则不是限额恢复时间（与 Node 正则一致）
                    let mut suffix = cursor + 8;
                    while space(suffix) {
                        suffix += 1;
                    }
                    if bytes[suffix..].starts_with(b"UTC+8") {
                        if let Some(millis) = to_utc_plus_8_millis(year, month, day, hour, minute, second) {
                            return millis;
                        }
                    }
                }
            }
        }
        index += 1;
    }
    0
}

/// `YYYY-MM-DD HH:MM:SS`（UTC+8）→ 毫秒时间戳；字段非法时 None。
///
/// 不引入 regex：手写扫描比正则更容易控制「失败即 0」的语义。
fn to_utc_plus_8_millis(
    year: u32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
) -> Option<i64> {
    // UTC+8 固定偏移：上游文案里就是这么标的，不做时区探测
    let offset = chrono::FixedOffset::east_opt(8 * 3600)?;
    let naive = chrono::NaiveDate::from_ymd_opt(year as i32, month, day)?
        .and_hms_opt(hour, minute, second)?;
    Some(offset.from_local_datetime(&naive).single()?.timestamp_millis())
}

/// 恢复时间的本地化展示（对应 Node 版 `toLocaleString('zh-CN', { hour12:false })`）。
/// 统一按 UTC+8 渲染：上游恢复时间本身就是 UTC+8 标定的，
/// 用本机时区反而会让用户对不上上游给的原文。
fn format_reset_at_text(reset_at: i64) -> String {
    // 固定偏移 +8 一定能构造成功；万一失败就给空串（这只是展示文案，
    // 绝不能因为一个格式化失败把请求带崩 —— release 是 panic=abort）
    let Some(offset) = chrono::FixedOffset::east_opt(8 * 3600) else {
        return String::new();
    };
    match chrono::DateTime::from_timestamp_millis(reset_at) {
        Some(utc) => utc
            .with_timezone(&offset)
            .format("%Y/%m/%d %H:%M:%S")
            .to_string(),
        None => String::new(),
    }
}
