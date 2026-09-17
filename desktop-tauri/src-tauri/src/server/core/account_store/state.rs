//! 账号文件的磁盘形态与记录访问器。
//!
//! ── 为什么记录用 `serde_json::Map` 而不是 struct ──
//! Node 版把每条账号记录当普通对象读写（`record.anything`），字段集随版本演进，
//! 而我们的硬约束是「用户升级绝不能丢账号里的任何字段」。用 struct 有两处风险：
//!   1. 定义不全 → 反序列化时未知字段被丢弃，写回就永久丢了；
//!   2. 类型太严 → 手工编辑出的 `enabled: "false"` 这类脏值会让整条记录读不出来。
//! 因此这里保留 JSON 原样（`Map<String, Value>`），只在上层提供**容错的取值器**：
//! 值类型不对时按 Node 的宽松语义回落（`''`/`0`/`null`/默认启用），而不是报错。
//!
//! 代价是这里没有编译期的字段名检查 —— 所以所有字段名都在本文件的访问器里写一次，
//! 上层一律走访问器，不要再散落字符串字面量。

use serde_json::{Map, Value};

use crate::server::core::account_store::priority::normalize_priority_value;

/// 整个 accounts.json 的内存形态。
///
/// 顶层只认识 `accounts`（Node 版 `load()` 同样只解析这一个键），
/// 其余顶层字段（旧版本的 currentAccountId 之类）刻意不解析 —— Node 版读盘时
/// 就已经丢弃它们，「下次保存即自然消失」是既定行为。
#[derive(Clone, Debug, Default)]
pub struct AccountState {
    pub accounts: Vec<StoredAccount>,
}

/// 一条账号记录：原样持有的 JSON 对象 + 容错访问器。
#[derive(Clone, Debug)]
pub struct StoredAccount {
    fields: Map<String, Value>,
}

/// 容错取字符串：非字符串一律当空串（对应 JS 里 `String(x || '')` 的常见用法）
fn as_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        _ => String::new(),
    }
}

/// 容错取可选字符串：非字符串/空串都当「未设置」
fn as_optional_text(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(text)) if !text.is_empty() => Some(text.clone()),
        _ => None,
    }
}

impl StoredAccount {
    /// 用一份字段表构造（调用方保证 id 已就位）
    pub fn from_map(fields: Map<String, Value>) -> Self {
        Self { fields }
    }

    /// 从磁盘条目构造；缺 id（或 id 非字符串）时返回 None
    /// （对应 Node 版 `filter(item => typeof item.id === 'string')`）。
    pub fn from_value(value: Value) -> Option<Self> {
        let Value::Object(fields) = value else {
            return None;
        };
        match fields.get("id") {
            Some(Value::String(id)) if !id.is_empty() => Some(Self { fields }),
            _ => None,
        }
    }

    /// 序列化回 JSON（保留全部未知字段）
    pub fn to_value(&self) -> Value {
        Value::Object(self.fields.clone())
    }

    /// 直接借用底层字段表（导出、导入合并等需要遍历键的地方用）
    pub fn fields(&self) -> &Map<String, Value> {
        &self.fields
    }

    /// 可变借用字段表（导入合并按 key 覆写）
    pub fn fields_mut(&mut self) -> &mut Map<String, Value> {
        &mut self.fields
    }

    // ── 身份 ────────────────────────────────────────────────

    pub fn id(&self) -> &str {
        self.fields.get("id").and_then(Value::as_str).unwrap_or("")
    }

    pub fn name(&self) -> String {
        as_text(self.fields.get("name"))
    }

    pub fn uid(&self) -> String {
        as_text(self.fields.get("uid"))
    }

    pub fn nickname(&self) -> String {
        as_text(self.fields.get("nickname"))
    }

    /// 账号类型：缺省视为个人版（对应 Node 版 `type || 'personal'`）
    pub fn account_type(&self) -> String {
        let value = as_text(self.fields.get("type"));
        if value.is_empty() { "personal".to_string() } else { value }
    }

    pub fn enterprise_id(&self) -> String {
        as_text(self.fields.get("enterpriseId"))
    }

    pub fn enterprise_name(&self) -> String {
        as_text(self.fields.get("enterpriseName"))
    }

    // ── 凭证 ────────────────────────────────────────────────

    pub fn access_token(&self) -> String {
        as_text(self.fields.get("accessToken"))
    }

    pub fn refresh_token(&self) -> String {
        as_text(self.fields.get("refreshToken"))
    }

    /// 过期时间戳（毫秒）。非法/缺失都当「未提供」——
    /// 与 Node 版 `Number(x) || null` 一致（0 也归到未提供）。
    pub fn expires_at(&self) -> Option<f64> {
        positive_number(self.fields.get("expiresAt"))
    }

    pub fn refresh_expires_at(&self) -> Option<f64> {
        positive_number(self.fields.get("refreshExpiresAt"))
    }

    pub fn domain(&self) -> String {
        as_text(self.fields.get("domain"))
    }

    // ── 端点与版本 ──────────────────────────────────────────

    /// 记录里显式保存的版本 id；缺失时返回 None（由调用方按默认版本兜底）
    pub fn edition(&self) -> Option<String> {
        as_optional_text(self.fields.get("edition"))
    }

    /// 记录里显式保存的 prefixPath。
    /// 注意与 edition 的差别：Node 用 `??`（null/undefined 才兜底），
    /// 所以空串是**有效值**（国际版端点也有空串前缀的场景），不能当未设置。
    pub fn prefix_path(&self) -> Option<String> {
        match self.fields.get("prefixPath") {
            Some(Value::String(text)) => Some(text.clone()),
            _ => None,
        }
    }

    pub fn endpoint(&self) -> Option<String> {
        as_optional_text(self.fields.get("endpoint"))
    }

    pub fn platform(&self) -> Option<String> {
        as_optional_text(self.fields.get("platform"))
    }

    // ── 运营属性 ────────────────────────────────────────────

    pub fn priority(&self) -> i64 {
        let fallback = crate::server::core::account_store::priority::DEFAULT_PRIORITY;
        let value = self.fields.get("priority");
        let parsed = match value {
            Some(Value::Number(number)) => number.as_f64(),
            Some(Value::String(text)) => text.trim().parse::<f64>().ok(),
            _ => None,
        };
        match parsed {
            Some(number) if number.is_finite() => {
                normalize_priority_value(number.round() as i64)
            }
            _ => fallback,
        }
    }

    pub fn set_priority(&mut self, priority: i64) {
        self.fields
            .insert("priority".to_string(), Value::from(priority));
    }

    /// 是否启用：缺省视为启用（对应 Node 版 `enabled !== false`）
    pub fn enabled(&self) -> bool {
        !matches!(self.fields.get("enabled"), Some(Value::Bool(false)))
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.fields
            .insert("enabled".to_string(), Value::Bool(enabled));
    }

    pub fn added_at(&self) -> i64 {
        integer_of(self.fields.get("addedAt"))
    }

    pub fn updated_at(&self) -> i64 {
        integer_of(self.fields.get("updatedAt"))
    }

    pub fn set_updated_at(&mut self, value: i64) {
        self.fields
            .insert("updatedAt".to_string(), Value::from(value));
    }

    /// 账号级出网代理配置（缺失返回 Value::Null）
    pub fn proxy(&self) -> Value {
        self.fields.get("proxy").cloned().unwrap_or(Value::Null)
    }

    pub fn set_proxy(&mut self, proxy: Value) {
        self.fields.insert("proxy".to_string(), proxy);
    }

    /// 是否「有可用凭证」（当前账号派生、凭据查询都以此为准）
    pub fn has_token(&self) -> bool {
        !self.access_token().is_empty()
    }

    /// 选路排序键：`(优先级, 加入时间)`
    pub fn order_key(&self) -> (i64, i64) {
        (self.priority(), self.added_at())
    }

    // ── 通用字段读写（导入合并、限额标记等需要直接改字段）──

    pub fn set(&mut self, key: &str, value: Value) {
        self.fields.insert(key.to_string(), value);
    }

    pub fn remove(&mut self, key: &str) {
        self.fields.remove(key);
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.fields.get(key)
    }
}

/// 把数值转成 JSON：**整数形式的数写成整数**。
///
/// 为什么必须这样：`serde_json::Value::from(1e12_f64)` 会输出 `1000000000000.0`，
/// 而 Node 的 `JSON.stringify(1730000000000)` 输出 `1730000000000`。
/// 时间戳一律是整数毫秒，写成浮点会让 accounts.json 的 diff 全是噪音，
/// 也可能让「按文本比对配置」的用法出现意外差异。
pub fn json_number(value: f64) -> Value {
    if !value.is_finite() {
        return Value::Null;
    }
    if value.fract() == 0.0 && value.abs() < 9_007_199_254_740_992.0 {
        return Value::from(value as i64);
    }
    serde_json::Number::from_f64(value)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

/// 正的数值（>0 才算，负数/0/非数字都当未提供）
fn positive_number(value: Option<&Value>) -> Option<f64> {
    let number = match value {
        Some(Value::Number(number)) => number.as_f64()?,
        Some(Value::String(text)) => text.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    if number.is_finite() && number > 0.0 {
        Some(number)
    } else {
        None
    }
}

/// 时间戳口径：Node 里 `addedAt || 0` 是「保留原值（含浮点毫秒），缺失当 0」，
/// 所以这里按 i64 取整保存（毫秒时间戳用整数表达即可）。
fn integer_of(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Number(number)) => number.as_i64().unwrap_or_else(|| {
            number.as_f64().map(|float| float as i64).unwrap_or(0)
        }),
        Some(Value::String(text)) => text.trim().parse::<i64>().unwrap_or(0),
        _ => 0,
    }
}

// ─── 派生视图（对应 Node 版 getCurrentEntry / getCredentialsById / getSessionById）──

/// 当前账号（队首的可用账号）与其会话
#[derive(Clone, Debug)]
pub struct CurrentEntry {
    pub id: String,
    pub session: Value,
}

/// `getCredentialsById` 的返回形态（含 token 与端点身份）。
///
/// 字段与 Node 版 `workbuddy-account-store.mjs` 的同名导出逐一对齐：这是该导出的
/// 强类型对等物，续期（refresh_account）已用 name/uid/refresh_token/endpoint 等；
/// 下面几个字段当前无读取点，但保留以维持与 Node 返回形态的完整对应。
#[derive(Clone, Debug)]
pub struct CredentialsById {
    /// Node 版 `getCredentialsById().id` 的对等字段（调用方已持有 id，形态对齐保留）
    #[allow(dead_code)]
    pub id: String,
    pub name: String,
    pub uid: String,
    /// Node 版 `getCredentialsById().accessToken` 的对等字段（续期改用 refreshToken）
    #[allow(dead_code)]
    pub access_token: String,
    pub refresh_token: String,
    /// Node 版 `getCredentialsById().expiresAt` 的对等字段
    #[allow(dead_code)]
    pub expires_at: Option<f64>,
    pub endpoint: String,
    pub prefix_path: String,
    pub platform: String,
    pub edition: String,
    /// Node 版 `getCredentialsById().priority` 的对等字段
    #[allow(dead_code)]
    pub priority: i64,
    /// Node 版 `getCredentialsById().enabled` 的对等字段
    #[allow(dead_code)]
    pub enabled: bool,
    pub proxy: Value,
    pub proxy_error: Option<String>,
}

/// `getSessionById` 的返回形态（会话 + 该账号解析出的出口）
#[derive(Clone, Debug)]
pub struct SessionById {
    /// Node 版 `getSessionById()` 返回对象里的 id（调用方已持有 id，形态对齐保留）
    #[allow(dead_code)]
    pub id: String,
    pub session: Value,
    pub proxy: Value,
    pub proxy_error: Option<String>,
}
