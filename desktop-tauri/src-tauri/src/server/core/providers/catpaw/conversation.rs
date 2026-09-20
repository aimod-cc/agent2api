//! CatPaw **轮次状态机**：round → event(running) → turn(SSE) → 工具循环 → event(completed)。
//!
//! ── 这个模块存在的理由（与其它 provider 的本质差别）────────────
//! WorkBuddy / 小浣熊是「一次 HTTP 请求 = 一次对话」；CatPaw 的上游是一套
//! **有状态会话协议**（UPSTREAM_PROTOCOL.md 是唯一权威），一次客户端请求会
//! 变成好几个上游请求，中间还要维护「x-session-id → conversationId」映射与
//! 已同步消息的指纹链。时序与状态因此不能塞进 `build_chat_request` 那种
//! 单请求契约（架构文档 §4.2.1），本文件就是那份时序的实现。
//!
//! ```text
//! 全新会话         round(全量)   → event(running) → turn(user)     → … → event(completed)
//! 长会话新轮次     round(增量)   → event(running) → turn(user)     → … → event(completed)
//! 工具续接         （不 round）  →                  turn(tool 结果) → …
//! ```
//!
//! ── 三条轮次判定规则（架构文档 §9.2；**顺序勿改**）─────────────
//!   1. 历史末尾是 `assistant(tool_calls) + tool 结果` 且 `tool_call_id` 命中
//!      注册表 → **工具续接**：turn 提交 tool 结果，不 round、不重复 event(running)；
//!   2. 请求带 `x-session-id` 且注册表存在映射 → **长会话新轮次**：复用
//!      conversationId，round 只提交增量（`fingerprint::locate_increment`）；
//!   3. 其余 → **全新会话**：新 conversationId，round 提交全量。
//!
//! ── 三条硬约束（上游行为特性，违反会导致会话卡死或 504，§9.3）──
//!   1. **每个轮次结束必须回报 event(completed)**，否则 conversation 停在上游的
//!      「执行中」状态，下一轮 round 会被拒绝（`会话正在执行中，无法创建新轮次`）。
//!      失败/打断路径回报 failed / canceled（见 `turn_executor::FinishGuard`）；
//!      例外：本轮返回工具调用时**不报** completed —— 那一轮在上游语义里还没
//!      结束，客户端马上会带着 tool 结果回来续接（原实现同）。
//!   2. turn 的 SSE 必须消费到**服务端关闭连接**（`message.finished=true`
//!      ≠ turn 结束），这条在 `turn_executor.rs` 里落实。
//!   3. 同一 `x-session-id` 已有流式请求在跑 → 新请求走**独立 conversation**，
//!      结束释放占用；`stream:false` 的辅助请求（标题/摘要）**不读不写**会话映射。
//!
//! ── 与原实现的三处有意差异（报告里已写明）────────────────────
//!   1. 不搬运会话里那几份「上一轮的副本」（systemPrompt、rulesMessage、
//!      toolConfigs、toolCatalog、contextWindow）：客户端每次请求都会重发这些
//!      内容（OpenAI 语义是无状态的），用**本次请求的值**比用上一轮的副本更准。
//!      工具名回填同理 —— 从本次请求历史里那条 assistant 消息取。
//!   2. 诊断日志（原实现 `writeDiagnostic` JSONL）明确不迁移（§9.4），
//!      改为 `logging::verbose("[CatPaw]", …)` 的关键节点日志。
//!   3. round 被上游以「会话正在执行中」拒绝时**自愈重试一次**（新 conversation
//!      全量 round，见 [`submit_round_with_self_heal`]）—— 原实现只依赖下一轮先
//!      `turn/stop`；这里多一层兜底，因为网关注册表可能因进程重启/TTL 淘汰而
//!      丢失「上一轮还在跑」的信息，那条旧 conversation 就再也没人停得掉。
//!
//! ── 文件分工（单文件行数约定，架构文档 §8 第 1 条）─────────────
//!   本文件          判定 + 请求体构造 + round/event 编排 + 公开入口
//!   prepare.rs       入站归一化（消息 / 工具 / 模型参数 / 图片压缩）
//!   turn_executor.rs turn 传输、SSE 消费到底、轮次收尾与注册表写回
//!   openai.rs        上游 SSE 事件 → OpenAI chunk 的翻译与 usage 修正
//!   models.rs        modelType / effort / context 的映射（入站参数）
//!   tools.rs         tools / tool_choice → toolConfigs 的归一化（入站参数）
//!
//! ── 硬约束（代码纪律）────────────────────────────────────────
//! release 是 `panic=abort`：零 unwrap/expect/panic；持锁期间不发网络请求
//! （注册表的调用都是「取副本 → 释放锁 → 再出网」）。

use std::sync::{Arc, OnceLock};

use axum::http::HeaderMap;
use serde_json::{json, Value};

use crate::server::core::proxies::ResolvedProxy;
use crate::server::core::upstream::usage::RequestTelemetry;
use crate::server::core::upstream::ForwardOutcome;
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::decision::{decide, submitted_fingerprints, Decision, TurnMode};
use super::fingerprint::fingerprints_for;
use super::models::CatPawError;
use super::openai::{include_usage, new_chat_id};
use super::prepare::{prepare, Prepared};
use super::registry::{AccountIdentity, InvalidationReason, SessionRegistry};
use super::turn_executor::{FinishGuard, HistoryFingerprints, TurnContext};
use super::upstream_http::{
    post_json, report_status, report_terminal, stop_turn, turn_request, REQUEST_TIMEOUT_MS,
};
use super::{DEFAULT_BASE_URL, DEFAULT_MODE, DEFAULT_SOURCE, DEFAULT_TOOL_VERSION};

/// 上游 `permissionMode` 的固定取值（原实现写死在 round/turn 请求体里）
const PERMISSION_MODE: &str = "unsafeBypassPermissions";

/// 进程级会话注册表。
///
/// ── 为什么是全局单例而不是适配器字段 ─────────────────────────
/// 会话映射必须**跨请求共享**；放全局与放适配器字段是同一件事，但前者能让
/// [`run_conversation`] 保持一个纯粹的入参形态（调用方不需要先拿到适配器实例）。
/// `ProviderAdapter` 的实现本来就是 `&'static` 无状态单例（见 `adapter.rs`），
/// 而 `SessionRegistry::new()` 不是 const 构造函数，所以用 `OnceLock` 惰性建。
///
/// ⚠️ W5 接线注意：账号切换必须调 `registry().clear_account(..)` / `clear_all()`
/// —— conversationId 是上游账号上下文里的对象，换账号后续接必然失败
/// （见 `registry/mod.rs` 的模块说明）。
pub fn registry() -> &'static SessionRegistry {
    static REGISTRY: OnceLock<SessionRegistry> = OnceLock::new();
    REGISTRY.get_or_init(SessionRegistry::new)
}

/// 账号凭证（架构文档 §9.1：`X-Passport-Token` + `user-uid`）。
///
/// 与其它 provider 的差别：CatPaw 的凭证是 **Cookie 形态**而不是 Bearer，且
/// `uid` 是独立请求头 —— 它不在 token 里（不是 JWT），所以必须由账号一起给出。
#[derive(Clone, Debug, Default)]
pub struct CatPawCredentials {
    /// `accessToken`（进 `Cookie: X-Passport-Token=<token>`）
    pub token: String,
    /// `user-uid` 头的值（账号的 uid / loginName）
    pub uid: String,
}

impl CatPawCredentials {
    pub fn new(token: impl Into<String>, uid: impl Into<String>) -> Self {
        Self { token: token.into(), uid: uid.into() }
    }
}

/// 一次会话转发的入参（W5 的适配器接线按这个形态构造）。
pub struct ConversationRequest {
    /// **归一化前**的原始 OpenAI 请求体（`messages` / `tools` / `model` …）
    pub body: Value,
    /// 客户端 `x-session-id` 请求头（可为空；空 = 无长会话语义）
    pub session_id: String,
    /// 账号 id（注册表按它做「换了账号就作废」的判定；空串 = **无账号身份**
    /// （环境变量旁路 / 默认登录态），是显式身份而不是通配 —— 空只匹配空）
    pub account_id: String,
    /// 账号凭证
    pub credentials: CatPawCredentials,
    /// 账号级出网代理（与 provider 无关，由编排层解析后带上）
    pub proxy: Option<ResolvedProxy>,
    /// 客户端是否要流式（`body.stream`；同时决定「无状态请求」判定 —— §9.3）
    pub stream: bool,
    /// 上游 base URL（默认 [`DEFAULT_BASE_URL`]；留字段是为了本地联调与将来换域名）
    pub base_url: String,
    /// usage / 尝试次数的旁路槽（可选：无记账需求时传 None）
    pub telemetry: Option<Arc<RequestTelemetry>>,
}

impl ConversationRequest {
    /// 建一个用默认上游地址的请求
    pub fn new(
        body: Value,
        session_id: impl Into<String>,
        account_id: impl Into<String>,
        credentials: CatPawCredentials,
        proxy: Option<ResolvedProxy>,
        stream: bool,
    ) -> Self {
        Self {
            body,
            session_id: session_id.into(),
            account_id: account_id.into(),
            credentials,
            proxy,
            stream,
            base_url: DEFAULT_BASE_URL.to_string(),
            telemetry: None,
        }
    }

    /// 从入站请求头取 `x-session-id` 并归一化（原实现 `normalizeClientSessionId`）。
    ///
    /// 归一化规则：非字符串 / 空 / 超 256 个字符一律当「没有会话 id」——
    /// 超长 id 不做截断而是丢弃：截断后的 id 会在两次请求之间不稳定地碰撞
    /// （前 256 位相同、后面不同的两个客户端会话会被当成同一个）。
    pub fn session_id_from_headers(headers: &HeaderMap) -> String {
        let value = headers
            .get("x-session-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
            .unwrap_or_default();
        let trimmed = value.trim();
        if trimmed.is_empty() || trimmed.chars().count() > 256 {
            return String::new();
        }
        trimmed.to_string()
    }
}

/// 一次会话转发的公开入口。
///
/// ── 出参（与无状态路径**同一种** `ForwardOutcome`，chat.rs 其余链路零改动）──
///   - `stream:true` → `ForwardOutcome::Stream`：round/event/turn 都成功之后才返回
///     （状态码恒 200，与其它 provider 的 SSE 分支一致）；帧流末尾带 `[DONE]`，
///     上游 SSE 由一个后台任务消费到底（硬约束 2）。
///   - `stream:false` → `ForwardOutcome::Completion`：内部仍走 turn SSE，
///     聚合成完整 `chat.completion`。
///   - 任何「返回给客户端之前」的失败都返回 `Err(GatewayError)`
///     （客户端看到正常的 OpenAI 错误体，而不是一条断掉的流）。
///
/// ── 失败时的上游收尾 ────────────────────────────────────────
/// 见 `turn_executor::FinishGuard`：round + event(running) 之后的任何失败都会回报
/// `failed`（上游/翻译错）或 `canceled`（客户端断开），且客户端断开时先
/// `turn/stop` 再回报。
pub async fn run_conversation(
    request: ConversationRequest,
) -> Result<ForwardOutcome, GatewayError> {
    if request.credentials.token.trim().is_empty() {
        return Err(GatewayError::with_status(
            503,
            "CatPaw 没有可用的登录凭证：请在账号页添加账号，或配置环境变量",
        ));
    }
    let prepared = prepare(&request).await.map_err(|error| error.to_gateway())?;
    execute(request, prepared).await
}

/// 复用会话注册表（供 W5 的账号切换调用，`clear_account` / `clear_all`）
pub fn session_registry() -> &'static SessionRegistry {
    registry()
}

// ─── 执行 ──────────────────────────────────────────────────────

/// 执行一次会话：占用判定 → 判定模式 → round → event(running) → turn → 收尾。
async fn execute(
    request: ConversationRequest,
    prepared: Prepared,
) -> Result<ForwardOutcome, GatewayError> {
    // ── 并发占用与无状态判定（原实现 `streamRequest` 开头）──────────
    // `stream:false` 的辅助请求（客户端标题/摘要类）**不读也不写**会话映射：
    // 它们的历史末尾常是 assistant，与主对话共用映射会互相覆盖（§9.3 硬约束 3）。
    let stateless = !request.stream;
    // 本次请求的账号身份（见 registry 的 `AccountIdentity`）：账号 id 取选路结果，
    // 用户标识取**本次实际使用的凭证**里的 uid（上游 `user-uid` 头的同一个值）。
    // 两个维度都要：前者管「换了账号记录」，后者管「同一记录底下换了用户」
    // （桌面端实时登录态在客户端侧换号）。
    let identity = AccountIdentity::new(request.account_id.as_str(), request.credentials.uid.as_str());
    let mut inflight = false;
    let mut inflight_conflict = false;
    if !request.session_id.is_empty() && !stateless {
        inflight = registry().mark_inflight(&request.session_id, &identity);
        inflight_conflict = !inflight;
        if inflight_conflict {
            logging::verbose(
                "[CatPaw]",
                "同一 x-session-id 已有流式请求在跑，本次走独立 conversation",
            );
        }
    }
    let persistent = inflight;

    let decision = match decide(&request, &prepared, persistent) {
        Ok(decision) => decision,
        Err(error) => {
            // 判定阶段失败也要把占用的标记放开（否则同 session 会被永久挡住）
            if inflight {
                registry().release_inflight(&request.session_id);
            }
            return Err(error.to_gateway());
        }
    };
    // turnRequestId 在 round 之前就定好：它属于本轮，round 之后才有 turn，
    // 而「上一轮被打断」时要用它去 stop
    let mut ctx = TurnContext {
        base_url: request.base_url.clone(),
        credentials: request.credentials.clone(),
        proxy: request.proxy.clone(),
        chat_id: new_chat_id(),
        model_name: prepared.resolution.display_name.clone(),
        include_usage: include_usage(&request.body),
        choice: prepared.choice.clone(),
        account_id: request.account_id.clone(),
        model_type: prepared.resolution.model_type,
        session_id: request.session_id.clone(),
        persistent,
        conversation_id: decision.conversation_id.clone(),
        turn_request_id: new_request_id(),
        telemetry: request.telemetry.clone(),
        registry: registry(),
    };
    let mut history = HistoryFingerprints::new(
        decision
            .session
            .as_ref()
            .map(|session| session.fingerprints.clone())
            .unwrap_or_default(),
        submitted_fingerprints(&decision),
    );
    let mut guard = FinishGuard::new(&ctx, inflight);

    // ── round（工具续接不 round）─────────────────────────────────
    if decision.round_messages.is_some() {
        // 上一轮的工具调用被打断时 conversation 仍停在上游「执行中」，
        // 必须先停掉旧轮次（对齐桌面端的 turn_stop），否则新 round 被拒 ——
        // 这是 §9.3 第二处 turn/stop 的用途。
        if let Some(session) = &decision.session {
            if session.is_awaiting_tool_results() {
                if let Some(turn_id) = &session.turn_request_id {
                    stop_turn(
                        &request.base_url,
                        &request.credentials,
                        request.proxy.as_ref(),
                        &session.conversation_id,
                        turn_id,
                    )
                    .await;
                    report_terminal(
                        &request.base_url,
                        &request.credentials,
                        request.proxy.as_ref(),
                        &session.conversation_id,
                        "canceled",
                        None,
                    )
                    .await;
                }
            }
        }
        match submit_round_with_self_heal(&request, &prepared, &mut guard, &decision).await {
            Ok(RoundOutcome::Fresh) => {}
            Ok(RoundOutcome::Healed) => {
                // 自愈换了 conversation 且**全量**重提交：旧指纹链对新 conversation
                // 毫无意义（它没见过那些消息），前缀必须清空、提交集换成全量 ——
                // 否则下一轮 `locate_increment` 会以为上游已见过整段历史，
                // 增量从末尾开始，新 conversation 就只看到一段断层的历史。
                history = HistoryFingerprints::new(
                    Vec::new(),
                    fingerprints_for(&prepared.messages),
                );
                ctx.conversation_id = guard.conversation_id.clone();
            }
            Err(error) => {
                // round 没成功：conversation 没建起来（或没进入执行），不必报终态
                drop(guard);
                return Err(error.to_gateway());
            }
        }
    }

    // ── event(running)（工具续接不重复报）────────────────────────
    if decision.round_messages.is_some() {
        if let Err(error) = report_status(
            &request.base_url,
            &request.credentials,
            request.proxy.as_ref(),
            &guard.conversation_id,
            "running",
            None,
        )
        .await
        {
            // round 成功但 running 回报失败：上游可能不认这次轮次，
            // 报一个 failed 收尾（尽力而为），再把错误交给客户端
            guard.mark_active();
            guard.close("failed", Some(&error)).await;
            return Err(error.to_gateway());
        }
    }
    guard.mark_active();

    // ── turn 请求体（含「末尾必须是 user」的持久会话校验）───────────
    let TurnBody { body: turn_body, final_type } = build_turn_body(&prepared, &decision, &guard)?;
    if decision.mode != TurnMode::ToolContinuation && final_type != "user" && persistent {
        // 只有持久会话要求轮次以 user 结尾；无状态辅助请求末尾可以是 assistant。
        // 走到这里说明客户端提交的历史与映射对不上（例如上一轮工具会话已失效），
        // 作废映射让它下一轮全量重来
        registry().invalidate(&request.session_id, InvalidationReason::Explicit);
        guard.close("failed", None).await;
        return Err(GatewayError::bad_request("工具会话已失效，请重新发起当前回合"));
    }

    logging::verbose(
        "[CatPaw]",
        &format!(
            "mode={} conversationId={} modelType={} msgs={} roundMsgs={} finalType={} \
             tools={} stream={} persistent={} stateless={} inflightConflict={}",
            decision.mode.as_str(),
            short_id(&guard.conversation_id),
            prepared.resolution.model_type,
            prepared.messages.len(),
            decision.round_messages.as_ref().map(Vec::len).unwrap_or(0),
            final_type,
            prepared.selected_tools.len(),
            request.stream,
            persistent,
            stateless,
            inflight_conflict,
        ),
    );

    let response = match turn_request(&request, &ctx, &turn_body).await {
        Ok(response) => response,
        Err(error) => {
            // turn 在请求头阶段就失败（网络/上游拒绝）：对话已经 round 过了，
            // 不报终态会让上游卡在「执行中」，因此必须报 failed
            logging::log("[CatPaw]", &format!("turn 请求失败: {}", error.message));
            guard.close("failed", Some(&error)).await;
            return Err(error.to_gateway());
        }
    };

    if request.stream {
        let (sender, receiver) = tokio::sync::mpsc::channel(64);
        let task_ctx = ctx.clone();
        tokio::spawn(async move {
            super::turn_executor::drive_stream(task_ctx, response, sender, guard, history).await;
        });
        Ok(ForwardOutcome::Stream {
            status: 200,
            stream: Box::new(tokio_stream::wrappers::ReceiverStream::new(receiver)),
        })
    } else {
        match super::turn_executor::drive_aggregate(ctx, response, guard, history).await {
            Ok(body) => Ok(ForwardOutcome::Completion { body }),
            Err(error) => Err(error.to_gateway()),
        }
    }
}

/// turn 请求体 + 末尾消息类型（类型用于「持久会话必须以 user 结尾」的校验）
struct TurnBody {
    body: Value,
    final_type: String,
}

/// 构造 turn 请求体（UPSTREAM_PROTOCOL §3.2）
fn build_turn_body(
    prepared: &Prepared,
    decision: &Decision,
    guard: &FinishGuard,
) -> Result<TurnBody, GatewayError> {
    let message = match decision.mode {
        TurnMode::ToolContinuation => decision
            .continuation
            .clone()
            .ok_or_else(|| GatewayError::bad_request("工具结果续接请求缺少 tool 消息"))?,
        _ => prepared.final_message.clone(),
    };
    let final_type = message
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let selected_tools = prepared.selected_tools.clone();
    let available: Vec<Value> = selected_tools
        .iter()
        .filter_map(|tool| tool.get("name").cloned())
        .collect();
    let mut body = serde_json::Map::new();
    body.insert("conversationId".to_string(), Value::String(guard.conversation_id.clone()));
    body.insert("turnRequestId".to_string(), Value::String(guard.turn_request_id.clone()));
    body.insert("source".to_string(), Value::String(DEFAULT_SOURCE.to_string()));
    body.insert("action".to_string(), Value::String("turn".to_string()));
    body.insert("message".to_string(), message);
    body.insert("modelType".to_string(), Value::from(prepared.resolution.model_type));
    body.insert("mode".to_string(), Value::String(DEFAULT_MODE.to_string()));
    body.insert("permissionMode".to_string(), Value::String(PERMISSION_MODE.to_string()));
    body.insert("toolVersion".to_string(), Value::String(DEFAULT_TOOL_VERSION.to_string()));
    body.insert("toolConfigs".to_string(), Value::Array(selected_tools));
    body.insert("availableTools".to_string(), Value::Array(available));
    if let Some(system_prompt) = &prepared.system_prompt {
        body.insert(
            "systemPromptContext".to_string(),
            json!({ "systemPromptOverride": system_prompt }),
        );
    }
    if let Some(rules_message) = &prepared.rules_message {
        body.insert("rulesMessage".to_string(), Value::String(rules_message.clone()));
    }
    Ok(TurnBody { body: Value::Object(body), final_type })
}

/// round 提交 + 「会话正在执行中」的自愈重试（模块头差异 3）。
///
/// 只在**长会话新轮次**模式上重试，且只重试一次：失败消息里出现「执行中」
/// （上游原文 `会话正在执行中，无法创建新轮次`）时，说明注册表记的 conversationId
/// 在上游仍处于 running，而我们拿不到它的 turnRequestId（进程重启、TTL 淘汰都会
/// 让 turnRequestId 丢失，那条旧轮次就再也停不掉）。此时把映射作废、改成
/// **全新会话全量 round**，客户端这一次请求就能走通。
async fn submit_round_with_self_heal(
    request: &ConversationRequest,
    prepared: &Prepared,
    guard: &mut FinishGuard,
    decision: &Decision,
) -> Result<RoundOutcome, CatPawError> {
    let round_messages = decision.round_messages.clone().unwrap_or_default();
    match submit_round(request, prepared, guard, &round_messages).await {
        Ok(()) => Ok(RoundOutcome::Fresh),
        Err(error) if decision.mode == TurnMode::SessionRound && is_busy_error(&error) => {
            logging::log(
                "[CatPaw]",
                "上一轮仍在上游执行中，作废会话映射并改为全新会话重试一次",
            );
            registry().invalidate(&request.session_id, InvalidationReason::UpstreamRejected);
            guard.conversation_id = new_request_id();
            submit_round(request, prepared, guard, &prepared.messages)
                .await
                .map(|()| RoundOutcome::Healed)
        }
        Err(error) => Err(error),
    }
}

/// round 的两种成功形态（决定调用方要不要重建指纹链，见自愈那一段）
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RoundOutcome {
    /// 按判定结果提交（长会话增量 / 全新会话全量）
    Fresh,
    /// 自愈后**全量**重提交（conversation 也是新的）
    Healed,
}

/// 上游「会话正在执行中」的判定。
///
/// 为什么是文案匹配而不是错误码：上游这个拒绝没有稳定的业务码（原实现也只靠
/// 文案描述这一现象），匹配失败只会退化成「不自愈」，不会误伤别的错误。
fn is_busy_error(error: &CatPawError) -> bool {
    error.message.contains("执行中")
}

/// 真正发一次 round（请求体见 UPSTREAM_PROTOCOL §3.1）
async fn submit_round(
    request: &ConversationRequest,
    prepared: &Prepared,
    guard: &FinishGuard,
    round_messages: &[Value],
) -> Result<(), CatPawError> {
    let mut body = serde_json::Map::new();
    body.insert("conversationId".to_string(), Value::String(guard.conversation_id.clone()));
    body.insert("source".to_string(), Value::String(DEFAULT_SOURCE.to_string()));
    body.insert("messages".to_string(), Value::Array(round_messages.to_vec()));
    body.insert("modelType".to_string(), Value::from(prepared.resolution.model_type));
    body.insert("mode".to_string(), Value::String(DEFAULT_MODE.to_string()));
    body.insert("permissionMode".to_string(), Value::String(PERMISSION_MODE.to_string()));
    body.insert("toolVersion".to_string(), Value::String(DEFAULT_TOOL_VERSION.to_string()));
    if let Some(system_prompt) = &prepared.system_prompt {
        body.insert(
            "systemPromptContext".to_string(),
            json!({ "systemPromptOverride": system_prompt }),
        );
    }
    if let Some(rules_message) = &prepared.rules_message {
        body.insert("rulesMessage".to_string(), Value::String(rules_message.clone()));
    }
    let mut declarative = serde_json::Map::new();
    if let Some(effort) = &prepared.effort {
        declarative.insert("effort".to_string(), Value::String(effort.clone()));
    }
    if let Some(context) = &prepared.context {
        declarative.insert("context".to_string(), Value::String(context.clone()));
    }
    if !declarative.is_empty() {
        body.insert(
            "requestContext".to_string(),
            json!({ "modelParams": { "declarativeParams": Value::Object(declarative) } }),
        );
    }
    let body_bytes = serde_json::to_string(&body).map(|text| text.len()).unwrap_or(0);
    let started_at = logging::now_ms();
    // ── 调试模式：采这一次 round 往返 ────────────────────────────
    // CatPaw 是**会话式**：一次用户请求内部有多次上游往返（round → events
    // 轮询 → turn/stop）。只有 round 承载对话内容，所以只采它 —— 控制类
    // 往返（event / stop）与目录刷新都传 None（见 `post_json` 的说明）。
    // 自愈重试会再走一次本函数，后一次覆盖前一次（`reset_request` 的
    // 「最后一次为准」，与无状态路径的重试口径一致）。
    // 开关关着时 `capture()` 为 None，整段不执行。
    let capture = request
        .telemetry
        .as_ref()
        .and_then(|telemetry| telemetry.capture());
    post_json(
        &request.base_url,
        "/api/agent/conversation/round",
        &request.credentials,
        request.proxy.as_ref(),
        &Value::Object(body),
        REQUEST_TIMEOUT_MS,
        capture.as_deref(),
    )
    .await?;
    logging::verbose(
        "[CatPaw]",
        &format!(
            "round 完成 conversationId={} msgs={} bodyBytes={} durationMs={}",
            short_id(&guard.conversation_id),
            round_messages.len(),
            body_bytes,
            logging::now_ms() - started_at,
        ),
    );
    Ok(())
}

// ─── 小工具 ────────────────────────────────────────────────────

/// 新建 conversationId / turnRequestId（原实现用 `randomUUID()`）
fn new_request_id() -> String {
    crate::server::core::upstream::request::new_request_id()
}

/// 取前 8 个字符（日志里只打 id 前缀：够区分、又不落完整标识）
pub(super) fn short_id(value: &str) -> &str {
    if value.is_empty() {
        return "-";
    }
    match value.char_indices().nth(8) {
        Some((index, _)) => &value[..index],
        None => value,
    }
}
