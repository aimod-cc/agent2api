//! JS 语义工具函数（账号存储三兄弟共用）。
//!
//! Node 版把账号记录当普通 JS 对象读写，所以大量取值逻辑依赖 JS 的宽松语义
//! （`x || y`、`x ?? y`、`Number(x)`、`String(x)`、`JSON.stringify` 比较…）。
//! 这些函数的唯一目标就是**在 Rust 里逐条复刻那些语义**，让行为与 Node 版一致：
//! 宁可容忍脏数据（类型不对就回落），也不要报错或丢字段 —— 用户手工编辑
//! accounts.json 是明确支持的用法。
//!
//! 从 store.rs 拆出（单文件行数约定）。三处使用：store.rs（读写与公开形态）、
//! store_crud.rs（增删改查）、account_transfer.rs（导入导出归一）。

use serde_json::{Map, Value};

use crate::server::core::account_store::state::json_number;

/// Node 的 `jwt.slice(-4)`：取末 4 个字符（不足 4 个则整串）。
/// 供导入归一复用（tokenTail 缺省时按最终 accessToken 重算）。
pub(crate) fn token_tail_of(token: &str) -> String {
    token_tail(token)
}

/// Node 的 `text.slice(0, max)`：按字符截断（供导入归一复用）
pub(crate) fn truncate_text(text: &str, max: usize) -> String {
    truncate_chars(text, max)
}

/// Node 的 `jwt.slice(-4)`：取末 4 个字符（不足 4 个则整串）
pub(super) fn token_tail(token: &str) -> String {
    let chars: Vec<char> = token.chars().collect();
    let start = chars.len().saturating_sub(4);
    chars[start..].iter().collect()
}

/// Node 的 `text.slice(0, max)`：按字符截断
pub(super) fn truncate_chars(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// JS 真值判定（用于 `x || default` 与 `Boolean(x)` 的语义）：
/// null / false / 0 / "" 为假，其余（含空数组、空对象）为真。
///
/// 对账号存储之外的同层模块（core::routing 等）可见：它们要复刻同一套
/// JS 语义，复述一个几乎相同的函数不如共用一份实现。
pub(crate) fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().map(|item| item != 0.0).unwrap_or(false),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// JS `String(x)`（只对真值调用）：字符串原样、数字/布尔按字面量、
/// 其余（数组/对象）用 JSON 文本近似 —— 这些取值在实际数据里不会出现，
/// 但不能因此 panic 或丢失字段。
pub(super) fn js_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        Value::Bool(flag) => flag.to_string(),
        other => other.to_string(),
    }
}

/// JS `x || fallback`：假值（null/false/0/""）取 fallback
pub(super) fn value_or(value: Option<&Value>, fallback: Value) -> Value {
    match value {
        Some(inner) if js_truthy(inner) => inner.clone(),
        _ => fallback,
    }
}

/// JS `x ?? fallback`：只有 null / 缺失才取 fallback（空串是有效值）
pub(super) fn value_or_nullish(value: Option<&Value>, fallback: Value) -> Value {
    match value {
        Some(Value::Null) | None => fallback,
        Some(inner) => inner.clone(),
    }
}

/// 设置里的值 → 非空字符串（对应 `existing?.nickname` 这类「可能是任意类型」的取值）。
/// 返回 None 表示「未设置」，调用方据此走下一级兜底。
pub(super) fn optional_text(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(text)) if !text.is_empty() => Some(text.clone()),
        Some(other) if js_truthy(other) => Some(js_string(other)),
        _ => None,
    }
}

/// 对应 Node 的 `Number(x) || fallback`（NaN / 0 / 缺失都取 fallback）。
///
/// 已是 JSON 数字时**原样返回**（保住整数形态，见 `json_number` 的说明）；
/// 字符串则按 JS 的 `Number()` 解析后写成整数或浮点。
pub(super) fn number_or(value: Option<&Value>, fallback: Value) -> Value {
    match value {
        Some(Value::Number(number)) => {
            let parsed = number.as_f64().unwrap_or(0.0);
            if parsed.is_finite() && parsed != 0.0 {
                value.cloned().unwrap_or(fallback)
            } else {
                fallback
            }
        }
        Some(Value::String(text)) => match text.trim().parse::<f64>() {
            Ok(number) if number.is_finite() && number != 0.0 => json_number(number),
            _ => fallback,
        },
        _ => fallback,
    }
}

/// 去 `Bearer ` 前缀（大小写不敏感，至少一个空白），不匹配时原样返回
/// —— 与 Node 的 `String.replace(/^Bearer\s+/i, '')` 一致。
pub(super) fn strip_bearer_prefix(value: &str) -> String {
    const PREFIX: &str = "bearer";
    if value.len() < PREFIX.len() {
        return value.to_string();
    }
    let (head, rest) = value.split_at(PREFIX.len());
    if !head.eq_ignore_ascii_case(PREFIX) {
        return value.to_string();
    }
    if rest.trim_start().len() == rest.len() {
        return value.to_string();
    }
    rest.trim_start().to_string()
}

/// 对照 Node 版 `pickToken`：按候选键名依次取第一个非空字符串，去掉 Bearer 前缀
pub(super) fn pick_token(payload: &Map<String, Value>, keys: &[&str]) -> String {
    for key in keys {
        if let Some(Value::String(text)) = payload.get(*key) {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return strip_bearer_prefix(trimmed);
            }
        }
    }
    String::new()
}

/// 取对象成员；非对象（含数组）一律当空表 —— 对 `payload.auth` / `payload.account`
/// 这类访问而言，与 Node 读出 undefined 的结果等价。
pub(super) fn object_or_empty(value: Option<&Value>) -> Map<String, Value> {
    match value {
        Some(Value::Object(map)) => map.clone(),
        _ => Map::new(),
    }
}
