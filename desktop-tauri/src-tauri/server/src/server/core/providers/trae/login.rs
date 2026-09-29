//! 一轮 Trae 网页登录的编排：绑端口 → 要授权地址 → 等回调 → 换凭据 → 读身份。
//!
//! ── 这一段为什么必须串在一个对象里 ──────────────────────────
//! 三样东西同生共死，分开传就会漏掉某一条收尾路径：
//!   * **回调监听器**（端口）—— 授权地址里的 `auth_callback_url` 就是它，
//!     地址发出去之后端口不能再变（变了回调就落到别的进程上）；
//!   * **PKCE 的 code_verifier** 与**设备密钥对** —— 换证请求要用当轮的那一份，
//!     公钥已经在授权页那一步跟 DeviceID/MachineID 绑过了；
//!   * **guidance 拿到的 loginHost** —— 回调可能带回另一个（`loginHost` 键），
//!     以回调为准、以发起时的为兜底。
//!
//! 所以它们合在 [`Session`] 里，`Drop` 即释放端口。
//!
//! ── 回调的两条分支（都实测存在于上游）───────────────────────
//! `authCode`（标准授权码）与 `refreshToken`（直接给续期串）。参考实现的优先级
//! 是 **refreshToken 在前**（`pending_persist.go:363`）：拿到续期串就直接走续期链
//! 换新凭据，**换证失败也仍然把这条串存成凭据**（`accessToken = refreshToken`），
//! 因为"存下一条可能可用的凭据"比"丢掉一次已经花掉用户点击的授权"划算。
//! 授权码那条同理：响应只有 refreshToken 时也用它的值当 accessToken 兜底。
//!
//! ── 谁负责超时与取消 ────────────────────────────────────────
//! 不在这里。`complete()` 只等回调；超时与"用户在面板上点了取消"由
//! `core::login::trae` 那一层用任务状态判（任务表是所有 provider 共用的，
//! 取消语义必须与其他家一致：`/cancel` 之后 `/wait` 立刻 404、端口释放）。

use crate::server::errors::GatewayError;
use crate::server::core::proxies::ResolvedProxy;

use super::callback_server::CallbackListener;
use super::credentials::Credential;
use super::oauth::{
    exchange_auth_code, request_login_guidance, verification_uri, Callback, LoginContext, DEFAULT_LOGIN_HOST,
};
use super::profile::{callback_identity, get_user_info, Identity};
use super::refresh::{refresh_candidates, refresh_once};

/// 登录落盘时写死的两个字段（参考实现 `selfCompleteCN` 同值）。
///
/// `domain` 是凭据的谱系标记（决定 ClientID 与目录血统），`api_host` 是
/// OAuth/积分那条链的第一候选 —— 两者都**不**跟着 loginHost 漂：
/// CN 账号的兑换永远打固定 origin（见 `oauth_fallback.go` 注释）。
pub const CN_DOMAIN: &str = "trae.cn";

/// 进行中的一轮登录。
pub struct Session {
    pub context: LoginContext,
    pub login_host: String,
    pub auth_url: String,
    listener: CallbackListener,
}

impl Session {
    /// 起一轮登录：先绑端口，再问 guidance 要登录 host，最后拼授权地址。
    ///
    /// 顺序是有讲究的：**端口必须先落地**，因为 `LoginContext::new` 拿端口去
    /// 拼 `auth_callback_url`；反过来（先造上下文再绑）就会让地址里的端口
    /// 与实际监听的端口不是同一个 —— 上游那个正则照样放行，回调却落错地方。
    pub async fn begin(variant: &str, proxy: Option<&ResolvedProxy>) -> Result<Self, String> {
        let listener = CallbackListener::bind().await?;
        let context = LoginContext::new(variant, listener.port(), crate::server::logging::now_ms())?;
        let login_host = request_login_guidance(proxy).await;
        let auth_url = verification_uri(&login_host, &context);
        Ok(Self { context, login_host, auth_url, listener })
    }

    /// 主动结束这一轮：释放回调端口，并让正在等的 `complete()` 以错误退出。
    ///
    /// 取消路径必须能走到这里 —— 见 `callback_server::CallbackListener::close`
    /// 那一段（会话被 `Arc` 共享着，只靠 `Drop` 释放端口会变成"等超时才还"）。
    pub fn close(&self) {
        self.listener.close();
    }

    /// 把远程浏览器粘贴的回调 query 投递给正在等待的登录流程。
    pub fn submit_callback(&self, query: &str) -> Result<(), String> {
        self.listener.submit_callback(query)
    }

    /// 等回调并把它换成凭据（**不**含落盘，落盘由调用方做）。
    ///
    /// 回调一次只认一个：监听器已经把"认不出材料的噪音"（favicon /
    /// preconnect）挡在门外，所以这里拿到的一定是授权材料或错误。
    pub async fn complete(&self, proxy: Option<&ResolvedProxy>) -> Result<Credential, GatewayError> {
        let query = self
            .listener
            .next_callback()
            .await
            .ok_or_else(|| GatewayError::with_status(408, "Trae 登录回调已丢失（本轮监听已结束），请重新发起登录"))?;
        let callback = Callback::from_query(&query);
        if !callback.error.is_empty() {
            // 上游明说的失败（用户取消、errorCode=20405 设备绑定被拒、
            // isRedirect=false）要原样透出去：那是"这一轮没成"，不是"网关坏了"。
            return Err(GatewayError::with_status(400, callback.error.clone()));
        }
        if callback.auth_code.is_empty() && callback.refresh_token.is_empty() {
            return Err(GatewayError::with_status(400, "Trae 回调里没有授权码也没有续期串"));
        }
        // 回调带回的 loginHost 优先（它才是这次授权实际发生的站点），
        // 没有就退回发起时 guidance 的那一份。
        let login_host = if callback.login_host.is_empty() { self.login_host.clone() } else { callback.login_host.clone() };
        let (access_token, refresh_token, expires_at) = if !callback.refresh_token.is_empty() {
            self.exchange_via_refresh_token(&callback, proxy).await
        } else {
            self.exchange_via_auth_code(&callback, &login_host, proxy).await?
        };
        if access_token.is_empty() && refresh_token.is_empty() {
            return Err(GatewayError::with_status(502, "Trae 换证成功但响应里没有任何令牌，请重试"));
        }
        let mut credential = Credential {
            access_token,
            refresh_token,
            expires_at,
            domain: CN_DOMAIN.to_string(),
            api_host: DEFAULT_LOGIN_HOST.to_string(),
            machine_id: self.context.machine_id.clone(),
            device_id: self.context.device_id.clone(),
            variant: self.context.variant.clone(),
            device_public_key: self.context.device_public_pem.clone(),
            device_private_key: self.context.device_private_pem.clone(),
            ..Default::default()
        };
        // 身份是**读数**，不是门槛：GetUserInfo 挂了就用回调回显，两个都没有
        // 就落一个每个 variant 固定的 unknown 名（见 `profile.rs` 模块头 ——
        // 用 per-login 的随机 id 兜底会造出重复账号）。
        let authoritative = get_user_info(&credential, proxy).await.unwrap_or_default();
        let identity = Identity::merged(&authoritative, &callback_identity(&query), credential.variant());
        credential.uid = identity.uid;
        credential.nickname = identity.nickname;
        credential.enterprise_id = identity.enterprise_id;
        Ok(credential)
    }

    /// `refreshToken` 分支：直接走续期链换新凭据。
    ///
    /// 续期也失败时**不报错**：把这条串既当 accessToken 又当 refreshToken 存下
    /// （参考实现同一条）。判成失败等于把一次已经花掉用户点击的授权丢掉，
    /// 而存下来最坏也只是第一次转发时 401。
    async fn exchange_via_refresh_token(&self, callback: &Callback, proxy: Option<&ResolvedProxy>) -> (String, String, i64) {
        let seed = Credential {
            refresh_token: callback.refresh_token.clone(),
            api_host: DEFAULT_LOGIN_HOST.to_string(),
            domain: CN_DOMAIN.to_string(),
            machine_id: self.context.machine_id.clone(),
            device_id: self.context.device_id.clone(),
            variant: self.context.variant.clone(),
            ..Default::default()
        };
        match refresh_once(&seed, &refresh_candidates(&seed.api_host), proxy).await {
            Ok(next) => (next.access_token, next.refresh_token, next.expires_at),
            Err(_) => (seed.refresh_token.clone(), seed.refresh_token.clone(), 0),
        }
    }

    /// `authCode` 分支：授权码换证。
    async fn exchange_via_auth_code(
        &self,
        callback: &Callback,
        login_host: &str,
        proxy: Option<&ResolvedProxy>,
    ) -> Result<(String, String, i64), GatewayError> {
        let exchanged = exchange_auth_code(&self.context, &callback.auth_code, login_host, proxy).await?;
        // 只有 refreshToken 回来时用它当 accessToken（参考实现 `if accessToken == "" { = refreshToken }`）。
        let access_token = if exchanged.access_token.is_empty() { exchanged.refresh_token.clone() } else { exchanged.access_token };
        Ok((access_token, exchanged.refresh_token, exchanged.expires_at_ms))
    }
}

#[cfg(test)]
mod tests {
    //! 这一层的测试只碰**本机 loopback**：`begin()` 要问 guidance、`complete()`
    //! 成功路径要问 GetUserInfo，那都是真上游 —— 所以这里只测"不需要出网就能
    //! 定论"的分支（错误回调、没材料、取消释放端口、续期失败仍存种子串）。
    use std::sync::Arc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::super::oauth::CALLBACK_PATH;

    use super::*;

    async fn session() -> Arc<Session> {
        let listener = CallbackListener::bind().await.expect("回调监听器要能绑上");
        let port = listener.port();
        Arc::new(Session {
            context: LoginContext::new("solo", port, 1_700_000_000_000).expect("上下文可构造"),
            login_host: "https://api.trae.cn".to_string(),
            auth_url: format!("https://api.trae.cn/authorization?port={port}"),
            listener,
        })
    }

    /// 等一个登录任务收尾，**最多 10 秒**。
    ///
    /// 不这么包的话，任何一条"等待方永远等不到东西"的回归都会表现成
    /// 整个测试套件挂住（而不是某条用例红）—— 那是最难查的失效方式。
    async fn await_with_timeout(handle: tokio::task::JoinHandle<Result<Credential, GatewayError>>) -> Result<Credential, GatewayError> {
        tokio::time::timeout(std::time::Duration::from_secs(10), handle)
            .await
            .expect("10 秒内登录任务应有结论（挂住 = 等待方收不到任何回调）")
            .expect("任务不该 panic")
    }

    /// 往这一轮的回调端口发一个真实 GET（浏览器点完授权后做的事）。
    async fn deliver(session: &Session, query: &str) {
        let port = session.listener.port();
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap_or_else(|error| panic!("回调端口 {port} 应在监听：{error}"));
        let request =
            format!("GET {CALLBACK_PATH}?{query} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).await.expect("请求要写进去");
        // 读到 EOF，但**带超时**：hyper 在 `Connection: close` 上何时关连接
        // 不由我们决定，测试不能挂在那儿（挂住的表现是整个套件不动）。
        let response = tokio::time::timeout(std::time::Duration::from_secs(5), stream.read_to_end(&mut Vec::new()))
            .await
            .expect("5 秒内应读完回执")
            .expect("回执可读");
        assert!(response > 0, "监听器连回执都没给：{query}");
    }

    #[tokio::test]
    async fn an_upstream_error_callback_is_passed_through_verbatim() {
        // 用户取消 / errorCode=20405 设备绑定被拒 / isRedirect=false 都走这条。
        // 判成 500 会把"这一轮没成"说成"网关坏了"，用户会去重启而不是重登。
        let session = session().await;
        let waiting = {
            let session = Arc::clone(&session);
            tokio::spawn(async move { session.complete(None).await })
        };
        deliver(&session, "error=access_denied&errorDescription=user%20cancelled").await;
        let error = await_with_timeout(waiting).await.expect_err("错误回调必须失败");
        assert_eq!(400, error.status_code, "实际：{} {}", error.status_code, error.message);
        assert!(error.message.contains("access_denied"), "上游给的原因要原样出去：{}", error.message);
    }

    #[tokio::test]
    async fn a_callback_with_no_material_is_noise_and_does_not_burn_the_round() {
        // 实测行为（写这条用例时才发现的）：**这种请求根本不会被投递给等待方** ——
        // 判定在监听器那层（`callback_server::looks_like_credential_material`），
        // 不是 `complete()` 里那条"没有授权码也没有续期串"。所以这里断言的是
        // 真正重要的那条性质：**噪音不会把这一轮消耗掉**，用户随后真点完授权
        // 仍然能成。若哪天判定被挪到 complete() 里，这条会先红。
        let session = session().await;
        let waiting = {
            let session = Arc::clone(&session);
            tokio::spawn(async move { session.complete(None).await })
        };
        deliver(&session, "scope=solo&userRegion=cn").await;
        let mut waiting = Some(waiting);
        let pending = tokio::time::timeout(std::time::Duration::from_secs(2), waiting.as_mut().unwrap()).await;
        assert!(pending.is_err(), "无材料的回调被当成正经回调投进来了");
        // 但**同一轮不能被它烧掉**：随后送来真材料（这里用一条错误回调，
        // 因为真授权材料要出网换证）时，等待方必须照常醒并给出结论 ——
        // 少了这后半段，上面那条断言就只证明了"卡住"，没证明"这一轮还活着"。
        deliver(&session, "error=access_denied").await;
        let error = tokio::time::timeout(std::time::Duration::from_secs(10), waiting.take().unwrap())
            .await
            .expect("真材料到达后等待方应醒来")
            .expect("任务不该 panic")
            .expect_err("错误回调应失败");
        assert_eq!(400, error.status_code, "实际：{} {}", error.status_code, error.message);
    }

    #[tokio::test]
    async fn closing_the_session_releases_the_waiter_immediately() {
        // 取消语义（面板点「取消」→ `/cancel`）：等待方必须**立刻**以错误退出，
        // 且端口当场还给系统 —— 否则下一轮登录要么等到 5 分钟超时，
        // 要么抢不到端口（这是 AutoClaw 那个坑的随机端口版本）。
        let session = session().await;
        let port = session.listener.port();
        let waiting = {
            let session = Arc::clone(&session);
            tokio::spawn(async move { session.complete(None).await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        session.close();
        let error = await_with_timeout(waiting).await.expect_err("关闭后不该拿到凭据");
        assert_eq!(408, error.status_code, "实际：{} {}", error.status_code, error.message);
        drop(session);
        // 端口还能再绑上一次 = 真的还掉了（异步任务收尾有一瞬延迟，重试几次）
        let mut rebound = false;
        for _ in 0..40 {
            if CallbackListener::bind().await.is_ok() {
                rebound = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(rebound, "监听器没能释放端口（{port}）");
    }

    #[tokio::test]
    async fn a_failed_renewal_still_keeps_the_refresh_token_the_user_just_granted() {
        // 参考实现 `pending_persist.go:363` 那条"换证失败也存种子串"的分支：
        // 丢掉它等于把一次已经花掉用户点击的授权扔掉。这里把候选指向一个
        // **本机没人听的端口**，保证不碰真上游也必然走失败支。
        let session = session().await;
        let callback = Callback { refresh_token: "rt-just-granted".to_string(), ..Default::default() };
        let (access, refresh, expires) = session.exchange_via_refresh_token(&callback, None).await;
        assert_eq!("rt-just-granted", access, "续期失败时 accessToken 用种子串兜底");
        assert_eq!("rt-just-granted", refresh);
        assert_eq!(0, expires);
    }

    #[tokio::test]
    async fn the_refresh_branch_is_preferred_over_the_auth_code_branch() {
        // 两条材料同时回来时 refreshToken 优先（参考实现同一条）：授权码是一次性的，
        // 先花掉它再去用续期串，等于白废一次授权。
        let session = session().await;
        let callback = Callback {
            auth_code: "AC-once-only".to_string(),
            refresh_token: "rt-wins".to_string(),
            ..Default::default()
        };
        let (access, _, _) = session.exchange_via_refresh_token(&callback, None).await;
        assert_eq!("rt-wins", access, "有续期串时不该去动授权码");
    }
}
