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
///
/// `created_at`：回调关联用。`navigate_uri` 是客户端同款形态、**不带**
/// state（见 `providers::autoclaw::oauth::CALLBACK_PATH_PREFIX`），回调进来时
/// 靠「变体匹配 + 最近发起」找到它属于哪一轮 —— 同一变体同时挂着两轮登录时
/// 取最近发起的那个（用户几乎不会这么做；真发生了，早的那轮等超时收尾）。
pub(super) struct PendingOauth {
    region: Region,
    vendor: Vendor,
    navigate_uri: String,
    device_id: String,
    created_at: i64,
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
        let navigate = oauth::navigate_uri(callback_base, vendor);
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
                created_at: logging::now_ms(),
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

    /// 回调关联：按变体找那一轮登录的 `(state, handle)`。
    ///
    /// `navigate_uri` 是客户端同款形态、**不带**任何任务标识（见
    /// `providers::autoclaw::oauth::CALLBACK_PATH_PREFIX`），回调 URL 里只有
    /// 路径末段的变体与上游拼回的 code/state —— 任务关联靠这里：在待办表里
    /// 筛出变体匹配的候选，按发起时间**取最近的**一个。
    ///
    /// 同一变体并发两轮的场景没有完美答案（上游的授权码只对应其中一轮），
    /// 「最近发起」是实际使用下最可能的意图；早的那轮由超时兜底收尾。
    /// 已取消的任务直接跳过（回调迟到时不应复活它）。
    ///
    /// 锁序：先在待办表的锁内收集候选（state + 发起时间），**释放后再**查任务
    /// 表 —— 两张表各是独立锁，不在一张锁的临界区里去碰另一张，避免与
    /// `start`（先任务后待办）形成反向嵌套。
    fn find_pending_for_vendor(&self, vendor: Vendor) -> Option<(String, LoginTaskHandle)> {
        let mut candidates: Vec<(String, i64)> = {
            let table = self.autoclaw_oauth.lock().unwrap_or_else(|error| error.into_inner());
            table
                .iter()
                .filter(|(_, pending)| pending.vendor == vendor)
                .map(|(state, pending)| (state.clone(), pending.created_at))
                .collect()
        };
        candidates.sort_by(|left, right| right.1.cmp(&left.1));
        for (state, _) in candidates {
            let Some(handle) = self.tasks.get(&state) else {
                continue;
            };
            if handle.snapshot().canceled {
                continue;
            }
            return Some((state, handle));
        }
        None
    }

    /// 超时收尾：到点任务还没结束就置失败（并清掉待办状态）。
    ///
    /// 与 CatPaw 的 `run_catpaw_poll` 分工相同 —— 那边是轮询循环顺手负责超时，
    /// 这条没有循环，所以单独起一个只睡一次的定时任务。
    fn spawn_autoclaw_oauth_timeout(&self, handle: LoginTaskHandle, state: String) {
        let service = self.clone();
        crate::spawn_task(async move {
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

    /// 浏览器回调：校验 → 用授权码换凭证 → 落账号 → 标记任务完成。
    ///
    /// ── 回调 URL 里只有上游的 state（没有我们的 task_state）────────
    /// `navigate_uri` 是客户端同款形态、不带任何任务标识（见
    /// `oauth::CALLBACK_PATH_PREFIX`），上游 302 拼回的 `?code=…&state=…` 里的
    /// `state` 是**上游自己生成的**那个（`upstream_state`）—— 换码
    /// （`oauth-login`）要求回传的正是它（官方客户端也是
    /// `searchParams.get("state")`）。我们自己的任务 state 只存在于待办表的
    /// 键里，回调靠 [`Self::find_pending_for_vendor`]（变体匹配 + 最近发起）
    /// 关联，URL 里既放不下也不能放（放查询串会与上游的 state 撞参数名，
    /// 放子路径会偏离客户端已验证的形态）。
    ///
    /// 返回成功时的账号 id（浏览器停在成功页上，这个值只进日志）。
    ///
    /// ── 校验口径 ────────────────────────────────────────────────
    /// 回调落在本机 HTTP 端口上，任何本机进程都能伪造一次 GET，因此：
    ///   1. 关联到的任务必须**进行中且属于 AutoClaw 系**；
    ///   2. 授权码一次性：同一个任务被回调两次（用户刷新页面 / 浏览器预取）
    ///      直接返回成功，不再换一次码（第二次必然得到 631001
    ///      「授权码无效」，把一次成功变成一次失败）。
    ///
    /// 与旧形态（state 进回调 URL 逐字比对）的取舍：伪造回调不再被秘密 state
    /// 挡住 —— 本机进程可以毁掉一场进行中的登录（换码失败 → 任务失败）。
    /// 这是 loopback 回调威胁模型下的可接受代价：换不到任何凭证，重试即可。
    pub async fn finish_autoclaw_oauth_callback(
        &self,
        vendor: Vendor,
        upstream_state: &str,
        code: &str,
    ) -> Result<String, GatewayError> {
        let code = code.trim();
        if code.is_empty() {
            return Err(GatewayError::with_status(400, "回调没有携带授权码"));
        }
        let Some((state, handle)) = self.find_pending_for_vendor(vendor) else {
            return Err(GatewayError::with_status(
                404,
                "这次登录已取消或已过期，请重新发起",
            ));
        };
        let snapshot = handle.snapshot();
        if Region::from_provider_id(&snapshot.provider).is_none() {
            return Err(GatewayError::with_status(400, "这次登录不属于 AutoClaw"));
        }
        if snapshot.done {
            // 幂等：同一个回调被送来两次（用户刷新页面 / 浏览器预取）不是错误
            return Ok(String::new());
        }
        let Some(pending) = self.take_pending_oauth(&state) else {
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
