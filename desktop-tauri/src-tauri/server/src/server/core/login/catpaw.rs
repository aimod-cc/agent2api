//! CatPaw 的网页登录：**passport 会话 + loopback 回调**。
//!
//! ── 协议（逆向自本机安装的 CatPaw 客户端，逐条已实测）──────────
//! ```text
//! ① GET  {gateway}/api/gateway/passport/login-config        （无需鉴权）
//!      → data.loginEntryUrl（实测 https://catpaw.meituan.com/api/gateway/passport/login-entry）
//! ② 把 loginEntryUrl 拼上三个 query 参数交给浏览器打开：
//!      ?state=<本地随机>&redirect=<loopback 地址>&sid=<本地随机>
//!    上游 302 → 美团 passport 登录 → settoken → login-callback?state=&sid=&redirect=
//! ③ 登录完成后，`login-callback` 页面把 `{token, state}` **POST 到 redirect 地址**
//!    （CSP 的 form-action 只允许 http://127.0.0.1:* 与 http://localhost:*，
//!     这就是 redirect 白名单的由来）
//! ④ 我们校验 state 后把 token 落账号（token 就是 auth.json 的 auth.accessToken）
//! ```
//!
//! ── 两条通道，都要做（本次修正）─────────────────────────────
//! 客户端是 `Promise.race([loopback, poll-token])` —— **两条通道赛跑，谁先到用谁**。
//! 一开始这里只做了 loopback，理由是「poll 那条路同样要求 loopback redirect，
//! 省不掉约束」。实测证明那个理由不成立：loopback 那次 POST 是**从公网页面
//! （`catpaw.meituan.com`）发往本机 127.0.0.1 的跨源请求**，浏览器会做私有网络
//! 检查（PNA）——官方客户端的 loopback 因此专门回了
//! `Access-Control-Allow-Private-Network: true`（见 `auth-DSFS1FEr.js` 的 `Lr`
//! 常量），我们的网关没有那个头，POST 就被浏览器挡在门外：
//! 用户看到上游的「登录成功」页、窗口一直不关、网关永远等不到凭证。
//!
//! 所以现在两条都做：
//!   - **loopback**（首选）：回调直接送到，毫秒级；
//!   - **poll-token**（兜底）：每 1 秒用自己生成的 `sid` 问一次上游，
//!     拿到就直接是 token。它不经过浏览器，因此不受任何 CORS / PNA 策略影响 ——
//!     这正是官方客户端把它并进来的原因，也是我们的保底。
//! 两条通道共用同一把任务锁：先到的落账号并置 `done`，后到的那条看到 `done`
//! 就静默退出（不会重复落账号，也不会把成功盖成失败）。
//!
//! ── 为什么可以直接复用网关的监听端口 ─────────────────────────
//! `redirect` 白名单只校验「是不是 loopback 地址」，**不要求端口是随机的、
//! 也不校验端口是否在监听**（实测传 `127.0.0.1:1` 依然 302），因此把回调挂在
//! 网关自己的 `127.0.0.1:<gateway_port>` 上是合规的 —— 省掉一个临时监听器，
//! 也省掉「临时端口被占用 / 忘了关」这类故障面。
//!
//! ── 与另三家（workbuddy / 小浣熊 / Qoder）的差别 ──────────────
//! 那三家的凭证由**后端自己去上游取**（轮询 auth/token、设备令牌、授权码换码）；
//! CatPaw 是**上游把 token 推给我们**：登录流程的终点是一次主动 POST。
//! 因此这条链没有轮询循环，只在任务登记后等回调（超时由 `start_catpaw_login`
//! 起的定时任务收尾），与小浣熊那条的结构一致。
//!
//! ── 安全 ────────────────────────────────────────────────────
//! * `state` 逐字比对（一次性随机串）：回调落在本机 HTTP 端口上，任何本机进程
//!   都能伪造一个 POST，不校验就等于「用别人的 token 写进你的账号库」——
//!   与 `raccoon::oauth` 同一威胁模型、同一处置。
//! * token 只进账号库，不进日志（`credentials::log_source` 只打尾部四位）。

use std::time::Duration;

use serde_json::{json, Value};

use crate::server::core::auth_http::send_raw;
use crate::server::core::providers::{kind_id, ProviderKind};
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::{finish_task_error, LoginService, LoginTaskHandle, LOGIN_TIMEOUT_MS};

/// `login-config` 与 `current-user` 所在的基址。
///
/// 客户端按「租户 × 环境」选地址，external 租户的 prod 值就是这个（已实测）。
const GATEWAY_BASE: &str = "https://catx.nocode.cn";

/// 取登录入口地址的接口（客户端 `fetchLoginConfig` 的同一个地址）
const LOGIN_CONFIG_PATH: &str = "/api/gateway/passport/login-config";

/// 取会话信息的接口（登录成功后用它补 uid / 昵称）
const CURRENT_USER_PATH: &str = "/api/gateway/passport/current-user";

/// 轮询取件的接口（兜底通道，见模块头）。`?sid=<sid>` 即可，无需鉴权。
const POLL_TOKEN_PATH: &str = "/api/gateway/passport/poll-token";

/// 回调路径（挂在本网关自己的监听端口上）。
///
/// 路径名上游不校验（实测任意路径都放行），取一个带 provider 前缀的名字，
/// 免得与将来的其它回调撞车。
pub const CALLBACK_PATH: &str = "/api/session/login/catpaw-callback";

/// HTTP 超时（客户端 `fetchLoginConfig` 用 5s，这里给同一个量级）
const REQUEST_TIMEOUT_MS: u64 = 10_000;

/// 单次轮询的超时（客户端 `pollForToken` 用 800ms —— 轮询口本身是轻查询，
/// 给太长会把 1 秒的间隔拖成实际 1 秒 + 超时时间）。
const POLL_TIMEOUT_MS: u64 = 5_000;

impl LoginService {
    /// 发起一次 CatPaw 网页登录：取授权地址并登记任务，等 loopback 回调来收尾。
    ///
    /// `callback_base` 是本网关自己的 loopback 基址（`http://127.0.0.1:<port>`）——
    /// 由调用方给出，因为这个模块拿不到监听端口（它在 `ServerState` 上）。
    ///
    /// 与 `start_web_login`（小浣熊）的差别只有一处：那家的授权地址是**静态**
    /// 拼出来的（`build_authorize_url`），这家要先问一次 `login-config`。
    pub async fn start_catpaw_login(&self, callback_base: &str) -> Result<LoginTaskHandle, String> {
        let entry = fetch_login_entry().await?;
        let state = random_hex()?;
        let sid = random_hex()?;
        let redirect = format!("{}{}", callback_base.trim_end_matches('/'), CALLBACK_PATH);
        let mut url = url::Url::parse(&entry)
            .map_err(|_| "CatPaw 登录入口地址无效".to_string())?;
        url.query_pairs_mut()
            .append_pair("state", &state)
            .append_pair("redirect", &redirect)
            .append_pair("sid", &sid);

        let info = crate::server::core::endpoints::resolve_edition(Some("cn"));
        let handle = self.new_handle_for_provider(info, kind_id(ProviderKind::CatPaw));
        let auth_url = url.to_string();
        handle.update(|task| {
            task.state = Some(state.clone());
            task.auth_url = Some(auth_url.clone());
        });
        self.tasks.register(&state, handle.clone());
        // ── 兜底通道：poll-token 轮询（见模块头）──────────────────
        // 官方客户端也是这么做的（`Promise.race([loopback, poll])`）。这条走
        // **服务端到服务端**，不经浏览器，因此不受任何 CORS / PNA 策略影响；
        // loopback 被拦时它就是唯一能拿到凭证的路。
        let service = self.clone();
        let poll_handle = handle.clone();
        let poll_sid = sid.clone();
        crate::spawn_task(async move {
            service.run_catpaw_poll(poll_handle, poll_sid).await;
        });
        logging::log("[Login]", "发起 CatPaw 网页登录（等待浏览器回调…）");
        Ok(handle)
    }

    /// CatPaw 的 `poll-token` 轮询（兜底通道，直到超时或任务落定）。
    ///
    /// 契约为客户端 `pollForToken` 的同一份（已实测）：
    ///   - `GET {base}/api/gateway/passport/poll-token?sid=<sid>`
    ///   - 未就绪 → `{code:0, data:null}`，继续等；
    ///   - 就绪 → `data` **直接是 token 字符串**（不再包一层结构）；
    ///   - 网络错误单拍忽略（客户端也是 `try/catch` 后继续）。
    ///
    /// 间隔 1 秒（客户端同值）。整体截止时间与 loopback 一致，且**由本任务
    /// 负责超时收尾** —— 两条通道都可能在等，超时只能有一处落定（共用任务锁，
    /// 先到者置 `done`，另一个随后看到就静默退出）。
    async fn run_catpaw_poll(&self, handle: LoginTaskHandle, sid: String) {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(LOGIN_TIMEOUT_MS);
        loop {
            if handle.snapshot().finished_at.is_some() || handle.snapshot().canceled {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                if handle.snapshot().finished_at.is_none() {
                    finish_task_error(&handle, "CatPaw 网页登录超时，请重新发起");
                }
                return;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
            if handle.snapshot().finished_at.is_some() || handle.snapshot().canceled {
                return;
            }
            let url = format!("{GATEWAY_BASE}{POLL_TOKEN_PATH}?sid={sid}");
            let headers = vec![("Accept".to_string(), "application/json".to_string())];
            let Ok(response) = send_raw("GET", &url, None, &headers, None, Some(POLL_TIMEOUT_MS)).await
            else {
                continue; // 单拍失败继续轮询（客户端同）
            };
            let token = response
                .payload
                .as_ref()
                .and_then(|payload| payload.get("data"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string);
            let Some(token) = token else {
                continue; // data:null = 还没就绪
            };
            logging::log("[Login]", "CatPaw 轮询已取到登录凭证（回调通道未命中，走兜底）");
            let _ = self.finish_catpaw_login_with(&handle, &token).await;
            return;
        }
    }

    /// loopback 回调：上游把 `{token, state}` POST 到本网关，这里校验并落账号。
    ///
    /// 返回 `Err(原因)` 表示这次回调不可用（state 不认识 / token 为空 / 已有结果），
    /// 调用方据此回 400（浏览器里会看到失败页）。
    pub async fn finish_catpaw_login(&self, token: &str, state: &str) -> Result<(), String> {
        let token = token.trim();
        if token.is_empty() {
            return Err("回调没有携带 token".to_string());
        }
        let state = state.trim();
        if state.is_empty() {
            return Err("回调没有携带 state".to_string());
        }
        let Some(handle) = self.tasks.get(state) else {
            return Err("这次登录已取消或已过期，请重新发起".to_string());
        };
        // 先看任务本身是否属于 CatPaw、是否还没结束（拿锁前读一次快照即可，
        // 真正的裁决在下面持锁时再做一遍）。
        let snapshot = handle.snapshot();
        if snapshot.provider != kind_id(ProviderKind::CatPaw) {
            return Err("这次登录不属于 CatPaw".to_string());
        }
        if snapshot.done || snapshot.canceled {
            return Err("这次登录已经结束，请重新发起".to_string());
        }
        self.finish_catpaw_login_with(&handle, token).await
    }

    /// 两条通道的**共同收尾**：拿 token 换账号资料并落盘。
    ///
    /// 共用一把任务锁裁决「谁落账号」：先到的置 `done` 并写 `session`，
    /// 后到的看到 `done`/`canceled` 就静默返回（不重复落账号，也不会把已经
    /// 成功的那次盖成失败）—— 这正是 `Promise.race` 两条腿的服务端对应物。
    async fn finish_catpaw_login_with(
        &self,
        handle: &LoginTaskHandle,
        token: &str,
    ) -> Result<(), String> {
        let payload = login_session(token).await;
        let mut task = handle.lock();
        // 持锁后再判一次：网络请求期间另一条通道可能已经落定，或用户取消了。
        if task.done || task.canceled {
            return Ok(());
        }
        let payload = match payload {
            Ok(payload) => payload,
            Err(error) => {
                task.error = Some(error.message.clone());
                task.done = true;
                task.finished_at = Some(logging::now_ms());
                return Err(error.message);
            }
        };
        match self.store.add_catpaw_account(&payload, None) {
            Ok(account) => {
                task.session = Some(json!({
                    "accountUid": account.get("id"),
                    "nickname": account.get("name"),
                    "provider": "catpaw",
                }));
                task.done = true;
                task.finished_at = Some(logging::now_ms());
                logging::log("[Login]", "✅ CatPaw 网页登录完成，账号已加入列表");
                Ok(())
            }
            Err(error) => {
                task.error = Some(error.message.clone());
                task.done = true;
                task.finished_at = Some(logging::now_ms());
                Err(error.message)
            }
        }
    }
}

/// 取登录入口地址（`login-config`，无需鉴权）。
async fn fetch_login_entry() -> Result<String, String> {
    let url = format!("{GATEWAY_BASE}{LOGIN_CONFIG_PATH}");
    let headers = vec![("Accept".to_string(), "application/json".to_string())];
    let response = send_raw("GET", &url, None, &headers, None, Some(REQUEST_TIMEOUT_MS))
        .await
        .map_err(|error| format!("无法连接 CatPaw 登录服务: {error}"))?;
    let payload = response.payload.unwrap_or(Value::Null);
    payload
        .get("data")
        .and_then(|data| data.get("loginEntryUrl"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "CatPaw 登录服务未返回登录入口地址".to_string())
}

/// 32 位小写 hex 随机串（客户端用 `crypto.getRandomValues(16)` 生成 state/sid，
/// 这里沿用同一强度与形态）。
fn random_hex() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).map_err(|_| "无法生成安全的登录随机串".to_string())?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// 把回调里拿到的 `token` 加工成 `add_catpaw_account` 认得的 payload
/// （顺带问一次 `current-user` 补 uid / 昵称）。
///
/// `current-user` 失败**不算登录失败**：token 本身已经到手，uid 缺失时账号库会
/// 用 loginName 兜底，最坏情况是备注名稍差一点，不该因此把一次成功登录判死。
async fn login_session(token: &str) -> Result<Value, GatewayError> {
    let headers = vec![
        ("Accept".to_string(), "application/json".to_string()),
        ("X-Auth-Token".to_string(), token.to_string()),
    ];
    let mut auth = serde_json::Map::new();
    auth.insert("loginType".to_string(), Value::String("passport".to_string()));
    auth.insert("accessToken".to_string(), Value::String(token.to_string()));
    auth.insert("tokenType".to_string(), Value::String("Bearer".to_string()));
    let mut account = serde_json::Map::new();
    let url = format!("{GATEWAY_BASE}{CURRENT_USER_PATH}");
    if let Ok(response) = send_raw("GET", &url, None, &headers, None, Some(REQUEST_TIMEOUT_MS)).await {
        if let Some(data) = response.payload.as_ref().and_then(|value| value.get("data")) {
            // userId 可能是数字或字符串，两种都收（客户端也做 String() 转换）
            let uid = data
                .get("userId")
                .map(|value| match value {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                })
                .unwrap_or_default();
            let name = data
                .get("userName")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if !uid.is_empty() && uid != "null" {
                account.insert("uid".to_string(), Value::String(uid));
            }
            if !name.is_empty() {
                account.insert("loginName".to_string(), Value::String(name.clone()));
                account.insert("name".to_string(), Value::String(name));
            }
        }
    }
    Ok(json!({ "auth": Value::Object(auth), "account": Value::Object(account) }))
}
