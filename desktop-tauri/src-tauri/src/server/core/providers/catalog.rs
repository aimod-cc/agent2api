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
//!   - **有能力**（`providers_for_model`）：只看清单里有没有这个名字，
//!     不查账号、**也不查启用状态**（启停是另一层，见下条）。
//!   - **路由候选链**（`router::route_for_forward`）：在能力之上再剔除
//!     **被禁用 / 隐藏的家**，然后交给转发层逐家尝试。
//!     ⚠️ 「转发层会自己跳过不可用的家」**只对账号维度成立**（无账号 / 已禁用
//!     账号 / 该模型限流中，见 `routing::account_usability`）——它**没有**模型
//!     级启停的判据，所以启停必须在候选链那一层就过滤掉，否则会出现
//!     「管理页关了某家的模型，请求仍然落到那家」。
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
use crate::server::core::key_scope::{self, KeyScope};
use crate::server::core::model_rules;
use crate::server::core::models::{list_item, list_response_from, model_id, suggest_from, ModelCatalog};
use crate::server::core::providers::adapter::adapter_for;
use crate::server::core::providers::{kind_from_id, kind_id, ProviderKind, PROVIDERS};

/// 某一 provider 当前的模型清单（克隆；调用方看不到后续刷新）。
///
/// **来源 = 适配器注册表**（Agent2API W2b-T3 接线）：每家清单由它自己的
/// `ProviderAdapter::list_models` 给出，本模块不认识任何一家的清单实现。
/// 未实现适配器的 provider（将来新增的那些）返回空清单 —— 不猜模型名，
/// 猜出来的名字会把请求路由到不存在的上游。
///
/// **这是「能力」口径的清单**（`providers_for_model` / 路由候选链用）：
/// 客户端点名一个模型时，这里有的才允许转发。**不要**在这里过滤
/// （要按偏好收窄广告用 `advertised_manifest_for`，见 `list_models` 的契约说明）。
///
/// ── 自定义模型的注入点（就在这一处）──────────────────────────
/// 用户手动登记的上游模型（`modelRules.custom`）拼在该家清单之后。选这里而不是
/// 各家自己的 `list()`，有两个硬理由：
///
///   1. **一处覆盖全部出口**：本函数是能力判定、路由候选链、发送名改写、
///      `/v1/models`、`/api/session`、管理页、Key 页候选与入口校验的共同上游，
///      改这一处它们全部认识自定义模型。改各家 `list()` 则是六家六套改法
///      （其中四家是 `const` 静态数组，要改成 `Vec` 拼接）。
///   2. **不会被远程刷新冲掉**：workbuddy 的 `apply_remote` 会**整体替换**
///      自己的 `CatalogState.models`，注进各家的清单里会被下一次刷新抹掉，
///      还会被 `seed_default_enabled` 判成「不在默认白名单 → 默认禁用」。
///      本函数是每次调用现算的，刷新与种子都碰不到它。
///
/// **同名不追加**：该家清单里已有同名条目时跳过 —— 否则管理页会出现两行同
/// `(provider, id)`（`manage_entries` 不去重），而两行的启停开关会互相打架。
/// 上游目录后来真的广告了这个模型时，用户登记的那条自动让位给目录那份
/// （目录条目带完整能力位，比手动条目信息更全），这是想要的方向。
fn manifest_for(kind: ProviderKind) -> Vec<Value> {
    let mut models = adapter_for(kind).list_models();
    let custom = model_rules::custom_models_for(kind_id(kind));
    if !custom.is_empty() {
        // 先滤成一个独立列表再拼接：`models.extend(iter)` 里 iter 若还借着
        // `models` 就是同时可变借用 + 不可变借用，编译不过
        let additions: Vec<Value> = custom
            .into_iter()
            .filter(|item| {
                let id = model_id(item);
                !id.is_empty()
                    && !models
                        .iter()
                        .any(|existing| model_id(existing).eq_ignore_ascii_case(&id))
            })
            .collect();
        models.extend(additions);
    }
    models
}

/// 某一 provider 的**广告用**清单：`manifest_for` 再经适配器的
/// `advertise_models` 过滤。
///
/// 与 `manifest_for` 分开的理由见 `ProviderAdapter::advertise_models` 的文档：
/// 被过滤掉的模型**仍然可以点名调用**，只是不出现在 `/v1/models` 与管理页里。
///
/// ── 目前没有一家覆盖这个方法（这是对的状态）──────────────────
/// 唯一的实现者曾是 Cline：那时两个额度池共用一家 provider，池是**账号的属性**，
/// 所以要在广告时按「账号库里有哪个池」现算一遍。池拆成两家之后，过滤是身份的
/// 固有属性（`cline::models::list(pool)` 只列本池），已经体现在 `manifest_for`
/// 里 —— 于是这里走 trait 的默认实现（原样透传）就是正确答案。
///
/// 保留这个中间层与 `advertise_models` 这个钩子：它是「清单（能路由什么）」
/// 与「广告（对客户端露出什么）」这两件事的分离点，将来若有家需要按运行期
/// 状态收窄（额度、地区、实验开关），该家在这里给出实现即可，不必再动
/// `manifest_for` 的调用点。
///
/// `store` 原样透传给适配器（它需要读账号状态才能决定收窄口径，且必须读
/// **调用方手上那个句柄**，理由见那个方法的文档）。
fn advertised_manifest_for(store: &AccountStore, kind: ProviderKind) -> Vec<Value> {
    adapter_for(kind).advertise_models(store, manifest_for(kind))
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
/// workbuddy 走它自己的目录句柄（既有契约），其余各家走各自的进程级句柄。
///
/// **各家都有远程目录**（CatPaw 与 AutoClaw 原先被当成「内置清单」语义说
/// 「上游没有目录接口」，那两个前提后来都被证伪 —— 接口一直都在，
/// 分别是 `POST /api/agent/maas/model-types` 与
/// `GET .../proxy/autoclaw-model-config`；Cline 与 Qoder 接入时就带着各自的
/// 目录接口）。
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
        // Qoder 也有远程目录（`algo/api/v2/model/list`，按地区各缓存一份）：
        // 两个地区只要有一边成功拉到过，就算「远程」。
        ProviderKind::Qoder => (
            crate::server::core::providers::qoder::models::remote_refreshed(
                crate::server::core::providers::qoder::endpoints::Region::Global,
            ) || crate::server::core::providers::qoder::models::remote_refreshed(
                crate::server::core::providers::qoder::endpoints::Region::Cn,
            ),
            crate::server::core::providers::qoder::models::last_refreshed_at(),
        ),
        // 这两家的远程清单一拿到就非空（`catalog::refresh` 对空清单报失败、
        // 不动缓存），所以「清单非空」等价于「远程刷新成功过」。
        ProviderKind::CatPaw => (
            !crate::server::core::providers::catpaw::catalog::remote_models().is_empty(),
            crate::server::core::providers::catpaw::catalog::last_refreshed_at(),
        ),
        ProviderKind::AutoClaw => (
            !crate::server::core::providers::autoclaw::catalog::remote_models().is_empty(),
            crate::server::core::providers::autoclaw::catalog::last_refreshed_at(),
        ),
        // Cline 也有远程目录（`GET /api/v1/ai/cline/recommended-models`，实测
        // **无需鉴权**即可拉），刷新到非空才算「远程」。两个池共用同一份缓存
        // （一次请求就拿到两池），所以这两个 provider 的元信息是同一个值。
        ProviderKind::ClineFree | ProviderKind::ClinePass => (
            crate::server::core::providers::cline::models::remote_refreshed(),
            crate::server::core::providers::cline::models::last_refreshed_at(),
        ),
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
///
/// 用的是**广告用**清单（`advertised_manifest_for`，经适配器的
/// `advertise_models` 收窄）—— 能力判定仍走未收窄的 `manifest_for`。
pub fn active_manifests(store: &AccountStore) -> Vec<(ProviderKind, Vec<Value>)> {
    sorted_by_route(all_kinds())
        .into_iter()
        .filter(|kind| provider_available(store, *kind))
        .filter_map(|kind| {
            let manifest = advertised_manifest_for(store, kind);
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
/// 不查账号、**也不查禁用 / 隐藏**（那是 `route_for_forward` 在候选链上做的
/// 过滤，以及 `advertised_manifest_contains` 在广告视图上做的收窄）。
/// 返回空数组 = 目录里没有这个模型名（未知模型）。
///
/// ⚠️ 消费方注意：本函数的结果**不能直接当转发候选链用**（会漏掉启停过滤），
/// 转发一律走 `router::route_for_forward`；这里保留「不过滤」是为了让
/// 「入口全禁判定」（`model_blocked_everywhere`）能问出「有没有家承载」。
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
            // 被禁用 / 隐藏的名字不进回落候选（按提供商判定，理由见函数头说明）
            if !id.is_empty() && rules.is_blocked(kind_id(kind), &id) {
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

/// 合并各家的清单：**先剔除该家自己禁用 / 隐藏的条目，再做跨提供商去重**。
///
/// 与 `merged_items` 的差别就在先后顺序：按提供商区分启停后，同一模型 id
/// 可能 A 家被禁、B 家可用 —— 若沿用「先认领再过滤」，A 家会先把名字认领掉
/// 然后被过滤，B 家的可用模型就从对外视图里凭空消失。这里改成「可用的先认领」：
/// 只要还有一家提供且未禁用，名字就对外存在（由那家承载）。
///
/// 去重只跨 provider 做（同一家内部的重复条目原样保留），与 `merged_items`
/// 同一取向。
fn merged_items_available(
    active: &[(ProviderKind, Vec<Value>)],
    rules: &model_rules::ModelRules,
) -> Vec<(&'static str, Value)> {
    let mut items: Vec<(&'static str, Value)> = Vec::new();
    let mut claimed: Vec<String> = Vec::new();
    for (kind, manifest) in active {
        let provider_id = kind_id(*kind);
        for model in manifest {
            let id = model_id(model);
            // 该家自己禁用 / 隐藏 → 这一家不参与（名字可能由别家继续承载）
            if !id.is_empty() && rules.is_blocked(provider_id, &id) {
                continue;
            }
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

/// 请求名是否在**广告视图**里（`/v1/models` 与管理页实际列出的那些名字）。
///
/// ── 为什么校验要用广告视图（而不是能力视图）─────────────────────
/// 「列表里能看到什么，就只允许点什么」：客户端手里的模型清单来自
/// `/v1/models`，点一个列表里没有的名字应当立刻 400，而不是转给上游换回一个
/// 与真实原因无关的上游报错（Cline 实测回 `invalid model format`，
/// 而真正的原因是那个模型属于这个账号用不了的额度池）。
///
/// 广告视图与能力视图的差别只在**收窄**：能力视图是「这家有没有这个名字」，
/// 广告视图还叠加了「按账号收窄（Cline 按额度池）+ 有可用登录态 + 未被禁用」
/// 三层过滤，并追加映射别名。校验用后者，于是「广告里没有」与「请求被拒」
/// 是同一件事，不存在「列表里看不到却能调通」的中间态。
///
/// ── 匹配口径 ────────────────────────────────────────────────
/// 与能力判定一致：**先 id 后 name、忽略大小写**，映射别名以 id 形态参与
/// （`models_response` 把每条别名复制成一个独立条目，见那里）。保留 name 轮
/// 是为了既有客户端：传展示名（`Auto` / `Deepseek-V4-Pro`）在它**确实被广告**
/// 时依然可用，只是不再能绕过广告视图。
///
/// ── 调用方为什么要先判 `active` 非空 ──────────────────────────
/// 一家可用提供商都没有（没加账号）时广告视图必然为空，此时一律 400「模型
/// 不存在」会盖掉转发层更准确的「没有可用账号，无账号可转发」。所以那种情形
/// 由调用方跳过本判定，让请求走到转发层拿那条可操作的错误。
pub fn advertised_manifest_contains(
    active: &[(ProviderKind, Vec<Value>)],
    model: &str,
) -> bool {
    let target = model.trim().to_lowercase();
    if target.is_empty() {
        return false;
    }
    let rules = model_rules::current();
    let items = merged_items_available(active, &rules);
    if items
        .iter()
        .any(|(_, item)| model_id(item).to_lowercase() == target)
    {
        return true;
    }
    // 映射别名：按 `models_response` 同一份 `aliases_of_any` 判定 ——
    // 「广告里有」与「这里放行」必须是同一个集合，否则又会出现能看不能点
    if items.iter().any(|(_, item)| {
        rules
            .aliases_of_any(&model_id(item))
            .iter()
            .any(|alias| alias.eq_ignore_ascii_case(&target))
    }) {
        return true;
    }
    items.iter().any(|(_, item)| {
        item.get("name")
            .map(crate::server::core::models::shape_value_text)
            .unwrap_or_default()
            .to_lowercase()
            == target
    })
}

/// 请求名是否在**路由候选全链**上都被禁用 / 隐藏。
///
/// 按提供商区分启停后，chat 入口的 404 校验不能再用「名字在 blocked 列表里」
/// 判定：只要还有一家提供且可用就应放行（转发候选链自己会跳过被禁的家），
/// 只有全部承载家都被禁用时才返回 model_not_found。判定需要每家的清单来把
/// 请求名解析成条目 id（**先 id 后 name、忽略大小写**，与 `providers_for_model`
/// 同口径），所以放在目录模块而不是 model_rules。
///
/// ── 候选全链口径（原生 + 映射，照抄 OmniProxy 的候选语义）──────
/// 判定范围 = 请求名的原生承载家 + 各映射条目的候选家：请求名被映射后，
/// 「原生家全禁」正是映射该接手的时刻，此时若按单名判定返回 404，映射就
/// 永远没有出场机会。每段各自按单名口径判定，**所有**段都全禁才返回 true。
///
/// 请求名在所有家都不存在时原生段返回 false —— 那由调用方的「模型不存在」
/// 分支处理，错误文案应区分「没这个模型」与「被禁用了」。
pub fn model_blocked_everywhere(model: &str) -> bool {
    let rules = model_rules::current();
    // 原生段：请求名自己的承载家
    if !model_blocked_single(model, &rules) {
        return false;
    }
    // 映射段：任一条映射的候选家可用（未全禁）即放行
    let mappings = rules.mappings_of(model);
    if mappings.is_empty() {
        return true;
    }
    mappings.iter().all(|mapping| match &mapping.provider {
        // 指定家：该家被禁用 / 隐藏了 target 才算这段全禁。判定与转发候选链的
        // 过滤用**同一个** `provider_blocks_model`（含「先 id 后 name」的解析），
        // 否则两边口径会分叉成「过滤说这家的 target 没被禁、这里说全禁」，
        // 请求既过不了入口 404、又落不到那家。
        Some(provider) => kind_from_id(provider)
            .map(|kind| provider_blocks_model(&rules, kind, &mapping.target))
            // 未知 provider id（配置里写了注册表没有的家）→ 不算全禁，
            // 让它走「未知模型」那条更贴切的错误
            .unwrap_or(false),
        // 旧版全局条目：候选 = 承载 target 的所有家
        None => model_blocked_single(&mapping.target, &rules),
    })
}

/// 单个模型名的「承载家是否全被禁用 / 隐藏」（`model_blocked_everywhere` 的
/// 单名段；见它的说明）。
fn model_blocked_single(model: &str, rules: &model_rules::ModelRules) -> bool {
    let providers = providers_for_model(model);
    if providers.is_empty() {
        return false;
    }
    providers.iter().all(|kind| {
        match entry_id_in_manifest(&manifest_for(*kind), model) {
            Some(id) => rules.is_blocked(kind_id(*kind), &id),
            None => false,
        }
    })
}

/// 某一 provider 对「这个名字」是否已被禁用 / 隐藏 —— **转发候选链的过滤判据**。
///
/// 与 `model_blocked_single` 的分工：那个回答「承载家是否**全**被禁」（入口 404
/// 判定用，需要「被禁的家也算承载」才能答出「全禁」），这个回答「这一家还能不能
/// 收这个名字」（`router::route_for_forward` 逐家过滤用）。判据本身相同：把名字
/// 解析成该家目录里的条目 id（**先 id 后 name、忽略大小写**）再查规则，于是
/// 「用户禁用的是该家目录里的真名」与「客户端请求的是它的展示名」能对上。
///
/// 名字不在该家目录里时（映射 target 指向一个没被收录的上游名）按**原值**查，
/// 与 `model_blocked_everywhere` 的映射段同口径：规则里写了这个名字就认。
///
/// `rules` 由调用方传入而不是内部取 `model_rules::current()`：候选链要为链上
/// 每一家、每一个映射段各判一次，逐次读锁并克隆整份规则是白费的（转发热路径）。
pub fn provider_blocks_model(
    rules: &model_rules::ModelRules,
    kind: ProviderKind,
    name: &str,
) -> bool {
    let id = entry_id_in_manifest(&manifest_for(kind), name)
        .unwrap_or_else(|| name.trim().to_string());
    rules.is_blocked(kind_id(kind), &id)
}

/// 清单里「请求名」对应的条目 id（该家认识的**发送名**）。
///
/// ── 匹配口径：先比 id、再比 name（与 `providers_for_model` 逐字同口径）──
/// 两轮而不是单轮 OR：只有当清单里**没有任何条目的 id 命中**时才启用 name
/// 匹配 —— 与 `providers_for_model` 的「id 全不命中才走 name 轮」一致
///（避免「某家的 name 恰好等于另一条目的 id」时选错条目）。
///
/// ── 为什么发送前需要它 ──────────────────────────────────────
/// 客户端可能按**展示名**点名：Cline 的 `openai/gpt-6-astra` 展示名正是
/// `gpt-6-astra`。`providers_for_model` 的 name 轮会放行这类请求（这是既有
/// 契约：客户端传展示名不得突然 400），但把展示名原样发给上游会被拒
/// （Cline 实测 400 `invalid model format. Expected format: modelType/model`
/// —— 它要的是带通道前缀的完整 id）。所以发送前把展示名还原成该家的真名。
///
/// id 命中时返回**请求名原值**（不取清单里的大小写）：那是零改写的常见路径，
/// 统一大小写会让 body 白复制一次、日志里也多一行「改写」噪音。
fn entry_id_in_manifest(manifest: &[Value], requested: &str) -> Option<String> {
    let target = requested.trim().to_lowercase();
    if target.is_empty() {
        return None;
    }
    if manifest
        .iter()
        .any(|entry| model_id(entry).to_lowercase() == target)
    {
        return Some(requested.trim().to_string());
    }
    manifest
        .iter()
        .find(|entry| {
            entry
                .get("name")
                .map(crate::server::core::models::shape_value_text)
                .unwrap_or_default()
                .to_lowercase()
                == target
        })
        .map(model_id)
}

/// 一次「按家改写发送名」的结果：**该家要收到的模型名**，以及跟着它走的思考等级。
///
/// ── 为什么两个属性绑在一个返回值里（不要拆成两次解析）─────────
/// 「发什么名字」与「绑了什么等级」是**同一条映射**上的两个属性（见
/// `model_rules::reasoning` 的模块头）。若调用方分两次解析（先问名字、再问等级），
/// 两处各自的候选选择规则迟早会分叉 —— 而分叉的表现就是本项目最忌讳的那类
/// 静默错误：「请求落到了 B 家，却把 A 家那条映射的等级注入了」。
/// 一次解析、两个属性同源，跨家串味在结构上就不可能发生。
///
/// `reasoning` 为 `None` = 这次发送没有可注入的等级（本名直发、没有映射、
/// 或那条映射没绑等级），调用方据此跳过整段注入逻辑。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WireTarget {
    /// 该家认识的那个模型名（零改写时就是请求名原值）
    pub model: String,
    /// 决定了这个发送名的那条映射上绑的思考等级（`None` = 无）
    pub reasoning: Option<String>,
}

/// 请求名在该家**自己的映射条目**里命中的那一条（`provider` 明确等于本家）。
///
/// 抽成独立函数是为了让「原生直发」与「映射改写」两个分支用**同一个**选择规则
/// （见 `wire_target_for_provider` 的 ②）：原生分支只借用它的等级、映射分支连
/// 名字一起用。两处若各写一遍，同家多条映射时就会一个分支选第一条、另一个分支
/// 选前缀命中的那条。
///
/// `same_target_only` 为真时只保留 `target` 与 `name` 相同（忽略大小写）的条目
/// —— 原生直发分支用它挑「名字不变的那条按家映射」，理由见
/// `wire_target_for_provider` 的「思考等级跟着哪一条走」。
///
/// 入参是**已经查好的**同名映射条目（调用方只需查一次 `mappings_of`，见
/// `wire_target_for_provider`）—— 转发热路径上不做重复的线性扫描。
///
/// 选择规则（与函数头 ② 一致）：同家多条命中时优先选 target 落在本家通道前缀
/// 里的那条，其余家最多一条按家条目，这一步等于没开销。
fn pick_own_mapping<'a>(
    mappings: &[&'a model_rules::Mapping],
    provider_id: &str,
    name: &str,
    same_target_only: bool,
) -> Option<&'a model_rules::Mapping> {
    let own: Vec<&model_rules::Mapping> = mappings
        .iter()
        .copied()
        .filter(|m| {
            m.provider
                .as_deref()
                .map_or(false, |p| p.eq_ignore_ascii_case(provider_id))
        })
        .filter(|m| !same_target_only || m.target.eq_ignore_ascii_case(name))
        .collect();
    // 本家的**通道前缀**（`cline-free/` / `cline-pass/`）：只有 Cline 系有这个概念，
    // 前缀本身定义在 `cline::models::Pool`（那是「池 → 前缀」的唯一事实来源，
    // 转发侧与种子侧都用它）—— 这里按 provider id 反查，不另写一份字面量。
    let channel_prefix = super::cline::models::Pool::from_provider_id(provider_id)
        .map(super::cline::models::Pool::target_prefix);
    own.iter()
        .copied()
        .find(|m| channel_prefix.is_some_and(|prefix| m.target.starts_with(prefix)))
        .or_else(|| own.first().copied())
}

/// 发给某个 provider 时应使用的**该家认识的**模型名，以及跟着这条映射走的
/// 思考等级（见 [`WireTarget`]）。
///
/// ── 为什么需要这个改写 ─────────────────────────────────────
/// 候选链经映射扩池后，链上各家的目录里认的名字未必相同：WorkBuddy 认
/// `deepseek-v4.1-flash`，小浣熊只认 `sn-deepseek-v4-1-flash`。把请求名原样
/// 发给映射家必然 404 —— 上游只认识自己目录里的真名。所以**每家即将发送前**
/// 把 body 的 model 改成该家承载的那个名字。解析优先级：
///   ① 该家原生承载请求名 → 用**该家清单里那条目的 id**：id 命中时即请求名
///      原值（大多数请求是零改写，同名映射时原生家自动优先于映射，这正是
///      「主路」的来源）；展示名命中时还原成真名 —— 客户端按展示名点名
///      （Cline 的 `gpt-6-astra` 是 `openai/gpt-6-astra` 的展示名）时，
///      原样发上游会被拒（Cline 实测 400 `invalid model format`），
///      见 [`entry_id_in_manifest`]；
///   ② 请求名的映射条目里指定了该家 → 用那条的 target。同家多条命中（Cline
///      两池的短名各挂一条，**只有这一家会这样**）时优先选 target 落在本家
///      通道前缀里的那条 —— 前缀就是 provider id（`cline-free/` / `cline-pass/`，
///      见 `cline::models::Pool::target_prefix`），于是「这条映射属于哪个池」
///      由 target 自己说了算，不再需要读账号上的字段（早先按账号的 `pool`
///      字段判定，那套已随「池是身份而不是账号属性」的改动退场）；
///      其余家最多一条按家条目，这一步等于没开销；
///   ③ 旧版全局条目（provider 缺失）且该家承载 target → 用 target；
///      **② 恒先于 ③**，与数组顺序无关（全局条目是兼容形态，不挡新条目）；
///   ④ 都不命中 → 原样返回（防御：正常选路下不会发生，链就是这么算的）。
///
/// ── 思考等级跟着哪一条走（本函数里唯一容易做错的地方）─────────
/// `reasoning` 取自**决定了发送名的那条映射**：
///   - ② / ③ 命中 → 取那条的 `reasoning`（名字与等级同源，不可能串到别家）；
///   - ① 原生直发 → 名字不是任何映射给的，此时**只认「名字不变的那条按家映射」**
///     （`target` 与即将发出的名字相同，见下面的判据）。这一步存在的理由：
///     本项目的界面只能把等级绑在映射上，所以「给 catpaw 的 kimi-k3 绑 high」
///     在界面上表现为建一条 `kimi-k3 → kimi-k3（catpaw）` 的映射 —— 那条映射
///     对**选路**是冗余的（原生直发本来就落到这家），但它是用户表达「这个名字
///     在这家要用这个等级」的**唯一入口**，不认它等于让这条绑定永远无效。
///
///   为什么只认「名字不变」的那条（而不是该家为这个请求名建的任意一条）：
///   一条 `foo → bar（catpaw）` 的映射在 `foo` 被 catpaw 原生承载时**并没有
///   生效**（原生路由优先，发出去的是 `foo` 不是 `bar`）。它的等级是为「发到
///   catpaw 的 bar」写的，把它用到一次发的是 `foo` 的请求上，就是让一个没生效的
///   映射去影响另一个模型 —— 与「等级必须跟着那条映射走」自相矛盾。
///   判据因此是「这条映射的 target 就是即将发出的名字」：名字一致时，无论走原生
///   还是走映射，用户看到的上游模型都是同一个，等级跟着它不会错。
///   旧版全局条目（provider 缺失）**不参与**这一支：它在别家也显示，拿它的等级
///   去注入一个没被它改写过名字的原生请求，属于超出用户表达范围的推断。
///
/// 与改写前旧实现的差别：改写不再发生在请求入口（payload 的 model 保持
/// 客户端原值），只发生在**发出去的字节**上 —— 记账 / 限额键 / 日志里的模型
/// 始终是客户端请求的名字，报表不会因为映射而「换姓」。
///
/// `account` 参数保留但**不再被读**：它原先只用于「同家多条映射时按账号的
/// `pool` 字段挑一条」，现在这个判断由 target 前缀给出（见 ②）。留着它是因为
/// 调用方（`upstream::payload`）手上本来就有账号对象，而「这条映射该按谁的
/// 身份挑」将来可能再需要账号信息；去掉参数只会让那条链多一处改动。
pub fn wire_target_for_provider(
    model: &str,
    provider_id: &str,
    account: Option<&Value>,
) -> WireTarget {
    let _ = account;
    let requested = model.trim();
    if requested.is_empty() {
        return WireTarget { model: model.to_string(), reasoning: None };
    }
    let rules = model_rules::current();
    // 同名映射条目查一次、两个分支共用（原生分支只借等级、映射分支连名字一起用）
    let mappings = rules.mappings_of(requested);
    // ① 该家原生承载：发送名取该家清单里那条目的 id（id 命中 = 原值，
    //   展示名命中 = 还原真名）。「哪家承载」与「发什么名字」用的是**同一份
    //   清单、同一套匹配口径**（`providers_for_model` 与本函数都先 id 后 name），
    //   所以这里不会出现「认了它却发不出它的真名」。
    if providers_for_model(requested)
        .iter()
        .any(|kind| kind_id(*kind) == provider_id)
    {
        let name = kind_from_id(provider_id)
            .and_then(|kind| entry_id_in_manifest(&manifest_for(kind), requested))
            .unwrap_or_else(|| requested.to_string());
        // 等级只认「名字不变的那条按家映射」（理由见函数头「思考等级跟着哪一条走」）
        let reasoning = pick_own_mapping(&mappings, provider_id, &name, true)
            .and_then(|mapping| mapping.reasoning.clone());
        return WireTarget { model: name, reasoning };
    }
    // ② 先于 ③ 是**两遍扫描**而不是单循环按数组顺序：具体条目必须优先于
    // 旧版全局条目 —— 后者是升级兼容形态，存量用户盘上留着的旧条目（如
    // Cline 短名指向 pass 池的那条）在数组里排在前面，单循环会让它永远
    // 挡住新种出来的按家条目，短名改写就切不过来了。
    if let Some(mapping) = pick_own_mapping(&mappings, provider_id, requested, false) {
        return WireTarget {
            model: mapping.target.clone(),
            reasoning: mapping.reasoning.clone(),
        };
    }
    // ③ 旧版全局条目（provider 缺失）且该家承载 target
    for mapping in mappings.iter().filter(|m| m.provider.is_none()) {
        if providers_for_model(&mapping.target)
            .iter()
            .any(|kind| kind_id(*kind) == provider_id)
        {
            return WireTarget {
                model: mapping.target.clone(),
                reasoning: mapping.reasoning.clone(),
            };
        }
    }
    // ④ 防御：正常选路下不会走到（链就是这么算的）
    WireTarget { model: requested.to_string(), reasoning: None }
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
///
/// ── `scope`：网关 Key 的可用模型 / 可用提供商白名单（R9）────────
/// `None` = 不限制（免鉴权模式 / 环境变量 Key / 未知 Key，见 `key_scope` 模块头）。
/// 有 scope 时**在列表生成处过滤**，而不是留给前端 —— 这是服务端语义：
/// 「让对方拉列表只看到被授权的部分」（照抄 OmniProxy 在 `GET /v1/models` 里的
/// `allowedModelNames` 过滤）。在**这里**过滤而不是在 `api::chat::list_models`
/// 之后对 JSON 二次裁剪，是因为这一层才认识「条目的对外名」与「承载它的家」。
///
/// ── 提供商过滤为什么在**合并之前**（顺序上唯一的坑）──────────────
/// `merged_items_available` 是「可用的先认领」：同名模型由注册表顺序靠前的
/// 那家认领，后面那家的同名条目被丢掉。若先把合并做完再按 provider 过滤，
/// 会出现「A 家认领了 X、但 Key 只允许 B 家 → 过滤掉 A 的条目 → X 从列表里
/// 消失」，而转发侧按承载家过滤后**留的正是 B**（它确实能提供 X）。
/// 于是「列表里看不到、却能调通」—— 恰好是本项目最忌讳的那种不一致
///（见 `advertised_manifest_contains` 的「列表里没有就拒绝」）。
/// 因此先把**不允许的家整个从 active 里剔除**，再交给合并：认领只发生在
/// 允许的家之间，列表与路由两侧的白名单口径逐字同源。
///
/// 模型白名单则按**对外名**判，且上游 id 与每个映射别名**各自判定**
///（它们是不同的对外名，授权其中一个不该顺带授权另一个 —— 用户在白名单里
/// 勾的是名字）。两个白名单同时给定时是**交集**（与 OmniProxy 的注释一致）。
pub fn models_response(store: &AccountStore, scope: Option<&KeyScope>) -> Value {
    let active: Vec<(ProviderKind, Vec<Value>)> = active_manifests(store)
        .into_iter()
        .filter(|(kind, _)| key_scope::allows_provider(scope, kind_id(*kind)))
        .collect();
    let (source, last_refreshed_at) = aggregate_source(&active);
    let rules = model_rules::current();
    // 禁用 / 隐藏的模型不广告；映射名作为独立条目追加（复制目标条目、换 id），
    // 让客户端能在 /v1/models 里发现它们
    let mut data: Vec<Value> = Vec::new();
    let mut aliases: Vec<Value> = Vec::new();
    // 「可用的先认领」：某家禁用的模型由仍提供的另一家继续对外暴露（见
    // merged_items_available 的说明）；映射条目同样只在目标模型仍可用时追加，
    // 同一对外名跨提供商去重（同名多条映射只广告一条）
    for (_, item) in merged_items_available(&active, &rules) {
        let id = model_id(&item);
        let mut pending: Vec<Value> = Vec::new();
        for alias in rules.aliases_of_any(&id) {
            let mut copy = item.clone();
            if let Some(object) = copy.as_object_mut() {
                object.insert("id".to_string(), Value::String(alias.to_string()));
                object.insert("is_default".to_string(), Value::Bool(false));
            }
            pending.push(copy);
        }
        if key_scope::allows_model(scope, &id) {
            data.push(item);
        }
        for copy in pending {
            let name = model_id(&copy);
            if key_scope::allows_model(scope, &name) {
                aliases.push(copy);
            }
        }
    }
    data.extend(aliases);
    list_response_from(data, source, last_refreshed_at)
}

/// 广告视图里的全部对外名（条目 id + 映射别名，去重、忽略大小写）。
///
/// 与 `/v1/models` 的 data 同源（`merged_items_available` + `aliases_of_any`，
/// 见 `models_response`）：供「相近模型提示」使用 —— 提示里出现的名字一定能
/// 被列出来，也一定能通过 `resolve_model` 的广告视图校验，不会出现
/// 「提示它、点它又被拒」的循环。
pub fn advertised_model_ids(active: &[(ProviderKind, Vec<Value>)]) -> Vec<String> {
    let rules = model_rules::current();
    let mut ids: Vec<String> = Vec::new();
    for (_, item) in merged_items_available(active, &rules) {
        let id = model_id(&item);
        if id.is_empty() {
            continue;
        }
        let aliases: Vec<String> = rules
            .aliases_of_any(&id)
            .into_iter()
            .map(str::to_string)
            .collect();
        let names = std::iter::once(id).chain(aliases);
        for name in names {
            if !ids.iter().any(|known| known.eq_ignore_ascii_case(&name)) {
                ids.push(name);
            }
        }
    }
    ids
}

/// 管理页视图：**每家提供商自己的完整清单**（含禁用 / 隐藏），每条带
/// `enabled` / `hidden` / `aliases` / `source`，另附映射表。形状：
/// `{ models: [...], mappings: [{alias, target, provider}] }`
///
/// `source` 是这家清单的出处（`"remote"` / `"builtin"`，见 [`catalog_source`]），
/// 供前端的「来源」列显示 —— 用户在管理页看到某个模型，能一眼知道它是从上游
/// 目录实时拉来的，还是内置静态表里的（后者在上游目录接口失效 / 未登录时
/// 才会成为唯一数据源，两者可信度不同）。
///
/// ── 为什么这里**不做跨提供商去重**（与 `/v1/models` 的关键差别）──
/// 同一个模型名（如 `glm-5.3-flash`）可能同时出现在多家的清单里。
/// 对外的 `/v1/models` 按名字去重（一个名字一个条目，先到的家认领），
/// 但管理页如果也去重，「被别家认领」的那家的真实模型就从列表里消失了 ——
/// 看起来像「这家只有一个模型」，而它明明有两个（CatPaw 的 `glm-5.3-flash`
/// 曾因此被 WorkBuddy 认领而不显示）。管理页的职责是管理**每家上游的真实
/// 清单**，所以这里每家各列各的，同名模型在每家各占一行。
///
/// 禁用 / 删除规则按 **(提供商, 模型 id)** 生效（modelRules 的键带 provider）：
/// 管理页里关掉某一家的模型，只是这一家不再接收该模型的请求，别家照常 ——
/// 与 /v1/models「可用的先认领」的对外语义一致（见 `merged_items_available`）。
///
/// **映射（aliases）按行归属过滤**：带 provider 的条目只出现在自己那家的行上，
/// 旧版全局条目（provider 缺失）在所有承载 target 的行上都显示 —— 管理页的
/// 一行 = 一次「这条上游模型以哪些对外名暴露」，映射的主备关系（同一对外名
/// 在多行出现）在表格上一眼可见。顶层 `mappings` 是全量映射表（含 provider），
/// 前端添加映射弹窗的提供商下拉用注册表，不需要它，但留着便于排查。
///
/// 顶层还带 `reasoningLevels`（思考等级的可选值，见 `model_rules::REASONING_LEVELS`）
/// —— 那是「照抄 OmniProxy 的手动思考等级绑定」的候选表，界面按它铺下拉项。
/// 每条映射的 `reasoning` 字段是它的绑定值（`null` = 不覆盖）。
/// **绑定已接入转发**：等级与「这一家收哪个模型名」在同一次解析里取出
/// （`wire_target_for_provider`），由承载那家的适配器翻译成本家上游认识的字段
/// —— 各家的翻译规则与「哪些情况故意不注入」见 `model_rules::reasoning` 的模块头。
pub fn manage_view(store: &AccountStore) -> Value {
    let rules = model_rules::current();
    let models: Vec<Value> = manage_entries(store, &rules)
        .into_iter()
        .map(|mut entry| {
            let id = model_id(&entry);
            let provider =
                entry.get("provider").and_then(Value::as_str).unwrap_or("").to_string();
            if let Some(object) = entry.as_object_mut() {
                object.insert(
                    "enabled".to_string(),
                    Value::Bool(!rules.is_disabled(&provider, &id)),
                );
                object.insert(
                    "hidden".to_string(),
                    Value::Bool(rules.is_hidden(&provider, &id)),
                );
                object.insert(
                    "aliases".to_string(),
                    Value::Array(
                        rules.aliases_of(&provider, &id)
                            .into_iter()
                            .map(|a| Value::String(a.to_string()))
                            .collect(),
                    ),
                );
            }
            entry
        })
        .collect();
    let mappings: Vec<Value> = rules
        .mappings
        .iter()
        .map(|m| {
            // `dangling`：这条映射挂不到**上面那批行里的任何一行**。
            //
            // 判据与渲染映射 chip 的 `aliases_of` 逐字同源（target 比行 id，
            // provider 缺失的旧版条目对任何家都命中）—— 直接对着刚构建好的
            // `models` 问「有没有一行接得住它」，而不是另查一份清单：
            // 管理页的行来自**广告清单**（Cline 还会按账号池收窄），另查
            // 路由清单会出现「标了未挂载、表格里却有那一行」这种自相矛盾。
            //
            // 前端据此在表尾列一个「未挂载的映射」分组：映射存进了配置、
            // 也真的参与转发，却在表里没有任何行可以显示 —— 用户会以为
            // 没保存成功。被账号池收窄的（Cline 的 `cline-pass/*`）同样落进
            // 这一组，那是对的：它们确实一行都挂不上，而这正好解释了
            // 「为什么 pass 池的模型一个都不显示」。
            let dangling = !models.iter().any(|row| {
                model_id(row).eq_ignore_ascii_case(&m.target)
                    && m.provider.as_deref().map_or(true, |provider| {
                        row.get("provider")
                            .and_then(Value::as_str)
                            .map_or(false, |row_provider| {
                                row_provider.eq_ignore_ascii_case(provider)
                            })
                    })
            });
            // `carried`：这个名字**路由层认不认**（不分账号、不看池收窄）——
            // 带 provider 的条目只看那一家，旧版全局条目看所有家（与
            // `wire_target_for_provider` 的候选口径一致）。
            //
            // 与 `dangling` 配对使用，把「表里看不见」的两种情形分开：
            //   dangling=true,  carried=true  → 配置有效，只是这家现在不广告它
            //                                   （Cline 的池收窄是典型：没加
            //                                   pass 池账号时 pass 系模型不广告，
            //                                   但路由链仍然认得它们）；
            //   dangling=true,  carried=false → 名字哪儿都没有（手输打错、
            //                                   上游下架），那条映射是死的。
            // 两种都要列出来（都挂不到行上），但该给用户的建议相反：前者可能
            // 只是缺个账号，后者该改或该删 —— 由前端分档措辞，后端只给事实。
            let carried = match m.provider.as_deref() {
                Some(provider) => kind_from_id(provider).is_some_and(|kind| {
                    manifest_for(kind)
                        .iter()
                        .any(|entry| model_id(entry).eq_ignore_ascii_case(&m.target))
                }),
                None => !providers_for_model(&m.target).is_empty(),
            };
            json!({
                "alias": m.alias,
                "target": m.target,
                "provider": m.provider,
                // 思考等级绑定（None = 不覆盖）。放在**顶层这份全量映射表**里
                // 而不是给每行的 `aliases` 再加一个平行数组：行上的 chip 已经
                // 带着 (alias, target, provider) 三元组，前端拿它对这里查一次
                // 就能拿到等级 —— 两个数组一旦因为过滤口径不同而对不上，
                // 界面上会出现「chip 在、等级丢了」这种无从解释的空档。
                "reasoning": m.reasoning,
                "dangling": dangling,
                "carried": carried,
            })
        })
        .collect();
    json!({
        "models": models,
        "mappings": mappings,
        // 思考等级的可选值（照抄 OmniProxy 的 GENERIC_REASONING_LEVELS）。
        // 由后端下发而不是前端自己抄一份：这张表将来若要调整（或改成按家给
        // 候选），只有一处要改；前端只管把数组铺成下拉项。
        "reasoningLevels": model_rules::REASONING_LEVELS,
    })
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
    // 与 /v1/models 同一过滤（按提供商判定 + 可用的先认领）：某家禁用的模型
    // 由仍提供的另一家继续出现在界面的「可用模型」里
    let rules = model_rules::current();
    merged_items_available(&active_manifests(store), &rules)
        .into_iter()
        .map(|(_, item)| item)
        .collect()
}

/// 每家的**对外名清单**（条目 id + 映射别名，去重、忽略大小写）：
/// `{"workbuddy": ["..."], "raccoon": [...]}`，只含**当前可用**的家
/// （有登录态且清单非空，判据同 `active_manifests`）。
///
/// ── 为什么要单独一个函数（不能拿 `session_models` 过滤）──────────
/// `session_models` / `/v1/models` 是**跨家去重**后的聚合视图：同名模型只留
/// 最先认领的那一家（见 `merged_items_available`）。网关 Key 页要让用户勾
/// 「这把 Key 能用哪几家的哪些模型」，若拿那份去重视图按 `provider` 字段过滤，
/// **被别家认领的同名模型会凭空消失** —— 例如 `glm-5.3-flash` 同时由 WorkBuddy
/// 与 AutoClaw 提供、条目上记的是 WorkBuddy，用户勾了 AutoClaw 却看不到它，
/// 而 AutoClaw 确实收这个模型（`route_for_forward` 认得）。
/// 所以本函数按**家**遍历原始清单（不去重），这正是「哪家能收哪些名字」的真值。
///
/// ── 为什么包含映射别名 ──────────────────────────────────────
/// 别名是用户自己起的对外名，客户端就是用它发请求的（见 `model_rules` 的映射
/// 语义），所以它必须可勾 —— 与 `/v1/models` 把别名当独立条目广告同一口径。
/// 判定用 `aliases_of_any`（跨家全量）而不是 `aliases_of`（按行归属）：后者要求
/// 映射条目带 provider 字段，而**旧版映射条目是不带的**（见 `model_rules`
/// 的兼容说明），用按行归属去判会让旧条目的别名在任何一家下都勾不到。
///
/// ── 为什么交出去的是「每家的清单」而不是「一次请求的并集」──────
/// 界面要随用户勾选**即时**联动模型候选（勾一家多一批、取消一家少一批）。
/// 给整张表，前端本地取并集，勾选时零往返；给并集接口则每勾一下就发一次请求，
/// 还可能出现「响应回来时用户已经改了勾选」的竞态。
///
/// 某家不在表里 = 这家现在没有可用登录态或清单为空 → 界面按空清单处理
/// （用户勾了它也勾不到任何模型，这是事实而不是缺陷）。
pub fn models_by_provider(store: &AccountStore) -> Value {
    let rules = model_rules::current();
    let mut map = Map::new();
    for (kind, manifest) in active_manifests(store) {
        let provider_id = kind_id(kind);
        let mut names: Vec<String> = Vec::new();
        for item in manifest {
            let id = model_id(&item);
            if id.is_empty() || rules.is_blocked(provider_id, &id) {
                continue;
            }
            push_unique(&mut names, &id);
            for alias in rules.aliases_of_any(&id) {
                push_unique(&mut names, alias);
            }
        }
        names.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()));
        map.insert(
            provider_id.to_string(),
            Value::Array(names.into_iter().map(Value::String).collect()),
        );
    }
    Value::Object(map)
}

/// 往清单里放一个名字（忽略大小写去重）。同一个名字可能同时是某家的条目 id
/// 与另一家的别名，只该出现一次。
fn push_unique(out: &mut Vec<String>, name: &str) {
    let name = name.trim();
    if name.is_empty() {
        return;
    }
    if out.iter().any(|known| known.eq_ignore_ascii_case(name)) {
        return;
    }
    out.push(name.to_string());
}

/// 管理页条目：**不去重**的每家清单（`manage_view` 的数据源）。
///
/// 与 `session_models` 的唯一差别是数据源：那里走 `merged_items_available`
/// （跨提供商按模型 id 去重，服务「客户端能请求什么」的对外视图），
/// 这里直接遍历 `active_manifests` 的每家原始清单 —— 管理页要看见的是
/// 「每家上游到底有哪些模型」，同名模型（如 WorkBuddy 与 AutoClaw 都有
/// `glm-5.3-flash`）必须每家各显示一行，否则被别家认领的那家会凭空少模型。
///
/// 条目字段（`id` / `name` / `isDefault` / `credits` / `provider` /
/// `providerLabel` / `source`）里，**只有 `source` 是本层独有的**：前六个与
/// `/api/session` 的 `models` 同形，前端两份数据共用一套渲染；`source` 是管理页
/// 「来源」列专用（`/api/session` 的条目不带它，见 [`catalog_source`]）。
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
        // 家级的来源（远程 / 内置）。**逐条**判定见下面的 `manual` 覆盖：
        // 手动登记的自定义模型与这家清单的来源无关，标「远程」或「内置」都是
        // 在回答另一个问题（用户要知道的是「这条是我自己加的」）。
        let source = catalog_source(kind);
        let mut items = manifest;
        // 组内排序：启用的排前（同一家内按该家自己的启停状态）
        items.sort_by_key(|item| rules.is_disabled(provider_id, &model_id(item)));
        for item in items {
            let id = model_id(&item);
            let item_source = if rules
                .custom
                .iter()
                .any(|custom| custom.matches(provider_id, &id))
            {
                "manual"
            } else {
                source
            };
            entries.push(manage_entry_json(provider_id, &item, item_source));
        }
    }
    entries
}

/// 某一家的清单**当前来自哪里**（管理页「来源」列）。
///
/// ── 为什么是「家」级而不是「条」级 ────────────────────────────
/// 五家的清单都是**二选一**的：远程拉到过就用远程那份，否则用内置静态表
/// （`list()` 全是「远程非空 → 返回远程；否则返回静态」这一种形状，见各家
/// 的清单模块）。模型条目本身不携带来源位，所以来源是这一家的属性，
/// 同一次渲染里该家所有行标同一个值。
///
/// ── 唯一的例外：手动登记的自定义模型（"manual"）──────────────
/// 它们不是从这家清单来的（见 `manifest_for` 的注入说明），标「远程」或
/// 「内置」都是在回答另一个问题。所以 `manage_entries` 对这类条目**逐条**改成
/// `"manual"` —— 本函数只负责「家级」那部分，不知道也不该知道手动条目。
///
/// ── 为什么复用 `refresh_meta` ────────────────────────────────
/// 它已经给出「这家是否远程刷新成功过」的判据（各家的判定细节不同：WorkBuddy
/// 认整个 catalog 的 `remote_refreshed`、Qoder 认两个地区任一、CatPaw / AutoClaw
/// 认远程清单非空）。这里再写一份就会与该判据分叉 —— 界面上标「远程」而
/// `/v1/models` 的 `meta.source` 标 `builtin`（或反过来）是纯粹的误导。
///
/// 注意判据是「**曾经**成功拉到过」而不是「这次请求返回的就是远程那份」：
/// 刷新失败会保留上一份成功结果（各家的 `refresh` 都是这个取向），
/// 那份数据的来源仍是远程，标「远程」正确。
fn catalog_source(kind: ProviderKind) -> &'static str {
    if refresh_meta(kind).0 {
        "remote"
    } else {
        "builtin"
    }
}

/// 管理页条目的公共成形逻辑（从 `list_item` 输出里挑字段）。
///
/// `source` 是这一家的清单来源（`"remote"` / `"builtin"`，见 [`catalog_source`]）：
/// 前端的「来源」列直接读它，不在前端做任何 id→来源的映射（那是后端注册表
/// 与各家清单模块的职责，加一家 provider 时前端零改动）。
fn manage_entry_json(provider_id: &str, item: &Value, source: &str) -> Value {
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
    entry.insert("source".to_string(), Value::String(source.to_string()));
    Value::Object(entry)
}

/// provider id → 注册表里的展示名；未登记的 id 原样回显。
///
/// 回退成 id 而不是「未知」：条目里的 id 来自 `owned_by`，它是实际承载该模型的
/// 那家 —— 真出现未登记的 id，回显原文比一句笼统的「未知」更能定位问题。
/// 前端对「归属缺失」另有兜底组（见 app.js 的 renderModels），两处都不会丢条目。
///
/// 实现落在注册表模块（`providers::label_of`）：那是「id → 名字」的唯一事实
/// 来源，本模块只沿用它的取舍，不另写一份 match。
fn provider_label_of(provider_id: &str) -> &str {
    crate::server::core::providers::label_of(provider_id)
}

/// 未知模型报错用的「相近模型」提示：在**广告视图**的对外名集合里找最相近的。
///
/// 数据源是 `advertised_model_ids`（条目 id + 映射别名）而不是能力视图：
/// 提示里出现的名字一定能被 `/v1/models` 列出、也一定能通过 `resolve_model`
/// 的校验 —— 否则就成了「提示它、点它又被拒」的循环。判定规则复用
/// `models::suggest_from`（纯函数），与改造前 workbuddy 单家的提示口径一致。
pub fn suggest_advertised(
    active: &[(ProviderKind, Vec<Value>)],
    model: &str,
    limit: usize,
) -> Vec<String> {
    suggest_from(advertised_model_ids(active), model, limit)
}