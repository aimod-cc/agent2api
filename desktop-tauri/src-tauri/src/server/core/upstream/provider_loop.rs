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

use std::borrow::Cow;
use std::collections::HashMap;
use std::time::Duration;

use serde_json::{json, Value};

use crate::server::core::providers::adapter::{adapter_for, ProviderAdapter, UpstreamErrorClass};
use crate::server::core::providers::router::route_for_forward;
use crate::server::core::providers::{kind_from_id, kind_id, meta, ProviderKind};
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::payload::{send_body, ProviderContext};
use super::request::{read_upstream_error, send_chat_request, TransportRequest};
use super::{
    account_display, account_label, describe_proxy, reset_hint, rotate, ForwardOutcome,
    InFlightGuard, RouteTarget, UpstreamService, MAX_ROUTE_ATTEMPTS,
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

/// 转发入口：在候选家的全部账号里按全局优先级逐个尝试。
///
/// `slot` 是在途槽位凭证（`&mut` 是因为它只在**成功转为流式**时才被取走，
/// 失败重试时仍由本函数持有；见 `InFlightGuard` 的说明）。
pub(super) async fn forward_with_providers(
    service: &UpstreamService,
    ctx: ProviderContext<'_>,
    slot: &mut Option<InFlightGuard>,
) -> Result<ForwardOutcome, GatewayError> {
    let model = model_of(ctx.body);
    // 候选家来自 `route_for_forward`（目录里没有这个模型名时会回落成默认
    // provider 一家）—— 本函数是唯一消费方，日志也打在这里。
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
        // 注册表里连默认 provider 都没有时才会走到（见 `route_for_forward`）
        return Err(GatewayError::new("没有可用的提供商，无法转发"));
    }
    let provider_ids: Vec<&str> = candidates.iter().map(|kind| kind_id(*kind)).collect();
    logging::verbose(
        "[Upstream]",
        &format!("候选提供商 {}（按账号全局优先级选路）", provider_ids.join(" / ")),
    );
    attempt_queue(service, &ctx, &provider_ids, slot).await
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
) -> Result<ForwardOutcome, GatewayError> {
    let model = model_of(ctx.body);
    let model_label = if model.is_empty() { "(默认)".to_string() } else { model.clone() };
    let mut tried_ids: Vec<String> = Vec::new();
    // 各家的发送体：某一家即将发送前按作用范围决定一次，换到同一家的另一个账号
    // 时复用（不重复处理、不重复统计）。勾选的家用处理副本，未勾选的用原始 body。
    let mut send_cache: HashMap<&'static str, Cow<'_, Value>> = HashMap::new();

    // 标签是必需的：下面「429 降级到下一个账号」发生在**内层发送循环**里，
    // 裸 `continue` 会回到内层（用同一个账号再发一次，正好是要避免的事）。
    // `continue 'accounts` 才表达「换队列里的下一个账号」。
    'accounts: for _ in 0..=MAX_ROUTE_ATTEMPTS {
        let target = rotate::select_target_account(service, provider_ids, &model, &tried_ids).await?;
        let Some(kind) = kind_from_id(&target.provider) else {
            return Err(GatewayError::with_status(
                503,
                format!("账号所属提供商「{}」未知，无法转发", target.provider),
            ));
        };
        let adapter = adapter_for(kind);
        let provider_id = kind_id(kind);
        if adapter.is_stateful() {
            // 会话式转发（CatPaw）内部没有轮换，但**队列的兜底仍然生效**：
            // 它失败了就把它记入已尝试、回到循环挑下一个账号 —— 可能已经换了一家。
            // 没有下一个可用账号时，把这个错误原样透传（它的文案最贴近真实原因）。
            let stateful_account_id = target.account_id.clone();
            match attempt_stateful(service, ctx, kind, adapter, target, slot).await {
                Ok(outcome) => return Ok(outcome),
                Err(error) => {
                    if let Some(account_id) = stateful_account_id {
                        if !tried_ids.contains(&account_id) {
                            tried_ids.push(account_id);
                        }
                    }
                    match rotate::pick_next_account(service, provider_ids, &model, &tried_ids) {
                        Some(next) => {
                            logging::log(
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
        ctx.telemetry.note_attempt(
            target.account_id.as_deref(),
            &account_label(
                target.account.as_ref(),
                target.account_id.as_deref().unwrap_or(""),
                &session,
            ),
            provider_id,
        );

        // ── 内容处理：凭证已就绪、这一家**即将发送**，此刻才决定发送体 ────
        // 位置在选路/凭证之后：没有可用账号（上面的 503/401 提前返回）的请求
        // 走不到这里，不会产生一次「已转发的处理」统计。
        let body = send_cache
            .entry(provider_id)
            .or_insert_with(|| send_body(ctx, provider_id));

        // ── 一次账号内的发送链：最多两次（首次 + 401 刷新后重试一次）────
        // 为什么把刷新重试并进同一个循环：重试**自己也可能是** 429
        // （额度确实用尽）。若把它当成独立分支直接返回，就会把一条限额错误
        // 当成终态发给客户端 —— 而正确的动作是「标记冷却 + 降级到下一个账号」。
        // 并进同一循环后，两次发送的错误走同一套分类处理。
        let started_at = logging::now_ms();
        let mut refreshed = false;
        let response = loop {
            let plan = adapter.build_chat_request(&session, body, ctx.client_headers)?;
            let payload = serde_json::to_string(&plan.body)
                .map_err(|error| GatewayError::new(format!("请求体序列化失败: {error}")))?;
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
            match send_with_retry(adapter, &transport).await {
                Ok(response) => break response,
                Err(failure) => {
                    // ── 动作 2：token 失效 → 刷新后同一账号重试一次 ──────
                    // 只在「首次失败 + 是 token 失效 + 还没刷新过」时走。
                    // 刷新失败、或重试再失败（此时 `refreshed` 已为 true）
                    // 都落到下面同一套分类处理，不会空转。
                    if matches!(failure.class, UpstreamErrorClass::TokenExpired { .. })
                        && !refreshed
                    {
                        refreshed = true;
                        let account_id = target.account_id.clone().unwrap_or_default();
                        logging::log(
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
                        }
                    }
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
                                    logging::log(
                                        "[Upstream]",
                                        &format!(
                                            "⚠️ 账号 {from_label} 对模型 {model} 已限额{reset_hint}，\
                                             按优先级降级 → {next_label}（优先级 {}{next_home}）",
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
                    if let Some(account_id) = target.account_id.clone() {
                        if !tried_ids.contains(&account_id) {
                            tried_ids.push(account_id);
                        }
                    }
                    match rotate::pick_next_account(service, provider_ids, &model, &tried_ids) {
                        Some(next) => {
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
                            logging::log(
                                "[Upstream]",
                                &format!(
                                    "⚠️ 账号 {} 对模型 {model} 转发失败（HTTP {}），                                     按队列顺延 → {}（优先级 {}{next_home}）",
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

        if ctx.stream {
            let status = response.status().as_u16();
            return Ok(ForwardOutcome::Stream {
                status,
                // 槽位交给流：流跑完 / 客户端断开 / 流被 drop 时才放行等待者
                stream: Box::new(super::ForwardStream::new(
                    response,
                    slot.take(),
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
async fn attempt_stateful(
    service: &UpstreamService,
    ctx: &ProviderContext<'_>,
    kind: ProviderKind,
    adapter: &dyn ProviderAdapter,
    target: RouteTarget,
    slot: &mut Option<InFlightGuard>,
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
    let body = send_body(ctx, provider_id);
    // 旁路记账：本 provider + 本账号是这一轮的实际承载者（attempts +1）。
    // 账号展示名的兜底链与无状态路径同（账号名 → 会话昵称 → 账号 id）；
    // 这里没有会话对象可传（会话还没建），用公开形态的名字。
    ctx.telemetry.note_attempt(
        target.account_id.as_deref(),
        &account_label(
            target.account.as_ref(),
            target.account_id.as_deref().unwrap_or(""),
            &Value::Null,
        ),
        provider_id,
    );
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
            Ok(attach_slot(outcome, slot))
        }
        // 一律透传（Fatal 语义，核对结论见函数头）：不换账号、不冷却、不重试
        Err(error) => {
            logging::log("[Upstream]", &format!("❌ {}", error.message));
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
fn attach_slot(outcome: ForwardOutcome, slot: &mut Option<InFlightGuard>) -> ForwardOutcome {
    match outcome {
        ForwardOutcome::Stream { status, stream } => ForwardOutcome::Stream {
            status,
            stream: Box::new(SlotHoldingStream { inner: stream, _slot: slot.take() }),
        },
        other => {
            // 非流式：聚合已经完成，凭证在这里析构即放行（与无状态路径同）
            drop(slot.take());
            other
        }
    }
}

/// 只为一个目的存在的流包装：**持有在途槽位凭证**（见 `attach_slot`）。
///
/// 逐项原样转发 `inner` 的每个 `Item`（含 `Err`），没有自己的缓冲与状态。
struct SlotHoldingStream {
    inner: Box<dyn futures::Stream<Item = Result<bytes::Bytes, std::io::Error>> + Send + Unpin>,
    /// 凭证：本流被 drop（流跑完 / 客户端断开 / 服务退出）时析构并放行等待者
    _slot: Option<InFlightGuard>,
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

/// 发一次上游请求，含「可退避重试」循环（11128 之类；建议由适配器给出）。
///
/// 成功的定义是 HTTP 2xx —— 与改造前 `request_with_waf_retry` 一致。
async fn send_with_retry(
    adapter: &dyn ProviderAdapter,
    transport: &TransportRequest,
) -> Result<reqwest::Response, OutboundFailure> {
    let mut attempt = 0usize;
    loop {
        let response = match send_chat_request(transport).await {
            Ok(response) => response,
            Err(error) => {
                // 传输层失败（DNS/代理/连接）：收敛成 502，与改造前一致
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
        let detail = read_upstream_error(response).await;
        let body = detail.to_value();
        // 退避重试：要不要退避、退多久、文案，全由适配器回答（见模块头）
        if let Some(advice) = adapter.retry_advice(&body, attempt) {
            logging::log("[Upstream]", &advice.log_message);
            tokio::time::sleep(Duration::from_millis(advice.delay_ms)).await;
            attempt += 1;
            continue;
        }
        logging::log(
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
