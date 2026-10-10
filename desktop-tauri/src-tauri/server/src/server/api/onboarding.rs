//! 新手任务的两个管理端点（查询 / 领取），按账号的提供商分派执行体。
//!
//! ```text
//!   GET  /api/accounts/{id}/onboarding        任务状态（只读）
//!   POST /api/accounts/{id}/onboarding/claim  一键领取全部未完成任务
//! ```
//!
//! ── 为什么查询是 GET、领取是 POST ───────────────────────────
//! 与本仓「账号 + 子动作」后缀路由的既有形状一致：读走 GET（可收藏、可重放、
//! 无副作用），写走 POST（`onboarding/claim` 不以 `/onboarding` 结尾，两条
//! 后缀互不包含，分派顺序无所谓）。分派接线在 `api::accounts::dispatch`
//! 的 GET 后缀段与 POST 后缀段各一条。
//!
//! ── 按提供商分派 ────────────────────────────────────────────
//! 新手任务目前三家有：Loomy（任务表 + 逐项上报，`loomy::onboarding`）、
//! 小浣熊（首次登录奖励双任务探测 —— 电脑端 / 手机端各一条端点，
//! `raccoon::onboarding`），以及 CodeArts（运营活动里的**新人注册礼**这一条，
//! `codearts::onboarding` —— 与每日福利共用 delivery/claim/confirm 执行体）。
//! 分派键是账号记录里的 provider 字段；查不到记录或缺省一律走 Loomy ——
//! 与既有行为一致（`loomy_account_record` 会对别家 id 报 404，兜底语义不变）。
//!
//! 本家的两条"自动"要分清：后端的**定时签到**（`auto_checkin`）不含 codearts，
//! 而面板上签到后的**自动补领新手任务**含它 —— 与 Loomy / 小浣熊同构，
//! 且因为上游领过之后不再回 `claimable`，那条自动路径总共只会发一次写请求。
//! 界面侧的口径在 `ui-islands/src/islands/checkin-state.ts`。
//!
//! ── 与签到的关系（为什么不在 checkin 里顺手领）──────────────
//! 签到是每日动作且与定时签到共用同一段执行体（`core::billing::checkin`），
//! 把一次性的新手任务塞进去，会让之后每一轮定时签到都白打查询接口。界面侧
//! 在签到完成后自己查一次、有未领取才自动领取（见 ui-islands 的
//! checkin-state），领完自然不再弹。
//!
//! ── 一次性福利的结算记忆 ─────────────────────────────────────
//! 三家都是**一次性**福利：领完就没了。全部结清后账号记录上留一份记忆
//! （`onboardingSettled`；小浣熊是它自己的 `onboardingGrants` 台账派生），
//! 之后的查询与领取**零上游请求**（见 `providers::onboarding_memory`）。
//! 手动「查询任务」按钮带 `?refresh=1` 强制实查 —— 那是唯一的"重新问上游"
//! 入口，也是记忆被覆盖 / 清除的路径。
//!
//! 响应形状（三家对齐）：
//!   · status  → `{tasks:[{key,title,group,points,done,blocked?}], earned, total,
//!                 unclaimed, settled}`
//!   · claim   → `{results:[{key,ok,already?,error?}], claimed, failed,
//!                 claimedPoints, tasks, earned, total, unclaimed, settled}`

use serde_json::Value;

use axum::response::Response;

use crate::server::core::account_store::AccountStore;
use crate::server::core::providers::{
    codearts::onboarding as codearts_onboarding, loomy::onboarding as loomy_onboarding,
    officeace::onboarding as officeace_onboarding, raccoon::onboarding as raccoon_onboarding,
};
use crate::server::errors::management_error;
use crate::server::http::ok_json;
use crate::server::ServerState;

/// 小浣熊（raccoon）新手任务的分派键（`providers::ProviderMeta` 的 id）
const RACCOON_PROVIDER_ID: &str = "raccoon";
/// CodeArts 的分派键：它的新手任务是运营活动里的**新人注册礼**那一条，
/// 走 `providers::codearts::onboarding`（与每日福利共用执行体，只换挑哪一条活动）。
/// 常量与本家共用一处，免得界面认的 id 和存储写的 id 分叉。
const CODEARTS_PROVIDER_ID: &str =
    crate::server::core::account_store::codearts_accounts::CODEARTS_PROVIDER_ID;
/// OfficeAce 的分派键：它的新手任务与每日签到是**同一个上游动作**
/// （`POST /v1/subscription/bonus/claim` 一次发当天所有奖励），见
/// `officeace::onboarding` 的模块头。
const OFFICEACE_PROVIDER_ID: &str =
    crate::server::core::account_store::OFFICEACE_PROVIDER_ID;

/// 从账号存储里查一条账号的 provider id（查不到账号给空串 —— 分派会落到
/// Loomy 分支，由 `loomy_account_record` 报 404，与既有兜底行为一致）。
fn provider_of_account(state: &ServerState, account_id: &str) -> String {
    state
        .store()
        .list_accounts()
        .get("accounts")
        .and_then(Value::as_array)
        .and_then(|accounts| {
            accounts
                .iter()
                .find(|account| account.get("id").and_then(Value::as_str) == Some(account_id))
                .and_then(|account| account.get("provider").and_then(Value::as_str))
        })
        .unwrap_or("")
        .to_string()
}

/// 账号的「已结算」记忆视图（签到中心快照用；已结算 ⇒ 零上游直接渲染，见
/// `core::providers::onboarding_memory`）。三家形状一致，消费方不必按 provider
/// 分叉；没结算 / 未领完 → None。
///
/// 分派键与下面两个端点同一口径：provider 字符串直接来自账号记录，未知一律
/// 走 Loomy（`loomy_account_record` 会对别家 id 返回 None，天然给出 None）。
pub fn settled_view(store: &AccountStore, account_id: &str, provider: &str) -> Option<Value> {
    match provider {
        RACCOON_PROVIDER_ID => {
            raccoon_onboarding::settled_view(store.raccoon_account_record(account_id).as_ref())
        }
        CODEARTS_PROVIDER_ID => {
            codearts_onboarding::settled_view(store.codearts_account_record(account_id).as_ref())
        }
        _ => loomy_onboarding::settled_view(store.loomy_account_record(account_id).as_ref()),
    }
}

/// `GET /api/accounts/{id}/onboarding` —— 任务状态快照。
///
/// `refresh`（`?refresh=1`）强制实查上游：只在用户手点「查询任务」时置位，
/// 其余（进页面、签到后自动补领）都吃结算记忆。
pub async fn status(state: &ServerState, account_id: &str, refresh: bool) -> Response {
    let result = match provider_of_account(state, account_id).as_str() {
        RACCOON_PROVIDER_ID => {
            raccoon_onboarding::get_tasks(state.store(), account_id, refresh).await
        }
        CODEARTS_PROVIDER_ID => {
            codearts_onboarding::get_tasks(state.store(), account_id, refresh).await
        }
        OFFICEACE_PROVIDER_ID => {
            officeace_onboarding::get_tasks(state.store(), account_id, refresh).await
        }
        _ => loomy_onboarding::get_tasks(state.store(), account_id, refresh).await,
    };
    match result {
        Ok(data) => ok_json(data),
        Err(error) => management_error(error.status_code, error.message),
    }
}

/// `POST /api/accounts/{id}/onboarding/claim` —— 领取全部未完成任务。
/// 三家执行体自身都幂等，重复调用不会重复加分。
pub async fn claim(state: &ServerState, account_id: &str) -> Response {
    let result = match provider_of_account(state, account_id).as_str() {
        RACCOON_PROVIDER_ID => raccoon_onboarding::claim_all(state.store(), account_id).await,
        CODEARTS_PROVIDER_ID => codearts_onboarding::claim_all(state.store(), account_id).await,
        OFFICEACE_PROVIDER_ID => officeace_onboarding::claim_all(state.store(), account_id).await,
        _ => loomy_onboarding::claim_all(state.store(), account_id).await,
    };
    match result {
        Ok(data) => ok_json(data),
        Err(error) => management_error(error.status_code, error.message),
    }
}
