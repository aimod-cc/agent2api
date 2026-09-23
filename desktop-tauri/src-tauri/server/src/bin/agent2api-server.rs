//! headless 服务器入口：Agent2API 网关的**无图形**运行形态。
//!
//! ── 与桌面端的关系 ──────────────────────────────────────────
//! 两者链接同一个网关 lib（`agent2api_server`），HTTP 契约、数据目录、
//! 迁移逻辑完全一致。差异只有三件：
//!   · 监听地址：本二进制按 `AGENT2API_HOST` 解析（默认 0.0.0.0，
//!     供容器端口映射）；桌面壳固定 127.0.0.1。
//!   · 界面：桌面由 Tauri 壳的 custom-protocol 出面板；本形态由网关
//!     自己托管 `ui/` 静态目录（`set_ui_dir`），并注入网页端 bridge
//!     （`web_shim`，UI 代码不感知差异）。
//!   · 安全闸门：本形态面向网络部署，两层的最低保护 ——
//!     管理面要「管理员账号或 API Key」（都没有则拒绝启动）；
//!     转发面（/v1/*）没配 Key 时 fail-closed（登录面板自建第一把即恢复；
//!     桌面默认只听回环，同一风险不存在；确要裸跑见 `AGENT2API_ALLOW_NO_KEY`）。
//!
//! ── 环境变量 ────────────────────────────────────────────────
//!   AGENT2API_PROXY_HOME        配置/数据目录（旧名 WORKBUDDY_PROXY_HOME 兼容读）
//!   AGENT2API_HOST              监听地址，默认 0.0.0.0
//!   AGENT2API_PROXY_PORT        监听端口（旧名 WORKBUDDY_PROXY_PORT），默认 3065
//!   AGENT2API_PANEL_PORT        可选：面板分端口 —— 设置后管理面（界面 + /api/*）
//!                               单独监听该端口，主端口只保留 /v1/* 网关；
//!                               公网部署只映射主端口，即可把面板留在内网
//!   AGENT2API_UI_DIR            管理界面静态目录，默认 `ui/`（相对可执行文件）
//!   AGENT2API_ADMIN_USER        面板管理员账号（与下面的密码变量之一同时设置）
//!   AGENT2API_ADMIN_PASSWORD    面板管理员密码（明文，启动时自动转 bcrypt 哈希）
//!   AGENT2API_ADMIN_PASSWORD_HASH 面板管理员密码的 bcrypt 哈希（优先于明文；
//!                               htpasswd -nBC 10 user 的输出整行可粘）
//!   AGENT2API_ALLOW_NO_KEY      置 `1` 关闭全部闸门（未配 Key 也放行，纯内网用）
//!   AGENT2API_CAPTCHA_ENABLED   登录页人机验证组件环境变量，默认为1开启，0为关闭
//!   AGENT2API_VERBOSE           置 `1` 打开 debug 级日志（与桌面一致）

use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::process::ExitCode;

use agent2api_server::server::{config, logging, start, ServerState};

fn main() -> ExitCode {
    // 端口口径与桌面壳一致：新名 > 旧名 > 默认（非法值当未设置）
    let port = env_port("AGENT2API_PROXY_PORT")
        .or_else(|| env_port("WORKBUDDY_PROXY_PORT"))
        .unwrap_or(3065);
    let host: IpAddr = match std::env::var("AGENT2API_HOST") {
        Ok(value) if !value.trim().is_empty() => match value.trim().parse() {
            Ok(host) => host,
            Err(_) => {
                eprintln!("❌ AGENT2API_HOST 不是合法的 IP 地址: {value}");
                return ExitCode::FAILURE;
            }
        },
        // headless 形态默认 0.0.0.0：它的宿主通常是容器 / 服务器，
        // 绑回环会让端口映射与外部访问全部失效。对外暴露的安全由
        // 「未配 Key 拒绝启动」闸门 + API Key 认证兜底。
        _ => IpAddr::from(Ipv4Addr::UNSPECIFIED),
    };
    let ui_dir = resolve_ui_dir();

    // 面板分端口（可选）：设了就把管理面（静态界面 + /api/*）挪到独立端口，
    // 主端口只留 /v1/* 网关 —— 想收敛暴露面时，公网只映射主端口即可。
    // 与主端口相同没有意义（等于没分），直接报错不让部署带着歧义上线。
    let panel_port = match std::env::var("AGENT2API_PANEL_PORT") {
        Ok(value) if !value.trim().is_empty() => match value.trim().parse::<u16>() {
            Ok(p) if p == port => {
                eprintln!("❌ AGENT2API_PANEL_PORT 与网关端口相同（{p}）：分端口部署要求两者不同");
                return ExitCode::FAILURE;
            }
            Ok(p) => Some(p),
            Err(_) => {
                eprintln!("❌ AGENT2API_PANEL_PORT 不是合法端口: {value}");
                return ExitCode::FAILURE;
            }
        },
        _ => None,
    };

    // bootstrap：开库、读配置、探测旧数据、装日志库（与桌面同一套时序）。
    // 失败只可能来自目录迁移防御性校验 —— 原样透出，不能带错继续。
    let mut state = match ServerState::bootstrap(port, host) {
        Ok(state) => state,
        Err(reason) => {
            eprintln!("❌ 网关初始化失败: {reason}");
            return ExitCode::FAILURE;
        }
    };

    state.panel_port = panel_port;
    // headless 面板闸门：未注册时 /api/* 只放行注册相关端点与 API Key（强制
    // 先注册，见 access::panel_gate 与 http::require_api_key 的注释）。
    // 唯一的关闭口在下面的安全闸门里：AGENT2API_ALLOW_NO_KEY=1 是「我自己
    // 要全放行」的显式声明，闸门不得拦它。
    agent2api_server::server::access::set_panel_gate(true);
    if let Some(p) = panel_port {
        logging::log(
            "[Server]",
            &format!(
                "面板分端口已启用：管理面（界面 + /api/*）监听 :{p}，主端口只保留 /v1/* 网关 —— 公网部署建议防火墙/compose 只映射主端口"
            ),
        );
    }

    // 面板访问控制的库句柄与刷新令牌载入（注册 / 双令牌落盘都走它）
    agent2api_server::server::access::attach_db(state.db().cloned());
    agent2api_server::server::access::load_refresh_tokens();
    // env 预置的管理员同步进库（明文在此前已转哈希，库里只存哈希）
    agent2api_server::server::access::sync_env_admin_to_store();

    // ── 安全闸门：/v1/* 的 fail-closed 与注册提示 ───────────────
    // 桌面形态的安全边界是「只监听 127.0.0.1」，免鉴权语义（一把 Key 都
    // 没配 → 全放行）在此之上成立；本形态绑 0.0.0.0、对外可见：
    //   · 转发面（/v1/*）：只认 API Key。没有任何 Key 时 fail-closed
    //     （拒绝服务并引导去面板建第一把），额度绝不对全网开放；
    //   · 管理面（/api/*）：管理员注册后需要登录。**未注册的窗口期**放行
    //     （否则连「创建管理员」都进不去）—— 这是刻意的取舍：抢注只可能
    //     发生在「部署完到用户注册」之间，所以启动日志会强烈提醒立即注册；
    //     注册一完成，管理面立即要求登录。介意这个窗口的，用环境变量
    //     预置管理员（AGENT2API_ADMIN_USER / …_PASSWORD_HASH，跳过注册）。
    // 纯内网确要无 Key 裸跑的，显式设 AGENT2API_ALLOW_NO_KEY=1 自己负责。
    if !config::current().active_api_keys().is_empty() {
        logging::log("[Security]", "API Key 认证已启用");
    } else if std::env::var("AGENT2API_ALLOW_NO_KEY")
        .map(|v| v.trim() == "1")
        .unwrap_or(false)
    {
        // 裸跑是「我自己要全放行」的显式声明：面板闸门必须让路，
        // 否则 /api/* 会被未注册闸门挡住，与声明自相矛盾
        agent2api_server::server::access::set_panel_gate(false);
        logging::log(
            "[Security]",
            "⚠️  AGENT2API_ALLOW_NO_KEY=1：未配置 API Key，网关对所有来源完全开放",
        );
    } else {
        agent2api_server::server::access::set_v1_fail_closed(true);
        if agent2api_server::server::access::panel_auth_enabled() {
            logging::log(
                "[Security]",
                "尚未配置 API Key：登录面板后在「网关 Key」页创建第一把，/v1/* 在此之前拒绝服务",
            );
        } else {
            logging::log(
                "[Security]",
                "⚠️  尚未注册管理员且未配置 API Key：请立即打开 http://<面板地址>/login 完成首次注册",
            );
            logging::log(
                "[Security]",
                "    /v1/* 已进入 fail-closed（拒绝服务）；管理接口在注册完成前不校验来源，注册后立即要求登录",
            );
        }
    }

    state.set_ui_dir(ui_dir);
    let shutdown_tx = match start(&state) {
        Ok(tx) => tx,
        Err(conflict) => {
            eprintln!("❌ 网关启动失败：{}", conflict.message);
            return ExitCode::FAILURE;
        }
    };

    logging::log(
        "[Server]",
        &format!(
            "headless 运行中：面板 http://{}:{}/ ｜ API http://{}:{}/v1",
            display_host(host),
            port,
            display_host(host),
            port
        ),
    );

    // 停机信号：Ctrl+C（全平台）+ SIGTERM（unix，容器 stop 走这条）。
    // 收到任一个就触发 graceful shutdown —— 在途请求跑完再退。
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("❌ 创建 Tokio 运行时失败: {error}");
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(wait_for_shutdown_signal());
    let _ = shutdown_tx.send(());
    // 给在途请求留出跑完的时间（start 内部会等 graceful shutdown 收尾）
    std::thread::sleep(std::time::Duration::from_millis(300));
    ExitCode::SUCCESS
}

/// 读端口环境变量：未设置 / 非数字 / 0 一律当未设置（与桌面 env_port 同口径）
fn env_port(name: &str) -> Option<u16> {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<u16>().ok())
        .filter(|port| *port > 0)
}

/// UI 目录：AGENT2API_UI_DIR 优先，默认可执行文件同级的 `ui/`。
///
/// 这里只判目录**可定位**（能拼出路径），不判存在 —— 目录缺失时面板 404，
/// API 照常工作（纯 API 部署可以不带 ui/），启动不该因此失败。
fn resolve_ui_dir() -> PathBuf {
    if let Some(dir) = std::env::var("AGENT2API_UI_DIR")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    {
        return PathBuf::from(dir);
    }
    let exe = std::env::current_exe().unwrap_or_default();
    exe.parent()
        .map(|parent| parent.to_path_buf())
        .unwrap_or_default()
        .join("ui")
}

/// 日志与提示里的地址：0.0.0.0 显示为 127.0.0.1（它表示「所有网卡」，
/// 用户实际访问用回环或机器 IP，直接打 0.0.0.0 在多数客户端里不通）
fn display_host(host: IpAddr) -> String {
    if host.is_unspecified() {
        "127.0.0.1".to_string()
    } else {
        host.to_string()
    }
}

/// 等待停机信号：Ctrl+C（全平台）或 SIGTERM（unix）
async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("注册 SIGTERM 处理失败");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
