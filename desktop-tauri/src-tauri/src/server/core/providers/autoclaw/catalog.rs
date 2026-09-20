//! AutoClaw 模型目录的**远程拉取**（`GET {CONFIG_BASE_URL}autoclaw-model-config`）。
//!
//! ── 接口为什么藏在「上一级」路径（先前找不到它的原因）──────────
//! 对话上游是 `{...}/autoclaw-proxy/proxy/autoclaw/chat/completions`，而模型
//! 配置在同 host 的 **`/proxy/` 这一级**：`.../autoclaw-proxy/proxy/autoclaw-model-config`
//! —— 桌面端用 `getConfigBaseUrl()` 取 `lastIndexOf("/proxy/")` 之后切片，
//! 把尾部的 `/autoclaw` 砍掉。顺着 `chat/completions` 找永远找不到它。
//!
//! ── 鉴权 ────────────────────────────────────────────────────
//! `authorization: Bearer <accessToken>` + 一套**可自行复算**的签名头
//! （`X-Auth-Appid` / `X-Auth-TimeStamp` / `X-Auth-Sign`，签名是
//! `md5(appId&秒级时间戳&appKey)`）。签名实现在 `refresh::signed_auth_headers`
//! —— 与本家的刷新接口共用同一份，不复制（复制会让 appId/appKey 与时间戳单位
//! 各自演进，而签名一旦分叉就是稳定 400002）。
//!
//! ── 响应形态（**与同批兄弟接口不同**）───────────────────────
//! 顶层直接是 `{"models":[...]}`，**没有 `{code, data}` 信封**
//! （同批的 `provider-config` / `promotion-config` 有信封，所以不能照抄）。
//!
//! ── 缓存 ────────────────────────────────────────────────────
//! 5 分钟（与桌面端自身对齐）。失败**保留上一份成功结果**不清空 ——
//! 桌面端也是这么做的（`RemoteConfig` 在 401/超时时保留既有目录），
//! 清空会让「网络抖了一下」变成「模型全没了」。

use std::sync::{OnceLock, RwLock};

use serde_json::{json, Value};

use crate::server::logging;

use super::credentials::AutoClawCredentials;

/// 配置基址：`{DEFAULT_UPSTREAM_BASE_URL}` 砍掉尾部的 `/autoclaw`，再补 `/proxy/`。
///
/// 逐字对应的桌面端实现（`/out/main/index.js`）：
/// ```js
/// const idx = ZAI_PROXY_BASE_URL.lastIndexOf("/proxy/");
/// return ZAI_PROXY_BASE_URL.slice(0, idx + "/proxy/".length);
/// ```
/// 取 `lastIndexOf` 而不是首次出现：域名里也可能出现 `/proxy/` 片段。
fn config_base_url() -> String {
    let upstream = super::credentials::upstream_base_url();
    match upstream.rfind("/proxy/") {
        Some(index) => format!("{}/", &upstream[..index + "/proxy/".len()]),
        // 没有 `/proxy/` 时按 `URL.origin + /autoclaw-proxy/proxy/` 拼
        // （桌面端同款兜底）。这里退化成「上游基址 + /」——
        // 上游基址被 `AUTOCLAW_UPSTREAM_BASE_URL` 覆盖成别的形态时，
        // 拼一个必定 404 的 URL 不如让调用方看到真实的请求地址
        None => format!("{}/", upstream.trim_end_matches('/')),
    }
}

/// 模型目录路径（`CONFIG_PATHS.MODEL_CONFIG`）
const MODEL_CONFIG_PATH: &str = "autoclaw-model-config";

/// 目录请求超时（毫秒）。桌面端给自己设的是 **5 秒**；网关放宽到 15 秒 ——
/// 桌面端失败只是晚一轮刷新，我们失败会让用户看到一条失败结果。
const REQUEST_TIMEOUT_MS: u64 = 15_000;

/// 远程清单缓存有效期（毫秒）。与桌面端自身的轮询周期对齐（prod 5 分钟）。
const CACHE_TTL_MS: i64 = 5 * 60 * 1000;

/// 目录状态：远程清单 + 上次成功刷新时刻（0 = 从未成功过）
#[derive(Default, Clone)]
struct CatalogState {
    models: Vec<Value>,
    fetched_at: i64,
}

fn catalog() -> &'static RwLock<CatalogState> {
    static CATALOG: OnceLock<RwLock<CatalogState>> = OnceLock::new();
    CATALOG.get_or_init(|| RwLock::new(CatalogState::default()))
}

fn read_state() -> CatalogState {
    match catalog().read() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

/// 远程清单（空 = 还没成功拉过，调用方回落到静态路由表）
pub fn remote_models() -> Vec<Value> {
    read_state().models
}

/// 上次成功刷新时刻（毫秒；0 = 从未成功）
pub fn last_refreshed_at() -> i64 {
    read_state().fetched_at
}

/// 按**目录口径**找一个模型对应的**完整路由 ID**（`X-Request-Model` 要发的那个）。
///
/// 三种写法都认（与 `providers_for_model` 的「先 id 后 name」同口径）：
///   1. 目录名 `glm-5.3`（`id`，客户端最常给的）；
///   2. 展示名 `GLM-5.3`（`name`）；
///   3. 完整路由 ID `zaicoding_glm-5.3`（`routeId`）。
///
/// ── 为什么不从 `id` 反推路由 ID ─────────────────────────────
/// `id` 是**剥过前缀的目录名**、`routeId` 才是完整路由 ID。剥前缀不可逆：
/// `auto` 无法知道自己是 `zai_auto` 还是 `zaicoding_auto`，而发错路由 ID
/// 等于换了一个模型（还可能走错计费通道）。所以这里读 `routeId` 字段本身。
pub fn remote_route_id(name: &str) -> Option<String> {
    let wanted = name.trim().to_lowercase();
    if wanted.is_empty() {
        return None;
    }
    read_state().models.iter().find_map(|item| {
        let route = item
            .get("routeId")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            // 兜底：老缓存里可能没有 `routeId`（那时 `id` 就是路由 ID）
            .or_else(|| item.get("id").and_then(Value::as_str).map(str::trim))?;
        let same = |key: &str| {
            item.get(key)
                .and_then(Value::as_str)
                .map(|text| text.trim().to_lowercase() == wanted)
                .unwrap_or(false)
        };
        if same("id") || same("name") || route.to_lowercase() == wanted {
            Some(route.to_string())
        } else {
            None
        }
    })
}

/// 把一条上游条目归一成**聚合层认的形态**（键名对齐
/// `core/models/shape.rs::list_item`）。
///
/// ── 关于两条特殊规则 ─────────────────────────────────────────
/// 1. **工具调用**：上游目录里**没有**这个能力位（桌面端也不从它读），
///    但 AutoClaw 的上游是 OpenAI 兼容网关，工具调用确实可用。显式给 true：
///    聚合层的 `supports_tool_call` 在键缺失时输出 false，会让客户端以为
///    这批模型不能调工具（与 `models::list()` 对静态表的取舍一致）。
/// 2. **图像能力**：上游给 `input: ["text","image",...]`，我们只认 `image`
///    这一项（`video` / `audio` 会被上游自身的归一化丢掉）。
fn normalize_entry(item: &Value) -> Option<Value> {
    // 条目可能是裸字符串（上游允许 `models: ["zai_auto"]` 这种简写）。
    // 与对象形态同口径：`id` 是**剥前缀后的目录名**（客户端用这个名字请求），
    // 完整路由 ID 进 `routeId`（转发要发 `X-Request-Model`）
    if let Some(raw) = item.as_str() {
        let route_id = raw.trim();
        if route_id.is_empty() {
            return None;
        }
        let id = super::models::strip_route_prefix(route_id).to_string();
        return Some(json!({
            "id": id,
            "name": id.to_uppercase(),
            "routeId": route_id,
            "supportsToolCall": true,
            "kind": "chat",
        }));
    }
    let route_id = item
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())?
        .to_string();
    // 目录 id = 剥前缀后的名字（客户端用这个名字请求）
    let id = super::models::strip_route_prefix(&route_id).to_string();
    let name = item
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| id.to_uppercase());
    // 图像能力：`input` 数组里有没有 `image`
    let support_image = item
        .get("input")
        .and_then(Value::as_array)
        .map(|items| items.iter().any(|value| value.as_str() == Some("image")))
        .unwrap_or(false);
    // 思考能力：`reasoning` 缺省为 true（上游缺省即真，桌面端同款回退）
    let reasoning = item
        .get("reasoning")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let mut entry = json!({
        "id": id,
        "name": name,
        "routeId": route_id,
        "supportsImages": support_image,
        "supportsReasoning": reasoning,
        "supportsToolCall": true,
        "kind": "chat",
    });
    let object = entry.as_object_mut()?;
    if let Some(window) = item.get("contextWindow").and_then(Value::as_i64) {
        object.insert("maxInputTokens".to_string(), Value::from(window));
    }
    if let Some(max_tokens) = item.get("maxTokens").and_then(Value::as_i64) {
        object.insert("maxOutputTokens".to_string(), Value::from(max_tokens));
    }
    // ── 倍率：`creditConsumptionLevel` ──────────────────────────
    // 这是**档位文案而不是数值**（实测取值 `低` / `中` / `高`），
    // 官方术语是「积分消耗等级」。上游与桌面端都**没有**「档位 → 数字」的
    // 映射表（已穷举确认），所以这里原样透传字符串，不换算成 `x?.?? credits`
    // —— 那会是一个编造的数。前端那一列对非 `x…` 形态的文本会原样显示
    // （`formatCredits` 的正则匹配不上就回显原文），于是界面上看到的是
    // 「低 / 中 / 高」，与 AutoClaw 自家 UI 一致。
    if let Some(level) = item
        .get("creditConsumptionLevel")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        object.insert("credits".to_string(), Value::String(level.to_string()));
    }
    Some(entry)
}

/// 拉取并落地远程目录。
///
/// 三档返回值与 `ModelRefreshOutcome` 的契约一致（成功 / 没刷 / 失败了）。
/// `force = false` 时命中 TTL 直接早退（自动路径）；用户手动点刷新时传 `true`。
pub async fn refresh(
    credentials: &AutoClawCredentials,
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
        // 无 token 时上游必然 401（桌面端离线期间也真实抓到过 401）。
        // 「没刷」而不是「失败」：不用 AutoClaw 的用户点刷新时不该看到红色错误
        logging::verbose("[Models]", "AutoClaw 模型目录刷新跳过：没有可用登录态");
        return ModelRefreshOutcome::unchanged();
    }

    let url = format!("{}{MODEL_CONFIG_PATH}", config_base_url());
    let headers = super::refresh::signed_auth_headers(&credentials.token);
    let outcome = crate::server::core::auth_http::send_raw(
        "GET",
        &url,
        None,
        &headers,
        // 鉴权域与 LLM 域是两个站点：不套账号级出网代理
        // （与本家刷新 / 余额查询的既有取舍一致）
        None,
        Some(REQUEST_TIMEOUT_MS),
    )
    .await;
    let payload = match outcome {
        Ok(response) => {
            if !response.ok {
                if response.status == 401 {
                    return ModelRefreshOutcome::failed(
                        "AutoClaw 登录态已失效，请重新登录或刷新凭证",
                    );
                }
                return ModelRefreshOutcome::failed(format!(
                    "上游返回 HTTP {}",
                    response.status
                ));
            }
            response.payload.unwrap_or(Value::Null)
        }
        Err(error) => {
            return ModelRefreshOutcome::failed(
                crate::server::core::egress::describe_error_detail(&error),
            );
        }
    };
    // **没有 `{code, data}` 信封**：顶层就是 `{models: [...]}`
    let Some(entries) = payload.get("models").and_then(Value::as_array) else {
        return ModelRefreshOutcome::failed("上游返回的模型目录缺少 models 数组");
    };
    let models: Vec<Value> = entries.iter().filter_map(normalize_entry).collect();
    if models.is_empty() {
        return ModelRefreshOutcome::failed("上游返回的模型目录为空");
    }
    let count = models.len();
    let mut state = read_state();
    state.models = models;
    state.fetched_at = logging::now_ms();
    match catalog().write() {
        Ok(mut guard) => *guard = state,
        Err(poisoned) => *poisoned.into_inner() = state,
    }
    logging::log("[Models]", &format!("✅ AutoClaw 模型目录已更新（{count} 个）"));
    ModelRefreshOutcome::refreshed(count)
}

/// 供排障：远程目录条数
#[allow(dead_code)]
pub fn count() -> usize {
    remote_models().len()
}
