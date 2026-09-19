//! 管理 API 的 HTTP 客户端。
//!
//! 职责：
//!   - 从配置目录读 API Key 并自动带上（网关开启鉴权时否则全是 401）
//!   - 统一解包 `{ success, data }` 信封，只把 data 交给前端
//!   - 超时保护：签到、批量查询这类接口本身较慢，给足 60 秒
//!
//! 只访问 127.0.0.1 上的明文 HTTP，不需要 TLS。

use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;

/// 网关默认端口（与 server.mjs 的默认值一致）
pub const DEFAULT_PORT: u16 = 3065;
pub const REQUEST_TIMEOUT_MS: u64 = 60_000;

/// 实际使用端口：允许用 AGENT2API_PROXY_PORT 覆盖
/// （旧名 WORKBUDDY_PROXY_PORT 仍可读，新名优先）。
/// 默认 3065；端口被别的程序占用时可用它改端口并行运行。
pub fn proxy_port() -> u16 {
    env_port("AGENT2API_PROXY_PORT")
        .or_else(|| env_port("WORKBUDDY_PROXY_PORT")) // 旧名兼容读（1.x 起沿用）
        .unwrap_or(DEFAULT_PORT)
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

/// 读取本地 API Key。仅读文件、不出本机；未配置时返回 None。
/// 先取 `apiKeys` 里第一把启用的 Key，没有该列表再退回旧的单 Key 字段 `apiKey`。
fn read_api_key() -> Option<String> {
    let text = std::fs::read_to_string(config_dir().join("config.json")).ok()?;
    let json: Value = serde_json::from_str(&text).ok()?;
    if let Some(list) = json.get("apiKeys").and_then(Value::as_array) {
        return list
            .iter()
            .filter(|item| !matches!(item.get("enabled"), Some(Value::Bool(false))))
            .filter_map(|item| item.get("key").and_then(Value::as_str))
            .map(str::trim)
            .find(|key| !key.is_empty())
            .map(str::to_string);
    }
    let key = json.get("apiKey")?.as_str()?.trim().to_string();
    if key.is_empty() {
        None
    } else {
        Some(key)
    }
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
