//! 对话链路的公共管道：模型解析、请求记账、响应构造、调试落盘。
//!
//! ── 为什么需要这个模块 ──────────────────────────────────────
//! 网关对外提供三条对话入口（`/v1/chat/completions`、`/v1/responses`、
//! `/v1/messages`），它们的**差异只在协议翻译**上，其余三件事完全相同：
//!   1. 模型解析（默认模型回落 → 映射别名 → 禁用/存在性校验）
//!   2. 请求记账（每个用户请求恰好一条明细，流式也要覆盖到最后字节）
//!   3. 响应构造（JSON / SSE 的头与状态码）
//! 这三件事从 `api/chat.rs` 抽到这里，三条入口共用一份 —— 否则任何一处
// 口径调整（比如记账字段、默认模型回落顺序）都要在三个文件里各改一遍，
// 迟早会漂移成三套不一致的行为。
//!
//! ── 与 `api/chat.rs` 的关系 ─────────────────────────────────
//! `chat.rs` 仍是对外入口（`/v1/chat/completions` 的 handler），它现在从本模块
//! 取公共件；`api/protocol.rs` 里的两个新入口同理。**本模块不认识任何具体
//! 协议**（不看 messages / input / content），协议差异一律留在各 handler 里。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：零 unwrap/expect/panic。

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::Response;
use serde_json::Value;

use crate::server::config;
use crate::server::core::key_scope::{self, KeyScope};
use crate::server::core::providers::catalog::{
    advertised_manifest_contains, default_model_catalog, default_model_usable,
    has_available_providers, model_blocked_everywhere, suggest_advertised,
};
use crate::server::core::upstream::usage::{self, RequestTelemetry, TelemetrySnapshot};
use crate::server::errors::GatewayError;
use crate::server::logging;
use crate::server::request_stats::{
    AttemptDetail, MAX_RAW_BODY_BYTES, NewRequestEntry, RequestStats, RetryEvent, RunningProgress,
    SensitiveHit,
};
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
pub const STREAM_ABORTED: &str = "响应流未完整下发（客户端中断或服务退出）";

/// 一条「本协议收尾帧」的字节特征（见 [`RecordingStream`] 的说明）。
///
/// 由**调用方**按自己那条协议给出（三条入口各知道自己发什么收尾），本模块
/// 只当字节比对，不认识任何协议 —— 这是 `api::pipeline` 的既定边界
/// （见模块头：「本模块不认识任何具体协议」）。
///
/// 匹配串**必须带帧尾**：收尾帧在线上是完整的一帧（`data: [DONE]\n\n` /
/// `event: response.completed\n`），带上 `\n` 才不会误命中正文里出现的同名字符串
/// —— 模型完全可以吐出一段包含 `data: [DONE]` 的文字，那种命中会让一次**真的**
/// 中断被记成成功。这个精度是刻意的：宁可少认（退化成改前的行为，
/// 只是多一条摘要），也不能把真实中断洗成成功。
pub type TerminalFrames = &'static [&'static [u8]];

/// SSE 收尾帧的字节特征表（三条入口各取自己那几条）。
///
/// 单独定义在这几个常量里而不是散在调用点：它们与各状态机的产出是**成对**
/// 的事实，写在一起才能一眼看出「谁跟谁对得上」。
pub mod terminal {
    /// Chat Completions：`ReasoningCoalescer` 透传的 `data: [DONE]\n\n`
    pub const CHAT: super::TerminalFrames = &[b"data: [DONE]\n\n"];
    /// Responses：正常收尾 `response.completed`，失败收尾 `response.failed`
    pub const RESPONSES: super::TerminalFrames =
        &[b"event: response.completed\n", b"event: response.failed\n"];
    /// Anthropic Messages：`message_stop`（成功与流内错误都以它收尾）
    pub const ANTHROPIC: super::TerminalFrames = &[b"event: message_stop\n"];
}

// ─── 模型解析 ───────────────────────────────────────────────

/// 解析并落地请求体里的模型名，返回**实际使用**的模型。
///
/// 顺序（与改造前的 `/v1/chat/completions` 逐条一致）：
///   ① 未指定 → 默认模型回落链（config 的 defaultModel → 目录 isDefault → 目录首项）
///   ② 路由全链（原生 + 映射）全关闭 → 404（`model_not_found`）
///   ③ **广告视图里没有** → 404（带相近模型提示，`model_not_found`）
///
/// ── ② 与 ③ 的状态码为什么是 404（本轮修正）────────────────────
/// 这两个判定的语义都是「这个模型对你（下游）不存在」：② 是网关主动关闭、
/// ③ 是目录里压根没有。改造前用 400 表达「请求有问题」，但 OpenAI 兼容协议
/// 对「模型不存在」的约定是 **404 + code=model_not_found**（OmniProxy 的
/// `fail(res, ..., 404, 'model_not_found')` 同一形态，客户端据此识别并刷新
/// 模型列表）。改用 404 还消除了一个不一致：R9 白名单那条原本就是 404，
/// 同一个 code 下的三条路径现在状态码也一致了。
///
/// ── ③ 为什么以广告视图为准（「列表里没有就拒绝」）──────────────
/// 客户端手里的模型清单来自 `/v1/models`；点一个那里没有的名字，应当立刻
/// 404，而不是转给上游换回一个与真实原因无关的上游报错。收窄掉的模型
/// （Cline 按账号额度池收窄、没有可用账号的家、被关闭的）因此**不再
/// 可点名调用** —— 校验口径与广告口径就此统一，不存在「列表里看不到却能调通」
/// 的中间态。判定见 `catalog::advertised_manifest_contains`。
///
/// 两处**例外**（都不做这层判定，交给转发层给更准确的错误）：
///   - 客户端**没点名**（走 ① 注入的默认模型）：默认值是网关自己挑的，
///     不该被自己的广告视图否掉；
///   - **一家可用提供商都没有**（没加账号）：此时广告视图必然为空，
///     一律报「模型不存在」会盖掉「没有可用账号，无账号可转发」这条
///     可操作的提示。同一条例外对下面的白名单判定也生效（理由见那里）。
///
/// ── 映射为什么不再在这里改写（照抄 OmniProxy 的候选语义）──────
/// 改造前的映射是「请求名命中 alias → payload 整体改写成 target」：请求名
/// 与上游 id 同名时，改写会**遮蔽**那个同名的上游模型（它的原生路由收不到
/// 流量），所以旧版干脆禁止同名。映射语义重做后（同名允许、同名校多提供商
/// 主备），改写下沉到**发送侧按家进行**（`payload::send_body` →
/// `catalog::wire_target_for_provider`）：请求名全程保持客户端原值，原生承载
/// 家收到本名、映射家收到各自的 target —— 候选链的展开在
/// `providers::router::route_for_forward`。
///
/// `payload` 会被就地改写（仅填默认模型）：后续转发与记账都用一致的请求名，
/// 而客户端原始 body 的去重键在调用方算（见各 handler）。
///
/// ── `scope`：网关 Key 的可用模型白名单（R9）────────────────────
/// `None` = 不限制（免鉴权模式 / 环境变量 Key / 未知 Key，见 `core::key_scope`）。
/// 有 scope 时在**这里**判，而不是留到转发层：模型白名单的语义是
/// 「这个模型对你这把 Key 不存在」，所以拒绝形态与「模型不在目录里」逐字相同
/// （404 + `model_not_found`，照抄 OmniProxy 的
/// `if (!keyAllowsModel(...)) return fail(res, \`模型 '${publicModel}' 不可用\`, 404, 'model_not_found')`）——
/// 连「存在性」都不该向下游暴露：能列出却打不通，是比 404 更难解释的一种。
///
/// 两条例外（与下面 ③ 的两条例外同源，各有各的理由）：
///   - 客户端**没点名**（走默认模型回落）：回落链的候选也要按白名单收窄
///     （见下方 `allowed` 过滤），否则会给一把只授权了 A 的 Key 注入一个它
///     没被授权的 B —— 那不是限制，那是「用网关的默认值绕过限制」。
///     收窄后一个都没有时给一条**可操作的**错误，而不是把无 model 的请求
///     转给上游（那样由上游挑模型，白名单等于不存在）。
///   - **一家可用提供商都没有**（没加账号，`active` 为空）：与 ③ 同一条例外 ——
///     广告视图必然为空，此时报「没有可用账号」比报「模型不存在」有用。
pub fn resolve_model(
    state: &ServerState,
    payload: &mut Value,
    scope: Option<&KeyScope>,
) -> Result<String, GatewayError> {
    let requested = model_field_text(payload);
    // 客户端是否**点名**了模型：默认值注入后这个事实就丢了，先记下来
    let client_named = !requested.is_empty();
    if !client_named {
        // 客户端没点名 → 网关默认。三级回落链的候选集合走
        // `default_model_catalog()`（按「认不认默认模型概念」收窄后的聚合清单）：
        // 一家都不认时不注入 model，按「未指定」处理（让上游用它自己的默认）
        let snapshot = config::current();
        let default_model = snapshot.default_model();
        // 白名单收窄：不限制时 `allowed` 恒真，等价于原逻辑（零行为差异）
        let allowed = |name: &str| key_scope::allows_model(scope, name);
        let models = default_model_catalog();
        let fallback = if default_model_usable(default_model) && allowed(default_model) {
            Some(default_model.to_string())
        } else {
            models
                .iter()
                .find(|model| {
                    model.get("isDefault").map(value_is_truthy).unwrap_or(false)
                        && model
                            .get("id")
                            .map(crate::server::core::models::shape_value_text)
                            .map(|id| allowed(&id))
                            .unwrap_or(false)
                })
                .or_else(|| {
                    models.iter().find(|model| {
                        model
                            .get("id")
                            .map(crate::server::core::models::shape_value_text)
                            .map(|id| allowed(&id))
                            .unwrap_or(false)
                    })
                })
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
        } else if scope.is_some_and(KeyScope::restricts_models) && !models.is_empty() {
            // 这条 Key 限制了模型、而目录里**没有一个是它被授权的**：
            // 继续往下走只会让上游拿它自己的默认模型去回答（白名单形同虚设），
            // 所以在这里就明确拒绝。文案指向该去改什么（管理页那两处）。
            return Err(GatewayError::bad_request(
                "这把网关 Key 的可用模型列表里没有任何当前可用的模型：请到「网关 Key」页为它勾选可用模型",
            )
            .with_code("model_not_found"));
        }
    }
    let requested_model = model_field_text(payload);
    // 按提供商区分启停后，404 判定是「路由全链（原生 + 映射）都不可用」
    if !requested_model.is_empty() && model_blocked_everywhere(&requested_model) {
        return Err(GatewayError::with_status(404, format!(
            "模型已在网关中关闭: {requested_model}。完整列表见 GET /v1/models"
        ))
        .with_code("model_not_found"));
    }
    // ③ 广告视图里没有 → 400（「列表里没有就拒绝」；例外见函数头说明）
    if client_named && !requested_model.is_empty() {
        // 例外：**一家可用提供商都没有**（没加账号）时跳过本判定 —— 广告视图
        // 必然为空，一律报「模型不存在」会盖掉「没有可用账号，无账号可转发」
        // 这条更可操作的提示（见函数头两条例外的第二条例）。
        // ── 这里的 active 用**未按 Key 收窄**的那一份（刻意）───────────
        // `/v1/models` 按 Key 的提供商白名单收窄后再广告（见
        // `catalog::models_response`），这里却用全量：两者的差异正好构成
        // 「模型存在、但这把 Key 不允许任何承载它的家」这一种情形。
        // 那种情形**必须由转发层拒**（`provider_loop::forward_with_providers`
        // 的可用提供商分支），因为只有它手里有候选链、能说清「提供它的家都被
        // 你这把 Key 挡了」；在这里按全量放行、到转发层给准确文案，
        // 比在这里用收窄后的视图报一句笼统的「模型不存在」对用户有用得多
        //（后者会让人去查模型管理页的启停，而真正要改的是 Key 的可用提供商）。
        if has_available_providers(state.store())
            && !advertised_manifest_contains(state.store(), &requested_model)
        {
            let hint = suggest_advertised(state.store(), &requested_model, 5);
            let message = format!(
                "模型不存在: {requested_model}{}。完整列表见 GET /v1/models",
                if hint.is_empty() {
                    String::new()
                } else {
                    format!("（目录里相近的模型: {}）", hint.join("、"))
                },
            );
            return Err(GatewayError::with_status(404, message).with_code("model_not_found"));
        }
        // R9 模型白名单：**排在广告视图校验之后**。顺序上先是「这个模型对谁都
        // 不存在」（目录口径），再是「对你这把 Key 不存在」—— 两者的响应形状
        // 逐字相同（同一个 code、同一个状态码），所以顺序只影响排查时的先后，
        // 但把「目录里根本没有」排在前面能让用户先看到更容易理解的那条。
        //
        // 与 ③ 的两条例外一致：客户端没点名（走默认模型）与「一家可用提供商都
        // 没有」都不在这里判（前者已在回落链里按白名单收窄过，后者交给转发层
        // 给「没有可用账号」那条更准的提示）。第二条例外在这个分支里不能省：
        // 白名单本身与「有没有账号」无关，但**没有账号时这把 Key 的路由照样
        // 不通**，报「不在白名单里」会把用户指向网关 Key 页，而真正要做的是
        // 先加账号。
        if has_available_providers(state.store())
            && !key_scope::allows_model(scope, &requested_model)
        {
            return Err(GatewayError::with_status(404, format!(
                "模型 '{requested_model}' 不可用：不在这把网关 Key 的可用模型列表里"
            ))
            .with_code("model_not_found"));
        }
    }
    // 记录本次实际用的模型（默认值已填充完毕），供账号页筛选默认选中
    if !requested_model.is_empty() {
        config::remember_request_model(&requested_model);
    }
    Ok(requested_model)
}

/// 请求体里的 model → 供校验/记录用的文本。
///
/// 复刻 Node 的取值链：`if (!body.model)` 先做真值判定（null/""/0/false 都算
/// 未指定），真值再交给目录查（`get()` 内部是 `String(id).toLowerCase()`），
/// 所以数字/对象这类非字符串值也会被字符串化后参与比对 —— 本函数做的就是
/// 那个字符串化，且**不改动 payload 里的原值**。
pub fn model_field_text(payload: &Value) -> String {
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

/// 下游请求体里客户端显式指定的思考等级（请求日志「下游等级」；空串 = 没指定）。
///
/// 与 [`model_field_text`] 同一采集时机（入口 handler 在解析出 payload 后取一次，
/// 随 [`RecordContext`] 一路带到记账点）。读取器是
/// `model_rules::reasoning::read_client_level` —— 那是**两家的并集链、只服务
/// 显示**：转发时「客户端指定了没有」由各家适配器自己的 resolver 判，两边
/// 口径的差异（CatPaw 空串会 400、Qoder 链到第一个键为止）不影响这里要显示
/// 的意图值。注意 payload 传入前可能已被 [`resolve_model`] 就地改写 —— 但它只
/// 动 `model` 字段，等级键不受影响，前后取值一致。
pub fn client_reasoning_of(payload: &Value) -> String {
    crate::server::core::model_rules::read_client_level(payload).unwrap_or_default()
}

/// JS 真值判定（`Boolean(x)`）
pub fn value_is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().map(|item| item != 0.0).unwrap_or(false),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// 从请求头取 User-Agent（日志用；缺失给 `-`）
pub fn user_agent_of(headers: &axum::http::HeaderMap) -> String {
    headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("-")
        .to_string()
}

// ─── 请求记账（请求统计的唯一写入点）────────────────────────

/// 一条请求的收尾上下文：流式分支要把它交给响应流，等流真的结束时再记账。
///
/// 为什么字段是「值」而不是引用：`RecordContext` 会被移进响应流，
/// 而响应流是 `'static`（它要活得比 handler 的栈帧久）。
pub struct RecordContext {
    /// 统计存储句柄（`Arc` 克隆，与 ServerState 里那份是同一实例）
    pub stats: Arc<RequestStats>,
    /// usage / 尝试次数旁路槽
    pub telemetry: Arc<RequestTelemetry>,
    /// 请求开始时刻（毫秒 Unix 时间戳）
    pub started_at: i64,
    /// **请求侧解析**的模型（默认模型回落、目录兜底都已生效；映射**不改写**
    /// 它 —— 客户端点名映射别名时就是别名本身，见 `resolve_model` 的说明）。
    /// 请求日志的主列显示它；报表按模型聚合**不**用它（映射别名会被当成
    /// 独立模型），统计键是明细里的 `upstream_model`（空回落本字段，
    /// 见 `fold_into_daily` 的 `model_stat_key`）。
    pub model: String,
    /// **下游请求的**模型名（客户端请求体里的原值，映射 / 默认注入生效前；
    /// 客户端没点名时为空串）。请求日志用它和上游名（telemetry 的
    /// `upstream_model`）分两行展示「请求的什么 → 转发的什么」。
    pub client_model: String,
    /// **下游请求体里**客户端显式指定的思考等级（`max` 等；空串 = 没指定）。
    /// 请求日志给下游模型名带 `(等级)` 后缀；与上游侧的
    /// `snapshot.upstream_reasoning` 分开记 —— 「客户端要的档位」与「实际发出的
    /// 档位」是两个读数（客户端没指定时映射绑定可补一个，承载家也可能归并）。
    /// 采集时机与 [`Self::client_model`] 相同（入口解析出 payload 后取一次）。
    pub client_reasoning: String,
    /// 下发给客户端的 HTTP 状态码
    pub status: i64,
    /// **下游原始请求体文本**（`request_raw` 表的请求侧；已按
    /// [`MAX_RAW_BODY_BYTES`] 截断，None = 无正文可存 —— 转发前就失败的
    /// 路径连 body 都没解析成）。采集点在各入口 handler（它们手里才有
    /// 客户端发来的原始字节）；与调试模式（`core::debug_traffic` 的**上游侧**
    /// 报文）平行，这条是**下游侧**、始终采集。
    pub raw_request: Option<String>,
    /// **响应正文文本**（`request_raw` 表的响应侧；同样截断到上限）。
    /// 非流式路径在记账前由入口直接填（完整 JSON）；流式路径构造时是 None，
    /// 由 `RecordingStream` 在流结束时用累积缓冲定稿 —— 那时才见得到最后一个字节。
    pub raw_response: Option<String>,
}

/// 把原始字节变成可入库的正文文本（请求侧 / 响应侧共用）。
///
/// 截断按**字节**进行并回退到 UTF-8 边界：上限是存储/内存的度量（字节），
/// 而正文是文本 —— 卡在多字节字符中间截断会切出半个字。先 `from_utf8_lossy`
/// 再按 str 截断一步到位：非法字节替换成 U+FFFD（报文本应是 UTF-8 JSON，
/// 替换只影响极端情形下的排障显示），`is_char_boundary` 保证不切半个字。
/// 空字节给 None（`store_raw` 两侧全空时不写行，这里提前短路）。
pub fn raw_body_text(bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() {
        return None;
    }
    let text = String::from_utf8_lossy(bytes);
    let mut cut = text.len().min(MAX_RAW_BODY_BYTES);
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    Some(text[..cut].to_string())
}

/// 转发前就失败（body 非法 / 缺字段 / 模型不在目录）时的记账。
///
/// 这些请求**一次都没往上游发**，所以 attempts 记 0 会被存储层夹成 1
/// （契约是「含首次、恒 ≥1」）。用 1 表示「至少被处理过一次」，
/// 这与「上游被打了 N 次」在报表里是两回事，报表侧靠 status 与 error 区分。
///
/// 为什么这几条也要记：`model_not_found` 是客户端配置错误最常见的形态，
/// 不记的话用户在报表里看不到「请求全在失败」，只会以为统计漏了。
pub fn record_early_failure(
    state: &ServerState,
    started_at: i64,
    model: &str,
    client_reasoning: &str,
    error: &GatewayError,
) {
    let context = RecordContext {
        stats: state.request_stats(),
        telemetry: Arc::new(RequestTelemetry::new()),
        started_at,
        client_model: model.to_string(),
        client_reasoning: client_reasoning.to_string(),
        model: model.to_string(),
        // 与 `payload_response` 同一口径：非法状态码会被归一成 500
        status: i64::from(error.http_status().as_u16()),
        // 转发前就失败：没有 id（telemetry 是空的）、也没有任何原始报文 ——
        // 请求侧的正文可能解析都没解析成功，存半截没有意义
        raw_request: None,
        raw_response: None,
    };
    record_entry(&context, Some(error.message.clone()));
}

/// 在途回写的接线：把 telemetry 的每一次状态变化写进那条「进行中」行。
///
/// ── 它解决什么 ──────────────────────────────────────────────
/// `record_started` 插的进行中行只有 id / ts / 模型名 —— 选路与发送体定稿都发生
/// 在它之后，所以整段转发期间列表里那一行看不出「谁在承载、转发的是哪个模型、
/// 已经试了几轮」。而这几样在请求真正发出去之前就已经确定，只是此前没有回写的
/// 落点（`requests` 表上原本只有收尾与僵尸行清理两条写路径）。
///
/// ── 为什么是闭包而不是让 telemetry 直接持有 `Arc<RequestStats>` ──
/// `core::upstream` 不认识 `request_stats`（依赖方向见 `core/mod.rs` 的约定），
/// 而「快照 → 落库形态」的转换本来就在本模块（`record_entry` 收尾时做的是同一件
/// 事，两份转换共用下面两个 `stored_*` 函数）。接线在这里，core 只负责在状态
/// 变化时调一下。
///
/// `started_at` 由调用方带进来：首响在 telemetry 里存的是**绝对时刻**，而明细里
/// 那一列是相对请求开始的毫秒数（口径见 `RequestEntry::first_response_ms`）——
/// 减法只能在这里做，telemetry 不知道请求什么时候开始的。
pub fn live_row_sink(
    stats: Arc<RequestStats>,
    id: String,
    started_at: i64,
) -> impl Fn(&TelemetrySnapshot) + Send + Sync {
    move |snapshot| {
        stats.update_running(
            &id,
            &RunningProgress {
                provider: snapshot.provider.clone().unwrap_or_default(),
                account_id: snapshot.account_id.clone(),
                account_name: snapshot.account_name.clone(),
                upstream_model: snapshot.upstream_model.clone(),
                upstream_reasoning: snapshot.upstream_reasoning.clone(),
                // 与收尾同口径：一次都没发出去（0）按 1 次算，理由见
                // `TelemetrySnapshot::attempts` 的说明
                attempts: snapshot.attempts.max(1),
                first_response_ms: snapshot.first_response_at.map(|at| (at - started_at).max(0)),
                attempt_details: stored_attempt_details(&snapshot.attempts_detail),
                sensitive_hits: stored_sensitive_hits(&snapshot.sensitive_hits),
            },
        );
    }
}

/// telemetry 的尝试明细 → 落库形态。
///
/// 收尾记账（`record_entry`）与在途回写共用这一份：两处各写一遍转换，迟早会因为
/// 「只改了一处」让进行中行与收尾行显示成两种样子。
fn stored_attempt_details(list: &[usage::AttemptDetail]) -> Vec<AttemptDetail> {
    list.iter()
        .map(|item| AttemptDetail {
            provider: item.provider.clone(),
            account: item.account.clone(),
            status: item.status,
            error: item.error.clone(),
            retries: item
                .retries
                .iter()
                .map(|retry| RetryEvent {
                    reason: retry.reason.clone(),
                    status: retry.status,
                    delay_ms: retry.delay_ms,
                })
                .collect(),
            notice: item.notice.clone(),
        })
        .collect()
}

/// telemetry 的脱敏命中 → 落库形态（与 [`stored_attempt_details`] 同一理由）
fn stored_sensitive_hits(list: &[usage::SensitiveHit]) -> Vec<SensitiveHit> {
    list.iter()
        .map(|hit| SensitiveHit {
            word: hit.word.clone(),
            count: hit.count,
        })
        .collect()
}

/// 记一条请求日志。
///
/// ── 记账为什么绝不能影响请求 ─────────────────────────────────
/// `RequestStats::record` 本身不返回 `Result`：内部对写盘失败只打
/// `[Stats] 统计写入失败`，锁中毒走 `poisoned.into_inner()` 继续用，
/// 序列化失败跳过该条 —— 存储层已把「统计失败」全部收敛成「少记一条」，
/// 没有任何 unwind 路径，所以这里不需要 `catch_unwind`。
///
/// ── 字段口径 ────────────────────────────────────────────────
///   ts          请求**开始**时刻（不是记账时刻）：趋势图要按「用户什么时候
///               发的请求」归日
///   durationMs  收尾 - 开始（含排队、选路、上游等待、流下发）
///   firstResponseMs  首帧到达 - 开始；全程没有帧到达时为 null
///   attempts    旁路槽里累计的上游请求数；一次都没发出去时回落 1
///   error       旁路槽里的原因优先（更接近根因），否则用调用方给的兜底文案
pub fn record_entry(context: &RecordContext, fallback_error: Option<String>) {
    let snapshot = context.telemetry.snapshot();
    // 收尾时刻只取一次：明细里的 durationMs 与日志里打的那一个是同一个值
    let finished_at = logging::now_ms();
    let duration_ms = finished_at - context.started_at;
    let first_response_ms = snapshot
        .first_response_at
        .map(|at| (at - context.started_at).max(0));
    let error = snapshot
        .error
        .or(fallback_error)
        .map(|text| truncate_chars(&text, ERROR_SUMMARY_CHARS));
    let attempts = snapshot.attempts.max(1);
    let mut entry = NewRequestEntry::new(context.model.clone(), context.status);
    entry.ts = Some(context.started_at);
    // 关联 id：调试模式的原始报文按它取（见 `core::debug_traffic`）。
    // 空串 = 该请求没生成 id（转发前就失败的路径），前端不显示详情入口。
    // clone 而不是 move：snapshot.id 下面给 store_raw 复用（同一关联键）
    entry.id = snapshot.id.clone();
    entry.duration_ms = duration_ms;
    entry.first_response_ms = first_response_ms;
    entry.attempts = attempts;
    // 存储契约里这两个字段是 String（不是 Option），空串就是「没有账号」的表示
    entry.account_id = snapshot.account_id;
    entry.account_name = snapshot.account_name;
    entry.provider = snapshot.provider;
    // 下游名 / 上游名分记：下游名是客户端原值（context 带入），上游名是
    // 实际发出的名字（telemetry 采集，空串 = 一次都没发出去）。两者都空时
    // 前端回落显示 model 一行 —— 旧数据没有这两个键，同一口径。
    entry.client_model = context.client_model.clone();
    entry.upstream_model = snapshot.upstream_model;
    // 等级双端透传（与上面两行同源）：下游等级来自入口采集（context），
    // 上游等级来自发送体定稿处的采集（telemetry 快照）
    entry.client_reasoning = context.client_reasoning.clone();
    entry.upstream_reasoning = snapshot.upstream_reasoning;
    // 两个明细字段（本次改造）：都是「有采集才有值」的旁路数据，采集点在
    // 转发链路上（`core::upstream::usage` 槽），这里只负责搬运。
    //   · attemptDetails：每次上游尝试的（provider / 账号 / 状态码 / 错误摘要
    //     / 本轮内部的退避重试 / 提示），请求日志「重试」列的弹层显示它；
    //     转发前就失败的请求没有它（空表）。
    //   · sensitiveHits：本次命中的敏感词与次数，同一列的紫色标签显示它。
    // 存储层的两个类型与采集侧**各自定义、当前同形**（见 `RequestEntry` 的
    // 注释），所以这里逐字段转一次 —— 不做 `From` impl 是为了让两处结构能
    // 各自演化（把「转发期形态」与「落库形态」绑成一个类型，将来改一处
    // 就得同时改另一处，而它们的变化理由本来不同）。
    // 转换走 `stored_*` 两个函数：在途回写（`live_row_sink`）用的是同一份，
    // 两处各写一遍迟早会让进行中行与收尾行显示成两种样子。
    entry.attempt_details = stored_attempt_details(&snapshot.attempts_detail);
    entry.sensitive_hits = stored_sensitive_hits(&snapshot.sensitive_hits);
    entry.error = error;
    entry.prompt_tokens = snapshot.prompt_tokens;
    entry.completion_tokens = snapshot.completion_tokens;
    entry.total_tokens = snapshot.total_tokens;
    entry.cache_read_tokens = snapshot.cache_read_tokens;
    context.stats.record(entry);
    // 原始正文落库（request_raw 表）：与明细同 id、同开始时刻。独立于 record
    // 的一次写入（大字段不进记账热路径，理由见 `RequestStats::store_raw`）；
    // id 为空 / 两侧全空在 store_raw 内部拦下，失败只打控制台 —— 正文是
    // 排障辅助，丢一侧不能影响已经收尾的请求
    context.stats.store_raw(
        &snapshot.id,
        context.started_at,
        context.raw_request.as_deref(),
        context.raw_response.as_deref(),
    );
    logging::verbose(
        "[Stats]",
        &format!(
            "记一条请求: model={} status={} {duration_ms}ms 首响={} attempts={attempts} tokens={}+{}（缓存 {}）",
            if context.model.is_empty() { "(未指定)" } else { &context.model },
            context.status,
            first_response_ms
                .map(|value| format!("{value}ms"))
                .unwrap_or_else(|| "-".to_string()),
            snapshot.prompt_tokens,
            snapshot.completion_tokens,
            snapshot.cache_read_tokens,
        ),
    );
}

/// 按字符截断（超出部分用 `…` 收尾）。
pub fn truncate_chars(text: &str, limit: usize) -> String {
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
/// 客户端中途断开、上游断流这两种「非正常收尾」也能各记一条。
///
/// ── 透传是否受影响 ──────────────────────────────────────────
/// 不影响：本类型对每个 `Item` 原样转发（只补一个「是不是到 None 了」的
/// 观察），不读、不改、不缓存字节，也不吞错误。
///
/// ── 为什么还要认「收尾帧」（`terminals`）─────────────────────
/// 上面那条「跑到 `None` 才算成功」的判据，对**按帧收尾**的客户端是不成立的：
/// 它们在收到本协议的收尾帧时就已知这一轮结束，**当场关连接**，从不读到 EOF。
/// 于是 `Drop` 里那支「没跑到 None」的分支会被触发 —— 一次**成功**的请求被记成
/// 「响应流未完整下发」，并因「失败清零 token」的归一（`NewRequestEntry::normalize`）
/// 把真实用量一并抹掉。这不是偶发竞态：只要客户端这么收尾，它就**稳定**复现。
///
/// 实测到的两类客户端都这样：
///   - Codex Desktop（`/v1/responses`）：收到 `response.completed` 即断开；
///   - 任何读到 `data: [DONE]` 就停的 Chat 客户端（含 Node 的
///     `reader.cancel()` 写法）：`/v1/chat/completions` 同样中招。
///
/// 所以判据补一条：**已经下发过本协议的收尾帧** ⇒ 这一轮在协议层已经完整，
/// 之后客户端怎么关都算正常收尾。收尾帧表由调用方给（各入口知道自己发什么），
/// 本类型只做字节比对。
///
/// 注意这不改变「谁先到算谁」的语义：真中断（客户端在收尾帧之前就跑掉）仍然
/// 按 `STREAM_ABORTED` 记，`error` 也仍然由各转发层写进 telemetry。
struct TerminalScan {
    /// 待匹配的收尾帧字节特征
    frames: TerminalFrames,
    /// 跨 chunk 的匹配窗口：收尾帧可能被 TCP 分片切开
    ///
    /// 窗口只留「最长特征 - 1」个字节 —— 够接上被切开的那一段，又不会
    /// 随着整段响应无界增长（一次 SSE 响应动辄数百 KB）。
    window: Vec<u8>,
    /// 已经命中过（幂等标记，命中后不再扫）
    seen: bool,
}

impl TerminalScan {
    fn new(frames: TerminalFrames) -> Self {
        Self { frames, window: Vec::new(), seen: false }
    }

    /// 吃一段刚下发的字节，返回「本次是否首次命中收尾帧」。
    fn push(&mut self, chunk: &[u8]) -> bool {
        if self.seen || self.frames.is_empty() {
            return false;
        }
        // 窗口 = 上次残留 + 本段：收尾帧被 TCP 分片切开时，靠这段残留把两半接上
        self.window.extend_from_slice(chunk);
        let hit = self
            .frames
            .iter()
            .any(|frame| contains(&self.window, frame));
        // 只留「最长特征 - 1」个字节：比这更长的残留接不上任何一帧的开头，
        // 留着只会让窗口随响应体积无界增长（一次 SSE 响应动辄数百 KB）
        let longest = self.frames.iter().map(|frame| frame.len()).max().unwrap_or(0);
        let keep = longest.saturating_sub(1);
        if self.window.len() > keep {
            let drop = self.window.len() - keep;
            self.window.drain(..drop);
        }
        if hit {
            self.seen = true;
        }
        hit
    }
}

/// `haystack` 里是否包含 `needle`（空 needle 恒假）。
///
/// 逐字节朴素匹配：特征串最长也就 30 来个字节、窗口不超过它的两倍，
/// 单段响应里扫这点字节的开销可以忽略（对比之下，为它引一个字符串搜索
/// 依赖或写 KMP 都是过度设计）。
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack.len() >= needle.len()
        && haystack.windows(needle.len()).any(|window| window == needle)
}

pub struct RecordingStream {
    inner: Box<dyn futures::Stream<Item = Result<Bytes, std::io::Error>> + Send + Unpin>,
    /// 收尾上下文；`take()` 走即表示「已记账」（保证恰好记一条）
    context: Option<RecordContext>,
    /// 收尾帧扫描器（`None` = 该入口没提供特征，等价于改前的行为）
    terminal: Option<TerminalScan>,
    /// 响应正文累积缓冲（`request_raw` 表的响应侧采集，见 [`RawCapture`]）
    raw: RawCapture,
}

/// 响应正文的累积缓冲。
///
/// ── 为什么在透传流上再攒一份 ─────────────────────────────────
/// 流式请求的响应正文是逐帧下发的，`RecordingStream` 是唯一能看到**全部**
/// 下发字节的地方（协议转换层在它里面，见 `transformed_stream` 的顺序说明）。
/// 预览对话要的就是「客户端实际看到的响应」，在这里抄一份最准确，也
/// 不用各条转发路径再各接一个钩子。
///
/// ── 内存有上界 ──────────────────────────────────────────────
/// 攒到 [`MAX_RAW_BODY_BYTES`] 就停（后续字节只透传不缓存）：一次 SSE 响应
/// 动辄数百 KB，不设上限等于把整条响应复制进内存 —— 请求的透传不受任何影响，
/// 但网关不该为一个排障功能长期多占一份响应大小的内存。截断即终态：
/// `raw_body` 读取时按「长度达到上限」给出 truncated 提示。
struct RawCapture {
    buf: Vec<u8>,
}

impl RawCapture {
    fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// 抄一段刚下发的字节（达到上限后丢弃，不报错 —— 采集永远不影响透传）
    fn push(&mut self, chunk: &[u8]) {
        let remaining = MAX_RAW_BODY_BYTES.saturating_sub(self.buf.len());
        if remaining == 0 {
            return;
        }
        let take = chunk.len().min(remaining);
        self.buf.extend_from_slice(&chunk[..take]);
    }

    /// 定稿：一个字节都没抄到给 None（不写行）；UTF-8 边界与截断由
    /// [`raw_body_text`] 统一处理。取 `&mut self`：settle 之后本缓冲不再使用，
    /// 清空与否无所谓，但 RecordingStream 只拿得到 `&mut self`
    fn into_text(&mut self) -> Option<String> {
        raw_body_text(&self.buf)
    }
}

impl RecordingStream {
    /// 带收尾帧识别的构造（见 [`RecordingStream`] 的说明）。
    ///
    /// `frames` 由调用方按自己那条协议给出（`terminal::CHAT` 等）——
    /// 三条入口**都必须**传自己那份：漏传等于退回改前的行为（客户端按帧收尾时
    /// 成功请求被记成中断），所以这里不再提供「不带特征」的构造入口。
    pub fn with_terminals(
        inner: Box<dyn futures::Stream<Item = Result<Bytes, std::io::Error>> + Send + Unpin>,
        context: RecordContext,
        frames: TerminalFrames,
    ) -> Self {
        let terminal = if frames.is_empty() { None } else { Some(TerminalScan::new(frames)) };
        Self { inner, context: Some(context), terminal, raw: RawCapture::new() }
    }

    /// 记账并清空上下文（幂等：第二次调用什么都不做）。
    ///
    /// 响应正文在这里定稿：流的收尾点（跑到 None / 被丢弃）正是「正文完整了」
    /// 或「正文就这么多」的时刻，此后不再有字节。
    fn settle(&mut self, fallback_error: Option<String>) {
        if let Some(mut context) = self.context.take() {
            // 流式路径构造时 raw_response 恒为 None，这里用缓冲定稿直接覆盖；
            // 若未来有入口想预填响应正文，那它不该再走流式包装（自相矛盾的约定）
            context.raw_response = self.raw.into_text();
            record_entry(&context, fallback_error);
            // 手动终止的令牌在此注销：流的结束点 = 请求不再在途（与
            // DisconnectGuard::complete 同一条生命周期；settle 由 Drop 兜底
            // 调用，所以「客户端断开」「流跑完」「服务退出」三条路都会走到这里）
            crate::server::core::upstream::cancellation::unregister(&context.telemetry.id());
        }
    }

    /// 本协议收尾帧是否已下发（`true` = 这一轮在协议层已经完整）
    fn terminal_seen(&self) -> bool {
        self.terminal.as_ref().is_some_and(|scan| scan.seen)
    }
}

impl futures::Stream for RecordingStream {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use futures::StreamExt;
        // 本类型的字段全部是 Unpin，自身也就 Unpin，get_mut 是安全的
        let this = self.get_mut();
        // 已记账（说明上游流已结束）→ 不再去 poll 上游，直接报结束
        if this.context.is_none() {
            return std::task::Poll::Ready(None);
        }
        let polled = this.inner.poll_next_unpin(cx);
        match &polled {
            std::task::Poll::Ready(Some(Ok(bytes))) => {
                // 首响采集：**下发的第一个字节**到达时记一次（挂在透传流上而不是
                // 上游字节流上，于是各条转发路径在此收敛为同一处）
                if let Some(context) = &this.context {
                    context.telemetry.note_first_frame();
                }
                // 响应正文采集：抄进缓冲（上限后丢弃），透传字节原样不动
                this.raw.push(bytes);
                // 收尾帧识别：命中即说明客户端接下来随时可能断开，那属于正常收尾
                if let Some(scan) = &mut this.terminal {
                    if scan.push(bytes) {
                        logging::verbose(
                            "[Stats]",
                            "已下发协议收尾帧：此后客户端断开按正常收尾记账",
                        );
                    }
                }
            }
            std::task::Poll::Ready(Some(Err(_))) => {}
            std::task::Poll::Ready(None) => {}
            std::task::Poll::Pending => {}
        }
        if matches!(polled, std::task::Poll::Ready(None)) {
            this.settle(None);
        }
        polled
    }
}

impl Drop for RecordingStream {
    fn drop(&mut self) {
        // 走到这里说明响应流**没有**跑到 None 就被丢弃了。分两种：
        //   · 本协议的收尾帧已经下发过 → 客户端按协议收尾（正常），不写摘要；
        //   · 否则是真的提前丢弃（客户端中途断开 / 服务退出）→ 记一条中断摘要。
        // 两者都仍记一条 —— 请求确实发生了，控制流已经走到 settle 之外。
        if self.terminal_seen() {
            self.settle(None);
        } else {
            self.settle(Some(STREAM_ABORTED.to_string()));
        }
    }
}

/// 把一条响应流包进「协议转换 + 记账」两层的通用装配。
///
/// ── 为什么顺序是「先转换、后记账」────────────────────────────
/// `RecordingStream` 的职责是记账与首响采集，它必须看到**最终下发**的字节
/// （客户端视角的完成点）。协议转换层（Responses / Anthropic 的流式状态机）
/// 在它**里面**，于是：
///   转换器 → RecordingStream → axum Body
/// 首响时刻因此是「转换后的第一帧到达客户端」的时刻，与客户端感知一致。
///
/// `transform` 是一个「吃字节吐字节」的闭包：转换状态机自己持有，
/// 本函数不认识任何协议。
///
/// `terminals` 是**本协议收尾帧的字节特征**，由调用方按自己那条入口给出
/// （见 [`RecordingStream`] 的说明）—— 本函数同样只当字节比对，不做协议判断。
///
/// ── 为什么收 `Box<dyn Stream>` 而不是泛型 `S` ────────────────
/// 泛型要额外要求 `S: Unpin`（`flat_map` 的组合流不自动 Unpin），
/// 而调用点手里本来就是 `ForwardOutcome::Stream` 给的装箱流 —— 直接收装箱
/// 少一层泛型参数，也不必在调用点写 `Unpin` 约束。
pub fn transformed_stream(
    source: Box<dyn futures::Stream<Item = Result<Bytes, std::io::Error>> + Send + Unpin>,
    context: RecordContext,
    terminals: TerminalFrames,
    mut transform: impl FnMut(&[u8]) -> Vec<Bytes> + Send + 'static,
) -> Box<dyn futures::Stream<Item = Result<Bytes, std::io::Error>> + Send + Unpin> {
    use futures::StreamExt;
    // 上游字节流 → 转换后的帧流
    let converted = source.flat_map(move |item| {
        let frames = match item {
            Ok(chunk) => transform(&chunk),
            // 上游断流：把错误原样透出（由 axum 结束连接）
            Err(error) => return futures::stream::iter(vec![Err(error)]).boxed(),
        };
        futures::stream::iter(frames.into_iter().map(Ok)).boxed()
    });
    Box::new(RecordingStream::with_terminals(Box::new(converted), context, terminals))
}

// ─── 响应构造 ───────────────────────────────────────────────

/// 非流式响应：`Content-Type: application/json; charset=utf-8` + 200。
///
/// 状态码固定 200：上游非 2xx 时转发层已经抛出（不会走到这里），
/// 与 Node 的 `res.writeHead(200, ...)` 一致。charset 显式写上 ——
/// 响应体里有中文（错误文案/思考内容）。
pub fn json_response(body: Value) -> Response {
    let text = match serde_json::to_string(&body) {
        Ok(text) => text,
        Err(error) => {
            // 序列化失败只可能是内部数据坏了：给一个 500，绝不 panic
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

/// 流式响应：`Content-Type: text/event-stream; charset=utf-8` +
/// `Cache-Control: no-cache` + `Connection: keep-alive`，状态码透传。
///
/// 收 `StatusCode` 而不是 `u16`：调用点要先做一次「非法码落 200」的归一
/// （记账要用同一个值），归一放在调用点、这里只负责下发。
pub fn sse_response(
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

/// 非 2xx 的转发错误 → 按协议形态下发的错误响应。
///
/// 三条入口的错误体形态不同（OpenAI / Responses / Anthropic），所以形态由
/// 调用方给的闭包决定；本函数只负责把「状态码归一 + 记账」这两件共同的事做掉。
///
/// `convert` 收 `(status, error)`，返回该协议的错误响应体。
pub fn error_response(
    context: RecordContext,
    error: &GatewayError,
    convert: impl FnOnce(u16, &GatewayError) -> Response,
) -> Response {
    let status = error.http_status().as_u16();
    // 记账里的状态码必须与客户端看到的那个一致
    let recorded = RecordContext { status: i64::from(status), ..context };
    record_entry(&recorded, Some(error.message.clone()));
    convert(status, error)
}

// ─── 调试落盘 ───────────────────────────────────────────────

/// 调试落盘：原始 body + 一行 meta（覆盖写；**失败不影响请求**）。
///
/// 与 Node 版一致：目录不存在时创建，任何 IO 失败都静默吞掉 ——
/// 调试落盘是排障辅助，不能因为它失败就让用户的请求失败。
pub fn write_debug_files(body: &[u8], method: &str, path: &str, user_agent: &str) {
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

/// ISO-8601 UTC 时间戳（对应 Node 的 `new Date().toISOString()`）
pub fn iso_timestamp() -> String {
    let millis = logging::now_ms();
    match chrono::DateTime::from_timestamp_millis(millis) {
        Some(utc) => utc.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
        None => String::new(),
    }
}

/// 请求体 sha256（十六进制小写）——去重键，对应 Node 的
/// `createHash('sha256').update(rawBody).digest('hex')`
pub fn sha256_hex(body: &[u8]) -> String {
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
