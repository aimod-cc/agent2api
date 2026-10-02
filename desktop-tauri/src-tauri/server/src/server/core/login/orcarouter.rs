//! OrcaRouter 的网页登录任务收尾（OAuth 2.0 授权码 + PKCE S256，
//! 回调落在本机 loopback 端口）。
//!
//! ── 这条链在九家登录里排第几种形态 ──────────────────────────
//! ```text
//!   workbuddy  上游推 authUrl（auth/state）→ 轮询 auth/token
//!   小浣熊      本地拼静态授权地址 → 等**壳侧窗口**捕获的自定义协议回调
//!   Qoder      同步问上游要设备码 → 轮询设备令牌
//!   CatPaw     同步问上游要登录入口 → 等**上游 POST 到本机**的回调
//!   Cline      同步问上游要设备码 → 轮询
//!   AutoClaw   前端过一次风控验证码 → 网关拿授权地址 → 等**浏览器 302 到本机**
//!   Accio      本地拼授权地址（PKCE）→ 等**浏览器 302 到本机**
//!   ZCode      服务端中介的 CLI 轮询
//!   Trae       本地拼授权地址 + 本机回调监听
//!   OrcaRouter 本地拼授权地址（PKCE）→ 等**浏览器 302 到本机**
//! ```
//! 与 Accio 是**同一条形态**（都是「本地拼 PKCE 授权地址 → 浏览器带 code
//! 回到本机 loopback」），差别只有两处：认证 origin 不同、以及本家**没有**
//! 按地区参数化的两份实现（OrcaRouter 只有一个服务端）。因此这条链刻意
//! **不**另发明一套机制：`/api/session/login/start` 走的仍是通用的
//! `start_web_login`（与 Accio 同一个入口），收尾也走同一套：
//! `LoginService::finish_orcarouter_login` 在这里，回调路由在 `api::session`。
//!
//! ── 为什么换码要单独一个方法（而不用 `submit_login_callback`）────
//! 那条是**小浣熊专属**的（回调 URL 是 `office-raccoon://auth/callback`，
//! 靠 `parse_callback_code` 解析）。OrcaRouter 的回调是标准 HTTP 查询串
//! （`?code=…&state=…`），解析方式、state 校验口径（常量时间）与失败语义
//! 都不同，因此这条链的收尾写在这里，由 `/auth/callback-orcarouter` 路由直接调用。
//!
//! ── state 的两道校验（都在，不要删任何一道）────────────────────
//!   1. 任务表里按 `state` 能找到这一轮**属于 OrcaRouter** 的登录
//!      （`LoginTasks` 的既有能力，见 [`LoginService::finish_orcarouter_login`]）；
//!   2. PKCE pending 表里按 `state` 能取到 verifier，且回调查询串里的 `state`
//!      与预期值**常量时间**相等（`login::parse_callback`）。
//! 只有第 2 道能证明「这次回调确实由本进程发起」——它是换码的前提。
//!
//! ── 硬约束 ────────────────────────────────────────────────
//! 绝不 unwrap/expect/panic；verifier / code / key 一律不进日志与错误文案。

use crate::server::core::providers::orcarouter::login;
use crate::server::core::providers::adapter::adapter_for;
use crate::server::core::providers::ProviderKind;
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::{finish_task_error, LoginService};

impl LoginService {
    /// 浏览器回调：校验 state → 用授权码换 Key → 落账号 → 标记任务完成。
    ///
    /// `params` 是回调的查询串（`code` / `state` / 可选的 `error`）。
    ///
    /// ── 校验顺序（规范明确要求 state 先于 code）───────────────────
    /// 回调落在本机 HTTP 端口上，任何本机进程都能伪造一次 GET。因此先把
    /// 查询串里的 `state` 与**本进程发起时生成的那个**做常量时间比对
    /// （`login::parse_callback`），比对通过才去碰 `code`。
    ///
    /// ── 为什么「任务已完成」要幂等返回成功 ────────────────────────
    /// 用户刷新回调页、浏览器预取、公司代理重放都会让同一个回调打进来两次；
    /// 第二次必然拿到「授权码已失效（403）」。把一次成功变成一次失败是纯粹的
    /// 体验损伤，所以已完成的任务直接返回成功（与 Accio 同一处置）。
    pub async fn finish_orcarouter_login(
        &self,
        code: &str,
        state: &str,
    ) -> Result<String, GatewayError> {
        let code = code.trim();
        let state = state.trim();
        if state.is_empty() {
            return Err(GatewayError::with_status(
                400,
                "回调没有携带 state，无法确认这次登录归属（可能不是本网关发起的那一轮）",
            ));
        }
        let Some(handle) = self.tasks.get(state) else {
            return Err(GatewayError::with_status(
                404,
                "这次 OrcaRouter 登录已取消或已过期，请重新发起",
            ));
        };
        let snapshot = handle.snapshot();
        if snapshot.provider != crate::server::core::providers::kind_id(
            ProviderKind::OrcaRouter,
        ) {
            return Err(GatewayError::with_status(400, "这次登录不属于 OrcaRouter"));
        }
        if snapshot.done {
            // 幂等：重复回调不是错误（见上面的说明）
            return Ok(String::new());
        }
        if code.is_empty() {
            // 拒绝 / 超时也会走回调查询串（没有 code），这里如实报出并作废这一轮
            let reason = "授权被拒绝或未完成（回调没有携带授权码），请重新发起登录";
            self.drop_orcarouter_pending(state);
            finish_task_error(&handle, reason);
            return Err(GatewayError::with_status(400, reason));
        }
        // 深度防御：解析层再做一次常量时间 state 比对（`adapter` 里还会按
        // pending 表取一次 verifier —— 那才是真正不可伪造的那道闸门）
        let expected = state.to_string();
        let mut query = std::collections::HashMap::new();
        query.insert("code".to_string(), code.to_string());
        query.insert("state".to_string(), state.to_string());
        let parsed = match login::parse_callback(&query, &expected) {
            Ok(code) => code,
            Err(error) => {
                self.drop_orcarouter_pending(state);
                finish_task_error(&handle, &error.message);
                logging::log(
                    "[Login]",
                    &format!("❌ OrcaRouter 网页登录回调被拒绝: {}", error.message),
                );
                return Err(error);
            }
        };
        match adapter_for(ProviderKind::OrcaRouter)
            .exchange_login_code(&self.store, &parsed, state)
            .await
        {
            Ok(account_id) => {
                handle.update(|task| {
                    task.done = true;
                    task.session = Some(serde_json::json!({
                        "accountUid": account_id,
                        "nickname": serde_json::Value::Null,
                        "provider": "orcarouter",
                    }));
                    task.finished_at = Some(logging::now_ms());
                });
                Ok(account_id)
            }
            Err(error) => {
                finish_task_error(&handle, &error.message);
                logging::log(
                    "[Login]",
                    &format!("❌ OrcaRouter 网页登录换取凭据失败: {}", error.message),
                );
                Err(error)
            }
        }
    }

    /// 取消 / 失败时把 pending 表里那一轮也丢掉。
    ///
    /// 不清的话这一轮的 verifier 会一直留在表里占到超时（11 分钟）——
    /// 而用户取消后往往立刻重试，那会撞上 `MAX_PENDING` 的淘汰逻辑。
    pub fn drop_orcarouter_pending(&self, state: &str) {
        login::drop_pending(state);
    }
}
