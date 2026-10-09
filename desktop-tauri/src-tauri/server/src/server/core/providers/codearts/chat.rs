//! CodeArts 对话：出站请求构造、非流式聚合、流内错误判定与首包门的接线。
//!
//! ── 上游协议 ────────────────────────────────────────────────
//! 对话打 `POST {base}/api/v2/chat/completions`，**但请求不是纯 OpenAI 形状**：
//! 除了标准的 `model` / `messages` / `stream`，还要带上模型名相关的三个头
//! （`model-id` / `model-name` / `x-model-id`，值都是上游模型名），福利模型还要
//! 额外带 `maas_type: benefit`。这些头**参与签名**，所以必须与 body 一起决定。
//!
//! ── 为什么这里有一整套「喂帧」的纯函数 ──────────────────────
//! 流式转发的正确性有三块特别容易错、又都不需要真上游就能验：
//!   1. **流内错误信封不能当成内容发出去**（`stream_fault.rs`）—— 判错就丢账号。
//!   2. **首包门**：在"发出第一字节"之前把响应按住；流干净结束却一个字都没答 →
//!      这是**空回答**，必须当成失败（否则跨账号降级链永远不会被触发）。
//!   3. **聚合**：非流式请求要把 SSE 折叠成一条 completion，其中**只有 reasoning
//!      没有 content** 的回复要按"有内容"计（`max_tokens` 给小值时上游会只吐
//!      reasoning，若按 content 判空就会把一次成功的回答报成失败）。
//! 所以本模块把这三块都写成可单独调用的纯函数，测试直接喂字节。
//!
//! 真正的网络编排（拿到 `reqwest::Response`、按首包门决定是否交给客户端、
//! 换账号）与 accio 同构，放在适配器里复用既有机制。

use serde_json::{json, Value};

use crate::server::core::egress;
use crate::server::core::upstream::usage::RequestTelemetry;
use crate::server::errors::GatewayError;

use super::credentials::Credential;
use super::oauth::signer_credential;
use super::redact;
use super::signer;
use super::stream_fault::{self, StreamFault};

/// 对话端点（相对 base）。
pub const CHAT_PATH: &str = "/api/v2/chat/completions";
/// SSE 结束哨兵。
const DONE_SENTINEL: &str = "[DONE]";
/// 客户端指纹的默认值（照参考实现的 config 默认档）。
pub const DEFAULT_PLUGIN_NAME: &str = "snap_vscode";
pub const DEFAULT_PLUGIN_VERSION: &str = "26.9.101";
pub const DEFAULT_LANGUAGE: &str = "en-us";

/// 一次对话请求要用的头（不含签名，签名在 [`build_upstream_request`] 里做）。
///
/// 这些头**全部参与签名**，所以顺序与内容都不能随手改
/// （`signer.rs` 会把它们按名字排序后拼进规范请求串）。
pub struct HeaderProfile {
    pub plugin_name: String,
    pub plugin_version: String,
    pub language: String,
    pub is_confidential: bool,
    /// 会话并发心跳用的 `User-Session-Id`（M3 接上；None 即不带）
    pub chat_session_id: Option<String>,
}

impl Default for HeaderProfile {
    fn default() -> Self {
        Self {
            plugin_name: DEFAULT_PLUGIN_NAME.to_string(),
            plugin_version: DEFAULT_PLUGIN_VERSION.to_string(),
            language: DEFAULT_LANGUAGE.to_string(),
            is_confidential: false,
            chat_session_id: None,
        }
    }
}

impl HeaderProfile {
    fn headers(&self, model: &str) -> Vec<(String, String)> {
        let mut headers = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Accept".to_string(), "text/event-stream".to_string()),
            (
                "client_version".to_string(),
                format!("Vscode_{}", self.plugin_version),
            ),
            ("Agent-Type".to_string(), "ChatAgent".to_string()),
            ("X-Language".to_string(), self.language.clone()),
            (
                "is_confidential".to_string(),
                if self.is_confidential { "true" } else { "false" }.to_string(),
            ),
            ("plugin-name".to_string(), self.plugin_name.clone()),
            ("plugin-version".to_string(), self.plugin_version.clone()),
            // 上游把模型名也当请求头（三个键都要，少一个都不行）
            ("model-id".to_string(), model.to_string()),
            ("model-name".to_string(), model.to_string()),
            ("x-model-id".to_string(), model.to_string()),
        ];
        if let Some(session_id) = self.chat_session_id.as_ref() {
            headers.push(("User-Session-Id".to_string(), session_id.clone()));
        }
        headers
    }
}

/// 构造一次出站对话请求。
///
/// * `payload` 是客户端原始请求体（OpenAI 形状）；`model` 是**上游模型名**
///   （不是客户端看到的名字，映射在目录层做）
/// * `benefit` 为真时注入 `maas_type: benefit`（福利模型才需要，普通模型带了
///   反而会被判成"没领福利"）
/// * 出站前把 `max_tokens` / `max_completion_tokens` 超过上游硬顶的取值钳到
///   65536（[`clamp_output_tokens`]）—— 上游对超顶取值**整条**拒收
///   （`InferHub.001001005.400`），不钳就等于每次对话全账号 502
/// * 返回 `(完整地址, 已签名的头, 请求体)`；`credential` 允许为空 —— 空的时候
///   不签名（调试逃生口，与参考实现一致）
pub fn build_upstream_request(
    base_url: &str,
    model: &str,
    mut payload: Value,
    stream: bool,
    benefit: bool,
    profile: &HeaderProfile,
    credential: Option<&Credential>,
) -> Result<(String, Vec<(String, String)>, Vec<u8>), GatewayError> {
    let model = model.trim();
    if model.is_empty() {
        return Err(GatewayError::with_status(400, "CodeArts 模型名为空，请从 /v1/models 里选一个"));
    }
    if payload.get("messages").and_then(Value::as_array).is_none() {
        return Err(GatewayError::with_status(400, "请求体缺少 messages 数组"));
    }
    let endpoint = format!("{}{}", base_url.trim_end_matches('/'), CHAT_PATH);
    let object = payload
        .as_object_mut()
        .ok_or_else(|| GatewayError::with_status(400, "请求体必须是 JSON 对象"))?;
    object.insert("model".to_string(), Value::String(model.to_string()));
    object.insert("stream".to_string(), Value::Bool(stream));
    if stream {
        // 让上游在最后一帧带上 usage（不要求它就永远不会给）
        object.insert("stream_options".to_string(), json!({ "include_usage": true }));
    }
    // 「关思考」在本家没有可发送的表达：始终思考的名字收到 `reasoning_effort:"none"`
    // 会被上游整条拒掉（dev 一手：Qoder 的侧路调用每发都撞，错误是
    // `InferHub.malformed_json.400：该模型始终思考，不支持关闭思考；请使用 low、high 或 max`）。
    // 与 `model_rules::reasoning_is_off` 那一侧的既有口径同一条：off/none 不注入上游 ——
    // 客户端自己写的这个键也算注入，所以在签名前摘掉（对照：同一份 body 不带该键，本家直接 200）。
    for key in drop_thinking_off(&mut payload) {
        crate::server::logging::verbose(
            "[CodeArts]",
            &format!("出站前摘掉 {key}：本家没有「关闭思考」这一档，带着它上游会整条拒收"),
        );
    }
    // 丢历史思考也要在序列化之前：签名覆盖的就是这串字节，少发的字节才算数
    if !keep_historical_reasoning() {
        let dropped = strip_historical_reasoning(&mut payload);
        if dropped > 0 {
            crate::server::logging::verbose(
                "[CodeArts]",
                &format!("出站前丢掉 {dropped} 段历史思考（重放给下一个模型没有信息量，实测占体积 38–45%）"),
            );
        }
    }
    // 强制某工具的 `tool_choice` 在本家发不出去，折成字符串 `required`
    if let Some(named) = fold_forced_tool_choice(&mut payload) {
        crate::server::logging::verbose(
            "[CodeArts]",
            &format!("出站前把 tool_choice 指名折成 required（客户端点名的是 {named}）：本家不收对象形态，折完模型自己挑一个工具"),
        );
    }
    // 输出上限钳制（实测依据见 `clamp_output_tokens`）：放在序列化前的最后一步，
    // 此后没有任何一步会再动 body。
    clamp_output_tokens(&mut payload);
    let body = serde_json::to_vec(&payload)
        .map_err(|error| GatewayError::with_status(400, format!("请求体序列化失败：{error}")))?;
    let mut headers = profile.headers(model);
    if benefit {
        headers.push(("maas_type".to_string(), "benefit".to_string()));
    }
    let Some(credential) = credential else {
        // 无凭据不发签名请求：这是排障用的逃生口，正常路径不该走到
        return Ok((endpoint, headers, body));
    };
    let signed = signer::sign("POST", &endpoint, &headers, &body, &signer_credential(credential), false)
        .map_err(|reason| GatewayError::with_status(500, reason))?;
    Ok((endpoint, signed, body))
}

/// 上游对**输出上限**（`max_tokens` / `max_completion_tokens` 的取值）的硬顶，
/// 单位是 Token 个数。实测值，不是推测值 —— 依据见 [`clamp_output_tokens`]。
pub const MAX_OUTPUT_TOKENS: i64 = 65_536;

/// 出站请求体 `max_tokens` / `max_completion_tokens` 的**上限归一**（就地修改）。
///
/// ── 上游行为（2026-10-09 本机实测，逐格结论）──────────────────
/// 这两个键的**取值**参与上游预校验：**65536 放行、65537 起一律拒收**，整条
/// 请求回 `InferHub.001001005.400：The request param is invalid`（不是截断、
/// 也不是部分失败；两个键各自都受这道校验）。边界两侧都逐点打过：65536 过，
/// 65537 / 73728 / 81920 / 98304 / 100000 / 122880 / 128000 / 131071 / 131072 全拒。
/// 三条"不是它"（都已排除，别再把排查引回去）：**与提示词长度无关**（2 万字符
/// 的真实请求体 + 65536 → 200）、**与模型无关**（福利 `glm-5.3-flash` 与非福利
/// `glm-5.2-sft-harmony` 边界一致，是平台级校验）、**与其它参数无关**（35 个
/// `tools` + `tool_choice` / `thinking` / `enable_thinking` / `reasoning` /
/// `reasoning_effort` / `prompt_cache_key` 单独加都是 200）。
///
/// ── 目录里的 maxOutputTokens 不是依据 ───────────────────────
/// 上游目录给 `glm-5.3-flash` 声明 `maxOutputTokens: 131072`（`deepseek-v4-*`
/// 甚至声明 393216），**与实际执行的 65536 不符** —— 照声明钳等于没钳，
/// 128000 照样被拒。
///
/// ── 为什么必须修：不钳 = 每次对话整条 502 ────────────────────
/// ZCode 这类客户端按自己的上下文长度发 `max_completion_tokens: 128000`，原样
/// 透传时表现为「CodeArts 账号逐个 502、换号也没用」，而模型测试路径不带这个
/// 字段、一路 200 —— 两边现象对不上，极易把排查带向账号 / 网络 / 思考等级
/// （实测：真实请求体**只去掉**这一个字段 → 200，加回 128000 → 502）。
///
/// ── 归一规则（只在「超过硬顶」窗口里动手）────────────────────
///   - 字段存在、是整数（`as_i64`）且 > [`MAX_OUTPUT_TOKENS`] → 改写为
///     `MAX_OUTPUT_TOKENS`。**钳而不是删**：删等于让上游按默认档回答（通常
///     几千），客户端"要长回答"的意图直接丢失；钳到硬顶是上游能给的最大值；
///   - **缺省时不注入**、非整数形态（字符串 / null / 浮点）不碰：其余形态交
///     上游自己报错，不在网关里猜语义；
///   - 两个键各判各的，只改客户端**用了的那个键**，不替它新造键；
///   - 本函数**不打日志**：客户端每个请求都带这个值，钳一次写一行会把日志刷满
///     （与 autoclaw 的 `normalize_max_tokens` 同一取舍）。
///
/// ── 判据纪律：写在本适配器里 = 按 provider 生效，别改按模型名 ──
/// 与 [`reserve_for_thinking`] 同一条：别家上游没有这道预校验（小浣熊那边
/// `max_completion_tokens` 原样可用），因此判据只住本文件、只被
/// [`build_upstream_request`] 调用，天然只对 CodeArts 出站生效。
fn clamp_output_tokens(body: &mut Value) {
    let Some(object) = body.as_object_mut() else {
        return;
    };
    for key in ["max_tokens", "max_completion_tokens"] {
        let over = matches!(object.get(key), Some(Value::Number(n))
            if n.as_i64().is_some_and(|value| value > MAX_OUTPUT_TOKENS));
        if over {
            object.insert(
                key.to_string(),
                Value::Number(serde_json::Number::from(MAX_OUTPUT_TOKENS)),
            );
        }
    }
}

/// 摘掉客户端请求体里表达「关闭思考」的键，返回被摘掉的键名（用于日志）。
///
/// 三种写法都算「关思考」：`reasoning_effort` / `reasoningEffort` / `effort` 的
/// 值为 `off` 或 `none`（判据复用 `model_rules::reasoning_is_off`，与绑定注入那一侧
/// 同一个口径，不再各写一份），以及布尔形式的 `enable_thinking:false`。
///
/// **只摘这几种，别的值一律原样走**：`low` / `medium` / `high` / `max` 是上游收的档位，
/// `enable_thinking:true` 是「开思考」，都不该由网关替客户端做主。表外值（比如自定义
/// 字符串）也不摘 —— 摘掉就等于把客户端的意图吞了，而本家对它的回答我们没测过。
pub fn drop_thinking_off(payload: &mut Value) -> Vec<String> {
    use crate::server::core::model_rules::reasoning_is_off;

    let mut dropped = Vec::new();
    let Some(object) = payload.as_object_mut() else {
        return dropped;
    };
    for key in ["reasoning_effort", "reasoningEffort", "effort"] {
        let off = object
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(reasoning_is_off);
        if off {
            object.remove(key);
            dropped.push(key.to_string());
        }
    }
    if object.get("enable_thinking") == Some(&Value::Bool(false)) {
        object.remove("enable_thinking");
        dropped.push("enable_thinking".to_string());
    }
    dropped
}

#[cfg(test)]
mod thinking_off_switches {
    use super::{build_upstream_request, drop_thinking_off, HeaderProfile};
    use serde_json::json;

    #[test]
    fn only_the_thinking_off_expressions_are_dropped() {
        let mut payload = json!({
            "reasoning_effort": "none",
            "enable_thinking": false,
        });
        assert_eq!(
            vec!["reasoning_effort".to_string(), "enable_thinking".to_string()],
            drop_thinking_off(&mut payload)
        );
        assert_eq!(payload, json!({}), "摘完不该留下空壳之外的东西");

        // 驼峰与简写两种拼法都算同一件事
        let mut aliases = json!({"reasoningEffort": "off", "effort": "NONE"});
        assert_eq!(
            vec!["reasoningEffort".to_string(), "effort".to_string()],
            drop_thinking_off(&mut aliases)
        );
        assert_eq!(aliases, json!({}));
    }

    #[test]
    fn real_levels_and_an_explicit_yes_survive_untouched() {
        // 反空跑：这一条保证实现没有退化成「把思考相关的键全删」。上游认的四个档位
        // 与 `enable_thinking:true` 都是客户端的有效意图，删掉就是替客户端做主。
        for level in ["low", "medium", "high", "max"] {
            let mut payload = json!({"reasoning_effort": level, "enable_thinking": true});
            assert_eq!(Vec::<String>::new(), drop_thinking_off(&mut payload), "{level}");
            assert_eq!(
                json!({"reasoning_effort": level, "enable_thinking": true}),
                payload
            );
        }
        // 表外值（自定义档位）不摘：本家会怎么回它我们没测过，摘掉等于吞掉客户端意图
        let mut unknown = json!({"reasoning_effort": "turbo"});
        assert_eq!(Vec::<String>::new(), drop_thinking_off(&mut unknown));
        assert_eq!(json!({"reasoning_effort": "turbo"}), unknown);
    }

    #[test]
    fn the_signed_path_never_carries_a_thinking_off_key() {
        // 端到端钉在 build_upstream_request 的出口上（`credential=None` 是排障逃生口，
        // 返回的就是没签名前那串字节）：判据落在真正出门的字节上，不落在中间函数上。
        let payload = json!({
            "messages": [{"role": "user", "content": "短答"}],
            "reasoning_effort": "none",
            "enable_thinking": false,
        });
        let profile = HeaderProfile::default();
        let (_, _, body) = build_upstream_request(
            "https://example.invalid",
            "glm-5.3-flash",
            payload,
            true,
            true,
            &profile,
            None,
        )
        .expect("构造出站请求");
        let text = String::from_utf8_lossy(&body).to_string();
        assert!(!text.contains("reasoning_effort"), "出门字节里不该有它：{text}");
        assert!(!text.contains("enable_thinking"), "出门字节里不该有它：{text}");
    }
}

/// 出站前把**历史里的思考内容**丢掉，返回丢掉的块数。
///
/// ── 为什么是这一刀 ────────────────────────────────────────
/// 实测上界在 6 MiB 附近（5,310,247 过 / 6,372,244 拒，见 `SIZE_CEILING_KNOWN_BYTES`），
/// 而真实 agent 会话里 `thinking` 占 messages 内字节 **38–45%**（两个大会话量出来：
/// 26.8 MB 与 12.9 MB 的转录），是结构化削减里唯一还站得住的大头 ——
/// 重复文件读取只值 9–12%、相邻 tool_result 打包实测省 0%、这类会话里图片是 0%。
///
/// 客户端每轮都把**全部历史思考**原样重放回来，而它对下游模型没有信息量：
/// 那是上一个模型自己怎么想的，不是这个模型需要读的事实。
///
/// ── 只动 assistant、只动思考 ──────────────────────────────
/// `user` 那一侧不碰（思考只可能出现在 assistant 轮里，动了就是改用户输入）。
/// **消息条数一条不少** —— 这一刀只减内容、不减轮次：别家按条数做会话预算的口径
/// 不该被本家的一次削减顺手改掉。只剩思考的那条不删整条，改成空串内容：
/// 上游对"缺 assistant 轮"的容忍度没人保证，形状稳定比省一条更值。
///
/// 两种形状都清：OpenAI 系的 `reasoning_content` / `reasoning` 字段，
/// 与 Anthropic 系 content 数组里的 `thinking` / `redacted_thinking` 块。
/// （不在别的 provider 上做的理由：Anthropic 的 `tool_use` 轮要求把带签名的
/// thinking 一起回传，全局一刀切会直接变成上游报错 —— 那一家要单独判。）
pub fn strip_historical_reasoning(payload: &mut Value) -> usize {
    let Some(messages) = payload.get_mut("messages").and_then(Value::as_array_mut) else {
        return 0;
    };
    let mut dropped = 0;
    for message in messages.iter_mut() {
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(object) = message.as_object_mut() else {
            continue;
        };
        // OpenAI 形状：思考是消息上的一个字符串字段
        for key in ["reasoning_content", "reasoning"] {
            match object.remove(key) {
                Some(Value::String(text)) if !text.trim().is_empty() => dropped += 1,
                Some(Value::Null) | None => {}
                Some(_) => dropped += 1,
            }
        }
        // Anthropic 形状：思考是 content 数组里的块
        let mut emptied = false;
        if let Some(blocks) = object.get_mut("content").and_then(Value::as_array_mut) {
            let before = blocks.len();
            blocks.retain(|block| {
                !matches!(
                    block.get("type").and_then(Value::as_str),
                    Some("thinking") | Some("redacted_thinking")
                )
            });
            dropped += before - blocks.len();
            emptied = before != blocks.len() && blocks.is_empty();
        }
        if emptied {
            // 只剩思考的那条：留一个空串内容，别把整条消息删掉（见函数头的理由）
            object.insert("content".to_string(), Value::String(String::new()));
        }
    }
    dropped
}

/// 要不要保留历史思考（默认丢）。留给现网出问题时**不改代码**就能退回旧行为的一格。
fn keep_historical_reasoning() -> bool {
    keep_historical_reasoning_from(std::env::var("CODEARTS_KEEP_REASONING").ok().as_deref())
}

/// 上一行的判据本体（认 `1` / `true` / `yes`，大小写敏感，其余一律按默认走）。
/// 拆开只为能测：进程环境变量是全局的，测试里改它会污染并发跑别的用例。
fn keep_historical_reasoning_from(value: Option<&str>) -> bool {
    matches!(value, Some("1") | Some("true") | Some("yes"))
}

/// 折叠出来的非流式回答。
#[derive(Debug, Default)]
pub struct Aggregated {
    /// 正文（`choices[].delta.content` 的拼接）
    pub content: String,
    /// 思考内容（`reasoning_content`），单列出来不混进正文
    pub reasoning: String,
    pub tool_calls: Vec<Value>,
    pub usage: Option<Value>,
    pub finish_reason: String,
    /// 上游给过的 id / model（有就用，没有就自己造一个）
    pub id: Option<String>,
    pub model: Option<String>,
}

impl Aggregated {
    /// 这次回答算不算"有内容"。
    ///
    /// **只有 reasoning 也算有**：`max_tokens` 给小值时上游会只吐思考段而正文为空，
    /// 若按正文判空就会把一次成功的回答报成"上游空回复"并触发没必要的换账号。
    pub fn has_content(&self) -> bool {
        !self.content.is_empty() || !self.reasoning.is_empty() || !self.tool_calls.is_empty()
    }

    /// 渲染成 OpenAI 非流式回答体。
    pub fn completion(&self, fallback_model: &str) -> Value {
        let mut message = json!({
            "role": "assistant",
            "content": self.content,
        });
        if !self.reasoning.is_empty() {
            message["reasoning_content"] = Value::String(self.reasoning.clone());
        }
        if !self.tool_calls.is_empty() {
            message["tool_calls"] = Value::Array(self.tool_calls.clone());
        }
        let finish_reason = if !self.finish_reason.is_empty() {
            self.finish_reason.clone()
        } else if self.tool_calls.is_empty() {
            "stop".to_string()
        } else {
            "tool_calls".to_string()
        };
        let mut body = json!({
            "id": self.id.clone().unwrap_or_else(|| format!("chatcmpl-codearts-{}", short_id())),
            "object": "chat.completion",
            "created": crate::server::logging::now_ms() / 1000,
            "model": self.model.clone().unwrap_or_else(|| fallback_model.to_string()),
            "choices": [{ "index": 0, "message": message, "finish_reason": finish_reason }],
        });
        if let Some(usage) = self.usage.clone() {
            body["usage"] = usage;
        }
        body
    }
}

/// 把一整段上游 SSE 体折叠成一条回答；同时把流内错误信封挖出来。
///
/// 返回 `Err` 的两种情形都要与"成功但空"区分开：
///   * 流里带了错误信封 → 那个错误（**这是换账号的唯一触发点**）
///   * 流干净结束但一个字都没有 → `upstream_empty_response`（502）
pub fn aggregate_sse(body: &[u8], fallback_model: &str) -> Result<Value, GatewayError> {
    let text = String::from_utf8_lossy(body);
    let mut aggregated = Aggregated::default();
    let mut seen_done = false;
    for line in text.lines() {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data == DONE_SENTINEL {
            seen_done = true;
            continue;
        }
        if let Some(fault) = stream_fault::stream_frame_fault(data) {
            return Err(stream_fault::fault_to_error(&fault));
        }
        merge_chunk(&mut aggregated, data);
    }
    if !aggregated.has_content() {
        // 空回答是**失败**：不这样处理，跨账号降级链永远不会被触发
        return Err(GatewayError::with_status(
            502,
            if seen_done {
                "CodeArts 上游流正常结束但没有返回任何内容".to_string()
            } else {
                "CodeArts 上游流意外中断且没有返回任何内容".to_string()
            },
        ));
    }
    Ok(aggregated.completion(fallback_model))
}

/// 把一帧 `choices[].delta` 合进聚合器（tool_calls 按下标合并，参数是分片拼接）。
fn merge_chunk(aggregated: &mut Aggregated, payload: &str) {
    let Ok(chunk) = serde_json::from_str::<Value>(payload) else {
        return;
    };
    if let Some(id) = chunk.get("id").and_then(Value::as_str).filter(|value| !value.is_empty()) {
        aggregated.id = Some(id.to_string());
    }
    if let Some(model) = chunk.get("model").and_then(Value::as_str).filter(|value| !value.is_empty()) {
        aggregated.model = Some(model.to_string());
    }
    if let Some(usage) = chunk.get("usage").filter(|value| !value.is_null()) {
        aggregated.usage = Some(usage.clone());
    }
    let Some(choices) = chunk.get("choices").and_then(Value::as_array) else {
        return;
    };
    for choice in choices {
        if let Some(delta) = choice.get("delta").and_then(Value::as_object) {
            if let Some(content) = delta.get("content").and_then(Value::as_str) {
                aggregated.content.push_str(content);
            }
            if let Some(reasoning) = delta.get("reasoning_content").and_then(Value::as_str) {
                aggregated.reasoning.push_str(reasoning);
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    merge_tool_call(&mut aggregated.tool_calls, call);
                }
            }
        }
        if let Some(reason) = choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            aggregated.finish_reason = reason.to_string();
        }
    }
}

/// 一次响应里最多接受多少个并行 tool_call。**这个上限存在的理由是内存，不是协议**：
/// 上游只要发一帧 `{"index":4294967295}`，下面那个「补齐到 index」的循环就会去分配
/// 四十亿个 JSON 值 —— 进程当场被 OOM killer 带走，而这只是解析一个响应帧。
/// 参考实现没有这个按 index 增长的形状，所以这条是我自己引入的，兜底也得我自己加。
const MAX_TOOL_CALLS: usize = 64;

/// 按 `index` 合并 tool_call 分片：id/type/name 取首个非空，`arguments` 是**拼接**。
fn merge_tool_call(calls: &mut Vec<Value>, incoming: &Value) {
    let index = incoming.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
    if index >= MAX_TOOL_CALLS {
        // 越界的分片**丢掉**而不是扩容：真实的并行工具调用不会到这个量级，
        // 到了就说明上游（或中间人）在喂我们异常数据，宁可少一个工具调用也不炸进程。
        return;
    }
    while calls.len() <= index {
        calls.push(json!({ "index": calls.len(), "type": "function", "function": { "name": "", "arguments": "" } }));
    }
    let target = &mut calls[index];
    if let Some(id) = incoming.get("id").and_then(Value::as_str).filter(|value| !value.is_empty()) {
        target["id"] = Value::String(id.to_string());
    }
    if let Some(kind) = incoming.get("type").and_then(Value::as_str).filter(|value| !value.is_empty()) {
        target["type"] = Value::String(kind.to_string());
    }
    if let Some(function) = incoming.get("function").and_then(Value::as_object) {
        if let Some(name) = function.get("name").and_then(Value::as_str).filter(|value| !value.is_empty()) {
            target["function"]["name"] = Value::String(name.to_string());
        }
        if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
            let joined = format!(
                "{}{}",
                target["function"]["arguments"].as_str().unwrap_or(""),
                arguments
            );
            target["function"]["arguments"] = Value::String(joined);
        }
    }
}

/// 实测的**可用上界**：5,310,247 字节走得过去，6,372,244 起被拒。取 6 MiB
/// （6,291,456）—— 外部实现报过同一个数（"max bytes … 6291456"）。
///
/// 这个数只给「文案里没有尺寸字样」那条判据用（见 [`size_rejection_by_shape`]）：
/// 它是**相关性的门槛**，不是我们要把体削到的目标 —— 真正拦体的是尺寸门
/// （`codearts::size_gate`），那里记的是这一次实际撞到的字节数。
pub const SIZE_CEILING_KNOWN_BYTES: i64 = 6 * 1024 * 1024;

/// 第二条判据：泛化的「请求参数无效」+ **这发的字节数已越过实测上界** ⇒ 按尺寸失败处理。
///
/// 为什么需要它：上游对超限体的回法不止一种形状。探针直发时带
/// `details[].error_code = PARSE_REQUEST_DATA_EXCEPTION`（文案能唯一指向体积），
/// 而经网关这一路（真模型名）拿到的 SSE 帧只有
/// `InferHub.001001005.400 / The request param is invalid, Please check it`
/// —— 没有任何尺寸字样。只等标记就永远不登记（dev 上连打四版都没动静，就是这个原因）。
///
/// 所以这条用**相关性**而不是文案：错误码 + 我们确知的发出字节数越界 ⇒ 记门。
/// 小体撞上同一个码就是真的参数错，不能记 —— 那是把一家冤枉关在门外 120 秒。
pub fn size_rejection_by_shape(text: &str, wire_bytes: i64) -> Option<&'static str> {
    if wire_bytes <= SIZE_CEILING_KNOWN_BYTES {
        return None;
    }
    if text.to_lowercase().contains("inferhub.001001005.400") {
        return Some("上游回「请求参数无效」，而这发已越过实测的 6 MiB 上界（按尺寸失败处理）");
    }
    None
}

/// 三处出口统一走这里：标记版优先，命不中再按体积相关性判。
///
/// `status` 不参与判定（理由见 `size_rejection_text` 的说明：错误码与状态码都不可信），
/// 留着只是让 HTTP 那条调用点与流内那条调用点共用一个函数。
pub fn size_rejection_any(status: u16, text: &str, wire_bytes: i64) -> Option<&'static str> {
    let _ = status;
    size_rejection_text(text).or_else(|| size_rejection_by_shape(text, wire_bytes))
}

/// 这一发是不是**体积**被拒？是的话给出那句给人读的原因。
///
/// ── 两道墙、两种症状（2026-10-09 实测；探针用不存在的模型名 ⇒ 到路由就拒、一发没计费）──
///   · **APIG 网关层**：`413` + `{"error_msg":"Request entity too large","error_code":"APIG.0201"}`
///     —— 实测 12,744,384 与 13,806,381 字节落这里（与华为文档的默认上限 12 MB 一致）。
///   · **后端解析层**：`400` + `details[].error_code = PARSE_REQUEST_DATA_EXCEPTION`
///     （外层是 `InferHub.001001005.400 The request param is invalid`）
///     —— 实测 6,372,244 / 7,434,241 / 8,496,317 落这里，而 5,310,247 走得过去。
///
/// 为什么必须两种都认：`APIG.0201` 是**过载码**（官方错误码表把它同时给了 400/413/414/494/
/// 502/504/500）⇒ 判据只能取文案；而只盯 413 会把 6–12 MB 这一整段当成「上游参数错」放过，
/// 于是每一发大请求都要先白打一次才换家。
///
/// 两条**反面对照**（用例钉着）：`404 InferHub.002002009.404 The model is not registered`
/// 是模型名问题不是体积；`400 TM.00001001 request body is invalid JSON` 是「我们发了压过的体
/// 而上游不解压」，那份体可能只有几 KB ⇒ 登记成尺寸门等于瞎挡（也说明请求侧 gzip 这条路是死的）。
pub fn size_rejection(status: u16, body: &[u8]) -> Option<&'static str> {
    let _ = status; // 见下面的说明：这一对标记**不按状态码收窄**
    size_rejection_text(&String::from_utf8_lossy(body))
}

/// 同一判据的纯文本版。**判据只取文案，不取状态码、也不取错误码**：
///
///   · `APIG.0201` 是过载码（官方错误码表把它同时给了 400/413/414/494/502/504/500），
///     所以错误码不可信；
///   · 状态码同样不可信 —— 同一句 `Request entity too large` / `PARSE_REQUEST_DATA_EXCEPTION`
///     实测出现过 **400**（探针直发）与 **502**（经网关这一路）两种状态 —— 判据若锁在
///     400/413 上，502 那发就带着标记不认，门一次也立不起来。
///
/// 这两句文案本身足够特定：反面对照里的 `model is not registered` 与
/// `request body is invalid JSON`（我们发了压缩体、上游没解压）都不含它们。
pub fn size_rejection_text(text: &str) -> Option<&'static str> {
    let lowered = text.to_lowercase();
    if lowered.contains("parse_request_data_exception") {
        return Some("上游解析不了该体积的请求体（400 PARSE_REQUEST_DATA_EXCEPTION）");
    }
    if lowered.contains("request entity too large") {
        return Some("上游网关拒收该体积（413 Request entity too large）");
    }
    None
}

/// 上游非 2xx 时的错误：**保留原始 HTTP 状态**，附脱敏后的诊断体。
///
/// 参考实现踩过的坑：错误体不读就会让客户端只看到 `upstream returned HTTP 400:`
/// 后面什么都没有；而读得太久又会把上游的 400 变成 502。所以读体有硬预算
/// （8 KiB / 128 块 / 2 秒），并且**状态码用上游原值**。
pub fn upstream_http_error(
    status: u16,
    body: &[u8],
    credential: &Credential,
    truncated: bool,
) -> GatewayError {
    let detail = redact::redact(&String::from_utf8_lossy(body), credential, truncated);
    let detail = detail.trim();
    let mut message = format!("CodeArts 上游返回 HTTP {status}");
    if !detail.is_empty() {
        message.push('：');
        message.push_str(&truncate(detail, 2000));
    }
    if truncated {
        message.push_str("（诊断体已截断）");
    }
    let error = GatewayError::with_status(i32::from(status), message);
    // 与 `stream_fault::fault_to_error` 同一张表：客户端里有一批是按 `code` 而不是
    // 状态码分支的（`insufficient_quota` 决定要不要停手重试）。参考实现也是这么分的
    // （executor.go 把 403 直接标成 insufficient_quota），别只给流内那条补、
    // 这条 HTTP 状态的路劲漏着。
    match status {
        403 => error.with_code("insufficient_quota"),
        429 => error.with_code("rate_limit_exceeded"),
        _ => error,
    }
}

/// 非 2xx 的诊断体最多读这么多字节。
///
/// 上游的报错有整页 HTML 的先例（网关层的 5xx 页面尤其长），而这段文本会进日志库、
/// 也会回给客户端。参考实现给这条留了 8 KiB 的预算并明确标注"截断了"，
/// 我第一版只在注释里写了预算、代码里却 `response.text()` 全读 ——
/// 于是"预算"是一句空话，而且 `upstream_http_error` 那个 `truncated` 参数
/// 永远收到 false，它专门实现的「末尾是某个秘密的前缀也要盖掉」那条分支成了死码。
pub const ERROR_BODY_BUDGET: usize = 8 * 1024;

/// 读诊断体，带上限；第二个返回值表示是否被截断。
pub async fn read_error_body(mut response: reqwest::Response) -> (Vec<u8>, bool) {
    let mut body: Vec<u8> = Vec::new();
    let mut truncated = false;
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                let room = ERROR_BODY_BUDGET.saturating_sub(body.len());
                if chunk.len() > room {
                    body.extend_from_slice(&chunk[..room]);
                    truncated = true;
                    break;
                }
                body.extend_from_slice(&chunk);
                if body.len() >= ERROR_BODY_BUDGET {
                    // 正好读满：还不知道后面有没有内容，按"可能还有"处理
                    truncated = true;
                    break;
                }
            }
            Ok(None) => break,
            Err(_) => {
                // 读诊断体本身失败：手上这一截照样有用，并如实标注不完整
                truncated = true;
                break;
            }
        }
    }
    (body, truncated)
}

/// 首包门的判定：流结束时的状态。
#[derive(Debug, PartialEq, Eq)]
pub enum HeadVerdict {
    /// 拿到了内容，已经把 `head` 交给客户端
    Answered,
    /// 流在第一个内容帧之前就结束了 → 空回答（当失败处理，触发换账号）
    Empty,
    /// 第一个内容帧之前撞上错误信封
    Fault(StreamFault),
}

/// 首包门核心：喂一串**已经解析好的** SSE 数据帧，判定能不能交给客户端。
///
/// 与 accio 的 `prefetch_stream_head` 同思路，但这里只做判定（不含网络），
/// 好让"额度耗尽必须给客户端 403 而不是 200 空"这条验收能离线跑。
pub fn head_verdict<'a>(data_frames: impl IntoIterator<Item = &'a str>) -> HeadVerdict {
    for data in data_frames {
        let data = data.trim();
        if data.is_empty() || data == DONE_SENTINEL {
            continue;
        }
        if let Some(fault) = stream_fault::stream_frame_fault(data) {
            return HeadVerdict::Fault(fault);
        }
        // 「有内容」的判据与聚合器一致（只有 reasoning 也算）
        let mut scratch = Aggregated::default();
        merge_chunk(&mut scratch, data);
        if scratch.has_content() {
            return HeadVerdict::Answered;
        }
    }
    HeadVerdict::Empty
}

fn short_id() -> String {
    let mut bytes = [0u8; 8];
    let _ = getrandom::getrandom(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    text.chars().take(limit).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::core::providers::codearts::credentials::{DpopKeyPair, Jwk, OAuthContext, PkcePair};

    fn credential() -> Credential {
        Credential {
            access_key_id: "HSTAPROBE0000000000".to_string(),
            secret_access_key: "secret-key-probe-000000000000000000".to_string(),
            security_token: "sts-probe+token/0000".to_string(),
            domain_id: "dom".to_string(),
            oauth_context: Some(OAuthContext {
                pkce_pair: PkcePair { code_verifier: "verifier".to_string(), ..PkcePair::default() },
                dpop_key_pair: DpopKeyPair {
                    private_key_jwk: Jwk { kty: "EC".into(), crv: "P-256".into(), d: "AQ".into(), ..Jwk::default() },
                    ..DpopKeyPair::default()
                },
            }),
            ..Credential::default()
        }
    }

    /// 2026-09-21 从真上游抓到的额度耗尽信封。
    const QUOTA_FRAME: &str = r#"{"error_code":"InferHub.4291.200","error_msg":"insufficient quota","details":[{"error_code":"InferHub.4291.200","error_msg":"modelId: glm-5.3-flash"}]}"#;
    const CONTENT_FRAME: &str = r#"{"id":"c1","model":"GLM-5.2","choices":[{"index":0,"delta":{"role":"assistant","content":"2"},"finish_reason":"stop"}]}"#;

    #[test]
    fn request_carries_the_model_headers_and_benefit_flag() {
        let profile = HeaderProfile::default();
        let (endpoint, headers, body) = build_upstream_request(
            "https://snap-access.cn-north-4.myhuaweicloud.com/",
            "GLM-5.2",
            json!({ "messages": [{"role": "user", "content": "hi"}] }),
            true,
            true,
            &profile,
            Some(&credential()),
        )
        .expect("应当能构造请求");
        assert_eq!("https://snap-access.cn-north-4.myhuaweicloud.com/api/v2/chat/completions", endpoint);
        let find = |name: &str| {
            headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.clone())
                .unwrap_or_default()
        };
        // 三个模型头都要在（少一个上游会按"没有模型"处理）
        assert_eq!("GLM-5.2", find("model-id"));
        assert_eq!("GLM-5.2", find("model-name"));
        assert_eq!("GLM-5.2", find("x-model-id"));
        assert_eq!("benefit", find("maas_type"), "福利模型要显式带 maas_type");
        assert_eq!("ChatAgent", find("Agent-Type"));
        assert_eq!("Vscode_26.9.101", find("client_version"));
        assert!(find("Authorization").starts_with("SDK-HMAC-SHA256 Access="), "必须签过名");
        // body：模型名与服务端收到的 stream 必须被写进去
        let sent: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!("GLM-5.2", sent["model"]);
        assert_eq!(true, sent["stream"]);
        assert_eq!(true, sent["stream_options"]["include_usage"], "流式要请求 usage");
    }

    #[test]
    fn non_benefit_requests_do_not_carry_maas_type() {
        let (_, headers, _) = build_upstream_request(
            "https://example.invalid",
            "GLM-5.2",
            json!({ "messages": [] }),
            false,
            false,
            &HeaderProfile::default(),
            None,
        )
        .unwrap();
        assert!(!headers.iter().any(|(key, _)| key.eq_ignore_ascii_case("maas_type")));
        // 无凭据时不签名（排障逃生口）
        assert!(!headers.iter().any(|(key, _)| key.eq_ignore_ascii_case("Authorization")));
    }

    #[test]
    fn request_rejects_a_body_without_messages() {
        let error = build_upstream_request(
            "https://example.invalid",
            "GLM-5.2",
            json!({ "model": "x" }),
            false,
            false,
            &HeaderProfile::default(),
            None,
        )
        .expect_err("没有 messages 就该本地报错");
        assert_eq!(400, error.status_code);
    }

    /// 普通回答：折叠成一条 completion。
    #[test]
    fn ordinary_stream_aggregates_into_a_completion() {
        let body = format!("data: {CONTENT_FRAME}\n\ndata: [DONE]\n\n");
        let completion = aggregate_sse(body.as_bytes(), "fallback").expect("应当能聚合");
        assert_eq!("2", completion["choices"][0]["message"]["content"]);
        assert_eq!("stop", completion["choices"][0]["finish_reason"]);
        assert_eq!("GLM-5.2", completion["model"], "上游给了 model 就用上游的");
    }

    /// **验收③**：额度耗尽必须变成带 403 的错误，绝不能折叠成 200 空回答。
    #[test]
    fn exhausted_allowance_aggregates_into_a_forbidden_error() {
        let body = format!("data: {QUOTA_FRAME}\n\ndata: [DONE]\n\n");
        let error = aggregate_sse(body.as_bytes(), "fallback")
            .expect_err("额度耗尽绝不能变成一次成功的空回答");
        assert_eq!(403, error.status_code, "403 才会让编排层换账号");
        assert!(error.message.contains("insufficient quota"));
    }

    /// **验收②**：只吐 reasoning 没有正文时，要当"有内容"而不是空回答。
    #[test]
    fn reasoning_only_replies_count_as_content() {
        let frame = r#"{"id":"c2","choices":[{"index":0,"delta":{"reasoning_content":"让我想想"},"finish_reason":"length"}]}"#;
        let body = format!("data: {frame}\n\ndata: [DONE]\n\n");
        let completion = aggregate_sse(body.as_bytes(), "m").expect("只有 reasoning 也是成功");
        assert_eq!("", completion["choices"][0]["message"]["content"]);
        assert_eq!("让我想想", completion["choices"][0]["message"]["reasoning_content"]);
        assert_eq!("length", completion["choices"][0]["finish_reason"]);
    }

    /// 干净结束但一个字都没有 → 空回答**失败**（这是跨账号降级的触发条件）。
    #[test]
    fn a_clean_but_empty_stream_is_a_failure() {
        let error = aggregate_sse(b"data: [DONE]\n\n", "m").expect_err("空回答必须当失败");
        assert_eq!(502, error.status_code);
        let error = aggregate_sse(b"", "m").expect_err("空体也必须当失败");
        assert_eq!(502, error.status_code);
        assert!(error.message.contains("没有返回任何内容"));
    }

    /// tool_calls 的分片要按 index 合并、arguments 要拼接。
    #[test]
    fn streamed_tool_calls_are_merged_by_index() {
        let first = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"get_weather","arguments":"{\"ci"}}]}}]}"#;
        let second = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"ty\":\"SF\"}"}}]},"finish_reason":"tool_calls"}]}"#;
        let body = format!("data: {first}\n\ndata: {second}\n\ndata: [DONE]\n\n");
        let completion = aggregate_sse(body.as_bytes(), "m").expect("应当能聚合");
        let call = &completion["choices"][0]["message"]["tool_calls"][0];
        assert_eq!("call_1", call["id"]);
        assert_eq!("function", call["type"]);
        assert_eq!("get_weather", call["function"]["name"]);
        assert_eq!("{\"city\":\"SF\"}", call["function"]["arguments"], "参数分片必须拼接");
    }

    /// 首包门：三种结局都要分得清。
    #[test]
    fn head_gate_distinguishes_answered_fault_and_empty() {
        assert_eq!(HeadVerdict::Answered, head_verdict(["", CONTENT_FRAME]));
        assert_eq!(HeadVerdict::Empty, head_verdict(["", "[DONE]"]));
        match head_verdict([QUOTA_FRAME]) {
            HeadVerdict::Fault(fault) => {
                assert_eq!(403, fault.status, "首包门撞上额度耗尽时也要带上 403")
            }
            other => panic!("额度信封应当判成 Fault，得到 {other:?}"),
        }
        // 只有 reasoning 的帧也算"答了"（否则会误判成空回答）
        let reasoning = r#"{"choices":[{"delta":{"reasoning_content":"想"}}]}"#;
        assert_eq!(HeadVerdict::Answered, head_verdict([reasoning]));
    }

    /// 上游非 2xx：保留原始状态码，诊断体要脱敏。
    #[test]
    fn upstream_http_errors_keep_the_status_and_redact_the_body() {
        let credential = credential();
        let body = format!(
            "{{\"secret_access_key\":\"{}\",\"access_key_id\":\"{}\"}}",
            credential.secret_access_key, credential.access_key_id
        );
        let error = upstream_http_error(400, body.as_bytes(), &credential, false);
        assert_eq!(400, error.status_code, "上游的 400 不能被改成 502");
        assert!(!error.message.contains(&credential.secret_access_key), "诊断体必须脱敏");
        assert!(!error.message.contains(&credential.access_key_id));
        assert!(error.message.contains("[REDACTED]"));
        // URL 转义的秘密（STS 里带 + 与 /）也要盖住
        let escaped = error.message.contains(&credential.security_token);
        assert!(!escaped);
    }

    #[test]
    fn truncated_error_bodies_say_so() {
        let error = upstream_http_error(500, b"{\"a\":", &credential(), true);
        assert!(error.message.contains("诊断体已截断"));
    }

    /// 打一次**真**对话端点，验证 chat 这条路的请求形状（头集合 + body 签名）。
    ///
    /// 用**一个不存在的模型名**，所以上游会在路由阶段就拒掉、不会真的推理 ——
    /// 这是「零消耗」的关键：一个没注册的模型不可能被调用。
    ///
    /// 为什么值得单独验：chat 路与目录路不是同一套头（`ChatAgent` vs
    /// `PromptCenter`，外加三个 `model-*` 头），而且**body 参与签名**
    /// （`sha256(body)` 进规范请求串）。如果 body 的字节与发出去的不一致，
    /// 拿到的会是签名错误而不是"模型没注册"——这条探针正好把两者分开。
    ///
    /// 默认不跑，手工开：
    /// ```bash
    /// CODEARTS_AK=… CODEARTS_SK=… CODEARTS_STS=… CODEARTS_DOMAIN=… \
    ///   cargo test -p agent2api-server codearts -- --ignored --nocapture
    /// ```
    #[tokio::test]
    #[ignore]
    async fn live_chat_endpoint_accepts_the_signed_request_shape() {
        let mut given = 0;
        let mut pick = |key: &str| {
            let value = std::env::var(key).unwrap_or_default();
            if value.is_empty() { value } else { given += 1; value }
        };
        let credential = Credential {
            access_key_id: pick("CODEARTS_AK"),
            secret_access_key: pick("CODEARTS_SK"),
            security_token: pick("CODEARTS_STS"),
            domain_id: pick("CODEARTS_DOMAIN"),
            ..Credential::default()
        };
        if given < 4 {
            println!("跳过：四个 CODEARTS_* 环境变量没给齐（只给了 {given} 个）");
            return;
        }
        // 一个绝不存在的模型名 —— 上游在路由阶段就会拒，不会产生任何推理消耗
        let (endpoint, headers, body) = build_upstream_request(
            "https://snap-access.cn-north-4.myhuaweicloud.com",
            "codearts-port-probe-not-a-real-model",
            json!({ "messages": [{ "role": "user", "content": "probe" }] }),
            false,
            false,
            &HeaderProfile::default(),
            Some(&credential),
        )
        .expect("应当能构造请求");
        let mut request = reqwest::Client::new()
            .post(&endpoint)
            .timeout(std::time::Duration::from_secs(30));
        for (name, value) in headers {
            request = request.header(name.as_str(), value.as_str());
        }
        let response = request.body(body).send().await.expect("请求发不出去（网络或代理）");
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();
        println!("HTTP {status}\n{}", text.chars().take(400).collect::<String>());
        assert!(
            text.contains("002002009") || text.contains("not registered"),
            "期望上游说「模型没注册」—— 若报的是签名/DPoP/缺字段，说明请求形状不对：HTTP {status} {text}"
        );
    }

    /// **尺寸墙 × 请求侧 gzip 对照探针**（零推理消耗，2026-10-08）。
    ///
    /// 要分开的是两件事：APIG 的 413（`APIG.0201` Request entity too large）量的到底是
    /// **线上字节**还是**解压后的字节**。前一发裸 body，后一发同一份 body 压成 gzip 再签一次名
    /// （`X-Sdk-Content-Sha256` 按压缩后的字节算），其余头与签名流程逐字相同 —— 两发只差编码，
    /// 所以任何差异都归给编码这一层。
    ///
    /// 模型名是**不存在**的，上游在路由阶段就拒 ⇒ 不产生任何推理消耗，所以敢发 3 MB 的体
    /// （这是本条探针成立的前提，别换成真模型名，那会按整份上下文计费）。
    ///
    /// 读法：
    ///   · 裸的 413、gzip 的回「模型没注册」⇒ 量线字节 ⇒ 请求侧压缩是有效的扩容手段；
    ///   · 两发都 413 ⇒ 量解压后的字节 ⇒ 压缩白做，只能削体或换家；
    ///   · gzip 那发回签名/鉴权错（`SignatureDoesNotMatch`、`APIG.0301`）⇒ 它按**解压后的体**
    ///     重算摘要，与我们签的压缩字节对不上 ⇒ 要走这条路得先摸清它的摘要口径；
    ///   · 裸的那发就回了「模型没注册」⇒ 还没到墙，把 `CODEARTS_PROBE_MB` 加大重跑。
    ///
    /// 用法（四个凭据与邻居探针同源；STS 短寿命，过期会先看到 401 而不是尺寸结论）：
    /// ```bash
    /// CODEARTS_AK=… CODEARTS_SK=… CODEARTS_STS=… CODEARTS_DOMAIN=… \
    ///   CODEARTS_PROBE_MB=3 cargo test -p agent2api-server size_wall -- --ignored --nocapture
    /// ```
    #[tokio::test]
    #[ignore]
    async fn live_size_wall_compares_plain_and_gzip_request_bodies() {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use std::io::Write;

        let mut given = 0;
        let mut pick = |key: &str| {
            let value = std::env::var(key).unwrap_or_default();
            if value.is_empty() { value } else { given += 1; value }
        };
        let credential = Credential {
            access_key_id: pick("CODEARTS_AK"),
            secret_access_key: pick("CODEARTS_SK"),
            security_token: pick("CODEARTS_STS"),
            domain_id: pick("CODEARTS_DOMAIN"),
            ..Credential::default()
        };
        if given < 4 {
            println!("跳过：四个 CODEARTS_* 环境变量没给齐（只给了 {given} 个）");
            return;
        }
        let megabytes: usize = std::env::var("CODEARTS_PROBE_MB")
            .ok()
            .and_then(|value| value.trim().parse().ok())
            .filter(|value: &usize| (1..=16).contains(value))
            .unwrap_or(3);

        // 填充用「像代码的文本」而不是随机字节：随机数据压不动，会得出「压缩没用」的假结论
        let line = "    let router = build_router(state.clone()).await?; // 装配选路与限额\n";
        let mut content = String::with_capacity(megabytes * 1024 * 1024);
        while content.len() < megabytes * 1024 * 1024 {
            content.push_str(line);
        }
        let payload = json!({ "messages": [{ "role": "user", "content": content }] });
        let (endpoint, headers, raw) = build_upstream_request(
            "https://snap-access.cn-north-4.myhuaweicloud.com",
            "codearts-size-wall-not-a-real-model",
            payload,
            false,
            false,
            &HeaderProfile::default(),
            Some(&credential),
        )
        .expect("应当能构造请求");

        let mut encoder = GzEncoder::new(Vec::new(), Compression::best());
        encoder.write_all(&raw).expect("压缩写入不该失败");
        let gzipped = encoder.finish().expect("gzip 收尾不该失败");
        println!(
            "体积：裸 {} 字节 → gzip {} 字节（压缩比 {:.2}）",
            raw.len(),
            gzipped.len(),
            raw.len() as f64 / gzipped.len().max(1) as f64
        );

        // 重签：摘掉上一轮的签名头与摘要头，把压缩后的字节交给同一个签名器
        let mut gzip_headers: Vec<(String, String)> = headers
            .iter()
            .filter(|(name, _)| {
                !matches!(
                    name.as_str(),
                    "Authorization" | "X-Sdk-Content-Sha256" | "x-sdk-content-sha256"
                )
            })
            .cloned()
            .collect();
        gzip_headers.push(("Content-Encoding".to_string(), "gzip".to_string()));
        let gzip_signed = signer::sign(
            "POST",
            &endpoint,
            &gzip_headers,
            &gzipped,
            &signer_credential(&credential),
            false,
        )
        .expect("gzip 这一发应当能签名");

        let client = reqwest::Client::new();
        let mut shots = Vec::new();
        for (label, headers, body) in [
            ("A 裸 body", headers.clone(), raw.clone()),
            ("B gzip body", gzip_signed, gzipped.clone()),
        ] {
            let mut request = client.post(&endpoint).timeout(std::time::Duration::from_secs(40));
            for (name, value) in headers {
                request = request.header(name.as_str(), value.as_str());
            }
            let outcome = match request.body(body).send().await {
                Ok(response) => {
                    let status = response.status().as_u16();
                    let text = response.text().await.unwrap_or_default();
                    format!("HTTP {status} :: {}", text.chars().take(220).collect::<String>())
                }
                Err(error) => format!("发送失败 :: {error}"),
            };
            println!("{label} → {outcome}");
            shots.push(outcome);
        }
        assert_eq!(shots.len(), 2, "两发都应当拿到上游的答复（否则本探针没有对照）");
        println!(
            "结论读法：A 是 413 而 B 是 not-registered ⇒ 墙量线字节；两发都 413 ⇒ 墙量解压后的字节；\
             B 报签名/鉴权 ⇒ 上游按解压后的体重算摘要。"
        );
    }

    /// **端到端交接探针**：真凭据走完「刷新 → 用新材料 → 真对话」整条链。
    ///
    /// 这就是 §10.3 / §12 里一直卡着的两个验收（M1 的"到期前自动换新"与
    /// M2 的"正常出字"）。2026-09-27 经用户同意，把 CPA 手里的一条 codearts
    /// 凭据停掉归本项目专用，refresh_token 从此可以放心轮换。
    ///
    /// 用法（整份凭据从**停用的 auth 文件**读，不要手抄或只挑四个字段 ——
    /// 刷新必须要 oauth_context 里的 PKCE verifier 与 DPoP 私钥）：
    /// ```bash
    /// CODEARTS_CREDENTIAL_FILE=/path/to/parked.json \
    /// CODEARTS_SAVE_TO=/path/to/parked.json \
    ///   cargo test -p agent2api-server codearts -- --ignored --nocapture
    /// ```
    ///
    /// **`CODEARTS_SAVE_TO` 不是可选项**：刷新会轮换 refresh_token，旧串用一次
    /// 就烧掉 —— 不落盘，这条账号就死了。落盘形状与 CPA 的 auth 文件一致
    /// （`codearts_provider_credential` 包一层），随时可以搬回任何一边。
    ///
    /// 消耗：一次 `GET /v1/model/builtin`（只读）+ 一次小对话
    /// （GLM-5.2，`max_tokens=512`，约几分钱额度）。
    #[tokio::test]
    #[ignore]
    async fn live_full_chain_refresh_then_chat() {
        let path = std::env::var("CODEARTS_CREDENTIAL_FILE").unwrap_or_default();
        if path.is_empty() {
            println!("跳过：未设 CODEARTS_CREDENTIAL_FILE");
            return;
        }
        let raw = std::fs::read(&path).expect("凭据文件读不到");
        let original = Credential::from_payload(
            &serde_json::from_slice::<serde_json::Value>(&raw).expect("凭据文件不是 JSON"),
        )
        .expect("凭据文件解析失败");
        assert!(original.can_refresh(), "这份凭据没有完整的 oauth_context，刷不了");

        // ① 基线：现有材料还能签出 200 的只读目录
        let catalog = "https://snap-access.cn-north-4.myhuaweicloud.com/v1/model/builtin";
        // 基线**不做断言**：临期与已过期正是这条路径要处理的常态 —— 拿"刷新前
        // 材料必须可用"当断言，等于在最该测的场景（凭据已过期）下先把测试弄崩。
        let status = signed_catalog_status(catalog, &original).await;
        println!("① 刷新前目录：HTTP {status}（200 = 材料尚可用；401/400 = 已过期，正是要刷的场合）");

        // ② 真刷新（M1 的验收）：换一套新的 AK/SK/STS，refresh_token 可能轮换
        let fresh = super::super::oauth::refresh_credential(&original, None)
            .await
            .expect("刷新失败 —— 检查 oauth_context 是否完整");
        println!(
            "② 刷新成功：AK {}… → {}…，到期 {} → {}，refresh_token 轮换：{}",
            &original.access_key_id[..8.min(original.access_key_id.len())],
            &fresh.access_key_id[..8.min(fresh.access_key_id.len())],
            original.expires_at,
            fresh.expires_at,
            fresh.refresh_token != original.refresh_token
        );
        assert!(fresh.valid() && !fresh.security_token.is_empty());
        assert!(
            fresh.expires_at_ms().unwrap_or(0) > original.expires_at_ms().unwrap_or(i64::MAX),
            "刷新后的到期时刻应当更晚"
        );
        assert!(fresh.can_refresh(), "刷新后的凭据必须还能再刷（oauth_context 要原样保留）");
        // **先落盘再继续**：后面任何一步失败，都不能丢掉这份新 refresh_token
        if let Ok(path) = std::env::var("CODEARTS_SAVE_TO") {
            let payload = json!({ "codearts_provider_credential": fresh.to_value() });
            std::fs::write(&path, serde_json::to_vec_pretty(&payload).unwrap())
                .expect("刷新后的凭据落盘失败 —— 旧 refresh_token 已经烧掉，必须拿到这份新文件");
            println!("   已把刷新后的凭据写到 {path}");
        } else {
            println!("   ⚠️ 未设 CODEARTS_SAVE_TO，刷新后的凭据只在本次进程里 —— 旧串已烧，请立刻重跑并落盘");
        }

        // ③ 新材料能签出 200（证明换证真的有效，不只是响应解析对了）
        let status = signed_catalog_status(catalog, &fresh).await;
        println!("③ 刷新后目录：HTTP {status}");
        assert_eq!(200, status, "刷新后的材料应当可用");

        // ④ 真对话（M2 的验收「正常出字」）：GLM-5.2、流式、小预算
        let (endpoint, headers, body) = build_upstream_request(
            "https://snap-access.cn-north-4.myhuaweicloud.com",
            "GLM-5.2",
            json!({
                "messages": [{ "role": "user", "content": "只回答七个字：一加一等于几？" }],
                "max_tokens": 512
            }),
            true,
            false,
            &HeaderProfile::default(),
            Some(&fresh),
        )
        .expect("应当能构造请求");
        let mut request = reqwest::Client::new()
            .post(&endpoint)
            .timeout(std::time::Duration::from_secs(120));
        for (name, value) in headers {
            request = request.header(name.as_str(), value.as_str());
        }
        let response = request.body(body).send().await.expect("对话请求发不出去");
        let status = response.status().as_u16();
        let text = response.text().await.expect("读不到对话响应体");
        println!("④ 对话：HTTP {status}，{} 字节", text.len());
        assert_eq!(200, status, "对话被拒：{}", text.chars().take(400).collect::<String>());

        // 首包门在真流上走一遍：要么答了，要么是带分类的故障（不该是空回答）
        let frames: Vec<&str> = text
            .split("\n\n")
            .filter_map(|chunk| chunk.strip_prefix("data:"))
            .collect();
        match head_verdict(frames.iter().copied()) {
            HeadVerdict::Answered => println!("   首包门：有内容"),
            HeadVerdict::Fault(fault) => panic!("首包门判成故障（{fault:?}）—— 这条用例预期是成功对话"),
            HeadVerdict::Empty => panic!("真对话居然是空回答 —— 这正是 M2 要拦的情形"),
        }
        let completion = aggregate_sse(text.as_bytes(), "GLM-5.2").expect("应当能折叠出回答");
        let content = completion["choices"][0]["message"]["content"].as_str().unwrap_or("");
        let reasoning = completion["choices"][0]["message"]["reasoning_content"].as_str().unwrap_or("");
        println!(
            "   折叠结果：content {} 字 / reasoning {} 字 / finish_reason {} / usage {}",
            content.chars().count(),
            reasoning.chars().count(),
            completion["choices"][0]["finish_reason"],
            if completion["usage"].is_null() { "无" } else { "有" }
        );
        assert!(
            !content.is_empty() || !reasoning.is_empty(),
            "既没正文也没思考段：{completion}"
        );
    }

    /// 签一个只读目录请求并返回状态码（交接探针的 ①③ 两步共用）。
    async fn signed_catalog_status(catalog: &str, credential: &Credential) -> u16 {
        let headers = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Accept".to_string(), "application/json".to_string()),
            ("Agent-Type".to_string(), "PromptCenter".to_string()),
            ("X-Language".to_string(), "en-us".to_string()),
            ("plugin-name".to_string(), "snap_vscode".to_string()),
            ("plugin-version".to_string(), "26.9.101".to_string()),
            ("client_version".to_string(), "Vscode_26.9.101".to_string()),
            ("is_confidential".to_string(), "false".to_string()),
        ];
        let signed = super::super::signer::sign("GET", catalog, &headers, b"", &super::super::oauth::signer_credential(credential), false)
            .expect("签名应当成功");
        let mut request = reqwest::Client::new()
            .get(catalog)
            .timeout(std::time::Duration::from_secs(30));
        for (name, value) in signed {
            request = request.header(name.as_str(), value.as_str());
        }
        match request.send().await {
            Ok(response) => response.status().as_u16(),
            Err(error) => panic!("目录请求发不出去：{}", crate::server::core::egress::describe_error_detail(&error)),
        }
    }
    /// 尺寸判据：两道墙的**原文**都要认，两种"看着像但不是"的都不能认。
    /// 每段响应体都是 2026-10-09 那发零计费探针从上游原样拿回的。
    #[test]
    fn both_size_walls_are_recognized_from_their_own_copy() {
        let parse = br#"{"error_code":"InferHub.001001005.400","error_msg":"The request param is invalid, Please check it","details":[{"error_code":"PARSE_REQUEST_DATA_EXCEPTION","error_msg":"PARSE_REQUEST_DATA_EXCEPTION"}]}"#;
        assert!(
            size_rejection(400, parse).is_some(),
            "6 MB 级那发必须被认成尺寸拒绝，否则每轮都要白打一次"
        );
        let entity = br#"{"error_msg":"Request entity too large","error_code":"APIG.0201","request_id":"2464cdca99b2e672aff3df71978de6d8"}"#;
        assert!(size_rejection(413, entity).is_some());
        // 两句原因分别指向两道墙（面板与日志靠它分清「该削体」还是「该换家」）
        assert!(size_rejection(400, parse).unwrap().contains("解析"));
        assert!(size_rejection(413, entity).unwrap().contains("网关"));
    }

    /// 反对照：状态码像、但**不是体积问题**的形状不许登记成尺寸门。
    #[test]
    fn lookalikes_are_not_size_rejections() {
        assert!(size_rejection(413, br#"{"error_msg":"something else"}"#).is_none());
        // 同码不同面孔（APIG.0201 也用于 414/494 之类）⇒ 只凭码会连坐
        assert!(
            size_rejection(413, br#"{"error_code":"APIG.0201","error_msg":"too many headers"}"#)
                .is_none(),
            "判据取文案，不取错误码"
        );
        let unregistered = br#"{"error_code":"InferHub.002002009.404","error_msg":"The model is not registered, please request other model","details":[{"error_code":"InferHub.002002009.404"}]}"#;
        assert!(size_rejection(404, unregistered).is_none());
        assert!(
            size_rejection(
                400,
                br#"{"text":"[DONE]","error_code":"TM.00001001","error_msg":"request body is invalid JSON"}"#
            )
            .is_none()
        );
    }

    /// 真实帧的形状：标记**只**出现在 `details[].error_code`，顶层没有尺寸字样。
    ///
    /// 这条是第二次改动的原因 —— 同一句体积错，**经网关这一路回的是 HTTP 502**
    /// （探针直发时是 400），把判据锁在状态码上就等于漏登记：dev 上连打两发
    /// 8 MB，两发都完整发了出去，日志里一行「尺寸门已登记」都没有。
    #[test]
    fn the_size_marker_survives_whatever_status_upstream_picks() {
        let frame = r#"{"error_code":"InferHub.001001005.400","error_msg":"The request param is invalid, Please check it","details":[{"error_code":"PARSE_REQUEST_DATA_EXCEPTION"}]}"#;
        assert!(size_rejection_text(frame).is_some());
        // 状态无关：400（探针直发）与 502（经网关实测）都要认
        assert!(size_rejection(400, frame.as_bytes()).is_some());
        assert!(
            size_rejection(502, frame.as_bytes()).is_some(),
            "上游把同一句体积错放在 502 上也必须认"
        );
        // 同一段文字里没有那个标记就不认（`InferHub.…400` 本身不构成尺寸结论）
        assert!(size_rejection_text(r#"{"error_code":"InferHub.001001005.400"}"#).is_none());
    }

    /// 丢历史思考：两种形状都清、**条数一条不少**、user 轮不碰、没思考时一字节都不改。
    #[test]
    fn historical_reasoning_is_stripped_without_changing_the_shape() {
        // Anthropic 形状：content 数组里的 thinking 块
        let mut body = json!({"messages": [
            {"role": "user", "content": [
                {"type": "text", "text": "看这个"},
                {"type": "thinking", "thinking": "这行不属于我们该改的用户输入"},
            ]},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "先读文件", "signature": "sig-1"},
                {"type": "text", "text": "答案是 42"},
                {"type": "tool_use", "id": "t1", "name": "read", "input": {"p": "a.rs"}},
            ]},
            {"role": "assistant", "content": [
                {"type": "redacted_thinking", "data": "AAAA"},
            ]},
        ]});
        // 2 而不是 3：user 轮那条 thinking **故意不碰**（那是用户给的内容，
        // 我们无权改写；思考只可能出现在 assistant 轮，那种输入本来就是异常的）
        assert_eq!(2, strip_historical_reasoning(&mut body));
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(3, messages.len(), "条数必须一条不少：条数是别家（catpaw）的主变量");
        assert_eq!(2, messages[0]["content"].as_array().unwrap().len(), "user 轮不碰");
        let assistant = messages[1]["content"].as_array().unwrap();
        assert_eq!(2, assistant.len());
        assert_eq!(assistant[0]["text"], "答案是 42");
        assert_eq!(assistant[1]["name"], "read", "tool_use 必须原样留着");
        assert_eq!(messages[2]["content"], "", "只剩思考的那条改成空内容，不删整条");

        // OpenAI 形状：思考挂在消息字段上
        let mut openai = json!({"messages": [
            {"role": "assistant", "content": "答", "reasoning_content": "想过", "reasoning": "也想过"},
        ]});
        assert_eq!(2, strip_historical_reasoning(&mut openai));
        assert!(openai["messages"][0].get("reasoning_content").is_none());
        assert!(openai["messages"][0].get("reasoning").is_none());
        assert_eq!(openai["messages"][0]["content"], "答");

        // 对照组：没有思考 ⇒ 序列化结果逐字节不变（不然"省体积"其实是偷偷改内容）
        let clean = json!({"messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "yo"},
        ]});
        let mut same = clean.clone();
        assert_eq!(0, strip_historical_reasoning(&mut same));
        assert_eq!(
            serde_json::to_string(&clean).unwrap(),
            serde_json::to_string(&same).unwrap()
        );

        // 残缺形状不炸（release 是 panic=abort）
        assert_eq!(0, strip_historical_reasoning(&mut json!({})));
        assert_eq!(0, strip_historical_reasoning(&mut json!({"messages": "x"})));
        assert_eq!(
            0,
            strip_historical_reasoning(&mut json!({"messages": [
                "not-an-object",
                {"role": "assistant", "content": null}
            ]}))
        );
    }

    /// 保留开关的判据（拆成纯函数只为能测：进程 env 是全局的，会污染并发用例）。
    #[test]
    fn the_retain_switch_only_accepts_the_three_documented_values() {
        for value in ["1", "true", "yes"] {
            assert!(keep_historical_reasoning_from(Some(value)), "{value} 应当认");
        }
        for value in ["0", "", "no", "TRUE", "Yes"] {
            assert!(!keep_historical_reasoning_from(Some(value)), "{value} 不该开");
        }
        assert!(!keep_historical_reasoning_from(None), "没设 env = 默认丢");
    }

    /// 第二条判据（体积相关性）：**大体才贴这个码，小体撞上它是真参数错**。
    ///
    /// 反对照就是这条的全部价值 —— 没有它，任何一次普通的参数错都会把这一家按体积
    /// 关在门外 120 秒；有了它，只有「我们确知这发已越过实测上界」时才记门。
    #[test]
    fn the_generic_param_error_is_a_size_failure_only_above_the_ceiling() {
        let generic = r#"{"error_code":"InferHub.001001005.400","error_msg":"The request param is invalid, Please check it"}"#;
        let above = SIZE_CEILING_KNOWN_BYTES + 1;
        assert!(
            size_rejection_by_shape(generic, above).is_some(),
            "越界 + 这个码 ⇒ 按尺寸失败"
        );
        assert!(
            size_rejection_by_shape(generic, SIZE_CEILING_KNOWN_BYTES).is_none(),
            "刚好在上界以内不记门"
        );
        assert!(
            size_rejection_by_shape(generic, 155).is_none(),
            "小体撞上同一个码是参数错，不是体积"
        );
        // 越界但错误码无关 ⇒ 也不记（额度那种码别想关这一家的门）
        assert!(
            size_rejection_by_shape(
                r#"{"error_code":"InferHub.4291.200","error_msg":"insufficient quota"}"#,
                above
            )
            .is_none()
        );
        // 合成入口：标记版命中时不需要体积；两条都不命中才返回 None
        let marked = r#"{"error_code":"InferHub.001001005.400","details":[{"error_code":"PARSE_REQUEST_DATA_EXCEPTION"}]}"#;
        assert!(size_rejection_any(0, marked, 1_000).is_some());
        assert!(
            size_rejection_any(413, r#"{"error_msg":"Request entity too large"}"#, 1_000).is_some()
        );
        assert!(size_rejection_any(0, r#"{"error_code":"InferHub.002002009.404"}"#, 1_000).is_none());
        assert!(size_rejection_any(0, generic, 1_000).is_none());
        assert!(size_rejection_any(0, generic, above).is_some());
    }

}

/// 首包门在「拿到内容」之前最多缓冲多少字节。
const HEAD_BUFFER_LIMIT: usize = 64 * 1024;

/// 网络侧的首包门：把上游流拉到**第一个有内容的帧**为止。
///
/// 返回 `(已经读到的字节, 剩下的字节流)` —— 已读的那段要原样补给客户端（其中
/// 包含首个内容帧），所以这里一个字节都不丢。
///
/// 三条出口，与 [`head_verdict`] 的判定完全一致：
///   * 撞到流内错误信封 → `Err`（带 403/429/502 状态码，编排层据此换账号）
///   * 流干净结束却一个字都没答 → `Err`（空回答**当失败**，否则永远不会换号）
///   * 拿到内容 → 交回可继续读的流
///
/// 上游这里已经是 OpenAI chunk 形状，所以拿到内容之后是**透传**，不做二次翻译
/// （与 qoder/accio 不同，那两家的帧要重写）。
pub async fn prefetch_head(
    response: reqwest::Response,
) -> Result<
    (
        Vec<u8>,
        futures::stream::BoxStream<'static, Result<bytes::Bytes, std::io::Error>>,
    ),
    GatewayError,
> {
    use futures::StreamExt;
    let mut source = response.bytes_stream();
    let mut seen: Vec<u8> = Vec::new();
    let mut text = String::new();
    loop {
        // 先看已缓冲的字节里有没有完整帧（一次 read 可能带来好几帧）
        let frames: Vec<&str> = extract_data_lines(&text);
        match head_verdict(frames.iter().copied()) {
            HeadVerdict::Answered => {
                let rest = source.map(|item| {
                    item.map_err(|error| std::io::Error::other(egress::describe_error_detail(&error)))
                });
                // `seen` 里已经是"读到的全部字节"，直接交回 —— **不能再追加 text**
                // （那样会把首包之前的内容整体重发一遍；客户端会看到重复的字）
                return Ok((seen, Box::pin(rest)));
            }
            HeadVerdict::Fault(fault) => return Err(stream_fault::fault_to_error(&fault)),
            HeadVerdict::Empty => {}
        }
        match source.next().await {
            None => {
                // 流结束仍没有内容：先对尾部再判一次（最后一帧可能没有换行结尾）
                let frames: Vec<&str> = extract_data_lines(&text);
                return match head_verdict(frames.iter().copied()) {
                    HeadVerdict::Fault(fault) => Err(stream_fault::fault_to_error(&fault)),
                    _ => Err(GatewayError::with_status(
                        502,
                        "CodeArts 上游流结束但未返回任何内容（空回答按失败处理，以便换账号重试）",
                    )),
                };
            }
            Some(Err(error)) => {
                return Err(GatewayError::with_status(
                    502,
                    format!("CodeArts 上游流中断：{}", egress::describe_error_detail(&error)),
                ))
            }
            Some(Ok(chunk)) => {
                seen.extend_from_slice(&chunk);
                text.push_str(&String::from_utf8_lossy(&chunk));
                // 上游可以在「一个字都不答」的前提下无限发元数据（思考帧、心跳注释行、
                // 或者干脆是坏掉的服务）。参考实现为此留了一条 64 KiB 的硬顶，
                // 我第一版没搬 —— 于是首包门变成了一个**由上游决定大小**的缓冲区。
                if seen.len() > HEAD_BUFFER_LIMIT {
                    return Err(GatewayError::with_status(
                        502,
                        format!(
                            "CodeArts 上游在给出内容前已发送超过 {} KiB 的元数据（按失败处理，避免无界缓冲）",
                            HEAD_BUFFER_LIMIT / 1024
                        ),
                    ));
                }
            }
        }
    }
}

/// 从一段（可能不完整的）SSE 文本里取出所有 `data:` 帧载荷。
///
/// 只认**成对换行结束**的帧，尾部没结束的那行留给下一次读 —— 否则会把半截 JSON
/// 当成一帧，`head_verdict` 判成"无内容"从而漏发首包。
fn extract_data_lines(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    for block in text.split("\n\n") {
        for line in block.lines() {
            if let Some(data) = line.strip_prefix("data:") {
                out.push(data);
            }
        }
    }
    out
}

#[cfg(test)]
mod head_gate_tests {
    //! 首包门的三条出口用**进程内 mock 上游**跑真流（与 session.rs 同一手法）。
    use super::{aggregate_sse, extract_data_lines, prefetch_head, HeadVerdict, head_verdict};
    use futures::StreamExt;

    /// 2026-09-21 从真上游抓到的额度信封（与 `stream_fault` 的金向量同一份）。
    const QUOTA_FRAME: &str = r#"{"error_code":"InferHub.4291.200","error_msg":"insufficient quota","details":[{"error_msg":"requestId: abe754a5417344caa306d5b38a57923b"}]}"#;

    /// mock SSE 上游：`body` 用 String 是为了让测试能把拼接出来的响应体交进去
    async fn mock_sse(body: String) -> String {
        use axum::http::{header, StatusCode};
        use axum::response::IntoResponse;
        let app = axum::Router::new().route(
            "/sse",
            axum::routing::get(move || async move {
                (StatusCode::OK, [(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}/sse")
    }

    async fn open(url: &str) -> reqwest::Response {
        reqwest::Client::new().get(url).send().await.expect("mock 上游应当可达")
    }

    #[tokio::test]
    async fn head_gate_passes_through_a_real_answer_and_keeps_every_byte() {
        // 每帧一行、行首就顶格（SSE 规范如此）—— 用 concat! 而不是反斜杠续行，
        // 否则续行的缩进会混进载荷，`extract_data_lines` 认不出 `data:` 前缀
        let body = String::from(concat!(
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"你\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"好\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        ));
        let url = mock_sse(body).await;
        let (prefetched, mut rest) = match prefetch_head(open(&url).await).await {
            Ok(pair) => pair,
            Err(error) => panic!("有内容时应当放行，却报错：{}", error.message),
        };
        let mut all = prefetched;
        while let Some(item) = rest.next().await {
            all.extend_from_slice(&item.expect("剩余流应当可读"));
        }
        let text = String::from_utf8_lossy(&all).to_string();
        assert!(text.contains('你') && text.contains('好') && text.contains("[DONE]"), "透传丢了字节：{text}");
        let completion = aggregate_sse(&all, "GLM-5.2").expect("整段应当能折叠");
        assert_eq!("你好", completion["choices"][0]["message"]["content"]);
    }

    #[tokio::test]
    async fn head_gate_turns_an_instream_quota_envelope_into_403() {
        let body = format!("data: {QUOTA_FRAME}\n\ndata: [DONE]\n\n");
        let url = mock_sse(body).await;
        let error = match prefetch_head(open(&url).await).await {
            Err(error) => error,
            Ok(_) => panic!("额度信封必须判成失败，否则客户端会收到 200 空回答"),
        };
        assert_eq!(403, error.status_code, "403 才会让编排层换账号");
    }

    #[tokio::test]
    async fn head_gate_reports_a_clean_but_empty_stream_as_failure() {
        let url = mock_sse("data: [DONE]\n\n".to_string()).await;
        let error = match prefetch_head(open(&url).await).await {
            Err(error) => error,
            Ok(_) => panic!("空回答必须当失败，否则永远不会换账号"),
        };
        assert_eq!(502, error.status_code);
        assert!(error.message.contains("空回答"), "文案要说明为什么：{}", error.message);
    }

    #[test]
    fn verdict_and_frame_extraction_agree_on_unterminated_tails() {
        // 成对换行结尾的两帧
        assert_eq!(2, extract_data_lines("data: {\"a\":1}\n\ndata: {\"b\":2}\n\n").len());
        // 尾部没有成对换行的那一帧也取得到（JSON 本身完整，判"有无内容"不会误判）
        assert_eq!(2, extract_data_lines("data: {\"a\":1}\n\ndata: {\"b\":2}").len());
        // 半截 JSON 判不出内容 → 空回答（继续等下一块，不会误放行）
        assert_eq!(HeadVerdict::Empty, head_verdict(["{\"choices\":[{\"delta\":{\"content\":\"hu"]));
    }
}

#[cfg(test)]
mod catalog_fixtures {
    //! 把目录三源的真实响应抓成 fixtures（全部只读、零推理消耗），
    //! 供 `models.rs` 的解析与合并逻辑离线测试。手工跑：
    //! ```bash
    //! CODEARTS_CREDENTIAL_FILE=<停用的auth文件> CODEARTS_FIXTURES_OUT=<目录> \
    //!   cargo test -p agent2api-server codearts -- --ignored --nocapture
    //! ```
    use super::*;
    use crate::server::core::providers::codearts::oauth::signer_credential;
    use crate::server::core::providers::codearts::signer;

    async fn fetch(base: &str, path: &str, agent_type: &str, credential: &Credential, host_signed: bool, domainless: bool) -> (u16, String) {
        let url = format!("{}{}", base.trim_end_matches('/'), path);
        let mut signer_credential = signer_credential(credential);
        if domainless {
            signer_credential.domain_id = String::new();
        }
        let headers = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Accept".to_string(), "application/json".to_string()),
            ("Agent-Type".to_string(), agent_type.to_string()),
            ("X-Language".to_string(), "en-us".to_string()),
            ("plugin-name".to_string(), "snap_vscode".to_string()),
            ("plugin-version".to_string(), "26.9.101".to_string()),
            ("client_version".to_string(), "Vscode_26.9.101".to_string()),
            ("is_confidential".to_string(), "false".to_string()),
        ];
        let signed = signer::sign("GET", &url, &headers, b"", &signer_credential, host_signed).expect("签名应当成功");
        let mut request = reqwest::Client::new().get(&url).timeout(std::time::Duration::from_secs(30));
        for (name, value) in signed {
            request = request.header(name.as_str(), value.as_str());
        }
        let response = request.send().await.expect("请求发不出去");
        (response.status().as_u16(), response.text().await.unwrap_or_default())
    }

    #[tokio::test]
    #[ignore]
    async fn capture_catalog_fixtures() {
        let path = std::env::var("CODEARTS_CREDENTIAL_FILE").unwrap_or_default();
        if path.is_empty() {
            println!("跳过：未设 CODEARTS_CREDENTIAL_FILE");
            return;
        }
        let out = std::env::var("CODEARTS_FIXTURES_OUT").unwrap_or_default();
        if out.is_empty() {
            println!("跳过：未设 CODEARTS_FIXTURES_OUT");
            return;
        }
        std::fs::create_dir_all(&out).unwrap();
        let raw = std::fs::read(&path).unwrap();
        let credential = Credential::from_payload(&serde_json::from_slice::<serde_json::Value>(&raw).unwrap()).unwrap();
        let base = "https://snap-access.cn-north-4.myhuaweicloud.com";
        for (name, path, agent_type, host_signed, domainless) in [
            ("builtin", "/v1/model/builtin", "PromptCenter", false, false),
            ("useragents", "/v1/agent-center/agents/useragents?offset=0&limit=100&is_primary_agent=true&supported_client=VSCODE&min_compatible_plugin_version=26.9.101", "AgentCenter", false, false),
            ("benefit-gate", "/v1/benefit-gateway-config", "PromptCenter", false, false),
        ] {
            let (status, body) = fetch(base, path, agent_type, &credential, host_signed, domainless).await;
            std::fs::write(format!("{out}/{name}.json"), &body).unwrap();
            println!("{name}: HTTP {status}, {} bytes", body.len());
        }
        // agent detail 需要 agent_id：从 useragents 里取第一个
        let (_, agents_body) = fetch(base, "/v1/agent-center/agents/useragents?offset=0&limit=100&is_primary_agent=true&supported_client=VSCODE&min_compatible_plugin_version=26.9.101", "AgentCenter", &credential, false, false).await;
        let agents: serde_json::Value = serde_json::from_str(&agents_body).unwrap_or(serde_json::Value::Null);
        let agent_id = agents["agents"]
            .as_array()
            .and_then(|items| items.first())
            .and_then(|first| first["agent_id"].as_str().or(first["original_id"].as_str()))
            .unwrap_or("NO-AGENT")
            .to_string();
        println!("第一个 agent_id: {agent_id}");
        let (status, body) = fetch(base, &format!("/v1/agent-center/agents/detail?agent_id={agent_id}"), "AgentCenter", &credential, false, false).await;
        std::fs::write(format!("{out}/agent-detail.json"), &body).unwrap();
        println!("agent-detail: HTTP {status}, {} bytes", body.len());

        // 福利网关是另一个主机、签名契约不同（带 Host、无 domain）
        let gate: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(format!("{out}/benefit-gate.json")).unwrap()).unwrap_or(serde_json::Value::Null);
        let enabled = gate["enabled"].as_bool().unwrap_or(false);
        println!("福利网关开关: {enabled}");
        if enabled {
            let (status, body) = fetch("https://opengw.developer.huaweicloud.com", "/api/v1/gateway/config", "", &credential, true, true).await;
            std::fs::write(format!("{out}/benefit-gateway-config.json"), &body).unwrap();
            println!("benefit-gateway-config: HTTP {status}, {} bytes", body.len());
        }
    }
}


/// 透传流上的 **usage 旁路嗅探**（只读，一个字节都不改）。
///
/// ── 为什么本家需要它 ─────────────────────────────────────
/// CodeArts 的上游只有流式，且这一家是把 chunk **原样透传**给客户端的 —— 不经过
/// `upstream::sse` 的折叠器，而那条 `report_usage` 旁路（流式路径唯一会写请求日志
/// 用量的地方）只长在折叠器里。后果实测得很干脆：客户端拿得到
/// `{"prompt_tokens":35,"completion_tokens":333,…}`，而 `requests` 表里
/// `prompt_tokens` / `completion_tokens` **两列恒为 0**。用量报表、按模型的消耗
/// 排名、成本读数全部读那张表 —— 于是这一家在报表里像一个从不烧额度的黑洞。
///
/// ── 三条成本约束 ────────────────────────────────────────
///   · **不解析没有嫌疑的行**：先按 `usage` 这五个字节粗筛，一条长回答里的几百帧
///     绝大多数连一次 JSON 解析都换不来；
///   · **行缓冲有上限**（[`Self::MAX_LINE_BYTES`]）：超限就丢缓冲。usage 帧是
///     几十字节量级，丢一个超长行最多漏记一次用量，而不设上限等于让上游的一帧
///     决定我们的内存占用；
///   · **绝不改写流**：本类型只消费副本，`push` 的返回值恒为 `()`（保留签名是为
///     了将来真要改写时看得出这里刻意没做）。
#[derive(Default)]
pub struct UsageSniffer {
    pending: Vec<u8>,
}

impl UsageSniffer {
    const MAX_LINE_BYTES: usize = 64 * 1024;

    /// 吃进一段透传字节，把其中完整行里带的 usage 报给 telemetry。
    pub fn feed(&mut self, bytes: &[u8], telemetry: &RequestTelemetry) {
        self.pending.extend_from_slice(bytes);
        loop {
            let Some(newline) = find_byte(&self.pending, b'\n') else {
                break;
            };
            let line: Vec<u8> = self.pending.drain(..=newline).collect();
            self.consume_line(&line, telemetry);
        }
        if self.pending.len() > Self::MAX_LINE_BYTES {
            // 一行了无休止地长：丢掉缓冲（不是丢流 —— 流是旁路看的，照旧原样透传）
            self.pending.clear();
        }
    }

    /// 流结束时把最后一段（上游可能不给结尾换行）也看一遍。
    pub fn finish(&mut self, telemetry: &RequestTelemetry) {
        if !self.pending.is_empty() {
            let rest = std::mem::take(&mut self.pending);
            self.consume_line(&rest, telemetry);
        }
    }

    fn consume_line(&self, line: &[u8], telemetry: &RequestTelemetry) {
        let text = String::from_utf8_lossy(line);
        let trimmed = text.trim_end_matches(['\r', '\n']);
        let Some(payload) = trimmed.strip_prefix("data:") else {
            return;
        };
        let payload = payload.trim();
        if payload.is_empty() || !contains_needle(payload.as_bytes(), b"usage") {
            return;
        }
        let Ok(chunk) = serde_json::from_str::<Value>(payload) else {
            return;
        };
        if let Some(usage) = chunk.get("usage").filter(|value| value.is_object()) {
            telemetry.report_usage(usage);
        }
    }
}

/// 子串查找（手写而非引依赖：这里只找五个固定字节，`memchr` 那点收益不值得加一个 crate）。
fn contains_needle(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.len() >= needle.len()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn find_byte(haystack: &[u8], byte: u8) -> Option<usize> {
    haystack.iter().position(|value| *value == byte)
}

#[cfg(test)]
mod usage_sniffer {
    //! 嗅探器的四条判据：报得到、跨块拼得上、超长不炸、非 usage 帧不误报。

    use std::sync::Arc;

    use serde_json::json;

    use crate::server::core::upstream::usage::RequestTelemetry;

    use super::UsageSniffer;

    fn frame(payload: &str) -> String {
        format!("data: {payload}\n\n")
    }

    fn reported(telemetry: &RequestTelemetry) -> (i64, i64) {
        let snapshot = telemetry.snapshot();
        (snapshot.prompt_tokens, snapshot.completion_tokens)
    }

    #[test]
    fn it_reports_a_usage_frame_while_leaving_the_bytes_alone() {
        let telemetry = Arc::new(RequestTelemetry::new());
        let mut sniffer = UsageSniffer::default();
        sniffer.feed(
            frame("{\"choices\":[{\"delta\":{\"content\":\"你\"}}]}").as_bytes(),
            &telemetry,
        );
        sniffer.feed(
            frame(
                &json!({"usage": {"prompt_tokens": 35, "completion_tokens": 333, "total_tokens": 368}})
                    .to_string(),
            )
            .as_bytes(),
            &telemetry,
        );
        sniffer.finish(&telemetry);
        assert_eq!((35, 333), reported(&telemetry), "用量要进请求日志的那两列");
    }

    #[test]
    fn a_usage_frame_split_across_two_chunks_is_still_read() {
        // 上游按 TCP 段吐字节，usage 帧完全可能被劈成两半 —— 只看单个 chunk 的
        // 实现会在这里静默漏记，而那正是本类型存在的理由
        let telemetry = Arc::new(RequestTelemetry::new());
        let mut sniffer = UsageSniffer::default();
        let whole = frame("{\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":9}}");
        let head = &whole[..whole.len() - 6];
        let tail = &whole[whole.len() - 6..];
        sniffer.feed(head.as_bytes(), &telemetry);
        assert_eq!((0, 0), reported(&telemetry), "半行还不该被当成一帧解析");
        sniffer.feed(tail.as_bytes(), &telemetry);
        sniffer.finish(&telemetry);
        assert_eq!((7, 9), reported(&telemetry));
    }

    #[test]
    fn an_oversized_line_drops_the_buffer_rather_than_growing_forever() {
        let telemetry = Arc::new(RequestTelemetry::new());
        let mut sniffer = UsageSniffer::default();
        for _ in 0..80 {
            sniffer.feed(&[b'x'; 4096], &telemetry);
        }
        assert!(
            sniffer.pending.len() <= UsageSniffer::MAX_LINE_BYTES,
            "缓冲必须有上限：{}",
            sniffer.pending.len()
        );
        sniffer.finish(&telemetry);
        assert_eq!((0, 0), reported(&telemetry), "一帧都没读出来也不该报错");
    }

    #[test]
    fn frames_without_usage_are_not_parsed_as_one() {
        let telemetry = Arc::new(RequestTelemetry::new());
        let mut sniffer = UsageSniffer::default();
        // `usagex` 之类含前缀的键、以及正文里出现 usage 这个词的文本帧，都不算
        sniffer.feed(
            frame("{\"choices\":[{\"delta\":{\"content\":\"about usage today\"}}]}").as_bytes(),
            &telemetry,
        );
        sniffer.feed(
            frame("{\"usagex\":{\"prompt_tokens\":1}}").as_bytes(),
            &telemetry,
        );
        sniffer.feed("not json at all\n\n".as_bytes(), &telemetry);
        sniffer.finish(&telemetry);
        assert_eq!(
            (0, 0),
            reported(&telemetry),
            "误报会把整次请求的用量写成 0 以外的假数"
        );
    }
}

/// 「客户端额度撑不下思考」时的抬额度规则（阈值与形状照 `zcode::reasoning`，
/// 那条通道上游自己就是这么做的，两处保持一致）。
pub const SHORT_OUTPUT_TOKENS: i64 = 1024;
pub const THINKING_RESERVE: i64 = 1024;

/// 某些模型**一定先思考**，而思考与正文共用 `max_tokens` 这一个额度。
///
/// 实测（2026-10-02，同一句提示、同一个福利模型）：
///
/// | 客户端写法 | 结果 |
/// |---|---|
/// | `max_tokens:60` | 正文 **0 字**、reasoning 100 字、`finish=length`（用量 35/60） |
/// | `max_tokens:1084` | 正文 102 字、reasoning 387 字、`finish=stop` |
/// | 加 `thinking:{type:"disabled"}` | 正文 81 字、reasoning 0 —— 上游认这一格 |
/// | 加 `reasoning_effort:"minimal"` / `thinking.budget:32` | **上游不理**，照样想满、正文空 |
///
/// 所以"让它少想点"这条路在上游不存在，而"关掉思考"会改变模型行为（同一个名字
/// 在同一个客户端里两种表现）。抬上限是唯一既不动模型行为、又能让正文出来的救法。
///
/// 三条边界：
///   · **只在客户端额度低于 [`SHORT_OUTPUT_TOKENS`] 时抬** —— 那种请求要的是短回答,
///     思考模型给不出"又短又有正文"；给得起大额度（含没给）的客户端不关它的事；
///   · 按该模型声明的 `max_output_tokens` **截顶**，`0`（未声明）时不截 ——
///     与目录出口"不编一个数"同一口径；
///   · 只可能把额度**变大**：截顶后仍不大于原值时原样不动（把客户端的 60 改成
///     更小是替客户端做主，不在本函数的权限里）。
///
/// 两种写法都认（`max_tokens` / `max_completion_tokens`），改的是**客户端用了的那个键**，
/// 不会替客户端新造一个键。返回 `Some((抬前, 抬后))` 让调用方有话可写。
pub fn reserve_for_thinking(payload: &mut Value, max_output_tokens: i64) -> Option<(i64, i64)> {
    let key = ["max_tokens", "max_completion_tokens"]
        .into_iter()
        .find(|name| payload.get(*name).and_then(Value::as_i64).is_some())?;
    let client = payload.get(key).and_then(Value::as_i64)?;
    if client <= 0 || client >= SHORT_OUTPUT_TOKENS {
        return None;
    }
    let wanted = client.saturating_add(THINKING_RESERVE);
    let next = if max_output_tokens > 0 {
        wanted.min(max_output_tokens)
    } else {
        wanted
    };
    if next <= client {
        return None;
    }
    payload
        .as_object_mut()?
        .insert(key.to_string(), Value::from(next));
    Some((client, next))
}

#[cfg(test)]
mod thinking_reserve {
    //! 抬额度的三条边界：只在短额度动手、按模型上限截顶、永不把额度改小。

    use serde_json::json;

    use super::{reserve_for_thinking, SHORT_OUTPUT_TOKENS, THINKING_RESERVE};

    fn max_of(payload: &serde_json::Value) -> i64 {
        payload
            .get("max_tokens")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(-1)
    }

    #[test]
    fn a_short_cap_is_raised_by_the_reserve() {
        let mut payload = json!({"model": "m", "max_tokens": 60});
        assert_eq!(
            Some((60, 60 + THINKING_RESERVE)),
            reserve_for_thinking(&mut payload, 384_000),
            "实测形状：60 会被思考吃光，正文 0 字"
        );
        assert_eq!(
            60 + THINKING_RESERVE,
            max_of(&payload),
            "抬的是「原值 + 预留」，不是阈值 + 预留"
        );
    }

    #[test]
    fn a_generous_cap_is_left_alone() {
        // 客户端自己给得够（或压根没给）就不关本函数的事 —— 抬别人的大额度
        // 只是把成本推高，救不了任何请求
        for cap in [SHORT_OUTPUT_TOKENS, 4096] {
            let mut payload = json!({"max_tokens": cap});
            assert_eq!(None, reserve_for_thinking(&mut payload, 384_000));
            assert_eq!(cap, max_of(&payload), "值不能被改");
        }
        let mut absent = json!({"model": "m"});
        assert_eq!(None, reserve_for_thinking(&mut absent, 384_000));
        assert!(
            absent.get("max_tokens").is_none(),
            "没给额度的请求不该被凭空造一个上限"
        );
    }

    #[test]
    fn the_model_declared_ceiling_caps_the_reserve() {
        let mut payload = json!({"max_tokens": 60});
        assert_eq!(
            Some((60, 80)),
            reserve_for_thinking(&mut payload, 80),
            "按模型上限截顶"
        );
        assert_eq!(80, max_of(&payload));

        // 上限比原额度还小：不动（把 60 改成 40 是替客户端做主）
        let mut tight = json!({"max_tokens": 60});
        assert_eq!(None, reserve_for_thinking(&mut tight, 40));
        assert_eq!(60, max_of(&tight));

        // 0 = 未声明：不截顶，也绝不编一个数出来
        let mut unknown = json!({"max_tokens": 60});
        assert_eq!(
            Some((60, 60 + THINKING_RESERVE)),
            reserve_for_thinking(&mut unknown, 0)
        );
    }

    #[test]
    fn the_newer_field_name_is_raised_in_place() {
        // 生态里两种写法都在跑；改的是客户端用的那个键，不替它新造一个
        let mut payload = json!({"max_completion_tokens": 100});
        assert_eq!(
            Some((100, 100 + THINKING_RESERVE)),
            reserve_for_thinking(&mut payload, 384_000)
        );
        assert_eq!(
            100 + THINKING_RESERVE,
            payload["max_completion_tokens"].as_i64().unwrap_or(-1)
        );
        assert!(
            payload.get("max_tokens").is_none(),
            "不该同时存在两个额度键"
        );
    }
}

/// 把 OpenAI 的对象形态 `tool_choice`（指名某函数）折成字符串 `"required"`。
///
/// ── 依据（2026-10-10 在 dev 上量的，同一模型同一套短工具名，只变 `tool_choice`）──
/// · 对象形态 `{"type":"function","function":{"name":X}}` ⇒ **必拒**：
///   `InferHub.001001005.400 The request param is invalid`（发出约 0.2 秒回来，被拒不计费）；
/// · 与 `tools` 有没有无关（空 `tools` 带对象形态同样拒），与名字长短也无关；
/// · 换成 `"required"` ⇒ 200 且真产生 tool_call；`"none"` / `"auto"` / 不带 ⇒ 都 200；
/// · 旧版 `functions` + `function_call` 那套**别拿来做翻译目标**：实测 HTTP 200 但什么都没发生
///   （`finish=stop`、`tool_calls` 空、`function_call` 也没有，`prompt_tokens` 与不带工具同级
///   ⇒ `functions` 根本没进模型）。那是"成功的空转"，比一次明确失败难查得多。
///
/// 语义损失要说清：`required` 保住了「这轮必须调工具」，但**不再指定调哪个** ——
/// 只有一个工具时等价于强制，多个工具时由模型自己挑。点名的那个名字进 verbose 日志，
/// 免得日后"为什么没调我要的那个"变成一次无据排查。
///
/// 只动 `type == "function"` 这一种。别的不认识的对象形态（`allowed_tools` 之类）原样发：
/// 上游怎么回我们没测过，替它猜等于把一种未知换成另一种未知。
///
/// ── 为什么没照抄这个改法来源的另外两条 ──────────────────────
/// 同一个修法在外部项目（`HITZY2002/codearts2api` PR #6）里还带两条：把 param-invalid 归成
/// 400、并且**停止轮转**。那两条的前提这里不成立：
/// · 他们把 400 映射成 502，而它对 ≥500 一律冷却账号池 10 分钟 ⇒ 一次客户端形状错误打停整池。
///   我们的冷却判据挂在**错误类**上（只有 `QuotaLimited` 冷却，见 `provider_loop.rs` 的
///   `UpstreamErrorClass::QuotaLimited` 分支），400/502 都是 `Fatal`，不碰冷却；
/// · 他们的池子里只有 CodeArts 一家，轮转必然撞同一堵墙；我们是跨家的，实测对象形态被
///   CodeArts 拒掉后换到别家就正常答出来了 —— 跟着"停止轮转"等于把一条出路砍掉。
pub fn fold_forced_tool_choice(payload: &mut Value) -> Option<String> {
    let Value::Object(object) = payload else {
        return None;
    };
    let named = object
        .get("tool_choice")
        .and_then(Value::as_object)
        .filter(|choice| choice.get("type").and_then(Value::as_str) == Some("function"))
        .and_then(|choice| choice.get("function"))
        .and_then(|function| function.get("name"))
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .map(str::to_owned)?;
    object.insert("tool_choice".to_string(), Value::String("required".to_string()));
    Some(named)
}

#[cfg(test)]
mod forced_tool_choice {
    use super::{build_upstream_request, fold_forced_tool_choice, HeaderProfile};
    use serde_json::{json, Value};

    fn forced(name: &str) -> Value {
        json!({"tool_choice": {"type": "function", "function": {"name": name}}})
    }

    #[test]
    fn the_object_form_is_folded_into_required_and_the_name_is_reported() {
        let mut payload = forced("alpha");
        assert_eq!(Some("alpha".to_string()), fold_forced_tool_choice(&mut payload));
        assert_eq!("required", payload["tool_choice"], "折完必须是字符串 required");
        // 点名的名字整个从出站体里消失：这道墙与 64 字符那道叠在同一条路径上
        assert_eq!(0, serde_json::to_string(&payload).unwrap().matches("alpha").count());
    }

    #[test]
    fn string_forms_and_unknown_object_forms_are_left_alone() {
        // 反空跑：实现若写成「逢 tool_choice 就改」也照样绿过前一条，这里把三种字符串钉住
        for kept in ["auto", "none", "required"] {
            let mut payload = json!({"tool_choice": kept});
            assert_eq!(None, fold_forced_tool_choice(&mut payload), "{kept}");
            assert_eq!(Value::String(kept.to_string()), payload["tool_choice"]);
        }
        // 没测过上游怎么回的形状不动它
        let mut unknown = json!({"tool_choice": {"type": "allowed_tools"}});
        assert_eq!(None, fold_forced_tool_choice(&mut unknown));
        assert_eq!("allowed_tools", unknown["tool_choice"]["type"]);
        // 指名但名字是空的对象也不动（那是客户端写坏了，不是本家拒的形状）
        let mut blank = forced("   ");
        assert_eq!(None, fold_forced_tool_choice(&mut blank));
    }

    #[test]
    fn the_signed_bytes_carry_no_object_form() {
        // 端到端钉在出口字节上（`credential=None` 是排障逃生口，返回的就是出门那串）：
        // 判据落在真发出去的东西上，不落在中间函数上
        let payload = json!({
            "messages": [{"role": "user", "content": "短答"}],
            "tools": [{"type": "function", "function": {"name": "alpha", "parameters": {"type": "object"}}}],
            "tool_choice": {"type": "function", "function": {"name": "alpha"}},
        });
        let (_, _, body) = build_upstream_request(
            "https://snap-access.cn-north-4.myhuaweicloud.com",
            "GLM-5.2",
            payload,
            false,
            false,
            &HeaderProfile::default(),
            None,
        )
        .expect("应当能构造请求");
        let sent: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!("required", sent["tool_choice"], "出门的字节里不该再有对象形态：{sent}");
        assert_eq!(1, sent["tools"].as_array().unwrap().len(), "tools 本身不该被牵连");
    }
}
