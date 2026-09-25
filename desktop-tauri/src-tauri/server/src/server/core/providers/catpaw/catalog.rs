//! CatPaw 模型目录的**远程拉取**（`POST /api/agent/maas/model-types`）。
//!
//! ── 为什么现在能拉了（推翻先前的「上游没有目录接口」）─────────
//! 本家原先的结论是「上游不提供模型目录接口，`MODELS` 静态表是唯一事实来源」
//! （见 `models.rs` 模块头的原始叙述）。那个结论来自**看错了接口形态**：
//! 桌面端 `modelTypesService` 里那句
//! `T('/api/agent/maas/model-types', {tenant, scene, env})` 看着像 GET 的 query
//! 参数，实际 `tenant/scene/env` 是 **POST 的 JSON body**（`auth-*.js` 里
//! `async function ar(e,t,r)` 是 POST 专用封装，`JSON.stringify(t)` 进 body）。
//! 而它的 base 域名也不是 `catx.nocode.cn`（那是 Gateway 域名，只跑积分与设置），
//! 是**桌面端直连域名** `ai.catpaw.meituan.com` —— 与我们转发链路用的
//! `DEFAULT_BASE_URL` 同一个 host，凭证头也完全一致（`Cookie: X-Passport-Token`）。
//!
//! ── 静态表为什么还要留着 ──────────────────────────────────────
//! 远程目录是**锦上添花，不是替代**：
//!   - 目录拉取需要登录态（无凭证时上游 401），而转发在环境变量旁路
//!     （`CATPAW_COOKIE`）下也应当可用；
//!   - `MODELS` 表里的 `host_model_id` / `context_windows` 是**实测核对过**的
//!     上游数字 ID 与档位（`models.rs` 表头逐条记了证据），远程条目里
//!     上下文窗口藏在 `parameterDefinitions` 的 `context` 枚举里、
//!     思考档位藏在 `effort` 枚举里，形态比静态表**更难**直接消费。
//! 因此两者的分工是：远程目录负责「有哪些模型、叫什么、倍率多少」这类
//! 会变的信息；静态表继续负责「这个模型的上游数字 ID 与 context 档位」
//! 这类**必须实测坐实**的信息（数字 ID 写错会把请求打到另一个模型）。
//! `list()` 的输出以远程条目为准（id / 展示名 / 倍率 / 能力位），
//! 命中静态表时把它的数字 ID 与档位并进去。
//!
//! ── 缓存 ─────────────────────────────────────────────────────
//! 15 分钟（上游桌面端自己用 5 分钟；我们比客户端更频地打上游没有收益，
//! 而这一列信息变化极慢）。失败**保留上一份成功结果**，不清空 —— 与
//! AutoClaw / 小浣熊同一取向。

use std::sync::{OnceLock, RwLock};
use std::time::Duration;

use serde_json::{json, Value};

use crate::server::core::proxies::ResolvedProxy;
use crate::server::core::providers::catalog_cache;
use crate::server::logging;

use super::models::MODELS;
use super::{conversation::CatPawCredentials, upstream_http};

/// 目录路径（`modelTypesService-CPgwL2qX.js` 的路径常量 `_`）
const MODEL_TYPES_PATH: &str = "/api/agent/maas/model-types";

/// 请求体：`{tenant, scene, env}` —— 桌面端的调用参数逐字照搬
/// （`tenant = 'CatDesk'`、`scene = 'CATX_APP'`、`env = 'EXTERNAL'`）。
///
/// 三个值都是常量而不是配置：它们是**客户端身份**的一部分，客户端发错
/// 拿不到目录，我们单独改一个也没有语义。
fn request_body() -> Value {
    json!({
        "tenant": "CatDesk",
        "scene": "CATX_APP",
        "env": "EXTERNAL",
    })
}

/// 目录请求超时（目录是短响应；与 `upstream_http::REQUEST_TIMEOUT_MS` 同档）
const REQUEST_TIMEOUT_MS: u64 = 30_000;

/// 远程清单缓存有效期（毫秒）
const CACHE_TTL_MS: i64 = 15 * 60 * 1000;

/// 目录状态：远程清单 + 上次成功刷新时刻（0 = 从未成功过）
#[derive(Default, Clone)]
struct CatalogState {
    models: Vec<Value>,
    fetched_at: i64,
}

/// 进程级目录句柄。首次初始化时**先从持久化缓存恢复**（上次成功拉到的远程
/// 清单），没有再留空 —— 空状态的读取语义就是「回落到静态表」。
fn catalog() -> &'static RwLock<CatalogState> {
    static CATALOG: OnceLock<RwLock<CatalogState>> = OnceLock::new();
    CATALOG.get_or_init(|| RwLock::new(restored_state()))
}

/// 首次初始化读一次持久化缓存（见 `providers::catalog_cache` 的模块头）。
///
/// 缓存里存的就是 `CatalogState` 的形态（`refresh` 落地的那份），所以这里只做
/// 「搬回来」：不重新归一 —— 两处各写一份映射迟早分叉。
fn restored_state() -> CatalogState {
    match catalog_cache::load(catalog_cache::SCOPE_CATPAW) {
        Some(cached) => CatalogState { models: cached.models, fetched_at: cached.fetched_at },
        None => CatalogState::default(),
    }
}

fn read_state() -> CatalogState {
    match catalog().read() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

/// 远程清单是否已就绪（聚合目录判「这家现在有没有可用清单」时用；
/// 空 = 还没成功拉过，调用方回落到静态表）
pub fn remote_models() -> Vec<Value> {
    read_state().models
}

/// 上次成功刷新时刻（毫秒；0 = 从未成功）
pub fn last_refreshed_at() -> i64 {
    read_state().fetched_at
}

/// 按 id 或展示名找远程条目（大小写不敏感）。
///
/// ── 为什么要同时认展示名 ────────────────────────────────────
/// 聚合目录的 `providers_for_model` 是**先比 id、再比展示名**的两轮判定，
/// 所以「客户端传展示名」在目录层是**放行**的（既有客户端行为，不得退化）。
/// 若这里只比 id，同一个名字就会「目录层认识、转发层不认识」——
/// `/v1/models` 里列着、请求却报 400。两处必须同口径。
pub fn find_remote(id: &str) -> Option<Value> {
    let wanted = id.trim().to_lowercase();
    if wanted.is_empty() {
        return None;
    }
    read_state()
        .models
        .iter()
        .find(|item| {
            ["id", "name"].iter().any(|key| {
                item.get(*key)
                    .and_then(Value::as_str)
                    .map(|text| text.trim().to_lowercase() == wanted)
                    .unwrap_or(false)
            })
        })
        .cloned()
}

/// 当前清单里所有可转发的模型名（报错文案用）
pub fn known_ids() -> Vec<String> {
    list()
        .iter()
        .filter_map(|item| item.get("id").and_then(Value::as_str).map(str::to_string))
        .collect()
}

/// 把一条上游条目归一成**聚合层认的形态**。
///
/// 字段名对齐 `core/models/shape.rs::list_item` 读的那些键
/// （`id` / `name` / `maxInputTokens` / `maxOutputTokens` / `supportsImages` /
/// `supportsReasoning` / `credits` / `supportsToolCall`）。
///
/// ── 为什么过滤 `USER_CUSTOM` ─────────────────────────────────
/// 上游目录里除了官方模型还有用户自建的模型（`provider === "USER_CUSTOM"`）。
/// 那些模型依赖客户端本地的 apiKey 配置，网关拿到它们既没有凭证也不该转发 ——
/// 列出来会让客户端以为能请求。官方条目的 `provider` 是别的值（实测为空）。
///
/// ── 为什么丢弃 auto 伪模型 ───────────────────────────────────
/// `extendedInfo.isAuto = "true"` 的那条是**档位入口**而不是真实模型
/// （实测它没有 `rateMultiplier`、没有 `parameterDefinitions`），
/// 与 Qoder 的 `Auto` 不同 —— Qoder 的 Auto 是上游真认的 key，这里的 auto
/// 只是 UI 的「自动选择」。列进 `/v1/models` 会让客户端拿一个转不了发的名字。
fn normalize_entry(item: &Value) -> Option<Value> {
    let model_type_id = item.get("modelTypeId").and_then(Value::as_i64);
    let extended = item.get("extendedInfo");
    // auto 伪模型：`isAuto` 可能是布尔或字符串 `"true"`（上游两种都给过）
    if extended
        .and_then(|info| info.get("isAuto"))
        .map(js_truthy)
        .unwrap_or(false)
    {
        return None;
    }
    if item
        .get("provider")
        .and_then(Value::as_str)
        .map(|text| text.trim().eq_ignore_ascii_case("USER_CUSTOM"))
        .unwrap_or(false)
    {
        return None;
    }
    // 上游 id 取 `modelTypeName`（真实名字，如 `glm-5.3-flash`）——
    // `id` 是本地记录主键、`catPawModelType` 是数字的字符串形态，都不是名字
    let id = item
        .get("modelTypeName")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)?;
    // 展示名优先中文 caption，回落 `description`（实测是英文展示名）
    let name = extended
        .and_then(|info| info.get("modelCaptionZhCN"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .or_else(|| {
            item.get("description")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_else(|| id.clone());
    // 倍率：`extendedInfo.rateMultiplier`，**字符串**形态（`"0.94"` / `"0.00"`），
    // 尾零是上游刻意保留的展示形态，转成数字再格式化会丢（`"0.00"` 会变成 `0`）
    let credits = extended
        .and_then(|info| info.get("rateMultiplier"))
        .map(rate_text)
        .unwrap_or_default();
    // 思考能力：`supportThinking` 是条目顶层布尔。**缺省时看参数的佐证** ——
    // 目录里带 `effort` 枚举（low/high/max）的模型一定支持思考档位，
    // 拿它当兜底比直接判 false 更不容易错（把一个能思考的模型报成不能，
    // 会让客户端自己禁用思考相关的请求参数）。
    // 【推断】依据：本家 `models.rs` 的 `resolve_effort` 把 `effort` 映射到上游
    // `declarativeParams.effort`，而它只在思考链路上有意义。
    let supports_thinking = match item.get("supportThinking") {
        Some(value) => js_truthy(value),
        None => item
            .get("parameterDefinitions")
            .and_then(Value::as_array)
            .map(|definitions| definitions.iter().any(|definition| {
                definition.get("id").and_then(Value::as_str) == Some("effort")
            }))
            .unwrap_or(false),
    };
    let mut entry = json!({
        "id": id,
        "name": name,
        "supportsImages": item.get("supportImage").map(js_truthy).unwrap_or(false),
        "supportsReasoning": supports_thinking,
        "supportsToolCall": true,
        "kind": "chat",
    });
    let object = entry.as_object_mut()?;
    if let Some(model_type_id) = model_type_id {
        // 上游数字 modelType（转发必须用；远程条目自带，比静态表更权威）
        object.insert("modelType".to_string(), Value::from(model_type_id));
    }
    if !credits.is_empty() {
        object.insert("credits".to_string(), Value::String(credits));
    }
    // 上下文档位与思考档位：上游藏在 `parameterDefinitions` 的 ENUM 里。
    // 取档位**上限**作为 `maxInputTokens`（客户端按它估算截断），
    // 默认档位另存 —— 与静态表的两个字段语义一致
    if let Some(definitions) = item.get("parameterDefinitions").and_then(Value::as_array) {
        if let Some(max_window) = enum_max(definitions, "context") {
            object.insert("maxInputTokens".to_string(), Value::from(max_window));
        }
        if let Some(default_window) = enum_default(definitions, "context") {
            object.insert("defaultContextWindow".to_string(), Value::String(default_window));
        }
    }
    Some(entry)
}

/// `extendedInfo.rateMultiplier` → 倍率文本。
///
/// 上游给的是字符串形式的浮点（`"0.94"` / `"0.03"` / `"0.00"`），
/// **原样保留**：`"0.03"` 去掉尾零会变成 `0.03`（同样）但 `"0.00"` 会变成 `0`
/// —— 而 `0` 在界面上的语义是「免费」，与「上游没给」只差一个键的存在性。
/// 与 WorkBuddy 的 `"x0.16 credits"` 同形，前端同一条渲染分支。
fn rate_text(value: &Value) -> String {
    let plain = match value {
        Value::String(text) => text.trim().to_string(),
        Value::Number(number) => number.to_string(),
        _ => return String::new(),
    };
    if plain.is_empty() {
        return String::new();
    }
    if plain.parse::<f64>().is_err() {
        return String::new();
    }
    format!("x{plain} credits")
}

/// `parameterDefinitions[]` 里某个 ENUM 参数的档位上限（数值最大的一项）
fn enum_max(definitions: &[Value], id: &str) -> Option<i64> {
    let values = definitions
        .iter()
        .find(|item| item.get("id").and_then(Value::as_str) == Some(id))?
        .get("values")?
        .as_array()?;
    values
        .iter()
        .filter_map(|item| item.get("value"))
        .filter_map(|value| match value {
            Value::String(text) => text.trim().parse::<i64>().ok(),
            Value::Number(number) => number.as_i64(),
            _ => None,
        })
        .max()
}

/// `parameterDefinitions[]` 里某个 ENUM 参数的默认档位
fn enum_default(definitions: &[Value], id: &str) -> Option<String> {
    definitions
        .iter()
        .find(|item| item.get("id").and_then(Value::as_str) == Some(id))?
        .get("defaultValue")
        .and_then(Value::as_str)
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

/// 拉取并落地远程目录。
///
/// 三档返回值与 `ModelRefreshOutcome` 的契约一致（成功 / 没刷 / 失败了，
/// 见 `providers::adapter`）。`force = false` 时命中 TTL 直接早退（自动路径）；
/// 用户手动点刷新时调用方传 `true` 绕过。
///
/// 凭证 = `Cookie: X-Passport-Token`，与转发链路**同一套**
/// （`upstream_http::request_headers` 复用，不另起一份）。
pub async fn refresh(
    credentials: &CatPawCredentials,
    proxy: Option<&ResolvedProxy>,
    force: bool,
) -> crate::server::core::providers::adapter::ModelRefreshOutcome {
    use crate::server::core::providers::adapter::ModelRefreshOutcome;

    if !force {
        let state = read_state();
        if !state.models.is_empty()
            && state.fetched_at > 0
            && logging::now_ms() - state.fetched_at < CACHE_TTL_MS
        {
            return ModelRefreshOutcome::unchanged();
        }
    }
    if credentials.token.trim().is_empty() {
        // 没有登录态时上游必然 401，本家也拉不到目录。**不是错误**：
        // 脚本 / CI 用户走环境变量旁路用过 CatPaw 是常见情形，
        // 报一条红色失败会让他以为哪里坏了
        logging::verbose("[Models]", "CatPaw 模型目录刷新跳过：没有可用登录态");
        return ModelRefreshOutcome::unchanged();
    }

    let body = request_body();
    let outcome = upstream_http::post_json(
        &super::credentials::upstream_base_url(),
        MODEL_TYPES_PATH,
        credentials,
        proxy,
        &body,
        REQUEST_TIMEOUT_MS,
        // 目录刷新不是用户要看的「上游报文」：不采
        None,
    )
    .await;
    // `post_json` 已经过了 `unwrapApiData`：成功时返回的是**解包后的 data**
    // （模型数组），失败时是带状态码的 `CatPawError`
    let payload = match outcome {
        Ok(payload) => payload,
        Err(error) => {
            // 401 单独给一句可操作的文案：这是最可能的一种失败，
            // 而「上游 HTTP 401」对用户没有信息量
            if error.status == 401 {
                return ModelRefreshOutcome::failed(
                    "CatPaw 登录态已失效，请在账号页重新导入或重新登录",
                );
            }
            return ModelRefreshOutcome::failed(error.message);
        }
    };
    let Some(entries) = payload.as_array() else {
        return ModelRefreshOutcome::failed("上游返回的模型目录不是数组");
    };
    let models: Vec<Value> = entries.iter().filter_map(normalize_entry).collect();
    if models.is_empty() {
        return ModelRefreshOutcome::failed("上游返回的模型目录为空");
    }
    let count = models.len();
    let mut state = read_state();
    state.models = models;
    state.fetched_at = logging::now_ms();
    // 落持久化缓存（进程重启后由 `restored_state` 读回）。`state` 要被
    // `write` 消费，所以先存；**不在目录锁内** —— 缓存写入要拿库连接锁，
    // 两把锁不能嵌套。
    catalog_cache::save(catalog_cache::SCOPE_CATPAW, &state.models, state.fetched_at);
    match catalog().write() {
        Ok(mut guard) => *guard = state,
        Err(poisoned) => *poisoned.into_inner() = state,
    }
    logging::log("[Models]", &format!("✅ CatPaw 模型目录已更新（{count} 个）"));
    ModelRefreshOutcome::refreshed(count)
}

/// 清单：**远程优先**，命中静态表时把静态表里实测坐实的数字 ID 与档位并进去。
///
/// 远程拿不到时回落到静态表（`MODELS`），于是「离线 / 未登录」与「在线」
/// 两种状态下 `/v1/models` 都非空 —— 与其它三家的取向一致。
///
/// ── 最后一道闸：没有上游数字 ID 的条目一律不广告 ──────────────
/// CatPaw 建会话**必须先有数字 `modelType`**（协议要求，见 `models.rs`）。
/// 一条既没有远程 `modelTypeId`、静态表里也查不到的条目转发不了 ——
/// 把它列进 `/v1/models` 会造出「清单里有、请求却报 400」的自相矛盾。
/// 宁可少广告一个，也不要广告一个转不了的。
pub fn list() -> Vec<Value> {
    let remote = remote_models();
    if remote.is_empty() {
        return static_list();
    }
    remote
        .into_iter()
        .filter_map(|mut entry| {
            let id = entry
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if let Some(spec) = spec_of(&id) {
                if let Some(object) = entry.as_object_mut() {
                    // 静态表里的数字 ID 是**实测**过的，远程条目也带
                    // `modelTypeId`；两者一致时以远程为准（它更新），
                    // 但静态表命中说明这个模型我们确实能转发 —— 远程没给
                    // 数字时用静态表的补上
                    object
                        .entry("modelType".to_string())
                        .or_insert_with(|| Value::from(spec.host_model_id));
                    if !object.contains_key("maxInputTokens") {
                        if let Some(window) = spec
                            .context_windows
                            .iter()
                            .filter_map(|text| text.parse::<i64>().ok())
                            .max()
                        {
                            object.insert("maxInputTokens".to_string(), Value::from(window));
                        }
                    }
                    if !object.contains_key("name") {
                        object.insert("name".to_string(), Value::String(spec.name.to_string()));
                    }
                }
            }
            // 闸门：数字 ID 是转发的必要条件（远程或静态表，二者至少有一个）
            entry.get("modelType").and_then(Value::as_i64)?;
            Some(entry)
        })
        .collect()
}

/// 静态表形态的清单（远程不可用时的回落，也是 `list()` 的兜底）
fn static_list() -> Vec<Value> {
    MODELS
        .iter()
        .map(|model| {
            let mut entry = json!({
                "id": model.id,
                "name": model.name,
                "supportsImages": model.support_image,
                "supportsReasoning": model.support_thinking,
                "supportsToolCall": true,
                "kind": "chat",
                // 上游数字 modelType：转发必须用（客户端请求的模型名要反查它）
                "modelType": model.host_model_id,
            });
            if let Some(object) = entry.as_object_mut() {
                if let Some(window) = model
                    .context_windows
                    .iter()
                    .filter_map(|text| text.parse::<i64>().ok())
                    .max()
                {
                    object.insert("maxInputTokens".to_string(), Value::from(window));
                }
                if let Some(default) = model.default_context_window {
                    object.insert(
                        "defaultContextWindow".to_string(),
                        Value::String(default.to_string()),
                    );
                }
                // 静态表没有倍率（上游目录才有），键缺失 → 界面显示 `—`
            }
            entry
        })
        .collect()
}

/// 静态表里按 id 找条目（大小写/分隔符宽容，见 `models::find_model_entry`）
fn spec_of(id: &str) -> Option<&'static super::models::ModelSpec> {
    let wanted = normalize(id);
    MODELS.iter().find(|entry| {
        normalize(entry.id) == wanted || normalize(entry.name) == wanted
    })
}

/// 与 `models::normalize_model_name` 同口径（小写 + 空白/下划线/点 → 连字符）
fn normalize(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.trim().chars() {
        if ch.is_whitespace() || ch == '_' || ch == '.' {
            out.push('-');
        } else {
            out.push(ch.to_ascii_lowercase());
        }
    }
    out
}

/// JS 真值判定（`Boolean(x)`）：null/false/0/"" 为假，其余为真
/// （上游的 `isAuto` 两种形态都给过：布尔与字符串 `"true"`）
fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().map(|item| item != 0.0).unwrap_or(false),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// 供排障：远程目录条数
#[allow(dead_code)]
pub fn count() -> usize {
    remote_models().len()
}

/// 目录请求超时的公开常量（与其它三家的 `REFRESH_TIMEOUT` 同用途）
#[allow(dead_code)]
pub const REFRESH_TIMEOUT: Duration = Duration::from_millis(REQUEST_TIMEOUT_MS);
