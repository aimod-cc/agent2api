//! WorkBuddy 模型目录（对照 Node 版 src/workbuddy-models.mjs 全量移植）。
//!
//! 模型清单内嵌于 WorkBuddy 内置 CLI 的 product.json（v5.3.14）。这里收录
//! 主要对话/多模态模型，作为 /v1/models 响应与默认模型回退。
//!
//! 运行时权威来源：`GET {endpoint}/v3/config` → `data.models`（企业账号再取
//! `{endpoint}/console/enterprises/{id}/config/models`，本模块一并实现）。
//! 注意：服务端按官方客户端 UA 区分来源，缺失 UA 时 /v3/config 的 models
//! 可能返回 null，因此 refresh 时必须带 User-Agent。
//!
//! ── 一个与任务书不一致的确认结论（有意为之）──────────────────
//! 任务书提到「只在版本变化时写盘 {config_dir}/models.json」，但**Node 版
//! 没有任何磁盘缓存**（全仓 grep `models.json` 零命中，workbuddy-models.mjs
//! 全文没有 fs 调用）：目录是纯内存态，进程重启后回到内置清单，再由启动时/
//! 每次 GET /v1/models 的异步刷新重新拉取。契约以 Node 版为准，所以这里
//! **不新增缓存文件** —— 多一个文件就多一处与 Node 版的数据格式分叉点，
//! 而收益只是省一次启动请求。
//!
//! ── 并发 ──────────────────────────────────────────────────
//! 目录是「读多写少」的共享态：句柄内一把 `RwLock`，读路径（/v1/models、
//! /health、聊天路由的模型校验，以及经聚合目录间接读它的 /api/session）
//! 拿读锁，刷新落地时拿写锁。
//! 硬约束（与账号存储相同）：**持锁期间绝不做网络请求** —— 刷新先把请求
//! 发完、解析完，最后才在锁内做一次整体替换。
//!
//! ── 多提供商（Agent2API 改造 W2a-T2）后的定位 ──────────────
//! 本模块**只负责 workbuddy 这一家的清单**（内置清单 + /v3/config 远程刷新），
//! 它继续是 workbuddy 的清单事实来源。跨 provider 的合并视图在
//! `core::providers::catalog`（聚合目录），两边共用 `shape.rs` 的「来源无关」
//! 构造件（条目字段映射、响应信封、相近提示纯函数）——
//! 于是「单家清单」与「聚合清单」永远不会在字段映射上分叉。
//!
//! 词法上本模块是个目录（`models/mod.rs` + `models/shape.rs`）：拆分的唯一
//! 理由是单文件行数约定（≤800 行），职责边界与拆分前完全一致。
//!
//! ── 进程级句柄 ──────────────────────────────────────────────
//! `global_catalog()` 是聚合层的读取口（`OnceLock` 单例，与
//! `core::desensitize::global()` 同一模式）：`ServerState::bootstrap` 与聚合层
//! 拿到的是同一实例，刷新对两边同时可见。

mod shape;

use std::sync::{Arc, OnceLock, RwLock};

use serde_json::{json, Value};

use crate::server::core::auth_http::send_raw;
use crate::server::core::endpoints::{normalize_endpoint, user_agent_for_edition};
use crate::server::core::providers::{kind_id, ProviderKind};
use crate::server::core::proxies::ResolvedProxy;
use crate::server::logging;

// 本模块内部用（is_chat_model / apply_filters / get / apply_remote / list_response）
use shape::{js_truthy, value_text};

// 对外再导出：聚合目录（core::providers::catalog）与排障方从 `core::models`
// 取这些名字，保持与拆分前同一条导入路径。
// `shape_value_text` 是 `shape::value_text` 的别名导出：聚合目录要按 `name`
// 匹配模型（对齐 `ModelCatalog::get` 的第二段），需要与这里同一套 JS 文本化。
pub use shape::{list_item, list_response_from, model_id, suggest_from, value_text as shape_value_text};

/// 下游可见模型白名单：/v1/models 与路由解析只暴露这些模型。
///
/// 空数组 = 不限制，全量放行（/v3/config 拉回的真实清单，约 51 个模型）。
/// 与 Node 版一样保持空数组 —— 需要收窄时填入模型 id。
pub const MODEL_ALLOWLIST: &[&str] = &[];

/// 非对话模型的排除前缀，照抄 Node 版 CHAT_MODEL_EXCLUDE_RE 的九个分支。
///
/// 判定依据来自 /v3/config 下发的模型元数据（实测 2026-09-10）：
///   - 内部补全/工具模型带 supportsExtra，输出上限 ≤8192
///   - 图像模型没有 maxOutputTokens（只有 id/name/tags）
///   - 旧一代小输出对话模型（r1-0528 / v3-0324 / kimi-k2-instruct / Claude 内部 id）
///     由黑名单排除，官方客户端 UI 也不展示它们
///
/// 注意 `default-1.` 这一条会把内置清单里的 `default-1.1` / `default-1.2`
/// 一起排除 —— 这是 Node 版的既有行为（那两个是 Claude 的内部 id），
/// 移植时保持一致，不做「顺手修复」。
const CHAT_MODEL_EXCLUDE_PREFIXES: &[&str] = &[
    "completion-",
    "codewise-",
    "hunyuan-image",
    "hunyuan-3b",
    "hunyuan-7b",
    "deepseek-r1-0528",
    "deepseek-v3-0324",
    "kimi-k2-instruct",
    "default-1.",
];

/// 远程目录刷新失败时保留内置目录的说明（Node 的 reason 文案）
const REASON_NO_SESSION: &str = "缺少登录态，使用内置模型目录";

/// 内置模型目录（id → 元数据），逐字段照抄 Node 版 BUILTIN_MODELS。
///
/// credits 字段为桌面端展示的单价，仅作参考。
fn builtin_models() -> Vec<Value> {
    json!([
        { "id": "auto", "name": "Auto", "vendor": "f", "credits": "", "maxOutputTokens": 32000, "maxInputTokens": 168000, "supportsToolCall": true, "supportsImages": true, "isDefault": true, "descriptionZh": "平衡效果与速度" },
        { "id": "default", "name": "Default", "vendor": "v", "credits": "x2.00 credits", "maxOutputTokens": 24000, "maxInputTokens": 200000, "supportsToolCall": true, "supportsImages": false },

        { "id": "deepseek-v4-pro", "name": "Deepseek-V4-Pro", "vendor": "f", "credits": "x0.16 credits", "maxOutputTokens": 50000, "maxInputTokens": 1000000, "supportsToolCall": true, "supportsImages": true, "supportsReasoning": true, "onlyReasoning": true, "descriptionZh": "DeepSeek 旗舰模型，支持 1M 上下文窗口" },
        { "id": "deepseek-v4-flash", "name": "Deepseek-V4-Flash", "vendor": "f", "credits": "x0.06 credits", "maxOutputTokens": 50000, "maxInputTokens": 1000000, "supportsToolCall": true, "supportsImages": true, "supportsReasoning": true, "onlyReasoning": true, "descriptionZh": "DeepSeek 旗舰模型，支持 1M 上下文窗口" },
        // deepseek-v4.1-flash 不在 5.3.14 内置清单/旧版 /v3/config 里，但上游实测可路由；
        // 作为远程清单不可用时的兜底定义（远程正常时以远程元数据为准）。
        { "id": "deepseek-v4.1-flash", "name": "Deepseek-V4.1-Flash", "vendor": "f", "credits": "x0.03 credits", "maxOutputTokens": 50000, "maxInputTokens": 1000000, "supportsToolCall": true, "supportsImages": true, "supportsReasoning": true, "onlyReasoning": true, "descriptionZh": "DeepSeek 快速模型，支持 1M 上下文窗口" },
        { "id": "deepseek-v3-2-volc", "name": "DeepSeek-V3.2", "vendor": "f", "credits": "", "maxOutputTokens": 32000, "maxInputTokens": 96000, "supportsToolCall": true, "supportsImages": true, "supportsReasoning": true, "onlyReasoning": true },

        { "id": "glm-5.1", "name": "GLM-5.1", "vendor": "e", "credits": "", "maxOutputTokens": 48000, "maxInputTokens": 200000, "supportsToolCall": true, "supportsImages": true, "supportsReasoning": true, "onlyReasoning": true },
        { "id": "glm-5.0", "name": "GLM-5.0", "vendor": "f", "credits": "", "maxOutputTokens": 48000, "maxInputTokens": 200000, "supportsToolCall": true, "supportsImages": true, "supportsReasoning": true, "onlyReasoning": true },
        { "id": "glm-5.0-turbo", "name": "GLM-5.0-Turbo", "vendor": "e", "credits": "", "maxOutputTokens": 48000, "maxInputTokens": 200000, "supportsToolCall": true, "supportsImages": true, "supportsReasoning": true, "onlyReasoning": true },
        { "id": "glm-5v-turbo", "name": "GLM-5v-Turbo", "vendor": "e", "credits": "", "maxOutputTokens": 38000, "maxInputTokens": 200000, "supportsToolCall": true, "supportsImages": true, "supportsReasoning": true, "onlyReasoning": true },
        { "id": "glm-4.7", "name": "GLM-4.7", "vendor": "f", "credits": "", "maxOutputTokens": 48000, "maxInputTokens": 200000, "supportsToolCall": true, "supportsImages": true, "supportsReasoning": true, "onlyReasoning": true },
        { "id": "glm-4.6", "name": "GLM-4.6", "vendor": "f", "credits": "", "maxOutputTokens": 32000, "maxInputTokens": 168000, "supportsToolCall": true, "supportsImages": true, "supportsReasoning": true },
        { "id": "glm-4.6v", "name": "GLM-4.6V", "vendor": "f", "credits": "", "maxOutputTokens": 32000, "maxInputTokens": 128000, "supportsToolCall": true, "supportsImages": true, "supportsReasoning": true, "onlyReasoning": true },

        { "id": "minimax-m2.5", "name": "MiniMax-M2.5", "vendor": "f", "credits": "", "maxOutputTokens": 48000, "maxInputTokens": 200000, "supportsToolCall": true, "supportsImages": true, "supportsReasoning": true, "onlyReasoning": true },
        { "id": "minimax-m2.7", "name": "MiniMax-M2.7", "vendor": "f", "credits": "", "maxOutputTokens": 48000, "maxInputTokens": 200000, "supportsToolCall": true, "supportsImages": true, "supportsReasoning": true, "onlyReasoning": true },
        { "id": "minimax-m3-play", "name": "MiniMax-M3", "vendor": "f", "credits": "x0.25 credits", "maxOutputTokens": 48000, "maxInputTokens": 512000, "supportsToolCall": true, "supportsImages": true, "supportsReasoning": true, "onlyReasoning": true, "descriptionZh": "原生多模态，擅长代码、智能体任务" },

        { "id": "kimi-k2.6", "name": "Kimi-K2.6", "vendor": "f", "credits": "", "maxOutputTokens": 32000, "maxInputTokens": 256000, "supportsToolCall": true, "supportsImages": true, "supportsReasoning": true, "onlyReasoning": true },
        { "id": "kimi-k2.5", "name": "Kimi-K2.5", "vendor": "f", "credits": "", "maxOutputTokens": 32000, "maxInputTokens": 256000, "supportsToolCall": true, "supportsImages": true, "supportsReasoning": true, "onlyReasoning": true },
        { "id": "kimi-k2-thinking", "name": "Kimi-K2-Thinking", "vendor": "f", "credits": "", "maxOutputTokens": 32000, "maxInputTokens": 256000, "supportsToolCall": true, "supportsImages": true, "supportsReasoning": true, "onlyReasoning": true },
        { "id": "kimi-k2-instruct-taiji", "name": "Kimi-K2", "vendor": "f", "credits": "", "maxOutputTokens": 8192, "maxInputTokens": 31000, "supportsToolCall": true, "supportsImages": true },

        { "id": "hy3-preview", "name": "Hy3 preview", "vendor": "j", "credits": "", "maxOutputTokens": 64000, "maxInputTokens": 192000, "supportsToolCall": true, "supportsImages": true, "supportsReasoning": true, "onlyReasoning": true },
        { "id": "hy3-preview-agent", "name": "Hy3 preview", "vendor": "j", "credits": "x0.04 credits", "maxOutputTokens": 64000, "maxInputTokens": 192000, "supportsToolCall": true, "supportsImages": true, "supportsReasoning": true, "onlyReasoning": true },
        { "id": "hunyuan-chat", "name": "Hunyuan-Turbos", "vendor": "j", "credits": "", "maxOutputTokens": 8192, "maxInputTokens": 128000, "supportsToolCall": true, "supportsImages": true, "descriptionZh": "腾讯自研的轻量、快速的通用模型" },
        { "id": "hunyuan-2.0-instruct", "name": "Hunyuan-2.0-Instruct", "vendor": "j", "credits": "", "maxOutputTokens": 16000, "maxInputTokens": 128000, "supportsToolCall": true, "supportsImages": true, "supportsReasoning": true, "onlyReasoning": true },
        { "id": "hunyuan-2.0-thinking", "name": "Hunyuan-2.0-Thinking", "vendor": "e", "credits": "", "maxOutputTokens": 24000, "maxInputTokens": 128000, "supportsToolCall": true, "supportsImages": true, "supportsReasoning": true, "onlyReasoning": true },

        { "id": "default-1.1", "name": "Claude-3.7-Sonnet", "vendor": "e", "credits": "", "maxOutputTokens": 8192, "maxInputTokens": 200000, "supportsToolCall": true, "supportsImages": true, "supportsReasoning": true, "onlyReasoning": true },
        { "id": "default-1.2", "name": "Claude-4.0-Sonnet", "vendor": "e", "credits": "", "maxOutputTokens": 24000, "maxInputTokens": 200000, "supportsToolCall": true, "supportsImages": true, "supportsReasoning": true, "onlyReasoning": true },
    ])
    .as_array()
    .cloned()
    .unwrap_or_default()
}

/// 对话模型判定：网关只服务对话模型，非对话项（补全/内部工具/图像小模型）
/// 不进目录 —— /v1/models、/api/session（经聚合目录）与路由解析三者保持一致。
pub fn is_chat_model(model: &Value) -> bool {
    let id = model.get("id").map(value_text).unwrap_or_default();
    if CHAT_MODEL_EXCLUDE_PREFIXES
        .iter()
        .any(|prefix| id.starts_with(prefix))
    {
        return false;
    }
    // `model.supportsExtra` 真值判定（任何真值都排除）
    if model.get("supportsExtra").map(js_truthy).unwrap_or(false) {
        return false;
    }
    // `typeof model.maxOutputTokens !== 'number'` → 排除
    let Some(max_output) = model.get("maxOutputTokens").and_then(Value::as_f64) else {
        return false;
    };
    max_output >= 16_000.0
}

/// 目录的内部状态：模型清单 + 刷新元信息（对应 Node 版的闭包变量）
#[derive(Clone, Debug)]
struct CatalogState {
    models: Vec<Value>,
    /// 最后一次成功的远程刷新时间（毫秒），初始 0
    last_refreshed_at: i64,
    /// 是否曾成功采用远程清单（决定 list_response 的 meta.source）
    remote_refreshed: bool,
}

/// 目录准入过滤：对话模型判定 + 白名单（空白名单全量放行）
fn apply_filters(models: Vec<Value>) -> Vec<Value> {
    models
        .into_iter()
        .filter(is_chat_model)
        .filter(|model| {
            if MODEL_ALLOWLIST.is_empty() {
                return true;
            }
            let id = model.get("id").map(value_text).unwrap_or_default();
            MODEL_ALLOWLIST.contains(&id.as_str())
        })
        .collect()
}

/// 白名单模型必须有可用定义：远程/传入清单缺失时回退内置定义
fn allowlist_fallback() -> Vec<Value> {
    builtin_models()
        .into_iter()
        .filter(|model| {
            MODEL_ALLOWLIST.contains(&model.get("id").map(value_text).unwrap_or_default().as_str())
        })
        .filter(is_chat_model)
        .collect()
}

/// 初始目录：内置清单（经准入过滤）——与 Node 版 createModelCatalog 的初始化一致。
fn initial_state() -> CatalogState {
    let allowed = apply_filters(builtin_models());
    let models = if allowed.is_empty() { allowlist_fallback() } else { allowed };
    CatalogState { models, last_refreshed_at: 0, remote_refreshed: false }
}

/// 模型目录句柄：内部一把 `RwLock`，克隆共享同一份状态。
#[derive(Clone)]
pub struct ModelCatalog {
    inner: Arc<RwLock<CatalogState>>,
}

impl Default for ModelCatalog {
    fn default() -> Self {
        Self::new()
    }
}

impl ModelCatalog {
    /// 构造目录（不刷新；刷新由启动流程与 /v1/models 触发）
    pub fn new() -> Self {
        Self { inner: Arc::new(RwLock::new(initial_state())) }
    }

    /// 读取状态快照；锁中毒（持锁 panic）时接管内部数据继续用，
    /// 与账号存储同一策略：宁可容忍一次中毒，也不要让网关永久不可用。
    fn read(&self) -> CatalogState {
        match self.inner.read() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    fn write(&self, next: CatalogState) {
        match self.inner.write() {
            Ok(mut guard) => *guard = next,
            Err(poisoned) => *poisoned.into_inner() = next,
        }
    }

    /// 按 id 或 name 查模型（大小写不敏感）；查不到返回 None。
    ///
    /// 顺序照抄 Node：先按 id 精确（忽略大小写），再按 name。
    pub fn get(&self, id: Option<&str>) -> Option<Value> {
        let id = id.map(str::trim).filter(|value| !value.is_empty())?;
        let lower = id.to_lowercase();
        let models = self.read().models;
        models
            .iter()
            .find(|model| value_text(&model["id"]).to_lowercase() == lower)
            .or_else(|| {
                models.iter().find(|model| {
                    model
                        .get("name")
                        .map(value_text)
                        .unwrap_or_default()
                        .to_lowercase()
                        == lower
                })
            })
            .cloned()
    }

    /// 目录里是否有该模型。
    ///
    /// **不再被对话链路调用**（Agent2API 改造 W2b-T3 起模型校验走
    /// `providers::catalog::providers_for_model`，视图是聚合目录）：这里是
    /// 「只看 workbuddy 这一家」的判定，排障时与聚合判定对比即可分辨
    /// 「是清单不对」还是「聚合/去重不对」。
    #[allow(dead_code)]
    pub fn has(&self, id: &str) -> bool {
        self.get(Some(id)).is_some()
    }

    /// 解析模型 id：只接受目录里真实存在的 id（大小写不敏感）。
    ///
    /// 未知名直接返回 None —— 绝不静默改写/回退：用户点名要 A 就必须路由到 A，
    /// 不存在就报错让下游自己改（路由层据此返回 400 与相近模型提示）。
    ///
    /// Node 版 `createModelCatalog` 导出的 `resolve(id)` 对等物（Node 里未命中会抛
    /// ModelRouteError）：本进程的请求校验走 `models().has()` + `suggest()`，
    /// 因此这条暂无调用点，保留以便与 Node 的目录 API 一一对应。
    #[allow(dead_code)]
    pub fn resolve(&self, id: &str) -> Option<String> {
        self.get(Some(id))
            .and_then(|model| model.get("id").map(value_text))
    }

    /// 仅供 400 报错信息用的「你是不是想要」提示：列出目录里与请求名最相近的 id。
    ///
    /// 结果不参与路由 —— 路由永远只认精确存在的模型。
    /// 判定逻辑在 `suggest_from`（聚合目录共用同一份，保证单一来源与聚合两种
    /// 视角下的提示口径一致）。
    ///
    /// **不再被对话链路调用**（Agent2API 改造 W2b-T3 起 400 提示走
    /// `providers::catalog::suggest_models`，数据源是聚合目录）：这里是
    /// 「只看 workbuddy 这一家」的视图，排障时与聚合提示对比即可分辨
    /// 「是清单不对」还是「聚合/去重不对」。对只有 workbuddy 一家的用户，
    /// 两者结果相同（共用 `suggest_from`）。
    #[allow(dead_code)]
    pub fn suggest(&self, id: &str, limit: usize) -> Vec<String> {
        let ids: Vec<String> = self
            .read()
            .models
            .iter()
            .filter_map(|model| model.get("id").map(value_text))
            .collect();
        suggest_from(ids, id, limit)
    }

    /// 当前目录（克隆；调用方不会看到后续刷新）
    pub fn list(&self) -> Vec<Value> {
        self.read().models
    }

    /// 是否曾成功采用远程清单（挂进响应 meta 用；聚合层据此算「来源」）
    pub fn remote_refreshed(&self) -> bool {
        self.read().remote_refreshed
    }

    /// 最后一次成功远程刷新的时间（毫秒，未刷新过为 0）
    pub fn last_refreshed_at(&self) -> i64 {
        self.read().last_refreshed_at
    }

    /// 目录条数（/health 的 models 字段）
    pub fn count(&self) -> usize {
        self.read().models.len()
    }

    /// GET /v1/models 的 OpenAI 风格响应：`{object, data: [...], meta: {...}}`
    ///
    /// 响应体本身的拼装走 `list_response_from`（聚合目录共用同一函数，
    /// 区别只在 data 是合并后的数组）—— 单家与聚合两个视角的信封字段
    /// 与顺序不可能分叉。
    ///
    /// **保留但不再被 /v1/models 调用**（Agent2API 改造 W2a-T2 起该端点走
    /// `providers::catalog::models_response`）：这里是「只看 workbuddy 这一家」
    /// 的视图，排障时与聚合输出对比即可立刻分辨「是 workbuddy 清单不对」
    /// 还是「聚合/过滤逻辑不对」。对只有 workbuddy 一家的用户，
    /// 它与聚合输出逐字段相同（聚合层复用同一个 list_item / 信封）。
    #[allow(dead_code)]
    pub fn list_response(&self) -> Value {
        let state = self.read();
        let data: Vec<Value> = state
            .models
            .iter()
            .map(|model| list_item(model, kind_id(ProviderKind::WorkBuddy)))
            .collect();
        list_response_from(
            data,
            if state.remote_refreshed { "remote" } else { "builtin" },
            state.last_refreshed_at,
        )
    }

    /// 是否已采用远程清单（排障用）。
    /// Node 版 `createModelCatalog` 导出的 `get isRemote()` 对等物（Node 里同样
    /// 没有生产调用点）：`list_response` 的 `meta.source` 已表达同一事实，
    /// 这条保留给排障探针按布尔直读。
    #[allow(dead_code)]
    pub fn is_remote(&self) -> bool {
        self.read().remote_refreshed
    }

    /// 远程配置刷新（与桌面端行为一致）：
    ///
    ///   1) GET {endpoint}/v3/config  → data.models 为运行时清单
    ///   2) 企业账号再取 /console/enterprises/{id}/config/models
    ///
    /// 两者都不可用时保留内置目录。拉回的清单先过 MODEL_ALLOWLIST 白名单。
    /// 返回 `{refreshed, count, source}` 或 `{refreshed:false, reason}` 语义
    /// 与 Node 版逐字一致 —— 调用方据此决定打 ✅ 还是 verbose。
    pub async fn refresh(&self, params: RefreshParams<'_>) -> RefreshOutcome {
        let base = params.endpoint.trim_end_matches('/').to_string();
        let has_auth = params.headers.iter().any(|(key, value)| {
            key.eq_ignore_ascii_case("authorization") && !value.trim().is_empty()
        });

        if !base.is_empty() && has_auth {
            let url = format!("{base}/v3/config");
            // 与 Node 一致：显式 Accept + 官方 UA（服务端按 UA 识别客户端通道，
            // 缺失时会只下发旧版兼容清单）。Accept 由 send_raw 统一补
            // `application/json`（Node 这里给的也是同一个值），不必重复声明 ——
            // reqwest 的 header() 是 append，重复给会变成两个 Accept 头。
            let mut headers: Vec<(String, String)> =
                vec![("User-Agent".to_string(), params.user_agent.to_string())];
            headers.extend(params.headers.iter().cloned());
            match send_raw("GET", &url, None, &headers, params.proxy, None).await {
                // 不校验 HTTP 状态：与 Node 一致 —— 先按 JSON 解析，再看有没有可用模型
                Ok(response) => match response.payload {
                    Some(payload) => {
                        let raw = payload
                            .get("data")
                            .and_then(|data| data.get("models"))
                            .and_then(Value::as_array)
                            .cloned()
                            .unwrap_or_default();
                        let allowed = apply_filters(raw.clone());
                        if !allowed.is_empty() {
                            self.apply_remote(allowed, "auto", false);
                            return RefreshOutcome::refreshed(
                                self.count(),
                                "v3/config",
                            );
                        }
                        logging::verbose(
                            "[Models]",
                            &format!("远程清单({} 个)不含可用对话模型，保留内置目录", raw.len()),
                        );
                    }
                    None => {
                        logging::verbose("[Models]", "远程配置拉取失败: 响应不是合法 JSON");
                    }
                },
                Err(error) => {
                    logging::verbose(
                        "[Models]",
                        &format!(
                            "远程配置拉取失败: {}",
                            crate::server::core::egress::describe_error_detail(&error)
                        ),
                    );
                }
            }
        }

        if !base.is_empty() {
            if let Some(enterprise_id) = params.enterprise_id.filter(|id| !id.is_empty()) {
                let url = format!(
                    "{base}/console/enterprises/{}/config/models",
                    encode_uri_component(enterprise_id)
                );
                let headers: Vec<(String, String)> = params.headers.to_vec();
                return match send_raw("GET", &url, None, &headers, params.proxy, None).await {
                    Ok(response) => {
                        let raw = response
                            .payload
                            .as_ref()
                            .and_then(|payload| payload.get("data"))
                            .and_then(Value::as_array)
                            .cloned()
                            .unwrap_or_default();
                        let allowed = apply_filters(raw);
                        if allowed.is_empty() {
                            RefreshOutcome::not_refreshed("企业模型列表为空")
                        } else {
                            self.apply_remote(allowed, "custom:", true);
                            RefreshOutcome::refreshed(self.count(), "enterprise")
                        }
                    }
                    Err(error) => RefreshOutcome::not_refreshed(
                        crate::server::core::egress::describe_error_detail(&error),
                    ),
                };
            }
        }

        RefreshOutcome::not_refreshed(REASON_NO_SESSION)
    }

    /// 落地一份远程清单（锁内只做替换，不做任何 IO；锁外再补一次默认规则
    /// 种子，见函数尾）
    ///
    /// 默认模型统一为 auto（远程可能把 fast-model 等档位模型标为默认，
    /// 与网关默认不一致）。`enterprise` 为 true 时走企业分支：
    /// modelType 按 `custom:` 前缀区分企业定制/内置模型
    /// （判定用**原始 id**，大小写不敏感的处理只用于 isDefault 那一处）。
    fn apply_remote(&self, models: Vec<Value>, custom_prefix: &str, enterprise: bool) {
        let mapped: Vec<Value> = models
            .into_iter()
            .map(|model| {
                let raw_id = model.get("id").map(value_text).unwrap_or_default();
                let lower_id = raw_id.to_lowercase();
                let is_auto = lower_id == "auto";
                let mut object = match model {
                    Value::Object(map) => map,
                    other => return other,
                };
                object.insert("isDefault".to_string(), Value::Bool(is_auto));
                let model_type = if enterprise {
                    if raw_id.starts_with(custom_prefix) { "enterprise" } else { "built-in" }
                } else if is_auto {
                    "auto"
                } else {
                    "built-in"
                };
                object.insert("modelType".to_string(), Value::String(model_type.to_string()));
                Value::Object(object)
            })
            .collect();
        let ids: Vec<String> = mapped
            .iter()
            .filter_map(|model| model.get("id").map(value_text))
            .collect();
        let state = CatalogState {
            models: mapped,
            last_refreshed_at: logging::now_ms(),
            remote_refreshed: true,
        };
        self.write(state);
        // 远程清单首次落地时补一次 WorkBuddy 默认规则种子（默认只启用白名单内的
        // 模型，见 model_rules::seed_workbuddy_defaults）。必须放在写锁之外：种子
        // 要写 config.json，而「持锁期间不做任何 IO」是本目录的硬约束。
        if let Some(summary) = crate::server::core::model_rules::seed_workbuddy_defaults(&ids) {
            logging::log("[Models]", &summary);
        }
    }

    /// 按当前账号的凭证/端点/UA/出口刷新一次（路由层与启动流程的入口）。
    ///
    /// 「当前账号 = 队首的可用账号」（由 store 派生，与转发默认使用的账号一致），
    /// 这里直接用它的凭证与出口，保证模型目录与转发看到的是同一个账号。
    ///
    /// 返回 [`RefreshOutcome`]（本函数内部本来就算出了它，只是过去只用于打日志）：
    /// 自动路径不看返回值（照旧只写日志），**手动刷新路径**靠它如实汇报
    /// 「刷到了几个 / 为什么没刷」—— 见 `providers::adapter` 的 `refresh_models`
    /// 契约。取不到可用登录态、`/v3/config` 失败或返回的清单不含对话模型，
    /// 都走 `not_refreshed(原因)`，既有行为（保留现有清单 + verbose 日志）不变。
    pub async fn refresh_with_current_account(
        &self,
        store: &crate::server::core::account_store::AccountStore,
        auth: &crate::server::core::auth::AuthService,
    ) -> RefreshOutcome {
        // 队首账号优先（多账号时与转发一致）；没有账号时回落到默认登录态
        // （环境变量 WORKBUDDY_TOKEN 或单账号 auth.json）
        let (session, proxy) = match store.get_current_entry() {
            Some(entry) => {
                let proxy = crate::server::core::proxies::session_proxy(&entry.session);
                (Some(entry.session), proxy)
            }
            None => match auth.get_current_session().await {
                Ok(session) => (session, None),
                Err(error) => {
                    logging::verbose("[Models]", &format!("模型目录刷新失败: {}", error.message));
                    return RefreshOutcome::not_refreshed(error.message);
                }
            },
        };
        let Some(session) = session else {
            logging::verbose("[Models]", &format!("模型目录未更新: {REASON_NO_SESSION}"));
            return RefreshOutcome::not_refreshed(REASON_NO_SESSION);
        };

        let endpoint = session
            .get("endpoint")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(normalize_endpoint)
            .unwrap_or_else(|| normalize_endpoint(auth.default_context().base_url.as_str()));
        let enterprise_id = session
            .get("account")
            .and_then(|account| account.get("enterpriseId"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let headers = crate::server::core::auth::AuthService::build_auth_headers(&session);
        let user_agent = user_agent_for_edition(session.get("edition").and_then(Value::as_str));

        let outcome = self
            .refresh(RefreshParams {
                endpoint: &endpoint,
                enterprise_id: Some(&enterprise_id),
                headers: &headers,
                user_agent: &user_agent,
                proxy: proxy.as_ref(),
            })
            .await;
        if outcome.refreshed {
            logging::log(
                "[Models]",
                &format!("✅ 模型目录已更新（{} 个，来源 {}）", outcome.count, outcome.source),
            );
        } else {
            logging::verbose("[Models]", &format!("模型目录未更新: {}", outcome.reason));
        }
        outcome
    }
}

/// 刷新入参（对应 Node 版 refresh 的 options 对象）
pub struct RefreshParams<'a> {
    pub endpoint: &'a str,
    pub enterprise_id: Option<&'a str>,
    pub headers: &'a [(String, String)],
    pub user_agent: &'a str,
    pub proxy: Option<&'a ResolvedProxy>,
}

/// 刷新结果：成功 `{refreshed:true, count, source}`，
/// 失败 `{refreshed:false, reason}`（与 Node 版返回值同形）。
#[derive(Clone, Debug)]
pub struct RefreshOutcome {
    pub refreshed: bool,
    pub count: usize,
    pub source: String,
    pub reason: String,
}

impl RefreshOutcome {
    pub fn refreshed(count: usize, source: &str) -> Self {
        Self {
            refreshed: true,
            count,
            source: source.to_string(),
            reason: String::new(),
        }
    }

    pub fn not_refreshed(reason: impl Into<String>) -> Self {
        Self {
            refreshed: false,
            count: 0,
            source: String::new(),
            reason: reason.into(),
        }
    }
}

/// 对照 JS 的 `encodeURIComponent`（企业 id 进 URL 路径前编码）
fn encode_uri_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        let ch = *byte as char;
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '!' | '~' | '*' | '\'' | '(' | ')')
        {
            out.push(ch);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// 供 /api/endpoints 与排障展示：内置清单（不含运行时刷新结果）。
/// Node 版 workbuddy-models.mjs 同名导出 `builtinModels()` 的对等物
/// （Node 侧注释「供 CLI/调试」，本进程没有 CLI，故只作排障口保留）。
#[allow(dead_code)]
pub fn builtin_models_snapshot() -> Value {
    Value::Array(builtin_models())
}

// ─── 进程级目录句柄（聚合目录的读取口）──────────────────────

/// 进程级 workbuddy 目录句柄。**为什么要全局**：聚合目录
/// （`core::providers::catalog`）的公开 API 只收 `&AccountStore`（handler 手边
/// 就有），不该再要求调用方层层传一个 ModelCatalog —— 那会把 `list_models` 与
/// 将来 chat_completions 的签名都改一遍。全局句柄与 `config::current()` /
/// `core::desensitize::global()` 是同一模式：启动时装一次，之后所有模块共用。
///
/// 取用方式是 `get_or_init`：`ServerState::bootstrap` 与聚合层拿到的是**同一个
/// 实例**（共享同一把 RwLock），即使 bootstrap 被调用多次（重启路径）也不会
/// 出现「ServerState 里刷新了、聚合层读的是另一份旧目录」这种分叉。
static GLOBAL: OnceLock<ModelCatalog> = OnceLock::new();

/// 取进程级目录句柄；首次调用即构造（内置清单，不读盘、不刷新）。
///
/// 构造顺序无关：任何模块在启动流程的任何阶段调它都能拿到一个可用的目录
/// （最坏情况是「还没被 `/v3/config` 刷新过」的内置清单，与改造前的初始态一致）。
pub fn global_catalog() -> ModelCatalog {
    GLOBAL.get_or_init(ModelCatalog::new).clone()
}
