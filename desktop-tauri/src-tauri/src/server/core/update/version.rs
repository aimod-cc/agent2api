//! 软件更新：版本比较、域名白名单、资产挑选、文件名安全化（纯函数）。
//!
//! 对照 Node 版 src/workbuddy-update.mjs 的版本/校验部分逐条移植。
//! 与 `mod.rs`（网络与状态机）拆开，是因为这里全是**纯函数**：
//! 改动原因不同（这里随「版本号与资产命名约定」变，mod.rs 随「HTTP 与文件」变），
//! 也方便单测式排查时直接调用。

use serde_json::Value;

/// 默认仓库（本项目的上游）；fork 后可自行覆盖（Node 版 DEFAULT_REPO）
pub const DEFAULT_REPO: &str = "aimod-cc/agent2api";

/// GitHub API 根（Node 版 GITHUB_API）
pub const GITHUB_API: &str = "https://api.github.com";

/// 允许下载的域名：Release 资产实际会落在 github.com 或 *.githubusercontent.com
/// （Node 版 ALLOWED_HOSTS）
pub const ALLOWED_HOSTS: &[&str] = &[
    "github.com",
    "www.github.com",
    "objects.githubusercontent.com",
    "codeload.github.com",
];

/// 允许下载的域名后缀（Node 版 ALLOWED_HOST_SUFFIX）
pub const ALLOWED_HOST_SUFFIX: &str = ".githubusercontent.com";

/// 安装包体积上限：只用于拦明显异常的响应，避免把磁盘写满（Node 版 600MB）
pub const MAX_INSTALLER_BYTES: u64 = 600 * 1024 * 1024;

/// 更新模块的错误（对应 Node 版 UpdateError，默认 502）。
///
/// `status_code` 会原样变成 HTTP 状态码（400 参数/域名非法、502 网络与 GitHub 侧）。
#[derive(Clone, Debug)]
pub struct UpdateError {
    pub message: String,
    pub status_code: i32,
}

impl UpdateError {
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into(), status_code: 502 }
    }

    pub fn with_status(status_code: i32, message: impl Into<String>) -> Self {
        Self { message: message.into(), status_code }
    }

    /// 转成统一的网关错误。更新路由的失败 body 走 OpenAI 风格
    /// （这些写法的错误在 Node 里都被最外层大 try 的 errorPayload 兜住）
    pub fn to_gateway_error(&self) -> crate::server::errors::GatewayError {
        crate::server::errors::GatewayError::with_status(self.status_code, self.message.clone())
    }
}

impl std::fmt::Display for UpdateError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.message)
    }
}

// ─── 版本比较 ───────────────────────────────────────────────

/// 解析版本号为三段数字。
///
/// 对应 Node 版 parseVersion：先去首尾空白、剥掉可选的 `v` 前缀，
/// 再要求以 `d+.d+.d+` **开头**（后面的预发布/构建元数据不管）。
/// 非语义化版本返回 None（此时不做「有新版本」判断，只展示原文）。
pub fn parse_version(text: &str) -> Option<[u64; 3]> {
    let trimmed = text.trim();
    let body = trimmed
        .strip_prefix('v')
        .or_else(|| trimmed.strip_prefix('V'))
        .unwrap_or(trimmed);
    // `^(\d+)\.(\d+)\.(\d+)`：只要求前缀匹配，多余部分（-beta.1 等）忽略
    let mut parts = [0u64; 3];
    let mut rest = body;
    for (index, slot) in parts.iter_mut().enumerate() {
        let digits_end = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits_end == 0 {
            return None;
        }
        // 只用 ASCII 数字：JS 的 \d 在非 u 标志下就是 [0-9]
        *slot = rest[..digits_end].parse().ok()?;
        rest = &rest[digits_end..];
        if index < 2 {
            rest = rest.strip_prefix('.')?;
        }
    }
    Some(parts)
}

/// 语义化比较：a > b 返回 1，相等 0，a < b 返回 -1；无法解析时 None。
pub fn compare_versions(a: &str, b: &str) -> Option<i32> {
    let left = parse_version(a)?;
    let right = parse_version(b)?;
    for index in 0..3 {
        if left[index] > right[index] {
            return Some(1);
        }
        if left[index] < right[index] {
            return Some(-1);
        }
    }
    Some(0)
}

// ─── URL 与资产 ─────────────────────────────────────────────

/// 域名白名单校验：只允许 https 且落在 GitHub 资产域名下（对应 assertDownloadable）。
///
/// Node 用 `new URL(url)` 解析：非法 URL 抛 400「下载地址不是合法 URL」，
/// 非 https 抛 400「下载地址必须是 https」，域名不在列表抛 400
/// 「下载地址域名不在允许列表内: <host>」。
pub fn assert_downloadable(url: &str) -> Result<url::Url, UpdateError> {
    let parsed = url::Url::parse(url)
        .map_err(|_| UpdateError::with_status(400, "下载地址不是合法 URL"))?;
    if parsed.scheme() != "https" {
        return Err(UpdateError::with_status(400, "下载地址必须是 https"));
    }
    let host = parsed.host_str().unwrap_or("").to_lowercase();
    let allowed = ALLOWED_HOSTS.contains(&host.as_str()) || host.ends_with(ALLOWED_HOST_SUFFIX);
    if !allowed {
        return Err(UpdateError::with_status(
            400,
            format!("下载地址域名不在允许列表内: {host}"),
        ));
    }
    Ok(parsed)
}

/// 从 Release 资产里挑安装包：优先名字带 setup 的 exe，其次任意 exe
/// （对应 Node 版 pickInstaller）。没有 exe 返回 None。
pub fn pick_installer(assets: Option<&Value>) -> Option<Value> {
    let list = assets.and_then(Value::as_array)?;
    let exes: Vec<&Value> = list
        .iter()
        .filter(|item| {
            // `String(item?.name || '')` 后判 /\.exe$/i
            item.get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_lowercase()
                .ends_with(".exe")
        })
        .collect();
    if exes.is_empty() {
        return None;
    }
    // 首选：名字含 setup 或 install（Node 的 /setup|install/i）
    let chosen = exes
        .iter()
        .find(|item| {
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_lowercase();
            name.contains("setup") || name.contains("install")
        })
        .copied()
        .unwrap_or(exes[0]);
    Some(serde_json::json!({
        "name": chosen.get("name").and_then(Value::as_str).unwrap_or(""),
        // `Number(installer.size) || 0`：非数字/NaN/0 都得 0
        "size": chosen.get("size").and_then(Value::as_u64).unwrap_or(0),
        "url": chosen.get("browser_download_url").and_then(Value::as_str).unwrap_or(""),
    }))
}

/// JS `String.prototype.trim` 的空白集合 = White_Space ∪ {U+FEFF}。
///
/// 与 `core::desensitize` 里那份实现规则相同，但那个函数是模块内可见
/// （`pub(super)`），跨模块拿不到；这里为 safe_file_name 的 `.trim()` 复制一份。
/// 规则只有一行，重复的代价小于把它提升成全局公共设施。
fn js_trim(value: &str) -> &str {
    value.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
}

/// 文件名安全化：只取基名，去掉路径分隔符与可疑字符（对应 Node 版 safeFileName）。
///
/// Node 是 `basename(name).replace(/[\\/:*?"<>|]/g, '_').trim()`，
/// 空结果回落到 'workbuddy-update.exe'。注意 `basename` 在 Windows 上
/// 同时识别 `\` 与 `/`，与 Node 的 path.basename（POSIX 语义，只认 `/`）
/// 有差别 —— 但紧接着的替换会把残留的 `\` 变成 `_`，最终文件名同样不含路径分隔符，
/// 安全性一致（这里是**有意**用 Windows 语义，避免把 `..\evil.exe` 原样留下）。
pub fn safe_file_name(name: &str) -> String {
    let base = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .to_string();
    let cleaned: String = base
        .chars()
        .map(|ch| {
            if matches!(ch, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                ch
            }
        })
        .collect();
    let cleaned = js_trim(&cleaned);
    if cleaned.is_empty() {
        "workbuddy-update.exe".to_string()
    } else {
        cleaned.to_string()
    }
}
