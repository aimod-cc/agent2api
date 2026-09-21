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
//!      **但 enabled 两条路径都要看**（见 ② 与 `resolve_checkin_targets` 的说明）——
//!      「不看 available」与「不看 enabled」是两回事：前者是「账号暂时不可用不影响
//!      手动操作」，后者是「用户明确禁用了它」，语义相反，不能一起照抄；
//!   ② `skipped` 的分母是「可用账号总数」，同时含「已禁用」与「国际版无签到」
//!      两类，与 /api/accounts/usage 的口径（只算被禁用的）**不同**。
//!
//! ── 本次修的 BUG：单账号路径漏查 enabled ────────────────────
//! 上面 ① 原先被过度执行成了「单账号路径什么状态都不看」，于是对已禁用的账号
//! 点「签到」会真的打上游签到接口。修复后单账号路径按 `enabled` → `supports_checkin`
//! 顺序检查、都返回 400；批量路径维持「过滤掉 + 计入 skipped」。
//! 「不看的只有 available」这一条**没有变**（那是原版 Node 的既定语义）。

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

/// 账号的提供商 id（缺失时按默认 provider 处理，与账号存储的兜底口径一致）。
///
/// 签到范围的判定按提供商分派：WorkBuddy 走腾讯的每日签到接口，小浣熊走
/// 桌面登录积分链路（`providers::raccoon::balance::claim_daily_grant`）——
/// 两家的接口互不相通，拿小浣熊的 token 去打腾讯的签到接口只会稳定报错。
fn provider_of(account: &Value) -> &str {
    account
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or(crate::server::core::providers::DEFAULT_PROVIDER_ID)
}

/// 该账号是否在本次签到的提供商范围内
fn matches_provider_filter(account: &Value, providers: &[String]) -> bool {
    providers.iter().any(|id| id == provider_of(account))
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
/// 批量（`id` 为空）：可用账号 ∩ 已启用 ∩ **提供商在 `providers` 范围内** ∩ 非国际版，
/// `skipped` = 可用总数 − 可签到数。范围由配置给出（WorkBuddy / 小浣熊可勾选），
/// 定时签到与账号页批量签到共用同一份口径。
///
/// 指定 id：命中即用（**不过滤 available，也不过滤 provider**），
/// 国际版与**已禁用**都直接报 400 —— 用户点的是谁就签谁，与「显式指定就执行」
/// 的既有语义一致；批量路径必须过滤，否则会把范围外的账号也签一遍。
///
/// ── 两条路径的「不满足条件」为什么语义不同（有意如此）──────────
///   批量路径 → **静默跳过**（计入 `skipped`）：定时任务会一次扫过几十个账号，
///     用户没在看着，为一个「国际版没有签到活动」把整轮任务报错没有意义。
///   单账号路径 → **明确 400 + 原因**：用户显式点了某个账号的按钮，
///     他需要知道为什么不行。静默成功或静默跳过都会让他以为签到了。
/// 所以两种检查（`supports_checkin` 与 `enabled`）在单账号路径都是报错，
/// 在批量路径都是过滤掉 —— **不要为了「统一」把其中一处改掉**。
///
/// ── `enabled` 检查是本次修的 BUG ──────────────────────────────
/// 改造前单账号路径只查了 `supports_checkin`，没有查 `enabled`：用户对
/// **已禁用**的账号点「签到」会真的打上游签到接口。禁用是「别用它转发」的意思，
/// 而签到会消耗上游的每日额度、并写回 `checkinAt`（前端据此显示「已签到」）——
/// 一个被禁用的账号不该产生这类副作用。原先的漏判让「禁用」这个动作漏了半张网。
///
/// 检查顺序与批量路径的过滤器顺序一致（`is_enabled` → `supports_checkin`）：
/// 两者用同一套判据、同一套顺序，读起来就是「同一份规则的两条出口」。
/// 顺序本身不影响结果（两个条件互不依赖），一致只是为了少一层认知负担。
pub fn resolve_checkin_targets(
    store: &AccountStore,
    providers: &[String],
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
        // 已禁用 → 400 并说明怎么恢复（而不是静默跳过）：
        // 用户显式点了按钮，必须给他一句能操作的话。文案与账号页
        // 明细面板里那句「账号已禁用，不参与批量签到；如需签到请先启用」
        // 同源同义（见 ui/accounts-model.js 的 checkinPanelHtml）——
        // 两处说法不一致会让用户以为遇到的是两个不同的问题。
        if !is_enabled(&found[0]) {
            return Err(CheckinError::new("账号已被禁用，请先启用后再签到", 400));
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
        .filter(|account| matches_provider_filter(account, providers))
        .collect();
    let skipped = total - eligible.len();
    Ok((eligible, skipped))
}

/// 单个账号签到。已签到（上游非 0 code）不算错误，原样返回结果 ——
/// 前端把「今天已签到」显示成一条 warn 提示。
///
/// ── 按提供商分派（三家的接口互不相通）────────────────────────
///   - **WorkBuddy**：计费服务的每日签到（`billing.claim_daily_checkin`）；
///   - **小浣熊**：桌面登录积分链路（`providers::raccoon::balance::claim_daily_grant`）；
///   - **AutoClaw**：通用任务接口的 `daily_signin` 任务
///     （`providers::autoclaw::checkin::claim_daily_signin`）。
///
/// 拿一家的 token 去打另一家的签到接口只会稳定报错，所以这条分派是必需的而不是
/// 优化。三个分支的收尾（claim → 结果行 + 日志）完全一致，共用 [`claim_result`]；
/// 各家的 claim 都由各自的实现对齐成 `{success, msg}` 形状。
pub async fn checkin_for(
    store: &AccountStore,
    billing: &BillingService,
    account: &Value,
) -> Value {
    let id = account.get("id").and_then(Value::as_str).unwrap_or("").to_string();
    let name = account.get("name").cloned().unwrap_or(Value::Null);
    let display = name.as_str().unwrap_or(&id).to_string();
    match provider_of(account) {
        "raccoon" => {
            let claim =
                crate::server::core::providers::raccoon::balance::claim_daily_grant(store, &id)
                    .await
                    .map_err(|error| error.message);
            claim_result(id, name, &display, true, claim)
        }
        "autoclaw" => {
            let claim =
                crate::server::core::providers::autoclaw::checkin::claim_daily_signin(store, &id)
                    .await
                    .map_err(|error| error.message);
            claim_result(id, name, &display, true, claim)
        }
        _ => {
            let Some(entry) = store.get_session_by_id(&id) else {
                return json!({
                    "id": id,
                    "name": name,
                    "claim": Value::Null,
                    "error": "没有可用凭证",
                });
            };
            let claim = billing
                .claim_daily_checkin(Some(&entry.session))
                .await
                .map_err(|error| error.message);
            claim_result(id, name, &display, false, claim)
        }
    }
}

/// 把一次签到调用翻成统一的结果行（`{id, name, claim, error}`）。
///
/// ── `log_success_msg` 为什么是一个参数而不是统一口径 ─────────
/// 小浣熊与 AutoClaw 的 claim `msg` 带**具体收益**（「今日积分 +100」
/// 「签到成功，获得 100 积分」），拼进日志才有排查价值；WorkBuddy 保持原样
/// （照抄 Node 版，不在这里做「顺手统一」—— 那会改变它既有的日志文案，
/// 而日志是用户已经在看的输出）。失败分支三家一致。
fn claim_result(
    id: String,
    name: Value,
    display: &str,
    log_success_msg: bool,
    result: Result<Value, String>,
) -> Value {
    match result {
        Ok(claim) => {
            let success = claim.get("success").and_then(Value::as_bool).unwrap_or(false);
            let msg = claim.get("msg").and_then(Value::as_str).unwrap_or("");
            if success {
                if log_success_msg && !msg.is_empty() {
                    logging::log("[Accounts]", &format!("账号 {display}: 签到成功（{msg}）"));
                } else {
                    logging::log("[Accounts]", &format!("账号 {display}: 签到成功"));
                }
            } else {
                logging::log("[Accounts]", &format!("账号 {display}: 签到未领取（{msg}）"));
            }
            json!({ "id": id, "name": name, "claim": claim, "error": Value::Null })
        }
        Err(message) => {
            logging::verbose("[Accounts]", &format!("账号 {id} 签到失败: {message}"));
            json!({
                "id": id,
                "name": name,
                "claim": Value::Null,
                "error": message,
            })
        }
    }
}

/// 这次签到是否意味着「今天已经签过了」—— 决定要不要落 `checkinAt`。
///
/// 三种情况都算，因为它们在「今天不能再领」这件事上没有区别：
///   1. `success === true`：本次真的领到了；
///   2. `alreadyCompleted === true`：上游明确告知今天已完成
///      （AutoClaw 的 `daily_signin` 会带这个字段，见 `providers::autoclaw::checkin`）；
///   3. `msg` 里含「已签到 / 已领取」：WorkBuddy 只把「今日已签到」放在文案里，
///      没有专门的码位可用，所以这里只能看文案。
///
/// 小浣熊不需要第 3 条：它的「今天已领过」是通过**账单核对**发现的 ——
/// 今天有入账记录就会算出 `granted_today > 0`，于是自然落到第 1 条。
///
/// ── 为什么不能宽到「只要没报错就算」─────────────────────────
/// 失败行（网络错误 / 凭证失效 / 5xx）与「今天已签到」是两回事：前者意味着今天
/// 可能一次都没签上，把它当作已签到去置灰按钮会白丢一天。这不是假想 ——
/// 实测见过自动签到 4 个账号全部未领取、17 秒后手动逐个重试全部成功
/// （批次撞上上游风控），所以判据必须收紧到「上游说签过了」。
fn checkin_completed_today(claim: &Value) -> bool {
    if claim.get("success").and_then(Value::as_bool) == Some(true) {
        return true;
    }
    if claim.get("alreadyCompleted").and_then(Value::as_bool) == Some(true) {
        return true;
    }
    let message = claim.get("msg").and_then(Value::as_str).unwrap_or("");
    message.contains("已签到") || message.contains("已领取")
}

/// 执行一次签到并汇总（Node 版 `runCheckin(id)`）。
///
/// `id` 为 None 时签全部符合条件的账号（定时签到走这条），范围由 `providers`
/// 决定（配置里勾选的提供商，缺省全选；**指定 id 单签时不受范围限制**）。
/// **串行**：避免多账号同时打上游触发 11128 风控。
pub async fn run_checkin(
    store: &AccountStore,
    billing: &BillingService,
    providers: &[String],
    id: Option<&str>,
) -> Result<Value, CheckinError> {
    let (targets, skipped) = resolve_checkin_targets(store, providers, id)?;
    let mut results = Vec::with_capacity(targets.len());
    for account in &targets {
        let row = checkin_for(store, billing, account).await;
        // 落签到时间：手动单签与定时签到走的是**这一段**（两条链都调本函数），
        // 所以账号页的「已签到」在两种路径下都会亮起来，不需要各自记一次。
        // 写盘失败只记日志、不改签到结果 —— 上游那边积分已经领到了，
        // 因为一次落盘失败就把成功的签到报成失败是本末倒置。
        if let Some(account_id) = row.get("id").and_then(Value::as_str) {
            let completed = row
                .get("claim")
                .map(checkin_completed_today)
                .unwrap_or(false);
            if completed && !store.mark_checkin(account_id, logging::now_ms()) {
                logging::verbose(
                    "[Accounts]",
                    &format!("账号 {account_id} 的签到时间未能落盘（账号可能已被删除）"),
                );
            }
        }
        results.push(row);
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
