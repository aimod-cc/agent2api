//! Agent2API 网关本体（独立 crate）。
//!
//! ── 本 crate 服务两种形态 ────────────────────────────────────
//!   · Tauri 桌面端：壳进程把本 crate 当库链接，网关作为进程内 HTTP
//!     服务器跑在 tauri 的 Tokio 运行时上（`server::start`）；
//!   · headless 服务器 / Docker：`agent2api-server` 二进制（`src/bin/`）
//!     自建 Tokio 运行时、自己托管管理界面（`server::static_files`），
//!     不链接任何 GUI 依赖。
//!
//! 模块结构是**刻意保留的一层壳**：`server/`、`port_conflict.rs` 的目录
//! 与文件名与拆分前（桌面 crate 的 `src/server`）完全一致，内部所有
//! `crate::server::…` / `crate::port_conflict::…` 路径零改动 —— 桌面侧
//! 用 `use agent2api_server as server` 重导出后，两边的既有路径都继续成立。
//!
//! ── 为什么 spawn 必须经由 [`spawn_task`] ─────────────────────
//! 拆分前模块里用的是 `tauri::async_runtime::spawn`（壳提供全局运行时，
//! 任意线程可调）；本 crate 不依赖 tauri，headless 又自带 `#[tokio::main]`
//! 运行时。统一入口 [`spawn_task`] 兼容两种宿主：
//!   · 已在 Tokio 运行时上下文里（桌面：任务跑在 tauri 运行时的工作
//!     线程上；headless：`#[tokio::main]`）→ 直接 `Handle::spawn`，
//!     任务落在**宿主**的运行时上，与拆分前行为一致；
//!   · 不在运行时上下文（从任意同步线程起后台任务）→ 落到惰性创建的
//!     全局运行时，**不会 panic** —— release 档是 panic=abort，
//!     一次误用就会带走整个进程，这正是拆分前统一走
//!     `tauri::async_runtime::spawn` 的原因，本入口继承同一保证。

pub mod paths;
pub mod port_conflict;
pub mod server;
pub mod web_shim;

use std::future::Future;
use std::sync::OnceLock;

use tokio::runtime::{Handle, Runtime};

/// 惰性全局运行时：只承接「在非 Tokio 上下文里发起的后台任务」，
/// 正常路径（handler / 既有任务体内）不会碰到它。
static GLOBAL_RUNTIME: OnceLock<Runtime> = OnceLock::new();

/// 项目统一的 spawn 入口：桌面与 headless 共用（原
/// `tauri::async_runtime::spawn` 的替身，见 crate 文档头）。
pub fn spawn_task<F>(future: F) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    match Handle::try_current() {
        Ok(handle) => handle.spawn(future),
        Err(_) => global_runtime().spawn(future),
    }
}

fn global_runtime() -> &'static Runtime {
    GLOBAL_RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("创建后台 Tokio 运行时失败")
    })
}
