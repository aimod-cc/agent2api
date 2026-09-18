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
//!
//! ── 请求记账（本切片接入）──────────────────────────────────
//! 本文件是 `RequestStats::record` 的**唯一调用方**（见 `record_entry`）：
//! 无论成功 / 最终失败 / 客户端中断，一次用户请求**只记一条** ——
//! 429 自动换账号属于同一次请求，不额外记账。
//! 流式分支把记账交给 `RecordingStream`（收尾发生在 handler 返回之后），
//! 非流式与转发前失败则在原地记账。usage 由 `core::upstream` 的旁路槽提供。

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::Value;

use crate::server::config;
use crate::server::core::upstream::usage::RequestTelemetry;
use crate::server::core::upstream::{ForwardOutcome, ForwardRequest};
use crate::server::errors::GatewayError;
use crate::server::http::raw_json;
use crate::server::logging;
use crate::server::request_stats::{NewRequestEntry, RequestStats};
use crate::server::ServerState;

/// 调试落盘的目录名与文件名（Node 版 `join(CONFIG_DIR, 'debug', ...)`）
const DEBUG_DIR: &str = "debug";
const DEBUG_REQUEST_FILE: &str = "last-request.json";
const DEBUG_META_FILE: &str = "last-request.meta.txt";

/// 明细里 `error` 摘要的字符上限。
///
/// 上游报错可能带上整段 HTML/长文案（`read_upstream_error` 自己截到 500 字符），
/// 但报表页是把 error 直接铺在列表里的一列 —— 200 字符足够看清「为什么失败」，
/// 再长只会把行撑爆。按**字符**截而不是字节：中文报错按字节截会切出半个字。
const ERROR_SUMMARY_CHARS: usize = 200;

/// 客户端中断 / 服务退出导致响应流被提前丢弃时的错误摘要。
///
/// HTTP 状态早就发出去了（2xx），明细里只能靠这条文案解释「为什么没有 token」。
const STREAM_ABORTED: &str = "响应流未完整下发（客户端中断或服务退出）";

/// POST /v1/chat/completions
pub async fn chat_completions(
    State(state): State<ServerState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // 请求开始时刻（请求统计用）。放在最前面：它要覆盖 body 解析与选路的耗时，
    // 而不是只覆盖「已经确定要转发」之后的那段。
    let started_at = logging::now_ms();
    // ① body 必须是 JSON 对象（数组/标量/null 都算非法）
    let parsed = serde_json::from_slice::<Value>(&body).ok();
    let Some(mut payload) = parsed.filter(Value::is_object) else {
        let error = GatewayError::bad_request("请求体必须是 JSON 对象");
        // body 都没解析出来，模型自然无从谈起 —— 记一条空模型的失败明细，
        // 让「客户端配错了」这类问题在报表里也看得见（见 record_early_failure）
        record_early_failure(&state, started_at, "", &error);
        return error.payload_response();
    };
    // ② messages 必须是数组
    if !payload
        .get("messages")
        .map(Value::is_array)
        .unwrap_or(false)
    {
        let error = GatewayError::bad_request("缺少 messages 数组");
        let model = model_field_text(&payload);
        record_early_failure(&state, started_at, &model, &error);
        return error.payload_response();
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
        record_early_failure(&state, started_at, &requested_model, &error);
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
    // usage / 尝试次数的旁路槽：本函数持有一份，另一份随请求进转发链路。
    // 流式请求的收尾在响应流里发生（那时本函数早就返回了），两份克隆指向
    // 同一份数据，所以读得到最终值。
    let telemetry = Arc::new(RequestTelemetry::new());
    let outcome = state
        .upstream()
        .forward(ForwardRequest {
            body: payload,
            stream,
            dedupe_key,
            telemetry: telemetry.clone(),
        })
        .await;
    // 记账句柄在这里取：下面各分支都要用（`state` 在本函数结束时才析构）
    let stats = state.request_stats();

    match outcome {
        Ok(ForwardOutcome::Stream { status, stream }) => {
            // 流式：记账**不能**在这里做 —— 这里只是「响应头已就绪」，
            // 内容还在下发。包装一层，由流自己在跑完/被丢弃时记账，
            // 于是 durationMs 覆盖到「最后一个字节发完」，中断也能记上一条。
            //
            // 状态码与 `sse_response` 用同一套归一（非法 u16 落 200），
            // 明细里记的必须是客户端实际看到的那个码
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
            let context = RecordContext {
                stats,
                telemetry,
                started_at,
                model: requested_model.clone(),
                status: i64::from(status.as_u16()),
            };
            sse_response(status, Box::new(RecordingStream::new(stream, context))).into_response()
        }
        Ok(ForwardOutcome::Completion { body }) => {
            // 非流式：聚合已经完成，此刻就是用户视角的「请求完成点」
            record_entry(
                &RecordContext {
                    stats,
                    telemetry,
                    started_at,
                    model: requested_model.clone(),
                    status: 200,
                },
                None,
            );
            json_response(body)
        }
        Err(error) => {
            // headers 还没发出（流式还没开始）→ 直接给 OpenAI 风格错误
            logging::log("[Model]", &format!("❌ {}", error.message));
            // 状态码取**实际下发**的那个（与 payload_response 同一口径）：
            // 非法的 status_code 会被归一成 500，明细要与客户端看到的一致
            let status = i64::from(error.http_status().as_u16());
            let message = error.message.clone();
            record_entry(
                &RecordContext {
                    stats,
                    telemetry,
                    started_at,
                    model: requested_model.clone(),
                    status,
                },
                Some(message),
            );
            error.payload_response()
        }
    }
}

/// ─── 请求记账（请求统计的唯一写入点）────────────────────────

/// 一条请求的收尾上下文：流式分支要把它交给响应流，等流真的结束时再记账。
///
/// 为什么字段是「值」而不是引用：`RecordContext` 会被移进响应流，
/// 而响应流是 `'static`（它要活得比 handler 的栈帧久）。
struct RecordContext {
    /// 统计存储句柄（`Arc` 克隆，与 ServerState 里那份是同一实例）
    stats: Arc<RequestStats>,
    /// usage / 尝试次数旁路槽
    telemetry: Arc<RequestTelemetry>,
    /// 请求开始时刻（毫秒 Unix 时间戳）
    started_at: i64,
    /// **实际使用**的模型（默认模型回落、目录兜底都已生效的那个）
    model: String,
    /// 下发给客户端的 HTTP 状态码
    status: i64,
}

/// 转发前就失败（body 非法 / messages 缺失 / 模型不在目录）时的记账。
///
/// 这些请求**一次都没往上游发**，所以 attempts 记 0 会被存储层夹成 1
/// （契约是「含首次、恒 ≥1」，0 不是一个合法的尝试次数）—— 用 1 表示
/// 「至少被处理过一次」，这与「上游被打了 N 次」在报表里是两回事，
/// 报表侧靠 status 与 error 区分。
///
/// 为什么这几条也要记：`model_not_found` 是客户端配置错误最常见的形态，
/// 不记的话用户在报表里看不到「请求全在失败」，只会以为统计漏了。
fn record_early_failure(
    state: &ServerState,
    started_at: i64,
    model: &str,
    error: &GatewayError,
) {
    let context = RecordContext {
        stats: state.request_stats(),
        telemetry: Arc::new(RequestTelemetry::new()),
        started_at,
        model: model.to_string(),
        // 与 `payload_response` 同一口径：非法状态码会被归一成 500
        status: i64::from(error.http_status().as_u16()),
    };
    record_entry(&context, Some(error.message.clone()));
}

/// 记一条请求明细。
///
/// ── 记账为什么绝不能影响请求 ─────────────────────────────────
/// `RequestStats::record` 本身不返回 `Result`：内部对写盘失败只打
/// `[Stats] 统计写入失败`，锁中毒走 `poisoned.into_inner()` 继续用，
/// 序列化失败跳过该条 —— 也就是存储层已把「统计失败」全部收敛成「少记一条」，
/// 没有任何 unwind 路径，所以这里不需要 `catch_unwind`，也不可能因为
/// 统计把用户请求带崩（release 是 panic=abort，这一点是硬要求）。
///
/// ── 字段口径 ────────────────────────────────────────────────
///   ts          请求**开始**时刻（不是记账时刻）：趋势图要按「用户什么时候
///               发的请求」归日，长请求若按收尾时刻归档会落到错误的日期
///   durationMs  收尾 - 开始（含排队、选路、上游等待、流下发）
///   attempts    旁路槽里累计的上游请求数；一次都没发出去（400/裸错误）时才回落 1
///   error       旁路槽里的原因优先（更接近根因），否则用调用方给的兜底文案；
///               成功请求两者都没有 → 落盘为 null
fn record_entry(context: &RecordContext, fallback_error: Option<String>) {
    let snapshot = context.telemetry.snapshot();
    // 收尾时刻只取一次：明细里的 durationMs 与日志里打的那一个是同一个值
    // （取两次会让两处偶尔差 1ms，排障时看着像对不上账）
    let finished_at = logging::now_ms();
    let duration_ms = finished_at - context.started_at;
    let error = snapshot
        .error
        .or(fallback_error)
        .map(|text| truncate_chars(&text, ERROR_SUMMARY_CHARS));
    let attempts = snapshot.attempts.max(1);
    let mut entry = NewRequestEntry::new(context.model.clone(), context.status);
    entry.ts = Some(context.started_at);
    entry.duration_ms = duration_ms;
    entry.attempts = attempts;
    // 存储契约里这两个字段是 String（不是 Option），空串就是「没有账号」的表示
    // —— 未配置账号列表、走默认登录态转发时就是这种情况
    entry.account_id = snapshot.account_id;
    entry.account_name = snapshot.account_name;
    entry.error = error;
    // 失败请求的 token 由存储层归一成 0（见 NewRequestEntry::normalize），
    // 这里照抄上游上报的原值即可，不必自己判成功与否
    entry.prompt_tokens = snapshot.prompt_tokens;
    entry.completion_tokens = snapshot.completion_tokens;
    entry.total_tokens = snapshot.total_tokens;
    entry.cache_read_tokens = snapshot.cache_read_tokens;
    context.stats.record(entry);
    logging::verbose(
        "[Stats]",
        &format!(
            "记一条请求: model={} status={} {duration_ms}ms attempts={attempts} tokens={}+{}（缓存 {}）",
            if context.model.is_empty() { "(未指定)" } else { &context.model },
            context.status,
            snapshot.prompt_tokens,
            snapshot.completion_tokens,
            snapshot.cache_read_tokens,
        ),
    );
}

/// 按字符截断（超出部分用 `…` 收尾）。
fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut out: String = text.chars().take(limit).collect();
    out.push('…');
    out
}

/// 边透传边记账的响应流。
///
/// ── 为什么包一层（而不是在 handler 里记账）────────────────────
/// 流式请求的「用户视角完成点」是**最后一个字节发完**，而 handler 在交出
/// 响应头时就返回了。包一层之后，明细的 durationMs 覆盖整个下发过程，
/// 客户端中途断开、上游断流这两种「非正常收尾」也能各记一条（否则这些请求
/// 在报表里会整条消失，看起来像统计漏了）。
///
/// ── 透传是否受影响 ──────────────────────────────────────────
/// 不影响：本类型对每个 `Item` 原样转发（只是补一个「是不是到 None 了」的
/// 观察），不读、不改、不缓存字节，也不吞错误 —— 客户端收到的字节序列与
/// 不包这一层时完全一致。
struct RecordingStream {
    inner: Box<dyn futures::Stream<Item = Result<Bytes, std::io::Error>> + Send + Unpin>,
    /// 收尾上下文；`take()` 走即表示「已记账」（保证恰好记一条）
    context: Option<RecordContext>,
}

impl RecordingStream {
    fn new(
        inner: Box<dyn futures::Stream<Item = Result<Bytes, std::io::Error>> + Send + Unpin>,
        context: RecordContext,
    ) -> Self {
        Self { inner, context: Some(context) }
    }

    /// 记账并清空上下文（幂等：第二次调用什么都不做）
    fn settle(&mut self, fallback_error: Option<String>) {
        if let Some(context) = self.context.take() {
            record_entry(&context, fallback_error);
        }
    }
}

impl futures::Stream for RecordingStream {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use futures::StreamExt;
        // 本类型的字段全部是 Unpin（Box / Option<String> / Arc / i64），
        // 自身也就 Unpin，get_mut 是安全的（没有结构体钉住的字段）
        let this = self.get_mut();
        // 已记账（说明上游流已结束）→ 不再去 poll 上游，直接报结束。
        // 少了这一步，axum 在收到 None 之后若再 poll 一次，就会去碰
        // 已经结束的底层流（reqwest 的流在 None 之后行为未定义）。
        if this.context.is_none() {
            return std::task::Poll::Ready(None);
        }
        let polled = this.inner.poll_next_unpin(cx);
        if matches!(polled, std::task::Poll::Ready(None)) {
            // 正常收尾：此刻记账，durationMs 就是真实的下发耗时
            this.settle(None);
        }
        polled
    }
}

impl Drop for RecordingStream {
    fn drop(&mut self) {
        // 还能走到这里，说明响应流**没有**跑到 None 就被丢弃了
        // （客户端断开 / 服务退出）。仍记一条 —— 请求确实发生了，
        // 让它从报表里消失比记成「中断」更糟。
        self.settle(Some(STREAM_ABORTED.to_string()));
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
///
/// 收 `StatusCode` 而不是 `u16`：调用点要先做一次「非法码落 200」的归一
/// （记账要用同一个值），归一放在调用点、这里只负责下发，避免两处各写一遍。
fn sse_response(
    status: StatusCode,
    stream: Box<dyn futures::Stream<Item = Result<Bytes, std::io::Error>> + Send + Unpin>,
) -> Response {
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

