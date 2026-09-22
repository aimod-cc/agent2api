//! JWT 声明解码（Agent2API W3-T4；移植来源 `jwt-helper.mjs`）。
//!
//! 小浣熊的登录 token 是标准 JWT（HS256 由服务端校验），**本地只解 payload、
//! 不验签**：网关需要的是 `exp`（临期判定）、`name`（缺省备注名）与十六进制
//! 用户 id（账号标识）。验签没有意义 —— 网关不做鉴权判定，token 好不好用
//! 由上游说了算。
//!
//! ── 为什么单独一个文件 ──────────────────────────────────────
//! 这些是**纯函数**（无 IO、无状态），与「读 auth.json / 调刷新接口」那两件事
//! 天然分开；拆开的直接原因是单文件行数约定（凭证那几个文件加起来已经超过
//! 800 行）。拆开之后 `credentials.rs` 只剩刷新编排，读起来是一张流程图。
//!
//! ── 绝不 panic ──────────────────────────────────────────────
//! 解析失败一律返回 None（源实现同样返回 null）——release 是 panic=abort，
//! 一条畸形 token（用户手改文件、复制时截断）不能带走整个桌面应用。

use serde_json::Value;

/// 解码 JWT 的 payload 段。
///
/// 与源实现 `decodeJwtClaims` 逐条对齐：去掉 `Bearer ` 前缀、按 `.` 切段、
/// 取第二段做 base64url → base64 的替换与填充、解析 JSON、根必须是对象。
pub fn decode_jwt_claims(token: &str) -> Option<Value> {
    let raw = strip_bearer(token);
    let mut parts = raw.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    if payload.is_empty() {
        return None;
    }
    let decoded = base64_url_to_bytes(payload)?;
    let text = String::from_utf8(decoded).ok()?;
    let claims = serde_json::from_str::<Value>(&text).ok()?;
    if claims.is_object() {
        Some(claims)
    } else {
        None
    }
}

/// token 的过期时间（毫秒）；没有 `exp` 或 token 不可解析时 None
/// （源实现 `jwtExpiryMs`）。`exp` 是**秒**，这里换算成毫秒。
pub fn jwt_expiry_ms(token: &str) -> Option<f64> {
    let claims = decode_jwt_claims(token)?;
    let exp = claims.get("exp").and_then(number_value)?;
    if exp > 0.0 {
        Some(exp * 1000.0)
    } else {
        None
    }
}

/// 按小浣熊约定提取用户 ID（源实现 `extractRaccoonUserId`）：
/// 依次尝试 `sid`（十六进制）→ `iss`（十六进制）→ `sub` / `id` / `user_id`。
///
/// ── 本机实测（2026-09）─────────────────────────────────────
/// `sid` 形如 `web65aa393e65a6b6d446a51dde22c-a6d6-48e2`：它不是纯十六进制
/// （带 `w` 前缀与 `-`），所以十六进制那条返回空、**真正命中是 `iss`**
/// （如 `6d446a` → 十进制 `7160938`，与旧账号文件里的 userId 完全一致）。
/// 两条路径都必须实现：少了任何一条都会让一批账号的 userId 变成空串，
/// 而 userId 正是小浣熊账号的标识。
pub fn extract_user_id(claims: &Value) -> String {
    for key in ["sid", "iss"] {
        if let Some(value) = claims.get(key).and_then(hex_id_to_decimal) {
            if !value.is_empty() {
                return value;
            }
        }
    }
    for key in ["sub", "id", "user_id"] {
        let text = claims.get(key).map(js_text).unwrap_or_default();
        let text = text.trim();
        if !text.is_empty() {
            return text.to_string();
        }
    }
    String::new()
}

/// 展示用名字（JWT 的 `name`；账号缺省备注名从这里来）
pub fn display_name(claims: &Value) -> String {
    claims
        .get("name")
        .map(js_text)
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// 十六进制字符串 → 十进制字符串（源实现 `hexIdToDecimalString`）。
///
/// 手写转换而不是 `u64::from_str_radix`：`iss` 可能超出 u64，源实现用 BigInt
/// 无上限；这里用「十进制结果 × 16 + 当前位」的大数运算复刻同一语义。
fn hex_id_to_decimal(value: &Value) -> Option<String> {
    let raw = match value {
        Value::String(text) => text.trim().to_string(),
        Value::Number(number) => number.to_string(),
        _ => return None,
    };
    if raw.is_empty() {
        return None;
    }
    let normalized = raw
        .strip_prefix("0x")
        .or_else(|| raw.strip_prefix("0X"))
        .unwrap_or(&raw);
    if normalized.is_empty() || !normalized.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return None;
    }
    let mut digits: Vec<u8> = vec![0];
    for ch in normalized.chars() {
        let mut carry = ch.to_digit(16)?;
        for digit in digits.iter_mut() {
            let value = (*digit as u32) * 16 + carry;
            *digit = (value % 10) as u8;
            carry = value / 10;
        }
        while carry > 0 {
            digits.push((carry % 10) as u8);
            carry /= 10;
        }
    }
    let trimmed: String = digits
        .iter()
        .rev()
        .map(|digit| char::from(b'0' + *digit))
        .collect();
    let trimmed = trimmed.trim_start_matches('0');
    Some(if trimmed.is_empty() {
        "0".to_string()
    } else {
        trimmed.to_string()
    })
}

/// base64url → 字节（源实现的 `replace(/-/g,'+').replace(/_/g,'/')` + 补 `=`）
fn base64_url_to_bytes(input: &str) -> Option<Vec<u8>> {
    let mut normalized: String = input
        .chars()
        .map(|ch| match ch {
            '-' => '+',
            '_' => '/',
            other => other,
        })
        .collect();
    let remainder = normalized.len() % 4;
    if remainder != 0 {
        normalized.push_str(&"=".repeat(4 - remainder));
    }
    base64_decode(&normalized)
}

/// 标准 base64 解码（自己实现，避免为一个纯函数引进新依赖）。
///
/// 只处理 `A-Za-z0-9+/=`，遇非法字符返回 None —— JWT payload 就是这些字符，
/// 出现别的说明这不是 base64（宁可返回 None，也不做容错式猜测）。
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let table = |ch: char| -> Option<u8> {
        match ch {
            'A'..='Z' => Some(ch as u8 - b'A'),
            'a'..='z' => Some(ch as u8 - b'a' + 26),
            '0'..='9' => Some(ch as u8 - b'0' + 52),
            '+' => Some(62),
            '/' => Some(63),
            _ => None,
        }
    };
    let mut out: Vec<u8> = Vec::with_capacity(input.len() / 4 * 3);
    let mut buffer: u32 = 0;
    let mut bits = 0u32;
    for ch in input.chars() {
        if ch == '=' {
            break;
        }
        let value = table(ch)?;
        buffer = (buffer << 6) | value as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buffer >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}

/// 去 `Bearer ` 前缀（大小写不敏感，至少一个空白），不匹配时原样返回
/// （与源实现 `String.replace(/^Bearer\s+/i, '')` 一致）
pub fn strip_bearer(value: &str) -> String {
    const PREFIX: &str = "bearer";
    let trimmed = value.trim();
    if trimmed.len() < PREFIX.len() {
        return trimmed.to_string();
    }
    let (head, rest) = trimmed.split_at(PREFIX.len());
    if !head.eq_ignore_ascii_case(PREFIX) {
        return trimmed.to_string();
    }
    if rest.trim_start().len() == rest.len() {
        return trimmed.to_string();
    }
    rest.trim_start().to_string()
}

/// JS `String(x)`（字符串原样、数字/布尔按字面量、null 给空串）
pub fn js_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        Value::Number(number) => number.to_string(),
        Value::Bool(flag) => flag.to_string(),
        other => other.to_string(),
    }
}

/// JS `Number(x)`（只给 JWT 的 exp 用；非有限值当没有）
fn number_value(value: &Value) -> Option<f64> {
    let number = match value {
        Value::Number(number) => number.as_f64()?,
        Value::String(text) => text.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    if number.is_finite() {
        Some(number)
    } else {
        None
    }
}
