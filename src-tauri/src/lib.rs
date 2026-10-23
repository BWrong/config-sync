//! config-sync 的 Tauri 2 外壳。
//!
//! 这里的 `#[tauri::command]` 只做转发：所有逻辑在 `config-sync-core` 里。
//! 环境事实（`$HOME`、`config.toml`）由外壳读取后注入 Core，Core 自己绝不读。

mod settings;

use std::path::{Path, PathBuf};

use config_sync_core::{Core, CoreError, FileDiff, InventoryEntry, RepoStatus};
use settings::Settings;
use tauri::State;

pub struct AppState {
    core: Core,
    source_dir: PathBuf,
}

/// `~/Library/Application Support/config-sync/config.toml`。
fn config_path(home: &Path) -> PathBuf {
    home.join("Library/Application Support/config-sync/config.toml")
}

/// 读设置、解析源目录、构造 Core。
///
/// `config.toml` 坏了不阻塞启动：退回默认设置，并把手写错误打到 stderr（文件保持原样）。
fn state_from_environment() -> AppState {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .expect("HOME 未设置，无法定位 $HOME");
    let settings = Settings::load_or_create(&config_path(&home)).unwrap_or_else(|e| {
        eprintln!("config-sync: {e}；本次用默认设置启动");
        Settings::default()
    });
    let source_dir = settings.resolved_source_dir(&home);
    AppState { core: Core::new(&home, &source_dir, settings.core_config()), source_dir }
}

#[tauri::command]
fn inventory(state: State<'_, AppState>) -> Result<Vec<InventoryEntry>, CoreError> {
    state.core.inventory()
}

#[tauri::command]
fn repo_status(state: State<'_, AppState>) -> Result<RepoStatus, CoreError> {
    state.core.repo_status()
}

/// 点开一行时的 unified diff；比较哪两侧由 Core 按该行状态决定。
#[tauri::command]
fn diff(state: State<'_, AppState>, target: String) -> Result<FileDiff, CoreError> {
    state.core.diff(Path::new(&target))
}

/// 源目录里是否已经有仓库：false 时前端进「输入远端地址」。
#[tauri::command]
fn has_repo(state: State<'_, AppState>) -> bool {
    state.core.has_repo()
}

#[tauri::command]
fn source_dir(state: State<'_, AppState>) -> String {
    state.source_dir.display().to_string()
}

#[tauri::command]
fn clone_remote(state: State<'_, AppState>, remote: String) -> Result<(), CoreError> {
    state.core.clone(&remote)
}

pub fn run() {
    tauri::Builder::default()
        .manage(state_from_environment())
        .invoke_handler(tauri::generate_handler![
            inventory,
            repo_status,
            diff,
            has_repo,
            source_dir,
            clone_remote
        ])
        .run(tauri::generate_context!())
        .expect("config-sync 启动失败");
}
