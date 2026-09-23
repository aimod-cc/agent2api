//! Qoder 对话转发（移植来源 `Qoder-Proxy/src/upstream.mjs` 的 `postChat` /
//! `runOnce` 与 `chat.mjs` 的 `handleChat`）。
//!
//! ── 为什么走会话式转发入口（`is_stateful`）────────────────────
//! `ProviderAdapter` 的无状态路径（`build_chat_request`）假设上游是「一次 HTTP
//! 请求 = 一次对话」且**请求体由通用层序列化后原样发出**。Qoder 有两处不满足：
//!
//!   1. 请求体必须**先编码再签名**（`cosy::encode_body` → `cosy::build_auth_headers`），
//!      而签名覆盖编码后的字节 —— 通用层的 `serde_json::to_string` 出来的字节
//!      既没编码也没被签名覆盖，发出去必然被上游拒绝；
//!   2. 下游帧需要**拆掉上游的一层信封**（`{statusCodeValue, body}` → 内层
//!      OpenAI chunk），通用层的 SSE 透传只做帧的**转发**与可选 model 回写，
//!      不认识这层包装。
//!
//! 因此本模块实现 `forward_conversation`，把「构造 → 发送 → 翻译」整条链收进来。
//! 产出仍然是编排层认识的 `ForwardOutcome`，于是 `chat.rs` 的其余链路
//! （脱敏、记账、错误写出）零改动。
//!
//! ── 额度/鉴权错误为什么在流内也要判 ───────────────────────────
//! Qoder 上游的业务错误**不体现在 HTTP 状态码上**（永远是 200），而是放在
//! SSE 信封的 `statusCodeValue` 里。所以「换个账号重试」这个动作不能只靠
//! HTTP 错误触发 —— 本模块在读到信封错误时，把它转成一个带状态码的网关错误
//! 交回编排层，由编排层的账号循环接着换下一个账号（`QuotaLimited` 语义）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：零 unwrap/expect/panic；不持锁穿越 await
//! （凭证在进函数时取好快照）。

use std::sync::Arc;

use serde_json::{json, Map, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::core::egress;
use crate::server::core::proxies::ResolvedProxy;
use crate::server::core::upstream::usage::RequestTelemetry;
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::cosy::{self, CosyIdentity};
use super::credentials::Credentials;
use super::protocol;
use super::stream;

/// 一次转发所需的全部素材（进函数时组装好，之后只读）
pub struct ChatPlan {
    pub url: String,
    pub headers: Vec<(String, String)>,
    /// 已编码的请求体（签名覆盖的就是它）
    pub body: Vec<u8>,
    /// 客户端请求的模型名（下发帧的 `model` 字段用它）
    pub model_name: String,
    /// 上游模型标识（会话派生与日志用）
    pub upstream_key: String,
    /// 是否要下发思考内容（决定要不要启用标签拆解器）
    pub thinking: bool,
}

/// 客户端请求体 → 一次上游调用的完整计划。
///
/// 这是「构造」阶段，**不含网络**，因此可以在账号循环里对每个候选账号各跑一次
/// （每个账号的凭证不同 → 签名不同）。
pub fn build_plan(
    credentials: &Credentials,
    body: &Value,
    model_name: &str,
) -> Result<ChatPlan, GatewayError> {
    let model = super::models::resolve(model_name, credentials.region).ok_or_else(|| {
        GatewayError::bad_request(format!(
            "模型不存在: {model_name}。Qoder 可用模型见 GET /v1/models"
        ))
        .with_code("model_not_found")
    })?;
    let upstream_key = model
        .get("upstreamKey")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if upstream_key.is_empty() {
        return Err(GatewayError::with_status(502, "Qoder 模型目录缺少上游标识"));
    }
    let model_config = model.get("config").cloned().unwrap_or(Value::Null);

    // ── 消息规整 ──────────────────────────────────────────────
    let raw_messages = body.get("messages").and_then(Value::as_array).cloned().unwrap_or_default();
    let messages = protocol::normalize_messages(&raw_messages);
    // system 提示必须放进 messages 里（上游顶层 system 字段无效），
    // 且要排在**最前面** —— 源实现在调用处显式做了这一步
    let system_texts: Vec<String> = raw_messages
        .iter()
        .filter(|message| {
            let role = message.get("role").and_then(Value::as_str).unwrap_or("");
            role == "system" || role == "developer"
        })
        .map(|message| {
            message
                .get("content")
                .map(protocol::content_to_text)
                .unwrap_or_default()
        })
        .filter(|text| !text.is_empty())
        .collect();
    let final_messages: Vec<Value> = if system_texts.is_empty() {
        messages
    } else {
        let mut ordered: Vec<Value> = system_texts
            .iter()
            .map(|text| json!({ "role": "system", "content": text }))
            .collect();
        ordered.extend(
            messages
                .into_iter()
                .filter(|message| message.get("role").and_then(Value::as_str) != Some("system")),
        );
        ordered
    };

    let tools = protocol::normalize_tools(body.get("tools"));
    let thinking = protocol::resolve_thinking(body, &model);
    let max_tokens = body
        .get("max_tokens")
        .and_then(Value::as_i64)
        .or_else(|| body.get("max_completion_tokens").and_then(Value::as_i64));
    // 下游若给了 user / session_id，用它做会话种子，同一对话复用同一 session
    let session_seed = body
        .get("user")
        .and_then(Value::as_str)
        .or_else(|| body.get("session_id").and_then(Value::as_str));

    let upstream_body = protocol::build_upstream_body(
        &upstream_key,
        &model_config,
        &final_messages,
        tools.as_ref(),
        max_tokens,
        &thinking,
        &credentials.user_id,
        session_seed,
    );

    // ── 编码 → 签名（顺序不能颠倒，理由见模块头）────────────────
    let encoded = cosy::encode_body(upstream_body.to_string().as_bytes());
    let url = format!(
        "{}algo/api/v2/service/pro/sse/agent_chat_generation\
         ?FetchKeys=llm_model_result&AgentId=agent_common&Encode=1",
        credentials.region.gateway()
    );
    let identity = CosyIdentity {
        user_id: &credentials.user_id,
        auth_token: &credentials.access_token,
        name: &credentials.name,
        email: &credentials.email,
        machine_id: &credentials.machine_id,
    };
    let mut headers = cosy::build_auth_headers(Some(&encoded), &url, &identity)?;
    headers.push(("Content-Type".to_string(), "application/json".to_string()));
    headers.push(("Accept".to_string(), "text/event-stream".to_string()));
    headers.push(("Cache-Control".to_string(), "no-cache".to_string()));
    headers.push(("Accept-Encoding".to_string(), "identity".to_string()));
    // 上游靠这两个头做模型路由与来源标记
    headers.push(("X-Model-Key".to_string(), upstream_key.clone()));
    headers.push(("X-Model-Source".to_string(), "system".to_string()));

    Ok(ChatPlan {
        url,
        headers,
        body: encoded,
        model_name: model_name.to_string(),
        upstream_key,
        // 只有「模型支持思考」时才启用标签拆解（否则正文里的尖括号是用户内容）
        thinking: thinking.enable.is_some() || thinking.effort.is_some(),
    })
}

/// 发一次上游请求（不读体，交给调用方决定怎么消费）。
///
/// 等待响应头有上限：值取设置页「请求超时 → 等待响应超时」（与
/// `upstream::request::send_chat_request` 同一口径、同一份配置）。Qoder 走的
/// 是自己的发送函数而不是那个入口（请求形状差异大），这个上限不能只加在
/// 统一入口上 —— 否则改名成「等待响应超时」的设置对 Qoder 静默失效。
pub async fn send(
    plan: &ChatPlan,
    proxy: Option<&ResolvedProxy>,
) -> Result<reqwest::Response, GatewayError> {
    let client = egress::client_for(proxy);
    let mut builder = client.post(&plan.url).body(plan.body.clone());
    for (key, value) in &plan.headers {
        builder = builder.header(key, value);
    }
    let via = match proxy {
        Some(proxy) if !proxy.label.is_empty() => format!("经代理 {}", proxy.label),
        Some(proxy) => format!("经代理 {}", proxy.host),
        None => "直连".to_string(),
    };
    let budget = std::time::Duration::from_millis(
        crate::server::config::timeout_settings().headers_ms(),
    );
    match tokio::time::timeout(budget, builder.send()).await {
        Ok(Ok(response)) => Ok(response),
        Ok(Err(error)) => Err(GatewayError::with_status(
            502,
            format!("Qoder 上游请求失败（{via}）: {}", egress::describe_error_detail(&error)),
        )),
        Err(_elapsed) => Err(GatewayError::with_status(
            502,
            format!(
                "Qoder 上游响应超时（等待响应头超过 {} 秒，出口 {via}）",
                budget.as_secs()
            ),
        )),
    }
}

/// 把**失败**的上游响应转成客户端可用的错误。
///
/// ── 为什么返回裸错误而不是 `Option`（调用方已经判过状态了）────────
/// 成功响应是**流式**的：`response.text()` 会把整条 SSE 拉完并丢掉流句柄，
/// 调用方就再也拿不到字节流。所以「要不要读体」这个判断必须留在调用方
/// （它先看 `is_success()`）。若本函数返回 `Option`，编译器会看到一条
/// 「读完了体、又没返回错误、继续用那个已被移动的 response」的路径 ——
/// 那是**不可达但类型上成立**的分支，只能靠调用方 `unwrap` 才能消掉，
/// 而 release 是 panic=abort，不能 unwrap。返回裸错误即让类型如实反映契约：
/// 「调用我 = 我已经不成功」。
pub async fn http_error(status: u16, response: reqwest::Response) -> GatewayError {
    let text = response.text().await.unwrap_or_default();
    let classified = protocol::classify_upstream_error(status, &text);
    let detail: String = text.chars().take(300).collect();
    let message = if classified.message.is_empty() {
        format!("上游返回 {status}: {detail}")
    } else {
        let pricing = classified
            .pricing_url
            .as_deref()
            .map(|url| format!(" 套餐与额度：{url}"))
            .unwrap_or_default();
        format!("上游请求失败：{}{pricing}（上游原文：{detail}）", classified.message)
    };
    // 额度/限流用 429、鉴权用 401：编排层按这两档决定「换账号」与「刷新后重试」
    let mapped = match classified.kind {
        protocol::UpstreamKind::Quota | protocol::UpstreamKind::Rate => 429,
        protocol::UpstreamKind::Auth => 401,
        _ => 502,
    };
    GatewayError::with_status(mapped, message).with_optional_code(Some(status as i64))
}

/// 上游帧 → 客户端帧 的翻译状态（流式与非流式共用同一套累积逻辑）。
///
/// ── 为什么把「翻译」与「下发」分开 ────────────────────────────
/// 流式要把每个 delta 立刻变成 SSE 帧；非流式要把它们聚合成一个完整
/// `chat.completion`。两者的**解析与拆解规则必须完全一致**（尤其是思考标签
/// 的跨分片处理），所以规则只写在这里一份，两条出口各自决定怎么消费产出。
pub struct Translator {
    /// 面向客户端的响应 id
    pub response_id: String,
    created: i64,
    model_name: String,
    /// 上游回传的模型名（映射回对外 id 后下发）
    model_reported: Option<String>,
    /// 工具调用按 index 累积
    tool_state: std::collections::BTreeMap<i64, ToolCallState>,
    /// 是否已下发过 role 帧（流式）
    role_sent: bool,
    /// 思考标签拆解器（**跨 chunk 长驻**）。
    ///
    /// ── 为什么是字段而不是每次现建 ─────────────────────────────
    /// 拆解器要在缓冲区里留「可能是标签前缀」的尾巴（`<thi` + `nking>` 分两片
    /// 到达）。若每个 chunk 都新建一个，那片尾巴会在 chunk 结束时被当成正文
    /// 吐出去 —— 跨分片的标签拆解就永远不会生效，而这正是它存在的全部理由。
    /// None = 该模型不支持思考，正文不作拆解（此时尖括号是用户内容）。
    parser: Option<stream::ThinkingParser>,
    /// usage 取最后一次出现
    pub usage: Option<Value>,
    /// finish_reason 取最后一次非空
    pub finish_reason: Option<String>,
    /// 非流式累积的正文
    pub content: String,
    /// 非流式累积的思考
    pub reasoning: String,
    /// 收到的 chunk 数（排障用）
    pub chunk_count: usize,
}

#[derive(Default)]
struct ToolCallState {
    id: String,
    name: String,
    arguments: String,
}

impl Translator {
    pub fn new(response_id: String, model_name: String, thinking_enabled: bool) -> Self {
        Self {
            response_id,
            created: logging::now_ms() / 1000,
            model_name,
            model_reported: None,
            tool_state: std::collections::BTreeMap::new(),
            role_sent: false,
            parser: if thinking_enabled {
                Some(stream::ThinkingParser::new())
            } else {
                None
            },
            usage: None,
            finish_reason: None,
            content: String::new(),
            reasoning: String::new(),
            chunk_count: 0,
        }
    }

    /// 一个上游 chunk → 若干个「面向客户端的 delta」。
    ///
    /// 每条产出是 `(delta, is_reasoning)`：`is_reasoning` 为真时进
    /// `reasoning_content`，否则进 `content`。工具调用单独走 `tool_deltas`。
    pub fn consume(
        &mut self,
        chunk: &Value,
        telemetry: Option<&Arc<RequestTelemetry>>,
    ) -> Vec<TranslatedDelta> {
        self.chunk_count += 1;
        if let Some(model) = chunk.get("model").and_then(Value::as_str) {
            if !model.is_empty() {
                self.model_reported = protocol::map_model_back(model);
            }
        }
        if let Some(usage) = chunk.get("usage").filter(|value| value.is_object()) {
            self.usage = Some(usage.clone());
            if let Some(telemetry) = telemetry {
                telemetry.report_usage(usage);
            }
        }
        let mut out: Vec<TranslatedDelta> = Vec::new();
        let Some(choice) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        else {
            return out;
        };
        if let Some(finish) = choice.get("finish_reason").and_then(Value::as_str) {
            if !finish.is_empty() {
                self.finish_reason = Some(finish.to_string());
            }
        }
        let Some(delta) = choice.get("delta") else {
            return out;
        };

        // 上游显式给出的 reasoning_content 优先（且要去掉偶带的标签）
        if let Some(reasoning) = delta.get("reasoning_content").and_then(Value::as_str) {
            let cleaned = stream::strip_thinking_tags(reasoning);
            if !cleaned.is_empty() {
                self.reasoning.push_str(&cleaned);
                out.push(TranslatedDelta::Reasoning(cleaned));
            }
        }
        if let Some(content) = delta.get("content").and_then(Value::as_str) {
            if !content.is_empty() {
                if self.parser.is_some() {
                    // 上游可能把思考混在正文里：按标签拆开（**跨分片的标签前缀
                    // 留在解析器缓冲里**，所以解析器必须长驻，见字段说明）
                    let pieces = {
                        let parser = match self.parser.as_mut() {
                            Some(parser) => parser,
                            None => return out,
                        };
                        parser.push(content);
                        parser.take()
                    };
                    for piece in pieces {
                        if piece.is_thinking {
                            self.reasoning.push_str(&piece.text);
                            out.push(TranslatedDelta::Reasoning(piece.text));
                        } else {
                            self.content.push_str(&piece.text);
                            out.push(TranslatedDelta::Content(piece.text));
                        }
                    }
                } else {
                    self.content.push_str(content);
                    out.push(TranslatedDelta::Content(content.to_string()));
                }
            }
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                let index = call.get("index").and_then(Value::as_i64).unwrap_or(0);
                let entry = self.tool_state.entry(index).or_default();
                if let Some(id) = call.get("id").and_then(Value::as_str) {
                    if !id.is_empty() {
                        entry.id = id.to_string();
                    }
                }
                if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
                    if !name.is_empty() {
                        entry.name = name.to_string();
                    }
                }
                if let Some(arguments) =
                    call.pointer("/function/arguments").and_then(Value::as_str)
                {
                    entry.arguments.push_str(arguments);
                }
                out.push(TranslatedDelta::ToolCall {
                    index,
                    id: call.get("id").and_then(Value::as_str).map(str::to_string),
                    name: call.pointer("/function/name").and_then(Value::as_str).map(str::to_string),
                    arguments: call
                        .pointer("/function/arguments")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                });
            }
        }
        out
    }

    /// 收尾：把解析器缓冲里残留的尾巴冲刷出来。
    ///
    /// **必须在流结束时调用**：解析器留着「可能是标签前缀」的尾巴（见
    /// `parser` 字段的说明），不冲刷的话回答末尾会少几个字符
    /// （例如正文以 `<think` 结尾时，那几个字符会永远留在缓冲里）。
    /// 幂等：解析器的 `finish` 有 finished 标记，重复调用不会再产出。
    /// 返回值与 `consume` 同形，调用方按同一套规则下发。
    pub fn finish(&mut self) -> Vec<TranslatedDelta> {
        let Some(parser) = self.parser.as_mut() else {
            return Vec::new();
        };
        parser.finish();
        let mut out = Vec::new();
        for piece in parser.take() {
            if piece.is_thinking {
                self.reasoning.push_str(&piece.text);
                out.push(TranslatedDelta::Reasoning(piece.text));
            } else {
                self.content.push_str(&piece.text);
                out.push(TranslatedDelta::Content(piece.text));
            }
        }
        out
    }

    /// 下游要的 model 名：优先用上游回传值映射后的结果，否则用客户端请求的名字
    pub fn model_out(&self) -> String {
        self.model_reported
            .clone()
            .unwrap_or_else(|| self.model_name.clone())
    }

    /// 流式的 role 帧是否已发过（第一次产出 delta 前要补一帧 role）
    pub fn take_role_frame(&mut self) -> bool {
        if self.role_sent {
            return false;
        }
        self.role_sent = true;
        true
    }

    /// 收尾的工具调用聚合（非流式用；参数拼好后做一次 JSON 规范化）
    pub fn final_tool_calls(&self) -> Vec<Value> {
        self.tool_state
            .iter()
            .map(|(index, state)| {
                let arguments = match serde_json::from_str::<Value>(&state.arguments) {
                    Ok(parsed) => parsed.to_string(),
                    Err(_) => {
                        if state.arguments.is_empty() {
                            "{}".to_string()
                        } else {
                            state.arguments.clone()
                        }
                    }
                };
                let id = if state.id.is_empty() {
                    format!("call_{index}")
                } else {
                    state.id.clone()
                };
                json!({
                    "id": id,
                    "type": "function",
                    "function": { "name": state.name, "arguments": arguments },
                })
            })
            .collect()
    }

    /// 最终 finish_reason：有工具调用就是 `tool_calls`
    pub fn final_finish(&self) -> String {
        if !self.tool_state.is_empty() {
            return "tool_calls".to_string();
        }
        self.finish_reason.clone().unwrap_or_else(|| "stop".to_string())
    }

    /// 流式 chunk 帧（OpenAI `chat.completion.chunk`）
    pub fn chunk_frame(&self, delta: Value, finish: Option<&str>) -> Value {
        let mut choice = Map::new();
        choice.insert("index".to_string(), Value::from(0));
        choice.insert("delta".to_string(), delta);
        choice.insert(
            "finish_reason".to_string(),
            finish.map(|text| Value::String(text.to_string())).unwrap_or(Value::Null),
        );
        json!({
            "id": self.response_id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model_out(),
            "choices": [Value::Object(choice)],
        })
    }

    /// usage 帧（OpenAI 在流的末尾下发的形态：choices 为空数组）
    pub fn usage_frame(&self) -> Value {
        json!({
            "id": self.response_id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model_out(),
            "choices": [],
            "usage": self.usage.clone().unwrap_or(Value::Null),
        })
    }

    /// 非流式的完整响应体（OpenAI `chat.completion`）
    pub fn completion_body(&self) -> Value {
        let mut message = Map::new();
        message.insert("role".to_string(), Value::String("assistant".to_string()));
        message.insert(
            "content".to_string(),
            if self.content.is_empty() {
                Value::Null
            } else {
                Value::String(self.content.clone())
            },
        );
        if !self.reasoning.is_empty() {
            message.insert(
                "reasoning_content".to_string(),
                Value::String(self.reasoning.clone()),
            );
        }
        let tool_calls = self.final_tool_calls();
        if !tool_calls.is_empty() {
            message.insert("tool_calls".to_string(), Value::Array(tool_calls));
        }
        json!({
            "id": self.response_id,
            "object": "chat.completion",
            "created": self.created,
            "model": self.model_out(),
            "choices": [{
                "index": 0,
                "message": Value::Object(message),
                "finish_reason": self.final_finish(),
            }],
            "usage": self.usage.clone().unwrap_or_else(|| json!({
                "prompt_tokens": 0,
                "completion_tokens": 0,
                "total_tokens": 0,
            })),
        })
    }
}

/// 从上游 chunk 翻译出的一条增量
pub enum TranslatedDelta {
    Content(String),
    Reasoning(String),
    ToolCall {
        index: i64,
        id: Option<String>,
        name: Option<String>,
        arguments: Option<String>,
    },
}

/// 把一条 `TranslatedDelta` 变成 OpenAI 的 delta JSON（流式下发用）
pub fn delta_json(delta: &TranslatedDelta) -> Value {
    match delta {
        TranslatedDelta::Content(text) => json!({ "content": text }),
        TranslatedDelta::Reasoning(text) => json!({ "reasoning_content": text }),
        TranslatedDelta::ToolCall { index, id, name, arguments } => {
            let mut function = Map::new();
            if let Some(name) = name {
                function.insert("name".to_string(), Value::String(name.clone()));
            }
            if let Some(arguments) = arguments {
                function.insert("arguments".to_string(), Value::String(arguments.clone()));
            }
            let mut call = Map::new();
            call.insert("index".to_string(), Value::from(*index));
            if let Some(id) = id {
                call.insert("id".to_string(), Value::String(id.clone()));
            }
            call.insert("type".to_string(), Value::String("function".to_string()));
            call.insert("function".to_string(), Value::Object(function));
            json!({ "tool_calls": [Value::Object(call)] })
        }
    }
}

/// 上游业务错误（信封里的 statusCodeValue）→ 网关错误。
///
/// ── 状态码映射必须基于**分类结果**，不能只看业务码 ──────────────
/// 上游用 403 表达多种情况：带 pricingUrl 是套餐/额度不足、裸 403 才是鉴权问题
/// （见 `protocol::classify_upstream_error` 的说明）。所以编排层要看的**不是**
/// 上游的业务码，而是「这条错误该触发哪个动作」：
///   额度 / 限流 → **429**（编排层据此标记该账号冷却并换下一个账号）；
///   鉴权        → **401**（编排层据此刷新凭证后同账号重试一次）；
///   其余        → 502（原样透传给客户端）。
/// 直接拿业务码当状态码会把「该充值」变成「登录失效」，客户端与用户都会走错方向。
pub fn business_error(
    status: u16,
    kind: protocol::UpstreamKind,
    raw: &str,
    message: &str,
    pricing_url: Option<&str>,
) -> GatewayError {
    let detail: String = raw.chars().take(300).collect();
    let message = if message.is_empty() {
        format!("上游返回 {status}: {detail}")
    } else {
        let pricing = pricing_url.map(|url| format!(" 套餐与额度：{url}")).unwrap_or_default();
        format!("上游请求失败：{message}{pricing}（上游原文：{detail}）")
    };
    let mapped = match kind {
        protocol::UpstreamKind::Quota | protocol::UpstreamKind::Rate => 429,
        protocol::UpstreamKind::Auth => 401,
        _ => 502,
    };
    GatewayError::with_status(mapped, message).with_optional_code(Some(status as i64))
}

/// 一次会话的凭证快照（跨 await 使用的形态）
pub struct AccountContext {
    pub credentials: Credentials,
    pub proxy: Option<ResolvedProxy>,
}

/// 取某账号的凭证快照（含临期主动刷新）。
pub async fn account_context(
    store: &AccountStore,
    account_id: &str,
    force_refresh: bool,
) -> Result<AccountContext, GatewayError> {
    let credentials = super::refresh::ensure_fresh(store, account_id, force_refresh).await?;
    let (record, _) = super::refresh::snapshot(store, account_id)?;
    let proxy = super::auth::account_proxy(&record)?;
    Ok(AccountContext { credentials, proxy })
}
