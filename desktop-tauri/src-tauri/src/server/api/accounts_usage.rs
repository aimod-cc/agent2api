//! `/api/accounts/usage` 的实现（**薄壳**：查询逻辑已下沉到
//! `core::usage_query`，见那里的模块头）。
//!
//! 本文件只做两件事：调 core 拿结果、把 `TargetError` 转成管理信封的响应。
//! 目标集合解析、跨账号并发、单账号失败收敛、401 的刷新重试全在 core
//! ——「定时查询积分」也要走同一份逻辑，而它不认识 axum。
//!
//! ── 额外的一条：定时查询的结果快照 ─────────────────────────
//! `GET /api/accounts/usage/snapshot` 读 `core::usage_query` 里的内存快照，
//! 供界面在**不点按钮**的情况下拿到定时那一轮的最新余额（含失败行）。

use axum::response::Response;

use crate::server::core::usage_query;
use crate::server::errors::management_error;
use crate::server::http::ok_json;
use crate::server::ServerState;

/// GET /api/accounts/usage
///
/// 逐账号并发查询余额 / 积分汇总（`{ results: [{id,name,usage,error,code?}], skipped }`）。
/// 用户手动点「查询积分」走这条；查询行为见 `core::usage_query::query_all`。
pub async fn accounts_usage(state: &ServerState) -> Response {
    match usage_query::query_all(state.store()).await {
        Ok(report) => ok_json(report),
        Err(error) => management_error(error.status_code as i32, error.message),
    }
}

/// GET /api/accounts/usage/snapshot
///
/// 最近一次**定时查询**的结果快照
/// （`{ at, results: [...], skipped }`，`at` 是毫秒时间戳、0 表示本进程还没查过）。
///
/// 形状与 `/api/accounts/usage` 的响应**逐字一致**（多一个 `at`）：
/// 前端因此可以用同一个 `applyBalances` 写缓存，不必为「手动」与「定时」两条
/// 来源各写一套解析。多出来的 `at` 只用于「这份快照我应用过了没」的判断。
pub async fn accounts_usage_snapshot() -> Response {
    ok_json(usage_query::snapshot())
}
