//! 一次转发的只读输入与**发送体选择**（从 `provider_loop.rs` 拆出，单文件行数约定）。
//!
//! ── 发送体为什么按 provider 逐家决定 ──────────────────────────
//! 请求体从 `api::chat` **原样**进来（去重键也取自原始请求体）。内容处理只在
//! **某一家 provider 的凭证已就绪、即将发送之前**发生，并按 `desensitize.json`
//! 的 `providers` 作用范围逐家判定（范围快照见 [`ProviderContext::desensitize_scope`]）：
//!   - 未勾选的家 → 客户端**原始**请求体（不处理、不计数）；
//!   - 勾选的家   → 用 core 的现有处理在**副本**上处理一次（命中日志由 core 打）。
//! 于是首选与故障转移到的未勾选家拿到的都是未修改的 body，绝不会复用上一家
//! 处理过的值；同一 provider 内换账号重试复用同一份（不重复处理、不重复统计）；
//! 「这一家没有可用凭证」时根本走不到处理点，不产生一次已转发的处理。
//!
//! 处理算法本身不在这里：判定与改写都在 `core::desensitize`（本模块只决定
//! 「在什么时机、对哪一家、用哪一份 body」）。

use std::borrow::Cow;
use std::sync::Arc;

use axum::http::HeaderMap;
use serde_json::Value;

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
/// 返回 `Cow`：未勾选的家零拷贝借出客户端原始 body，勾选的家借出处理副本。
/// 判定与处理都在 `core::desensitize`（`process_body_for_provider`），
/// 本函数只负责「在正确的时机问一次」—— 时机是「这一家即将发送之前」，
/// 所以同一家内部换账号重试不会重复处理、重复统计。
pub(super) fn send_body<'a>(ctx: &'a ProviderContext<'_>, provider_id: &str) -> Cow<'a, Value> {
    match crate::server::core::desensitize::global()
        .process_body_for_provider(provider_id, ctx.desensitize_scope, ctx.body)
    {
        Some(processed) => Cow::Owned(processed),
        None => Cow::Borrowed(ctx.body),
    }
}
