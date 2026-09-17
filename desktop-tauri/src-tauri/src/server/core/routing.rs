//! 账号选路 —— 严格优先级排队（对照 Node 版 src/workbuddy-routing.mjs 全量移植）。
//!
//! 优先级是「主备序号」：优先级必须全局唯一（由 account-store 在写入侧保证），
//! 不允许多个账号并列 —— 并列会让「同级」语义失效。本模块只做**纯判定**，
//! 数据来源是账号存储的公开形态（`store.list_accounts()` 的 `accounts` 数组），
//! 字段就是 UI 上看到的那几个：`id` / `name` / `priority` / `addedAt` /
//! `enabled` / `rateLimits`。
//!
//! ── 为什么用 `serde_json::Value` 而不是强类型结构 ──────────────
//! Node 版的判据全部作用在「公开形态对象」上，而公开形态是容错的
//! （字段缺失/类型不对都不报错，按 JS 语义回落）。这里保持同一数据源与同一
//! 语义，避免为了「类型好看」把手工编辑出的脏数据在解析期整条丢掉 ——
//! 那会让选路结果与 Node 版分叉（例如某个账号因为 `priority: "abc"` 而消失）。
//!
//! ── 规则（照抄 Node 版头部注释）──────────────────────────────
//!   1. 候选 = 启用中（`enabled !== false`）且未对「该模型」处于限额冷却期的账号；
//!   2. 取候选里优先级数值最小的那个（数值小的先用）；
//!   3. 本次已尝试过的账号（429 降级）从候选中排除，避免回环；
//!   4. 全部候选都不可用时，由调用方决定是降级重试还是透传错误。
//!
//! 「当前账号」不是独立的手动选择，而是本模块选路结果在「不限模型」下的那个账号
//! （account-store 的 `get_current_entry` 按同样的判据派生）。因此界面上的「当前」
//! 与转发默认使用谁始终一致；仅当账号对某具体模型限额时，该模型的请求才会临时
//! 降级到下一个候选 —— 那是按模型的一次性决策，不改写「当前账号」。

use serde_json::{json, Value};

use crate::server::core::account_store::priority::{by_priority_order, normalize_priority};
use crate::server::core::account_store::store_util::js_truthy;

/// 账号对某模型是否处于限额冷却期。
///
/// 对应 Node 版 `isRateLimited(account, model, now)`：只认 `rateLimits[model].resetAt`
/// 是**未来**时间戳的情况；记录存在但已过期 = 未限额（冷却自然结束，不必清理）。
pub fn is_rate_limited(account: &Value, model: &str, now: i64) -> bool {
    rate_limit_reset_at(account, model, now) > 0
}

/// 限额恢复时间（未限额返回 0）。
///
/// Node 版口径：`Number(limit.resetAt) || 0`，只有大于 now 才返回，
/// 否则返回 0 —— 这个 0 在选路里表示「当前可用」，不是「立刻恢复」。
pub fn rate_limit_reset_at(account: &Value, model: &str, now: i64) -> i64 {
    let Some(limit) = account
        .get("rateLimits")
        .and_then(|limits| limits.get(model))
    else {
        return 0;
    };
    let reset_at = limit
        .get("resetAt")
        .and_then(js_number)
        .unwrap_or(0.0);
    if reset_at > now as f64 {
        reset_at as i64
    } else {
        0
    }
}

/// 账号是否可用于转发：启用 + 未被该模型限额。
/// `reason` ∈ `Some("disabled")` | `Some("rate-limited")` | `None`（可用）。
pub fn account_usability(account: &Value, model: &str, now: i64) -> AccountUsability {
    // Node: `account?.enabled === false` —— 只有显式 false 才算禁用，
    // 缺失/字符串 "false"/0 都视为启用（与 account-store 的 enabled() 同口径）
    if matches!(account.get("enabled"), Some(Value::Bool(false))) {
        return AccountUsability { usable: false, reason: Some("disabled") };
    }
    if is_rate_limited(account, model, now) {
        return AccountUsability { usable: false, reason: Some("rate-limited") };
    }
    AccountUsability { usable: true, reason: None }
}

/// `account_usability` 的结果
#[derive(Clone, Copy, Debug)]
pub struct AccountUsability {
    pub usable: bool,
    pub reason: Option<&'static str>,
}

/// 按优先级挑选本次请求使用的账号。
///
/// `accounts` 为 store.listAccounts().accounts 的公开形态；
/// `exclude_ids` 是本次请求已尝试过的账号 id（429 降级用）。
/// 返回账号对象，或 None（没有可用账号）。
///
/// 排序取首位即为唯一答案（优先级唯一由写入侧保证）；并列属手工编辑出来的
/// 异常数据，按加入时间兜底，结果依旧稳定。
pub fn pick_account_by_priority(
    accounts: &[Value],
    model: &str,
    exclude_ids: &[String],
    now: i64,
) -> Option<Value> {
    let mut candidates: Vec<Value> = accounts
        .iter()
        .filter(|account| {
            let Some(id) = account_id(account) else {
                return false;
            };
            if exclude_ids.iter().any(|excluded| excluded == id) {
                return false;
            }
            account_usability(account, model, now).usable
        })
        .cloned()
        .collect();
    if candidates.is_empty() {
        return None;
    }
    candidates.sort_by(compare_by_priority);
    candidates.into_iter().next()
}

/// 选路决策的完整快照（供日志展示与排障）：
///   `{ picked, priority, candidateCount, blocked: [{ id, name, reason }], total }`
///
/// Node 版 workbuddy-routing.mjs 同名导出 `describeRouteDecision` 的对等物：
/// 生产路径用不到（Node 同样只导出未使用，仅 smoke test 覆盖），
/// 但排障时能直接拿来比对「为什么选了 B 而不是 A」，故保留 —— 字段与 Node 逐一对齐。
#[allow(dead_code)]
pub fn describe_route_decision(
    accounts: &[Value],
    model: &str,
    exclude_ids: &[String],
    now: i64,
) -> Value {
    let mut blocked: Vec<Value> = Vec::new();
    let mut candidate_count = 0usize;
    for account in accounts {
        let Some(id) = account_id(account) else {
            continue;
        };
        if exclude_ids.iter().any(|excluded| excluded == id) {
            blocked.push(json!({
                "id": id,
                "name": account_name(account, id),
                "reason": "tried",
            }));
            continue;
        }
        let usability = account_usability(account, model, now);
        if usability.usable {
            candidate_count += 1;
        } else {
            blocked.push(json!({
                "id": id,
                "name": account_name(account, id),
                "reason": usability.reason.unwrap_or(""),
            }));
        }
    }
    let picked = pick_account_by_priority(accounts, model, exclude_ids, now);
    let priority = picked
        .as_ref()
        .map(|account| normalize_priority_value_of(account))
        .unwrap_or(Value::Null);
    json!({
        "picked": picked.unwrap_or(Value::Null),
        "priority": priority,
        "candidateCount": candidate_count,
        "blocked": blocked,
        "total": accounts.len(),
    })
}

/// 账号 id（非空字符串才算；对应 Node 的 `account?.id` 真值判定）
pub fn account_id(account: &Value) -> Option<&str> {
    account
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
}

/// 账号展示名：`account.name || account.id`（Node 的 blocked 列表用这个兜底）
pub fn account_name(account: &Value, id: &str) -> String {
    match account.get("name") {
        Some(value) if js_truthy(value) => match value.as_str() {
            Some(text) => text.to_string(),
            None => value.to_string(),
        },
        _ => id.to_string(),
    }
}

/// 从 store 快照 `{ currentAccountId, accounts: [...] }` 取账号数组。
/// 形状不对（手工编辑坏数据）时返回空列表 —— 与 Node 的
/// `Array.isArray(accounts) ? accounts : []` 同义。
pub fn accounts_of(snapshot: &Value) -> Vec<Value> {
    snapshot
        .get("accounts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// 选路排序：优先级升序，同优先级按加入时间（复用 account-store 的规则实现，
/// 保证「界面排序」与「转发顺序」永远同一套判据）。
fn compare_by_priority(a: &Value, b: &Value) -> std::cmp::Ordering {
    let key = |account: &Value| {
        (
            normalize_priority(account.get("priority"), crate::server::core::account_store::priority::DEFAULT_PRIORITY),
            js_number(account.get("addedAt").unwrap_or(&Value::Null)).unwrap_or(0.0),
        )
    };
    let (a_priority, a_added) = key(a);
    let (b_priority, b_added) = key(b);
    by_priority_order((a_priority, a_added as i64), (b_priority, b_added as i64))
}

/// 公开形态里的 priority（归一后的数值）——对应 Node 在日志里打印的 `priority`。
/// 这里沿用 store 归一后的值（Rust 的公开形态一定带这个字段）。
fn normalize_priority_value_of(account: &Value) -> Value {
    Value::from(normalize_priority(
        account.get("priority"),
        crate::server::core::account_store::priority::DEFAULT_PRIORITY,
    ))
}

/// JS `Number(x)`：解析不出（NaN）时返回 None，由调用方按 `|| 0` 兜底。
fn js_number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                // JS: Number('') === 0（与 Number('abc') 的 NaN 不同）
                Some(0.0)
            } else {
                trimmed.parse::<f64>().ok().filter(|value| value.is_finite())
            }
        }
        // Boolean / null / 对象 / 数组：Number(true)=1、Number(null)=0、
        // 其余 NaN。这些取值在真实数据里不会出现，但按 JS 语义实现不费事
        Value::Bool(flag) => Some(if *flag { 1.0 } else { 0.0 }),
        Value::Null => Some(0.0),
        _ => None,
    }
}
