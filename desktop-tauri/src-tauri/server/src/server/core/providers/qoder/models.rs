//! Qoder 模型目录（移植来源 `Qoder-Proxy/src/models.mjs`）。
//!
//! ── 清单从哪来 ──────────────────────────────────────────────
//!   1. **静态兜底**：`FALLBACK` 里按地区的两个清单。进程启动即可用，
//!      网络不通 / 没账号时 `/v1/models` 仍然有内容。
//!   2. **远程刷新**：`GET {gateway}algo/api/v2/model/list?Encode=1`（COSY 签名，
//!      缓存 1 小时）。失败保留现有清单 —— 与源实现 `refreshModels` 的两级兜底
//!      逐条一致。
//!
//! ── 为什么按地区分开缓存（与其它四家不同）─────────────────────
//! Qoder 分国际版（`api3.qoder.sh`）与中国版（`gateway.qoder.com.cn`），
//! 两边的目录**不是同一份**（套餐档位不同）。缓存按 region 分开，
//! `list()` 返回两者的并集（按 id 去重，global 优先）供聚合目录使用；
//! 请求时 `resolve` 先在**账号所属地区**的目录里找，再退到并集 ——
//! 于是「这个账号能不能用这个模型」由上游说了算，而不是我们猜。
//!
//! ── 条目的键名口径（与另外四家对齐）──────────────────────────
//! 聚合层的 `models::list_item` 认的是 `id` / `name` / `maxInputTokens` /
//! `maxOutputTokens` / `supportsImages` / `supportsReasoning` /
//! `supportsToolCall` / `isDefault` / `kind`（见 `core::models::shape`）。
//! 本模块直接**按这套键名产出**，不再走中间形态；协议层要的 `upstreamKey`
//! 与 `config` 另存两个键 —— 它们不会被 `list_item` 带进 `/v1/models`
//! （那个函数只挑它认识的键），但路由与请求构造要读。
//!
//! ── 进程级句柄 ──────────────────────────────────────────────
//! 与 `raccoon::models` / `core::models` 同一模式（`OnceLock` + `RwLock`）：
//! 适配器是无状态单例，清单必须挂在进程级的共享句柄上，刷新才能对所有调用点
//! （`/v1/models`、路由判定、后台刷新）同时可见。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：零 unwrap/expect/panic；持锁期间绝不做网络请求
//! （刷新先把请求发完、解析完，最后才在锁内做一次整体替换）。

use std::sync::{OnceLock, RwLock};

use serde_json::{json, Value};

use crate::server::core::providers::adapter::ModelRefreshOutcome;
use crate::server::logging;

use super::cosy::{self, CosyIdentity};
use super::credentials::Credentials;
use super::endpoints::Region;

/// 远程目录缓存有效期（源实现 `CACHE_TTL_MS`：1 小时）
const CACHE_TTL_MS: i64 = 60 * 60 * 1000;

/// 目录请求超时（毫秒）。目录是短响应，给 20 秒足够；
/// 与转发链路共用出网点（`egress::client_for`），这里只叠加一层总超时。
const REQUEST_TIMEOUT_MS: u64 = 20_000;

/// 输出上限固定 32768。
///
/// 源实现取这个值而不是更大：实测超过约 32K 后上游行为会退化
/// （关闭思考时仍输出思考内容，约 49K 起返回空内容甚至直接断连）。
/// 这是上游的实际约束，不是保守取值。
pub const MAX_OUTPUT_TOKENS: i64 = 32_768;

/// 目录条目没给上下文长度时的兜底
const DEFAULT_CONTEXT_WINDOW: i64 = 200_000;

/// 内置兜底清单。
///
/// `enabled` 在这里只是「免费档通常可用」的保守猜测（源实现同款注释）；
/// 真实可用性以运行时从上游拉回的清单为准 —— 拉回来的 `enable` 字段才是
/// 「当前套餐是否可用」。
///
/// **不过滤不可用的模型**：用户需要看到完整清单，否则分不清「模型不存在」
/// 与「没权限」，也看不到升级套餐能解锁什么。
///
/// ── 元组末尾的倍率是什么 ────────────────────────────────────
/// 上游 `price_factor` 的静态快照（2026-09-20 实测）。取**正常价**而不是
/// 折扣价：断网 / 未登录时才用这张表，而折扣是错峰时段的临时态，把它写死
/// 会让离线用户在非折扣时段看到低一档的价 —— 折扣由远程刷新如实带回来。
/// 空串 = 上游也没有这个值（会显示成 `—`）。
fn fallback(region: Region) -> Vec<Value> {
    let global: &[(&str, &str, bool, bool, &[&str], bool, bool, &str)] = &[
        ("Qwen3.8-Flash", "qfmodel", true, true, &["low", "medium", "xhigh"], true, true, "0.1"),
        ("Qwen3.8-Max", "qmodel_38max", true, true, &["low", "medium", "xhigh"], true, true, "0.5"),
        ("Auto", "auto", true, false, &[], true, false, "1"),
        ("Ultimate", "ultimate", true, true, &[], true, false, "1.6"),
        ("Performance", "performance", true, true, &[], true, false, "1.1"),
        ("Efficient", "efficient", false, false, &[], true, false, "0.3"),
        ("Sonus", "smodel", true, true, &[], true, false, "3.2"),
        ("Cantus", "cmodel", true, true, &[], true, false, "3.2"),
        ("Qwen3.7-Max", "qmodel_latest", true, true, &[], true, false, "0.5"),
        // `Qwen3.7-Plus` 带连字符：上游 `display_name` 就是这个形态，而远程
        // 刷新走的是 `display_name` 去空白（见 `parse_catalog`）。少了连字符
        // 会让同一个模型产出两种 id（离线用兜底、在线用远程），客户端缓存里
        // 留下两条记录
        ("Qwen3.7-Plus", "qmodel", false, false, &[], true, false, "0.1"),
        ("Kimi-K3", "kmodel_latest", false, false, &[], true, false, "0.8"),
        ("Kimi-K2.8-Preview", "kmodel", false, false, &[], true, false, "0.3"),
        ("GLM-5.3", "gmodel", true, true, &[], true, false, "0.6"),
        ("GLM-5.3-Flash", "gfmodel", true, true, &[], true, false, "0.1"),
        ("DeepSeek-V4-Pro", "dmodel", true, true, &[], true, false, "0.8"),
        ("DeepSeek-Flash", "dfmodel", true, true, &[], true, false, "0.2"),
        ("MiniMax-M3", "mmodel", false, false, &[], true, false, "0.2"),
    ];
    let cn: &[(&str, &str, bool, bool, &[&str], bool, bool, &str)] = &[
        ("Qwen3.8-Flash", "qfmodel", true, true, &["low", "medium", "xhigh"], true, true, "0.1"),
        ("Qwen3.8-Max", "qmodel_38max", true, true, &["low", "medium", "xhigh"], true, true, "0.5"),
        ("Auto", "auto", true, false, &[], true, false, "1"),
        ("Qwen3.7-Max", "qmodel_latest", true, false, &[], true, false, "0.5"),
        ("Qwen3.7-Plus", "qmodel", true, false, &[], false, false, "0.1"),
        ("DeepSeek-V4-Pro", "dmodel", true, false, &[], false, false, "0.8"),
        ("DeepSeek-Flash", "dfmodel", false, false, &[], false, false, "0.2"),
        ("GLM-5.3", "gmodel", true, false, &[], true, false, "0.6"),
        ("Kimi-K2.8-Preview", "kmodel", true, false, &[], true, false, "0.3"),
        ("MiniMax-M3", "mmodel", false, false, &[], false, false, "0.2"),
    ];
    let rows = if region == Region::Cn { cn } else { global };
    rows.iter()
        .map(|(id, key, reasoning, supports_effort, efforts, vision, enabled, factor)| {
            entry(
                id,
                key,
                *reasoning,
                if *supports_effort { efforts } else { &[] },
                *vision,
                *enabled,
                DEFAULT_CONTEXT_WINDOW,
                "system",
                &credits_of_text(factor),
            )
        })
        .collect()
}

/// 倍率数值 → `credits` 列的展示文本。
///
/// ── 为什么是 `x{n} credits` 而不是裸数字 ──────────────────────
/// 这一列是**跨 provider 共用**的：WorkBuddy 的 `credits` 就是上游下发的
/// `"x0.16 credits"` 形态（见 `core::models::builtin_models`），前端用同一个
/// 正则 `/x\s*([\d.]+)/` 把它渲染成 `0.16x`。本家用同形文本就自动落进同一条
/// 渲染分支，不必为一个 provider 加特例。
///
/// ── 为什么不用 `format!("{:.1}")` ────────────────────────────
/// Qoder 的倍率有 `0.04` 这一档（Qwen3.7-Plus 的折扣价），一位小数会把它
/// 四舍五入成 `0.0` —— 一个「免费」的错误暗示。这里保留两位并按需裁零：
/// `0.04` → `0.04`、`0.1` → `0.1`、`1` → `1`。
fn credits_of_text(plain: &str) -> String {
    if plain.trim().is_empty() {
        return String::new();
    }
    format!("x{} credits", plain.trim())
}

/// 上游 `price_factor` → 展示文本（远程路径）。
///
/// `price_factor = 0` 是**合法值**（Qwen3.8-Flash 免费档，界面显示 `0x`），
/// 不能当缺失去掉。这里刻意返回**字符串**而非数字：`list_item` 的
/// `credits` 走 `js_truthy` 判定，数字 `0` 会被判成假值而丢掉这个键 ——
/// 于是「免费」在界面上变成「未知」，两者含义正好相反。
///
/// 接受字符串形态（实测上游给的是 JSON 数字，但同一家的其它目录字段就混着
/// 两种形态）—— 只认数字时，上游哪天改成 `"0.5"` 会静默变成「没给」。
fn credits_of(item: &Value) -> String {
    let factor = match item.get("price_factor") {
        Some(Value::Number(number)) => number.as_f64(),
        Some(Value::String(text)) => text.trim().parse::<f64>().ok(),
        _ => None,
    };
    let Some(factor) = factor else {
        return String::new();
    };
    if !factor.is_finite() || factor < 0.0 {
        return String::new();
    }
    credits_of_text(&format_factor(factor))
}

/// 倍率数字 → 紧凑文本（整数不带小数点，其余最多两位小数、裁掉尾零）。
fn format_factor(value: f64) -> String {
    let rounded = (value * 100.0).round() / 100.0;
    if (rounded - rounded.trunc()).abs() < 1e-9 {
        return format!("{}", rounded.trunc() as i64);
    }
    let text = format!("{rounded:.2}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// 构造一条目录条目（协议层与聚合层共用同一形状）。
///
/// `config` 是发往上游的 `model_config` 素材（见 `protocol::slim_model_config`）：
/// 只保留上游判定推理链路要用的几个字段 —— 源实现明确剥掉 `thinking_config`，
/// 因为它会**覆盖** `parameters.enable_thinking`，让「关闭思考」失效。
///
/// `credits` 是倍率列的文本（空串 = 上游没给）；它只进清单展示，
/// 不参与 `config`（那是发往上游的请求素材，多一个键都可能被上游校验拒绝）。
fn entry(
    id: &str,
    upstream_key: &str,
    reasoning: bool,
    efforts: &[&str],
    vision: bool,
    enabled: bool,
    context_window: i64,
    source: &str,
    credits: &str,
) -> Value {
    let mut model = json!({
        "id": id,
        "name": id,
        "upstreamKey": upstream_key,
        "enabled": enabled,
        "reasoning": reasoning,
        "supportsReasoning": reasoning,
        "supportsImages": vision,
        // Qoder 的上游是 agent 形态，工具调用是固有能力
        "supportsToolCall": true,
        "efforts": efforts,
        "maxInputTokens": context_window,
        "maxOutputTokens": MAX_OUTPUT_TOKENS,
        "isDefault": false,
        "kind": "chat",
        "config": {
            "key": upstream_key,
            "is_reasoning": reasoning,
            "is_vl": vision,
            "source": source,
        },
    });
    // 倍率键只在有值时插入：`list_item` 对缺失的 `credits` 会输出空串
    // （`js_truthy` 判定），这与插入空串等价；少一个键让「上游没给」与
    // 「上游给了空串」在内部状态里可区分
    if !credits.is_empty() {
        if let Some(object) = model.as_object_mut() {
            object.insert("credits".to_string(), Value::String(credits.to_string()));
        }
    }
    model
}

/// 内部状态：每个地区一份远程清单
#[derive(Default, Clone)]
struct CatalogState {
    global: Vec<Value>,
    cn: Vec<Value>,
    global_fetched_at: i64,
    cn_fetched_at: i64,
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

/// 某地区当前生效的清单（远程优先，否则静态兜底）
fn catalog_for(state: &CatalogState, region: Region) -> Vec<Value> {
    let remote = match region {
        Region::Global => &state.global,
        Region::Cn => &state.cn,
    };
    if remote.is_empty() {
        fallback(region)
    } else {
        remote.clone()
    }
}

/// 当前清单（两个地区的**并集**，按 id 去重、global 优先）。
///
/// 聚合目录与路由判定读这里 —— 它们只有「这个模型名认不认识」这一个问题，
/// 不关心账号在哪一边（那由请求时的 `resolve` 收窄）。
pub fn list() -> Vec<Value> {
    let state = read_state();
    let mut out: Vec<Value> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for region in [Region::Global, Region::Cn] {
        for model in catalog_for(&state, region) {
            let id = text_of(&model, "id");
            if id.is_empty() {
                continue;
            }
            let key = id.to_lowercase();
            if seen.iter().any(|known| known == &key) {
                continue;
            }
            seen.push(key);
            let mut model = model;
            if let Some(object) = model.as_object_mut() {
                object.insert("region".to_string(), Value::String(region.id().to_string()));
            }
            out.push(model);
        }
    }
    out
}

/// 把客户端的模型名解析成「上游标识 + 模型配置 + 地区」。
///
/// ── 匹配顺序（与 `catalog::providers_for_model` 同口径）──────────
/// 先按 `id` 全等（忽略大小写），再按 `name` 全等。源实现还允许按
/// `upstreamKey` 匹配 —— 那是同一条链路上「客户端恰好写了内部 key」的容错，
/// 这里一并保留（客户端真写 `qfmodel` 时不该报「模型不存在」）。
///
/// `region` 是**账号所属地区**：先在该地区的目录里找，找不到再看另一地区 ——
/// 这样「国际版账号请求中国版专属模型」会拿到一个明确的解析结果，
/// 由上游报出真实原因（而不是网关谎报「模型不存在」）。
pub fn resolve(model_id: &str, region: Region) -> Option<Value> {
    let wanted = model_id.trim().to_lowercase();
    if wanted.is_empty() {
        return None;
    }
    let state = read_state();
    // ① 账号所属地区优先，② 另一地区兜底，③ 两边都没有 → None
    let other = if region == Region::Cn { Region::Global } else { Region::Cn };
    for candidate_region in [region, other] {
        let models = catalog_for(&state, candidate_region);
        if let Some(found) = find_in(&models, &wanted) {
            return Some(found);
        }
    }
    // ④ 兜底清单也在两个地区都查一遍（远程目录为空时上面已经用过兜底，
    //    这一步覆盖「远程目录在一边有、另一边没有」的交错情形）
    for candidate_region in [other, region] {
        let models = fallback(candidate_region);
        if let Some(found) = find_in(&models, &wanted) {
            return Some(found);
        }
    }
    None
}

/// 在一个清单里按 id → name → upstreamKey 的顺序找（都与源实现同序）
fn find_in(models: &[Value], wanted: &str) -> Option<Value> {
    let matches = |value: &Value, key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(|text| text.trim().to_lowercase() == wanted)
            .unwrap_or(false)
    };
    for key in ["id", "name", "upstreamKey"] {
        if let Some(found) = models.iter().find(|model| matches(model, key)) {
            return Some(found.clone());
        }
    }
    None
}

/// 是否已经成功采用过远程清单（`catalog_refresh_meta` 用）
pub fn remote_refreshed(region: Region) -> bool {
    let state = read_state();
    match region {
        Region::Global => !state.global.is_empty(),
        Region::Cn => !state.cn.is_empty(),
    }
}

/// 最后一次成功刷新远程目录的时间（毫秒；从未成功过为 0）
pub fn last_refreshed_at() -> i64 {
    let state = read_state();
    state.global_fetched_at.max(state.cn_fetched_at)
}

/// 拉取并落地某地区的远程目录。
///
/// 三档返回值与 `ModelRefreshOutcome` 的契约一致（成功 / 没刷 / 失败了）。
/// `force = false` 时命中 TTL 直接早退（自动路径）；用户手动点刷新时
/// 调用方传 `true` 绕过 —— 缓存该不该复用只由**谁发起**决定。
pub async fn refresh(
    credentials: &Credentials,
    proxy: Option<&crate::server::core::proxies::ResolvedProxy>,
    force: bool,
) -> ModelRefreshOutcome {
    let region = credentials.region;
    if !force {
        let state = read_state();
        let (models, fetched_at) = match region {
            Region::Global => (&state.global, state.global_fetched_at),
            Region::Cn => (&state.cn, state.cn_fetched_at),
        };
        if !models.is_empty() && logging::now_ms() - fetched_at < CACHE_TTL_MS {
            return ModelRefreshOutcome::unchanged();
        }
    }

    let url = format!("{}algo/api/v2/model/list?Encode=1", region.gateway());
    let identity = CosyIdentity {
        user_id: &credentials.user_id,
        auth_token: &credentials.access_token,
        name: &credentials.name,
        email: &credentials.email,
        machine_id: &credentials.machine_id,
    };
    // 目录是 GET：请求体为空，签名覆盖空体（源实现传 null 同上）
    let headers = match cosy::build_auth_headers(None, &url, &identity) {
        Ok(headers) => headers,
        Err(error) => return ModelRefreshOutcome::failed(error.message),
    };
    let outcome = crate::server::core::auth_http::send_raw(
        "GET",
        &url,
        None,
        &headers,
        proxy,
        Some(REQUEST_TIMEOUT_MS),
    )
    .await;
    let payload = match outcome {
        Ok(response) => {
            if !response.ok {
                return ModelRefreshOutcome::failed(format!("上游返回 HTTP {}", response.status));
            }
            response.payload.unwrap_or(Value::Null)
        }
        Err(error) => {
            return ModelRefreshOutcome::failed(
                crate::server::core::egress::describe_error_detail(&error),
            );
        }
    };
    // 上游信封：业务错误也在 200 里（`statusCodeValue` / `code`）
    if let Some(message) = envelope_error(&payload) {
        return ModelRefreshOutcome::failed(message);
    }
    let models = parse_catalog(&payload);
    if models.is_empty() {
        return ModelRefreshOutcome::failed("上游返回的模型目录为空");
    }
    let count = models.len();
    let mut state = read_state();
    let now = logging::now_ms();
    match region {
        Region::Global => {
            state.global = models;
            state.global_fetched_at = now;
        }
        Region::Cn => {
            state.cn = models;
            state.cn_fetched_at = now;
        }
    }
    match catalog().write() {
        Ok(mut guard) => *guard = state,
        Err(poisoned) => *poisoned.into_inner() = state,
    }
    logging::log(
        "[Models]",
        &format!("✅ Qoder 模型目录已更新（{}，{count} 个）", region.label()),
    );
    // 默认规则种子（只默认启用 QODER_DEFAULT_ENABLED 白名单里的模型，见
    // `model_rules::seed_qoder_defaults`）：对**并集**种一次而不是本次这一边的
    // 清单 —— 两个地区的 id 一次全覆盖，另一边不必等自己刷新过才轮到。只对
    // 首次出现的 id 生效，用户的手动调整不会被这里覆盖。
    seed_default_rules();
    ModelRefreshOutcome::refreshed(count)
}

/// 对当前清单（两个地区的并集）补一次 Qoder 的默认规则种子。
///
/// 刷新成功后由 [`refresh`] 调用；编排入口（`providers::adapter` 的
/// `seed_current_qoder_defaults`）也调它 —— 覆盖「远程刷新失败、手里只有静态
/// 兜底清单」与**升级用户**首次打开管理页的情形（那时并集来自兜底清单，
/// 同样要按白名单落一次默认值）。幂等：种过的 id 不再动。
pub fn seed_default_rules() {
    let ids: Vec<String> = list()
        .iter()
        .filter_map(|item| item.get("id").and_then(Value::as_str).map(str::to_string))
        .collect();
    if let Some(summary) = crate::server::core::model_rules::seed_qoder_defaults(&ids) {
        logging::log("[Models]", &summary);
    }
}

/// 上游目录响应里的业务错误（HTTP 200 也可能带错误）
fn envelope_error(payload: &Value) -> Option<String> {
    let code = payload.get("statusCodeValue").and_then(Value::as_i64)
        .or_else(|| payload.get("code").and_then(Value::as_i64));
    match code {
        Some(200) | None => None,
        Some(code) => Some(format!(
            "上游返回业务错误 {code}: {}",
            payload
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| payload.get("body").and_then(Value::as_str))
                .unwrap_or("")
                .chars()
                .take(200)
                .collect::<String>()
        )),
    }
}

/// 解析目录响应：`{ chat: [ { key, display_name, enable, is_vl, ... } ] }`。
///
/// 字段口径照抄源实现：只收 `chat` 数组，缺 `key` 或缺 `display_name` 的条目丢弃。
/// 对外的模型 id 用 `display_name` **去掉空白**（源实现 `toModelId`）——
/// 于是客户端看到的名称可读，请求时再映射回 `key`。
fn parse_catalog(payload: &Value) -> Vec<Value> {
    let Some(chat) = payload.get("chat").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut models: Vec<Value> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for item in chat {
        let key = item.get("key").and_then(Value::as_str).unwrap_or("").trim();
        let display = item.get("display_name").and_then(Value::as_str).unwrap_or("").trim();
        if key.is_empty() || display.is_empty() {
            continue;
        }
        let id: String = display.chars().filter(|ch| !ch.is_whitespace()).collect();
        if id.is_empty() {
            continue;
        }
        let lowered = id.to_lowercase();
        if seen.iter().any(|known| known == &lowered) {
            continue;
        }
        seen.push(lowered);

        let vision = item.get("is_vl").map(js_truthy).unwrap_or(false);
        let reasoning = item.get("is_reasoning").map(js_truthy).unwrap_or(false)
            || item.get("thinking_config").map(js_truthy).unwrap_or(false);
        // 上游在该模型条目里声明支持的思考档位，作为请求时的白名单
        let efforts: Vec<Value> = item
            .pointer("/thinking_config/enabled/efforts")
            .and_then(Value::as_object)
            .map(|map| map.keys().map(|key| Value::String(key.clone())).collect())
            .unwrap_or_default();
        let context_window = context_window_of(item);
        let source = item.get("source").and_then(Value::as_str).unwrap_or("system");
        // 倍率（官方中文名「Credit 消耗倍率」，界面标签「消耗」）：
        // 上游在模型条目**顶层**给 `price_factor`，取值 0～3.2。
        // 折扣时段 `price_factor` 本身就是折后价（另有 `original_price_factor`
        // 记原价）—— 这里取实际生效的那个，与 Qoder 界面一致。
        let credits = credits_of(item);

        let mut model = entry(
            &id,
            key,
            reasoning,
            &[],
            vision,
            item.get("enable").map(js_truthy).unwrap_or(false),
            context_window,
            source,
            &credits,
        );
        if let Some(object) = model.as_object_mut() {
            object.insert(
                "name".to_string(),
                Value::String(display.to_string()),
            );
            object.insert("efforts".to_string(), Value::Array(efforts));
            if let Some(format) = item.get("format") {
                if !format.is_null() {
                    if let Some(config) = object.get_mut("config").and_then(Value::as_object_mut) {
                        config.insert("format".to_string(), format.clone());
                    }
                }
            }
        }
        models.push(model);
    }
    models
}

/// 上下文窗口：取 `context_config` 里各档位的最大 `token_count`（源实现同款）
fn context_window_of(item: &Value) -> i64 {
    let Some(config) = item.get("context_config").and_then(Value::as_object) else {
        return DEFAULT_CONTEXT_WINDOW;
    };
    let max = config
        .values()
        .filter_map(|entry| entry.get("token_count").and_then(Value::as_i64))
        .max()
        .unwrap_or(0);
    if max > 0 {
        max
    } else {
        DEFAULT_CONTEXT_WINDOW
    }
}

/// 条目里某个键的文本形态（沿用小浣熊那边同一份 JS 语义）
fn text_of(value: &Value, key: &str) -> String {
    value
        .get(key)
        .map(super::super::raccoon::jwt::js_text)
        .unwrap_or_default()
}

/// JS 真值判定（`Boolean(x)`）：null/false/0/"" 为假，其余为真
fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().map(|item| item != 0.0).unwrap_or(false),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// 供排障：清单条数
#[allow(dead_code)]
pub fn count() -> usize {
    list().len()
}
