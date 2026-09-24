//! Accio 模型目录：静态兜底 + `/api/llm/config/v2` 远程刷新。
//!
//! ── 清单从哪来 ──────────────────────────────────────────────
//!   1. **静态兜底**：`FALLBACK`（11 个可见模型，抄自客户端内置的
//!      `@ali/accio-adk-ts/model-catalog.json`，2026-09 快照）。进程启动即可用，
//!      没有账号 / 网络不通时 `/v1/models` 仍有内容。
//!   2. **远程刷新**：`POST {gw}/api/llm/config/v2`，body `{token, supportAutoModel}`，
//!      头带 `x-package-region`（GLOBAL / CN）。上游按地区与账号权限给清单，
//!      并支持 `If-None-Match`（我们不用 304 缓存：本网关自己按 TTL 早退，
//!      少一条「快照与 etag 对不上」的状态）。
//!
//! ── 条目的键名口径（与另外几家对齐）──────────────────────────
//! 聚合层的 `models::list_item` 认 `id` / `name` / `maxInputTokens` /
//! `maxOutputTokens` / `supportsImages` / `supportsReasoning` / `supportsToolCall` /
//! `isDefault` / `kind`（见 `core::models::shape`）。上游给的是
//! `modelCode` / `modelDisplayName` / `contextWindow` / `multimodal` /
//! `reasoningEfforts` / `isDefault`，本模块**在归一函数里一次翻译到位**，
//! 让 `list()` 的返回值直接就是聚合层认识的形态（并保留 `upstreamKey`
//! 与 `reasoningEfforts` 两个自有键 —— `list_item` 只挑它认识的键，
//! 这两个不会被带进 `/v1/models`，但路由与思考档位要用）。
//!
//! ── 两个地区各自的清单（与 Qoder 同一处置）────────────────────
//! GLOBAL 与 CN 的目录**可能不同**（上游按 `x-package-region` 过滤），
//! 因此缓存按地区分开；`list()` 返回并集（按 id 去重，GLOBAL 优先）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：零 unwrap/expect/panic；持锁期间绝不做网络请求。

use std::sync::{OnceLock, RwLock};

use serde_json::{json, Value};

use crate::server::core::providers::adapter::ModelRefreshOutcome;
use crate::server::logging;

use super::auth;
use super::credentials::Credentials;
use super::endpoints::{self, Region};

/// 远程目录缓存有效期（与 Qoder 的 1 小时同量级：目录变化不频繁，
/// 但用户点「刷新模型」时必须能真打上游 —— `force` 会绕过这里）
const CACHE_TTL_MS: i64 = 60 * 60 * 1000;

/// 静态兜底：客户端内置的 `model-catalog.json` 里 `visible: true` 的条目。
///
/// 字段名已经是**归一后的形态**（见模块头），`upstreamKey` 是发给上游的
/// `model` 字段值（就是 `modelCode` —— Accio 不做通道前缀那一套）。
const FALLBACK: &[(&str, &str, u64, bool, &[&str], bool)] = &[
    // (modelCode, 展示名, contextWindow, multimodal, reasoningEfforts, isDefault)
    ("gemini-3-flash-preview", "Gemini 3 Flash", 1_000_000, true, &["low", "high"], true),
    ("gemini-3.1-pro-preview", "Gemini 3.1 Pro", 1_000_000, true, &["low", "high"], false),
    ("qwen3.6-plus", "Qwen 3.6 Plus", 991_808, false, &[], false),
    ("qwen3-max-2026-01-23", "Qwen 3 Max", 262_144, false, &[], false),
    ("gpt-5.4", "GPT 5.4", 1_050_000, true, &["low", "high"], false),
    ("gpt-5.2-1211", "GPT 5.2", 400_000, true, &["low", "high"], false),
    ("claude-sonnet-4-6", "Claude Sonnet 4.6", 1_000_000, true, &["low", "medium", "high", "max"], false),
    ("claude-opus-4-6", "Claude Opus 4.6", 1_000_000, true, &["low", "medium", "high", "max"], false),
    ("kimi-k2.5", "Kimi K2.5", 256_000, true, &[], false),
    ("glm-5", "GLM-5", 200_000, false, &[], false),
    ("MiniMax-M2.5", "MiniMax M2.5", 204_800, true, &[], false),
];

/// 一个地区的远程清单 + 拉取时刻
#[derive(Clone, Default)]
struct RegionCache {
    models: Vec<Value>,
    fetched_at: i64,
}

fn cache_slot(region: Region) -> &'static RwLock<RegionCache> {
    static SLOTS: OnceLock<[RwLock<RegionCache>; 2]> = OnceLock::new();
    let slots = SLOTS.get_or_init(|| [RwLock::new(RegionCache::default()), RwLock::new(RegionCache::default())]);
    let index = match region {
        Region::Global => 0,
        Region::Cn => 1,
    };
    &slots[index]
}

fn fallback_models() -> Vec<Value> {
    FALLBACK
        .iter()
        .map(|(code, name, window, multimodal, efforts, is_default)| {
            normalize_entry(&json!({
                "modelCode": code,
                "modelDisplayName": name,
                "contextWindow": window,
                "multimodal": multimodal,
                "reasoningEfforts": efforts,
                "isDefault": is_default,
            }))
        })
        .collect()
}

/// 远程条目的归一（上游字段 → 聚合层认识的键名）。
fn normalize_entry(entry: &Value) -> Value {
    let code = entry
        .get("modelCode")
        .or_else(|| entry.get("modelName"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let display = entry
        .get("modelDisplayName")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(&code)
        .to_string();
    let context_window = entry
        .get("contextWindow")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let multimodal = entry.get("multimodal").and_then(Value::as_bool).unwrap_or(false);
    let efforts: Vec<Value> = entry
        .get("reasoningEfforts")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).map(|text| Value::String(text.to_string())).collect())
        .unwrap_or_default();
    json!({
        "id": code,
        "name": display,
        "maxInputTokens": context_window,
        // 上游没给输出上限；与 contextWindow 同量级地给一个保守值会让界面误导，
        // 因此留空（`list_item` 在缺键时不输出这个字段）
        "supportsImages": multimodal,
        "supportsReasoning": !efforts.is_empty(),
        // ADK 信封支持 functionCall / functionResponse，工具调用一律可用
        "supportsToolCall": true,
        "isDefault": entry.get("isDefault").and_then(Value::as_bool).unwrap_or(false),
        "kind": "chat",
        // 自有键（进不了 /v1/models，路由与思考档位要用）
        "upstreamKey": code,
        "reasoningEfforts": Value::Array(efforts),
        "modelDesc": entry.get("modelDesc").cloned().unwrap_or(Value::Null),
        "usageMultiple": entry.get("usageMultiple").cloned().unwrap_or(Value::Null),
    })
}

/// 当前清单：远程（有的话）优先，按 id 去重后拼接兜底。
fn region_models(region: Region) -> Vec<Value> {
    let cached = {
        let guard = cache_slot(region).read().unwrap_or_else(|error| error.into_inner());
        guard.models.clone()
    };
    if !cached.is_empty() {
        return cached;
    }
    fallback_models()
}

/// 全部地区的并集（GLOBAL 优先；同 id 去重）。聚合目录与 `resolve` 都用它。
pub fn list() -> Vec<Value> {
    let mut merged: Vec<Value> = Vec::new();
    for region in Region::ALL {
        for item in region_models(region) {
            let id = item.get("id").and_then(Value::as_str).unwrap_or("");
            if id.is_empty() {
                continue;
            }
            if merged.iter().any(|existing| {
                existing.get("id").and_then(Value::as_str).map(str::to_ascii_lowercase)
                    == Some(id.to_ascii_lowercase())
            }) {
                continue;
            }
            merged.push(item);
        }
    }
    merged
}

/// 某地区最近一次远程刷新是否成功过（模型管理页「来源」列用）
pub fn remote_models(region: Region) -> Vec<Value> {
    let guard = cache_slot(region).read().unwrap_or_else(|error| error.into_inner());
    guard.models.clone()
}

/// 某地区最近一次刷新的时刻（毫秒；0 = 从未）
pub fn last_refreshed_at(region: Region) -> i64 {
    let guard = cache_slot(region).read().unwrap_or_else(|error| error.into_inner());
    guard.fetched_at
}

/// 客户端点名的模型 → 上游发送名。
///
/// 与聚合目录同一口径（先 `id` 再 `name`，trim + 忽略大小写），否则会出现
/// 「目录放行、转发层认不出」的静默失配（与 AutoClaw 那次同一个坑）。
pub fn resolve(model_name: &str, region: Region) -> Option<Value> {
    let wanted = model_name.trim();
    if wanted.is_empty() {
        return None;
    }
    let candidates = {
        let own = region_models(region);
        let mut merged = own;
        for item in list() {
            let id = item.get("id").and_then(Value::as_str).unwrap_or("");
            if !merged.iter().any(|existing| {
                existing.get("id").and_then(Value::as_str).map(str::to_ascii_lowercase)
                    == Some(id.to_ascii_lowercase())
            }) {
                merged.push(item);
            }
        }
        merged
    };
    candidates
        .iter()
        .find(|item| {
            ["id", "name", "upstreamKey"].iter().any(|key| {
                item.get(*key)
                    .and_then(Value::as_str)
                    .is_some_and(|value| value.eq_ignore_ascii_case(wanted))
            })
        })
        .cloned()
}

/// 公开的重查入口（自检 / 路由失败时的文案用）
pub fn known_names() -> Vec<String> {
    list()
        .iter()
        .filter_map(|item| item.get("id").and_then(Value::as_str).map(str::to_string))
        .collect()
}

/// 远程刷新（`POST /api/llm/config/v2`）。
///
/// `force = false` 时按 TTL 早退（自动路径：客户端每次拉模型列表都真打上游既
/// 没必要也招风控）；`force = true` 是用户点「刷新模型清单」的语义，真打一次。
///
/// `proxy` 是账号级出口（由调用方从账号记录解析好，见 `mod.rs` 的
/// `refresh_models`）；`None` = 直连。
pub async fn refresh(
    credentials: &Credentials,
    proxy: Option<&crate::server::core::proxies::ResolvedProxy>,
    force: bool,
) -> ModelRefreshOutcome {
    let region = credentials.region;
    let fetched_at = last_refreshed_at(region);
    if !force && fetched_at > 0 && logging::now_ms() - fetched_at < CACHE_TTL_MS {
        return ModelRefreshOutcome::unchanged();
    }
    let body = json!({ "token": credentials.access_token, "supportAutoModel": true });
    let response = match auth::post_json(region, endpoints::MODEL_CONFIG_PATH, &body, proxy).await {
        Ok(response) => response,
        Err(error) => return ModelRefreshOutcome::failed(format!("目录请求失败：{}", error.message)),
    };
    if !response.ok {
        return ModelRefreshOutcome::failed(format!(
            "上游返回 HTTP {}（{}）",
            response.status,
            if response.status == 401 || response.status == 403 {
                "凭证可能已失效，请重新登录"
            } else {
                "请稍后重试"
            }
        ));
    }
    let payload = match response.payload {
        Some(payload) => payload,
        None => return ModelRefreshOutcome::failed("上游未返回有效 JSON"),
    };
    let models = parse_catalog(&payload);
    if models.is_empty() {
        return ModelRefreshOutcome::failed("上游目录里没有可见模型");
    }
    let count = models.len();
    {
        let mut guard = cache_slot(region).write().unwrap_or_else(|error| error.into_inner());
        guard.models = models;
        guard.fetched_at = logging::now_ms();
    }
    logging::log(
        "[Models]",
        &format!("Accio {}模型目录已刷新（{count} 个模型）", region.label()),
    );
    ModelRefreshOutcome::refreshed(count)
}

/// 解析 `/api/llm/config/v2` 的响应：`{providers:[{modelList:[…]}]}`。
fn parse_catalog(payload: &Value) -> Vec<Value> {
    let root = match payload.get("data") {
        Some(Value::Object(_)) => payload.get("data").unwrap_or(payload),
        _ => payload,
    };
    let providers = match root.get("providers").and_then(Value::as_array) {
        Some(items) => items,
        None => return Vec::new(),
    };
    let mut models: Vec<Value> = Vec::new();
    for provider in providers {
        let Some(model_list) = provider.get("modelList").and_then(Value::as_array) else {
            continue;
        };
        for entry in model_list {
            // `visible: false` 是上游给内部用的模型（图像/预览版），不当转发目标
            if entry.get("visible").and_then(Value::as_bool) == Some(false) {
                continue;
            }
            let normalized = normalize_entry(entry);
            let id = normalized.get("id").and_then(Value::as_str).unwrap_or("");
            if id.is_empty() {
                continue;
            }
            if models.iter().any(|existing| {
                existing.get("id").and_then(Value::as_str).map(str::to_ascii_lowercase)
                    == Some(id.to_ascii_lowercase())
            }) {
                continue;
            }
            models.push(normalized);
        }
    }
    models
}
