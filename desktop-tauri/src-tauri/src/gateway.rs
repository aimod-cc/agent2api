//! 管理 API 的 HTTP 客户端。
//!
//! 职责：
//!   - 从配置目录读 API Key 并自动带上（网关开启鉴权时否则全是 401）
//!   - 统一解包 `{ success, data }` 信封，只把 data 交给前端
//!   - 超时保护：签到、批量查询这类接口本身较慢，给足 60 秒
//!
//! 只访问 127.0.0.1 上的明文 HTTP，不需要 TLS。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;

use serde_json::Value;

/// 网关默认端口（与 server.mjs 的默认值一致）
pub const DEFAULT_PORT: u16 = 3065;
pub const REQUEST_TIMEOUT_MS: u64 = 60_000;

/// 本次运行实际使用的端口。
///
/// 端口在进程启动时定死（服务端 bind 之后就改不了），但「用户改了设置」这件事
/// 发生在运行期 —— 保存新端口后要重启进程才生效。这个原子量让**重启之前**的
/// 读取（界面查状态、管理 API 客户端拼 URL）拿到的仍是当前真正在监听的端口，
/// 不会因为设置里已经写上新值就指向一个没人监听的端口。
static ACTIVE_PORT: AtomicU16 = AtomicU16::new(0);

/// 端口选择优先级：环境变量 > 配置文件 > 默认值。
///
/// 环境变量优先是历史行为（1.x 起就支持用它并行跑第二实例做测试），
/// 且它只影响本次运行、不写盘，语义清晰。
///
/// **配置文件里的端口**（`desktop-settings.json` 的 `proxyPort`）是 2.0.4 新增的：
/// 端口被系统保留段挡住时，改环境变量对普通用户来说门槛太高（要改启动方式），
/// 而这条故障又只能靠换端口解决 —— 界面上的「更换端口」写的就是这个字段。
pub fn proxy_port() -> u16 {
    let cached = ACTIVE_PORT.load(Ordering::Relaxed);
    if cached != 0 {
        return cached;
    }
    resolve_port()
}

/// 按优先级解析端口并缓存进 `ACTIVE_PORT`（只在首次调用时真正解析）。
fn resolve_port() -> u16 {
    let port = env_port("AGENT2API_PROXY_PORT")
        .or_else(|| env_port("WORKBUDDY_PROXY_PORT")) // 旧名兼容读（1.x 起沿用）
        .or_else(|| configured_port())
        .unwrap_or(DEFAULT_PORT);
    ACTIVE_PORT.store(port, Ordering::Relaxed);
    port
}

/// 读设置文件里的端口；未设置 / 非法一律当未设置（回落到下一级）。
///
/// 用 `settings::load()` 而不是自己读文件：设置文件的路径与容错口径
/// （缺失、损坏、权限不足都回落默认值）只该有一处实现。
fn configured_port() -> Option<u16> {
    let port = crate::settings::load().proxy_port;
    if port > 0 {
        Some(port)
    } else {
        None
    }
}

/// 环境变量是否显式指定了端口（显式指定时界面不该再提供「更换端口」——
/// 改了设置也不会生效，只会让用户白忙一场）
pub fn port_from_env() -> Option<u16> {
    env_port("AGENT2API_PROXY_PORT").or_else(|| env_port("WORKBUDDY_PROXY_PORT"))
}

/// 读环境变量里的端口：未设置 / 非数字 / 0 一律当未设置（回落到下一级）
fn env_port(name: &str) -> Option<u16> {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<u16>().ok())
        .filter(|port| *port > 0)
}

/// 管理 API 的响应信封：`{ success, data, error }`。
/// 解析失败时保留原文，便于把上游的真实报错透给用户。
fn unwrap_envelope(text: &str, status: u16) -> Result<Value, String> {
    let payload: Value = match serde_json::from_str(text) {
        Ok(value) => value,
        // 非 JSON：直接把原文当错误说明（例如反向代理返回的 HTML 错误页）
        Err(_) => {
            if (200..300).contains(&status) {
                return Err(if text.trim().is_empty() {
                    "本地代理返回了空响应".to_string()
                } else {
                    text.trim().to_string()
                });
            }
            return Err(text.trim().to_string());
        }
    };

    let ok = (200..300).contains(&status);
    let success = payload.get("success").and_then(Value::as_bool);
    if !ok || success == Some(false) {
        let detail = payload
            .get("error")
            .or_else(|| payload.get("message"))
            .map(describe_error)
            .unwrap_or_else(|| format!("HTTP {status}"));
        return Err(detail);
    }
    Ok(match payload.get("data") {
        Some(data) => data.clone(),
        None => payload,
    })
}

/// 错误字段可能是字符串，也可能是 `{ message }` / `{ error: { message } }`
fn describe_error(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        return text.to_string();
    }
    if let Some(text) = value.get("message").and_then(Value::as_str) {
        return text.to_string();
    }
    if let Some(text) = value.get("msg").and_then(Value::as_str) {
        return text.to_string();
    }
    value.to_string()
}

/// 配置目录：与后端共用 `~/.agent2api`（可用环境变量覆盖）。
///
/// **这是配置目录的唯一事实来源**：`server::config::config_dir()` 直接转发到
/// 这里，所以「壳读 key」与「服务端读写数据」永远指向同一个目录。改名时
/// 只需改本函数与下面 `default_config_dir` 的后缀，别处不要再写目录名字面量
/// —— 唯一的例外是 `server::config` 里的 `LEGACY_DIR_NAME`，它要引用 1.x 的
/// 旧目录名做一次性拷贝（迁移实现在 `server::config_migration`）。
///
/// 环境变量：`AGENT2API_PROXY_HOME` 优先，旧名 `WORKBUDDY_PROXY_HOME` 兼容读
/// （1.x 的启动脚本/快捷方式里可能还留着旧名）。
pub fn config_dir() -> PathBuf {
    env_path("AGENT2API_PROXY_HOME")
        .or_else(|| env_path("WORKBUDDY_PROXY_HOME")) // 旧名兼容读
        .unwrap_or_else(default_config_dir)
}

/// 读环境变量里的目录覆盖：未设置 / 全空白一律当未设置
fn env_path(name: &str) -> Option<PathBuf> {
    let custom = std::env::var(name).ok()?;
    let trimmed = custom.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(PathBuf::from(trimmed))
    }
}

/// 默认配置目录：`{用户主目录}/.agent2api`
fn default_config_dir() -> PathBuf {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".agent2api")
}

/// 读取本地 API Key。仅在本机内存里取；未配置时返回 None。
/// 先取 `apiKeys` 里第一把启用的 Key，没有该列表再退回旧的单 Key 字段 `apiKey`。
///
/// ── 为什么不再读文件（本切片改掉的一处真实开销）──────────────
/// 改造前这里是 `fs::read_to_string(config.json)` —— 而本函数在**每一个**管理
/// API 请求上都会被调用（`request_builder` 里加 `X-API-Key` 头），于是每次
/// 点界面都是一次「打开文件 + 读全文 + 解析 JSON」。配置进了统一库之后，
/// 再照原样读库会更糟：那要开连接、抢那把全局连接锁（与转发记账、日志写入
/// 互斥），把管理 API 的每次请求都排到数据库串行队列里。
/// 改为走**进程内配置快照**（`RuntimeConfig::active_api_keys`，解析实现是
/// `core::api_keys::active_keys_from`）：与鉴权中间件、`keys_api` 读的是
/// **同一份**值（`config::current()` 的克隆），因此「刚保存的新 Key 下一个
/// 请求就生效」这条性质不变，且壳侧不再依赖 `config.json` 文件存在。
///
/// ── 未初始化时的行为 ────────────────────────────────────────
/// `config::current()` 在快照未装入时按「空配置 + 环境变量」临时构造一份
/// （见它的说明），所以启动极早期（`config::init` 之前）调用本函数不会 panic，
/// 只是可能拿不到 Key —— 而那段窗口里还没有管理 API 请求要发。
fn read_api_key() -> Option<String> {
    crate::server::config::current()
        .active_api_keys()
        .into_iter()
        .next()
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(Duration::from_millis(REQUEST_TIMEOUT_MS))
        .build()
        .map_err(|error| format!("创建 HTTP 客户端失败: {error}"))
}

fn request_builder(method: &str, path: &str, body: Option<&Value>) -> Result<reqwest::RequestBuilder, String> {
    let url = format!("http://127.0.0.1:{}{path}", proxy_port());
    let base = client()?;
    let mut builder = match method.to_uppercase().as_str() {
        "GET" => base.get(&url),
        "POST" => base.post(&url),
        "PUT" => base.put(&url),
        "PATCH" => base.patch(&url),
        "DELETE" => base.delete(&url),
        other => return Err(format!("不支持的请求方法: {other}")),
    };
    builder = builder.header("Accept", "application/json");
    if let Some(key) = read_api_key() {
        builder = builder.header("X-API-Key", key);
    }
    // 与原实现一致：POST/PUT/PATCH 即使无参数也发一个空对象，
    // 后端部分路由按「有 body」判定，省略会让它们走错分支
    if let Some(payload) = body {
        builder = builder
            .header("Content-Type", "application/json")
            .body(serde_json::to_string(payload).map_err(|e| format!("请求体序列化失败: {e}"))?);
    } else if matches!(method.to_uppercase().as_str(), "POST" | "PUT" | "PATCH") {
        builder = builder
            .header("Content-Type", "application/json")
            .body("{}");
    }
    Ok(builder)
}

/// 发一次管理请求，返回解包后的 data
pub async fn call(method: &str, path: &str, body: Option<&Value>) -> Result<Value, String> {
    let builder = request_builder(method, path, body)?;
    let response = builder
        .send()
        .await
        .map_err(|error| describe_transport_error(&error))?;
    let status = response.status().as_u16();
    let text = response
        .text()
        .await
        .map_err(|error| format!("读取响应失败: {error}"))?;
    unwrap_envelope(&text, status)
}

/// 取原始文本（导出日志用；call 会按 JSON 解析，不适合）
pub async fn call_text(method: &str, path: &str) -> Result<String, String> {
    let builder = request_builder(method, path, None)?;
    let response = builder
        .send()
        .await
        .map_err(|error| describe_transport_error(&error))?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status().as_u16()));
    }
    response
        .text()
        .await
        .map_err(|error| format!("读取响应失败: {error}"))
}

fn describe_transport_error(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "连接本地代理超时".to_string()
    } else {
        format!("无法连接本地代理（127.0.0.1:{}）: {error}", proxy_port())
    }
}
