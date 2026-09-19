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
//!   desensitize     ← 脱敏摘要（切片 5 起为真值：enabled/termCount/roles）

use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::response::Response;
use serde_json::{json, Value};

use crate::server::api::health::UNCONFIGURED_REASON;
use crate::server::config;
use crate::server::core::login::AUTH_URL_WAIT_MS;
use crate::server::core::providers::{kind_id, router};
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
    let routed = routed_account_id(&accounts, snapshot.last_request_model());
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
        // 脱敏摘要（对照 server.mjs 805-808）：只有 enabled/termCount/roles，
        // 完整词表在 GET /api/desensitize（前端面板不许它覆盖完整状态）
        "desensitize": state.desensitize().summary(),
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
fn routed_account_id(accounts: &Value, model: Option<&str>) -> Value {
    let Some(model) = model.map(str::trim).filter(|value| !value.is_empty()) else {
        return Value::Null;
    };
    let providers: Vec<&str> = router::route_for_forward(model)
        .into_iter()
        .map(kind_id)
        .collect();
    routing::pick_for_model(
        &routing::accounts_of(accounts),
        model,
        &providers,
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
    let canceled = state.login().tasks().cancel(&task_state);
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

/// 发送短信验证码（**AutoClaw 专用**，手机号验证码登录的第一步）。
///
/// ── 为什么这不是「网页登录」──────────────────────────────────
/// 另外两家的网页登录形态是「开登录窗口 → 用户登录 → 回调带授权码 → 换凭证」。
/// AutoClaw 没有这条路（详见 `providers::autoclaw::login` 的模块头：桌面端没有
/// 公网 Web 应用、账号体系里没有授权码，国内版唯一入口是手机号 + 验证码）。
/// 因此这里既不开窗口也不起任务，就是**一次同步的上游调用**。
///
/// body `{phone}` → `{deviceId}`。
///
/// ── 为什么把 deviceId 回给前端 ──────────────────────────────
/// 上游把「发的这个码」绑在发码时的 device_id 上，登录必须带同一个 ——
/// 但网关不替用户保存这个中间态（一次登录可以跨多次 HTTP 请求、也可以被用户
/// 放弃，存在服务端只会多一份要清理的状态）。回给前端让它随下一次请求带回，
/// 是这里最省事又不丢正确性的做法。
pub async fn login_sms_send(body: Bytes) -> Response {
    let payload = parse_body(&body).unwrap_or(Value::Null);
    let phone = payload.get("phone").and_then(Value::as_str).unwrap_or("");
    match crate::server::core::providers::autoclaw::login::send_code(phone).await {
        Ok(result) => ok_json(result),
        Err(error) => management_error(error.status_code, error.message),
    }
}

/// 用手机号 + 验证码登录并**直接落成账号**（AutoClaw 专用）。
///
/// body `{phone, code, deviceId?, name?}` → `{account, list}` —— 响应形状与
/// `POST /api/accounts` **逐字一致**：登录只是另一种拿到凭证的方式，落盘、
/// 命名、去重、优先级分配全部复用既有的添加路径（`add_autoclaw_account`），
/// 前端因此可以直接把结果交给同一个「已添加账号」收尾逻辑。
pub async fn login_sms_verify(State(state): State<ServerState>, body: Bytes) -> Response {
    let payload = parse_body(&body).unwrap_or(Value::Null);
    let phone = payload.get("phone").and_then(Value::as_str).unwrap_or("");
    let code = payload.get("code").and_then(Value::as_str).unwrap_or("");
    let device_id = payload.get("deviceId").and_then(Value::as_str);
    let credentials =
        match crate::server::core::providers::autoclaw::login::login_with_code(phone, code, device_id)
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
    match store.add_autoclaw_account(&credentials, name.as_deref()) {
        Ok(account) => {
            let label = account
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("AutoClaw 账号");
            logging::log("[Login]", &format!("✅ AutoClaw 登录成功: {label}"));
            ok_json(json!({ "account": account, "list": store.list_accounts() }))
        }
        Err(error) => super::accounts::store_error(error),
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
