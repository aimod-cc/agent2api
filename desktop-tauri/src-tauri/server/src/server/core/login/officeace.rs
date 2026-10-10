//! OfficeAce 自助登录任务：**服务端中介的轮询**（与 ZCode 同一形态）。
//!
//! ── 与 ZCode 那支的两处结构共性 ──────────────────────────────
//!   · 授权地址**要等一次网络往返**（OfficeAce 是先 `POST /v1/claw/auth/state`
//!     拿 state、再拼地址），而 `start_*` 是同步函数 —— 所以先把任务登记进
//!     状态表（state 用本地关联串），再由 spawn 出去的任务回填 `auth_url`；
//!     `api::session::login_start` 用 `wait_for_auth_url` 等它落地再响应。
//!   · 取码靠**轮询云端**（OfficeAce 的授权回调页在云端、不往本机跳），
//!     与 ZCode 的 `flow.poll()` 同构。
//!
//! ── 与本仓 CodeArts 那支最大的差别 ────────────────────────────
//! CodeArts 是本机 loopback 回调（要壳侧开监听窗口）；OfficeAce **不需要**，
//! 用户只需在浏览器里登完、回面板等几秒。所以这里不开窗口、不注册回调路由。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use std::time::Duration;

use serde_json::json;

use crate::server::core::providers::officeace::oauth::LoginFlow;
use crate::server::core::providers::ProviderKind;
use crate::server::core::providers::kind_id;
use crate::server::logging;

use super::{finish_task_error, LoginService, LoginTaskHandle};

impl LoginService {
    /// 发起 OfficeAce 自助登录（等待浏览器授权）。
    pub fn start_officeace_login(&self) -> Result<LoginTaskHandle, String> {
        let info = crate::server::core::endpoints::resolve_edition(None);
        let handle = self.new_handle_for_provider(info, kind_id(ProviderKind::OfficeAce));
        // 本地关联串（真 state 要到 `LoginFlow::start` 之后才有）
        let state = format!("officeace-{}", logging::now_ms());
        handle.update(|task| {
            task.state = Some(state.clone());
        });
        self.tasks.register(&state, handle.clone());
        let service = self.clone();
        let task = handle.clone();
        crate::spawn_task(async move {
            service.run_officeace_login(task).await;
        });
        logging::log("[Login]", "发起 OfficeAce 果办网页登录（等待授权…）");
        Ok(handle)
    }

    async fn run_officeace_login(&self, handle: LoginTaskHandle) {
        // ① 要 state → ③ 拼授权地址：失败就直接结束
        let flow = match LoginFlow::start().await {
            Ok(flow) => flow,
            Err(error) => {
                finish_task_error(&handle, &error);
                return;
            }
        };
        if handle.snapshot().canceled {
            return;
        }
        // 回填授权地址：**同时**把真 state 写进任务（前端只展示，不参与匹配）
        let auth_url = flow.auth_url().to_string();
        let real_state = flow.state().to_string();
        handle.update(|task| {
            task.auth_url = Some(auth_url.clone());
            task.state = Some(real_state.clone());
        });
        self.tasks.register(&real_state, handle.clone());

        let deadline =
            tokio::time::Instant::now() + Duration::from_millis(super::LOGIN_TIMEOUT_MS);
        loop {
            if handle.snapshot().canceled {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                finish_task_error(&handle, "OfficeAce 网页登录超时，请重新发起");
                return;
            }
            tokio::time::sleep(flow.poll_interval()).await;
            if handle.snapshot().canceled {
                return;
            }
            let code = match flow.poll_code().await {
                // 还没授权完 —— 继续等
                Ok(None) => continue,
                Ok(Some(code)) => code,
                // 404 等作废情形：立即结束（不拖到 10 分钟超时）
                Err(error) => {
                    finish_task_error(&handle, &error);
                    return;
                }
            };
            // ⑤⑥ 换凭据（网络动作，放在拿任务锁之前）
            let credential = match flow.finish(&code).await {
                Ok(credential) => credential,
                Err(error) => {
                    logging::log("[Login]", &format!("❌ OfficeAce 登录换取凭据失败: {error}"));
                    finish_task_error(&handle, &error);
                    return;
                }
            };
            // 与取消共用任务锁：取消先发生就绝不落账号
            let mut task = handle.lock();
            if task.done || task.canceled {
                return;
            }
            let payload = json!({
                "baseUrl": credential.base_url,
                "modelAppKey": credential.model_app_key,
                "modelAppSecret": credential.model_app_secret,
                "accessKeyId": credential.access_key_id,
                "secretAccessKey": credential.secret_access_key,
                "securityToken": credential.security_token,
                "projectId": credential.project_id,
                "expiresAt": credential.expires_at,
            });
            match self.store.add_officeace_account(&payload, None) {
                Ok(account) => {
                    task.session = Some(json!({
                        "accountUid": account.get("id"),
                        "nickname": account.get("name"),
                        "provider": kind_id(ProviderKind::OfficeAce),
                    }));
                    logging::log("[Login]", "✅ OfficeAce 果办网页登录完成，账号已加入列表");
                }
                Err(error) => task.error = Some(error.message),
            }
            task.done = true;
            task.finished_at = Some(logging::now_ms());
            return;
        }
    }
}
