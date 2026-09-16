//! 应用共享状态。
//!
//! 只放可变、跨命令共享的部分：后端进程句柄、登录会话，以及窗口生命周期
//! 相关的两个开关（退出标志、关闭到托盘）。
//! 窗口句柄不在这里 —— 由 Tauri 的 `AppHandle::get_webview_window` 按标签查找，
//! 避免自己维护一份可能失同步的副本。

use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// 后端进程由我们拉起时才会记录；复用外部已启动的服务时保持 None，
/// 退出时据此决定要不要杀进程（不能误杀用户自己起的服务）。
#[derive(Default)]
pub struct BackendHandle {
    pub child: Option<Child>,
    pub port: u16,
}

/// 一次进行中的登录：state 用于轮询后端，edition/mode 供界面展示与取消时判断。
#[derive(Clone)]
pub struct ActiveLogin {
    pub state: String,
    pub edition: String,
    pub mode: String,
}

/// 与窗口生命周期相关的开关。
///
/// `exiting` 必须在「用户主动退出」与「关窗被拦下」之间做区分：
/// 两者都会走到 `RunEvent::ExitRequested`，只看 `close_to_tray`
/// 会让托盘的「退出」永远退不掉。
///
/// `close_to_tray` 缓存一份当前设置：窗口事件回调（含 `ExitRequested`）
/// 是同步的、每关一次窗都可能触发，不该每次都去读磁盘。
#[derive(Default)]
pub struct WindowState {
    pub exiting: AtomicBool,
    pub close_to_tray: AtomicBool,
}

impl WindowState {
    /// 用户主动退出（托盘菜单）：置位后 `ExitRequested` 必须放行
    pub fn begin_exit(&self) {
        self.exiting.store(true, Ordering::SeqCst);
    }

    pub fn is_exiting(&self) -> bool {
        self.exiting.load(Ordering::SeqCst)
    }

    pub fn set_close_to_tray(&self, value: bool) {
        self.close_to_tray.store(value, Ordering::SeqCst);
    }

    pub fn close_to_tray(&self) -> bool {
        self.close_to_tray.load(Ordering::SeqCst)
    }
}

#[derive(Default)]
pub struct AppState {
    pub backend: Mutex<BackendHandle>,
    pub login: Mutex<Option<ActiveLogin>>,
    pub window: WindowState,
}

impl AppState {
    pub fn new() -> Self {
        Self::default()
    }

    /// 用户主动退出：置位后 `RunEvent::ExitRequested` 不再拦截
    pub fn begin_exit(&self) {
        self.window.begin_exit();
    }
}
