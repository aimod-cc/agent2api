//! 模型管理规则（config.json 的 `modelRules` 字段）：禁用 / 隐藏 / 映射。
//!
//! 形状：
//! ```json
//! "modelRules": {
//!   "disabled": [{ "provider": "catpaw", "id": "kimi-k3" }],
//!   "hidden":   [{ "provider": "raccoon", "id": "sn-glm-5-3" }],
//!   "mappings": [{ "alias": "gpt-4o", "target": "deepseek-v4-pro" }],
//!   "seeded":   ["workbuddy:hy3"]
//! }
//! ```
//! - **禁用**：模型在管理页里仍可见（开关关着），但不出现在 `/v1/models`，
//!   请求它返回 404 model_not_found；
//! - **隐藏**（管理页的「删除」）：从清单里拿掉，管理页可在「已删除」筛选里恢复；
//!   对外效果与禁用相同；
//! - **映射**：下游用 `alias` 请求时改写成 `target` 再转发；alias 同时出现在
//!   `/v1/models` 里。同一 alias 只能指向一个 target；alias 不得与任何上游模型 id 同名。
//!
//! ── 启停粒度是「提供商 × 模型 id」，不再是全局 id ─────────────
//! 同一个模型 id（或对外名）常被多家同时提供（如 `kimi-k3`：CatPaw 的上游 id
//! 与小浣熊经去前缀映射暴露的对外名同名）。规则按 `(provider, id)` 存放，
//! 关掉某一家只是这一家不再接收该模型的请求，别家照常。
//!
//! ── 历史条目的兼容（不要「顺手迁移」）─────────────────────────
//! 旧版把 disabled / hidden 存成**纯 id 字符串数组**（全局生效）。读取时兼容：
//! 字符串条目解析成 `provider: None`，对**任何提供商**都命中 —— 升级后用户
//! 之前做的全局启停保持原样，直到他在管理页里对某一家重新启停（那时写侧会
//! 按「展开为其余各家」的语义把它替换掉，见 `set_state`）。
//!
//! - **seeded**：已经做过「默认规则种子」的 (provider, id)，写成
//!   `"provider:id"` 字符串（旧版是纯 id，读取时对任何 provider 都算已种 ——
//!   只影响「要不要再种一次默认值」，保守方向是正确的）。
//!   种子只在模型**首次出现**时生效一次：之后用户在管理页的手动调整
//!   （重新启用、删除映射）不会被下一次清单刷新悄悄改回去。
//!
//! 所有比对忽略大小写（与 `catalog::providers_for_model` 同口径）。

use serde_json::{json, Map, Value};

use crate::server::config;

pub const KEY_MODEL_RULES: &str = "modelRules";

/// 一条映射
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mapping {
    pub alias: String,
    pub target: String,
}

/// 一条启停规则的键：`(provider, id)`。
///
/// `provider` 为 `None` 的条目来自旧版配置（纯 id 字符串），语义是
/// 「对所有提供商生效」；新版写侧永远带 provider。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleEntry {
    pub provider: Option<String>,
    pub id: String,
}

impl RuleEntry {
    fn new(provider: Option<&str>, id: &str) -> Self {
        Self { provider: provider.map(str::to_string), id: id.to_string() }
    }

    fn to_value(&self) -> Value {
        match &self.provider {
            Some(provider) => json!({ "provider": provider, "id": self.id }),
            None => Value::String(self.id.clone()),
        }
    }

    /// 条目是否命中 `(provider, id)`：id 同（忽略大小写），且 provider 为
    /// None（全局条目）或与目标一致。
    fn matches(&self, provider: &str, id: &str) -> bool {
        self.id.eq_ignore_ascii_case(id)
            && self.provider.as_deref().map_or(true, |p| p.eq_ignore_ascii_case(provider))
    }
}

/// 解析后的规则快照
#[derive(Clone, Debug, Default)]
pub struct ModelRules {
    pub disabled: Vec<RuleEntry>,
    pub hidden: Vec<RuleEntry>,
    pub mappings: Vec<Mapping>,
    pub seeded: Vec<String>,
}

/// 从规则数组的 JSON 形态还原条目列表（兼容旧版纯 id 字符串）
fn entries_from(value: Option<&Value>) -> Vec<RuleEntry> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| match item {
                    // 旧形态：纯 id 字符串 = 全局规则
                    Value::String(id) => {
                        let id = id.trim();
                        if id.is_empty() {
                            None
                        } else {
                            Some(RuleEntry::new(None, id))
                        }
                    }
                    // 新形态：{provider, id}
                    Value::Object(object) => {
                        let id = object.get("id").and_then(Value::as_str).map(str::trim).unwrap_or("");
                        if id.is_empty() {
                            return None;
                        }
                        let provider = object
                            .get("provider")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|provider| !provider.is_empty());
                        Some(RuleEntry::new(provider, id))
                    }
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

impl ModelRules {
    pub fn from_raw(raw: &Map<String, Value>) -> Self {
        let Some(object) = raw.get(KEY_MODEL_RULES).and_then(Value::as_object) else {
            return Self::default();
        };
        let mappings = object
            .get("mappings")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        let alias = item.get("alias")?.as_str()?.trim();
                        let target = item.get("target")?.as_str()?.trim();
                        if alias.is_empty() || target.is_empty() {
                            return None;
                        }
                        Some(Mapping {
                            alias: alias.to_string(),
                            target: target.to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            disabled: entries_from(object.get("disabled")),
            hidden: entries_from(object.get("hidden")),
            mappings,
            seeded: object
                .get("seeded")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    fn to_value(&self) -> Value {
        json!({
            "disabled": self.disabled.iter().map(RuleEntry::to_value).collect::<Vec<_>>(),
            "hidden": self.hidden.iter().map(RuleEntry::to_value).collect::<Vec<_>>(),
            "mappings": self.mappings.iter().map(|m| json!({"alias": m.alias, "target": m.target})).collect::<Vec<_>>(),
            "seeded": self.seeded,
        })
    }

    /// 单条列表（disabled / hidden）上「按 id 取条目」的匹配判定
    fn hit(list: &[RuleEntry], provider: &str, id: &str) -> bool {
        list.iter().any(|entry| entry.matches(provider, id))
    }

    pub fn is_disabled(&self, provider: &str, id: &str) -> bool {
        Self::hit(&self.disabled, provider, id)
    }

    pub fn is_hidden(&self, provider: &str, id: &str) -> bool {
        Self::hit(&self.hidden, provider, id)
    }

    /// 禁用或隐藏 → 该提供商对该模型对外不可用
    pub fn is_blocked(&self, provider: &str, id: &str) -> bool {
        self.is_disabled(provider, id) || self.is_hidden(provider, id)
    }

    /// alias → target（忽略大小写）
    pub fn resolve_alias(&self, model: &str) -> Option<&str> {
        self.mappings
            .iter()
            .find(|m| m.alias.eq_ignore_ascii_case(model))
            .map(|m| m.target.as_str())
    }

    /// 指向某个上游模型的全部 alias
    pub fn aliases_of(&self, target: &str) -> Vec<&str> {
        self.mappings
            .iter()
            .filter(|m| m.target.eq_ignore_ascii_case(target))
            .map(|m| m.alias.as_str())
            .collect()
    }

    /// 该 (provider, id) 是否已做过默认规则种子。
    ///
    /// ── 旧版纯 id 条目的兼容**只对 workbuddy / raccoon 成立**（别放宽）──
    /// 升级前的 `seeded` 存的是纯 id（旧版种子是全局动作），读取时对这两家
    /// 保留「纯 id 也算已种」的兼容：重种一遍只是把同样的默认值再写一次，
    /// 没必要。
    ///
    /// 但这个兼容**不能给所有 provider 开**：Qoder 的目录里有 `Auto` /
    /// `GLM-5.3` / `DeepSeek-V4-Pro` 这类与 workbuddy / raccoon 清单**同名**的
    /// 模型，它们的纯 id 早已躺在旧版 `seeded` 里 —— 一律算已种的话，Qoder
    /// 这几个模型会**跳过白名单种子**而在全新安装上默认启用，与「只默认开
    /// Qwen3.8-Flash」的预期正好相反。旧版种子从未处理过 Qoder，
    /// 那批标记对 Qoder 不构成「已种」的证据。
    fn is_seeded(&self, provider: &str, id: &str) -> bool {
        let key = format!("{provider}:{id}");
        if self.seeded.iter().any(|item| item.eq_ignore_ascii_case(&key)) {
            return true;
        }
        if !matches!(provider, "workbuddy" | "raccoon") {
            return false;
        }
        self.seeded.iter().any(|item| item.eq_ignore_ascii_case(id))
    }
}

/// 当前生效的规则
pub fn current() -> ModelRules {
    ModelRules::from_raw(config::current().raw())
}

fn save(rules: &ModelRules) -> bool {
    config::update_raw_field(KEY_MODEL_RULES, rules.to_value())
}

/// 把 `(provider, id)` 的**启用**落到列表上。
///
/// 启用某一家时，直接移除该家的条目就够；但如果存在旧版的**全局条目**
/// （provider=None，对所有提供商生效），只移除它会把其他家也一并放开 ——
/// 那不是用户的意图。此时把全局条目替换为「其余当前也提供该模型的家」的
/// 精确条目：它们的禁用状态原样保留，目标家则恢复可用。其余各家取自
/// `other_providers`（调用方从当前清单里取，见 `api::model_manage`）。
fn enable_on(list: &mut Vec<RuleEntry>, provider: &str, id: &str, other_providers: &[String]) {
    let had_global = list
        .iter()
        .any(|entry| entry.provider.is_none() && entry.id.eq_ignore_ascii_case(id));
    list.retain(|entry| !entry.matches(provider, id));
    if had_global {
        // 全局条目展开：其他家逐家补条目（已有的保持原样，不重复加）
        for other in other_providers {
            if other.eq_ignore_ascii_case(provider) {
                continue;
            }
            if !ModelRules::hit(list, other, id) {
                list.push(RuleEntry::new(Some(other), id));
            }
        }
    }
}

/// 设置某模型的启用 / 隐藏状态（`None` = 该项不动）。
///
/// `provider` 是规则的目标提供商；`None` 走**旧版全局语义**（只有旧前端会
/// 这么传），enabled=false 等价于「对所有提供商禁用」，enabled=true 等价于
/// 「清掉该 id 的全部条目」—— 与升级前行为完全一致。
///
/// `other_providers`：当前清单里同样提供该模型的其他提供商（启用分支展开
/// 全局条目时用）；调用方从 catalog 取，这里不回头依赖目录模块。
pub fn set_state(
    provider: Option<&str>,
    id: &str,
    enabled: Option<bool>,
    hidden: Option<bool>,
    other_providers: &[String],
) -> ModelRules {
    let mut rules = current();
    if let Some(enabled) = enabled {
        if enabled {
            enable_on(&mut rules.disabled, provider.unwrap_or(""), id, other_providers);
        } else {
            set_membership(&mut rules.disabled, provider, id, true);
        }
    }
    if let Some(hidden) = hidden {
        if !hidden {
            enable_on(&mut rules.hidden, provider.unwrap_or(""), id, other_providers);
        } else {
            set_membership(&mut rules.hidden, provider, id, true);
        }
    }
    save(&rules);
    rules
}

/// 把 `(provider, id)` 的**禁用 / 隐藏**落到列表上（幂等；provider=None = 全局）
fn set_membership(list: &mut Vec<RuleEntry>, provider: Option<&str>, id: &str, present: bool) {
    list.retain(|entry| !(entry.provider == provider.map(str::to_string) && entry.id.eq_ignore_ascii_case(id)));
    if present {
        list.push(RuleEntry::new(provider, id));
    }
}

/// alias 允许的字符：字母 / 数字 / `- _ . / :`
pub fn alias_valid(alias: &str) -> bool {
    !alias.is_empty()
        && alias.len() <= 128
        && alias
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':'))
}

/// 新增映射；同名 alias 已存在时替换其 target
pub fn add_mapping(alias: &str, target: &str) -> ModelRules {
    let mut rules = current();
    rules.mappings.retain(|m| !m.alias.eq_ignore_ascii_case(alias));
    rules.mappings.push(Mapping {
        alias: alias.to_string(),
        target: target.to_string(),
    });
    save(&rules);
    rules
}

/// 删除映射；返回是否存在
pub fn remove_mapping(alias: &str) -> (ModelRules, bool) {
    let mut rules = current();
    let before = rules.mappings.len();
    rules.mappings.retain(|m| !m.alias.eq_ignore_ascii_case(alias));
    let removed = rules.mappings.len() != before;
    if removed {
        save(&rules);
    }
    (rules, removed)
}

// ─── 小浣熊清单的默认规则种子 ────────────────────

/// 小浣熊的「内部模型」id：`raccoon-` 后跟一段**纯十六进制**短哈希
/// （`raccoon-8c4485` / `raccoon-19b265` / `raccoon-405a1c` 这种 Work 模型的
/// 内部代号）。`raccoon-chat-ml-5-5` 这类正经命名不会命中。
fn is_opaque_raccoon_id(id: &str) -> bool {
    match id.strip_prefix("raccoon-") {
        Some(rest) => !rest.is_empty()
            && rest.len() <= 8
            && rest.chars().all(|c| c.is_ascii_hexdigit()),
        None => false,
    }
}

/// 小浣熊清单的**默认规则种子**：对 `ids` 里每个还没种过的模型做一次默认处理 ——
///
///   - `raccoon-<hex>` 内部模型 → 默认**禁用**（对外不可见；管理页里仍可看到并手动启用）；
///   - `sn-` 前缀的模型 → 默认加一条**去前缀映射**（`sn-glm-5-3` → `glm-5-3`），
///     客户端用两种名字都行；去前缀后的名字若已被映射占用、或与小浣熊自己的
///     另一个上游模型 id 撞名，则跳过（照常记入 seeded，不再反复尝试）。
///
/// 处理过的 (provider, id) 记入 `seeded`（持久化在 modelRules 里）：之后用户手动启用某个
/// 内部模型、或删掉某条自动映射，清单刷新都不会把它们改回去；上游将来新增的
/// `sn-` 模型因为 id 没种过，会在下一次种子时自动获得映射。
///
/// 只处理小浣熊的清单；调用点在清单落地 / 刷新编排处（见 raccoon::models）。
/// 返回给日志的摘要；没有任何新模型时返回 None（不落盘）。
pub fn seed_raccoon_defaults(ids: &[String]) -> Option<String> {
    let mut rules = current();
    let mut disabled_added: Vec<String> = Vec::new();
    let mut mappings_added: Vec<String> = Vec::new();
    for id in ids {
        let id = id.trim();
        if id.is_empty() || rules.is_seeded("raccoon", id) {
            continue;
        }
        rules.seeded.push(format!("raccoon:{id}"));
        if is_opaque_raccoon_id(id) {
            // 已在 disabled 里就不重复计数（set_membership 本身幂等）
            if !rules.is_disabled("raccoon", id) {
                set_membership(&mut rules.disabled, Some("raccoon"), id, true);
                disabled_added.push(id.to_string());
            }
        } else if let Some(alias) = id.strip_prefix("sn-") {
            if alias.is_empty() || !alias_valid(alias) {
                continue;
            }
            // alias 已有映射（不管是自动还是人工）或与上游 id 撞名 → 不动
            if rules.resolve_alias(alias).is_some()
                || ids.iter().any(|other| other.eq_ignore_ascii_case(alias))
            {
                continue;
            }
            rules.mappings.retain(|m| !m.alias.eq_ignore_ascii_case(alias));
            rules.mappings.push(Mapping {
                alias: alias.to_string(),
                target: id.to_string(),
            });
            mappings_added.push(format!("{alias} → {id}"));
        }
    }
    if disabled_added.is_empty() && mappings_added.is_empty() {
        return None;
    }
    save(&rules);
    let mut parts: Vec<String> = Vec::new();
    if !disabled_added.is_empty() {
        parts.push(format!("默认禁用内部模型 [{}]", disabled_added.join(", ")));
    }
    if !mappings_added.is_empty() {
        parts.push(format!("默认新增映射 [{}]", mappings_added.join(", ")));
    }
    Some(format!("🧩 小浣熊模型默认规则: {}", parts.join("；")))
}

// ─── 各清单的「默认启用白名单」种子 ────────────────────

/// WorkBuddy 清单的**默认启用白名单**：模型首次出现在清单里时，只有这里的
/// 模型保持默认启用，其余一律默认**禁用**（管理页可见、开关关着，用户可手动
/// 启用 —— 与小浣熊种子同一哲学：「默认值」只决定初始状态，不决定 forever）。
pub const WORKBUDDY_DEFAULT_ENABLED: &[&str] = &["hy3", "hy4-preview-f", "deepseek-v4.1-flash"];

/// Qoder 清单的**默认启用白名单**：同 WorkBuddy 种子的语义，只是白名单里
/// 只留一个模型。
///
/// ── 为什么 Qoder 只留 Qwen3.8-Flash ────────────────────────
/// Qoder 是 agent 形态的上游，目录里绝大多数条目要么是**套餐档位别名**
/// （`Auto` / `Ultimate` / `Performance` / `Efficient` / `Sonus` / `Cantus`），
/// 要么是需要更高档套餐才可用的模型。默认全开会让这台网关对外的模型列表
/// 凭空多出十几个用户几乎不会点名的名字，而且它们还会参与 `/v1/models` 的
/// 同名认领 —— 把别家真正在用的同名模型（如 `GLM-5.3` / `Kimi-K3`）挤掉。
/// `Qwen3.8-Flash` 是免费档通常可用的那个（静态兜底清单里 `enabled` 恒为真
/// 的两个 Qwen3.8 系模型之一，且面向日常对话），拿它当唯一默认项最贴近
/// 「装上就能用」的预期。
pub const QODER_DEFAULT_ENABLED: &[&str] = &["Qwen3.8-Flash"];

/// WorkBuddy 清单的**默认规则种子**：对 `ids` 里每个还没种过的模型记入
/// `seeded`，不在 [`WORKBUDDY_DEFAULT_ENABLED`] 里的同时默认禁用。
///
/// 调用点有两类：清单**首次落地**（`core::models` 的 `apply_remote`，覆盖
/// /v3/config 与企业清单两条路径），以及编排入口对**当前缓存清单**的补种
/// （见 `providers::adapter` 的 `seed_current_workbuddy_defaults` —— 覆盖启动时
/// 只有内置清单、或刷新失败停留在旧清单的情形）。幂等：种过的 id 不再动，
/// 用户事后在管理页的手动启用 / 禁用不会被清单刷新改回去。
///
/// 返回给日志的摘要；没有新种过的模型时返回 None（不落盘）。
pub fn seed_workbuddy_defaults(ids: &[String]) -> Option<String> {
    seed_default_enabled("workbuddy", "WorkBuddy", WORKBUDDY_DEFAULT_ENABLED, ids)
}

/// Qoder 清单的**默认规则种子**：语义与 [`seed_workbuddy_defaults`] 完全一致，
/// 只是白名单是 [`QODER_DEFAULT_ENABLED`]。
///
/// 调用点同样两类：清单**首次落地**（`providers::qoder::models::refresh`，远程
/// 目录从上游拉回来时），以及编排入口对**当前缓存清单**的补种（见
/// `providers::adapter` 的 `seed_current_qoder_defaults` —— 覆盖「有账号但远程
/// 刷新失败，手里只有静态兜底清单」与升级用户首次打开管理页的情形）。
pub fn seed_qoder_defaults(ids: &[String]) -> Option<String> {
    seed_default_enabled("qoder", "Qoder", QODER_DEFAULT_ENABLED, ids)
}

/// 「默认启用白名单」种子的公共实现（WorkBuddy 与 Qoder 共用）。
///
/// 对 `ids` 里每个还没种过的模型：不在 `whitelist` 里的默认禁用，并把
/// `(provider, id)` 记入 `seeded`。`label` 只进日志文案。
///
/// ── 为什么 seeded 单独变化也要落盘（而不是只看「有没有新禁用」）──────
/// 旧实现只在「这次真禁用了某个模型」时才 save，于是「模型已被别处的规则
/// 禁用 → 本次没有新禁用 → seeded 没落地」这个组合下，下次启动会**重种一遍** ——
/// 用户在这期间手动启用过它的话，会被这一次重种悄悄改回禁用。种子是
/// 「只对首次出现生效」的承诺，那承诺必须落盘才算数。
fn seed_default_enabled(
    provider: &str,
    label: &str,
    whitelist: &[&str],
    ids: &[String],
) -> Option<String> {
    let mut rules = current();
    let seeded_before = rules.seeded.len();
    let mut disabled_added: Vec<String> = Vec::new();
    for id in ids {
        let id = id.trim();
        if id.is_empty() || rules.is_seeded(provider, id) {
            continue;
        }
        rules.seeded.push(format!("{provider}:{id}"));
        let default_enabled = whitelist.iter().any(|name| name.eq_ignore_ascii_case(id));
        if !default_enabled && !rules.is_disabled(provider, id) {
            set_membership(&mut rules.disabled, Some(provider), id, true);
            disabled_added.push(id.to_string());
        }
    }
    if rules.seeded.len() == seeded_before {
        // 没有新模型：一个字节都不用落盘
        return None;
    }
    save(&rules);
    if disabled_added.is_empty() {
        // 有新的种子标记、但都被别处的规则禁着了 —— 落盘即可，日志不必吵
        return None;
    }
    Some(format!(
        "🧩 {label}模型默认规则: 默认只启用 [{}]；默认禁用 {} 个 [{}]",
        whitelist.join(", "),
        disabled_added.len(),
        disabled_added.join(", ")
    ))
}
