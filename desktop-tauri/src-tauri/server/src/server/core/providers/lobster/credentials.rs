//! LobsterAI 凭证处理（feat/lobster-provider PR-1）。
//!
//! ── 本文件负责什么（PR-1 范围）─────────────────────────────────
//!   - **凭证来源**：桌面端实时登录态
//!     `~/Library/Application Support/LobsterAI/lobsterai.sqlite` 的
//!     `kv.auth_tokens` 行（accessToken + refreshToken）。
//!   - **状态摘要**：`credentials_status()` 给 `/api/accounts/usage` 等高频
//!     轮询路径；不出错，只返 None 表示缺失。
//!   - **桌面端导入摘要**：`desktop_summary()` 给 UI 显示（路径、token 末尾指纹、
//!     JWT exp）；出错抛 String 给前端提示用户去 LobsterAI App 登录。
//!
//! ── PR-2 才接的事（本文件暂留位）──────────────────────────────
//!   - 调 `/api/auth/refresh` 主动刷新；
//!   - mtime + TTL 缓存读盘（PR-1 不缓存，每次读盘；桌面端导入是低频操作够用）；
//!   - JWT 余量不足时自动刷新。
//!
//! ── SQLite 直读实现要点 ─────────────────────────────────────
//!   - URI mode=ro：绝不写库；同目录 WAL 文件同用户可读。
//!   - 路径含空格（`Application Support`），URI 自动处理。
//!   - 业务超时 3 秒：libsqlite 自身有 busy handler，本文件不叠加。
//!
//! ── panic=abort ────────────────────────────────────────────
//! 本文件在对话链路上，绝不 unwrap/expect/panic：取值走 Option 链与
//! `unwrap_or`，SQL 失败一律转成 String/GatewayError。

use std::path::PathBuf;
use std::time::SystemTime;

use rusqlite::OpenFlags;
use serde_json::{json, Value};

use crate::server::errors::GatewayError;

/// 桌面端实时账号的固定 id
pub const DESKTOP_ACCOUNT_ID: &str = "lobster-desktop";

fn home_dir() -> Option<PathBuf> {
    // 与同仓其他 provider 一致：std::env::home_dir 已 deprecated，
    // 这里直接读取 $HOME（macOS 上必存在），落到 fallback 时返回 None。
    std::env::var_os("HOME").map(PathBuf::from)
}

/// SQLite 文件路径（macOS 上）
pub fn desktop_db_file() -> Option<PathBuf> {
    let home = home_dir()?;
    Some(home.join("Library/Application Support/LobsterAI/lobsterai.sqlite"))
}

/// 只读打开 sqlite 取 kv.auth_tokens 行；失败返回 None（PR-1 不缓存，每次读盘）
pub fn read_auth_tokens_from_db() -> Option<Value> {
    let path = desktop_db_file()?;
    if !path.exists() {
        return None;
    }
    let conn = rusqlite::Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .ok()?;
    let value: String = conn
        .query_row(
            "SELECT value FROM kv WHERE key='auth_tokens'",
            [],
            |row| row.get(0),
        )
        .ok()?;
    serde_json::from_str(&value).ok()
}

/// 解析出 (accessToken, refreshToken)；都非空才算成功
pub fn extract_credentials(tokens: &Value) -> Option<(String, String)> {
    let access = tokens
        .get("accessToken")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?
        .to_string();
    let refresh = tokens
        .get("refreshToken")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?
        .to_string();
    Some((access, refresh))
}

/// JWT 解码（不验签，只读 exp）。失败 → None。
pub fn jwt_exp_seconds(token: &str) -> Option<i64> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() < 2 {
        return None;
    }
    let payload_b64 = parts[1];
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    URL_SAFE_NO_PAD
        .decode(payload_b64)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|v| v.get("exp").and_then(|e| e.as_i64()))
}

/// 账号状态摘要（高频轮询路径用；不抛异常）
pub fn credentials_status() -> Value {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let Some(tokens) = read_auth_tokens_from_db() else {
        return json!({
            "available": false,
            "message": "LobsterAI 登录态缺失：请安装并登录 LobsterAI App",
        });
    };
    let Some((access, _refresh)) = extract_credentials(&tokens) else {
        return json!({
            "available": false,
            "message": "LobsterAI 凭证字段缺失",
        });
    };
    let jwt_exp = jwt_exp_seconds(&access);
    let (jwt_expired, jwt_seconds_left) = match jwt_exp {
        Some(exp) => (Some(exp <= now as i64), Some(exp - now as i64)),
        None => (None, None),
    };
    json!({
        "available": true,
        "jwt_expires_at": jwt_exp,
        "jwt_seconds_left": jwt_seconds_left,
        "jwt_expired": jwt_expired,
        "message": "",
    })
}

/// 桌面端文件存在性 + 凭证字段摘要（用于 import 流；失败抛真实错误给 UI）
pub fn desktop_summary() -> Result<Value, String> {
    let path = desktop_db_file()
        .ok_or_else(|| "无法定位 LobsterAI 数据目录".to_string())?;
    if !path.exists() {
        return Err(format!("LobsterAI sqlite 不存在: {}", path.display()));
    }
    let tokens = read_auth_tokens_from_db()
        .ok_or_else(|| "LobsterAI 凭证缺失：未找到本机登录态".to_string())?;
    let (access, refresh) = extract_credentials(&tokens)
        .ok_or_else(|| "LobsterAI 凭证字段缺失".to_string())?;
    let token_tail = |s: &str| {
        let bytes = s.as_bytes();
        let start = bytes.len().saturating_sub(4);
        String::from_utf8_lossy(&bytes[start..]).into_owned()
    };
    Ok(json!({
        "path": path.display().to_string(),
        "accessToken_tail": token_tail(&access),
        "refreshToken_tail": token_tail(&refresh),
        "jwt_exp": jwt_exp_seconds(&access),
    }))
}

/// 取得当前凭证（手动添加账号时复用桌面端 sqlite 内容作模板）
pub fn snapshot_for_desktop() -> Result<(String, String), GatewayError> {
    let tokens = read_auth_tokens_from_db().ok_or_else(|| {
        GatewayError::with_status(
            401,
            "LobsterAI 凭证缺失：未找到本机登录态，请先在 LobsterAI App 登录",
        )
    })?;
    extract_credentials(&tokens).ok_or_else(|| {
        GatewayError::with_status(401, "LobsterAI 凭证字段缺失")
    })
}