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

#[cfg(windows)]
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use tauri::{AppHandle, Manager};

// 端口归属查询与进程树结束改用 Win32 API 的原因见 Cargo.toml 里 windows-sys
// 依赖处的说明：外部命令（taskkill / powershell）的字符串与进程创建行为会被
// 杀软的流量拦截木马启发式命中。这里按需导入，非 Windows 平台不带这份依赖。
#[cfg(windows)]
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, INVALID_HANDLE_VALUE,
};
#[cfg(windows)]
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, MIB_TCP6TABLE_OWNER_PID, MIB_TCP6ROW_OWNER_PID, MIB_TCP_STATE_LISTEN,
    MIB_TCPROW_OWNER_PID, MIB_TCPTABLE_OWNER_PID, TCP_TABLE_CLASS, TCP_TABLE_OWNER_PID_LISTENER,
};
#[cfg(windows)]
use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6};
#[cfg(windows)]
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
#[cfg(windows)]
use windows_sys::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, TerminateProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_TERMINATE,
};

use crate::gateway::proxy_port;
use crate::port_conflict::{ConflictKind, Occupant, PortConflict, StartupFailure};
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
/// 失败返回 [`StartupFailure`]（调用方把它原样透给 UI 的 `backend:error` 事件
/// 与 `backend_status` 命令），不 panic —— release profile 是 panic=abort。
///
/// 为什么要带结构而不只是一句话：端口冲突有两种性质相反的原因（被别的进程占用
/// vs 落在系统保留段里），界面据此给的出路完全不同，而分类所需的 OS 错误码
/// 只有在 bind 失败的那一刻才拿得到。详见 `crate::port_conflict`。
pub async fn ensure_ready(app: &AppHandle) -> Result<(), StartupFailure> {
    let port = proxy_port();

    // 起服务：日志库与配置在这里初始化（bootstrap 内部完成）。
    // 失败一律原样返回：`bootstrap` 只在「本该执行的目录迁移没有执行」时返回
    // Err（防御性校验，见那边的注释）—— 继续下去会把新配置目录建出来，
    // 让迁移永远无法重试，所以这里必须中断而不是带着错误往下走。
    // 桌面形态永远只听本机回环：管理 API 不出 127.0.0.1（headless 形态
    // 由 agent2api-server 二进制按 AGENT2API_HOST 自行决定监听地址）。
    let state = server::ServerState::bootstrap(port, std::net::IpAddr::from([127, 0, 0, 1]))
        .map_err(StartupFailure::other)?;

    // 面板访问控制的库句柄注入（桌面不注册管理员 → 该体系不启用，行为不变；
    // 与容器共用数据目录且注册过时，桌面管理 API 需要 Key —— 壳自动携带）
    server::access::attach_db(state.db().cloned());

    // 端口已被占用：唯一合法的占用者是「本产品的旧版 node 网关」（升级场景），
    // 先尝试自动接管；接管不了（别的程序 / 用户手工起的服务）才走报错。
    // 这里不静默复用（见模块头部说明）—— 复用别人的端口等于把管理 API
    // 交给一个不受控的旧版本进程。
    if is_ready(port).await && !reclaim_port_from_legacy(port).await {
        // 端口上有服务应答但接管失败：属于「被占用」这一类，查出监听进程
        // 一并交给界面（能查出进程时用户就不必自己去翻 netstat）
        let occupant = occupant_of(port);
        return Err(StartupFailure::port(PortConflict::new(
            ConflictKind::Occupied,
            port,
            occupant,
            "端口上已有服务在响应",
        )));
    }
    let shutdown_tx = server::start(&state).map_err(|conflict| {
        // bind 失败：补上「谁占着」再交给界面（查进程是平台细节，见 backend 模块头）
        StartupFailure::port(conflict.with_occupant(occupant_of(port)))
    })?;

    // 句柄先入 state：即使后面的自检失败，退出路径也能正常发停机信号，
    // 不会留下一个「已经起来但没人管」的监听端口
    {
        let app_state = app.state::<AppState>();
        let mut guard = app_state
            .backend
            .lock()
            .map_err(|_| StartupFailure::other("服务器状态锁不可用"))?;
        guard.shutdown_tx = Some(shutdown_tx);
        guard.port = port;
    }

    // 本地自检：bind 成功不代表路由可用（例如路由构造期出错），
    // 因此仍然按旧版的「轮询 /health」风格确认端到端可用
    let deadline = std::time::Instant::now() + STARTUP_TIMEOUT;
    while std::time::Instant::now() < deadline {
        if is_ready(port).await {
            server::logging::log("[Server]", &format!("服务已就绪（127.0.0.1:{port}）"));
            // 起来了就清掉上一次的失败记录：那条记录描述的是「之前起不来」，
            // 留着会让界面在网关正常运行时仍显示「端口被占用」——
            // 用户刚结束完进程重启，看到这个只会以为没生效。
            if let Ok(mut guard) = app.state::<AppState>().backend.lock() {
                guard.failure = None;
            }
            // 升级迁移收尾：清掉旧版安装目录残留的 node.exe / server.cjs。
            // 放在就绪之后 —— 网关可用是本程序存在的意义，先保主链路再清磁盘。
            cleanup_legacy_resources();
            return Ok(());
        }
        tokio::time::sleep(HEALTH_INTERVAL).await;
    }
    Err(StartupFailure::other(format!(
        "服务启动超时（{port} 端口未就绪）：端口已绑定但健康检查未通过，请查看运行日志"
    )))
}

/// 查端口上的监听进程（PID、镜像名、路径，以及两个身份标记）。
///
/// 查不到返回 None —— 这可能是因为权限不足（别人的进程），也可能因为端口
/// 根本没被进程占着（系统保留段就是这种：`netstat` 里空无一物）。两种情况
/// 都不该编造一个「占用者」出来。
pub fn occupant_of(port: u16) -> Option<Occupant> {
    let (pid, path) = listener_process(port)?;
    let name = Path::new(&path)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| path.clone());
    let self_pid = std::process::id();
    Some(Occupant {
        pid,
        name,
        path: path.clone(),
        is_self: pid == self_pid,
        system: inside_system_dir(&path),
    })
}

/// 结束占用端口的进程（含其子树），并在宽限期内确认端口真的释放。
///
/// **只结束 `occupant` 里那个 PID**，不做「按端口号无差别清理」：端口在两次
/// 查询之间易主时，无差别清理会杀掉一个无辜的进程。结束前由调用方（界面）
/// 把进程名与路径显示给用户确认 —— 这个函数本身不做询问，它是个动作。
///
/// 返回结束后的端口状态：`Ok(())` 表示端口已可 bind（可以重试启动了）；
/// Err 是仍然存在的冲突（例如系统保留段 —— 那里根本没有进程可杀）。
pub fn end_occupant(port: u16, occupant: &Occupant) -> Result<(), PortConflict> {
    // 本进程自己：结束它等于让界面所在的程序退出，这是用户无法预期的行为，
    // 一律拒绝（界面在这种情况下也不该给出这个按钮，这里是第二道闸）。
    if occupant.is_self {
        return Err(PortConflict::new(
            ConflictKind::Other,
            port,
            None,
            "该进程就是本程序自己，不能结束",
        ));
    }
    if occupant.system {
        return Err(PortConflict::new(
            ConflictKind::Other,
            port,
            None,
            "该进程属于系统组件，不应由本程序结束",
        ));
    }

    if !kill_process_tree(occupant.pid) {
        return Err(PortConflict::new(
            ConflictKind::Other,
            port,
            None,
            &format!(
                "结束进程失败（PID {}，{}）：可能权限不足，请以管理员身份重试",
                occupant.pid, occupant.name
            ),
        ));
    }

    server::logging::log(
        "[Server]",
        &format!(
            "已结束占用端口的进程（PID {}，{}）",
            occupant.pid, occupant.name
        ),
    );
    Ok(())
}

/// 结束进程后等端口释放：强杀后监听句柄立即关闭，但给安全软件拦截、
/// 进程退出钩子这类抖动留一个窗口。
///
/// 返回 Ok(()) 表示端口已可 bind；Err 是仍存在的冲突（附当前占用者）。
pub fn wait_port_released(port: u16) -> Result<(), PortConflict> {
    const RELEASE_TIMEOUT: Duration = Duration::from_secs(5);
    let deadline = std::time::Instant::now() + RELEASE_TIMEOUT;
    loop {
        match server::probe_port(port) {
            Ok(()) => return Ok(()),
            Err(conflict) => {
                if std::time::Instant::now() >= deadline {
                    // 超时：把当时查到的占用者带上，好让界面能如实说明现状
                    return Err(conflict.with_occupant(occupant_of(port)));
                }
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
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
///   3. 结束进程树成功，且端口在宽限期内释放（旧服务的 /health 不再应答）。
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

/// 结束进程树（等价于 `taskkill /PID <pid> /T /F`）。
///
/// 用 Toolhelp 快照 + TerminateProcess 而不是 spawn taskkill：后者会在二进制里
/// 留下 `taskkill` 字符串与进程创建行为，与「监听本地端口」叠加正好命中杀软
/// 对流量拦截类木马的启发式（详见 Cargo.toml 中 windows-sys 依赖处的说明）。
#[cfg(windows)]
fn kill_process_tree(pid: u32) -> bool {
    // 先把整棵子树收集完再动手：一旦结束父进程，子进程会变成孤儿（父 PID 指向
    // 一个已消失的进程），快照里就再也还原不出这层父子关系了。
    // 结束顺序与 taskkill /T 一致：先子孙、最后目标本身。
    let mut descendants = descendant_pids(pid);
    descendants.reverse();
    for child in descendants {
        // 子进程结束失败不影响主进程的结果判定：旧实现的 taskkill /T 同样容忍
        // 「部分子进程已退出 / 权限不足打不开」这类情况。
        terminate_process(child);
    }
    terminate_process(pid)
}

#[cfg(not(windows))]
fn kill_process_tree(_pid: u32) -> bool {
    // 本项目只发布 Windows 版；桩实现让本文件保持跨平台可编译。
    false
}

/// 收集 `root` 的全部后代 PID（不含 `root` 自身）。
#[cfg(windows)]
fn descendant_pids(root: u32) -> Vec<u32> {
    // 一次快照读全表，比递归时反复枚举快照便宜；更重要的是避免「父进程已退出、
    // 关系断裂」导致漏杀。
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        // 拿不到快照只能退化成「只结束目标进程」——进程可能确实没有子进程，
        // 所以这里不能当作失败。
        return Vec::new();
    }

    // dwSize 必须先填结构体大小，否则 Process32FirstW 会直接失败。
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;

    let mut children_by_parent: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut has_entry = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    while has_entry {
        children_by_parent
            .entry(entry.th32ParentProcessID)
            .or_default()
            .push(entry.th32ProcessID);
        has_entry = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
    }
    unsafe { CloseHandle(snapshot) };

    // HashSet 既去重也防环：PID 复用期间可能出现自指的父子关系，防一手免得死循环。
    // 环里可能绕回 root 自身，必须排除 —— 否则 root 会被当成后代多杀一次，
    // 最后那次 terminate_process(root) 就会因为进程已消失而误判为失败。
    let mut seen: HashSet<u32> = HashSet::new();
    let mut pending = vec![root];
    let mut result = Vec::new();
    while let Some(current) = pending.pop() {
        let Some(children) = children_by_parent.get(&current) else {
            continue;
        };
        for &child in children {
            if child == root || child == 0 || !seen.insert(child) {
                continue;
            }
            result.push(child);
            pending.push(child);
        }
    }
    result
}

/// 结束单个进程；失败（打不开句柄 / 结束被拒）返回 false。
#[cfg(windows)]
fn terminate_process(pid: u32) -> bool {
    let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
    if handle.is_null() {
        return false;
    }
    let ok = unsafe { TerminateProcess(handle, 1) };
    unsafe { CloseHandle(handle) };
    ok != 0
}

/// 监听该端口的进程（PID + 可执行文件路径）。
///
/// 用 GetExtendedTcpTable 取代原来的 `powershell Get-NetTCPConnection`：既是
/// 杀软误报的根源（powershell 字符串 + 进程创建），也省掉一次解释器冷启动。
/// 表类型用 TCP_TABLE_OWNER_PID_LISTENER（只列监听态），比 ALL 少遍历大量
/// 已建立连接。IPv4 查不到再兜底查 IPv6 —— 本程序与旧版网关都是 IPv4，
/// IPv6 只覆盖「用户手工用 ::1 起了旧服务」这种边角情况。
#[cfg(windows)]
fn listener_process(port: u16) -> Option<(u32, String)> {
    let pid = listener_pid(AF_INET as u32, port).or_else(|| listener_pid(AF_INET6 as u32, port))?;
    let path = process_image_path(pid)?;
    Some((pid, path))
}

#[cfg(not(windows))]
fn listener_process(_port: u16) -> Option<(u32, String)> {
    // 本项目只发布 Windows 版；桩实现让本文件保持跨平台可编译。
    None
}

/// 在指定地址族的监听表里找出占用 `port` 的 PID。
#[cfg(windows)]
fn listener_pid(family: u32, port: u16) -> Option<u32> {
    let buffer = tcp_listener_table(family)?;

    if family == AF_INET as u32 {
        let table = buffer.as_ptr() as *const MIB_TCPTABLE_OWNER_PID;
        // 表声明里 table 只有 1 项，真实条目数由 dwNumEntries 给出，所以先取首行
        // 地址再按条目数切成切片遍历（IPv4 行是 6 个 u32，布局与声明完全一致）。
        let rows = unsafe {
            std::slice::from_raw_parts(
                std::ptr::addr_of!((*table).table).cast::<MIB_TCPROW_OWNER_PID>(),
                (*table).dwNumEntries as usize,
            )
        };
        return rows
            .iter()
            .find(|row| {
                row.dwState == MIB_TCP_STATE_LISTEN as u32 && tcp_port(row.dwLocalPort) == port
            })
            .map(|row| row.dwOwningPid);
    }

    let table = buffer.as_ptr() as *const MIB_TCP6TABLE_OWNER_PID;
    // 同 IPv4：IPv6 行按 MIB_TCP6ROW_OWNER_PID 布局解释，端口字段同样取低位 ntohs。
    let rows = unsafe {
        std::slice::from_raw_parts(
            std::ptr::addr_of!((*table).table).cast::<MIB_TCP6ROW_OWNER_PID>(),
            (*table).dwNumEntries as usize,
        )
    };
    rows.iter()
        .find(|row| row.dwState == MIB_TCP_STATE_LISTEN as u32 && tcp_port(row.dwLocalPort) == port)
        .map(|row| row.dwOwningPid)
}

/// 取监听表（`TCP_TABLE_OWNER_PID_LISTENER`）的原始字节。
///
/// 表是变长结构，只能两趟调用：第一趟传空指针探大小（返回
/// ERROR_INSUFFICIENT_BUFFER，同时把所需字节数写进 size），第二趟拿按大小分配好
/// 的缓冲区去填。缓冲区用 `Vec<u32>` 而不是 `Vec<u8>`：表的行只含 u32 字段
/// （对齐要求 4），用 u32 承载才能保证把裸缓冲区解释成表时指针对齐是成立的。
#[cfg(windows)]
fn tcp_listener_table(family: u32) -> Option<Vec<u32>> {
    let class: TCP_TABLE_CLASS = TCP_TABLE_OWNER_PID_LISTENER;
    let mut size: u32 = 0;
    // 探大小：null 指针必然失败，只看它回填的 size
    let probe = unsafe { GetExtendedTcpTable(std::ptr::null_mut(), &mut size, 0, family, class, 0) };
    if probe != ERROR_INSUFFICIENT_BUFFER || size == 0 {
        return None;
    }

    let mut buffer = vec![0u32; size as usize / 4 + 1];
    // 第二趟按缓冲区真实字节数报 size（比探测值多出补齐的零头），避免 API
    // 认为缓冲区比实际更大
    let mut capacity = std::mem::size_of_val(buffer.as_slice()) as u32;
    let filled =
        unsafe { GetExtendedTcpTable(buffer.as_mut_ptr().cast(), &mut capacity, 0, family, class, 0) };
    if filled != ERROR_SUCCESS {
        return None;
    }
    Some(buffer)
}

/// 结构体里的端口是「网络字节序值放在 u32 高位」的形态，取低 16 位再 ntohs。
#[cfg(windows)]
fn tcp_port(raw: u32) -> u16 {
    u16::from_be((raw & 0xFFFF) as u16)
}

/// 进程的可执行文件绝对路径（拿不到路径返回 None，不回退到任何外部命令）。
#[cfg(windows)]
fn process_image_path(pid: u32) -> Option<String> {
    // PROCESS_QUERY_LIMITED_INFORMATION：Vista 之后的最低查询权限，对「同用户但
    // 完整性级别不同」的进程（旧版网关就是这种）也能成功打开，足以拿路径。
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return None;
    }

    let mut buffer = [0u16; 1024];
    let mut length = buffer.len() as u32;
    let ok = unsafe { QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut length) };
    unsafe { CloseHandle(handle) };
    if ok == 0 {
        return None;
    }

    // 成功时 length 是不含结尾 NUL 的字符数。
    Some(String::from_utf16_lossy(&buffer[..length as usize]))
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

/// 进程的可执行文件是否位于本应用安装目录之内。
///
/// 两侧都 canonicalize 再比前缀：消除大小写差异、8.3 短路径与
/// `\\?\` verbatim 前缀的形态差（进程路径返回的是普通盘符路径，
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

/// 进程的可执行文件是否位于系统目录（`%SystemRoot%`）之内。
///
/// 用来拦下「结束进程」按钮：svchost 这类系统组件既可能承载别的服务，
/// 也不该由本程序去结束。拿不到系统目录时保守返回 true（宁可少杀一个，
/// 也不要误杀系统进程 —— 这条判据的误判代价是不对称的）。
fn inside_system_dir(candidate: &str) -> bool {
    let Ok(root) = std::env::var("SystemRoot") else {
        return true;
    };
    let Ok(root) = std::fs::canonicalize(root) else {
        return true;
    };
    let Ok(target) = std::fs::canonicalize(candidate) else {
        return false;
    };
    target.starts_with(root)
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
