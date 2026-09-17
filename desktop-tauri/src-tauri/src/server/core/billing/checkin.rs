//! 账号级签到：目标集合解析 + 串行执行（对照 Node 版 workbuddy-account-routes.mjs
//! 的 `resolveCheckinTargets` / `checkinFor` / `runCheckin` 三个函数逐条移植）。
//!
//! ── 为什么下沉到 core ───────────────────────────────────────
//! 定时签到（core::auto_checkin，对照 workbuddy-auto-checkin.mjs）与
//! `POST /api/accounts/checkin` 必须是**同一段逻辑**。Node 版靠依赖注入做到这点：
//! `createAutoCheckin({ runCheckin: id => accountRoutes.runCheckin(id) })` ——
//! 调度器拿到的就是账号路由里那个函数，所以「限额跳过、国际版排除、串行防风」
//! 的规则只维护一份，不存在两套行为。
//!
//! Rust 侧的 core 不能依赖 api（core 不认识 axum，见 core/mod.rs 的约定），
//! 于是把这段共享逻辑放到这里：api/accounts.rs 与 core/auto_checkin 各自持有
//! store / billing 句柄调用它，规则依旧只有一份。调用方负责把 `CheckinError`
//! 翻成响应（api 层用管理信封，调度器只取 message 记进 lastResult）。
//!
//! ── 两处易错点（照抄 Node，不做「顺手统一」）─────────────────
//!   ① 指定 id 时**不看 available**：Node 的 `resolveTargets(id)` 从全量账号里
//!      `find` 命中即用，只有批量分支才过滤 `available !== false`；
//!   ② `skipped` 的分母是「可用账号总数」，同时含「已禁用」与「国际版无签到」
//!      两类，与 /api/accounts/usage 的口径（只算被禁用的）**不同**。

use serde_json::{json, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::core::billing::BillingService;
use crate::server::logging;

/// 签到路径上的错误。对应 Node 版抛出的 AccountStoreError：
/// 404「账号不存在」与 400「国际版账号暂不支持签到」。
#[derive(Clone, Debug)]
pub struct CheckinError {
    pub message: String,
    pub status_code: i32,
}

impl CheckinError {
    fn new(message: impl Into<String>, status_code: i32) -> Self {
        Self { message: message.into(), status_code }
    }
}

impl std::fmt::Display for CheckinError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.message)
    }
}

/// 国际版没有签到活动，签到相关操作一律排除该版本账号
/// （Node: `account.edition !== 'intl'`）
pub fn supports_checkin(account: &Value) -> bool {
    account.get("edition").and_then(Value::as_str) != Some("intl")
}

/// 账号快照里的「可用」判定（Node: `account.available !== false`）
fn is_available(account: &Value) -> bool {
    account
        .get("available")
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

/// 账号快照里的「启用」判定（Node: `account.enabled !== false`）
fn is_enabled(account: &Value) -> bool {
    account
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

/// 账号列表快照（`store.listAccounts().accounts`）
fn accounts_of(store: &AccountStore) -> Vec<Value> {
    store
        .list_accounts()
        .get("accounts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// 签到目标集合。
///
/// 批量（`id` 为空）：可用账号 ∩ 已启用 ∩ 非国际版，`skipped` = 可用总数 − 可签到数。
/// 指定 id：命中即用（**不过滤 available**），国际版直接报 400。
pub fn resolve_checkin_targets(
    store: &AccountStore,
    id: Option<&str>,
) -> Result<(Vec<Value>, usize), CheckinError> {
    let all = accounts_of(store);
    if let Some(id) = id.filter(|value| !value.is_empty()) {
        let found: Vec<Value> = all
            .into_iter()
            .filter(|account| account.get("id").and_then(Value::as_str) == Some(id))
            .collect();
        if found.is_empty() {
            return Err(CheckinError::new("账号不存在", 404));
        }
        if !supports_checkin(&found[0]) {
            return Err(CheckinError::new("国际版账号暂不支持签到", 400));
        }
        return Ok((found, 0));
    }
    let available: Vec<Value> = all.into_iter().filter(is_available).collect();
    let total = available.len();
    let eligible: Vec<Value> = available
        .into_iter()
        .filter(is_enabled)
        .filter(supports_checkin)
        .collect();
    let skipped = total - eligible.len();
    Ok((eligible, skipped))
}

/// 单个账号签到。已签到（上游非 0 code）不算错误，原样返回结果 ——
/// 前端把「今天已签到」显示成一条 warn 提示。
pub async fn checkin_for(
    store: &AccountStore,
    billing: &BillingService,
    account: &Value,
) -> Value {
    let id = account.get("id").and_then(Value::as_str).unwrap_or("").to_string();
    let name = account.get("name").cloned().unwrap_or(Value::Null);
    let Some(entry) = store.get_session_by_id(&id) else {
        return json!({ "id": id, "name": name, "claim": Value::Null, "error": "没有可用凭证" });
    };
    match billing.claim_daily_checkin(Some(&entry.session)).await {
        Ok(claim) => {
            let success = claim.get("success").and_then(Value::as_bool).unwrap_or(false);
            let display = name.as_str().unwrap_or(&id);
            if success {
                logging::log("[Accounts]", &format!("账号 {display}: 签到成功"));
            } else {
                let msg = claim.get("msg").and_then(Value::as_str).unwrap_or("");
                logging::log("[Accounts]", &format!("账号 {display}: 签到未领取（{msg}）"));
            }
            json!({ "id": id, "name": name, "claim": claim, "error": Value::Null })
        }
        Err(error) => {
            logging::verbose("[Accounts]", &format!("账号 {id} 签到失败: {}", error.message));
            json!({
                "id": id,
                "name": name,
                "claim": Value::Null,
                "error": error.message,
            })
        }
    }
}

/// 执行一次签到并汇总（Node 版 `runCheckin(id)`）。
///
/// `id` 为 None 时签全部符合条件的账号（定时签到走这条）。
/// **串行**：避免多账号同时打上游触发 11128 风控。
pub async fn run_checkin(
    store: &AccountStore,
    billing: &BillingService,
    id: Option<&str>,
) -> Result<Value, CheckinError> {
    let (targets, skipped) = resolve_checkin_targets(store, id)?;
    let mut results = Vec::with_capacity(targets.len());
    for account in &targets {
        results.push(checkin_for(store, billing, account).await);
    }
    let succeeded = results
        .iter()
        .filter(|item| {
            item.get("claim")
                .and_then(|claim| claim.get("success"))
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .count();
    if skipped > 0 {
        logging::log(
            "[Accounts]",
            &format!("已跳过 {skipped} 个账号（已禁用或国际版无签到活动）"),
        );
    }
    logging::log(
        "[Accounts]",
        &format!("签到完成: {succeeded}/{} 个账号成功领取", results.len()),
    );
    Ok(json!({
        "results": results,
        "succeeded": succeeded,
        "total": results.len(),
        "skipped": skipped,
    }))
}
