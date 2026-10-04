//! LobsterAI 的每日签到（feat/lobster-provider PR-2；移植来源 参考实现 的
//! `claim_daily_checkin` / `_fetch_checkin_state_live`，官方 App 同款三步）。
//!
//! ── 协议（activity 系统，三步）───────────────────────────────
//!   1. `GET /api/client-activities/slot?placement=desktop_sidebar&…`
//!      → `{data:{slotState:"available", activity:{activityCode, configRevision}}}`
//!   2. `GET /api/client-activities/{code}/context?configRevision={rev}`
//!      → `{data:{lifecycleState, state:{claimedToday, completed, claimedDays,
//!      totalDays, rewardCredits, claimedCredits, timezone}}}`
//!   3. `POST /api/client-activities/{code}/actions/check_in`
//!      body `{configRevision, idempotencyKey, payload:{}}`
//!
//! ── 幂等 ────────────────────────────────────────────────────
//!   - 本地判定 `claimedToday` 已领 → 直接返回「今日已签」；
//!   - 服务端错误码 **51104**（App 端 ActivityServerErrorCode 的
//!     AlreadyClaimed）视同幂等成功 —— 两端同时领（App 手快 / 昨天的守护在
//!     今天的时区边界附近补签）时上游会回它；
//!   - `idempotencyKey` 形态照抄 App：`daily-check-in-{uuid}`（≤64 字符）。
//!
//! ── 「今日」的口径 ──────────────────────────────────────────
//! 由服务端按活动时区判定（context.state.timezone，实测 Asia/Shanghai），
//! 本地不做日历计算 —— claimedToday 翻假后下一轮签到就能领上。
//!
//! ── 调度 ────────────────────────────────────────────────────
//! 本仓不另起守护线程：`billing::checkin::checkin_for` 的分派 + 定时签到
//! （`core::auto_checkin`，每天一轮）复用全仓同一条链路。参考实现 自带
//! 「00:00 + 随机偏移」守护是因为它没有调度系统；本仓的定时签到已把节奏、
//! 失败退避、范围勾选（`CHECKIN_PROVIDERS`）做成基础设施。
//!
//! ── panic=abort ────────────────────────────────────────────
//! 零 unwrap/expect/panic。

use serde_json::{json, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::core::auth_http::send_raw;
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::balance;
use super::credentials;

/// 活动接口超时（短请求；参考实现 同值 15s）
const REQUEST_TIMEOUT_MS: u64 = 15_000;

/// 签到活动的展示位（官方桌面端侧栏，参考实现 `ACTIVITY_CHECKIN_PLACEMENT`）
const CHECKIN_PLACEMENT: &str = "desktop_sidebar";

/// activity 容器 API 版本（App 端 NativeDailyCheckInV1，参考实现 同值）
const CONTAINER_API_VERSION: i64 = 2;

/// 客户端版本号：读不到 App 的 Info.plist 时用兜底（服务端只按形态校验，
/// 参考实现 同样「读不到就用 fallback」；这里不解析二进制 plist ——
/// 为一个展示参数引入 plist 解析不值当，版本老了上游也不会拒）。
const CLIENT_VERSION_FALLBACK: &str = "2026.8.28";

/// 服务端「今日已领」错误码（App 端 ActivityServerErrorCode）
const ERR_ALREADY_CLAIMED: i64 = 51104;

/// 一次签到的实时状态（slot → context 两步查回）
struct CheckinState {
    activity_code: String,
    config_revision: i64,
    /// 今日已领（服务端按活动时区判定）
    claimed_today: bool,
    /// 本期签到已全部完成（领满 totalDays）
    completed: bool,
    /// 今日奖励积分（展示用）
    reward_credits: f64,
    claimed_days: i64,
    total_days: i64,
    claimed_credits: f64,
}

/// 查签到实时状态；无可用活动时返回 None（文案由调用方给）。
async fn fetch_checkin_state(token: &str) -> Result<Option<CheckinState>, String> {
    // 第 1 步：slot（展示位 → 当前可用活动）
    let slot_query = format!(
        "?placement={CHECKIN_PLACEMENT}&clientVersion={CLIENT_VERSION_FALLBACK}\
&containerApiVersion={CONTAINER_API_VERSION}&platform=darwin"
    );
    let slot = authed_request("GET", &format!("/api/client-activities/slot{slot_query}"), None, token)
        .await?;
    let data = slot.get("data").cloned().unwrap_or(Value::Null);
    if slot.get("code").and_then(Value::as_i64) != Some(0)
        || data.get("slotState").and_then(Value::as_str) != Some("available")
    {
        return Ok(None);
    }
    let activity = data.get("activity").cloned().unwrap_or(Value::Null);
    let activity_code = activity
        .get("activityCode")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let config_revision = activity
        .get("configRevision")
        .and_then(Value::as_i64)
        .unwrap_or(-1);
    if activity_code.is_empty() || config_revision < 0 {
        return Ok(None);
    }
    // 第 2 步：context（活动状态：claimedToday / 进度 / 奖励）
    let context_path =
        format!("/api/client-activities/{activity_code}/context?configRevision={config_revision}");
    let context = authed_request("GET", &context_path, None, token).await?;
    let state = context
        .get("data")
        .and_then(|data| data.get("state"))
        .cloned()
        .unwrap_or(Value::Null);
    let number = |key: &str| {
        state
            .get(key)
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite())
            .unwrap_or(0.0)
    };
    Ok(Some(CheckinState {
        activity_code,
        config_revision,
        claimed_today: state.get("claimedToday") == Some(&Value::Bool(true)),
        completed: state.get("completed") == Some(&Value::Bool(true)),
        reward_credits: number("rewardCredits"),
        claimed_days: number("claimedDays") as i64,
        total_days: number("totalDays") as i64,
        claimed_credits: number("claimedCredits"),
    }))
}

/// 带鉴权请求（GET/POST），解包 `{code, message, data}`；网络/HTTP 失败给文案。
/// 401/403 抛 `GatewayError(401)`（调用方走刷新重试），其余失败给 502 文案。
async fn authed_request(
    method: &str,
    path: &str,
    body: Option<&Value>,
    token: &str,
) -> Result<Value, String> {
    let url = format!("{}{path}", super::DEFAULT_LLM_BASE_URL);
    let mut headers = vec![
        ("Accept".to_string(), "application/json".to_string()),
        (
            "Authorization".to_string(),
            format!("Bearer {}", token.trim()),
        ),
    ];
    if body.is_some() {
        headers.push(("Content-Type".to_string(), "application/json".to_string()));
    }
    let outcome = send_raw(method, &url, body, &headers, None, Some(REQUEST_TIMEOUT_MS)).await;
    let response = match outcome {
        Ok(response) => response,
        Err(error) => {
            return Err(if error.is_timeout() {
                "签到接口超时".to_string()
            } else {
                format!("签到接口请求失败: {error}")
            })
        }
    };
    let payload = response.payload.unwrap_or(Value::Null);
    if !response.ok {
        return Err(format!(
            "签到接口返回 HTTP {}: {}",
            response.status,
            payload
                .get("message")
                .or_else(|| payload.get("msg"))
                .and_then(Value::as_str)
                .unwrap_or("")
        ));
    }
    Ok(payload)
}

/// 领取每日签到（幂等：今日已领直接跳过；服务端 51104 亦视为已领）。
///
/// 返回形状与 WorkBuddy / 小浣熊的 claim 对齐：`{success, msg}` ——
/// `billing::checkin` 的汇总只认这两个字段。
pub async fn claim_daily_checkin(
    store: &AccountStore,
    account_id: &str,
) -> Result<Value, GatewayError> {
    // 签到是写操作：用 ensure 语义拿 token（临期先刷新，避免领到一半过期）
    let adapter_token = ensure_token(store, account_id).await?;
    let account_key = account_key_of(store, account_id);

    let state = fetch_checkin_state(&adapter_token)
        .await
        .map_err(|message| GatewayError::with_status(502, message))?;
    let Some(state) = state else {
        return Ok(json!({
            "success": false,
            "msg": "当前无可用签到活动",
        }));
    };
    if state.claimed_today {
        return Ok(json!({
            "success": true,
            "msg": "今日已签（App 或本网关此前已领）",
            "claimedDays": state.claimed_days,
            "totalDays": state.total_days,
            "claimedCredits": state.claimed_credits,
        }));
    }
    if state.completed {
        return Ok(json!({
            "success": true,
            "msg": "本期签到已完成",
            "claimedDays": state.claimed_days,
            "totalDays": state.total_days,
            "claimedCredits": state.claimed_credits,
        }));
    }
    // 第 3 步：领取（幂等键 + 配置版本号，payload 空对象 —— App 同款）
    let idempotency_key = format!(
        "daily-check-in-{}",
        crate::server::core::upstream::request::new_request_id()
    );
    let claim_path =
        format!("/api/client-activities/{}/actions/check_in", state.activity_code);
    let claim_body = json!({
        "configRevision": state.config_revision,
        "idempotencyKey": idempotency_key,
        "payload": {},
    });
    let claim = authed_request("POST", &claim_path, Some(&claim_body), &adapter_token)
        .await
        .map_err(|message| GatewayError::with_status(502, message))?;
    let code = claim.get("code").and_then(Value::as_i64).unwrap_or(-1);
    if code == 0 {
        balance::invalidate_quota_cache(&account_key);
        logging::log(
            "[Lobster]",
            &format!("✅ LobsterAI 签到成功 +{} 积分", state.reward_credits),
        );
        return Ok(json!({
            "success": true,
            "msg": format!("签到成功 +{} 积分", state.reward_credits as i64),
            "claimedDays": state.claimed_days + 1,
            "totalDays": state.total_days,
            "claimedCredits": state.claimed_credits + state.reward_credits,
        }));
    }
    if code == ERR_ALREADY_CLAIMED {
        // 服务端确认已领（与本地判定之间有时区/并发窗口）：按幂等成功收场
        balance::invalidate_quota_cache(&account_key);
        return Ok(json!({
            "success": true,
            "msg": "今日已签（服务端确认）",
            "claimedDays": state.claimed_days,
            "totalDays": state.total_days,
            "claimedCredits": state.claimed_credits,
        }));
    }
    let message = claim
        .get("message")
        .or_else(|| claim.get("msg"))
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .unwrap_or("未返回错误说明");
    Ok(json!({
        "success": false,
        "msg": format!("签到失败（code {code}）: {message}"),
    }))
}

/// 取可用 token（临期先刷新）。凭证缺失 / 刷新失败给 401 终态文案。
async fn ensure_token(store: &AccountStore, account_id: &str) -> Result<String, GatewayError> {
    let credentials = credentials::snapshot_for(store, account_id)?;
    if credentials.access_token.is_empty() {
        return Err(GatewayError::with_status(
            401,
            "该账号没有可用凭证，无法签到",
        ));
    }
    let refreshed = credentials::refresh(store, &credentials, false).await?;
    Ok(refreshed.access_token)
}

/// 额度缓存失效用的账号键（与 `balance::query_usage` 的缓存键同源）
fn account_key_of(store: &AccountStore, account_id: &str) -> String {
    if !account_id.is_empty() {
        return account_id.to_string();
    }
    store
        .lobster_account_record("")
        .and_then(|record| record.get("id").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| credentials::DESKTOP_ACCOUNT_ID.to_string())
}
