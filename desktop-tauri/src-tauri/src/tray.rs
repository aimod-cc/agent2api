//! 系统托盘：图标、右键菜单、左键唤起主窗口。
//!
//! 托盘是本应用唯一的「后台入口」：关窗后进程仍在跑（见 lib.rs 的关闭到托盘逻辑），
//! 用户只能靠托盘图标把界面叫回来或彻底退出，所以菜单项与左键点击都复用了
//! 同一套 `show_main_window` / 退出实现，避免出现「图标恢复不了窗口」这类死角。
//!
//! 图标直接复用 `app.default_window_icon()`（即 tauri.conf.json 里配置的 bundle 图标），
//! 不额外引入托盘专用图片资源。

use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager,
};

use crate::state::AppState;

pub const TRAY_ID: &str = "main-tray";
const MENU_SHOW: &str = "tray-show";
const MENU_QUIT: &str = "tray-quit";

/// 托盘提示文字
const TOOLTIP: &str = "WorkBuddy 本地代理";

/// 创建托盘图标。应用启动时调用一次即可。
pub fn create(app: &AppHandle) -> tauri::Result<()> {
    let show_item = MenuItem::with_id(app, MENU_SHOW, "显示主窗口", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, MENU_QUIT, "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show_item, &quit_item])?;

    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .tooltip(TOOLTIP)
        .menu(&menu)
        // 左键单击留给「唤起窗口」，因此不让它弹菜单；右键照常弹
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            MENU_SHOW => show_main_window(app),
            MENU_QUIT => request_exit(app),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            // 只认左键「抬起」：Windows 下按下与抬起各发一次事件，
            // 不过滤会让单击时恢复两次窗口
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        });

    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    // 即使拿不到默认图标也照样把托盘建起来：宁可是个无图标的占位，
    // 也不能让关窗后的用户失去唯一入口
    builder.build(app)?;
    Ok(())
}

/// 显示并聚焦主窗口。窗口可能处于「隐藏」或「最小化」两种状态，
/// 两种都要能恢复，所以固定按 unminimize → show → set_focus 走一遍。
pub fn show_main_window(app: &AppHandle) {
    let Some(window) = app.get_webview_window(crate::MAIN_WINDOW_LABEL) else {
        return;
    };
    let _ = window.unminimize();
    let _ = window.show();
    let _ = window.set_focus();
}

/// 真正退出应用。
///
/// 先置位「正在退出」标志再 `exit(0)`：开启关闭到托盘时，`exit(0)` 同样会走
/// `RunEvent::ExitRequested`，没有这个标志就会被当成「用户关窗」再拦一次，
/// 表现为「托盘点了退出但进程还在」。
pub fn request_exit(app: &AppHandle) {
    app.state::<AppState>().begin_exit();
    app.exit(0);
}
