//! Cline 模型目录：两个额度池 + 远程刷新 + 对下游的友好名。
//!
//! ── 上游模型 id 的形状（实测，2026-09）───────────────────────
//! `GET {apiBase}/ai/cline/recommended-models` 返回四个组（实测）：
//!
//! ```jsonc
//! {
//!   "recommended": [{"id":"openai/gpt-6-astra","name":"gpt-6-astra",...}, ...],  // 4 条
//!   "free":        [{"id":"cline-free/deepseek-v4.1-flash","name":"Deepseek-v4.1-Flash",...},
//!                   {"id":"z-ai/glm-5.3-flash","name":"glm-5.3-flash",...}, ...],  // 5 条，**混着无前缀条目**
//!   "clinePass":   [{"id":"cline-pass/glm-5.3","name":"cline-pass/glm-5.3",...}, ...], // 14 条
//!   "clineCloud":  [{"id":"cline-cloud/glm-5.3",...}]                                  // 3 条
//! }
//! ```
//!
//! **归池一律以响应里的分组为准**（`free` 组 → 免费池、`clinePass` 组 → 订阅池），
//! 不按 id 前缀猜 —— 理由见下面第 2 条。
//!
//! ── 三组模型怎么取舍（这是本模块最需要想清楚的一件事）────────
//!
//!   1. **`clinePass`（订阅池）**：id 一律带 `cline-pass/` 前缀。
//!      这是 ClinePass 订阅用户的实际可用集，也是我们主要在意的那个池。
//!   2. **`free`（免费池）**：**只有部分条目带 `cline-free/` 前缀** ——
//!      实测 5 条里有 2 条是别家的裸 id（`z-ai/glm-5.3-flash`、
//!      `poolside/laguna-s-2.1:free`）。那两条**同样是免费池成员**（实测用
//!      免费账号打它们返回 200，走 OpenRouter 那类通道转售），只是上游用了
//!      承载方的裸 id 而没加自己的通道前缀。**因此归池必须看分组，不能看前缀**
//!      —— 早先按 `pool_of(id)` 猜前缀，把这 2 条错判进订阅池，而订阅池没有
//!      账号时整家不广告，于是它们在界面上**彻底消失**（只显示 3 条的那个 bug）。
//!   3. **`recommended`**：上游的「推荐位」（4 条），与上面两组**有重叠**
//!      （如 `moonshotai/kimi-k3` 与 pass 池的 `cline-pass/kimi-k3` 是同一个模型
//!      的不同通道）。收录它们会让 `/v1/models` 里出现一批「同一个模型的两种
//!      写法」，而它们的计费通道不同、可用性也不同 —— 用户点名哪一个就走哪一个，
//!      这符合本项目「不静默替换模型」的既有纪律（README 的模型名契约），
//!      所以照收。这一组**没有池归属信息**（上游只按「推荐」分组，不分池），
//!      只能按 id 前缀兜底判池 —— 它们实测走 credit 计费（402 余额不足），
//!      与两个池都不同，判进哪一池都只是「挂在哪家广告」的问题。
//!   4. **`clineCloud`**：实测打它返回 403（未开通），**不收录** ——
//!      广告一个必然 403 的模型只会让客户端在列表里选中它然后失败。
//!
//! ── 前缀必须原样发给上游（关键）─────────────────────────────
//! `cline-free/`、`cline-pass/` 这两个前缀**是计费通道选择器**，剥掉它们
//! 转发会被上游 404（实测：`model not found`）。所以：
//!   - 转发时（`adapter::build_chat_request`）用**完整 id**；
//!   - 对下游的友好名（剥前缀）由 **`model_rules` 的默认映射种子**给出，
//!     见 [`seed_cline_defaults`] —— 那是一条 alias → target 映射，
//!     客户端用 `deepseek-v4.1-flash` 请求、网关改写成
//!     `cline-free/deepseek-v4.1-flash` 再转发。这正是「不静默替换」的实现
//!     方式：映射是**显式可见**的（管理页里能看到、能删）。
//!
//! ── 与聚合目录的衔接 ────────────────────────────────────────
//! `list()` 输出**聚合层认的原始形态**（`id` / `name` / `maxInputTokens` /
//! `supportsImages` / `supportsReasoning`，见 `core/models/shape.rs`），
//! 不自己拼 `/v1/models` 信封 —— 与小浣熊/AutoClaw 同一做法。
//!
//! ── 两池为什么是两家（本模块最重要的取舍）───────────────────
//! `cline-free` 与 `cline-pass` 在上游是**同一个账号上的两个额度池**，但本项目
//! 把它们当**两个提供商**接入：各自有独立的账号、清单、启停规则与映射。
//!
//! 早先的建模是「一家的一个选项」：账号记录带 `pool` 字段，池过滤按「账号库里
//! 有哪个池」决定广告哪一批。那套做法有三个说不通的地方：
//!   - 界面上一家 Cline 里混着两个池的模型，**还随账号变化**，
//!     「哪个池能用」在模型列表里完全看不出来；
//!   - 模型的启停规则键是「(provider, 模型 id)」，两个池共用一个 provider，
//!     关掉某家的一个模型会影响另一个池的同名模型；
//!   - 同名模型（`deepseek-v4.1-flash` 两池都有）的短名归属要靠「账号里有哪个
//!     池」来排序决定，规则随账号漂移。
//!
//! 拆成两家后池过滤变成**身份的固有属性**：`list(pool)` 只列这个池的模型，
//! 与账号状态无关。于是「加了 Cline Free 的账号才看到 Free 的模型」由既有的
//! `provider_available` 口径自然给出，不需要任何池联动逻辑。
//!
//! **拆分的只是身份，不是实现**：凭证格式、API 基址、错误分类、远程目录接口
//! （`GET {apiBase}/ai/cline/recommended-models` 拉一次就拿到两个池）全部共用，
//! 见 `adapter.rs` 的按池参数化。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：绝不 unwrap/expect/panic。

use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

use crate::server::logging;

use super::credentials;

/// 免费池前缀（上游 id 的通道标记之一）
pub const FREE_PREFIX: &str = "cline-free/";

/// 订阅池前缀（上游 id 的通道标记之一）
pub const PASS_PREFIX: &str = "cline-pass/";

/// 云池前缀（**已知但不收录**：实测 403，见模块头第 4 条）
pub const CLOUD_PREFIX: &str = "cline-cloud/";

/// 额度池。**一个池 = 一家提供商**（见模块头），所以这个枚举同时是
/// 「这家是谁」的定义点 —— 池与 provider id / `ProviderKind` 的互查都在这里，
/// 别处不要再写 `"cline-free"` 这类字面量。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pool {
    /// 免费额度池（`cline-free/...` 与免费组的无前缀条目）
    Free,
    /// ClinePass 订阅池（`cline-pass/...`）
    Pass,
}

impl Pool {
    /// 两个池（注册表顺序：Free 在前）。刷清单、遍历 provider 时用。
    pub const ALL: [Pool; 2] = [Pool::Free, Pool::Pass];

    /// 解析 `pool` / 提供商 id 的各种写法（容忍 `cline-free`、`free`、`Free`）。
    ///
    /// ── 缺省为什么是 `pass`（迁移期的历史口径）──────────────────
    /// 这个函数现在只为**读旧数据**服务：存量账号记录带着 `pool: "free"` /
    /// `"pass"` 字段（拆分前写入的），迁移时按它决定账号归哪一家。缺省 `pass`
    /// 沿用旧口径 —— 旧实现里字段缺失或值非法都回落 `pass`，迁移必须给出
    /// **逐字相同**的结果，否则用户升一次级就换了池。新写入的账号不带这个字段，
    /// 池由 provider id 决定。
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "free" | "cline-free" => Self::Free,
            _ => Self::Pass,
        }
    }

    /// 这个池现在是哪家 provider（拆分后的**唯一**池 → 身份映射）
    pub fn kind(self) -> crate::server::core::providers::ProviderKind {
        match self {
            Self::Free => crate::server::core::providers::ProviderKind::ClineFree,
            Self::Pass => crate::server::core::providers::ProviderKind::ClinePass,
        }
    }

    /// 这个池的 provider id（`"cline-free"` / `"cline-pass"`）
    pub fn provider_id(self) -> &'static str {
        crate::server::core::providers::kind_id(self.kind())
    }

    /// provider id → 池（`cline::` 之外的 id 返回 None）
    pub fn from_provider_id(provider_id: &str) -> Option<Self> {
        crate::server::core::providers::kind_from_id(provider_id).and_then(|kind| match kind {
            crate::server::core::providers::ProviderKind::ClineFree => Some(Self::Free),
            crate::server::core::providers::ProviderKind::ClinePass => Some(Self::Pass),
            _ => None,
        })
    }

    /// 上游 id 的**通道前缀**（`cline-free/` / `cline-pass/`）。
    ///
    /// 这个前缀必须原样发上游（是计费通道选择器），也是「某条映射属于哪个池」
    /// 的判据（见 `catalog::wire_target_for_provider`）。
    pub fn target_prefix(self) -> &'static str {
        match self {
            Self::Free => FREE_PREFIX,
            Self::Pass => PASS_PREFIX,
        }
    }
}

/// 一条模型条目（静态形态；远程刷新会整体替换）
#[derive(Clone, Debug)]
pub struct ModelEntry {
    /// 上游模型 id（**含池前缀**，原样转发）
    pub id: String,
    /// 展示名（上游 name，通常已是人类可读的）
    pub name: String,
    /// 归属池（由 id 前缀判定）
    pub pool: Pool,
    /// 上下文窗口（上游 recommended 接口不给，用模型家族常识兜底）
    pub context_window: i64,
    /// 是否支持思维链（Cline 的很多模型都带 reasoning 字段）
    pub reasoning: bool,
}

/// 进程级清单缓存（远程刷新落地在这里；没刷新过时为空，`list()` 回落静态表）
static REMOTE: OnceLock<Mutex<Option<Vec<ModelEntry>>>> = OnceLock::new();
/// 上一次成功刷新的时间（毫秒）
static REFRESHED_AT: OnceLock<Mutex<i64>> = OnceLock::new();

/// 静态兜底清单（远程刷新失败 / 尚未刷新时用）。
///
/// ── 为什么这几条 ────────────────────────────────────────────
/// 取自实测的 recommended-models 响应（2026-09）：订阅池 4 条主力 + 免费池 3 条，
/// 覆盖「装上就能用」的最低限度。**不抄全**：上游那个清单会变（模型上下架频繁），
/// 抄全了只会得到一份很快过时的表；这里只留稳定存在的主力模型，
/// 其余靠远程刷新补齐。
const FALLBACK_MODELS: &[(&str, &str, Pool, i64, bool)] = &[
    // (上游 id, 展示名, 归属池, 上下文窗口, 是否支持思维链)
    //
    // **池在这里显式写死**（不调 `pool_of` 猜）：表里那两条免费池的裸 id 正是
    // 按前缀猜会判错的那一类（见模块头第 2 条）。
    ( "cline-pass/glm-5.3",             "GLM-5.3 (ClinePass)",           Pool::Pass, 200_000, true),
    ( "cline-pass/kimi-k3",             "Kimi K3 (ClinePass)",           Pool::Pass, 256_000, true),
    ( "cline-pass/deepseek-v4.1-flash", "DeepSeek V4.1 Flash (ClinePass)", Pool::Pass, 1_000_000, true),
    ( "cline-pass/deepseek-v4-pro",     "DeepSeek V4 Pro (ClinePass)",   Pool::Pass, 128_000, true),
    ( "cline-pass/qwen3.8-max",         "Qwen3.8 Max (ClinePass)",       Pool::Pass, 256_000, true),
    ( "cline-free/deepseek-v4.1-flash", "DeepSeek V4.1 Flash (免费)",    Pool::Free, 1_000_000, true),
    ( "cline-free/muse-spark-1.3-contributor", "Muse Spark 1.3 (免费)", Pool::Free, 200_000, true),
    ( "cline-free/solar-pro4",          "Solar Pro 4 (免费)",            Pool::Free, 128_000, false),
    // 免费池的裸 id 两条（**不带 `cline-free/` 前缀**，见模块头第 2 条）。
    // 兜底表也收它们：上游目录接口拉不到时，它们同样是「装上就能用」的免费模型，
    // 少了这两条会让免费池在离线/未刷新时又退回「只有 3 个」的旧观感。
    // 上下文窗口取自 Cline 官方目录（`@cline/llms` 的 `cline` 块实测值）。
    ( "z-ai/glm-5.3-flash",             "GLM-5.3-Flash (免费)",          Pool::Free, 1_310_720, true),
    ( "poolside/laguna-s-2.1:free",     "Laguna S 2.1 (免费)",           Pool::Free, 262_144, true),
];

/// 上游 id 的**通道前缀** → 池（按 id 前缀猜的兜底判据）。
///
/// ── 它不再是归池的主判据（本次修复的要点）────────────────────
/// 远程目录的归池看**响应里的分组**（见 [`parse_recommended`]）：免费组里有
/// 不带 `cline-free/` 前缀的裸 id，按前缀猜会把它们错判进订阅池。
///
/// 保留本函数的三个用途：
///   - `recommended` 组没有池信息，只能按前缀兜底（那里带前缀的只有 pass）；
///   - **拆分迁移**的判据（`model_rules::cline::migrate_cline_split`）：存量账号
///     的 `pool` 字段当初就是按这个口径写的，迁移必须给出逐字相同的结果；
///   - 静态兜底的条目全部带前缀，按它判与分组判等价。
///
/// ── 兜底表优先于前缀（本次修复）─────────────────────────────
/// 前缀判不出来时先查 [`FALLBACK_MODELS`]：免费池那两条裸 id 的归属写在表里，
/// 查表比「无前缀一律 Pass」准 —— 而且**迁移与运行期因此同一口径**
/// （存量规则若把这两条判进 pass，迁移后它们会与运行期的归属不一致）。
///
/// 表里也查不到才回落 `Pass`：前缀都没带时无从判断通道，归 pass 是**保守**选择。
pub fn pool_of(model_id: &str) -> Pool {
    if model_id.starts_with(FREE_PREFIX) {
        return Pool::Free;
    }
    if let Some((_, _, pool, _, _)) = FALLBACK_MODELS
        .iter()
        .find(|(id, ..)| id.eq_ignore_ascii_case(model_id))
    {
        return *pool;
    }
    // 无前缀的 id 与 pass 前缀都归 pass 池。这条归属**同时是迁移的判据**：
    // 拆分前账号的 `pool` 字段只影响广告，而广告内容正是按这个函数分的，
    // 所以按它把旧账号归到 `cline-pass` 与「保持原样」等价。
    Pool::Pass
}

/// 上游 id → 对下游的**友好名**（这条模型该有的去前缀写法）；没有可剥的返回 None。
///
/// ── 按池判，而不是只看 id（本次修复）─────────────────────────
/// 免费池里混着**不带通道前缀**的条目（`z-ai/glm-5.3-flash`、
/// `poolside/laguna-s-2.1:free`，见模块头第 2 条）：它们的通道由分组给出，
/// id 上只有承载方的厂商前缀。这类条目同样该有一个能直接敲的短名字，于是：
///   - 带通道前缀（`cline-free/` / `cline-pass/` / `cline-cloud/`）→ 剥通道前缀；
///   - **免费池**里不带通道前缀、但带厂商前缀（`z-ai/…`）→ 剥厂商前缀；
///   - 其余（订阅池的无前缀条目、`recommended` 组的 `openai/gpt-6-astra` 这类）
///     → `None`。那里厂商前缀是**模型身份**（`openai/gpt-6-astra` 与
///     `anthropic/claude-opus-5` 是不同模型），剥掉会造出上游不存在的名字。
///
/// ── 为什么只在免费池剥厂商前缀（别类推到别处）─────────────────
/// `z-ai/glm-5.3-flash` 里的 `z-ai/` 是「这条免费额度由谁承载」，与
/// `cline-free/` 属于同一层含义（都是**通道**）；而 `openai/gpt-6-astra` 里的
/// `openai/` 是模型本身是谁（**身份**）。剥通道前缀才得到用户认得的名字，
/// 剥身份前缀会造出上游不存在的名字。免费池这 2 条恰好把承载方写在了通道位上。
///
/// ── 撞名（明确接受）─────────────────────────────────────────
/// 剥出来的短名可能与别家的原生 id 撞（`glm-5.3-flash` 同时是 CatPaw 的原生
/// id）。撞名在本项目的路由语义下是**正常**的：同一 alias 允许多家各一条映射、
/// 一起进候选链做主备（见 `model_rules` 模块头），不遮蔽任何东西 ——
/// 「这家也能接这个名字」这件事本来就该在候选链里体现出来。
pub fn friendly_alias(pool: Pool, model_id: &str) -> Option<&str> {
    let id = model_id.trim();
    if id.is_empty() {
        return None;
    }
    // 通道前缀优先：`cline-free/deepseek-v4.1-flash` → `deepseek-v4.1-flash`
    for prefix in [FREE_PREFIX, PASS_PREFIX, CLOUD_PREFIX] {
        if let Some(rest) = id.strip_prefix(prefix) {
            return Some(rest);
        }
    }
    // 免费池的裸厂商 id：`z-ai/glm-5.3-flash` → `glm-5.3-flash`。
    // 没有 `/` 的 id（上游本来就给的友好名）无需映射。
    if pool == Pool::Free {
        let (_, rest) = id.split_once('/')?;
        // 再剥变体后缀 `:free`（`poolside/laguna-s-2.1:free` → `laguna-s-2.1`）——
        // Cline 自己的展示名归一化也是这么做的（它的 `oK()` 就是去掉 `:free`
        // 与 ` (free)` 后缀）。留着后缀会得到一个客户端难敲的名字。
        let rest = rest.strip_suffix(":free").unwrap_or(rest);
        return (!rest.is_empty()).then_some(rest);
    }
    None
}

/// 上下文窗口兜底（上游 recommended 接口不给这个字段）。
///
/// 按模型名里能看出的家族给一个**保守**值：宁可小报（客户端会自己按需截断），
/// 不要大报（超了上游会报错，而客户端以为还有空间）。
///
/// ── 兜底表里有权威值的先查表 ────────────────────────────────
/// 表里那两条免费池裸 id 的窗口取自 Cline 官方目录（实测值），比按家族猜准得多
/// （`z-ai/glm-5.3-flash` 猜出来是 256K、实际 1.28M）。先查表还有第二个好处：
/// **远程与静态两条路径给出同一个数** —— 否则同一个模型刷新前后会报不同的窗口。
fn context_window_for(model_id: &str) -> i64 {
    if let Some((_, _, _, context, _)) = FALLBACK_MODELS
        .iter()
        .find(|(id, ..)| id.eq_ignore_ascii_case(model_id))
    {
        return *context;
    }
    let name = model_id.to_ascii_lowercase();
    if name.contains("deepseek-v4.1") || name.contains("1m") {
        1_000_000
    } else if name.contains("kimi") || name.contains("qwen3.8") || name.contains("glm-5") {
        256_000
    } else if name.contains("claude") || name.contains("gpt-6") || name.contains("grok") {
        200_000
    } else if name.contains("minimax") || name.contains("mimo") {
        1_000_000
    } else {
        128_000
    }
}

/// 是否支持思维链（实测这些模型都回 `reasoning` 字段；静态判断按家族给）。
fn reasoning_for(model_id: &str) -> bool {
    let name = model_id.to_ascii_lowercase();
    // 这些是实测明确有 reasoning 输出的；少数小模型没有
    !(name.contains("solar-pro4") || name.contains("flash-lite"))
}

/// 当前清单：远程刷新过就用它，否则用静态兜底。
///
/// **不做池过滤** —— 池过滤在 `list(pool)` 里（见那里）。远程接口一次拉回
/// 两个池，缓存也共用一份（两个 provider 只是各自过滤它），因此这里不需要
/// 「谁来拉」的信息。
pub fn entries() -> Vec<ModelEntry> {
    if let Some(remote) = REMOTE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .ok()
        .and_then(|guard| guard.clone())
    {
        if !remote.is_empty() {
            return remote;
        }
    }
    FALLBACK_MODELS
        .iter()
        .map(|(id, name, pool, context, reasoning)| ModelEntry {
            id: (*id).to_string(),
            name: (*name).to_string(),
            pool: *pool,
            context_window: *context,
            reasoning: *reasoning,
        })
        .collect()
}

/// 某条目 → 聚合层认的原始形态（`core/models/shape.rs::list_item` 的输入）。
fn to_raw(entry: &ModelEntry) -> Value {
    json!({
        "id": entry.id,
        "name": entry.name,
        // 聚合层认这两个键名（见 shape.rs 的模块头）
        "maxInputTokens": entry.context_window,
        "maxOutputTokens": 32_000,
        // Cline 的上游是 OpenAI 兼容的 agent 网关，工具调用与视觉都支持
        "supportsToolCall": true,
        "supportsImages": true,
        "supportsReasoning": entry.reasoning,
        "isDefault": false,
        "kind": "chat",
    })
}

/// 某个池的模型清单（供聚合目录 / 路由判定）。
///
/// ── 池过滤是身份，不是账号（拆分后的口径）────────────────────
/// 只列 `pool` 这个池的模型 —— 池由 provider 身份决定（`cline-free` 这家只看
/// 免费池、`cline-pass` 只看订阅池），**与账号状态无关**。早先那套「按账号库里
/// 有哪个池决定广告哪一批」的做法已经退场，见模块头。
///
/// 于是这份清单直接就是广告清单：某家没有可用账号时，它整个不进广告
/// （`active_manifests` 的既有语义，与另外六家一致），不需要本家再收窄一层。
pub fn list(pool: Pool) -> Vec<Value> {
    entries()
        .iter()
        .filter(|entry| entry.pool == pool)
        .map(to_raw)
        .collect()
}

/// 某个池的模型 id 列表（含静态兜底，顺序即清单顺序）。
///
/// 种子入口用：种子按 **(provider, id)** 记账，两家各自种自己那一批，
/// 所以这里必须只给本池的 id —— 混进另一个池的 id 会让「这家」的 seeded
/// 里出现不属于它的键。
pub fn ids_of(pool: Pool) -> Vec<String> {
    entries()
        .into_iter()
        .filter(|entry| entry.pool == pool)
        .map(|entry| entry.id)
        .collect()
}

/// 上一次成功远程刷新的时间（毫秒）；0 = 从未刷新过
pub fn last_refreshed_at() -> i64 {
    REFRESHED_AT
        .get_or_init(|| Mutex::new(0))
        .lock()
        .map(|guard| *guard)
        .unwrap_or(0)
}

/// 是否远程刷新过（聚合目录的 `meta.source` 用）
pub fn remote_refreshed() -> bool {
    REMOTE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map(|guard| guard.is_some())
        .unwrap_or(false)
}

/// 拉一次远程目录并落地。
///
/// ── 无鉴权也能拉（实测）─────────────────────────────────────
/// `GET {apiBase}/ai/cline/recommended-models` **不需要凭证**（实测无
/// Authorization 头也返回 200 完整清单）。这对「还没加账号但想看看有哪些模型」
/// 的场景很关键，也让刷新可以完全独立于账号状态。
///
/// ── 失败不改动现有清单 ──────────────────────────────────────
/// 与另外几家一致：拉不到就保留手上的（可能是静态兜底，也可能是上次的成功结果），
/// 并把原因回给调用方（`Err`），由适配器决定怎么汇报给用户。
pub async fn refresh() -> Result<usize, String> {
    let client = crate::server::core::egress::client_for(None);
    let url = format!(
        "{}/ai/cline/recommended-models",
        credentials::API_BASE_URL
    );
    let response = client
        .get(&url)
        .header("Accept", "application/json")
        .header("X-CLIENT-TYPE", super::adapter::CLIENT_TYPE)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|error| {
            format!(
                "请求失败: {}",
                crate::server::core::egress::describe_error_detail(&error)
            )
        })?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    if !(200..300).contains(&status) {
        return Err(format!("上游返回 {status}"));
    }
    let payload: Value = serde_json::from_str(&text).map_err(|error| format!("响应不是 JSON: {error}"))?;
    let parsed = parse_recommended(&payload);
    if parsed.is_empty() {
        return Err("上游清单为空（响应里没有任何可用模型）".to_string());
    }
    let count = parsed.len();
    let snapshot = parsed.clone();
    if let Ok(mut guard) = REMOTE.get_or_init(|| Mutex::new(None)).lock() {
        *guard = Some(snapshot);
    }
    if let Ok(mut guard) = REFRESHED_AT.get_or_init(|| Mutex::new(0)).lock() {
        *guard = logging::now_ms();
    }
    Ok(count)
}

/// 解析 recommended-models 响应 → 条目表（可单独调用，便于核对格式）。
///
/// 取舍见模块头：收 `clinePass` 与 `free`，收 `recommended`（按通道身份区分），
/// **不收 `clineCloud`**（实测 403）。
///
/// ── 归池看分组，不看前缀（本次修复）──────────────────────────
/// 遍历时把**分组本身**作为归池依据传下去：`free` 组一律进免费池、
/// `clinePass` 组一律进订阅池。早先这里丢掉分组、改调 `pool_of(id)` 按前缀猜，
/// 于是免费组里那 2 条裸 id 被错判进订阅池 —— 订阅池没有账号时整家不广告，
/// 它们在界面上直接消失（用户看到「免费只有 3 个」）。
///
/// `recommended` 组没有池信息（上游只按「推荐」分组），仍按前缀兜底判池，
/// 见模块头第 3 条。
pub fn parse_recommended(payload: &Value) -> Vec<ModelEntry> {
    let mut out: Vec<ModelEntry> = Vec::new();
    let push = |item: &Value, group: Option<Pool>, out: &mut Vec<ModelEntry>| {
        let Some(id) = item
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
        else {
            return;
        };
        if id.starts_with(CLOUD_PREFIX) {
            return;
        }
        if out.iter().any(|entry| entry.id == id) {
            return;
        }
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .unwrap_or(id);
        out.push(ModelEntry {
            id: id.to_string(),
            name: name.to_string(),
            // 分组给的就用分组，没给（recommended）才按前缀兜底
            pool: group.unwrap_or_else(|| pool_of(id)),
            context_window: context_window_for(id),
            reasoning: reasoning_for(id),
        });
    };
    // 组名 → 该组的池归属；`None` = 这组不携带池信息（按前缀兜底）
    for (key, group) in [
        ("clinePass", Some(Pool::Pass)),
        ("free", Some(Pool::Free)),
        ("recommended", None),
    ] {
        if let Some(items) = payload.get(key).and_then(Value::as_array) {
            for item in items {
                push(item, group, &mut out);
            }
        }
    }
    out
}

/// Cline 清单的**默认映射种子**（按池各自种，两家互不干涉）。
///
/// 给本池每个带前缀的模型加一条「去前缀」映射，让客户端可以用
/// `deepseek-v4.1-flash` 请求到 `cline-free/deepseek-v4.1-flash`。
///
/// ── 为什么用「映射」而不是「改写模型名」──────────────────────
/// 项目里已经有这套机制（`model_rules` 的 mappings），好处是：
///   - **显式可见**：管理页里能看到这条映射、能删掉，不是藏在代码里的隐式改写；
///   - **不违反「不静默替换」**：网关没有把 A 悄悄换成 B，而是「客户端说的名字
///     有一条公开的别名规则指向目标」；
///   - **与种子机制同一套**：处理过的 (provider, id) 记入 `seeded`，
///     用户手动删掉这条映射后，下一次清单刷新不会改回来。
///
/// ── 冲突处理（照抄小浣熊种子的三条纪律，实现在 `model_rules` 里）──
/// `model_rules::seed_cline_defaults` 与 `seed_raccoon_defaults` 同构：
///   1. alias 已被别的映射占用 → 跳过；
///   2. alias 与本池清单里另一个上游 id 撞名 → 跳过。**本池会撞**：
///      `deepseek-v4.1-flash` 两池都有，于是**先刷的那家**拿到这个短名
///      （两家现在各记各的 seeded，先到先得仍然成立）；
///   3. 空别名 / 非法别名 → 跳过。
///
/// **两家的短名都由 `model_rules::EXTRA_ALIASES` 点名**，所以无论哪家先刷，
/// `deepseek-v4.1-flash` 都有两条映射（各指一个池的 target），路由时一起进
/// 候选链、发送名跟着实际承载的 provider 走 —— 短名不会因为刷新顺序而
/// 「只认一个池」。同一个 alias 允许跨 provider 各一条是既有语义（与
/// 小浣熊的点号别名共存用的是同一套判重）。
///
/// 返回给日志的摘要；没有新动作时返回 None（不落盘）。
pub fn seed_cline_defaults(pool: Pool, ids: &[String]) -> Option<String> {
    crate::server::core::model_rules::seed_cline_defaults(pool.provider_id(), ids)
}
