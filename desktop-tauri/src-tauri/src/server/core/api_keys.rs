//! 网关 API Key 列表（config.json 的 `apiKeys` 字段）。
//!
//! 形状：`apiKeys: [{id, name, key, enabled, createdAt}]`。多把 Key 任一命中即通过；
//! 一把启用的都没有 → 不鉴权（网关只监听 127.0.0.1，与旧的「未配置 apiKey」语义一致）。
//!
//! ── 与旧字段 `apiKey` 的关系 ────────────────────────────────
//! 1.x 只有一把 Key（`apiKey` 字符串）。读侧兼容：`apiKeys` 缺失而 `apiKey` 存在时，
//! 把它当成一条 id 为 `legacy` 的记录展示 / 校验；写侧一旦动过列表就落成 `apiKeys`
//! 并删掉 `apiKey`，从此只有一份真相。环境变量 `WORKBUDDY_PROXY_API_KEY` 仍然
//! 算一把额外的启用 Key（启动注入语义不变）。

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
        })
    }

    fn to_value(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "key": self.key,
            "enabled": self.enabled,
            "createdAt": self.created_at,
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
        })
    }
}

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
pub fn add(name: &str, key: Option<&str>) -> Result<ApiKeyEntry, String> {
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
    };
    entries.push(entry.clone());
    save(&entries);
    Ok(entry)
}

/// 改名 / 启停
pub fn update(id: &str, name: Option<&str>, enabled: Option<bool>) -> Result<ApiKeyEntry, String> {
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
