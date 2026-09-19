//! 余额 / 积分查询：目标集合解析、跨账号并发查询、单账号失败收敛。
//!
//! 余额查询是**四家混查**：目标集合跨全部启用账号，逐账号按所属 provider
//! 分流到 `ProviderAdapter::query_usage`。本模块只负责并发调度与单账号失败的
//! 收敛 ——「这个账号的余额怎么查」是 provider 知识，全在各家适配器里。
//!
//! ── 本模块为什么在 core ─────────────────────────────────────
//! 它有两条调用点：`GET /api/accounts/usage`（用户手动点「查询积分」）与
//! `core::scheduled_tasks` 的「定时查询积分」（后端按间隔自动查）。后者不认识
//! axum，因此查询逻辑不能长在 api 层 —— 与「定时签到与
//! `POST /api/accounts/checkin` 共用 `core::billing::checkin`」同一取舍：
//! 规则只有一份，两条入口各自持有 store 句柄调用它。
//!
//! ── 定时那一轮的结果快照 ────────────────────────────────────
//! 定时查询的结果存进本模块的内存快照（`store_snapshot`），由
//! `GET /api/accounts/usage/snapshot` 供界面读取 —— 于是用户不点按钮也能看到
//! 最新的余额，**失败的行同样进快照**（界面按「查询失败」渲染，不静默丢掉）。
//! 快照不落盘：它是「本次运行的观察值」，重启后定时任务自会再查一次
//!（与 `scheduled_tasks` 的运行状态同一取舍）。
//!
//! 本文件是 `api::accounts_usage` 的下沉版本：那里原先是「查询 + 结果组装」
//! 全在一起，现在只剩把结果转成 HTTP 响应的薄壳，判据（`supports_usage` 的
//! 能力过滤、401 的刷新重试、`skipped` 的口径）都跟着逻辑搬到了这里。

use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::core::providers::adapter::adapter_for;
use crate::server::logging;

/// 余额查询失败的原因：区分「没有凭证」（Node 的早退分支，少 name 键）与
/// 「请求失败」（带 name 键）。第二个字段是给前端的**机器可识别标记**（当前
/// 只有「未配置查询凭证」用，见 `adapter::USAGE_NOT_CONFIGURED_CODE`）：前端据此
/// 把这一条显示成中性提示而不是红色失败，比按文案匹配可靠。
enum UsageFailure {
    /// 账号没有可用凭证 —— Node 里那个 `return { id, usage:null, error }`
    NoCredentials,
    Request(String, Option<String>),
}

/// 目标集合解析失败。
///
/// core 不认识 axum，因此这里带 `message` / `status_code` 两个字段，由 api 层
/// 转成管理信封的响应 —— 而不是在 core 里造一个 `Response`。
#[derive(Clone, Debug)]
pub struct TargetError {
    pub message: String,
    pub status_code: u16,
}

/// 批量操作的目标集合：**全部可用账号中被启用的那些**，外加被跳过的数量。
///
/// `provider`：`Some(id)` 只取该家；`None` 跨四家取（**余额查询**用它 ——
/// 四家的余额接口各不相同、由各自适配器负责）。显式指定 id 时**不做过滤**
/// （与「显式指定就执行」的既有语义一致）。
///
/// 从 `api::accounts` 下沉（原 `resolve_batch_targets`）：它是纯粹的数据判定，
/// 不含任何 HTTP 语义，而「定时查询积分」必须与手动查询用**同一份口径**
/// —— 两条路径各写一份，「跳过了几个账号」这种算法迟早会漂。
///
/// 返回 `(targets, skipped)`：`skipped` 是「可用账号里被禁用掉的数量」。
pub fn resolve_batch_targets(
    store: &AccountStore,
    provider: Option<&str>,
    id: Option<&str>,
) -> Result<(Vec<Value>, usize), TargetError> {
    let snapshot = store.list_accounts();
    let accounts: Vec<Value> = snapshot
        .get("accounts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // provider 缺失的账号按默认 provider 归属（与 store 的兜底口径一致：
    // 旧记录没有这个字段），否则它们会在两种过滤下都被漏掉
    let in_scope = |account: &Value| match provider {
        None => true,
        Some(provider) => {
            account
                .get("provider")
                .and_then(Value::as_str)
                .unwrap_or(crate::server::core::providers::DEFAULT_PROVIDER_ID)
                == provider
        }
    };
    let is_available = |account: &Value| {
        account.get("available").and_then(Value::as_bool).unwrap_or(true)
    };
    let is_enabled = |account: &Value| {
        account.get("enabled").and_then(Value::as_bool).unwrap_or(true)
    };
    if let Some(id) = id.filter(|value| !value.is_empty()) {
        let found: Vec<Value> = accounts
            .iter()
            .filter(|account| account.get("id").and_then(Value::as_str) == Some(id))
            .cloned()
            .collect();
        if found.is_empty() {
            return Err(TargetError { message: "账号不存在".to_string(), status_code: 404 });
        }
        return Ok((found, 0));
    }
    let available: Vec<Value> = accounts
        .into_iter()
        .filter(in_scope)
        .filter(is_available)
        .collect();
    let skipped = available.len();
    let targets: Vec<Value> = available.into_iter().filter(is_enabled).collect();
    let skipped = skipped - targets.len();
    Ok((targets, skipped))
}

/// 单账号余额查询：任何失败都收敛为 `{error}` 而不是抛出（批量查询不被单账号
/// 拖垮），刷新重试在 `query_usage_inner` 里。
async fn query_usage_for(store: &AccountStore, account: &Value) -> Value {
    let id = account.get("id").and_then(Value::as_str).unwrap_or("").to_string();
    let name = account.get("name").cloned().unwrap_or(Value::Null);
    // 凭证与 provider 都从目标集合里的账号对象读，**不回读账号文件**：
    // 20 个账号并发时那次回读会变成 20 次整库读盘，而答案已在手上。
    // `hasCredentials` 是 store 逐条给出的统一判据（缺省视为有）。
    let has_credentials = account.get("hasCredentials").and_then(Value::as_bool) != Some(false);
    let outcome = if has_credentials {
        let provider_id = account
            .get("provider")
            .and_then(Value::as_str)
            .unwrap_or(crate::server::core::providers::DEFAULT_PROVIDER_ID);
        query_usage_inner(store, provider_id, &id).await
    } else {
        Err(UsageFailure::NoCredentials)
    };
    match outcome {
        Ok(usage) => json!({ "id": id, "name": name, "usage": usage, "error": Value::Null }),
        // Node 的这条早退 `return` **不含 name 键**（只有 catch 分支才带）
        Err(UsageFailure::NoCredentials) => {
            json!({ "id": id, "usage": Value::Null, "error": "没有可用凭证" })
        }
        Err(UsageFailure::Request(message, code)) => {
            logging::verbose("[Accounts]", &format!("账号 {id} 余额查询失败: {message}"));
            json!({ "id": id, "name": name, "usage": Value::Null, "error": message, "code": code })
        }
    }
}

/// 查询一个账号的余额 / 积分（**按账号所属 provider 分流到适配器**）。
///
/// ── 401：刷新后重试一次，但**只有能刷新的家才有这一步** ────────
/// 「401 说明 token 被服务端拒绝而非临期」——Node 版据此才走刷新重试，其它错误
/// 直接返回；四家的刷新协议各不相同，统一走 `refresh_access_token`（force 语义）。
///
/// 先问 `supports_refresh()` 是 **CatPaw 跳过重试的依据**：它上游没有刷新接口
/// （`X-Passport-Token` 过期只能在桌面端重新登录，见 `catpaw/adapter.rs`），对它
/// 的 401 做刷新重试会稳定失败，把一条「凭证过期」变成两条错误（刷新失败的信息
/// 盖住真正原因，用户反而看不出该做什么）。
///
/// workbuddy 也走适配器：它的 `query_usage` 转调既有计费服务（结果形状不变）。
async fn query_usage_inner(
    store: &AccountStore,
    provider_id: &str,
    id: &str,
) -> Result<Value, UsageFailure> {
    // 注册表里没有的 id（前端比后端新、或手改过的账号文件）：明确报「未知的
    // 提供商」，不猜成任何一家（与全仓的 provider 口径一致）
    let Some(kind) = crate::server::core::providers::kind_from_id(provider_id) else {
        return Err(UsageFailure::Request(
            format!("未知的提供商 {provider_id}，无法查询余额"),
            None,
        ));
    };
    let adapter = adapter_for(kind);
    match adapter.query_usage(store, id).await {
        Ok(usage) => Ok(usage),
        Err(error) => {
            // 不支持刷新的家直接返回原错误（CatPaw：重试必然失败，见上）
            if error.status_code != 401 || !adapter.supports_refresh() {
                return Err(UsageFailure::Request(error.message, error.code));
            }
            if let Err(refresh_error) = adapter.refresh_access_token(store, id).await {
                return Err(UsageFailure::Request(refresh_error.message, None));
            }
            adapter
                .query_usage(store, id)
                .await
                .map_err(|retry| UsageFailure::Request(retry.message, retry.code))
        }
    }
}

/// 该账号所属 provider 是否声明了余额能力（`ProviderAdapter::supports_usage`）。
///
/// 为什么批量查询要过滤掉「不支持」的家：不支持的实现会给**每个账号**产出一行
/// 501 —— 用户看到一片红，而那不是故障、只是能力缺失（当前四家都支持，
/// 但注册表是可扩展的）。判据是各家的恒定能力声明，不是这次请求成不成功。
/// 未知 provider id 一并跳过；显式指定 id 的调用路径仍会走到那条明确报错。
fn supports_usage(account: &Value) -> bool {
    let provider_id = account
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or(crate::server::core::providers::DEFAULT_PROVIDER_ID);
    crate::server::core::providers::kind_from_id(provider_id)
        .map(|kind| adapter_for(kind).supports_usage())
        .unwrap_or(false)
}

/// 逐账号并发查询余额 / 积分汇总
/// （`{ results: [{id,name,usage,error,code?}], skipped }`）。
///
/// 目标集合是**全部启用账号**（`resolve_batch_targets(provider = None)`），逐账号
/// 按 `provider` 分流到 `ProviderAdapter::query_usage`。改造前这条路径只查
/// workbuddy —— 三家账号被静默跳过，前端连按钮都不给。
///
/// **并发**是关键：Node 版用 `Promise.all`，20 个账号串行会让前端转圈 20 次
/// 往返。这里用 `join_all` 在同一个任务里并发轮询（每个 future 都是网络等待，
/// 天然交错），且**结果顺序与 targets 一致** —— 与 `Promise.all` 语义相同。
/// **超时由各适配器自己设**（15~20 秒）；这里不叠加第二层超时 —— 那会让
/// 「上游慢」与「网关掐断」在日志里无法区分。
pub async fn query_all(store: &AccountStore) -> Result<Value, TargetError> {
    // `provider = None`：跨四家取目标（签到那条仍按 provider 过滤）
    let (targets, skipped) = resolve_batch_targets(store, None, None)?;
    let futures: Vec<_> = targets
        .iter()
        .filter(|account| supports_usage(account))
        .map(|account| query_usage_for(store, account))
        .collect();
    let results = futures::future::join_all(futures).await;
    Ok(json!({ "results": results, "skipped": skipped }))
}

// ─── 定时查询的结果快照 ──────────────────────────────────────

/// 最近一次「定时查询积分」的结果。内含 `{at, results, skipped}`：
/// `at` 是那一刻的毫秒时间戳，界面按它判断「这份快照我应用过了没」。
static SNAPSHOT: OnceLock<Mutex<Option<Value>>> = OnceLock::new();

fn snapshot_slot() -> &'static Mutex<Option<Value>> {
    SNAPSHOT.get_or_init(|| Mutex::new(None))
}

/// 存下定时那一轮的查询结果，返回 `(成功数, 失败数)`。
///
/// **失败的行照样进快照**：定时查询的价值就在于「不点按钮也知道现在是好是坏」，
/// 把失败悄悄丢掉会让界面永远停在上一轮的旧余额上（那是比显示失败更糟的误导）。
///
/// 成功 / 失败的判据是 `usage` 键是否为 null —— 与前端 `cacheEntryOf` 同源
/// （它也是「有 usage 就是结果，否则是失败行」）。
pub fn store_snapshot(report: Value) -> (usize, usize) {
    let results = report
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let ok = results
        .iter()
        .filter(|row| row.get("usage").is_some_and(|usage| !usage.is_null()))
        .count();
    let failed = results.len().saturating_sub(ok);
    let snapshot = json!({
        "at": logging::now_ms(),
        "results": results,
        "skipped": report.get("skipped").cloned().unwrap_or(Value::from(0)),
    });
    // 锁中毒时沿用「不写」而不是 panic：少一次快照更新远比整个应用退出轻
    if let Ok(mut slot) = snapshot_slot().lock() {
        *slot = Some(snapshot);
    }
    (ok, failed)
}

/// 最近一次定时查询的快照。从未查过时给 `{at: 0, results: [], skipped: 0}` ——
/// 界面据此显示「还没有定时查询结果」，而不是把空数组当成「一个账号都没有」。
pub fn snapshot() -> Value {
    snapshot_slot()
        .lock()
        .ok()
        .and_then(|slot| slot.clone())
        .unwrap_or_else(|| json!({ "at": 0, "results": [], "skipped": 0 }))
}
