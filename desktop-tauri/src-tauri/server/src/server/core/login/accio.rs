//! Accio 的网页登录任务（OAuth 授权码 + PKCE，回调落在本机 loopback 端口）。
//!
//! ── 这条链在八家登录里排第几种形态 ──────────────────────────
//! ```text
//!   workbuddy  上游推 authUrl（auth/state）→ 轮询 auth/token
//!   小浣熊      本地拼静态授权地址 → 等**壳侧窗口**捕获的自定义协议回调
//!   Qoder      同步问上游要设备码 → 轮询设备令牌
//!   CatPaw     同步问上游要登录入口 → 等**上游 POST 到本机**的回调
//!   Cline      同步问上游要设备码 → 轮询
//!   AutoClaw   前端过一次风控验证码 → 网关拿授权地址 → 等**浏览器 302 到本机**
//!   Accio      本地拼授权地址（PKCE）→ 等**浏览器 302 到本机**
//! ```
//! 与 AutoClaw 最像（都是「浏览器带你回到本机 HTTP 端口」），差别是：
//!   - 授权地址**不需要**先过一次风控验证码，网关自己能拼（见
//!     `providers::accio::oauth::build_authorize_url`），因此走的是**通用**
//!     网页登录入口（`LoginService::start_web_login`，与小浣熊同一条路），
//!     不另开 `/api/session/login/oauth/start` 那种专用路由；
//!   - 回调 URL 里**带着我们的 state**（return_url 是我们给的，登录页把它原样
//!     带回来），因此不需要 AutoClaw 那套「按变体匹配最近一次发起」的关联 ——
//!     直接按 state 查任务表即可。
//!
//! ── 为什么换码要单独一个方法（而不用 `submit_login_callback`）────
//! 那条是**小浣熊专属**的（回调 URL 是 `office-raccoon://auth/callback`，
//! 靠 `parse_callback_code` 解析）。Accio 的回调是标准 HTTP 查询串
//! （`?code=…&state=…`），解析方式与失败语义都不同，因此这条链的收尾写在这里，
//! 由 `/auth/callback-accio` 路由直接调用。

use serde_json::{json, Value};

use crate::server::core::providers::accio::endpoints::Region;
use crate::server::core::providers::accio::oauth;
use crate::server::core::providers::adapter::adapter_for;
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::{finish_task_error, LoginService};

impl LoginService {
    /// 浏览器回调：校验 state → 用授权码换凭证 → 落账号 → 标记任务完成。
    ///
    /// `params` 是回调的查询串（`code` / `state` / 可选的 `error`）。
    ///
    /// ── 校验口径 ────────────────────────────────────────────────
    /// 回调落在本机 HTTP 端口上，任何本机进程都能伪造一次 GET，因此：
    ///   1. 查询串里的 state 必须对应一个**进行中且属于 Accio** 的登录任务；
    ///   2. PKCE verifier 在 pending 表里（只有本进程发起过那一轮才有）——
    ///      它既是换码的必需品，也是「这次回调确实由我们发起」的第二道闸门；
    ///   3. 授权码一次性：任务已完成时直接返回成功（用户刷新页面 / 浏览器预取
    ///      会重复回调，第二次必然拿到「授权码已失效」，把一次成功变成一次失败）。
    pub async fn finish_accio_login(
        &self,
        code: &str,
        state: &str,
    ) -> Result<String, GatewayError> {
        let code = code.trim();
        let state = state.trim();
        if code.is_empty() {
            return Err(GatewayError::with_status(400, "回调没有携带授权码"));
        }
        if state.is_empty() {
            return Err(GatewayError::with_status(400, "回调没有携带 state，无法确认这次登录归属"));
        }
        let Some(handle) = self.tasks.get(state) else {
            return Err(GatewayError::with_status(404, "这次登录已取消或已过期，请重新发起"));
        };
        let snapshot = handle.snapshot();
        let Some(region) = Region::from_provider_id(&snapshot.provider) else {
            return Err(GatewayError::with_status(400, "这次登录不属于 Accio"));
        };
        if snapshot.done {
            // 幂等：重复回调不是错误（见校验口径 3）
            return Ok(String::new());
        }
        match adapter_for(region.kind())
            .exchange_login_code(&self.store, code, state)
            .await
        {
            Ok(account_id) => {
                handle.update(|task| {
                    task.done = true;
                    task.session = Some(json!({
                        "accountUid": account_id,
                        "nickname": Value::Null,
                        "edition": region.edition(),
                        "provider": region.provider_id(),
                    }));
                    task.finished_at = Some(logging::now_ms());
                });
                Ok(account_id)
            }
            Err(error) => {
                finish_task_error(&handle, &error.message);
                logging::log(
                    "[Login]",
                    &format!("❌ Accio {}网页登录换取凭证失败: {}", region.label(), error.message),
                );
                Err(error)
            }
        }
    }

    /// 取消：把 pending 表里那一轮也丢掉（否则换码时才会发现换不了）
    pub fn drop_accio_pending(&self, state: &str) {
        oauth::drop_pending(state);
    }
}
