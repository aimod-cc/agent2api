//! 账号优先级号段规则（对照 workbuddy-account-store.mjs 的优先级部分）。
//!
//! 优先级是**严格主备序号**：必须唯一，允许并列会让「同级」语义失效
//! （退化成永远只取加入最早的那个，其余账号形同虚设）。因此：
//!   - 写入侧（新增/修改）遇到冲突一律拒绝（409），由用户显式选一个空闲值；
//!   - 启动时对既有数据做一次性去重迁移（`renumber_consecutively` + store 的
//!     启动迁移），保住原有选路顺序；
//!   - 校验范围含已禁用账号 —— 否则禁用期间让出号段，重新启用就会撞车。
//!
//! ── 唯一性的作用域：**全局**（所有提供商共用一条队列）─────────
//! 曾经按 provider 各排各的队（workbuddy 与 raccoon 各自维护 0–9999），
//! 现在改回**全局唯一**：四家账号混在同一条队列里，转发时按优先级从小到大
//! 逐个尝试，跳过禁用 / 不支持该模型 / 该模型限流中的账号。于是「先用哪一家」
//! 由账号优先级本身决定，不再有独立的 provider 路由优先级。
//! 本模块的函数都按**传入的那一组账号**计算，调用方应把全部账号传进来。
//!
//! 本模块只做纯数值运算，不认识磁盘与 HTTP。

use serde_json::Value;

/// 默认优先级：首个账号从这里开始编号，后续账号依次追加
pub const DEFAULT_PRIORITY: i64 = 100;
pub const MIN_PRIORITY: i64 = 0;
pub const MAX_PRIORITY: i64 = 9999;

/// 优先级归一：非法值回落到默认值，并夹在允许区间内
/// （对应 Node 版 `normalizePriority`：null/undefined/'' 与 NaN 都回落）。
pub fn normalize_priority(value: Option<&Value>, fallback: i64) -> i64 {
    let number = match value {
        None | Some(Value::Null) => return fallback,
        // 空字符串在 JS 里 `Number('') === 0`，但 Node 版显式把它归到 fallback 分支
        Some(Value::String(text)) if text.trim().is_empty() => return fallback,
        Some(Value::Number(number)) => match number.as_f64() {
            Some(value) if value.is_finite() => value,
            _ => return fallback,
        },
        Some(Value::String(text)) => match text.trim().parse::<f64>() {
            Ok(value) if value.is_finite() => value,
            _ => return fallback,
        },
        // 对象/数组/布尔在 JS 里 Number() 多为 NaN → 回落
        _ => return fallback,
    };
    let rounded = number.round();
    // Math.min(MAX, Math.max(MIN, round(num)))
    (rounded as i64).clamp(MIN_PRIORITY, MAX_PRIORITY)
}

/// 直接从强类型记录上归一（避免每处都包一层 Value）
pub fn normalize_priority_value(value: i64) -> i64 {
    value.clamp(MIN_PRIORITY, MAX_PRIORITY)
}

/// 选路顺序：优先级升序，同优先级按加入时间（与 Node 版 `byPriorityOrder` 一致）。
/// 唯一性由写入侧保证，这里的次级排序键只是手工编辑文件时的兜底。
pub fn by_priority_order(a: (i64, i64), b: (i64, i64)) -> std::cmp::Ordering {
    let (a_priority, a_added) = a;
    let (b_priority, b_added) = b;
    normalize_priority_value(a_priority)
        .cmp(&normalize_priority_value(b_priority))
        .then(a_added.cmp(&b_added))
}

/// 找占用某优先级且不是 `exclude_id` 的账号。
/// 返回其 `(id, name)` —— 报错文案里要带占位者的名字。
///
/// `entries` 是全部账号的三元组 `(id, name, priority)`（唯一性是全局的，见模块头）。
pub fn find_priority_holder(
    entries: &[(String, String, i64)],
    priority: i64,
    exclude_id: Option<&str>,
) -> Option<(String, String)> {
    entries
        .iter()
        .find(|(id, _, value)| {
            Some(id.as_str()) != exclude_id && normalize_priority_value(*value) == priority
        })
        .map(|(id, name, _)| (id.clone(), name.clone()))
}

/// 下一个可用优先级：排在现有账号之后（新增账号不抢占已有转发顺序）。
///
/// `used` 是全部账号已占用的号段。
/// 号段用满时从默认值起找第一个空位；再找不到就回落默认值
/// （与 Node 版完全相同的兜底路径）。
pub fn next_free_priority(used: &[i64]) -> i64 {
    let max = used
        .iter()
        .map(|value| normalize_priority_value(*value))
        .max()
        .unwrap_or(DEFAULT_PRIORITY - 1);
    let mut candidate = max + 1;
    while used.contains(&candidate) && candidate <= MAX_PRIORITY {
        candidate += 1;
    }
    if candidate <= MAX_PRIORITY {
        return candidate;
    }
    for priority in MIN_PRIORITY..=MAX_PRIORITY {
        if !used.contains(&priority) {
            return priority;
        }
    }
    DEFAULT_PRIORITY
}

/// 一条优先级变更记录（返回给前端与日志展示）
#[derive(Clone, Debug)]
pub struct PriorityAssignment {
    pub id: String,
    pub name: String,
    pub from: i64,
    pub to: i64,
}

/// 把已排好序的账号重新连续编号，保持这个顺序不变。
///
/// 起点优先沿用原有最小优先级（这样只是整体平移，相对顺序与「第 N 位」都不变）；
/// 若从那里起排不下（号段顶到 MAX_PRIORITY），就整体下移到刚好放得下的位置 ——
/// 绝不能简单地对每个值取 `min(MAX, base + i)`，那样会把靠后的账号压成同一个
/// 数值，直接破坏「优先级唯一」这条不变量。
///
/// `records` 为 `(id, name, priority)` 的可变切片，按调用方排好的顺序传入。
/// 返回变更清单（未变化的项不列入）。
pub fn renumber_consecutively(records: &mut [(String, String, i64)]) -> Vec<PriorityAssignment> {
    if records.is_empty() {
        return Vec::new();
    }
    let base = records
        .iter()
        .map(|(_, _, priority)| normalize_priority_value(*priority))
        .min()
        .unwrap_or(DEFAULT_PRIORITY);
    let span = records.len() as i64 - 1;
    let start = if base + span <= MAX_PRIORITY {
        base
    } else {
        (MAX_PRIORITY - span).max(MIN_PRIORITY)
    };

    let mut assignments = Vec::new();
    for (index, (id, name, priority)) in records.iter_mut().enumerate() {
        let next = (start + index as i64).clamp(MIN_PRIORITY, MAX_PRIORITY);
        let from = normalize_priority_value(*priority);
        if from != next {
            *priority = next;
            assignments.push(PriorityAssignment {
                id: id.clone(),
                name: name.clone(),
                from,
                to: next,
            });
        }
    }
    assignments
}
