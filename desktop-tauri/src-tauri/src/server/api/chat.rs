//! 对话链路路由（对照 server.mjs 466-560、956-961 行逐条实现）。
//!
//!   POST /v1/chat/completions   protected（Node 版这条查 API Key）
//!   GET  /v1/models             public（Node 版这条**不查** API Key ——
//!                               它是只读探针，客户端启动时常在配 key 之前调用）
//!
//! ── handleModelRequest 的处理顺序（照抄，别调整）──────────────
//!   ① body 必须是 JSON 对象（400「请求体必须是 JSON 对象」）
//!   ② 必须有 messages 数组（400「缺少 messages 数组」）
//!   ③ verbose 日志：method/path/model/stream/msgs/bytes/ua
//!   ④ 调试落盘 {config_dir}/debug/last-request.json + .meta.txt（失败不影响请求）
//!   ⑤ 模型校验：未指定 → defaultModel → 目录 isDefault → 首项；
//!      点名的模型不在目录 → 400（带相近模型提示）
//!   ⑥ rememberRequestModel（默认值已填充完毕）
//!   ⑦ 脱敏钩子（切片 5 的接入点，见 `desensitize_body`）
//!   ⑧ 转发：流式透传 / 非流式聚合
//!
//! ── 错误中途写出 ────────────────────────────────────────────
//! 转发失败时：headers 还没发出 → errorPayload（OpenAI 风格 + 429 的 reset_at）；
//! headers 已发出（流式已开始）→ 由响应流自身报错终止连接（Node 版是补写
//! `data: {"error":...}` + `data: [DONE]`，Rust 侧的等价语义见 forward 模块注释）。
//!
//! ── 脱敏钩子（切片 5 已接入）────────────────────────────────
//! `desensitize_body` 对 `body.messages` 里命中角色的文本插零宽空格，
//! 命中时调用点会打 `[Desensitize] 已脱敏命中 N 处：词×次数、…`（文案与
//! server.mjs 528 行一致）。实现在 `core::desensitize`，本文件只是接入点。

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::Value;

use crate::server::config;
use crate::server::core::upstream::{ForwardOutcome, ForwardRequest};
use crate::server::errors::GatewayError;
use crate::server::http::raw_json;
use crate::server::logging;
use crate::server::ServerState;

/// 调试落盘的目录名与文件名（Node 版 `join(CONFIG_DIR, 'debug', ...)`）
const DEBUG_DIR: &str = "debug";
const DEBUG_REQUEST_FILE: &str = "last-request.json";
const DEBUG_META_FILE: &str = "last-request.meta.txt";

/// POST /v1/chat/completions
pub async fn chat_completions(
    State(state): State<ServerState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // ① body 必须是 JSON 对象（数组/标量/null 都算非法）
    let parsed = serde_json::from_slice::<Value>(&body).ok();
    let Some(mut payload) = parsed.filter(Value::is_object) else {
        return GatewayError::bad_request("请求体必须是 JSON 对象")
            .payload_response();
    };
    // ② messages 必须是数组
    if !payload
        .get("messages")
        .map(Value::is_array)
        .unwrap_or(false)
    {
        return GatewayError::bad_request("缺少 messages 数组").payload_response();
    }

    let method = "POST";
    let path = "/v1/chat/completions";
    // 模型字段按 JS 语义取值：`!body.model` 是**真值判定**（null/"" /0/false 都算未指定），
    // 真值里非字符串的（数字/对象）会被 `String()` 化后参与目录比对 ——
    // 与 Node 的 `modelCatalog.has(body.model)` 内部那次转换一致。
    let model_field = model_field_text(&payload);
    let stream = payload.get("stream").map(|value| value == &Value::Bool(true)).unwrap_or(false);
    let message_count = payload
        .get("messages")
        .and_then(Value::as_array)
        .map(|items| items.len())
        .unwrap_or(0);
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("-")
        .to_string();

    logging::verbose(
        "[Model]",
        &format!(
            "← {method} {path} model={} stream={stream} msgs={message_count} bytes={} ua={user_agent}",
            if model_field.is_empty() { "(未指定)" } else { &model_field },
            body.len(),
        ),
    );

    // ④ 调试落盘：保存最近一次入站请求体，供重放分析（覆盖写）
    write_debug_files(&body, method, path, &user_agent);

    // ⑤ 模型路由：点名的模型必须是上游目录里真实存在的 id。
    // 不做任何静默改写/回退 —— 请求 4.1 就必须路由到 4.1，
    // 不存在就 400 报错（附近似名提示），让下游自己改配置。
    let catalog = state.models();
    if model_field.is_empty() {
        // 客户端没点名 → 网关默认；默认值不可用时落到目录里 isDefault 的模型，
        // 再不行取目录首项（三者的判定顺序照抄 Node 的
        // `has(defaultModel) ? defaultModel : list().find(isDefault)?.id ?? list()[0]?.id`）
        //
        // 注意目录为空时 Node 会把 body.model 置成 undefined 并**跳过**下面的
        // 校验（`if (body.model && ...)`），请求照样往上游发 —— 上游会自己报错。
        // 这里保持同一行为：拿不到兜底值就什么都不填。
        let snapshot = config::current();
        let models = catalog.list();
        let fallback = if catalog.has(snapshot.default_model()) {
            Some(snapshot.default_model().to_string())
        } else {
            models
                .iter()
                .find(|model| model.get("isDefault").map(value_is_truthy).unwrap_or(false))
                .or_else(|| models.first())
                .and_then(|model| model.get("id"))
                .filter(|id| !id.is_null())
                .map(|id| match id {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                })
        };
        if let Some(fallback) = fallback {
            if let Some(object) = payload.as_object_mut() {
                object.insert("model".to_string(), Value::String(fallback));
            }
        }
    }
    // 校验用的是**客户端原样给出的值**（真值化后的文本形态）：非字符串的真值
    // （数字/对象）也被字符串化后参与比对，与 Node 的 `has(body.model)` 一致
    let requested_model = model_field_text(&payload);
    if !requested_model.is_empty() && !catalog.has(&requested_model) {
        let hint = catalog.suggest(&requested_model, 5);
        let message = format!(
            "模型不存在: {requested_model}{}。完整列表见 GET /v1/models",
            if hint.is_empty() {
                String::new()
            } else {
                format!("（目录里相近的模型: {}）", hint.join("、"))
            },
        );
        let mut error = GatewayError::bad_request(message);
        // Node 版这条错误手写了 `code: 'model_not_found'`（其它 400 没有），
        // 客户端据此可以区分「参数错」与「模型名错」
        error = error.with_code("model_not_found");
        return error.payload_response();
    }
    // ⑥ 记录本次实际用的模型（默认值已填充完毕），供账号页筛选默认选中
    if !requested_model.is_empty() {
        config::remember_request_model(&requested_model);
    }

    // ⑦ 内容脱敏钩子：切片 5 在这里替换实现（当前恒为「未改动」）
    let desensitized = desensitize_body(&mut payload);
    if desensitized.changed {
        logging::log("[Desensitize]", &desensitized.log_line());
    }

    // ⑧ 转发：请求体哈希作为去重键（相同 body 的快速重试在代理内排队）
    let dedupe_key = sha256_hex(&body);
    let outcome = state
        .upstream()
        .forward(ForwardRequest { body: payload, stream, dedupe_key })
        .await;

    match outcome {
        Ok(ForwardOutcome::Stream { status, stream }) => {
            sse_response(status, stream).into_response()
        }
        Ok(ForwardOutcome::Completion { body }) => json_response(body),
        Err(error) => {
            // headers 还没发出（流式还没开始）→ 直接给 OpenAI 风格错误
            logging::log("[Model]", &format!("❌ {}", error.message));
            error.payload_response()
        }
    }
}

/// 非流式响应：`Content-Type: application/json; charset=utf-8` + 200。
///
/// 状态码固定 200：上游非 2xx 时转发层已经抛出（不会走到这里），
/// 与 Node 的 `res.writeHead(200, ...)` 一致。charset 显式写上 ——
/// Node 也是这么发的，且响应体里有中文（错误文案/思考内容）。
fn json_response(body: Value) -> Response {
    let text = match serde_json::to_string(&body) {
        Ok(text) => text,
        Err(error) => {
            // 序列化失败只可能是内部数据坏了：给一个 OpenAI 风格 500，
            // 绝不 panic（release 是 panic=abort）
            let message = format!("响应序列化失败: {error}");
            logging::log("[Model]", &format!("❌ {message}"));
            return GatewayError::with_status(500, message).payload_response();
        }
    };
    let mut response = Response::new(Body::from(text));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    response
}

/// 请求体里的 model → 供校验/记录用的文本。
///
/// 复刻 Node 的取值链：`if (!body.model)` 先做真值判定（null/""/0/false 都算
/// 未指定），真值再交给目录查（`get()` 内部是 `String(id).toLowerCase()`），
/// 所以数字/对象这类非字符串值也会被字符串化后参与比对 —— 本函数做的就是
/// 那个字符串化，且**不改动 payload 里的原值**（上游收到什么由客户端决定）。
fn model_field_text(payload: &Value) -> String {
    let Some(value) = payload.get("model") else {
        return String::new();
    };
    if !value_is_truthy(value) {
        return String::new();
    }
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// JS 真值判定（`Boolean(x)`）
fn value_is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().map(|item| item != 0.0).unwrap_or(false),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// 流式响应：`Content-Type: text/event-stream; charset=utf-8` +
/// `Cache-Control: no-cache` + `Connection: keep-alive`，状态码透传。
///
/// 流项目是 `Result<Bytes, io::Error>`：上游断流时 axum 结束连接
/// （Node 版此时是补写 error 帧 + `[DONE]`，见模块头部说明）。
fn sse_response(
    status: u16,
    stream: Box<dyn futures::Stream<Item = Result<Bytes, std::io::Error>> + Send + Unpin>,
) -> Response {
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream; charset=utf-8"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    headers.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
    response
}

/// GET /v1/models —— 免鉴权（Node 版这条没调 checkApiKey）
pub async fn list_models(State(state): State<ServerState>) -> Response {
    let response = raw_json(state.models().list_response());
    // 异步刷新模型目录，不阻塞响应（对照 Node 的 `void refreshModelCatalog()`）
    spawn_catalog_refresh(&state);
    response
}

/// 起一个后台任务刷新模型目录（不阻塞当前响应）。
///
/// 必须用 `tauri::async_runtime::spawn` 而不是 `tokio::spawn`：
/// 本函数在 axum handler 里调用（运行时上下文已成立），但显式选择项目里
/// 统一的 spawn 入口，避免以后有人把它挪到非 Tokio 上下文时 panic
/// （release 是 panic=abort，那会带走整个桌面应用）。
pub fn spawn_catalog_refresh(state: &ServerState) {
    let models = state.models().clone();
    let store = state.store().clone();
    let auth = state.auth().clone();
    tauri::async_runtime::spawn(async move {
        models.refresh_with_current_account(&store, &auth).await;
    });
}

/// ─── 脱敏钩子（切片 5 的唯一接入点）──────────────────────────

/// 脱敏处理结果（对应 Node 版 `desensitizer.processBody` 的返回）
pub struct DesensitizeResult {
    pub changed: bool,
    /// 命中总次数
    pub hits: usize,
    /// 每个词各自的命中次数（按次数降序）
    pub term_counts: Vec<(String, usize)>,
}

impl DesensitizeResult {
    /// 未改动（当前空实现的返回值，也是无命中时的形态）
    pub fn unchanged() -> Self {
        Self { changed: false, hits: 0, term_counts: Vec::new() }
    }

    /// 运行日志文案（照抄 Node 的 `已脱敏命中 N 处：词×次数、…`）
    pub fn log_line(&self) -> String {
        let per_term = self
            .term_counts
            .iter()
            .map(|(term, count)| format!("{term}×{count}"))
            .collect::<Vec<_>>()
            .join("、");
        format!("已脱敏命中 {} 处：{per_term}", self.hits)
    }
}

/// 内容脱敏钩子：对 `body.messages` 里命中角色的文本插零宽空格（原地改写 body）。
///
/// 对照 Node 版 server.mjs 520-530 行：
/// ```js
/// const result = desensitizer.processBody(body);
/// if (result.changed) { body = result.body; log('[Desensitize]', `已脱敏命中 ${result.hits} 处：…`); }
/// ```
/// 这里把「处理 + 统计」都交给 `core::desensitize`，本函数只负责把请求体递进去、
/// 把命中统计转成调用点要的形状。hook 之所以不带 state：脱敏处理器是进程级单例
/// （`core::desensitize::global()`，与 `config::current()` 同一模式），
/// 保持这个签名不变才能让切片 4 定下的调用点一行不动。
pub fn desensitize_body(body: &mut Value) -> DesensitizeResult {
    let outcome = crate::server::core::desensitize::global().process_body(body);
    if !outcome.changed {
        return DesensitizeResult::unchanged();
    }
    DesensitizeResult {
        changed: true,
        hits: outcome.hits,
        term_counts: outcome.term_counts,
    }
}

/// ─── 杂项 ───────────────────────────────────────────────────

/// 调试落盘：原始 body + 一行 meta（覆盖写；**失败不影响请求**）。
///
/// 与 Node 版一致：目录不存在时创建，任何 IO 失败都静默吞掉 ——
/// 调试落盘是排障辅助，不能因为它失败就让用户的请求失败。
fn write_debug_files(body: &[u8], method: &str, path: &str, user_agent: &str) {
    let dir = config::config_dir().join(DEBUG_DIR);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let _ = std::fs::write(dir.join(DEBUG_REQUEST_FILE), body);
    let meta = format!(
        "{} {method} {path} {}B ua={user_agent}",
        iso_timestamp(),
        body.len(),
    );
    let _ = std::fs::write(dir.join(DEBUG_META_FILE), meta);
}

/// ISO-8601 UTC 时间戳（对应 Node 的 `new Date().toISOString()`，形如
/// `2026-09-17T08:30:00.123Z`）
fn iso_timestamp() -> String {
    let millis = logging::now_ms();
    match chrono::DateTime::from_timestamp_millis(millis) {
        Some(utc) => utc.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
        None => String::new(),
    }
}

/// 请求体 sha256（十六进制小写）——去重键，对应 Node 的
/// `createHash('sha256').update(rawBody).digest('hex')`
fn sha256_hex(body: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(body);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

