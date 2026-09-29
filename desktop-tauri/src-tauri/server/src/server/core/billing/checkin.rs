//! 账号级签到：目标集合解析 + 串行执行（对照 Node 版 workbuddy-account-routes.mjs
//! 的 `resolveCheckinTargets` / `checkinFor` / `runCheckin` 三个函数逐条移植）。
//!
//! ── 为什么下沉到 core ───────────────────────────────────────
//! 定时签到（core::auto_checkin，对照 workbuddy-auto-checkin.mjs）与
//! `POST /api/accounts/checkin` 必须是**同一段逻辑**。Node 版靠依赖注入做到这点：
//! `createAutoCheckin({ runCheckin: id => accountRoutes.runCheckin(id) })` ——
//! 调度器拿到的就是账号路由里那个函数，所以「范围过滤、版本分流、串行防风」
//! 的规则只维护一份，不存在两套行为。
//!
//! Rust 侧的 core 不能依赖 api（core 不认识 axum，见 core/mod.rs 的约定），
//! 于是把这段共享逻辑放到这里：api/accounts.rs 与 core/auto_checkin 各自持有
//! store / billing 句柄调用它，规则依旧只有一份。调用方负责把 `CheckinError`
//! 翻成响应（api 层用管理信封，调度器只取 message 记进 lastResult）。
//!
//! ── 签到不看 `enabled`（本次改动；此前两轮口径相反）─────────
//! `enabled` 管的是「别让这个账号承接转发」，签到则是用户对某个账号显式发起的
//! 一次动作（定时签到则是调度器对所有账号的统一动作），与转发无关：一个被禁用的
//! 账号依然可以每天签到攒积分。所以单账号与批量两条路径都**不看** `enabled` ——
//! 禁用账号照常进入签到目标集合，界面上照常有签到按钮。
//!
//! ── 历史（别又改回去）────────────────────────────────────────
//! 这里先后有过两种相反口径：先是单账号路径漏查 `enabled`（当时算 bug ——
//! 「显式指定就什么都不看」被过度执行了，于是对禁用账号点签到会真的打上游），
//! 修成「单账号 400 / 批量过滤」；再是现在这次全部放开。中间那版把「禁用转发」
//! 与「禁止签到」当成了一件事 —— 但签到消耗的是**积分额度**，与转发配额不是
//! 同一个池子，用户对禁用账号点「签到」本身就是明确意图，替他拦下来反而多余。
//!
//! 仍然要看的只剩两处，两条路径各自一致：`available`（批量路径过滤，单账号不看 ——
//! Node 版既定语义：账号暂时不可用不影响手动操作）与 `supports_checkin`。
//!
//! ── `skipped` 的分母 ────────────────────────────────────────
//! 「可用账号总数 − 可签到数」，只可能由**不支持签到/活跃任务的版本**与**范围外
//! 提供商**两类构成（`enabled` 不再参与），与 /api/accounts/usage 的「只算被禁用的」
//! 口径不同 ——
//! 两个动作的「不适用」集合本来就不一样。

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
        Self {
            message: message.into(),
            status_code,
        }
    }
}

impl std::fmt::Display for CheckinError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.message)
    }
}

/// 计算账号是否进入自动/手动签到目标集合。
///
/// WorkBuddy 国际版虽然没有国内版的常规签到面板，但参考客户端把它接到同一
/// `DailyCheckin` 调度器，执行「活动探测 + 条件领取 + 免费对话保活」，因此按
/// provider + edition 联合判断放行。其它 provider 继续沿用各自的地区能力判据。
///
/// Accio 系（两个地区）**整家**也没有签到活动：上游客户端全包检索不到
/// 「签到 / checkin / 每日任务」的任何痕迹（见 `providers::accio` 的模块头）。
/// 它按 **provider id** 排除而不是 edition —— 两个地区都没有活动，而 provider
/// 是落盘契约，不会因为凭证里多一个字段而改变判定。
///
/// 这一步是**必需的**：不在范围的家会落到 `checkin_for` 的分派里，拿另一家的
/// 令牌去打错的签到接口只会稳定报错（见那里的最后两条分支）。
pub fn supports_checkin(account: &Value) -> bool {
    let provider = account
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or(crate::server::core::providers::DEFAULT_PROVIDER_ID);
    // WorkBuddy 国内版走计费签到，国际版走每日活跃任务；两者都复用本入口。
    if provider == crate::server::core::providers::DEFAULT_PROVIDER_ID {
        return true;
    }
    // AutoClaw 国际版也有独立的 daily_signin 任务链路，不能被通用 intl 判据挡掉。
    if provider == crate::server::core::account_store::AUTOCLAW_INTL_PROVIDER_ID {
        return true;
    }
    if account.get("edition").and_then(Value::as_str) == Some("intl") {
        return false;
    }
    // CodeArts 与 Trae 两家都没有「签到」链路，必须先排除：
    // `checkin_for` 的分派 match 把「不在范围里的家」报成「未接入」，而这两家
    // 的按钮在界面上由能力位 `checkin: false` 收起 —— 这一层是批量路径
    // （`resolve_checkin_targets` 的 filter）与 API 直调的兜底，双保险。
    // 注意 CodeArts 的每日福利**不是**签到（那是 ops 福利领取，独立的「领福利」
    // 按钮，见 `providers::codearts::welfare`），与这条链无交集。
    if provider == crate::server::core::account_store::codearts_accounts::CODEARTS_PROVIDER_ID
        || provider == crate::server::core::account_store::TRAE_PROVIDER_ID
    {
        return false;
    }
    !crate::server::core::account_store::is_accio_family(provider)
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
/// 批量（`id` 为空）：可用账号 ∩ **提供商在 `providers` 范围内** ∩ 支持签到/活跃任务，
/// `skipped` = 可用总数 − 可执行数。范围由配置给出（WorkBuddy / 小浣熊 / AutoClaw
/// 可勾选），定时签到与账号页批量签到共用同一份口径。**禁用账号照常参与** ——
/// 签到与转发是两件事（见模块头「签到不看 enabled」）。
///
/// 指定 id：命中即用（**不过滤 available，也不过滤 provider**），
/// 不支持签到/活跃任务的账号直接报 400 —— 用户点的是谁就执行谁，与「显式指定就执行」的既有语义一致；
/// 批量路径必须过滤 available 与 provider，否则会把范围外的账号也签一遍。
///
/// ── 两条路径的「不满足条件」为什么语义不同（有意如此）──────────
///   批量路径 → **静默跳过**（计入 `skipped`）：定时任务会一次扫过几十个账号，
///     用户没在看着，为一个「不支持签到/活跃任务」把整轮任务报错没有意义。
///   单账号路径 → **明确 400 + 原因**：用户显式点了某个账号的按钮，
///     他需要知道为什么不行。静默成功或静默跳过都会让他以为签到了。
/// 所以 `supports_checkin` 在单账号路径报错、在批量路径过滤掉 ——
/// **不要为了「统一」把其中一处改掉**。
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
        // 账号页明细面板与这里共用「不支持签到」语义；WorkBuddy 国际版已经
        // 由活跃任务分支放行，Qoder 等其它不支持的版本仍在这里给出明确 400。
        if !supports_checkin(&found[0]) {
            return Err(CheckinError::new("该账号暂不支持签到或每日活跃任务", 400));
        }
        return Ok((found, 0));
    }
    let available: Vec<Value> = all.into_iter().filter(is_available).collect();
    let total = available.len();
    let eligible: Vec<Value> = available
        .into_iter()
        .filter(supports_checkin)
        .filter(|account| matches_provider_filter(account, providers))
        .collect();
    let skipped = total - eligible.len();
    Ok((eligible, skipped))
}

/// 单个账号签到。已签到（上游非 0 code）不算错误，原样返回结果 ——
/// 前端把「今天已签到」显示成一条 warn 提示。
///
/// ── 按提供商分派（四家的接口互不相通）────────────────────────
///   - **WorkBuddy 国内版**：计费服务的每日签到（`billing.claim_daily_checkin`）；
///   - **WorkBuddy 国际版**：活跃探测、条件领取与免费模型保活
///     （`billing.workbuddy_daily_activity`）；
///   - **小浣熊**：桌面登录积分链路（`providers::raccoon::balance::claim_daily_grant`）；
///   - **AutoClaw**：通用任务接口的 `daily_signin` 任务
///     （`providers::autoclaw::checkin::claim_daily_signin`）；
///   - **Qoder**：活动（campaign）领取链路，只有中国版有
///     （`providers::qoder::checkin::claim_daily_checkin`）。
///
/// 拿一家的 token 去打另一家的签到接口只会稳定报错，所以这条分派是必需的而不是
/// 优化。各分支的收尾（claim → 结果行 + 日志）完全一致，共用 [`claim_result`]；
/// 各家的 claim 都由各自的实现对齐成 `{success, msg}` 形状。最后的兜底**只认**
/// 默认那家（WorkBuddy），未知家明确报「未接入」——见那里的说明。
pub async fn checkin_for(store: &AccountStore, billing: &BillingService, account: &Value) -> Value {
    let id = account
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let name = account.get("name").cloned().unwrap_or(Value::Null);
    let display = name.as_str().unwrap_or(&id).to_string();
    // 分派的键就是账号的 provider id（`provider_of` 已归一）；AutoClaw 两个
    // 地区各是一个 provider，因此下面按 `region.provider_id()` 反查地区，
    // 而不是写死 `"autoclaw"`（那样国际版账号会掉进 `_` 分支）
    let provider_id = provider_of(account);
    match provider_id {
        "raccoon" => {
            let claim =
                crate::server::core::providers::raccoon::balance::claim_daily_grant(store, &id)
                    .await
                    .map_err(|error| error.message);
            claim_result(id, name, &display, true, claim)
        }
        "autoclaw" | "autoclaw-intl" => {
            let region =
                crate::server::core::providers::autoclaw::Region::from_provider_id(provider_id)
                    .unwrap_or(crate::server::core::providers::autoclaw::Region::Cn);
            let claim = crate::server::core::providers::autoclaw::checkin::claim_daily_signin(
                region, store, &id,
            )
            .await
            .map_err(|error| error.message);
            claim_result(id, name, &display, true, claim)
        }
        "qoder" => {
            // 中国版的每日权益以活动（campaign）形式下发；国际版由
            // `supports_checkin` 挡在普通签到分支，保持其它提供商的地区判据。
            let claim =
                crate::server::core::providers::qoder::checkin::claim_daily_checkin(store, &id)
                    .await
                    .map_err(|error| error.message);
            claim_result(id, name, &display, true, claim)
        }
        // WorkBuddy 国内版与国际版共用 provider id，按 edition 选择任务形态。
        _ if provider_id == crate::server::core::providers::DEFAULT_PROVIDER_ID => {
            let Some(entry) = store.get_session_by_id(&id) else {
                return json!({
                    "id": id,
                    "name": name,
                    "claim": Value::Null,
                    "error": "没有可用凭证",
                });
            };
            if account.get("edition").and_then(Value::as_str) == Some("intl") {
                let activity = billing.workbuddy_daily_activity(&entry.session).await;
                let claim = activity.get("claim").cloned().unwrap_or_else(
                    || json!({ "success": false, "code": -1, "msg": "活跃保活失败" }),
                );
                let claim_success = claim
                    .get("success")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let message = claim.get("msg").and_then(Value::as_str).unwrap_or("");
                if claim_success {
                    logging::log("[Accounts]", &format!("账号 {display}: 签到成功"));
                } else {
                    logging::log(
                        "[Accounts]",
                        &format!("账号 {display}: WorkBuddy 国际版 {message}"),
                    );
                }
                return json!({
                    "id": id,
                    "name": name,
                    "claim": claim,
                    "activity": activity.get("activity").cloned().unwrap_or(Value::Null),
                    "error": Value::Null,
                });
            }
            let claim = billing
                .claim_daily_checkin(Some(&entry.session))
                .await
                .map_err(|error| error.message);
            claim_result(id, name, &display, false, claim)
        }
        other => json!({
            "id": id,
            "name": name,
            "claim": Value::Null,
            "error": format!("{other} 的签到链路尚未接入"),
        }),
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
            let success = claim
                .get("success")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let msg = claim.get("msg").and_then(Value::as_str).unwrap_or("");
            if success {
                if log_success_msg && !msg.is_empty() {
                    logging::log("[Accounts]", &format!("账号 {display}: 签到成功（{msg}）"));
                } else {
                    logging::log("[Accounts]", &format!("账号 {display}: 签到成功"));
                }
            } else {
                logging::log(
                    "[Accounts]",
                    &format!("账号 {display}: 签到未领取（{msg}）"),
                );
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
    // 「真实领取」与「今日已领取」都表示本日签到已完成；只有活跃保活不计入
    // succeeded，避免把普通签到时间与活跃任务混为一谈。
    let succeeded = results
        .iter()
        .filter(|item| {
            item.get("claim")
                .map(checkin_completed_today)
                .unwrap_or(false)
        })
        .count();
    let active = results
        .iter()
        .filter(|item| {
            item.get("activity")
                .and_then(|activity| activity.get("pokeSucceeded"))
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .count();
    if skipped > 0 {
        logging::log(
            "[Accounts]",
            &format!("已跳过 {skipped} 个账号（不支持签到/活跃任务或不在签到范围内）"),
        );
    }
    logging::log(
        "[Accounts]",
        &format!(
            "签到完成: {succeeded}/{} 个账号成功领取，{active} 个账号完成活跃保活",
            results.len()
        ),
    );
    Ok(json!({
        "results": results,
        "succeeded": succeeded,
        "active": active,
        "total": results.len(),
        "skipped": skipped,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workbuddy_international_is_included_in_checkin_targets() {
        assert!(supports_checkin(&json!({
            "provider": "workbuddy",
            "edition": "intl",
        })));
        assert!(supports_checkin(&json!({
            "provider": "workbuddy",
            "edition": "cn",
        })));
    }

    #[test]
    fn unrelated_international_providers_remain_excluded() {
        assert!(!supports_checkin(&json!({
            "provider": "qoder",
            "edition": "intl",
        })));
        assert!(!supports_checkin(&json!({
            "provider": "accio",
            "edition": "intl",
        })));
        assert!(supports_checkin(&json!({
            "provider": "autoclaw-intl",
            "edition": "intl",
        })));
    }

    #[test]
    fn activity_keepalive_does_not_mark_regular_checkin() {
        assert!(!checkin_completed_today(&json!({
            "success": false,
            "msg": "活跃保活完成",
        })));
        assert!(checkin_completed_today(&json!({
            "success": false,
            "alreadyCompleted": true,
            "msg": "今日已领取",
        })));
        assert!(checkin_completed_today(&json!({ "success": true })));
    }
}
