//! 聚合模型目录（Agent2API 改造 W2a-T2，架构文档 §4.4）：
//! 把各 provider 的模型清单合并成 `/v1/models` 的单一视图，并回答
//! 「某个模型名由哪些 provider 提供」。
//!
//! ── 为什么要有聚合层 ────────────────────────────────────────
//! 改造前只有 workbuddy，模型清单就是 `core::models::ModelCatalog` 的全部。
//! 多提供商之后每家都有自己的清单来源（workbuddy 是内置清单 + `/v3/config`
//! 远程刷新；raccoon 是小浣熊自己的 `/model_catalog`），而网关对外只暴露
//! **一个** OpenAI 风格的 `/v1/models`，路由也只有在「同一模型名多家可提供」
//! 时才需要知道都有谁 —— 这个合并与查询就是本模块的职责。
//!
//! ── 清单来源的接入点（W3-T4 已接上）───────────────────────────
//! 清单来源**统一走适配器注册表**：`manifest_for(kind)` →
//! `adapter_for(kind).list_models()`。各家自己决定清单从哪来：
//!   - `WorkBuddy` → `core::models` 的进程级句柄（既有逻辑原样不动）；
//!   - `Raccoon`   → `providers::raccoon::models`（静态兜底 5 个模型 +
//!     `/model_catalog` 远程刷新）；
//!   - `CatPaw`    → `providers::catpaw::adapter`（W5-T-d4 接线）：静态 `MODELS`
//!     表（`kimi-k3` / `glm-5.3-flash`），无远程目录；
//!   - `AutoClaw`  → `providers::autoclaw::adapter`（W4b-T-c2 接线）：静态路由表
//!     （`autoclaw-models` 的映射规则 + `zai_auto` 回退），无远程目录。
//! 本模块其余代码（可用性过滤、去重、排序、响应拼装）不认识任何一家的清单实现。
//!
//! ── 两种「有」的区分（重要）────────────────────────────────
//!   - **有能力**（`providers_for_model` / 路由候选链）：只看清单里有没有这个名字。
//!     候选链是「如果这家可用，就该轮到它」，由转发层逐家尝试时自然跳过
//!     无可用账号的家（架构文档 §4.3）。
//!   - **当前可用**（`models_response`）：还要这家此刻有可用登录态，否则
//!     `/v1/models` 会广告一堆客户端根本用不了的模型。
//!
//! ── 「有可用账号」的判定 ────────────────────────────────────
//! 账号文件里该 provider 存在启用且有凭证的账号（`AccountStore::accounts_for_provider`
//! 的口径）。唯一例外是**环境变量旁路登录态**：workbuddy 的 `WORKBUDDY_TOKEN`
//! 与小浣熊的 `RACCOON_TOKEN`、CatPaw 的 `CATPAW_COOKIE`、AutoClaw 的
//! `AUTOCLAW_TOKEN`（四者都由各自的适配器 `allows_anonymous_default_session()`
//! 声称为真）—— 脚本 / CI 用户的常规用法，此时账号文件可能是空的，只认账号列表
//! 会让他们的 `/v1/models` 变成空数组，而改造前清单与登录态无关，那属于功能退化。
//! 故一并算作可用。
//!
//! 「清单为空的家不会出现在 `/v1/models`」这条过滤仍然成立（`active_manifests`
//! 要求清单非空）：它现在是各家适配器**自行决定清单内容**之后的兜底 —— 例如
//! 某家此刻拉不到远程目录又没有静态兜底时，宁可不出现在广告里，也不给客户端
//! 一个路由不到的名字。
//!
//! ── 同名模型去重：保留路由优先级小的那家 ──────────────────────
//! 按路由优先级升序逐家合并，已出现过的模型 id 直接跳过 —— 「先到的赢」
//! 自然实现了「保留优先级更小者」，不需要额外的比较与替换逻辑。
//! 条目里的 `owned_by` 记为**实际承载该模型的那家** provider id。
//!
//! ── 与既有行为的一致性 ──────────────────────────────────────
//! 对「只有 workbuddy 一家、且有可用账号」的既有用户，本模块输出与改造前
//! `ModelCatalog::list_response()` **逐字段相同**：条目构造与响应信封直接复用
//! `core::models` 的 `list_item` / `list_response_from`，`meta.source` 也沿用
//! 旧的 `remote` / `builtin`（多家合并时才出现新值 `aggregate`）。
//!
//! ── 两条出口，同一份清单 ────────────────────────────────────
//! 对外的 `/v1/models`（`models_response`）与桌面端首屏状态里的
//! `GET /api/session` → `models`（`session_models`）取的是**同一个合并步骤**
//! 的结果：前者要 OpenAI 信封、后者要数组且每条多带 provider 归属。共用一步是
//! 「界面上看到的模型 = 客户端实际拿到的模型」这条一致性的实现方式 ——
//! 两处各跑一遍合并，规则漂移时不会报错，只会让两边对不上。

use serde_json::{json, Map, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::core::model_rules;
use crate::server::core::models::{list_item, list_response_from, model_id, suggest_from, ModelCatalog};
use crate::server::core::providers::adapter::adapter_for;
use crate::server::core::providers::{kind_from_id, kind_id, meta, ProviderKind, PROVIDERS};

/// 某一 provider 当前的模型清单（克隆；调用方看不到后续刷新）。
///
/// **来源 = 适配器注册表**（Agent2API W2b-T3 接线）：每家清单由它自己的
/// `ProviderAdapter::list_models` 给出，本模块不认识任何一家的清单实现。
/// 未实现适配器的 provider（将来新增的那些）返回空清单 —— 不猜模型名，
/// 猜出来的名字会把请求路由到不存在的上游。
fn manifest_for(kind: ProviderKind) -> Vec<Value> {
    adapter_for(kind).list_models()
}
/// workbuddy 的目录句柄（`core::models` 的进程级实例）。
///
/// 只用于**刷新元信息**（`remote_refreshed` / `last_refreshed_at` →
/// `/v1/models` 的 `meta.source`）：那是 workbuddy 单家路径的既有输出契约
/// （单家时 `remote`/`builtin` 必须逐字不变），ProviderAdapter 契约里没有
/// 对应的方法（§4.2 只有 list_models / refresh_models），因此这里保留直读。
/// 清单本身已改走适配器（见 `manifest_for`）。
fn workbuddy_catalog() -> ModelCatalog {
    crate::server::core::models::global_catalog()
}

/// 某一家的刷新元信息 `(是否远程刷新过, 最后刷新时间)` —— 用于 `models_response`
/// 的 `meta.source` / `lastRefreshedAt`。
///
/// workbuddy 走它自己的目录句柄（既有契约），小浣熊走
/// `providers::raccoon::models` 的进程级句柄；CatPaw 与占位的 AutoClaw 给
/// `(false, 0)` —— 两家都是**内置清单**语义（CatPaw 的模型表是静态的，
/// 上游没有目录接口；AutoClaw 的占位实现清单恒为空，根本进不了
/// `active_manifests`），这个分支只为让 match 穷举。
fn refresh_meta(kind: ProviderKind) -> (bool, i64) {
    match kind {
        ProviderKind::WorkBuddy => (
            workbuddy_catalog().remote_refreshed(),
            workbuddy_catalog().last_refreshed_at(),
        ),
        ProviderKind::Raccoon => (
            crate::server::core::providers::raccoon::models::remote_refreshed(),
            crate::server::core::providers::raccoon::models::last_refreshed_at(),
        ),
        ProviderKind::CatPaw | ProviderKind::AutoClaw => (false, 0),
    }
}

/// 注册表顺序（稳定）下的全部 provider，已映射成枚举。
///
/// **按 kind 去重**：`kind_from_id` 现在是「未知 id → None」，且注册表里
/// 四项都有对应分支（W4a 起不再是「除 raccoon 之外一律 workbuddy」的兜底）。
/// 「注册表里有、`kind_from_id` 里没有」这种漂移**没法在编译期发现**
/// （`&str` 的 match 必须有兜底分支），所以那个函数用 `debug_assert!` 在开发期
/// 喊出来、release 返回 None，本函数的去重则是**第二道防线**：
/// 万一将来又出现「多个 id 映到同一 kind」的写法，重复 kind 的影响被压到
/// 「少一家」而不是「同一家合并两遍 + 候选链里出现两个同样的 kind」。
///
/// 对占位的 AutoClaw 的效果：它**会被列进来**（身份合法、注册表里有），
/// 但 `manifest_for` 拿到空清单 → 目录输出与候选链里都自然不出现。
/// （CatPaw 在 W5-T-d4 接上真身后有静态清单，会正常参与这两条路径。）
fn all_kinds() -> Vec<ProviderKind> {
    let mut kinds: Vec<ProviderKind> = Vec::new();
    for meta in PROVIDERS {
        let Some(kind) = kind_from_id(meta.id) else {
            continue;
        };
        if !kinds.contains(&kind) {
            kinds.push(kind);
        }
    }
    kinds
}

/// 目录合并时的 provider 顺序 = **注册表顺序**（`all_kinds` 已按注册表给出，
/// 这里原样返回）。provider 路由优先级已随「全局账号队列」下线：同名模型多家
/// 可提供时先试哪家由账号优先级决定，目录合并只需要一个稳定的去重顺序。
fn sorted_by_route(kinds: Vec<ProviderKind>) -> Vec<ProviderKind> {
    kinds
}

/// 该 provider 现在是否有可用登录态（`models_response` 的过滤条件）。
///
/// 判据一：账号文件里存在启用且带凭证的账号（`accounts_for_provider` 已按
/// 「enabled 且 has_token」过滤）。判据二：该 provider 的**环境变量旁路凭证**
/// （workbuddy 的 `WORKBUDDY_TOKEN` / 小浣熊的 `RACCOON_TOKEN`）此刻存在。
///
/// 判据二走适配器的 `env_credentials_present()` 而不是在本文件里硬编码变量名：
/// 变量名与「怎么算有凭证」都是各家的知识（workbuddy 只认一个 token 变量，
/// 小浣熊还要去 `Bearer ` 前缀），放在适配器里才能让本模块加第三家时零改动。
pub fn provider_available(store: &AccountStore, kind: ProviderKind) -> bool {
    if !store.accounts_for_provider(kind_id(kind)).is_empty() {
        return true;
    }
    adapter_for(kind).env_credentials_present()
}

/// 参与 `/v1/models` 聚合的 `(provider, 清单)`：清单非空 **且** 当前有可用登录态。
pub fn active_manifests(store: &AccountStore) -> Vec<(ProviderKind, Vec<Value>)> {
    sorted_by_route(all_kinds())
        .into_iter()
        .filter(|kind| provider_available(store, *kind))
        .filter_map(|kind| {
            let manifest = manifest_for(kind);
            if manifest.is_empty() {
                None
            } else {
                Some((kind, manifest))
            }
        })
        .collect()
}

/// 能提供该模型名的 provider，**按路由优先级升序**。
///
/// 语义是「能力」而不是「当前可用」：只看各家的清单里有没有这个名字，
/// 不查账号 —— 候选链给出后，转发层逐家尝试时会自然跳过无可用账号的家。
/// 返回空数组 = 目录里没有这个模型名（未知模型）。
///
/// ── 匹配口径：先比 id、再比 name（逐字对齐既有 `ModelCatalog::get`）──
/// 改造前的校验是 `ModelCatalog::has(model)`，而 `has` 走 `get`：
/// **先按 id（忽略大小写）找，找不到再按 `name` 找**。客户端传展示名
/// （例如 `Auto`、`Deepseek-V4-Pro`）在改造前是**被接受并原样转发**的，
/// 所以这里必须保持同一口径 —— 只比 id 会让那类客户端突然收到 400
/// （架构文档 §8：既有客户端行为不得退化）。
///
/// 两轮「先 id 后 name」的次序也照抄 `get`：只有当**没有任何一家的 id 命中**
/// 时才启用 name 匹配（避免「某家的 name 恰好等于另一家的 id」时选错家）。
/// 上游收到的 model 始终是客户端原值 —— 这里只做「认不认识它」的判定，
/// 不做任何映射/改写（§2）。
///
/// 消费链：`providers::router::{route_for_model, route_for_forward}` →
/// `upstream::provider_loop` 的候选链 + `api::chat` 的两处判定
/// （模型校验 / 脱敏范围）。
pub fn providers_for_model(model: &str) -> Vec<ProviderKind> {
    let target = model.trim().to_lowercase();
    if target.is_empty() {
        return Vec::new();
    }
    let by_id: Vec<ProviderKind> = all_kinds()
        .into_iter()
        .filter(|kind| {
            manifest_for(*kind)
                .iter()
                .any(|entry| model_id(entry).to_lowercase() == target)
        })
        .collect();
    if !by_id.is_empty() {
        return sorted_by_route(by_id);
    }
    // id 全不命中 → 按 name 再找一轮（`ModelCatalog::get` 的第二段）
    let by_name: Vec<ProviderKind> = all_kinds()
        .into_iter()
        .filter(|kind| {
            manifest_for(*kind).iter().any(|entry| {
                entry
                    .get("name")
                    .map(crate::server::core::models::shape_value_text)
                    .unwrap_or_default()
                    .to_lowercase()
                    == target
            })
        })
        .collect();
    sorted_by_route(by_name)
}

/// 默认模型目录：**认「默认模型」概念**的那些 provider 的清单（按路由优先级升序，
/// 同名去重，**剔除已禁用 / 隐藏的模型**）。
///
/// 架构文档 §4.4 末句的落地：客户端**未指定 `model`** 时，网关的回落顺序是
/// 「config 的 defaultModel（若可用）→ 目录里 isDefault 的模型 → 目录首项」。
/// 这条链在改造前只服务 workbuddy；多提供商之后必须按**能力**收窄 ——
/// isDefault / 首项这类语义只有 `supports_default_model` 的家才认
/// （它们的清单里才有 `isDefault` 字段），拿别家的清单去挑默认模型只会挑出
/// 一个该家不认的名字。所以：
///   - 有认这个概念的家（本期只有 workbuddy）→ 返回它们的清单，回落顺序照旧，
///     既有用户拿到的默认模型与改造前**逐字相同**；
///   - 一家都不认 → 返回空，调用点不注入 model（按「未指定」处理，
///     让上游用它自己的默认模型，符合 §4.4「仅当命中的 provider 无默认概念时
///     不注入」）。
///
/// ── 为什么剔除被禁用 / 隐藏的模型（modelRules 的 blocked）─────────
/// 回落链选出的名字随后要过 chat 的 blocked 校验（被禁用的模型请求返回 404
/// model_not_found）：不过滤的话，WorkBuddy 种子把 `auto` 默认禁用后
/// （见 `model_rules::seed_workbuddy_defaults`），未指定 model 的请求会先被
/// 注入 `auto`、再被自己的校验拒掉 —— 回落链必须在**挑选时**就跳过这些名字，
/// 顺延到下一个候选（isDefault 的其他模型 → 首项）。
///
/// 消费方：`api::chat` 的默认模型回落分支与 `default_model_usable`。
pub fn default_model_catalog() -> Vec<Value> {
    let mut merged: Vec<Value> = Vec::new();
    let mut claimed: Vec<String> = Vec::new();
    let rules = model_rules::current();
    for kind in sorted_by_route(all_kinds()) {
        if !adapter_for(kind).supports_default_model() {
            continue;
        }
        for entry in manifest_for(kind) {
            let id = model_id(&entry);
            // 被禁用 / 隐藏的名字不进回落候选（理由见函数头说明）
            if !id.is_empty() && rules.is_blocked(&id) {
                continue;
            }
            if !id.is_empty() {
                let key = id.to_lowercase();
                if claimed.iter().any(|known| known == &key) {
                    continue;
                }
                claimed.push(key);
            }
            merged.push(entry);
        }
    }
    merged
}

/// config 的 `defaultModel` 是否可用（回落链的第一级判定）。
///
/// 「可用」= 认「默认模型」概念的 provider 清单里存在这个名字**且未被禁用 /
/// 隐藏**（`default_model_catalog` 已剔除 blocked，这里自然继承该口径），
/// 匹配口径与 `providers_for_model` 一致（**先 id 后 name、忽略大小写**）。
/// 与 `providers_for_model` 的区别只在候选集合：这里只看
/// `supports_default_model` 的家（理由见 `default_model_catalog`）。
pub fn default_model_usable(model: &str) -> bool {
    let target = model.trim().to_lowercase();
    if target.is_empty() {
        return false;
    }
    let models = default_model_catalog();
    let by_id = models
        .iter()
        .any(|entry| model_id(entry).to_lowercase() == target);
    if by_id {
        return true;
    }
    models.iter().any(|entry| {
        entry
            .get("name")
            .map(crate::server::core::models::shape_value_text)
            .unwrap_or_default()
            .to_lowercase()
            == target
    })
}

/// 聚合后的模型名列表（去重，顺序 = `models_response` 的 data 顺序，**不查可用性**）。///
/// 与 `providers_for_model` 同一份清单视角（能力而非可用性）：提示里出现的名字
/// 一定要能被 `/v1/models` 列出（有账号的那几家），否则提示本身就是误导；
/// 但在「这台机器恰好没账号」时仍给出提示，比给一片空白更有用。
fn aggregated_model_ids() -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for kind in sorted_by_route(all_kinds()) {
        for entry in manifest_for(kind) {
            let id = model_id(&entry);
            if id.is_empty() {
                continue;
            }
            if !ids.iter().any(|known| known.eq_ignore_ascii_case(&id)) {
                ids.push(id);
            }
        }
    }
    ids
}

/// 合并后的清单条目 `(provider id, /v1/models 条目)` —— **唯一一处合并逻辑**。
///
/// 入参是 `active_manifests` 的结果（调用方各取一次，本函数不自己再查一遍：
/// 查一次就要克隆一遍各家的清单，两条出口各调一次会白干一倍的活）。
/// 同名模型去重（先合并者胜 = 路由优先级小的那家），条目构造走
/// `models::list_item`（`owned_by` 记为实际承载该模型的那家 provider id）。
/// 两条出口都从这里取数据：`models_response` 拼信封，`session_models` 补
/// provider 归属后交前端 —— 去重口径、条目字段、顺序三者只有这一份实现。
///
/// 条目里没有 provider id 的独立字段（`owned_by` 就是它）：多一个同义字段
/// 就多一处可能与 `owned_by` 不一致的写法，而下游要的就是这个 id。
fn merged_items(active: &[(ProviderKind, Vec<Value>)]) -> Vec<(&'static str, Value)> {
    let mut items: Vec<(&'static str, Value)> = Vec::new();
    // 已被**更早（优先级更小）的 provider** 认领的模型名。去重只跨 provider 做：
    // 同一家清单内部若出现重复条目（远程下发的脏数据），原样全部保留 ——
    // 改造前 `ModelCatalog::list_response` 就是逐条映出去，顺手去重会让
    // 「单家用户的输出」与改造前产生差异，那是本任务明令禁止的。
    let mut claimed: Vec<String> = Vec::new();
    for (kind, manifest) in active {
        let provider_id = kind_id(*kind);
        for model in manifest {
            let id = model_id(model);
            // 没有 id 的条目无法参与去重（现实中不存在），原样保留
            if !id.is_empty() {
                let key = id.to_lowercase();
                if claimed.iter().any(|known| known == &key) {
                    continue;
                }
                claimed.push(key);
            }
            items.push((provider_id, list_item(model, provider_id)));
        }
    }
    items
}

/// 聚合结果的来源元信息 `(meta.source, meta.lastRefreshedAt)`。
///
/// 单家时沿用该家的来源值（workbuddy 的 `remote`/`builtin`，保证老客户端的
/// 输出逐字不变；小浣熊同样「远程目录成功过就是 remote」）；多家合并时
/// `aggregate`；一家都没有时 `none`（`data` 为空数组 —— 网关确实无模型可广告）。
/// `lastRefreshedAt` 取**参与聚合的第一家**的刷新时间（多家时 workbuddy 优先，
/// 因为它是既有客户端唯一见过的来源）。
fn aggregate_source(active: &[(ProviderKind, Vec<Value>)]) -> (&'static str, i64) {
    let workbuddy_active = active.iter().any(|(kind, _)| *kind == ProviderKind::WorkBuddy);
    match active {
        // 单家：沿用既有来源值（workbuddy 远程刷新过就是 remote）
        [(kind, _)] => {
            let (remote, refreshed_at) = refresh_meta(*kind);
            (if remote { "remote" } else { "builtin" }, refreshed_at)
        }
        [] => ("none", 0),
        _ => (
            "aggregate",
            if workbuddy_active {
                workbuddy_catalog().last_refreshed_at()
            } else {
                // 没有 workbuddy 参与：用第一家（路由优先级最小）的刷新时间
                active
                    .first()
                    .map(|(kind, _)| refresh_meta(*kind).1)
                    .unwrap_or(0)
            },
        ),
    }
}

/// `/v1/models` 的聚合响应体：`{object:"list", data:[...], meta:{...}}`。
///
/// 规则（架构文档 §4.4）：
///   - 只合并「有可用账号」的 provider 的清单（`active_manifests`）；
///   - 同名模型去重，保留路由优先级小的那家的条目（先合并者胜）；
///   - OpenAI 响应结构不变（`data` 数组，`id` 为上游原始模型名）。
///
/// 合并与来源元信息分别走 `merged_items` / `aggregate_source`（见它们的说明）。
pub fn models_response(store: &AccountStore) -> Value {
    let active = active_manifests(store);
    let (source, last_refreshed_at) = aggregate_source(&active);
    let rules = model_rules::current();
    // 禁用 / 隐藏的模型不广告；映射名作为独立条目追加（复制目标条目、换 id），
    // 让客户端能在 /v1/models 里发现它们
    let mut data: Vec<Value> = Vec::new();
    let mut aliases: Vec<Value> = Vec::new();
    for (_, item) in merged_items(&active) {
        let id = model_id(&item);
        if rules.is_blocked(&id) {
            continue;
        }
        for alias in rules.aliases_of(&id) {
            let mut copy = item.clone();
            if let Some(object) = copy.as_object_mut() {
                object.insert("id".to_string(), Value::String(alias.to_string()));
                object.insert("is_default".to_string(), Value::Bool(false));
            }
            aliases.push(copy);
        }
        data.push(item);
    }
    data.extend(aliases);
    list_response_from(data, source, last_refreshed_at)
}

/// 管理页视图：**每家提供商自己的完整清单**（含禁用 / 隐藏），每条带
/// `enabled` / `hidden` / `aliases`，另附映射表。形状：
/// `{ models: [...], mappings: [{alias, target}] }`
///
/// ── 为什么这里**不做跨提供商去重**（与 `/v1/models` 的关键差别）──
/// 同一个模型名（如 `glm-5.3-flash`）可能同时出现在多家的清单里。
/// 对外的 `/v1/models` 按名字去重（一个名字一个条目，先到的家认领），
/// 但管理页如果也去重，「被别家认领」的那家的真实模型就从列表里消失了 ——
/// 看起来像「这家只有一个模型」，而它明明有两个（CatPaw 的 `glm-5.3-flash`
/// 曾因此被 WorkBuddy 认领而不显示）。管理页的职责是管理**每家上游的真实
/// 清单**，所以这里每家各列各的，同名模型在每家各占一行。
///
/// 禁用 / 删除规则本身仍按**模型 id 全局生效**（modelRules 的键就是 id）：
/// 同名模型在哪家停用，对所有家一起停用 —— 与 /v1/models「一个名字一个
/// 条目」的对外语义一致。
pub fn manage_view(store: &AccountStore) -> Value {
    let rules = model_rules::current();
    let models: Vec<Value> = manage_entries(store, &rules)
        .into_iter()
        .map(|mut entry| {
            let id = model_id(&entry);
            if let Some(object) = entry.as_object_mut() {
                object.insert("enabled".to_string(), Value::Bool(!rules.is_disabled(&id)));
                object.insert("hidden".to_string(), Value::Bool(rules.is_hidden(&id)));
                object.insert(
                    "aliases".to_string(),
                    Value::Array(rules.aliases_of(&id).into_iter().map(|a| Value::String(a.to_string())).collect()),
                );
            }
            entry
        })
        .collect();
    let mappings: Vec<Value> = rules
        .mappings
        .iter()
        .map(|m| json!({ "alias": m.alias, "target": m.target }))
        .collect();
    json!({ "models": models, "mappings": mappings })
}

/// `GET /api/session` 的 `models` 字段：聚合清单的**数组**形态，每条带 provider 归属。
///
/// 与 `/v1/models` 同一份合并结果（`merged_items`），差别只有形状：
///   - 交出去的是数组而不是 OpenAI 信封（`/api/session` 的 `models` 历来是数组，
///     改形状会把前端契约一起改掉）；
///   - 每条多出 `provider`（provider id）与 `providerLabel`（注册表里的展示名，
///     查不到时回退成 id 本身）—— 网关页要按家分组展示，前端不该自己维护一份
///     id → 展示名的映射（那是后端注册表的职责，加一家 provider 时前端零改动）。
///
/// **这两个字段只加在这里**：`/v1/models` 是给 OpenAI 客户端消费的对外契约，
/// 多字段虽无害却会改变字节（既有客户端可能做响应比对/缓存），所以两条出口
/// 共用合并步骤、只有本函数补归属字段。
///
/// 字段名沿用 `/api/session` 的既有契约（`isDefault` 驼峰，不是 `/v1/models` 的
/// `is_default`）：前端的默认模型徽标与账号页下拉都读 `m.isDefault`，改成对外接口
/// 那套命名会让「哪个是默认模型」静默失准（`find(m => m.isDefault)` 恒为 undefined，
/// 只能回落到数组首项 —— 路由优先级一改就指到别家的模型上）。取值则从
/// `/v1/models` 的条目里读，默认/倍率的判定规则与对外接口同源，两处不可能各说各话。
pub fn session_models(store: &AccountStore) -> Vec<Value> {
    // 与 /v1/models 同一过滤：禁用 / 隐藏的模型不出现在界面的「可用模型」里
    let rules = model_rules::current();
    session_entries(store)
        .into_iter()
        .filter(|entry| !rules.is_blocked(&model_id(entry)))
        .collect()
}

/// `session_models` 的未过滤版本（管理页要看到禁用 / 隐藏的条目）
fn session_entries(store: &AccountStore) -> Vec<Value> {
    merged_items(&active_manifests(store))
        .into_iter()
        .map(|(provider_id, item)| manage_entry_json(provider_id, &item))
        .collect()
}

/// 管理页条目：**不去重**的每家清单（`manage_view` 的数据源）。
///
/// 与 `session_entries` 的唯一差别是数据源：那里走 `merged_items`
/// （跨提供商按模型 id 去重，服务「客户端能请求什么」的对外视图），
/// 这里直接遍历 `active_manifests` 的每家原始清单 —— 管理页要看见的是
/// 「每家上游到底有哪些模型」，同名模型（如 WorkBuddy 与 AutoClaw 都有
/// `glm-5.3-flash`）必须每家各显示一行，否则被别家认领的那家会凭空少模型。
/// 条目形状与 `session_entries` 完全一致（`id` / `name` / `isDefault` /
/// `credits` / `provider` / `providerLabel`），前端两份数据共用一套渲染。
///
/// **组内排序**：每家清单内启用的模型排在前、禁用的排在后（`sort_by_key`
/// 稳定排序，两段各自保持清单原顺序）。理由：管理页每组默认只展开前几行
/// （前端的 `GROUP_LIMIT`），排序后默认看到的是启用的模型，禁用的一长串
/// 不会把有用的行挤出首屏。排序与 `manage_view` 的 enabled 标记用**同一份
/// rules 快照**（入参传入），排序和开关状态不会各说各话。
fn manage_entries(store: &AccountStore, rules: &model_rules::ModelRules) -> Vec<Value> {
    let mut entries: Vec<Value> = Vec::new();
    for (kind, manifest) in active_manifests(store) {
        let provider_id = kind_id(kind);
        let mut items = manifest;
        items.sort_by_key(|item| rules.is_disabled(&model_id(item)));
        for item in items {
            entries.push(manage_entry_json(provider_id, &item));
        }
    }
    entries
}

/// 管理页 / 会话条目的公共成形逻辑（从 `list_item` 输出里挑字段）。
fn manage_entry_json(provider_id: &str, item: &Value) -> Value {
    let mut entry = Map::new();
    entry.insert(
        "id".to_string(),
        item.get("id").cloned().unwrap_or(Value::String(String::new())),
    );
    if let Some(name) = item.get("name").filter(|value| !value.is_null()) {
        entry.insert("name".to_string(), name.clone());
    }
    entry.insert(
        "isDefault".to_string(),
        item.get("is_default").cloned().unwrap_or(Value::Bool(false)),
    );
    entry.insert(
        "credits".to_string(),
        item.get("credits")
            .cloned()
            .unwrap_or(Value::String(String::new())),
    );
    entry.insert("provider".to_string(), Value::String(provider_id.to_string()));
    entry.insert(
        "providerLabel".to_string(),
        Value::String(provider_label_of(provider_id).to_string()),
    );
    Value::Object(entry)
}

/// provider id → 注册表里的展示名；未登记的 id 原样回显。
///
/// 回退成 id 而不是「未知」：条目里的 id 来自 `owned_by`，它是实际承载该模型的
/// 那家 —— 真出现未登记的 id，回显原文比一句笼统的「未知」更能定位问题
/// （与 `request_stats::report::provider_label` 的取舍一致）。
/// 前端对「归属缺失」另有兜底组（见 app.js 的 renderModels），两处都不会丢条目。
fn provider_label_of(provider_id: &str) -> &str {
    match kind_from_id(provider_id) {
        Some(kind) => meta(kind).label,
        None => provider_id,
    }
}

/// 未知模型报错用的「相近模型」提示：在**聚合后的模型名集合**里找最相近的。
///
/// 判定规则复用 `models::suggest_from`（纯函数），与改造前 workbuddy 单家的
/// 提示口径完全一致；数据源换成聚合目录（架构文档 §4.4「未知模型校验与相近
/// 提示逻辑保持（聚合后判定）」）。
///
/// 由 chat.rs 调用（已接线）：聚合目录的相近提示 —— 数据源与 `providers_for_model`
/// 同一份（能力判定），所以「提示里出现的名字」一定能被 `/v1/models` 列出。
pub fn suggest_models(model: &str, limit: usize) -> Vec<String> {
    suggest_from(aggregated_model_ids(), model, limit)
}
