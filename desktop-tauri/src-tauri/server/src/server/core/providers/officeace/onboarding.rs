//! OfficeAce 的「新手任务 / 奖励活动」：把订阅里的 `bonus_skus` 当任务列出来。
//!
//! ── 与每日签到共用同一个上游动作（重要）──────────────────────
//! 上游 `POST /v1/subscription/bonus/claim` **一次把当天所有可领的奖励全部发下来**
//! （每日签到 + 一次性新人礼混在同一批 `bonus_skus` 里），没有「逐项领」的接口。
//! 所以本模块的领取动作就是重打那个接口 —— 与 `checkin` 是同一个调用，只是这里
//! 按活动逐条汇报结果。响应形状对齐 `codearts::onboarding`（签到中心前端读那套）。
//!
//! ── `done` 为什么用本地台账 ──────────────────────────────────
//! 上游的 `bonus_skus` 只给「总量 / 已用」，**不给「这个活动今天领过没有」** ——
//! 而领没领正是「任务完成」这条 UI 要的事实。所以像 CodeArts 那样落一份**本地台账**
//! （账号记录的 `bonusClaims`）：领取成功后把当前所有活动记为「今天已处理」。
//! 日界口径复用 `codearts::welfare::today`（北京自然日）—— 上游的活动周期就是 UTC+8
//! 零点，两处同源才不会有「今天」的分歧。
//!
//! 记「全部活动」而不是「只有增量的那些」是刻意的：一次 claim 就把整个集合结算了，
//! 剩下那些本来就没增量的（已领满 / 今天不发）如果留成 unclaimed，签到中心的
//! 「有未领取就自动补领」会每次都再打一发（幂等但白打）。见 `claim_all`。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use std::collections::BTreeSet;

use serde_json::{json, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::{credentials, subscription};

/// 任务分组名（签到中心按它分块显示）。
const TASK_GROUP: &str = "奖励积分";

/// 本地领取台账：`{ "day": "YYYY-MM-DD", "items": ["<activityId>"…] }`。
struct Ledger {
    day: String,
    claimed: BTreeSet<String>,
}

/// 读台账；跨天（或没台账 / 记录坏了）都当空表 —— 只回答「**今天**领过没有」。
fn ledger_of(store: &AccountStore, account_id: &str) -> Ledger {
    let day = crate::server::core::providers::codearts::welfare::today(logging::now_ms());
    let claimed = store
        .officeace_bonus_ledger(account_id)
        .filter(|stored| stored.get("day").and_then(Value::as_str) == Some(day.as_str()))
        .and_then(|stored| stored.get("items").cloned())
        .and_then(|items| items.as_array().cloned())
        .map(|rows| {
            rows.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    Ledger { day, claimed }
}

fn persist_ledger(store: &AccountStore, account_id: &str, ledger: &Ledger) {
    let payload = json!({
        "day": ledger.day,
        "items": ledger.claimed.iter().cloned().collect::<Vec<_>>(),
    });
    if let Err(error) = store.put_officeace_bonus_ledger(account_id, &payload) {
        // 落盘失败不改签到结果：上游那边可能已经发下来了，把成功报成失败本末倒置。
        logging::verbose(
            "[OfficeAce]",
            &format!("账号 {account_id} 的奖励台账未能落盘：{}", error.message),
        );
    }
}

/// 把一批活动记进「今天已处理」台账（签到与领取共用）。
///
/// ── 为什么签到也要调它（与用户预期对齐）──────────────────────
/// OfficeAce 的签到与新手任务是**同一个上游动作**（一次 claim 就把当天所有奖励
/// 都发下来）—— 所以签到成功之后，新手任务其实**已经领过了**。若不在签到里落这笔
/// 台账，面板的「新手任务」会一直显示未完成（要再点一次「领取」才补上），
/// 与用户预期「和其他家一样在签到时自动完成新手任务」不符。
pub fn mark_claimed(store: &AccountStore, account_id: &str, bonuses: &[subscription::Bonus]) {
    if bonuses.is_empty() {
        return;
    }
    let mut ledger = ledger_of(store, account_id);
    for bonus in bonuses {
        ledger.claimed.insert(bonus.activity_id.clone());
    }
    persist_ledger(store, account_id, &ledger);
}

/// 任务行 + 汇总：返回 `(tasks, total, earned, unclaimed)`。
fn build_tasks(
    bonuses: &[subscription::Bonus],
    claimed: &BTreeSet<String>,
) -> (Vec<Value>, f64, f64, usize) {
    let tasks: Vec<Value> = bonuses
        .iter()
        .map(|bonus| {
            let key = bonus.activity_id.clone();
            let title = bonus.name.clone();
            let done = claimed.contains(&key);
            json!({
                "key": key,
                "title": title,
                "group": TASK_GROUP,
                "points": bonus.total,
                "done": done,
                "claimable": !done,
                "remaining": bonus.remaining(),
                "expiresAt": bonus.expires_at,
            })
        })
        .collect();
    let total: f64 = bonuses.iter().map(|bonus| bonus.total).sum();
    let earned: f64 = bonuses
        .iter()
        .filter(|bonus| claimed.contains(&bonus.activity_id))
        .map(|bonus| bonus.total)
        .sum();
    let unclaimed = bonuses
        .iter()
        .filter(|bonus| !claimed.contains(&bonus.activity_id))
        .count();
    (tasks, total, earned, unclaimed)
}

/// 取账号与其网关/控制面凭据（两处入口共用）。
fn credential_of(
    store: &AccountStore,
    account_id: &str,
) -> Result<credentials::OfficeAceCredential, GatewayError> {
    let record = store
        .officeace_account_record(account_id)
        .ok_or_else(|| GatewayError::with_status(400, "OfficeAce 账号不存在或不可用，请重新选择"))?;
    credentials::from_record(Some(&record))
}

/// 任务状态（只读）：列订阅里的奖励活动。
pub async fn get_tasks(store: &AccountStore, account_id: &str) -> Result<Value, GatewayError> {
    let credential = credential_of(store, account_id)?;
    let subscription = subscription::fetch_subscription(&credential).await?;
    let bonuses = subscription::bonuses_of(&subscription);
    let ledger = ledger_of(store, account_id);
    let (tasks, total, earned, unclaimed) = build_tasks(&bonuses, &ledger.claimed);
    Ok(json!({
        "tasks": tasks,
        "earned": earned,
        "total": total,
        "unclaimed": unclaimed,
        "provider": "officeace",
        "note": "OfficeAce 的奖励由一次领取动作统一发放（每日签到 + 新人礼）；点「全部领取」即领当天所有可领项",
    }))
}

/// 领取全部未领的奖励（与每日签到同一个上游动作，见模块头）。
pub async fn claim_all(store: &AccountStore, account_id: &str) -> Result<Value, GatewayError> {
    let credential = credential_of(store, account_id)?;
    let before = subscription::fetch_subscription(&credential).await?;
    let after = subscription::claim_bonus(&credential).await?;
    let previous = subscription::remaining_map(&before);
    let bonuses = subscription::bonuses_of(&after);
    let mut ledger = ledger_of(store, account_id);
    let mut results = Vec::new();
    let mut claimed = 0usize;
    let mut claimed_points = 0.0f64;
    for bonus in &bonuses {
        let key = bonus.activity_id.clone();
        let title = bonus.name.clone();
        let remaining = bonus.remaining();
        let gained = match previous.get(&key) {
            Some(prev) => (remaining - prev).max(0.0),
            None => remaining,
        };
        let already = gained <= 0.0;
        if !already {
            claimed += 1;
            claimed_points += gained;
        }
        // 一次 claim 把整个集合都结算了：所有当前活动都记成「今天已处理」，
        // 免得没增量的那些永远挂着 unclaimed、被自动补领反复重打（见模块头）。
        ledger.claimed.insert(key.clone());
        results.push(json!({
            "key": key,
            "title": title,
            "ok": true,
            "already": already,
            "gained": gained,
        }));
    }
    persist_ledger(store, account_id, &ledger);
    let (tasks, total, earned, unclaimed) = build_tasks(&bonuses, &ledger.claimed);
    Ok(json!({
        "results": results,
        "claimed": claimed,
        "failed": 0,
        "claimedPoints": claimed_points,
        "tasks": tasks,
        "earned": earned,
        "total": total,
        "unclaimed": unclaimed,
        "provider": "officeace",
    }))
}
