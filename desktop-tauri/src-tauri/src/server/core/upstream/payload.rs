//! 一次转发的只读输入与**发送体选择**（从 `provider_loop.rs` 拆出，单文件行数约定）。
//!
//! ── 发送体为什么按 provider 逐家决定 ──────────────────────────
//! 请求体从 `api::chat` **原样**进来（去重键也取自原始请求体）。内容处理只在
//! **某一家 provider 的凭证已就绪、即将发送之前**发生，并按 `desensitize.json`
//! 的 `providers` 作用范围逐家判定（范围快照见 [`ProviderContext::desensitize_scope`]）：
//!   - 未勾选的家 → 客户端**原始**请求体（不处理、不计数）；
//!   - 勾选的家   → 用 core 的现有处理在**副本**上处理一次（命中日志由 core 打）。
//! 于是首选与故障转移到的未勾选家拿到的都是未修改的 body，绝不会复用上一家
//! 处理过的值；同一 provider **同池**换账号重试复用同一份（不重复处理、不重复
//! 统计 —— 键是「家 × 账号池」，因为发送名跟着账号所在池走，见下）；「这一家
//! 没有可用凭证」时根本走不到处理点，不产生一次已转发的处理。
//!
//! ── model 字段的按家改写（备援名）────────────────────────────
//! 候选链经备援扩池后，链上某家的目录里认的可能是**备援名**而不是请求名
//! （WorkBuddy 的 `deepseek-v4.1-flash` vs 小浣熊的 `sn-deepseek-v4-1-flash`）。
//! 上游只认识自己目录里的真名，所以这一家即将发送前，body 的 model 也要换成
//! 该家承载的那个名字（`catalog::wire_model_for_provider`）—— 同家多条映射
//! （Cline 两池同名的短名）时按**当前账号所在池**选，改写只发生在
//! **发出去的字节**上：`ctx.body`（记账 / 限额键 / 日志里的模型）保持客户端
//! 请求名不变。与脱敏同一个时机与缓存口径：同一家同池换账号重试复用同一份。
//!
//! 处理算法本身不在这里：脱敏判定与改写在 `core::desensitize`，模型名判定在
//! `core::providers::catalog`（本模块只决定「在什么时机、对哪一家、用哪一份」）。

use std::borrow::Cow;
use std::sync::Arc;

use axum::http::HeaderMap;
use serde_json::Value;

use crate::server::logging;

use super::usage::RequestTelemetry;

/// 一次转发的只读输入（打包传入，避免多参数函数在两层循环里各自展开）。
pub(super) struct ProviderContext<'a> {
    /// 客户端请求体（**原始**：已 stream:true；未做任何内容处理；未注入 system
    /// —— 那是适配器的事）。各 provider 的实际发送体由 [`send_body`] 决定。
    pub body: &'a Value,
    /// 客户端是否要流式（决定成功后的形态：SSE 透传 or 聚合）
    pub stream: bool,
    /// 客户端入站请求头（适配器契约的一部分；本期实现不读）
    pub client_headers: &'a HeaderMap,
    /// usage / 尝试次数旁路槽
    pub telemetry: &'a Arc<RequestTelemetry>,
    /// 本请求的脱敏作用范围**快照**（请求开始时取一次，见 `upstream::forward`）。
    ///
    /// 为什么随请求取快照而不是每家转发前现读：同一次请求内作用范围必须一致，
    /// 否则用户在请求进行中改了设置，会出现「前一家处理过、后一家不处理」
    /// 这类语义漂移；快照也让判定与日志用的是同一份范围。
    pub desensitize_scope: &'a [String],
}

/// 某一家 provider 实际要发送的请求体（**每次转发前**决定，不做跨家复用）。
///
/// 返回 `Cow`：脱敏未命中且模型名无需改写时零拷贝借出客户端原始 body；
/// 脱敏命中或需要把 model 换成该家真名时借出处理副本。判定与处理分别在
/// `core::desensitize` / `core::providers::catalog`，本函数只负责「在正确的
/// 时机问一次」—— 时机是「这一家即将发送之前」，所以同一家内部换账号重试
/// 不会重复处理、重复统计。
///
/// **顺带采集上游模型名**：发给该家的名字在这里定稿（无论是否改写），
/// 立即记入 telemetry（覆盖式，最后一次为准 —— 与 provider 字段同一口径，
/// 429 换家后留下的是实际承载那一次的名字）。请求日志的「上游模型」列
/// 因此不再需要猜测。
pub(super) fn send_body<'a>(
    ctx: &'a ProviderContext<'_>,
    provider_id: &str,
    account: Option<&Value>,
) -> Cow<'a, Value> {
    let mut body = match crate::server::core::desensitize::global().process_body_for_provider(
        provider_id,
        ctx.desensitize_scope,
        ctx.body,
    ) {
        Some(processed) => Cow::Owned(processed),
        None => Cow::Borrowed(ctx.body),
    };
    let requested = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if !requested.is_empty() {
        let wire = crate::server::core::providers::catalog::wire_model_for_provider(
            &requested,
            provider_id,
            account,
        );
        ctx.telemetry.note_upstream_model(&wire);
        rewrite_model(&mut body, &requested, &wire, provider_id);
    }
    body
}

/// 把发送体里的 model 字段换成该 provider 认识的真名（仅当需要换时才复制）。
///
/// `requested` 是请求名、`wire` 是 `catalog::wire_model_for_provider` 已经算好
/// 的该家真名（调用方算一次，这里不再查目录）；只有两者不同时才把 `Cow`
/// 升级成 Owned —— 常规路径（本名该家认识）保持借用零拷贝。
fn rewrite_model(body: &mut Cow<'_, Value>, requested: &str, wire: &str, provider_id: &str) {
    if wire.eq_ignore_ascii_case(requested) {
        return;
    }
    logging::verbose(
        "[Upstream]",
        &format!("provider={provider_id} 按该家目录改写模型名 {requested} → {wire}（备援名）"),
    );
    let object = body.to_mut().as_object_mut();
    if let Some(object) = object {
        object.insert("model".to_string(), Value::String(wire.to_string()));
    }
}
