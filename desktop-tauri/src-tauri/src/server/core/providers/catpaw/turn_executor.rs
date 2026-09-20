//! CatPaw **轮次执行层**：turn 请求的传输、SSE 消费到底、以及轮次收尾。
//!
//! ── 与 `conversation.rs` 的分工 ──────────────────────────────
//! `conversation.rs` 管**决策与请求体**（轮次判定三规则、round/turn 的 body
//! 构造），本文件管**执行与收尾**：
//!
//! ```text
//! conversation.rs                          turn_executor.rs
//!   判定模式 → round → event(running)        发 turn 请求（SSE）
//!   → build turn body ────────────────────→ 逐 chunk 解析上游事件
//!                                             → openai.rs 翻译成 chunk
//!                                             → 读到底（硬约束 2）
//!                                             → 注册表写回 / 终态上报
//! ```
//!
//! 拆文件的唯一原因是单文件行数约定（架构文档 §8 第 1 条）。
//!
//! ── 为什么流式分支要起一个后台任务（而不是把 SSE 包成 Stream）──
//! §9.3 的硬约束要求「turn 的 SSE **必须消费到服务端关闭连接**」，而 `[DONE]`
//! 与 finish_reason 只能在**整个流读完之后**才下发（`message.finished=true`
//! ≠ turn 结束）。若把上游字节流直接用 `Stream` 适配器转发，客户端一断开流就被
//! drop，上游那次 turn 就再也没人读完 —— 上游侧停在「执行中」，下一轮 round
//! 直接被拒。
//!
//! 因此这里的结构是：起一个后台任务**独立地**把上游 SSE 读到底。客户端断开只
//! 表现为「发送失败」，消费继续，读完照常收尾（注册表写回 + 终态上报 +
//! 尽力下发 `[DONE]`）。代价是多一个任务与一条通道，收益是「客户端断开也不会
//! 把上游会话搞卡」。
//!
//! ── 失败要进记账旁路（W5 修复）───────────────────────────────
//! HTTP 头早就发出去了，客户端的 `RecordingStream` 只按「流跑到 None 且
//! error 为空」判成功，所以协议层必须在关闭发送通道**之前**把失败原因写进
//! `RequestTelemetry::note_error`（首次为准）：不写的话，上游错误帧、翻译错误、
//! 提前 EOF 都会在请求日志里被记成一次成功。真实的上游/翻译失败与「客户端
//! 取消」必须分开：取消走 `interrupt` 且不记 error（那是用户行为，不是上游
//! 故障）。「返回工具调用、等待客户端续接」不是失败，不记。
//! 失败路径只报 usage/error 与一次 attempts（编排层已记），**不杜撰 token**。
//!
//! ── 下行帧为什么先攒在 `Vec` 里再 flush ──────────────────────
//! 翻译层的写入是同步的（一个上游事件可能产出多帧），而通道发送是异步的。
//! `try_send` 在通道满时会**丢帧**（客户端看到内容缺口，不可接受）；所以同步侧
//! 只往 `Vec<Bytes>` 里推，每处理完一个上游 chunk 再 async flush 一次 ——
//! 既有背压（不丢帧），又不必把翻译层的签名变成异步。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：零 unwrap/expect/panic；持锁不发网络请求
//! （注册表的调用都是「取副本 → 释放锁 → 再出网」）。

use std::sync::Arc;

use bytes::Bytes;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::server::core::egress;
use crate::server::core::proxies::ResolvedProxy;
use crate::server::core::upstream::usage::RequestTelemetry;
use crate::server::logging;

use super::conversation::{short_id, CatPawCredentials};
use super::upstream_http::{report_terminal, stop_turn};
use super::fingerprint::message_fingerprint;
use super::models::CatPawError;
use super::openai::{SseEvent, SseReader, TurnResult, TurnTranslator, DONE_FRAME};
use super::registry::{SessionRecord, SessionRegistry};
use super::tools::ToolChoice;

/// 一次 turn 的执行上下文（`conversation.rs` 装配好交给本文件）
#[derive(Clone)]
pub(super) struct TurnContext {
    pub base_url: String,
    pub credentials: CatPawCredentials,
    pub proxy: Option<ResolvedProxy>,
    /// 面向客户端的响应 id（`chatcmpl-…`）
    pub chat_id: String,
    /// 客户端请求的模型名（进 chunk 的 `model` 字段）
    pub model_name: String,
    /// 客户端是否要 usage 帧（`stream_options.include_usage`）
    pub include_usage: bool,
    /// 本轮的 tool_choice（收尾校验用）
    pub choice: ToolChoice,
    /// 账号 id（注册表的账号维度与记账用）
    pub account_id: String,
    /// 上游数字模型 ID（注册表按它判「换模型就重建」）
    pub model_type: i64,
    /// 客户端 `x-session-id`（空 = 无长会话语义）
    pub session_id: String,
    /// 是否长会话（决定要不要把映射写回注册表）
    pub persistent: bool,
    /// 本轮最终使用的 conversationId（自愈重试会换成新的，以守卫为准）
    pub conversation_id: String,
    /// 本轮的 turnRequestId（`turn/stop` 要带上它）
    pub turn_request_id: String,
    pub telemetry: Option<Arc<RequestTelemetry>>,
    /// 全局注册表（避免每处都写 `conversation::registry()`）
    pub registry: &'static SessionRegistry,
}

/// 成功轮次要写回注册表的指纹链（原实现 `catpaw-upstream-client.mjs` 第 529-533 行）。
///
/// 组成 = **旧链**（命中会话时它已有的）+ 本轮**实际提交**的消息指纹 +
/// 上游返回的那条消息指纹。
///
/// 「实际提交的」必须与发出去的一致：长会话只提交增量、工具续接只提交那条
/// tool 消息 —— 多算会让下一次 `locate_increment` 在客户端历史里定位到错误的
/// 位置（增量从错误的地方开始，上游看到的历史就断层了）。
pub(super) struct HistoryFingerprints {
    prefix: Vec<String>,
    submitted: Vec<String>,
}

impl HistoryFingerprints {
    pub(super) fn new(prefix: Vec<String>, submitted: Vec<String>) -> Self {
        Self { prefix, submitted }
    }

    /// 补上上游返回消息的指纹，得到最终要登记的链
    fn chain(&self, response_message: &Value) -> Vec<String> {
        let mut chain = self.prefix.clone();
        chain.extend(self.submitted.iter().cloned());
        chain.push(message_fingerprint(response_message));
        chain
    }
}

// ─── 收尾责任（硬约束 1 的落点）─────────────────────────────────

/// 一次会话的收尾凭证（原实现 `try/catch/finally` 在 Rust 里的形态）。
///
/// ── 谁负责什么 ──────────────────────────────────────────────
///   - `Drop` **只释放 inflight 占用**（同步内存操作，Drop 里做网络请求是错的）；
///   - 正常/失败/取消三个终态由调用方显式 `close(status)` 上报；
///   - 流被 drop（任务被提前结束）而来不及显式收尾时，`Drop` 起一个后台任务补
///     `turn/stop` + 回报 `canceled` —— §9.3「失败/打断路径也要回报 canceled」
///     的最后一道网。
pub(super) struct FinishGuard {
    pub base_url: String,
    pub credentials: CatPawCredentials,
    pub proxy: Option<ResolvedProxy>,
    /// conversationId（自愈重试时会换成新的，所以是可变的）
    pub conversation_id: String,
    pub turn_request_id: String,
    session_id: String,
    /// 是否占用了 inflight 标记
    inflight: bool,
    /// round + event(running) 是否都成功过（决定要不要报终态）
    active: bool,
    /// 是否已显式收尾（幂等）
    closed: bool,
    /// 是否已完成本轮交接（返回工具调用或已报终态）。
    ///
    /// ── 与 `closed` 的区别（这个字段是必需的）────────────────────
    /// `closed` 只在**报了终态**时为真；而「返回工具调用」这条路径既不能报
    /// 终态（上游还在等 tool 结果），也不能走 `Drop` 的兜底取消。两者合用一个
    /// 标志会让 `Drop` 无法区分「被提前中断」与「正常交接完成」。
    settled: bool,
    registry: &'static SessionRegistry,
}

impl FinishGuard {
    pub(super) fn new(ctx: &TurnContext, inflight: bool) -> Self {
        Self {
            base_url: ctx.base_url.clone(),
            credentials: ctx.credentials.clone(),
            proxy: ctx.proxy.clone(),
            conversation_id: ctx.conversation_id.clone(),
            turn_request_id: ctx.turn_request_id.clone(),
            session_id: ctx.session_id.clone(),
            inflight,
            active: false,
            closed: false,
            settled: false,
            registry: ctx.registry,
        }
    }

    /// 上游对话执行已开始（round + event(running) 成功）—— 此后失败都要报终态
    pub fn mark_active(&mut self) {
        self.active = true;
    }

    /// 报一个终态并收尾（幂等；`event` 上报失败不影响结果）
    pub async fn close(&mut self, status: &str, error: Option<&CatPawError>) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.settled = true;
        if self.active {
            report_terminal(
                &self.base_url,
                &self.credentials,
                self.proxy.as_ref(),
                &self.conversation_id,
                status,
                error,
            )
            .await;
        }
        self.release();
    }

    /// **只**放开占用，并标记本轮「已交接」——用于「返回工具调用、等客户端续接」
    /// 这条路径：本轮不报终态（上游语义里这一轮还没结束），但也绝不能触发
    /// `Drop` 的兜底取消（那会给上游发一个错误的 canceled，让客户端接下来提交的
    /// tool 结果无处可去）。
    pub fn release(&mut self) {
        self.settled = true;
        if self.inflight && !self.session_id.is_empty() {
            self.registry.release_inflight(&self.session_id);
            self.inflight = false;
        }
    }

    /// 打断当前轮次（客户端断开时调）
    pub async fn interrupt(&self) {
        stop_turn(
            &self.base_url,
            &self.credentials,
            self.proxy.as_ref(),
            &self.conversation_id,
            &self.turn_request_id,
        )
        .await;
    }
}

impl Drop for FinishGuard {
    fn drop(&mut self) {
        // ① 先看本轮有没有正常交接完（`release` 会把 settled 置真，所以要**先**判）
        let spurious = self.closed || self.settled || !self.active;
        // ② 占用必须释放（同步、无网络）
        self.release();
        if spurious {
            return;
        }
        // ③ 没显式收尾就走到 Drop：起后台任务补「打断 + 回报 canceled」。
        //    上游硬约束要求 conversation 不能停在「执行中」。
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let base_url = self.base_url.clone();
        let credentials = self.credentials.clone();
        let proxy = self.proxy.clone();
        let conversation_id = self.conversation_id.clone();
        let turn_request_id = self.turn_request_id.clone();
        logging::verbose(
            "[CatPaw]",
            &format!(
                "轮次 {} 未显式收尾（流被中断？），后台补 canceled",
                short_id(&conversation_id),
            ),
        );
        handle.spawn(async move {
            stop_turn(&base_url, &credentials, proxy.as_ref(), &conversation_id, &turn_request_id)
                .await;
            report_terminal(
                &base_url,
                &credentials,
                proxy.as_ref(),
                &conversation_id,
                "canceled",
                None,
            )
            .await;
        });
    }
}

/// 把一次**真实**的上游/翻译失败写进记账旁路。
///
/// ── 为什么必须写、且必须同步写完 ─────────────────────────────
/// HTTP 头（200）早就发出去了，客户端那条 `RecordingStream` 只能靠
/// `RequestTelemetry::error` 判断这次请求是不是失败 —— 不写就会被记成成功。
/// 而这里的写必须**先于任何 await**：`guard.close(...)` 里有终态上报的网络请求，
/// 发送通道（`sender`）也要等收尾全部结束才析构，客户端看到的 EOF 在那之后；
/// 一旦收尾途中任务被打断，晚写的错误会连同收尾一起消失，而它是客户端侧判定
/// 「这次不是成功」的唯一依据（见 `InterruptedNote` 的兜底）。
/// 因此本函数只在上游失败分支调用，调用点先于错误帧下发（见 `finish_stream`）。
///
/// ── 为什么只报 error 不报 attempts/token ─────────────────────
/// attempts 是编排层的职责（`provider_loop::attempt_stateful` 已记一次，见
/// `report_telemetry` 的说明），协议层再记会让同一次尝试翻倍。失败时也不补
/// token：失败请求的 token 由存储层归一成 0，杜撰一个数字只会让报表失真。
///
/// `note_error` 首次为准且空串忽略，所以这里可以放心地按「每条失败路径各调
/// 一次」来写，不会互相覆盖。摘要直接用 `CatPawError` 的文案（它已含「上游…」
/// 之类来源信息，与无状态路径 `ForwardStream` 的 note_error 口径一致）。
fn note_stream_error(ctx: &TurnContext, error: &CatPawError) {
    let Some(telemetry) = &ctx.telemetry else {
        return;
    };
    telemetry.note_error(&error.message);
}

// ─── SSE 消费（硬约束 2 的落点）─────────────────────────────────

/// 收尾未完成时的兜底记账（见 `drive_stream` 的调用点）。
///
/// ── 为什么需要它（覆盖哪段窗口）─────────────────────────────
/// 后台任务在 `drive_stream` 跑完之前被 Drop（运行时停机等）时，收尾里的
/// `note_stream_error` 不会执行，客户端侧只会看到一次普通的 EOF（HTTP 早就是
/// 200）—— 没有这条同步兜底，明细里就会留下一条「成功」。
///
/// 解除时机与结果绑定（见 `drive_stream` 的两处 `armed`）：
///   - 结果 `Ok`（正常完成）→ 进 `finish_stream` 之前就解除：本轮已经完整下发，
///     收尾里若恰好被停机打断，不该把一次成功请求记成失败；
///   - 结果 `Err` 且客户端已断开（取消语义）→ 同样解除：取消是用户行为，
///     本层不写错误，明细里由 `RecordingStream` 的中断摘要解释；
///   - 结果 `Err` 且客户端还在（上游/翻译失败）→ 保持武装到收尾返回：真实错误
///     由 `note_stream_error` 在第一个 await 之前落笔（首次为准），收尾途中被
///     中断则由这条兜底补一个通用原因。
///
/// ── 为什么它一定早于主流的 EOF ───────────────────────────────
/// `interrupted` 是局部变量、`sender` 是函数参数，Rust 的 drop 顺序里局部先于
/// 参数析构，所以 note_error 必然发生在发送通道关闭（客户端看到 EOF）之前。
///
/// 只持有 `Arc` 克隆而不是 `&TurnContext`：本结构要跨 await 存活，克隆让
/// `drive_stream` 的 Send 推导不必依赖 `TurnContext: Sync`。`RequestTelemetry`
/// 是纯内存操作，Drop 里写它是安全的（Drop 里不发网络请求是 `FinishGuard` 的
/// 同一条纪律）；`note_error` 首次为准，与 `finish_stream` 已写的错误不会打架。
struct InterruptedNote {
    telemetry: Option<Arc<RequestTelemetry>>,
    /// 为真 = 本轮还没收尾完（`drive_stream` 正常走完时置假）
    armed: bool,
}

impl Drop for InterruptedNote {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Some(telemetry) = &self.telemetry else {
            return;
        };
        telemetry.note_error("后台任务中断（本轮未正常收尾）");
    }
}

/// 流式分支：把上游 SSE 翻译成 OpenAI chunk 交给 `sender`，读到底后收尾。
///
/// 没有返回值：所有失败都已经转成客户端可见的**流内错误帧**（HTTP 头早发出去了）
/// 或终态上报，没有别的出口。
pub(super) async fn drive_stream(
    ctx: TurnContext,
    response: reqwest::Response,
    sender: mpsc::Sender<Result<Bytes, std::io::Error>>,
    mut guard: FinishGuard,
    history: HistoryFingerprints,
) {
    let mut translator = TurnTranslator::new(
        ctx.chat_id.clone(),
        ctx.model_name.clone(),
        ctx.include_usage,
        ctx.choice.clone(),
    );
    // 首帧 role：客户端据此建立 assistant 消息（原实现 `catpaw-upstream-openai.mjs`
    // 在进入事件循环前先 send 一次）
    let mut pending: Vec<Bytes> = vec![translator.role_frame()];
    let mut downstream = true;
    // 收尾未完成时的兜底（见 `InterruptedNote`）：读流期间被 Drop 时，
    // 收尾里的 note_stream_error 不会执行，客户端只会看到一次 EOF
    let mut interrupted = InterruptedNote { telemetry: ctx.telemetry.clone(), armed: true };

    let mut reader = SseReader::new();
    let mut stream = response.bytes_stream();
    let mut outcome: Result<TurnResult, CatPawError> =
        Err(CatPawError::upstream("上游没有返回消息"));
    let mut finished = false;
    while !finished {
        match futures::StreamExt::next(&mut stream).await {
            // ── 服务端关闭连接 = turn 真正结束（硬约束 2）──────────────
            None => {
                if let Some(event) = reader.finish() {
                    if let Err(error) = handle_event(&mut translator, event, &mut pending) {
                        outcome = Err(error);
                        flush(&sender, &mut pending, &mut downstream).await;
                        break;
                    }
                }
                outcome = translator.finish(&mut pending);
                finished = true;
            }
            Some(Err(error)) => {
                outcome = Err(CatPawError::upstream(format!(
                    "上游流中断: {}",
                    egress::describe_error_detail(&error),
                )));
                finished = true;
            }
            Some(Ok(chunk)) => {
                for event in reader.push(&chunk) {
                    if let Err(error) = handle_event(&mut translator, event, &mut pending) {
                        outcome = Err(error);
                        finished = true;
                        break;
                    }
                }
            }
        }
        flush(&sender, &mut pending, &mut downstream).await;
        if !downstream {
            // 客户端断了：继续消费（硬约束 2），但不再攒帧
            pending.clear();
        }
    }
    // 收尾前决定是否保留兜底（三种情形见 `InterruptedNote`）：成功、或客户端
    // 已经先断开（取消语义，本层不记错误）都立刻解除；只有「上游/翻译失败且
    // 客户端还在」才需要它兜住收尾途中被中断。
    if outcome.is_ok() || !downstream {
        interrupted.armed = false;
    }
    finish_stream(&ctx, &mut guard, &history, outcome, &sender, &mut pending, &mut downstream).await;
    interrupted.armed = false;
}

/// 非流式分支：消费完 SSE 聚合成完整 `chat.completion`。
pub(super) async fn drive_aggregate(
    ctx: TurnContext,
    response: reqwest::Response,
    mut guard: FinishGuard,
    history: HistoryFingerprints,
) -> Result<Value, CatPawError> {
    let mut translator = TurnTranslator::new(
        ctx.chat_id.clone(),
        ctx.model_name.clone(),
        ctx.include_usage,
        ctx.choice.clone(),
    );
    // 非流式没有下游：帧只攒不问。翻译层的差分逻辑仍然要跑（tool_calls 的增量
    // 累积、finish_reason 与 usage 都在收尾时产出）
    let mut pending: Vec<Bytes> = Vec::new();

    let mut reader = SseReader::new();
    let mut stream = response.bytes_stream();
    let mut outcome: Result<TurnResult, CatPawError> =
        Err(CatPawError::upstream("上游没有返回消息"));
    let mut finished = false;
    while !finished {
        match futures::StreamExt::next(&mut stream).await {
            None => {
                if let Some(event) = reader.finish() {
                    if let Err(error) = handle_event(&mut translator, event, &mut pending) {
                        outcome = Err(error);
                        break;
                    }
                }
                outcome = translator.finish(&mut pending);
                finished = true;
            }
            Some(Err(error)) => {
                outcome = Err(CatPawError::upstream(format!(
                    "上游流中断: {}",
                    egress::describe_error_detail(&error),
                )));
                finished = true;
            }
            Some(Ok(chunk)) => {
                for event in reader.push(&chunk) {
                    if let Err(error) = handle_event(&mut translator, event, &mut pending) {
                        outcome = Err(error);
                        finished = true;
                        break;
                    }
                }
                // 聚合分支不需要帧，只保留翻译层内部的累积状态
                pending.clear();
            }
        }
    }

    let result = match outcome {
        Ok(result) => result,
        Err(error) => {
            guard.close("failed", Some(&error)).await;
            return Err(error);
        }
    };
    write_back(&ctx, &guard, &history, &result);
    if result.tool_calls.is_empty() {
        // 轮次正常结束：必须回报 completed（硬约束 1），否则下一轮 round 被拒
        guard.close("completed", None).await;
    } else {
        // 与非流式分支同一条例外：返回工具调用时这一轮在上游语义里还没结束
        // （客户端马上带着 tool 结果回来续接），因此不报终态、只释放占用。
        // 少这一步会让上游以为本轮已完，客户端随后提交的 tool 结果续接失败。
        logging::verbose(
            "[CatPaw]",
            &format!(
                "轮次 {} 返回 {} 个工具调用，等待客户端续接（不报 completed）",
                short_id(&guard.conversation_id),
                result.tool_calls.len(),
            ),
        );
        guard.release();
    }
    Ok(completion_body(&ctx, &result))
}

/// 处理一个上游事件：`Failed` 直接抛出，`Data` 交给翻译层
fn handle_event(
    translator: &mut TurnTranslator,
    event: SseEvent,
    pending: &mut Vec<Bytes>,
) -> Result<(), CatPawError> {
    match event {
        SseEvent::Data(value) => translator.consume(&value, pending),
        SseEvent::Failed(error) => Err(error),
    }
}

/// 把攒下的帧发给客户端；发送失败说明客户端已经断开（`downstream = false`）
async fn flush(
    sender: &mpsc::Sender<Result<Bytes, std::io::Error>>,
    pending: &mut Vec<Bytes>,
    downstream: &mut bool,
) {
    for frame in pending.drain(..) {
        if sender.send(Ok(frame)).await.is_err() {
            *downstream = false;
            return;
        }
    }
}

/// 收尾一条流式轮次：注册表写回 + 终态上报 + `[DONE]`。
async fn finish_stream(
    ctx: &TurnContext,
    guard: &mut FinishGuard,
    history: &HistoryFingerprints,
    outcome: Result<TurnResult, CatPawError>,
    sender: &mpsc::Sender<Result<Bytes, std::io::Error>>,
    pending: &mut Vec<Bytes>,
    downstream: &mut bool,
) {
    match outcome {
        Ok(result) => {
            write_back(ctx, guard, history, &result);
            if result.tool_calls.is_empty() {
                // 轮次正常结束：必须回报 completed（硬约束 1），否则下一轮 round 被拒
                guard.close("completed", None).await;
            } else {
                // 例外：返回了工具调用 —— 这一轮在上游语义里**还没结束**（客户端
                // 马上带着 tool 结果回来续接），因此不报终态、只释放占用
                logging::verbose(
                    "[CatPaw]",
                    &format!(
                        "轮次 {} 返回 {} 个工具调用，等待客户端续接（不报 completed）",
                        short_id(&guard.conversation_id),
                        result.tool_calls.len(),
                    ),
                );
                guard.release();
            }
            pending.push(Bytes::from_static(DONE_FRAME));
        }
        Err(error) => {
            // 取消判定必须**先于**本函数末尾的 flush：同一批次里的发送失败若在
            // 这里之后才被观察到，会把已经发生的上游错误误判成客户端断开
            let canceled = !*downstream;
            logging::log(
                "[CatPaw]",
                &format!(
                    "轮次中断（{}）: {}",
                    if canceled { "客户端断开" } else { "上游/翻译失败" },
                    error.message,
                ),
            );
            if canceled {
                // 客户端先走一步：打断上游轮次再回报 canceled（原实现 catch 分支）。
                // 客户端取消是用户行为、不是上游故障，**不写**记账旁路 ——
                // 明细里由 `RecordingStream` 的中断摘要解释「为什么没有 token」。
                guard.interrupt().await;
                guard.close("canceled", None).await;
            } else {
                // 先写旁路再下发错误帧：HTTP 200 早发出去了，客户端侧只认 error
                // 字段（这是唯一失败依据）；而写旁路是同步的，必须排在下面的
                // `guard.close(...)`（含终态上报的 await）之前，否则收尾途中被打断
                // 时错误会丢（见 `note_stream_error` 与 `InterruptedNote`）
                note_stream_error(ctx, &error);
                pending.push(stream_error_frame(ctx, &error));
                pending.push(Bytes::from_static(DONE_FRAME));
                guard.close("failed", Some(&error)).await;
            }
        }
    }
    flush(sender, pending, downstream).await;
}

/// 流内错误帧（原实现 `streamErrorChunk`：`delta:{}` + `finish_reason:"stop"`
/// + `error` 字段）—— HTTP 头早就发出去了，只能这样告诉客户端这次补全失败了。
fn stream_error_frame(ctx: &TurnContext, error: &CatPawError) -> Bytes {
    let translator = TurnTranslator::new(
        ctx.chat_id.clone(),
        ctx.model_name.clone(),
        false,
        ToolChoice::Auto,
    );
    translator.error_frame(&error.message, error.status)
}

/// 非流式响应体（原实现 `catpaw-upstream-openai.mjs` 的非流式分支）
fn completion_body(ctx: &TurnContext, result: &TurnResult) -> Value {
    json!({
        "id": ctx.chat_id,
        "object": "chat.completion",
        "created": logging::now_ms() / 1000,
        "model": ctx.model_name,
        "choices": [{
            "index": 0,
            "message": result.openai_message,
            "finish_reason": result.finish_reason(),
        }],
        "usage": result.final_usage(),
    })
}

// ─── 注册表写回（原实现 `registerClientSession` / `registerClientToolSession`）──

/// 成功轮次后的状态写回。
///
/// ── 四种组合（与原实现逐条对齐）──────────────────────────────
/// | 有没有工具调用 | 是不是长会话 | 写什么 |
/// |---|---|---|
/// | 有 | 任意 | 「等工具结果」记录（`pending_call_ids` → call 索引） |
/// | 无 | 长会话 | 覆盖登记长会话记录（指纹链前移、清空待响应） |
/// | 无 | 无状态/并发冲突 | **不登记** |
///
/// ── 为什么「无工具调用的无状态请求」一条都不写 ─────────────────
/// 原实现这一条路径是 `registry.forgetClientToolSession(session)`（清），也就是
/// 「什么都不留下」。无状态请求（客户端标题/摘要类）不读不写映射是 §9.3 的硬
/// 约束：写了会让主对话的下一次 round 拿到一份错误的历史。
fn write_back(
    ctx: &TurnContext,
    guard: &FinishGuard,
    history: &HistoryFingerprints,
    result: &TurnResult,
) {
    log_turn_result(result);
    report_telemetry(ctx, result);
    let pending = call_ids(&result.tool_calls);
    // 并发冲突时走的是独立 conversation：**不写映射**（写进去会让下一个同
    // session 请求错误地续接到这条并行历史），与无状态请求同待遇
    let session_id = if ctx.persistent { ctx.session_id.clone() } else { String::new() };
    if session_id.is_empty() && pending.is_empty() {
        return;
    }
    let now = logging::now_ms();
    let conversation_id = if result.conversation_id.is_empty() {
        guard.conversation_id.clone()
    } else {
        result.conversation_id.clone()
    };
    ctx.registry.register(SessionRecord {
        conversation_id,
        fingerprints: history.chain(&result.message),
        model_type: ctx.model_type,
        account_id: ctx.account_id.clone(),
        created_at: now,
        last_active_at: now,
        inflight: false,
        session_id,
        // 等工具结果时要带上「哪个 turn 在跑」：下一轮若发现会话仍在等待，
        // 要先 stop 掉它才能 round（§9.3 第二处 turn/stop 用途）
        turn_request_id: if pending.is_empty() { None } else { Some(guard.turn_request_id.clone()) },
        pending_call_ids: pending,
    });
}

/// 一轮结果的**元数据日志**（`TurnResult::text` / `reasoning` 的唯一消费点）。
///
/// ── 为什么只打长度不打正文（隐私口径）─────────────────────────
/// 与注册表 `log_removal` 同一条纪律（UPSTREAM_PROTOCOL §8）：日志里只留
/// 「这一轮产出了多少内容」这种可对照的量，不落用户的消息正文与思考内容。
/// 长度足以排障（比如「升级后正文恒为 0」= 翻译层映射断了），
/// 而正文进日志只会带来泄露风险与刷屏。
fn log_turn_result(result: &TurnResult) {
    logging::verbose(
        "[CatPaw]",
        &format!(
            "轮次产出 content={} 字符 reasoning={} 字符 toolCalls={}",
            result.text.chars().count(),
            result.reasoning.chars().count(),
            result.tool_calls.len(),
        ),
    );
}

/// usage 旁路（记账点用；不影响下发内容）
///
/// ── 只报 usage，不报 attempts（W5-T-d4 修正）───────────────────
/// `note_attempt` 是**编排层**的职责：`provider_loop::attempt_stateful` 在每轮
/// 账号选路后调它一次（口径与无状态路径逐字相同 —— 「换了几个账号」，且带
/// provider id）。协议层再报一次会让同一次上游尝试被记两遍，报表里的
/// attempts 直接翻倍。W4b-T-d3 时期协议层是唯一的调用点（那时编排层还没有
/// 有状态分派），接线后必须让出去 —— 这里只留 usage 上报。
fn report_telemetry(ctx: &TurnContext, result: &TurnResult) {
    let Some(telemetry) = &ctx.telemetry else {
        return;
    };
    telemetry.report_usage(&result.final_usage());
}

/// tool_calls → 待响应 id 列表
fn call_ids(tool_calls: &[Value]) -> Vec<String> {
    tool_calls
        .iter()
        .filter_map(|call| call.get("id").and_then(Value::as_str))
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .collect()
}
