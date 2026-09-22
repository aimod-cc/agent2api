//! Qoder 设备授权任务与通用 start / wait / cancel 状态表的衔接。

use std::time::Duration;

use serde_json::json;

use crate::server::core::providers::qoder::{endpoints::Region, oauth::DeviceLogin};
use crate::server::core::providers::{kind_id, ProviderKind};
use crate::server::logging;

use super::{finish_task_error, LoginService, LoginTaskHandle, LOGIN_TIMEOUT_MS};

impl LoginService {
    /// 发起 Qoder 设备授权（两站同构，`edition` 决定打哪一站）。
    ///
    /// 缺省 `intl`：老版本前端不带这个字段，行为必须逐字保持。
    pub fn start_qoder_login(&self, edition: Option<&str>) -> Result<LoginTaskHandle, String> {
        let region = Region::parse(edition.unwrap_or("intl")).map_err(|error| error.message)?;
        let flow = DeviceLogin::new(region).map_err(|error| error.message)?;
        // 任务表里的 `edition` 只影响日志与前端回显（登录窗口标题等），
        // 用 provider 自己的取值而不是 workbuddy 那套 id。
        let info = crate::server::core::endpoints::resolve_edition(Some(region.edition()));
        let handle = self.new_handle_for_provider(info, kind_id(ProviderKind::Qoder));
        handle.update(|task| {
            task.state = Some(flow.state.clone());
            task.auth_url = Some(flow.auth_url.clone());
        });
        self.tasks.register(&flow.state, handle.clone());
        let service = self.clone();
        let task = handle.clone();
        tauri::async_runtime::spawn(async move {
            service.run_qoder_login(task, flow).await;
        });
        logging::log("[Login]", &format!("发起 Qoder {}网页登录（等待授权…）", region.label()));
        Ok(handle)
    }

    async fn run_qoder_login(&self, handle: LoginTaskHandle, flow: DeviceLogin) {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(LOGIN_TIMEOUT_MS);
        loop {
            if handle.snapshot().canceled {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                finish_task_error(&handle, "Qoder 网页登录超时，请重新发起");
                return;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
            if handle.snapshot().canceled {
                return;
            }
            let outcome = tokio::time::timeout_at(deadline, flow.poll()).await;
            if handle.snapshot().canceled {
                return;
            }
            match outcome {
                Ok(Ok(None)) => continue,
                Ok(Err(error)) if error.code.as_deref() == Some("qoder_transport") => {
                    logging::verbose("[Login]", "Qoder 设备授权暂时不可达，继续等待");
                }
                Ok(Err(error)) => {
                    finish_task_error(&handle, &error.message);
                    return;
                }
                Err(_) => {
                    finish_task_error(&handle, "Qoder 网页登录超时，请重新发起");
                    return;
                }
                Ok(Ok(Some(credentials))) => {
                    // 与取消共用任务锁：取消先发生就绝不落账号，落盘先发生则视为已完成。
                    let mut task = handle.lock();
                    if task.done || task.canceled {
                        return;
                    }
                    match self.store.add_qoder_account(&credentials, None, "web") {
                        Ok(account) => {
                            // edition 取凭证自己记着的地区（不代表用户这次的界面选择）：
                            // 落账号时 `add_qoder_account` 已按 region 去重与生成 id，
                            // 这里只把它原样回给前端，避免显示成另一站。
                            task.session = Some(json!({
                                "accountUid": account.get("id"),
                                "nickname": account.get("name"),
                                "edition": credentials.region.edition(),
                                "provider": "qoder",
                            }));
                            logging::log("[Login]", &format!(
                                "✅ Qoder {}网页登录完成，账号已加入列表", credentials.region.label()));
                        }
                        Err(error) => task.error = Some(error.message),
                    }
                    task.done = true;
                    task.finished_at = Some(logging::now_ms());
                    return;
                }
            }
        }
    }
}
