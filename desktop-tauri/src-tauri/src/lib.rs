/**
 * 应用入口 — 组装状态、注册插件与命令、创建主窗口。
 *
 * 前端源码在 desktop-tauri/ui/，通过 tauri.conf.json 的 frontendDist 引入。
 * 界面代码只依赖 window.workbuddyDesktop 这个接口，不感知具体壳，
 * 因此从 Electron 迁到 Tauri 时前端一行未改。职责拆成几块：
 *   server/      进程内 HTTP 服务器（网关本体；原为外部 node 后端进程）
 *   backend.rs   服务生命周期（启动/健康检查/退出停机信号）
 *   gateway.rs   管理 API 的 HTTP 客户端（含 API Key 读取与统一解包）
 *   login.rs     登录窗口与轮询
 *   commands.rs  暴露给前端的 invoke 命令（对齐原 preload 的 API 面）
 *   settings.rs  应用设置（关闭到托盘、开机自启）的持久化
 *   tray.rs      系统托盘图标、菜单与窗口唤起
 *
 * 窗口生命周期（关闭到托盘 / 托盘退出 / 单实例）集中在本文件：
 * 这些都是「应用级」行为，散落到各模块反而看不清谁拦了退出。
 *
 * 前端桥接：主窗口在创建时注入 bridge.js，在页面脚本执行前把
 * window.workbuddyDesktop 装好，因此渲染层代码无需感知 Tauri。
 */

mod backend;
mod bridge;
mod commands;
mod gateway;
mod login;
mod server;
mod settings;
mod state;
mod tray;
mod update;

use tauri::{Emitter, Manager, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_autostart::MacosLauncher;

use state::AppState;

/** 主窗口默认尺寸与最小尺寸（与原 Electron 端保持一致） */
const WIN_WIDTH: f64 = 1060.0;
const WIN_HEIGHT: f64 = 800.0;
const WIN_MIN_WIDTH: f64 = 820.0;
const WIN_MIN_HEIGHT: f64 = 600.0;

/// 主窗口标签：托盘唤起、关闭拦截、单实例激活都按它查找
pub const MAIN_WINDOW_LABEL: &str = "main";

/// 开机自启时附加的命令行参数。启动时见到它就不显示窗口，
/// 只把托盘留在后台，避免开机弹窗打扰用户。
const AUTOSTART_FLAG: &str = "--autostart";

/// 本次启动是否由开机自启触发
fn launched_by_autostart() -> bool {
    std::env::args().any(|arg| arg == AUTOSTART_FLAG)
}

pub fn run() {
    let mut builder = tauri::Builder::default();

    // 单实例必须第一个注册：插件按注册顺序执行，晚于其它插件时，
    // 第二个实例可能已经建好窗口/托盘才被判定为重复启动
    #[cfg(desktop)]
    {
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // 第二个实例已被插件结束进程，这里只负责把已有窗口叫到前台
            tray::show_main_window(app);
        }));
    }

    builder
        .plugin(tauri_plugin_dialog::init())
        .plugin(
            // 自启动登记带 --autostart，启动时据此判断要不要静默（不显示窗口）。
            // MacosLauncher 是占位参数，本目标只关心 Windows。
            tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, Some(vec![AUTOSTART_FLAG])),
        )
        .manage(AppState::new())
        .invoke_handler(tauri::generate_handler![
            commands::api_request,
            commands::api_request_text,
            commands::backend_status,
            commands::start_login,
            commands::login_state,
            commands::cancel_login,
            commands::export_logs,
            commands::get_app_settings,
            commands::save_app_settings,
            commands::export_accounts,
            commands::import_accounts,
            commands::check_update,
            commands::download_update,
            commands::update_progress,
            commands::cancel_update,
            commands::run_installer,
            commands::set_window_theme,
            commands::open_release_page,
        ])
        .setup(|app| {
            let handle = app.handle().clone();

            // 设置先读一次并缓存：窗口事件回调是同步的，需要立即拿到
            // close_to_tray 才能决定关窗是隐藏还是退出
            let app_settings = settings::load();
            {
                let state = app.state::<AppState>();
                state.window.set_close_to_tray(app_settings.close_to_tray);
            }

            // 托盘先建好再建窗口：开机自启不显示窗口，托盘是唯一入口
            if let Err(error) = tray::create(&handle) {
                eprintln!("[tray] 创建托盘图标失败: {error}");
            }

            // 主窗口先建好并加载界面，后端在后台异步拉起，
            // 界面不会因为等待 node 启动而白屏。
            // 开机自启时不显示窗口（visible(false) 比先显示再隐藏更干净，
            // 不会在任务栏闪一下）。
            WebviewWindowBuilder::new(app, MAIN_WINDOW_LABEL, WebviewUrl::App("index.html".into()))
                .title("WorkBuddy 本地代理 · 会话与账号管理")
                .inner_size(WIN_WIDTH, WIN_HEIGHT)
                .min_inner_size(WIN_MIN_WIDTH, WIN_MIN_HEIGHT)
                .center()
                .maximized(true)
                .visible(!launched_by_autostart())
                .initialization_script(bridge::BRIDGE_JS)
                .build()?;

            // 启动后端；就绪后再跑一次启动维护（临期 token 刷新 + 余额查询）
            tauri::async_runtime::spawn(async move {
                if let Err(error) = backend::ensure_ready(&handle).await {
                    eprintln!("[backend] 启动失败: {error}");
                    let _ = handle.emit("backend:error", error);
                    return;
                }
                commands::startup_maintenance(handle).await;
            });

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("Tauri 应用初始化失败")
        .run(|app, event| match event {
            // 关窗时按设置决定「隐藏到托盘」还是「真退出」。
            // 拦截关闭后窗口只是隐藏，后端进程继续存活，网关照常转发。
            tauri::RunEvent::WindowEvent {
                label,
                event: tauri::WindowEvent::CloseRequested { api, .. },
                ..
            } => {
                if label != MAIN_WINDOW_LABEL {
                    return;
                }
                let state = app.state::<AppState>();
                // 用户主动退出（托盘菜单）时就别拦了，否则退出路径被自己挡死
                if state.window.close_to_tray() && !state.window.is_exiting() {
                    api.prevent_close();
                    if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
                        let _ = window.hide();
                    }
                }
            }
            // 最后一个窗口关闭后 Tauri 会请求退出，这里再兜一次：
            // 只拦「用户关窗」这一种，托盘退出与主动 exit 都要放行。
            // 放行时顺手回收后端 —— 例如用户关掉了「关闭到托盘」后直接关窗真退出，
            // 这条路径不会走到下面的 Exit 分支，不在这里回收就会漏下 node 进程
            // （shutdown 幂等，与其它调用点重复也不会有副作用）。
            tauri::RunEvent::ExitRequested { api, .. } => {
                let state = app.state::<AppState>();
                if state.window.close_to_tray() && !state.window.is_exiting() {
                    api.prevent_exit();
                } else {
                    backend::shutdown(&state);
                }
            }
            // 最后兜底回收后端进程（复用外部服务的不会被动）。
            // 注意：程序化退出（托盘退出、安装前退出走的都是 `AppHandle::exit(0)`）
            // 不保证触发本事件，这是 Tauri 的已知行为，所以那几条主动退出路径
            // 已各自显式调用了 shutdown，这里只覆盖其余自然退出。
            tauri::RunEvent::Exit => {
                let state = app.state::<AppState>();
                backend::shutdown(&state);
            }
            _ => {}
        });
}
