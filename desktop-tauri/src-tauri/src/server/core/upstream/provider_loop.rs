//! 全局账号队列的转发循环（从 `mod.rs` 拆出）。
//!
//! ── 一条队列，两层动作 ────────────────────────────────────
//! `mod.rs` 的 `forward()` 负责**协议无关的一次转发**：去重排队 → 交给本模块
//! → 把结果（流 / 聚合体）交还 axum。本模块负责**选谁去发**：
//!
//! ```text
//! 候选家 = 清单里有这个模型名的 provider（router::route_for_forward）
//! 账号循环：在候选家的全部账号里按**全局优先级**选一个
//!   └ 一次发送（该账号所属 provider 的适配器；含退避重试 + 401 刷新后重试一次）
//!        └ 429 → 标记该账号对该模型冷却，回到账号循环选下一个（可能换了一家）
//! ```
//!
//! 曾经是「外层按 provider 路由优先级轮询、内层在该家账号里选路」两层循环；
//! 现在四家账号排在同一条队里，先用哪一家由账号优先级本身决定，provider
//! 只是每个账号的属性（决定用哪个适配器发）。候选池按 provider 过滤只剩一个
//! 目的：不把不提供该模型的家的账号放进来。
//!
//! ── 错误分类怎么驱动控制流（架构文档 §4.2 的三个动作）────────
//!   1. `QuotaLimited` → 标记该账号对该模型冷却 + 换下一个候选账号；
//!   2. `TokenExpired` → 调 `refresh_access_token` 刷新凭证，**同一账号**
//!      原样重试一次（只一次：再失败按普通失败处理，不会空转）；
//!   3. `Fatal` → 原样透传给客户端。
//!
//! 分类由适配器给出，本模块只按这三档行动 —— 于是「11128 要退避、6004 是限额」
//! 这类 provider 知识全在适配器里，本文件对「上游是哪一家」一无所知。
//!
//! ── 退避重试（11128）为什么留在这里 ─────────────────────────
//! 架构文档 §4.3 明确要求「11128 退避逻辑保持在转发层」：**循环**在这里
//! （打日志、睡、再发），而「哪个码要退避、退多久、文案怎么写」由适配器的
//! `retry_advice` 给出。这样既能满足契约，又不会让具体错误码漏进本文件。
//!
//! ── 有状态 provider 的分流（W5-T-d4，架构文档 §4.2.1）─────────
//! 有状态 provider（CatPaw）**只替换「一次发送」，不替换账号循环**：选路、
//! telemetry 记账全部共用；不走 `build_chat_request` 那条路，因为会话式协议的
//! 「一次发送」是 round + turn + 工具循环，产出的是同形的 `ForwardOutcome`
//! 而不是 `reqwest::Response`。分流点是账号循环里的一处 `if adapter.is_stateful()`，
//! 实现见 `attempt_stateful`（只做「凭证 → 记账 → 转发 → 错误透传」，
//! 三个分类动作不适用：CatPaw 的原项目没有多账号轮换也没有限额码）。
//!
//! ── 内容处理（脱敏）在哪一步生效 ─────────────────────────────
//! 见 `payload.rs`：处理只在某一家即将发送前按作用范围逐家决定。本文件负责
//! 在正确时机调 [`send_body`]（选路与凭证就绪之后、构造请求之前），同一家
//! 的发送体在本次请求内只算一次（换到同一家的另一个账号时复用）。
//!
//! ── 旁路记账（usage）────────────────────────────────────────
//! 每一轮账号尝试都 `telemetry.note_attempt(...)`，并带上 provider id ——
//! 报表按「实际承载这次请求的家」记账，而不是按客户端请求的模型名猜。
//!
//! 同一处还要配一次 `note_attempt_started(provider_id, account)`，并在本轮定局时
//! `finish_last_attempt(status, error)` —— 那一对写的是**尝试明细链**
//! （请求日志「重试」列的弹层要显示「A → B → C」），与 `note_attempt` 的
//! 「最后一次为准」不同，它是追加式历史。两个出口（成功 `break response`、
//! 失败 `Err(failure)`）各记一次，所以明细条数恒等于 `attempts`。
//! **改这里的任何一条出口路径时都要一并检查那两处**：漏一处就会让明细
//! 比 attempts 少一条（前端会显示一条「无状态码、无错误」的悬空项）。
//!
//! ── 逐请求的日志一律不进运行日志页（本次改造）───────────────────
//! 本文件里凡是「每转发一次就会发生」的事件（换号顺延、退避重试、401 刷新、
//! 限额降级、上游报错、代理回退）都只写**终端**（`console_line`）或**请求
//! 日志**（明细的 `error` / `retries` / `notice`）—— 运行日志页只留「不随
//! 请求量增长的网关自身状态事件」（启动、登录、账号增删改、凭证维护、
//! 模型目录刷新、定时任务、账号被标记限额）。判断一条日志该去哪边，就看
//! **它的条数会不会跟着用户发请求一起涨**。

use std::borrow::Cow;
use std::collections::HashMap;
use std::time::Duration;

use serde_json::{json, Value};

use crate::server::config;
use crate::server::core::providers::adapter::{
    adapter_for, ProviderAdapter, RetryAdvice, UpstreamErrorClass,
};
use crate::server::core::providers::router::route_for_forward;
use crate::server::core::providers::{kind_from_id, kind_id, meta, ProviderKind};
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::payload::{send_body, ProviderContext};
use super::request::{read_upstream_error, send_chat_request, TransportRequest};
use super::{
    account_display, account_label, connections::ConnectionGuard, describe_proxy, reset_hint,
    rotate, ForwardOutcome, InFlightGuard, RouteTarget, UpstreamService, MAX_ROUTE_ATTEMPTS,
};

/// 上游一次请求的失败（已分类 + 已构好给客户端的错误）。
struct OutboundFailure {
    /// 适配器给出的分类（决定编排动作）
    class: UpstreamErrorClass,
    /// 给客户端的网关错误（状态码 / 文案 / 上游码）。
    ///
    /// 限额记录与事件上报也读它的 `message` —— 改造前 `markAccountLimited`
    /// 收的就是 `error.message`（`上游返回 429: {上游原文}`），
    /// accounts.json 里落的那段文案因此逐字不变。
    error: GatewayError,
}

/// **原地重发**的退避预算（见 `send_with_retry` 的说明）。
///
/// 整份请求共用一份：从第一个账号开始扣，扣完后面的账号就不再原地重发。
/// 这就是「同一账号重试 N 次」在本实现里的口径（见 `config::RetrySettings`）。
///
/// 与「还能换几个账号」是**两套独立的预算**：那份归 `attempt_queue` 的
/// `switches_left` 管，两者互不抵扣。
///
/// `total` 只用于日志里的「第 n/N 次」，不参与判定 —— 判定看 `remaining`。
#[derive(Clone, Copy, Debug)]
struct RetryBudget {
    /// 总共几次（日志用）
    total: usize,
    /// 还剩几次
    remaining: usize,
}

impl RetryBudget {
    fn new(total: usize) -> Self {
        Self { total, remaining: total }
    }

    /// 已经用掉几次（适配器要这个来写文案）
    fn used(&self) -> usize {
        self.total.saturating_sub(self.remaining)
    }
}

/// 换账号前扣一次预算：还能换返回 true，已经换满返回 false。
///
/// 换满时顺手写一行终端日志 —— 这是「为什么这次只试了 N 个账号就收尾」的
/// 唯一落点（请求日志的尝试链只显示试过谁，不回答为什么停在那个数）。
///
/// 两条顺延路径（会话式失败、普通错误）共用它，保证「最多换几个账号」在
/// 两条路上口径一致。**429 降级不走这里**：那是「冷却该账号再换下一个」的
/// 降级动作，与「重试换号」是两条路（见设置页那段说明）。
fn take_switch(switches_left: &mut usize, total: usize) -> bool {
    if *switches_left == 0 {
        logging::console_line(
            "[Upstream]",
            &format!("⚠️ 换账号次数已用尽（最多 {total} 个），不再顺延，返回本次错误"),
        );
        return false;
    }
    *switches_left -= 1;
    true
}

/// 瞬时 HTTP 状态码：适配器没声明专属重试时，按全局重试设置原样重发再看一眼。
///
/// 只收 408（请求超时）与 5xx（网关 / 服务器错误）—— 都是上游或链路自己的
/// 抖动，重试才有意义。**不含 429**：那是限额，走「账号冷却 + 换号」的分类
/// 动作（见模块头的三个动作），对同一账号原地重试只会白等一个间隔。
const TRANSIENT_RETRY_STATUSES: &[u16] = &[408, 500, 502, 503, 504];

/// 瞬时 HTTP 错误的统一退避建议（设置页「请求重试」的全局兜底）。
///
/// `remaining` 是原地重发**还剩几次**（见 [`RetryBudget`] 的说明）；
/// 用尽即返回 None，由调用方收敛成终态错误。
fn transient_retry_advice(status: u16, remaining: usize) -> Option<RetryAdvice> {
    if remaining == 0 || !TRANSIENT_RETRY_STATUSES.contains(&status) {
        return None;
    }
    Some(RetryAdvice {
        delay_ms: config::retry_settings().delay_ms(),
        reason: format!("上游瞬时错误（HTTP {status}）"),
    })
}

/// 传输层失败（DNS / 代理 / 连接）的统一退避建议：与瞬时 HTTP 错误同一套设置。
fn transport_retry_advice(remaining: usize) -> Option<RetryAdvice> {
    if remaining == 0 {
        return None;
    }
    Some(RetryAdvice {
        delay_ms: config::retry_settings().delay_ms(),
        reason: "上游连接失败".to_string(),
    })
}

/// 退避重试的运行日志行：`⚠️ {原因}；{n} 秒后重试（第 {i}/{N} 次）`。
///
/// 原因来自 `RetryAdvice::reason`（provider 专属措辞），进度由这里补 ——
/// 「第几次 / 共几次」只有编排层知道（预算归 [`RetryBudget`]，适配器只看到
/// 一个已经算好的数）。改造前这句整句由适配器与两个兜底函数各自拼好，
/// 措辞与现在一致，只是分隔符统一成「；」（原先瞬时错误与连接失败那两句用「，」）。
fn retry_log_line(reason: &str, delay_ms: u64, used: usize, total: usize) -> String {
    format!("⚠️ {reason}；{} 秒后重试（第 {used}/{total} 次）", delay_ms / 1000)
}

/// 转发入口：在候选家的全部账号里按全局优先级逐个尝试。
///
/// `slot` 是在途槽位凭证（`&mut` 是因为它只在**成功转为流式**时才被取走，
/// 失败重试时仍由本函数持有；见 `InFlightGuard` 的说明）。
/// `connections` 同理是账号级活跃连接的凭证：本函数按选中的账号改绑它，
/// 成功转流式时移交给响应流。
pub(super) async fn forward_with_providers(
    service: &UpstreamService,
    ctx: ProviderContext<'_>,
    slot: &mut Option<InFlightGuard>,
    connections: &mut ConnectionGuard,
) -> Result<ForwardOutcome, GatewayError> {
    let model = model_of(ctx.body);
    // 候选家来自 `route_for_forward`（目录里没有这个模型名时会回落成默认
    // provider 一家；模型有家承载但全被禁用时**不回落**，见那个函数）——
    // 本函数是唯一消费方，日志也打在这里。
    let candidates = route_for_forward(&model);
    if crate::server::core::providers::catalog::providers_for_model(&model).is_empty() {
        logging::verbose(
            "[Upstream]",
            &format!(
                "模型 {} 不在聚合目录中，按默认提供商转发",
                if model.is_empty() { "(未指定)" } else { &model },
            ),
        );
    }
    if candidates.is_empty() {
        // 空链有两条来源（见 `route_for_forward`）：注册表里连默认 provider 都
        // 没有，或者这个名字的承载家**全被禁用**（那时不回落默认家）。后者是
        // 用户可操作的，文案要指出去哪儿改，不能只说「没有可用的提供商」。
        if crate::server::core::providers::catalog::model_blocked_everywhere(&model) {
            return Err(GatewayError::with_status(
                404,
                format!(
                    "模型已在网关中禁用: {model}。完整列表见 GET /v1/models"
                ),
            )
            .with_code("model_not_found"));
        }
        return Err(GatewayError::new("没有可用的提供商，无法转发"));
    }
    // ── Key 的可用提供商白名单（R9，参考 OmniProxy 的 `filterRoutesForKey`）──
    // 位置照抄 OmniProxy：**在候选链与选路之间**过滤 —— 候选链仍然是「哪些家
    // 能提供这个模型」（能力事实），白名单只把其中不被允许的那几家剔除。
    // 过滤与「全被排除时的错误」都在 `filter_by_key_scope` 里（它要能提前返回
    // 错误，所以返回 Result）。
    let candidates = filter_by_key_scope(candidates, ctx.key_scope, &model)?;
    let provider_ids: Vec<&str> = candidates.iter().map(|kind| kind_id(*kind)).collect();
    // 映射扩池的提示：链比本名承载家长，说明追加了映射的提供商（选路顺序仍是
    // 原生优先）。用户看到请求落到映射家时，这里与发送侧的改写日志对得上。
    let with_mapping = crate::server::core::model_rules::current()
        .mappings_of(&model)
        .len() > 0;
    logging::verbose(
        "[Upstream]",
        &format!(
            "候选提供商 {}（按账号全局优先级选路{}）",
            provider_ids.join(" / "),
            if with_mapping { "，含映射" } else { "" },
        ),
    );
    attempt_queue(service, &ctx, &provider_ids, slot, connections).await
}

/// 按网关 Key 的**可用提供商**白名单过滤候选链（R9，照抄 OmniProxy 的
/// `filterRoutesForKey` 在「候选链与选路之间」的位置）。
///
/// 不把这个过滤塞进 `route_for_forward`：那个函数的返回值还背着别的语义
///（「未知模型」与「全被禁用」的区分，见它的模块头），而白名单是**逐请求**的
/// 东西，混进去会让「路由结果」变成与请求相关的量。
///
/// ── 白名单把候选全部排除时给一条**可读错误**（本需求明确要求的一条）──
/// 照抄 OmniProxy「空候选必有明确 404」的做法（它那里也是
/// `fail(res, ..., 404, 'model_not_found')`），但文案要点出真正的原因：
/// 不是模型不存在，而是这把 Key 不允许提供它的那些家 —— 后者会让人去查
/// 「模型管理」页的启停，而真正要改的是 Key 的可用提供商。
///
/// 与同文件其它错误路径一样：**绝不静默失败、绝不 panic**
///（release 是 panic=abort，落到那个分支会带走整个桌面应用）。
fn filter_by_key_scope(
    candidates: Vec<ProviderKind>,
    scope: Option<&crate::server::core::key_scope::KeyScope>,
    model: &str,
) -> Result<Vec<ProviderKind>, GatewayError> {
    let Some(scope) = scope.filter(|scope| scope.restricts_providers()) else {
        // 没有 scope / 这个维度不限制 —— 原样放行（绝大多数请求走这条）
        return Ok(candidates);
    };
    let kept: Vec<ProviderKind> = candidates
        .iter()
        .copied()
        .filter(|kind| scope.allows_provider(kind_id(*kind)))
        .collect();
    if !kept.is_empty() {
        return Ok(kept);
    }
    // 只在**终端**留痕：这条拒绝会作为 GatewayError 一路抛回入口，
    // 由 `api::chat` / `api::protocol` 记进请求日志的「错误」列（同一个原因、
    // 更完整的措辞），运行日志页因此不再为每一次被拒的请求写一行。
    logging::console_line(
        "[Security]",
        &format!(
            "🚫 请求被网关 Key 的可用提供商拒绝: 模型 {model}（{}）",
            scope.describe()
        ),
    );
    Err(GatewayError::with_status(
        404,
        format!(
            "模型 '{model}' 不可用：提供它的提供商都不在这把网关 Key 的可用提供商列表里（{}）",
            scope.describe()
        ),
    )
    .with_code("model_not_found"))
}

/// 账号循环：每一轮从候选池里按全局优先级选一个账号，用它所属家的适配器发一次。
///
/// 按 `is_stateful` 分流（架构文档 §4.2.1）：
///   - 无状态（workbuddy / 小浣熊 / AutoClaw）→ 下面这段「构造请求 → 发送 →
///     按分类动作」；
///   - 有状态（CatPaw）→ [`attempt_stateful`]（一次会话式转发，错误一律透传）。
async fn attempt_queue(
    service: &UpstreamService,
    ctx: &ProviderContext<'_>,
    provider_ids: &[&str],
    slot: &mut Option<InFlightGuard>,
    connections: &mut ConnectionGuard,
) -> Result<ForwardOutcome, GatewayError> {
    let model = model_of(ctx.body);
    let model_label = if model.is_empty() { "(默认)".to_string() } else { model.clone() };
    let mut tried_ids: Vec<String> = Vec::new();
    // ── 两份独立的预算（见 config::RetrySettings）─────────────────────
    //   - `budget`：同一个账号上还能**原地重发**几次。整份请求共用一份，
    //     由 `send_with_retry` 逐次扣减 —— 于是它天然花在「第一个真正发出去的
    //     账号」上，这正是「同一账号重试 N 次」该有的样子。
    //   - `switches_left`：这份请求还能**换几个账号**。换一次扣一次，由下面
    //     两条顺延路径扣（会话式失败 / 普通错误）。
    //
    // 两者互不抵扣：原地重发用尽不吃掉换号额度，换号也不重置重发预算 ——
    // 「先在当前账号试几次，实在不行再换号」这条直觉因此成立。
    //
    // 第二档按**账号**计、不分家：换到同一家的下一个账号，与换到另一家，
    // 都算一次换号。旧实现按提供商分段（同一家名下共用一份、只有跨家才重开），
    // 结果是「能换几个账号」根本没人管 —— 某家囤了 9 个账号时一次请求会一路
    // 试到第 10 个，而用户填的那个数字要等跨家才生效。
    let settings = config::retry_settings();
    let mut budget = RetryBudget::new(settings.resend_budget());
    let switch_total = settings.switch_budget();
    let mut switches_left = switch_total;
    // 各家的发送体：某一家即将发送前按作用范围决定一次，换到**同一家同池**的
    // 另一个账号时复用（不重复处理、不重复统计）。键带账号的池（Cline 的账号
    // 记录有 `free`/`pass`）：发送名跟着实际承载的账号所在池走
    // （`wire_target_for_provider`），跨池账号的发送名不同，各算一份。
    // 勾选的家用处理副本，未勾选的用原始 body。
    let mut send_cache: HashMap<(&'static str, String), Cow<'_, Value>> = HashMap::new();

    // 标签是必需的：下面「429 降级到下一个账号」发生在**内层发送循环**里，
    // 裸 `continue` 会回到内层（用同一个账号再发一次，正好是要避免的事）。
    // `continue 'accounts` 才表达「换队列里的下一个账号」。
    'accounts: for _ in 0..=MAX_ROUTE_ATTEMPTS {
        let target = rotate::select_target_account(service, provider_ids, &model, &tried_ids).await?;
        // 连接计数改绑到这一轮选中的账号：失败重试换账号时计数跟着走，
        // 于是「一个请求任意时刻只占一个账号」这条口径不需要每个分支各维护一次
        // （429 降级、401 刷新后换号、会话式失败顺延三条路径都经过这里）。
        connections.rebind(target.account_id.clone());
        let Some(kind) = kind_from_id(&target.provider) else {
            return Err(GatewayError::with_status(
                503,
                format!("账号所属提供商「{}」未知，无法转发", target.provider),
            ));
        };
        let adapter = adapter_for(kind);
        let provider_id = kind_id(kind);
        // 没有「按家重置预算」这一步了：两份预算都在循环外开好、整份请求共用
        // （理由见上面的注释）。`provider_id` 仍然要取 —— 下面的发送体缓存、
        // 日志与遥测记账都用它。
        if adapter.is_stateful() {
            // 会话式转发（CatPaw）内部没有轮换，但**队列的兜底仍然生效**：
            // 它失败了就把它记入已尝试、回到循环挑下一个账号 —— 可能已经换了一家。
            // 没有下一个可用账号时，把这个错误原样透传（它的文案最贴近真实原因）。
            let stateful_account_id = target.account_id.clone();
            match attempt_stateful(service, ctx, kind, adapter, target, slot, connections).await {
                Ok(outcome) => return Ok(outcome),
                Err(error) => {
                    if let Some(account_id) = stateful_account_id {
                        if !tried_ids.contains(&account_id) {
                            tried_ids.push(account_id);
                        }
                    }
                    match rotate::pick_next_account(service, provider_ids, &model, &tried_ids) {
                        Some(next) => {
                            // 换号额度用尽 → 队列里即使还有人也不再顺延
                            // （`take_switch` 已写过那行终端日志），本次错误原样
                            // 返回：与「没有下一个可用账号」同一个出口。
                            if !take_switch(&mut switches_left, switch_total) {
                                return Err(error);
                            }
                            // 顺延这件事本身由请求日志的尝试链回答（本轮明细已经
                            // 定稿为失败、下一轮会追加新明细），这里只留终端一行
                            logging::console_line(
                                "[Upstream]",
                                &format!(
                                    "⚠️ 会话式转发失败，按队列顺延 → {}（优先级 {}）",
                                    account_display(&next),
                                    next.get("priority").and_then(Value::as_i64)
                                        .map(|value| value.to_string())
                                        .unwrap_or_else(|| "-".to_string()),
                                ),
                            );
                            continue 'accounts;
                        }
                        None => return Err(error),
                    }
                }
            }
        }
        // 没有账号记录（选路回落到默认登录态）时，只有声明了环境变量旁路的
        // provider 才能继续 —— 否则下一步 session_for 会去「该 provider 的
        // 当前账号」里找，找不到就是 401。提前拦住能把原因说清楚。
        if target.account_id.is_none() && !adapter.allows_anonymous_default_session() {
            return Err(GatewayError::with_status(
                503,
                "没有可用账号，无账号可转发：请在账号页添加并启用账号",
            ));
        }
        let mut session = match rotate::session_for(service, provider_id, target.account_id.as_deref()).await {
            Ok(session) => session,
            Err(error) => {
                // ── 默认登录态的兜底（Agent2API W3-T4）──────────────────
                // `session_for(None)` 只认「auth 层认识的默认登录态」：workbuddy 是
                // `WORKBUDDY_TOKEN` 环境变量 + 账号文件派生的当前账号。小浣熊的
                // 旁路凭证（`RACCOON_TOKEN`）不在那一层，而它的**账号文件里可能
                // 一条记录都没有**（脚本/CI 用户的常规用法）—— 那种情况下
                // 上面的调用必然 401。
                //
                // 兜底只对**自己声明了匿名默认会话**的 provider 生效
                // （`allows_anonymous_default_session`），且仅在没有指定账号时：
                // 适配器给出的 token 单独构成一个最小会话（只有 Authorization
                // 需要它），不覆盖 workbuddy 那条已经能拿到完整会话的路径 ——
                // 所以既有行为逐字不变。
                if target.account_id.is_none() && adapter.allows_anonymous_default_session() {
                    match adapter.ensure_access_token(&service.store, "").await {
                        Ok(token) if !token.is_empty() => json!({
                            "auth": {
                                "accessToken": token,
                                "tokenType": "Bearer",
                            },
                        }),
                        // 适配器也拿不到凭证：返回**原始错误**（它比 401「请先登录」
                        // 更贴近真实原因，例如环境变量为空、auth.json 缺失）
                        _ => return Err(error),
                    }
                } else {
                    return Err(error);
                }
            }
        };
        // ── 凭证可用性（架构文档 §4.2 的 ensure_access_token）──────────
        // 显式选中的账号在这里补一次「临期主动刷新」：改造前只有默认登录态
        // 走 get_current_session 时才刷新，多账号链路上一个即将过期的 token
        // 会直接打到上游吃 401。刷新结果由适配器回写 store，随后重取会话。
        //
        // **失败不致命**：ensure 报错（例如该账号的刷新正被另一处进行中，
        // auth 层给 409）时沿用 store 里现有的 token 继续发 —— 真正的 token
        // 失效由 401 → refresh_access_token 那条路径兜底，这里提前报错
        // 反而会让一个本来能成功的请求失败。
        if let Some(account_id) = target.account_id.clone() {
            match adapter.ensure_access_token(&service.store, &account_id).await {
                Ok(_) => {
                    // 刷新可能已回写：重取会话，让头里的 token 是最新的
                    if let Ok(fresh) =
                        rotate::session_for(service, provider_id, Some(&account_id)).await
                    {
                        session = fresh;
                    }
                }
                Err(error) => logging::verbose(
                    "[Upstream]",
                    &format!(
                        "账号 {account_id} 的凭证准备失败（沿用现有 token）: {}",
                        error.message
                    ),
                ),
            }
        }
        // ── 旁路记账：本 provider + 本账号是这一轮的实际承载者 ──────────
        // 账号展示名算一次、两处用（`note_attempt` 的 account_name 与
        // `note_attempt_started` 的 account）：弹层里「这一轮谁在承载」与
        // 报表的账号列因此不可能对不上。
        let attempt_account = account_label(
            target.account.as_ref(),
            target.account_id.as_deref().unwrap_or(""),
            &session,
        );
        ctx.telemetry.note_attempt(
            target.account_id.as_deref(),
            &attempt_account,
            provider_id,
        );
        // 尝试明细的「起头」：本轮的承载者定了，结果稍后由下面两个出口补上
        // （成功出口 / 失败出口）。与 note_attempt 必须成对且在它之后 ——
        // 明细的条数因此恒等于 attempts，前端「共 N 次尝试」与链长对得上。
        // 放在这里而不是 send_with_retry 里：函数内那层退避重试（同账号重发）
        // **不算一次新尝试**（口径见 TelemetrySnapshot::attempts 的说明），
        // 若在循环里起头就会多出几条「同名同账号」的重复项。
        ctx.telemetry.note_attempt_started(provider_id, &attempt_account);
        // 代理回退提示：选路时记下的「代理不可用、本次直连」跟着这一轮走
        // （改造前它是一行运行日志，见 `with_proxy_notice`）
        if let Some(notice) = target.proxy_notice.as_deref() {
            ctx.telemetry.note_attempt_notice(notice);
        }

        // ── 内容处理：凭证已就绪、这一家**即将发送**，此刻才决定发送体 ────
        // 位置在选路/凭证之后：没有可用账号（上面的 503/401 提前返回）的请求
        // 走不到这里，不会产生一次「已转发的处理」统计。账号的池进缓存键，
        // 让发送名随实际承载的账号走（同池换账号复用，跨池各算一份）。
        let account_pool = target
            .account
            .as_ref()
            .and_then(|account| account.get("pool"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let body = send_cache
            .entry((provider_id, account_pool))
            .or_insert_with(|| send_body(ctx, provider_id, target.account.as_ref()));

        // ── 一次账号内的发送链：最多两次（首次 + 401 刷新后重试一次）────
        // 为什么把刷新重试并进同一个循环：重试**自己也可能是** 429
        // （额度确实用尽）。若把它当成独立分支直接返回，就会把一条限额错误
        // 当成终态发给客户端 —— 而正确的动作是「标记冷却 + 降级到下一个账号」。
        // 并进同一循环后，两次发送的错误走同一套分类处理。
        let started_at = logging::now_ms();
        let mut refreshed = false;
        let response = loop {
            // 构造请求计划**可能失败**（适配器自己的校验，例如小浣熊账号缺
            // accessToken → 401）。这里显式处理而不是用 `?` 直接抛出：
            // 上面的 `note_attempt_started` 已经为这一轮起了头，直接返回会让
            // 那条明细永远停在「无状态码、无错误」的悬空态（前端的重试面板
            // 会把它渲染成「无结果记录」）—— 明明有一个明确的失败原因。
            // 所以先给明细定稿，再把错误抛出：明细条数与 attempts 的一一对应
            // 在此也成立（那是本字段的全部前提，见模块头）。
            let plan = match adapter.build_chat_request(&session, body, ctx.client_headers) {
                Ok(plan) => plan,
                Err(error) => {
                    ctx.telemetry.finish_last_attempt(
                        Some(i64::from(error.status_code)),
                        Some(&error.message),
                    );
                    return Err(error);
                }
            };
            // 序列化失败只可能是内部数据坏了（适配器给出的 body 里含不可序列化的
            // 值），按 500 收敛。同样要先给明细定稿（理由同上一条）。
            let payload = match serde_json::to_string(&plan.body) {
                Ok(payload) => payload,
                Err(error) => {
                    let gateway = GatewayError::new(format!("请求体序列化失败: {error}"));
                    ctx.telemetry.finish_last_attempt(
                        Some(i64::from(gateway.status_code)),
                        Some(&gateway.message),
                    );
                    return Err(gateway);
                }
            };
            logging::verbose(
                "[Upstream]",
                &format!(
                    "POST {} model={} stream={} uid={} priority={} 出口={} msgs={} provider={provider_id}",
                    plan.url,
                    model_label,
                    ctx.stream,
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
                    ctx.body
                        .get("messages")
                        .and_then(Value::as_array)
                        .map(|items| items.len().to_string())
                        .unwrap_or_else(|| "?".to_string()),
                ),
            );
            let transport = TransportRequest {
                url: plan.url,
                headers: plan.headers,
                payload,
                proxy: target.proxy.clone(),
            };
            // ── 调试模式：抓一份即将发出去的原始报文 ──────────────────
            // 位置在 `build_chat_request` 之后（URL / 头 / body 都已定稿）。
            // 每次尝试前刷新一次（退避重试与 401 刷新重试都会走到这里），
            // 于是留下的是**最终生效**的那一次往返。开关关着时 `capture`
            // 为 None，这一段完全不执行（零开销）。
            let capture = ctx.telemetry.capture();
            if let Some(capture) = capture.as_deref() {
                capture.reset_request(&transport.url, provider_id, &transport.headers, &plan.body);
            }
            match send_with_retry(adapter, &transport, &mut budget, capture.as_deref(), ctx.telemetry)
                .await
            {
                Ok(response) => {
                    // 成功出口：这一轮的结果是「成功 + 状态码」，明细在这里定稿。
                    // 失败出口在下面（`Err(failure)` 那一支的开头）—— 两个出口
                    // 各记一次，一次起头对应恰好一次定稿。
                    //
                    // 状态码取**上游返回的那个**（不是下发给客户端的）：明细要
                    // 回答「上游怎么回的」，而客户端的 200 在流式响应头阶段就
                    // 发出去了，之后还可能断流失败 —— 那属于另一列（状态）的
                    // 口径，见 `RequestEntry::is_success` 的说明。
                    ctx.telemetry
                        .finish_last_attempt(Some(i64::from(response.status().as_u16())), None);
                    break response;
                }
                Err(failure) => {
                    // ── 动作 2：token 失效 → 刷新后同一账号重试一次 ──────
                    // 只在「首次失败 + 是 token 失效 + 还没刷新过」时走。
                    // 刷新失败、或重试再失败（此时 `refreshed` 已为 true）
                    // 都落到下面同一套分类处理，不会空转。
                    //
                    // 注意这一段在**明细记账之前**：401 刷新重试属于同一轮
                    // （同一个账号、同一条明细），换的是凭证不是账号，所以它
                    // 不该产生新明细、也不该在这一刻把这一轮定成失败 ——
                    // 重试若成功，这一轮的结局就是成功（`break response` 那条
                    // 出口会记上）。这也与 `attempts` 的口径一致：那一轮只 +1。
                    if matches!(failure.class, UpstreamErrorClass::TokenExpired { .. })
                        && !refreshed
                    {
                        refreshed = true;
                        let account_id = target.account_id.clone().unwrap_or_default();
                        // 401 刷新重试也是「本轮内部的一次重试」（换凭证不换账号），
                        // 与退避重试同一落点 —— 请求日志的重试链因此能回答
                        // 「这一轮重试过没有、为什么」，运行日志不再写这一行。
                        ctx.telemetry.note_attempt_retry(
                            &format!("token 被上游拒绝（401），刷新后重试一次（账号 {account_id}）"),
                            Some(i64::from(failure.error.status_code)),
                            0,
                        );
                        logging::console_line(
                            "[Upstream]",
                            &format!(
                                "token 被上游拒绝（401），尝试刷新后重试一次（账号 {account_id}）"
                            ),
                        );
                        if adapter
                            .refresh_access_token(&service.store, &account_id)
                            .await
                            .is_ok()
                        {
                            // 刷新已回写 store：重取会话（内含新 accessToken）再发一次
                            if let Ok(fresh) = rotate::session_for(
                                service,
                                provider_id,
                                target.account_id.as_deref(),
                            )
                            .await
                            {
                                session = fresh;
                                continue;
                            }
                        } else {
                            // 刷新**失败**：这是「401 了、也试过续期、但没救回来」的
                            // 唯一解释点。以前这里什么都不写，用户只看到一条 401，
                            // 无从判断到底是「没续期」还是「续期也失败」——
                            // 而那正是排查「token 过期不自动续期」时的第一个问题。
                            // 用 log（不是 verbose）：它只在真发生 401 时才出现，
                            // 频率低、信息价值高，不会像每请求路径那样刷屏。
                            logging::log(
                                "[Upstream]",
                                &format!(
                                    "账号 {account_id} 的凭证续期失败，本次 401 无法通过重试恢复\
                                     （请检查该账号的 refreshToken 是否仍有效）"
                                ),
                            );
                        }
                    }
                    // ── 这一轮的结局已定（不会再重发同一个账号）→ 记明细 ────
                    // 位置在动作 1 / 动作 3 之前：那两段会 `continue 'accounts`
                    // （开下一轮、追加新明细）或 `return Err`（整体收尾），
                    // 两条路之后都读不到这一轮的 `failure` 了。
                    // 一次 `note_attempt_started` 对应恰好一次这里（成功则由
                    // 下面 `break response` 那条出口记），所以明细条数恒等于
                    // attempts，不存在重复覆盖。
                    ctx.telemetry.finish_last_attempt(
                        Some(i64::from(failure.error.status_code)),
                        Some(&failure.error.message),
                    );
                    // ── 动作 1：限额 → 标记冷却 + 换下一个账号 ──────────
                    if let (
                        UpstreamErrorClass::QuotaLimited {
                            status,
                            upstream_code,
                            reset_at,
                            ..
                        },
                        Some(account_id),
                    ) = (&failure.class, target.account_id.clone())
                    {
                        if !tried_ids.contains(&account_id) {
                            tried_ids.push(account_id.clone());
                            let reset_text = rotate::mark_account_limited(
                                service,
                                &account_id,
                                &model,
                                *status as i32,
                                *upstream_code,
                                *reset_at,
                                &failure.error.message,
                            );
                            let limit_at =
                                rotate::account_limit_reset_at(service, &account_id, &model);
                            let from_label =
                                account_label(target.account.as_ref(), &account_id, &session);
                            match rotate::pick_next_account(
                                service,
                                provider_ids,
                                &model,
                                &tried_ids,
                            ) {
                                Some(next) => {
                                    let next_label = account_display(&next);
                                    let next_priority =
                                        next.get("priority").and_then(Value::as_i64);
                                    let next_provider = rotate::provider_of(&next);
                                    let next_home = if next_provider == provider_id {
                                        String::new()
                                    } else {
                                        format!(
                                            "，切换提供商 → {}",
                                            kind_from_id(next_provider)
                                                .map(|kind| meta(kind).label)
                                                .unwrap_or(next_provider)
                                        )
                                    };
                                    let reset_hint = reset_hint(&reset_text);
                                    // 这一行只在终端：请求日志那边由「本轮明细的
                                    // error + 下一轮的账号」表达同一条事实
                                    // （换号链就是降级过程本身）。
                                    logging::console_line(
                                        "[Upstream]",
                                        &format!(
                                            "⚠️ 账号 {from_label} 对模型 {model} 已限额{reset_hint}，\
                                             按优先级降级 → {next_label}（优先级 {}{next_home}）",
                                            next_priority
                                                .map(|value| value.to_string())
                                                .unwrap_or_else(|| "-".to_string()),
                                        ),
                                    );
                                    // 账号被标记限额是**状态变更**（账号页的「限额」
                                    // 列会跟着变、用户可操作），不是逐请求的过程
                                    // 事实 —— 所以它是保留在运行日志里的那一条，
                                    // 与上面那行终端日志的区别就在这里。
                                    rotate::report_limit_event(
                                        "warn",
                                        &format!(
                                            "账号「{from_label}」对模型 {model_label} 已限额{reset_hint}，\
                                             按优先级降级 → 「{next_label}」",
                                        ),
                                        Some(&from_label),
                                        Some(&next_label),
                                        &model,
                                        *upstream_code,
                                        *status as i32,
                                        limit_at,
                                        next_priority,
                                        provider_id,
                                    );
                                    // 换这一家的下一个账号（跳到外层选路循环）
                                    continue 'accounts;
                                }
                                None => {
                                    let message = format!(
                                        "{}（所有候选账号对模型 {model} 均已限额或禁用）",
                                        failure.error.message
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
                                        *upstream_code,
                                        *status as i32,
                                        limit_at,
                                        None,
                                        provider_id,
                                    );
                                    return Err(GatewayError::with_status(
                                        *status as i32,
                                        message,
                                    )
                                    .with_optional_code(*upstream_code));
                                }
                            }
                        }
                    }
                    // ── 动作 3：其它错误 → 换队列里的下一个账号 ──────────
                    // 旧版（按家轮询）遇到任何一家失败都会换下一家再试；全局队列把
                    // 这层兜底收敛成「换下一个账号」—— 可能是同家的下一位，也可能
                    // 直接换了一家。只有确实多出「没试过的账号」才继续（否则会拿
                    // 同一个默认登录态空转），没有就原样透传本次错误。
                    //
                    // 换号受「切换账号重试次数」约束（`switches_left`）：额度用尽
                    // 时即使队列里还有人也不再顺延 —— 这是「一次请求最多牵连几个
                    // 账号」的唯一闸门（429 那条降级路径不受它管，见设置页说明）。
                    if let Some(account_id) = target.account_id.clone() {
                        if !tried_ids.contains(&account_id) {
                            tried_ids.push(account_id);
                        }
                    }
                    match rotate::pick_next_account(service, provider_ids, &model, &tried_ids) {
                        Some(next) => {
                            if !take_switch(&mut switches_left, switch_total) {
                                return Err(failure.error);
                            }
                            let next_home = {
                                let next_provider = rotate::provider_of(&next);
                                if next_provider == provider_id {
                                    String::new()
                                } else {
                                    format!(
                                        "，切换提供商 → {}",
                                        kind_from_id(next_provider)
                                            .map(|kind| meta(kind).label)
                                            .unwrap_or(next_provider)
                                    )
                                }
                            };
                            // 只在终端：请求日志那侧由「本轮明细（账号 + 错误）
                            // + 下一轮的明细」完整表达这条顺延链 —— 这条日志
                            // 里除了这两者之外没有第三个信息。
                            logging::console_line(
                                "[Upstream]",
                                &format!(
                                    "⚠️ 账号 {} 对模型 {model} 转发失败（HTTP {}），\
                                     按队列顺延 → {}（优先级 {}{next_home}）",
                                    account_label(target.account.as_ref(), &target.account_id.clone().unwrap_or_default(), &session),
                                    failure.error.status_code,
                                    account_display(&next),
                                    next.get("priority").and_then(Value::as_i64)
                                        .map(|value| value.to_string())
                                        .unwrap_or_else(|| "-".to_string()),
                                ),
                            );
                            continue 'accounts;
                        }
                        None => return Err(failure.error),
                    }
                }
            }
        };

        // 请求成功：该账号对该模型的限额标记（如有）已失效，清除
        cap_cleared(service, &target, &model, &model_label, &session, provider_id);
        logging::verbose(
            "[Upstream]",
            &format!(
                "上游响应 HTTP {}（{}ms）",
                response.status().as_u16(),
                logging::now_ms() - started_at
            ),
        );

        // ── 调试模式：响应头到手（必须在 consume response 之前）──────────
        // 状态码与响应头在这里定稿；响应体由后续的流 / 聚合函数逐段补进同一个
        // 采集器（见 `ForwardStream` / `aggregate_sse_completion`）。
        if let Some(capture) = ctx.telemetry.capture() {
            capture.attach_response(response.status().as_u16(), response.headers());
        }

        if ctx.stream {
            let status = response.status().as_u16();
            return Ok(ForwardOutcome::Stream {
                status,
                // 槽位交给流：流跑完 / 客户端断开 / 流被 drop 时才放行等待者；
                // 连接计数同样移交（`handoff` 转移所有权，本栈帧的凭证随即失效，
                // 避免同一账号被两份凭证各算一次）
                stream: Box::new(super::ForwardStream::new(
                    response,
                    slot.take(),
                    connections.handoff(),
                    ctx.telemetry.clone(),
                    model_rewrite_of(adapter, &model),
                )),
            });
        }
        let aggregated = super::aggregate::aggregate_sse_completion(
            response,
            ctx.telemetry.clone(),
            model_rewrite_of(adapter, &model),
        )
        .await?;
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

/// **有状态 provider** 的一次转发（架构文档 §4.2.1；当前只有 CatPaw）。
///
/// ── 共用与不共用的部分（与无状态路径逐条对照）───────────────
/// ```text
///   共用：账号选路（全局队列里选出的 `target` 由调用方传入）、telemetry 记账、
///         成功后的限额标记清理、在途槽位（SlotHoldingStream）
///   不共用：build_chat_request / send_chat_request / classify_error 的三档动作
/// ```
///
/// ── 为什么这里**没有**账号轮换循环（核对结论，W5-T-d4）────────
/// 无状态路径的选路是个 `loop`：401/429 之后换下一个账号重发，依赖「上游给出
/// 可轮换的错误信号」。而 CatPaw 的原项目**没有任何轮换链路**：
///   - `catpaw-local-proxy` 全仓没有 429 判定（`grep -rn "429" *.mjs` 零命中），
///     也没有限额码解析；
///   - 账号是「用户在账号页选中的那一个」（`account-store.mjs` 的
///     `getCurrentCredentials`），切换账号只作废旧 conversation
///     （`account-routes.mjs` 的 `notifySwitch` → `clearClientToolSessions()`），
///     没有「失败 → 换人重试」的路径；
///   - 「会话正在执行中，无法创建新轮次」**不是**账号级限额：那是同一条
///     conversation 在上游仍 running，原实现的动作是 `turn/stop` + 自愈重开
///     （本项目的 `conversation::submit_round_with_self_heal`），与换账号无关。
/// 本函数内部对上游错误**不换账号不重试**（CatPaw 没有轮换信号）；
/// 「失败后顺延到下一个账号」由调用方的账号循环统一兜底（见 `attempt_queue`）。
/// 选路仍然共用 —— 全局队列选到 CatPaw 的账号时才进入本函数。
///
/// ── 在途槽位（去重队列）─────────────────────────────────────
/// 无状态路径把槽位交给 `ForwardStream`；有状态路径把同一个凭证包进
/// [`SlotHoldingStream`]，「槽位占到响应体发完」的语义在两家形态上一致。
/// 账号级连接计数（`connections`）与它同一处理：成功转为流式时一同移交给流。
async fn attempt_stateful(
    service: &UpstreamService,
    ctx: &ProviderContext<'_>,
    kind: ProviderKind,
    adapter: &dyn ProviderAdapter,
    target: RouteTarget,
    slot: &mut Option<InFlightGuard>,
    connections: &mut ConnectionGuard,
) -> Result<ForwardOutcome, GatewayError> {
    let provider_id = kind_id(kind);
    let model = model_of(ctx.body);
    let model_label = if model.is_empty() { "(默认)".to_string() } else { model.clone() };
    // 没有账号记录（走默认登录态）时，只有声明了环境变量旁路的 provider 能继续
    // —— 与无状态路径同一判据与文案（`allows_anonymous_default_session`）
    if target.account_id.is_none() && !adapter.allows_anonymous_default_session() {
        return Err(GatewayError::with_status(
            503,
            format!(
                "{} 没有可用账号，无账号可转发：请在账号页添加并启用账号",
                meta(kind).label
            ),
        ));
    }
    // 取凭证。失败**不致命**：真正的凭证问题会由 `forward_conversation` 内部
    // 给出更准的文案（例如「auth.json 缺失，请先在 CatPaw 桌面端登录」），
    // 这里提前报错反而会把「适配器其实能拿到凭证」的请求挡掉。注意 CatPaw
    // 没有刷新机制（§9.1）：`ensure_access_token` 只做存在性校验。
    if let Some(account_id) = target.account_id.clone() {
        if let Err(error) = adapter.ensure_access_token(&service.store, &account_id).await {
            logging::verbose(
                "[Upstream]",
                &format!(
                    "账号 {account_id} 的凭证准备失败（继续尝试转发）: {}",
                    error.message
                ),
            );
        }
    }
    // ── 内容处理：凭证已就绪、这一家**即将发送**，此刻才决定发送体 ────────
    // 与无状态路径同一时机与同一判据：选路失败（503/401）的请求走不到这里，
    // 不会产生一次「已转发的处理」；未勾选的家拿到的是客户端原始请求体。
    let body = send_body(ctx, provider_id, target.account.as_ref());
    // 旁路记账：本 provider + 本账号是这一轮的实际承载者（attempts +1）。
    // 账号展示名的兜底链与无状态路径同（账号名 → 会话昵称 → 账号 id）；
    // 这里没有会话对象可传（会话还没建），用公开形态的名字。
    // 算一次、两处用，理由同无状态路径。
    let attempt_account = account_label(
        target.account.as_ref(),
        target.account_id.as_deref().unwrap_or(""),
        &Value::Null,
    );
    ctx.telemetry
        .note_attempt(target.account_id.as_deref(), &attempt_account, provider_id);
    // 尝试明细的起头：与 note_attempt 配对（同上一条注释的说明）。
    // 本路径的定稿在下面 match 的两个分支里 —— 有状态 provider 没有账号轮换，
    // 所以一轮就是一条明细，链路至多一项（`provider_loop` 的 `'accounts` 循环
    // 仍可能在外层顺延到下一个账号，那会走本函数第二次调用）。
    ctx.telemetry.note_attempt_started(provider_id, &attempt_account);
    if let Some(notice) = target.proxy_notice.as_deref() {
        ctx.telemetry.note_attempt_notice(notice);
    }
    let started_at = logging::now_ms();
    logging::verbose(
        "[Upstream]",
        &format!(
            "会话式转发 model={} stream={} account={} priority={} 出口={} provider={provider_id}",
            model_label,
            ctx.stream,
            target.account_id.as_deref().unwrap_or("-"),
            target
                .priority
                .map(|value| value.to_string())
                .unwrap_or_else(|| "-".to_string()),
            describe_proxy(target.proxy.as_ref()),
        ),
    );
    match adapter
        .forward_conversation(
            &service.store,
            target.account_id.as_deref().unwrap_or(""),
            &body,
            ctx.client_headers,
            target.proxy.clone(),
            ctx.stream,
            ctx.telemetry,
        )
        .await
    {
        Ok(outcome) => {
            // 成功：该账号对该模型的限额标记（如有）已失效，清除。对本路径是
            // 空操作（不写标记），共用它是为了「将来某家产生标记」时自动获得清理
            cap_cleared(service, &target, &model, &model_label, &Value::Null, provider_id);
            logging::verbose(
                "[Upstream]",
                &format!("会话式转发完成（{}ms）", logging::now_ms() - started_at),
            );
            // 明细定稿：这条路径的成功标志是「outcome 拿到了」（会话式转发的
            // 状态码由协议层自定，取不到上游 HTTP 码），所以 status 给 None、
            // 不带错误 —— 前端把它渲染成「成功」（无状态码）。
            ctx.telemetry.finish_last_attempt(None, None);
            Ok(attach_slot(outcome, slot, connections))
        }
        // 一律透传（Fatal 语义，核对结论见函数头）：不换账号、不冷却、不重试
        Err(error) => {
            // 只在终端：这条错误会作为 GatewayError 抛回入口，由 `api::chat` /
            // `api::protocol` 记进请求日志（明细的 error 也是同一条文案）。
            logging::console_line("[Upstream]", &format!("❌ {}", error.message));
            // 明细定稿：状态码取网关错误的（有状态路径的错误由适配器定档，
            // 502/500/上游码都可能），错误摘要用同一条文案 —— 与列表里
            // 「错误」列显示的是同一个根因。
            ctx.telemetry
                .finish_last_attempt(Some(i64::from(error.status_code)), Some(&error.message));
            Err(error)
        }
    }
}

/// 有状态路径的槽位处理：**流式挂到流上、非流式当即释放**（见 `attempt_stateful`）。
///
/// 为什么要挂到流上（而不是拿个 outcome 就放行）：去重队列的语义是「相同 body 的
/// 快速重试在代理内排队等前一个**完成**」，而「完成」的定义是「响应字节下发完」
/// （`InFlightGuard` 的说明）。有状态 provider 的下行帧由协议层的后台任务产出，
/// 流对象本身是通用的 `Box<dyn Stream>`，于是这里包一层只为**持有那个凭证** ——
/// 不读、不改、不缓存字节，透传语义逐字节不变。
///
/// 账号级连接凭证（`connections`）走**同一条规则**：流式时一起挂到流上，
/// 非流式时随本函数返回析构。两条凭证的生命周期因此完全一致，不需要各自
/// 维护释放时机。
fn attach_slot(
    outcome: ForwardOutcome,
    slot: &mut Option<InFlightGuard>,
    connections: &mut ConnectionGuard,
) -> ForwardOutcome {
    match outcome {
        ForwardOutcome::Stream { status, stream } => ForwardOutcome::Stream {
            status,
            stream: Box::new(SlotHoldingStream {
                inner: stream,
                _slot: slot.take(),
                // handoff 转移所有权：本栈帧的凭证随即失效（见其说明）
                _connection: connections.handoff(),
            }),
        },
        other => {
            // 非流式：聚合已经完成，凭证在这里析构即放行（与无状态路径同）
            drop(slot.take());
            other
        }
    }
}

/// 只为一个目的存在的流包装：**持有在途槽位与连接计数两份凭证**（见 `attach_slot`）。
///
/// 逐项原样转发 `inner` 的每个 `Item`（含 `Err`），没有自己的缓冲与状态。
struct SlotHoldingStream {
    inner: Box<dyn futures::Stream<Item = Result<bytes::Bytes, std::io::Error>> + Send + Unpin>,
    /// 凭证：本流被 drop（流跑完 / 客户端断开 / 服务退出）时析构并放行等待者
    _slot: Option<InFlightGuard>,
    /// 账号级连接凭证：同一时刻析构，把该账号的计数 -1
    _connection: ConnectionGuard,
}

impl futures::Stream for SlotHoldingStream {
    type Item = Result<bytes::Bytes, std::io::Error>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use futures::StreamExt;
        // 字段全是 Unpin（Box / Option），自身即 Unpin，get_mut 安全
        self.get_mut().inner.poll_next_unpin(cx)
    }
}

/// 请求成功后的限额标记清理（含「已恢复可用」事件）。
///
/// 独立出来是因为成功路径在 401 刷新重试之后才到达，而那时 `target` 与
/// `session` 都还在作用域里 —— 抽成函数让「成功到底清了什么」一眼可见。
fn cap_cleared(
    service: &UpstreamService,
    target: &RouteTarget,
    model: &str,
    model_label: &str,
    session: &Value,
    provider_id: &str,
) {
    let Some(account_id) = target.account_id.as_deref() else {
        return;
    };
    let had_limit = rotate::account_had_limit(service, account_id, model);
    service.store.clear_rate_limit(account_id, model);
    // 仅当之前确实处于限额状态才记一条，避免每次成功请求都刷日志
    if had_limit {
        let label = account_label(target.account.as_ref(), account_id, session);
        rotate::report_limit_event(
            "info",
            &format!("账号「{label}」对模型 {model_label} 已恢复可用"),
            None,
            Some(&label),
            model,
            None,
            0,
            0,
            None,
            provider_id,
        );
    }
}

/// 上游请求体里的模型名（缺失/非字符串给空串，与改造前一致）
fn model_of(body: &Value) -> String {
    body.get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// SSE/聚合响应的 model 名回写参数：要不要改写由适配器回答
/// （小浣熊上游会回自己的内部名，见 `providers::raccoon` 与 `sse.rs` 的模块头）。
/// 未声明回写的 provider 得 None，下发帧逐字节不变（workbuddy 的硬要求）。
fn model_rewrite_of(adapter: &dyn ProviderAdapter, model: &str) -> Option<super::sse::ModelRewrite> {
    if adapter.sse_model_rewrite() {
        Some(super::sse::ModelRewrite { requested: model.to_string() })
    } else {
        None
    }
}

/// 发一次上游请求，含「可退避重试」循环（次数 / 间隔来自设置页的全局重试设置）。
///
/// 成功的定义是 HTTP 2xx —— 与改造前 `request_with_waf_retry` 一致。
///
/// 重试判定分两档：
///   - **适配器声明**（`retry_advice`）：provider 专属知识（workbuddy 的 11128），
///     要不要退避、原因，全由适配器回答（见模块头）；
///   - **统一兜底**（[`transient_retry_advice`] / [`transport_retry_advice`]）：
///     瞬时 HTTP 状态码与传输层失败按同一套设置原样重发 —— 这是「重试设置
///     对全部提供商生效」的入口，适配器没声明的家也能吃到同一份配置。
///
/// ── 每次退避为什么只写请求日志、不写运行日志 ─────────────────
/// 退避重试是**逐请求**的过程事实（一次 11128 拦截会连打 3 行），而它此前
/// 只能去「日志」页看 —— 请求日志那一行请求上恰好什么都看不出来
/// （同账号重试不计入 `attempts`，也不追加尝试明细）。现在每次退避由
/// `note_attempt_retry` 记进**本轮那条明细**的下级重试链，请求日志一次悬停
/// 就能看到「为什么重试了几次」；控制台仍留一行（`console_line`），
/// 方便用终端排障时实时看到。
///
/// ── `budget` 为什么要 `&mut`（原地重发这一档怎么落地）────────
/// 「同一个账号重试 N 次」的预算是**整份请求共用一份**：由 `attempt_queue`
/// 在循环外按 `RetrySettings::resend_budget()` 开好，本函数每退避一次就减一。
/// 于是它天然花在第一个真正发出去的账号上 —— 那份用完，后面换来的账号就只发
/// 一次（不再原地重发），这正是「同一账号重试 3 次」该有的样子。
///
/// 与「还能换几个账号」是两套独立预算：那份归 `attempt_queue` 的 `switches_left`
/// 管（见 [`take_switch`]），两者互不抵扣。
async fn send_with_retry(
    adapter: &dyn ProviderAdapter,
    transport: &TransportRequest,
    budget: &mut RetryBudget,
    capture: Option<&crate::server::core::debug_traffic::TrafficCapture>,
    telemetry: &crate::server::core::upstream::usage::RequestTelemetry,
) -> Result<reqwest::Response, OutboundFailure> {
    loop {
        let response = match send_chat_request(transport).await {
            Ok(response) => response,
            Err(error) => {
                // 传输层失败（DNS/代理/连接）：按设置退避重发，吸收链路抖动；
                // 次数用完才收敛成 502，与改造前的兜底一致
                if let Some(advice) = transport_retry_advice(budget.remaining) {
                    budget.remaining -= 1;
                    let used = budget.used();
                    telemetry.note_attempt_retry(&advice.reason, None, advice.delay_ms);
                    logging::console_line(
                        "[Upstream]",
                        &retry_log_line(&advice.reason, advice.delay_ms, used, budget.total),
                    );
                    tokio::time::sleep(Duration::from_millis(advice.delay_ms)).await;
                    continue;
                }
                let gateway = error.to_gateway_error();
                return Err(OutboundFailure {
                    class: UpstreamErrorClass::Fatal {
                        status: 502,
                        message: gateway.message.clone(),
                        upstream_code: None,
                    },
                    error: gateway,
                });
            }
        };
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status().as_u16();
        let detail = read_upstream_error(response, capture).await;
        let body = detail.to_value();
        // 退避重试：provider 专属判定优先，没声明时对瞬时状态码统一兜底
        let advice = adapter
            .retry_advice(&body, budget.used(), budget.total)
            .or_else(|| transient_retry_advice(status, budget.remaining));
        if let Some(advice) = advice {
            budget.remaining = budget.remaining.saturating_sub(1);
            let used = budget.used();
            telemetry.note_attempt_retry(&advice.reason, Some(i64::from(status)), advice.delay_ms);
            logging::console_line(
                "[Upstream]",
                &retry_log_line(&advice.reason, advice.delay_ms, used, budget.total),
            );
            tokio::time::sleep(Duration::from_millis(advice.delay_ms)).await;
            continue;
        }
        // 定论的上游错误：这一行只在**终端**留痕。请求日志那侧由本轮明细的
        // `error`（同一个 message）回答，两处不再各写一份。
        logging::console_line(
            "[Upstream]",
            &format!("上游错误 HTTP {status}: {}", detail.message),
        );
        let class = adapter.classify_error(status, &body);
        // 文案由适配器给出（含 provider 提示），编排层原样组装成网关错误
        let error = match &class {
            UpstreamErrorClass::QuotaLimited { status, message, upstream_code, .. } => {
                GatewayError::with_status(*status as i32, message.clone())
                    .with_optional_code(*upstream_code)
            }
            UpstreamErrorClass::Fatal { status, message, upstream_code } => {
                GatewayError::with_status(*status as i32, message.clone())
                    .with_optional_code(*upstream_code)
            }
            UpstreamErrorClass::TokenExpired { message } => {
                GatewayError::with_status(status as i32, message.clone())
                    .with_optional_code(detail.code)
            }
        };
        return Err(OutboundFailure { class, error });
    }
}
