//! AutoClaw **国际版**的 OAuth 网页登录任务（与 `core::login` 的任务表衔接）。
//!
//! ── 这条链在四家登录里排第几种形态 ──────────────────────────
//! ```text
//!   workbuddy  上游推 authUrl（auth/state）→ 轮询 auth/token
//!   小浣熊      本地拼静态授权地址 → 等**壳侧窗口**捕获的自定义协议回调
//!   Qoder      同步问上游要设备码 → 轮询设备令牌
//!   CatPaw     同步问上游要登录入口 → 等**上游 POST 到本机**的回调
//!   Cline      同步问上游要设备码 → 轮询
//!   AutoClaw   前端过一次风控验证码 → 网关拿授权地址 → 等**浏览器 302 到本机**
//! ```
//!
//! 与 CatPaw 最像（回调落在本网关自己的 loopback 端口上），差别有两点：
//!   1. CatPaw 是上游页面**表单 POST** 过来，这条是**浏览器导航**（GET，
//!      查询串带 code/state）—— 所以那边要回 HTML 给浏览器看，这边也一样，
//!      但解析的是查询串而不是表单体；
//!   2. 这条的授权地址**不是**我们能自己拼的：它要过一次强制风控验证码
//!      （见 `providers::autoclaw::oauth` 的模块头），因此「发起」这一步
//!      由前端带着验证码参数来调，而不是壳侧 `start_login` 自己发起 ——
//!      验证码必须在**浏览器环境**里跑（阿里云 SDK 是浏览器端 JS）。
//!
//! ── 因此没有「后台轮询」────────────────────────────────────
//! 授权码由浏览器直接送到本机回调上，网关拿到就换凭证 —— 中间没有任何需要
//! 轮询的状态（不像设备授权那样「用户确认了没有」只能靠问上游）。所以这条
//! 链路与 CatPaw 一样：登记任务 → 等回调 → 收尾，超时由一个定时任务兜底。

use std::time::Duration;

use serde_json::{json, Value};

use crate::server::core::providers::autoclaw::oauth::{self, Vendor};
use crate::server::core::providers::autoclaw::Region;
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::{finish_task_error, LoginService, LoginTaskHandle, LOGIN_TIMEOUT_MS};

/// 一次 AutoClaw OAuth 登录任务上额外要记的东西。
///
/// ── 为什么不能塞进 `LoginTaskState` ──────────────────────────
/// 那个结构是**所有 provider 共用**的（`/wait` 按它序列化响应），往里加
/// AutoClaw 专属字段会让每次 `/wait` 都多带一份别人用不到的负载，也会让
/// 「哪些字段是这家独有的」这件事从类型上看不出来。
/// 因此这里另存一张表，按 state 索引 —— 与任务表同生命周期（收尾时一起清）。
///
/// ── `navigate_uri` 为什么要留着 ─────────────────────────────
/// 换码那一跳（`oauth-login`）要求它**与取授权地址时逐字相同**
/// （客户端也是这么传的）。它包含本机端口与 state，回调进来时重新拼不出来
/// （端口在 `ServerState` 上，且 state 虽然能从回调 URL 里取到、但那样拼出的
/// 字符串未必与当初发给上游的那个一致）—— 因此存下来原样回传。
///
/// `pub(super)`：`LoginService` 的字段类型要写它（那张表在 `core::login` 里）。
///
/// 这里**没有** state 字段：表的键就是 state，再存一份等于同一个事实有两处，
/// 而两处一旦不一致（改了一处忘了另一处）就会让「回调是不是我们这一轮」的
/// 判断开始说谎。要比对 state 直接比表的键即可。
pub(super) struct PendingOauth {
    region: Region,
    vendor: Vendor,
    navigate_uri: String,
    device_id: String,
}

impl LoginService {
    /// 发起一次 AutoClaw OAuth 登录：拿授权地址、登记任务、等浏览器回调。
    ///
    /// `captcha_verify_param` 由前端跑完阿里云 SDK 得到（见
    /// `providers::autoclaw::oauth` 的模块头）—— 网关不解析它，只转交上游。
    ///
    /// `callback_base` 是本网关自己的 loopback 基址（`http://127.0.0.1:<port>`），
    /// 由调用方给出（本模块拿不到监听端口，它在 `ServerState` 上）。
    pub async fn start_autoclaw_oauth_login(
        &self,
        region: Region,
        vendor: Vendor,
        captcha_verify_param: &str,
        callback_base: &str,
    ) -> Result<LoginTaskHandle, String> {
        let state = random_hex()?;
        let device_id = oauth::new_oauth_device_id();
        let navigate = oauth::navigate_uri(callback_base, vendor, &state);
        // 这一步要过风控，失败原因（630014 / 631002）由 oauth 模块翻成人话
        let auth_url = oauth::request_oauth_url(
            region,
            vendor,
            &navigate,
            captcha_verify_param,
            &device_id,
        )
        .await
        .map_err(|error| error.message)?;

        let info = crate::server::core::endpoints::resolve_edition(Some("intl"));
        let handle = self.new_handle_for_provider(info, region.provider_id());
        handle.update(|task| {
            task.state = Some(state.clone());
            task.auth_url = Some(auth_url.clone());
        });
        self.tasks.register(&state, handle.clone());
        self.store_pending_oauth(
            &state,
            PendingOauth {
                region,
                vendor,
                navigate_uri: navigate,
                device_id,
            },
        );
        // 超时兜底：这条链没有后台轮询（回调是唯一入口），因此必须有人负责
        // 「用户打开授权页之后一直没回来」的收尾，否则任务会永远留在表里
        // （前端会一直转圈到自己的 5 分钟超时，但那之后任务仍是 pending）。
        self.spawn_autoclaw_oauth_timeout(handle.clone(), state.clone());
        logging::log(
            "[Login]",
            &format!(
                "发起 AutoClaw {} {}登录（等待浏览器回调…）",
                region.label(),
                vendor.label()
            ),
        );
        Ok(handle)
    }

    /// 把这次登录的额外状态记进待办表（见 `PendingOauth`）。
    fn store_pending_oauth(&self, state: &str, pending: PendingOauth) {
        let mut table = self.autoclaw_oauth.lock().unwrap_or_else(|error| error.into_inner());
        table.insert(state.to_string(), pending);
    }

    /// 取一次待办状态（回调进来时用）。
    fn take_pending_oauth(&self, state: &str) -> Option<PendingOauth> {
        let mut table = self.autoclaw_oauth.lock().unwrap_or_else(|error| error.into_inner());
        table.remove(state)
    }

    /// 看一眼待办状态但**不移除**它（校验用，见 `finish_autoclaw_oauth_callback`）。
    ///
    /// 只回变体：它是这次校验唯一需要的事实（state 本身就是表的键，
    /// 而地区在下面 `take` 出来的那份里现成就有）。
    fn peek_pending_vendor(&self, state: &str) -> Option<Vendor> {
        let table = self.autoclaw_oauth.lock().unwrap_or_else(|error| error.into_inner());
        table.get(state).map(|pending| pending.vendor)
    }

    /// 超时收尾：到点任务还没结束就置失败（并清掉待办状态）。
    ///
    /// 与 CatPaw 的 `run_catpaw_poll` 分工相同 —— 那边是轮询循环顺手负责超时，
    /// 这条没有循环，所以单独起一个只睡一次的定时任务。
    fn spawn_autoclaw_oauth_timeout(&self, handle: LoginTaskHandle, state: String) {
        let service = self.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(Duration::from_millis(LOGIN_TIMEOUT_MS)).await;
            if handle.snapshot().finished_at.is_some() || handle.snapshot().canceled {
                return;
            }
            // 任务还在等：落定失败。用与其它家一致的措辞（前端直接展示它）
            finish_task_error(&handle, "网页登录等待超时（5 分钟），请重新发起");
            service.take_pending_oauth(&state);
            logging::log("[Login]", "AutoClaw OAuth 登录等待超时，任务已落定");
        });
    }

    /// 浏览器回调：校验 state → 用授权码换凭证 → 落账号 → 标记任务完成。
    ///
    /// ── 两个 state 是**两个不同的值**（别合并，这是实测踩到的坑）──────
    ///   - `task_state`：**我们**生成的一次性随机串，放在 `navigate_uri` 的
    ///     **路径**里。它只用于「这次回调属于哪一轮登录」的查找与 CSRF 校验；
    ///   - `upstream_state`：**上游**在 302 时拼在查询串里的 `state`。换码
    ///     （`oauth-login`）要求回传的是**这一个** —— 官方客户端也是这么做的
    ///     （`overseaOAuthLogin(vendor, searchParams.get("code"),
    ///     searchParams.get("state"), callbackUri)`，读的就是查询串）。
    ///
    /// 把 `task_state` 当 `upstream_state` 传过去，换码会稳定失败
    /// （上游认不出这个 state 与它发出去的授权码配对）—— 而症状是
    /// 「用户明明登录成功、网关却说授权码无效」，极难从现象反推。
    ///
    /// 返回成功时的账号 id（浏览器停在成功页上，这个值只进日志）。
    ///
    /// ── 校验口径（与 CatPaw 同）─────────────────────────────────
    /// 回调落在本机 HTTP 端口上，任何本机进程都能伪造一次 GET，因此：
    ///   1. task_state 必须对应一个**进行中**的登录任务（否则 404 / 已结束）；
    ///   2. 任务记录的 provider 必须是 AutoClaw 系（防止把别家的 state 送进来）；
    ///   3. 待办表里必须有这个 state，且记录的变体与 URL 里的**一致** ——
    ///      待办表只在我们发起那一轮写入，因此它存在就等于「这个 state 是我们
    ///      发给上游的那个」（表的键就是 state 本身，没有第二份可比）；
    ///   4. 授权码一次性：重复回调直接返回成功，不再换一次码
    ///      （第二次必然得到 631001「授权码无效」，把一次成功变成一次失败）。
    pub async fn finish_autoclaw_oauth_callback(
        &self,
        vendor: Vendor,
        task_state: &str,
        upstream_state: &str,
        code: &str,
    ) -> Result<String, GatewayError> {
        let state = task_state.trim();
        let code = code.trim();
        if code.is_empty() {
            return Err(GatewayError::with_status(400, "回调没有携带授权码"));
        }
        if state.is_empty() {
            return Err(GatewayError::with_status(400, "回调没有携带 state"));
        }
        let Some(handle) = self.tasks.get(state) else {
            return Err(GatewayError::with_status(
                404,
                "这次登录已取消或已过期，请重新发起",
            ));
        };
        let snapshot = handle.snapshot();
        if Region::from_provider_id(&snapshot.provider).is_none() {
            return Err(GatewayError::with_status(400, "这次登录不属于 AutoClaw"));
        }
        if snapshot.done || snapshot.canceled {
            // 幂等：同一个回调被送来两次（用户刷新页面 / 浏览器预取）不是错误
            return Ok(String::new());
        }
        // ── 先校验、后取走（顺序不能反）─────────────────────────
        // `peek` 而不是 `take`：变体不匹配的那次回调**不能**把待办状态吃掉 ——
        // 吃掉了的话，紧接着到达的、真正匹配的那次回调会看到「上下文已丢失」，
        // 一次正常登录就被一个伪造（或陈旧）的回调毁掉了。只有确认这一轮
        // 确实是我们的（变体一致），才在下面 take 走它。
        let Some(pending_vendor) = self.peek_pending_vendor(state) else {
            // 待办状态不在（进程重启 / 已被超时任务清掉）：任务表里还有它，
            // 但我们拼不出那次 `navigate_uri`，换码必然失败 —— 说清原因
            finish_task_error(&handle, "登录上下文已丢失，请重新发起");
            return Err(GatewayError::with_status(410, "登录上下文已丢失，请重新发起"));
        };
        if pending_vendor != vendor {
            // ── 不碰任务、也不吃掉待办状态（有意的）─────────────────
            // 走到这里说明 URL 里的变体与发起时那个不符。可能是伪造的请求，
            // 也可能是我们自己拼错了 —— 两种情况下**都不该动这次登录**：
            //   · 若是伪造：把任务标记失败等于让一个本机进程能掐断用户正在
            //     进行的登录（DoS）；而它本来什么都换不到（state 校验在后面）；
            //   · 若是我们自己的 bug：更不该顺手把用户那次登录也毁了。
            // 因此只回一条错误，任务照旧等着真正的那次回调。
            return Err(GatewayError::with_status(
                400,
                "登录回调与本次登录不匹配，请重新发起",
            ));
        }
        let Some(pending) = self.take_pending_oauth(state) else {
            finish_task_error(&handle, "登录上下文已丢失，请重新发起");
            return Err(GatewayError::with_status(410, "登录上下文已丢失，请重新发起"));
        };
        // 换码要用**上游**的 state（见上面的说明）；它为空时退回落空串 ——
        // 官方客户端也是 `searchParams.get("state") || ""`，上游接受这种形态
        let credentials = match oauth::exchange_code(
            pending.region,
            vendor,
            code,
            upstream_state.trim(),
            &pending.navigate_uri,
            &pending.device_id,
        )
        .await
        {
            Ok(credentials) => credentials,
            Err(error) => {
                finish_task_error(&handle, &error.message);
                logging::log(
                    "[Login]",
                    &format!("❌ AutoClaw OAuth 换取凭证失败: {}", error.message),
                );
                return Err(error);
            }
        };
        // 备注名：上游给了 user_name 就用它（OAuth 账号有用户名），
        // 与手机验证码那条用脱敏手机号的分工一致
        let name = credentials
            .get("userName")
            .and_then(Value::as_str)
            .map(str::to_string);
        match self
            .store
            .add_autoclaw_account(pending.region, &credentials, name.as_deref())
        {
            Ok(account) => {
                let account_id = account
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let label = account
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("AutoClaw 账号");
                handle.update(|task| {
                    task.done = true;
                    task.session = Some(json!({
                        "accountUid": account.get("id").cloned().unwrap_or(Value::Null),
                        "nickname": account.get("name").cloned().unwrap_or(Value::Null),
                        "provider": pending.region.provider_id(),
                        "edition": "intl",
                    }));
                    task.finished_at = Some(logging::now_ms());
                });
                logging::log(
                    "[Login]",
                    &format!("✅ AutoClaw OAuth 登录成功: {label}"),
                );
                Ok(account_id)
            }
            Err(error) => {
                finish_task_error(&handle, &error.message);
                Err(GatewayError::with_status(error.status_code, error.message))
            }
        }
    }
}

/// 32 位小写 hex 随机串（与 CatPaw 的 `random_hex` 同一强度与形态）。
///
/// 不复用那边的私有函数：两处都只是十行，而跨模块暴露一个「只是为了少写十行」
/// 的工具函数会让 `core::login` 的模块边界变模糊。这里的用途是**安全**的
/// （一次性 state），所以两边都必须用密码学随机源。
fn random_hex() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).map_err(|_| "无法生成安全的登录随机串".to_string())?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}
