//! 上游 SSE 流聚合（对照 Node 版 aggregateSseCompletion，600-670 行）。
//!
//! 上游**只支持流式**（`stream:false` 返回 code=11101），所以客户端要非流式时，
//! 代理内部仍以 `stream:true` 请求上游，再把 SSE 帧聚合成完整的
//! OpenAI `chat.completion` 结构返回。
//!
//! 聚合规则逐条对照 Node：
//!   - 按行切 `data:`，`[DONE]` 与空行跳过
//!   - `delta.content` / `delta.reasoning_content` / `delta.reasoning` 字符串拼接
//!     （最后一种是 Cline 免费池的写法，见 `consume_line` 的说明）
//!   - `delta.tool_calls` 按 `index` 合并（id/type 覆盖、function.name 与
//!     function.arguments 追加）
//!   - `usage` 取最后一次出现（上游在末尾 chunk 下发）
//!   - `finish_reason` 取最后一次非空
//!   - 帧里的 `error` 字段 → 抛 502（上游把错误写在流里）
//!   - id/created 缺失时给 `wb-agg-<毫秒>` / 当前秒兜底
//!
//! ── usage 旁路提取（请求统计）────────────────────────────────
//! `consume_chunk` 里顺手把 `usage` 抄进 `usage::RequestTelemetry`。
//! **不影响响应体**：决定下发 `usage` 字段的是 `self.usage`（本文件的
//! 既有逻辑），旁路只写另一个结构体，两者互不干扰。

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::StreamExt;
use serde_json::{json, Map, Value};

use crate::server::errors::GatewayError;

use super::cancellation;
use super::sse::{is_newline_keepalive, FramePolicy, ModelRewrite};
use super::usage::RequestTelemetry;

/// 流式读取的上限保护：单条 SSE 行长度（解析失败的行会原样丢掉，
/// 但不能让一条畸形行把内存吃满）
const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

/// 聚合后的完整响应 + 调试用的 chunk 计数
pub struct AggregatedCompletion {
    pub body: Value,
    pub chunk_count: usize,
}

/// 把上游响应体（SSE 字节流）聚合成一个完整 chat.completion。
///
/// `on_chunk` 参数已去掉：Node 里 `controller.signal.aborted` 检查的作用是
/// 「客户端断开时停止聚合」，Rust 侧等价物是 caller 在 select! 里取消整个
/// future（见 forward.rs）—— 无需在循环里重复检查。
///
/// `telemetry` 是 usage 旁路槽（`Arc` 而非引用：聚合要在 await 之间持用，
/// 而转发链路本身是异步的）。它只被写入、不参与 `acc` 的构建 ——
/// **不影响返回的 body**（usage 的透传口径由 aggregate 自己的
/// `self.usage` 负责，与这里无关，两条路径互不干扰）。
///
/// `policy` 与流式分支同源（适配器的 `sse_model_rewrite()` 与
/// `sse_strip_newline_chunks()`）：**非流式响应体里的 `model` 同样要回写**成
/// 客户端请求的名字 —— 客户端拿到的 model 名不该因为「要不要流式」而变样
/// （源实现 `forwardChatCompletions` 的非流式分支也是
/// `payload.model = requestedModel`）。保活换行分片同理：上游插的那些
/// 「整片只有换行」的分片在流式里会被丢掉，非流式若照抄就把同一个噪声以
/// 词间换行的形式交给了客户端（见 `sse.rs` 模块头的实测）。
pub async fn aggregate_sse_completion(
    response: reqwest::Response,
    telemetry: Arc<RequestTelemetry>,
    policy: FramePolicy,
) -> Result<AggregatedCompletion, GatewayError> {
    // 与 `ForwardStream::new` 同款：reqwest 错误在这里就地描述成文案折进
    // io::Error（`describe_error_detail` 只认 reqwest::Error）
    let capture = telemetry.capture();
    let stream = response.bytes_stream().map(move |item| {
        let item = item.map_err(|error| {
            std::io::Error::other(crate::server::core::egress::describe_error_detail(&error))
        });
        if let (Ok(bytes), Some(capture)) = (&item, &capture) {
            capture.push(bytes);
        }
        item
    });
    aggregate_frame_stream(Box::pin(stream), telemetry, policy).await
}

/// 聚合一条**标准 chat SSE** 字节流（不限定来源）。
///
/// 自定义家的翻译协议（responses / anthropic 上游）在进入本函数前先过
/// `providers::custom` 的 `ProtocolTranslateStream` —— 本函数与
/// [`aggregate_sse_completion`] 共用同一套聚合规则，只是输入从
/// reqwest::Response 换成已翻译的帧流。telemetry / policy 的语义
/// 与那个函数完全一致（见它的说明）。
/// 原始报文由输入流在翻译前采集，本函数不重复采集内部 chat 帧。
pub async fn aggregate_frame_stream(
    stream: futures::stream::BoxStream<'static, Result<bytes::Bytes, std::io::Error>>,
    telemetry: Arc<RequestTelemetry>,
    policy: FramePolicy,
) -> Result<AggregatedCompletion, GatewayError> {
    // 非流式响应总超时（设置页「请求超时」第四项）：**一次性计时、不重置**
    // —— 与流式的空闲超时是两种语义（那是「两次数据之间」，这里读完整份
    // 响应体的总预算，对应 OmniProxy 的 readBodyWithStallGuard）。
    // 超时中止整个聚合：非流式客户端此时还没收到任何响应，收尾记账不会丢，
    // 错误原样返回（502 + 明确文案）。
    let budget = std::time::Duration::from_millis(
        crate::server::config::timeout_settings().body_ms(),
    );
    match tokio::time::timeout(budget, aggregate_frame_stream_inner(stream, telemetry, policy))
        .await
    {
        Ok(result) => result,
        Err(_elapsed) => Err(GatewayError::with_status(
            502,
            format!("非流式响应超时({}秒)", budget.as_secs()),
        )),
    }
}

/// [`aggregate_frame_stream`] 的主体（总超时由外层套上，见那里的说明）。
async fn aggregate_frame_stream_inner(
    stream: futures::stream::BoxStream<'static, Result<bytes::Bytes, std::io::Error>>,
    telemetry: Arc<RequestTelemetry>,
    policy: FramePolicy,
) -> Result<AggregatedCompletion, GatewayError> {
    // ── 手动终止的旁路流（与 `ForwardStream::from_translated` 同一手法）──
    // 聚合是 `while let Some(item) = stream.next().await` 的拉取循环：没有
    // 旁路流时，取消要等下一个上游分片（上游停滞时可能等很久）。把令牌的
    // 等待合进流里，置位后下一轮 await 立刻拿到 Err，由下面的 map_err 折成
    // 408 的网关错误（非流式客户端此时还没收到任何响应，收尾记账会把它记成
    // 「请求已被手动终止」）。放在本函数而不是 `aggregate_sse_completion`：
    // 自定义家的翻译协议流也走这里（聚合规则共用），两处都要覆盖。
    //
    // 合成器用 `cancellation::cancellable` 而**不是** `stream::select`：
    // 后者的收尾判据是「两条都结束」，旁路流在上游正常结束时永不产出，于是
    // 下面这个 `while let` 永不退出 —— 聚合明明已经读到上游 EOF，却要空转到
    // 非流式总超时（默认 300 秒）才报 502（详见 `cancellable` 的说明）。
    let mut stream = cancellation::cancellable(stream, telemetry.cancel_token());
    let mut buffer = String::new();
    let mut acc = CompletionAccumulator {
        rewrite: policy.rewrite,
        strip_newline_chunks: policy.strip_newline_chunks,
        ..Default::default()
    };
    // 首响采集：聚合路径不走 RecordingStream（客户端要的是完整 JSON，
    // 没有下发流可言），所以第一个**上游** chunk 在这里记 —— 它就是
    // 「上游开始吐内容」的时刻。非流式请求的用时要等聚合完才有意义，
    // 首响补上了「上游是快是慢」这半边（与流式路径同口径：首次为准）。
    let mut first_chunk_seen = false;
    while let Some(item) = stream.next().await {
        let chunk = item.map_err(|error| {
            // 手动终止的旁路流给的就是原文（见上面的说明）：折成 408 的
            // 网关错误，不加「上游流式传输中断」前缀 —— 它不是上游的问题
            let text = error.to_string();
            if text == cancellation::MANUAL_TERMINATED {
                return cancellation::cancelled_error();
            }
            // 空闲超时是保护性判定不是中断：自带完整文案（含设定秒数），
            // 直接用（与 `ForwardStream` 的错误帧同一处理）
            if text.starts_with(super::stall::IDLE_TIMEOUT_PREFIX) {
                return GatewayError::with_status(502, text);
            }
            // 错误描述已在构造时折进 io::Error（reqwest 直连在
            // `aggregate_sse_completion`、翻译流在 `ProtocolTranslateStream`）
            GatewayError::with_status(502, format!("上游流式传输中断: {error}"))
        })?;
        if !first_chunk_seen {
            first_chunk_seen = true;
            telemetry.note_first_frame();
        }
        // 原始字节由输入侧（HTTP 字节流或协议翻译流）采集，聚合器只消费 chat 帧。
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        // 逐行消费（只处理到最后一个 '\n' 之前的内容）
        while let Some(index) = buffer.find('\n') {
            let line = buffer[..index].trim().to_string();
            buffer.drain(..=index);
            if line.is_empty() {
                continue;
            }
            acc.consume_line(&line, &telemetry)?;
        }
        if buffer.len() > MAX_LINE_BYTES {
            return Err(GatewayError::with_status(502, "上游返回的单行数据过大，已中断"));
        }
    }
    // 尾行（上游没以换行结尾）
    let tail = buffer.trim().to_string();
    if !tail.is_empty() {
        acc.consume_line(&tail, &telemetry)?;
    }

    Ok(acc.into_completion())
}

/// 聚合状态（Node 的局部变量集中到这里，便于 `consume_line` 返回 Result）
#[derive(Default)]
struct CompletionAccumulator {
    id: String,
    model: String,
    created: i64,
    role: String,
    content: String,
    reasoning: String,
    finish: String,
    usage: Option<Value>,
    chunk_count: usize,
    tool_calls: BTreeMap<i64, Value>,
    /// model 名回写参数（None = 不改写，沿用上游给的 model）
    rewrite: Option<ModelRewrite>,
    /// 丢弃「整片只有换行」的 content 分片（WorkBuddy / AutoClaw 的保活节拍，
    /// 判据与理由见 `sse::is_newline_keepalive`）。false = 与接入前逐字一致。
    strip_newline_chunks: bool,
}

impl CompletionAccumulator {
    /// 处理一行 SSE（`data: {...}` / `data: [DONE]` / 其他）
    fn consume_line(&mut self, line: &str, telemetry: &RequestTelemetry) -> Result<(), GatewayError> {
        let Some(data) = line.strip_prefix("data:") else {
            return Ok(());
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            return Ok(());
        }
        // Node: `try { handleChunk(JSON.parse(line)) } catch { /* 非 JSON 行忽略 */ }`
        // —— 非 JSON 行静默忽略，但 handleChunk 抛出的 WorkBuddyUpstreamError 要透出
        let Ok(chunk) = serde_json::from_str::<Value>(data) else {
            return Ok(());
        };
        self.consume_chunk(&chunk, telemetry)
    }

    fn consume_chunk(&mut self, chunk: &Value, telemetry: &RequestTelemetry) -> Result<(), GatewayError> {
        let Some(object) = chunk.as_object() else {
            return Ok(());
        };
        // ── usage 旁路提取（请求统计）──────────────────────────────
        // 放在错误判定**之前**：上游把错误写在流里时也可能带上已消耗的
        // usage（提示词已计费），旁路先抄一份不影响下面照常抛 502。
        // 为什么这里的失败同样是「无副作用」的：只读 `chunk` 的一个成员，
        // 不碰 `self.usage`（那才是决定响应体 usage 字段的那份），
        // 所以返回值与不接钩子时完全一致。
        if let Some(usage) = object.get("usage") {
            telemetry.report_usage(usage);
        }
        if let Some(error) = object.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .unwrap_or("上游流式返回错误");
            return Err(GatewayError::with_status(502, message.to_string()));
        }
        self.chunk_count += 1;
        if let Some(id) = object.get("id").and_then(Value::as_str) {
            if !id.is_empty() {
                self.id = id.to_string();
            }
        }
        if let Some(model) = object.get("model").and_then(Value::as_str) {
            self.model = model.to_string();
        }
        // Node: `created = chunk.created || created` —— 真值判定（0 不覆盖已有值）
        if let Some(created) = object.get("created") {
            if js_number_truthy(created) {
                self.created = created.as_i64().or_else(|| created.as_f64().map(|value| value as i64)).unwrap_or(0);
            }
        }
        if let Some(usage) = object.get("usage") {
            if usage.is_object() {
                self.usage = Some(usage.clone());
            }
        }

        let Some(choice) = object
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        else {
            return Ok(());
        };
        let delta = choice.get("delta").cloned().unwrap_or(json!({}));
        if let Some(role) = delta.get("role").and_then(Value::as_str) {
            if !role.is_empty() {
                self.role = role.to_string();
            }
        }
        if let Some(content) = delta.get("content").and_then(Value::as_str) {
            // 保活换行分片：只跳过**正文拼接**，不提前 return —— 同一帧的
            // id / model / created / role / usage 照常记账（流式分支也是这个
            // 次序：先旁路提取与记元数据，再决定丢这一帧）。两条路径口径一致，
            // 「同一个请求要不要流式」才不会出现两种正文。
            let keepalive = self.strip_newline_chunks && is_newline_keepalive(chunk);
            if !keepalive {
                self.content.push_str(content);
            }
        }
        // 思考增量挂在哪个字段上，上游并不统一：`reasoning_content`（多数
        // OpenAI 兼容网关）与 `reasoning`（Cline 免费池实测，配 `reasoning_details`）
        // 两种都见过。只认前者会把「整段输出都是思考」的响应聚合成一条
        // content 空、reasoning 也空的消息 —— 下游的空回检测据此报
        // 「empty response detected」。参考实现 cline-proxy 的 `sawContent`
        // 同样把 `reasoning` 算作内容，理由与这里一样（见其 proxy.go 的注释）。
        // 两个字段都累加、不互斥：与 cline-proxy 逐字同口径。
        for key in ["reasoning_content", "reasoning"] {
            if let Some(reasoning) = delta.get(key).and_then(Value::as_str) {
                self.reasoning.push_str(reasoning);
            }
        }
        if let Some(finish) = choice.get("finish_reason").and_then(Value::as_str) {
            if !finish.is_empty() {
                self.finish = finish.to_string();
            }
        }
        if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in tool_calls {
                let index = call
                    .get("index")
                    .and_then(Value::as_i64)
                    // Node: `Number.isInteger(tc.index) ? tc.index : 0`
                    .unwrap_or(0);
                let entry = self
                    .tool_calls
                    .entry(index)
                    .or_insert_with(|| {
                        json!({
                            "id": "",
                            "type": "function",
                            "function": { "name": "", "arguments": "" },
                        })
                    });
                merge_tool_call(entry, call);
            }
        }
        Ok(())
    }

    /// 组装最终 body（对应 Node 的 handleChunk 结束后的返回对象）
    fn into_completion(self) -> AggregatedCompletion {
        // ── 诊断：上游只回了思考、正文为空 ──────────────────────────
        // 这是**允许发生**的一种结局（始终思考的模型在小输出额度下会把额度
        // 全花在思考上，上游仍回 200，见 `providers::zcode::reasoning` 的模块头）。
        // 网关不在这里改协议 —— 伪造一个错误帧会让「上游到底说了什么」失去
        // 可信度；这里只把事实写进日志：排查这类问题时最缺的正是
        // 「上游其实只回了思考」这一条。
        if self.content.trim().is_empty() && !self.reasoning.trim().is_empty() {
            crate::server::logging::verbose(
                "[Model]",
                &format!(
                    "⚠️ 上游只回了思考、正文为空（model={} finish={} 思考 {} 字）：\
                     多为输出额度被思考吃光，可调高 max_tokens 或降低思考档位",
                    self.model,
                    if self.finish.is_empty() { "-" } else { &self.finish },
                    self.reasoning.chars().count(),
                ),
            );
        }
        let mut message = Map::new();
        message.insert(
            "role".to_string(),
            Value::String(if self.role.is_empty() { "assistant".to_string() } else { self.role }),
        );
        message.insert("content".to_string(), Value::String(self.content));
        if !self.reasoning.is_empty() {
            message.insert("reasoning_content".to_string(), Value::String(self.reasoning));
        }
        if !self.tool_calls.is_empty() {
            message.insert(
                "tool_calls".to_string(),
                Value::Array(self.tool_calls.into_values().collect()),
            );
        }
        let mut body = Map::new();
        body.insert(
            "id".to_string(),
            Value::String(if self.id.is_empty() {
                format!("wb-agg-{}", crate::server::logging::now_ms())
            } else {
                self.id
            }),
        );
        body.insert("object".to_string(), Value::String("chat.completion".to_string()));
        body.insert(
            "created".to_string(),
            Value::from(if self.created != 0 {
                self.created
            } else {
                crate::server::logging::now_ms() / 1000
            }),
        );
        // model 名回写（见 `aggregate_sse_completion` 的说明）：配置了回写时
        // 用客户端请求的名字，否则沿用上游给的（可能为空串 —— 与改造前一致）
        let model = match &self.rewrite {
            Some(rewrite) => rewrite.requested.clone(),
            None => self.model,
        };
        body.insert("model".to_string(), Value::String(model));
        body.insert(
            "choices".to_string(),
            json!([{
                "index": 0,
                "message": Value::Object(message),
                "finish_reason": if self.finish.is_empty() { "stop".to_string() } else { self.finish },
            }]),
        );
        // Node 的返回对象里 `usage` 是**始终存在**的键：上游没下发时是 null
        // （JSON.stringify 会保留 null，而不是丢掉这个键）——
        // OpenAI SDK 对 `usage: null` 与缺键的处理不完全一样，照抄更稳。
        body.insert(
            "usage".to_string(),
            self.usage.unwrap_or(Value::Null),
        );
        AggregatedCompletion { body: Value::Object(body), chunk_count: self.chunk_count }
    }
}

/// 合并一个 tool_call 增量到已累积的条目上（id/type 覆盖、name/arguments 追加）
fn merge_tool_call(entry: &mut Value, incoming: &Value) {
    let Some(target) = entry.as_object_mut() else {
        return;
    };
    if let Some(id) = incoming.get("id").and_then(Value::as_str) {
        if !id.is_empty() {
            target.insert("id".to_string(), Value::String(id.to_string()));
        }
    }
    if let Some(kind) = incoming.get("type").and_then(Value::as_str) {
        if !kind.is_empty() {
            target.insert("type".to_string(), Value::String(kind.to_string()));
        }
    }
    let Some(function) = incoming.get("function") else {
        return;
    };
    let Some(target_function) = target
        .get_mut("function")
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    if let Some(name) = function.get("name").and_then(Value::as_str) {
        let current = target_function
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        target_function.insert("name".to_string(), Value::String(format!("{current}{name}")));
    }
    if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
        let current = target_function
            .get("arguments")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        target_function.insert(
            "arguments".to_string(),
            Value::String(format!("{current}{arguments}")),
        );
    }
}

/// 真值判定（Node 的 `chunk.created || created`）
fn js_number_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().map(|item| item != 0.0).unwrap_or(false),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 喂一行 SSE（非 JSON / 非 data: 行静默忽略，与生产路径一致）
    fn feed(acc: &mut CompletionAccumulator, line: &str) {
        let telemetry = RequestTelemetry::new();
        acc.consume_line(line, &telemetry)
            .unwrap_or_else(|error| panic!("consume_line 失败：{error}"));
    }

    /// 上游把思考放在 `delta.reasoning`（Cline 免费池实测的写法，配
    /// `reasoning_details`）时，非流式聚合必须把它留下：只认 `reasoning_content`
    /// 会让「整段输出都是思考」的响应聚合成 content 空、reasoning 也空的消息，
    /// 下游的空回检测（如 AxonHub）据此报「empty response detected」。
    /// 参考实现 cline-proxy 的 `sawContent` 同样把 `reasoning` 算作内容。
    #[test]
    fn reasoning_delta_is_kept() {
        let mut acc = CompletionAccumulator::default();
        feed(
            &mut acc,
            r#"data: {"choices":[{"delta":{"role":"assistant"},"finish_reason":null}]}"#,
        );
        feed(
            &mut acc,
            r#"data: {"choices":[{"delta":{"reasoning":"我们先分析"},"finish_reason":null}]}"#,
        );
        feed(
            &mut acc,
            r#"data: {"choices":[{"delta":{"reasoning":"题目。"},"finish_reason":"length"}]}"#,
        );
        feed(&mut acc, "data: [DONE]");

        let body = acc.into_completion().body;
        let message = &body["choices"][0]["message"];
        assert_eq!(message["reasoning_content"], "我们先分析题目。");
        // content 本来就空 —— 聚合结果里必须仍有一条非空的可读字段，
        // 这正是「有没有内容」的判据
        assert_eq!(message["content"], "");
    }

    /// `reasoning_content` 与 `reasoning` 都累加、互不排斥（与 cline-proxy
    /// 逐字同口径）：上游在不同分片里换字段时不会丢内容。
    #[test]
    fn both_reasoning_fields_accumulate() {
        let mut acc = CompletionAccumulator::default();
        feed(
            &mut acc,
            r#"data: {"choices":[{"delta":{"reasoning_content":"甲"},"finish_reason":null}]}"#,
        );
        feed(
            &mut acc,
            r#"data: {"choices":[{"delta":{"reasoning":"乙"},"finish_reason":null}]}"#,
        );
        let body = acc.into_completion().body;
        assert_eq!(body["choices"][0]["message"]["reasoning_content"], "甲乙");
    }

    /// 纯正文分片不受影响（reasoning 字段为空时不写回该键）
    #[test]
    fn content_only_stays_as_before() {
        let mut acc = CompletionAccumulator::default();
        feed(
            &mut acc,
            r#"data: {"choices":[{"delta":{"content":"你好"},"finish_reason":"stop"}]}"#,
        );
        let body = acc.into_completion().body;
        let message = &body["choices"][0]["message"];
        assert_eq!(message["content"], "你好");
        assert!(message.get("reasoning_content").is_none());
    }

    /// 把「A / \n / ' B'」三帧折成一份非流式正文（`strip` = 丢不丢保活换行）
    fn fold(strip: bool) -> String {
        let mut acc = CompletionAccumulator {
            strip_newline_chunks: strip,
            ..Default::default()
        };
        let telemetry = RequestTelemetry::new();
        for delta in [json!({"content": "A"}), json!({"content": "\n"}), json!({"content": " B"})] {
            let chunk = json!({
                "id": "c1",
                "model": "m",
                "created": 1,
                "choices": [{"index": 0, "delta": delta, "finish_reason": null}],
                "usage": Value::Null,
            });
            acc.consume_chunk(&chunk, &telemetry)
                .expect("测试里的帧不该被判成上游错误");
        }
        let body = acc.into_completion().body;
        body.pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }

    #[test]
    fn non_stream_folding_drops_the_keepalive_newlines_too() {
        // 两条路径口径必须一致：同一个请求要不要流式，正文不该长得不一样
        assert_eq!(fold(false), "A\n B", "关掉时照旧逐字拼接");
        assert_eq!(fold(true), "A B", "开启时非流式客户端也拿不到那一记换行");
    }
}
