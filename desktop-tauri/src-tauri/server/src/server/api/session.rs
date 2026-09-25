//! 会话相关路由（对照 Node 版 server.mjs 776-869、963-974 行逐条实现）。
//!
//!   GET    /api/session                 前端首屏状态（**免鉴权**）
//!   POST   /api/session/login/start     发起无头登录，最多等 15 秒拿 authUrl
//!   GET    /api/session/login/wait      轮询登录结果 ?state=
//!   POST   /api/session/login/cancel    取消登录（关弹窗/用户放弃）
//!   POST   /api/session/login/callback  提交网页登录回调（壳侧登录窗口捕获）
//!   POST   /api/session/refresh         刷新当前账号 token
//!   POST   /api/session/logout          清除登录态（删掉当前账号）
//!   POST   /auth/login                  同步登录（等完成才响应）
//!   POST   /auth/logout                 清除登录态
//!
//! ── 鉴权分组（务必与 Node 版一致）────────────────────────────
//! 只有 `GET /api/session` 是免鉴权的（前端首屏拿不到 key 时也要能显示状态）；
//! 其余全部 checkApiKey。在 http.rs 里它们分别挂在 public / protected 组。
//!
//! ── 字段来源（/api/session）────────────────────────────────
//!   health          ← auth.get_config_summary()（纯本地：只查凭证是否存在）
//!   session         ← auth.get_status()（含临期自动刷新）
//!   accounts        ← store.list_accounts()
//!   lastRequestModel← config.json 的 lastRequestModel
//!   routedAccountId ← 按 lastRequestModel 派生的「下一个请求会先用谁」（见其函数注释）
//!   defaultModel    ← config.json 的 defaultModel
//!   proxies         ← Clash 摘要（切片 3 起为实时读取的真实值）
//!   models          ← 模型目录（切片 4 起为真实清单，含远程刷新结果）
//!   sanitizeBlacklistFingerprints ← 出站指纹脱敏开关（与 GET /api/sanitize 同源）

use std::collections::HashMap;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::response::Response;
use serde_json::{json, Value};

use crate::server::api::health::UNCONFIGURED_REASON;
use crate::server::config;
use crate::server::core::login::AUTH_URL_WAIT_MS;
use crate::server::core::providers::router;
use crate::server::core::routing;
use crate::server::errors::management_error;
use crate::server::http::{ok_json, parse_body};
use crate::server::logging;
use crate::server::ServerState;

/// GET /api/session —— 免鉴权（前端首屏在配置 API Key 之前也要能读）
pub async fn get_session(State(state): State<ServerState>) -> Response {
    let snapshot = config::current();
    // 账号快照取一次：`accounts` 字段与下面的 routedAccountId 读的是同一份数据，
    // 分两次取会各读一遍盘，还可能出现「两次读之间账号被改」的撕裂
    let accounts = state.store().list_accounts();
    // 在途请求计数：★ 的推算口径要与真实选路一致 —— 转发选路会跳过已达并发
    // 上限的账号（`routing::pick_account_by_priority` 的并发过滤），★ 也要剔除
    // 它们，否则界面上标的「下一个请求会先用谁」在实际都忙时是错的。取快照的
    // 方式与选路层同源（`UpstreamService::connections()`）。
    let counts: HashMap<String, usize> = state
        .upstream()
        .connections()
        .snapshot()
        .into_iter()
        .collect();
    let routed = routed_account_id(&accounts, snapshot.last_request_model(), &counts);
    let summary = state.auth().get_config_summary();
    let configured = summary
        .get("configured")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    ok_json(json!({
        "health": {
            "upstreamConfigured": configured,
            "upstreamBaseUrl": summary.get("baseUrl").cloned().unwrap_or(Value::Null),
            "authSource": summary.get("authSource").cloned().unwrap_or(Value::Null),
            "unavailableReason": if configured {
                Value::Null
            } else {
                summary
                    .get("unavailableReason")
                    .cloned()
                    .unwrap_or_else(|| Value::String(UNCONFIGURED_REASON.to_string()))
            },
        },
        "session": state.auth().get_status().await,
        // accounts.currentAccountId = **不限模型**的队首（判据 = 启用 + 有可用凭证 +
        // 优先级序，见 core::account_store）。它回答「队列第一位是谁」，别的地方
        // （凭证来源、刷新目标）读的也一直是它，语义不变。
        // 账号页的 ★ / 「首选」要读的是**下一个请求会先用谁** —— 那是按最近一次
        // 请求的模型派生的 `routedAccountId`（见下），两者在队首正被限流时会不同。
        "accounts": accounts,
        // 最近一次实际请求用的模型：账号页的「模型」筛选默认选它
        "lastRequestModel": snapshot.last_request_model(),
        // 按 lastRequestModel 派生的「下一个请求会先用谁」（账号页 ★ / 「首选」）。
        // 判据与转发层同一套（候选 = 提供该模型的那些家的账号，再按全局优先级取
        // 第一个「启用 + 有凭证 + 对该模型未限流」的）；模型未知或该模型下没有
        // 可用账号时为 null，界面回落到上面的 currentAccountId。
        "routedAccountId": routed,
        "defaultModel": snapshot.default_model(),
        "proxies": proxies_summary(),
        // 模型目录真值（对照 server.mjs 802-804 的字段形状 {id, name, isDefault, credits}）。
        // 来源是**聚合目录**（`providers::catalog::session_models`），与 `GET /v1/models`
        // 同一份合并结果：界面上的模型清单必须等于客户端能拿到的清单，否则用户会照着一个
        // 路由不到的模型名去调（改造前这里只读 workbuddy 单家，另三家在界面上不可见）。
        // 每条另带 provider / providerLabel，供网关页按家分组。
        "models": crate::server::core::providers::catalog::session_models(state.store()),
        // 指纹脱敏开关（与 GET /api/sanitize 同一个键、同一个值）
        config::KEY_SANITIZE_FINGERPRINTS: snapshot.sanitize_fingerprints(),
    }))
}

/// 按「最近一次请求的模型」派生**下一个请求实际会先用谁**的账号 id
/// （账号页的 ★ / 「首选」读它）。
///
/// ── 为什么由后端派生，而不是界面拿 currentAccountId 自己推 ────────
/// 1. `accounts.currentAccountId` 是**不限模型**的队首（只判「启用 + 有凭证」），
///    不看按模型记的限额。队首正被限流时，界面标 ★ 的是那个必被跳过的账号，
///    请求却落到下一个 —— 两边说的不是一件事。
/// 2. 候选集合还要先收窄到「清单里有这个模型的家」（`route_for_forward`），
///    而 `/api/session` 的 `models` 是跨家**去重后**的聚合视图（同名模型只留
///    认领的那一家，见 `catalog::merged_items`），界面据此还原会漏掉并发提供
///    该模型的其它家。
///
/// 直接复用转发层的 `routing::pick_for_model`，界面标 ★ 的账号便与转发会先试的
/// 那个同判据；不做「取不到会话就继续往后找」那层运行时探测（要读客户端登录态
/// 文件，属转发编排的职责），改用 `hasCredentials` 这个静态事实近似。
///
/// 模型未知 / 该模型下确实没有可用账号时给 null —— 界面回落到
/// `currentAccountId`，宁可让它标一个「队列第一位」，也不要整列 ★ 凭空消失。
fn routed_account_id(accounts: &Value, model: Option<&str>, counts: &HashMap<String, usize>) -> Value {
    let Some(model) = model.map(str::trim).filter(|value| !value.is_empty()) else {
        return Value::Null;
    };
    // 候选链是 id 空间（内置家 + 自定义家同列，见 `router` 的模块头）；
    // 自定义家的账号同样在全局队列里，`pick_for_model` 按 provider 字符串
    // 过滤候选，两种 id 天然可比。
    let candidates = router::route_for_forward(model);
    let providers: Vec<&str> = candidates.iter().map(String::as_str).collect();
    routing::pick_for_model(
        &routing::accounts_of(accounts),
        model,
        &providers,
        counts,
        logging::now_ms(),
    )
    .and_then(|account| routing::account_id(&account).map(str::to_string))
    .map(Value::String)
    .unwrap_or(Value::Null)
}

/// Clash 摘要（Node 版这里只带三项，完整形态在 /api/proxies）。
/// 三项的含义：是否读到 Clash Verge 配置 / 读不到的原因 / 可选项数量
/// （混合端口 + 各监听器）。切片 3 起是真实值。
fn proxies_summary() -> Value {
    let clash = crate::server::core::proxies::clash_proxy_options();
    json!({
        "clashAvailable": clash.get("available").and_then(Value::as_bool).unwrap_or(false),
        "clashError": clash.get("error").cloned().unwrap_or(Value::Null),
        "optionCount": clash
            .get("options")
            .and_then(Value::as_array)
            .map(|items| items.len())
            .unwrap_or(0),
    })
}

// ─── POST /api/session/login/start ──────────────────────────

/// 发起登录：起任务后最多等 15 秒拿 authUrl。
///
/// 拿不到就 502 `{success:false, error}` —— 注意**任务仍在后台跑**
/// （Node 版同样如此：返回 502 只是这一次没等到 URL）。
///
/// ── `provider` 维度（本次新增）──────────────────────────────
/// body 里的可选 `provider`（缺省 = workbuddy）决定走哪条链：
///   - `workbuddy`（或字段缺失）→ **既有实现一行不改**：`state.login().start()`
///     起后台任务去问上游 `auth/state` 要 state/authUrl（客户端兼容：老版本
///     前端不带 provider，行为必须逐字保持）；
///   - 其余已注册 provider → 问适配器的 `build_login_url()`：
///     拿得到 (url, state) 就走 `start_web_login`（由登录窗口捕获回调、
///     再 POST 到 `/api/session/login/callback` 换凭证）；
///     拿不到（`supports_web_login() == false`）→ 400，文案点名这家不支持
///     并给出可行的替代（粘贴凭证 / 导入桌面端登录态）；
///   - 注册表里没有的 id → 400「未知的提供商」（不静默回落 workbuddy：
///     那会把一次小浣熊登录发起成 workbuddy 登录）。
pub async fn login_start(State(state): State<ServerState>, body: Bytes) -> Response {
    // edition 解析失败不是错误（Node 版 `catch { edition = null }`）
    let payload = parse_body(&body).ok();
    let edition = payload
        .as_ref()
        .and_then(|payload| payload.get("edition").and_then(Value::as_str))
        .map(str::to_string)
        .filter(|value| !value.is_empty());
    let provider = payload
        .as_ref()
        .and_then(|payload| payload.get("provider").and_then(Value::as_str))
        .map(str::trim)
        .unwrap_or("");
    // 注册表是 provider 白名单的唯一事实来源（见 providers::kind_from_id 的说明）
    let kind = match provider {
        "" => crate::server::core::providers::ProviderKind::WorkBuddy,
        other => match crate::server::core::providers::kind_from_id(other) {
            Some(kind) => kind,
            None => return management_error(400, format!("未知的提供商：{other}")),
        },
    };
    if kind == crate::server::core::providers::ProviderKind::Qoder {
        let handle = match state.login().start_qoder_login(edition.as_deref()) {
            Ok(handle) => handle,
            Err(error) => return management_error(400, error),
        };
        let task = handle.snapshot();
        return ok_json(json!({ "state": task.state, "authUrl": task.auth_url,
            "edition": task.edition, "provider": "qoder" }));
    }
    // ZCode：**服务端中介的 CLI 轮询**（`/oauth/cli/init` 拿授权地址，
    // 用户在浏览器里授权后由后台任务轮询换令牌）。响应形状与 Qoder 那条
    // 一致（`{state, authUrl, edition, provider}`），前端不需要新分支。
    //
    // ── 地区取谁：**provider id**，不是请求里的 `edition` ────────
    // 界面上两个地区是两张卡片（`zcode` / `zcode-intl`），点哪张就发哪个
    // provider id —— 那是权威。`edition` 是前端回显用的字段，若拿它定地区，
    // 两张卡片共用一个表单时就会串味（点了国际版却落了国内版账号，
    // 而账号记录一旦落错家，转发会稳定打错域名）。因此这里由 kind 反查地区，
    // 再把地区交给登录任务（任务表里的 edition 串只用做日志与回显）。
    if let Some(region) =
        crate::server::core::providers::zcode::region::Region::from_kind(kind)
    {
        let handle = match state.login().start_zcode_login(region) {
            Ok(handle) => handle,
            Err(error) => return management_error(400, error),
        };
        // ★ 必须等授权地址落地再响应，不能直接回快照：地址只能从上游 init 拿，
        // 而 init 在 spawn 出去的任务里跑 —— 立刻回快照就是回一个 `authUrl: null`，
        // 壳侧读一次就放弃（「后端未返回登录链接，请检查网络」），不会轮询补取。
        // 等待与登记用的都是 WorkBuddy 那条的现成机制（`wait_for_auth_url`
        // 顺带把任务按 state 登记进任务表，`/wait` 靠它查）。
        let (task_state, auth_url, task_edition) = match state
            .login()
            .wait_for_auth_url(&handle, Duration::from_millis(AUTH_URL_WAIT_MS))
            .await
        {
            Ok(values) => values,
            Err(error) => {
                logging::log("[Login]", &format!("❌ 发起 ZCode 登录失败: {error}"));
                return management_error(502, error);
            }
        };
        return ok_json(json!({ "state": task_state, "authUrl": auth_url,
            "edition": task_edition, "provider": region.provider_id() }));
    }
    // Cline：**设备授权登录**（WorkOS RFC 8628）。形态上介于「网页登录」与
    // 「Qoder 设备授权」之间：同步问上游要 user_code 与授权页地址（一次 POST），
    // 把地址交给界面打开；用户确认后由后台任务轮询换令牌。
    // 响应形状与另外两条登录链一致（`{state, authUrl}`），前端不需要新分支。
    //
    // ── 池怎么定（拆分后由 provider 身份给出，不再读参数）──────
    // provider id 自己就是池（`cline-free` / `cline-pass`），登录只决定
    // **落哪一家的账号**；上游那套设备授权两个池共用，没有站点或通道维度。
    // 因此这里不再解析 `pool` / `edition` —— 早先那套「池是账号的属性，
    // 得从 body 传进来」的前提已随拆分退场（见 `providers::cline::models`
    // 的模块头）。前端仍可能发来 `pool` 字段（旧版界面），忽略即可：
    // 它要表达的意思已经由 provider 表达了。
    if matches!(
        kind,
        crate::server::core::providers::ProviderKind::ClineFree
            | crate::server::core::providers::ProviderKind::ClinePass
    ) {
        let name = payload
            .as_ref()
            .and_then(|payload| payload.get("name").and_then(Value::as_str))
            .map(str::to_string)
            .filter(|value| !value.trim().is_empty());
        let provider_id = crate::server::core::providers::kind_id(kind);
        let handle = match state.login().start_cline_device_login(provider_id, name).await {
            Ok(handle) => handle,
            Err(error) => return management_error(400, error),
        };
        let task = handle.snapshot();
        return ok_json(json!({ "state": task.state, "authUrl": task.auth_url,
            "edition": task.edition, "provider": provider_id }));
    }
    // CatPaw：上游把 token **推**到我们的 loopback 回调上（见 core::login::catpaw），
    // 所以这里除了发起还要把回调基址告诉它 —— 那必须是本网关自己的监听地址，
    // 而上游的 redirect 白名单只放行 127.0.0.1 / localhost（实测）。
    if kind == crate::server::core::providers::ProviderKind::CatPaw {
        let callback_base = format!("http://127.0.0.1:{}", state.port);
        let handle = match state.login().start_catpaw_login(&callback_base).await {
            Ok(handle) => handle,
            Err(error) => return management_error(400, error),
        };
        let task = handle.snapshot();
        return ok_json(json!({ "state": task.state, "authUrl": task.auth_url,
            "edition": task.edition, "provider": "catpaw" }));
    }
    // AutoClaw：网页登录走**另一条入口**（`/api/session/login/oauth/start`），
    // 因此这里必须显式挡掉并说清去哪儿 —— 不能让它落到下面的通用分支：
    // 通用分支会回「这家不支持网页登录」，而国际版**是支持的**，只是发起方式
    // 不同（它的授权地址要先过一次浏览器端风控验证码，因此由前端拿地址再交壳
    // 开窗口，见 `providers::autoclaw::oauth` 与 `login.rs` 的
    // `start_autoclaw_oauth`）。一条「不支持」的文案会把用户引向
    // 「填写凭证」，而那条路只是绕远，不是必须。
    if let Some(region) = crate::server::core::providers::autoclaw::Region::from_kind(kind) {
        let message = if region == crate::server::core::providers::autoclaw::Region::Intl {
            "AutoClaw 国际版的网页登录需要先完成一次风控验证，请回到「添加账号」弹窗，\
             用「网页登录（Zai / Google）」发起"
        } else {
            // 国内版确实没有 OAuth（上游 `oauth-captcha-config` 返回 enabled:false），
            // 它的官方唯一登录方式是手机验证码 —— 文案要指向那条
            "AutoClaw 国内版没有网页授权登录，请改用「手机验证码登录」或「填写凭证」"
        };
        return management_error(400, message);
    }
    if kind != crate::server::core::providers::ProviderKind::WorkBuddy {
        return start_web_login(state, kind).await;
    }
    let handle = state.login().start(edition.as_deref());
    match state
        .login()
        .wait_for_auth_url(&handle, Duration::from_millis(AUTH_URL_WAIT_MS))
        .await
    {
        Ok((task_state, auth_url, task_edition)) => ok_json(json!({
            "state": task_state,
            "authUrl": auth_url,
            "edition": task_edition,
        })),
        Err(error) => {
            logging::log("[Login]", &format!("❌ 发起登录失败: {error}"));
            management_error(502, error)
        }
    }
}

/// 网页登录分支（provider 适配器自带授权地址的那条链）。
///
/// 响应形状与 workbuddy 分支**完全一致**（`{state, authUrl}` + 一个 `edition`
/// 字段）：前端与壳侧轮询逻辑只认这三个键，多一个 provider 维度不该改动它们。
/// `edition` 对非 workbuddy 没有语义（这里仍给默认值，省得前端读到 null）。
async fn start_web_login(state: ServerState, kind: crate::server::core::providers::ProviderKind) -> Response {
    let label = crate::server::core::providers::meta(kind).label;
    let handle = match state.login().start_web_login(kind) {
        Ok(handle) => handle,
        Err(reason) => return management_error(400, reason),
    };
    let task = handle.snapshot();
    let (Some(task_state), Some(auth_url)) = (task.state, task.auth_url) else {
        return management_error(500, format!("{label}网页登录未能生成 state/授权地址，请重试"));
    };
    ok_json(json!({
        "state": task_state,
        "authUrl": auth_url,
        "edition": task.edition,
        "provider": crate::server::core::providers::kind_id(kind),
    }))
}

// ─── GET /api/session/login/wait ────────────────────────────

/// 轮询登录结果（三种响应：pending / done+error / done+session）
pub async fn login_wait(
    State(state): State<ServerState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let task_state = params.get("state").cloned().unwrap_or_default();
    let Some(task) = state.login().tasks().get(&task_state) else {
        return management_error(404, "登录任务不存在或已过期");
    };
    ok_json(task.snapshot().to_wait_response())
}

// ─── POST /api/session/login/cancel ─────────────────────────

pub async fn login_cancel(State(state): State<ServerState>, body: Bytes) -> Response {
    let task_state = parse_body(&body)
        .ok()
        .and_then(|payload| {
            payload
                .get("state")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default();
    // 走 LoginService 那一层（而不是 `tasks().cancel`）：AutoClaw 那条链借来的
    // 回调端口挂在它的待办表上，取消时得一起释放（见 `LoginService::cancel`）
    let canceled = state.login().cancel(&task_state);
    ok_json(json!({ "canceled": canceled }))
}

// ─── POST /api/session/login/callback ───────────────────────

/// 提交网页登录的回调 URL（**壳侧登录窗口专用**）。
///
/// ── 为什么需要这个入口（Tauri 与 Electron 的差别）────────────
/// 小浣熊的回调是自定义协议 `office-raccoon://auth/callback`。Electron 版能用
/// `session.protocol.handle('office-raccoon', …)` 在会话内接管它；Tauri/WebView2
/// 没有等价能力 —— 壳只能把那个 URL 抓下来转交给网关。于是链路上多出这一步：
/// 登录窗口（`src-tauri/src/login.rs` 的 `on_navigation`）认出回调 URL、
/// **拦下导航**、把原文 POST 到这里。
///
/// ── 校验与失败语义 ──────────────────────────────────────────
/// body `{state, callbackUrl}`：
///   - state 必须对应一个**进行中**的登录任务（404 表示没有这个任务，前端应重新发起）；
///   - callbackUrl 必须是 `office-raccoon://auth/callback` 形态，且其中的 state
///     与任务一致（逐项解析比对，不是前缀匹配 —— 见 `raccoon::oauth`）；
///   - 换凭证失败（网络 / 授权码失效）→ 任务标记 failed，error 返回给调用方，
///     前端那次 `/wait` 会读到同一条错误文案。
///
/// 校验失败也**落定任务**（不留在 pending）：否则前端会一直轮询到 5 分钟超时，
/// 而用户其实早就该看到「state 校验失败，请重新发起」。
pub async fn login_callback(State(state): State<ServerState>, body: Bytes) -> Response {
    let payload = parse_body(&body).unwrap_or(Value::Null);
    let task_state = payload
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let callback_url = payload
        .get("callbackUrl")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if callback_url.is_empty() {
        return management_error(400, "缺少 callbackUrl（登录回调地址）");
    }
    match state
        .login()
        .submit_login_callback(&task_state, &callback_url)
        .await
    {
        Ok(account_id) => ok_json(json!({ "accountId": account_id })),
        Err(error) => management_error(error.status_code, error.message),
    }
}

// ─── POST /api/session/login/catpaw-callback ────────────────

/// CatPaw 网页登录的 loopback 回调（**上游直接 POST 到本机**）。
///
/// ── 与小浣熊那个 callback 的根本差别 ─────────────────────────
/// 小浣熊那条是**壳侧登录窗口**认出自定义协议 URL 后转交给网关的（Tauri 没有
/// Electron 的协议接管能力，见 `login_callback` 的说明）；CatPaw 这条是
/// **上游自己发起的 HTTP POST** —— 美团 passport 的 `login-callback` 页面把
/// `{token, state}` 表单提交到我们在授权 URL 里给的 `redirect` 地址，
/// 而那个地址就是本网关自己的 loopback 端口（见 `core::login::catpaw`）。
///
/// ── 为什么这条不挂 protected ─────────────────────────────────
/// 调用方是**浏览器里的上游页面**，它当然没有我们的 API Key。安全性由
/// `state` 承担：一次性随机串，只在本进程内生成并与登录任务一一对应
/// （`finish_catpaw_login` 里逐字比对）。伪造者猜不中 state 就换不到任何东西 ——
/// 这与小浣熊那条挂 protected 并不矛盾：那条的调用方是我们自己的登录窗口，
/// 它本来就带着 API Key。
///
/// ── 为什么同时接受表单与 JSON ────────────────────────────────
/// 上游是**表单提交**（content-type 为 `application/x-www-form-urlencoded`；
/// 依据是上游 CSP 的 `form-action` 与客户端 loopback 只接受表单/JSON 两种），
/// 而本网关内部调用用的是 JSON。两种都解析，省得将来换调用方时再改一次。
///
/// 响应是给人看的 HTML（浏览器会停在这一页），因此不走 `ok_json` 那套信封。
///
/// ── 为什么要显式补 `Access-Control-Allow-Private-Network` ────
/// 这次回调是**从公网页面（`catpaw.meituan.com`）发往本机 127.0.0.1 的跨源
/// 请求**（上游用 fetch/XHR 提交，不是顶层表单导航 —— 否则它不需要任何 CORS
/// 头）。浏览器对「公网 → 私有网络」的这类请求会做 Private Network Access
/// 检查：预检里带 `Access-Control-Request-Private-Network`，而响应**必须**回
/// `Access-Control-Allow-Private-Network: true`，否则请求被拦、凭证根本到不了
/// 我们这里（症状：用户看到上游的「登录成功」页、窗口不关、网关一直等）。
///
/// 官方客户端的 loopback 正是为此专门回了这个头（`auth-DSFS1FEr.js` 里的
/// `Lr` 常量 = `{Origin: *, Allow-Private-Network: true}`）。全局 CORS 中间件
/// 只回三个标准头（照抄 Node 版，不为这一个端点改动全局行为），因此这里单点补齐。
pub async fn login_catpaw_callback(State(state): State<ServerState>, body: Bytes) -> Response {
    let (token, task_state) = parse_catpaw_callback(&body);
    let response = match state.login().finish_catpaw_login(&token, &task_state).await {
        Ok(()) => catpaw_callback_page(200, "登录成功，已返回网关，可以关闭此页面。"),
        Err(message) => catpaw_callback_page(400, &format!("登录失败：{message}")),
    };
    attach_private_network_headers(response)
}

/// 给回调响应补上私有网络访问许可（见 `login_catpaw_callback` 的说明）。
///
/// 顺带把 `Allow-Origin` 也显式写一遍：全局 CORS 中间件已经写了，这里重复设置
/// 同一个值是无害的（幂等），但它让「这个端点的跨源许可」在一处可见 ——
/// 将来若有人调整全局 CORS 策略，这条回调不会跟着被改坏。
fn attach_private_network_headers(mut response: Response) -> Response {
    use axum::http::header::HeaderValue;
    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str("*") {
        headers.insert("access-control-allow-origin", value);
    }
    if let Ok(value) = HeaderValue::from_str("true") {
        headers.insert("access-control-allow-private-network", value);
    }
    response
}

/// 从回调 body 里取 `(token, state)`：先按表单解析，再退回 JSON。
fn parse_catpaw_callback(body: &[u8]) -> (String, String) {
    let text = String::from_utf8_lossy(body);
    if let Ok(payload) = serde_json::from_str::<Value>(&text) {
        if payload.get("token").is_some() || payload.get("state").is_some() {
            let token = payload.get("token").and_then(Value::as_str).unwrap_or("");
            let task_state = payload.get("state").and_then(Value::as_str).unwrap_or("");
            return (token.to_string(), task_state.to_string());
        }
    }
    // 表单（上游的实际形态）：token=<…>&state=<…>
    let mut token = String::new();
    let mut task_state = String::new();
    for pair in text.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        let key = decode_form_component(key);
        if key == "token" {
            token = decode_form_component(value);
        } else if key == "state" {
            task_state = decode_form_component(value);
        }
    }
    (token, task_state)
}

/// 表单字段解码（`+` → 空格，`%XX` → 字节）。
///
/// 不复用 `auth::urlencoding`：那个是**编码**（反方向），且这里的输入来自上游
/// 表单，必须按 `application/x-www-form-urlencoded` 的规则处理 `+`。
fn decode_form_component(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            // 需要完整的三字节 `%XX`：`index + 3 <= len`。
            // 若收尾处只剩两位（`%A`）就按普通字节处理，不吞掉它。
            b'%' if index + 3 <= bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3])
                    .ok()
                    .and_then(|text| u8::from_str_radix(text, 16).ok());
                match hex {
                    Some(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    None => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 回调结果页（浏览器停在它上边，用户看到人话即可）。
///
/// 不带任何脚本与外链：这一页的内容我们完全控制，注入面越小越好。
fn catpaw_callback_page(status: u16, message: &str) -> Response {
    use axum::response::IntoResponse;

    let escaped = message
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;");
    let html = format!(
        "<!doctype html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\">\
         <title>CatPaw 登录</title></head>\
         <body style=\"font-family:system-ui,sans-serif;padding:48px;text-align:center\">\
         <p style=\"font-size:16px\">{escaped}</p></body></html>"
    );
    (
        axum::http::StatusCode::from_u16(status).unwrap_or(axum::http::StatusCode::OK),
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        html,
    )
        .into_response()
}

// ─── POST /api/session/login/sms/send 与 /verify ────────────

/// 从请求体里读 AutoClaw 地区（`provider` 字段，与 `POST /api/accounts` 同名）。
///
/// 缺省与未知值都落**国内版**：与 provider id 的历史口径一致 ——
/// 老客户端不带这个字段，而那些用户本来就在用国内版。
/// 用 `Region::from_provider_id` 而不是自己 match 字符串：地区 ↔ id 的映射
/// 只有 `autoclaw::region` 一份，这里再写一遍就会在加地区时静默漏掉。
///
/// 读到国际版**不是**错误：这条链路的入口现在只服务国内版，但拒绝的判定在
/// 核心层（`providers::autoclaw::login::ensure_sms_region`）—— 让那里返回一条
/// 「该走哪条路」的人话错误，比在这里静默改成国内版好得多（静默改写的后果是
/// 用户在国际版弹窗里填的号码被发到另一个站点去，排障时看不出异常）。
fn autoclaw_region_of(payload: &Value) -> crate::server::core::providers::autoclaw::Region {
    payload
        .get("provider")
        .and_then(Value::as_str)
        .and_then(crate::server::core::providers::autoclaw::Region::from_provider_id)
        .unwrap_or(crate::server::core::providers::autoclaw::Region::Cn)
}

/// 发送短信验证码（**AutoClaw 国内版专用**，手机号验证码登录的第一步）。
///
/// ── 为什么这不是「网页登录」──────────────────────────────────
/// 另外两家的网页登录形态是「开登录窗口 → 用户登录 → 回调带授权码 → 换凭证」。
/// AutoClaw 国内版没有这条路（详见 `providers::autoclaw::login` 的模块头：
/// 桌面端没有公网 Web 应用、账号体系里没有授权码，它的唯一入口是手机号 +
/// 验证码；国际版反过来，只有 OAuth 网页登录）。因此这里既不开窗口也不起任务，
/// 就是**一次同步的上游调用**。
///
/// body `{phone, provider?}` → `{deviceId}`。
///
/// ── 为什么把 deviceId 回给前端 ──────────────────────────────
/// 上游把「发的这个码」绑在发码时的 device_id 上，登录必须带同一个 ——
/// 但网关不替用户保存这个中间态（一次登录可以跨多次 HTTP 请求、也可以被用户
/// 放弃，存在服务端只会多一份要清理的状态）。回给前端让它随下一次请求带回，
/// 是这里最省事又不丢正确性的做法。
///
/// ── 地区从哪来（`provider` 字段）────────────────────────────
/// 两个地区的接口是**同一个路径、两个站点**，因此「发给哪一家」由请求带上来
/// （前端把它要添加的那一家的 provider id 原样放进 `provider`，与
/// `POST /api/accounts` 的字段同名同语义；缺省与未知值落国内版）。
/// 但**只有国内版能走通**：国际版的手机验证码入口已从界面移除，带国际版进来
/// 会在核心层被明确拒绝（理由见 `ensure_sms_region`）。
pub async fn login_sms_send(body: Bytes) -> Response {
    let payload = parse_body(&body).unwrap_or(Value::Null);
    let phone = payload.get("phone").and_then(Value::as_str).unwrap_or("");
    let region = autoclaw_region_of(&payload);
    match crate::server::core::providers::autoclaw::login::send_code(region, phone).await {
        Ok(result) => ok_json(result),
        Err(error) => management_error(error.status_code, error.message),
    }
}

/// 用手机号 + 验证码登录并**直接落成账号**（**AutoClaw 国内版专用**）。
///
/// body `{phone, code, deviceId?, name?, provider?}` → `{account, list}` ——
/// 响应形状与 `POST /api/accounts` **逐字一致**：登录只是另一种拿到凭证的方式，
/// 落盘、命名、去重、优先级分配全部复用既有的添加路径
/// （`add_autoclaw_account`），前端因此可以直接把结果交给同一个
/// 「已添加账号」收尾逻辑。`provider` 的语义见 `login_sms_send`。
pub async fn login_sms_verify(State(state): State<ServerState>, body: Bytes) -> Response {
    let payload = parse_body(&body).unwrap_or(Value::Null);
    let phone = payload.get("phone").and_then(Value::as_str).unwrap_or("");
    let code = payload.get("code").and_then(Value::as_str).unwrap_or("");
    let device_id = payload.get("deviceId").and_then(Value::as_str);
    let region = autoclaw_region_of(&payload);
    let credentials = match crate::server::core::providers::autoclaw::login::login_with_code(
        region, phone, code, device_id,
    )
    .await
    {
            Ok(credentials) => credentials,
            Err(error) => return management_error(error.status_code, error.message),
        };
    // 备注名：用户显式填的优先；没填则用脱敏手机号（`130****4229`）——
    // 比默认的「账号 830290」更像用户自己认得出来的标识
    let name = payload
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            credentials
                .get("phoneTail")
                .and_then(Value::as_str)
                .map(str::to_string)
        });
    let store = state.store();
    match store.add_autoclaw_account(region, &credentials, name.as_deref()) {
        Ok(account) => {
            let label = account
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("AutoClaw 账号");
            logging::log(
                "[Login]",
                &format!("✅ AutoClaw {}登录成功: {label}", region.label()),
            );
            ok_json(json!({ "account": account, "list": store.list_accounts() }))
        }
        Err(error) => super::accounts::store_error(error),
    }
}

// ─── AutoClaw OAuth 网页登录（国际版，三段）──────────────────

/// 从请求体里读 OAuth 变体（`vendor` 字段：`zai` / `google`）。
///
/// 不认识的值一律 400：**不静默回落**到某一个变体 —— 那会让用户点 Google
/// 却打开 Zai 的授权页（而两者用的是不同的账号体系，登进去是个陌生账号）。
fn oauth_vendor_of(payload: &Value) -> Result<crate::server::core::providers::autoclaw::oauth::Vendor, Response> {
    let raw = payload
        .get("vendor")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    crate::server::core::providers::autoclaw::oauth::Vendor::from_id(raw)
        .ok_or_else(|| management_error(400, format!("未知的登录方式「{raw}」（只支持 zai / google）")))
}

/// `GET /api/session/login/oauth/captcha-config` —— 取风控验证配置。
///
/// 前端据此初始化阿里云 SDK（`prefix` / `sceneId` 是 SDK 的必填参数）。
/// `?provider=` 决定问哪个地区；缺省国内版。
///
/// ── 为什么这条单独成一个端点（不塞进 start）──────────────────
/// 验证码要在**浏览器环境**里跑（SDK 是浏览器端 JS），而 SDK 初始化需要
/// `prefix` / `sceneId` —— 那两个值只能从上游拿。若把「取配置」与「取授权
/// 地址」合并成一个请求，就变成「网关先取配置、但它没法自己跑验证码」，
/// 前端仍然要再发一次请求带验证码回来。分成两步之后，每一步的职责清楚：
/// 这一步拿配置（无状态、可缓存），下一步拿地址（带验证码）。
///
/// `enabled: false`（国内版）不是错误：前端据此不渲染 OAuth 按钮。
pub async fn login_oauth_captcha_config(body: Bytes) -> Response {
    let payload = parse_body(&body).unwrap_or(Value::Null);
    let region = autoclaw_region_of(&payload);
    match crate::server::core::providers::autoclaw::oauth::captcha_config(region).await {
        Ok(config) => ok_json(config),
        Err(error) => management_error(error.status_code, error.message),
    }
}

/// `POST /api/session/login/oauth/start` —— 带验证码参数发起 OAuth 登录。
///
/// body `{provider, vendor, captchaVerifyParam}` → `{state, authUrl, provider,
/// edition, warning?}`（与另外几条登录链**同一个响应形状**，前端与壳侧的等待
/// 逻辑不必为新家分叉；`warning` 是这一轮有降级时给用户看的一句话，没有就不带）。
///
/// ── 为什么这条不走壳侧的 `start_login` ────────────────────────
/// 壳侧那条命令的职责是「打开一个窗口去登录」，而这条的前提是「前端已经在
/// **主窗口里**跑完验证码 SDK 了」（SDK 只能在浏览器环境跑，主窗口就是那个
/// 环境）。把验证码参数从渲染层传给壳、再让壳回头调网关，等于绕一圈传一个
/// 不该由壳理解的不透明字符串。因此这条直接是**前端 → 网关**的一跳，
/// 拿到 authUrl 后由前端交给壳去打开（与其它家最终都由壳开窗口一致）。
///
/// ── 回调地址交给登录服务定（本次修正）────────────────────────
/// 这里只给「网关自己的 loopback 基址」（`http://localhost:<网关端口>`）与
/// 「浏览器是否在同一台机器上」，**不再**直接把网关端口当回调：
/// Zai 的 redirect_uri 白名单只认官方客户端那四个登记端口，网关端口会被拒
/// （`{"detail":"Redirect URI not registered for this client"}`，issue #11）。
/// 因此由 `core::login::autoclaw` 去借一个登记端口、并把回调转回这个基址 ——
/// 判据与回落顺序都在那边（见其 `callback_endpoint`）。
///
/// `local_browser` 的判据是**监听地址是不是 loopback**：桌面壳固定绑
/// 127.0.0.1；容器 / 远程部署按 `AGENT2API_HOST`（默认 0.0.0.0）—— 那种形态
/// 下浏览器解析的 `localhost` 是它自己那台机器，占登记端口没有意义。
pub async fn login_oauth_start(State(state): State<ServerState>, body: Bytes) -> Response {
    let payload = parse_body(&body).unwrap_or(Value::Null);
    let region = autoclaw_region_of(&payload);
    let vendor = match oauth_vendor_of(&payload) {
        Ok(vendor) => vendor,
        Err(response) => return response,
    };
    let captcha = payload
        .get("captchaVerifyParam")
        .and_then(Value::as_str)
        .unwrap_or("");
    // ── 浏览器要访问的地址按「回调路由挂在哪个端口」拼 ─────────────
    // 回调路由在**管理面**那套路由里（见 http.rs 的 `panel_router`）：同端口
    // 形态（桌面壳、未设 AGENT2API_PANEL_PORT 的 headless）就是 `state.port`；
    // 分端口形态它在 `panel_port` 上 —— 那时浏览器必须打到那个端口，否则
    // 回调（以及监听器的 302 转发目标）会落进主端口的 404。
    let api_port = state
        .panel_port
        .filter(|port| *port != state.port)
        .unwrap_or(state.port);
    let gateway_base = format!("http://localhost:{api_port}");
    let (handle, warning) = match state
        .login()
        .start_autoclaw_oauth_login(region, vendor, captcha, &gateway_base, state.host.is_loopback())
        .await
    {
        Ok(result) => result,
        Err(reason) => return management_error(400, reason),
    };
    let task = handle.snapshot();
    let mut body = json!({
        "state": task.state,
        "authUrl": task.auth_url,
        "edition": task.edition,
        "provider": region.provider_id(),
    });
    if let Some(warning) = warning {
        if let Some(object) = body.as_object_mut() {
            object.insert("warning".to_string(), Value::String(warning));
        }
    }
    ok_json(body)
}

/// `GET /auth/callback-{vendor}` —— 浏览器回调（客户端同款形态，见
/// `providers::autoclaw::oauth::CALLBACK_PATH_PREFIX`）。
///
/// ── 两个入口都落到这里（本次修正）────────────────────────────
/// `navigate_uri` 指向 z.ai 登记过的那四个端口之一，不是本路由所在端口：
///   - 那一轮抢到了登记端口 → 浏览器先落到那个监听器上，它再 302 到这里
///     （见 `providers::autoclaw::callback_server`）；
///   - 一个都没抢到（官方客户端在运行）→ 壳侧内嵌窗口把这次导航截回这里
///     （见 `src/login.rs` 的 `autoclaw_callback_forward`）。
/// 两条路都只把 `code` / `state` 带过来，因此这里的校验口径不变。
///
/// ── 为什么这条免鉴权（挂 public 组）──────────────────────────
/// 调用方是**用户的浏览器**（授权页 302 到这里），它当然没有我们的 API Key。
/// 与 CatPaw 那条 loopback 回调同一取舍。
///
/// ── 任务怎么关联（这里没有我们的 state）─────────────────────
/// 上游在 `navigate_uri` 后拼 `?code=…&state=…`，查询串里的 `state` 是**上游
/// 生成的**那个（换码要回传它）。我们自己的任务标识不在回调 URL 里（放查询串
/// 会与它撞参数名、放子路径会偏离客户端形态）—— 任务关联在登录服务里按
/// 「变体匹配的进行中任务」完成（见 `core::login::autoclaw` 的
/// `find_pending_for_vendor`）。
///
/// 响应是给人看的 HTML（浏览器停在这一页），因此不走 `ok_json` 那套信封。
pub async fn login_autoclaw_oauth_callback(
    State(state): State<ServerState>,
    axum::extract::Path(vendor_id): axum::extract::Path<String>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    use crate::server::core::providers::autoclaw::oauth::Vendor;
    let Some(vendor) = Vendor::from_id(vendor_id.trim()) else {
        return oauth_callback_page(400, "登录失败：无法识别的登录方式");
    };
    let code = params.get("code").cloned().unwrap_or_default();
    // 上游在查询串里回的 state —— 换码要用的就是这一个（官方客户端读的也是它）
    let upstream_state = params.get("state").cloned().unwrap_or_default();
    // 上游把 error 也回在这个查询串里（用户拒绝授权时）
    if let Some(error) = params.get("error").filter(|value| !value.trim().is_empty()) {
        return oauth_callback_page(400, &format!("登录失败：授权被拒绝（{error}）"));
    }
    match state
        .login()
        .finish_autoclaw_oauth_callback(vendor, &upstream_state, &code)
        .await
    {
        Ok(_) => oauth_callback_page(200, "登录成功，已返回网关，可以关闭此页面。"),
        Err(error) => oauth_callback_page(error.status_code, &format!("登录失败：{}", error.message)),
    }
}

/// OAuth 回调结果页（浏览器停在它上边，用户看到人话即可）。
///
/// 与 `catpaw_callback_page` 同形（不带任何脚本与外链，注入面越小越好），
/// 但状态码可能是 410（登录上下文已丢失）这类非 200 —— 统一用 `from_u16`
/// 兜底成 200，避免一个非法状态码让响应构造失败。
fn oauth_callback_page(status: i32, message: &str) -> Response {
    use axum::response::IntoResponse;

    let escaped = message
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;");
    let html = format!(
        "<!doctype html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\">\
         <title>AutoClaw 登录</title></head>\
         <body style=\"font-family:system-ui,sans-serif;padding:48px;text-align:center\">\
         <p style=\"font-size:16px\">{escaped}</p></body></html>"
    );
    let status = u16::try_from(status)
        .ok()
        .and_then(|code| axum::http::StatusCode::from_u16(code).ok())
        .unwrap_or(axum::http::StatusCode::OK);
    (
        status,
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        html,
    )
        .into_response()
}

// ─── GET /auth/callback-accio ───────────────────────────────

/// Accio 网页登录的回调（**浏览器 302 到这里**，查询串带 `code` / `state`）。
///
/// ── 为什么免鉴权（挂 public 组）──────────────────────────────
/// 调用方是**用户的浏览器**（授权页完成后顶层导航到我们交给它的 return_url），
/// 它当然没有我们的 API Key。与 CatPaw / AutoClaw 两条 loopback 回调同一取舍。
///
/// ── 路径是我们自己定的 ──────────────────────────────────────
/// 与 AutoClaw 那条「必须与官方客户端逐字同款（Zai 按 host 校验白名单）」不同：
/// Accio 的 `return_url` 由**发起方**随授权请求带上（桌面端自己用的是
/// `http://127.0.0.1:<port>/auth/callback`），登录页只负责把 code/state 拼回来。
/// 因此这里用 `callback-accio` 这个别家不会撞的名字。
///
/// ── 任务关联 ────────────────────────────────────────────────
/// `state` 是我们生成的那个（拼在 return_url 前的授权 URL 里），登录页原样带回，
/// 因此直接按它查任务表即可（与 AutoClaw 的「变体匹配 + 最近发起」不同）。
///
/// 响应是给人看的 HTML（浏览器停在这一页），不走 `ok_json` 那套信封。
pub async fn login_accio_callback(
    State(state): State<ServerState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let code = params.get("code").cloned().unwrap_or_default();
    let task_state = params.get("state").cloned().unwrap_or_default();
    if let Some(error) = params.get("error").filter(|value| !value.trim().is_empty()) {
        return oauth_callback_page(400, &format!("登录失败：授权被拒绝（{error}）"));
    }
    match state.login().finish_accio_login(&code, &task_state).await {
        Ok(_) => oauth_callback_page(200, "登录成功，已返回网关，可以关闭此页面。"),
        Err(error) => {
            // 这一轮已经作废：把 PKCE 的 pending 也丢掉，免得留在表里等超时
            state.login().drop_accio_pending(&task_state);
            oauth_callback_page(error.status_code, &format!("登录失败：{}", error.message))
        }
    }
}

// ─── POST /api/session/refresh ──────────────────────────────

/// POST /api/session/refresh
///
/// 出错时走**最外层 catch → errorPayload**（OpenAI 风格 `{error:{message,type}}`），
/// 而不是管理 API 的 `{success:false,error}` 信封 —— Node 版这条就在大 try 里，
/// 刷新失败的响应形状与 /api/accounts/refresh 不同，别混用。
pub async fn session_refresh(State(state): State<ServerState>) -> Response {
    match state.auth().refresh_stored_session().await {
        Ok(_) => ok_json(json!({ "session": state.auth().get_status().await })),
        Err(error) => {
            logging::log("[Auth]", &format!("❌ {}", error.message));
            use axum::response::IntoResponse;
            error.to_gateway_error().into_response()
        }
    }
}

// ─── POST /api/session/logout 与 POST /auth/logout ─────────

pub async fn session_logout(State(state): State<ServerState>) -> Response {
    state.auth().clear_session();
    crate::server::http::ok_empty()
}

/// POST /auth/logout —— 与 /api/session/logout 同一动作与同一响应形状
pub async fn auth_logout(State(state): State<ServerState>) -> Response {
    state.auth().clear_session();
    crate::server::http::ok_empty()
}

// ─── POST /auth/login ───────────────────────────────────────

/// 同步登录（对照 server.mjs 601-610 行的 `runLogin`）：等登录完成才响应。
///
/// 桌面端不用这条（它走 /api/session/login/* 的异步三步），保留它是为了
/// 与 Node 版的命令行/脚本入口保持契约一致。
pub async fn auth_login(State(state): State<ServerState>, body: Bytes) -> Response {
    let edition = parse_body(&body)
        .ok()
        .and_then(|payload| {
            payload
                .get("edition")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .filter(|value| !value.is_empty());
    match state.login().run_login(edition.as_deref()).await {
        Ok(session) => crate::server::http::raw_json(session),
        Err(error) => {
            logging::log("[Login]", &format!("❌ {}", error.message));
            // /auth/login 失败走最外层 catch → errorPayload（OpenAI 风格 body），
            // 而不是管理 API 的 `{success:false,error}` 信封 —— 与 Node 版一致
            use axum::response::IntoResponse;
            error.to_gateway_error().into_response()
        }
    }
}
