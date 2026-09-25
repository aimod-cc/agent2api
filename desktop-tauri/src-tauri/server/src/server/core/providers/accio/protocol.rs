//! Accio 协议转换：OpenAI Chat Completions ↔ 阿里 ADK 的 **Gemini 风格**信封。
//!
//! ── 两边长什么样（逆向来源：客户端 `@ali/accio-adk-ts` 与 `gateway-worker`）──
//! 下游（本项目内部统一形态，见 `core::protocol` 的模块头）：
//! ```jsonc
//! { "model": "…", "messages": [{role, content, tool_calls, tool_call_id}],
//!   "tools": [{"type":"function","function":{name, description, parameters}}],
//!   "temperature": 0.7, "max_tokens": 1024, "stream": true }
//! ```
//! 上游（`POST {llmBase}/generateContent`，body 是 protobuf-JSON 的
//! snake_case 形态，键名转换由客户端的 `camelCase → snake_case` 映射产生）：
//! ```jsonc
//! {
//!   "model": "gemini-3-flash-preview",
//!   "tenant": "accio-agent", "iai_tag": "phoenix-desktop", "empid": "",
//!   "request_id": "…", "token": "<accessToken>",
//!   "contents": [ {"role":"user","parts":[{"text":"hi"}]},
//!                 {"role":"model","parts":[{"function_call":{"id":"…","name":"f","args_json":"{}"}}]},
//!                 {"role":"tool","parts":[{"function_response":{"id":"…","name":"f","response_json":"{\"content\":\"…\",\"is_error\":false}"}}]} ],
//!   "system_instruction": "系统提示（字符串，不是对象）",
//!   "tools": [{"name":"f","description":"…","parameters_json":"{…}"}],
//!   "temperature": 0.7, "max_output_tokens": 1024,
//!   "tool_config": "{\"functionCallingConfig\":{\"streamFunctionCallArguments\":true}}",
//!   "include_thoughts": true, "reasoning_effort": "high",
//!   "properties": {"normalized_response": "true", "reasoning_effort": "high"}
//! }
//! ```
//!
//! ── 四个必须照抄的点（都是上游的实际约束，不是这里的取舍）────────
//!   1. **`tool_config` 是 JSON 字符串**（客户端对非 gemini 模型才加，
//!      内容是 `streamFunctionCallArguments: true`）——工具参数才能流式吐出来。
//!   2. **`function_call.args_json` / `function_response.response_json` 也是
//!      字符串**（里面才是 JSON 本体）。写成本体对象上游认不出参数。
//!   3. **`system_instruction` 是字符串**，不是 `{parts:[…]}` 那种 Content。
//!   4. **思考档位走 `properties.reasoning_effort`**（map<string,string>，
//!      值必须是字符串）；对 `gpt-*` / `o1-*` 这类 OpenAI 系模型，顶层的
//!      `reasoning_effort` 要**留空**（客户端的 `bo` 就是这么分的）。
//!
//! ── 媒体（图片）──────────────────────────────────────────────
//! 客户端用 `inline_data{mime_type,data}`（base64）或 `file_data{mime_type,file_uri}`
//! 两种 part。本网关把 OpenAI 的 `image_url` 映射过去：`data:` 开头的 data URI
//! 走 inline_data，其余远程 URL 走 file_data —— 上游能不能取到那个 URL 是它的事，
//! 至少形态是对的（不支持的模型会自己报错，比我们静默丢图要好）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：零 unwrap/expect/panic。

use serde_json::{json, Map, Value};

/// 上游的 `finish_reason` → OpenAI 的 `finish_reason`。
///
/// 客户端的归一表是：stop/end_turn/tool_calls/tool_use → STOP、
/// length/max_tokens → MAX_TOKENS、content_filter/safety → SAFETY。
/// 我们把它翻回 OpenAI 的四值域（下游说的是 Chat 协议）。
pub fn finish_reason(raw: &str) -> &'static str {
    match raw.to_ascii_lowercase().as_str() {
        "" | "stop" | "end_turn" | "complete" => "stop",
        "max_tokens" | "length" => "length",
        "tool_calls" | "tool_use" | "function_call" => "tool_calls",
        "content_filter" | "safety" | "blocked" => "content_filter",
        _ => "stop",
    }
}

/// 中文/英文错误文案里能认出的额度不足信号（客户端用同一类正则判「余额耗尽」）
const QUOTA_HINTS: &[&str] = &[
    "积分",
    "余额",
    "额度",
    "quota",
    "insufficient",
    "credit",
    "exceeded",
    "limit reached",
    "rate limit",
    "too many requests",
];

/// 鉴权类信号
const AUTH_HINTS: &[&str] = &["unauthorized", "not logged in", "invalid token", "token expired", "invalid_token"];

/// 上游错误的粗分类（转发层据此决定换账号 / 刷新 / 透传）
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UpstreamKind {
    /// 额度用尽 / 限流 → 标账号冷却 + 换号
    Quota,
    /// 凭证失效 → 刷新后同账号重试一次
    Auth,
    /// 其余（内容拦截由 `content_block` 再判）
    Other,
}

/// 按 HTTP 状态码 + 文案分类。
///
/// ── 为什么文案也要看 ────────────────────────────────────────
/// 上游把额度不足放在 HTTP 400/200 的 body 里（客户端自己的判断就是匹配
/// 「积分/余额…不足/耗尽」这类文案），只看状态码会把「余额耗尽」当成一次
/// 普通的 400 直接透传给客户端 —— 账号页不会进冷却，下一个请求还会选它。
pub fn classify_upstream_error(status: u16, message: &str) -> UpstreamKind {
    let lower = message.to_ascii_lowercase();
    if status == 401 || status == 403 {
        return UpstreamKind::Auth;
    }
    if status == 429 {
        return UpstreamKind::Quota;
    }
    if AUTH_HINTS.iter().any(|hint| lower.contains(hint)) {
        return UpstreamKind::Auth;
    }
    if QUOTA_HINTS.iter().any(|hint| lower.contains(hint)) {
        return UpstreamKind::Quota;
    }
    UpstreamKind::Other
}

/// 思考档位 → 上游取值。
///
/// ── 为什么按模型声明的档位收敛 ───────────────────────────────
/// 上游目录每个模型自己声明 `reasoningEfforts`（Gemini 系是 low/high，
/// Claude 系是 low/medium/high/max）。发一个它没声明的档位有两种可能：
/// 被忽略（用户以为生效了，其实没有）或 400。因此按**就近取值**收敛到
/// 声明集合里最接近的一档；集合为空（模型不支持思考）时返回 None，
/// 调用方据此连 `include_thoughts` 一起不发。
pub fn resolve_effort(level: &str, efforts: &[String]) -> Option<String> {
    if efforts.is_empty() {
        return None;
    }
    let wanted = crate::server::core::model_rules::reasoning_rank(level)?;
    let mut best: Option<(usize, &String)> = None;
    for effort in efforts {
        let Some(rank) = crate::server::core::model_rules::reasoning_rank(effort) else {
            continue;
        };
        let distance = rank.abs_diff(wanted);
        if best.is_none_or(|(best_distance, _)| distance < best_distance) {
            best = Some((distance, effort));
        }
    }
    best.map(|(_, effort)| effort.clone())
        // 声明了档位但没有一个落在通用 6 档表里（上游加了新词）→ 用第一个，
        // 至少是它自己声明过的取值
        .or_else(|| efforts.first().cloned())
}

/// 一个 OpenAI content（字符串或分块数组）→ ADK parts。
fn content_to_parts(content: &Value, parts: &mut Vec<Value>) {
    match content {
        Value::String(text) => {
            if !text.is_empty() {
                parts.push(json!({ "text": text }));
            }
        }
        Value::Array(items) => {
            for item in items {
                match item.get("type").and_then(Value::as_str) {
                    Some("text") | Some("input_text") | Some("output_text") => {
                        if let Some(text) = item.get("text").and_then(Value::as_str) {
                            if !text.is_empty() {
                                parts.push(json!({ "text": text }));
                            }
                        }
                    }
                    Some("image_url") | Some("input_image") => {
                        // OpenAI 两种形态：`{image_url:{url}}` 与 `{image_url:"…"}`
                        let url = item
                            .get("image_url")
                            .and_then(|value| {
                                value
                                    .get("url")
                                    .and_then(Value::as_str)
                                    .or_else(|| value.as_str())
                            })
                            .or_else(|| item.get("url").and_then(Value::as_str))
                            .unwrap_or("");
                        if let Some(part) = image_part(url) {
                            parts.push(part);
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

/// 图片 URL → ADK part（data URI 走 inline_data，其余走 file_data）
fn image_part(url: &str) -> Option<Value> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }
    if let Some(rest) = url.strip_prefix("data:") {
        let (meta, data) = rest.split_once(',')?;
        let mime = meta.split(';').next().unwrap_or("image/png");
        // 只处理 base64 内联；其它编码（理论上不存在）放弃
        if !meta.contains("base64") {
            return None;
        }
        return Some(json!({ "inline_data": { "mime_type": mime, "data": data } }));
    }
    Some(json!({ "file_data": { "mime_type": "image/png", "file_uri": url } }))
}

/// OpenAI 的 `tool_calls` → ADK 的 function_call parts
fn assistant_tool_calls_parts(tool_calls: &Value) -> Vec<Value> {
    let Some(items) = tool_calls.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .map(|call| {
            let id = call.get("id").and_then(Value::as_str).unwrap_or("").to_string();
            let function = call.get("function").cloned().unwrap_or(Value::Null);
            let name = function.get("name").and_then(Value::as_str).unwrap_or("").to_string();
            let args = function.get("arguments").and_then(Value::as_str).unwrap_or("{}");
            json!({ "function_call": {
                "id": id,
                "name": name,
                "args_json": args,
            }})
        })
        .collect()
}

/// 工具结果（role = tool）→ ADK 的 function_response part
fn tool_result_part(message: &Value) -> Value {
    let id = message.get("tool_call_id").and_then(Value::as_str).unwrap_or("").to_string();
    let name = message.get("name").and_then(Value::as_str).unwrap_or("").to_string();
    let content = match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    };
    let is_error = message
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // 上游认的是字符串包着的 JSON（见模块头要点 2）
    let response = json!({ "content": content, "is_error": is_error }).to_string();
    json!({ "function_response": {
        "id": id,
        "name": name,
        "response_json": response,
    }})
}

/// 一次请求转换的产出
pub struct OutboundBody {
    /// 已归一的上游请求体（键名 snake_case）
    pub body: Value,
    /// 需要下发的思考内容？（决定客户端帧里要不要带 reasoning_content）
    pub thinking: bool,
}

/// OpenAI Chat 请求体 → ADK `generateContent` 请求体。
///
/// `model_code` 是**已按家解析过的发送名**（`models::resolve` 的 `upstreamKey`），
/// `token` 是 accessToken（上游靠 body 里的这个字段鉴权）。
/// `placement` 是思考档位的落点（由目录条目的 `protocol` 决定，见
/// `models::EffortPlacement`）—— 不在这里按模型名猜。
pub fn build_upstream_body(
    body: &Value,
    model_code: &str,
    token: &str,
    effort: Option<&str>,
    placement: super::models::EffortPlacement,
) -> OutboundBody {
    let mut contents: Vec<Value> = Vec::new();
    let mut system_parts: Vec<String> = Vec::new();
    let mut pending_parts: Vec<Value> = Vec::new();
    let messages = body.get("messages").and_then(Value::as_array).cloned().unwrap_or_default();

    // ── 消息归一 ────────────────────────────────────────────────
    // OpenAI 允许同一角色连续出现（工具轮次尤其常见），ADK 侧按「相邻同角色合并」
    // 处理：上游对 contents 的角色交替没有硬要求，但合并能少一次无意义的往返。
    let push_content = |role: &str, parts: Vec<Value>, contents: &mut Vec<Value>| {
        if parts.is_empty() {
            return;
        }
        if let Some(last) = contents.last_mut() {
            let same_role = last.get("role").and_then(Value::as_str) == Some(role);
            if same_role {
                if let Some(existing) = last.get_mut("parts").and_then(Value::as_array_mut) {
                    existing.extend(parts);
                    return;
                }
            }
        }
        contents.push(json!({ "role": role, "parts": parts }));
    };

    for message in &messages {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("user");
        match role {
            "system" | "developer" => {
                let mut parts = Vec::new();
                content_to_parts(message.get("content").unwrap_or(&Value::Null), &mut parts);
                for part in parts {
                    if let Some(text) = part.get("text").and_then(Value::as_str) {
                        system_parts.push(text.to_string());
                    }
                }
            }
            "tool" | "function" => {
                pending_parts.push(tool_result_part(message));
                // 工具结果与下一条用户消息不合并（上游按 tool 角色识别结果）
                push_content("tool", std::mem::take(&mut pending_parts), &mut contents);
            }
            "assistant" => {
                let mut parts: Vec<Value> = Vec::new();
                content_to_parts(message.get("content").unwrap_or(&Value::Null), &mut parts);
                parts.extend(assistant_tool_calls_parts(
                    message.get("tool_calls").unwrap_or(&Value::Null),
                ));
                push_content("model", parts, &mut contents);
            }
            _ => {
                let mut parts = Vec::new();
                content_to_parts(message.get("content").unwrap_or(&Value::Null), &mut parts);
                push_content("user", parts, &mut contents);
            }
        }
    }

    let system_instruction = system_parts.join("\n\n");

    // ── 工具声明 ────────────────────────────────────────────────
    let tools: Vec<Value> = body
        .get("tools")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|tool| {
                    let function = tool.get("function").unwrap_or(tool);
                    let name = function.get("name").and_then(Value::as_str)?.trim();
                    if name.is_empty() {
                        return None;
                    }
                    let description = function
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let parameters = function
                        .get("parameters")
                        .cloned()
                        .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
                    Some(json!({
                        "name": name,
                        "description": description,
                        // 上游要字符串包着的 JSON（见模块头要点 2）
                        "parameters_json": parameters.to_string(),
                    }))
                })
                .collect()
        })
        .unwrap_or_default();

    // ── 生成参数与思考档位 ──────────────────────────────────────
    let mut properties: Map<String, Value> = Map::new();
    // 客户端自己的归一化开关（桌面端每个请求都带它）
    properties.insert("normalized_response".to_string(), Value::String("true".to_string()));

    let request_id = crate::server::core::upstream::request::new_request_id();
    let mut payload = Map::new();
    payload.insert("model".to_string(), Value::String(model_code.to_string()));
    payload.insert("tenant".to_string(), Value::String(super::endpoints::DEFAULT_TENANT.to_string()));
    payload.insert("iai_tag".to_string(), Value::String(super::endpoints::DEFAULT_IAI_TAG.to_string()));
    payload.insert("empid".to_string(), Value::String(String::new()));
    payload.insert("request_id".to_string(), Value::String(request_id.clone()));
    // `message_id` 是**必填**（2026-09 实测）：缺了它上游回
    // `{"error_code":"400","error_message":"invalid params"}`（HTTP 200 的帧），
    // 而带一个非空串就正常。形态不限，这里用与 request_id 同源的随机串加前缀。
    payload.insert(
        "message_id".to_string(),
        Value::String(format!("msg-{request_id}")),
    );
    payload.insert("token".to_string(), Value::String(token.to_string()));
    payload.insert("contents".to_string(), Value::Array(contents));
    payload.insert("system_instruction".to_string(), Value::String(system_instruction));
    if !tools.is_empty() {
        payload.insert("tools".to_string(), Value::Array(tools));
        // 工具的流式参数（见模块头要点 1）；非 gemini 才加，这里一律加 ——
        // 本网关能拿到的模型都吃这个字段（客户端也只对 gemini 不加）
        payload.insert(
            "tool_config".to_string(),
            Value::String(
                json!({ "functionCallingConfig": { "streamFunctionCallArguments": true } }).to_string(),
            ),
        );
    }
    if let Some(temperature) = body.get("temperature").and_then(Value::as_f64) {
        payload.insert("temperature".to_string(), json!(temperature));
    }
    let max_tokens = body
        .get("max_tokens")
        .or_else(|| body.get("max_completion_tokens"))
        .and_then(Value::as_u64);
    if let Some(max_tokens) = max_tokens.filter(|value| *value > 0) {
        payload.insert("max_output_tokens".to_string(), json!(max_tokens));
    }
    if let Some(top_p) = body.get("top_p").and_then(Value::as_f64) {
        payload.insert("top_p".to_string(), json!(top_p));
    }
    if let Some(stop) = body.get("stop") {
        let list: Vec<Value> = match stop {
            Value::String(text) => vec![Value::String(text.clone())],
            Value::Array(items) => items.clone(),
            _ => Vec::new(),
        };
        if !list.is_empty() {
            payload.insert("stop_sequences".to_string(), Value::Array(list));
        }
    }

    let mut thinking = false;
    if let Some(effort) = effort.map(str::trim).filter(|value| !value.is_empty()) {
        if !crate::server::core::model_rules::reasoning_is_off(effort) {
            thinking = true;
            payload.insert("include_thoughts".to_string(), Value::Bool(true));
            match placement {
                // `protocol` = responses / openai：只进 properties，顶层留空
                // （桌面端的分法；实测 GPT 系只有这一种能出思考内容）
                super::models::EffortPlacement::Properties => {
                    properties.insert("reasoning_effort".to_string(), Value::String(effort.to_string()));
                }
                // 其余（Claude / Gemini 系）：顶层。放 properties 在 Gemini 系
                // 是硬 400（`Unknown name "reasoning_effort"`）
                super::models::EffortPlacement::Top => {
                    payload.insert("reasoning_effort".to_string(), Value::String(effort.to_string()));
                }
            }
        }
    }
    // `properties` 总是要发（normalized_response 在里面），但要排在最后插入 ——
    // 上面的 reasoning_effort 可能又往里加了一条
    payload.insert("properties".to_string(), Value::Object(properties));

    OutboundBody {
        body: Value::Object(payload),
        thinking,
    }
}

/// `generateContent` 的 URL：`{llmBase}/generateContent?sg_k=<md5(requestId)>`。
///
/// `sg_k` 是桌面端拼的一个防重放标记（`md5(requestId)`，不是签名）——
/// 上游要它，我们就照算。
pub fn generate_content_url(llm_base: &str, request_id: &str) -> String {
    // 项目已在用 `md-5`（import 名 `md5`，见 autoclaw::refresh），
    // 这里直接用同一个 crate，不另引第二个 md5 实现
    use md5::{Digest as _, Md5};
    let mut hasher = Md5::new();
    hasher.update(request_id.as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(32);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    format!("{llm_base}/generateContent?sg_k={hex}")
}

/// 从请求体里读客户端显式指定的思考档位（绑定让位判据用）。
pub fn client_effort(body: &Value) -> Option<String> {
    crate::server::core::model_rules::read_client_level(body)
}
