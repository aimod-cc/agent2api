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
//! 消费方：`upstream::provider_loop`（转发候选链）。内容脱敏是「每家即将发送
//! 前逐家判定」（`upstream::payload::send_body` → `process_body_for_provider`），
//! 不再依赖本模块的候选集合 —— 那个旧的交集判定已随逐家脱敏下线。
//!
//! ── 映射的候选链展开（modelRules.mappings，照抄 OmniProxy）────
//! `route_for_forward` 在本名承载家之后**追加**各映射条目的提供商（同一家去重）：
//!   - 带 `provider` 的条目 → 直接追加该家（发送时 model 用该条目的 target，
//!     改写见 `catalog::wire_model_for_provider`）；
//!   - 旧版无 `provider` 的条目 → 追加「承载 target 的所有家」（与升级前的
//!     「改写后路由」逐字等价，见 `model_rules` 模块头的兼容说明）。
//! 顺序语义：本名承载家（注册表序）→ 各映射条目按配置顺序。账号全局优先级
//! 仍只在候选集合内部排序，所以本名的承载家天然排在映射家前面 ——
//! 「原生优先、映射兜底」不需要额外的优先级运算。请求名与目录里某上游 id
//! 同名时（同名映射的正用法），该 id 的原生承载家自动是主路。
//!
//! ── 禁用的过滤（`modelRules.disabled` / `hidden`）────────────
//! 候选链在这里就**剔除被禁用的家**，而不是「先放进去、靠转发层跳过」——
//! 转发层（账号选路）根本没有模型级禁用的判据（`routing::account_usability`
//! 只看账号启用状态与该模型的限流冷却），曾经因此出现过「管理页关了某家的
//! 模型，请求仍然落到那家」的漏洞：过滤只作用于广告视图与入口校验，
//! 转发候选链照旧包含它。
//!
//! 过滤是**逐段按各段自己的名字**判的，不是按请求名统一判：
//!   - 本名段（`route_for_model(model)`）→ 按请求名判；
//!   - 映射段 → 带 provider 的条目按该条目的 `target` 判（该家收的是 target，
//!     不是请求名），旧版全局条目按 `target` 的承载家判。
//! 统一按请求名判会漏掉「请求名没被禁、但它映射到的那家的 target 被禁了」
//! 这一种，那正是这次要堵的场景。
//!
//! 被过滤掉的家**不会**让请求回落默认 provider：那条回落只服务「未知模型」
//! （候选链本来为空）。只要这个模型确实有家承载、只是全被禁用，就该让请求
//! 在入口 404（`catalog::model_blocked_everywhere`）或转发层报错，而不是被
//! 悄悄改派到 workbuddy —— 那会把「我明明关掉了」变成一次莫名其妙的成功。

use crate::server::core::model_rules;
use crate::server::core::providers::catalog::{provider_blocks_model, providers_for_model};
use crate::server::core::providers::{kind_from_id, ProviderKind, DEFAULT_PROVIDER_ID};

/// 模型名 → 能提供它的 provider 集合（注册表顺序；**空 = 未知模型**）。
pub fn route_for_model(model: &str) -> Vec<ProviderKind> {
    providers_for_model(model)
}

/// 转发用的候选链：`route_for_model` **加上映射提供商 + 「未知模型回落默认
/// provider」**，并**剔除被禁用 / 隐藏的家**（见模块头）。
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
/// ── 映射的展开（modelRules.mappings）────────────────────────
/// 本名候选为空时**仍然**展开映射：请求名可以是一个纯对外名（目录里没有、
/// 只靠映射存在），这正是映射的基本用法。展开按配置顺序追加、同一家只出现
/// 一次（它已经在本名链上时不再作为映射家重复入选）。
///
/// **不打日志**：日志留在真正发起转发的 `provider_loop` 里（每个请求只打一次，
/// 本函数可能被同一请求多次调用）。
pub fn route_for_forward(model: &str) -> Vec<ProviderKind> {
    let rules = model_rules::current();
    // 本名段：按请求名判禁用
    let mut candidates = keep_enabled(route_for_model(model), model, &rules);
    for mapping in rules.mappings_of(model) {
        let providers: Vec<ProviderKind> = match &mapping.provider {
            Some(provider) => kind_from_id(provider).into_iter().collect(),
            None => route_for_model(&mapping.target),
        };
        // 映射段：按该条目的 target 判（这一家收的是 target 而不是请求名）
        for kind in keep_enabled(providers, &mapping.target, &rules) {
            if !candidates.contains(&kind) {
                candidates.push(kind);
            }
        }
    }
    if !candidates.is_empty() {
        return candidates;
    }
    // 空链有且只有两种可能，必须分开处理：
    //   - 这个名字**没有任何家承载** → 未知模型，回落默认 provider（既有语义，
    //     见函数头说明）；
    //   - 有家承载、但**全被禁用** → 不回落。禁用的家不该被默认家顶替，
    //     否则「我明明关掉了」会变成一次落到 workbuddy 的莫名其妙成功。
    if has_any_carrier(model, &rules) {
        return Vec::new();
    }
    kind_from_id(DEFAULT_PROVIDER_ID).map(|kind| vec![kind]).unwrap_or_default()
}

/// 剔除该段里被禁用 / 隐藏的家（判据见 `catalog::provider_blocks_model`）
fn keep_enabled(
    providers: Vec<ProviderKind>,
    name: &str,
    rules: &model_rules::ModelRules,
) -> Vec<ProviderKind> {
    providers
        .into_iter()
        .filter(|kind| !provider_blocks_model(rules, *kind, name))
        .collect()
}

/// 这个名字是否**有家承载**（不论是否被禁用）—— 「未知模型」与「全被禁用」
/// 的区分判据（见 `route_for_forward` 的说明）。
///
/// 覆盖原生承载家与映射条目的候选家两侧：请求名只以纯对外名存在（原生为空、
/// 全靠映射）时，映射家被禁同样属于「有路但全被禁」，不该回落默认 provider。
fn has_any_carrier(model: &str, rules: &model_rules::ModelRules) -> bool {
    if !providers_for_model(model).is_empty() {
        return true;
    }
    rules.mappings_of(model).iter().any(|mapping| match &mapping.provider {
        Some(provider) => kind_from_id(provider).is_some(),
        None => !providers_for_model(&mapping.target).is_empty(),
    })
}
