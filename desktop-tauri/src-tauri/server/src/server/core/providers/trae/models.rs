//! Trae SOLO 的模型目录（`POST /api/ide/v1/get_detail_param`）。
//!
//! ── 为什么这张表**只能**远程拉 ──────────────────────────────
//! 上游目录不是一张静态清单，而是"按客户端版本给的一张表"：
//! `X-Ide-Version-Code` 决定拿到哪一张（参考实现实测 `20260716` → 35 条且
//! 没有 `glm-5.3`、`20260820` → 36 条含）。把它抄成常量表就等于把一个时间点
//! 的读数当成契约，上游加一个模型我们要改代码发一版才行。所以本模块
//! **没有静态兜底**：没刷到就是空清单。
//!
//! 空清单的效果要如实理解：广告视图里没有本家模型 ⇒ 客户端点不动
//! （入口校验以广告视图为准），而不是"点了报错"。这与另外几家「静态兜底 +
//! 远程覆盖」的结构不同，别照抄它们的 `FALLBACK`。
//!
//! ── 目录里的三类非可选条目（v0.12.46 对 38 条实测目录校准）──────
//! 上游把三种东西和正式模型混在一张表里，全部要滤掉：
//!   - `is_invisible_to_user = true`：内部 subagent / 实验通道
//!     （`browser_use_subagent`、`file_search_agent`、`sagitta/aquila`…）；
//!   - `display_config.display_name` 为空：租户自定义模型的**占位模板**
//!     （`custom_model_*`、`summary` 等），它们能出现在表里但不是可选模型；
//!   - `config_switch = false`：已下线开关。
//!
//! 三个可布尔字段一律"缺省 = 可见 / 启用"（上游删字段时不能把整张表滤空 ——
//! 那是最坏的失效方式：看起来像"上游没模型了"，其实是本机判空判错了）。
//! 外加 [`errors::config_is_solo_agent_only`] 那张死配置名单：那些 config 在
//! `solo_work_lite` 通道必定流内 `4001`（属于 IDE 加密 agent 通道 /
//! `llm_raw_chat` 的 `solo_agent`），注册出来只会让用户点到必然失败的模型。
//!
//! ── 广告名带 `-solo` 后缀 ───────────────────────────────────
//! 本家的 config 名与别家会撞（`glm-5.2`、`minimax-m3` 在 ZCode 那张静态表里
//! 就有；聚合清单按"先到先得"去重，撞名那家会从 `/v1/models` 整个消失）。
//! 所以广告出去的 id 是 `<config_name>-solo`，出站前由
//! [`payload::sanitize_model_name`] 剥回裸名 —— 与参考实现同款约定，也只剥
//! **一层**（真以 `-solo` 结尾的 config 仍然往返得回来）。
//!
//! ── 输出上限与能力位：写"上游真给的数" ──────────────────────
//! 目录里 `model_detail_list[].encrypted_model_params` 确实是**密文**（没密钥解不开，
//! 参考实现因此把 `MaxTokens` 恒记 0），但**同层就有明文的 `max_tokens`**
//! （本机实测：17 个可见 config 里 16 个 32000、`agnes-2.5-flash` 16000）——
//! 参考实现没读它，不等于上游没给。同理还有 `display_config.model_capability`
//! （`reasoning_model` / `chat_model`）与 `extra_config.native_function_call`。
//! 这三条本家都读出来广告；仍然守同一条纪律：**上游没给的键就不写**，
//! 而不是补一个 0 —— 写 0 进 `/v1/models` 会被客户端当真上限，
//! 进而把 `max_output_tokens: 0` 发给上游。
//!
//! `display_config.multimodal`（识图能力）**也是端到端实测确认过才接的**：
//! 同一张 64×64 双色 PNG（上半红、下半蓝）以 `data:` URL 送进本家这条
//! `solo_work_lite` 通道，判据是"答对颜色的只有标 true 的那几个"。
//!   * `kimi-k2.6`（true）→ `上=红，下=蓝`（prompt 45）；
//!   * `minimax-m3`（true）→ `红色，蓝色`（prompt 203，图片真进了 token 账）；
//!   * `DeepSeek-V4-Flash-Official`（false）→ HTTP 200 但答 `上=未知，下=未知`
//!     ——**这才是最坏的一种**：客户端会以为模型看了图；
//!   * `glm-5.2`（false）→ 流内 `code=3004`，**同一条 body 带图必现、改成纯文本
//!     立刻 200** ⇒ 那句 "exceeded the rate limit" 是假文案，别照它把 3004
//!     映射成限流档（那会让宿主拿同一条必然失败的 body 去轮询其他账号）。
//!
//! ⇒ 上游这个标注在本通道上**有预测力**（2 真全对 / 3 假全废），所以接。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap / expect / panic；持锁期间不做网络。

use std::sync::{OnceLock, RwLock};

use serde_json::{Value, json};

use crate::server::core::providers::adapter::ModelRefreshOutcome;
use crate::server::core::providers::catalog_cache;
use crate::server::errors::GatewayError;
use crate::server::logging;
use crate::server::core::proxies::ResolvedProxy;

use super::credentials::Credential;
use super::errors::config_is_solo_agent_only;
use super::headers::{IDE_VERSION_CODE, solo_headers, HeaderIdentity};
use super::http::{Reply, get_json, post_json};
use super::{AGENT_BASE_URL, MODELS_PATH};

/// 远程目录缓存有效期（与另外几家同为 1 小时；上游改表不需要秒级可见）。
const CACHE_TTL_MS: i64 = 60 * 60 * 1000;

/// 广告 id 的后缀（见模块头"广告名带 `-solo` 后缀"）。
pub const ADVERTISE_SUFFIX: &str = "-solo";

#[derive(Clone, Default)]
struct Cache {
    models: Vec<Value>,
    fetched_at: i64,
}

static SLOTS: OnceLock<RwLock<Cache>> = OnceLock::new();

fn slot() -> &'static RwLock<Cache> {
    SLOTS.get_or_init(|| {
        // 首次初始化先从持久化缓存读回（上次成功拉到的那张表）：进程重启
        // 那一次刷新若失败，也不会退化成"本家模型全消失"。
        match catalog_cache::load(catalog_cache::SCOPE_TRAE) {
            Some(cached) => RwLock::new(Cache { models: cached.models, fetched_at: cached.fetched_at }),
            None => RwLock::new(Cache::default()),
        }
    })
}

fn snapshot() -> Cache {
    slot().read().unwrap_or_else(|error| error.into_inner()).clone()
}

/// 本家的清单（聚合目录认的形态）。
pub fn list() -> Vec<Value> {
    snapshot().models
}

/// 是不是刷过（界面的「来源」列：刷过=远程，没刷=内置/空）。
pub fn remote_refreshed() -> bool {
    !snapshot().models.is_empty()
}

pub fn last_refreshed_at() -> i64 {
    snapshot().fetched_at
}

/// 目录请求体（七个键，顺序无所谓 —— 上游按 JSON 对象读）。
///
/// `need_prompt:false` + `poly_prompt:true` 是官方客户端的取值：前者让它别把
/// 每个模型的 prompt 一起回传（体积），后者要的是多段 prompt 结构。参考实现
/// 逐字这么发，向量里也原样记着，别"顺手优化"成别的组合。
pub fn catalog_body(variant: &str) -> Value {
    catalog_body_for_function(super::payload::function_for(variant))
}

/// 同一张体，但 `function` 由调用方给定 —— 场景探针要用它问 `chat_v3`。
///
/// 七个键与参考实现逐字相同（见 `catalog_body` 的说明），只有 `function` 这一格
/// 换成参数。这一步是整条判别里最便宜的一格：如果 IDE 明细表也认 `chat_v3`，
/// 那些模型就**既有元数据、又有可发的 function**，一个 token 都不用花。
pub fn catalog_body_for_function(function: &str) -> Value {
    json!({
        "function": function,
        "config_names": Value::Null,
        "need_prompt": false,
        "current_config_info": Value::Null,
        "poly_prompt": true,
        "mode_type": Value::Null,
        "agent_type": Value::Null,
    })
}

/// 目录 URL。
pub fn catalog_url() -> String {
    format!("{AGENT_BASE_URL}{MODELS_PATH}")
}

/// remote 侧目录的场景名单 —— 外部那家实现把这一条当作**唯一**的目录来源
///（`Trae2api-cn@0075bb66` `src/trae_client.py:1294`：
/// `GET /api/remote/v1/models?functions=solo_agent_remote,solo_work_remote,`
/// `solo_design_remote&show_custom_model=true`）。
/// 我们自己的那张表走的是 IDE 侧 `get_detail_param`，两者不是一张表。
///
/// 这一条是 **GET /models**，只读、不起沙箱。remote 那条**会话**通道
///（`POST /chat_sessions`）不在本文件里、也不接进转发路径 —— 一次会话会在账号
/// 后面起一台云端机器，实测证据与决定都写在 `refresh` 里那段对照注释上。
pub const REMOTE_CATALOG_FUNCTIONS: [&str; 3] =
    ["solo_agent_remote", "solo_work_remote", "solo_design_remote"];

/// 待判别的场景候选 —— 一发一个，不并进上面那三个。
///
/// 为什么分开问：把未知名字混进同一次查询里，上游若整体回 400 我们就同时丢掉
/// 了基线读数和"是哪个名字惹的祸"。一发一个场景，才知道谁是真存在的。
///   · `chat_v3` —— 出自 `caigee-cmd/cli2api` 的 CHANGELOG（它说整个目录默认走
///     这个场景，且只有它带 Max 档）。`Trae2api-cn` 全仓零命中，所以这是一条
///     **待验**说法，不是两家一致的事实。
///   · `solo_agent_lite` —— 出自 `Trae2api-cn` 自己的分组回退逻辑
///     （`src/trae_client.py:1330-1345`：请求的档位没有就用它的 lite 同胞），
///     它读得到这个兄弟场景名，但没把它列进默认查询串。
/// 只在用户手点「刷新模型」（`force=true`）时发：这是判别用的一发，不该让每小时的
/// 自动刷新替它付配额。
///
/// ── 两个候选都已实测，各跑过两轮（18:23:54 与 19:17:10，读回完全同形）──────
///   · `chat_v3`：remote 24 个名字 / IDE 明细表 21 条，明细表里**我们没广告的 0 个**，
///     remote 侧"看不见"的 3 个 = agnes-2.0-flash、deepseek-v4-flash、deepseek-v4-pro。
///     → 存在、且能给明细表，已进 [`MERGE_CATALOG_FUNCTIONS`]。
///   · `solo_agent_lite`：remote 22 个名字 / 明细表 20 条，"没广告的"同样 **0 个**，
///     remote 侧"看不见"的是**同样那 3 个**。
///     → 存在，但比 `chat_v3` 少两个名字、且一个新模型都供不出来，所以**不并**：
///     并进来只是每轮多打一发上游。那 3 个名字是死名单里的（见 `errors.rs`），
///     "在 remote 目录里看得见"与"我们的通道收它"是两件事。
pub const CATALOG_PROBE_FUNCTIONS: [&str; 2] = ["chat_v3", "solo_agent_lite"];

/// remote 目录的 URL（查询串驱动，没有请求体）。
pub fn remote_catalog_url(functions: &[&str]) -> String {
    let joined = functions.join(",");
    format!("{AGENT_BASE_URL}/api/remote/v1/models?functions={joined}&show_custom_model=true")
}

/// 一份 remote 目录响应 → `(场景, 该场景收录的模型裸名)`。
///
/// 形状是**按场景分组**的：`data.list[]` 每项带 `function`（旧一点的写法是
/// `agent_type`）与 `models[]`（`src/trae_client.py:1301-1330` 的同一读法）。
/// 名字取 `name`，缺 `name` 的条目丢掉 —— 认不出来的名字不能进对照表，
/// 那会把"我们看不见的模型"虚报成"看不见的乱码"。
pub fn parse_remote_catalog(payload: &Value) -> Vec<(String, Vec<String>)> {
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    let Some(list) = payload
        .pointer("/data/list")
        .or_else(|| payload.get("list"))
        .and_then(Value::as_array)
    else {
        return groups;
    };
    for group in list {
        let function = group
            .get("function")
            .or_else(|| group.get("agent_type"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        if function.is_empty() {
            continue;
        }
        let names: Vec<String> = group
            .get("models")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.get("name").and_then(Value::as_str))
                    .map(|name| name.trim().to_string())
                    .filter(|name| !name.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        match groups.iter_mut().find(|(existing, _)| *existing == function) {
            Some((_, bucket)) => bucket.extend(names),
            None => groups.push((function, names)),
        }
    }
    groups
}

/// remote 里有、我们自己那张表里没有的名字（大小写不敏感，按首次出现顺序）。
///
/// 这一条差集就是 G1 要的答案 —— 「我们看不见哪些模型」是可数的，
/// 而"看不见"与"用不了"是两件事：接不了 B 族之前，这些名字**不能**进广告表。
pub fn remote_only_names(ours: &[String], groups: &[(String, Vec<String>)]) -> Vec<String> {
    let lowered: Vec<String> = ours.iter().map(|name| name.to_lowercase()).collect();
    let mut seen: Vec<String> = Vec::new();
    for (_, names) in groups {
        for name in names {
            let key = name.to_lowercase();
            if !lowered.contains(&key) && !seen.iter().any(|held| held.to_lowercase() == key) {
                seen.push(name.clone());
            }
        }
    }
    seen
}

/// 上游响应 → 清单（**纯函数**，由向量 `catalog` 段钉住）。
///
/// 条目顺序保留上游给的顺序：`/v1/models` 的阅读顺序与官方客户端一致，
/// 而且"顺序变了"本身就是一条可观察的漂移信号。
pub fn parse_catalog(payload: &Value) -> Vec<Value> {
    let Some(entries) = payload.get("config_info_list").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        let config_name = text(entry, "config_name");
        if config_name.is_empty() {
            continue; // 空 id 的条目不能路由，也不能广告
        }
        // `false` 才过滤；缺省（None）按启用处理（见模块头）
        if entry.get("config_switch").and_then(Value::as_bool) == Some(false) {
            continue;
        }
        if entry.get("is_invisible_to_user").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let display_name = entry
            .get("display_config")
            .map(|config| text(config, "display_name"))
            .unwrap_or_default();
        if display_name.is_empty() {
            continue; // 租户自定义占位模板
        }
        if config_is_solo_agent_only(&config_name) {
            continue; // 这条通道必死
        }
        let context = entry
            .get("context_window_tokens")
            .and_then(|window| window.get("dev"))
            .and_then(value_as_i64)
            .unwrap_or(0);
        let mut item = catalog_entry(&config_name, &display_name, entry);
        // 只有目录真给了这些数才写对应的键（缺值写 0 = 谎报上限，见模块头）。
        if let Some(object) = item.as_object_mut() {
            if context > 0 {
                object.insert("maxInputTokens".to_string(), Value::from(context));
            }
            if let Some(output) = max_output_tokens(entry) {
                object.insert("maxOutputTokens".to_string(), Value::from(output));
            }
            if let Some(reasoning) = supports_reasoning(entry) {
                object.insert("supportsReasoning".to_string(), Value::from(reasoning));
            }
            if let Some(vision) = supports_images(entry) {
                object.insert("supportsImages".to_string(), Value::from(vision));
            }
        }
        out.push(item);
    }
    out
}

/// 拉一次 remote 目录（只读对照用）。
///
/// 头是在我们这套 ug/IDE 画像上**只加一个** `X-Trae-Client-Type: web`：
/// 外部实现给 remote 用的就是这组（`src/trae_client.py:859-880`）。这一发失败
/// 不影响本通道目录 —— 它是观测，不是依赖，所以调用方拿 `Err` 只写一条 verbose。
async fn fetch_remote_catalog(
    prepared: &[(String, String)],
    proxy: Option<&ResolvedProxy>,
    functions: &[&str],
) -> Result<Vec<(String, Vec<String>)>, GatewayError> {
    let mut with_web = prepared.to_vec();
    with_web.push(("X-Trae-Client-Type".to_string(), "web".to_string()));
    let pairs: Vec<(&str, String)> = with_web
        .iter()
        .map(|(name, value)| (name.as_str(), value.clone()))
        .collect();
    let reply =
        get_json(&remote_catalog_url(functions), &pairs, std::time::Duration::from_secs(20), proxy)
            .await?;
    if reply.status >= 400 {
        let head: String = reply.body.chars().take(120).collect();
        return Err(GatewayError::new(format!(
            "remote 目录 HTTP {}：{}",
            reply.status,
            head.trim()
        )));
    }
    let Some(payload) = reply.json() else {
        return Err(GatewayError::new("remote 目录响应不是合法 JSON"));
    };
    Ok(parse_remote_catalog(&payload))
}

/// 用给定 function 问一次 **IDE 明细目录**（探针专用；主路径仍走 `refresh` 那一发）。
///
/// 这一发才是判别的关键：remote 的 `/models` 只给名字，而我们要广告一个模型得先有
/// `context_window_tokens` / `multimodal` / `native_function_call` 这些明细字段。
/// 所以"remote 里有 24 个名字"本身不说明我们能用；明细表认不认 `chat_v3` 才说明。
async fn fetch_detail_catalog(
    prepared: &[(String, String)],
    proxy: Option<&ResolvedProxy>,
    function: &str,
) -> Result<Vec<Value>, GatewayError> {
    let pairs: Vec<(&str, String)> = prepared
        .iter()
        .map(|(name, value)| (name.as_str(), value.clone()))
        .collect();
    let reply = post_json(
        &catalog_url(),
        &catalog_body_for_function(function),
        &pairs,
        std::time::Duration::from_secs(20),
        proxy,
    )
    .await?;
    if reply.status >= 400 {
        let head: String = reply.body.chars().take(120).collect();
        return Err(GatewayError::new(format!("明细目录 HTTP {}：{}", reply.status, head.trim())));
    }
    let Some(payload) = reply.json() else {
        return Err(GatewayError::new("明细目录响应不是合法 JSON"));
    };
    Ok(parse_catalog(&payload))
}

/// 把"这一批条目是哪个场景给的"盖到条目上。
///
/// 必须能区分，因为出站 body 里的 `function` 要按它选：把 `chat_v3` 专属的模型
/// 用 `solo_work_lite` 发出去，症状是流内 4001（与 `errors.rs` 那份死名单同一类
/// 失败形状），而 4001 看起来像"这个模型坏了"，不像"我们发错了场景"。
fn stamp_function(entries: &mut [Value], function: &str) {
    for entry in entries {
        if let Some(object) = entry.as_object_mut() {
            object.insert("catalogFunction".to_string(), Value::String(function.to_string()));
        }
    }
}

/// 一条目录条目的**裸模型名**（去掉我们加的广告后缀）。
fn bare_name(entry: &Value) -> String {
    entry
        .get("id")
        .and_then(Value::as_str)
        .map(|id| super::payload::sanitize_model_name(id, "solo"))
        .unwrap_or_default()
}

/// 这个模型该用哪个 `function` 发（纯函数版，便于单测）。
///
/// 命中目录条目 → 用它带来时盖的那个场景；没命中 → 回落到 `fallback`
/// （= 今天的行为）。目录里没这个名字的模型大多是上游刚下架或租户自定义占位，
/// 那些情况保持原样发比猜一个场景更安全。
pub fn function_for_model_in(models: &[Value], config_name: &str, fallback: &str) -> String {
    let wanted = config_name.trim();
    for entry in models {
        if bare_name(entry) == wanted {
            if let Some(function) = entry.get("catalogFunction").and_then(Value::as_str) {
                if !function.trim().is_empty() {
                    return function.to_string();
                }
            }
            break;
        }
    }
    fallback.to_string()
}

/// 出站点用的那一句：读内存里那张表，认不出来时回落到调用点按 variant 算出的值
/// （= 合并之前的老行为，一条都不变）。
pub fn function_for_model(config_name: &str, fallback: &str) -> String {
    function_for_model_in(&snapshot().models, config_name, fallback)
}

/// 除了主场景，还要把哪些场景的明细表并进广告表。
///
/// 这一格是"能不能多服务模型"的总开关。`chat_v3` 留下的依据不是目录读数而是
/// **真实对话**：18:29:15 那两轮转发带着 `function=chat_v3` 发 `llm_utils_chat`，
/// glm-5.3-flash 与 kimi-k2.8-preview 各拿回 200 与正常用量（18+69 / 90+82），
/// 也就是这条通道收这个 function。要回退就把这个数组清空，改动只有一行。
///
/// `solo_agent_lite` 不在这里：它供不出一个新名字（见 [`CATALOG_PROBE_FUNCTIONS`]
/// 的那两轮实测），并进来只多一发上游调用。
pub const MERGE_CATALOG_FUNCTIONS: [&str; 1] = ["chat_v3"];

/// 把额外场景的明细表并进来：只并**我们这张表里没有**的名字，已有的不许被覆盖。
///
/// 先到先留的取向与 `/v1/models` 的 first-wins 一致；反过来（让 chat_v3 覆盖主表）
/// 会让同一条目两套元数据互相打架，而我们没有任何依据说哪套更新。
async fn merge_extra_scenes(
    prepared: &[(String, String)],
    proxy: Option<&ResolvedProxy>,
    models: &mut Vec<Value>,
) {
    for function in MERGE_CATALOG_FUNCTIONS {
        let mut have: Vec<String> = models.iter().map(bare_name).collect();
        match fetch_detail_catalog(prepared, proxy, function).await {
            Ok(mut extra) => {
                let mut added = 0usize;
                for mut entry in extra.drain(..) {
                    let name = bare_name(&entry);
                    if name.is_empty() || have.iter().any(|held| *held == name) {
                        continue;
                    }
                    have.push(name);
                    added += 1;
                    // 盖场景就在进表之前盖：拿 `models[len-added..]` 事后补一段，
                    // 会在同一个表达式里对 `models` 既可变借又不可变借（编译期就拦下了，
                    // 但那处的正确修法是把两段逻辑拆开，反而更容易写错边界）。
                    stamp_function(std::slice::from_mut(&mut entry), function);
                    models.push(entry);
                }
                logging::log(
                    "[Models]",
                    &format!("Trae 目录合并 {function}：新增 {added} 条（合计 {} 条）", models.len()),
                );
            }
            Err(error) => logging::log(
                "[Models]",
                &format!("Trae 目录合并 {function} 跳过：{}", error.message),
            ),
        }
    }
}

/// 一条目录条目（聚合形态）。
///
/// `supportsToolCall` 取**上游说的**而不是我们硬编码的：`extra_config` 里带
/// `native_function_call` 键时以它为准（实测 17 个可见 config 里 16 个 true，
/// `agnes-2.5-flash` 整个 `extra_config` 没这个键），缺键仍按 true —— 本家这条
/// 通道确实收 tools（出站 body 里那四处 SOLO 专属变形就是为它做的，见
/// `payload.rs`），而"上游没写"不等于"不支持"（与三个布尔字段同一取向）。
fn catalog_entry(config_name: &str, display_name: &str, entry: &Value) -> Value {
    json!({
        "id": format!("{config_name}{ADVERTISE_SUFFIX}"),
        "name": display_name,
        "supportsToolCall": native_function_call(entry).unwrap_or(true),
    })
}

/// `extra_config` 是**一个 JSON 字符串**（上游就这么塞的），取里面的
/// `native_function_call`。解析失败/没这个键都给 `None`（= 上游没说）。
fn native_function_call(entry: &Value) -> Option<bool> {
    let raw = entry.get("extra_config").and_then(Value::as_str)?;
    let parsed: Value = serde_json::from_str(raw).ok()?;
    parsed.get("native_function_call").and_then(Value::as_bool)
}

/// 输出上限：`model_detail_list[].max_tokens`（**明文**，与同层的
/// `encrypted_model_params` 不同 —— 那份是密文，这一份直接可读）。
///
/// 一个 config 可能有多条 detail（实测全部是租户 `custom_model_*` 的
/// `__max` / `__dev` 两个档位，且两条的 `max_tokens` 相同）。取**最小值**：
/// 广告出去的上限必须两条都能用，取大的等于把客户端送到另一个档位去撞墙。
pub fn max_output_tokens(entry: &Value) -> Option<i64> {
    let details = entry.get("model_detail_list").and_then(Value::as_array)?;
    details
        .iter()
        .filter_map(|detail| detail.get("max_tokens").and_then(value_as_i64))
        .filter(|value| *value > 0)
        .min()
}

/// 思考能力：`display_config.model_capability` 是 `"reasoning_model"` /
/// `"chat_model"` / 空串三态（实测 45 条里 30 / 3 / 12）。
///
/// 空串给 `None`（= 上游没说，不写这个键），不要写成 false —— 我们与上游口径
/// 一致的前提是"没说过的事不替它说"。
/// ⚠️ 这与 `reasoning_effort_config.support_thinking` 是**两件事**：前者是
/// "这个模型会不会输出思考链"，后者是"官方客户端给不给这个账号开思考强度开关"
/// （实测本账号 45 条全 false，属账号权益而非模型属性）。
pub fn supports_reasoning(entry: &Value) -> Option<bool> {
    let capability = entry.get("display_config").map(|config| text(config, "model_capability")).unwrap_or_default();
    match capability.as_str() {
        "reasoning_model" => Some(true),
        "chat_model" => Some(false),
        _ => None,
    }
}

/// 识图能力：`display_config.multimodal` 是**三态**（true / false / 缺键）。
///
/// 缺键给 `None`（= 上游没说，就不写这个键）而不是 false —— 本机实测 45 条里
/// 就有 1 条整个不带这个字段，替它说"不能看图"与本家一直防的"缺省当假"是
/// 同一类错。取值本身经端到端实测，见模块头那张对照。
pub fn supports_images(entry: &Value) -> Option<bool> {
    entry.get("display_config").and_then(|config| config.get("multimodal")).and_then(Value::as_bool)
}

fn text(object: &Value, key: &str) -> String {
    object.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

fn value_as_i64(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| value.as_f64().map(|number| number as i64))
}

/// 拉一次远程目录并落地（内存 + 持久化缓存）。
///
/// TTL 早退由调用方是否 `force` 决定（自动路径 `false`、用户点"获取模型" `true`）
/// —— 缓存该不该复用只由**谁发起**决定，与另外几家同一契约。
pub async fn refresh(
    credential: &Credential,
    proxy: Option<&ResolvedProxy>,
    force: bool,
) -> ModelRefreshOutcome {
    let current = snapshot();
    if !force && current.fetched_at > 0 && logging::now_ms() - current.fetched_at < CACHE_TTL_MS {
        return ModelRefreshOutcome::unchanged();
    }
    if !credential.valid() {
        return ModelRefreshOutcome::unchanged();
    }
    let identity = HeaderIdentity {
        access_token: credential.access_token.trim(),
        uid: credential.uid.trim(),
        machine_id: credential.machine_id.trim(),
        device_id: credential.device_id.trim(),
    };
    // 头名是**本家自己造**的字符串（`solo_headers` 返回 BTreeMap），所以这里
    // 先落一份 owning 的表再借出键 —— 不拿 `Box::leak` 糊过去：刷新是长期
    // 后台动作，每次泄漏十几条字符串不是"小开销"，是漏。
    let prepared: Vec<(String, String)> = solo_headers(&identity, false).into_iter().collect();
    let headers: Vec<(&str, String)> = prepared.iter().map(|(name, value)| (name.as_str(), value.clone())).collect();
    let reply: Reply = match post_json(&catalog_url(), &catalog_body(credential.variant()), &headers, std::time::Duration::from_secs(20), proxy).await {
        Ok(reply) => reply,
        Err(error) => return ModelRefreshOutcome::failed(format!("目录请求失败：{}", error.message)),
    };
    if reply.status >= 400 {
        return ModelRefreshOutcome::failed(describe_failure(reply.status, &reply.body));
    }
    let Some(payload) = reply.json() else {
        return ModelRefreshOutcome::failed("上游目录响应不是合法 JSON".to_string());
    };
    let mut models = parse_catalog(&payload);
    if models.is_empty() {
        // 这条文案不能写成"上游没有模型"就完事：真实原因多半是
        // `X-Ide-Version-Code` 那一道版本闸门（版本不对时上游给的是**另一张表**
        // 或空表），或者是目录字段改名让解析整个落空。
        return ModelRefreshOutcome::failed(format!(
            "上游目录里没有可见模型（config_info_list 缺失/为空？IdeVersionCode={IDE_VERSION_CODE}）"
        ));
    }
    // 主表盖主场景：`chat_v3` 专属的名字是后面合并进来的，两者出站的 `function`
    // 不同，不盖就分不出来。
    stamp_function(&mut models, &super::payload::function_for(credential.variant()));
    // 先合并再落缓存与内存表 —— 顺序反了就会出现"缓存里 19 条、内存里 23 条"
    // 那种重启后模型凭空消失的形状。
    merge_extra_scenes(&prepared, proxy, &mut models).await;
    let count = models.len();
    let now = logging::now_ms();
    // 先落持久化缓存（不持内存锁：两把锁不能嵌套），再换内存里那张表。
    catalog_cache::save(catalog_cache::SCOPE_TRAE, &models, now);
    {
        let mut guard = slot().write().unwrap_or_else(|error| error.into_inner());
        guard.models = models;
        guard.fetched_at = now;
    }
    logging::log("[Models]", &format!("Trae 模型目录已刷新（{count} 个模型）"));
    // ── G1 的只读对照：remote 目录里哪些名字是我们看不见的 ─────────────
    // 我们那张表是从 IDE 侧 `get_detail_param` 按 `function=solo_work_lite` 拿的；
    // 外部那家实现的目录**只有** remote 这一处来源。两边的差集就是"上游有、
    // 我们看不见"的名单 —— 它现在只进日志，**不改广告表**：
    // "看不见"是事实，"看得见就能用"不是。这些名字大多挂在 B 族（remote 会话
    // 协议）的执行器上，拿到我们这条通道上会得到流内 4001（与 `errors.rs` 那份
    // 死名单同一类形状）。
    //
    // ── B 族量过了，并**决定不接**（2026-10-07 19:26，一发真实会话）────────
    // remote 不是"另一种 chat 端点"，是 **agent 运行时**：`POST /chat_sessions`
    // 一次 = 在账号后面起一台云端沙箱。那一发的证据链是 `sandbox_name=
    // run-harness-<sid>-…` 的 `platform_timing`、`session_title_message` /
    // `session_icon_message` / 5 条 `plan_item`，42 秒内**没有任何 assistant 文本**，
    // 收尾是 `error` 事件与会话 status 3→6、消息 `message_type=task` + `failed`。
    // 所以要接它得同时做三件事：给会话落一个 project/environment、把 agent 事件流
    // 翻成 chat delta、接受分钟级延迟与沙箱副作用 —— 那不"补一个通道"，是第二个产品。
    // 这份对照因此**留在只读侧**（只进日志、不进转发路径）：它的用处是让"trae 只有
    // 19/24 条模型"这种判断有数可依，Max(1M) 档在那边也一并记着，别照着这份名单
    // 往我们这条通道上加模型。复现脚本 `cpa-deploy/scripts/trae_remote_session_probe.py`
    //（默认只读，`--create` 才会起沙箱）。
    let ours: Vec<String> = snapshot()
        .models
        .iter()
        .filter_map(|item| item.get("id").and_then(Value::as_str))
        .map(|id| super::payload::sanitize_model_name(id, "solo"))
        .collect();
    match fetch_remote_catalog(&prepared, proxy, &REMOTE_CATALOG_FUNCTIONS).await {
        Ok(groups) => {
            let remote_total: usize = groups.iter().map(|(_, names)| names.len()).sum();
            let missing = remote_only_names(&ours, &groups);
            logging::log(
                "[Models]",
                &format!(
                    "Trae 目录对照：remote {} 个场景 / {} 个名字，本通道 {} 条，看不见的 {} 个",
                    groups.len(),
                    remote_total,
                    ours.len(),
                    missing.len()
                ),
            );
            let shape: Vec<String> = groups
                .iter()
                .map(|(function, names)| format!("{function}={}", names.len()))
                .collect();
            logging::verbose("[Models]", &format!("Trae remote 场景分布：{}", shape.join(", ")));
            if !missing.is_empty() {
                logging::verbose(
                    "[Models]",
                    &format!("Trae 本通道看不见的模型名：{}", missing.join(", ")),
                );
            }
        }
        Err(error) => logging::verbose(
            "[Models]",
            &format!("Trae remote 目录对照失败（不影响本通道目录）：{}", error.message),
        ),
    }
    // ── 场景探针：一发一个候选，只在手点「刷新模型」时发 ──────────────────
    // 判别对象是 cli2api 那句"整个目录默认走 `chat_v3`、且只有它带 Max 档"。
    // 三种回答都要能分开，否则下一次会去查错的方向：
    //   · 给回这一组 → 场景真存在，多出来的名字就是 G2 的可服务面；
    //   · 200 但没有这一组 → 上游不认这个 function 名（"不存在"，不是"没权限"）；
    //   · 4xx/5xx → 整体被拒，把状态码与原话头记下来。
    // 一次点刷新多发 2 发目录查询，换掉的是"要不要照那份 CHANGELOG 动手"这个判断。
    if force {
        for function in CATALOG_PROBE_FUNCTIONS {
            match fetch_remote_catalog(&prepared, proxy, &[function]).await {
                Ok(groups) => match groups.iter().find(|(name, _)| *name == function) {
                    Some((_, names)) => {
                        let one = vec![(function.to_string(), names.clone())];
                        let missing = remote_only_names(&ours, &one);
                        logging::log(
                            "[Models]",
                            &format!(
                                "Trae 场景探针 {function}：{} 个名字，本通道看不见的 {} 个",
                                names.len(),
                                missing.len()
                            ),
                        );
                        logging::verbose(
                            "[Models]",
                            &format!("Trae 场景探针 {function} 看不见的：{}", missing.join(", ")),
                        );
                    }
                    None => logging::log(
                        "[Models]",
                        &format!(
                            "Trae 场景探针 {function}：上游答了 200 但没给这一组（回来的场景：{}）",
                            groups
                                .iter()
                                .map(|(name, _)| name.as_str())
                                .collect::<Vec<_>>()
                                .join(",")
                        ),
                    ),
                },
                Err(error) => logging::log(
                    "[Models]",
                    &format!("Trae 场景探针 {function} 失败：{}", error.message),
                ),
            }
            // 同一个场景名再问一次我们自己在用的那张明细表：光有 remote 的名字
            // 不能广告（缺 `context_window_tokens` 等明细字段），明细表认了才谈得上服务。
            match fetch_detail_catalog(&prepared, proxy, function).await {
                Ok(entries) => {
                    // 只报条数没用：21 条里可能 19 条就是我们已有的，那才是"多不出
                    // 一个模型"。把明细表的名字一起过一遍差集，才看得见真新增。
                    let detail: Vec<String> = entries
                        .iter()
                        .filter_map(|item| item.get("id").and_then(Value::as_str))
                        .map(|id| super::payload::sanitize_model_name(id, "solo"))
                        .collect();
                    let groups = vec![(function.to_string(), detail.clone())];
                    let missing = remote_only_names(&ours, &groups);
                    logging::log(
                        "[Models]",
                        &format!(
                            "Trae 场景探针 {function}：IDE 明细表 {} 条，其中我们没广告的 {} 个",
                            entries.len(),
                            missing.len()
                        ),
                    );
                    logging::verbose(
                        "[Models]",
                        &format!("Trae 场景探针 {function} 明细表新增：{}", missing.join(", ")),
                    );
                }
                Err(error) => logging::log(
                    "[Models]",
                    &format!("Trae 场景探针 {function}：IDE 明细表失败 —— {}", error.message),
                ),
            }
        }
    }
    ModelRefreshOutcome::refreshed(count)
}

/// 目录请求失败时给的一句**有指向性**的话（状态码 + 原文片段）。
fn describe_failure(status: u16, body: &str) -> String {
    let head: String = body.chars().take(120).collect();
    match status {
        401 | 403 => format!("上游返回 HTTP {status}（凭据可能已失效，请重新登录该账号）"),
        _ if head.is_empty() => format!("上游返回 HTTP {status}"),
        _ => format!("上游返回 HTTP {status}：{head}"),
    }
}

/// 这个客户端名是不是本家认识的（含带后缀与裸名两种写法）。
pub fn is_known(model: &str) -> bool {
    let needle = model.trim();
    !needle.is_empty() && list().iter().any(|entry| text(entry, "id") == needle)
}

/// 客户端名 → 上游 config 名。
///
/// 两条都认：广告名 `<config>-solo` 与裸 config 名（后者是"用户照上游文档
/// 写名字"的常见情形，且 `sanitize_model_name` 对裸名本来就是恒等）。
pub fn upstream_name(model: &str) -> String {
    super::payload::sanitize_model_name(model, "solo")
}

#[cfg(test)]
mod tests {
    use super::*;

    const VECTORS: &str = include_str!("vectors/trae-vectors.json");

    fn document() -> Value {
        serde_json::from_str(VECTORS).expect("向量必须是合法 JSON")
    }

    #[test]
    fn the_remote_catalog_grouping_and_the_diff_are_read_from_the_documented_shape() {
        // 形状照外部实现读的那份：`data.list[]` 每项 `function` + `models[]`，
        // 旧写法里场景名在 `agent_type` 上 —— 两条都得认，否则对照表会静默变空。
        let payload = json!({
            "data": {"list": [
                {"function": "solo_agent_remote", "models": [
                    {"name": "glm-5.3"}, {"name": " kimi-k2.8-preview "}, {"name": ""}, {"config_name": "no-name"},
                ]},
                {"agent_type": "solo_work_remote", "models": [{"name": "glm-5.3"}, {"name": "Doubao-Seed-Code"}]},
            ]}
        });
        let groups = parse_remote_catalog(&payload);
        assert_eq!(2, groups.len(), "两个场景都要认出来：{groups:?}");
        assert_eq!("solo_agent_remote", groups[0].0);
        // 空名与没有 `name` 字段的条目丢掉：认不出来的名字进对照表只会虚报差额
        assert_eq!(vec!["glm-5.3", "kimi-k2.8-preview"], groups[0].1, "去空白、丢空名：{:?}", groups[0].1);
        assert_eq!("solo_work_remote", groups[1].0, "`agent_type` 那一写也要认");

        // 差集：大小写不敏感、跨场景去重、我们已有的不许出现
        let ours = vec!["GLM-5.3-solo".to_string()];
        let ours: Vec<String> = ours
            .iter()
            .map(|id| super::super::payload::sanitize_model_name(id, "solo"))
            .collect();
        let missing = remote_only_names(&ours, &groups);
        assert_eq!(
            vec!["kimi-k2.8-preview", "Doubao-Seed-Code"],
            missing,
            "大小写要减掉、跨场景不许重复：{missing:?}"
        );

        // 完全没有 `data.list` 时给空表，而不是 panic（release 是 panic=abort）
        assert!(parse_remote_catalog(&json!({"code": 1})).is_empty());
    }

    /// 探针问明细表时必须复用**同一张体**，只换 `function`。
    /// 七个键里任何一个变了，上游给的就不是同一张表 —— 那次探到的"不存在"
    /// 可能是形状造成的，而不是场景名。
    /// 出站 `function` 按模型选：`chat_v3` 专属的名字要用 `chat_v3` 发，
    /// 主表来的要用主场景发，目录里没有的名字回落到老行为。
    /// 三条缺一不可 —— 只测第一条的话，实现"永远返回 chat_v3"也能过。
    #[test]
    fn each_model_carries_the_scene_it_came_from() {
        let table = vec![
            json!({"id": "glm-5.3-flash-solo", "catalogFunction": "chat_v3"}),
            json!({"id": "kimi-k3-solo", "catalogFunction": "solo_work_lite"}),
            json!({"id": "no-stamp-solo"}),
        ];
        assert_eq!(
            "chat_v3",
            function_for_model_in(&table, "glm-5.3-flash", "solo_work_lite"),
            "合并进来的名字必须按它来的场景发"
        );
        assert_eq!(
            "solo_work_lite",
            function_for_model_in(&table, "kimi-k3", "solo_work_lite"),
            "主表来的不许被合并改写成别的场景"
        );
        assert_eq!(
            "solo_work_lite",
            function_for_model_in(&table, "no-stamp", "solo_work_lite"),
            "没盖场景的条目（老缓存/内置清单）走老行为"
        );
        assert_eq!(
            "solo_work_lite",
            function_for_model_in(&table, "根本不在表里", "solo_work_lite"),
            "目录不认识的名字要回落，而不是猜一个场景"
        );
    }

    #[test]
    fn the_probe_catalog_body_differs_from_the_real_one_only_by_function() {
        let real = catalog_body("solo");
        let probe = catalog_body_for_function("chat_v3");
        let real_map = real.as_object().expect("目录体是对象");
        let probe_map = probe.as_object().expect("探针体是对象");
        assert_eq!(real_map.keys().collect::<Vec<_>>(), probe_map.keys().collect::<Vec<_>>(), "键集与顺序都不许变");
        assert_eq!("chat_v3", probe_map["function"].as_str().unwrap_or_default());
        for key in real_map.keys() {
            if key == "function" {
                continue;
            }
            assert_eq!(real_map[key], probe_map[key], "{key} 不该被探针改动");
        }
    }

    #[test]
    fn the_remote_catalog_url_lists_every_scene_in_one_query() {
        let url = remote_catalog_url(&REMOTE_CATALOG_FUNCTIONS);
        assert!(url.starts_with("https://trae-api-cn.mchost.guru/api/remote/v1/models?"), "{url}");
        for function in REMOTE_CATALOG_FUNCTIONS {
            assert!(url.contains(function), "{function} 必须在同一次查询里：{url}");
        }
        assert!(url.contains("show_custom_model=true"), "不带这个参数就看不到租户自定义模板：{url}");
        assert!(!url.contains('%'), "逗号在查询串里是合法字符，不许被预编码：{url}");

        // 探针必须**单独成发**：混进基线那一次里，一个未知场景名就能把
        // 已经拿到的 53 个名字一起赔掉（而这正是我们要留着当对照的那份读数）。
        let probe = remote_catalog_url(&["chat_v3"]);
        assert!(probe.contains("chat_v3"), "{probe}");
        for function in REMOTE_CATALOG_FUNCTIONS {
            assert!(!probe.contains(function), "探针那一发不许带基线场景：{probe}");
        }
        assert!(CATALOG_PROBE_FUNCTIONS.contains(&"chat_v3"), "cli2api 那句要能被问到");
    }

    #[test]
    fn the_catalog_request_matches_the_reference_implementation() {
        let document = document();
        let want = &document["catalogRequest"];
        assert_eq!(want["method"].as_str().unwrap(), "POST");
        assert_eq!(want["path"].as_str().unwrap(), MODELS_PATH);
        assert_eq!(catalog_url(), format!("https://trae-api-cn.mchost.guru{MODELS_PATH}"));
        // 七个键逐个比（Go marshal 会按字母排序，所以比对象而不是比字节串）。
        let want_body: Value = serde_json::from_str(want["body"].as_str().unwrap()).unwrap();
        let got = catalog_body("solo");
        assert_eq!(want_body, got, "目录请求体的键与取值要和参考实现一致");
        assert_eq!(7, want_body.as_object().unwrap().len(), "七个键一个不能多不能少（少了改不了形、多了上游拒）");
    }

    #[test]
    fn catalog_filtering_matches_the_reference_implementation() {
        let document = document();
        let payload: Value = serde_json::from_str(document["catalogFixture"].as_str().unwrap()).expect("fixture 是 JSON");
        let got = parse_catalog(&payload);
        let want = document["catalog"].as_array().expect("catalog 段存在");
        assert_eq!(want.len(), got.len(), "过滤条数就不同：{got:?}");
        for (index, entry) in want.iter().enumerate() {
            let line = &got[index];
            assert_eq!(
                format!("{}{ADVERTISE_SUFFIX}", entry["id"].as_str().unwrap()),
                line["id"].as_str().unwrap(),
                "第 {} 条的 id 不对",
                index + 1
            );
            assert_eq!(entry["name"].as_str().unwrap(), line["name"].as_str().unwrap());
            let context = line.get("maxInputTokens").and_then(Value::as_i64).unwrap_or(0);
            assert_eq!(entry["contextWindow"].as_i64().unwrap(), context, "{} 的上下文窗口不对", entry["id"].as_str().unwrap());
            // 卷里 `maxTokens` **恒为 0**（参考实现不读明文的 `model_detail_list[].max_tokens`，
            // 只看到密文的 encrypted_model_params 就放弃了）。这里把这条事实读出来
            // 并断言它，是为了让"我们不跟"是一个**有记录的偏离**而不是静默分叉：
            // 本家改读明文，所以 fixture 里那些没带 model_detail_list 的条目仍不该有键。
            assert_eq!(Some(0), entry["maxTokens"].as_i64(), "参考实现这一列恒 0（偏离的基准）");
            assert!(
                line.get("maxOutputTokens").is_none(),
                "fixture 的条目没给 model_detail_list，就不该凭空冒出输出上限：{}",
                entry["id"].as_str().unwrap_or("?")
            );
        }
    }

    #[test]
    fn the_five_rejected_entry_kinds_stay_rejected() {
        // 向量已经证明过一次，这里补的是**每条各一个**的可定位断言：
        // 将来某一条判错，报错要直接指出是哪一类漏进来了。
        let cases = [
            ("invisible 内部通道", r#"{"config_info_list":[{"config_name":"sagitta","is_invisible_to_user":true,"display_config":{"display_name":"Sagitta"}}]}"#),
            ("空 display_name 的占位模板", r#"{"config_info_list":[{"config_name":"custom_model_x","display_config":{"display_name":""}}]}"#),
            ("config_switch=false 已下线", r#"{"config_info_list":[{"config_name":"legacy","config_switch":false,"display_config":{"display_name":"Legacy"}}]}"#),
            ("solo_agent-only 死配置", r#"{"config_info_list":[{"config_name":"deepseek-v4-flash","display_config":{"display_name":"DS V4 Flash"}}]}"#),
            ("空 config_name", r#"{"config_info_list":[{"config_name":"","display_config":{"display_name":"x"}}]}"#),
        ];
        for (label, body) in cases {
            let payload: Value = serde_json::from_str(body).unwrap();
            assert!(parse_catalog(&payload).is_empty(), "{label} 不该被透出");
        }
    }

    #[test]
    fn missing_boolean_flags_mean_visible() {
        // 上游省字段是常态。若把"缺省"判成"不可见"，整张表会被滤空，
        // 症状是"上游目录里没有可见模型"—— 一句会让人去查上游的谎话。
        let payload: Value = serde_json::from_str(r#"{"config_info_list":[{"config_name":"glm-5.2","display_config":{"display_name":"GLM 5.2"}}]}"#).unwrap();
        let models = parse_catalog(&payload);
        assert_eq!(1, models.len());
        assert_eq!("glm-5.2-solo", models[0]["id"].as_str().unwrap());
        assert!(models[0].get("maxInputTokens").is_none(), "目录没给窗口就不写这个键（写 0 是谎报上限）");
    }

    #[test]
    fn an_empty_or_malformed_payload_yields_no_models_rather_than_panicking() {
        for body in ["{}", r#"{"config_info_list":[]}"#, "[]", r#"{"config_info_list":null}"#, "not json"] {
            let payload = serde_json::from_str(body).unwrap_or(Value::Null);
            assert!(parse_catalog(&payload).is_empty(), "{body} 不该产出模型");
        }
    }

    #[test]
    fn the_advertised_name_round_trips_to_the_upstream_config() {
        assert_eq!("glm-5.2", upstream_name("glm-5.2-solo"));
        assert_eq!("glm-5.2", upstream_name("glm-5.2"), "裸名也认（用户照上游文档写名字）");
        assert_eq!("x-solo", upstream_name("x-solo-solo"), "只剥一层：真以 -solo 结尾的 config 还能回来");
        assert_eq!("deepseek-ai/deepseek-v4-pro", upstream_name("deepseek-ai/deepseek-v4-pro-solo"), "带斜杠的 config 名不能被切坏");
    }

    #[test]
    fn capability_fields_come_from_the_plaintext_catalog_columns() {
        // 这几条是**参考实现没读**的字段（它只解析 5 个键），所以对拍钉不住，
        // 只能自带用例；取值全部照本机真实响应写：可见 config 里 16 个
        // max_tokens=32000 + reasoning_model + native_function_call:true，
        // agnes-2.5-flash 是 16000 + chat_model + extra_config 里没那个键。
        let payload: Value = serde_json::from_str(
            r#"{"config_info_list":[
              {"config_name":"kimi-k3","config_switch":true,"context_window_tokens":{"dev":200000},
               "extra_config":"{\"native_function_call\":true,\"use_v2_process\":true}",
               "display_config":{"display_name":"Kimi-K3","model_capability":"reasoning_model","multimodal":true},
               "model_detail_list":[{"model_name":"kimi-k3","max_tokens":32000,"prompt_max_tokens":168000}]},
              {"config_name":"agnes-2.5-flash","config_switch":true,"context_window_tokens":{"dev":200000},
               "extra_config":"{\"apply_file_path\":true}",
               "display_config":{"display_name":"agnes-2.5-flash","model_capability":"chat_model","multimodal":true},
               "model_detail_list":[{"model_name":"agnes","max_tokens":16000}]},
              {"config_name":"custom_model_gemini","config_switch":true,"context_window_tokens":{"dev":300000},
               "display_config":{"display_name":"Gemini","model_capability":""},
               "model_detail_list":[{"model_name":"gemini__max","max_tokens":32000},{"model_name":"gemini__dev","max_tokens":24000}]}
            ]}"#,
        )
        .expect("fixture 是 JSON");
        let models = parse_catalog(&payload);
        assert_eq!(3, models.len());
        assert_eq!(Some(32000), models[0]["maxOutputTokens"].as_i64());
        assert_eq!(Some(true), models[0]["supportsReasoning"].as_bool());
        assert_eq!(Some(true), models[0]["supportsToolCall"].as_bool());
        assert_eq!(Some(200000), models[0]["maxInputTokens"].as_i64(), "窗口仍取 context_window_tokens.dev");
        assert_eq!(Some(16000), models[1]["maxOutputTokens"].as_i64());
        assert_eq!(Some(false), models[1]["supportsReasoning"].as_bool(), "chat_model 要如实标 false");
        assert_eq!(Some(true), models[0]["supportsImages"].as_bool(), "multimodal=true 要接出来");
        assert_eq!(Some(true), models[1]["supportsImages"].as_bool(), "agnes 也标了 true（识图与 capability 是两个维度）");
        // 三态里的"缺"（本机那 45 条里有 1 条整个不带这个键）：不能替它说不能看图
        assert!(models[2].get("supportsImages").is_none(), "上游没带 multimodal 时不能替它说不能看图");
        assert_eq!(
            Some(true),
            models[1]["supportsToolCall"].as_bool(),
            "extra_config 没这个键时按 true 兜底（本家通道实测收 tools）"
        );
        assert_eq!(
            Some(24000),
            models[2]["maxOutputTokens"].as_i64(),
            "一个 config 有多条 detail 时取**最小**：广告出去的上限两条档位都得能用"
        );
        assert!(models[2].get("supportsReasoning").is_none(), "capability 空串 = 上游没说，不写键");
    }

    #[test]
    fn upstream_silence_about_a_capability_stays_silent() {
        // 缺 model_detail_list / extra_config 不是"上限 0"也不是"不支持工具"，
        // 而是"不知道" —— 不知道就不写，别替上游编。
        let payload: Value =
            serde_json::from_str(r#"{"config_info_list":[{"config_name":"glm-5.2","display_config":{"display_name":"GLM"}}]}"#)
                .expect("fixture 是 JSON");
        let models = parse_catalog(&payload);
        assert_eq!(1, models.len());
        let item = &models[0];
        assert!(item.get("maxOutputTokens").is_none(), "没给 detail 就不写输出上限");
        assert!(item.get("supportsReasoning").is_none());
        assert_eq!(Some(true), item["supportsToolCall"].as_bool(), "这一条走的是通道事实兜底，不是上游标注");
        assert!(item.get("maxInputTokens").is_none());
    }

    #[test]
    fn the_failure_copy_points_at_the_version_gate() {
        // 空清单最常见的真实原因是那道 `X-Ide-Version-Code` 版本闸门，
        // 文案里带上它，排查时不必先去翻代码。
        assert!(describe_failure(401, "").contains("重新登录"));
        assert!(describe_failure(500, "boom").contains("boom"));
        let with_version = format!("IdeVersionCode={IDE_VERSION_CODE}");
        assert!(with_version.contains("2026"), "版本号要真的能印出来：{with_version}");
    }
}
