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

/// 本地领取台账：`{ "day": "YYYY-MM-DD", "items": ["<activityId>"…], "all": bool }`。
struct Ledger {
    day: String,
    claimed: BTreeSet<String>,
    /// 今天是否**整体领过**一次（签到或「一键领取」成功即置位）。
    ///
    /// ── 为什么不能只靠 `claimed` 判 done（实测踩到）────────────
    /// 领取后活动集合会变：已领满的会从 `bonus_skus` 掉出去、新的活动可能进来 ——
    /// 于是台账里记的 key 与「当前列出的任务」对不上，面板永远显示「待领取」。
    /// 而 OfficeAce 一次 claim 就把当天所有可领的都发了，所以「今天领过」就等于
    /// 「当前这批任务都已处理」。`done = all || claimed.contains(key)` 两者取并。
    all: bool,
}

/// 读台账；跨天（或没台账 / 记录坏了）都当空表 —— 只回答「**今天**领过没有」。
fn ledger_of(store: &AccountStore, account_id: &str) -> Ledger {
    let day = crate::server::core::providers::codearts::welfare::today(logging::now_ms());
    let stored = store
        .officeace_bonus_ledger(account_id)
        .filter(|stored| stored.get("day").and_then(Value::as_str) == Some(day.as_str()));
    let Some(stored) = stored else {
        return Ledger { day, claimed: BTreeSet::new(), all: false };
    };
    let claimed = stored
        .get("items")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let all = stored.get("all").and_then(Value::as_bool).unwrap_or(false);
    Ledger { day, claimed, all }
}

fn persist_ledger(store: &AccountStore, account_id: &str, ledger: &Ledger) {
    let payload = json!({
        "day": ledger.day,
        "items": ledger.claimed.iter().cloned().collect::<Vec<_>>(),
        "all": ledger.all,
    });
    if let Err(error) = store.put_officeace_bonus_ledger(account_id, &payload) {
        // 落盘失败不改签到结果：上游那边可能已经发下来了，把成功报成失败本末倒置。
        logging::verbose(
            "[OfficeAce]",
            &format!("账号 {account_id} 的奖励台账未能落盘：{}", error.message),
        );
    }
}

/// 把一批活动记进「今天已处理」台账（签到与领取共用），并置「整体领过」位。
///
/// ── 为什么签到也要调它（与用户预期对齐）──────────────────────
/// OfficeAce 的签到与新手任务是**同一个上游动作**（一次 claim 就把当天所有奖励
/// 都发下来）—— 所以签到成功之后，新手任务其实**已经领过了**。若不在签到里落这笔
/// 台账，面板的「新手任务」会一直显示未完成（要再点一次「领取」才补上），
/// 与用户预期「和其他家一样在签到时自动完成新手任务」不符。
pub fn mark_claimed(store: &AccountStore, account_id: &str, bonuses: &[subscription::Bonus]) {
    let mut ledger = ledger_of(store, account_id);
    // 即便这次一个活动都没列出来（上游当时没给 bonus_skus），也置位：
    // 这次 claim 本身已经把当天能领的都领了。
    ledger.all = true;
    for bonus in bonuses {
        ledger.claimed.insert(bonus.activity_id.clone());
    }
    persist_ledger(store, account_id, &ledger);
}

/// 任务行 + 汇总：返回 `(tasks, total, earned, unclaimed)`。
///
/// `done = 今天整体领过 || 这个 key 在台账里` —— 前者兜住「领取后活动集合变了、
/// key 对不上」那种情况（见 [`Ledger`] 的说明）。
fn build_tasks(
    bonuses: &[subscription::Bonus],
    ledger: &Ledger,
) -> (Vec<Value>, f64, f64, usize) {
    let is_done = |key: &str| ledger.all || ledger.claimed.contains(key);
    let tasks: Vec<Value> = bonuses
        .iter()
        .map(|bonus| {
            let key = bonus.activity_id.clone();
            let title = bonus.name.clone();
            let done = is_done(&key);
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
        .filter(|bonus| is_done(&bonus.activity_id))
        .map(|bonus| bonus.total)
        .sum();
    let unclaimed = bonuses
        .iter()
        .filter(|bonus| !is_done(&bonus.activity_id))
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
///
/// ── `refresh` 在本家为什么**不改变取数**────────────────────
/// 别家（Loomy / CodeArts / 小浣熊）的新人礼是**一次性**的，结清后写进
/// `onboarding_memory` 永久记忆，`refresh=false` 就直接吃记忆、零上游请求。
/// 本家不是那个形状：一次 claim 发的是**当天**的全部奖励，第二天上游会开新
/// 一批活动 —— 用「永久结清」那份记忆会把明天新开的活动也判成已领完。
/// 所以本家用**按天**的台账（[`ledger_of`]），它今天没结清就照样实查；
/// 而这里的读请求只是一次 `GET /v1/subscription`（无副作用、不领东西），
/// 真正会白打的那一发（claim）已经由台账在 [`claim_all`] 里挡住了。
/// `settled` 字段照别家的口径给出「今天整体领过 = 这条福利已结清」，
/// 界面据此可以在领完那天跳过自动查询。
pub async fn get_tasks(
    store: &AccountStore,
    account_id: &str,
    _refresh: bool,
) -> Result<Value, GatewayError> {
    let credential = credential_of(store, account_id)?;
    let subscription = subscription::fetch_subscription(&credential).await?;
    let bonuses = subscription::bonuses_of(&subscription);
    let ledger = ledger_of(store, account_id);
    let (tasks, total, earned, unclaimed) = build_tasks(&bonuses, &ledger);
    Ok(json!({
        "tasks": tasks,
        "earned": earned,
        "total": total,
        "unclaimed": unclaimed,
        // 今天整体领过 ⇒ 这条福利当天结清（台账按天复位，明天重新计）
        "settled": unclaimed == 0,
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
    // 一次 claim 把整个集合都结算了：置「今天整体领过」位（上面的循环已把每个
    // 活动记进台账）。`all` 兜住「领完之后活动集合变了、key 对不上」那种情况。
    ledger.all = true;
    persist_ledger(store, account_id, &ledger);
    let (tasks, total, earned, unclaimed) = build_tasks(&bonuses, &ledger);
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
