//! 桌面端应用设置（关闭到托盘、开机自启）的持久化。
//!
//! 放在配置目录而不是 WebView 的 localStorage：这些设置要影响进程自身行为
//! （关窗是否拦截、是否登记自启动），必须能在窗口还没建好、甚至界面没跑起来时读到。
//!
//! 读写容错与 `gateway.rs` 读 config.json 一致：文件缺失、被手工改坏、
//! 权限不足，一律回落到默认值，绝不因为一个配置文件让应用起不来。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::gateway;

/// 与后端 config.json 同目录，便于用户一起备份/迁移
const FILE_NAME: &str = "desktop-settings.json";

/// 设置文件路径
pub fn file_path() -> PathBuf {
    gateway::config_dir().join(FILE_NAME)
}

/// 应用设置。
///
/// 前后端之间以 camelCase JSON 传输（`closeToTray` / `autostart` / `proxyPort`），
/// 与 renderer 的字段名保持一致，界面无需做任何映射。
///
/// `default` 用在结构体上（而非逐字段）：这样后续新增字段时，**旧设置文件里
/// 缺这个键不会导致整份设置反序列化失败**——缺的字段各自取 Default。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AppSettings {
    /// 关闭窗口时最小化到托盘而不是退出
    pub close_to_tray: bool,
    /// 开机自动启动
    pub autostart: bool,
    /// 网关监听端口。
    ///
    /// 0 = 未设置，回落到默认 3065（与 `gateway::proxy_port()` 的语义一致）。
    /// 存这里而不是后端 config.json：端口决定**壳侧**管理客户端的连接目标，
    /// 且要在服务端 bind 之前就读到，属于「应用级启动设置」而非网关业务配置。
    /// 改这个值需要重启进程才生效（服务端 bind 之后端口改不了）。
    pub proxy_port: u16,
}

impl Default for AppSettings {
    fn default() -> Self {
        // 关闭到托盘默认开启：网关的价值在于后台持续转发，
        // 用户点关闭通常只是想收起界面，而不是让转发中断
        Self { close_to_tray: true, autostart: false, proxy_port: 0 }
    }
}

/// 读取设置；文件不存在或内容不可解析时返回默认值。
pub fn load() -> AppSettings {
    std::fs::read_to_string(file_path())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// 覆盖写入设置。写入前先确保配置目录存在（首次运行时目录可能还没有）。
pub fn save(settings: &AppSettings) -> Result<(), String> {
    let path = file_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|error| format!("创建配置目录失败: {error}"))?;
    }
    let text = serde_json::to_string_pretty(settings)
        .map_err(|error| format!("设置序列化失败: {error}"))?;
    std::fs::write(&path, text).map_err(|error| format!("写入设置文件失败: {error}"))
}
