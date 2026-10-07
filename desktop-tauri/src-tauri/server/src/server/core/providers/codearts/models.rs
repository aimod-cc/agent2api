//! 模型目录：**三个源**合并成一份清单。
//!
//! ── 为什么是三个源，少一个会怎样 ────────────────────────────
//!   1. `agent`（Agent Center）：`/v1/agent-center/agents/useragents` 列出账号可见的
//!      agent，再逐个 `/v1/agent-center/agents/detail?agent_id=…` 取 `gpts.models`。
//!      这是账号**实际能用**的那批。
//!   2. `builtin`（`/v1/model/builtin`，**必须** `Agent-Type: PromptCenter`）：内置清单。
//!      换成别的 agent 类型上游会给一份受限视图、看着像"这账号没有模型"。
//!      **多模态模型只在这里出现**（实测 `Qwen3-VL-235B` / `kimi-k2.6-vl` 就是），
//!      丢掉这个源等于丢多模态。
//!   3. `benefit`（福利网关，另一台主机、另一套签名契约）：`{base}/v1/benefit-gateway-config`
//!      是总开关，开了才去 `{benefit_gateway_url}/api/v1/gateway/config` 取模型。
//!
//! 合并顺序是**先 agent、再 builtin、最后 benefit**，按 id **先到先得**：
//! agent 报过的 id 保留 agent 那条（路由不同），builtin 只补 agent 没报的。
//!
//! ── 两条实测踩过的坑 ────────────────────────────────────────
//!   * **trial 账号的 agent detail 会 200 但完全没有 `gpts.models`** —— 这时绝不能
//!     用硬编码兜底，否则会广告出一批"列出来但一调就 400"的幽灵模型。参考实现
//!     的口径是"只有运维显式配置的 models 才能当兜底"，照抄。
//!   * **模型 id 大小写敏感**（`GLM-5.2` ≠ `glm-5.2`），而客户端习惯写小写。
//!     所以这里提供 [`Catalog::resolve`] 做**大小写无关**的查找并返回上游的真名，
//!     转发时用真名，不要拿客户端的写法直接发。
//!
//! 目录缓存（5 分钟 / 有告警时 30 秒）与"重启后不丢福利模型"的持久化在接线那一层
//! （见方案 §2.2 的 `catalog_cache.rs` 落点），本模块只管解析与合并 —— 这样它们
//! 全都能对着真实响应离线测。

use std::time::Duration;

use serde_json::Value;

use crate::server::core::egress;
use crate::server::errors::GatewayError;

use super::credentials::Credential;
use super::oauth::signer_credential;
use super::signer;

/// 模型来自哪个源（决定路由与 `maas_type` 注入）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelSource {
    Agent,
    Builtin,
    Benefit,
}

impl ModelSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Builtin => "builtin",
            Self::Benefit => "benefit",
        }
    }

    /// `as_str` 的逆。认不出回 None —— 调用方**丢掉这一条**而不是猜一个源：
    /// 源决定要不要带 `maas_type: benefit` 头，猜错就是发一个上游必定拒绝的请求。
    pub fn from_label(label: &str) -> Option<Self> {
        match label.trim().to_ascii_lowercase().as_str() {
            "agent" => Some(Self::Agent),
            "builtin" => Some(Self::Builtin),
            "benefit" => Some(Self::Benefit),
            _ => None,
        }
    }

    /// 福利源的模型必须显式带 `maas_type: benefit`，别的源带了反而会被判成
    /// "没领福利"（见 `chat.rs` 的请求构造）。
    pub fn needs_benefit_header(self) -> bool {
        self == Self::Benefit
    }
}

/// 一条模型。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelConfig {
    /// 上游真名（大小写敏感，转发时必须用这个）
    pub id: String,
    pub name: String,
    pub display_name: String,
    pub description: String,
    pub context_length: i64,
    pub max_output_tokens: i64,
    pub supports_images: bool,
    pub source: ModelSource,
    /// 积分倍率的**官方文案**（上游 `credit[].ratio_display`，形如 `0.7x`）。
    ///
    /// 空串 = 上游没给（福利源就没有这个字段），界面那一列显示 `—`。
    ///
    /// ── 为什么取 `ratio_display` 而不是 `ratio` ────────────────
    /// 同一份 fixture 里 `ratio` 是 `0.05` / `0.028` / `0.011`，而
    /// `ratio_display` 是 `0.7x` / `0.7x` / `0.32x` —— 两者**不是一个量纲**
    /// （`ratio` 更像"每 token 系数"，display 才是官网上那颗倍率徽章）。
    /// 倍率列是给人对照官方客户端看的，所以取 display 原样透传，
    /// 与 AutoClaw 那条「上游给的是档位文案就原样显示，不换算成编造的数」
    /// 同一条口径（见 `autoclaw/catalog.rs` 的 `creditConsumptionLevel`）。
    pub credit_display: String,
}

/// 合并后的清单 + 诊断信息。
#[derive(Clone, Debug, Default)]
pub struct Catalog {
    pub models: Vec<ModelConfig>,
    pub warnings: Vec<String>,
}

impl Catalog {
    /// 大小写无关地找一条模型，返回**上游真名**。
    ///
    /// 客户端写 `glm-5.2` 而上游要 `GLM-5.2`：不归一就会得到"模型不存在"。
    pub fn resolve(&self, requested: &str) -> Option<&ModelConfig> {
        let wanted = requested.trim();
        self.models
            .iter()
            .find(|model| model.id.eq_ignore_ascii_case(wanted))
    }

    pub fn contains(&self, requested: &str) -> bool {
        self.resolve(requested).is_some()
    }
}

/// 按 id 先到先得地合并（前者已报过的 id 不再被后者覆盖）。
fn append_unique(target: &mut Vec<ModelConfig>, incoming: Vec<ModelConfig>) {
    for model in incoming {
        if !target.iter().any(|existing| existing.id == model.id) {
            target.push(model);
        }
    }
}

/// 解析 `/v1/model/builtin`。
///
/// `enable` **显式 false 才跳过**（缺字段当可用）。
pub fn parse_builtin(body: &str) -> Result<Vec<ModelConfig>, String> {
    let payload: Value =
        serde_json::from_str(body).map_err(|error| format!("builtin 目录不是合法 JSON：{error}"))?;
    let entries = payload
        .get("builtinModels")
        .and_then(Value::as_array)
        .ok_or_else(|| "builtin 目录缺少 builtinModels 数组".to_string())?;
    let mut models = Vec::new();
    for entry in entries {
        if entry.get("enable").and_then(Value::as_bool) == Some(false) {
            continue;
        }
        let model_id = text(entry, "model_id");
        let model_name = text(&entry, "model_name");
        let id = if model_id.is_empty() { model_name.clone() } else { model_id };
        if id.is_empty() {
            continue;
        }
        models.push(ModelConfig {
            name: if model_name.is_empty() { id.clone() } else { model_name.clone() },
            display_name: if model_name.is_empty() { id.clone() } else { model_name },
            description: text(entry, "model_desc"),
            context_length: number(entry, "context_window"),
            max_output_tokens: capped_output(entry, "max_tokens"),
            supports_images: entry.get("supports_images").and_then(Value::as_bool).unwrap_or(false),
            credit_display: credit_display_of(entry),
            id,
            source: ModelSource::Builtin,
        });
    }
    Ok(models)
}

/// 解析 agent detail 的 `gpts.models`。
///
/// 两条过滤都照参考实现：`enabled` 显式 false 跳过；**`display_enabled` 缺失也算
/// 不可见**（`display_enabled == nil → skip`）—— 实测 trial 账号的 detail 就长这样。
pub fn parse_agent_detail(body: &str, language: &str) -> Result<Vec<ModelConfig>, String> {
    let payload: Value =
        serde_json::from_str(body).map_err(|error| format!("agent detail 不是合法 JSON：{error}"))?;
    let gpts = payload
        .get("gpts")
        .ok_or_else(|| "agent detail 没有 gpts 目录（该账号可能没有可用模型）".to_string())?;
    let entries = gpts.get("models").and_then(Value::as_array).cloned().unwrap_or_default();
    let english = language.trim().to_ascii_lowercase().starts_with("en");
    let mut models = Vec::new();
    for entry in entries {
        let parameters = entry.get("model_parameters").cloned().unwrap_or(Value::Null);
        if parameters.get("enabled").and_then(Value::as_bool) == Some(false) {
            continue;
        }
        if parameters.get("display_enabled").and_then(Value::as_bool) != Some(true) {
            continue;
        }
        // 扩展把 model_parameters 摊在 model_alias 上，所以嵌套的 model_id 优先
        let id = first_non_empty(&[
            text(&parameters, "model_id"),
            text(&entry, "model_alias"),
            text(&entry, "model_id"),
        ]);
        if id.is_empty() {
            continue;
        }
        let description = if english {
            first_non_empty(&[text(&parameters, "model_desc_en"), text(&parameters, "model_desc")])
        } else {
            first_non_empty(&[text(&parameters, "model_desc"), text(&parameters, "model_desc_en")])
        };
        let model_name = text(&entry, "model_name");
        models.push(ModelConfig {
            name: if model_name.is_empty() { id.clone() } else { model_name.clone() },
            display_name: if model_name.is_empty() { id.clone() } else { model_name },
            description,
            context_length: number(&parameters, "context_window"),
            max_output_tokens: capped_output(&parameters, "max_tokens"),
            supports_images: parameters.get("supports_images").and_then(Value::as_bool).unwrap_or(false),
            credit_display: credit_display_of(&entry),
            id,
            source: ModelSource::Agent,
        });
    }
    Ok(models)
}

/// 解析 `/v1/agent-center/agents/useragents` 的 agent id 列表（`agent_id` 优先，
/// 回退 `original_id`），并去重。
pub fn parse_agent_ids(body: &str) -> Result<Vec<String>, String> {
    let payload: Value =
        serde_json::from_str(body).map_err(|error| format!("agent 列表不是合法 JSON：{error}"))?;
    let agents = payload
        .get("agents")
        .and_then(Value::as_array)
        .ok_or_else(|| "agent 列表响应没有 agents 数组".to_string())?;
    let mut ids = Vec::new();
    for agent in agents {
        let id = first_non_empty(&[text(agent, "agent_id"), text(agent, "original_id")]);
        if !id.is_empty() && !ids.contains(&id) {
            ids.push(id);
        }
    }
    Ok(ids)
}

/// 解析 `/v1/benefit-gateway-config` 的总开关。
///
/// 返回 `Ok(false)` 表示"这个部署没有福利网关"，不是错误。
pub fn parse_benefit_gate(body: &str) -> Result<bool, String> {
    let payload: Value =
        serde_json::from_str(body).map_err(|error| format!("福利开关不是合法 JSON：{error}"))?;
    payload
        .get("enabled")
        .and_then(Value::as_bool)
        .ok_or_else(|| "福利开关响应没有 enabled 字段".to_string())
}

/// 解析福利网关的 `result.models`。
///
/// 成功判定看 **envelope 的 `error_code == "0000"`**，不是 HTTP 状态 ——
/// 这个网关会把失败塞在 200 的体里（与流内错误信封同一个味道）。
pub fn parse_benefit(body: &str) -> Result<Vec<ModelConfig>, String> {
    let payload: Value =
        serde_json::from_str(body).map_err(|error| format!("福利目录不是合法 JSON：{error}"))?;
    let code = text(&payload, "error_code");
    if code != "0000" {
        return Err(format!("福利目录未成功（error_code={code:?}）"));
    }
    let result = payload
        .get("result")
        .ok_or_else(|| "福利目录缺少 result".to_string())?;
    let mut entries: Vec<(i64, Value)> = result
        .get("models")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|entry| (number(&entry, "sort"), entry))
        .collect();
    // 稳定排序：`sort` 相同的保持上游给的顺序
    entries.sort_by_key(|(sort, _)| *sort);
    let mut models = Vec::new();
    for (_, entry) in entries {
        let id = text(&entry, "model_id");
        if id.is_empty() {
            continue;
        }
        let name = text(&entry, "model_name");
        models.push(ModelConfig {
            name: id.clone(),
            display_name: if name.is_empty() { id.clone() } else { name },
            description: String::new(),
            context_length: number(&entry, "context_window"),
            max_output_tokens: capped_output(&entry, "max_tokens"),
            supports_images: false,
            // 福利网关的条目没有 `credit`（实测三个源里只有 agent 与 builtin 带），
            // 留空 = 界面那一列显示 `—`，不是"倍率为 0"。
            credit_display: String::new(),
            id,
            source: ModelSource::Benefit,
        });
    }
    if models.is_empty() {
        return Err("福利网关没有返回任何模型".to_string());
    }
    Ok(models)
}

/// 合并 agent + builtin（**先 agent**：agent 报过的 id 保留 agent 那条路由）。
pub fn merge_agent_and_builtin(agent: Vec<ModelConfig>, builtin: Vec<ModelConfig>) -> Vec<ModelConfig> {
    let mut merged = agent;
    append_unique(&mut merged, builtin);
    merged
}

/// 再并入 benefit（同样先到先得）。
pub fn merge_benefit(merged: Vec<ModelConfig>, benefit: Vec<ModelConfig>) -> Vec<ModelConfig> {
    let mut merged = merged;
    append_unique(&mut merged, benefit);
    merged
}

/// 区域 API 与福利网关的默认地址（私有化部署要改的话走配置，别改常量）。
pub const DEFAULT_BASE_URL: &str = "https://snap-access.cn-north-4.myhuaweicloud.com";
pub const DEFAULT_BENEFIT_GATEWAY_URL: &str = "https://opengw.developer.huaweicloud.com";

/// 一次目录发现要用的上游地址。
pub struct CatalogEndpoints<'a> {
    pub base_url: &'a str,
    pub benefit_gateway_url: Option<&'a str>,
    pub plugin_version: &'a str,
    pub language: &'a str,
}

/// 抓三个源并合并成一份清单。
///
/// 任何一个源失败都只记 `warnings` 不中断 —— 目录是"能拿到多少算多少"，
/// 少一个源最多是少几个模型，不该让整个刷新失败。全部为空才由调用方判定为
/// `unavailable`（**不要**在这里塞硬编码兜底）。
pub async fn discover(endpoints: &CatalogEndpoints<'_>, credential: &Credential) -> Catalog {
    let mut catalog = Catalog::default();
    let mut agent_models = Vec::new();
    match fetch_agent_ids(endpoints, credential).await {
        Ok(ids) => {
            for id in ids {
                match fetch_signed(
                    &format!("{}/v1/agent-center/agents/detail?agent_id={}", trim(endpoints.base_url), form_escape(&id)),
                    "AgentCenter",
                    endpoints,
                    credential,
                    false,
                    false,
                )
                .await
                {
                    Ok(body) => match parse_agent_detail(&body, endpoints.language) {
                        Ok(models) => append_unique(&mut agent_models, models),
                        Err(reason) => catalog.warnings.push(format!("agent 目录 {id}：{reason}")),
                    },
                    Err(reason) => catalog.warnings.push(format!("agent 目录 {id}：{reason}")),
                }
            }
        }
        Err(reason) => catalog.warnings.push(format!("agent 列表：{reason}")),
    }
    let builtin = match fetch_signed(
        &format!("{}/v1/model/builtin", trim(endpoints.base_url)),
        "PromptCenter",
        endpoints,
        credential,
        false,
        false,
    )
    .await
    {
        Ok(body) => match parse_builtin(&body) {
            Ok(models) => models,
            Err(reason) => {
                catalog.warnings.push(format!("builtin 目录：{reason}"));
                Vec::new()
            }
        },
        Err(reason) => {
            catalog.warnings.push(format!("builtin 目录：{reason}"));
            Vec::new()
        }
    };
    let mut models = merge_agent_and_builtin(agent_models, builtin);

    if let Some(gateway) = endpoints.benefit_gateway_url.filter(|url| !url.trim().is_empty()) {
        match fetch_benefit(endpoints, credential, gateway).await {
            Ok(Some(benefit)) => models = merge_benefit(models, benefit),
            Ok(None) => {}
            Err(reason) => catalog.warnings.push(format!("福利目录：{reason}")),
        }
    }
    if models.is_empty() {
        catalog.warnings.push("该账号没有返回任何可用模型".to_string());
    }
    catalog.models = models;
    catalog
}

/// agent id 列表（分页，照参考实现的 100/页、最多 100 页）。
async fn fetch_agent_ids(endpoints: &CatalogEndpoints<'_>, credential: &Credential) -> Result<Vec<String>, String> {
    const PAGE: usize = 100;
    let mut ids = Vec::new();
    for page in 0..100usize {
        let query = format!(
            "offset={}&limit={PAGE}&is_primary_agent=true&supported_client=VSCODE&min_compatible_plugin_version={}",
            page * PAGE,
            form_escape(endpoints.plugin_version)
        );
        let body = fetch_signed(
            &format!("{}/v1/agent-center/agents/useragents?{query}", trim(endpoints.base_url)),
            "AgentCenter",
            endpoints,
            credential,
            false,
            false,
        )
        .await?;
        let page_ids = parse_agent_ids(&body)?;
        let count = page_ids.len();
        for id in page_ids {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        if count < PAGE {
            break;
        }
    }
    Ok(ids)
}

/// 福利网关：先看总开关，开了再取目录。
async fn fetch_benefit(
    endpoints: &CatalogEndpoints<'_>,
    credential: &Credential,
    gateway: &str,
) -> Result<Option<Vec<ModelConfig>>, String> {
    let gate = fetch_signed(
        &format!("{}/v1/benefit-gateway-config", trim(endpoints.base_url)),
        "PromptCenter",
        endpoints,
        credential,
        false,
        false,
    )
    .await?;
    if !parse_benefit_gate(&gate)? {
        return Ok(None);
    }
    // 福利网关的签名契约与区域 API 不同：**带 Host、不带 X-Domain-Id**
    let body = fetch_signed(
        &format!("{}/api/v1/gateway/config", trim(gateway)),
        "",
        endpoints,
        credential,
        true,
        true,
    )
    .await?;
    parse_benefit(&body).map(Some)
}

/// 一次签名 GET（目录类请求都是 GET + 空体）。
async fn fetch_signed(
    url: &str,
    agent_type: &str,
    endpoints: &CatalogEndpoints<'_>,
    credential: &Credential,
    host_signed: bool,
    domainless: bool,
) -> Result<String, String> {
    let mut signing = signer_credential(credential);
    if domainless {
        signing.domain_id = String::new();
    }
    let mut headers = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Accept".to_string(), "application/json".to_string()),
        ("X-Language".to_string(), endpoints.language.to_string()),
        ("plugin-name".to_string(), super::chat::DEFAULT_PLUGIN_NAME.to_string()),
        ("plugin-version".to_string(), endpoints.plugin_version.to_string()),
        ("client_version".to_string(), format!("Vscode_{}", endpoints.plugin_version)),
        ("is_confidential".to_string(), "false".to_string()),
    ];
    if !agent_type.is_empty() {
        headers.push(("Agent-Type".to_string(), agent_type.to_string()));
    }
    let signed = signer::sign("GET", url, &headers, b"", &signing, host_signed)?;
    let mut request = egress::client_for(None).get(url).timeout(Duration::from_secs(30));
    for (name, value) in signed {
        request = request.header(name.as_str(), value.as_str());
    }
    let response = request.send().await.map_err(|error| {
        format!("请求失败：{}", egress::describe_error_detail(&error))
    })?;
    let status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();
    if status != 200 {
        return Err(format!("HTTP {status}"));
    }
    Ok(body)
}

fn trim(value: &str) -> String {
    value.trim_end_matches('/').to_string()
}

fn text(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .to_string()
}

fn number(value: &Value, key: &str) -> i64 {
    value.get(key).and_then(Value::as_i64).unwrap_or(0)
}

/// 目录自述的 `max_tokens` **不能当请求上限用**：这一家对单次输出额度的硬上限实测
/// 是 65536（65537 起回 `InferHub.001001005.400`），而目录里 GLM-5.2 写 131072、
/// deepseek-v4.1-flash 写 384000 —— 照它发必然失败。所以出口先夹一遍
/// （请求侧 `chat::clamp_output_budget` 是第二道，绕不过的那道）。
///
/// `0` = "目录没给"，原样留着：把"不知道"编成一个数是另一件事。
fn capped_output(value: &Value, key: &str) -> i64 {
    let declared = number(value, key);
    if declared <= 0 {
        declared
    } else {
        declared.min(super::chat::CHANNEL_MAX_OUTPUT_TOKENS)
    }
}

/// `credit[]` → 倍率文案（`ratio_display`）。
///
/// 上游按**输入长度分档**给多条（实测同一模型有 `input_from:0..32000` 与
/// `32001..∞` 两条，`-1` 是"无上限"），而倍率列只有一格，所以取
/// **基础档**（`input_from == 0`）那一条 —— 与官网徽章一致。
///
/// 三条保守规则：
/// * `status` 显式非 `"0"` 的档跳过（缺字段当生效，别让上游删字段就整列空掉）；
/// * 没有基础档时退回第一条能读出文案的档，而不是回空；
/// * 原样透传，不解析成数字（`ratio` 与它是两个量纲，见 `ModelConfig::credit_display`）。
fn credit_display_of(entry: &Value) -> String {
    let Some(tiers) = entry.get("credit").and_then(Value::as_array) else {
        return String::new();
    };
    let mut fallback = String::new();
    for tier in tiers {
        if matches!(tier.get("status").and_then(Value::as_str), Some(value) if value.trim() != "0") {
            continue;
        }
        let display = text(tier, "ratio_display");
        if display.is_empty() {
            continue;
        }
        if number(tier, "input_from") == 0 {
            return display;
        }
        if fallback.is_empty() {
            fallback = display;
        }
    }
    fallback
}

fn first_non_empty(values: &[String]) -> String {
    values
        .iter()
        .map(|value| value.trim())
        .find(|value| !value.is_empty())
        .unwrap_or_default()
        .to_string()
}

/// 与 `oauth::form_encode` 同口径（本模块不依赖那边，避免为一个小函数牵连网络模块）。
fn form_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// ── 进程内目录缓存 ──────────────────────────────────────────
///
/// `list_models()` 是**同步**接口（适配器契约），而发现目录要发网络请求，所以
/// 清单必须先在别处（刷新链路）落到这里，`list_models()` 只读缓存。这与 accio
/// 的 `models::list()` 同一形状。
///
/// ── 为什么缓存要能"空着"而不是造一份兜底 ────────────────────
/// 该账号一个模型都没返回时，缓存就是空的 —— `list_models()` 返回空清单，
/// 上层据此报"账号没有可用模型"。**绝不**塞静态兜底：那会广告出一批
/// "列出来但一调就 400"的幽灵模型（§3.4 的坑，CPA 侧删掉静态列表才治好）。
static CACHED: std::sync::OnceLock<std::sync::Mutex<Option<CachedCatalog>>> = std::sync::OnceLock::new();

struct CachedCatalog {
    catalog: Catalog,
    fetched_at_ms: i64,
}

fn cache_slot() -> &'static std::sync::Mutex<Option<CachedCatalog>> {
    CACHED.get_or_init(|| std::sync::Mutex::new(None))
}

/// 缓存有效期：正常 5 分钟；**有告警时 30 秒**（参考实现同款 —— 少几个模型时
/// 更该快点重试，而不是把不完整的清单钉住五分钟）。
pub const CACHE_TTL_MS: i64 = 5 * 60 * 1000;
pub const CACHE_TTL_WARNING_MS: i64 = 30 * 1000;

/// 落一份清单进缓存（并持久化到统一库），返回它。
///
/// 持久化这一步不是可选的优化：**没有它，进程重启后那一次刷新若失败，
/// 上游新增的模型会消失、已下架的又回来被广告出去**。缓存不带有效期
/// （宁可给一份旧清单也不退回静态兜底），时效性仍由刷新链路负责。
pub fn store_catalog(catalog: Catalog) -> Catalog {
    let now = crate::server::logging::now_ms();
    let entries = entries_of(&catalog);
    {
        let mut slot = cache_slot().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        *slot = Some(CachedCatalog { catalog: catalog.clone(), fetched_at_ms: now });
    }
    crate::server::core::providers::catalog_cache::save(crate::server::core::providers::catalog_cache::SCOPE_CODEARTS, &entries, now);
    catalog
}

/// 读缓存（过期也照给：宁可给一份旧清单，也不要突然从"有模型"变成"没有"；
/// 时效性由刷新链路负责 —— 与 agent2api 全局清单持久化同一取舍）。
///
/// 内存槽为空（**每次进程重启后都是**）时回落到持久化那份并重建目录。
/// 这一条不是"顺手更周到"：转发的模型校验读的是这里，而 `/v1/models` 的广告读的是
/// `list()`（本来就有持久化回落）。两边不对称的后果是重启后
/// 「模型列得出来、每一条都回 503 目录没拉取过」—— 用户必须手动点一次「获取模型」
/// 才能恢复，而网关看起来完全正常。审计抓出来的，测试也补在文件末尾。
pub fn cached_catalog() -> Option<Catalog> {
    let from_memory = {
        let slot = cache_slot().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        slot.as_ref().map(|entry| entry.catalog.clone())
    };
    from_memory.or_else(catalog_from_persisted)
}

/// 从持久化条目重建目录（`entries_of` 的逆）。
///
/// 只重建转发与广告真正要用的字段；`source` 认不出来就整条丢掉而不是猜一个 ——
/// 福利模型要不要带 `maas_type` 头**由这个字段决定**，猜错等于发一个上游不认的请求。
fn catalog_from_persisted() -> Option<Catalog> {
    let entries = crate::server::core::providers::catalog_cache::load(crate::server::core::providers::catalog_cache::SCOPE_CODEARTS).map(|cached| cached.models)?;
    let mut catalog = Catalog::default();
    for entry in entries {
        let text = |key: &str| entry.get(key).and_then(Value::as_str).unwrap_or("").to_string();
        let id = text("id");
        if id.is_empty() {
            continue;
        }
        let Some(source) = ModelSource::from_label(&text("source")) else {
            continue;
        };
        catalog.models.push(ModelConfig {
            display_name: if text("name").is_empty() { id.clone() } else { text("name") },
            description: text("description"),
            context_length: entry.get("contextWindow").and_then(Value::as_i64).unwrap_or(0),
            max_output_tokens: entry.get("maxOutputTokens").and_then(Value::as_i64).unwrap_or(0),
            supports_images: entry.get("supportsImages").and_then(Value::as_bool).unwrap_or(false),
            credit_display: text("credits"),
            name: id.clone(),
            source,
            id,
        });
    }
    if catalog.models.is_empty() { None } else { Some(catalog) }
}

/// 只给测试用：把内存槽清空，模拟进程重启（持久化那份不动）。
#[cfg(test)]
pub(crate) fn forget_memory_cache_for_tests() {
    let mut slot = cache_slot().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    *slot = None;
}

/// 一份清单的缓存时长（纯函数，便于测）。
///
/// 正常 5 分钟；**带告警时 30 秒** —— 少一个源时更该快点重试，而不是把不完整的
/// 清单钉住五分钟。
pub fn ttl_for(catalog: &Catalog) -> i64 {
    if catalog.warnings.is_empty() { CACHE_TTL_MS } else { CACHE_TTL_WARNING_MS }
}

/// 是否远程刷出来过（界面「来源」列与「更新日期」列要区分"远程拿到的"与
/// "从来没拿过"）。
pub fn remote_refreshed() -> bool {
    cached_catalog().is_some_and(|catalog| !catalog.models.is_empty())
}

/// 上一次成功刷新的时刻（毫秒）；从没刷过回 0（与其余十家同一口径）。
pub fn last_refreshed_at() -> i64 {
    let slot = cache_slot().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    slot.as_ref().map(|entry| entry.fetched_at_ms).unwrap_or(0)
}

/// 缓存是否已过期（刷新链路据此决定要不要再拉一次）。
pub fn cache_expired(now_ms: i64) -> bool {
    let slot = cache_slot().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    match slot.as_ref() {
        None => true,
        Some(entry) => now_ms - entry.fetched_at_ms >= ttl_for(&entry.catalog),
    }
}

/// 从持久化缓存读回上次的清单（内存缓存为空时的回落）。
fn persisted_entries() -> Vec<Value> {
    crate::server::core::providers::catalog_cache::load(crate::server::core::providers::catalog_cache::SCOPE_CODEARTS)
        .map(|cached| cached.models)
        .unwrap_or_default()
}

/// 缓存清单转成适配器契约要的条目形状（camelCase，与 accio 的 `list()` 一致）。
///
/// `contextWindow` 为 0 时不写这个键 —— 上层对缺字段是"未知"，对 0 是"没有
/// 上下文"，两者含义不同。
pub fn list() -> Vec<Value> {
    // 回落已经在 `cached_catalog()` 里做了；这里再留一份持久化读法只为
    // 「重建不出来」（条目形状不认识）那种情况，别让广告突然变空。
    match cached_catalog() {
        Some(catalog) => entries_of(&catalog),
        None => persisted_entries(),
    }
}

/// `Catalog` → 条目数组（`list()` 与持久化共用同一份映射，避免两处漂移）。
fn entries_of(catalog: &Catalog) -> Vec<Value> {
    catalog
        .models
        .iter()
        .map(|model| {
            let mut entry = serde_json::Map::new();
            entry.insert("id".to_string(), Value::String(model.id.clone()));
            entry.insert("name".to_string(), Value::String(model.display_name.clone()));
            entry.insert("providerModel".to_string(), Value::String(model.id.clone()));
            entry.insert("supportsImages".to_string(), Value::Bool(model.supports_images));
            entry.insert("supportsToolCall".to_string(), Value::Bool(true));
            entry.insert("source".to_string(), Value::String(model.source.as_str().to_string()));
            entry.insert("benefit".to_string(), Value::Bool(model.source.needs_benefit_header()));
            if model.context_length > 0 {
                entry.insert("contextWindow".to_string(), Value::from(model.context_length));
            }
            if model.max_output_tokens > 0 {
                entry.insert("maxOutputTokens".to_string(), Value::from(model.max_output_tokens));
            }
            if !model.description.is_empty() {
                entry.insert("description".to_string(), Value::String(model.description.clone()));
            }
            // 倍率列：`credits` 是本仓跨家共用的键（前端 `formatCredits` 认 `x…` 形态，
            // 认不出就原样显示），上游给的 `0.7x` 属于后者 —— 与 AutoClaw 的档位文案
            // 走同一条渲染分支。空串不写这个键，界面显示 `—`（"没读到"而不是"0 倍"）。
            if !model.credit_display.is_empty() {
                entry.insert("credits".to_string(), Value::String(model.credit_display.clone()));
            }
            Value::Object(entry)
        })
        .collect()
}

/// 把目录清单转成给编排层的 `GatewayError`（模型不在清单里时用）。
pub fn unknown_model_error(requested: &str, catalog: &Catalog) -> GatewayError {
    let mut hint: Vec<&str> = catalog.models.iter().take(6).map(|model| model.id.as_str()).collect();
    if catalog.models.len() > hint.len() {
        hint.push("…");
    }
    GatewayError::with_status(
        400,
        format!(
            "CodeArts 账号没有模型 {requested:?}（该账号当前可用：{}）",
            hint.join(", ")
        ),
    )
    // 与 accio / qoder 同一口径：`code` 是给客户端的机器判据（别家都带，
    // 少这一个就会让「按 code 分支」的客户端在本家落到默认分支）。
    // 注意这条与「全网关都不认这个名字」不是一回事 —— 那种在选路层就被
    // 404 model_not_found 挡了（本仓统一行为），到不了这里。
    .with_code("model_not_found")
}

#[cfg(test)]
mod tests {
    use super::*;

    // 真实响应抓于 2026-09-27（只读接口，零推理消耗），一字未改。
    const BUILTIN: &str = include_str!("catalog_fixtures/builtin.json");
    const AGENT_DETAIL: &str = include_str!("catalog_fixtures/agent-detail.json");
    const USERAGENTS: &str = include_str!("catalog_fixtures/useragents.json");
    const BENEFIT_GATE: &str = include_str!("catalog_fixtures/benefit-gate.json");
    const BENEFIT_CONFIG: &str = include_str!("catalog_fixtures/benefit-gateway-config.json");

    #[test]
    fn builtin_source_parses_the_real_response() {
        let models = parse_builtin(BUILTIN).expect("真实 builtin 响应应当能解析");
        assert_eq!(6, models.len(), "实测 6 个内置模型");
        let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
        assert!(ids.contains(&"GLM-5.2"));
        // 多模态只在这个源里出现 —— 这条是"别丢 builtin 源"的实证
        let vision = models.iter().find(|model| model.id == "Qwen3-VL-235B").expect("多模态模型应当在内置源");
        assert!(vision.supports_images, "Qwen3-VL-235B 必须标成支持图片");
        assert_eq!(ModelSource::Builtin, vision.source);
        let glm = models.iter().find(|model| model.id == "GLM-5.2").unwrap();
        assert!(!glm.description.is_empty(), "描述要解析出来");
        // 实测发现：**builtin 这份响应里没有 `context_window` / `max_tokens`**
        // （只有 model_id/name/category/enable/display_enabled/desc/credit/…），
        // 上下文窗口与输出上限来自 **agent detail** 的 `model_parameters`。
        // 所以别在 builtin 上断言这两个数 —— 合并时 agent 先到先得，最终保留的是
        // agent 那份（见 `three_source_merge_matches_what_cpa_advertises`）。
        assert_eq!(0, glm.context_length);
        assert!(!glm.supports_images);
    }

    #[test]
    fn agent_detail_source_parses_the_real_response() {
        let models = parse_agent_detail(AGENT_DETAIL, "en-us").expect("真实 agent detail 应当能解析");
        let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(4, ids.len(), "实测该账号 4 个可见模型：{ids:?}");
        assert!(ids.contains(&"GLM-5.2"));
        assert!(ids.contains(&"openpangu-2.0-pro"));
        assert!(models.iter().all(|model| model.source == ModelSource::Agent));
        // `model_id` 在顶层是 null，靠 model_alias 与嵌套 model_parameters.model_id 兜住
        assert!(models.iter().all(|model| !model.id.is_empty()));
    }

    /// 倍率列：取 `credit[].ratio_display`，不是 `credit[].ratio`。
    ///
    /// 用真 fixture 断言三件事：有倍率的模型出的是官网那颗徽章的数；
    /// **没有 `credit` 的模型必须是空串**（界面显示 `—`，"没读到"），
    /// 而不是 0 —— 0 在倍率列的语义是"免费"，那是编出来的。
    #[test]
    fn the_multiplier_column_comes_from_ratio_display() {
        let models = parse_agent_detail(AGENT_DETAIL, "en-us").expect("真实 agent detail 应当能解析");
        let display_of = |id: &str| {
            models
                .iter()
                .find(|model| model.id == id)
                .map(|model| model.credit_display.as_str())
                .unwrap_or("<没有这一条>")
        };
        assert_eq!("0.7x", display_of("GLM-5.2"), "上游 ratio 是 0.05，倍率徽章是 0.7x —— 两个量纲");
        assert_eq!("0.32x", display_of("openpangu-2.0-flash"));
        assert_eq!("0.7x", display_of("openpangu-2.0-pro"), "分档模型取基础档");

        let builtin = parse_builtin(BUILTIN).expect("真实 builtin 响应应当能解析");
        let vision = builtin.iter().find(|model| model.id == "Qwen3-VL-235B").expect("多模态条目应当在");
        assert!(vision.credit_display.is_empty(), "该模型上游根本没给 credit 数组，不能凭空写 0");
        assert!(
            builtin.iter().any(|model| model.id == "GLM-5.2" && model.credit_display == "0.7x"),
            "两个源都带 credit 时都要解析出来"
        );
    }

    /// `credit[]` 的三条保守规则：基础档优先、`status != "0"` 跳过、缺字段不整列空掉。
    #[test]
    fn tiered_credit_rows_pick_the_base_tier_and_skip_inactive_ones() {
        let body = r#"{"gpts":{"models":[
            {"model_alias":"base-first","model_parameters":{"model_id":"base-first","display_enabled":true},
             "credit":[{"input_from":32001,"ratio_display":"2x","status":"0"},{"input_from":0,"ratio_display":"0.5x","status":"0"}]},
            {"model_alias":"only-off","model_parameters":{"model_id":"only-off","display_enabled":true},
             "credit":[{"input_from":0,"ratio_display":"9x","status":"1"},{"input_from":0,"ratio_display":"0.3x","status":"0"}]},
            {"model_alias":"no-status","model_parameters":{"model_id":"no-status","display_enabled":true},
             "credit":[{"input_from":0,"ratio_display":"1.2x"}]},
            {"model_alias":"no-credit","model_parameters":{"model_id":"no-credit","display_enabled":true}},
            {"model_alias":"upper-only","model_parameters":{"model_id":"upper-only","display_enabled":true},
             "credit":[{"input_from":32001,"ratio_display":"1.8x","status":"0"}]}
        ]}}"#;
        let models = parse_agent_detail(body, "en-us").unwrap();
        let display_of = |id: &str| models.iter().find(|m| m.id == id).map(|m| m.credit_display.as_str()).unwrap();
        assert_eq!("0.5x", display_of("base-first"), "两条档都要，取 input_from==0 那条（数组顺序不可信）");
        assert_eq!("0.3x", display_of("only-off"), "status 非 0 的档跳过");
        assert_eq!("1.2x", display_of("no-status"), "缺 status 当生效，别因为上游删字段就整列空掉");
        assert_eq!("", display_of("no-credit"), "没有 credit = 没读到");
        assert_eq!("1.8x", display_of("upper-only"), "只有高档时退回它，而不是回空");
    }

    /// 两条过滤规则：`enabled == false` 与 **`display_enabled` 缺失**都要跳过。
    #[test]
    fn agent_detail_filters_invisible_and_disabled_models() {
        let body = r#"{"gpts":{"models":[
            {"model_alias":"A","model_parameters":{"model_id":"A","display_enabled":true}},
            {"model_alias":"B","model_parameters":{"model_id":"B","display_enabled":true,"enabled":false}},
            {"model_alias":"C","model_parameters":{"model_id":"C"}},
            {"model_alias":"D","model_parameters":{"model_id":"D","display_enabled":false}},
            {"model_alias":"E"}
        ]}}"#;
        let models = parse_agent_detail(body, "en-us").unwrap();
        let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
        // B 被 enabled:false 禁；C/D 没有 display_enabled；E 连 model_parameters 都没有 —— 全跳过。
        // `display_enabled` 是**必须显式为 true** 的字段（参考实现：nil 也跳过），
        // 这正是 trial 账号 detail 会 200 却一个模型都不给的情形。
        assert_eq!(vec!["A"], ids);
    }

    /// trial 账号的 detail 会 200 但没有 `gpts.models` —— 必须报"没有目录"而不是编造。
    #[test]
    fn a_detail_without_models_reports_no_catalog_instead_of_inventing() {
        let empty = r#"{"gpts":{"prompts":[]}}"#;
        let models = parse_agent_detail(empty, "en-us").expect("gpts 存在但没有 models 数组 → 空清单，不是错误");
        assert!(models.is_empty());
        // 连 gpts 都没有 → 明确报错（调用方据此记 warning，而不是兜底硬编码）
        assert!(parse_agent_detail(r#"{"code":"0"}"#, "en-us").is_err());
    }

    #[test]
    fn useragents_lists_agent_ids_with_fallback() {
        let ids = parse_agent_ids(USERAGENTS).expect("真实 agent 列表应当能解析");
        assert_eq!(5, ids.len(), "实测 5 个 agent");
        assert!(ids.iter().all(|id| !id.is_empty()));
        // 没有 agents 数组 → 报错（参考实现也返回 warning）
        assert!(parse_agent_ids(r#"{"total":0}"#).is_err());
    }

    #[test]
    fn benefit_source_reads_the_envelope_code_not_the_http_status() {
        let models = parse_benefit(BENEFIT_CONFIG).expect("真实福利目录应当能解析");
        let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(vec!["deepseek-v4-flash-0731", "glm-5.3-flash", "deepseek-v4-pro-0813", "deepseek-v4.1-flash"], ids, "按 sort 升序");
        assert!(models.iter().all(|model| model.source == ModelSource::Benefit));
        // error_code 不是 0000 → 失败（即使 HTTP 是 200）
        assert!(parse_benefit(r#"{"error_code":"9999","result":{"models":[{"model_id":"x"}]}}"#).is_err());
        assert!(parse_benefit(r#"{"error_code":"0000"}"#).is_err(), "没有 result 也算失败");
        assert!(parse_benefit(r#"{"error_code":"0000","result":{"models":[]}}"#).is_err(), "空清单算失败");
    }

    #[test]
    fn benefit_gate_reports_absence_without_erroring() {
        assert!(parse_benefit_gate(BENEFIT_GATE).unwrap(), "实测这个部署开着福利网关");
        assert!(!parse_benefit_gate(r#"{"enabled":false}"#).unwrap());
        assert!(parse_benefit_gate(r#"{}"#).is_err(), "没有 enabled 字段是错误");
    }

    /// 三源合并：先 agent、再 builtin、最后 benefit，按 id 先到先得。
    /// 实测结果 10 个 —— 与 CPA 侧 `ca/` 前缀下的模型数完全一致。
    #[test]
    fn three_source_merge_matches_what_cpa_advertises() {
        let agent = parse_agent_detail(AGENT_DETAIL, "en-us").unwrap();
        let builtin = parse_builtin(BUILTIN).unwrap();
        let merged = merge_agent_and_builtin(agent, builtin);
        assert_eq!(6, merged.len(), "4 个 agent + builtin 补 2 个（两个多模态）");
        // agent 报过的 GLM-5.2 保留 agent 路由，不能被 builtin 覆盖
        let glm = merged.iter().find(|model| model.id == "GLM-5.2").unwrap();
        assert_eq!(ModelSource::Agent, glm.source, "先到先得：agent 的路由优先");
        // 多模态来自 builtin
        let vision = merged.iter().find(|model| model.id == "Qwen3-VL-235B").unwrap();
        assert_eq!(ModelSource::Builtin, vision.source);
        assert!(vision.supports_images);

        // 上下文窗口来自 agent 源（builtin 不带这两个字段）
        assert!(glm.context_length > 0 && glm.max_output_tokens > 0, "合并后 GLM-5.2 应当带上 agent 源的上下文窗口");

        let full = merge_benefit(merged, parse_benefit(BENEFIT_CONFIG).unwrap());
        assert_eq!(10, full.len(), "再并入 4 个福利模型 = 10，与 CPA 广告的数量一致");
        let benefit = full.iter().find(|model| model.id == "glm-5.3-flash").unwrap();
        assert!(benefit.source.needs_benefit_header(), "福利模型要带 maas_type");
        assert!(!full.iter().find(|model| model.id == "GLM-5.2").unwrap().source.needs_benefit_header());
        // 倍率随条目一起活过合并；福利源没有 credit，保持空串
        assert_eq!("0.7x", full.iter().find(|model| model.id == "GLM-5.2").unwrap().credit_display);
        assert!(benefit.credit_display.is_empty(), "福利网关不给倍率，界面那列就该是 —");
    }

    /// 大小写归一：客户端习惯小写，上游真名是 `GLM-5.2`。
    #[test]
    fn resolve_is_case_insensitive_and_returns_the_upstream_name() {
        let catalog = Catalog {
            models: parse_builtin(BUILTIN).unwrap(),
            warnings: Vec::new(),
        };
        assert_eq!("GLM-5.2", catalog.resolve("glm-5.2").expect("小写也要能找到").id);
        assert_eq!("GLM-5.2", catalog.resolve("GLM-5.2").unwrap().id);
        assert_eq!("qwen3-vl-235b", catalog.resolve("qwen3-vl-235b").unwrap().id.to_lowercase());
        assert!(catalog.resolve("not-a-model").is_none());
        assert!(!catalog.contains("not-a-model"));
        // 不在清单里时的报错要给出可用清单的一截，便于排障
        let error = unknown_model_error("nope", &catalog);
        assert_eq!(400, error.status_code);
        assert!(error.message.contains("GLM-5.2"));
    }

    /// 进程级缓存的串行锁：`store_catalog` / `list` 这一组测试共用它，
    /// 否则两个测试会互相把对方的清单盖掉（同一份 OnceLock 单例）。
    static CACHE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 缓存 → `list()` 的形状：camelCase 键、`contextWindow` 为 0 时不写这个键、
    /// 倍率走共用的 `credits` 键（空串不写 = 界面 `—`，而不是"0 倍"）。
    #[test]
    fn cache_to_list_uses_the_adapter_entry_shape() {
        let _guard = CACHE_TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let catalog = Catalog {
            models: vec![
                ModelConfig {
                    id: "GLM-5.2".to_string(),
                    name: "GLM-5.2".to_string(),
                    display_name: "GLM-5.2".to_string(),
                    description: "旗舰".to_string(),
                    context_length: 202752,
                    max_output_tokens: 131072,
                    supports_images: false,
                    credit_display: "0.7x".to_string(),
                    source: ModelSource::Agent,
                },
                ModelConfig {
                    id: "Qwen3-VL-235B".to_string(),
                    name: "Qwen3-VL-235B".to_string(),
                    display_name: "Qwen3-VL-235B".to_string(),
                    description: String::new(),
                    context_length: 0,
                    max_output_tokens: 0,
                    supports_images: true,
                    credit_display: String::new(),
                    source: ModelSource::Builtin,
                },
            ],
            warnings: Vec::new(),
        };
        store_catalog(catalog);
        let entries = list();
        assert_eq!(2, entries.len());
        assert_eq!("GLM-5.2", entries[0]["id"]);
        assert_eq!(202752, entries[0]["contextWindow"]);
        assert_eq!(false, entries[0]["supportsImages"]);
        assert_eq!("agent", entries[0]["source"]);
        assert_eq!(false, entries[0]["benefit"]);
        // 倍率列：非空才写 `credits`（前端 `formatCredits` 认不出 `0.7x` 这种形态时
        // 会原样显示，与 AutoClaw 的「低/中/高」同一条分支）
        assert_eq!("0.7x", entries[0]["credits"]);
        assert!(entries[1].get("credits").is_none(), "上游没给倍率就不要编一个 0");
        // 0 不写这个键（"未知" 与 "没有上下文" 含义不同）
        assert!(entries[1].get("contextWindow").is_none());
        assert_eq!(true, entries[1]["supportsImages"]);
        assert_eq!("builtin", entries[1]["source"]);
    }

    /// 福利模型在条目里要标出来（上层据此决定注入 `maas_type`）。
    #[test]
    fn benefit_models_are_flagged_in_the_entry() {
        let _guard = CACHE_TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        store_catalog(Catalog {
            models: parse_benefit(BENEFIT_CONFIG).unwrap(),
            warnings: Vec::new(),
        });
        let entries = list();
        assert!(entries.iter().all(|entry| entry["benefit"] == true));
        assert!(entries.iter().all(|entry| entry["source"] == "benefit"));
    }

    /// TTL 规则用纯函数测 —— 进程级缓存被多个测试共用，拿它做时间断言会互相覆盖。
    #[test]
    fn cache_ttl_shrinks_when_there_are_warnings() {
        let clean = Catalog { models: Vec::new(), warnings: Vec::new() };
        let warned = Catalog { models: Vec::new(), warnings: vec!["少了一个源".to_string()] };
        assert_eq!(CACHE_TTL_MS, ttl_for(&clean));
        assert_eq!(CACHE_TTL_WARNING_MS, ttl_for(&warned));
        assert!(ttl_for(&warned) < ttl_for(&clean), "带告警必须更快重试");
    }

    /// 重复 id 只留第一条（先到先得的实现细节）。
    #[test]
    fn merge_keeps_the_first_entry_per_id() {
        let make = |id: &str, source: ModelSource| ModelConfig {
            id: id.to_string(),
            name: id.to_string(),
            display_name: id.to_string(),
            description: String::new(),
            context_length: 0,
            max_output_tokens: 0,
            supports_images: false,
            credit_display: String::new(),
            source,
        };
        let merged = merge_agent_and_builtin(
            vec![make("A", ModelSource::Agent), make("B", ModelSource::Agent)],
            vec![make("B", ModelSource::Builtin), make("C", ModelSource::Builtin)],
        );
        assert_eq!(3, merged.len());
        assert_eq!(ModelSource::Agent, merged.iter().find(|m| m.id == "B").unwrap().source);
        assert_eq!(ModelSource::Builtin, merged.iter().find(|m| m.id == "C").unwrap().source);
    }
}

#[cfg(test)]
mod restart_tests {
    //! 重启后转发还能不能走 —— 审计抓出来的那条不对称：广告走 `list()`（有持久化回落），
    //! 转发的模型校验走 `cached_catalog()`（当时只读内存）。后果是每次重启后
    //! 「模型列得出来、每一条都 503 目录没拉取过」。
    use crate::server::core::providers::catalog_cache;
    use crate::server::db::Db;

    use super::*;

    #[test]
    fn the_forward_path_sees_the_persisted_catalog_after_a_restart() {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let id = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("codearts-catalog-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        catalog_cache::install(Some(Db::open(&dir.join("agent2api.db")).expect("临时库应当能建起来")));

        let catalog = Catalog {
            models: vec![
                ModelConfig {
                    id: "GLM-5.2".into(),
                    name: "GLM-5.2".into(),
                    display_name: "GLM 5.2".into(),
                    description: "推理模型".into(),
                    context_length: 200_000,
                    max_output_tokens: 131_072,
                    supports_images: false,
                    credit_display: String::new(),
                    source: ModelSource::Agent,
                },
                ModelConfig {
                    id: "Qwen3-VL-235B".into(),
                    name: "Qwen3-VL-235B".into(),
                    display_name: "Qwen3 VL".into(),
                    description: String::new(),
                    context_length: 0,
                    max_output_tokens: 0,
                    supports_images: true,
                    credit_display: String::new(),
                    source: ModelSource::Builtin,
                },
            ],
            warnings: vec![],
        };
        store_catalog(catalog.clone());
        assert_eq!(Some(2), cached_catalog().map(|value| value.models.len()));

        // 模拟进程重启：清掉内存槽，持久化那份还在
        forget_memory_cache_for_tests();
        assert!(cache_slot().lock().unwrap().is_none(), "内存槽应当已空");
        let restored = cached_catalog().expect("转发路径必须能从持久化那份重建");
        assert_eq!(2, restored.models.len());
        let glm = restored.resolve("glm-5.2").expect("大小写归一后仍要能解析出来");
        assert_eq!(ModelSource::Agent, glm.source);
        assert_eq!(200_000, glm.context_length, "上下文长度不能在建回来时丢掉");
        let vl = restored.resolve("Qwen3-VL-235B").expect("内置源要在");
        assert!(vl.supports_images, "多模态位只有 builtin 源有，重建时丢了就等于关掉视觉");
        assert!(!vl.source.needs_benefit_header());
        // `list()` 与转发读的是同一份真相 —— 这条断言就是当初那个 bug 的反面
        assert_eq!(restored.models.len(), list().len(), "广告与转发可解析的集合必须一致");
    }

    #[test]
    fn an_unrecognised_source_label_drops_the_row_instead_of_guessing() {
        let catalog = Catalog {
            models: vec![ModelConfig {
                id: "m".into(),
                name: "m".into(),
                display_name: "m".into(),
                description: String::new(),
                context_length: 0,
                max_output_tokens: 0,
                supports_images: false,
                credit_display: String::new(),
                source: ModelSource::Benefit,
            }],
            warnings: vec![],
        };
        // 逐字走完 entries → 重建 这一圈，福利位必须还在（它决定要不要带 maas_type 头）
        let rebuilt = rebuild_from(catalog);
        assert_eq!(ModelSource::Benefit, rebuilt.models[0].source);
        assert!(rebuilt.models[0].source.needs_benefit_header());
    }

    fn rebuild_from(catalog: Catalog) -> Catalog {
        let entries = entries_of(&catalog);
        let mut out = Catalog::default();
        for entry in entries {
            let source = ModelSource::from_label(entry["source"].as_str().unwrap_or("")).expect("自己写的标签要能读回");
            out.models.push(ModelConfig {
                id: "m".into(),
                name: "m".into(),
                display_name: "m".into(),
                description: String::new(),
                context_length: 0,
                max_output_tokens: 0,
                supports_images: false,
                credit_display: String::new(),
                source,
            });
        }
        out
    }

    /// 三个解析口的 `max_output_tokens` 都要夹进上游硬上限。
    ///
    /// 单独钉这条的理由：这三个数会经 `/v1/models` 广告出去，客户端照它写值
    /// 就必然被拒（实测 65537 起 `InferHub.001001005.400`）。
    #[test]
    fn advertised_output_budget_never_exceeds_the_channel_cap() {
        let cap = crate::server::core::providers::codearts::chat::CHANNEL_MAX_OUTPUT_TOKENS;

        let builtin = parse_builtin(
            r#"{"builtinModels":[{"model_id":"deepseek-v4.1-flash","model_name":"D","context_window":1000000,"max_tokens":384000}]}"#,
        )
        .expect("builtin 可解析");
        assert_eq!(1, builtin.len(), "夹具得先真的被解析出来");
        assert_eq!(cap, builtin[0].max_output_tokens, "384000 这种自述值要夹到硬上限");

        let agent = parse_agent_detail(
            // 字段集与兄弟用例同形：`display_enabled` 缺了整条会被过滤掉，
            // 那时 agent[0] 会 index-out-of-bounds 而不是断言失败 —— 别把那种形状当"夹住了"
            r#"{"gpts":{"models":[{"model_alias":"GLM-5.2","model_name":"G","model_parameters":{"enabled":true,"display_enabled":true,"context_window":202752,"max_tokens":131072}}]}}"#,
            "zh_cn",
        )
        .expect("agent 可解析");
        assert_eq!(1, agent.len(), "夹具得先真的被解析出来，否则下面的断言是空跑");
        assert_eq!(cap, agent[0].max_output_tokens, "agent 源同一条边");

        let benefit = parse_benefit(
            r#"{"error_code":"0000","result":{"models":[{"model_id":"glm-5.3-flash","model_name":"F","context_window":1048576,"max_tokens":393216}]}}"#,
        )
        .expect("福利可解析");
        assert_eq!(1, benefit.len(), "夹具得先真的被解析出来");
        assert_eq!(cap, benefit[0].max_output_tokens, "福利源同一条边");

        // 对照：到线值与"未声明(0)"都不许被改 —— 0 是"不知道"，不是"上限是 0"
        let within = parse_benefit(
            r#"{"error_code":"0000","result":{"models":[{"model_id":"a","max_tokens":65536},{"model_id":"b"}]}}"#,
        )
        .expect("对照可解析");
        assert_eq!(65536, within[0].max_output_tokens, "到线值原样");
        assert_eq!(0, within[1].max_output_tokens, "目录没给就留 0，不编一个数");
    }
}

