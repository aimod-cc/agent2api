//! AutoClaw 的每日签到（移植来源：AutoClaw 桌面端的「每日签到」Banner 链路）。
//!
//! ── 上游是什么（无参考项目可抄，从客户端产物里挖出来的）──────────
//! 老项目 `D:\APP\AutoClaw\autoclaw-local-proxy` **没有签到**（它的全部接口只有
//! 账号管理、积分钱包、订阅查询与模型转发），所以这一份不是移植而是**逆向**：
//! 接口从安装包 `D:\Program Files\AutoClaw\resources\app.asar` 里读出并实测确认。
//!
//! 桌面端首页侧栏有一个「每日签到」Banner，点击后走通用任务接口：
//!
//! ```text
//! POST {userapi}/autoclaw-proxy/proxy/autoclaw-task-complete
//! body: {"task_id":"daily_signin"}
//! → {"data":{"already_completed":true,"reward_points":0,"success":false,
//!            "task_id":"daily_signin"}}
//! ```
//!
//! 三个要点：
//!   1. **不是签到专用接口** —— `autoclaw-task-complete` 是「完成客户端任务」的
//!      通用入口，`daily_signin` 只是其中一个 task_id（同级的还有
//!      `daily_inspiration_center` / `upgrade_pc_app`）。所以请求体只有一个字段。
//!   2. **HTTP 200 + body 里的成功标记**：实测（2026-09-19）当天已签到时返回
//!      HTTP 200、`success: false`、`already_completed: true`、`reward_points: 0`。
//!      判「签到成功」要看 `reward_points > 0`（或 `success === true`），
//!      **不能只看 HTTP 状态码** —— 那会把「今天已领过」当成一次成功领取。
//!   3. 上游是**按天幂等**的：重复调用不会重复入账，返回「已领取」。
//!
//! ── 为什么复用积分与订阅那套请求封装 ─────────────────────────
//! 同一个 userapi 域、同一套 `X-Auth-Sign` 签名头（`refresh::signed_auth_headers`）、
//! 同一份超时口径。因此这里直接用 `balance.rs` 里那两个 `userapi_post` /
//! `userapi_get` 的对等实现 —— 但它们目前是 `balance.rs` 的私有函数。
//! **不复制**（复制会让 appId/appKey、时间戳单位与品牌头分叉，而签名一旦分叉
//! 就是稳定 400002），改为把它们提升为 `pub(super)` 供本模块调用。
//!
//! ── claim 形状与其他两家对齐 ───────────────────────────────
//! `billing::checkin` 的汇总只认 `{success, msg}` 两个字段（见那里的 `succeeded`
//! 统计）。本模块返回同一形状，于是工作流/日志/界面三处的口径与 WorkBuddy、
//! 小浣熊完全一致，不需要为第三家再加分支。
//!
//! ── 为什么带一个只读的任务列表调用 ─────────────────────────
//! `GET autoclaw-task-list` 能拿到所有任务的完成状态。它在**签到失败时**提供
//! 归因：拿不到列表说明登录态有问题，能拿到而 `daily_signin` 已完成，则说明
//! 是「今天已签到」而不是「接口坏了」。因此只在需要解释失败时才拉它，
//! 正常路径不多花一次往返。

use serde_json::{json, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::errors::GatewayError;

use super::balance::{userapi_get, userapi_post};
use super::credentials;

/// 每日签到任务的 id（源实现 `DAILY_SIGNIN_TASK_ID`）
const DAILY_SIGNIN_TASK_ID: &str = "daily_signin";

/// 签到接口路径（源实现 `completeClientTask`）
const TASK_COMPLETE_PATH: &str = "/autoclaw-proxy/proxy/autoclaw-task-complete";

/// 任务列表路径（源实现 `getTaskList`；`?lang=` 可选，省略时上游给默认语言）
const TASK_LIST_PATH: &str = "/autoclaw-proxy/proxy/autoclaw-task-list";

/// AutoClaw 账号的每日签到。
///
/// 返回 `{success, msg, ...}` —— `success` 的口径是「**本次真的领到了积分**」，
/// 而不是「HTTP 通了」。今天已领过时 `success: false` 且 msg 说明原因；
/// 前端把这条当 warn 提示显示（与 WorkBuddy 的「今天已签到」同一处理）。
pub async fn claim_daily_signin(
    store: &AccountStore,
    account_id: &str,
) -> Result<Value, GatewayError> {
    let record = store.autoclaw_account_record(account_id);
    if !account_id.is_empty() && record.is_none() {
        return Err(GatewayError::with_status(
            404,
            format!("AutoClaw 账号 {account_id} 不存在或不属于 AutoClaw"),
        ));
    }
    // 与积分查询同一条取凭证链（账号记录 → 桌面端实时登录态 → 环境变量）。
    let credentials = credentials::snapshot_for(record.as_ref())?;
    if credentials.token.trim().is_empty() {
        return Err(GatewayError::with_status(
            401,
            "该账号没有可用凭证，无法签到",
        ));
    }

    let body = json!({ "task_id": DAILY_SIGNIN_TASK_ID });
    let payload = userapi_post(&credentials, TASK_COMPLETE_PATH, &body, "签到").await?;

    // 业务码判定：签到走的是通用任务接口，登录态失效同样以业务码表达
    // （与积分查询同一组 AUTH_EXPIRED_CODES，见 balance.rs）。
    if let Some(code) = payload.get("code").and_then(Value::as_i64) {
        if super::balance::is_auth_expired_code(code) {
            return Err(GatewayError::with_status(
                401,
                "登录态已过期，无法签到",
            ));
        }
        if code != 0 {
            let message = payload
                .get("msg")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .unwrap_or("签到失败");
            return Ok(json!({ "success": false, "msg": message, "code": code }));
        }
    }

    let data = payload.get("data").cloned().unwrap_or(Value::Null);
    let already_completed = data
        .get("already_completed")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let reported_success = data.get("success").and_then(Value::as_bool).unwrap_or(false);
    let reward_points = data.get("reward_points").and_then(Value::as_i64).unwrap_or(0);

    // 「本次领到了」的判据是 reward_points > 0 或显式 success。**已领过不算成功**：
    // 上游对重复调用返回 success:false + already_completed:true，把它算成成功会让
    // 「今天已签到」在界面上显示成「签到成功」，与实际入账不符。
    let claimed = reward_points > 0 || (reported_success && !already_completed);
    if claimed {
        return Ok(json!({
            "success": true,
            "msg": if reward_points > 0 {
                format!("签到成功，获得 {reward_points} 积分")
            } else {
                "签到成功".to_string()
            },
            "rewardPoints": reward_points,
            "alreadyCompleted": false,
        }));
    }

    // 未领到：区分「今天已领过」与「上游异常」，前者是正常的幂等结果
    let msg = if already_completed {
        "今天已签到".to_string()
    } else {
        let task_hint = describe_task_state(&credentials).await;
        match task_hint {
            Some(hint) => format!("签到未领取（{hint}）"),
            None => "签到未领取".to_string(),
        }
    };
    Ok(json!({
        "success": false,
        "msg": msg,
        "rewardPoints": reward_points,
        "alreadyCompleted": already_completed,
    }))
}

/// 签到未领取时，拉一次任务列表给出归因（**只读**，且失败不升级为错误）。
///
/// 返回 None 表示「列表里没有可用信息」——调用方据此退回通用文案，
/// 不把一次辅助查询的失败拼接进用户看到的消息里。
async fn describe_task_state(
    credentials: &credentials::AutoClawCredentials,
) -> Option<String> {
    let payload = userapi_get(credentials, TASK_LIST_PATH, "任务列表查询").await.ok()?;
    if payload.get("code").and_then(Value::as_i64).unwrap_or(0) != 0 {
        return None;
    }
    let tasks = payload.get("data").and_then(Value::as_array)?;
    let task = tasks
        .iter()
        .find(|item| item.get("task_id").and_then(Value::as_str) == Some(DAILY_SIGNIN_TASK_ID))?;
    let completed = task
        .get("completed")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if completed {
        Some("任务列表显示今天已完成".to_string())
    } else {
        None
    }
}
