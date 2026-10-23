//! config-sync 的 Tauri 2 外壳。
//!
//! 这里的 `#[tauri::command]` 只做转发：所有逻辑在 `config-sync-core` 里。
//! 环境事实（`$HOME`）由外壳读取后注入 Core，Core 自己绝不读环境变量。

use std::path::PathBuf;

use config_sync_core::{Config, Core, CoreError, InventoryEntry, RepoStatus};
use tauri::State;

pub struct AppState {
    core: Core,
}

/// 用本机环境构造 Core。
///
/// `config.toml` 的读写属于「配置与 clone」那张票；这版直接用默认值。
fn core_from_environment() -> Core {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .expect("HOME 未设置，无法定位 $HOME");
    let source_dir = home.join(".config-sync");
    Core::new(home, source_dir, Config::default())
}

#[tauri::command]
fn inventory(state: State<'_, AppState>) -> Result<Vec<InventoryEntry>, CoreError> {
    state.core.inventory()
}

#[tauri::command]
fn repo_status(state: State<'_, AppState>) -> Result<RepoStatus, CoreError> {
    state.core.repo_status()
}

pub fn run() {
    tauri::Builder::default()
        .manage(AppState { core: core_from_environment() })
        .invoke_handler(tauri::generate_handler![inventory, repo_status])
        .run(tauri::generate_context!())
        .expect("config-sync 启动失败");
}
