//! Qoder 协议转换：OpenAI Chat Completions ↔ Qoder 上游（移植来源
//! `Qoder-Proxy/src/protocol.mjs`）。
//!
//! ── 两边长什么样 ────────────────────────────────────────────
//! 下游按 OpenAI 习惯发 `{model, messages, tools, stream, ...}`；上游要的是一个
//! 带会话、记录、业务上下文的**固定信封**（见 `build_upstream_body`）。
//! 本模块负责两边互转，并处理思考标签的拆解。
//!
//! ── 三个容易踩的点（都是上游的实际约束，不是这里的取舍）────────
//!   1. **system 提示必须放进 messages 里**：上游顶层 `system` 字段无效，
//!      源实现在调用处显式做了「把 system 提到消息序列最前」这一步，这里
//!      在 `build_upstream_body` 里统一做（见 `normalize_messages` 的说明）。
//!   2. **`model_config` 必须剥掉 `thinking_config`**：目录条目原样回传会
//!      **覆盖** `parameters.enable_thinking`，导致「关闭思考」失效
//!      （实测：带 thinking_config 时 enable_thinking=false 仍输出数千字思考）。
//!   3. **思考档位不能写死白名单**：各模型支持的档位由上游目录条目里的
//!      `thinking_config.enabled.efforts` 声明（Qwen3.8 系列是
//!      low / medium / xhigh），所以按模型自己声明的来，只做常见别名归一。
//!
//! ── 「关闭思考」为什么降级成「不指定」─────────────────────────
//! 见 `resolve_thinking`：Qwen3.8 系列**无法真正关闭思考** —— 传
//! `enable_thinking=false` 时，上游要么把思考以 `Thinking Process:` 形式混进
//! 正文，要么在需要真推理的问题上直接断开连接。因此对「关闭思考」的请求
//! 降级为「不指定档位」，让上游走它自己的默认行为，而不是发一个会让连接
//! 崩掉的参数。这是上游行为，不是网关可以规避的。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：零 unwrap/expect/panic。

use serde_json::{json, Value};

use super::models::MAX_OUTPUT_TOKENS;

/// 上游要求的会话类型与 agent 标识（源实现 `SESSION_TYPE` / `AGENT_ID`）
const SESSION_TYPE: &str = "qodercli";
const AGENT_ID: &str = "agent_common";
/// 上游的对话任务类型（源实现 `CHAT_TASK`）
const CHAT_TASK: &str = "FREE_INPUT";

/// 思考内容可能包裹在这些标签里（源实现 `THINK_TAGS`）
pub const THINK_TAGS: &[(&str, &str)] = &[
    ("<thinking>", "</thinking>"),
    ("<think>", "</think>"),
    ("<reasoning>", "</reasoning>"),
    ("<thought>", "</thought>"),
];

/// 把任意形态的 content 压平成文本（源实现 `contentToText`）。
///
/// OpenAI 的 content 允许是字符串或「内容块数组」；数组里既可能是
/// `{type:"text", text}`，也可能是 `{type:"image_url", image_url}` 这类
/// 非文本块，都需要跳过而不是报错。
pub fn content_to_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(items) => {
            let mut out = String::new();
            for item in items {
                match item {
                    Value::String(text) => out.push_str(text),
                    Value::Object(object) => {
                        if let Some(text) = object.get("text").and_then(Value::as_str) {
                            out.push_str(text);
                        } else if let Some(text) = object.get("content").and_then(Value::as_str) {
                            out.push_str(text);
                        }
                    }
                    _ => {}
                }
            }
            out
        }
        _ => String::new(),
    }
}

/// 抽出消息里的图片块（OpenAI 的 `image_url` 形态）
fn images_of(content: &Value) -> Vec<Value> {
    content
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter(|item| {
                    item.get("type").and_then(Value::as_str) == Some("image_url")
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// 把 OpenAI 的 messages 规整成上游能吃的形状（源实现 `normalizeMessages`）。
///
/// 处理的差异：
///   - 上一次出错/中断的 assistant 轮次连同其 tool 结果一起丢弃 ——
///     否则会留下没有对应 tool_call 的孤儿结果，上游会拒绝；
///   - assistant 只有工具调用时也要给一个非空 content（否则部分上游会拒）；
///   - 用户消息里的图片保持 `image_url` 块形态。
pub fn normalize_messages(messages: &[Value]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    // 被丢弃的 assistant 轮次里的 tool_call id，用于连带丢弃对应的 tool 结果
    let mut dropped_tool_call_ids: Vec<String> = Vec::new();

    for message in messages {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("");
        if role.is_empty() {
            continue;
        }
        // 上一轮失败/中断的 assistant：连带它的 tool 结果一起丢
        if role == "assistant" {
            let failed = message.get("__failed").map(truthy).unwrap_or(false)
                || message.get("__aborted").map(truthy).unwrap_or(false);
            if failed {
                if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
                    for call in calls {
                        if let Some(id) = call.get("id").and_then(Value::as_str) {
                            dropped_tool_call_ids.push(id.to_string());
                        }
                    }
                }
                continue;
            }
        }
        if role == "tool" {
            let call_id = message.get("tool_call_id").and_then(Value::as_str).unwrap_or("");
            if dropped_tool_call_ids.iter().any(|known| known == call_id) {
                continue;
            }
        }

        match role {
            "user" => {
                let content = message.get("content").cloned().unwrap_or(Value::Null);
                let images = images_of(&content);
                if images.is_empty() {
                    out.push(json!({ "role": "user", "content": content_to_text(&content) }));
                } else {
                    let text = content_to_text(&content);
                    let mut parts: Vec<Value> = Vec::new();
                    if !text.is_empty() {
                        parts.push(json!({ "type": "text", "text": text }));
                    }
                    for image in images {
                        parts.push(json!({
                            "type": "image_url",
                            "image_url": image.get("image_url").cloned().unwrap_or(Value::Null),
                        }));
                    }
                    out.push(json!({ "role": "user", "content": parts }));
                }
            }
            "assistant" => {
                let text = content_to_text(message.get("content").unwrap_or(&Value::Null));
                let tool_calls: Vec<Value> = message
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .map(|calls| {
                        calls
                            .iter()
                            .map(|call| {
                                let arguments = call
                                    .pointer("/function/arguments")
                                    .map(|value| match value {
                                        Value::String(text) => text.clone(),
                                        other => other.to_string(),
                                    })
                                    .unwrap_or_else(|| "{}".to_string());
                                json!({
                                    "id": call.get("id").and_then(Value::as_str).unwrap_or(""),
                                    "type": "function",
                                    "function": {
                                        "name": call
                                            .pointer("/function/name")
                                            .and_then(Value::as_str)
                                            .unwrap_or(""),
                                        "arguments": arguments,
                                    },
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                // 只有工具调用时也要给一个非空 content（上游会拒纯空体）
                let content = if text.is_empty() && !tool_calls.is_empty() {
                    " ".to_string()
                } else {
                    text
                };
                let mut mapped = json!({ "role": "assistant", "content": content });
                if !tool_calls.is_empty() {
                    if let Some(object) = mapped.as_object_mut() {
                        object.insert("tool_calls".to_string(), Value::Array(tool_calls));
                    }
                }
                out.push(mapped);
            }
            "tool" => out.push(json!({
                "role": "tool",
                "tool_call_id": message.get("tool_call_id").cloned().unwrap_or(Value::Null),
                "content": content_to_text(message.get("content").unwrap_or(&Value::Null)),
            })),
            "system" | "developer" => out.push(json!({
                "role": "system",
                "content": content_to_text(message.get("content").unwrap_or(&Value::Null)),
            })),
            _ => {}
        }
    }
    out
}

/// 把 OpenAI 的 tools 转成上游形态（源实现 `normalizeTools`）
pub fn normalize_tools(tools: Option<&Value>) -> Option<Vec<Value>> {
    let items = tools?.as_array()?;
    if items.is_empty() {
        return None;
    }
    let mapped: Vec<Value> = items
        .iter()
        .filter(|item| item.pointer("/function/name").and_then(Value::as_str).is_some())
        .map(|item| {
            let mut function = serde_json::Map::new();
            if let Some(name) = item.pointer("/function/name") {
                function.insert("name".to_string(), name.clone());
            }
            if let Some(description) = item.pointer("/function/description") {
                function.insert("description".to_string(), description.clone());
            }
            if let Some(parameters) = item.pointer("/function/parameters") {
                function.insert("parameters".to_string(), parameters.clone());
            }
            json!({ "type": "function", "function": Value::Object(function) })
        })
        .collect();
    if mapped.is_empty() {
        None
    } else {
        Some(mapped)
    }
}

/// 一次请求的思考设置（见模块头「关闭思考」的说明）
pub struct ThinkingChoice {
    /// 是否显式开启思考（None = 不指定参数，让上游走默认）
    pub enable: Option<bool>,
    /// 思考档位（仅当 `enable == Some(true)` 时才会被发出去）
    pub effort: Option<String>,
}

/// 下游请求体里表达思考档位的那个原始值（`None` = 三个键**都不存在**）。
///
/// ── 语义细节：`Some(Value::Null)` 是「键在、值是 null」──────────
/// 取值链是 `reasoning_effort` → `reasoning` → `thinking`，**命中第一个存在的
/// 键就停**（不跳过 null）—— 这与本文件原来的写法逐字一致，改这个函数不要动
/// 这条语义：`resolve_thinking` 的行为完全建立在它之上。
/// 因此「没表达」与「写了 null」是两件事，调用方按自己的语义区分
/// （`resolve_thinking` 把两者都当「未指定」处理，见那里；
/// 适配器判「客户端指定过没有」则只看非 null 的值）。
///
/// ── 为什么三个键的取值链只写在这里一份 ───────────────────────
/// 两个消费方必须同源：
///   1. [`resolve_thinking`]（真正翻译成上游参数）；
///   2. 适配器的 `reasoning_patch` 判「映射绑定要不要注入」—— 客户端已经显式
///      指定时绑定不覆盖（那是比映射默认值更具体的意图）。
/// 两处各抄一份键名清单的话，将来上游加一个别名时，绑定会开始**覆盖**一个
/// 「客户端其实已经指定了」的请求，而那种错误没有任何日志会提示。
pub fn declared_reasoning(body: &Value) -> Option<Value> {
    body.get("reasoning_effort")
        .or_else(|| body.get("reasoning"))
        .or_else(|| body.get("thinking"))
        .cloned()
}

/// 下游是不是**真的**指定了思考档位（键在且值非 null）。
///
/// 与 [`declared_reasoning`] 的差别只有 null 那一档，用在「映射绑定要不要让位」
/// 的判断上。**把 null 当作「没指定」**（而不是「客户端表达过」）的两条依据：
///   1. 同一批上游里 CatPaw 的 `resolve_effort` 就是这么做的（显式
///      `.filter(|value| !value.is_null())` 再往后找），两家的口径应当一致；
///   2. JSON 的可选字段写 null 是「没设」的常规写法（不少客户端会把未填的
///      可选字段序列化成 null），把它当成一次表达会让这些客户端的绑定静默失效。
///
/// 代价说清：客户端若真的同时写了 `reasoning_effort: null` 与另一个键
/// （如 `reasoning: "off"`），原实现本来也只认第一个键（见 `declared_reasoning`），
/// 所以那个「off」在注入前后都是被忽略的 —— 这里没有引入新的覆盖。
pub fn client_specified_reasoning(body: &Value) -> bool {
    declared_reasoning(body).is_some_and(|value| !value.is_null())
}

/// 把下游的思考强度映射成上游接受的值（源实现 `resolveThinking` + `chat.mjs`）。
///
/// 下游可能用 `reasoning_effort` / `reasoning` / `thinking` 任一字段表达
/// （取值链见 [`declared_reasoning`]）。
pub fn resolve_thinking(body: &Value, model: &Value) -> ThinkingChoice {
    let raw = declared_reasoning(body).unwrap_or(Value::Null);

    let reasoning = model.get("reasoning").map(truthy).unwrap_or(false);
    // 模型不支持思考：直接不指定
    if !reasoning {
        return ThinkingChoice { enable: None, effort: None };
    }

    // 下游请求「关闭思考」：上游无法真正关闭，不发 false（理由见模块头）
    if matches!(raw, Value::Bool(false))
        || matches!(raw.as_str(), Some("off") | Some("none") | Some("disabled"))
    {
        return ThinkingChoice { enable: None, effort: None };
    }

    // 未指定（或显式 true）：沿用上游默认档位
    if raw.is_null() || matches!(raw, Value::Bool(true)) {
        return ThinkingChoice { enable: Some(true), effort: None };
    }

    let asked = match &raw {
        Value::String(text) => text.trim().to_lowercase(),
        other => other.to_string().trim().to_lowercase(),
    };

    // 常见别名归一：OpenAI 侧的 minimal/high 与上游档位对齐
    let wanted = match asked.as_str() {
        "minimal" | "min" => "low",
        "high" | "max" => "xhigh",
        other => other,
    };

    let declared: Vec<String> = model
        .get("efforts")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    // 模型声明了档位就以它为准；没声明就只在标准三档里做保守映射
    let pool: Vec<String> = if declared.is_empty() {
        vec!["low".to_string(), "medium".to_string(), "xhigh".to_string()]
    } else {
        declared.clone()
    };
    let effort = if pool.iter().any(|known| known == wanted) {
        wanted.to_string()
    } else {
        // 请求的档位模型不支持：退回该模型的默认档，而不是发一个无效值
        declared
            .iter()
            .find(|known| known.as_str() == "medium")
            .or_else(|| declared.first())
            .cloned()
            .unwrap_or_else(|| "medium".to_string())
    };
    ThinkingChoice { enable: Some(true), effort: Some(effort) }
}

/// 组装上游请求体（源实现 `buildUpstreamBody`）。
///
/// `session_seed` 用于「同一段对话复用同一 session」：下游给了 `user` 或
/// `session_id` 时用它派生，否则每次新建。
#[allow(clippy::too_many_arguments)]
pub fn build_upstream_body(
    upstream_key: &str,
    model_config: &Value,
    messages: &[Value],
    tools: Option<&Vec<Value>>,
    max_tokens: Option<i64>,
    thinking: &ThinkingChoice,
    user_id: &str,
    session_seed: Option<&str>,
) -> Value {
    let limit = max_tokens
        .filter(|value| *value > 0)
        .map(|value| value.min(MAX_OUTPUT_TOKENS))
        .unwrap_or(MAX_OUTPUT_TOKENS);
    let record_id = record_id_for(upstream_key, messages, tools, limit);

    // 取最后一条用户文本：上游要求带在 context 与 business 里
    let mut last_user_text = String::new();
    for message in messages.iter().rev() {
        if message.get("role").and_then(Value::as_str) == Some("user") {
            last_user_text = content_to_text(message.get("content").unwrap_or(&Value::Null));
            break;
        }
    }

    let mut parameters = serde_json::Map::new();
    parameters.insert("max_tokens".to_string(), Value::from(limit));
    // thinking 为 None 表示「不指定」—— 让上游走自己的默认行为。
    // 不能落成 false：Qwen3.8 系列在 enable_thinking=false 时行为异常
    // （把思考混进正文，或在需要推理的问题上直接断连），见模块头。
    match thinking.enable {
        Some(true) => {
            parameters.insert("enable_thinking".to_string(), Value::Bool(true));
            if let Some(effort) = &thinking.effort {
                parameters.insert("reasoning_effort".to_string(), Value::String(effort.clone()));
            }
        }
        Some(false) => {
            parameters.insert("enable_thinking".to_string(), Value::Bool(false));
        }
        None => {}
    }

    let request_id = super::cosy::random_uuid().unwrap_or_default();
    let session_id = session_id_for(user_id, upstream_key, session_seed);

    json!({
        "request_id": request_id,
        "request_set_id": record_id,
        "chat_record_id": record_id,
        "session_id": session_id,
        "stream": true,
        "chat_task": CHAT_TASK,
        "is_reply": true,
        "is_retry": false,
        "source": 1,
        "version": "3",
        "session_type": SESSION_TYPE,
        "agent_id": AGENT_ID,
        "task_id": "common",
        "code_language": "",
        "chat_prompt": "",
        "image_urls": Value::Null,
        "aliyun_user_type": "",
        // 上游不看这个顶层字段，system 提示要放进 messages 里（见模块头）
        "system": "",
        "messages": messages,
        "tools": tools.cloned().unwrap_or_default(),
        "parameters": Value::Object(parameters),
        "chat_context": {
            "chatPrompt": "",
            "imageUrls": Value::Null,
            "extra": {
                "context": [],
                "modelConfig": {
                    "key": upstream_key,
                    "is_reasoning": model_config
                        .get("is_reasoning")
                        .map(truthy)
                        .unwrap_or(false),
                },
                "originalContent": last_user_text,
            },
            "features": [],
            "text": last_user_text,
        },
        "model_config": slim_model_config(model_config, upstream_key),
        "business": {
            "product": "cli",
            "version": "1.0.0",
            "type": "agent",
            "stage": "start",
            "id": super::cosy::random_uuid().unwrap_or_default(),
            "name": last_user_text.chars().take(30).collect::<String>(),
            "begin_at": crate::server::logging::now_ms(),
        },
    })
}

/// 精简发往上游的 `model_config`（源实现 `slimModelConfig`）。
///
/// **必须剥掉 `thinking_config`**：目录条目原样回传会覆盖
/// `parameters.enable_thinking`，导致「关闭思考」失效（见模块头）。
pub fn slim_model_config(config: &Value, upstream_key: &str) -> Value {
    let mut slim = serde_json::Map::new();
    slim.insert("key".to_string(), Value::String(upstream_key.to_string()));
    for key in ["is_reasoning", "is_vl", "source", "format"] {
        if let Some(value) = config.get(key) {
            if !value.is_null() {
                slim.insert(key.to_string(), value.clone());
            }
        }
    }
    Value::Object(slim)
}

/// 会话 id：同一账号 + 同一模型 + 同一种子派生同一个（源实现 `sessionIdFor`）。
///
/// 种子为空时退化成一次性会话（每次请求新建），与源实现同一语义。
fn session_id_for(user_id: &str, upstream_key: &str, seed: Option<&str>) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"qoder-session");
    hasher.update([0u8]);
    hasher.update(user_id.as_bytes());
    hasher.update([0u8]);
    hasher.update(upstream_key.as_bytes());
    let base = format!("{:x}", hasher.finalize());
    let base: String = base.chars().take(16).collect();
    match seed.filter(|value| !value.is_empty()) {
        Some(seed) => format!("{base}-{seed}"),
        None => format!(
            "{base}-{}",
            super::cosy::random_uuid().unwrap_or_default()
        ),
    }
}

/// 请求指纹（源实现 `recordIdFor`）：进 `chat_record_id` / `request_set_id`。
///
/// 同一段消息 + 同一个模型 + 同一个输出上限 → 同一个记录 id，
/// 上游据此识别「这是同一次对话的延续」。
fn record_id_for(
    upstream_key: &str,
    messages: &[Value],
    tools: Option<&Vec<Value>>,
    max_tokens: i64,
) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"qoder-record");
    hasher.update([0u8]);
    hasher.update(upstream_key.as_bytes());
    for message in messages {
        if let Some(role) = message.get("role").and_then(Value::as_str) {
            hasher.update([0u8]);
            hasher.update(role.as_bytes());
        }
        if let Some(content) = message.get("content").filter(|value| !value.is_null()) {
            hasher.update([0u8]);
            match content {
                Value::String(text) => hasher.update(text.as_bytes()),
                other => hasher.update(other.to_string().as_bytes()),
            }
        }
    }
    if let Some(tools) = tools {
        hasher.update([0u8]);
        hasher.update(Value::Array(tools.clone()).to_string().as_bytes());
    }
    hasher.update([0u8]);
    hasher.update(format!("mt={max_tokens}").as_bytes());
    format!("{:x}", hasher.finalize()).chars().take(16).collect()
}

/// 从上游返回的模型名反查回对外 id：上游可能回真实 key，也可能回展示名。
pub fn map_model_back(name: &str) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    // 先按 upstreamKey 查，再按 id/name 查（与源实现 `displayNameFor` 同序）
    if let Some(model) = super::models::list().iter().find(|model| {
        model.get("upstreamKey").and_then(Value::as_str) == Some(name)
    }) {
        return model.get("name").and_then(Value::as_str).map(str::to_string);
    }
    super::models::resolve(name, super::endpoints::Region::Global)
        .and_then(|model| model.get("id").and_then(Value::as_str).map(str::to_string))
}

/// 上游状态码/错误文本 → 下游能理解的语义分类（源实现 `classifyUpstreamError`）。
///
/// ── 顺序很重要 ──────────────────────────────────────────────
/// **先看响应体里的语义特征，再看状态码**：上游用 403 表达多种情况 ——
/// 带上 pricingUrl 是套餐/额度不足，裸 403 才是鉴权问题。只按状态码判断
/// 会把「该充值」误报成「登录失效」。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UpstreamKind {
    /// 额度/套餐不足（可换账号重试）
    Quota,
    /// 触发限流（可换账号重试）
    Rate,
    /// 鉴权失败（刷新后可重试）
    Auth,
    /// 上游服务异常（可重试）
    Server,
    /// 其它（换账号也没用）
    Unknown,
}

/// 分类结果的文案与可重试性
pub struct ClassifiedError {
    /// 分类（决定编排层走哪个动作：换账号 / 刷新重试 / 原样透传）
    pub kind: UpstreamKind,
    /// 面向客户端的人话（源实现 `classifyUpstreamError` 的 `message`）
    pub message: String,
    /// 上游给出的套餐/定价页链接（有的话拼进提示）
    pub pricing_url: Option<String>,
}

/// 从文本里抠出定价页链接（源实现用正则 `/https?:\/\/[^"\\]*\/pricing[^"\\]*/i`）
fn pricing_url_of(raw: &str) -> Option<String> {
    let lowered = raw.to_lowercase();
    let mut search_from = 0usize;
    while let Some(offset) = lowered[search_from..].find("http") {
        let start = search_from + offset;
        let rest = &raw[start..];
        let end = rest
            .find(|ch: char| ch == '"' || ch == '\\' || ch.is_whitespace())
            .unwrap_or(rest.len());
        let candidate = &rest[..end];
        if candidate.to_lowercase().contains("/pricing") {
            return Some(candidate.to_string());
        }
        search_from = start + 4;
        if search_from >= raw.len() {
            break;
        }
    }
    None
}

/// 分类上游错误（源实现 `classifyUpstreamError` 的判定链，逐条对应）
pub fn classify_upstream_error(status: u16, text: &str) -> ClassifiedError {
    let body = text.to_lowercase();
    let pricing = pricing_url_of(text);
    let quota_signals = [
        "pricingurl",
        "insufficient",
        "no_quota",
        "quota_exceed",
        "exceed_quota",
        "exceeded",
        "credit",
        "upgrade",
        "subscription",
        "plan",
        "trial",
    ];
    if pricing.is_some() || quota_signals.iter().any(|signal| body.contains(signal)) {
        return ClassifiedError {
            kind: UpstreamKind::Quota,
            message: "当前账号额度不足或套餐不支持该模型".to_string(),
            pricing_url: pricing,
        };
    }
    if status == 429 || body.contains("rate limit") || body.contains("too many") {
        return ClassifiedError {
            kind: UpstreamKind::Rate,
            message: "请求过于频繁，请稍后重试".to_string(),
            pricing_url: None,
        };
    }
    if status == 401 {
        return ClassifiedError {
            kind: UpstreamKind::Auth,
            message: "登录态已失效，请重新登录".to_string(),
            pricing_url: None,
        };
    }
    if status == 403 {
        // 走到这里说明没有配额特征，按权限问题处理
        return ClassifiedError {
            kind: UpstreamKind::Auth,
            message: "上游拒绝访问，可能是登录态失效或权限不足".to_string(),
            pricing_url: None,
        };
    }
    // 上游业务码 112 是额度类，兜底再认一次
    if text.contains("\"code\":112") || text.contains("\"code\": 112") {
        return ClassifiedError {
            kind: UpstreamKind::Quota,
            message: "当前账号额度不足或套餐不支持该模型".to_string(),
            pricing_url: None,
        };
    }
    if status >= 500 {
        return ClassifiedError {
            kind: UpstreamKind::Server,
            message: "上游服务异常".to_string(),
            pricing_url: None,
        };
    }
    ClassifiedError {
        kind: UpstreamKind::Unknown,
        message: String::new(),
        pricing_url: None,
    }
}

/// JS 真值判定（`Boolean(x)`）
pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().map(|item| item != 0.0).unwrap_or(false),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}
