// Release 版本不弹终端窗口（Windows 专用，macOS 上无副作用）。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    config_sync_app::run()
}
