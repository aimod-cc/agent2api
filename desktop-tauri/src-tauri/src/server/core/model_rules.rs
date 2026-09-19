//! 模型管理规则（config.json 的 `modelRules` 字段）：禁用 / 隐藏 / 映射。
//!
//! 形状：
//! ```json
//! "modelRules": {
//!   "disabled": ["deepseek-v3-2-volc"],
//!   "hidden":   ["sn-glm-5-3"],
//!   "mappings": [{ "alias": "gpt-4o", "target": "deepseek-v4-pro" }]
//! }
//! ```
//! - **禁用**：模型仍在管理页里可见（开关关着），但不出现在 `/v1/models`，
//!   请求它返回 404 model_not_found；
//! - **隐藏**（管理页的「删除」）：从清单里拿掉，管理页可在「已删除」筛选里恢复；
//!   对外效果与禁用相同；
//! - **映射**：下游用 `alias` 请求时改写成 `target` 再转发；alias 同时出现在
//!   `/v1/models` 里。同一 alias 只能指向一个 target；alias 不得与任何上游模型 id 同名。
//!
//! - **seeded**：已经做过「默认规则种子」的模型 id（小浣熊清单见
//!   `seed_raccoon_defaults`，WorkBuddy 清单见 `seed_workbuddy_defaults`）。
//!   种子只在模型**首次出现**时生效一次：
//!   之后用户在管理页的手动调整（重新启用、删除映射）不会被下一次清单刷新
//!   悄悄改回去 —— 「默认值」只该决定初始状态，不该决定 forever。
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

/// 解析后的规则快照
#[derive(Clone, Debug, Default)]
pub struct ModelRules {
    pub disabled: Vec<String>,
    pub hidden: Vec<String>,
    pub mappings: Vec<Mapping>,
    pub seeded: Vec<String>,
}

impl ModelRules {
    pub fn from_raw(raw: &Map<String, Value>) -> Self {
        let Some(object) = raw.get(KEY_MODEL_RULES).and_then(Value::as_object) else {
            return Self::default();
        };
        let list = |key: &str| -> Vec<String> {
            object
                .get(key)
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
                .unwrap_or_default()
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
            disabled: list("disabled"),
            hidden: list("hidden"),
            mappings,
            seeded: list("seeded"),
        }
    }

    fn to_value(&self) -> Value {
        json!({
            "disabled": self.disabled,
            "hidden": self.hidden,
            "mappings": self.mappings.iter().map(|m| json!({"alias": m.alias, "target": m.target})).collect::<Vec<_>>(),
            "seeded": self.seeded,
        })
    }

    pub fn is_disabled(&self, id: &str) -> bool {
        self.disabled.iter().any(|item| item.eq_ignore_ascii_case(id))
    }

    pub fn is_hidden(&self, id: &str) -> bool {
        self.hidden.iter().any(|item| item.eq_ignore_ascii_case(id))
    }

    /// 禁用或隐藏 → 对外不可用
    pub fn is_blocked(&self, id: &str) -> bool {
        self.is_disabled(id) || self.is_hidden(id)
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
}

/// 当前生效的规则
pub fn current() -> ModelRules {
    ModelRules::from_raw(config::current().raw())
}

fn save(rules: &ModelRules) -> bool {
    config::update_raw_field(KEY_MODEL_RULES, rules.to_value())
}

fn set_membership(list: &mut Vec<String>, id: &str, present: bool) {
    list.retain(|item| !item.eq_ignore_ascii_case(id));
    if present {
        list.push(id.to_string());
    }
}

/// 设置某模型的启用 / 隐藏状态（`None` = 该项不动）
pub fn set_state(id: &str, enabled: Option<bool>, hidden: Option<bool>) -> ModelRules {
    let mut rules = current();
    if let Some(enabled) = enabled {
        set_membership(&mut rules.disabled, id, !enabled);
    }
    if let Some(hidden) = hidden {
        set_membership(&mut rules.hidden, id, hidden);
    }
    save(&rules);
    rules
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
/// 处理过的 id 记入 `seeded`（持久化在 modelRules 里）：之后用户手动启用某个
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
        if id.is_empty()
            || rules.seeded.iter().any(|seeded| seeded.eq_ignore_ascii_case(id))
        {
            continue;
        }
        rules.seeded.push(id.to_string());
        if is_opaque_raccoon_id(id) {
            // 已在 disabled 里就不重复计数（set_membership 本身幂等）
            if !rules.is_disabled(id) {
                set_membership(&mut rules.disabled, id, true);
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

// ─── WorkBuddy 清单的默认规则种子 ────────────────────

/// WorkBuddy 清单的**默认启用白名单**：模型首次出现在清单里时，只有这里的
/// 模型保持默认启用，其余一律默认**禁用**（管理页可见、开关关着，用户可手动
/// 启用 —— 与小浣熊种子同一哲学：「默认值」只决定初始状态，不决定 forever）。
pub const WORKBUDDY_DEFAULT_ENABLED: &[&str] = &["hy3", "hy4-preview-f", "deepseek-v4.1-flash"];

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
    let mut rules = current();
    let mut disabled_added: Vec<String> = Vec::new();
    for id in ids {
        let id = id.trim();
        if id.is_empty()
            || rules.seeded.iter().any(|seeded| seeded.eq_ignore_ascii_case(id))
        {
            continue;
        }
        rules.seeded.push(id.to_string());
        let default_enabled = WORKBUDDY_DEFAULT_ENABLED
            .iter()
            .any(|name| name.eq_ignore_ascii_case(id));
        if !default_enabled && !rules.is_disabled(id) {
            set_membership(&mut rules.disabled, id, true);
            disabled_added.push(id.to_string());
        }
    }
    if disabled_added.is_empty() {
        return None;
    }
    save(&rules);
    Some(format!(
        "🧩 WorkBuddy模型默认规则: 默认只启用 [{}]；默认禁用 {} 个 [{}]",
        WORKBUDDY_DEFAULT_ENABLED.join(", "),
        disabled_added.len(),
        disabled_added.join(", ")
    ))
}
