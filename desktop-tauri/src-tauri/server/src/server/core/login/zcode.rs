//! ZCode CLI 轮询登录任务与通用 start / wait / cancel 状态表的衔接。
//!
//! ── 与 Qoder 那支的两处结构差异（都源自「init 是异步的」）────────
//! Qoder 的 `DeviceLogin::new` 是**同步**的（设备授权码由本地随机数先定，
//! 再去问上游），所以它能在 `start_*` 里就把 `auth_url` 写进任务状态、
//! 按 `flow.state` 登记任务表。ZCode 不是：它的授权地址**只能从上游 init
//! 拿**（`POST /oauth/cli/init` 返回 `flow_id` + `authorize_url`），
//! 而 `start_*` 是同步函数（API 层的分派是同步 match），不能 await。
//!
//! 因此这里：
//!   1. 先把任务登记进状态表，state 用**本地生成的关联串**（上游的 flow_id
//!      此刻还不存在）—— 任务表的用途是「按 state 找到 handle 以便取消」，
//!      这个串在本机唯一即可；
//!   2. `auth_url` 在 spawn 出去的任务里 init 成功后回填。★ 所以 `start_*`
//!      返回时它**必定还是空的**：调用方（`api::session::login_start`）要用
//!      `wait_for_auth_url` 等到它落地再响应，不能直接把快照发出去 —— 壳侧
//!      只读一次 start 响应，读到 `authUrl: null` 就直接报「后端未返回登录
//!      链接」，不会去轮询补取。init 通常几十毫秒，等待上限与 WorkBuddy
//!      那条同为 `AUTH_URL_WAIT_MS`。
//!
//! ── 超时取「本地 5 分钟」与「上游 expires_at」的更早者 ────────
//! 上游到点会作废 flow，再轮询只是白打上游；本地超时则兜住「上游没给
//! expires_at」的情形（`CliLogin::expires_at()` 此时为 0，取 min 会变成
//! 一个已过去的时刻，所以只在它非 0 时参与取min）。

use std::time::Duration;

use serde_json::json;

use crate::server::core::providers::zcode::oauth::CliLogin;
use crate::server::core::providers::zcode::region::Region;
use crate::server::core::providers::kind_id;
use crate::server::logging;

use super::{finish_task_error, LoginService, LoginTaskHandle, LOGIN_TIMEOUT_MS};

impl LoginService {
    /// 发起 ZCode 登录（两个地区同一套协议，`region` 决定打哪一站的推理面）。
    ///
    /// 地区由调用方直接给定，不在这里解析字符串：API 层的地区来源是
    /// **provider id**（`zcode` / `zcode-intl`），从 id 反查出 `Region` 后再传进来
    /// —— 中间插一层「转成 edition 串、再解析回来」只会在取值漂移时静默落到
    /// 国内版，而落错家的账号会稳定打错域名（转发时才暴露，且不像登录那样
    /// 一眼可见）。
    pub fn start_zcode_login(&self, region: Region) -> Result<LoginTaskHandle, String> {
        // `edition` 传给任务表只影响日志与前端回显（登录窗口标题等）：
        // 用上游认得的那两个取值（cn / intl），而不是 provider id
        let info = crate::server::core::endpoints::resolve_edition(Some(match region {
            Region::Cn => "cn",
            Region::Intl => "intl",
        }));
        let handle = self.new_handle_for_provider(info, kind_id(region.kind()));
        // 本地关联串（上游 flow_id 要到 init 之后才有，见模块头）
        let state = format!("zcode-{}-{}", region.provider_id(), logging::now_ms());
        handle.update(|task| {
            task.state = Some(state.clone());
        });
        self.tasks.register(&state, handle.clone());
        let service = self.clone();
        let task = handle.clone();
        crate::spawn_task(async move {
            service.run_zcode_login(task, region).await;
        });
        logging::log(
            "[Login]",
            &format!("发起 ZCode {}网页登录（等待授权…）", region.label()),
        );
        Ok(handle)
    }

    async fn run_zcode_login(&self, handle: LoginTaskHandle, region: Region) {
        // init：失败就直接结束（这一步拿不到授权地址，没有可等的东西）
        let flow = match CliLogin::start(region).await {
            Ok(flow) => flow,
            Err(error) => {
                finish_task_error(&handle, &error.message);
                return;
            }
        };
        if handle.snapshot().canceled {
            return;
        }
        // 回填授权地址：前端轮询到它就把用户送去浏览器授权
        let auth_url = flow.auth_url().to_string();
        handle.update(|task| {
            task.auth_url = Some(auth_url.clone());
        });

        let local_deadline = tokio::time::Instant::now() + Duration::from_millis(LOGIN_TIMEOUT_MS);
        // 上游的 expires_at 更权威（到点 flow 作废），但它可能没给（0）
        let upstream_deadline = match flow.expires_at() {
            0 => None,
            seconds => {
                let now_sec = logging::now_ms() / 1000;
                let remaining = (seconds - now_sec).max(0) as u64;
                Some(tokio::time::Instant::now() + Duration::from_secs(remaining))
            }
        };
        let deadline = match upstream_deadline {
            Some(value) => value.min(local_deadline),
            None => local_deadline,
        };

        loop {
            if handle.snapshot().canceled {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                finish_task_error(&handle, "ZCode 网页登录超时，请重新发起");
                return;
            }
            tokio::time::sleep(flow.poll_interval()).await;
            if handle.snapshot().canceled {
                return;
            }
            match flow.poll().await {
                // 还没授权完 —— 继续等
                Ok(None) => continue,
                Ok(Some(credentials)) => {
                    // 与取消共用任务锁：取消先发生就绝不落账号，落盘先发生则视为已完成
                    // （与 Qoder 那支逐字相同的处置）
                    let mut task = handle.lock();
                    if task.done || task.canceled {
                        return;
                    }
                    match self.store.add_zcode_account(&credentials, None, "web") {
                        Ok(account) => {
                            task.session = Some(json!({
                                "accountUid": account.get("id"),
                                "nickname": account.get("name"),
                                "edition": credentials.region.provider_id(),
                                "provider": credentials.region.provider_id(),
                            }));
                            logging::log(
                                "[Login]",
                                &format!(
                                    "✅ ZCode {}网页登录完成，账号已加入列表",
                                    credentials.region.label()
                                ),
                            );
                        }
                        Err(error) => task.error = Some(error.message),
                    }
                    task.done = true;
                    task.finished_at = Some(logging::now_ms());
                    return;
                }
                Err(error) => {
                    finish_task_error(&handle, &error.message);
                    return;
                }
            }
        }
    }
}
