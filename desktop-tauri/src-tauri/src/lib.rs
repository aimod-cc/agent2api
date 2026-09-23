/**
 * 应用入口 — 组装状态、注册插件与命令、创建主窗口。
 *
 * 前端源码在 desktop-tauri/ui/，通过 tauri.conf.json 的 frontendDist 引入。
 * 界面代码只依赖 window.workbuddyDesktop 这个接口，不感知具体壳，
 * 因此从 Electron 迁到 Tauri 时前端一行未改。职责拆成几块：
 *   server/      进程内 HTTP 服务器（网关本体；原为外部 node 后端进程）
 *                `server/config_migration.rs` 是 1.x 配置目录迁移的唯一实现，
 *                由本文件 setup 第一步调用（必须早于任何配置目录写盘）
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
mod legacy_install;
mod login;
mod login_profile;
mod settings;
mod state;
mod tray;
mod update;

// 网关本体与端口冲突分类已拆到独立 crate（`server/`，桌面与 headless 二进制
// 共用）。在这里以原名引入：crate 内所有 `crate::server::…` /
// `crate::port_conflict::…` 路径与拆分前完全一致，两侧代码零改动。
use agent2api_server::port_conflict;
use agent2api_server::server;

use tauri::{Emitter, Manager, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_autostart::MacosLauncher;
use tauri_plugin_autostart::ManagerExt as AutostartExt;
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};

use server::config_migration::MigrationOutcome;
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

/// 启动期致命错误的用户提示与收尾：显示可读原因 → 结束进程。
///
/// ── 为什么不是 `return Err(...)` ─────────────────────────────
/// 当前 Tauri（2.11）在 `setup` 返回 `Err` 时是 `panic!("Failed to setup app")`，
/// 而本项目 release 是 `panic = "abort"` —— 用户只会看到一个闪退，拿不到
/// 「旧数据在哪、为什么失败、怎么恢复」这些关键信息。这里改为显式路径：
/// 错误原样（含路径与系统错误，不含文件内容）走控制台 + 原生错误对话框，
/// 用户确认后以非零码结束进程，全程不写任何配置文件。
///
/// ── 对话框为什么用非阻塞 `show` ─────────────────────────────
/// setup 钩子跑在主线程上，`blocking_show` 的文档明确要求不能在主线程调用
/// （会冻住事件循环）；插件文档给的 setup 用法就是 `show(callback)`。
/// 回调在用户点掉对话框后触发，那里再请求退出 —— 期间事件循环照常运行，
/// 但 setup 已提前返回、窗口与托盘都没建，不会有任何初始化或写盘动作。
///
/// ── 看门狗 ─────────────────────────────────────────────────
/// 对话框万一没能显示（例如主线程任务投递失败），进程会变成「没有窗口、
/// 没有托盘、却仍占着单实例锁」的僵尸：用户再点图标只会把消息发给这个
/// 隐形进程，永远启动不起来。看门狗保证失败路径一定有终点；超时足够长
/// （5 分钟），正常用户读提示的时间远小于它。
///
/// ── 退出路径不会写盘 ────────────────────────────────────────
/// 迁移失败时 `settings::load()` 从未执行，`close_to_tray` 保持默认 false，
/// 因此 `ExitRequested` 分支不会拦截退出，只会调 `backend::shutdown` ——
/// 那里只做停机信号与日志，不发网络请求、不写配置文件（后端从未启动，
/// 定时签到全局句柄也未初始化）。`app.exit` 只触发 ExitRequested/Exit，
/// 不会再走一遍 setup 之后的初始化；看门狗走 `process::exit` 更直接：
/// 此时没有窗口、托盘、后台任务或文件句柄需要收尾。
fn report_startup_failure(app: &tauri::AppHandle, error: &str) {
    /// 用户长时间不确认时强制结束的上限
    const FORCE_EXIT_AFTER: std::time::Duration = std::time::Duration::from_secs(300);

    let message = format!(
        "Agent2API 启动已中止：{error}\n\n\
         本次未改动、未删除任何旧数据，也未创建新配置目录。\
         排除原因（磁盘空间、文件占用、权限）后重新打开本程序即可自动重试迁移。"
    );
    eprintln!("[Startup] ❌ {message}");
    let handle = app.clone();
    app.dialog()
        .message(message)
        .title("Agent2API 无法启动")
        .kind(MessageDialogKind::Error)
        .show(move |_| handle.exit(1));

    std::thread::spawn(|| {
        std::thread::sleep(FORCE_EXIT_AFTER);
        eprintln!("[Startup] ⚠️  错误提示长时间未确认，强制结束本次启动");
        std::process::exit(1);
    });
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
            commands::port_occupant,
            commands::end_port_occupant,
            commands::restart_app,
            commands::check_port,
            commands::change_port,
            commands::start_login,
            commands::start_autoclaw_oauth_login,
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
            // 自定义标题栏的窗口三键（窗口已 decorations(false)，见建窗处）
            commands::window_minimize,
            commands::window_toggle_maximize,
            commands::window_close,
            commands::window_is_maximized,
        ])
        .setup(|app| {
            let handle = app.handle().clone();

            // ── 第一步：一次性配置目录迁移（`~/.workbuddy-proxy` → `~/.agent2api`）──
            // 必须是**全进程最早**的一处配置目录访问：晚于任何写盘（桌面设置、
            // config.json、日志库、账号库）时，一旦迁移失败，后续写盘就会把新目录
            // 建出来，`target.exists()` 从此为真，迁移再也无法重试，用户看到的是
            // 「账号/配置全没了」。放在这里，失败时下面所有会写盘的初始化都不执行。
            //
            // 这时日志库还没装，所以迁移日志只能走控制台（`eprintln!`，见 logging）。
            match server::config_migration::migrate_config_dir() {
                Ok(MigrationOutcome::Migrated { from, to }) => {
                    eprintln!(
                        "[Config] 📦 配置目录已迁移: {} → {}（旧目录保留，可回退）",
                        from.display(),
                        to.display()
                    );
                }
                Ok(MigrationOutcome::NothingToDo) => {}
                // 用户用环境变量显式指定了配置目录：迁移不适用，也不该提示打扰
                Ok(MigrationOutcome::SkippedEnvOverride) => {}
                Err(error) => {
                    // 迁移失败不能继续：以空配置启动会让新目录被后续写盘建出来，
                    // 迁移从此不会重试，且用户看到的是「数据消失」。
                    // 这里显示可读错误（原生对话框）后结束本次启动 ——
                    // 旧目录原样保留，新目录未被创建，下次启动自动重试。
                    report_startup_failure(&handle, &error);
                    return Ok(());
                }
            }

            // 设置先读一次并缓存：窗口事件回调是同步的，需要立即拿到
            // close_to_tray 才能决定关窗是隐藏还是退出
            let app_settings = settings::load();
            {
                let state = app.state::<AppState>();
                state.window.set_close_to_tray(app_settings.close_to_tray);
            }

            // ── 旧「当前用户」级安装的清理 + 自启登记刷新（1.x → perMachine 迁移）──
            // 安装器改成 perMachine 后以管理员身份运行，此时 `%LOCALAPPDATA%` 与
            // HKCU 指向管理员账户，读不到也删不掉发起安装的用户的旧安装 ——
            // 只有这里（普通用户上下文的启动期）能覆盖到它。
            //
            // 旧版还把自启登记写成 `%LOCALAPPDATA%\<旧产品名>`、值名用旧产品名；
            // 新版装在 Program Files、值名换成 productName，两条互不覆盖，旧的那条
            // 指向已被删除的路径，用户不开设置页就永远不会被修正，自启静默失效。
            //
            // ── 为什么两件事放在同一个后台任务里顺序执行 ──────────────
            // 两件事都要读/写 HKCU 的 Run 键（清理判「旧登记是否已失效」、刷新写
            // 新登记），顺序执行让刷新看到的是清理之后的状态，也避免两个线程交错
            // 操作同一处注册表。
            //
            // 放后台：旧目录可能含 88MB 的 node.exe，`remove_dir_all` 叠加杀软扫描
            // 会明显拖慢启动，而这两件事与后续初始化没有任何依赖关系。
            // 失败只记日志、下次启动重试，绝不阻断启动（见 legacy_install 模块头）。
            let autostart_wanted = app_settings.autostart;
            let cleanup_handle = handle.clone();
            tauri::async_runtime::spawn_blocking(move || {
                // ── 开发态（debug 构建）不做安装迁移 ────────────────────────
                // 这两个动作都会改真实系统状态：清理会删掉本机已安装的正式版
                // （安装目录 + 快捷方式 + 卸载项），自启刷新会把开机自启登记指向
                // `target\debug` 下的调试副本。而 `tauri dev` 是日常动作 ——
                // 跑一次就把用户装的正式版删了，代价远大于收益（实际发生过：
                // 开发态启动后，开始菜单/桌面快捷方式全部失效）。
                // release 打包（唯一的发布形态）行为完全不变；要验证清理逻辑
                // 本身，用 `tauri build` 出的安装包跑。
                if cfg!(debug_assertions) {
                    return;
                }
                legacy_install::cleanup_legacy_user_install();
                if !autostart_wanted {
                    return;
                }
                let name = cleanup_handle.package_info().name.clone();
                // 幂等：登记已指向当前 exe 时不做任何写入；用户在任务管理器
                // 「启动」页里停用过的也不重建（判定见 legacy_install）
                if !legacy_install::autostart_needs_refresh(&name) {
                    return;
                }
                match cleanup_handle.autolaunch().enable() {
                    Ok(()) => {
                        eprintln!("[Cleanup] 已按当前安装路径重新登记开机自启（升级迁移）: {name}")
                    }
                    Err(error) => {
                        eprintln!("[Cleanup] 重新登记开机自启失败（下次启动再试）: {error}")
                    }
                }
            });

            // 托盘先建好再建窗口：开机自启不显示窗口，托盘是唯一入口
            if let Err(error) = tray::create(&handle) {
                eprintln!("[tray] 创建托盘图标失败: {error}");
            }

            // 主窗口先建好并加载界面，后端在后台异步拉起，
            // 界面不会因为等待 node 启动而白屏。
            // 开机自启时不显示窗口（visible(false) 比先显示再隐藏更干净，
            // 不会在任务栏闪一下）。
            //
            // ── decorations(false)：去掉系统标题栏，换界面自绘的标题栏 ──
            // 参考 OmniProxy 的 TitleBar：32px 高、左标题右三键，与界面
            // 同一套配色（titlebar.js 动态创建）。无装饰之后三项原生行为
            // 的恢复方式（Tauri 2 内建，无需额外代码）：
            //   · 拖动 / 双击最大化：前端给标题栏元素声明 `data-tauri-drag-region`
            //     属性 —— Tauri 的 core 脚本监听 mousedown，目标元素带该属性
            //     才拖动（子元素不带不拖），双击（连击）切最大化；
            //   · 边缘拖拽缩放：`resizable(true)`（默认即开）下，Windows 端
            //     的 tao 对无装饰窗口做命中测试，边缘仍可拖拽缩放；
            //   · 关闭：界面的 ✕ 走 window_close 命令 → CloseRequested，
            //     与系统关闭按钮同一条事件路径，托盘拦截逻辑不变。
            // 注：macOS 上这会一并去掉「红绿灯」，本项目面向 Windows
            // （NSIS 安装包），macOS 如需保留要用 titleBarStyle: Overlay
            // 另行适配，此处不做特殊处理。
            WebviewWindowBuilder::new(app, MAIN_WINDOW_LABEL, WebviewUrl::App("index.html".into()))
                .title("Agent2API · 多提供商本地网关")
                .decorations(false)
                .inner_size(WIN_WIDTH, WIN_HEIGHT)
                .min_inner_size(WIN_MIN_WIDTH, WIN_MIN_HEIGHT)
                .center()
                .maximized(true)
                .visible(!launched_by_autostart())
                .initialization_script(bridge::bridge_js())
                .build()?;

            // 启动后端；就绪后再跑一次启动维护（临期 token 刷新 + 余额查询）
            tauri::async_runtime::spawn(async move {
                if let Err(failure) = backend::ensure_ready(&handle).await {
                    eprintln!("[backend] 启动失败: {}", failure.message);
                    // 存一份到 AppState：事件是一次性的，界面可能在事件发出之后
                    // 才订阅（窗口还在加载、WebView 被刷新），那时只能靠主动查
                    // （backend_status）拿到失败原因，否则状态灯永远说不清为什么灰着
                    if let Some(state) = handle.try_state::<AppState>() {
                        if let Ok(mut guard) = state.backend.lock() {
                            guard.failure = Some(failure.clone());
                        }
                    }
                    let _ = handle.emit("backend:error", failure);
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
