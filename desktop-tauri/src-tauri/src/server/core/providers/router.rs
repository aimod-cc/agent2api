//! 模型路由：把「模型名」解析成一条 **provider 候选集合**。
//!
//! ── 语义（消费方必须按这个理解用）───────────────────────────
//! `route_for_model(model)` 返回**注册表顺序**的 provider 列表：
//!   - 列表里的每一家都「清单里有这个模型名」（能力判定，不查账号状态）；
//!   - **空列表 = 未知模型**（聚合目录里没有任何一家提供这个名字）。
//!
//! 它回答的是「哪些家**能**提供这个模型」，**不是**「先试哪一家」——
//! 先试谁由账号优先级决定：转发层把这些家的账号放进同一条队列，按全局优先级
//! 逐个尝试（`upstream::provider_loop`）。曾经存在的 provider 路由优先级
//! （config.json 的 `providerRoute`）已随全局队列下线。
//!
//! 消费方：`chat_completions`（脱敏范围判定：候选集合与脱敏作用集合有交集即脱敏）
//! 与 `upstream::provider_loop`（候选账号的过滤集合）。两处必须用同一份集合，
//! 否则会出现「转发到了需脱敏的上游却没脱敏」。

use crate::server::core::providers::catalog::providers_for_model;
use crate::server::core::providers::{kind_from_id, ProviderKind, DEFAULT_PROVIDER_ID};

/// 模型名 → 能提供它的 provider 集合（注册表顺序；**空 = 未知模型**）。
pub fn route_for_model(model: &str) -> Vec<ProviderKind> {
    providers_for_model(model)
}

/// 转发用的候选链：`route_for_model` **加上「未知模型回落默认 provider」**。
///
/// ── 为什么需要这个包装（与 `route_for_model` 的区别）───────────
/// `route_for_model` 的空返回值是「未知模型」这个**路由语义**的事实
/// （`/v1/models` 与模型校验都依赖它）。但转发层不能用空链拒绝请求：
/// 改造前 Node 版对「客户端没给 model 且目录为空」的情形是**照发上游**、
/// 让上游自己报错（架构文档 §8 要求既有客户端不退化），
/// 而且校验与转发用的是**同一份候选链**才不会有「校验说没有、转发却发了」
/// 的自相矛盾。因此转发专用的入口把空链补成「注册表默认 provider」。
///
/// 默认 provider 取自 `DEFAULT_PROVIDER_ID`（不是写死的 workbuddy）：
/// 换默认 provider 只改注册表。注册表里查不到那个 id 时返回空链，
/// 由调用点按「没有可用的提供商」报错。
///
/// **纯函数**（不打日志）：每个请求会调它两次（chat 的脱敏判定 +
/// 转发的候选链），日志留在真正发起转发的 `provider_loop` 里，避免重复。
///
pub fn route_for_forward(model: &str) -> Vec<ProviderKind> {
    let candidates = route_for_model(model);
    if !candidates.is_empty() {
        return candidates;
    }
    kind_from_id(DEFAULT_PROVIDER_ID).map(|kind| vec![kind]).unwrap_or_default()
}
