//! 对话链路路由（对照 server.mjs 466-560、956-961 行逐条实现）。
//!
//!   POST /v1/chat/completions   protected（Node 版这条查 API Key）
//!   GET  /v1/models             public（Node 版这条**不查** API Key ——
//!                               它是只读探针，客户端启动时常在配 key 之前调用）
//!
//! 另外两条协议入口（`/v1/responses`、`/v1/messages`）在 `api::protocol` 里，
//! 它们与本文件共用 `api::pipeline` 的公共管道（模型解析 / 记账 / 响应构造），
//! 差异只在出入口的协议翻译（见 `core::protocol` 模块头）。
//!
//! ── handleModelRequest 的处理顺序（照抄，别调整）──────────────
//!   ① body 必须是 JSON 对象（400「请求体必须是 JSON 对象」）
//!   ② 必须有 messages 数组（400「缺少 messages 数组」）
//!   ③ verbose 日志：method/path/model/stream/msgs/bytes/ua
//!   ④ 调试落盘 {config_dir}/debug/last-request.json + .meta.txt（失败不影响请求）
//!   ⑤ 模型校验：未指定 → defaultModel → 目录 isDefault → 首项；
//!      点名的模型不在目录 → 400（带相近模型提示）
//!   ⑥ rememberRequestModel（默认值已填充完毕）
//!   ⑦ 转发：流式透传 / 非流式聚合
//!
//! ── 错误中途写出 ────────────────────────────────────────────
//! 转发失败时：headers 还没发出 → errorPayload（OpenAI 风格 + 429 的 reset_at）；
//! headers 已发出（流式已开始）→ 由响应流自身报错终止连接（Node 版是补写
//! `data: {"error":...}` + `data: [DONE]`，Rust 侧的等价语义见 forward 模块注释）。
//!
//! ── 内容处理（指纹脱敏）的接入位置 ──────────────────────────
//! 本文件**不做内容处理**：请求体按客户端原样交给转发层，去重键也取自原始
//! 请求体。处理发生在**某一家 provider 即将发送前**，是否处理由配置开关
//! `sanitizeBlacklistFingerprints` 决定（见 `core::upstream::payload` 的
//! `send_body`）—— 开关关着时无论首选还是故障转移，拿到的都是未修改的请求体。
//!
//! ── 请求记账（本切片接入）──────────────────────────────────
//! 本文件是 `RequestStats::record` 的**唯一调用方**（见 `record_entry`）：
//! 无论成功 / 最终失败 / 客户端中断，一次用户请求**只记一条** ——
//! 429 自动换账号属于同一次请求，不额外记账。
//! 流式分支把记账交给 `RecordingStream`（收尾发生在 handler 返回之后），
//! 非流式与转发前失败则在原地记账。usage 由 `core::upstream` 的旁路槽提供。
//!
//! ── 公共管道已抽到 `api::pipeline`（三协议共用）──────────────
//! 模型解析、记账、响应构造、调试落盘这四件事现在住在那里面，本文件只保留
//! 「Chat 协议专属」的部分：messages 数组的校验、OpenAI 风格的错误体。
//! 这样 `/v1/responses` 与 `/v1/messages` 能共用同一份口径，不会漂移。

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Extension, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};

use crate::server::core::upstream::usage::RequestTelemetry;
use crate::server::core::upstream::{ForwardOutcome, ForwardRequest};
use crate::server::errors::GatewayError;
use crate::server::http::raw_json;
use crate::server::logging;
use crate::server::ServerState;

use super::pipeline::{
    self, json_response, model_field_text, record_early_failure, record_entry, sse_response,
    write_debug_files, RecordContext, RecordingStream,
};

/// POST /v1/chat/completions
pub async fn chat_completions(
    State(state): State<ServerState>,
    headers: HeaderMap,
    // R9：中间件放进请求扩展的「命中 Key 的限制」。`Option` 是刻意的 ——
    // 免鉴权模式、环境变量 Key、转发链路之外的管理调用都没有它，
    // 而「取不到 = 不限制」正是本需求的语义（见 `core::key_scope` 模块头）。
    // axum 对 `Option<Extension<T>>` 有专门实现：取不到就是 `None`，不会像
    // 裸 `Extension<T>` 那样直接拒绝请求（那会把整个免鉴权模式打回 500）。
    key_scope: Option<Extension<crate::server::core::key_scope::KeyScope>>,
    body: Bytes,
) -> Response {
    let scope = key_scope.map(|Extension(scope)| scope);
    // 请求开始时刻（请求统计用）。放在最前面：它要覆盖 body 解析与选路的耗时
    let started_at = logging::now_ms();
    // ① body 必须是 JSON 对象（数组/标量/null 都算非法）
    let parsed = serde_json::from_slice::<Value>(&body).ok();
    let Some(mut payload) = parsed.filter(Value::is_object) else {
        let error = GatewayError::bad_request("请求体必须是 JSON 对象");
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
    let stream = payload.get("stream").map(|value| value == &Value::Bool(true)).unwrap_or(false);
    let message_count = payload
        .get("messages")
        .and_then(Value::as_array)
        .map(|items| items.len())
        .unwrap_or(0);
    let user_agent = pipeline::user_agent_of(&headers);

    // ③ 调试落盘：保存最近一次入站请求体，供重放分析（覆盖写）
    write_debug_files(&body, method, path, &user_agent);

    // ④ 模型路由走公共管道（默认模型回落 / 别名映射 / 目录校验，
    //    与另外两条协议入口逐条同源）。下游原始请求名在改写前取一次：
    //    请求日志的「下游模型」显示的是客户端发来的名字，不是解析后的。
    //    第三参是这把 Key 的可用模型白名单（None = 不限制）
    let client_model = model_field_text(&payload);
    let requested_model = match pipeline::resolve_model(&state, &mut payload, scope.as_ref()) {
        Ok(model) => model,
        Err(error) => {
            record_early_failure(&state, started_at, &model_field_text(&payload), &error);
            return error.payload_response();
        }
    };
    logging::verbose(
        "[Model]",
        &format!(
            "← {method} {path} model={} stream={stream} msgs={message_count} bytes={} ua={user_agent}",
            if requested_model.is_empty() { "(未指定)" } else { &requested_model },
            body.len(),
        ),
    );

    // ⑤ 转发：请求体原样交给转发层（内容处理在每家 provider 发送前按作用范围
    // 逐家判定，见 `core::upstream::payload` 的 `send_body`）；去重键取**客户端
    // 原始请求体**的哈希，与「某一家是否被处理」无关。
    let dedupe_key = pipeline::sha256_hex(&body);
    let telemetry = Arc::new(RequestTelemetry::with_id());
    let outcome = state
        .upstream()
        .forward(ForwardRequest {
            body: payload,
            stream,
            dedupe_key,
            client_headers: headers,
            telemetry: telemetry.clone(),
            // 提供商白名单进转发层（选路时按承载家过滤）；模型白名单已经在
            // `resolve_model` 里判过（见那里的说明，两者分工不同）
            allowed_providers: scope,
        })
        .await;
    let stats = state.request_stats();

    match outcome {
        Ok(ForwardOutcome::Stream { status, stream: source }) => {
            // 流式：记账**不能**在这里做 —— 这里只是「响应头已就绪」，
            // 内容还在下发。包装一层，由流自己在跑完/被丢弃时记账
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
            let context = RecordContext {
                stats,
                telemetry,
                started_at,
                model: requested_model.clone(),
                client_model: client_model.clone(),
                status: i64::from(status.as_u16()),
            };
            // 收尾帧特征取 Chat 的：客户端读到 `data: [DONE]` 就停是常态写法，
            // 那时连接会被立刻关掉、`Drop` 不会被拉到 EOF（见 `RecordingStream`）
            sse_response(
                status,
                Box::new(RecordingStream::with_terminals(
                    source,
                    context,
                    pipeline::terminal::CHAT,
                )),
            )
            .into_response()
        }
        Ok(ForwardOutcome::Completion { body }) => {
            record_entry(
                &RecordContext {
                    stats,
                    telemetry,
                    started_at,
                    model: requested_model.clone(),
                    client_model: client_model.clone(),
                    status: 200,
                },
                None,
            );
            json_response(body)
        }
        Err(error) => {
            // headers 还没发出（流式还没开始）→ 直接给 OpenAI 风格错误。
            // 这行只在终端：同一条 message 紧接着由 `record_entry` 的
            // fallback_error 记进请求日志的「错误」列（含被 Key 白名单拒绝、
            // 模型不可用这类网关自己的判定）—— 运行日志页不再为每一次失败的
            // 请求写一行。
            logging::console_line("[Model]", &format!("❌ {}", error.message));
            let status = i64::from(error.http_status().as_u16());
            let message = error.message.clone();
            record_entry(
                &RecordContext {
                    stats,
                    telemetry,
                    started_at,
                    model: requested_model.clone(),
                    client_model: client_model.clone(),
                    status,
                },
                Some(message),
            );
            error.payload_response()
        }
    }
}

/// GET /v1/models —— 免鉴权（Node 版这条没调 checkApiKey）。
///
/// 响应体来自**聚合模型目录**（Agent2API 改造 §4.4）：合并「当前有可用账号」
/// 的各 provider 清单，同名去重（保留注册表顺序靠前的那家），OpenAI 响应结构不变。
/// 对只有 workbuddy 一家且有账号的用户，输出与改造前逐字段相同
/// （见 `core::providers::catalog` 模块头）。
///
/// 聚合查询需要账号存储判断「哪几家可用」，所以这里用 `state.store()` ——
/// 句柄是 `Clone` 的轻量 Arc，且绝不在持锁时做网络请求（这是本项目的硬约束，
/// 聚合层只调 `accounts_for_provider` 做一次文件读取）。
///
/// ── R9：带 Key 请求时只返回被授权的模型 ────────────────────────
/// 这条路由挂在**免鉴权组**（Node 版就是不查 API Key 的只读探针），但它仍然
/// 要认 Key：客户端带着某把 Key 来拉列表时，只该看到那把 Key 被授权的部分 ——
/// 否则「限制」在下游看起来根本不存在（列表里有，点了却 404，是最难解释的一种）。
/// 所以这里主动从请求头解析命中的 Key（`http::key_scope_from_headers`，
/// 与鉴权中间件**共用同一套匹配口径**），把它的白名单交给
/// `catalog::models_response` 在列表生成处过滤。
///
/// 没带 Key / 带的是没命中的 Key / 网关一把启用的 Key 都没有（免鉴权模式）：
/// 一律**不过滤**（空 `scope`）。这是刻意的：`/v1/models` 是探针，
/// 在客户端配好 Key 之前就会被打一次（README 的接口说明也这么写），
/// 那种请求拿不到完整列表会让「装完先看看有哪些模型」这一步直接失效。
/// 与 `key_scope` 模块头列的三条「不限制」情形是同一套语义。
pub async fn list_models(State(state): State<ServerState>, headers: HeaderMap) -> Response {
    let scope = crate::server::http::key_scope_from_headers(&headers);
    let body = crate::server::core::providers::catalog::models_response(state.store(), scope.as_ref());
    // 异步刷新模型目录，不阻塞响应（对照 Node 的 `void refreshModelCatalog()`）
    spawn_catalog_refresh(&state);
    // Anthropic 客户端（Claude Code / Claude Desktop）走同一路径，但它们
    // 只认 Anthropic 原生的列表形态（`type:"model"` + `display_name` +
    // 顶层 `has_more`/`first_id`/`last_id`），拿到 OpenAI 的
    // `{object:"list"}` 会解析不出任何模型。判据用 `anthropic-version` 头
    // ——那是 Anthropic SDK 必带的，OpenAI 客户端不会发。
    //
    // ── 为什么不拆成 /anthropic/v1/models ───────────────────────
    // Claude Code 的网关发现是**直接打 `{ANTHROPIC_BASE_URL}/v1/models`**
    // 的（它自己拼路径，不会加前缀），拆路径等于让发现永远失败。
    // 按请求头分流是这里唯一可行的做法。
    if headers.contains_key("anthropic-version") || headers.contains_key("x-api-key") {
        return json_response(anthropic_models_view(&body));
    }
    raw_json(body)
}

/// 把聚合目录的 OpenAI 形态转成 Anthropic 原生列表形态。
///
/// 字段对应关系（Anthropic 官方 List Models 响应）：
///   `data[].id`            ← 原 `id`
///   `data[].type`          ← 固定 `"model"`
///   `data[].display_name`  ← 原 `name`（缺省回落 id）
///   `data[].created_at`    ← 原 `created`（Unix 秒转 ISO；缺省给当前时刻）
///   顶层 `has_more` / `first_id` / `last_id` 由列表首尾算出
///
/// 额外字段（`maxInputTokens` 等）**保留**：Anthropic 的客户端会忽略不认识的
/// 键，而本项目的其它前端（模型管理页）需要它们。
fn anthropic_models_view(body: &Value) -> Value {
    let empty: Vec<Value> = Vec::new();
    let items = body.get("data").and_then(Value::as_array).unwrap_or(&empty);
    let mut data: Vec<Value> = Vec::with_capacity(items.len());
    for item in items {
        // 注意取值函数：这里要的是**成员值本身**，不是 `model_field_text`
        // ——那个函数读的是请求体的 `model` 字段，传一个裸字符串进去只会得到空串
        let text_of = crate::server::core::protocol::string_value;
        let id = item.get("id").map(text_of).unwrap_or_default();
        let display = {
            let name = item.get("name").map(text_of).unwrap_or_default();
            if name.is_empty() { id.clone() } else { name }
        };
        let mut entry = match item.as_object() {
            Some(map) => map.clone(),
            None => serde_json::Map::new(),
        };
        entry.insert("id".to_string(), Value::String(id.clone()));
        entry.insert("type".to_string(), Value::String("model".to_string()));
        entry.insert("display_name".to_string(), Value::String(display));
        // created_at：Anthropic 用 ISO-8601 字符串；本地目录没有创建时间，
        // 用「目录最近刷新时刻」代替（比编一个假日期诚实），拿不到就省略
        if let Some(refreshed) = body.pointer("/meta/lastRefreshedAt").and_then(Value::as_i64) {
            if refreshed > 0 {
                if let Some(utc) = chrono::DateTime::from_timestamp_millis(refreshed) {
                    entry.insert(
                        "created_at".to_string(),
                        Value::String(utc.format("%Y-%m-%dT%H:%M:%SZ").to_string()),
                    );
                }
            }
        }
        data.push(Value::Object(entry));
    }
    let first = data.first().and_then(|item| item.get("id")).cloned().unwrap_or(Value::Null);
    let last = data.last().and_then(|item| item.get("id")).cloned().unwrap_or(Value::Null);
    json!({
        "data": data,
        "has_more": false,
        "first_id": first,
        "last_id": last,
    })
}

/// POST /v1/messages/count_tokens
///
/// Anthropic 的官方契约里这是**独立端点**（Claude Code 启动时会探它）。
/// 真正的分词只有上游知道，而本项目六家上游都没有对应的 tokenizer 接口 ——
/// 所以这里给一个**估算**并如实标注 `estimated: true`：
/// 按「字符数 ÷ 3.5」估（中英混排的经验值，与 Anthropic 官方「约 3.5 字符
/// 一个 token」的量级一致）。
///
/// ── 为什么不直接 404 ────────────────────────────────────────
/// Claude Code 在网关模式下会调它做上下文预算；404 会让它按「网关不支持」
/// 处理（行为随版本变化，可能是降级也可能是报错）。给一个量级正确的估算，
/// 比让它拿到 404 更接近「能用」。宁可数字粗略，也不要让整条链断掉。
pub async fn count_tokens(State(state): State<ServerState>, body: Bytes) -> Response {
    let parsed = serde_json::from_slice::<Value>(&body).ok();
    let Some(payload) = parsed.filter(Value::is_object) else {
        let error = GatewayError::bad_request("请求体必须是 JSON 对象");
        return error.payload_response();
    };
    // 复用 Anthropic → Chat 的转换来统计：这样「工具定义 / 图片块 / system」
    // 的文本都被算进去，口径与真正转发时一致（转换失败则按原始 JSON 估）
    let text = match crate::server::core::protocol::anthropic::chat_from_anthropic(&payload) {
        Ok(chat) => serde_json::to_string(&chat).unwrap_or_default(),
        Err(_) => serde_json::to_string(&payload).unwrap_or_default(),
    };
    let characters = text.chars().count();
    let tokens = ((characters as f64) / 3.5).ceil().max(1.0) as i64;
    let _ = state;
    json_response(json!({
        "input_tokens": tokens,
        "estimated": true,
    }))
}

/// 起一个后台任务刷新模型目录（不阻塞当前响应）。
///
/// 刷新走**适配器注册表**（`providers::adapter::refresh_implemented`）：
/// 每个已实现的 provider 自己决定去哪儿拉清单（workbuddy 是 /v3/config），
/// 本函数不认识任何一家。W3 加上小浣熊适配器后这里一行都不用改。
///
/// 必须用 `tauri::async_runtime::spawn` 而不是 `tokio::spawn`：
/// 本函数在 axum handler 里调用（运行时上下文已成立），但显式选择项目里
/// 统一的 spawn 入口，避免以后有人把它挪到非 Tokio 上下文时 panic
/// （release 是 panic=abort，那会带走整个桌面应用）。
pub fn spawn_catalog_refresh(state: &ServerState) {
    let store = state.store().clone();
    tauri::async_runtime::spawn(async move {
        crate::server::core::providers::adapter::refresh_implemented(&store).await;
    });
}
