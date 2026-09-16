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
