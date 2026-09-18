//! 对话转发主链路（对照 Node 版 workbuddy-upstream-client.mjs 的转发部分）。
//!
//! ── 一次转发的完整流程 ────────────────────────────────────────
//!   ① 去重排队：相同 body 的快速重试在代理内等前一个完成（防风控）
//!   ② 选路循环：按优先级选账号 → 发请求（含 11128 退避重试）→
//!      429/6004 时标记限额并降级到下一个候选
//!   ③ 流式：SSE 透传（reasoning 帧合并）；非流式：内部流式聚合成 JSON
//!
//! ── 文件分工（单文件行数约定）────────────────────────────
//!   mod.rs       转发编排：去重槽位、选路循环、SSE 流（ForwardStream）
//!   rotate.rs    账号选路与 429 轮换：selectTargetAccount / WAF 退避 /
//!                限额标记 / 429 结构化事件上报
//!   request.rs   请求构造：头集合、URL、system 注入、上游错误解析
//!   sse.rs       SSE reasoning 帧合并（跨 chunk 半行缓冲）+ usage 旁路提取
//!   aggregate.rs 非流式聚合（SSE → 完整 chat.completion）+ usage 旁路提取
//!   usage.rs     usage 旁路槽：token 用量 / 尝试账号 / 尝试次数的共享记录点
//!
//! ── 与 Node 版的两处结构差异（都是为了 Rust 的所有权模型）────
//!   1. Node 是「先 writeHead、再一边读上游一边写 res」的回调推进模式；
//!      Rust 侧必须一次性把 `Response` 交还给 axum，所以流式转发把
//!      「上游响应 + 合并器 + 在途槽位」打包成一个 `Stream`，由 axum 拉取。
//!      好处是**客户端断开自动传播**：响应体被 drop 时整条 Stream 被 drop，
//!      reqwest 的 `bytes_stream` 随之 drop，hyper 检测到 body 接收端消失后
//!      会直接 `close_read()` 关掉上游连接（详见 `ForwardStream` 的注释）。
//!   2. Node 的 `pipeSse` 在 res close 时 abort controller；Rust 侧不需要
//!      那个 controller —— drop 传播已经覆盖，且没有「忘了 abort」的风险。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本模块**绝不** unwrap/expect/panic。
//! SSE 流的中断（客户端断开、上游断开）是**正常路径**，一律用 Result/Option。

pub mod aggregate;
pub mod request;
mod rotate;
pub mod sse;
pub mod usage;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures::Stream;
use serde_json::{json, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::core::auth::AuthService;
use crate::server::core::proxies::ResolvedProxy;
use crate::server::errors::GatewayError;
use crate::server::logging;

use self::request::{
    chat_completions_url, chat_headers, ensure_leading_system_message, is_quota_limit_error,
    new_request_id, ChatRequestPlan, DEFAULT_SYSTEM_PROMPT,
};
use self::sse::ReasoningCoalescer;

/// 去重等待上限（对照 Node 的 INFLIGHT_WAIT_MS）
const INFLIGHT_WAIT_MS: u64 = 45_000;

/// 选路循环的最大轮数（防病态数据下的无限回环）。
///
/// Node 版靠 `pickNextAccount` 返回 null 收尾 —— 每次轮换都会往 triedIds 里
/// 加一个账号，而账号上限是 20，所以必然收敛。这里额外加一个轮数上限兜底：
/// 万一账号数据被手工改出「同一个 id 出现两次」之类的怪状，宁可报错也不要空转。
const MAX_ROUTE_ATTEMPTS: usize = 32;

/// 在途请求的完成信号（去重队列用）。
///
/// ── 为什么不是一个裸 `Notify` ──────────────────────────────
/// `notify_waiters()` 只唤醒**当时已登记**的等待者、且不留凭证，
/// 所以「等的人在 notify 之后才登记」就会白等到超时。完成标志与通知
/// 拆成两步（先置位、后唤醒）后，等待者「先登记等待、再看标志」即可覆盖两条路径：
///   - 置位在登记之前 → 看标志即可立刻返回；
///   - 置位在登记之后 → notify 一定能唤醒已登记的等待者。
/// 顺序上两者不可能都落空，这就是无竞态的判据。
struct InFlight {
    done: AtomicBool,
    signal: tokio::sync::Notify,
}

impl InFlight {
    fn new() -> Self {
        Self { done: AtomicBool::new(false), signal: tokio::sync::Notify::new() }
    }

    /// 标记完成并唤醒所有等待者（调用方保证：先置位、后唤醒）
    fn complete(&self) {
        self.done.store(true, Ordering::SeqCst);
        self.signal.notify_waiters();
    }

    fn is_done(&self) -> bool {
        self.done.load(Ordering::SeqCst)
    }
}

/// 在途槽位的持有凭证：**drop 即放行**（含「流式响应还在下发」的阶段）。
///
/// ── 为什么要有它（与 Node 对齐的关键点）────────────────────
/// Node 的 `trackInFlight(key, promise)` 存的是 `doForwardChatCompletions()`
/// 这个 **async 函数返回的 promise**，而那个函数要等 `pipeSse` 跑完才 resolve
/// —— 也就是说槽位一直占到大半个响应体发完。如果 Rust 侧在「拿到响应头」
/// 就放行，一个失败重试就可能与仍在流式输出的上一个请求并发打到上游，
/// 正好是去重队列要防的事。
///
/// 因此这里把「槽位」从选路函数里延长到流本身：流式分支把本凭证交给
/// `ForwardStream`，流跑完（或客户端断开、流被 drop）时凭证析构，
/// 槽位才释放。非流式分支的凭证在 `forward()` 返回时析构 —— 与 Node 一致。
struct InFlightGuard {
    table: Arc<Mutex<HashMap<String, Arc<InFlight>>>>,
    key: String,
    signal: Arc<InFlight>,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        // 先置位再唤醒（顺序是 InFlight 的判据，别调换）
        self.signal.complete();
        // 表项可能已被后来的同 body 请求覆盖 —— 只在仍指向自己时移除
        let mut guard = lock_table(&self.table);
        let is_self = guard
            .get(&self.key)
            .map(|entry| Arc::ptr_eq(entry, &self.signal))
            .unwrap_or(false);
        if is_self {
            guard.remove(&self.key);
        }
    }
}

/// 转发器句柄：账号存储 + 鉴权 + 在途请求表。
#[derive(Clone)]
pub struct UpstreamService {
    store: AccountStore,
    auth: AuthService,
    /// 在途请求表：sha256(body) → 完成信号（去重队列，见 `wait_for_in_flight`）
    in_flight: Arc<Mutex<HashMap<String, Arc<InFlight>>>>,
}

/// 一次转发的入参
pub struct ForwardRequest {
    /// 客户端请求体（是否补默认 system 消息在内部判断）
    pub body: Value,
    /// 客户端是否要流式（`body.stream === true`）
    pub stream: bool,
    /// 请求体 sha256（去重键；空串表示不去重）
    pub dedupe_key: String,
    /// usage / 尝试次数的旁路槽。
    ///
    /// 为什么走「调用方建好、随请求传进来」而不是由 responder 自己造一个：
    /// 流式转发的收尾发生在 handler 返回之后（流被 axum 拉完才算结束），
    /// 那时数据必须落在**调用方仍持有**的句柄里才拿得到。调用方（记账点）
    /// 拿同一个 `Arc` 的另一份克隆，就能在流收尾时读到最终值。
    pub telemetry: Arc<usage::RequestTelemetry>,
}

/// 转发结果：要么是可直接下发的流，要么是聚合好的 JSON
pub enum ForwardOutcome {
    /// 流式：上游状态码 + SSE 帧流（已经过 reasoning 合并）
    Stream {
        status: u16,
        stream: Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send + Unpin>,
    },
    /// 非流式：聚合后的完整 JSON
    Completion { body: Value },
}

/// 一次选路的结果（对应 Node 的 `{ accountId, account, proxy, proxyError }`）
struct RouteTarget {
    account_id: Option<String>,
    /// 账号公开形态（限额事件与日志用；无账号列表时为 null）
    account: Option<Value>,
    proxy: Option<ResolvedProxy>,
    priority: Option<i64>,
}

impl UpstreamService {
    pub fn new(store: AccountStore, auth: AuthService) -> Self {
        Self { store, auth, in_flight: Arc::new(Mutex::new(HashMap::new())) }
    }

    /// 转发一次对话请求。
    ///
    /// 去重语义（对照 Node 的 waitForInFlight / trackInFlight）：
    ///   - **只等一个**在途请求完成（不是全串行）：相同 body 的第 2、3 个请求
    ///     等到第 1 个结束就各自开跑。Node 里 `waitForInFlight` 是与「当前表里的
    ///     那个 promise」赛跑，多个后来者等的是同一个 promise，语义相同。
    ///   - 等待有 45 秒上限（Node 同值），超时后照常发请求 —— 不能让用户因为
    ///     一个卡住的前序请求被无限期挂住。
    ///   - **槽位一直占到大半个响应结束**（流式请求也一样，见 InFlightGuard）。
    pub async fn forward(&self, request: ForwardRequest) -> Result<ForwardOutcome, GatewayError> {
        let slot = match self.begin_slot(&request.dedupe_key).await {
            Some(slot) => Some(slot),
            None => None,
        };
        match self.do_forward(&request, slot).await {
            Ok(outcome) => Ok(outcome),
            Err(error) => Err(error),
        }
    }

    /// 等待同 body 的在途请求完成，然后占住槽位。
    ///
    /// 返回 None 表示不需要去重（dedupe_key 为空）。
    async fn begin_slot(&self, dedupe_key: &str) -> Option<InFlightGuard> {
        if dedupe_key.is_empty() {
            return None;
        }
        self.wait_for_in_flight(dedupe_key).await;
        let signal = Arc::new(InFlight::new());
        lock_table(&self.in_flight).insert(dedupe_key.to_string(), signal.clone());
        Some(InFlightGuard {
            table: self.in_flight.clone(),
            key: dedupe_key.to_string(),
            signal,
        })
    }

    /// 等待同 body 的在途请求完成（最多 45 秒）
    async fn wait_for_in_flight(&self, key: &str) {
        let signal = lock_table(&self.in_flight).get(key).cloned();
        let Some(signal) = signal else {
            return;
        };
        logging::log("[Upstream]", "⏳ 检测到相同请求正在处理，排队等待（防重试风暴）");
        // 先注册等待、再检查完成标志：notify_waiters 只唤醒「当时已登记」的
        // 等待者，这个顺序保证「置位在前」与「置位在后」两种情况都不会漏
        let mut notified = std::pin::pin!(signal.signal.notified());
        if notified.as_mut().enable() {
            return;
        }
        if signal.is_done() {
            return;
        }
        let _ = tokio::time::timeout(Duration::from_millis(INFLIGHT_WAIT_MS), notified).await;
    }

    /// 选路循环 + 请求发送（对应 Node 的 doForwardChatCompletions）
    ///
    /// `slot` 是在途槽位凭证：流式分支把它交给 `ForwardStream`（流跑完才释放），
    /// 非流式分支与错误分支在这里自然析构（请求结束即释放）—— 与 Node 的
    /// promise 生命周期一致。
    async fn do_forward(
        &self,
        request: &ForwardRequest,
        slot: Option<InFlightGuard>,
    ) -> Result<ForwardOutcome, GatewayError> {
        // 无论客户端要不要流式，上游都必须以 stream:true 请求
        let mut upstream_body = request.body.clone();
        if let Some(object) = upstream_body.as_object_mut() {
            object.insert("stream".to_string(), Value::Bool(true));
        }
        // 上游要求首条消息是 system prompt：客户端没带时补一条兜底系统消息
        if let Some(with_system) = ensure_leading_system_message(&upstream_body) {
            upstream_body = with_system;
            logging::verbose(
                "[Upstream]",
                &format!("首条消息非 system，已注入系统消息：{DEFAULT_SYSTEM_PROMPT}"),
            );
        }
        let model = upstream_body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let payload = serde_json::to_string(&upstream_body)
            .map_err(|error| GatewayError::new(format!("请求体序列化失败: {error}")))?;
        let model_label = if model.is_empty() { "(默认)".to_string() } else { model.clone() };

        let mut tried_ids: Vec<String> = Vec::new();
        for _ in 0..=MAX_ROUTE_ATTEMPTS {
            let target = rotate::select_target_account(self, &model, &tried_ids).await?;
            let session = rotate::session_for(self, target.account_id.as_deref()).await?;
            // ── 旁路记账：本账号就是这一轮的承载账号 ──────────────
            // 每轮选路都记一次，于是 attempts = 本次请求实际用过的账号数
            // （429 降级会多走几轮，即「换了几个账号」）；同一账号内的 11128
            // 退避重试不算一轮，故不计入（口径见 usage.rs）。account 取最后
            // 一轮的值 —— 它才是真正承载本次请求的那个账号。
            // 只写旁路槽，不改本轮任何控制流：拿不到账号信息时传 None，
            // 记账点按「无账号记录（默认登录态）」处理。
            request.telemetry.note_attempt(
                target.account_id.as_deref(),
                &account_label(
                    target.account.as_ref(),
                    target.account_id.as_deref().unwrap_or(""),
                    &session,
                ),
            );
            let request_id = new_request_id();
            let url = chat_completions_url(&session, &self.auth.default_context().base_url);
            let headers = chat_headers(&session, &request_id, Some("text/event-stream"));
            logging::verbose(
                "[Upstream]",
                &format!(
                    "POST {url} model={} stream={} uid={} priority={} 出口={} msgs={}",
                    model_label,
                    request.stream,
                    session
                        .get("account")
                        .and_then(|account| account.get("uid"))
                        .and_then(Value::as_str)
                        .unwrap_or("-"),
                    target
                        .priority
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "-".to_string()),
                    describe_proxy(target.proxy.as_ref()),
                    request
                        .body
                        .get("messages")
                        .and_then(Value::as_array)
                        .map(|items| items.len().to_string())
                        .unwrap_or_else(|| "?".to_string()),
                ),
            );

            let plan = ChatRequestPlan {
                url,
                headers,
                payload: payload.clone(),
                proxy: target.proxy.clone(),
            };
            let started_at = logging::now_ms();
            let response = match rotate::request_with_waf_retry(&plan).await {
                Ok(response) => response,
                Err(error) => {
                    if let Some(account_id) = target.account_id.clone() {
                        if is_quota_limit_error(error.status_code, error.upstream_code)
                            && !tried_ids.iter().any(|id| *id == account_id)
                        {
                            tried_ids.push(account_id.clone());
                            let reset_text = rotate::mark_account_limited(self, &account_id, &model, &error);
                            let limit_at = rotate::account_limit_reset_at(self, &account_id, &model);
                            let from_label = account_label(target.account.as_ref(), &account_id, &session);
                            match rotate::pick_next_account(self, &model, &tried_ids) {
                                Some(next) => {
                                    let next_label = account_display(&next);
                                    let next_priority = next.get("priority").and_then(Value::as_i64);
                                    let reset_hint = reset_hint(&reset_text);
                                    logging::log(
                                        "[Upstream]",
                                        &format!(
                                            "⚠️ 账号 {from_label} 对模型 {model} 已限额{reset_hint}，\
                                             按优先级降级 → {next_label}（优先级 {}）",
                                            next_priority
                                                .map(|value| value.to_string())
                                                .unwrap_or_else(|| "-".to_string()),
                                        ),
                                    );
                                    rotate::report_limit_event(
                                        "warn",
                                        &format!(
                                            "账号「{from_label}」对模型 {model_label} 已限额{reset_hint}，\
                                             按优先级降级 → 「{next_label}」",
                                        ),
                                        Some(&from_label),
                                        Some(&next_label),
                                        &model,
                                        error.upstream_code,
                                        error.status_code,
                                        limit_at,
                                        next_priority,
                                    );
                                    continue;
                                }
                                None => {
                                    let message = format!(
                                        "{}（所有候选账号对模型 {model} 均已限额或禁用）",
                                        error.message
                                    );
                                    rotate::report_limit_event(
                                        "error",
                                        &format!(
                                            "模型 {model_label} 在所有候选账号均已限额或禁用，\
                                             无法继续转发（尝试过 {} 个账号）",
                                            tried_ids.len(),
                                        ),
                                        Some(&from_label),
                                        None,
                                        &model,
                                        error.upstream_code,
                                        error.status_code,
                                        limit_at,
                                        None,
                                    );
                                    return Err(GatewayError::with_status(
                                        error.status_code,
                                        message,
                                    )
                                    .with_optional_code(error.upstream_code));
                                }
                            }
                        }
                    }
                    return Err(error);
                }
            };

            // 请求成功：该账号对该模型的限额标记（如有）已失效，清除
            if let Some(account_id) = target.account_id.as_deref() {
                let had_limit = rotate::account_had_limit(self, account_id, &model);
                self.store.clear_rate_limit(account_id, &model);
                // 仅当之前确实处于限额状态才记一条，避免每次成功请求都刷日志
                if had_limit {
                    let label = account_label(target.account.as_ref(), account_id, &session);
                    rotate::report_limit_event(
                        "info",
                        &format!("账号「{label}」对模型 {model_label} 已恢复可用"),
                        None,
                        Some(&label),
                        &model,
                        None,
                        0,
                        0,
                        None,
                    );
                }
            }
            logging::verbose(
                "[Upstream]",
                &format!(
                    "上游响应 HTTP {}（{}ms）",
                    response.status().as_u16(),
                    logging::now_ms() - started_at
                ),
            );

            if request.stream {
                let status = response.status().as_u16();
                return Ok(ForwardOutcome::Stream {
                    status,
                    // 槽位交给流：流跑完 / 客户端断开 / 流被 drop 时才放行等待者
                    stream: Box::new(ForwardStream::new(
                        response,
                        slot,
                        request.telemetry.clone(),
                    )),
                });
            }
            let aggregated =
                aggregate::aggregate_sse_completion(response, request.telemetry.clone()).await?;
            let choice = aggregated.body.get("choices").and_then(|value| value.get(0));
            let content_chars = choice
                .and_then(|choice| choice.pointer("/message/content"))
                .and_then(Value::as_str)
                .map(|text| text.chars().count())
                .unwrap_or(0);
            let finish = choice
                .and_then(|choice| choice.get("finish_reason"))
                .and_then(Value::as_str)
                .unwrap_or("");
            logging::verbose(
                "[Upstream]",
                &format!(
                    "聚合完成: chunks={} content={content_chars} 字符 finish={finish}",
                    aggregated.chunk_count
                ),
            );
            return Ok(ForwardOutcome::Completion { body: aggregated.body });
        }
        Err(GatewayError::with_status(500, "上游转发重试次数超限"))
    }

}

/// 取在途表锁；锁中毒不致命（与账号存储同一策略）
fn lock_table<'a>(
    table: &'a Mutex<HashMap<String, Arc<InFlight>>>,
) -> std::sync::MutexGuard<'a, HashMap<String, Arc<InFlight>>> {
    match table.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// SSE 透传流：上游字节 → reasoning 合并 → 客户端。
///
/// ── 客户端断开如何传播（本切片的实现方式与依据）───────────────
/// 这里**不显式取消**上游请求，而是依赖 drop 传播，依据是两层实现细节：
///   1. `reqwest::Response::bytes_stream()` 返回的流持有 hyper 的 body 接收端
///      （`Incoming` 的 data channel）。该流被 drop 时接收端消失，hyper 的
///      h1 dispatch 在 `try_send_data` 上拿到 `Err(_canceled)`，执行
///      `self.conn.close_read()`（hyper 1.x `proto/h1/dispatch.rs` 里
///      「body receiver dropped before eof, closing」那段），上游 TCP 连接
///      随之关闭 —— 上游侧看到的正是「客户端断开」。
///   2. axum 在客户端断开时会把正在发送的响应体 future 丢掉（连接任务结束），
///      本流的 `poll_next` 不再被调用、随后被 drop。
/// 所以「下游断开 → 上游取消」是自动且无漏点的。
/// 与 Node 的差别：Node 用 AbortController 显式 abort（undici 主动断连），
/// 效果一致（上游都会看到断连），只是触发路径不同。
///
/// ── 上游流中断（headers 已发出）──────────────────────────────
/// Node 版此时补写一帧 `data: {"error": {...}}` + `data: [DONE]`（见
/// server.mjs 551-554）。这里做同一件事：把两帧塞进流再正常结束 ——
/// 对 OpenAI SDK 来说，这比「连接被截断」更容易识别成一次失败的补全。
pub struct ForwardStream {
    /// 上游字节流（已 consume 掉 Response，流自身是 'static）
    inner: futures::stream::BoxStream<'static, Result<Bytes, reqwest::Error>>,
    coalescer: ReasoningCoalescer,
    /// 上游已结束（不再 poll 上游，只把 pending 吐完）
    upstream_done: bool,
    /// 缓存的待下发帧（一个上游 chunk 可能产出多帧）
    pending: std::collections::VecDeque<Bytes>,
    /// 在途槽位凭证：本流被 drop 时释放（含客户端断开、上游断开两条路径）。
    /// `Option` 只是为了让它能在结构里可选传入，实际总是 Some。
    _slot: Option<InFlightGuard>,
    /// usage / 尝试次数的旁路槽：与合并器共用同一份（见 `ForwardStream::new`）
    telemetry: Arc<usage::RequestTelemetry>,
}

impl ForwardStream {
    fn new(
        response: reqwest::Response,
        slot: Option<InFlightGuard>,
        telemetry: Arc<usage::RequestTelemetry>,
    ) -> Self {
        Self {
            inner: Box::pin(response.bytes_stream()),
            coalescer: ReasoningCoalescer::with_telemetry(telemetry.clone()),
            upstream_done: false,
            pending: std::collections::VecDeque::new(),
            _slot: slot,
            telemetry,
        }
    }
}

impl Stream for ForwardStream {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use futures::StreamExt;
        loop {
            if let Some(frame) = self.pending.pop_front() {
                return std::task::Poll::Ready(Some(Ok(frame)));
            }
            if self.upstream_done {
                return std::task::Poll::Ready(None);
            }
            match self.inner.poll_next_unpin(context) {
                std::task::Poll::Pending => return std::task::Poll::Pending,
                std::task::Poll::Ready(None) => {
                    self.upstream_done = true;
                    for frame in self.coalescer.finish() {
                        self.pending.push_back(frame);
                    }
                }
                std::task::Poll::Ready(Some(Ok(bytes))) => {
                    for frame in self.coalescer.push(&bytes[..]) {
                        self.pending.push_back(frame);
                    }
                }
                std::task::Poll::Ready(Some(Err(error))) => {
                    // 上游流中断：那是**正常路径**（客户端断开、上游主动结束），
                    // 不 panic。先把已累积的 reasoning 冲刷出去，再补上
                    // 「错误帧 + [DONE]」收尾（与 Node 一致）。
                    self.upstream_done = true;
                    for frame in self.coalescer.finish() {
                        self.pending.push_back(frame);
                    }
                    let detail = crate::server::core::egress::describe_error_detail(&error);
                    let message = format!("上游流中断: {detail}");
                    logging::log("[Model]", &format!("❌ {message}"));
                    // 旁路记账：断流原因要进请求明细（客户端此时已收到部分内容，
                    // HTTP 状态早就是 200，只有这里能解释「为什么这条是失败的」）
                    self.telemetry.note_error(&message);
                    self.pending.push_back(self::sse::sse_frame(&json!({
                        "error": {
                            "message": message,
                            "type": "proxy_error",
                        }
                    })));
                    self.pending.push_back(Bytes::from_static(b"data: [DONE]\n\n"));
                }
            }
        }
    }
}

/// 会话是否带 accessToken（Node: `session?.auth?.accessToken` 真值判定）
fn has_access_token(session: &Value) -> bool {
    session
        .get("auth")
        .and_then(|auth| auth.get("accessToken"))
        .and_then(Value::as_str)
        .map(|token| !token.is_empty())
        .unwrap_or(false)
}

/// 账号展示名（Node 的 `account.name || account.id`）
fn account_display(account: &Value) -> String {
    account
        .get("name")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| account.get("id").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_default()
}

/// 限额日志里的账号文案：账号名 → 会话昵称 → 账号 id
/// （对应 Node 的 `target.account?.name || session.account?.nickname || accountId`）
fn account_label(account: Option<&Value>, account_id: &str, session: &Value) -> String {
    account
        .and_then(|account| account.get("name"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            session
                .get("account")
                .and_then(|account| account.get("nickname"))
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_else(|| account_id.to_string())
}

/// 出口的可读描述（日志用）
fn describe_proxy(proxy: Option<&ResolvedProxy>) -> String {
    match proxy {
        Some(proxy) if !proxy.label.is_empty() => proxy.label.clone(),
        Some(proxy) => proxy.host.clone(),
        None => "直连".to_string(),
    }
}

/// 账号公开形态里的优先级（日志里打的 `priority=N`）
fn priority_of(account: &Value) -> Option<i64> {
    account.get("priority").and_then(Value::as_i64)
}

/// 限额文案里的恢复时间片段：`（<时间> 恢复）`，空串表示没有明确恢复时间
fn reset_hint(reset_text: &str) -> String {
    if reset_text.is_empty() {
        String::new()
    } else {
        format!("（{reset_text} 恢复）")
    }
}

/// 恢复时间的本地化展示（对应 Node 的 `toLocaleString('zh-CN', { hour12:false })`）。
/// 与 errors.rs 的 reset_at_text 同口径：统一按 UTC+8 渲染，
/// 因为上游给的恢复时间本身就是 UTC+8 标定的，用本机时区会让用户对不上原文。
fn format_reset_text(reset_at: f64) -> String {
    if !(reset_at > 0.0) {
        return String::new();
    }
    let Some(utc) = chrono::DateTime::from_timestamp_millis(reset_at as i64) else {
        return String::new();
    };
    // 固定偏移 +8 一定能构造成功；万一失败就给空串（这条只是日志文案，
    // 绝不能因为一个格式化失败把整个请求带崩 —— release 是 panic=abort）
    let Some(offset) = chrono::FixedOffset::east_opt(8 * 3600) else {
        return String::new();
    };
    utc.with_timezone(&offset)
        .format("%Y/%m/%d %H:%M:%S")
        .to_string()
}
