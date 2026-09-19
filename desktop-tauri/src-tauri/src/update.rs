//! 软件更新中「壳」这一侧要做的事。
//!
//! 检测新版本、下载安装包都放在 Node 后端（壳侧 reqwest 为省 TLS 依赖
//! 关掉了默认特性，发不出 GitHub 的 HTTPS 请求）。这里只负责三件
//! 壳才能做的事：
//!   1. 报出当前应用版本（后端不知道自己被哪个壳打包，比较必须有此值）
//!   2. 校验后端下载好的安装包路径确实落在受控目录内
//!   3. 启动安装包，并按需要退出本程序（NSIS 覆盖安装前要先让出文件占用）
//!
//! 路径校验不是多余的：`run_installer` 会执行一个可执行文件，
//! 若把路径当信任输入，等于给渲染层开了任意程序执行的入口。

use std::path::{Path, PathBuf};

use crate::gateway;

/// 下载目录：与后端 `createUpdateManager` 的 downloadDir 一致
pub fn download_dir() -> PathBuf {
    gateway::config_dir().join("updates")
}

/// 规范化路径，消除 `..` 与重复分隔符后再比较前缀。
/// 目标不存在时 canonicalize 会失败，因此只对存在的父目录做规范化。
fn normalize(path: &Path) -> Option<PathBuf> {
    if path.exists() {
        return path.canonicalize().ok();
    }
    let parent = path.parent()?;
    let name = path.file_name()?;
    Some(parent.canonicalize().ok()?.join(name))
}

/// 校验待运行的安装包：必须存在、是 .exe、且位于下载目录内。
///
/// 三者缺一不可 —— 少了「目录内」这一条，渲染层就能传入任意路径
/// 让本进程替它执行；少了后缀检查，则可能被用来启动脚本类文件。
pub fn verify_installer(raw: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(raw.trim());
    if raw.trim().is_empty() {
        return Err("安装包路径为空".to_string());
    }
    if !path.is_absolute() {
        return Err("安装包路径必须是绝对路径".to_string());
    }

    let suffix = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if suffix != "exe" {
        return Err("只允许运行 .exe 安装包".to_string());
    }

    let target = normalize(&path).ok_or_else(|| "无法解析安装包路径".to_string())?;
    let root = normalize(&download_dir())
        .ok_or_else(|| "无法解析下载目录".to_string())?;
    if !target.starts_with(&root) {
        return Err("安装包不在受控的下载目录内，已拒绝执行".to_string());
    }
    if !target.is_file() {
        return Err("安装包不存在或不是文件".to_string());
    }
    Ok(target)
}

/// 启动安装包。`silent` 为 true 时带上 NSIS 的静默参数。
///
/// 不等待安装结束：NSIS 安装程序通常需要用户交互，阻塞在这里会让
/// 界面一直转圈。调用方随后自行决定是否退出程序。
pub fn launch_installer(path: &Path, silent: bool) -> Result<(), String> {
    #[cfg(windows)]
    {
        launch_installer_elevated(path, silent)
    }
    #[cfg(not(windows))]
    {
        let mut command = std::process::Command::new(path);
        if silent {
            // /S 静默，/R 装完自动重启；Tauri 的 NSIS 模板支持这两个参数
            command.arg("/S").arg("/R");
        }
        command
            .current_dir(path.parent().unwrap_or_else(|| Path::new(".")))
            .spawn()
            .map(|_| ())
            .map_err(|error| format!("启动安装包失败: {error}"))
    }
}

/// 把 `canonicalize` 得到的路径还原成 shell 能接受的普通形式。
///
/// `verify_installer` 返回的是 `canonicalize` 的结果，而 Windows 上的
/// `canonicalize` 一律带 `\\?\` verbatim 前缀（UNC 则是 `\\?\UNC\...`）。
/// shell 的 API 不认这种形态：实测 `ShellExecuteW` 对 `\\?\C:\...\x.exe`
/// 直接返回 SE_ERR_FNF(2)，而 `Command::spawn`（CreateProcess）本来能吃下它 ——
/// 换成 ShellExecute 之后这一步是必需的，否则每次自动更新都会「找不到安装包」。
#[cfg(windows)]
fn shell_path(path: &Path) -> String {
    let text = path.to_string_lossy().to_string();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    match text.strip_prefix(r"\\?\") {
        Some(rest) => rest.to_string(),
        None => text,
    }
}

/// Windows 上的安装包启动：必须走 `ShellExecuteW` + `runas` 提权。
///
/// ── 为什么不能用 `Command::spawn`（提权后必然踩到）────────────────
/// perMachine 的安装包带 requireAdministrator 清单。`Command::spawn` 内部是
/// CreateProcess，而 CreateProcess **不会触发 UAC**：它直接失败并返回
/// `ERROR_ELEVATION_REQUIRED (740)`。只有 ShellExecute 会走 shell 的提权路径
/// 弹出 UAC 确认框，因此这里是唯一可行的调用方式。
///
/// 声明手法与 `login.rs` 的 `open_in_browser` 相同（不新增依赖）。
/// `SW_SHOWNORMAL` 让 UAC 确认后安装向导正常显示在用户面前。
#[cfg(windows)]
fn launch_installer_elevated(path: &Path, silent: bool) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;

    const SW_SHOWNORMAL: i32 = 1;
    /// 用户在 UAC 弹窗点了「否」：ShellExecuteW 此时返回 SE_ERR_ACCESSDENIED(5)
    /// （`ShellExecuteEx` 则是 GetLastError 给 ERROR_CANCELLED(1223)，两者含义
    /// 相同 —— 都表示「提权请求没被批准」）。
    const SE_ERR_ACCESSDENIED: isize = 5;
    const ERROR_CANCELLED: isize = 1223;

    #[link(name = "shell32")]
    extern "system" {
        fn ShellExecuteW(
            hwnd: *mut std::ffi::c_void,
            operation: *const u16,
            file: *const u16,
            parameters: *const u16,
            directory: *const u16,
            show_cmd: i32,
        ) -> *mut std::ffi::c_void;
    }

    let to_wide = |text: &str| -> Vec<u16> {
        std::ffi::OsStr::new(text).encode_wide().chain(std::iter::once(0)).collect()
    };
    // runas = 提权启动；安装包自身带 requireAdministrator 清单
    let operation = to_wide("runas");
    let file = to_wide(&shell_path(path));
    // /S 静默，/R 装完自动重启（与改动前的行为一致）
    let parameters = if silent { to_wide("/S /R") } else { to_wide("") };
    let directory = path
        .parent()
        .map(|dir| to_wide(&shell_path(dir)))
        .unwrap_or_else(|| to_wide(""));

    // ShellExecuteW 的返回值 <= 32 表示失败（这是 Win32 的历史约定）
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            operation.as_ptr(),
            file.as_ptr(),
            parameters.as_ptr(),
            directory.as_ptr(),
            SW_SHOWNORMAL,
        )
    };
    let code = result as isize;
    if code > 32 {
        return Ok(());
    }
    // 5 / 1223 都是「提权没被批准」：用户在 UAC 弹窗点了「否」，或系统的
    // 提权策略不允许（标准账户且无人可输管理员密码）。给一句能照做的提示，
    // 而不是一个光秃秃的错误码 —— 这是新安装模式下最常见的一种失败。
    if code == SE_ERR_ACCESSDENIED || code == ERROR_CANCELLED {
        return Err(
            "安装已取消：没有获得管理员权限（UAC 确认被拒绝）。新版本安装在 Program Files，\
             安装程序需要管理员权限；可重新点击「安装并重启」，并在系统弹窗中选择「是」"
                .to_string(),
        );
    }
    Err(format!("启动安装包失败: ShellExecute 返回 {code}"))
}
