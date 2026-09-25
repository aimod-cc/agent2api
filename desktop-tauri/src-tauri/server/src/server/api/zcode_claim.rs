//! ZCode「周末套餐」的领取接口（探测 + 领取）。
//!
//! ── 为什么单独成文件（不在 `api::accounts` 里）────────────────
//! `api::accounts` 已经是账号 CRUD + 十来个动作的入口（接近 550 行），
//! 而领取这条链路自带一段**协议知识**（biz code 分类、404 是正常状态、
//! 验证码由前端解），塞进去会让那个文件同时承担「账号簿记」与「套餐协议」
//! 两件事。与 `api::accounts_usage`（余额查询单独拆出去）同一处置。
//!
//! ── 两个接口 ────────────────────────────────────────────────
//! ```text
//!   POST /api/accounts/{id}/zcode-claim/preview   探测可领套餐（**不要验证码**）
//!   POST /api/accounts/{id}/zcode-claim           领取（**要**验证码参数）
//! ```
//! 拆成两个而不是「一个接口两段」的理由：探测是幂等的只读动作，可以放心由
//! 定时任务反复跑、也可以在界面上随便刷新；领取是有副作用的写动作，且必须
//! 等前端把验证码解出来才能发。两者的调用时机与失败语义完全不同。
//!
//! ── 验证码为什么是**请求体里的参数** ─────────────────────────
//! 上游要求 `X-Aliyun-Captcha-Verify-Param`，而解它的唯一可行方式是在
//! webview 里跑阿里云官方 SDK（见 `providers::zcode::claim` 的模块头：
//! 参考实现在进程内跑 happy-dom，本网关是纯 Rust、不引 JS 引擎）。
//! 因此本接口把 `captchaVerifyParam` 当**入参**收，原样转给上游 ——
//! 与 AutoClaw 的 OAuth 登录同一条思路（那边是
//! `/api/session/login/oauth/start` 收 `captchaVerifyParam`）。
//!
//! ── 出口要不要挂账号代理：**要**（与「余额查询直连」相反）────────
//! 小浣熊的余额查询刻意直连（照抄源实现的裸 fetch），本家**不照抄那个决定**：
//! 领取打的是 `zcode.z.ai`，而国内用户访问它通常需要代理；账号记录上的
//! 出口正是用户为这个账号配的（他配代理就是为了让这个账号能上网）。
//! 直连会让「配了代理的账号领不了套餐」，且报错是超时这种看不出原因的形状。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use axum::body::Bytes;
use serde_json::{json, Value};

use crate::server::core::providers::zcode::claim::{
    self, ClaimFailure, ClaimOutcome, PreviewOutcome,
};
use crate::server::core::providers::zcode::region::Region;
use crate::server::errors::management_error;
use crate::server::http::{ok_json, parse_body};
use crate::server::ServerState;

/// 从账号记录里取出领取链路要的三样东西。
///
/// `jwt` 与 `deviceMid` **不走投影列**（它们是本家独有的字段，账号存储的
/// 列只认各家共用的那几个），因此这里读的是记录的 JSON 形态而不是会话形态。
/// `accessToken` 那条路（转发）才走会话（见 `providers::zcode::adapter`）。
struct ClaimTarget {
    /// 地区（决定上游 `provider` 取值与提示文案）
    region: Region,
    /// 套餐令牌（领取的必要条件）
    jwt: String,
    /// 设备标识（活动期要求，见 `credentials.rs` 的模块头）
    device_mid: Option<String>,
    /// 账号展示名（日志与错误文案用）
    name: String,
}

/// 载入目标账号；缺账号或缺 jwt 时返回一句给用户的话
///
/// 状态码用 `i32`（与 `management_error` 同型），避免在每个调用点反复转换。
fn load_target(state: &ServerState, account_id: &str) -> Result<ClaimTarget, (i32, String)> {
    let record = state
        .store()
        .zcode_account_record(account_id)
        .ok_or_else(|| (404, "找不到该 ZCode 账号".to_string()))?;
    let provider_id = record
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("");
    let region = Region::from_provider_id(provider_id)
        .ok_or_else(|| (400, "该账号不是 ZCode 账号".to_string()))?;
    let text = |key: &str| {
        record
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_string()
    };
    let jwt = text("jwt");
    let name = text("name");
    if jwt.is_empty() {
        // 明确说清是哪一样缺了：本家有两个互不替代的凭证，用户很容易以为
        // 「填了一个就够」（见 `credentials.rs` 的模块头）
        return Err((
            400,
            format!(
                "该 ZCode {}账号没有套餐令牌（jwt），无法领取。请重新用「网页登录」添加，\
                 或在账号里补填 Coding Plan JWT",
                region.label()
            ),
        ));
    }
    Ok(ClaimTarget {
        region,
        jwt,
        device_mid: {
            let value = text("deviceMid");
            (!value.is_empty()).then_some(value)
        },
        name,
    })
}

/// 取该账号配置的出口代理（理由见模块头：领取**要**挂代理）
fn account_proxy(state: &ServerState, account_id: &str) -> Option<crate::server::core::proxies::ResolvedProxy> {
    let session = state.store().get_session_by_id(account_id)?.session;
    crate::server::core::proxies::session_proxy(&session)
}

/// `POST /api/accounts/{id}/zcode-claim/preview`：探测当前可领的套餐。
///
/// 成功响应的形状（前端据此渲染列表与「领取」按钮）：
/// ```json
/// { "plans": [ { "planId", "name", "description", "priority",
///                "startsAt", "endsAt", "entitlements": [...] } ],
///   "deployed": true }
/// ```
/// `deployed: false` 表示上游活动接口尚未部署（404）—— **这不是错误**，
/// 是周末套餐开抢前的正常状态（见 `providers::zcode::claim` 的模块头）。
/// 前端应当据此显示「当前没有可领套餐」而不是报错。
pub async fn preview(state: &ServerState, account_id: &str) -> axum::response::Response {
    let target = match load_target(state, account_id) {
        Ok(target) => target,
        Err((status, message)) => return management_error(status, message),
    };
    let proxy = account_proxy(state, account_id);
    let outcome = claim::preview(
        target.region,
        &target.jwt,
        target.device_mid.as_deref(),
        proxy.as_ref(),
    )
    .await;
    match outcome {
        Ok(PreviewOutcome::NotDeployed) => ok_json(json!({ "plans": [], "deployed": false })),
        Ok(PreviewOutcome::Plans(plans)) => ok_json(json!({
            "plans": plans.iter().map(plan_to_json).collect::<Vec<Value>>(),
            "deployed": true,
        })),
        Err(error) => management_error(error.status_code, error.message),
    }
}

/// `POST /api/accounts/{id}/zcode-claim/captcha-config`：取阿里云风控配置。
///
/// 前端拿它去初始化滑块 SDK（`prefix` / `region` / `sceneId` 三样缺一不可）。
/// 与 AutoClaw OAuth 的 `/api/session/login/oauth/captcha-config` 是同一个位置
/// 的同一件事，只是来源不同 —— 那边从 AutoClaw 的登录接口取，这边从 ZCode
/// 的 `/api/v1/client/configs` 取（见 `claim::captcha_config`）。
///
/// ── 不缓存 ──────────────────────────────────────────────────
/// 领取是低频动作（一次领一次），不值得在这里维护带 TTL 的进程级缓存 ——
/// 参考实现在进程里缓存 60 秒，是因为它还要在转发热路径上反复取。
pub async fn captcha_config(state: &ServerState, account_id: &str) -> axum::response::Response {
    // 这一支**不要求账号有 jwt**：风控配置是「上游此刻要不要验证码」的公共
    // 信息，与哪个账号无关。提前报「缺 jwt」会让用户以为账号有问题，
    // 而真正的原因是他还没登录过。
    let record = match state.store().zcode_account_record(account_id) {
        Some(record) => record,
        None => return management_error(404, "找不到该 ZCode 账号"),
    };
    let region = match record
        .get("provider")
        .and_then(Value::as_str)
        .and_then(Region::from_provider_id)
    {
        Some(region) => region,
        None => return management_error(400, "该账号不是 ZCode 账号"),
    };
    let proxy = account_proxy(state, account_id);
    match claim::captcha_config(region, proxy.as_ref()).await {
        // `enabled: false` 或配置不全 → 前端不弹滑块（见 claim::captcha_config 的说明）
        Ok(None) => ok_json(json!({ "enabled": false })),
        Ok(Some(config)) => ok_json(json!({
            "enabled": config.enabled,
            "prefix": config.prefix,
            "sceneId": config.scene_id,
            "region": config.region,
        })),
        Err(error) => management_error(error.status_code, error.message),
    }
}

/// `POST /api/accounts/{id}/zcode-claim`：领取一个套餐。
///
/// 请求体：
/// ```json
/// { "planId": "...", "captchaVerifyParam": "...", "captchaRegion": "..." }
/// ```
/// `planId` 省略或为空时取**优先级最高**的那个可领套餐（与参考实现的
/// `claim.planId` 配置为空的语义一致）。因此前端可以「先探测、用户直接点
/// 领取」，也可以「探测后让用户选一个再领」。
///
/// ── 失败也返回 HTTP 200（业务结果不是 HTTP 错误）─────────────
/// 「已领过」「额度用尽」「验证码没过」都是**正常的业务结果**，前端要靠
/// `failure` 字段给不同提示。只有「请求本身没打成」（账号不存在、缺 jwt、
/// 网络故障）才用非 2xx。
pub async fn claim_plan(
    state: &ServerState,
    account_id: &str,
    body: &Bytes,
) -> axum::response::Response {
    let target = match load_target(state, account_id) {
        Ok(target) => target,
        Err((status, message)) => return management_error(status, message),
    };
    let payload = parse_body(body).ok().unwrap_or(Value::Null);
    let field = |key: &str| {
        payload
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_string()
    };
    let captcha_verify_param = field("captchaVerifyParam");
    // 空值**放行**（不再 400）：上游的风控配置可能是「此刻不要验证码」，
    // 那时前端不会弹滑块、也就没有参数可给 —— 拦在这里会让「不需要验证码」
    // 的那一刻反而领不了。有没有值交给上游判：要就回 3007（我们分类成
    // 「验证码校验未通过」），不要就放行。头部只在有值时添加，
    // 见 `claim::claim` 的那段注释（发空头会被当成无效验证串）。
    let captcha_region = {
        let value = field("captchaRegion");
        (!value.is_empty()).then_some(value)
    };

    let proxy = account_proxy(state, account_id);
    let requested_plan_id = field("planId");
    let plan_id = if requested_plan_id.is_empty() {
        // 没点名就探测一次，取优先级最高的那个
        match claim::preview(
            target.region,
            &target.jwt,
            target.device_mid.as_deref(),
            proxy.as_ref(),
        )
        .await
        {
            Ok(PreviewOutcome::Plans(plans)) => match claim::pick_target(&plans, "") {
                Some(plan) => plan.plan_id.clone(),
                None => {
                    return ok_json(json!({
                        "ok": false,
                        "failure": "unavailable",
                        "message": "当前没有可领取的套餐",
                    }))
                }
            },
            Ok(PreviewOutcome::NotDeployed) => {
                return ok_json(json!({
                    "ok": false,
                    "failure": "unavailable",
                    "message": "活动尚未开始（上游接口未部署）",
                }))
            }
            Err(error) => return management_error(error.status_code, error.message),
        }
    } else {
        requested_plan_id
    };

    let outcome = claim::claim(
        target.region,
        &target.jwt,
        &plan_id,
        &captcha_verify_param,
        captcha_region.as_deref(),
        target.device_mid.as_deref(),
        proxy.as_ref(),
    )
    .await;

    match outcome {
        Err(error) => management_error(error.status_code, error.message),
        Ok(ClaimOutcome::Claimed { plan_id, starts_at, ends_at }) => {
            crate::server::logging::log(
                "[Claim]",
                &format!(
                    "✅ ZCode {}账号「{}」领取成功：{plan_id}",
                    target.region.label(),
                    target.name
                ),
            );
            ok_json(json!({
                "ok": true,
                "planId": plan_id,
                "startsAt": starts_at,
                "endsAt": ends_at,
            }))
        }
        Ok(ClaimOutcome::Failed { plan_id, failure, code, message, failure_ends_at }) => {
            // 失败也如实记一行：这个功能的排障全在「为什么没领到」上，
            // 而 `failure` 的分类正是给人看的那一句话
            crate::server::logging::log(
                "[Claim]",
                &format!(
                    "ZCode {}账号「{}」领取失败：{}（{code}）—— {message}",
                    target.region.label(),
                    target.name,
                    failure.label()
                ),
            );
            ok_json(json!({
                "ok": false,
                "planId": plan_id,
                "failure": failure_key(failure),
                "failureLabel": failure.label(),
                "code": code,
                "message": message,
                "failureEndsAt": failure_ends_at,
            }))
        }
    }
}

/// 一条可领套餐 → 前端 JSON（字段名与后端结构体同名，便于对照）
fn plan_to_json(plan: &claim::ClaimablePlan) -> Value {
    json!({
        "planId": plan.plan_id,
        "name": plan.name,
        "description": plan.description,
        "priority": plan.priority,
        "startsAt": plan.starts_at,
        "endsAt": plan.ends_at,
        "entitlements": plan
            .entitlements
            .iter()
            .map(|item| json!({
                "showName": item.show_name,
                "unitType": item.unit_type,
                "grantUnits": item.grant_units,
                "effectiveAt": item.effective_at,
            }))
            .collect::<Vec<Value>>(),
    })
}

/// 失败语义 → 前端用的稳定字符串（前端按它选文案，不按中文 label 匹配）
///
/// 与参考实现的 `ClaimFailureKind` 逐字同名：那套名字已经是这条链路的事实
/// 标准，前端将来要对齐别的实现时不用做一层翻译。
fn failure_key(failure: ClaimFailure) -> &'static str {
    match failure {
        ClaimFailure::NotFound => "not_found",
        ClaimFailure::Unavailable => "unavailable",
        ClaimFailure::AlreadyClaimed => "already_claimed",
        ClaimFailure::Ineligible => "ineligible",
        ClaimFailure::QuotaExhausted => "quota_exhausted",
        ClaimFailure::InvalidRequest => "invalid_request",
        ClaimFailure::Captcha => "captcha",
        ClaimFailure::LoginRequired => "login_required",
        ClaimFailure::HttpError => "http_error",
        ClaimFailure::Unknown => "unknown",
    }
}
