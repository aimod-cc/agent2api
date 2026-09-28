//! Trae 网页登录的**本机回调监听器**（一次登录一个，随机端口）。
//!
//! ── 为什么不能挂在网关自己的端口上 ─────────────────────────
//! AutoClaw / Accio / CatPaw 的回调都落在网关端口上的一个已知路径，靠回调里
//! 的 `state` 认回那一轮登录。Trae **没有 state**：它的回调只有
//! `authCode` / `refreshToken` / `loginHost` / `userTag` 这几个键（向量
//! `callback` 段 12 条用例逐条验过），没有任何字段能把一次回调对回一次登录。
//! 于是"这一轮是谁的"只能由**端口**来表达 —— 与官方客户端同款：
//! 登录时现绑一个 loopback 端口，把 `http://127.0.0.1:<port>/authorize`
//! 原样放进授权地址的 `auth_callback_url`。
//!
//! 而且这个 URL 是上游**逐字校验**的（参考实现注释：
//! `^http://127.0.0.1:<port>/authorize$`），路径、host 写法都不许漂 —— 漂了
//! 的表现是登录页直接拒绝，而不是一句可读的错误。
//!
//! ── 与 autoclaw::callback_server 的分工差异 ─────────────────
//! 那个是**纯转发器**（把登记端口上的请求 302 回网关，换码逻辑只有网关一份）。
//! 这里必须自己认回调：Trae 没有 state，转发回网关就等于丢掉端口这个唯一身份；
//! 且上游要求回调 URL 就是这一台机器的这个端口，没有第二跳可绕。
//!
//! ── 浏览器噪音怎么处理 ──────────────────────────────────────
//! 打开授权页的那一类请求里，落到本端口上的第一个请求经常不是真回调
//! （preconnect / favicon / 标签页预览）。判据交给
//! [`super::oauth::Callback::resolves_login`]：认不出授权材料就回 **404 且
//! 不投递**，监听器继续等 —— 一次就收尾等于把真回调挡在门外（CPA 同一条，
//! 它的 `handleCallbackConn` 对认不出的 query 回 404 后接着 listen）。
//!
//! ── 端口什么时候还 ─────────────────────────────────────────
//! `Drop` 时通知监听任务退出。它被存在 `PendingLogin` 里（`login.rs`），
//! 而那条记录在回调换证、超时、取消、以及**下一次登录发起**时都会被移除 ——
//! 四条收尾路径都通向端口释放，不需要显式 close。

use std::net::{Ipv4Addr, Ipv6Addr};

use axum::extract::{OriginalUri, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};

use crate::server::logging;

use super::oauth::{Callback, CALLBACK_PATH};

/// 一次登录占用的 loopback 监听器。
///
/// `port` 用来拼授权地址（必须先 bind 成功、再拿真实端口造
/// `LoginContext`，否则回调 URL 与实际监听地址会不是同一个端口）；
/// `inbox` 由 [`Self::next_callback`] 消费。
pub struct CallbackListener {
    port: u16,
    shutdown: watch::Sender<bool>,
    /// 远程浏览器粘贴回调地址时，复用真实 HTTP 回调的投递通道。
    sender: std::sync::Mutex<Option<mpsc::UnboundedSender<String>>>,
    inbox: tokio::sync::Mutex<mpsc::UnboundedReceiver<String>>,
}

impl CallbackListener {
    /// 绑一个空闲 loopback 端口（IPv4 必须成功；IPv6 尽力而为）。
    ///
    /// 只绑回环：回调里带着一次性授权码，不给局域网开口子（与
    /// `autoclaw::callback_server` 同一口径）。
    pub async fn bind() -> Result<Self, String> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|error| format!("绑定本机回调端口失败：{error}"))?;
        let port = listener
            .local_addr()
            .map_err(|error| format!("读不到回调端口：{error}"))?
            .port();
        let (shutdown, receiver) = watch::channel(false);
        let (sender, inbox) = mpsc::unbounded_channel();
        let app = router(sender.clone());
        spawn_server(listener, app.clone(), receiver.clone());
        // 浏览器把 localhost 解析成 ::1 时也要有人接（多数环境先试 IPv4，
        // 但 macOS / 双栈 Docker 上出现过只连 ::1 的情况）。拿不到不影响。
        if let Ok(v6) = TcpListener::bind((Ipv6Addr::LOCALHOST, port)).await {
            spawn_server(v6, app, receiver);
        }
        logging::log("[Login]", &format!("Trae OAuth 回调已监听 127.0.0.1:{port}{CALLBACK_PATH}"));
        Ok(Self {
            port,
            shutdown,
            sender: std::sync::Mutex::new(Some(sender)),
            inbox: tokio::sync::Mutex::new(inbox),
        })
    }

    /// 这一轮占到的端口（拼 `auth_callback_url` 用）。
    pub fn port(&self) -> u16 {
        self.port
    }

    /// 主动收摊：让监听任务退出、路由状态（含投递用的 sender）随之释放。
    ///
    /// ── 为什么"取消"必须有这一手，不能只靠 `Drop` ────────────
    /// 会话被 `Arc` 共享着：待办表移除那条记录时，正在 `await` 回调的任务
    /// **自己还握着一份引用**，`Drop` 因此不会发生 —— 只靠 Drop 释放端口的话，
    /// 取消会退化成"等 5 分钟超时才还端口"。所以取消/超时/完成这三条收尾路径
    /// 都要显式 close 一次（幂等：`watch` 重复置位没有副作用）。
    /// close 之后 `next_callback` 会以 `None` 结束，等待方据此退出。
    pub fn close(&self) {
        self.sender
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        let _ = self.shutdown.send(true);
    }

    /// 等下一次**真回调**（认不出的噪音不会进到这里）。
    ///
    /// 返回 `None` = 通道关闭（监听器已 close / 任务已退出），调用方按"这一轮
    /// 不会再有回调"处理，不要当成空 query 继续循环。
    pub async fn next_callback(&self) -> Option<String> {
        let mut guard = self.inbox.lock().await;
        guard.recv().await
    }

    /// 从受保护的网关接口注入一条用户粘贴的回调 query。
    ///
    /// 注入仍经过与真实 HTTP 回调相同的 `Callback::resolves_login` 判定，
    /// 因此空地址和浏览器噪音不会消耗这一轮登录。
    pub fn submit_callback(&self, query: &str) -> Result<(), String> {
        let callback = Callback::from_query(query);
        if !callback.resolves_login() {
            return Err("回调地址没有携带可用的 Trae 授权信息".to_string());
        }
        let sender = self
            .sender
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            .ok_or_else(|| "Trae 登录任务已结束，请重新发起".to_string())?;
        sender
            .send(query.to_string())
            .map_err(|_| "Trae 登录任务已结束，请重新发起".to_string())
    }
}

impl Drop for CallbackListener {
    fn drop(&mut self) {
        // 引用计数归零时兜底关一次（正常路径已经显式 close 过，这里是幂等兜底：
        // 忘记放进待办表、或待办表被整体清空时，端口不至于占到进程结束）。
        let _ = self.shutdown.send(true);
    }
}

/// 只有 `/authorize` 一条路径（上游那个正则的对面一半），其余 404。
fn router(sender: mpsc::UnboundedSender<String>) -> Router {
    Router::new()
        .route(CALLBACK_PATH, get(authorize))
        .fallback(not_found)
        .with_state(sender)
}

/// 收下一次回调：认出材料才投递，否则回 404 让监听器继续等。
///
/// 用 `OriginalUri` 拿**原始查询串**而不是 `Query<HashMap>` —— axum 会先做
/// 一次百分号解码，而 `authCodeInfo` 里装的是 URL 编码过的 JSON
/// （`%7B%22authCode%22%3A%22…%22%7D`）：解两次会把里面的 `&`/`=` 变成字面量，
/// `Callback::from_query` 再按 `&` 切就切坏了。键名候选与顺序本来就是它的活。
async fn authorize(State(sender): State<mpsc::UnboundedSender<String>>, uri: OriginalUri) -> Response {
    let query = uri.0.query().unwrap_or_default().to_string();
    let callback = Callback::from_query(&query);
    if !callback.resolves_login() {
        return not_found().await;
    }
    // 投递失败只可能是监听器已丢（超时/取消先走），此时回什么都无所谓 ——
    // 但**不能** panic，也不能把它当成一次成功登录。
    if sender.send(query).is_err() {
        return callback_page(410, "Login expired", "这一轮登录已经结束，请回到面板重新发起。");
    }
    if callback.error.is_empty() {
        callback_page(200, "Login successful", "You can close this window now.")
    } else {
        callback_page(200, "Login failed", &callback.error)
    }
}

/// 非回调路径：不暴露任何东西（这个端口只为一次登录而存在）。
async fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "Not Found").into_response()
}

/// 给浏览器的那一页（与参考实现 `writeCallbackHTML` 同文案）。
///
/// 状态码一律 200：这一步之后还要换证，成败此刻还不知道；页面只说明
/// "授权信息收到了"。真正的失败由面板的 `/wait` 轮询显示。
fn callback_page(status: u16, title: &str, message: &str) -> Response {
    let escaped = message.replace('<', "&lt;").replace('>', "&gt;");
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>{title}</title>\
         <div style=\"font:16px/1.6 system-ui;padding:40px;text-align:center\">\
         <h2>{title}</h2><p>{escaped}</p></div>"
    );
    (
        StatusCode::from_u16(status).unwrap_or(StatusCode::OK),
        [("content-type", "text/html; charset=utf-8")],
        body,
    )
        .into_response()
}

/// 起一个只服务这一轮登录的 HTTP 任务，收到 shutdown 信号就退出。
fn spawn_server(listener: TcpListener, app: Router, mut shutdown: watch::Receiver<bool>) {
    crate::spawn_task(async move {
        let serve = axum::serve(listener, app).with_graceful_shutdown(async move {
            // Drop 早于任务启动时 `changed()` 不会返回 —— 先查一次当前值。
            if *shutdown.borrow() {
                return;
            }
            let _ = shutdown.changed().await;
        });
        if let Err(error) = serve.await {
            logging::log("[Login]", &format!("⚠️ Trae 回调监听器异常退出: {error}"));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// 用真 HTTP 打真监听器（不是只测那个 handler 函数）。
    ///
    /// 这一家的登录**只能这样验**：端口相关行为（谁能连上、什么算噪音、
    /// 谁负责收尾）都是运行时性质，纯函数测试一条也验不到。
    async fn hit(port: u16, path_and_query: &str) -> (u16, String) {
        let url = format!("http://127.0.0.1:{port}{path_and_query}");
        match reqwest::get(url).await {
            Ok(response) => {
                let status = response.status().as_u16();
                let body = response.text().await.unwrap_or_default();
                (status, body)
            }
            Err(error) => panic!("本机监听器应当可达：{error}"),
        }
    }

    /// 500 毫秒内没投递就算"没有"（有则说明噪音被判成了回调）。
    async fn nothing_arrives(listener: &CallbackListener) -> bool {
        tokio::time::timeout(Duration::from_millis(500), listener.next_callback())
            .await
            .is_err()
    }

    #[tokio::test]
    async fn browser_noise_does_not_end_the_login() {
        let listener = CallbackListener::bind().await.expect("本机端口应当能绑上");
        // 授权页那一下会先带一堆市场噪音（favicon、preconnect、只带 scope 的
        // 探针）。这些都是**真的 200 页面请求**，一旦"收到请求就算回调"，
        // 真回调就再也进不来了 —— 这是本家最容易写错的一条。
        let (status, _) = hit(listener.port(), "/favicon.ico").await;
        assert_eq!(404, status, "非回调路径要 404");
        let (status, _) = hit(listener.port(), "/authorize?isRedirect=true&scope=solo").await;
        assert_eq!(404, status, "认不出材料的 query 也要 404");
        let (status, _) = hit(listener.port(), "/authorize").await;
        assert_eq!(404, status, "空 query（无 `?`）同样不算回调");
        assert!(nothing_arrives(&listener).await, "噪音一条都不许投递");
    }

    #[tokio::test]
    async fn a_real_callback_is_delivered_once_and_answers_the_browser() {
        let listener = CallbackListener::bind().await.expect("本机端口应当能绑上");
        let (status, body) = hit(listener.port(), "/authorize?authCode=AC-1&loginHost=www.trae.cn").await;
        assert_eq!(200, status, "收到材料就要给浏览器一个收尾页");
        assert!(body.contains("Login successful"), "{body}");
        let query = tokio::time::timeout(Duration::from_secs(2), listener.next_callback())
            .await
            .expect("真回调应当投递")
            .expect("通道不该关闭");
        // 投递的是**原始查询串**：`Callback::from_query` 自己认键名与转义，
        // 这里若先解过一层码，`authCodeInfo` 那种"值里装 JSON"的键就会被切坏。
        assert_eq!("authCode=AC-1&loginHost=www.trae.cn", query);

        // 上游明说的失败也要收尾（否则用户停在空白页、这边还在等）。
        let (status, body) = hit(listener.port(), "/authorize?error=access_denied").await;
        assert_eq!(200, status);
        assert!(body.contains("Login failed"), "{body}");
    }

    #[tokio::test]
    async fn a_remote_callback_can_be_injected_without_opening_the_loopback_port() {
        let listener = CallbackListener::bind().await.expect("本机端口应当能绑上");
        listener
            .submit_callback("authCode=AC-REMOTE&loginHost=api.trae.com.cn")
            .expect("有效回调应能注入");
        let query = tokio::time::timeout(Duration::from_secs(2), listener.next_callback())
            .await
            .expect("注入回调应当唤醒等待方")
            .expect("通道不该关闭");
        assert_eq!("authCode=AC-REMOTE&loginHost=api.trae.com.cn", query);
        assert!(listener.submit_callback("scope=solo").is_err(), "噪音不能注入");
    }

    #[tokio::test]
    async fn the_json_auth_code_survives_one_layer_of_encoding() {
        let listener = CallbackListener::bind().await.expect("本机端口应当能绑上");
        // 生产实测形状：授权码在 `authCodeInfo` 里，值是一段 URL 编码的 JSON，
        // 键名还是大写的 `AuthCode`。整串原样投递，认出它由 `Callback` 负责。
        let path = "/authorize?isRedirect=true&scope=solo&authCodeInfo=%7B%22AuthCode%22%3A%22AC-JSON%22%7D&loginTraceID=t&host=https%3A%2F%2Fapi.trae.com.cn";
        let (status, _) = hit(listener.port(), path).await;
        assert_eq!(200, status, "authCodeInfo 要认得出是回调");
        let query = tokio::time::timeout(Duration::from_secs(2), listener.next_callback())
            .await
            .expect("应当投递")
            .expect("通道不该关闭");
        let callback = Callback::from_query(&query);
        assert_eq!("AC-JSON", callback.auth_code, "解码只能有一层：{query}");
        assert_eq!("https://api.trae.com.cn", callback.login_host);
    }

    #[tokio::test]
    async fn closing_the_listener_ends_the_waiter() {
        let listener = CallbackListener::bind().await.expect("本机端口应当能绑上");
        let port = listener.port();
        // 取消路径就是这一手：待办表移除记录后必须让等待方退出，
        // 不能让它干等到 5 分钟超时（端口也一并被占到那时候）。
        listener.close();
        let ended = tokio::time::timeout(Duration::from_secs(3), listener.next_callback())
            .await
            .map(|value| value.is_none())
            .expect("等待应当在超时前结束");
        assert!(ended, "close 之后 next_callback 必须回 None，等待方才认这一轮已结束");
        // 端口随监听任务退出释放：同一个端口再绑一次要成功（绑不上就说明还占着）。
        let again = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await;
        assert!(again.is_ok(), "close 后端口 {port} 应当已归还系统");
    }
}
