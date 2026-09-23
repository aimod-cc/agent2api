//! 自定义提供商（用户自建的上游端点）：存储 + 校验。
//!
//! ── 与编译期内置注册表的区别（为什么单开一份存储）──────────────
//! 内置八家（`providers::PROVIDERS`）是**编译期**事实：id 是代码里的字面量、
//! 各家的凭证链路与协议翻译都是代码。自定义提供商是**运行期数据**：用户填
//! 名称、协议与基址，id 由本模块生成 —— 它不进 `ProviderKind` 枚举、不进
//! `PROVIDERS` 注册表，也不参与 `kind_from_id`（那条链路的未知判定必须保持
//! 「不认识就是不认识」）。
//!
//! 存放位置沿用配置模块的 kv 表：键 `customProviders`，值是 JSON 数组。
//! 读写口径与 `model_rules` 逐字同款（`config::current().raw()` 读、
//! `config::update_raw_field(KEY, value)` 写）—— 这样整个项目的「运行期可变
//! 配置」只有一套读写机制，排查时不必分心。
//!
//! ── 条目形状（对外契约，改动要前后端一起改）──────────────────
//! ```json
//! {
//!   "id": "custom-3f2a91b04c7e",   // custom- + 12 位小写十六进制（随机）
//!   "name": "我的网关",             // 1~64 字符
//!   "protocol": "chat_completions", // chat_completions / responses / anthropic
//!   "baseUrl": "https://api.example.com/v1",
//!   "enabled": true,
//!   "createdAt": 1730000000000,
//!   "models": [                     // 用户登记的模型清单（第二阶段起）
//!     { "id": "gpt-x", "enabled": true, "reasoning": "" }
//!   ],
//!   "mappings": [                   // 对外别名 → 上游模型 id 的映射（同上）
//!     { "alias": "my-alias", "target": "gpt-x", "enabled": true, "reasoning": "" }
//!   ]
//! }
//! ```
//!
//! `models` / `mappings` 为什么放在**提供商记录**里而不是像内置家那样走
//! `modelRules`：内置家的清单来自适配器（远程目录 + 静态表），启停规则挂在
//! (provider, model id) 上；自定义家的清单本身就是用户逐条登记的运行期数据，
//! 「enabled」天然是每条记录自己的字段 —— 再套一层 modelRules 等于把同一份
//! 启停状态存两处（`set_models` 整表替换的语义也对不上「逐条开关」）。
//! 因此 modelRules 的 disabled/hidden **不适用于**自定义家（管理页也不进）。
//!
//! ── 三条设计约束（改代码前务必读）───────────────────────────
//!   1. **id 用随机而不是时间戳/计数器**：卸载重装、多进程并发添加都不能
//!      撞号；`custom-` 前缀是「这是自定义家」的判据（`is_custom_provider_id`），
//!      去掉前缀就是一条普通随机串；
//!   2. **baseUrl 保存即规范化**：去首尾空白 + 去掉末尾斜杠。转发时拼路径
//!      （`{baseUrl}/chat/completions` 这类）会原样追加，留着末尾斜杠会拼出
//!      `//` 形态 —— 多数上游能容忍，但签名类网关（Anthropic 兼容端点做过
//!      URL 校验的）会拒，与其让用户在每个入口试错，不如落盘时归一；
//!   3. **apiKey 不进提供商记录**：它是**账号**的属性（同一家可以加多个账号、
//!      每个账号一把 key），落在账号记录的 `apiKey` 字段里（见
//!      `account_store::custom_accounts`）。提供商记录里只放「这家是谁」。
//!
//! ── 本阶段的范围（W 切片拆分）────────────────────────────────
//! 存储与管理 API、账号接入、**模型清单存储 + 目录/路由接线 + Chat
//! Completions 协议转发**（转发实现见 `providers::custom::forward`）。
//! `is_custom_provider_id` / `protocol_of` / `wire_model_for` 是转发层的
//! 判据入口；`responses` / `anthropic` 两种协议的转换器尚未实现 —— 配置可以
//! 保存，转发时报 400（见 `providers::custom::forward` 的分派骨架）。

mod bindings;
/// 自定义提供商**定义**的导入合并（账号导出文件里的 `customProviders` 段）。
///
/// `pub(crate)`：调用方是 `account_transfer::import_accounts` —— 账号导入的
/// 前置步骤（先有提供商，账号的外键才不悬空）。除此之外它不该被任何人触碰：
/// 定义的手工增删改走 `create` / `update` / `remove`，不走导入。
pub(crate) mod transfer;
pub use bindings::public_models;

use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::server::config;
use crate::server::core::account_store::AccountStore;
use crate::server::core::egress;
use crate::server::logging;

/// kv 键名：配置里这份数组的存放位置。
///
/// 与 `model_rules::KEY_MODEL_RULES` 同一口径 —— 它是 `config` 模块的普通
/// 顶层键，因此**绝不能**与 `db::schema::RESERVED_KV_KEYS` 相撞（撞名的后果
/// 见 `config` 模块头）。
pub const KEY_CUSTOM_PROVIDERS: &str = "customProviders";

/// 自定义 provider id 的前缀。
///
/// 判据的一部分：`is_custom_provider_id` 要求「以它开头**且**能在存储里找到」。
/// 单独以 `custom-` 开头的陌生 id（手改数据塞进来的）不算数 —— 那正是
/// 「注册表里没有、存储里也没有」的死 id，按未知处理（与全仓的 provider
/// 口径一致：不认识的 id 不猜成任何一家）。
pub const ID_PREFIX: &str = "custom-";

/// 协议枚举：OpenAI Chat Completions
pub const PROTOCOL_CHAT_COMPLETIONS: &str = "chat_completions";
/// 协议枚举：OpenAI Responses
pub const PROTOCOL_RESPONSES: &str = "responses";
/// 协议枚举：Anthropic Messages
pub const PROTOCOL_ANTHROPIC: &str = "anthropic";

/// 全部合法协议（校验用；顺序即错误文案与前端下拉的顺序）
pub const PROTOCOLS: &[&str] = &[PROTOCOL_CHAT_COMPLETIONS, PROTOCOL_RESPONSES, PROTOCOL_ANTHROPIC];

/// 展示名长度上限（字符数；1~64 是契约）
const MAX_NAME_CHARS: usize = 64;

/// 模型 id / 映射名的长度上限（字符数）。上游模型 id 一般几十字符，
/// 128 与 `model_rules` 的映射名上限同量级 —— 粘贴进一整段文档时不落盘。
const MAX_MODEL_ID_CHARS: usize = 128;
/// 映射别名（对外名）的长度上限（字符数；与 `model_rules::alias_valid` 同值）
const MAX_ALIAS_CHARS: usize = 128;
/// 思考等级绑定的长度上限（字符数；与 `model_rules::normalize_reasoning` 同值）
const MAX_REASONING_CHARS: usize = 32;

/// 拉取上游模型清单的总超时（毫秒）。清单接口是一次性的管理动作，
/// 不参与对话转发的长连接口径（egress 的 read_timeout 是给 SSE 的）。
const FETCH_MODELS_TIMEOUT_MS: u64 = 15_000;

/// 读全量条目（容错：非数组 / 坏条目一律丢弃）。
///
/// 丢弃的判据只有一条：**不是带非空 id 的对象**。其余字段（name / protocol /
/// baseUrl / enabled）缺失时按各自默认值补齐（见 `item_of`），不在这里丢弃 ——
/// 「能列出来但编辑时才报缺字段」比「静默消失」好定位。
fn read_items() -> Vec<Value> {
    let raw = config::current();
    let items = raw
        .raw()
        .get(KEY_CUSTOM_PROVIDERS)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut items: Vec<Value> = items
        .into_iter()
        .filter_map(|item| {
            let object = item.as_object()?;
            let id = object.get("id").and_then(Value::as_str)?.trim();
            if id.is_empty() {
                return None;
            }
            Some(item_of(object))
        })
        .collect();
    // 对外契约是「按 createdAt 升序」。存储里本来就应当是这个顺序（create 追加），
    // 但手改配置/旧版本写入可能乱序，所以读取时**总是**重排一次 —— 稳定排序，
    // 同一个 createdAt（毫秒相同）保持原有相对顺序。
    items.sort_by_key(|item| {
        item.get("createdAt").and_then(Value::as_i64).unwrap_or(0)
    });
    items
}

/// 一条存储条目 → 对外形态：补齐缺失的默认字段，**只有契约里的八个键**。
///
/// 为什么在这里做「补键」而不是原样透出：手改过的配置可能少一个 `enabled`，
/// 原样透出会让前端拿到 undefined，行为在多处分叉（开关渲染成关闭、过滤被跳过）。
/// 补齐后对外形态恒定，与新建路径写出来的字节一致。
///
/// `models` / `mappings` 同样在这里补默认（缺省空数组、坏条目丢弃）——
/// 转发热路径（`carriers_of_model` / `wire_model_for`）直接读本函数的产物，
/// 容错只在这一处做，读侧不必各自再判形状。
fn item_of(object: &Map<String, Value>) -> Value {
    let id = object
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .unwrap_or(&id)
        .to_string();
    let protocol = object
        .get("protocol")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| valid_protocol(text))
        .unwrap_or(PROTOCOL_CHAT_COMPLETIONS)
        .to_string();
    let base_url = object
        .get("baseUrl")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    json!({
        "id": id,
        "name": name,
        "protocol": protocol,
        "baseUrl": base_url,
        "enabled": object.get("enabled").and_then(Value::as_bool).unwrap_or(true),
        "createdAt": object.get("createdAt").and_then(Value::as_i64).unwrap_or(0),
        "models": model_entries_of(object.get("models")),
        "mappings": mapping_entries_of(object.get("mappings")),
    })
}

/// 读取容错：`models` 数组 → 归一后的条目列表（非数组 / 坏条目一律丢弃）。
///
/// 丢弃的判据与 `read_items` 同一取向：**不是带非空 id 的对象**就不要 ——
/// 一条没有 id 的模型登记对路由毫无意义，留着只会让转发侧多一次空判。
/// 长度超限（手改数据塞进来的超长值）**不丢弃、截断**：读取路径的职责是
/// 「能显示、能使用」，把脏值裁进合法范围比整条消失好定位。
fn model_entries_of(value: Option<&Value>) -> Vec<Value> {
    let mut entries: Vec<Value> = Vec::new();
    for item in value.and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]) {
        let Some(object) = item.as_object() else {
            continue;
        };
        let id = truncate_chars(
            object.get("id").and_then(Value::as_str).map(str::trim).unwrap_or(""),
            MAX_MODEL_ID_CHARS,
        );
        if id.is_empty() {
            continue;
        }
        entries.push(json!({
            "id": id,
            "enabled": object.get("enabled").and_then(Value::as_bool).unwrap_or(true),
            "reasoning": truncate_chars(
                object.get("reasoning").and_then(Value::as_str).map(str::trim).unwrap_or(""),
                MAX_REASONING_CHARS,
            ),
        }));
    }
    entries
}

/// 读取容错：`mappings` 数组 → 归一后的条目列表（判据见 [`model_entries_of`]）。
///
/// 比模型多一条判据：**alias 与 target 缺一不可** —— 只有别名的映射指不到
/// 任何上游模型（写了等于没写），只有 target 的映射没有对外名（永远不会有
/// 请求命中它）。两者都在才算一条可用的映射。
fn mapping_entries_of(value: Option<&Value>) -> Vec<Value> {
    let mut entries: Vec<Value> = Vec::new();
    for item in value.and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]) {
        let Some(object) = item.as_object() else {
            continue;
        };
        let alias = truncate_chars(
            object.get("alias").and_then(Value::as_str).map(str::trim).unwrap_or(""),
            MAX_ALIAS_CHARS,
        );
        let target = truncate_chars(
            object.get("target").and_then(Value::as_str).map(str::trim).unwrap_or(""),
            MAX_MODEL_ID_CHARS,
        );
        if alias.is_empty() || target.is_empty() {
            continue;
        }
        entries.push(json!({
            "alias": alias,
            "target": target,
            "enabled": object.get("enabled").and_then(Value::as_bool).unwrap_or(true),
            "reasoning": truncate_chars(
                object.get("reasoning").and_then(Value::as_str).map(str::trim).unwrap_or(""),
                MAX_REASONING_CHARS,
            ),
        }));
    }
    entries
}

/// 按字符截断（超过上限的部分裁掉，不 panic）
fn truncate_chars(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// 整份写回（唯一写路径）。
///
/// 返回是否写盘成功；失败时**调用方必须当成整体失败**（新值没有生效）——
/// 与 `model_rules::save` 一样，落库失败由 `config::update_raw_field` 自己
/// 打日志，这里只把布尔交给上层，不再重复记一条。
fn write_items(items: &[Value]) -> bool {
    config::update_raw_field(KEY_CUSTOM_PROVIDERS, Value::Array(items.to_vec()))
}

/// 当前全部自定义提供商（按 createdAt 升序）。
pub fn list() -> Vec<Value> {
    read_items()
}

/// 按 id 取一条；不存在返回 None。
pub fn get(id: &str) -> Option<Value> {
    let id = id.trim();
    if id.is_empty() {
        return None;
    }
    read_items()
        .into_iter()
        .find(|item| item.get("id").and_then(Value::as_str) == Some(id))
}

/// 这个 id 是不是**已注册的自定义提供商**。
///
/// 判据两条都要满足：以 [`ID_PREFIX`] 开头、且能在存储里找到。只看前缀会让
/// 「手改数据塞进来的 custom-xxx」被当成真家（账号写入、级联删除都会认它，
/// 而它没有协议与基址，转发阶段必然失败）；只看存储又太慢且语义不清。
///
/// 消费方：`providers::label_of`（展示名回退）、`api::accounts::add_account`
/// 的分派、账号公开形态的兜底判断，以及下一阶段的转发热路径（先判这里，
/// 再决定走不走自定义通道）。
pub fn is_custom_provider_id(id: &str) -> bool {
    let id = id.trim();
    id.starts_with(ID_PREFIX) && get(id).is_some()
}

/// 自定义提供商的展示名；不存在返回 None（调用方决定回退到什么）。
pub fn label_of(id: &str) -> Option<String> {
    get(id).and_then(|item| {
        item.get("name")
            .and_then(Value::as_str)
            .map(str::to_string)
    })
}

/// 自定义提供商的协议；不存在返回 None。
///
/// 下一阶段转发的分派依据（三选一的协议翻译）；本阶段供管理 API 与排查使用。
pub fn protocol_of(id: &str) -> Option<String> {
    get(id).and_then(|item| {
        item.get("protocol")
            .and_then(Value::as_str)
            .map(str::to_string)
    })
}

/// 新建一个自定义提供商：`{name, protocol, baseUrl}` → 落盘后的完整条目。
///
/// `apiKey` **刻意不在本函数的处理范围**：它是账号的属性（见模块头），由
/// 调用方（`api::custom_providers::create`）在拿到 provider id 之后写进
/// 第一个账号记录。
pub fn create(payload: &Value) -> Result<Value, String> {
    let object = payload
        .as_object()
        .ok_or_else(|| "请求体必须是 JSON 对象".to_string())?;
    let name = normalize_name(object.get("name"))?;
    let protocol = normalize_protocol(object.get("protocol"))?;
    let base_url = normalize_base_url(
        object
            .get("baseUrl")
            .and_then(Value::as_str)
            .unwrap_or(""),
    )?;
    let id = new_provider_id()?;
    let item = json!({
        "id": id,
        "name": name,
        "protocol": protocol,
        "baseUrl": base_url,
        "enabled": true,
        "createdAt": logging::now_ms(),
    });
    let mut items = read_items();
    items.push(item.clone());
    if !write_items(&items) {
        return Err("保存失败：配置写入未成功（请检查磁盘空间与配置目录权限）".to_string());
    }
    Ok(item)
}

/// 更新一个自定义提供商：字段缺省不动，改 protocol / baseUrl 时同样校验。
///
/// 为什么不支持改 `id` / `createdAt`：前者是账号记录外键的指向目标（改了会让
/// 名下账号变成孤儿），后者是排序依据 —— 两者都是身份的一部分，不是可编辑属性。
pub fn update(id: &str, payload: &Value) -> Result<Value, String> {
    let id = id.trim();
    if id.is_empty() {
        return Err("缺少提供商 id".to_string());
    }
    let object = payload
        .as_object()
        .ok_or_else(|| "请求体必须是 JSON 对象".to_string())?;
    let mut items = read_items();
    let Some(index) = items
        .iter()
        .position(|item| item.get("id").and_then(Value::as_str) == Some(id))
    else {
        return Err(format!("自定义提供商不存在: {id}"));
    };
    // 逐项取值：**只在请求里带了合法值时才改**。非法值一律报错（不静默忽略）——
    // 「保存成功但没生效」比一条明确的 400 难排查得多。
    let current = items[index].clone();
    let mut merged = current.as_object().cloned().unwrap_or_default();
    if let Some(value) = object.get("name") {
        let name = normalize_name(Some(value))?;
        merged.insert("name".to_string(), Value::String(name));
    }
    if let Some(value) = object.get("protocol") {
        let protocol = normalize_protocol(Some(value))?;
        merged.insert("protocol".to_string(), Value::String(protocol));
    }
    if let Some(value) = object.get("baseUrl") {
        let base_url = normalize_base_url(value.as_str().unwrap_or(""))?;
        merged.insert("baseUrl".to_string(), Value::String(base_url));
    }
    if let Some(value) = object.get("enabled") {
        let enabled = value
            .as_bool()
            .ok_or_else(|| "enabled 必须是布尔值".to_string())?;
        merged.insert("enabled".to_string(), Value::Bool(enabled));
    }
    let updated = Value::Object(merged);
    items[index] = updated.clone();
    if !write_items(&items) {
        return Err("保存失败：配置写入未成功（请检查磁盘空间与配置目录权限）".to_string());
    }
    Ok(updated)
}

/// 删除一个自定义提供商，**并级联删除该家名下的全部账号**；返回删掉的账号数。
///
/// ── 为什么级联删除是必须的（而不是留给用户手删）──────────────
/// 账号记录的归属是 `provider` 字符串字段。提供商删掉后，那些账号会变成
/// 「指向一个不存在家的孤儿」：界面分组里挂着、路由候选链里永远排不上、
/// 用户想清理还得逐条点删。这与 `model_rules::remove_custom` 顺带清理孤儿
/// 规则是同一个取舍 —— 删除动作的语义是「这一家我不要了」，占着的东西一起清。
///
/// ── 顺序：先删账号，再删提供商 ───────────────────────────────
/// 账号删除可能因为数据库不可用而失败；那种情况下提供商**必须保留**（否则
/// 留下一批孤儿账号，比「什么都没发生」更难恢复）。反过来（提供商写失败、
/// 账号已删）只会留下一个账号数为 0 的提供商，用户重试一次即可 —— 两个方向的
/// 后果不对等，所以顺序不可交换。
pub fn remove(id: &str, store: &AccountStore) -> Result<usize, String> {
    let id = id.trim();
    if id.is_empty() {
        return Err("缺少提供商 id".to_string());
    }
    if get(id).is_none() {
        return Err(format!("自定义提供商不存在: {id}"));
    }
    let accounts_removed = store
        .remove_custom_accounts(id)
        .map_err(|error| error.message)?;
    let mut items = read_items();
    items.retain(|item| item.get("id").and_then(Value::as_str) != Some(id));
    if !write_items(&items) {
        return Err("保存失败：配置写入未成功（请检查磁盘空间与配置目录权限）".to_string());
    }
    Ok(accounts_removed)
}

// ─── 模型清单与映射（第二阶段：目录 / 路由 / 转发的数据源）──────

/// 该家的模型清单（归一后；`item_of` 已补默认，这里只是取键）。
///
/// 消费方：目录聚合（`providers::custom` 的追加）、`carriers_of_model` 的
/// 能力判定、以及管理弹窗的回显。**enabled 过滤不在这一层做** —— 「清单」
/// 与「清单里可用的部分」是两个口径（对齐 `catalog::manifest_for` 与
/// `advertised_manifest_for` 的分工），调用方按需过滤。
pub fn models_of(id: &str) -> Vec<Value> {
    get(id)
        .and_then(|item| item.get("models").and_then(Value::as_array).cloned())
        .unwrap_or_default()
}

/// 该家的映射表（归一后；判据见 [`models_of`]）。
pub fn mappings_of(id: &str) -> Vec<Value> {
    get(id)
        .and_then(|item| item.get("mappings").and_then(Value::as_array).cloned())
        .unwrap_or_default()
}

/// **整体替换**该家的 `models` 与 `mappings` 两个键并落盘（UI 弹窗整表保存）。
///
/// ── 为什么是「整体替换」而不是逐条增删 ────────────────────────
/// 模型管理弹窗的交互是「编辑一份本地副本 → 点保存」：允许顺序调整、批量
/// 启停、一次改多条 reasoning。逐条 API 要么把顺序丢失、要么逼前端发 N 个
/// 请求且中途失败会留下半成品。整表替换的语义与弹窗一一对应，回滚也简单
/// （失败即整体不生效，见 `write_items` 的说明）。
///
/// ── 整体读写下的键保护 ────────────────────────────────────────
/// 记录是**整体读写**的（`read_items` / `write_items`），本函数必须先把
/// 既有记录完整取出来、只改这两个键再写回 —— 直接写一个只含这两键的对象
/// 会把 name / protocol / baseUrl / enabled 全部冲掉。校验在写入前完成：
/// 非法值（空 id、超长、类型不对）一律 400，绝不静默丢弃 —— 「保存成功但
/// 没生效」比一条明确的错误难排查得多。
///
/// 返回更新后的**完整记录**（响应契约 `{provider: ...}` 直接透传给前端重绘）。
pub fn set_models(id: &str, models: Value, mappings: Value) -> Result<Value, String> {
    let id = id.trim();
    if id.is_empty() {
        return Err("缺少提供商 id".to_string());
    }
    let models_raw = models
        .as_array()
        .ok_or_else(|| "models 必须是数组".to_string())?;
    let mappings_raw = mappings
        .as_array()
        .ok_or_else(|| "mappings 必须是数组".to_string())?;
    let models = validate_model_entries(models_raw)?;
    let mappings = validate_mapping_entries(mappings_raw)?;
    let mut items = read_items();
    let Some(index) = items
        .iter()
        .position(|item| item.get("id").and_then(Value::as_str) == Some(id))
    else {
        return Err(format!("自定义提供商不存在: {id}"));
    };
    // 从既有记录的键表出发整体写回：除 models/mappings 外的键原样保留
    let mut merged = items[index].as_object().cloned().unwrap_or_default();
    merged.insert("models".to_string(), Value::Array(models));
    merged.insert("mappings".to_string(), Value::Array(mappings));
    let updated = Value::Object(merged);
    items[index] = updated.clone();
    if !write_items(&items) {
        return Err("保存失败：配置写入未成功（请检查磁盘空间与配置目录权限）".to_string());
    }
    Ok(updated)
}

/// 「这个名字由哪些自定义家提供」（`catalog::providers_for_model` 的自定义段）。
///
/// 判据（三条同时成立才算承载）：该家 `enabled=true`，且存在一条 **enabled**
/// 的同 id 模型，或一条 **enabled** 且 `alias==name` 的映射。名字比较
/// `eq_ignore_ascii_case` —— 与全仓的模型名匹配口径一致（`model_rules` 的
/// 模块头），客户端传 `GPT-X` 与 `gpt-x` 必须落到同一条登记上。
///
/// 注意这里**不查账号、不查 baseUrl**：它回答的是「能力」（谁登记了这个
/// 名字），「此刻可不可用」由目录聚合（还要求有启用账号）与转发层（账号
/// 选路失败会如实报错）分层回答 —— 与内置家的 `manifest_for` 同一分工。
///
/// 性能：单次 `read_items()` 遍历（配置是进程级快照，无磁盘 IO）；
/// 转发热路径每请求调用一两次，无需缓存。
pub fn carriers_of_model(name: &str) -> Vec<String> {
    let name = name.trim();
    if name.is_empty() { return Vec::new(); }
    read_items().into_iter()
        .filter(|provider| bindings::resolve(provider, name).is_some())
        .filter_map(|provider| provider.get("id").and_then(Value::as_str).map(str::to_string))
        .collect()
}

/// 发给某个自定义家时应使用的**上游真名**与思考等级绑定。
///
/// ── 解析优先级（与 `catalog::wire_target_for_provider` 对齐但数据源不同）──
///   ① 存在 enabled 且 `alias == 请求名` 的映射 → 真名 = 该映射的 `target`、
///      等级 = 该映射的 `reasoning`（映射上没绑则空）；
///   ② 否则存在 enabled 的同 id 模型 → 真名 = 请求名**原值**（零改写是常态，
///      统一大小写只会白复制一次 body）、等级 = 该模型上绑的 `reasoning`；
///   ③ 都不命中 → 真名 = 请求名、等级 = None（防御：正常选路下不会发生，
///      候选链就是按 [`carriers_of_model`] 算的）。
///
/// 名字比较一律 `eq_ignore_ascii_case`（理由同 [`carriers_of_model`]）。
///
/// ── 与思考等级注入的约定（`providers::custom::forward`）────────
/// 返回的 `Some(level)` 表示「这条映射 / 模型上绑了等级」；注入还要满足两个
/// 条件（在调用方判，这里不重复）：客户端请求体里**没有**显式给
/// `reasoning_effort` / `reasoning`（用户显式意图优先于默认绑定），且等级不是
/// `off`/`none`（本网关不向任何上游发「关闭思考」字段，判据
/// `model_rules::reasoning_is_off`）。名字与等级**同源同次解析** —— 拆成两次
/// 调用就会出现「A 条映射改了名、B 条映射的等级被注入」的串味。
pub fn wire_model_for(provider_id: &str, requested_name: &str) -> (String, Option<String>) {
    let requested = requested_name.trim();
    get(provider_id).and_then(|provider| bindings::resolve(&provider, requested))
        .unwrap_or_else(|| (requested.to_string(), None))
}

/// **服务端代理拉取**上游的模型清单（`POST /api/custom-providers/fetch-models`）。
///
/// 为什么由网关代拉而不是浏览器直连：桌面壳的页面跑在本地 origin 上，
/// 直连会撞上游的 CORS；且 apiKey 只在服务端流转（公开形态刻意不含它），
/// 让前端自己去拉就得把 key 交给页面 —— 两条理由都指向同一做法。
///
/// ── URL 与鉴权（各协议的 baseUrl 惯例不同，别拼错）─────────────
///   - `chat_completions` / `responses`：baseUrl 惯例**带 /v1**（如
///     `https://api.example.com/v1`）→ 拼 `{baseUrl}/models`；
///   - `anthropic`：baseUrl 惯例是**不带 /v1 的根地址**（如
///     `https://api.anthropic.com`）→ 拼 `{baseUrl}/v1/models`。
///   鉴权：前两者 `Authorization: Bearer <key>`；anthropic 是
///   `x-api-key` + `anthropic-version: 2023-06-01`（它的鉴权头不是 Bearer）。
///
/// key 取该家**第一个 enabled 且 apiKey 非空**的账号（`first_custom_credential`，
/// 按优先级升序 —— 与转发选路的「队首优先」同口径）；一个都没有时报
/// 「请先添加账号」。账号配了出网代理就带上（`ResolvedProxy`），没有则直连。
///
/// 响应解析：OpenAI 与 anthropic 的清单形态相同（`{data: [{id}]}`），
/// 取每个条目的非空 `id`。**不落盘** —— 清单拉回来给前端确认后再调
/// `set_models`，拉取与保存是弹窗里的两步。
pub async fn fetch_upstream_models(
    provider_id: &str,
    store: &AccountStore,
) -> Result<Vec<String>, String> {
    let provider = get(provider_id)
        .ok_or_else(|| format!("自定义提供商不存在: {}", provider_id.trim()))?;
    let protocol = provider
        .get("protocol")
        .and_then(Value::as_str)
        .unwrap_or(PROTOCOL_CHAT_COMPLETIONS)
        .to_string();
    let base_url = provider
        .get("baseUrl")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if base_url.is_empty() {
        return Err("该提供商没有配置 baseUrl，无法拉取模型清单".to_string());
    }
    let url = if protocol == PROTOCOL_ANTHROPIC {
        format!("{base_url}/v1/models")
    } else {
        format!("{base_url}/models")
    };
    let credential = store
        .first_custom_credential(provider_id)
        .ok_or_else(|| "请先添加账号：拉取模型清单需要一个启用且填写了 apiKey 的账号".to_string())?;
    let client = egress::client_for(credential.proxy.as_ref());
    let mut builder = client
        .get(&url)
        .timeout(Duration::from_millis(FETCH_MODELS_TIMEOUT_MS));
    if protocol == PROTOCOL_ANTHROPIC {
        builder = builder
            .header("x-api-key", credential.api_key.as_str())
            .header("anthropic-version", "2023-06-01");
    } else {
        builder = builder.header("Authorization", format!("Bearer {}", credential.api_key));
    }
    let response = builder.send().await.map_err(|error| {
        format!("上游请求失败: {}", egress::describe_error_detail(&error))
    })?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    if !response_ok(status) {
        // 摘要取前 200 字符：够定位（大多数错误体一两行），又不至于把
        // 一整页 HTML 灌进错误提示
        let summary: String = text.trim().chars().take(200).collect();
        return Err(format!("上游返回 {status}: {summary}"));
    }
    let payload: Value = serde_json::from_str(&text)
        .map_err(|error| format!("上游响应不是合法 JSON: {error}"))?;
    let ids = payload
        .get("data")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    item.get("id")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|id| !id.is_empty())
                        .map(str::to_string)
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(ids)
}

/// 上游清单接口是否成功（2xx 即可：个别网关会回 200 以外的成功码）
fn response_ok(status: u16) -> bool {
    (200..300).contains(&status)
}

/// `set_models` 的模型条目校验与归一：id 非空 ≤128、reasoning ≤32、同 id 去重。
///
/// 与读取路径（[`model_entries_of`]）的分工：那边是**容错**（脏数据截断/丢弃，
/// 保证读侧能用），这边是**门禁**（用户刚提交的值必须原样合法，报错把问题
/// 指回表单）。去重是「同 id 只留一条」（忽略大小写、先到的赢）—— 重复登记
/// 通常来自「获取模型」导入 + 手工添加的叠加，留谁对转发无差（名字相同），
/// 保留先出现的那条让「列表顺序 = 用户整理的顺序」成立。
fn validate_model_entries(raw: &[Value]) -> Result<Vec<Value>, String> {
    let mut entries: Vec<Value> = Vec::new();
    for (position, item) in raw.iter().enumerate() {
        let Some(object) = item.as_object() else {
            return Err(format!("models 第 {} 项必须是对象", position + 1));
        };
        let id = object
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if id.is_empty() {
            return Err(format!("models 第 {} 项缺少模型 id", position + 1));
        }
        if id.chars().count() > MAX_MODEL_ID_CHARS {
            return Err(format!(
                "模型 id 过长（最多 {MAX_MODEL_ID_CHARS} 个字符）"
            ));
        }
        let reasoning = validate_reasoning(object.get("reasoning"))?;
        let enabled = object
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let duplicated = entries.iter().any(|known| {
            known
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|text| text.eq_ignore_ascii_case(id))
        });
        if duplicated {
            continue;
        }
        entries.push(json!({
            "id": id,
            "enabled": enabled,
            "reasoning": reasoning,
        }));
    }
    Ok(entries)
}

/// `set_models` 的映射条目校验与归一：alias/target 非空（≤128）、
/// reasoning ≤32、同 (alias, target) 去重。
///
/// 去重判据是**成对**的：同一 alias 指向不同 target 是合法的主备用法
/// （与 `model_rules` 的映射语义一致），只 dedup「完全相同」的两条。
fn validate_mapping_entries(raw: &[Value]) -> Result<Vec<Value>, String> {
    let mut entries: Vec<Value> = Vec::new();
    for (position, item) in raw.iter().enumerate() {
        let Some(object) = item.as_object() else {
            return Err(format!("mappings 第 {} 项必须是对象", position + 1));
        };
        let alias = object
            .get("alias")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        let target = object
            .get("target")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if alias.is_empty() {
            return Err(format!("mappings 第 {} 项缺少映射名（alias）", position + 1));
        }
        if alias.chars().count() > MAX_ALIAS_CHARS {
            return Err(format!("映射名过长（最多 {MAX_ALIAS_CHARS} 个字符）"));
        }
        if target.is_empty() {
            return Err(format!("mappings 第 {} 项缺少目标上游模型（target）", position + 1));
        }
        if target.chars().count() > MAX_MODEL_ID_CHARS {
            return Err(format!(
                "目标上游模型过长（最多 {MAX_MODEL_ID_CHARS} 个字符）"
            ));
        }
        let reasoning = validate_reasoning(object.get("reasoning"))?;
        let enabled = object
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let duplicated = entries.iter().any(|known| {
            known.get("alias").and_then(Value::as_str)
                .is_some_and(|text| text.eq_ignore_ascii_case(alias))
                && known.get("target").and_then(Value::as_str)
                    .is_some_and(|text| text.eq_ignore_ascii_case(target))
        });
        if duplicated {
            continue;
        }
        entries.push(json!({
            "alias": alias,
            "target": target,
            "enabled": enabled,
            "reasoning": reasoning,
        }));
    }
    Ok(entries)
}

/// reasoning 绑定校验：缺省/空 → 空串（无绑定）；给了就必须是字符串且 ≤32 字符。
fn validate_reasoning(value: Option<&Value>) -> Result<String, String> {
    match value {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(text)) => {
            let text = text.trim();
            if text.chars().count() > MAX_REASONING_CHARS {
                return Err(format!(
                    "思考等级过长（最多 {MAX_REASONING_CHARS} 个字符）"
                ));
            }
            Ok(text.to_string())
        }
        Some(_) => Err("reasoning 必须是字符串或 null".to_string()),
    }
}

/// 生成新的 provider id：`custom-` + 12 位小写十六进制。
///
/// 6 字节随机源取自 `getrandom`（依赖已在 Cargo.toml；altcha / access 等模块
/// 同款用法）。**不用时间戳或计数器**：卸载重装、同一毫秒内并发添加、从
/// 备份恢复配置这三种场景下，时间戳都可能撞号，而撞号的后果是两条配置互相
/// 认成对方（`get` 按 id 单条匹配）—— 用了随机源则碰撞概率可以忽略。
///
/// 随机源失败时如实报错（返回 Err）：宁可让用户重试一次添加，也不落一条
/// id 靠时间戳兜底的记录 —— 那正是本函数刻意避开的形态。
fn new_provider_id() -> Result<String, String> {
    let mut bytes = [0u8; 6];
    getrandom::getrandom(&mut bytes)
        .map_err(|_| "无法生成安全的随机 id（系统随机源不可用），请重试".to_string())?;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!("{ID_PREFIX}{hex}"))
}

/// 展示名校验：trim 后 1~64 字符（按字符数，不是字节数 —— 中文名一样受 64 限制）。
fn normalize_name(value: Option<&Value>) -> Result<String, String> {
    let name = value
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if name.is_empty() {
        return Err("缺少提供商名称".to_string());
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(format!("提供商名称过长（最多 {MAX_NAME_CHARS} 个字符）"));
    }
    Ok(name.to_string())
}

/// 协议校验：必须是 [`PROTOCOLS`] 三选一。
fn normalize_protocol(value: Option<&Value>) -> Result<String, String> {
    let protocol = value
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if !valid_protocol(protocol) {
        return Err(format!(
            "协议必须是以下之一: {}",
            PROTOCOLS.join(" / ")
        ));
    }
    Ok(protocol.to_string())
}

/// 协议是否合法（不分配、不报错的纯判据；`item_of` 的读取容错也用它）
fn valid_protocol(protocol: &str) -> bool {
    PROTOCOLS.iter().any(|known| *known == protocol)
}

/// baseUrl 校验与规范化：必须是 http/https URL，去首尾空白并**去掉末尾斜杠**。
///
/// 用 `url::Url` 解析而不是字符串前缀检查：后者放得进 `https://` 这种空 host、
/// 含空格、或带 schema 以外内容的形态，而它们的失败时刻会推迟到转发时
/// （400/超时/被上游拒），排查成本比在这里拒掉高得多。
/// `Url::parse` 对「只有协议头没有 host」的形态会直接报错，所以解析成功即
/// host 非空；`as_str()` 对空路径的 URL 会补一个 `/`（`https://a.com` →
/// `https://a.com/`），末尾统一 `trim_end_matches('/')` 归一 —— 用户填不填
/// 尾斜杠落盘后都是同一个值。其余部分（大小写、查询串、端口）保留原样。
///
/// `pub`：账号记录上的 `baseUrl` **覆盖项**（`custom_accounts` 写入）走同一套
/// 校验 —— 覆盖值与提供商默认值最终都被转发层消费，两处各写一套迟早分叉。
pub fn normalize_base_url(raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("缺少 baseUrl".to_string());
    }
    let parsed = url::Url::parse(raw).map_err(|error| format!("baseUrl 不是合法的 URL: {error}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("baseUrl 必须以 http:// 或 https:// 开头".to_string());
    }
    Ok(parsed.as_str().trim_end_matches('/').to_string())
}
