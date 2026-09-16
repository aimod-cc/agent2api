// Windows 下以 GUI 子系统启动，避免弹出黑色控制台窗口
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    workbuddy_proxy_desktop_lib::run()
}
