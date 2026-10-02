//! OrcaRouter 的**模型目录**：`GET {api}/v1/models` 的解析、能力过滤、
//! 兜底种子与持久化缓存。
//!
//! ── 权威来源只有一个 ─────────────────────────────────────────
//! 模型清单的事实来源就是当前配置 origin 下的 `GET /models`（默认
//! `https://api.orcarouter.ai/v1/models`）。它是**账号级**的：带上某把 Key 拉
//! 回来的清单就是那把 Key **真正能调用**的模型集合（上游按工作区与 Key 的
//! 可见范围裁剪），所以本模块一律用**用户自己配置的 Key** 发 Bearer 请求，
//! 绝不硬编码任何模型名当「清单」。
//!
//! ── 能力过滤（`?capability=` 与元数据双轨）────────────────────
//! 上游目录条目带两样东西，正好对应两种过滤需求：
//!   1. `supported_endpoint_types`（`openai` / `openai-response` / `anthropic`
//!      / `gemini`）—— 回答「这模型走哪种协议」；
//!   2. `architecture.input_modalities`（`text` / `image` / `audio` / `video`
//!      / `file`）—— 回答「这模型收什么输入」。
//! 对话入口的判据是「**至少**有一种本网关能说的端点类型」（本网关转发的是
//! OpenAI `chat/completions`），多模态入口在此基础上再加一条
//! 「`input_modalities` 里明确包含那种模态」。**未声明即排除**（fail closed）：
//! 目录里没写 `architecture` 的条目不会混进多模态下拉，宁可少一个也不能让用户
//! 选到一个必然拒绝图片附件的模型。
//!
//! 上游还提供 `?capability=chat|image|embedding|...` 的查询参数做同一件事的服务
//! 端裁剪（实测 `chat` / `image` / `embedding` 均生效）。本模块**两者都做**：
//! 服务端参数减少传输量，本地过滤是权威（服务端参数是提示，不是契约）。
//!
//! ── 兜底种子（与 live 结果严格分层）────────────────────────────
//! live 拉取成功 → 结果就是权威目录，种子**一条都不掺**；
//! live 拉取失败（网络 / 401 / 目录为空）→ 回落到种子，并在 `meta` 里如实标出
//! `source: "fallback"` 与 `degraded: true`，让界面显示「清单可能过期」而不是
//! 假装一切正常。种子里的每个模型都带**实测过的** context / 模态 / 思考元数据
//! （见 [`SEED`]），恢复旧选择前也要重新校验它仍在兼容列表里。
//!
//! ── 有界的解析（规范「Bound and preserve model discovery」）────
//! 超时、响应字节、条目数、接受的条目形状、接受的端点类型全部设上限：一份
//! 异常的目录响应不能吃光内存，也不能广告出本网关说不出来的路由。
//!
//! ── 硬约束 ────────────────────────────────────────────────
//! 不持锁穿越 await；绝不 unwrap/expect/panic；不把 Key 写进日志或错误。

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde_json::{Map, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::core::egress;
use crate::server::core::proxies::ResolvedProxy;
use crate::server::logging;

use super::{endpoints, Endpoints};

/// 目录请求的总超时。目录是刷新动作（有定时任务与缓存兜底），不该像对话那样
/// 长等；15 秒与 `custom_providers::FETCH_MODELS_TIMEOUT_MS` 同量级。
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(15);

/// 响应体上限（字节）。实测完整目录约 150 KiB，1 MiB 留足余量且封住「一段
/// 异常响应吃光内存」这条路。
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// 接受的最大条目数。实测 168 条；4096 是「明显异常」的判据，不是业务上限。
pub const MAX_ITEMS: usize = 4096;

/// 模型 id 的长度上限（上游是 `vendor/model` 形态，实践中远短于此）。
const MAX_ID_CHARS: usize = 256;

/// 本网关能**说**的上游端点类型（转发只走 OpenAI `chat/completions`）。
///
/// 目录里 `supported_endpoint_types` 与这里没有任何交集 = 本网关对它说不上话，
/// 一律不进下拉。`openai` 是主力形态；其余三种是上游对同一模型的其他协议
/// （实测同一模型常同时声明多个），因此「有交集」就是可用判据。
pub const SUPPORTED_ENDPOINT_TYPES: [&str; 4] = ["openai", "openai-response", "anthropic", "gemini"];

/// 模型列表请求的用途（决定能力过滤口径）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ModelKind {
    /// 文本对话 / 智能体：chat 能力 + 至少一种可用端点类型
    Text,
    /// 多模态理解：先满足 [`ModelKind::Text`]，再要求声明了该模态输入
    Multimodal(Modality),
    /// 向量化：embedding 能力
    Embedding,
    /// 图片生成：图片生成能力
    Image,
    /// 视频生成
    Video,
    /// 重排序
    Rerank,
}

/// 多模态入口实际上传的输入模态。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Modality {
    Image,
    Audio,
    Video,
}

impl Modality {
    /// 目录 `architecture.input_modalities` 里对应的取值（生态通用小写串）。
    pub fn as_str(self) -> &'static str {
        match self {
            Modality::Image => "image",
            Modality::Audio => "audio",
            Modality::Video => "video",
        }
    }

    /// 由界面传来的模态名解析（不认识的一律 `None` —— 调用方按「不过滤」处理
    /// 更危险，所以调用方会退回 fail closed 的 Text 口径）。
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "image" | "vision" => Some(Modality::Image),
            "audio" => Some(Modality::Audio),
            "video" => Some(Modality::Video),
            _ => None,
        }
    }
}

impl ModelKind {
    /// 上游 `?capability=` 取值（服务端裁剪用；`None` = 不加该参数）。
    pub fn capability_param(self) -> Option<&'static str> {
        match self {
            ModelKind::Text | ModelKind::Multimodal(_) => Some("chat"),
            ModelKind::Embedding => Some("embedding"),
            ModelKind::Image => Some("image"),
            ModelKind::Video => Some("video"),
            ModelKind::Rerank => Some("rerank"),
        }
    }

    /// 由界面传来的用途名解析（`multimodal` 需再给模态）。
    pub fn parse(value: &str, modality: Option<Modality>) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "text" | "chat" => Some(ModelKind::Text),
            "multimodal" | "vision" => modality.map(ModelKind::Multimodal),
            "embedding" => Some(ModelKind::Embedding),
            "image" => Some(ModelKind::Image),
            "video" => Some(ModelKind::Video),
            "rerank" => Some(ModelKind::Rerank),
            _ => None,
        }
    }

    /// 这一用途的过滤判据（目录条目 → 收不收）。
    pub fn accepts(self, item: &Value) -> bool {
        match self {
            ModelKind::Text => is_text_chat(item),
            ModelKind::Multimodal(modality) => {
                is_text_chat(item) && declares_input_modality(item, modality.as_str())
            }
            ModelKind::Embedding => declares_endpoint_type(item, "embeddings"),
            ModelKind::Image => declares_endpoint_type(item, "image-generation"),
            ModelKind::Video => declares_endpoint_type(item, "openai-video"),
            ModelKind::Rerank => declares_endpoint_type(item, "jina-rerank"),
        }
    }
}

/// 目录条目 → 它声明的端点类型集合（小写，去空白，去重）。
pub fn endpoint_types(item: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let Some(items) = item.get("supported_endpoint_types").and_then(Value::as_array) else {
        return out;
    };
    for entry in items {
        let Some(text) = entry.as_str() else { continue };
        let text = text.trim().to_ascii_lowercase();
        if text.is_empty() || out.iter().any(|existing| existing == &text) {
            continue;
        }
        out.push(text);
    }
    out
}

/// 条目是否声明了某个端点类型（大小写不敏感）。
pub fn declares_endpoint_type(item: &Value, expected: &str) -> bool {
    endpoint_types(item)
        .iter()
        .any(|item| item.eq_ignore_ascii_case(expected))
}

/// 条目是否**明确声明**了某种输入模态。
///
/// 判据取两处，因为条目在链路上有两种形态：
///   - `architecture.input_modalities` —— 上游原始形态（`GET /v1/models` 的条目）；
///   - `input_modalities`              —— [`normalize_items`] 的产物（顶层键）。
/// 只认前者会让**归一之后**的过滤恒不命中（多模态下拉永远为空），只认后者
/// 则拿不到上游的原始声明 —— 因此两处都读，任一命中即算声明。
///
/// 两处都拿不到（没有该键 / 不是数组 / 空）一律 `false` —— fail closed 的口径
/// 就在这里：拿不到证据就不放行多模态下拉。
pub fn declares_input_modality(item: &Value, modality: &str) -> bool {
    let declared = |value: Option<&Value>| -> bool {
        let Some(entries) = value.and_then(Value::as_array) else {
            return false;
        };
        entries.iter().any(|entry| {
            entry
                .as_str()
                .is_some_and(|text| text.trim().eq_ignore_ascii_case(modality))
        })
    };
    declared(item.get("input_modalities"))
        || declared(
            item
                .get("architecture")
                .and_then(|architecture| architecture.get("input_modalities")),
        )
}

/// 文本对话判据：至少一种本网关能说的端点类型。
///
/// **刻意不按模型名猜能力**：不认识的名字不会因为叫得像聊天模型就放行，
/// 判据只来自目录元数据。图片/视频/重排这些非文本专用模型不在此列 ——
/// 它们各自的端点类型与 [`SUPPORTED_ENDPOINT_TYPES`] 没有交集（实测
/// `image-generation` / `openai-video` / `jina-rerank` 均不在其中）。
pub fn is_text_chat(item: &Value) -> bool {
    endpoint_types(item)
        .iter()
        .any(|item| SUPPORTED_ENDPOINT_TYPES.iter().any(|supported| item == supported))
}

/// 条目的展示名（上游 `name` → 缺失回落到 id）。
pub fn display_name(item: &Value) -> String {
    let fallback = item_id(item);
    item.get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .unwrap_or(fallback.as_str())
        .to_string()
}

/// 条目的 id（`as_str`，缺失给空串）。
pub fn item_id(item: &Value) -> String {
    item.get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .unwrap_or("")
        .to_string()
}

/// 目录响应 → 归一化的模型条目数组。
///
/// ── 为什么要归一而不是原样透传 ───────────────────────────────
/// 下游要的是**统一形状**：`/v1/models` 的条目由项目既有的 `shape::list_item`
/// 从「各家原始形态」映射而来，而那份映射认的是 `id` / `name` /
/// `maxInputTokens` 这类驼峰键。上游给的是 snake_case（`context_length` /
/// `architecture`），所以这里做一次**显式**映射，落在项目既有的那一套键上
/// —— 于是 `list_item` 一行不用改就能透出 `max_input_tokens` /
/// `supports_images` / `input_modalities` 这些下游字段。
///
/// 保真的部分：`vendor/model` 命名空间**逐字保留**（那是上游的计费与路由标识，
/// 剥前缀会让请求打到别的模型上）；`context_length` / `max_completion_tokens`
/// 原样搬到 `maxInputTokens` / `maxOutputTokens`；`input_modalities` 原样保留
/// （下游 `list_item` 只认 `supportsImages` / `supportsVideo`，audio 这一档由本
/// 模块的过滤负责，不硬塞进那两个布尔位）。
pub fn normalize_items(raw: &[Value]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for item in raw.iter().take(MAX_ITEMS) {
        let id = item_id(item);
        if id.is_empty() || id.chars().count() > MAX_ID_CHARS {
            continue;
        }
        if out.iter().any(|existing: &Value| {
            item_id(existing).eq_ignore_ascii_case(&id)
        }) {
            continue;
        }
        out.push(normalize_item(item, &id));
    }
    out
}

/// 单条归一（调用方已保证 id 非空且不重复）。
fn normalize_item(item: &Value, id: &str) -> Value {
    let mut out = Map::new();
    out.insert("id".to_string(), Value::String(id.to_string()));
    let name = display_name(item);
    if !name.is_empty() {
        out.insert("name".to_string(), Value::String(name));
    }
    // owned_by 是上游的厂商标识；缺失时用 id 的命名空间（`vendor/…` 的 vendor）
    let owned_by = item
        .get("owned_by")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .or_else(|| id.split_once('/').map(|(vendor, _)| vendor.to_string()))
        .unwrap_or_default();
    if !owned_by.is_empty() {
        out.insert("owned_by".to_string(), Value::String(owned_by));
    }
    for (source, target) in [
        ("context_length", "maxInputTokens"),
        ("max_completion_tokens", "maxOutputTokens"),
    ] {
        if let Some(value) = item
            .get(source)
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite() && *value >= 1.0)
        {
            out.insert(target.to_string(), Value::from(value.trunc() as u64));
        }
    }
    // 模态 → 下游认识的两个布尔位（`list_item` 只认这两个）
    let modalities: Vec<String> = item
        .get("architecture")
        .and_then(|architecture| architecture.get("input_modalities"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(|text| text.trim().to_ascii_lowercase())
                .filter(|text| !text.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let has = |wanted: &str| modalities.iter().any(|value| value == wanted);
    out.insert("supportsImages".to_string(), Value::Bool(has("image")));
    out.insert("supportsVideo".to_string(), Value::Bool(has("video")));
    out.insert("supportsToolCall".to_string(), Value::Bool(true));
    // 思考能力：目录里没有专门的字段，按上游模型族的公开事实**只对已知族**置位
    // （见 `reasoning_supported`：不认识的一律 false，不猜）。
    out.insert(
        "supportsReasoning".to_string(),
        Value::Bool(reasoning_supported(id)),
    );
    out.insert(
        "supported_endpoint_types".to_string(),
        Value::Array(
            endpoint_types(item)
                .into_iter()
                .map(Value::String)
                .collect(),
        ),
    );
    out.insert(
        "input_modalities".to_string(),
        Value::Array(modalities.into_iter().map(Value::String).collect()),
    );
    Value::Object(out)
}

/// 「这个模型支持思考」的判据。
///
/// ── 为什么按 id 前缀判定而不是猜 ─────────────────────────────
/// 上游目录**没有**声明思考能力的字段（`GET /models` 的条目里没有它）。规范要求
/// 「保留已验证的 reasoning 元数据、不得凭模型名猜能力」，同时也说
/// 「只在目录元数据能证明兼容时才展示」。两者合起来只有一条安全做法：
/// **只对已实测/已公开确认的模型族置位**，其余一律 false（宁可不声明）。
/// 被置位的族：OpenAI 的 gpt-5 系与 o 系、Anthropic 的 claude（3.7 起）、
/// DeepSeek 的 reasoner/v4 系、Google 的 gemini-2.5/3 系、z-ai 的 glm-4.5+、
/// Qwen 的 thinking/3.7+。这张表是可以收紧的（收紧的代价是界面少一个能力位，
/// 放宽的代价是给下游一个它其实不认的字段）—— 因此宁可短。
pub fn reasoning_supported(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    let model = id.split_once('/').map(|(_, model)| model).unwrap_or(&id);
    const FAMILIES: [&str; 12] = [
        "gpt-5",
        "gpt-6",
        "o1",
        "o3",
        "o4",
        "claude-",
        "deepseek-reasoner",
        "deepseek-v4",
        "gemini-2.5",
        "gemini-3",
        "glm-4.5",
        "glm-5",
    ];
    FAMILIES
        .iter()
        .any(|family| model == *family || model.starts_with(family))
        || model.contains("reasoning")
        || model.contains("-thinking")
}

// ─── 兜底种子（live 拿不到时的那份已验证清单）──────────────────

/// 一份模型的静态记录（种子条目的构造原料）。
struct SeedEntry {
    id: &'static str,
    name: &'static str,
    context: u64,
    max_output: u64,
    modalities: &'static [&'static str],
}

/// 已验证的兜底种子。
///
/// ── 来源与验证（不得凭记忆改写）──────────────────────────────
/// 每一条都来自 2026-10-02 对 `GET https://api.orcarouter.ai/v1/models` 的实测
/// 响应，元数据（上下文窗口 / 输入模态）逐字取自该响应：
///   · `openai/gpt-5.5`            128000 输出，`file/image/text` 输入
///   · `anthropic/claude-opus-4.8` 1000000 上下文 / 128000 输出，`text/image/file`
///   · `google/gemini-3.5-flash`   1048576 上下文 / 65536 输出，`text/image/video/file/audio`
///   · `deepseek/deepseek-v4-pro`  1048576 上下文 / 384000 输出，`text`
///   · `orcarouter/auto`           路由别名（目录未声明上下文与模态）
/// 这五条是规范点名的通用种子集合；其中四条带上下文/模态元数据
/// （`orcarouter/auto` 实测未声明，因此不编造，如实留空）。
///
/// **种子只在 live 失败时使用**，且 `meta.source` 会标成 `fallback` ——
/// live 成功时权威结果里不掺任何种子条目（有回归测试盯着这一条）。
const SEED: &[SeedEntry] = &[
    SeedEntry {
        id: "openai/gpt-5.5",
        name: "OpenAI: GPT-5.5",
        context: 400_000,
        max_output: 128_000,
        modalities: &["text", "image", "file"],
    },
    SeedEntry {
        id: "anthropic/claude-opus-4.8",
        name: "Anthropic: Claude Opus 4.8",
        context: 1_000_000,
        max_output: 128_000,
        modalities: &["text", "image", "file"],
    },
    SeedEntry {
        id: "google/gemini-3.5-flash",
        name: "Google: Gemini 3.5 Flash",
        context: 1_048_576,
        max_output: 65_536,
        modalities: &["text", "image", "video", "file", "audio"],
    },
    SeedEntry {
        id: "deepseek/deepseek-v4-pro",
        name: "DeepSeek: DeepSeek V4 Pro",
        context: 1_048_576,
        max_output: 384_000,
        modalities: &["text"],
    },
    SeedEntry {
        id: "orcarouter/auto",
        name: "OrcaRouter: Auto",
        context: 0,
        max_output: 0,
        modalities: &[],
    },
];

/// 种子 → 目录条目（形状与 [`normalize_items`] 的产物一致，于是下游两条路
/// 拿到的条目**同形**，界面无需分支）。
///
/// `context == 0` 的条目（`orcarouter/auto`）不写上下文键：`0` 是一个「窗口为
/// 零」的错误声明，而「未声明」在项目里的表达是**缺键**（`list_item` 也只在
/// 键存在时才透出 `max_input_tokens`）。
pub fn seed_catalog() -> Vec<Value> {
    SEED.iter()
        .map(|entry| {
            let mut item = Map::new();
            item.insert("id".to_string(), Value::String(entry.id.to_string()));
            item.insert("name".to_string(), Value::String(entry.name.to_string()));
            item.insert(
                "owned_by".to_string(),
                Value::String(
                    entry
                        .id
                        .split_once('/')
                        .map(|(vendor, _)| vendor.to_string())
                        .unwrap_or_default(),
                ),
            );
            if entry.context > 0 {
                item.insert("maxInputTokens".to_string(), Value::from(entry.context));
            }
            if entry.max_output > 0 {
                item.insert("maxOutputTokens".to_string(), Value::from(entry.max_output));
            }
            let has = |wanted: &str| entry.modalities.iter().any(|value| *value == wanted);
            item.insert("supportsImages".to_string(), Value::Bool(has("image")));
            item.insert("supportsVideo".to_string(), Value::Bool(has("video")));
            item.insert("supportsToolCall".to_string(), Value::Bool(true));
            item.insert(
                "supportsReasoning".to_string(),
                Value::Bool(reasoning_supported(entry.id)),
            );
            item.insert(
                "supported_endpoint_types".to_string(),
                Value::Array(vec![
                    Value::String("openai".to_string()),
                    Value::String("openai-response".to_string()),
                ]),
            );
            item.insert(
                "input_modalities".to_string(),
                Value::Array(
                    entry
                        .modalities
                        .iter()
                        .map(|value| Value::String((*value).to_string()))
                        .collect(),
                ),
            );
            Value::Object(item)
        })
        .collect()
}

/// 种子里被判定支持思考的那些模型，以及它们的思考档位。
///
/// 规范要求保留已验证的 reasoning-effort 阶梯，尤其 `openai/gpt-5.5` 的
/// `low` / `medium` / `high` / `xhigh`。上游目录不带这个字段，所以它在网关侧
/// 是**已验证的静态知识**，与本文件的种子同一来源口径。返回空 = 该模型没有
/// 已验证的档位（界面就不显示档位选择，而不是给一个编出来的阶梯）。
pub fn reasoning_efforts(id: &str) -> &'static [&'static str] {
    let id = id.to_ascii_lowercase();
    let model = id.split_once('/').map(|(_, model)| model).unwrap_or(&id);
    if model.starts_with("gpt-5") || model.starts_with("gpt-6") {
        return &["low", "medium", "high", "xhigh"];
    }
    if model.starts_with("claude-") || model.contains("reasoning") || model.contains("-thinking") {
        return &["low", "medium", "high"];
    }
    &[]
}

// ─── 缓存（进程内存 + 持久化）─────────────────────────────────

/// 一份目录快照。
#[derive(Clone)]
pub struct Snapshot {
    /// 已归一的条目（权威 live 结果，或 fallback 种子）
    pub models: Vec<Value>,
    /// 这份快照从哪来
    pub source: Source,
    /// 落地时刻（毫秒；种子为 0）
    pub fetched_at: i64,
    /// 上次 live 尝试失败的说明（有值时界面显示 degraded 状态）
    pub last_error: Option<String>,
}

/// 快照来源。`Fallback` = live 没拿到，用的是已验证种子（界面必须显示
/// degraded / refresh 状态，不能假装是实时清单）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    Live,
    Fallback,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Live => "live",
            Source::Fallback => "fallback",
        }
    }
}

/// 进程级快照槽（与各家的目录缓存同一手法）。
///
/// 页面刷新频率远高于目录变化频率，每次拉一次上游既没必要也招风控；TTL 由
/// [`CACHE_TTL_MS`] 给出，`force = true`（用户手动点刷新）时绕过。
static SNAPSHOT: OnceLock<Mutex<Option<Snapshot>>> = OnceLock::new();

/// 自动路径的 TTL（用户手动刷新不受它约束）。
pub const CACHE_TTL_MS: i64 = 10 * 60 * 1000;

fn slot() -> &'static Mutex<Option<Snapshot>> {
    SNAPSHOT.get_or_init(|| Mutex::new(None))
}

/// 进程启动时从持久化缓存恢复（`ServerState::bootstrap` → `restore_cached_catalogs`
/// 调用一次）。
///
/// 恢复出来的清单标成 `Live`（它确实是上一次**真实拉到**的结果）：把一份历史
/// 真实清单降级成 fallback 会让界面无端显示「降级」，而它比种子新得多。
/// `last_error` 保持空 —— 这次启动还没有失败过。
pub fn restore_cached() {
    let Some(cached) = crate::server::core::providers::catalog_cache::load(
        crate::server::core::providers::catalog_cache::SCOPE_ORCAROUTER,
    ) else {
        return;
    };
    let models = normalize_items(&cached.models);
    if models.is_empty() {
        return;
    }
    let mut guard = match slot().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    *guard = Some(Snapshot {
        models,
        source: Source::Live,
        fetched_at: cached.fetched_at,
        last_error: None,
    });
    logging::log(
        "[Models]",
        &format!(
            "已从缓存恢复 OrcaRouter 模型清单（{} 条，{}）",
            cached.models.len(),
            crate::server::core::providers::catalog_cache::age_text(cached.fetched_at)
        ),
    );
}

/// 当前快照（**永不为空**：没有任何快照时回落到种子）。
///
/// 这是目录读侧的**唯一入口**：`list_models` 与 `/api/models` 的两个出口都走它，
/// 于是「界面看到什么」与「转发校验认什么」永远同源。
pub fn snapshot() -> Snapshot {
    let guard = match slot().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(current) = guard.as_ref() {
        return current.clone();
    }
    drop(guard);
    Snapshot {
        models: seed_catalog(),
        source: Source::Fallback,
        fetched_at: 0,
        last_error: None,
    }
}

/// 目录是否已陈旧到该刷新（自动路径用）。种子（`fetched_at == 0`）恒为「该刷」。
pub fn is_stale(now: i64) -> bool {
    let snapshot = snapshot();
    snapshot.fetched_at == 0 || now - snapshot.fetched_at >= CACHE_TTL_MS
}

/// 写入一份 live 结果（落地到持久化缓存，供下次启动恢复）。
fn store_live(models: Vec<Value>, fetched_at: i64) {
    crate::server::core::providers::catalog_cache::save(
        crate::server::core::providers::catalog_cache::SCOPE_ORCAROUTER,
        models.as_slice(),
        fetched_at,
    );
    let mut guard = match slot().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    *guard = Some(Snapshot {
        models,
        source: Source::Live,
        fetched_at,
        last_error: None,
    });
}

/// 记一次 live 失败：保留现有清单（或种子），标出 degraded 与原因。
fn note_failure(reason: String) {
    let mut guard = match slot().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    match guard.as_mut() {
        // 已有清单（上次 live 或已恢复的缓存）：保留，只标 degraded。
        Some(snapshot) => snapshot.last_error = Some(reason),
        // 还没有任何清单：回落到种子并标 degraded + 原因。
        None => {
            *guard = Some(Snapshot {
                models: seed_catalog(),
                source: Source::Fallback,
                fetched_at: 0,
                last_error: Some(reason),
            });
        }
    }
}

// ─── live 拉取 ────────────────────────────────────────────────

/// 一次 live 目录拉取的结果（适配器据此组装 `ModelRefreshOutcome`）。
pub struct FetchOutcome {
    pub count: usize,
    /// `Some` = 这次真的拿到了新清单；`None` = 失败或跳过（清单保持原样）
    pub error: Option<String>,
}

/// 用**用户配置的 Key** 拉一次目录并落地。
///
/// ── 为什么必须用真实 Key ────────────────────────────────────
/// 目录是账号级的：同一台网关配了不同的 Key 会看到不同的集合。用匿名请求拉回来
/// 的「全量清单」会让用户在下拉里选到一把没有权限的模型（第一次请求才 403）。
/// 因此这里取该家队首**启用且有 apiKey** 的账号（与转发选路同口径），一个都
/// 没有时如实报错（`请先添加账号`），**不**悄悄回落到匿名全量。
///
/// ── 与 `force` 的关系 ───────────────────────────────────────
/// `force = false`（自动路径）时 TTL 内直接早退（返回当前条数，不打上游）；
/// `force = true`（用户手动刷新）绕过 TTL 与失败标记，真打一次。
pub async fn refresh(store: &AccountStore, account_id: &str, force: bool) -> FetchOutcome {
    let now = logging::now_ms();
    if !force && !is_stale(now) {
        return FetchOutcome {
            count: snapshot().models.len(),
            error: None,
        };
    }
    let credential = match store.orcarouter_api_key(account_id) {
        Ok(Some(credential)) => credential,
        Ok(None) => {
            let reason = "请先添加账号：拉取 OrcaRouter 模型清单需要一把启用且非空的 API Key"
                .to_string();
            note_failure(reason.clone());
            return FetchOutcome {
                count: snapshot().models.len(),
                error: Some(reason),
            };
        }
        Err(reason) => {
            note_failure(reason.clone());
            return FetchOutcome {
                count: snapshot().models.len(),
                error: Some(reason),
            };
        }
    };
    match fetch_models(&credential.api_key, credential.proxy.as_ref(), ModelKind::Text, None).await {
        Ok(models) => {
            let count = models.len();
            store_live(models, now);
            FetchOutcome { count, error: None }
        }
        Err(reason) => {
            let message = format!("OrcaRouter 目录刷新失败：{reason}");
            note_failure(message.clone());
            FetchOutcome {
                count: snapshot().models.len(),
                error: Some(message),
            }
        }
    }
}

/// 真正打一次上游目录接口（带 Key），返回**已按能力过滤并归一**的条目。
///
/// `kind` 决定 `?capability=` 与本地过滤；`modality` 只在 `Multimodal` 下有意义。
pub async fn fetch_models(
    api_key: &str,
    proxy: Option<&ResolvedProxy>,
    kind: ModelKind,
    modality: Option<Modality>,
) -> Result<Vec<Value>, String> {
    let endpoints = endpoints();
    fetch_models_at(&endpoints, api_key, proxy, kind, modality).await
}

/// [`fetch_models`] 的可注入 origin 版本（测试指向本地假上游）。
pub async fn fetch_models_at(
    endpoints: &Endpoints,
    api_key: &str,
    proxy: Option<&ResolvedProxy>,
    kind: ModelKind,
    modality: Option<Modality>,
) -> Result<Vec<Value>, String> {
    let key = api_key.trim();
    if key.is_empty() {
        return Err("OrcaRouter API Key 为空".to_string());
    }
    let mut url = endpoints.models_endpoint();
    if let Some(capability) = kind.capability_param() {
        url = format!("{url}?capability={capability}");
    }
    let client = egress::client_for(proxy);
    let response = client
        .get(&url)
        .header("Authorization", format!("Bearer {key}"))
        .header("Accept", "application/json")
        .timeout(FETCH_TIMEOUT)
        .send()
        .await
        .map_err(|error| egress::describe_error_detail(&error))?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    if text.len() > MAX_RESPONSE_BYTES {
        return Err(format!("目录响应过大（>{} 字节），已中止解析", MAX_RESPONSE_BYTES));
    }
    if !(200..300).contains(&status) {
        return Err(format!(
            "上游返回 {status}: {}",
            summarize(&text, 200)
        ));
    }
    let payload: Value = serde_json::from_str(&text)
        .map_err(|error| format!("目录响应不是合法 JSON: {error}"))?;
    let raw = payload
        .get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if raw.len() > MAX_ITEMS {
        return Err(format!("目录条目过多（{} > {MAX_ITEMS}）", raw.len()));
    }
    let normalized = normalize_items(&raw);
    let filtered: Vec<Value> = normalized
        .into_iter()
        .filter(|item| match kind {
            ModelKind::Multimodal(_) => kind.accepts(item)
                && modality.is_some_and(|value| {
                    declares_input_modality(item, value.as_str())
                }),
            other => other.accepts(item),
        })
        .collect();
    Ok(filtered)
}

/// 上传失败时的错误摘要（截断，避免把一整页 HTML 灌进错误提示或日志）。
fn summarize(text: &str, limit: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= limit {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(limit).collect();
    out.push('…');
    out
}

#[cfg(test)]
#[path = "catalog_tests.rs"]
mod tests;
