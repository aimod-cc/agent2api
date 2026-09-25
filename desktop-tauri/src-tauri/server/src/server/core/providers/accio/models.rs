//! Accio 模型目录：静态兜底 + `/api/llm/config` 远程刷新。
//!
//! ── 清单从哪来 ──────────────────────────────────────────────
//!   1. **静态兜底**：`FALLBACK`（11 个可见模型，抄自客户端内置的
//!      `@ali/accio-adk-ts/model-catalog.json`，2026-09 快照）。进程启动即可用，
//!      没有账号 / 网络不通时 `/v1/models` 仍有内容。
//!   2. **远程刷新**：`POST {gw}/api/llm/config`，body `{token}`，
//!      头带 `x-package-region`（GLOBAL / CN）。上游按地区与账号权限给清单，
//!      并支持 `If-None-Match`（我们不用 304 缓存：本网关自己按 TTL 早退，
//!      少一条「快照与 etag 对不上」的状态）。
//!
//! ── 上游响应形态（2026-09 实测，解析必须按这一份）──────────────
//! ```jsonc
//! { "success": true, "code": "200", "message": "0",
//!   "data": [                       // ← **provider 数组**，不是 {providers:[…]}
//!     { "provider": "openai", "providerDisplayName": "OpenAI",
//!       "modelList": [ { "modelCode": "1Helix-G6aS8tR2qN7m", "modelName": "…",
//!                        "modelDisplayName": "GPT 6 Astra", "visible": true,
//!                        "protocol": "responses", "group": "azure", … } ] } ] }
//! ```
//! 早先的解析假设 `data` 是 `{providers:[…]}` **对象**，而它其实是数组 ——
//! 于是 `root.providers` 取不到、清单恒为空，而 HTTP 是 200（走不到错误分支），
//! 前端只看到「上游目录里没有可见模型」。数组与对象两种形态都接受。
//!
//! ── 上游把模型名换成了混淆代号（同一批实测）────────────────────
//! `modelCode` / `modelName` 现在是 `1Orbit-I9eY7YK8bW1f`（Claude Sonnet 4.6）、
//! `1Helix-…`（GPT 系）、`1Nexus-…` / `1Drift-…` / `1Nova-…` / `1Chronos-…`
//! 这类不透明代号，人类可读名只在 `modelDisplayName` 里。**老名字仍然可用**
//! （实测 `claude-sonnet-4-6` 与代号都能正常出流），所以：
//!   - `upstreamKey` 存上游代号 —— 发给上游的就是它，转发永远用当前代号；
//!   - 对外 `id` 走 `STABLE_IDS`：**能沿用老名字就沿用**（用户已配好的名字
//!     不用改），新模型没有老名字可用，才用代号。
//! 两条合起来的效果是「清单里看到什么名字，就点什么名字」，而发上游的
//! 始终是代号 —— 上游再换一次代号，只需要改 `STABLE_IDS` 的匹配，配置不动。
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
use crate::server::core::providers::catalog_cache;
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
///
/// ── 这张表现在只服务「没有账号 / 拉不到目录」的时刻 ─────────────
/// 条目名是**代号之前**的那一代，上游接口现在仍然认（逐条实测：除
/// `kimi-k2.5` 已被上游摘掉、**故已从本表删除**之外，其余都还能发）。
/// 保留它的作用是进程启动即有清单可广告、断网 / 未登录时 `/v1/models` 不为空。
/// 远程清单拉到之后这份就退居兜底。
///
/// `EffortPlacement` 一列是思考档位落点（实测口径见 [`EffortPlacement`]）：
/// 静态表里没有上游的 `protocol` 字段，所以按同一批实测逐条写死 ——
/// 远程清单走「读 `protocol`」，两条路都不再按模型名猜。
const FALLBACK: &[(&str, &str, u64, bool, &[&str], bool, EffortPlacement)] = &[
    // (modelCode, 展示名, contextWindow, multimodal, reasoningEfforts, isDefault, 档位落点)
    ("gemini-3-flash-preview", "Gemini 3 Flash", 1_000_000, true, &["low", "high"], true, EffortPlacement::Top),
    ("gemini-3.1-pro-preview", "Gemini 3.1 Pro", 1_000_000, true, &["low", "high"], false, EffortPlacement::Top),
    ("qwen3.6-plus", "Qwen 3.6 Plus", 991_808, false, &[], false, EffortPlacement::Top),
    ("qwen3-max-2026-01-23", "Qwen 3 Max", 262_144, false, &[], false, EffortPlacement::Top),
    ("gpt-5.4", "GPT 5.4", 1_050_000, true, &["low", "high"], false, EffortPlacement::Properties),
    ("gpt-5.2-1211", "GPT 5.2", 400_000, true, &["low", "high"], false, EffortPlacement::Properties),
    ("claude-sonnet-4-6", "Claude Sonnet 4.6", 1_000_000, true, &["low", "medium", "high", "max"], false, EffortPlacement::Top),
    ("claude-opus-4-6", "Claude Opus 4.6", 1_000_000, true, &["low", "medium", "high", "max"], false, EffortPlacement::Top),
    ("glm-5", "GLM-5", 200_000, false, &[], false, EffortPlacement::Top),
    ("MiniMax-M2.5", "MiniMax M2.5", 204_800, true, &[], false, EffortPlacement::Properties),
];

/// 老模型名 → 上游**展示名**（`modelDisplayName`）。
///
/// ── 这张表干什么 ────────────────────────────────────────────
/// 上游把目录里的模型名换成了不透明代号（`1Orbit-I9eY7YK8bW1f`），
/// 而用户手上的配置写的是代号之前的名字（`claude-sonnet-4-6`）。
/// 若清单直接广告代号，那些配置会立刻 404。这里把它们接回来：
/// 目录里只要有「Claude Sonnet 4.6」这一条，它就**沿用老名字当对外 id**
/// （`upstreamKey` 仍是代号，发上游用代号）。于是：老配置不用改，
/// 列表里也不会同时出现「老名字 + 代号」两份。
///
/// ── 为什么按展示名而不是硬编码代号 ─────────────────────────────
/// 代号是上游随时会换的不透明串；硬编码进代码等于把「上游换次代号就失效」
/// 写进实现。展示名才是上游稳定对外的东西，这张表**不需要跟着代号变**。
///
/// ── 只收「同一个模型」的对应关系 ─────────────────────────────
/// 版本跃迁不在这里接（`kimi-k2.5` → `Kimi K2.6`、`MiniMax-M2.5` → `MiniMax M3`、
/// `qwen3.6-plus` → `Qwen 3.8 27B` 等）：用户点名 A 却拿到 B 是静默替换，
/// 宁可 404 让客户端看到相近候选（入口校验会给 `suggest_advertised` 的提示）。
/// 上游接口仍认、但目录里已下架的名字（`gpt-5.4` / `gpt-5.2-1211` /
/// `qwen3-max-2026-01-23`）同样不接 —— 清单以目录为准，只广告上游在卖的东西。
const STABLE_IDS: &[(&str, &str)] = &[
    ("claude-sonnet-4-6", "Claude Sonnet 4.6"),
    ("claude-opus-4-6", "Claude Opus 4.6"),
    ("gemini-3-flash-preview", "Gemini 3 Flash"),
    ("gemini-3.1-pro-preview", "Gemini 3.1 Pro"),
    ("glm-5", "GLM-5"),
];

/// 某条目录条目的**对外 id**：能用老名字就用老名字（见 [`STABLE_IDS`]），
/// 否则用上游代号（新模型没有老名字可用）。
fn stable_id(code: &str, display: &str) -> String {
    STABLE_IDS
        .iter()
        .find(|(_, name)| name.eq_ignore_ascii_case(display))
        .map(|(alias, _)| (*alias).to_string())
        .unwrap_or_else(|| code.to_string())
}

/// 一个地区的远程清单 + 拉取时刻
#[derive(Clone, Default)]
struct RegionCache {
    models: Vec<Value>,
    fetched_at: i64,
}

/// 按地区取那一格目录缓存。首次初始化时**先从持久化缓存恢复**（上次成功
/// 拉到的远程清单），没有再留空 —— 空状态的读取语义就是「回落到静态兜底」。
///
/// 两格互不覆盖的理由与 AutoClaw 那边相同：上游按 `x-package-region` 给的是
/// **两份清单**，共用一个格子会让两家互相覆盖，而时间戳还显示「刚刚更新」。
fn cache_slot(region: Region) -> &'static RwLock<RegionCache> {
    static SLOTS: OnceLock<[RwLock<RegionCache>; 2]> = OnceLock::new();
    let slots = SLOTS.get_or_init(|| {
        [
            RwLock::new(restored_cache(Region::Global)),
            RwLock::new(restored_cache(Region::Cn)),
        ]
    });
    let index = match region {
        Region::Global => 0,
        Region::Cn => 1,
    };
    &slots[index]
}

/// 地区 → 持久化缓存的 scope（两格各一条，互不覆盖）
fn cache_scope(region: Region) -> &'static str {
    match region {
        Region::Global => catalog_cache::SCOPE_ACCIO_GLOBAL,
        Region::Cn => catalog_cache::SCOPE_ACCIO_CN,
    }
}

/// 首次初始化读一次持久化缓存（见 `providers::catalog_cache` 的模块头）。
///
/// 缓存里存的就是 `RegionCache` 的形态（`refresh` 落地的那份），所以这里只做
/// 「搬回来」：不重新归一 —— 两处各写一份映射迟早分叉。
fn restored_cache(region: Region) -> RegionCache {
    match catalog_cache::load(cache_scope(region)) {
        Some(cached) => RegionCache { models: cached.models, fetched_at: cached.fetched_at },
        None => RegionCache::default(),
    }
}

fn fallback_models() -> Vec<Value> {
    FALLBACK
        .iter()
        .map(|(code, name, window, multimodal, efforts, is_default, placement)| {
            let mut item = normalize_entry(&json!({
                "modelCode": code,
                "modelDisplayName": name,
                "contextWindow": window,
                "multimodal": multimodal,
                "reasoningEfforts": efforts,
                "isDefault": is_default,
            }));
            // 静态表自己带着落点（没有上游的 `protocol` 可读），写进去覆盖
            // `normalize_entry` 的兜底值
            if let Some(object) = item.as_object_mut() {
                object.insert(
                    "protocolKind".to_string(),
                    Value::String(placement.as_str().to_string()),
                );
            }
            item
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
        // 对外 id 用老名字（能用的话），上游发送名走 `upstreamKey`
        "id": stable_id(&code, &display),
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
        // 思考档位落点，由目录里的 `protocol` 决定（见 `EffortPlacement`）
        "protocolKind": effort_placement(entry).as_str(),
        "modelDesc": entry.get("modelDesc").cloned().unwrap_or(Value::Null),
        "usageMultiple": entry.get("usageMultiple").cloned().unwrap_or(Value::Null),
    })
}

/// 思考档位该放哪一层（`properties.reasoning_effort` 还是顶层 `reasoning_effort`）。
///
/// ── 为什么不能按模型名猜（2026-09 实测）───────────────────────
/// 上游把模型名换成了不透明代号（`1Helix-…` / `1Orbit-…`），
/// `contains("gpt")` 这种判据全部失效。改用上游自己在目录里给的
/// `protocol` 字段：
///
/// | `protocol`  | 例子           | 档位落点   | 依据（实测）                              |
/// |-------------|----------------|------------|-------------------------------------------|
/// | `responses` | GPT 系         | properties | 只有 properties 能出思考内容（80 帧 thought，顶层 0） |
/// | `openai`    | MiniMax M3     | properties | 同上（`openai` 沿同一落点）                |
/// | 空          | Claude / Gemini 系 | 顶层    | Gemini 系放 properties **直接 400**：`Unknown name "reasoning_effort"` |
///
/// 兜底取「顶层」：它是唯一在三种模型上都不会报错的落点（Gemini 系放
/// properties 是硬错误，放顶层只是不产生思考内容 —— 少个功能好过 400）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EffortPlacement {
    /// `properties.reasoning_effort`（`protocol` = `responses` / `openai`）
    Properties,
    /// 顶层 `reasoning_effort`（`protocol` 为空或未知）
    Top,
}

impl EffortPlacement {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Properties => "properties",
            Self::Top => "top",
        }
    }

    /// 从条目上的自有键读回落点（发送侧用）。缺键 / 值不认识 → `Top`
    /// （兜底口径与 [`effort_placement`] 一致）。
    pub fn from_entry(entry: &Value) -> Self {
        match entry.get("protocolKind").and_then(Value::as_str) {
            Some("properties") => Self::Properties,
            _ => Self::Top,
        }
    }
}

/// 上游模型条目 → 思考档位落点（见 [`EffortPlacement`]）。
///
/// 认的是目录里的 `protocol`（上游自己声明的协议）；`group`
/// （`azure` / `google` / `dashscope` …）是执行池、`thinkLevel` 是另一回事，
/// 都不参与这条判定。
fn effort_placement(entry: &Value) -> EffortPlacement {
    match entry
        .get("protocol")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "responses" | "openai" => EffortPlacement::Properties,
        _ => EffortPlacement::Top,
    }
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
///
/// ── 老名字为什么也认 ────────────────────────────────────────
/// 远程条目的 `id` 已经是「能沿用就沿用」的老名字（见 [`STABLE_IDS`]），
/// 所以用户手上的老配置直接命中 `id`；`upstreamKey` 兜住「客户端从桌面端
/// 抄了代号过来」这一种输入。两条都在这里一次认全。
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

/// 远程刷新（`POST /api/llm/config`，失败回落 `/api/llm/config/v2`）。
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
    // v1 是完整清单（见 `endpoints::MODEL_CONFIG_PATH`），v2 只在它出意外时
    // 兜一次底 —— 两条路径的信封与解析完全一样，差别只在返回多少模型。
    let body = json!({ "token": credentials.access_token, "supportAutoModel": true });
    let mut last_error: Option<String> = None;
    let mut models: Vec<Value> = Vec::new();
    for path in [endpoints::MODEL_CONFIG_PATH, endpoints::MODEL_CONFIG_PATH_V2] {
        let response = match auth::post_json(region, path, &body, proxy).await {
            Ok(response) => response,
            Err(error) => {
                last_error = Some(format!("目录请求失败：{}", error.message));
                continue;
            }
        };
        if !response.ok {
            last_error = Some(format!(
                "上游返回 HTTP {}（{}）",
                response.status,
                if response.status == 401 || response.status == 403 {
                    "凭证可能已失效，请重新登录"
                } else {
                    "请稍后重试"
                }
            ));
            continue;
        }
        let Some(payload) = response.payload else {
            last_error = Some("上游未返回有效 JSON".to_string());
            continue;
        };
        let parsed = parse_catalog(&payload);
        if parsed.is_empty() {
            last_error = Some("上游目录里没有可见模型".to_string());
            continue;
        }
        // 兜底那条路径成功时留一行痕：它意味着 v1 那边出了问题（上游改版 /
        // 灰度下线），排障时「这次清单为什么少了三家」要看的就是这一行
        if path == endpoints::MODEL_CONFIG_PATH_V2 {
            logging::log(
                "[Models]",
                &format!(
                    "⚠️ Accio {}目录回落到 v2 精简清单（{} 个模型）—— v1 本次不可用",
                    region.label(),
                    parsed.len()
                ),
            );
        }
        models = parsed;
        break;
    }
    if models.is_empty() {
        return ModelRefreshOutcome::failed(
            last_error.unwrap_or_else(|| "上游目录里没有可见模型".to_string()),
        );
    }
    let count = models.len();
    let now = logging::now_ms();
    // 先落持久化缓存（进程重启后由 `restored_cache` 读回）；**不在缓存格锁内**
    // —— 缓存写入要拿库连接锁，两把锁不能嵌套。scope 与格子一一对应。
    catalog_cache::save(cache_scope(region), &models, now);
    {
        let mut guard = cache_slot(region).write().unwrap_or_else(|error| error.into_inner());
        guard.models = models;
        guard.fetched_at = now;
    }
    logging::log(
        "[Models]",
        &format!("Accio {}模型目录已刷新（{count} 个模型）", region.label()),
    );
    ModelRefreshOutcome::refreshed(count)
}

/// 解析模型目录响应。
///
/// ── 两种信封都要认（2026-09 实测的第一种是当前形态）───────────
///   1. `{success, data:[{provider, modelList:[…]}, …]}` —— **现在就是这个**，
///      `data` 是 provider 数组（早先的解析按对象找 `providers`，于是恒为空）；
///   2. `{data:{providers:[{modelList:[…]}]}}` —— 旧形态，保留兼容。
///
/// ── 两道过滤 ────────────────────────────────────────────────
///   - `visible: false` 是上游给内部用的（图像 / 预览版，实测 69 条里 28 条）；
///   - [`is_chat_entry`] 挡掉专用生成模型（见那个函数的说明）。
fn parse_catalog(payload: &Value) -> Vec<Value> {
    // 先看 `data` 本身是不是 provider 数组（形态 1）
    let root = payload.get("data").unwrap_or(payload);
    let providers = root
        .as_array()
        .or_else(|| root.get("providers").and_then(Value::as_array));
    let Some(providers) = providers else {
        return Vec::new();
    };
    let mut models: Vec<Value> = Vec::new();
    for provider in providers {
        let Some(model_list) = provider.get("modelList").and_then(Value::as_array) else {
            continue;
        };
        for entry in model_list {
            if entry.get("visible").and_then(Value::as_bool) == Some(false) {
                continue;
            }
            if !is_chat_entry(entry) {
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

/// 这条目录条目是不是**对话**模型。
///
/// ── 为什么光看 `visible` 不够（2026-09 实测）───────────────────
/// 目录里有专用生成模型（视频生成那一组），它们的 `visible` 是 **null 而不是
/// false**，只按 `visible == false` 过滤会漏进来一条 `rlab-1.0-storyboard`
/// （「Multi-shot Video」，`contextWindow` 也是 null）—— 它会以「上下文 0」
/// 的形态出现在模型列表里，点它必然失败。这类条目的判据是
/// `supportedOperationList`：实测**对话条目一律为 null**，只有视频生成那条
/// 给了 `["VIDEO_GENERATION"]`。
///
/// 用「点名挡掉已知的非对话操作」而不是「只放行空列表」：上游将来给对话条目
/// 填上这个字段时，宁可多收一个也不要把正常模型挡在门外（少个模型是静默的，
/// 多一个奇怪的模型至少看得见）。
fn is_chat_entry(entry: &Value) -> bool {
    const NON_CHAT_OPERATIONS: &[&str] = &["VIDEO_GENERATION", "IMAGE_GENERATION", "AUDIO_GENERATION"];
    entry
        .get("supportedOperationList")
        .and_then(Value::as_array)
        .is_none_or(|operations| {
            !operations
                .iter()
                .filter_map(Value::as_str)
                .any(|operation| {
                    NON_CHAT_OPERATIONS
                        .iter()
                        .any(|blocked| operation.eq_ignore_ascii_case(blocked))
                })
        })
}
