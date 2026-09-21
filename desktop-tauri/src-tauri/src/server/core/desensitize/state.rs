//! 持久化状态的**形态**：序列化、解析、JS 数值语义，以及迁移项用的两个入口。
//!
//! ── 为什么单独一层（而不是全留在 mod.rs）────────────────────
//! 与 `engine.rs` 同一条理由：`mod.rs` 现在装的是「状态化」的东西（句柄、读写锁、
//! 统计、默认词表迁移），而「一份状态记录长什么样、怎么解析、哪些值要宽容」是
//! 一组**不依赖任何句柄**的纯函数 —— 它们既被运行期（`load` / `save_locked`）用，
//! 也被迁移项（`import_legacy`）用。抽出来之后：
//!   - `mod.rs` 不必为了容纳那段注释与实现而挤到 900 行以上（本项目的单文件
//!     行数约定）；
//!   - 「解析口径只有一处」这件事在文件结构上直接可见 —— 谁想改宽容规则，改的是
//!     本文件，两个调用方同时生效，不可能只改一半。
//!
//! ── 与 key-value 形态的关系 ─────────────────────────────────
//! 这里**只管文本 ↔ 状态的转换**，不碰数据库（落库那一条 UPSERT 在 `sql.rs`，
//! 由 `import_legacy` 代调一次）。所以本文件可以脱离数据库单独读懂。
//!
//! ── 键名与「字节对齐」的历史 ────────────────────────────────
//! 五个键名（`enabled` / `terms` / `roles` / `providers` / `defaultsVersion`）
//! 是**数据契约**：迁移项读旧文件、运行期读库里的值走的都是本文件的
//! [`parse_state`]，键名对不上就等于丢配置。而**字段顺序与缩进**曾经也是契约
//! （要与 Node 的 `JSON.stringify(x, null, 2) + "\n"` 逐字节一致），那条约束
//! 随本次改造消失 —— 完整论证见 `mod.rs` 模块头，这里只留结论：
//! 用普通 `json!` 序列化即可，不再手工拼字符串。

use serde_json::{json, Value};

use super::normalize_terms;

/// 从 JSON 数组里取字符串项（对应 normalizeTerms 的 `typeof raw !== 'string'`
/// 过滤：非字符串项直接跳过，不报错）。
fn string_list(items: &[Value]) -> Vec<String> {
    items
        .iter()
        .filter_map(|item| item.as_str().map(str::to_string))
        .collect()
}

/// JS `Number(x)` 语义：null/非法 → NaN，布尔 → 0/1，字符串走 JS 数字语法
/// （空串与纯空白都是 0）。只用于 defaultsVersion 这一处比较，无需完整实现。
fn js_number(value: &Value) -> f64 {
    match value {
        Value::Number(number) => number.as_f64().unwrap_or(f64::NAN),
        Value::Bool(flag) => {
            if *flag {
                1.0
            } else {
                0.0
            }
        }
        Value::Null => f64::NAN,
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return 0.0;
            }
            trimmed.parse::<f64>().unwrap_or(f64::NAN)
        }
        Value::Array(_) | Value::Object(_) => f64::NAN,
    }
}

/// 一份持久化状态：`Inner` 里**需要落库的那几项**。
///
/// 为什么单独一个结构而不是直接序列化 `Inner`：`Inner` 还有 `matcher`（编译后
/// 的匹配器，不可序列化也不该落库）与 `stats`（**仅内存**的命中计数，本模块
/// 刻意不持久化 —— 见 `reset_stats`）两个字段。把它们排除在外这件事写在类型上，
/// 比每次手写 `json!` 漏一个字段要可靠。
///
/// `enabled` / `terms` 是 `Option`：它们表达「记录里有没有这一项」，供
/// `apply_state` 决定「保持当前值」还是「用记录里的值」。`roles` / `providers`
/// 不需要这层区分 —— 它们各自的 normalize 对「非数组」本来就有回落默认的实现。
pub(super) struct PersistedState {
    pub(super) enabled: Option<bool>,
    pub(super) terms: Option<Vec<String>>,
    pub(super) roles: Option<Value>,
    pub(super) providers: Option<Value>,
    pub(super) defaults_version: f64,
}

/// 构造要落库的值。
///
/// **唯一仍然需要手写**的是 `defaultsVersion` 的写法：它要按 JS 数字语义渲染
/// （`2.0` 落成 `2`），否则库里会出现 `2.0` 这种看着别扭的值 —— 这不再是兼容
/// 要求，只是「排障时看起来与改造前一致」的洁癖（见 [`js_number_value`]）。
pub(super) fn state_value(
    enabled: bool,
    terms: &[String],
    roles: &[String],
    providers: &[String],
    version: f64,
) -> Value {
    json!({
        "enabled": enabled,
        "terms": terms,
        "roles": roles,
        super::KEY_PROVIDERS: providers,
        "defaultsVersion": js_number_value(version),
    })
}

/// JS 数字 → `Value`：整数回落成整型（`JSON.stringify(2)` → `2`），
/// 非有限值在 JSON 里没有对应物，按 `null` 处理。
fn js_number_value(value: f64) -> Value {
    if !value.is_finite() {
        return Value::Null;
    }
    if value.fract() == 0.0 && value.abs() < 9.007_199_254_740_992e15 {
        return json!(value as i64);
    }
    json!(value)
}

/// 落库文本 → [`PersistedState`]。**读侧宽容口径逐字保留**（改造前 `load()` 里的
/// 那段判断一字未改，只是搬到了这里）。
///
/// 逐条的兜底：
///   - `enabled` 非布尔 → `None`（「保持当前值」，与改造前的
///     `if let Some(flag) = ...` 逐字同义）；
///   - `terms` 非数组 → `None`（同上）；是数组则走 `normalize_terms`
///     （trim、去重、滤空、上限，与写接口同一套归一）；
///   - `roles` / `providers` 原样带上，交给各自的 normalize（认不出就回落默认）；
///   - `defaultsVersion` 走 JS `Number()` 语义，小于 1 的（缺失、脏值、0）
///     一律视为版本 1。
/// 返回 `Err` 只有一种情形：**整段不是合法 JSON**（旧文件被写坏、库里那行被
/// 手工改坏）。调用方按「回退默认词表 + 打一行日志」处理。
pub(super) fn parse_state(text: &str) -> Result<PersistedState, String> {
    let parsed: Value = serde_json::from_str(text).map_err(|error| error.to_string())?;
    Ok(PersistedState {
        enabled: parsed.get("enabled").and_then(Value::as_bool),
        terms: parsed
            .get("terms")
            .and_then(Value::as_array)
            .map(|items| normalize_terms(&string_list(items))),
        roles: parsed.get("roles").cloned(),
        providers: parsed.get(super::KEY_PROVIDERS).cloned(),
        defaults_version: {
            let version = js_number(parsed.get("defaultsVersion").unwrap_or(&Value::Null));
            if version >= 1.0 {
                version
            } else {
                1.0
            }
        },
    })
}

// ─── 迁移项接口 ────────────────────────────────────────────

/// 迁移项用：`kv` 里有没有本模块的状态记录（幂等闸门）。
///
/// **本项与其它五个迁移项的判据不同**：它们各自有独立的表，判「表为空」即可；
/// `kv` 是**共享表**（账号的 `priorityScope`、日志的 `logsNextId`、桌面设置都在
/// 里面），「表里有没有行」与「本项迁过没有」是两件毫不相干的事 —— 用表非空当
/// 闸门会让本项**永远跳过**（那时 `kv` 里几乎必然已有别的键）。所以这里判的是
/// **键是否存在**（见 `db/migrate/desensitize.rs` 的模块头）。
pub(crate) fn legacy_present(conn: &rusqlite::Connection) -> rusqlite::Result<bool> {
    super::sql::has_key(conn)
}

/// 迁移项用：把一份旧文件文本落进 `kv`，返回记录里的词条数。
///
/// 走的是**与运行期完全相同**的那条路径：`parse_state` 解析、`state_value`
/// 序列化、`sql::save` 落库。于是「迁移前后行为一致」这件事由共用实现保证，
/// 而不是靠两处手写的纪律（与 T4「回填搬进迁移项、共用同一套 SQL」同构）。
///
/// 错误用 `String` 承载（而不是自定义错误枚举）：唯一的消费者是迁移项的那一行
/// 日志，它只需要一句能打给人看的话 —— 与 `Db::open` 的返回形态一致。
/// 「整段不是 JSON」与「SQL 写失败」在文案里是两句不同的话，用户能自己分辨。
pub(crate) fn import_legacy(conn: &rusqlite::Connection, text: &str) -> Result<usize, String> {
    let state = parse_state(text)?;
    let count = state.terms.as_ref().map(Vec::len).unwrap_or(0);
    let roles = super::normalize_roles(state.roles.as_ref());
    let providers = super::normalize_providers(state.providers.as_ref());
    // enabled 缺失时按 `true` 落库：这里没有「当前值」可保持（新库新记录），
    // 而「旧文件里没有 enabled」在改造前的语义就是保持默认态 = 启用
    let value = state_value(
        state.enabled.unwrap_or(true),
        state.terms.as_deref().unwrap_or(&[]),
        &roles,
        &providers,
        state.defaults_version,
    );
    super::sql::save(conn, &value.to_string()).map_err(|error| error.to_string())?;
    Ok(count)
}
