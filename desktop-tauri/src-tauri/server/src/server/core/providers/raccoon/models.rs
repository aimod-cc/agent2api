//! 小浣熊模型清单（Agent2API 改造 W3-T4；移植来源 `raccoon-models.mjs`）。
//!
//! ── 清单从哪来 ──────────────────────────────────────────────
//!   1. **静态兜底**：`FALLBACK_MODELS` 的 5 个模型（小浣熊客户端
//!      `resources/default-llm-config.json` 预置 + 实测的 `/model_catalog` 内容）。
//!      进程启动即可用，网络不通时 /v1/models 仍然有内容。
//!   2. **远程刷新**：`GET {llmBase}/model_catalog`（10 分钟缓存）。token 失效或
//!      网络失败时**保留旧缓存**，从未成功过就继续用静态兜底 —— 与源实现
//!      `createModelCatalog` 的两级兜底逐条一致。
//!
//! ── 为什么清单条目保留上游原始形态 ───────────────────────────
//! 聚合目录（`providers/catalog`）用 `models::list_item` 做字段映射，它认的是
//! **上游原始形态**（字段名与 `/v3/config` 或小浣熊 `/model_catalog` 一致）。
//! 因此本模块只做「字段名归一 + 类型修正」（源实现 `normalizeCatalogEntry`），
//! 不提前翻译成 OpenAI 条目 —— 翻译只发生在聚合出口，单家与聚合两个视角
//! 因此永远共用同一份字段映射（见 `core::models::shape` 的模块头）。
//!
//! ── 一处**必须**做的键名映射（与源实现的差异，实测发现）────────
//! 小浣熊目录用 `name` 当**模型 id**（源实现 `resolveModelRoute` 就是拿
//! `model.name` 与请求里的 model 比对），而聚合层与 `list_item` 认的是 `id`
//! （workbuddy 的清单里 `id` 是模型 id、`name` 是展示名）。不做这层映射，
//! `/v1/models` 会输出一批**没有 id 的条目**（本机实测：`meta.count` 有值、
//! `data` 里每条的 id 全空），路由匹配也只能退化成按 name 比 —— 那是静默的
//! 功能缺失。因此 `list()` 返回前把源形态翻成聚合层要的形状：
//!   `name`（模型 id）→ `id`；`displayName` → `name`（展示名）；
//!   `contextWindow` → `maxInputTokens`、`maxTokens` → `maxOutputTokens`
//!   （`list_item` 认的是后两个键）；
//!   `supportImage` / `supportThinking` → `supportsImages` / `supportsReasoning`。
//! `displayName` / `description` / `tags` 等源字段**原样保留**（前端与排障要用）。
//!
//! ── 进程级句柄 ──────────────────────────────────────────────
//! 与 `core::models::global_catalog()` 同一模式（`OnceLock` + `RwLock`）：
//! 适配器是无状态单例，清单必须挂在进程级的共享句柄上，刷新才能对所有
//! 调用点（`/v1/models`、路由判定、后台刷新）同时可见。
//!
//! ── 与 workbuddy 清单的差异（有意）──────────────────────────
//!   1. **不过滤**：workbuddy 有「非对话模型排除前缀 + 白名单」这套准入规则
//!      （它的 /v3/config 会下发补全/图像模型）；小浣熊的 `/model_catalog`
//!      本身就是对话模型目录，源实现也没有过滤，因此这里全量采用。
//!   2. **默认模型**：源实现的 `defaultModel` 只用于「请求没给 model」时的
//!      回退，而那个回退在网关层由 `providerRoute` 与 `defaultModel` 配置承担
//!      （架构文档 §4.4：小浣熊不认默认模型概念，`supports_default_model()`
//!      返回 false）。因此这里**不保存 defaultModel**。

use std::sync::{OnceLock, RwLock};
use std::time::Duration;

use serde_json::{json, Value};

use crate::server::core::model_rules;
use crate::server::core::providers::adapter::ModelRefreshOutcome;
use crate::server::core::providers::catalog_cache;
use crate::server::logging;

/// 静态兜底清单里的默认对话模型 id（源实现 `DEFAULT_MODEL_ID`）。
///
/// 源实现允许用环境变量覆盖（`RACCOON_DEFAULT_MODEL`）；这里保留同名支持，
/// 因为它是**用户可见的模型名**（客户端点名要它时上游要认得）——
/// 覆盖值只在静态兜底清单里生效，远程目录回来时以远程为准。
pub fn default_model_id() -> String {
    std::env::var("RACCOON_DEFAULT_MODEL")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "raccoon-chat-ml-5-5".to_string())
}

/// 远程目录缓存有效期（源实现 `CATALOG_CACHE_TTL_MS`：10 分钟）
const CATALOG_CACHE_TTL_MS: i64 = 10 * 60_000;

/// 目录请求超时（源实现 `CATALOG_TIMEOUT_MS`）
const CATALOG_TIMEOUT_MS: u64 = 10_000;

/// 静态兜底清单（照抄源实现 `FALLBACK_MODELS` 的 5 条）。
///
/// 字段名与远程 `normalizeCatalogEntry` 的输出**完全同形**（`name` /
/// `displayName` / `description` / `tags` / `contextWindow` / `maxTokens` /
/// `supportImage` / `supportThinking`）—— 于是「远程目录」与「静态兜底」两条
/// 来源在下游（聚合目录）看来没有区别，不会出现「离线时字段少一半」。
///
/// ── `credits` 是静态快照，取**基准倍率** ────────────────────────
/// 与远程路径同形（`"x1 credits"`）。取 `billing_multiplier`（基准/原价）而
/// 不是折后的 `billing_effective_multiplier`：折扣带明确的活动期
/// （目录里 `sn-glm-5-3-flash` 的折扣到 2026-09-30、`sn-sensenova-6-8-flash-lite`
/// 的限免到 2026-09-28），写死折后价会在活动结束后继续显示低价 ——
/// 折扣由远程刷新如实带回来，兜底表只保证「离线也有一个不会说谎的数」。
///
/// `raccoon-chat-ml-5-5` **刻意不给**：它不在远程目录里（是桌面端本地配置的
/// 默认模型），没有任何官方倍率可查 —— 编一个不如显示 `—`。
pub fn fallback_models() -> Vec<Value> {
    json!([
        {
            "name": "raccoon-8c4485",
            "displayName": "Raccoon-Work",
            "description": "适合复杂办公任务、代码开发、调试和图像识别等任务（目录默认旗舰，自称 GPT-5）",
            "tags": ["general", "office", "code", "vision"],
            "contextWindow": 1_000_000,
            "maxTokens": 100_000,
            "supportImage": true,
            "supportThinking": false,
            "credits": "x1 credits",
        },
        {
            "name": "raccoon-19b265",
            "displayName": "Raccoon-Work-260817-A",
            "description": "适合复杂办公任务、代码开发和综合分析等任务（A/B 变体 A，自称 GPT-5）",
            "tags": ["general", "office", "code", "analysis"],
            "contextWindow": 1_000_000,
            "maxTokens": 100_000,
            "supportImage": false,
            "supportThinking": false,
            "credits": "x1 credits",
        },
        {
            "name": "raccoon-405a1c",
            "displayName": "Raccoon-Work-260817-B",
            "description": "适合通用对话、轻量代码和内容处理等任务（A/B 变体 B，自称 GPT-5）",
            "tags": ["general", "chat", "rewrite", "summary", "fast"],
            "contextWindow": 1_000_000,
            "maxTokens": 100_000,
            "supportImage": false,
            "supportThinking": false,
            "credits": "x1 credits",
        },
        {
            "name": default_model_id(),
            "displayName": "Raccoon Chat (默认)",
            "description": "小浣熊默认对话模型（default-llm-config.json 预置）。",
            "tags": [],
            "contextWindow": 180_000,
            "maxTokens": 80_000,
            "supportImage": false,
            "supportThinking": true,
        },
        {
            "name": "sn-sensenova-6-8-flash-lite",
            "displayName": "SenseNova 6.8 Flash Lite",
            "description": "轻量模型，用于标题生成等小任务。",
            "tags": [],
            "contextWindow": 256_000,
            "maxTokens": 63_999,
            "supportImage": false,
            "supportThinking": true,
            "credits": "x0.5 credits",
        },
    ])
    .as_array()
    .cloned()
    .unwrap_or_default()
}

/// 目录的内部状态：远程清单 + 刷新元信息。
///
/// `models` 为空 = 还没成功拉过远程目录 → 读取时回落到静态兜底
/// （与源实现 `current()` 的判定同一语义：`!cache.models.length` 就用兜底）。
#[derive(Default, Clone)]
struct CatalogState {
    models: Vec<Value>,
    /// 最后一次成功的远程刷新时间（毫秒），初始 0
    fetched_at: i64,
}

/// 进程级目录句柄。首次初始化时**先从持久化缓存恢复**（上次成功拉到的远程
/// 清单），没有再留空 —— 空状态的读取语义就是「回落到静态兜底清单」。
fn catalog() -> &'static RwLock<CatalogState> {
    static CATALOG: OnceLock<RwLock<CatalogState>> = OnceLock::new();
    CATALOG.get_or_init(|| RwLock::new(restored_state()))
}

/// 首次初始化读一次持久化缓存（见 `providers::catalog_cache` 的模块头）。
///
/// 缓存里存的就是 `CatalogState` 的形态（`refresh` 落地的那份），所以这里只做
/// 「搬回来」：不重新解析、也不重新归一 —— 两处各写一份映射迟早分叉。
fn restored_state() -> CatalogState {
    match catalog_cache::load(catalog_cache::SCOPE_RACCOON) {
        Some(cached) => CatalogState { models: cached.models, fetched_at: cached.fetched_at },
        None => CatalogState::default(),
    }
}

/// 读锁；锁中毒（持锁 panic）时接管内部数据继续用（与账号存储同一策略）
fn read_state() -> CatalogState {
    match catalog().read() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

/// 当前清单（远程成功后是远程清单，否则静态兜底），**已翻成聚合层的键名**。
///
/// 每次调用克隆一份 —— 清单最多几十条，克隆代价远小于持锁穿越调用方逻辑。
/// 键名映射的理由见模块头（`name` 是模型 id、`displayName` 是展示名）。
pub fn list() -> Vec<Value> {
    let state = read_state();
    if state.models.is_empty() {
        return fallback_models().iter().map(listing_entry).collect();
    }
    state.models.iter().map(listing_entry).collect()
}

/// 源形态 → 聚合层的条目形态（键名映射；见模块头）。
///
/// 只做**改名与搬运**，不做任何值改写：上游给的 `null` 保持 `null`
/// （`list_item` 对 null 的处理是「不输出该键」，与源实现的 `undefined` 一致）。
fn listing_entry(model: &Value) -> Value {
    let Some(object) = model.as_object() else {
        return model.clone();
    };
    let mut out = object.clone();
    let model_id = object
        .get("name")
        .map(|value| super::jwt::js_text(value))
        .unwrap_or_default();
    out.insert("id".to_string(), Value::String(model_id));
    if let Some(display) = object.get("displayName") {
        out.insert("name".to_string(), display.clone());
    }
    for (source, target) in [
        ("contextWindow", "maxInputTokens"),
        ("maxTokens", "maxOutputTokens"),
    ] {
        if let Some(value) = object.get(source) {
            out.insert(target.to_string(), value.clone());
        }
    }
    for (source, target) in [
        ("supportImage", "supportsImages"),
        ("supportThinking", "supportsReasoning"),
    ] {
        if let Some(value) = object.get(source) {
            out.insert(target.to_string(), value.clone());
        }
    }
    Value::Object(out)
}

/// 是否已经成功采用过远程清单（排障与 `meta.source` 用）
pub fn remote_refreshed() -> bool {
    !read_state().models.is_empty()
}

/// 最后一次成功刷新远程目录的时间（毫秒；从未成功过为 0）
pub fn last_refreshed_at() -> i64 {
    read_state().fetched_at
}

/// 刷新远程目录（`GET {llmBase}/model_catalog`）。
///
/// 语义（源实现 `refresh`）：缓存 10 分钟内直接返回；HTTP 非 2xx、解析失败、
/// 目录为空都**保留现有清单**（已有远程清单就继续用，没有就继续用静态兜底）。
/// 失败不返回错误：这是后台维护动作，调用方按返回值打日志/汇报。
///
/// 入参 `token` 用于 Authorization 头；为空串表示没有任何可用凭证 ——
/// 源实现允许无 token 请求（目录接口本身可能公开），这里保持同一行为。
///
/// ── `force`：手动刷新必须能绕过 TTL（这是本函数唯一的语义开关）──────
/// TTL 早退是为**自动路径**设的（客户端每次拉 `/v1/models` 都真打一次上游既
/// 没必要也招风控）。但用户手动点「刷新模型清单」时预期是「现在真的去拉一次」：
/// 若在 TTL 窗口内照样早退，界面只能显示「已刷新」而清单一个字没变 ——
/// 与功能坏掉无法区分。因此 `force = true` 跳过 TTL，缓存是否该复用只由
/// **谁发起**决定，判断权交给调用方（见 `ProviderAdapter::refresh_models` 的说明）。
///
/// ── 返回值的三档（供手动路径如实汇报）──────────────────────────
///   - `refreshed(n)`：真拉到并落地了新清单；
///   - `unchanged()`：TTL 未到（只有 `force = false` 时会走到）；
///   - `failed(原因)`：**这次真的去拉了但没成功**（HTTP 非 2xx / 网络错误 /
///     目录为空）。清单仍按既有行为保留（不清空），失败原因如实带给调用方。
pub async fn refresh(
    token: &str,
    proxy: Option<&crate::server::core::proxies::ResolvedProxy>,
    force: bool,
) -> ModelRefreshOutcome {
    // 缓存命中：距上次成功刷新不足 TTL，直接跳过（源实现 `fresh` 判定）。
    // `force` 时整块跳过 —— 手动刷新就是要打这一次上游
    if !force {
        let state = read_state();
        if !state.models.is_empty()
            && logging::now_ms() - state.fetched_at < CATALOG_CACHE_TTL_MS
        {
            return ModelRefreshOutcome::unchanged();
        }
    }
    let url = format!("{}/model_catalog", super::llm_base_url());
    let mut headers: Vec<(String, String)> =
        vec![("Accept".to_string(), "application/json".to_string())];
    if !token.is_empty() {
        headers.push(("Authorization".to_string(), format!("Bearer {token}")));
    }
    let outcome = crate::server::core::auth_http::send_raw(
        "GET",
        &url,
        None,
        &headers,
        proxy,
        Some(CATALOG_TIMEOUT_MS),
    )
    .await;
    // 「沿用哪一份」的措辞在三条失败路径上都要用：它决定了用户看到的是
    // 「刷新失败（保留旧目录）」还是「刷新失败（仍在用静态兜底清单）」。
    // 写成闭包而不是提前算好：判据是**失败那一刻**的缓存状态，提前到请求前
    // 求值会因为期间别处刷新成功而说错（与改造前逐处内联的求值时机一致）
    let kept = || if remote_refreshed() { "旧缓存" } else { "静态兜底" };
    let payload = match outcome {
        Ok(response) => {
            if !response.ok {
                let reason = format!("上游返回 HTTP {}", response.status);
                logging::verbose(
                    "[Models]",
                    &format!("小浣熊模型目录拉取失败（沿用{}）: HTTP {}", kept(), response.status),
                );
                return ModelRefreshOutcome::failed(reason);
            }
            response.payload.unwrap_or(Value::Null)
        }
        Err(error) => {
            let reason = crate::server::core::egress::describe_error_detail(&error);
            logging::verbose(
                "[Models]",
                &format!("小浣熊模型目录拉取失败（沿用{}）: {reason}", kept()),
            );
            return ModelRefreshOutcome::failed(reason);
        }
    };
    let models = parse_catalog(&payload);
    if models.is_empty() {
        logging::verbose("[Models]", &format!("小浣熊模型目录为空（沿用{}）", kept()));
        // 非 2xx 之外的另一种「拉了但没用」：上游给的目录解析不出任何模型。
        // 报失败而不是「没刷」—— 用户点了按钮，这是需要他知道的异常
        return ModelRefreshOutcome::failed("上游返回的模型目录为空");
    }
    // 先取好 id 清单（条目的 `name` 键 = 模型 id），供落地后的默认规则种子用
    let ids: Vec<String> = models
        .iter()
        .filter_map(|model| model.get("name").map(super::jwt::js_text))
        .collect();
    let count = models.len();
    let next = CatalogState { models, fetched_at: logging::now_ms() };
    // 落持久化缓存（进程重启后由 `restored_state` 读回）：放在写内存状态之前，
    // 因为 `next` 要被 `write` 消费。**不在目录锁内** —— 缓存写入要拿库连接锁，
    // 两把锁不能嵌套（本目录的硬约束是「持锁期间不做 IO」）。
    catalog_cache::save(catalog_cache::SCOPE_RACCOON, &next.models, next.fetched_at);
    match catalog().write() {
        Ok(mut guard) => *guard = next,
        Err(poisoned) => *poisoned.into_inner() = next,
    }
    logging::log("[Models]", &format!("✅ 小浣熊模型目录已更新（{count} 个）"));
    // 默认规则种子（raccoon-* 内部模型默认禁用 / sn-* 默认加去前缀映射）。
    // 只对首次出现的 id 生效，用户的手动调整不会被这里覆盖。
    if let Some(summary) = model_rules::seed_raccoon_defaults(&ids) {
        logging::log("[Models]", &summary);
    }
    ModelRefreshOutcome::refreshed(count)
}

/// 解析 `/model_catalog` 响应：`{ code, data: { categories: [{ type,
/// default_model, models: [...] }] } }`（实测结构）。
///
/// 字段口径照抄源实现：根上没有 `data` 时把根当 `data`；平铺所有分类；
/// 同名模型**首次出现者胜**（`seen` 集合）；空 name 的条目丢弃。
pub(super) fn parse_catalog(payload: &Value) -> Vec<Value> {
    let data = match payload.get("data") {
        Some(Value::Object(_)) => payload.get("data").unwrap_or(payload),
        _ => payload,
    };
    let mut models: Vec<Value> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let Some(categories) = data.get("categories").and_then(Value::as_array) else {
        return models;
    };
    for category in categories {
        let Some(entries) = category.get("models").and_then(Value::as_array) else {
            continue;
        };
        for entry in entries {
            let Some(model) = normalize_catalog_entry(entry) else {
                continue;
            };
            let name = model
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if seen.iter().any(|known| known == &name) {
                continue;
            }
            seen.push(name);
            models.push(model);
        }
    }
    models
}

/// 单条目录记录归一（源实现 `normalizeCatalogEntry`）。
///
/// ── 三处易错点（照抄源实现，不做「顺手修正」）────────────────
///   ① `description` 在目录里是**内部代号**（`Raccoon-Work` 这类），营销文案在
///      `display_description`；`displayName` 优先取代号（≤64 字符时才用，
///      更长的多半是句子，用它当展示名会把列表撑爆）。
///   ② `params` 是嵌套对象，`context_window` / `max_tokens` 可能出现在
///      `params` 里、也可能在顶层（两种都实测过）—— 两级都要看。
///   ③ `supportThinking` 恒为 true（目录里没有对应字段，源实现就是这么写的）；
///      `supportImage` 由 tags 推断（vision / image / image-understanding）。
fn normalize_catalog_entry(entry: &Value) -> Option<Value> {
    let object = entry.as_object()?;
    let name = object
        .get("name")
        .map(text_value)
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .or_else(|| {
            object
                .get("model_name")
                .map(text_value)
                .map(|text| text.trim().to_string())
                .filter(|text| !text.is_empty())
        })?;
    let tags: Vec<Value> = object
        .get("tags")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str())
                .map(|text| text.trim().to_string())
                .filter(|text| !text.is_empty())
                .map(Value::String)
                .collect()
        })
        .unwrap_or_default();
    let tag_names: Vec<String> = tags
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    let params = object.get("params").and_then(Value::as_object);
    let pick_int = |keys: &[&str]| -> Value {
        for key in keys {
            if let Some(number) = object.get(*key).and_then(int_of) {
                return Value::from(number);
            }
            if let Some(number) = params
                .and_then(|params| params.get(*key))
                .and_then(int_of)
            {
                return Value::from(number);
            }
        }
        Value::Null
    };
    let codename = object
        .get("description")
        .map(text_value)
        .unwrap_or_default()
        .trim()
        .to_string();
    let display_name = object
        .get("display_name")
        .map(text_value)
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| {
            if !codename.is_empty() && codename.chars().count() <= 64 {
                codename.clone()
            } else {
                name.clone()
            }
        });
    let description = object
        .get("display_description")
        .map(text_value)
        .unwrap_or_default()
        .trim()
        .to_string();
    let support_image = ["vision", "image", "image-understanding"]
        .iter()
        .any(|wanted| tag_names.iter().any(|tag| tag == wanted));
    let mut normalized = serde_json::Map::new();
    normalized.insert("name".to_string(), Value::String(name));
    normalized.insert("displayName".to_string(), Value::String(display_name));
    normalized.insert("description".to_string(), Value::String(description));
    normalized.insert("tags".to_string(), Value::Array(tags));
    // 上下文窗口/输出上限用 `Value::Null` 表示「目录没给」——
    // 聚合出口的 `list_item` 对 null 的处理是**不输出该键**（与源实现的
    // `undefined` 一致），所以这里必须用 null 而不是 0
    normalized.insert("contextWindow".to_string(), pick_int(&["context_window", "contextWindow"]));
    normalized.insert("maxTokens".to_string(), pick_int(&["max_tokens", "maxTokens"]));
    normalized.insert("supportImage".to_string(), Value::Bool(support_image));
    // 源实现恒为 true（目录没有这个字段，客户端按「支持思考」渲染）
    normalized.insert("supportThinking".to_string(), Value::Bool(true));
    // 倍率：上游目录里的 `billing_multiplier`（基准/原价倍率）与
    // `billing_effective_multiplier`（计入活动与限免后的**实付**倍率）。
    //
    // ── 为什么不是 `points_multiplier` ──────────────────────────
    // 那个字段**已被上游废弃**：桌面端代码里保留着读取（`normalizeCatalogEntry`
    // 仍写 `pointsMultiplier: s(t.points_multiplier)`），但当前 `/model_catalog`
    // 的响应里一个都没有 —— 实测 8 个 chat 条目中 `points_multiplier` 出现 0 次、
    // `billing_multiplier` 出现 8 次。本文件早先从更早的实测快照移植，此后上游
    // 改了名而我们没跟上，于是透传出去的 `pointsMultiplier` 恒为 null。
    //
    // 三个字段的取向与桌面端界面一致：界面文案用「原 N 倍 / 现 N 倍」并列展示，
    // 我们这一列只有一格，取**实付**（`billing_effective_multiplier`）——
    // 用户真正会扣掉的那个数。实付缺失时回落基准值（老响应没有该字段）。
    //
    // 这两个数**不是积分功能**：网关不读、不判定、也不参与任何预算/账单计算，
    // 只是把上游目录里的展示信息带给前端（架构文档 §6）。
    let billing_base = number_of(object.get("billing_multiplier"));
    let billing_effective = number_of(object.get("billing_effective_multiplier"));
    let multiplier = billing_effective.or(billing_base);
    normalized.insert(
        "pointsMultiplier".to_string(),
        billing_base
            .map(crate::server::core::account_store::state::json_number)
            .unwrap_or(Value::Null),
    );
    // `credits` 是**聚合层与前端共用的那一列**（`list_item` 把它影子成
    // `credits`，前端 `formatCredits` 再转成 `0.2x` 展示）。空串 = 上游没给，
    // 界面显示 `—`。形态与 WorkBuddy 的 `"x0.16 credits"` 对齐，两家的
    // 倍率列因此走同一条渲染分支。
    normalized.insert(
        "credits".to_string(),
        Value::String(
            multiplier
                .map(|value| format!("x{} credits", format_multiplier(value)))
                .unwrap_or_default(),
        ),
    );
    normalized.insert(
        "billingStatus".to_string(),
        object
            .get("billing_status")
            .and_then(Value::as_str)
            .map(|text| Value::String(text.to_string()))
            .unwrap_or(Value::Null),
    );
    normalized.insert(
        "visible".to_string(),
        Value::Bool(!matches!(object.get("visible"), Some(Value::Bool(false)))),
    );
    Some(Value::Object(normalized))
}

/// JS `Math.floor(Number(x))`；非有限值返回 None（源实现 `toInt`）
fn int_of(value: &Value) -> Option<i64> {
    let number = match value {
        Value::Number(number) => number.as_f64()?,
        Value::String(text) => text.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    if number.is_finite() {
        Some(number.floor() as i64)
    } else {
        None
    }
}

/// 倍率字段读法：上游给的是 JSON 数字，但**字符串形态也接受**
/// （小浣熊的目录里 `context_window` 这类字段就混着字符串形态，
/// 而倍率一旦被上游改成 `"0.2"` 就会静默变成「没给」）。
///
/// `0` 是合法值（限免的 `billing_effective_multiplier = 0`），
/// 因此**不能**用 `filter(|v| v != 0.0)` 这种「假值即缺失」的写法。
fn number_of(value: Option<&Value>) -> Option<f64> {
    let number = match value? {
        Value::Number(number) => number.as_f64()?,
        Value::String(text) => text.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    if number.is_finite() && number >= 0.0 {
        Some(number)
    } else {
        None
    }
}

/// 倍率数字 → 紧凑文本：整数不带小数点，其余最多两位小数并裁掉尾零
/// （`0.2` → `0.2`、`0.75` → `0.75`、`1` → `1`、`0` → `0`）。
///
/// 桌面端界面的文案是「N 倍」（中文）用 `toFixed` 后裁零，这里同一取向；
/// 保留两位是因为目录里 `0.25` / `0.75` 这类值真实存在，一位小数会把
/// `0.25` 显示成 `0.3`。
fn format_multiplier(value: f64) -> String {
    let rounded = (value * 100.0).round() / 100.0;
    if (rounded - rounded.trunc()).abs() < 1e-9 {
        return format!("{}", rounded.trunc() as i64);
    }
    let text = format!("{rounded:.2}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// JS `String(x)`（name/description 这类字段的文本形态；null 给空串）
fn text_value(value: &Value) -> String {
    super::jwt::js_text(value)
}

/// 供排障：目录条数（`/health` 之类的探针会读；与 `list()` 的语义一致）
#[allow(dead_code)]
pub fn count() -> usize {
    list().len()
}

/// 目录请求超时的公开常量（适配器构造刷新参数时可能要用）
#[allow(dead_code)]
pub const REFRESH_TIMEOUT: Duration = Duration::from_millis(CATALOG_TIMEOUT_MS);
