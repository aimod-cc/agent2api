//! OfficeAce 的每日签到：`POST /v1/subscription/bonus/claim` 领奖励积分。
//!
//! ── 与「新手任务」的关系 ─────────────────────────────────────
//! 上游**一次调用**把当天所有可领的奖励（每日签到 + 一次性新人礼）一起发下来，
//! 所以签到与新手任务在服务端是**同一个动作**（见 `onboarding` 的模块头）。
//! 本模块是签到链（`billing::checkin_for`）要的 `{success, msg}` 形状。
//!
//! ── 幂等由上游保证 ──────────────────────────────────────────
//! 一天一个活动只能领一次，**重复领不报错**（返回同一份列表，`current_value` 不变），
//! 所以这里没有「已领过」这条分支 —— 上游不回错，重复调就是安全的空动作。

use serde_json::{json, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::errors::GatewayError;

use super::{credentials, subscription};

/// 领今天的奖励积分（`billing::checkin_for` 的 `{success, msg}` 契约）。
pub async fn claim_daily_checkin(
    store: &AccountStore,
    account_id: &str,
) -> Result<Value, GatewayError> {
    let record = store
        .officeace_account_record(account_id)
        .ok_or_else(|| GatewayError::with_status(400, "OfficeAce 账号不存在或不可用，请重新选择"))?;
    let credential = credentials::from_record(Some(&record))?;
    // 领之前记一份余额好算增量；读失败不阻断签到（只是少一个读数，不能因此判签到失败）
    let before = subscription::fetch_subscription(&credential).await.ok();
    let after = subscription::claim_bonus(&credential).await?;
    let gained = before
        .as_ref()
        .map(|before| subscription::gained_between(before, &after));
    let msg = match gained {
        Some(points) if points > 0.0 => format!("已领取 {points} 积分"),
        Some(_) => "今天没有可领的奖励（或已领过）".to_string(),
        // 增量算不出（领取前的读数没拿到）时如实说「已签到」，不编一个数字
        None => "已签到".to_string(),
    };
    Ok(json!({ "success": true, "msg": msg, "gained": gained }))
}
