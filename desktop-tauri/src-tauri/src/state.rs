//! 应用共享状态。
//!
//! 只放可变、跨命令共享的部分：进程内服务器句柄、登录会话，以及窗口生命周期
//! 相关的两个开关（退出标志、关闭到托盘）。
//! 窗口句柄不在这里 —— 由 Tauri 的 `AppHandle::get_webview_window` 按标签查找，
//! 避免自己维护一份可能失同步的副本。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use tokio::sync::oneshot;

use crate::port_conflict::StartupFailure;

/// 进程内 HTTP 服务器的停机句柄。
///
/// 旧版本这里放的是 `Option<Child>`（外部 node 子进程）；后端改成进程内服务器后
/// 不再有子进程可杀，改为「发送一次停机信号 + 记下端口」。
/// 语义上的关键差别：现在是**无条件**持有句柄 —— 不再有「复用外部已运行服务」
/// 那种「没有 child 就什么都不做」的分支，因为服务器一定由本进程启动。
#[derive(Default)]
pub struct BackendHandle {
    /// 停机信号发送端；take 走后不再持有（shutdown 幂等）
    pub shutdown_tx: Option<oneshot::Sender<()>>,
    pub port: u16,
    /// 最近一次启动失败（成功启动后清空）。
    ///
    /// 为什么要存下来：`backend:error` 是一次性事件，而界面可能在事件发出**之后**
    /// 才订阅（窗口还没加载完、用户刷新了 WebView）。只靠事件的话，用户重启界面
    /// 就再也看不到「为什么起不来」——只能看到一个永远灰着的状态灯。
    /// 存一份让界面随时能查，事件只负责「提醒你现在就去查」。
    pub failure: Option<StartupFailure>,
}

/// 一次进行中的登录：state 用于轮询后端，edition/mode 供界面展示与取消时判断。
#[derive(Clone)]
pub struct ActiveLogin {
    pub state: String,
    pub edition: String,
    pub mode: String,
    /// 这次登录属于哪一家（provider id）。
    ///
    /// 壳侧为什么也要记：登录窗口的导航拦截要判断「这个 URL 是不是本家的回调」，
    /// 而各家的回调协议不同（小浣熊是 `office-raccoon://auth/callback`，
    /// workbuddy 没有回调 —— 它的凭证由后端轮询上游拿）。把 provider 记在
    /// 活动登录上，拦截逻辑就不必猜、也不必让前端多传一个参数。
    pub provider: String,
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
