//! 网关自身配置（`{config_dir}/config.json`）的读写与运行时快照。
//!
//! 与 Node 版 `loadConfig` / `saveConfig` / `applyConfig` 对齐（server.mjs 242-264 行）。
//! 关键点：整个 JSON 对象是**松散**的 —— 后续切片会往同一个文件里加
//! autoCheckin、lastRequestModel、desensitize 等字段。因此这里刻意**不做 struct
//! 映射**，底稿一律是 `serde_json::Map`，全量保留未知字段：谁写盘都只能改自己那几项，
//! 绝不能因为结构体定义不全而把别人的字段吃掉。
//!
//! 运行期可变：Node 版改了配置直接改 `opts` 对象，请求热路径不再读盘；
//! Rust 里用 `RwLock<Option<RuntimeConfig>>` 持有同一份「当前生效值」，
//! 于是 POST /api/config 改完 API Key 后鉴权中间件立即生效（不重启、不读文件）。
//!
//! 优先级：配置文件的值 > 环境变量 > 内置默认值。
//! 注意 `WORKBUDDY_PROXY_API_KEY` 是「启动时注入」语义 —— 它会写进内存快照，
//! 但 POST /api/config 传 null 可以把它清掉（对应 Node 版 `opts.apiKey = null`）。

use std::path::PathBuf;
use std::sync::RwLock;

use serde_json::{Map, Value};

/// 默认模型：客户端未指定模型时使用（对应 Node 版 `--default-model` 默认值）
pub const DEFAULT_MODEL: &str = "auto";
/// 计费接口默认语言（对应 Node 版 `--locale` 默认值）
pub const DEFAULT_LOCALE: &str = "zh-CN";

/// 运行期生效的配置快照。
///
/// 字段是「本切片真正会用到的」子集，其余未知字段留在 `raw` 里原样保留，
/// 写盘时一起回写。
#[derive(Clone, Debug, Default)]
pub struct RuntimeConfig {
    api_key: Option<String>,
    locale: String,
    default_model: String,
    last_request_model: Option<String>,
    /// 磁盘上那份 JSON 对象（含未知字段），写盘时的全量底稿
    raw: Map<String, Value>,
}

impl RuntimeConfig {
    /// 当前生效的 API Key；未配置返回 None
    pub fn api_key(&self) -> Option<&str> {
        self.api_key.as_deref()
    }

    /// 是否配置了 API Key（对应 Node 版 `!!opts.apiKey`）
    pub fn api_key_set(&self) -> bool {
        self.api_key.as_ref().is_some_and(|key| !key.is_empty())
    }

    /// 计费接口语言（Accept-Language）
    pub fn locale(&self) -> &str {
        &self.locale
    }

    /// 默认模型
    pub fn default_model(&self) -> &str {
        &self.default_model
    }

    /// 最近一次实际转发的模型（账号页「模型」筛选的默认值）
    pub fn last_request_model(&self) -> Option<&str> {
        self.last_request_model.as_deref()
    }

    /// 掩码后的 API Key，格式照抄 server.mjs 920 行：前 6 后 4。
    /// 短 key 会前后重叠 —— Node 的 slice(0,6)/slice(-4) 也是这样，保持一致。
    pub fn masked_api_key(&self) -> Option<String> {
        let key = self.api_key.as_ref().filter(|key| !key.is_empty())?;
        let head: String = key.chars().take(6).collect();
        let total = key.chars().count();
        let tail: String = key.chars().skip(total.saturating_sub(4)).collect();
        Some(format!("{head}...{tail}"))
    }

    /// 原始 JSON 底稿（后续切片读自定义字段用）
    pub fn raw(&self) -> &Map<String, Value> {
        &self.raw
    }
}

/// 配置目录：复用壳侧实现，保证「壳读 key」与「服务端读 key」指向同一个目录
pub fn config_dir() -> PathBuf {
    crate::gateway::config_dir()
}

/// config.json 的完整路径
pub fn config_file() -> PathBuf {
    config_dir().join("config.json")
}

/// 环境变量里的 API Key（去空白，空串当未配置）
fn env_api_key() -> Option<String> {
    std::env::var("WORKBUDDY_PROXY_API_KEY")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// 环境变量里的非空字符串
fn env_text(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// 从原始 JSON 里取非空字符串字段
fn string_field(map: &Map<String, Value>, key: &str) -> Option<String> {
    map.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// 读磁盘上的 config.json（缺失/损坏都当空对象，对应 Node 版 catch 分支）
fn read_raw() -> Map<String, Value> {
    let Ok(text) = std::fs::read_to_string(config_file()) else {
        return Map::new();
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

/// 由磁盘内容 + 环境变量构造运行期配置（对应 Node 版 applyConfig 的优先级）
fn build(raw: Map<String, Value>) -> RuntimeConfig {
    RuntimeConfig {
        // 文件里有就用文件的，否则环境变量兜底（对应 `if (config.apiKey && !opts.apiKey)`）
        api_key: string_field(&raw, "apiKey").or_else(env_api_key),
        locale: env_text("WORKBUDDY_LOCALE")
            .or_else(|| string_field(&raw, "locale"))
            .unwrap_or_else(|| DEFAULT_LOCALE.to_string()),
        default_model: env_text("WORKBUDDY_DEFAULT_MODEL").unwrap_or_else(|| DEFAULT_MODEL.to_string()),
        last_request_model: string_field(&raw, "lastRequestModel"),
        raw,
    }
}

/// 进程内全局配置快照：所有模块共用，避免每个请求都读盘
static CONFIG: RwLock<Option<RuntimeConfig>> = RwLock::new(None);

/// 初始化全局配置（启动时调用一次；重复调用会重新读盘，幂等）
pub fn init() -> RuntimeConfig {
    let snapshot = build(read_raw());
    if let Ok(mut guard) = CONFIG.write() {
        *guard = Some(snapshot.clone());
    }
    snapshot
}

/// 读取当前生效配置的克隆。
///
/// 未初始化时按「空磁盘 + 环境变量」临时构造一份，保证任何初始化顺序都不会 panic。
/// 返回克隆而不是引用：避免调用方持有读锁跨越 await 与文件 IO。
pub fn current() -> RuntimeConfig {
    if let Ok(guard) = CONFIG.read() {
        if let Some(config) = guard.as_ref() {
            return config.clone();
        }
    }
    build(Map::new())
}

/// 用一个变换函数原子地更新配置（读 → 改 → 落盘 → 回写内存）。
///
/// `mutate` 只改内存快照；落盘由本函数统一负责，避免两处都写文件。
fn update<F>(mutate: F) -> bool
where
    F: FnOnce(&mut RuntimeConfig),
{
    let mut next = current();
    mutate(&mut next);
    let saved = save_raw(&next.raw);
    if let Ok(mut guard) = CONFIG.write() {
        *guard = Some(next);
    }
    saved
}

/// 把整份原始 JSON 写盘（对应 Node 版 saveConfig）。
///
/// 目录不存在时自动创建；写失败只打控制台日志、不中断请求
/// （Node 版同样返回 false 让请求继续跑）。
pub fn save_raw(raw: &Map<String, Value>) -> bool {
    let dir = config_dir();
    if let Err(error) = std::fs::create_dir_all(&dir) {
        crate::server::logging::log("[Config]", &format!("❌ 创建配置目录失败: {error}"));
        return false;
    }
    // 缩进与 Node 版 JSON.stringify(config, null, 2) 一致，便于用户手改
    let text = match serde_json::to_string_pretty(&Value::Object(raw.clone())) {
        Ok(text) => text,
        Err(error) => {
            crate::server::logging::log("[Config]", &format!("❌ 保存失败: {error}"));
            return false;
        }
    };
    if let Err(error) = std::fs::write(config_file(), text) {
        crate::server::logging::log("[Config]", &format!("❌ 保存失败: {error}"));
        return false;
    }
    true
}

/// 设置 API Key：`None` 表示删除（对应 Node 版 `body.apiKey === null` 分支）。
/// 返回是否写盘成功；无论成功与否内存快照都已更新（本次运行立即生效）。
pub fn set_api_key(api_key: Option<String>) -> bool {
    update(|config| match api_key.clone() {
        Some(key) => {
            config.raw.insert("apiKey".to_string(), Value::String(key.clone()));
            config.api_key = Some(key);
        }
        None => {
            config.raw.remove("apiKey");
            config.api_key = None;
        }
    })
}

/// 更新语言（只接受非空字符串，对应 Node 版 `typeof body.locale === 'string' && body.locale`）
pub fn set_locale(locale: &str) -> bool {
    let locale = locale.to_string();
    update(|config| {
        config.raw.insert("locale".to_string(), Value::String(locale.clone()));
        config.locale = locale.clone();
    })
}

/// 记住本次请求用的模型（对应 Node 版 rememberRequestModel：仅在变化时写盘）
pub fn remember_request_model(model: &str) {
    let trimmed = model.trim();
    if trimmed.is_empty() || current().last_request_model.as_deref() == Some(trimmed) {
        return;
    }
    let value = trimmed.to_string();
    update(|config| {
        config
            .raw
            .insert("lastRequestModel".to_string(), Value::String(value.clone()));
        config.last_request_model = Some(value.clone());
    });
}

/// 写入 config.json 里任意字段（其他字段原样保留）。低频路径专用。
pub fn update_raw_field(key: &str, value: Value) -> bool {
    let key = key.to_string();
    update(move |config| {
        config.raw.insert(key.clone(), value.clone());
        // apiKey 属于「生效字段」，写它时要同步内存里的值
        if key == "apiKey" {
            config.api_key = string_field(&config.raw, "apiKey");
        }
    })
}
