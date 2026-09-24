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
//! ── 内容处理（系统提示词 + 脱敏）在哪一步生效 ─────────────────
//! 见 `payload.rs`：两层处理只在某一家即将发送前落到**副本**上。本文件负责
//! 在正确时机调 [`send_body`]（选路与凭证就绪之后、构造请求之前），同一家
//! 同池同降级状态的发送体在本次请求内只算一次（换到同一家的另一个账号时复用）。
//!
//! ── 内容拦截的补救（动作 0）为什么也在本文件 ──────────────────
//! 上游按**逐字**匹配审核，撞上就是 HTTP 400 —— 那多半是客户端 system 模板
//! 的指纹误报（见 `core::sanitize`）。补救分两步：**换最小中性提示词立刻重发
//! 一次**（就在下面的账号内发送循环里，与 401 刷新重试同一位置），并触发降级
//! 状态机（`core::degrade`）让后续请求直接带中性提示词出门。两步都不换账号、
//! 不罚账号 —— 内容问题不是账号问题（照搬参考项目的 `ErrContentBlocked`）。
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

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{json, Value};

use crate::server::config;
use crate::server::core::custom_providers;
use crate::server::core::providers::adapter::{
    adapter_for, ProviderAdapter, RetryAdvice, UpstreamErrorClass,
};
use crate::server::core::providers::custom::forward as custom_forward;
use crate::server::core::providers::router::route_for_forward;
use crate::server::core::providers::{kind_from_id, kind_id, meta, ProviderKind};
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::payload::{send_body, ProviderContext, SendBody};
use super::request::{read_upstream_error, send_chat_request, TransportRequest};
use super::usage::LogPhase;
use super::{
    account_display, account_label, cancellation, connections::ConnectionGuard, describe_proxy,
    reset_hint, rotate, ForwardOutcome, InFlightGuard, RouteTarget, UpstreamService,
    MAX_ROUTE_ATTEMPTS,
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

/// 这次的失败状态码是否命中「指定错误码不重试」名单（设置页「通用 → 请求重试」）。
///
/// 命中的失败**原样收尾**：不进 [`send_with_retry`] 的原地重发，也不走
/// attempt_queue 的换号顺延 —— 名单的语义就是「这个码没什么可试的，把错误
/// 直接给客户端」。内容拦截补救（动作 0）与 401 刷新（动作 2）也是重试，
/// 同样被它挡住（所以判定点放在那两个动作之前）。
///
/// 传输层失败（DNS / 代理 / 连接）没有上游状态码，不受名单管。
/// `status_code` 是 `i64` 形态（GatewayError 存的是 `i32`，这里统一收宽），
/// 负值 / 越界不可能是 HTTP 状态码，按「不在名单」处理。
fn no_retry_status(status: i64) -> bool {
    u16::try_from(status)
        .map(|code| config::retry_settings().no_retry(code))
        .unwrap_or(false)
}

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
///
/// 原因取错误自带的简短形态（`UpstreamRequestError::reason`）：连接超时与
/// 等待响应头超时各自带设置页旋钮名与实际秒数，其余统一「上游连接失败」。
fn transport_retry_advice(error: &super::request::UpstreamRequestError, remaining: usize) -> Option<RetryAdvice> {
    if remaining == 0 {
        return None;
    }
    Some(RetryAdvice {
        delay_ms: config::retry_settings().delay_ms(),
        reason: error.reason.clone(),
    })
}

/// 兜底退避建议：**非限额**的上游错误，在换账号之前先在同一账号上重发。
///
/// ── 为什么需要这一档 ────────────────────────────────────────
/// 前两档都有明确的适用面：适配器专属判定只覆盖它认识的那几个码
/// （workbuddy 只认 11128 敏感词），瞬时状态码只收 408/5xx。两者都不命中时，
/// 请求会直接跳到「换账号」—— 而设置页那个「同一账号重试次数」**一次都没用上**。
/// 用户实测：填了 3，一次 400 却看到 6 个账号各试一次、重试链里
/// `retries` 全空（2026-09）。
///
/// 语义上这就是该设置项的字面承诺：**先在这个账号上多试几次，不行再换人**。
/// 判据放在「分类之后」而不是「按状态码硬编码」，是因为 400 这类码的含义
/// 完全由上游决定（同一个 400 可能是报文非法、也可能是上游自己状态不一致），
/// 网关无从分辨 —— 而重发的代价只是一个间隔，换号的代价是消耗另一个账号的
/// 额度与一次可能的限额标记。先重发更划算。
///
/// ── 排除项（各自有更合适的动作）─────────────────────────────
///   - `QuotaLimited`（429 / 限额码）：那是账号级限额，走「标记冷却 + 换号」，
///     在原地重发只会白等一个间隔（与 [`TRANSIENT_RETRY_STATUSES`] 不收 429
///     同一条理由）；
///   - `TokenExpired`（401）：有专属动作（刷新凭证后同账号重试一次），
///     走到这里说明已经刷过一轮仍失败，重发没有新变量；
///   - `ContentBlocked`（内容策略拦截）：也有专属动作（换中性提示词重发一次，
///     见 `attempt_queue` 的动作 0），走到这里说明那次补救已经用掉 ——
///     再原地重发同一份 body 结论不变（拦的是字节，不是账号）。
fn fallback_retry_advice(
    class: &UpstreamErrorClass,
    remaining: usize,
    status: u16,
) -> Option<RetryAdvice> {
    if remaining == 0 {
        return None;
    }
    match class {
        UpstreamErrorClass::QuotaLimited { .. }
        | UpstreamErrorClass::TokenExpired { .. }
        | UpstreamErrorClass::ContentBlocked { .. } => None,
        UpstreamErrorClass::Fatal { .. } => Some(RetryAdvice {
            delay_ms: config::retry_settings().delay_ms(),
            reason: format!("上游错误（HTTP {status}），换账号前先原地重发"),
        }),
    }
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
        // 没有，或者这个名字的承载家**全被关闭**（那时不回落默认家）。后者是
        // 用户可操作的，文案要指出去哪儿改，不能只说「没有可用的提供商」。
        if crate::server::core::providers::catalog::model_blocked_everywhere(&model) {
            return Err(GatewayError::with_status(
                404,
                format!(
                    "模型已在网关中关闭: {model}。完整列表见 GET /v1/models"
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
    // 候选链已是 id 空间（`route_for_forward` 的返回值，见 router 的模块头）：
    // 内置家与自定义家的 id 同列，自定义 id 直接透传给选路与分派。
    let provider_ids: Vec<&str> = candidates.iter().map(String::as_str).collect();
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
/// 候选链是 id 空间（内置家 + 自定义家同列）：`KeyScope` 的提供商白名单本来
/// 就是 id 字符串数组（见 `core::key_scope`），两种 id 直接可比 —— 自定义 id
/// 在这里不需要任何转换，勾了就放行、没勾就被剔除，与内置家同一判据。
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
    candidates: Vec<String>,
    scope: Option<&crate::server::core::key_scope::KeyScope>,
    model: &str,
) -> Result<Vec<String>, GatewayError> {
    let Some(scope) = scope.filter(|scope| scope.restricts_providers()) else {
        // 没有 scope / 这个维度不限制 —— 原样放行（绝大多数请求走这条）
        return Ok(candidates);
    };
    let kept: Vec<String> = candidates
        .iter()
        .filter(|id| scope.allows_provider(id))
        .cloned()
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
    // 限额冷却键的解析器：把请求名解析成**各家上游真名**（见 `routing::CooldownKeys`）。
    // 建一次、整条请求共用 —— 选路、429 记账、成功清理三处读的必须是同一个键，
    // 否则冷却会写在一个名字上、查在另一个名字上。
    let cooldown_keys = rotate::CooldownKeys::new(&model);
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
    // ── 系统提示词的降级标记（对应参考项目的 `degradedApplied`）────────
    // `true` = 本请求的出站提示词已切到**中性提示词**：进入本请求时降级期已经
    // 生效（状态机在别的请求里被触发过），或本请求撞了内容拦截后由下面
    // 「动作 0」置位。置位后不再重复触发 —— 一次请求最多补救一次。
    //
    // `custom` 模式不可降级（system 已由网关接管，内容拦截不再指向 system
    // 指纹），这里并进判定：后面「动作 0」与发送体计算读的都是它。
    let mut degraded = crate::server::core::degrade::active() && ctx.prompt.mode.degradable();
    // 各家的发送体：某一家即将发送前按作用范围决定一次，换到**同一家同池**的
    // 另一个账号时复用（不重复处理、不重复统计）。键带账号的池（Cline 的账号
    // 记录有 `free`/`pass`）：发送名跟着实际承载的账号所在池走
    // （`wire_target_for_provider`），跨池账号的发送名不同，各算一份。
    // 勾选的家用处理副本，未勾选的用原始 body。
    //
    // 键的第三维是**降级标记**：内容拦截后本请求会换中性提示词（见「动作 0」），
    // 那一份发送体是另一份副本，键不同就自然落进另一条缓存项 —— 不必手工失效
    // 缓存，也不会把「降级前那份」错发给后面的账号。
    //
    // 值里同时带着**该家实际收到的上游模型名**（`SendBody::wire_model`）——
    // 它是限额冷却的键，与发出去的字节同源（见 `payload::SendBody`）。
    // 缓存因此不只省一次脱敏：429 记账与成功清理都从这里取真名，不必再解析一遍。
    let mut send_cache: HashMap<(&'static str, String, bool), SendBody<'_>> = HashMap::new();

    // 标签是必需的：下面「429 降级到下一个账号」发生在**内层发送循环**里，
    // 裸 `continue` 会回到内层（用同一个账号再发一次，正好是要避免的事）。
    // `continue 'accounts` 才表达「换队列里的下一个账号」。
    'accounts: for _ in 0..=MAX_ROUTE_ATTEMPTS {
        // ── 手动终止：每一轮选路之前先看令牌 ────────────────────────
        // 覆盖「回到循环」的所有时刻：去重排队、退避睡眠、换号顺延、上游
        // 响应之后的下一轮。更细的等待点（等响应头、退避）由 `send_with_retry`
        // 各自 select 令牌，这里兜住其余（含会话式 / 自定义家的两轮之间）。
        // 此刻没有在途尝试需要定稿（上一轮的两个出口都已记过明细），直接返回。
        if ctx.telemetry.is_cancelled() {
            return Err(cancellation::cancelled_error());
        }
        let target =
            rotate::select_target_account(service, provider_ids, &cooldown_keys, &tried_ids).await?;
        // 连接计数改绑到这一轮选中的账号：失败重试换账号时计数跟着走，
        // 于是「一个请求任意时刻只占一个账号」这条口径不需要每个分支各维护一次
        // （429 降级、401 刷新后换号、会话式失败顺延三条路径都经过这里）。
        connections.rebind(target.account_id.clone());
        // ── 自定义提供商的分流（第二阶段；与下面的 is_stateful 同形状）──
        // 位置在 `kind_from_id` 之前：custom id **不进** `ProviderKind`
        // （「不认识就是不认识」的全仓口径，见 `custom_providers` 的模块头），
        // 它有自己的「一次发送」（`providers::custom::forward`）。选路是共用的
        // —— 自定义账号就在全局队列里（`RouteTarget.provider` 对它就是
        // custom id，`provider_of` 天然兼容），分派只替换发送动作。
        if custom_providers::is_custom_provider_id(&target.provider) {
            let custom_provider_id = target.provider.clone();
            let custom_account_id = target.account_id.clone();
            match attempt_custom(service, ctx, target, slot, connections, degraded).await {
                Ok(outcome) => return Ok(outcome),
                Err(error) => {
                    // 手动终止优先（与无状态路径同一判定与理由）：不把已受理的
                    // 终止当成「这一轮失败」去顺延下一个账号
                    if ctx.telemetry.is_cancelled() {
                        ctx.telemetry.finish_last_attempt(
                            Some(cancellation::MANUAL_TERMINATED_STATUS),
                            Some(cancellation::MANUAL_TERMINATED),
                        );
                        return Err(cancellation::cancelled_error());
                    }
                    // 「指定错误码不重试」命中即原样收尾（与无状态路径同一判定，
                    // 见 no_retry_status）：不换号、错误直接给客户端
                    if no_retry_status(i64::from(error.status_code)) {
                        return Err(error);
                    }
                    // 失败的账号记入 tried，回到账号循环顺延 —— 与会话式
                    // 失败（is_stateful 分支）同一套兜底。
                    if let Some(account_id) = custom_account_id {
                        let first_failure = !tried_ids.contains(&account_id);
                        if first_failure {
                            tried_ids.push(account_id.clone());
                            // 429 → 限额冷却（与无状态路径的动作 1 同一落库
                            // 口径：键是**上游真名**，恢复时间由 attempt_custom
                            // 并进 message、`mark_account_limited` 内部再解析）。
                            // 自定义家没有「同名多池」之类的键重排，冷却键
                            // 与发送名同源（`custom::forward::cooldown_model`）。
                            if error.is_quota_limit() {
                                let wire_model = custom_forward::cooldown_model(
                                    &custom_provider_id,
                                    &model,
                                );
                                rotate::mark_account_limited(
                                    service,
                                    &account_id,
                                    &wire_model,
                                    error.status_code,
                                    error.upstream_code,
                                    None,
                                    &error.message,
                                );
                            }
                        }
                    }
                    match rotate::pick_next_account(
                        service,
                        provider_ids,
                        &cooldown_keys,
                        &tried_ids,
                    ) {
                        Some(next) => {
                            // 换号额度用尽 → 队列里即使还有人也不再顺延
                            // （`take_switch` 已写过那行终端日志），本次错误原样
                            // 返回：与「没有下一个可用账号」同一个出口。
                            if !take_switch(&mut switches_left, switch_total) {
                                return Err(error);
                            }
                            logging::console_line(
                                "[Upstream]",
                                &format!(
                                    "⚠️ 自定义转发失败，按队列顺延 → {}（优先级 {}）",
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
            match attempt_stateful(
                service,
                ctx,
                kind,
                adapter,
                target,
                slot,
                connections,
                degraded,
            )
            .await
            {
                Ok(outcome) => return Ok(outcome),
                Err(error) => {
                    // 手动终止优先（与无状态 / 自定义两条路径同一判定与理由）：
                    // 会话式这一轮已经结束，但用户要的是终止 —— 不把它当成
                    // 普通失败去顺延下一个账号（那会把终止拖成另一轮转发）
                    if ctx.telemetry.is_cancelled() {
                        ctx.telemetry.finish_last_attempt(
                            Some(cancellation::MANUAL_TERMINATED_STATUS),
                            Some(cancellation::MANUAL_TERMINATED),
                        );
                        return Err(cancellation::cancelled_error());
                    }
                    if let Some(account_id) = stateful_account_id {
                        if !tried_ids.contains(&account_id) {
                            tried_ids.push(account_id);
                        }
                    }
                    match rotate::pick_next_account(
                        service,
                        provider_ids,
                        &cooldown_keys,
                        &tried_ids,
                    ) {
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
        //
        // **发送体在下面那层发送循环里取**（不是在这里取一次就固定）：内容拦截
        // 后的补救是「换中性提示词再发一次」，那一份要靠 `degraded` 翻转后重新
        // 计算（见「动作 0」）。池名在这里算一次，循环里只借用。
        let account_pool = target
            .account
            .as_ref()
            .and_then(|account| account.get("pool"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();

        // ── 一次账号内的发送链：最多三次（首次 + 内容拦截补救一次 + 401 刷新重试一次）──
        // 为什么把刷新重试并进同一个循环：重试**自己也可能是** 429
        // （额度确实用尽）。若把它当成独立分支直接返回，就会把一条限额错误
        // 当成终态发给客户端 —— 而正确的动作是「标记冷却 + 降级到下一个账号」。
        // 并进同一循环后，两次发送的错误走同一套分类处理。
        //
        // 同一层还承载「内容拦截 → 换中性提示词重试一次」（动作 0）：它同样是
        // 「不换账号、就地再发一次」，只是换了 body 而不是凭证。两个 `continue`
        // 各自有一个一次性开关（`degraded` / `refreshed`），所以迭代次数有上界
        // ——不存在「一直重发」的路径。
        let started_at = logging::now_ms();
        let mut refreshed = false;
        let (response, wire_model) = loop {
            // 发送体在这一轮发送前取一次（同一家同池同降级状态下复用缓存项）：
            // 借用在本次迭代内有效，`continue`（401 刷新 / 内容拦截补救）时
            // 重新取 —— 于是「换了 body 的那次重试」拿到的一定是新的一份。
            let send = send_cache.entry((provider_id, account_pool.clone(), degraded)).or_insert_with(
                || send_body(ctx, provider_id, target.account.as_ref(), degraded),
            );
            let body = &send.body;
            // 这一家实际收到的上游模型名 = 它的限额冷却键（与字节同源，见 `SendBody`）。
            // 随发送体一起取（发送体换了，真名也随之重算），成功时随返回值交给
            // 循环外（`cap_cleared` 要读它）—— 所以它是 break 的第二个元素。
            let wire_model = send.wire_model.clone();
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
            match send_with_retry(
                adapter,
                &transport,
                &mut budget,
                capture.as_deref(),
                ctx.telemetry,
                degraded,
            )
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
                    break (response, wire_model);
                }
                Err(failure) => {
                    // ── 手动终止优先于一切重试动作 ────────────────────────
                    // 用户已经点了「终止请求」：换号 / 退避 / 刷新凭证都没有
                    // 意义。判定放在分类动作之前 —— 否则一次「恰好同时发生」
                    // 的上游错误会按 Fatal 走换号顺延，把已受理的终止拖到下一轮
                    // （下一轮虽然也会被循环顶拦下，但白跑一次选路与发送）。
                    // 本轮明细在这里定稿（与下面那条定稿出口互斥：直接 return）。
                    if ctx.telemetry.is_cancelled() {
                        ctx.telemetry.finish_last_attempt(
                            Some(cancellation::MANUAL_TERMINATED_STATUS),
                            Some(cancellation::MANUAL_TERMINATED),
                        );
                        return Err(cancellation::cancelled_error());
                    }
                    // ── 指定错误码不重试：命中即原样收尾 ──────────────────
                    // 位置在动作 0 / 动作 2 之前：内容拦截补救与 401 刷新也是
                    // 「同账号再发一次」，同样被名单挡住 —— 用户点名的码没有
                    // 任何例外。明细在这里定稿（与下面「不会再重发同一个账号」
                    // 处的那条定稿出口互斥：一次起头对应恰好一次定稿），
                    // 错误原样返回给客户端。
                    if no_retry_status(i64::from(failure.error.status_code)) {
                        ctx.telemetry.finish_last_attempt(
                            Some(i64::from(failure.error.status_code)),
                            Some(&failure.error.message),
                        );
                        return Err(failure.error);
                    }
                    // ── 动作 0：内容策略拦截 → 换中性提示词，同账号立即重试一次 ──
                    // 上游按逐字精确匹配审核，命中即整单拦截；这是**误报**而不是
                    // 账号问题（余额健康、未限流、session 未死），所以既不罚账号
                    // 也不换账号 —— 换一份最小中性提示词再发一次才有意义
                    // （照搬参考项目的 `ErrContentBlocked` 处理）。
                    //
                    // 只在「还没换过 + 这个模式可降级」时走：`custom` 模式的 system
                    // 已由网关接管，再撞拦截多半是用户内容本身触发审核，换提示词
                    // 解决不了（那类情况落到下面按普通错误收尾）。
                    //
                    // 与 401 刷新重试同一性质（同一个账号、同一条尝试明细，
                    // 换的是 body 不是账号）：所以它也**不**在这里给明细定稿，
                    // 重试成功时这一轮的结局就是成功。`degraded` 置位后不再重复
                    // 触发，一次请求最多补救一次。
                    if matches!(failure.class, UpstreamErrorClass::ContentBlocked { .. })
                        && !degraded
                        && ctx.prompt.mode.degradable()
                    {
                        degraded = true;
                        // 触发状态机（已在降级期内则不续期，返回原来的截止时刻）
                        crate::server::core::degrade::trigger();
                        let until_text = crate::server::core::degrade::until_text();
                        let reason = format!(
                            "内容策略拦截（疑似 system 指纹误报），换中性提示词重试一次；\
                             降级持续到 {until_text}（届时恢复「{}」）",
                            ctx.prompt.mode.label(),
                        );
                        ctx.telemetry.note_attempt_retry(
                            &reason,
                            Some(i64::from(failure.error.status_code)),
                            0,
                        );
                        // 状态变更（降级期是一段持续状态，影响后续所有请求）——
                        // 与「账号被标记限额」同档，进运行日志页；触发它的那一条
                        // 请求本身在请求日志的重试链里也能看到原因。
                        logging::log("[Upstream]", &format!("⚠️ {reason}"));
                        continue;
                    }
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
                            // 冷却键 = **上游真名**（不是请求名）：上游按它记额度，
                            // 判定侧（`cooldown_keys`）也按它查，两处同源见
                            // `routing::CooldownKeys`。
                            let reset_text = rotate::mark_account_limited(
                                service,
                                &account_id,
                                &wire_model,
                                *status as i32,
                                *upstream_code,
                                *reset_at,
                                &failure.error.message,
                            );
                            let limit_at =
                                rotate::account_limit_reset_at(service, &account_id, &wire_model);
                            // 日志文案与账号页的「限流」列同口径：以真名为主，
                            // 映射生效时补 `请求名 →` 前缀（见 `limit_model_label`）
                            let limit_label = limit_model_label(&model, &wire_model);
                            let from_label =
                                account_label(target.account.as_ref(), &account_id, &session);
                            match rotate::pick_next_account(
                                service,
                                provider_ids,
                                &cooldown_keys,
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
                                            "⚠️ 账号 {from_label} 对模型 {limit_label} 已限额{reset_hint}，\
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
                                            "账号「{from_label}」对模型 {limit_label} 已限额{reset_hint}，\
                                             按优先级降级 → 「{next_label}」",
                                        ),
                                        Some(&from_label),
                                        Some(&next_label),
                                        &wire_model,
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
                                            "模型 {limit_label} 在所有候选账号均已限额或禁用，\
                                             无法继续转发（尝试过 {} 个账号）",
                                            tried_ids.len(),
                                        ),
                                        Some(&from_label),
                                        None,
                                        &wire_model,
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
                    match rotate::pick_next_account(
                        service,
                        provider_ids,
                        &cooldown_keys,
                        &tried_ids,
                    ) {
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

        // 请求成功：该账号对该模型的限额标记（如有）已失效，清除。
        // 键是**上游真名**（与写入侧、判定侧同一个键，见 `routing::CooldownKeys`）。
        cap_cleared(
            service,
            &target,
            &wire_model,
            &limit_model_label(&model, &wire_model),
            &session,
            provider_id,
        );
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

/// **自定义提供商**的一次转发（第二阶段；与 [`attempt_stateful`] 同形状）。
///
/// ── 共用与不共用的部分（与有状态路径逐条对照）───────────────
/// ```text
///   共用：账号选路（全局队列里选出的 `target` 由调用方传入）、telemetry 记账、
///         429 冷却标记（在调用方的 Err 分支做）、在途槽位与连接计数的移交
///   不共用：「一次发送」的实现 —— 走 `providers::custom::forward`
///          （自定义家没有适配器，也不进 ProviderKind，见 mod.rs 的模块头）
/// ```
///
/// ── 与有状态路径的两处差别（都是刻意的）──────────────────────
///   1. **没有环境变量旁路**：自定义家的凭证只来自账号记录，`account_id`
///      为空直接报 503（判据与文案在 `custom::forward` 内部，这里不再预判
///      —— 报错点离原因最近，文案才不会漂移）；
///   2. **不调 `ensure_access_token`**：自定义账号的凭证是用户填的 apiKey，
///      没有「临期主动刷新」的概念 —— 读凭证就是读记录（`custom_credential_by_id`）。
///
/// ── 发送体与冷却键（为什么这里不调 `wire_target_for_provider`）──
/// `send_body` 对自定义 id 会原样放行（那套改写只认 modelRules 的映射表），
/// 所以「alias → 上游真名」与「思考等级注入」发生在 `custom::forward` 内部
/// （数据源是提供商记录上的 `models` / `mappings`）。冷却键在调用方的 Err
/// 分支用 `custom_forward::cooldown_model` 解析 —— 与发送名同一纯函数、同一
/// 结果，两次调用不会分叉（见那个函数的说明）。
///
/// 明细记账的定稿粒度与无状态路径对齐：成功时记**上游真实状态码**（流式在
/// `ForwardOutcome::Stream.status`、聚合完成恒为 200 —— 非 2xx 在 forward
/// 内部已经分类成错误了），失败时记网关错误的状态码与文案。
async fn attempt_custom(
    service: &UpstreamService,
    ctx: &ProviderContext<'_>,
    target: RouteTarget,
    slot: &mut Option<InFlightGuard>,
    connections: &mut ConnectionGuard,
    degraded: bool,
) -> Result<ForwardOutcome, GatewayError> {
    let provider_id = target.provider.clone();
    // 旁路记账：本 provider + 本账号是这一轮的实际承载者（attempts +1）。
    // 账号展示名的兜底链与有状态路径同（账号名 → 账号 id）；没有会话对象可传。
    let attempt_account = account_label(
        target.account.as_ref(),
        target.account_id.as_deref().unwrap_or(""),
        &Value::Null,
    );
    ctx.telemetry
        .note_attempt(target.account_id.as_deref(), &attempt_account, &provider_id);
    ctx.telemetry
        .note_attempt_started(&provider_id, &attempt_account);
    if let Some(notice) = target.proxy_notice.as_deref() {
        ctx.telemetry.note_attempt_notice(notice);
    }
    let started_at = logging::now_ms();
    // 内容处理（系统提示词 + 脱敏）与内置家同一时机：凭证已就绪、这一家
    // **即将发送**。`send_body` 对自定义 id 是零改写（它的模型名改写只认
    // modelRules），处理结果就是「提示词/脱敏后的客户端请求体」—— 自定义
    // 语义的改写（映射 alias → 真名、思考等级）在 forward 里做。
    let send = send_body(ctx, &provider_id, target.account.as_ref(), degraded);
    // 「上游模型」列以**真名**为准：send_body 对自定义 id 是零改写（它记的
    // 是请求名），真名的解析与改写发生在 forward 内部 —— 这里按同源解析
    // 覆盖一次（note_upstream_model 是覆盖式，最后一次为准；空串被内部过滤）。
    let wire_model = custom_forward::cooldown_model(&provider_id, &model_of(ctx.body));
    ctx.telemetry.note_upstream_model(&wire_model);
    logging::verbose(
        "[Upstream]",
        &format!(
            "自定义转发 model={} stream={} account={} priority={} 出口={} provider={provider_id}",
            limit_model_label(&model_of(ctx.body), &custom_forward::cooldown_model(&provider_id, &model_of(ctx.body))),
            ctx.stream,
            target.account_id.as_deref().unwrap_or("-"),
            target
                .priority
                .map(|value| value.to_string())
                .unwrap_or_else(|| "-".to_string()),
            describe_proxy(target.proxy.as_ref()),
        ),
    );
    match custom_forward::forward(
        &service.store,
        &provider_id,
        target.account_id.as_deref().unwrap_or(""),
        &send.body,
        target.proxy.clone(),
        ctx.stream,
        ctx.telemetry,
        slot,
        connections,
    )
    .await
    {
        Ok(outcome) => {
            // 成功：明细记**上游真实状态码**（流式在 outcome 里、聚合恒 200）。
            // 限额标记的清理对自定义家是空操作 —— 它们的冷却由调用方的 429
            // 分支写入，成功清理走 `cap_cleared` 的同一条路径（这里与有状态
            // 路径一样在 outcome 到手后调用，见 attempt_stateful 的说明）。
            let status = match &outcome {
                ForwardOutcome::Stream { status, .. } => i64::from(*status),
                ForwardOutcome::Completion { .. } => 200,
            };
            cap_cleared(
                service,
                &target,
                &custom_forward::cooldown_model(&provider_id, &model_of(ctx.body)),
                &limit_model_label(
                    &model_of(ctx.body),
                    &custom_forward::cooldown_model(&provider_id, &model_of(ctx.body)),
                ),
                &Value::Null,
                &provider_id,
            );
            logging::verbose(
                "[Upstream]",
                &format!("自定义转发完成: HTTP {status}（{}ms）", logging::now_ms() - started_at),
            );
            ctx.telemetry.finish_last_attempt(Some(status), None);
            Ok(outcome)
        }
        Err(error) => {
            // 只在终端：这条错误会作为 GatewayError 抛回入口（或由调用方的
            // Err 分支顺延），由 `api::chat` / `api::protocol` 记进请求日志。
            logging::console_line(
                "[CustomProvider]",
                &format!("❌ {}", error.message),
            );
            ctx.telemetry
                .finish_last_attempt(Some(i64::from(error.status_code)), Some(&error.message));
            Err(error)
        }
    }
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
    degraded: bool,
) -> Result<ForwardOutcome, GatewayError> {
    let provider_id = kind_id(kind);
    let model = model_of(ctx.body);
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
    // `degraded` 由调用方（账号循环）给出：本路径**没有**就地补救（有状态
    // provider 一次转发就是一个会话轮次，没有「换提示词重发」这一步），
    // 但降级期内（状态机已生效）首发的提示词也要跟着换。
    let send = send_body(ctx, provider_id, target.account.as_ref(), degraded);
    let body = &send.body;
    // 这一家实际收到的上游模型名 = 限额冷却键（与字节同源，见 `SendBody`）
    let wire_model = &send.wire_model;
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
            limit_model_label(&model, wire_model),
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
            body,
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
            cap_cleared(
                service,
                &target,
                wire_model,
                &limit_model_label(&model, wire_model),
                &Value::Null,
                provider_id,
            );
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

/// 限额事件的模型文案：以**上游真名**为主读数。
///
/// 冷却键是上游真名，账号页的「限流」列读的也是它 —— 日志若打印请求名，
/// 就会出现「日志说 `gpt-5.6-luna`、账号页说 `deepseek-v4.1-flash`」这种
/// 看起来像记错账号的分歧。所以两者都用真名，仅在请求名不同（映射生效）时
/// 补一段 `请求名 →` 前缀，用户仍能对上「我刚发的那个名字」。
fn limit_model_label(requested: &str, wire: &str) -> String {
    let requested = requested.trim();
    let wire = wire.trim();
    if wire.is_empty() {
        // 没有真名（请求体没带 model）：退回请求名，与改造前逐字一致
        return if requested.is_empty() { "(默认)".to_string() } else { requested.to_string() };
    }
    if requested.is_empty() || requested.eq_ignore_ascii_case(wire) {
        return wire.to_string();
    }
    format!("{requested} → {wire}")
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

/// 手动终止在 `send_with_retry` 里的返回形态（见 [`cancelled_error`] 的说明：
/// 调用方按令牌判定，不读这里的 `class`）。
fn cancelled_failure() -> OutboundFailure {
    OutboundFailure {
        class: UpstreamErrorClass::Fatal {
            status: cancellation::MANUAL_TERMINATED_STATUS as u16,
            message: cancellation::MANUAL_TERMINATED.to_string(),
            upstream_code: None,
        },
        error: cancellation::cancelled_error(),
    }
}

/// 发一次上游请求；被手动终止时**立即**放弃等待（不等响应头超时）。
///
/// 上游连接随 future 被丢弃而关闭（与客户端断开时的取消是同一机制），
/// 错误文案直接用手动终止的原文 —— 它不会被当成传输失败重试（调用方在
/// 传输失败分支的最前面查令牌）。
async fn send_or_cancel(
    transport: &TransportRequest,
    telemetry: &crate::server::core::upstream::usage::RequestTelemetry,
) -> Result<reqwest::Response, super::request::UpstreamRequestError> {
    let Some(token) = telemetry.cancel_token() else {
        return send_chat_request(transport).await;
    };
    tokio::select! {
        result = send_chat_request(transport) => result,
        _ = token.cancelled() => Err(super::request::UpstreamRequestError {
            message: cancellation::MANUAL_TERMINATED.to_string(),
            reason: cancellation::MANUAL_TERMINATED.to_string(),
        }),
    }
}

/// 退避睡眠；被手动终止时立即醒来（用户点了终止就不该再等一个间隔）。
///
/// 醒来之后由调用方的循环顶检查令牌并收尾 —— 这里不做判定，只负责「别睡着」。
async fn sleep_or_cancel(
    telemetry: &crate::server::core::upstream::usage::RequestTelemetry,
    delay_ms: u64,
) {
    let sleep = tokio::time::sleep(Duration::from_millis(delay_ms));
    tokio::pin!(sleep);
    match telemetry.cancel_token() {
        Some(token) => tokio::select! {
            _ = &mut sleep => {}
            _ = token.cancelled() => {}
        },
        None => sleep.await,
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
    degraded: bool,
) -> Result<reqwest::Response, OutboundFailure> {
    loop {
        // 手动终止：发送前先看令牌（退避睡眠 / 上一轮失败之后回到这里）。
        // 出口是 `cancelled_failure`，但调用方（attempt_queue）在分类动作之前
        // 会再查一次令牌并直接返回 —— 这个 class 只是让本函数的返回形状成立。
        if telemetry.is_cancelled() {
            return Err(cancelled_failure());
        }
        // 阶段：这一下就是**真正发出**上游请求的时刻 → 等待响应。
        // 为什么不能只靠尝试起头那一处（`note_attempt_started`）：同账号内的
        // 退避重发也走本循环（外面看不见），退避期间阶段是「重试中」，
        // 睡醒重发时必须推回去 —— 否则那一段等待首字节的时间会被显示成
        // 「重试中」，而它其实已经在等上游出字了。
        telemetry.note_phase(LogPhase::Waiting);
        let response = match send_or_cancel(transport, telemetry).await {
            Ok(response) => response,
            Err(error) => {
                // 手动终止：不把它当传输失败去退避重发（原因不是链路抖动）
                if telemetry.is_cancelled() {
                    return Err(cancelled_failure());
                }
                // 传输层失败（DNS/代理/连接）：按设置退避重发，吸收链路抖动；
                // 次数用完才收敛成 502，与改造前的兜底一致
                if let Some(advice) = transport_retry_advice(&error, budget.remaining) {
                    budget.remaining -= 1;
                    let used = budget.used();
                    telemetry.note_attempt_retry(&advice.reason, None, advice.delay_ms);
                    logging::console_line(
                        "[Upstream]",
                        &retry_log_line(&advice.reason, advice.delay_ms, used, budget.total),
                    );
                    sleep_or_cancel(telemetry, advice.delay_ms).await;
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
        // 错误响应体的读取同样受「非流式响应超时」管（对应 OmniProxy
        // readBodyWithStallGuard 的用法之一）：上游接了错误响应却迟迟不吐完
        // 出错体时，不能让「读错误」把请求挂住 —— 读不出来就当上游没给细节，
        // 分类仍按状态码走（classify_error 只看 status 也能给出结论）。
        let detail = {
            let budget = Duration::from_millis(config::timeout_settings().body_ms());
            match tokio::time::timeout(budget, read_upstream_error(response, capture)).await {
                Ok(detail) => detail,
                Err(_elapsed) => super::request::UpstreamErrorDetail {
                    code: None,
                    message: format!("非流式响应超时({}秒)", budget.as_secs()),
                },
            }
        };
        let body = detail.to_value();
        let class = adapter.classify_error(status, &body);
        // 退避重试：provider 专属判定优先，没声明时对瞬时状态码统一兜底
        //
        // ── 内容拦截为什么在 `!degraded` 时跳过这一整层 ──────────────
        // 首次撞内容拦截时，正确的补救是「换一份中性提示词立刻重发」（调用方的
        // 动作 0）—— 睡一个间隔、把**同一份** body 再发一遍只会确定性再撞一次墙
        // （拦截由指纹逐字匹配触发，字节没变结论就不会变），还白吃掉重试预算。
        // 换过提示词之后（`degraded`）才回到既有口径：仍被拦就按适配器的退避建议
        // 重试（11-128 的「拦截窗口会持续一小段时间」是实测结论），再不行才换账号。
        //
        // ── 「指定错误码不重试」为什么是第二道闸 ────────────────────
        // 用户点名的状态码（默认 402）连「再看一眼」都不值得：重发同一份 body
        // 结论不变。这里返回 None 会让下面的终端错误路径立即收尾，不再消耗
        // 原地重发预算 —— 换号那条路由 attempt_queue 里的同一判定拦（见
        // `no_retry_status` 的说明），两处合起来才是「这个码不重试」的完整语义。
        let advice = if !degraded && matches!(class, UpstreamErrorClass::ContentBlocked { .. }) {
            None
        } else if no_retry_status(i64::from(status)) {
            None
        } else {
            adapter
                .retry_advice(&body, budget.used(), budget.total)
                .or_else(|| transient_retry_advice(status, budget.remaining))
                .or_else(|| fallback_retry_advice(&class, budget.remaining, status))
        };
        if let Some(advice) = advice {
            budget.remaining = budget.remaining.saturating_sub(1);
            let used = budget.used();
            telemetry.note_attempt_retry(&advice.reason, Some(i64::from(status)), advice.delay_ms);
            logging::console_line(
                "[Upstream]",
                &retry_log_line(&advice.reason, advice.delay_ms, used, budget.total),
            );
            sleep_or_cancel(telemetry, advice.delay_ms).await;
            continue;
        }
        // 定论的上游错误：这一行只在**终端**留痕。请求日志那侧由本轮明细的
        // `error`（同一个 message）回答，两处不再各写一份。
        logging::console_line(
            "[Upstream]",
            &format!("上游错误 HTTP {status}: {}", detail.message),
        );
        // 文案由适配器给出（含 provider 提示），编排层原样组装成网关错误
        let error = match &class {
            UpstreamErrorClass::QuotaLimited { status, message, upstream_code, .. } => {
                GatewayError::with_status(*status as i32, message.clone())
                    .with_optional_code(*upstream_code)
            }
            // 内容拦截与 Fatal 的客户端形态相同（状态码 + 上游原文 + 上游码）：
            // 区别只在**编排动作**（前者不罚账号、先换提示词补救），不在文案。
            UpstreamErrorClass::ContentBlocked { status, message, upstream_code } => {
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
