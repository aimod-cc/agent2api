//! 后端进程生命周期管理。
//!
//! 桌面端启动时确保本机网关在跑：先探测 3065 端口，已在跑就直接复用
//! （用户可能自己用 npm start 起的，不能重复拉起、更不能在退出时杀掉）；
//! 没在跑才由桌面端拉起随包分发的 node.exe + server.cjs。
//!
//! 这样做的好处是用户装完就能用，不需要另开终端跑后端；同时保留了
//! 「后端独立运行」的能力，方便排障时单独查看后端日志。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use tauri::{AppHandle, Manager};

use crate::gateway::proxy_port;
use crate::state::AppState;

/// 启动等待上限：node 冷启动 + 读取配置通常 1 秒内，给足 12 秒容错
const STARTUP_TIMEOUT: Duration = Duration::from_secs(12);
const HEALTH_TIMEOUT: Duration = Duration::from_millis(1500);

/// 探测本机网关是否已就绪（只看能否拿到 /health 响应）
pub async fn is_ready(port: u16) -> bool {
    let Ok(client) = reqwest::Client::builder().timeout(HEALTH_TIMEOUT).build() else {
        return false;
    };
    matches!(
        client.get(format!("http://127.0.0.1:{port}/health")).send().await,
        Ok(response) if response.status().is_success()
    )
}

/// 定位随包分发的后端脚本与 Node 运行时。
///
/// 安装后资源在 resource_dir 下；开发态（tauri dev）资源不一定被复制，
/// 因此再回落到源码目录，保证 `npm run tauri:dev` 也能直接跑起来。
fn resource_candidates(app: &AppHandle, file: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(dir) = app.path().resource_dir() {
        // 打包后：资源可平铺在根，也可能保留 resources/ 子目录
        paths.push(dir.join(file));
        paths.push(dir.join("resources").join(file));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            paths.push(dir.join(file));
            paths.push(dir.join("resources").join(file));
        }
    }
    // 开发态：源码里的 resources 目录（编译期路径）
    let dev_res = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("resources");
    paths.push(dev_res.join(file));
    paths
}

/// 去掉 Windows 的 verbatim 路径前缀。
///
/// Tauri 的 `resource_dir()` 返回的是规范化后的 `\\?\D:\...` 形式，
/// 而 Node 的模块解析拿到带前缀的脚本路径会直接崩：
///   Error: EISDIR: illegal operation on a directory, lstat 'D:'
/// 该前缀对普通路径操作无害，但对 Node 有害，因此在传给 node 之前统一剥掉。
/// UNC 形式 `\\?\UNC\server\share` 要还原成 `\\server\share`。
fn strip_verbatim(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    path
}

fn find_resource(app: &AppHandle, file: &str) -> Option<PathBuf> {
    resource_candidates(app, file)
        .into_iter()
        .find(|path| path.is_file())
        .map(strip_verbatim)
}

/// Node 运行时：优先用随包的 node.exe（用户机器无需装 Node），
/// 找不到时回落到 PATH 上的 node（开发态常用）。
fn resolve_node(app: &AppHandle) -> Result<PathBuf, String> {
    let name = if cfg!(windows) { "node.exe" } else { "node" };
    if let Some(path) = find_resource(app, name) {
        return Ok(path);
    }
    // 开发态回落到本机 Node
    which_node().ok_or_else(|| {
        "未找到 Node 运行时（随包 node.exe 缺失，且系统 PATH 中也没有 node）".to_string()
    })
}

fn which_node() -> Option<PathBuf> {
    let name = if cfg!(windows) { "node.exe" } else { "node" };
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// 确保后端可用：已在跑则复用，否则拉起并等待就绪。
///
/// 返回值区分两种来源，仅用于日志；退出时是否回收由 state 里有没有 child 决定。
pub async fn ensure_ready(app: &AppHandle) -> Result<(), String> {
    let port = proxy_port();
    if is_ready(port).await {
        eprintln!("[backend] 复用已在运行的服务（127.0.0.1:{port}）");
        return Ok(());
    }

    let server = find_resource(app, "server.cjs").ok_or_else(|| {
        "未找到后端脚本 server.cjs，请先执行 npm run build:backend".to_string()
    })?;
    let node = resolve_node(app)?;
    eprintln!("[backend] 启动 {} {}", node.display(), server.display());

    let mut command = Command::new(&node);
    command
        .arg(&server)
        .arg("--port")
        .arg(port.to_string())
        // bundle 后 import.meta.url 不可用，入口判定失效，
        // 用该变量显式要求 server 走 main() 启动服务
        .env("WORKBUDDY_PROXY_STANDALONE", "1")
        .current_dir(server.parent().unwrap_or(Path::new(".")))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    // 后端是控制台程序，不加这个标志会在 Windows 上弹出一个黑窗口
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    let child = command
        .spawn()
        .map_err(|error| format!("启动后端失败: {error}"))?;

    {
        let state = app.state::<AppState>();
        let mut guard = state
            .backend
            .lock()
            .map_err(|_| "后端状态锁不可用".to_string())?;
        guard.child = Some(child);
        guard.port = port;
    }

    let deadline = std::time::Instant::now() + STARTUP_TIMEOUT;
    while std::time::Instant::now() < deadline {
        if is_ready(port).await {
            eprintln!("[backend] 服务已就绪（127.0.0.1:{port}）");
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Err(format!(
        "后端启动超时（{port} 端口未就绪），请检查是否有安全软件拦截"
    ))
}

/// 退出时回收我们自己拉起的后端进程；复用外部服务时什么都不做。
pub fn shutdown(state: &AppState) {
    let Ok(mut guard) = state.backend.lock() else {
        return;
    };
    if let Some(mut child) = guard.child.take() {
        let _ = child.kill();
        let _ = child.wait();
        eprintln!("[backend] 已回收后端进程");
    }
}
