//! 后端（进程内 HTTP 服务器）生命周期管理。
//!
//! ── 与旧版本的根本区别 ──────────────────────────────────────
//! 旧实现是「探测 3065，没在跑就拉起随包分发的 node.exe + server.cjs」：
//! 它能复用用户手工 `npm start` 起的服务，也在退出时杀自己拉起的子进程。
//!
//! 现在网关就是本进程内的 HTTP 服务器（见 `crate::server`），因此：
//!   1. **必须自己 bind 成功**。「探测到端口已被占用就复用」的逻辑取消了 ——
//!      功能都在本进程里，复用别人的端口等于把自己的管理 API 交给一个
//!      不受控的进程（版本可能不匹配，行为也无从保证）。端口被占用时直接
//!      报错并提示用户结束旧网关，比「看起来启动成功、实际连的是旧服务」安全得多。
//!   2. **退出时不再杀进程**，只发一个停机信号让 axum 优雅收尾
//!      （不再有「杀不掉 node 子进程」导致覆盖安装失败的问题 ——
//!      这正是本次迁移要解决的问题之一）。
//!
//! 对外契约保持不变：`ensure_ready(app)` 与 `shutdown(&AppState)` 两个函数
//! 的签名与语义（成功/失败返回、幂等）与旧版一致，lib.rs / tray.rs /
//! commands.rs 的调用点一行都不用改。
//!
//! ── 升级迁移（1.0.x → 1.1.0）───────────────────────────────
//! 已装旧版的用户覆盖安装后，有两个遗留问题要在**新版首次启动**时自动处理：
//!   1. 旧版拉起的 node 子进程变孤儿继续监听 3065 → `reclaim_port_from_legacy`
//!      在确认是「自家旧版网关」后自动结束它（判定链见函数注释）；
//!   2. 旧版安装目录里残留的 resources\node.exe（88MB）+ server.cjs →
//!      `cleanup_legacy_resources` 在服务就绪后清掉。
//! 两个都只在升级后的第一次启动有实际动作，正常启动零开销。

use std::path::Path;
use std::time::Duration;

use tauri::{AppHandle, Manager};

use crate::gateway::proxy_port;
use crate::server;
use crate::state::AppState;

/// 启动等待上限。
///
/// 旧实现给 node 冷启动留了 12 秒；进程内服务器没有进程启动与模块加载开销，
/// bind + 首次响应是毫秒级的，5 秒足够覆盖「系统繁忙/杀软扫描」这类抖动。
const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const HEALTH_TIMEOUT: Duration = Duration::from_millis(1500);
const HEALTH_INTERVAL: Duration = Duration::from_millis(100);

/// 探测本机网关是否已就绪（只看能否拿到 /health 响应）。
///
/// 同时被 `commands::backend_status` 使用（前端启动阶段显示「服务已就绪」提示），
/// 因此保持为公开函数、语义不变：能拿到 2xx 就算就绪。
pub async fn is_ready(port: u16) -> bool {
    let Ok(client) = reqwest::Client::builder().timeout(HEALTH_TIMEOUT).build() else {
        return false;
    };
    matches!(
        client.get(format!("http://127.0.0.1:{port}/health")).send().await,
        Ok(response) if response.status().is_success()
    )
}

/// 确保后端可用：构造服务状态 → 绑定端口 → 异步 accept 循环 → 本地自检。
///
/// 失败一律返回可读的中文说明（调用方把错误原样透给 UI 的
/// `backend:error` 事件），不 panic —— release profile 是 panic=abort。
pub async fn ensure_ready(app: &AppHandle) -> Result<(), String> {
    let port = proxy_port();

    // 起服务：日志库与配置在这里初始化（bootstrap 内部完成）
    let state = server::ServerState::bootstrap(port);

    // 端口已被占用：唯一合法的占用者是「本产品的旧版 node 网关」（升级场景），
    // 先尝试自动接管；接管不了（别的程序 / 用户手工起的服务）才走报错。
    // 这里不静默复用（见模块头部说明）—— 复用别人的端口等于把管理 API
    // 交给一个不受控的旧版本进程。
    if is_ready(port).await && !reclaim_port_from_legacy(port).await {
        return Err(format!(
            "{port} 端口上已有服务在响应：可能是旧版网关（node server.mjs）尚未退出。\
             请先结束它再启动本程序，或用环境变量 WORKBUDDY_PROXY_PORT 指定其它端口。"
        ));
    }
    let shutdown_tx = server::start(&state)?;

    // 句柄先入 state：即使后面的自检失败，退出路径也能正常发停机信号，
    // 不会留下一个「已经起来但没人管」的监听端口
    {
        let app_state = app.state::<AppState>();
        let mut guard = app_state
            .backend
            .lock()
            .map_err(|_| "服务器状态锁不可用".to_string())?;
        guard.shutdown_tx = Some(shutdown_tx);
        guard.port = port;
    }

    // 本地自检：bind 成功不代表路由可用（例如路由构造期出错），
    // 因此仍然按旧版的「轮询 /health」风格确认端到端可用
    let deadline = std::time::Instant::now() + STARTUP_TIMEOUT;
    while std::time::Instant::now() < deadline {
        if is_ready(port).await {
            server::logging::log("[Server]", &format!("服务已就绪（127.0.0.1:{port}）"));
            // 升级迁移收尾：清掉旧版安装目录残留的 node.exe / server.cjs。
            // 放在就绪之后 —— 网关可用是本程序存在的意义，先保主链路再清磁盘。
            cleanup_legacy_resources();
            return Ok(());
        }
        tokio::time::sleep(HEALTH_INTERVAL).await;
    }
    Err(format!(
        "服务启动超时（{port} 端口未就绪）：端口已绑定但健康检查未通过，请查看运行日志"
    ))
}

/// 升级迁移：把端口从「本产品的旧版 node 网关」手里收回来。
///
/// 旧版（1.0.x）被 NSIS 覆盖安装结束时，它拉起的 node 子进程会变成孤儿
/// 继续监听 3065 —— 新版进程内服务器 bind 失败，用户只能手动结束进程。
/// 这是历史上「升级时杀不掉 Node 程序」问题的残留路径，这里自动兜底。
///
/// 判定链**每一环都必须满足**才会动手，任何一环不成立都返回 false
/// （让调用方走端口占用的报错提示）：
///   1. 端口上的服务应答 /health 且 `product == "WorkBuddy"` —— 自家网关的
///      特征字段；别的程序恰好占了 3065 时绝不乱杀；
///   2. 监听该端口的进程，其可执行文件位于**本应用安装目录**之内
///      （即旧版随包分发的 resources\node.exe）。用户自己 `npm start`
///      起的系统 node 在安装目录之外 —— 只提示、不杀；
///   3. taskkill 成功，且端口在宽限期内释放（旧服务的 /health 不再应答）。
async fn reclaim_port_from_legacy(port: u16) -> bool {
    if !health_reports_workbuddy(port).await {
        return false;
    }
    let Some((pid, exe_path)) = listener_process(port) else {
        server::logging::log(
            "[Server]",
            &format!("端口 {port} 上的服务疑似旧版网关，但查不到监听进程，需手动处理"),
        );
        return false;
    };
    if !inside_install_dir(&exe_path) {
        server::logging::log(
            "[Server]",
            &format!(
                "端口 {port} 被 WorkBuddy 网关占用，但进程不在本应用安装目录（{exe_path}），\
                 不自动结束 —— 可能是你自己用 npm start 起的服务"
            ),
        );
        return false;
    }

    server::logging::log(
        "[Server]",
        &format!("检测到旧版网关仍在运行（PID {pid}），正在自动结束（升级迁移）…"),
    );
    let killed = kill_process_tree(pid);
    if !killed {
        server::logging::log("[Server]", "结束旧版网关进程失败，需手动处理");
        return false;
    }

    // 等端口真正释放再放行：强杀后监听句柄立即关闭，但给安全软件拦截、
    // 进程退出钩子这类抖动留 5 秒窗口
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if !is_ready(port).await {
            server::logging::log("[Server]", "已自动结束旧版网关进程（升级迁移）");
            return true;
        }
        if std::time::Instant::now() >= deadline {
            server::logging::log("[Server]", "旧版网关进程已结束但端口迟迟未释放");
            return false;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// 结束进程树（taskkill /T /F）。
///
/// **必须加 CREATE_NO_WINDOW**：这是个控制台程序，不加标志时 Windows 会为它
/// 分配一个控制台窗口 —— 用户在升级后的首次启动会看到黑窗一闪。同理，
/// 下面查监听进程的 PowerShell 也要加（见 `creation_flags_no_window`）。
fn kill_process_tree(pid: u32) -> bool {
    std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags_no_window()
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// 给当前进程 spawn 出的控制台程序加 `CREATE_NO_WINDOW`。
///
/// 只在 Windows 生效；非 Windows 平台返回 `&mut Command` 本身，调用点无需
/// 平台分支。理由：GUI 应用（本程序无控制台）拉起 powershell/taskkill 这类
/// 控制台程序时，若不加标志 Windows 会新建一个控制台窗口并显示出来。
#[cfg(windows)]
trait NoWindowExt {
    fn creation_flags_no_window(&mut self) -> &mut Self;
}

#[cfg(windows)]
impl NoWindowExt for std::process::Command {
    fn creation_flags_no_window(&mut self) -> &mut Self {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        self.creation_flags(CREATE_NO_WINDOW)
    }
}

#[cfg(not(windows))]
trait NoWindowExt {
    fn creation_flags_no_window(&mut self) -> &mut Self;
}

#[cfg(not(windows))]
impl NoWindowExt for std::process::Command {
    fn creation_flags_no_window(&mut self) -> &mut Self {
        self
    }
}

/// 端口上的服务是否是 WorkBuddy 网关（读 /health 的 product 特征字段）。
async fn health_reports_workbuddy(port: u16) -> bool {
    let Ok(client) = reqwest::Client::builder().timeout(HEALTH_TIMEOUT).build() else {
        return false;
    };
    let Ok(response) = client.get(format!("http://127.0.0.1:{port}/health")).send().await
    else {
        return false;
    };
    let Ok(payload) = response.json::<serde_json::Value>().await else {
        return false;
    };
    payload.get("product").and_then(|value| value.as_str()) == Some("WorkBuddy")
}

/// 找到监听该端口的进程（PID + 可执行文件路径）。
///
/// 用 PowerShell 的 Get-NetTCPConnection：netstat 的输出在中文系统上
/// State 列是 GBK 编码的「侦听」，按文本解析容易碎。`[Console]::OutputEncoding`
/// 必须显式设为 UTF-8 —— 安装目录含中文（产品名就是中文），默认控制台
/// 编码会把路径弄坏。IPv4/IPv6 可能各有一条监听记录，取第一个能解析的。
fn listener_process(port: u16) -> Option<(u32, String)> {
    let script = format!(
        "[Console]::OutputEncoding=[Text.Encoding]::UTF8; \
         Get-NetTCPConnection -LocalPort {port} -State Listen | ForEach-Object {{ \
           $p = Get-Process -Id $_.OwningProcess -ErrorAction SilentlyContinue; \
           if ($p -and $p.Path) {{ Write-Output ($_.OwningProcess.ToString() + '|' + $p.Path) }} \
         }}"
    );
    let output = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .creation_flags_no_window()
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .find_map(|line| {
            let (pid, path) = line.split_once('|')?;
            let pid: u32 = pid.trim().parse().ok()?;
            Some((pid, path.trim().to_string()))
        })
}

/// 进程的可执行文件是否位于本应用安装目录之内。
///
/// 两侧都 canonicalize 再比前缀：消除大小写差异、8.3 短路径与
/// `\\?\` verbatim 前缀的形态差（PowerShell 返回的是普通路径，
/// canonicalize 后两侧形态一致，starts_with 才可靠）。
fn inside_install_dir(candidate: &str) -> bool {
    let Ok(current) = std::env::current_exe() else {
        return false;
    };
    let Ok(current) = current.canonicalize() else {
        return false;
    };
    let Some(install_dir) = current.parent() else {
        return false;
    };
    let Ok(target) = std::fs::canonicalize(candidate) else {
        // 进程可能恰好在这两条命令之间退出了 —— 文件没了就当不匹配
        return false;
    };
    target.starts_with(install_dir)
}

/// 清理旧版安装目录里残留的 Node 运行时。
///
/// NSIS 覆盖安装只覆盖同名文件，旧版多出来的 resources\node.exe（约 88MB）
/// 与 resources\server.cjs 会一直躺在安装目录占磁盘。这里在服务就绪后清一次；
/// 删除失败（被占用/权限不足）只记日志，下次启动再试。
///
/// 以当前可执行文件的父目录推导安装目录（而不是 Tauri 的 resource_dir，
/// 后者可能带 `\\?\` verbatim 前缀，与磁盘上的真实路径形态不一致）。
/// 开发态（target/debug 下）不存在 resources 子目录，天然跳过。
fn cleanup_legacy_resources() {
    let Some(install_dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
    else {
        return;
    };
    let resources = install_dir.join("resources");
    for name in ["node.exe", "server.cjs"] {
        let target = resources.join(name);
        if !target.exists() {
            continue;
        }
        match std::fs::remove_file(&target) {
            Ok(()) => {
                server::logging::log("[Server]", &format!("已清理旧版残留的资源文件（升级迁移）: {name}"))
            }
            Err(error) => {
                server::logging::log(
                    "[Server]",
                    &format!("清理旧版残留 {name} 失败（下次启动再试）: {error}"),
                )
            }
        }
    }
}

/// 退出时停掉进程内服务器：发送一次停机信号，让 axum 优雅收尾。
///
/// 签名与旧版一致（同步函数、接收 `&AppState`），因此 lib.rs 的三处调用点与
/// commands.rs / tray.rs 都无需改动。**幂等**：重复调用时发送端已被 take，
/// 直接返回 —— 这一点与旧版 `child.take()` 的行为一致。
pub fn shutdown(state: &AppState) {
    // 定时签到调度：在停服务之前先停它（对照 Node 版 closeAll 里的
    // `autoCheckin.stop(); closeDispatchers();`）。
    //
    // **放在幂等判断之前**：调度循环是独立于 HTTP 服务的后台任务
    // （由 auto_checkin 句柄持有），只要它在跑就该被停掉；若写在下面的
    // `shutdown_tx.take()` 之后，第二次调用（服务器已停 → 发送端已被 take）
    // 会整段跳过，留下一个仍在轮询的循环。stop 自身也是幂等的（task 取走后
    // 再调直接返回），所以不破坏本函数「重复调用无副作用」的既有语义。
    server::core::auto_checkin::stop_global();

    let Ok(mut guard) = state.backend.lock() else {
        return;
    };
    if let Some(sender) = guard.shutdown_tx.take() {
        // 接收端可能已经随运行时一起退出（发送失败）—— 那说明服务早就停了，无需处理
        let sent = sender.send(()).is_ok();
        server::logging::log(
            "[Server]",
            if sent {
                "已发送停机信号，服务正在退出"
            } else {
                "服务已停止（停机信号无需发送）"
            },
        );
        // 从日志确认：Node 版退出时也是打印一行说明，这里保持一致的可观测性。
        // 不做「等待确认」——shutdown 是同步函数且可能从程序退出路径调用，
        // 等异步收尾只会拖慢退出。
        server::logging::console_line("[Server]", "退出中…");
    }
}

/// 已启动服务的端口；未启动时返回 None（供后续切片查询状态时使用）。
#[allow(dead_code)]
pub fn running_port(state: &AppState) -> Option<u16> {
    state
        .backend
        .lock()
        .ok()
        .and_then(|guard| guard.shutdown_tx.is_some().then_some(guard.port))
}
