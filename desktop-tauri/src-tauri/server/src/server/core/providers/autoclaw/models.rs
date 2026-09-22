//! AutoClaw 模型路由表（Agent2API 二期 T-c1 前置；移植来源
//! `D:\APP\AutoClaw\autoclaw-local-proxy\autoclaw-models.mjs`）。
//!
//! ── 这不是「远程模型目录」────────────────────────────────────
//! 与小浣熊（`raccoon/models.rs`：静态兜底 + `/model_catalog` 远程刷新）不同，
//! AutoClaw **没有任何可拉取的模型目录接口**。源实现 `autoclaw-models.mjs` 是一份
//! **静态映射表**（AutoClaw 1.17.8 设置页目录 + model-provider-config 支持列表），
//! 外加两条回退规则。因此本模块是**纯数据 + 纯函数**：不读盘、不发请求、没有缓存。
//!
//! ── 两个模型标识（这是 AutoClaw 路由的核心）──────────────────
//! 上游代理用两个不同的标识定位模型（源实现模块头写得很清楚）：
//!
//!   - `X-Request-Model` **请求头**：完整**路由 ID**，带通道前缀
//!     （`zai_auto`、`zaicoding_glm-5.3`、`zai_glm-5.3-flash`）。
//!     路由前缀决定**计费通道**：
//!       `zai_`        → 会员 / 按量付费
//!       `zaicoding_`  → Coding Plan
//!       `zai_auto`    → 让服务端自动选型
//!   - `body.model` **请求体**：**剥掉路由前缀**后的模型 ID
//!     （`glm-5.3` 等）—— 这是 AutoClaw 主进程 Model Broker 的行为。
//!
//! 两者都必须发对：只发对一个，上游要么选错通道计费，要么认不出模型。
//!
//! ── 客户端给的名字可以是三种写法 ─────────────────────────────
//! `resolve_model_route` 接受（源实现 `resolveModelRoute`）：
//!   1. 目录条目：`glm-5.3`、`glm-5.3-flash`（目录 ID / 展示名 /
//!      完整 routeId 都算，见下）；
//!   2. 显式路由前缀：`zaicoding_glm-5.3` 这类；
//!   3. 其它合法路由 ID（`x_yyy` 形态，`ROUTE_ID_PATTERN`）直接透传。
//! 三者都不匹配时**回退默认路由** `zai_auto`（非严格模式）—— 这条回退是
//! 兜住 Anthropic 客户端那类会传 `claude-*` 的调用方的（源实现注释明说）。
//!
//! ── 目录匹配口径必须与聚合目录一致（这是本模块的一处关键修复）──────
//! `providers/catalog.rs::providers_for_model` 先比 `id`、再比 `name`，两轮都是
//! `trim` + 忽略 ASCII 大小写 —— 所以 `GLM-5.3`（展示名）、`Auto` 这类名字在
//! **目录校验层是通过的**。本模块的目录匹配必须用同一口径（先 id、再展示名、
//! 再完整 `routeId`），否则同一份清单在两个层给出不同判定：目录放行、路由层
//! 却因为大小写差异静默回退 `zai_auto` —— 客户端请求 `GLM-5.3` 会被换成
//! `auto`（选错模型 + 走错计费通道），这是必须避免的。
//!
//! 匹配命中后一律取条目的**规范** `routeId` / body 模型 ID；`requested_model`
//! 保留客户端原请求（响应回写用）。未知名字（`claude-*` 等）仍按源实现的
//! 非严格模式回退默认路由，但**不做任何大小写改写**：默认回退只兜真正未知的
//! 名字，不能是大小写转换的副作用。
//!
//! ── 与聚合目录的衔接（别在这里翻译字段）──────────────────────
//! 聚合层 `providers/catalog.rs` 用 `models::list_item` 做字段映射，它认的是
//! **上游原始形态**：`id` 是模型 id、`name` 是展示名、`maxInputTokens` /
//! `maxOutputTokens` / `supportsImages` / `supportsReasoning` 是能力位
//! （见 `core/models/shape.rs` 的模块头）。所以 `list()` 输出的条目**按
//! 聚合层认的键名**给：`routeId` 保留在新加的字段里（转发时要用），
//! `name` 放展示名 —— 这与 `raccoon/models.rs` 的做法一致。
//!
//! 与源实现 `modelListResponse()` 的差异（有意）：源实现输出一批
//! `support_image` / `context_window` / `_autoclaw` 这些**它自己 HTTP 层**的
//! 字段（那是给它的 /v1/models 直出用的）。本网关的 `/v1/models` 由聚合层统一
//! 成形（OpenAI 形态 + `meta`），因此这里只给「聚合层要的原始形态」，
//! 不重复拼一层响应信封。

use serde_json::{json, Value};

use super::region::Region;

/// 默认路由 ID（源实现 `DEFAULT_ROUTE`）。也是 `AUTOCLAW_DEFAULT_ROUTE`
/// 没配时的取值 —— 与源项目 `server.mjs` 第 41 行的默认值一致。
pub const DEFAULT_ROUTE: &str = "zai_auto";

/// 已知路由前缀（源实现 `KNOWN_ROUTE_PREFIXES`）。
///
/// **顺序有意义**：剥前缀时先试 `zaicoding_` 再试 `zai_`，
/// 反过来会把 `zaicoding_glm-5.3` 错剥成 `coding_glm-5.3`。
const KNOWN_ROUTE_PREFIXES: &[&str] = &["zaicoding_", "zai_"];

/// 合法路由 ID 的形态（源实现 `ROUTE_ID_PATTERN`）：
/// 小写字母开头 + 下划线 + 1..=127 个 `[A-Za-z0-9._:-]`。
///
/// 手写判定而不是引 regex 依赖：规则只有「开头、分隔符、字符集、长度」四项，
/// 一个纯函数足够，且避免了为一个校验引进一整棵 regex 依赖树。
fn is_route_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    let Some((&first, rest)) = bytes.split_first() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    // 首个 `_` 之前必须全是小写字母（`[a-z][a-z0-9]*`）
    let mut prefix_len = 0usize;
    for &byte in rest {
        if byte == b'_' {
            break;
        }
        if !byte.is_ascii_lowercase() && !byte.is_ascii_digit() {
            return false;
        }
        prefix_len += 1;
    }
    // rest[prefix_len] 必须是 `_`，且其后是 1..=127 个允许字符
    if prefix_len >= rest.len() || rest[prefix_len] != b'_' {
        return false;
    }
    let tail = &rest[prefix_len + 1..];
    if tail.is_empty() || tail.len() > 127 {
        return false;
    }
    tail.iter().all(|&byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-')
    })
}

/// 剥掉已知路由前缀（源实现 `stripRoutePrefix`）：
/// `zaicoding_glm-5.3` → `glm-5.3`；没有前缀的路由 ID 原样返回。
pub fn strip_route_prefix(route_id: &str) -> &str {
    for prefix in KNOWN_ROUTE_PREFIXES {
        if let Some(rest) = route_id.strip_prefix(prefix) {
            return rest;
        }
    }
    route_id
}

/// 一次模型路由解析的结果（源实现 `resolveModelRoute` 的返回对象）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelRoute {
    /// 完整路由 ID → 发进 `X-Request-Model` 请求头
    pub route_model_id: String,
    /// 剥前缀后的模型 ID → 填进 `body.model`
    pub body_model_id: String,
    /// 客户端原样请求的模型名（响应回写与日志用）
    pub requested_model: String,
}

/// 模型目录条目（源实现 `MODELS` 的每一项，字段取聚合层认的名字）。
///
/// 用静态数组而不是 `json!` 字面量：条目字段固定、没有可选键，
/// 结构体让「少填一个字段」在编译期就报错。
struct ModelEntry {
    /// 目录 ID（也是客户端最常给的那个名字）：`auto`、`glm-5.3`
    id: &'static str,
    /// 完整路由 ID（带通道前缀）
    route_id: &'static str,
    /// 展示名：`Auto`、`GLM-5.3`
    name: &'static str,
    /// 是否支持思考/reasoning（源实现 `reasoning`）
    reasoning: bool,
    /// 是否支持图片输入（源实现 `input` 数组里有没有 `image`）
    support_image: bool,
    /// 上下文窗口（token）
    context_window: i64,
    /// 单次最大输出（token）
    max_tokens: i64,
}

/// 静态模型路由表（对齐**实际客户端软件当前提供的模型**）。
///
/// **顺序即 /v1/models 的展示顺序**，改动会直接反映到客户端看到的列表。
/// 源代理实现（`autoclaw-models.mjs`）移植了 1.17.8 设置页目录的 9 个条目，
/// 但实际软件的模型选择器当前只提供 **GLM-5.3 与 GLM-5.3-Flash** 两个
/// （其余条目在软件里已下架），这里按实际软件收敛为两个 —— 网关能广告与
/// 转发的模型不应比真实客户端能选的更多。
///
/// 注意「目录条目」与「路由回退」是两回事：`zai_auto` 默认路由、显式路由
/// 前缀透传、`x_yyy` 形态识别都还在（见 `resolve_model_route`）——
/// 它们是**转发路由**的兜底逻辑，不依赖目录里有没有 `auto` 这个条目。
const MODELS: &[ModelEntry] = &[
    ModelEntry {
        id: "glm-5.3",
        route_id: "zaicoding_glm-5.3",
        name: "GLM-5.3",
        reasoning: true,
        support_image: false,
        context_window: 1_048_576,
        max_tokens: 307_200,
    },
    ModelEntry {
        id: "glm-5.3-flash",
        route_id: "zai_glm-5.3-flash",
        name: "GLM-5.3-Flash",
        reasoning: true,
        support_image: true,
        context_window: 1_048_576,
        max_tokens: 131_072,
    },
];

/// 默认路由的覆盖值：`AUTOCLAW_DEFAULT_ROUTE`（源项目 `server.mjs` 的同名环境变量；
/// 国际版是 `AUTOCLAW_INTL_DEFAULT_ROUTE`）。
///
/// 空值/缺失回落到 `zai_auto`。只在「请求没给 model」或「未知模型回退」时生效。
pub fn default_route(region: Region) -> String {
    region
        .env_override("DEFAULT_ROUTE")
        .unwrap_or_else(|| DEFAULT_ROUTE.to_string())
}

/// 目录里按 id 查条目（源实现 `MODEL_BY_ID` 的 Map）
fn entry_by_id(id: &str) -> Option<&'static ModelEntry> {
    MODELS.iter().find(|entry| entry.id == id)
}

/// 按**聚合目录的口径**认一个目录条目：先 `id`、再展示名 `name`、最后完整
/// `routeId`，三轮都是 `trim`（由调用方完成）+ 忽略 ASCII 大小写。
///
/// 前两轮与 `providers/catalog.rs::providers_for_model` 逐字同口径（那里的
/// `trim().to_lowercase()` 在纯 ASCII 的目录数据上与此等价），这是硬要求：
/// `chat.rs` 的模型校验走聚合目录，`GLM-5.3` / `Auto` 这类写法在那边是**放行**
/// 的，本模块必须给出同一判定 —— 否则同一个名字在目录层合法、在路由层却因
/// 大小写差异静默回退 `zai_auto`（选错模型 + 走错计费通道）。
///
/// 第三轮 `routeId` 只服务「显式路由写法」的规范化：`zaicoding_glm-5.3` 这类
/// 已知路由 ID（含其大小写变体）也解析到条目里的**规范** routeId 与 body 模型 ID。
/// 未知名字三轮都不命中，返回 `None` —— 本函数不做任何改写，未知自定义 route
/// 的大小写语义由 `resolve_model_route` 原样保留。
fn entry_by_catalog_name(name: &str) -> Option<&'static ModelEntry> {
    let same = |text: &str| text.eq_ignore_ascii_case(name);
    MODELS
        .iter()
        .find(|entry| same(entry.id))
        .or_else(|| MODELS.iter().find(|entry| same(entry.name)))
        .or_else(|| MODELS.iter().find(|entry| same(entry.route_id)))
}

/// 远程目录里按目录口径认一条路由（返回**完整路由 ID**，即 `X-Request-Model`
/// 要发的那个）。
///
/// ── 为什么必须在静态表之外再查一次 ──────────────────────────
/// 远程目录会广告静态表没有的模型（实测多出 `zai_auto` / `zai_auto-fast`）。
/// 不查这里的话，客户端请求 `auto-fast` 会走到 `resolve_model_route` 的最后
/// 一档 —— **静默回退 `zai_auto`**（选错模型，而且错得无声无息）。
/// `zai_auto-fast` 这种路由还有个陷阱：它剥前缀后是 `auto-fast`，而
/// `is_route_id("auto-fast")` 为 false（连字符不算合法路由 ID 字符），
/// 所以「按路由 ID 形态透传」那条路也接不住它 —— 只有远程目录知道它。
fn remote_route_by_catalog_name(region: Region, name: &str) -> Option<String> {
    super::catalog::remote_route_id(region, name)
}

/// 把客户端传入的 model 解析成上游路由（源实现 `resolveModelRoute` 的
/// `strict = false` 路径 —— 网关侧不做严格模式：未知模型在校验层就拦掉了，
/// 到这里还进来的都是要回退默认路由的）。
///
/// 判定顺序（在源实现的顺序上只放宽「目录条目怎么认」，其余逐条对齐）：
///   1. 空值 → 默认路由（`requested_model` 也回落成默认路由名，源实现同款）；
///   2. 目录条目命中（id / 展示名 / 完整 routeId，忽略 ASCII 大小写）→
///      用条目的规范 routeId 与剥前缀后的 body 模型 ID；
///   3. 已知前缀开头 → 原样当路由 ID，body 剥前缀（**大小写原样保留**：
///      未知自定义 route 不做小写改写）；
///   4. 合法路由 ID 形态 → 原样透传，body 剥前缀（没有已知前缀时不剥，等于原样）；
///   5. **手动登记的自定义模型** → 原样透传（见下）；
///   6. 都不中 → 默认路由（**只兜真正未知的名字**：目录能认的名字在上面就解析
///      掉了，不会因大小写差异落到这里）。
///
/// ── 第 5 档为什么必须存在（本次修复）────────────────────────
/// 用户可以在管理页手动登记上游模型（`modelRules.custom`），`manifest_for` 把
/// 它们拼进了清单，于是**入口校验与路由候选链都会放行**。但本函数此前不认识
/// 它们：`gpt-5.5-preview` 这种名字既不在静态表、也不在远程目录、还不是合法
/// route ID 形态（连字符不算），于是掉进第 6 档**静默回落 `zai_auto`** ——
/// 客户端要 A、上游跑了 B，日志上还是一次「成功」。用户看到的症状是「自定义
/// 模型加上了、也能调，但回答牛头不对马嘴」，而没有任何一处提示出错。
///
/// 放在第 4 档**之后**而不是最前面：名字若本来就长得像路由 ID（`zai_xxx`），
/// 那套前缀语义（`X-Request-Model` 带前缀、body 剥前缀）才是这家上游认的形态，
/// 自定义登记不该把用户从既有正确路径上挤下来。这一档只接住「前四档全不中、
/// 否则就会静默回落」的那些名字。
///
/// 代价说明：`is_custom` 要读一次规则快照（`config::current()` 的 clone），
/// 但只在这一档被求值 —— 前面四档命中时完全不付这个成本，而前四档覆盖了
/// 全部目录内模型（也就是绝大多数请求）。
///
/// `requested_model` 始终保留客户端请求里的名字（响应回写用），不被规范化。
///
/// `region` 决定三件事：默认路由的环境变量前缀、远程目录读哪一格的缓存、
/// 以及第 5 档「自定义模型」判据按哪一家查（`model_rules::is_custom` 认
/// provider id，两地的自定义登记是分开的）。
pub fn resolve_model_route(region: Region, raw_model: &str) -> ModelRoute {
    let name = raw_model.trim();
    let fallback = default_route(region);
    if name.is_empty() {
        return ModelRoute {
            body_model_id: strip_route_prefix(&fallback).to_string(),
            requested_model: fallback.clone(),
            route_model_id: fallback,
        };
    }
    if let Some(entry) = entry_by_catalog_name(name) {
        return ModelRoute {
            route_model_id: entry.route_id.to_string(),
            body_model_id: strip_route_prefix(entry.route_id).to_string(),
            requested_model: name.to_string(),
        };
    }
    // 远程目录命中：用它给的路由 ID（静态表里没有的模型 —— 实测多出
    // `zai_auto` / `zai_auto-fast`）。**必须放在兜底之前**，否则这些模型会
    // 静默回退默认路由：客户端要 A 却跑了 B，而且日志上只看到一次「成功」
    if let Some(route_id) = remote_route_by_catalog_name(region, name) {
        return ModelRoute {
            body_model_id: strip_route_prefix(&route_id).to_string(),
            requested_model: name.to_string(),
            route_model_id: route_id,
        };
    }
    for prefix in KNOWN_ROUTE_PREFIXES {
        if name.starts_with(prefix) {
            return ModelRoute {
                route_model_id: name.to_string(),
                body_model_id: strip_route_prefix(name).to_string(),
                requested_model: name.to_string(),
            };
        }
    }
    if is_route_id(name) {
        return ModelRoute {
            route_model_id: name.to_string(),
            body_model_id: strip_route_prefix(name).to_string(),
            requested_model: name.to_string(),
        };
    }
    // 手动登记的自定义模型：原样透传，**不要**掉进下面的默认路由回落。
    // 判据与聚合目录同源（`model_rules::is_custom`），所以「清单里认它」
    // 与「发送时也认它」是同一件事，不存在「校验放行、发送时换成别的模型」。
    // 按**本地区**的 provider id 查：两地的自定义登记是各自独立的清单
    if crate::server::core::model_rules::is_custom(region.provider_id(), name) {
        return ModelRoute {
            route_model_id: name.to_string(),
            body_model_id: strip_route_prefix(name).to_string(),
            requested_model: name.to_string(),
        };
    }
    ModelRoute {
        body_model_id: strip_route_prefix(&fallback).to_string(),
        requested_model: name.to_string(),
        route_model_id: fallback,
    }
}

/// 模型清单（聚合层认的**上游原始形态**）。
///
/// ── 远程目录优先 ────────────────────────────────────────────
/// 上游有 `GET .../proxy/autoclaw-model-config`（见 [`super::catalog`]），
/// 实测返回 4 条，而静态表只有 2 条 —— 缺的 `zai_auto` 恰恰是本家
/// `default_route()` 的回退目标。远程不可用（未登录 / 网络不通）时回落静态表，
/// 于是 `/v1/models` 在两种状态下都非空。
///
/// 键名对照 `core/models/shape.rs::list_item`：它读 `id` / `name` /
/// `maxInputTokens` / `maxOutputTokens` / `supportsImages` / `supportsReasoning` /
/// `credits`。`routeId` 不是聚合层要的键，但**转发必须用**
/// （`X-Request-Model`），所以一并带上（聚合层会原样忽略未知键）。
///
/// 远程目录按**地区**取（两地的清单独立，见 `catalog::catalog` 的说明）；
/// 静态兜底表两地共用 —— 它是「上游不可达时的最小可用集合」，不是某一地的
/// 真实目录。
pub fn list(region: Region) -> Vec<Value> {
    let remote = super::catalog::remote_models(region);
    if !remote.is_empty() {
        return remote;
    }
    MODELS
        .iter()
        .map(|entry| {
            json!({
                "id": entry.id,
                "name": entry.name,
                "routeId": entry.route_id,
                "maxInputTokens": entry.context_window,
                "maxOutputTokens": entry.max_tokens,
                "supportsImages": entry.support_image,
                "supportsReasoning": entry.reasoning,
                // AutoClaw 的目录里没有工具调用能力位（源实现也没给），
                // 但上游是 OpenAI 兼容网关，工具调用可用 —— 与源
                // `modelListResponse` 不给该字段相比，这里显式给 true，
                // 因为聚合层的 `supports_tool_call` 缺失时会输出 false，
                // 会让客户端以为这批模型不能调工具
                "supportsToolCall": true,
                // 静态表没有倍率（上游的 `creditConsumptionLevel` 只在远程
                // 目录里，且是「低/中/高」文案而非数值），键缺失 → 界面显示 `—`
            })
        })
        .collect()
}

/// AutoClaw 的**能力探测**：该模型是否支持图片输入
/// （目录里 `glm-5.3-flash` / `glm-4.6v` 两个是 `input: ['text','image']`）。
///
/// 入参既可以是目录 id（`glm-5.3-flash`），也可以是带前缀的路由 ID
/// （`zai_glm-5.3-flash`）—— 适配器拿到的是客户端给的名字，而它可能两种形态
/// 都有；这里按前缀剥一层再查。目录外的名字返回 **false**（保守：不让未知模型
/// 走多模态路径）。注意这只是给适配器做图片相关处理用的**能力探测**，
/// 与聚合目录的「这家能不能提供这个模型」是两件事（后者只认目录 id，
/// 见 `list()` 的说明），不要用它替代路由判定。
///
/// ── 为什么当前没有调用点（接线后的核对结论，T-c2）─────────────
/// AutoClaw 的上游是 OpenAI 兼容代理，图片随 `messages[].content` 原样透传
/// （源实现的 `autoclaw-upstream-client.mjs` 从不做图片压缩/改写，与 CatPaw 的
/// `image_compress` 完全不同），因此适配器不需要在构造请求时判断能力位。
/// 目录条目里的 `supportsImages` 已经由 `list()` 直接给出（聚合层读它），
/// 本函数是**能力探测口**：保留它是为了让「某个模型到底支不支持图片」有唯一
/// 的纯函数答案（排障 / 将来的多模态处理都会问它），源侧同一个能力位（`supportImage`）
/// 也是独立字段。`#[allow(dead_code)]`：当前无生产调用点，编译器据实报「没人用」。
///
/// 只看**静态兜底表**（不带 region）：远程目录的能力位由 `list()` 直出给聚合层，
/// 这个探测口服务的是「按名字问一个纯函数答案」，而静态表的两个条目两地同形。
#[allow(dead_code)]
pub fn supports_image(name: &str) -> bool {
    let trimmed = name.trim();
    if let Some(entry) = entry_by_id(trimmed) {
        return entry.support_image;
    }
    for prefix in KNOWN_ROUTE_PREFIXES {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            if let Some(entry) = entry_by_id(rest) {
                return entry.support_image;
            }
        }
    }
    false
}
