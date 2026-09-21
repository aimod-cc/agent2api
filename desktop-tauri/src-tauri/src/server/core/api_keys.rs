//! 网关 API Key 列表（config.json 的 `apiKeys` 字段）。
//!
//! 形状：`apiKeys: [{id, name, key, enabled, createdAt, allowedProviders, allowedModels}]`。
//! 多把 Key 任一命中即通过；一把启用的都没有 → 不鉴权（网关只监听 127.0.0.1，
//! 与旧的「未配置 apiKey」语义一致）。
//!
//! ── 与旧字段 `apiKey` 的关系 ────────────────────────────────
//! 1.x 只有一把 Key（`apiKey` 字符串）。读侧兼容：`apiKeys` 缺失而 `apiKey` 存在时，
//! 把它当成一条 id 为 `legacy` 的记录展示 / 校验；写侧一旦动过列表就落成 `apiKeys`
//! 并删掉 `apiKey`，从此只有一份真相。环境变量 `WORKBUDDY_PROXY_API_KEY` 仍然
//! 算一把额外的启用 Key（启动注入语义不变）。
//!
//! ── 每把 Key 的**可用提供商 / 可用模型**白名单（R9，参考 OmniProxy）──
//! 两个字段，语义与 OmniProxy 的 `allowed_models` / `allowed_provider_ids`
//! **逐条对齐**（那一份见 `server/dist/utils/apiKeyRestrictions.js`）：
//!   - `allowedProviders: string[]`：允许路由到的 **provider id**
//!     （`workbuddy` / `raccoon` / `catpaw` / `autoclaw` / `qoder` /
//!     `cline-free` / `cline-pass`）。OmniProxy 存的是**数字** provider id
//!     （它的上游表主键），我们这边 provider 的身份本来就是字符串
//!     （见 `providers::PROVIDERS` 的说明：「字符串本身就是契约」），
//!     所以这里用字符串 —— 不要为了「对齐」把 id 数字化。
//!   - `allowedModels: string[]`：允许请求的**对外模型名**（下游请求体里的
//!     那个名字，含映射的 alias）。
//!
//! **空数组 / 字段缺失 = 不限制**：这与 OmniProxy 的「`NULL` = 不限制」等价
//! （我们这边 `apiKeys` 是 JSON 数组，天然能表达空列表，不需要额外造一个 null
//! 语义）。两个白名单**同时生效时按交集**（模型要在白名单里，且实际承载的家也要
//! 在白名单里），与 OmniProxy 的注释一致。
//!
//! ── 一条硬不变量：升级绝不能让已有 Key 失效 ────────────────────
//! 旧记录没有这两个键，`from_value` 把它们读成空 `Vec`（= 不限制）；
//! **读取路径不许因为字段缺失而丢掉整条记录**（那等于把用户的 Key 静默作废，
//! 所有客户端立刻 401）。所以这两个字段的解析只做「取不到就用空」，
//! 不做任何校验性拒绝 —— 连 `allowedProviders` 里出现未知 provider id 也照收
//! （可能来自新版本写入、被旧版本读到；真正的校验点在管理 API 的写入侧）。
//! 写回时**总是**写出这两个键（空也得是 `[]`）：配置形状稳定，
//! 用户手改配置时也看得出这两个位置存在。

use serde_json::{json, Map, Value};

use crate::server::config;

pub const KEY_API_KEYS: &str = "apiKeys";
const LEGACY_ID: &str = "legacy";
const LEGACY_NAME: &str = "默认 Key";
/// 与旧版一致的最小长度
pub const MIN_KEY_LENGTH: usize = 8;

/// 一条 Key 记录
#[derive(Clone, Debug)]
pub struct ApiKeyEntry {
    pub id: String,
    pub name: String,
    pub key: String,
    pub enabled: bool,
    pub created_at: i64,
    /// 可用提供商白名单（provider id 字符串）；**空 = 不限制**（见模块头）
    pub allowed_providers: Vec<String>,
    /// 可用模型白名单（**对外模型名**，含映射 alias）；**空 = 不限制**
    pub allowed_models: Vec<String>,
}

/// 从 JSON 里读一个「字符串数组」白名单（缺失 / 类型不对 / 空 → 空数组）。
///
/// ── 为什么一律容错、绝不拒绝整条记录 ────────────────────────
/// 这两个字段是**限制**：读不出来时正确的兜底是「不限制」（放行），
/// 而不是「拒绝」或「丢掉这条 Key」。丢掉一条旧记录 = 用户的 Key 凭空失效、
/// 所有客户端立刻 401；把它读成「限制全部」= 所有请求 403。两种都不能接受。
/// 所以这里只做「能读多少读多少」：非数组给空、元素非字符串跳过、
/// 去空白、大小写去重（模型名比对忽略大小写，见 `allows_model`）。
///
/// 去重保留首次出现的拼写：界面上展示的是用户当初填的那个形态。
fn string_list(value: Option<&Value>) -> Vec<String> {
    let Some(Value::Array(items)) = value else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for item in items {
        let Some(text) = item.as_str() else { continue };
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        if out.iter().any(|known| known.eq_ignore_ascii_case(text)) {
            continue;
        }
        out.push(text.to_string());
    }
    out
}

/// 白名单列表 → 落盘形态（**空也写成 `[]`**，见模块头）
fn list_value(list: &[String]) -> Value {
    Value::Array(list.iter().map(|item| Value::String(item.clone())).collect())
}

impl ApiKeyEntry {
    fn from_value(value: &Value) -> Option<Self> {
        let key = value.get("key")?.as_str()?.trim().to_string();
        if key.is_empty() {
            return None;
        }
        let id = value
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_string)?;
        Some(Self {
            id,
            name: value
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            key,
            enabled: !matches!(value.get("enabled"), Some(Value::Bool(false))),
            created_at: value.get("createdAt").and_then(Value::as_i64).unwrap_or(0),
            allowed_providers: string_list(value.get("allowedProviders")),
            allowed_models: string_list(value.get("allowedModels")),
        })
    }

    fn to_value(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "key": self.key,
            "enabled": self.enabled,
            "createdAt": self.created_at,
            "allowedProviders": list_value(&self.allowed_providers),
            "allowedModels": list_value(&self.allowed_models),
        })
    }

    /// 管理接口的输出形态（含明文 key：桌面端本地管理界面要能复制给客户端）
    pub fn public_json(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "key": self.key,
            "masked": mask(&self.key),
            "enabled": self.enabled,
            "createdAt": self.created_at,
            "allowedProviders": list_value(&self.allowed_providers),
            "allowedModels": list_value(&self.allowed_models),
        })
    }
}

// ── 白名单判定住在 `core::key_scope`，这里**不再**各写一份 ──────────
// 本模块只负责「存取」：把两个列表读出来、写回去。判定（忽略大小写、空 =
// 不限制、两个维度各自独立）全在 `key_scope::KeyScope` 上，那里是唯一的
// 实现点 —— 曾经在这里也写过一对 `allows_model` / `allows_provider`，
// 编译器的 dead_code 警告指出了问题：两处实现同一套规则，改一处忘一处
// 就会让「管理页显示的限制」与「转发时实际执行的限制」不一致，
// 而那种分叉在界面上完全看不出来（都是 200，只是拒绝/放行的边界差了几个字）。

/// 掩码：前 6 后 4（沿用旧版口径）
pub fn mask(key: &str) -> String {
    let head: String = key.chars().take(6).collect();
    let total = key.chars().count();
    let tail: String = key.chars().skip(total.saturating_sub(4)).collect();
    format!("{head}...{tail}")
}

/// 从配置底稿解析全部记录（含旧字段兼容）
pub fn entries_from(raw: &Map<String, Value>) -> Vec<ApiKeyEntry> {
    if let Some(Value::Array(items)) = raw.get(KEY_API_KEYS) {
        return items.iter().filter_map(ApiKeyEntry::from_value).collect();
    }
    match raw.get("apiKey").and_then(Value::as_str).map(str::trim) {
        Some(key) if !key.is_empty() => vec![ApiKeyEntry {
            id: LEGACY_ID.to_string(),
            name: LEGACY_NAME.to_string(),
            key: key.to_string(),
            enabled: true,
            created_at: 0,
            // 旧字段形态的 Key 从来没有白名单概念 —— 空 = 不限制，
            // 与它升级前的行为（放行一切）逐字一致
            allowed_providers: Vec::new(),
            allowed_models: Vec::new(),
        }],
        _ => Vec::new(),
    }
}

/// 当前全部记录
pub fn list() -> Vec<ApiKeyEntry> {
    entries_from(config::current().raw())
}

/// 当前**启用**的明文 Key（鉴权中间件用）；空 = 免鉴权
pub fn active_keys_from(raw: &Map<String, Value>) -> Vec<String> {
    let mut keys: Vec<String> = entries_from(raw)
        .into_iter()
        .filter(|entry| entry.enabled)
        .map(|entry| entry.key)
        .collect();
    if let Some(env) = std::env::var("WORKBUDDY_PROXY_API_KEY")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        if !keys.contains(&env) {
            keys.push(env);
        }
    }
    keys
}

/// 按明文 Key 找出**当前启用**的那条记录（R9：把「命中的是哪把 Key」交给 handler）。
///
/// ── 为什么要有这个函数 ──────────────────────────────────────
/// 鉴权中间件（`http::require_api_key`）原先只需要回答「这个请求能不能进」，
/// 于是它只比对一串明文 Key（`active_keys_from`）。可用提供商 / 可用模型
/// 是**逐把 Key** 的限制，handler 因此必须知道「这次用的是哪一把」——
/// 本函数就是那个反向查找，中间件命中后调它一次，把记录塞进请求扩展
/// （见 `http::require_api_key` 的实现与说明）。
///
/// ── 什么情况下返回 None（且必须放行，不是拒绝）────────────────
///   - **免鉴权模式**：一把启用的 Key 都没有 —— 请求根本没有「命中的 Key」，
///     限制自然不存在（不限制）。中间件在那个分支根本不会调本函数。
///   - **环境变量 Key**（`WORKBUDDY_PROXY_API_KEY`）：它不是一个列表记录，
///     没有地方挂白名单。**按不限制处理**（`None`），这是启动注入旁路的既有
///     语义 —— 它是给脚本/CI 的通用口令，加一层「无法配置的限制」只会让人
///     误以为它被限制了。
///   - Key 在两次读盘之间被改/删：同样按不限制（宁可放行，不可因为在管理页
///     改了一下名称就把正在跑的客户端全挡掉）。
///
/// ── 为什么要传 `raw` 而不是自己调 `list()` ───────────────────
/// `config::current()` 会**克隆整份配置**（含全部 raw 字段）。鉴权是每个请求
/// 都要走的热路径，而调用方手里已经有一份刚取到的快照 —— 让它再调一次
/// `list()` 就是为同一件事多克隆一次配置。这里收快照、调用方复用，
/// 顺带让「判定用的是哪一份快照」在调用点一眼可见（不会出现「查 Key 用一份、
/// 查白名单用另一份」的窗口期）。
pub fn entry_for_key_from(raw: &Map<String, Value>, key: &str) -> Option<ApiKeyEntry> {
    let env = std::env::var("WORKBUDDY_PROXY_API_KEY")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    if env.as_deref() == Some(key) {
        return None;
    }
    entries_from(raw)
        .into_iter()
        .find(|entry| entry.enabled && entry.key == key)
}

/// 随机生成一把 Key：`sk-a2a-` + 32 位十六进制。
///
/// 项目刻意不引入随机数 crate（见 Cargo.toml 对 getrandom 的说明），这里用
/// sha256(纳秒时间戳 + 进程 id + 单调计数 + 栈地址) 取前 16 字节：本地工具的
/// 访问口令，防的是「猜到」而不是密码学攻击，这个熵源足够。
pub fn generate() -> String {
    use sha2::{Digest, Sha256};
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let counter = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let probe = 0u8;
    let address = &probe as *const u8 as usize;
    let mut hasher = Sha256::new();
    hasher.update(nanos.to_le_bytes());
    hasher.update(std::process::id().to_le_bytes());
    hasher.update(counter.to_le_bytes());
    hasher.update(address.to_le_bytes());
    let digest = hasher.finalize();
    let hex: String = digest.iter().take(16).map(|b| format!("{b:02x}")).collect();
    format!("sk-a2a-{hex}")
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 把整份列表写回配置（同时删掉旧字段 `apiKey`）
fn save(entries: &[ApiKeyEntry]) -> bool {
    let list: Vec<Value> = entries.iter().map(ApiKeyEntry::to_value).collect();
    config::replace_api_keys(Value::Array(list))
}

/// 新增：`key` 为空则自动生成。返回新记录；名称重复不限制，key 重复拒绝。
///
/// 两个白名单随记录一起建（空 = 不限制）。**不在这一层校验 provider id**
/// 是否在注册表里：那是管理 API（写入侧）的职责 —— 本模块是「存取 + 判定」，
/// 校验集中在入口可以让「哪些值算合法」只有一处定义，而读侧永远宽容
/// （见模块头那条硬不变量）。
pub fn add(
    name: &str,
    key: Option<&str>,
    allowed_providers: Vec<String>,
    allowed_models: Vec<String>,
) -> Result<ApiKeyEntry, String> {
    let mut entries = list();
    let key = match key.map(str::trim).filter(|k| !k.is_empty()) {
        Some(given) => {
            if given.chars().count() < MIN_KEY_LENGTH {
                return Err(format!("API Key 至少需要 {MIN_KEY_LENGTH} 个字符"));
            }
            given.to_string()
        }
        None => generate(),
    };
    if entries.iter().any(|entry| entry.key == key) {
        return Err("这把 Key 已经存在".to_string());
    }
    let created_at = now_millis();
    let entry = ApiKeyEntry {
        id: format!("k{created_at:x}{}", entries.len()),
        name: name.trim().to_string(),
        key,
        enabled: true,
        created_at,
        allowed_providers,
        allowed_models,
    };
    entries.push(entry.clone());
    save(&entries);
    Ok(entry)
}

/// 改名 / 启停 / 改白名单（`None` = 该项不动，与原来的两字段语义一致）。
///
/// ── 白名单为什么用 `Option<Vec<String>>` 而不是 `Vec<String>` ──────
/// `PATCH /api/keys/{id}` 是部分更新：旧版前端只发 `{name}` / `{enabled}`，
/// 用 `Vec` 表达「不改」只能靠「空列表」，而那正是「清除限制」的语义 ——
/// 于是「在管理页改个名字」会把已有的限制静默清掉。`Option` 把这两件事分开：
/// `None` = 不动，`Some(vec![])` = 清成不限制，`Some(list)` = 设成 list。
pub fn update(
    id: &str,
    name: Option<&str>,
    enabled: Option<bool>,
    allowed_providers: Option<Vec<String>>,
    allowed_models: Option<Vec<String>>,
) -> Result<ApiKeyEntry, String> {
    let mut entries = list();
    let Some(entry) = entries.iter_mut().find(|entry| entry.id == id) else {
        return Err("Key 不存在".to_string());
    };
    if let Some(name) = name {
        entry.name = name.trim().to_string();
    }
    if let Some(enabled) = enabled {
        entry.enabled = enabled;
    }
    if let Some(list) = allowed_providers {
        entry.allowed_providers = list;
    }
    if let Some(list) = allowed_models {
        entry.allowed_models = list;
    }
    let updated = entry.clone();
    save(&entries);
    Ok(updated)
}

/// 删除
pub fn remove(id: &str) -> Result<(), String> {
    let mut entries = list();
    let before = entries.len();
    entries.retain(|entry| entry.id != id);
    if entries.len() == before {
        return Err("Key 不存在".to_string());
    }
    save(&entries);
    Ok(())
}
