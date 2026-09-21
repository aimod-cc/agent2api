//! 模型管理规则（config.json 的 `modelRules` 字段）：禁用 / 隐藏 / 映射 / 自定义模型。
//!
//! 形状：
//! ```json
//! "modelRules": {
//!   "disabled": [{ "provider": "catpaw", "id": "kimi-k3" }],
//!   "hidden":   [{ "provider": "raccoon", "id": "sn-glm-5-3" }],
//!   "mappings": [{ "alias": "gpt-4o", "target": "deepseek-v4-pro", "provider": "raccoon",
//!                  "reasoning": "high" }],
//!   "custom":   [{ "provider": "catpaw", "id": "kimi-k3-preview" }],
//!   "seeded":   ["workbuddy:hy3"]
//! }
//! ```
//! - **禁用**：模型在管理页里仍可见（开关关着），但不出现在 `/v1/models`，
//!   请求它返回 404 model_not_found，**且不再参与转发路由** ——
//!   `providers::router::route_for_forward` 会把这一家从候选链里剔除，
//!   请求点名它、或被某条映射指向它（按该条目的 target 判）时都不会落到这里；
//! - **隐藏**（管理页的「删除」）：从清单里拿掉，管理页可在「已删除」筛选里恢复；
//!   对外效果与禁用相同（同样从转发候选链里剔除）；
//! - **映射**（照抄 OmniProxy 的模型映射语义）：把「上游模型（提供商 × 真名）」
//!   以对外名 `alias` 暴露给下游。`alias` **自由命名**——允许与任何上游模型 id
//!   同名（同名时该上游的原生路由仍在、且优先，映射是追加的兜底路，不存在遮蔽）；
//!   **同一 alias 可以有多条映射**（不同提供商各一条）——下游用同一个名字请求，
//!   路由在「原生承载家 + 各映射提供商」之间按账号全局优先级主备切换。
//!   候选链的展开与发送名的按家改写见 `providers::router` 与
//!   `catalog::wire_target_for_provider`。
//!
//! ── 映射条目的 provider 字段 ────────────────────────────────
//! 新条目都带 `provider`（target 所属的家，UI 从该家的模型行上创建）。
//! 旧版条目（升级前）没有 provider：语义是「alias 改写成 target，由所有承载
//! target 的家接收」——路由扩池时对 provider 缺失的条目取 `providers_for_model(target)`，
//! 发送名同样改成 target，行为与升级前逐字等价（旧校验保证 alias 不与上游 id
//! 同名，所以「原生承载 alias 的家」为空，不存在语义分叉）。
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
//! ── 自定义模型（`custom`）────────────────────────────────────
//! 用户在管理页手动登记「这家还有这个上游模型」。它解决的是一个具体的死角：
//! 上游目录接口没广告、但实际能路由的模型（灰度中的新模型、按账号下发但没进
//! 目录的模型），此前网关既列不出、也调不通。
//!
//! 这些条目**不是**禁用 / 隐藏那种「规则」——它们是**清单的补充来源**：
//! `providers::catalog::manifest_for` 把该家的自定义条目拼在自己的清单后面，
//! 于是能力判定、路由候选链、`/v1/models`、管理页、入口校验**一次全通**。
//! 因此「删除自定义模型」是从本数组里**移除**，而不是打 `hidden` 标记：
//! 打标记会让它留在清单里、只是不接收请求，恢复后又会回来 —— 而用户要的是
//! 「这个模型我登记错了，删掉」，语义上不存在「恢复」这一步。
//!
//! 这里刻意**不存** `maxOutputTokens` / `supportsImages` 之类的能力位：
//! 用户无从知道这些值，而编一个默认值会让 `/v1/models` 对下游**撒谎**
//! （例如声明支持图片实际不支持）。缺字段的后果只是 `list_item` 里少几个
//! 元数据键（与图像模型同款行为），下游据此保守处理 —— 比给错值好。
//!
//! 所有比对忽略大小写（与 `catalog::providers_for_model` 同口径）。
//!
//! ── 思考等级绑定（`mappings[].reasoning`）─────────────────────
//! 每条映射可以额外带一个思考等级（None / 空 = 不覆盖）。**这段机制、各家的
//! 翻译规则、以及「哪些情况故意不注入」都在 `reasoning.rs`** —— 那里有候选表、
//! 归一规则，以及「等级怎么进到转发链路」的完整说明（动手改这个字段前务必先读
//! 那一段）。这里只留结论：字段挂在映射条目上（与本模块的映射同生共死），
//! `ModelRules::from_raw` / `Mapping::to_value` 负责它的读写容错；
//! **转发侧**在 `upstream::payload::send_body` 按家改写模型名的那一步一并解析、
//! 交给该家适配器翻译（见 `providers::catalog::wire_target_for_provider`）。

use serde_json::{json, Map, Value};

use crate::server::config;

pub const KEY_MODEL_RULES: &str = "modelRules";

/// Cline 专用的规则件：默认映射种子 + 拆分迁移 + 池前缀工具。
///
/// 拆出去的理由见那个文件的模块头（一次性迁移与长期机制不该混在一起，
/// 以及单文件体量约定）。`pub use` 让调用方仍写 `model_rules::seed_cline_defaults`
/// 这类路径，不必知道它住在子模块里。
mod cline;

pub use cline::{migrate_cline_split, seed_cline_defaults};

/// 思考等级组件（候选表 + 归一规则 + 设计取舍说明）。
///
/// 与 `cline` 同一模式：拆出去的理由见那个文件的模块头（长期机制与
/// 「这一段为什么这么做」的大段说明不该与规则机制本身挤在一个文件里，
/// 以及单文件体量约定）。`pub use` 让调用方仍写
/// `model_rules::REASONING_LEVELS` / `model_rules::normalize_reasoning`
/// 这类路径，不必知道它住在子模块里。
mod reasoning;

pub use reasoning::{
    effort_rank as reasoning_rank, is_thinking_off as reasoning_is_off,
    normalize as normalize_reasoning, REASONING_LEVELS,
};

/// 一条映射：把上游模型（`provider` × `target`）以对外名 `alias` 暴露给下游。
///
/// `provider` 指明 target 所属的家（新条目总是带；旧版条目为 `None`，
/// 语义是「所有承载 target 的家」，读取兼容见模块头）。同一 `alias` 允许
/// 多条（不同提供商各一条），路由时一起进入候选链主备切换。
///
/// `reasoning` 是这条映射上的**思考等级绑定**（`None` = 不覆盖）。
/// 转发侧在按家改写模型名的同一步解析它（`catalog::wire_target_for_provider`），
/// 交给该家适配器翻译成本家上游认识的档位字段 —— 各家的规则与「故意不注入」
/// 的几种情形见 `reasoning.rs` 的模块头。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mapping {
    pub alias: String,
    pub target: String,
    pub provider: Option<String>,
    pub reasoning: Option<String>,
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

/// 一条**自定义模型**：用户手动登记「这家还有这个上游模型」。
///
/// 与 [`RuleEntry`] 的区别值得强调：`RuleEntry` 是**规则**（对清单里已有的
/// 条目做启停），本结构是**清单的补充**（往清单里加一个原本没有的条目）。
/// 两者的 `provider` 都是必填 —— 规则那边的 None 是旧版全局语义的兼容形态，
/// 这里没有历史包袱，缺 provider 的条目在读取时直接丢弃（见 `from_raw`）。
///
/// 字段只有两个：模型 id 与它所属的家。**展示名不单独存** —— 用户填的就是
/// 上游 id，再让他编一个显示名只会多一处要维护的事实；清单里的 `name` 由
/// [`custom_models_for`] 用 id 顶替（下游看到的名称与请求名一致，不会混淆）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CustomModel {
    pub provider: String,
    pub id: String,
}

impl CustomModel {
    fn to_value(&self) -> Value {
        json!({ "provider": self.provider, "id": self.id })
    }

    /// 是否命中 `(provider, id)`（都忽略大小写）。
    ///
    /// `pub`：管理页判「来源 = 手动」时要在 `catalog` 侧按同一口径比对
    /// （见 `catalog::manage_entries`）—— 在那里另写一遍忽略大小写的比较，
    /// 迟早会与这里的口径分叉。
    pub fn matches(&self, provider: &str, id: &str) -> bool {
        self.provider.eq_ignore_ascii_case(provider) && self.id.eq_ignore_ascii_case(id)
    }
}

impl Mapping {
    /// 落盘的 JSON 形态。
    ///
    /// `reasoning` 为 `None` 时**仍然写出 `null`**，与 `provider` 的写法同一
    /// 口径（`json!` 对 `Option::None` 给 `null`）：形状稳定比省几个字节重要，
    /// 且读取侧的容错本来就认 `null`（`as_str` 拿到 None）。反过来**不**在
    /// `None` 时省略这个键 —— 那样配置里同一个字段时有时无，排查时更难读。
    fn to_value(&self) -> Value {
        json!({
            "alias": self.alias,
            "target": self.target,
            "provider": self.provider,
            "reasoning": self.reasoning,
        })
    }
}

/// 解析后的规则快照
#[derive(Clone, Debug, Default)]
pub struct ModelRules {
    pub disabled: Vec<RuleEntry>,
    pub hidden: Vec<RuleEntry>,
    pub mappings: Vec<Mapping>,
    /// 用户手动登记的上游模型（清单的补充来源，见模块头）
    pub custom: Vec<CustomModel>,
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

/// 从 JSON 数组还原自定义模型列表。
///
/// 容错口径比 `entries_from` **更严**：那里字符串条目有「旧版全局规则」的
/// 合法含义，这里的 `provider` 是**必填**（没有 provider 就不知道该把条目
/// 拼进哪一家的清单，等于一条永远不生效的死数据）。因此缺 provider / 缺 id /
/// 非对象形态的条目一律丢弃 —— 静默留一条不生效的记录，比丢掉它更难排查。
fn custom_from(value: Option<&Value>) -> Vec<CustomModel> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let object = item.as_object()?;
                    let id = object
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .unwrap_or("");
                    let provider = object
                        .get("provider")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .unwrap_or("");
                    if id.is_empty() || provider.is_empty() {
                        return None;
                    }
                    Some(CustomModel {
                        provider: provider.to_string(),
                        id: id.to_string(),
                    })
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
                        let provider = item
                            .get("provider")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|provider| !provider.is_empty());
                        // 思考等级：**缺失 / 非字符串 / 空串一律 None（不覆盖）**。
                        // 旧数据（升级前写下的映射）没有这个键，这条容错就是
                        // 「升级不能让已有映射变样」的实现方式：读出来与写回去
                        // 都不带 `reasoning` 的条目，行为与升级前逐字相同。
                        let reasoning = item
                            .get("reasoning")
                            .and_then(Value::as_str)
                            .and_then(normalize_reasoning);
                        Some(Mapping {
                            alias: alias.to_string(),
                            target: target.to_string(),
                            provider: provider.map(str::to_string),
                            reasoning,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            disabled: entries_from(object.get("disabled")),
            hidden: entries_from(object.get("hidden")),
            mappings,
            custom: custom_from(object.get("custom")),
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
            "mappings": self.mappings.iter().map(Mapping::to_value).collect::<Vec<_>>(),
            // `custom` 必须在这里写出：本函数是**整份替换**（`save` → 
            // `config::update_raw_field`），漏掉哪个键，任何一次写规则
            // （启停 / 加映射 / 删映射 / 四个种子）都会把那个键的数据整份抹掉。
            "custom": self.custom.iter().map(CustomModel::to_value).collect::<Vec<_>>(),
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

    /// 某对外名的**全部**映射条目（同名多条 = 多提供商主备；顺序 = 配置顺序）。
    pub fn mappings_of(&self, model: &str) -> Vec<&Mapping> {
        self.mappings
            .iter()
            .filter(|m| m.alias.eq_ignore_ascii_case(model))
            .collect()
    }

    /// 该对外名是否已存在任何映射条目（种子防抢占判断用）。
    pub fn has_alias(&self, alias: &str) -> bool {
        self.mappings.iter().any(|m| m.alias.eq_ignore_ascii_case(alias))
    }

    /// 某条映射的对外名：target 归属 `provider`（None = 旧版全局条目）。
    ///
    /// 管理页按「提供商 × 模型 id」分行渲染：全局条目在所有承载 target 的行
    /// 上都显示（旧行为），带 provider 的条目只显示在自己那家的行上。
    pub fn aliases_of(&self, provider: &str, target: &str) -> Vec<&str> {
        self.mappings
            .iter()
            .filter(|m| m.target.eq_ignore_ascii_case(target))
            .filter(|m| m.provider.as_deref().map_or(true, |p| p.eq_ignore_ascii_case(provider)))
            .map(|m| m.alias.as_str())
            .collect()
    }

    /// 指向某上游模型名的全部对外名（**跨提供商**、去重；忽略大小写比对去重键）。
    ///
    /// `/v1/models` 是一个平面对外目录：同一个对外名无论有几家通过映射提供，
    /// 都只广告一条。
    pub fn aliases_of_any(&self, target: &str) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for m in &self.mappings {
            if m.target.eq_ignore_ascii_case(target)
                && !out.iter().any(|a| m.alias.eq_ignore_ascii_case(*a))
            {
                out.push(m.alias.as_str());
            }
        }
        out
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

/// 新增映射（照抄 OmniProxy：追加式，同名 alias 允许多条 —— 不同提供商各建
/// 一条即为主备）。`provider` 是 target 所属的家（新条目总是带；None = 旧版
/// 全局语义，由所有承载 target 的家接收）。
///
/// 完全相同的条目（alias + target + provider 全同）幂等跳过；「alias 与上游 id
/// 同名」「同一 alias 多条」都不再是错误 —— 前者正是同名主备的用法，后者就是
/// 多提供商兜底本身。
///
/// ── 已存在的条目也要能改思考等级（`reasoning`）───────────────
/// 幂等判定的键仍是三元组（不带 `reasoning`）：同一 (alias, target, provider)
/// 上的思考等级是**可编辑的属性**，不是身份的一部分。所以三元组命中时要把
/// 传入的 `reasoning` 写上去（而不是整条跳过）—— 否则管理页上对已有映射改等级
/// 会静默无效，看起来像「保存成功了、刷新又变回去」。
///
/// 代价要说清：这也意味着 `add_mapping` 传了 `reasoning = None` 时会**清掉**
/// 那条已有的绑定。调用方（`api::model_manage::add_mapping`）因此只在请求体里
/// **带有** `reasoning` 键时才传 `Some(...)`/`Some(None)`，完全不带该键时走
/// 「不动等级」的语义 —— 见那个 handler 的取值。
pub fn add_mapping(
    alias: &str,
    target: &str,
    provider: Option<&str>,
    reasoning: Option<Option<&str>>,
) -> ModelRules {
    let mut rules = current();
    let mut touched = false;
    if let Some(existing) = rules.mappings.iter_mut().find(|m| {
        m.alias.eq_ignore_ascii_case(alias)
            && m.target.eq_ignore_ascii_case(target)
            && m.provider.as_deref().map_or(provider.is_none(), |p| {
                provider.map_or(false, |given| p.eq_ignore_ascii_case(given))
            })
    }) {
        if let Some(next) = reasoning {
            // `Some(None)` = 显式清空；`Some(Some(x))` = 设成 x
            let next = next.and_then(normalize_reasoning);
            if existing.reasoning != next {
                existing.reasoning = next;
                touched = true;
            }
        }
    } else {
        rules.mappings.push(Mapping {
            alias: alias.to_string(),
            target: target.to_string(),
            provider: provider.map(str::to_string),
            reasoning: reasoning.flatten().and_then(normalize_reasoning),
        });
        touched = true;
    }
    if touched {
        save(&rules);
    }
    rules
}

/// 新增一条自定义模型（幂等：同 `(provider, id)` 已存在时不动）。
///
/// 与 `add_mapping` 的幂等口径一致：重复提交同一条不算错误、也不产生第二条 ——
/// 管理页的保存键被连点两次是最常见的来源。
pub fn add_custom(provider: &str, id: &str) -> ModelRules {
    let mut rules = current();
    let exists = rules.custom.iter().any(|item| item.matches(provider, id));
    if !exists {
        rules.custom.push(CustomModel {
            provider: provider.to_string(),
            id: id.to_string(),
        });
        save(&rules);
    }
    rules
}

/// 删除一条自定义模型；返回 `(规则快照, 是否真的删掉了)`。
///
/// ── 为什么顺带清理针对它的规则 ──────────────────────────────
/// 自定义条目一旦移除，它就彻底不在清单里了（不像隐藏那样还能恢复），
/// 此时 `disabled` / `hidden` 里针对它的条目、以及指向它的映射，都成了
/// 永远挂不上任何一行的孤儿：管理页的「已删除」筛选里会常驻一条点不开的记录，
/// 「未挂载的映射」里会多一条永远解释不清的条目。所以在这里一并清掉。
///
/// 清理是**尽力而为**：`(provider, id)` 精确匹配的那几条删掉，旧版全局条目
/// （provider 为 None）不动 —— 它可能同时服务别家，删它会误伤（与 `enable_on`
/// 的取舍一致：展开与清理都只碰能精确定位的那部分）。
pub fn remove_custom(provider: &str, id: &str) -> (ModelRules, bool) {
    let mut rules = current();
    let before = rules.custom.len();
    rules.custom.retain(|item| !item.matches(provider, id));
    let removed = rules.custom.len() != before;
    if removed {
        // 精确匹配的启停规则
        rules
            .disabled
            .retain(|entry| !entry.matches(provider, id));
        rules.hidden.retain(|entry| !entry.matches(provider, id));
        // 指向这个模型的映射（带 provider 的那种才算得准）
        rules.mappings.retain(|mapping| {
            !(mapping.target.eq_ignore_ascii_case(id)
                && mapping
                    .provider
                    .as_deref()
                    .is_some_and(|owner| owner.eq_ignore_ascii_case(provider)))
        });
        save(&rules);
    }
    (rules, removed)
}

/// 该 `(provider, id)` 是否是用户手动登记的自定义模型。
///
/// 消费方：管理页判「来源 = 手动」（`catalog::manage_entries`）、以及 AutoClaw
/// 的模型路由解析（未知名字不静默回落，见 `autoclaw::models::resolve_model_route`）。
pub fn is_custom(provider: &str, id: &str) -> bool {
    current().custom.iter().any(|item| item.matches(provider, id))
}

/// 某一家当前登记的自定义模型，转成**聚合层认的条目形态**。
///
/// ── 形状（为什么只有这几个键）───────────────────────────────
/// `id` / `name` 都用用户填的那个 id：下游看到的名称与请求名一致，不会出现
/// 「列表里叫 A、请求要用 B」的困惑。**不编造能力位**（`maxOutputTokens` 等）——
/// 用户无从知道真实值，给一个默认值等于让 `/v1/models` 对下游撒谎。
/// `list_item` 对缺失键的行为是「整键省略」（与图像模型同款），下游据此保守
/// 处理，比给错值好。
///
/// `isDefault` 恒 false：默认模型是各家目录自己的概念（当前只有 workbuddy 认），
/// 手动登记的条目不该去争这个位。
///
/// ── 为什么是「调用时现算」而不是缓存 ─────────────────────────
/// 数据源是 `model_rules::current()`（内存快照），条目通常只有几条，
/// 现算一次的成本远低于维护一份会与配置失同步的缓存。
pub fn custom_models_for(provider: &str) -> Vec<Value> {
    current()
        .custom
        .iter()
        .filter(|item| item.provider.eq_ignore_ascii_case(provider))
        .map(|item| {
            json!({
                "id": item.id,
                "name": item.id,
                "isDefault": false,
                "kind": "chat",
            })
        })
        .collect()
}

/// 删除一条映射（按 alias + target + provider 精确定位）；返回是否存在。
///
/// 同名映射允许多条后，按 alias 单独删会有歧义 —— 管理页的每条 chip 都带着
/// 它所在行的提供商信息，删除时一并传入。
///
/// ── 旧版全局条目（provider 缺失）的命中口径与展示一致 ──────────
/// 展示侧（[`ModelRules::aliases_of`]）把全局条目显示在**所有承载 target 的
/// 家**的行上；删除侧必须同口径，否则升级用户盘上的旧条目会「看得见却删不掉」
/// （chip 带着行提供商来删，旧实现只认 provider 也缺失的请求，必然报不存在）。
///
/// 但全局条目的语义是「所有承载 target 的家」，直接删掉会连带影响别家 ——
/// 与 `enable_on` 同一取舍：指名某家删除时把它**展开**成「其余承载 target 的
/// 家」的精确条目，目标家的那一条才真的消失。`other_providers` 由调用方从
/// 当前清单取（与 `set_state` 同），只有 target 被多家承载时展开才有内容。
///
/// `provider` 为 `None`（旧前端 / 无行上下文）时走旧版全局语义：命中该
/// (alias, target) 的全部条目，一次删干净。
pub fn remove_mapping(
    alias: &str,
    target: &str,
    provider: Option<&str>,
    other_providers: &[String],
) -> (ModelRules, bool) {
    let mut rules = current();
    let mut removed = false;
    let mut global_hit = false;
    rules.mappings.retain(|m| {
        if !(m.alias.eq_ignore_ascii_case(alias) && m.target.eq_ignore_ascii_case(target)) {
            return true;
        }
        let hit = match (m.provider.as_deref(), provider) {
            // 带 provider 的条目：指名那家才命中；不指名 = 全删（旧版语义）
            (Some(stored), Some(given)) => stored.eq_ignore_ascii_case(given),
            (Some(_), None) => true,
            // 旧版全局条目：对任何家都命中（与展示同口径）
            (None, _) => {
                global_hit = true;
                true
            }
        };
        if hit {
            removed = true;
        }
        !hit
    });
    if removed && global_hit {
        if let Some(owner) = provider {
            // 全局条目展开：其余承载 target 的家逐家补精确条目（已有的不重复加），
            // 于是「在 A 家行上删掉」不会顺手把 B 家的映射也弄丢
            for other in other_providers {
                if other.eq_ignore_ascii_case(owner) {
                    continue;
                }
                let exists = rules.mappings.iter().any(|m| {
                    m.alias.eq_ignore_ascii_case(alias)
                        && m.target.eq_ignore_ascii_case(target)
                        && m.provider
                            .as_deref()
                            .map_or(false, |p| p.eq_ignore_ascii_case(other))
                });
                if !exists {
                    rules.mappings.push(Mapping {
                        alias: alias.to_string(),
                        target: target.to_string(),
                        provider: Some(other.to_string()),
                        reasoning: None,
                    });
                }
            }
        }
    }
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

/// 各家的**额外对外名**：`(provider, 上游 id, 额外别名)`。
///
/// ── 为什么除「去前缀」外还需要这张表 ───────────────────────────
/// 去前缀种子只把上游 id 的前缀剥掉，得到的是**上游自己的拼法**；
/// 而下游（以及 WorkBuddy 那家）习惯的拼法未必相同，名字对不上就命不中这条链路，
/// 跨家的主备切换断掉一半。这张表按「上游 id → 额外别名」点名补挂，
/// 同一 alias 允许多家各一条（见模块头），不会遮蔽任何东西，只是把候选链补全。
///
/// 三条现有条目的来由：
///   - 小浣熊把 4.1 写成连字符（`sn-deepseek-v4-1-flash`），去前缀只能给出
///     `deepseek-v4-1-flash`，而下游习惯点号写法；
///   - Cline 的 `deepseek-v4.1-flash` **两池都有**（`cline-free/` 与
///     `cline-pass/`），两池现在是两家 provider，各自跑一遍去前缀种子 ——
///     先刷到的那家拿到那条短名，另一家只记 seeded 不建映射。这里**两家都
///     点名**，于是短名默认就有两条映射、各指一个池，不依赖清单刷新顺序：
///     路由时两条一起进候选链，发送名跟着实际承载的 provider 走
///     （见 `catalog::wire_target_for_provider` 的 ②）。缺了任何一条，
///     「短名默认能路由到那个池」这件事就会随刷新顺序时断时续。
const EXTRA_ALIASES: &[(&str, &str, &str)] = &[
    ("raccoon", "sn-deepseek-v4-1-flash", "deepseek-v4.1-flash"),
    // 两家 provider id 从 `Pool` 推导，别处已无 `"cline"` 这个 id（见
    // `providers::PROVIDERS` 的 cline-free / cline-pass 两条）
    ("cline-free", "cline-free/deepseek-v4.1-flash", "deepseek-v4.1-flash"),
    ("cline-pass", "cline-pass/deepseek-v4.1-flash", "deepseek-v4.1-flash"),
];

/// 额外别名的种子键：`<provider>:<id>#alias:<alias>`。
///
/// 与主 seeded 键**分开**的理由：这些模型在第一次出现时就被主种子记过
/// （存量用户的盘上早有 `<provider>:<id>`），若额外别名跟着主键一起短路，
/// 这张表里新加的条目在存量用户那里永远种不上。
/// `#alias:` 后缀不会与任何真实模型 id 相等，不干扰 `is_seeded` 的等值比较。
fn extra_alias_key(provider: &str, id: &str, alias: &str) -> String {
    format!("{provider}:{id}#alias:{alias}")
}

/// 给 `EXTRA_ALIASES` 里该家点名的模型补额外对外名。
///
/// 返回是否动过 `seeded`：动过就要落盘（否则用户删掉这条映射后，
/// 下一次清单刷新会再把它加回来）。
fn seed_extra_aliases(
    rules: &mut ModelRules,
    provider: &str,
    id: &str,
    ids: &[String],
    added: &mut Vec<String>,
) -> bool {
    let mut touched = false;
    for (owner, target, alias) in EXTRA_ALIASES {
        if !owner.eq_ignore_ascii_case(provider) || !target.eq_ignore_ascii_case(id) {
            continue;
        }
        let key = extra_alias_key(provider, id, alias);
        if rules.seeded.iter().any(|item| item.eq_ignore_ascii_case(&key)) {
            continue;
        }
        rules.seeded.push(key);
        touched = true;
        // 别名与本家清单里另一个上游 id 撞名 → 不种（与去前缀种子同一口径）
        if ids.iter().any(|other| other.eq_ignore_ascii_case(alias)) {
            continue;
        }
        // 已有同 (alias, provider, target) 的条目（用户手动加过 / 旧版残留）就不重复加。
        // 同 alias 允许多家各一条、**同一家也可以有多条**（各自指向不同的池 /
        // 上游 id，路由时一起进候选链、发送名各按自己的 target 改写），
        // 所以判重按三元组，而不是像去前缀种子那样「alias 被任何人占用就不动」
        //（点号别名天生与 WorkBuddy 的原生 id 同名，正是要共存）。
        // provider=None 的旧版全局条目（「所有承载 target 的家」）已覆盖本家，
        // 也算已存在 —— 不算的话，升级用户盘上会同时留一条旧 null 与一条新
        // provider 版的同义映射，管理页一屏两个 chip。
        let exists = rules.mappings.iter().any(|m| {
            m.alias.eq_ignore_ascii_case(alias)
                && m.target.eq_ignore_ascii_case(target)
                && m.provider.as_deref().map_or(true, |p| p.eq_ignore_ascii_case(provider))
        });
        if exists {
            continue;
        }
        rules.mappings.push(Mapping {
            alias: alias.to_string(),
            target: id.to_string(),
            provider: Some(provider.to_string()),
            // 种子建的映射不绑思考等级（那是用户手动绑定的东西）
            reasoning: None,
        });
        added.push(format!("{alias} → {id}"));
    }
    touched
}


/// 小浣熊清单的**默认规则种子**：对 `ids` 里每个还没种过的模型做一次默认处理 ——
///
///   - `raccoon-<hex>` 内部模型 → 默认**禁用**（对外不可见；管理页里仍可看到并手动启用）；
///   - `sn-` 前缀的模型 → 默认加一条**去前缀映射**（`sn-glm-5-3` → `glm-5-3`），
///     客户端用两种名字都行；去前缀后的名字若已被映射占用、或与小浣熊自己的
///     另一个上游模型 id 撞名，则跳过（照常记入 seeded，不再反复尝试）；
///   - `RACCOON_EXTRA_ALIASES` 点名的模型 → 再补一条额外对外名（如点号写法），
///     这条与主种子**独立计时**（见 `extra_alias_key`）。
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
    let mut aliases_seeded = false;
    for id in ids {
        let id = id.trim();
        if id.is_empty() {
            continue;
        }
        // 额外对外名先处理：**不受下面主 seeded 短路影响**（理由见 extra_alias_key）
        aliases_seeded |= seed_extra_aliases(&mut rules, "raccoon", id, ids, &mut mappings_added);
        if rules.is_seeded("raccoon", id) {
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
            // alias 已有映射（不管是自动还是人工）或与小浣熊自己的上游 id 撞名 → 不动
            if rules.has_alias(alias)
                || ids.iter().any(|other| other.eq_ignore_ascii_case(alias))
            {
                continue;
            }
            rules.mappings.retain(|m| {
                !(m.alias.eq_ignore_ascii_case(alias) && m.provider.as_deref() == Some("raccoon"))
            });
            rules.mappings.push(Mapping {
                alias: alias.to_string(),
                target: id.to_string(),
                provider: Some("raccoon".to_string()),
                reasoning: None,
            });
            mappings_added.push(format!("{alias} → {id}"));
        }
    }
    if disabled_added.is_empty() && mappings_added.is_empty() && !aliases_seeded {
        return None;
    }
    save(&rules);
    if disabled_added.is_empty() && mappings_added.is_empty() {
        // 只补记了「额外别名已种过」的标记（映射本来就在）——不写日志噪音
        return None;
    }
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
