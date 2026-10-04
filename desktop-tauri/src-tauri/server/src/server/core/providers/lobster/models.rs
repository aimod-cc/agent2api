//! LobsterAI 模型清单（feat/lobster-provider PR-2；移植来源 参考实现 的
//! `_dynamic_models` / `FALLBACK_MODELS`，按本仓 `raccoon::models` 的形态重写）。
//!
//! ── 清单从哪来（三条来源，优先级即回退顺序）──────────────────
//!   1. **远程刷新**：`GET {server}/api/proxy/v1/models`（OpenAI 兼容形态，
//!      带 Bearer）。拉到非空清单即落地（10 分钟 TTL + 持久化缓存）。
//!   2. **客户端同步的目录**：`~/Library/Application Support/LobsterAI/
//!      openclaw/state/openclaw.json` 的 `models.providers["lobsterai-server"]`
//!      —— App 同步给内嵌 OpenClaw 网关的目录（26 款上下，带 contextWindow /
//!      input 模态），字段最全。mtime 跟随 App 的同步，读它不需要网络。
//!   3. **静态兜底**：参考实现 固化的两条性价比款（App 未装 / 两条来源都取不到
//!      时目录仍然非空，路由与 `/v1/models` 不至于开天窗）。
//!
//! ── 模型名前缀（与 参考实现 的 `lobster_` 前缀同一目的，形态改用本仓的
//!    连字符风格）────────────────────────────────────────────
//! LobsterAI 的裸模型名（`glm-5.3-flash` 等）与本网关其它上游的目录大量撞名，
//! 因此对外一律加 `lobster-` 前缀暴露；`build_chat_request` 发送前剥掉。
//! 上游上新模型时目录刷新即可见，零代码适配。
//!
//! ── 条目形态 ────────────────────────────────────────────────
//! 聚合层（`models::list_item`）认 `id` / `name` / `maxInputTokens` /
//! `supportsImages` 这些键；openclaw.json 给的是 `id` / `name` / `input[]` /
//! `contextWindow`，这里做一次键名映射（与 raccoon 的 `listing_entry` 同一做法），
//! 不提前翻译成 OpenAI 条目 —— 翻译只发生在聚合出口。
//!
//! ── panic=abort ────────────────────────────────────────────
//! 零 unwrap/expect/panic：解析失败一律按「这一条不可用」跳过。

use std::sync::{OnceLock, RwLock};
use std::time::Duration;

use serde_json::{json, Value};

use crate::server::core::providers::adapter::ModelRefreshOutcome;
use crate::server::core::providers::catalog_cache;
use crate::server::logging;

/// 对外暴露的模型名前缀（发送侧 `strip_model_prefix` 剥掉）
pub const MODEL_PREFIX: &str = "lobster-";

/// 远程目录缓存有效期（与 raccoon 同值：客户端高频拉 `/v1/models`，
/// 不加 TTL 会每次都真打上游）
const CATALOG_CACHE_TTL_MS: i64 = 10 * 60_000;

/// 目录请求超时
const CATALOG_TIMEOUT_MS: u64 = 10_000;

/// openclaw.json 的路径（App 同步给内嵌网关的模型目录）
fn openclaw_state_path() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(|home| {
        std::path::PathBuf::from(home)
            .join("Library/Application Support/LobsterAI/openclaw/state/openclaw.json")
    })
}

/// 静态兜底清单（参考实现 `FALLBACK_MODELS` 固化的两条；name / 图像支持 /
/// 上下文窗口是 2026-08-30 自 openclaw.json 固化的快照）。
///
/// `maxOutputTokens` 刻意不给（上游未提供，编一个不如显示 `—`）。
fn fallback_models() -> Vec<Value> {
    vec![
        entry("glm-5.3-flash", "GLM-5.3-Flash", true, Some(1_000_000)),
        entry("deepseek-v4-flash", "DeepSeek-V4-Flash", false, Some(1_000_000)),
    ]
}

/// 兜底条目（已是聚合层键名形态）
fn entry(model_id: &str, name: &str, supports_images: bool, max_input: Option<i64>) -> Value {
    let mut object = serde_json::Map::new();
    object.insert("id".to_string(), json!(format!("{MODEL_PREFIX}{model_id}")));
    object.insert("name".to_string(), Value::String(name.to_string()));
    if let Some(max_input) = max_input {
        object.insert("maxInputTokens".to_string(), Value::from(max_input));
    }
    object.insert("supportsImages".to_string(), Value::Bool(supports_images));
    Value::Object(object)
}

/// 目录的内部状态：**已落地**的清单（远程刷新的产物）+ 刷新元信息。
///
/// 远程从未成功过时 `models` 为空，`list()` 回落到 openclaw.json / 静态兜底。
#[derive(Default, Clone)]
struct CatalogState {
    models: Vec<Value>,
    fetched_at: i64,
}

fn catalog() -> &'static RwLock<CatalogState> {
    static CATALOG: OnceLock<RwLock<CatalogState>> = OnceLock::new();
    CATALOG.get_or_init(|| RwLock::new(restored_state()))
}

fn restored_state() -> CatalogState {
    match catalog_cache::load(catalog_cache::SCOPE_LOBSTER) {
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

// ─── openclaw.json（来源 2，本地文件）──────────────────────

/// openclaw.json 的解析结果缓存（mtime 跟随：App 同步后自动失效）
fn openclaw_cache(
) -> &'static RwLock<Option<(i64, Vec<Value>)>> {
    static CACHE: OnceLock<RwLock<Option<(i64, Vec<Value>)>>> = OnceLock::new();
    CACHE.get_or_init(|| RwLock::new(None))
}

/// 解析 openclaw.json 的 lobsterai-server 目录；不可读/无模型返回 None。
fn parse_openclaw_models() -> Option<Vec<Value>> {
    parse_openclaw_models_from(&openclaw_state_path()?)
}

/// 从指定路径解析 lobsterai-server 目录（测试从临时文件喂入用）
fn parse_openclaw_models_from(path: &std::path::Path) -> Option<Vec<Value>> {
    let text = std::fs::read_to_string(path).ok()?;
    let config: Value = serde_json::from_str(&text).ok()?;
    let models = config
        .get("models")?
        .get("providers")?
        .get("lobsterai-server")?
        .get("models")?
        .as_array()?;
    let mut out: Vec<Value> = Vec::new();
    for model in models {
        let id = model.get("id").and_then(Value::as_str).unwrap_or("").trim();
        if id.is_empty() {
            continue;
        }
        let name = model
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .unwrap_or(id);
        let supports_images = model
            .get("input")
            .and_then(Value::as_array)
            .map(|items| items.iter().any(|item| item.as_str() == Some("image")))
            .unwrap_or(false);
        let max_input = model
            .get("contextWindow")
            .and_then(Value::as_i64)
            .filter(|value| *value > 0);
        out.push(entry(id, name, supports_images, max_input));
    }
    (!out.is_empty()).then_some(out)
}

/// 读 openclaw 目录（mtime 缓存；比静态兜底优先）
fn openclaw_models() -> Vec<Value> {
    let Some(path) = openclaw_state_path() else {
        return Vec::new();
    };
    let mtime = std::fs::symlink_metadata(&path)
        .ok()
        .and_then(|meta| meta.modified().ok())
        .and_then(|time| time.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(-1);
    // 缓存命中：mtime 没变
    if let Ok(guard) = openclaw_cache().read() {
        if let Some((cached_mtime, models)) = guard.as_ref() {
            if *cached_mtime == mtime && !models.is_empty() {
                return models.clone();
            }
        }
    }
    let parsed = parse_openclaw_models().unwrap_or_default();
    if !parsed.is_empty() {
        if let Ok(mut guard) = openclaw_cache().write() {
            *guard = Some((mtime, parsed.clone()));
        }
    }
    parsed
}

// ─── 对外清单 ───────────────────────────────────────────────

/// 当前清单：远程成功过用远程的，否则 openclaw.json，最后静态兜底。
pub fn list() -> Vec<Value> {
    let state = read_state();
    if !state.models.is_empty() {
        return state.models.clone();
    }
    let local = openclaw_models();
    if !local.is_empty() {
        return local;
    }
    fallback_models()
}

/// 是否已落地过远程/openclaw 之外的**缓存**清单（`meta.source` 用）
pub fn remote_refreshed() -> bool {
    !read_state().models.is_empty()
}

/// 最后一次成功刷新远程目录的时间（毫秒；从未成功过为 0）
pub fn last_refreshed_at() -> i64 {
    read_state().fetched_at
}

/// 剥掉对外前缀，还原上游裸模型 id（发送侧用）。不带前缀的名字原样返回
/// （未加前缀点名的请求按本网关惯例在入口就被模型校验拦下，这里只是防御）。
pub fn strip_model_prefix(model: &str) -> String {
    model.strip_prefix(MODEL_PREFIX).unwrap_or(model).to_string()
}

// ─── 刷新 ───────────────────────────────────────────────────

/// 刷新目录：远程 `/api/proxy/v1/models` 为主，openclaw.json 为本地回落。
///
/// `force = true`（用户手动点「刷新模型清单」）跳过 TTL；`token` 为空时
/// 不打远程（目录接口需要登录态），直接走 openclaw.json 那条本地路。
/// 失败保留现有清单不清空（与 raccoon 同一语义）。
pub async fn refresh(token: &str, force: bool) -> ModelRefreshOutcome {
    if !force {
        let state = read_state();
        if !state.models.is_empty()
            && logging::now_ms() - state.fetched_at < CATALOG_CACHE_TTL_MS
        {
            return ModelRefreshOutcome::unchanged();
        }
    }
    // 来源 1：远程目录（OpenAI 兼容 {data:[...]}）
    if !token.is_empty() {
        let url = format!("{}/api/proxy/v1/models", super::DEFAULT_LLM_BASE_URL);
        let headers = vec![
            ("Accept".to_string(), "application/json".to_string()),
            ("Authorization".to_string(), format!("Bearer {token}")),
        ];
        match crate::server::core::auth_http::send_raw(
            "GET",
            &url,
            None,
            &headers,
            None,
            Some(CATALOG_TIMEOUT_MS),
        )
        .await
        {
            Ok(response) if response.ok => {
                let payload = response.payload.unwrap_or(Value::Null);
                let models = parse_remote_models(&payload);
                if !models.is_empty() {
                    return land(models);
                }
                // 200 但解析不出模型：当作「远程没给」，走下面的本地路
            }
            Ok(response) => {
                logging::verbose(
                    "[Models]",
                    &format!("LobsterAI 远程目录拉取失败: HTTP {}", response.status),
                );
            }
            Err(error) => {
                logging::verbose(
                    "[Models]",
                    &format!(
                        "LobsterAI 远程目录拉取失败: {}",
                        crate::server::core::egress::describe_error_detail(&error)
                    ),
                );
            }
        }
    }
    // 来源 2：App 同步的 openclaw.json（本地文件，字段最全）
    let local = openclaw_models();
    if !local.is_empty() {
        return land(local);
    }
    ModelRefreshOutcome::failed("LobsterAI 模型目录不可用（远程未返回且本机没有 App 目录）")
}

/// 解析 `/api/proxy/v1/models` 响应：`{data: [{id, …}]}`（OpenAI 兼容）。
/// 条目只有 id 时 name 用 id 原值，上下文/模态字段缺就不给。
fn parse_remote_models(payload: &Value) -> Vec<Value> {
    let data = payload
        .get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for item in data {
        let Some(id) = item.get("id").and_then(Value::as_str).map(str::trim) else {
            continue;
        };
        if id.is_empty() {
            continue;
        }
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .unwrap_or(id);
        let max_input = item
            .get("context_window")
            .or_else(|| item.get("contextWindow"))
            .or_else(|| item.get("max_input_tokens"))
            .and_then(Value::as_i64)
            .filter(|value| *value > 0);
        let supports_images = item
            .get("input")
            .and_then(Value::as_array)
            .map(|items| items.iter().any(|v| v.as_str() == Some("image")))
            .unwrap_or(false);
        out.push(entry(id, name, supports_images, max_input));
    }
    out
}

/// 落地一份新清单（持久化缓存 + 内存状态 + 日志）
fn land(models: Vec<Value>) -> ModelRefreshOutcome {
    let count = models.len();
    let next = CatalogState { models, fetched_at: logging::now_ms() };
    catalog_cache::save(catalog_cache::SCOPE_LOBSTER, &next.models, next.fetched_at);
    match catalog().write() {
        Ok(mut guard) => *guard = next,
        Err(poisoned) => *poisoned.into_inner() = next,
    }
    logging::log("[Models]", &format!("✅ LobsterAI 模型目录已更新（{count} 个）"));
    ModelRefreshOutcome::refreshed(count)
}

/// 目录请求超时的公开常量（排障用）
#[allow(dead_code)]
pub const REFRESH_TIMEOUT: Duration = Duration::from_millis(CATALOG_TIMEOUT_MS);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_is_stripped_and_bare_names_pass_through() {
        assert_eq!(strip_model_prefix("lobster-glm-5.3-flash"), "glm-5.3-flash");
        // 未加前缀的名字原样返回（入口的模型校验会拦下它们，这里只是防御）
        assert_eq!(strip_model_prefix("glm-5.3-flash"), "glm-5.3-flash");
        assert_eq!(strip_model_prefix(""), "");
    }

    #[test]
    fn remote_models_are_prefixed_and_mapped() {
        let payload = serde_json::json!({
            "data": [
                { "id": "glm-5.3-flash", "name": "GLM-5.3-Flash", "contextWindow": 1_000_000,
                  "input": ["text", "image"] },
                { "id": "deepseek-v4-flash" },
                { "id": "" },
                { "no-id": true },
            ]
        });
        let models = parse_remote_models(&payload);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].get("id").and_then(Value::as_str), Some("lobster-glm-5.3-flash"));
        assert_eq!(
            models[0].get("supportsImages").and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            models[0].get("maxInputTokens").and_then(Value::as_i64),
            Some(1_000_000)
        );
        // 没有 name 的条目用 id 原值；没有 input 的条目按不支持图像
        assert_eq!(models[1].get("name").and_then(Value::as_str), Some("deepseek-v4-flash"));
        assert_eq!(models[1].get("supportsImages").and_then(Value::as_bool), Some(false));
    }

    #[test]
    fn openclaw_entries_map_input_modalities() {
        let text = serde_json::json!({
            "models": { "providers": { "lobsterai-server": { "models": [
                { "id": "glm-5.3-flash", "name": "GLM-5.3-Flash",
                  "input": ["text", "image"], "contextWindow": 1_000_000 },
                { "id": "deepseek-v4-flash", "name": "", "input": ["text"] },
            ] } } }
        })
        .to_string();
        let path = std::env::temp_dir().join(format!(
            "lobster-openclaw-test-{}-{}.json",
            std::process::id(),
            line!()
        ));
        std::fs::write(&path, &text).ok();
        let parsed = parse_openclaw_models_from(&path);
        let _ = std::fs::remove_file(&path);
        assert_eq!(parsed.as_ref().map(|items| items.len()), Some(2));
        let items = parsed.unwrap_or_default();
        assert_eq!(
            items[0].get("id").and_then(Value::as_str),
            Some("lobster-glm-5.3-flash")
        );
        // name 为空的条目回落 id 原值
        assert_eq!(
            items[1].get("name").and_then(Value::as_str),
            Some("deepseek-v4-flash")
        );
        assert_eq!(items[1].get("maxInputTokens"), None);
    }
}
