//! AutoClaw OAuth 回调的**登记端口**监听器 —— 「回调必须落在哪」这件事的
//! 唯一实现点（`core::login::autoclaw` 起登录时借它拿端口）。
//!
//! ── 为什么回调不能挂在网关自己的端口上（本次修正的根因）──────
//! 网关原先把手里的 `navigate_uri` 指向自己的监听端口（默认 3065）。这条
//! 链路在 Google 上一直能用，**Zai 上却必然失败**：登录完成后 Zai 的 OAuth
//! 服务会拿 `redirect_uri` 去比对**这个 client 登记过的白名单**，而白名单里
//! 只有官方客户端那四个 loopback 端口：
//!
//! ```text
//!   ALL_PORTS = [18432, 19654, 19723, 53699]        ← app.asar 里逐字读出
//!   getZaiCallbackUri() = http://localhost:<primaryPort>/auth/callback-zai
//! ```
//!
//! 不在名单里的 `redirect_uri`（`http://localhost:3065/...`、`http://127.0.0.1:…`、
//! 路径不对的形态……）一律被拒，窗口里直接渲染出
//! `{"detail":"Redirect URI not registered for this client"}`（issue #11 的现象）。
//! **Google 是另一套规则**（RFC 8252 的 loopback 宽松匹配，端口任意）—— 这就是
//! 「同一份代码，谷歌能登、Zai 登不上」的全部原因，别再往 host 或路径上找。
//!
//! ── 因此这里做的事 ───────────────────────────────────────────
//! 登录开始时**临时**占用四个登记端口里的第一个空闲端口，用它的端口号拼
//! `navigate_uri`（见 [`super::oauth::navigate_uri`]），并把落到这个端口上的
//! 回调 **302 转发**到网关自己的回调路由（`forward_base` 就是网关的 loopback
//! 基址，由调用方给出）—— 换码、落账号、成功页那套逻辑只有网关那一份，
//! 本模块只负责「把浏览器的请求引回网关」，不认识 code / state。
//!
//! 端口在**登录结束**时释放：监听器随 `PendingOauth` 一起被移除（回调、
//! 超时、取消三条收尾路径都会移除那条记录），见
//! `core::login::autoclaw` 的 `PendingOauth::listener`。因此它不会长期占着
//! 官户客户端的端口，也不会在用户没登录时影响别的软件。
//!
//! ── 四个端口都被占（官方客户端在跑）时怎么办 ─────────────────
//! `bind` 返回 `Err(人话文案)`，由调用方决定：桌面形态下**仍然**把
//! `navigate_uri` 指向第一个登记端口（白名单那关必须过），并让壳侧的内嵌窗口
//! 把这次导航截回网关（见 `src/login.rs` 的 `autoclaw_callback_forward`）——
//! 那条路上端口由谁监听都不影响我们拿到 code。系统浏览器方式没有窗口可截，
//! 只能靠这个监听器，因此调用方会顺手把一句提示带回界面（提示用户先退出
//! AutoClaw 桌面客户端）。
//!
//! ── 绑定口径 ────────────────────────────────────────────────
//! 只绑 **loopback**（IPv4 `127.0.0.1` + 尽力而为的 IPv6 `[::1]`）：
//!   - 与官方客户端同款（它 `app.listen(port, "127.0.0.1")`），不给局域网
//!     开口子；回调里带着一次性授权码，虽然换不到凭证也仍然按本机回环对待；
//!   - IPv6 那一半是给「浏览器把 localhost 解析成 ::1」的环境兜底（官方
//!     客户端只绑了 IPv4，macOS 上浏览器先试 ::1 时那台机器就会连不上）。
//!
//! 只接受两个已知的回调路径（`/auth/callback-zai` / `/auth/callback-google`），
//! 其它路径 404 —— 这个端口是**借来的**，尽量少应答东西。

use std::net::{Ipv4Addr, Ipv6Addr};

use axum::extract::{OriginalUri, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::Router;
use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::server::logging;

/// z.ai 给 AutoClaw 客户端登记的四个回调端口（顺序 = 尝试顺序 = 客户端
/// `ALL_PORTS` 的顺序，第一个通常就是 18432）。
///
/// 改这个列表前先确认两件事：官方客户端 `app.asar` 里的 `ALL_PORTS` 变了、
/// 且 Zai 的 redirect_uri 白名单跟着变了 —— 少一个都会让登录在 authorize
/// 那一跳被拒（现象就是那个 `Redirect URI not registered`）。
pub const REGISTERED_CALLBACK_PORTS: [u16; 4] = [18432, 19654, 19723, 53699];

/// 两个已知的回调路径（与 `oauth::CALLBACK_PATH_PREFIX` 同源，这里只做白名单
/// 匹配 —— 路径之外的东西一概 404）。
const CALLBACK_PATHS: [&str; 2] = ["/auth/callback-zai", "/auth/callback-google"];

/// 一次登录期间占用的回调端口。
///
/// **持有它 = 这个端口这一轮归我们**：`Drop` 时通知监听任务退出（端口随之
/// 释放）。因此它只需要被存进 `PendingOauth` 即可，不需要任何显式的
/// `close()` 调用 —— 那条记录被移除的三个时机（回调换码、5 分钟超时、用户
/// 取消）就是端口释放的三个时机。
pub struct CallbackListener {
    port: u16,
    shutdown: Option<watch::Sender<bool>>,
}

impl CallbackListener {
    /// 占一个登记端口：返回的监听器活多久，端口就归我们多久。
    ///
    /// `forward_base` 是**网关自己的** loopback 基址（`http://localhost:<网关端口>`）——
    /// 落到这个端口上的回调会被 302 转发到 `{forward_base}{原路径}{原查询串}`。
    /// 由调用方给出（本模块拿不到监听端口，它在 `ServerState` 上）。
    ///
    /// 四个端口都占不到时返回 `Err`：文案是**事实陈述**（「都被占用了」），
    /// 具体该怎么取舍由调用方决定 —— 桌面形态下仍然能靠壳侧改道完成登录
    /// （见 `src/login.rs` 的 `autoclaw_callback_forward`），因此这里不写
    /// 「请退出某软件」这类指令（那会把一条仍可用的路说成走不通）。
    pub async fn bind(forward_base: &str) -> Result<Self, String> {
        let base = forward_base.trim_end_matches('/').to_string();
        for port in REGISTERED_CALLBACK_PORTS {
            let Ok(v4) = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await else {
                continue;
            };
            // IPv6 那一半是尽力而为：拿不到也不影响（浏览器多数时候先连 IPv4），
            // 拿得到就多一条能落地的路。失败不记日志（官方客户端只绑 IPv4 时
            // 这里是常态，不是问题）。
            let v6 = TcpListener::bind((Ipv6Addr::LOCALHOST, port)).await.ok();
            let (shutdown, receiver) = watch::channel(false);
            let app = router(&base);
            spawn_forwarder(v4, app.clone(), receiver.clone());
            if let Some(v6) = v6 {
                spawn_forwarder(v6, app, receiver);
            }
            logging::log(
                "[Login]",
                &format!("AutoClaw OAuth 回调已占用登记端口 {port}（转发到网关 {base}）"),
            );
            return Ok(Self {
                port,
                shutdown: Some(shutdown),
            });
        }
        Err(format!(
            "AutoClaw 登录所需的本地回调端口（{}）都被占用（通常是 AutoClaw 桌面客户端在运行）",
            REGISTERED_CALLBACK_PORTS
                .iter()
                .map(u16::to_string)
                .collect::<Vec<_>>()
                .join(" / ")
        ))
    }

    /// 这一轮占到的端口（拼 `navigate_uri` 用）。
    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for CallbackListener {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            // 通知监听任务退出；任务自己收尾（这里不 join —— Drop 可能在
            // 任意线程上，等它没有意义，端口随任务结束释放）
            let _ = shutdown.send(true);
        }
    }
}

/// 回调转发路由：两个已知路径 → 302 到网关；其余 → 404。
fn router(forward_base: &str) -> Router {
    let state = forward_base.to_string();
    let mut app = Router::new();
    for path in CALLBACK_PATHS {
        app = app.route(path, get(forward));
    }
    app.fallback(not_found).with_state(state)
}

/// 把一次回调原样转给网关（路径与查询串逐字保留）。
///
/// ── 为什么只做转发、不在这里换码 ─────────────────────────────
/// 换码要读待办表、要落账号、要回成功页 —— 那些逻辑在网关的回调路由里只有
/// 一份（`api::session::login_autoclaw_oauth_callback`）。在这里再实现一份
/// 等于把「一次登录只能被处理一次」的幂等约束复制到第二个地方，迟早会分叉。
/// 转发目标是我们自己写死的网关基址（不是从请求里读的），因此不构成开放
/// 重定向；查询串里只有上游的一次性 code 与 state，原样带给网关由它校验。
async fn forward(State(forward_base): State<String>, uri: OriginalUri) -> Response {
    let path_and_query = uri
        .0
        .path_and_query()
        .map(|value| value.as_str())
        .unwrap_or("/");
    Redirect::to(&format!("{forward_base}{path_and_query}")).into_response()
}

/// 非回调路径：不暴露任何东西（这个端口是借来的）。
async fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "Not Found").into_response()
}

/// 起一个只服务这一轮登录的 HTTP 任务，收到 shutdown 信号就退出。
fn spawn_forwarder(listener: TcpListener, app: Router, mut shutdown: watch::Receiver<bool>) {
    crate::spawn_task(async move {
        let serve = axum::serve(listener, app).with_graceful_shutdown(async move {
            // 已经置位过（Drop 早于任务启动）时 `changed()` 不会返回 —— 先查一次
            if *shutdown.borrow() {
                return;
            }
            let _ = shutdown.changed().await;
        });
        if let Err(error) = serve.await {
            logging::log(
                "[Login]",
                &format!("⚠️ AutoClaw 回调监听器异常退出: {error}"),
            );
        }
    });
}
