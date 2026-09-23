//! 对话转发主链路（对照 Node 版 workbuddy-upstream-client.mjs 的转发部分）。
//!
//! ── 一次转发的完整流程 ────────────────────────────────────────
//!   ① 去重排队：相同 body 的快速重试在代理内等前一个完成（防风控）
//!   ② 选谁去发：provider 轮询（候选链）→ 每家内部走账号选路循环
//!      （优先级 → 429 降级 → 11128 退避）→ 全失败才报错
//!   ③ 流式：SSE 透传（reasoning 帧合并）；非流式：内部流式聚合成 JSON
//!
//! ── 文件分工（单文件行数约定）────────────────────────────
//!   mod.rs          转发编排入口：去重槽位、SSE 透传流（ForwardStream）、
//!                   账号/provider 的展示辅助函数
//!   payload.rs      一次转发的输入（ProviderContext）+ 发送体选择（按 provider
//!                   的脱敏作用范围逐家决定用原始 body 还是处理副本）
//!   provider_loop.rs provider 轮询 + 账号选路循环 + 错误分类动作 + 退避重试
//!   rotate.rs       账号选路与 429 轮换：selectTargetAccount / 限额标记 /
//!                   429 与 provider 切换的结构化事件上报
//!   request.rs      传输层：请求发送、上游错误解析、追踪 id（**不认识 provider**）
//!   sse.rs          SSE reasoning 帧合并（跨 chunk 半行缓冲）+ usage 旁路提取
//!   aggregate.rs    非流式聚合（SSE → 完整 chat.completion）+ usage 旁路提取
//!   usage.rs        usage 旁路槽：token 用量 / 承载 provider+账号 / 尝试次数
//!
//! ── provider 差异去哪了（Agent2API 改造 W2b-T3）───────────────
//! 本目录**不含任何 provider 分支**：请求头/URL/system 注入/错误码判定/
//! token 刷新全在 `core::providers` 的适配器里（`workbuddy.rs`），
//! 本目录只通过 `ProviderAdapter` 契约（`providers::adapter`）使用它们。
//! 这里出现的 `ProviderKind` 只作**身份标识**使用（候选链的元素、
//! 日志里的 provider id、记账槽里的 provider 字段），没有任何
//! 「如果 provider 是 X 就怎么做」的分支 —— 四家 provider 在 `adapter_for`
//! 里都已接上真身适配器（那个 match 是穷举的，加新 kind 会在编译期被拦住），
//! 编排层不需要为任何一家写特判。
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
pub mod cancellation;
pub mod connections;
pub mod request;
mod payload;
mod provider_loop;
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

use axum::http::HeaderMap;

use crate::server::core::account_store::AccountStore;
use crate::server::core::auth::AuthService;
use crate::server::core::proxies::ResolvedProxy;
use crate::server::errors::GatewayError;
use crate::server::logging;

use self::connections::{ConnectionGuard, Connections};
use self::sse::{ModelRewrite, ReasoningCoalescer};

/// 去重等待上限（对照 Node 的 INFLIGHT_WAIT_MS）
const INFLIGHT_WAIT_MS: u64 = 45_000;

/// 选路循环的最大轮数（防病态数据下的无限回环）。
///
/// Node 版靠 `pickNextAccount` 返回 null 收尾 —— 每次轮换都会往 triedIds 里
/// 加一个账号，而账号上限是 20，所以必然收敛。这里额外加一个轮数上限兜底：
/// 万一账号数据被手工改出「同一个 id 出现两次」之类的怪状，宁可报错也不要空转。
pub(super) const MAX_ROUTE_ATTEMPTS: usize = 32;

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
///
/// 可见性是 `pub(crate)`（而非 `pub(super)`）：自定义提供商的转发入口
/// （`providers::custom::forward`，pub 函数）的签名里带着它 —— 类型比
/// 调用面窄会触发 `private_interfaces` 告警；持凭证的永远是 `upstream`
/// 内部的编排层，这个类型本身没有更多暴露面。
pub(crate) struct InFlightGuard {
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
    pub(super) store: AccountStore,
    pub(super) auth: AuthService,
    /// 在途请求表：sha256(body) → 完成信号（去重队列，见 `wait_for_in_flight`）
    in_flight: Arc<Mutex<HashMap<String, Arc<InFlight>>>>,
    /// 账号级活跃连接计数（账号页「连接数」列的数据源，见 `connections.rs`）
    connections: Connections,
}

/// 一次转发的入参
pub struct ForwardRequest {
    /// 客户端请求体（是否补默认 system 消息在内部判断）
    pub body: Value,
    /// 客户端是否要流式（`body.stream === true`）
    pub stream: bool,
    /// 请求体 sha256（去重键；空串表示不去重）
    pub dedupe_key: String,
    /// 客户端入站请求头（适配器契约的一部分，见 `ProviderAdapter::build_chat_request`）
    pub client_headers: HeaderMap,
    /// usage / 尝试次数的旁路槽。
    ///
    /// 为什么走「调用方建好、随请求传进来」而不是由 responder 自己造一个：
    /// 流式转发的收尾发生在 handler 返回之后（流被 axum 拉完才算结束），
    /// 那时数据必须落在**调用方仍持有**的句柄里才拿得到。调用方（记账点）
    /// 拿同一个 `Arc` 的另一份克隆，就能在流收尾时读到最终值。
    pub telemetry: Arc<usage::RequestTelemetry>,
    /// 本次请求命中的网关 Key 所带来的**可用提供商**限制（R9）。
    ///
    /// `None` = 不限制（免鉴权模式 / 环境变量 Key / 未知 Key，见
    /// `core::key_scope` 的模块头）。为什么**放在入参里**而不是让转发层自己去
    /// 请求头里再解析一次：转发层不该认识「网关 Key」这个概念（它只认识上游与
    /// 账号），而且 handler 手里已经有 `KeyScope`（中间件放进请求扩展的那份），
    /// 再解析一次就是两处实现同一件事。传值（`Option<KeyScope>`）而不是引用：
    /// 它要活过整个转发过程，而 handler 的栈帧可能在流式路径上先返回。
    ///
    /// **只有提供商白名单进这里**：模型白名单是**入口校验**（handler 在解析
    /// 模型名时就判、以 404 拒绝），不该等到了转发层才按家过滤 —— 那条路
    /// 会把请求打到上游再失败，而正确的语义是「这个模型对你这把 Key 不存在」。
    ///
    /// 写成完整路径 `crate::server::core::key_scope::KeyScope` 而不是 `super::`：
    /// 本文件在 `server::core::upstream` 下，`super::key_scope` 指的是
    /// `server::core::upstream::key_scope`（不存在）—— 这里跨了一层模块。
    pub allowed_providers: Option<crate::server::core::key_scope::KeyScope>,
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
pub(super) struct RouteTarget {
    /// 本次要用的 provider id：全局队列里选出来的账号决定这一次发给哪家；
    /// 没有任何账号记录时是「候选集合里第一家」（回落到它的默认登录态）
    pub provider: String,
    pub account_id: Option<String>,
    /// 账号公开形态（限额事件与日志用；无账号列表时为 null）
    pub account: Option<Value>,
    pub proxy: Option<ResolvedProxy>,
    pub priority: Option<i64>,
    /// 选路阶段的提示（目前只有「账号代理不可用、本次回退直连」）。
    ///
    /// ── 为什么由选路层带到转发层，而不是当场打一行日志 ──────────
    /// 它是**逐请求**的事实（代理配错的账号上，每一条请求都会发生一次），
    /// 改造前按请求往运行日志里灌一行 —— 用户看到的是刷屏，而请求日志那边
    /// 又看不到「这条请求其实是直连出去的」。带到这里之后，转发层把它记进
    /// **本轮尝试明细**的 `notice`，请求日志一次悬停就能看到，运行日志不再写。
    pub proxy_notice: Option<String>,
}

impl UpstreamService {
    pub fn new(store: AccountStore, auth: AuthService) -> Self {
        Self {
            store,
            auth,
            in_flight: Arc::new(Mutex::new(HashMap::new())),
            connections: Connections::new(),
        }
    }

    /// 账号级活跃连接计数句柄（`api::accounts` 的 `/api/accounts/connections` 读它）。
    ///
    /// 返回克隆（内部 `Arc`）而不是引用：`ServerState` 的 handler 各持一份克隆，
    /// 读到的都是同一张表。
    pub fn connections(&self) -> Connections {
        self.connections.clone()
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
        // ── 调试模式：为本次请求装一个原始报文采集器 ─────────────────
        // 装在这里（转发入口）而不是各家适配器里：四条路径（流式 / 非流式 ×
        // 无状态 / 有状态）都要采，装一次全都覆盖到。开关关着时**不创建**
        // 采集器 —— 后续所有 `capture()` 都返回 None，采集代码整段跳过。
        // 只带 id：URL / 头 / 体等真正发送时才由 `reset_request` 填上。
        if crate::server::core::debug_traffic::enabled() {
            let id = request.telemetry.id();
            if !id.is_empty() {
                request.telemetry.set_capture(std::sync::Arc::new(
                    crate::server::core::debug_traffic::TrafficCapture::begin(&id),
                ));
            }
        }
        // 手动终止的取消令牌（入口 handler 装入；None = 未接线）。取一次、
        // 走完全程：去重排队与流式轮询都要用它，中途不会再变
        let cancel = request.telemetry.cancel_token();
        // 已受理终止（罕见：登记之后、转发之前就被点了）——立刻返回，
        // 不必白跑一次选路
        if cancel.as_ref().is_some_and(|token| token.is_cancelled()) {
            return Err(cancellation::cancelled_error());
        }
        let mut slot = self.begin_slot(&request.dedupe_key, cancel.as_ref()).await;
        // 账号级活跃连接计数：**本请求**的凭证，随选路改绑到实际使用的账号。
        // 建在这里（而不是选路处）是因为它必须活到响应体发完 —— 流式路径由
        // `handoff()` 把归属移进响应流，非流式路径随本函数返回而析构释放。
        // 于是「连接数」与去重槽位共享同一条生命周期，不会有第二条释放路径。
        let mut connections = ConnectionGuard::new(self.connections.clone());
        // 无论客户端要不要流式，上游都必须以 stream:true 请求
        let mut upstream_body = request.body.clone();
        if let Some(object) = upstream_body.as_object_mut() {
            object.insert("stream".to_string(), Value::Bool(true));
        }
        // 内容处理的两个开关**按请求取一次快照**：同一次请求里各 provider 的判定用同一
        // 份值（请求进行中改设置不会让语义漂移），且这里的 body 始终是客户端原始
        // 请求体 —— 处理只发生在「某一家即将发送之前」，见 payload.rs。
        //
        // 配置快照必须在本栈帧里活到转发结束：提示词文本是以**借用**形式随
        // `ProviderContext` 传下去的（见 `ProviderContext::prompt`），提前释放
        // 会让它悬空 —— 所以先取一份快照、再从它取两个读数值。
        let config = crate::server::config::current();
        let sanitize_fingerprints = config.sanitize_fingerprints();
        let prompt = config.prompt_plan();
        // Key 的提供商白名单随请求带进转发上下文：它要活过整条转发链
        //（含流式 —— 但流本身不需要它，只在选路与重试时读）。
        // 这里把 request 的字段**移出来**再借给 context：`request` 的其它部分
        // （body / headers）同样以借用形式进了 context，直接 `&request.allowed_providers`
        // 会因为「同时持有 request 的可变借用（上面改过 body）」而借不过 ——
        // 移出后所有权清晰，也不必再多一次克隆。
        let key_scope = request.allowed_providers;
        let context = payload::ProviderContext {
            body: &upstream_body,
            stream: request.stream,
            client_headers: &request.client_headers,
            telemetry: &request.telemetry,
            sanitize_fingerprints,
            prompt,
            key_scope: key_scope.as_ref(),
        };
        provider_loop::forward_with_providers(self, context, &mut slot, &mut connections).await
    }

    /// 等待同 body 的在途请求完成，然后占住槽位。
    ///
    /// 返回 None 表示不需要去重（dedupe_key 为空）。
    ///
    /// `cancel` 是手动终止的令牌（None = 未接线）：等待期间被置位就提前返回
    /// 且**不占槽位** —— 转发链紧接着会在循环顶查到令牌并收尾，不占槽位是为了
    /// 不给一次已经作废的转发留下需要清理的去重登记。
    async fn begin_slot(
        &self,
        dedupe_key: &str,
        cancel: Option<&Arc<cancellation::CancelToken>>,
    ) -> Option<InFlightGuard> {
        if dedupe_key.is_empty() {
            return None;
        }
        if cancel.is_some_and(|token| token.is_cancelled()) {
            return None;
        }
        self.wait_for_in_flight(dedupe_key, cancel).await;
        if cancel.is_some_and(|token| token.is_cancelled()) {
            return None;
        }
        let signal = Arc::new(InFlight::new());
        lock_table(&self.in_flight).insert(dedupe_key.to_string(), signal.clone());
        Some(InFlightGuard {
            table: self.in_flight.clone(),
            key: dedupe_key.to_string(),
            signal,
        })
    }

    /// 等待同 body 的在途请求完成（最多 45 秒；被手动终止则立即返回）
    async fn wait_for_in_flight(
        &self,
        key: &str,
        cancel: Option<&Arc<cancellation::CancelToken>>,
    ) {
        let signal = lock_table(&self.in_flight).get(key).cloned();
        let Some(signal) = signal else {
            return;
        };
        // 只在终端：这是**网关内部的排队协调**（相同 body 的请求撞在一起），
        // 不是转发结果 —— 请求日志那边没有它的落点（等待发生在选路之前，
        // 那时还没有任何尝试明细可挂），所以它既不进请求日志、也不再进运行
        // 日志页，只在终端留一行供排障时确认「这条请求为什么慢」。
        logging::console_line("[Upstream]", "⏳ 检测到相同请求正在处理，排队等待（防重试风暴）");
        // 先注册等待、再检查完成标志：notify_waiters 只唤醒「当时已登记」的
        // 等待者，这个顺序保证「置位在前」与「置位在后」两种情况都不会漏
        let mut notified = std::pin::pin!(signal.signal.notified());
        if notified.as_mut().enable() {
            return;
        }
        if signal.is_done() {
            return;
        }
        let wait = tokio::time::timeout(Duration::from_millis(INFLIGHT_WAIT_MS), notified);
        tokio::pin!(wait);
        match cancel {
            // 排队期间被手动终止：立即醒来（返回后由 begin_slot 放弃占位、
            // 转发链在循环顶收尾）
            Some(token) => tokio::select! {
                _ = &mut wait => {}
                _ = token.cancelled() => {}
            },
            None => {
                let _ = wait.await;
            }
        }
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
    /// 上游字节流（已 consume 掉 Response，流自身是 'static）。
    ///
    /// 错误统一成 `io::Error`：`new`（reqwest 直连）在 map 时就把错误用
    /// `describe_error_detail` 描述成文案折进去（那个函数只认 reqwest::Error，
    /// 转换后 poll_next 只拿得到文案）；`from_translated`（翻译协议）的转换层
    /// 同样上抛 io::Error。两个来源在 poll_next 里共用同一条「错误帧 + [DONE]」
    /// 收尾，描述口径也一致。
    inner: futures::stream::BoxStream<'static, Result<Bytes, std::io::Error>>,
    coalescer: ReasoningCoalescer,
    /// 上游已结束（不再 poll 上游，只把 pending 吐完）
    upstream_done: bool,
    /// 缓存的待下发帧（一个上游 chunk 可能产出多帧）
    pending: std::collections::VecDeque<Bytes>,
    /// 在途槽位凭证：本流被 drop 时释放（含客户端断开、上游断开两条路径）。
    /// `Option` 只是为了让它能在结构里可选传入，实际总是 Some。
    _slot: Option<InFlightGuard>,
    /// 账号级活跃连接凭证：与槽位同一生命周期 —— 本流跑完 / 被 drop 时才把
    /// 该账号的计数 -1。**不是** `Option` 的语义区别，它同样总是 Some，
    /// 只是「无账号的转发」（环境变量旁路）里那个凭证的 `account_id` 是 None，
    /// 增减都是空操作，所以不必特判。
    _connection: ConnectionGuard,
    /// usage / 尝试次数的旁路槽：与合并器共用同一份（见 `ForwardStream::new`）
    telemetry: Arc<usage::RequestTelemetry>,
    /// 调试模式的采集器（构造时取一次，None = 未开启调试模式）。
    ///
    /// 在这里缓存而不是每个分片现取（`telemetry.capture()`）：采集发生在
    /// **每个上游 chunk** 上，每次都加锁取一遍是纯浪费；而一条请求的采集器
    /// 在转发开始时就装好了，中途不会变。
    capture: Option<Arc<crate::server::core::debug_traffic::TrafficCapture>>,
}

impl ForwardStream {
    /// `model_rewrite` 由适配器的 `sse_model_rewrite()` 决定（见 `sse.rs` 模块头）：
    /// 为 None 时帧的字节与接入前完全一致（workbuddy 的透传逐字节不变）。
    pub(super) fn new(
        response: reqwest::Response,
        slot: Option<InFlightGuard>,
        connection: ConnectionGuard,
        telemetry: Arc<usage::RequestTelemetry>,
        model_rewrite: Option<ModelRewrite>,
    ) -> Self {
        use futures::StreamExt;
        // reqwest 错误在这里就地描述成文案（`describe_error_detail` 认的是
        // reqwest::Error；折进 io::Error 之后 poll_next 只能拿到文本）
        let inner = response.bytes_stream().map(|item| {
            item.map_err(|error| {
                std::io::Error::other(crate::server::core::egress::describe_error_detail(&error))
            })
        });
        Self::from_translated(Box::pin(inner), slot, connection, telemetry, model_rewrite)
    }

    /// 翻译协议的构造入口：`inner` 已经是**标准 chat SSE** 帧流。
    ///
    /// 自定义家的 responses / anthropic 上游先过 `providers::custom` 的
    /// `ProtocolTranslateStream`（上游协议事件 → chat 帧），再进本流的
    /// reasoning 合并 / usage 提取 / model 回写 —— 那三层只认 chat 帧，
    /// 不需要知道上游原本是什么协议。
    pub(super) fn from_translated(
        inner: futures::stream::BoxStream<'static, Result<Bytes, std::io::Error>>,
        slot: Option<InFlightGuard>,
        connection: ConnectionGuard,
        telemetry: Arc<usage::RequestTelemetry>,
        model_rewrite: Option<ModelRewrite>,
    ) -> Self {
        // ── 手动终止的旁路流（本次新增）────────────────────────────
        // 把令牌的等待挂成一条「只产出一个错误项」的旁路流，与上游流 select
        // 合并：置位后 poll 立刻拿到 Err，**不必等下一个上游分片**（上游停滞
        // 时正是这个场景，光靠轮询顶部的同步检查会拖到下一片数据才反应）。
        // 不额外起任务 —— 旁路流随本流一起被丢弃，没有「唤醒谁来收尾」的
        // 悬空问题（对比：spawn 一个等待任务需要 Weak 反查防止任务泄漏）。
        // 错误项的文案就是手动终止原文，poll_next 的错误分支据此不加
        // 「上游流中断」前缀（那会把它说成上游的问题）。
        let inner = match telemetry.cancel_token() {
            Some(token) => {
                let signal = futures::stream::once(async move {
                    token.cancelled().await;
                    Err::<Bytes, std::io::Error>(std::io::Error::other(
                        cancellation::MANUAL_TERMINATED,
                    ))
                });
                Box::pin(futures::stream::select(inner, signal))
            }
            None => inner,
        };
        // 采集器在构造时取一次（见字段说明）
        let capture = telemetry.capture();
        Self {
            inner,
            coalescer: ReasoningCoalescer::with_telemetry(telemetry.clone())
                .with_model_rewrite(model_rewrite),
            upstream_done: false,
            pending: std::collections::VecDeque::new(),
            _slot: slot,
            _connection: connection,
            telemetry,
            capture,
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
                    // 调试模式：把**上游原始字节**旁路给采集器 —— 在合并器
                    // 之前，因为用户要看的是上游原样吐出来的东西，而不是
                    // 我们改写 / 合并后的帧（那正是「上游到底发了什么」要回答的）
                    if let Some(capture) = &self.capture {
                        capture.push(&bytes);
                    }
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
                    // 错误描述已在构造时折进 io::Error（见 inner 字段说明）
                    // 手动终止的旁路流给的就是原文（见 from_translated）：
                    // 不加「上游流中断」前缀 —— 它不是上游的问题
                    let text = error.to_string();
                    let message = if text == cancellation::MANUAL_TERMINATED {
                        text
                    } else {
                        format!("上游流中断: {error}")
                    };
                    // 只在终端：这条原因由下面的 `note_error` 进请求日志
                    // （客户端此时已收到部分内容，HTTP 状态早就是 200，
                    // 只有请求日志的「错误」列能解释「为什么这条是失败的」）。
                    logging::console_line("[Model]", &format!("❌ {message}"));
                    // 旁路记账：断流原因要进请求日志（客户端此时已收到部分内容，
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
pub(super) fn has_access_token(session: &Value) -> bool {
    session
        .get("auth")
        .and_then(|auth| auth.get("accessToken"))
        .and_then(Value::as_str)
        .map(|token| !token.is_empty())
        .unwrap_or(false)
}

/// 账号展示名（Node 的 `account.name || account.id`）
pub(super) fn account_display(account: &Value) -> String {
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
pub(super) fn account_label(account: Option<&Value>, account_id: &str, session: &Value) -> String {
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
pub(super) fn describe_proxy(proxy: Option<&ResolvedProxy>) -> String {
    match proxy {
        Some(proxy) if !proxy.label.is_empty() => proxy.label.clone(),
        Some(proxy) => proxy.host.clone(),
        None => "直连".to_string(),
    }
}

/// 账号公开形态里的优先级（日志里打的 `priority=N`）
pub(super) fn priority_of(account: &Value) -> Option<i64> {
    account.get("priority").and_then(Value::as_i64)
}

/// 限额文案里的恢复时间片段：`（<时间> 恢复）`，空串表示没有明确恢复时间
pub(super) fn reset_hint(reset_text: &str) -> String {
    if reset_text.is_empty() {
        String::new()
    } else {
        format!("（{reset_text} 恢复）")
    }
}

/// 恢复时间的本地化展示（对应 Node 的 `toLocaleString('zh-CN', { hour12:false })`）。
/// 与 errors.rs 的 reset_at_text 同口径：统一按 UTC+8 渲染，
/// 因为上游给的恢复时间本身就是 UTC+8 标定的，用本机时区会让用户对不上原文。
pub(super) fn format_reset_text(reset_at: f64) -> String {
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
