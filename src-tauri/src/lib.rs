//! config-sync 的 Tauri 2 外壳。
//!
//! 这里的 `#[tauri::command]` 只做转发：所有逻辑在 `config-sync-core` 里。
//! 环境事实（`$HOME`、`config.toml`）由外壳读取后注入 Core，Core 自己绝不读。

mod settings;
mod watch;

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use config_sync_core::{AddOutcome, Core, CoreError, ErrorKind, FileDiff, InventoryEntry, RepoStatus};
use serde::Serialize;
use settings::Settings;
use tauri::{AppHandle, Manager, State};

/// 改设置后要整体重建的东西：Core 与解析后的源目录都从 `settings` 推导。
struct Runtime {
    settings: Settings,
    source_dir: PathBuf,
    core: Core,
    /// 启动时 `config.toml` 读坏的原因；保存成功后重建即清掉。
    config_error: Option<String>,
}

impl Runtime {
    fn new(home: &Path, settings: Settings, config_error: Option<String>) -> Self {
        let source_dir = settings.resolved_source_dir(home);
        Runtime {
            core: Core::new(home, &source_dir, settings.core_config()),
            source_dir,
            settings,
            config_error,
        }
    }
}

pub struct AppState {
    home: PathBuf,
    runtime: Mutex<Runtime>,
    /// 源目录 `home/` 的监视器；drop 即停止。换源目录时在 `watch` 里整体重建。
    watcher: Mutex<Option<notify::RecommendedWatcher>>,
}

impl AppState {
    /// 读设置、解析源目录、构造 Core。
    ///
    /// `config.toml` 坏了不阻塞启动：退回默认设置，把错误留在 `Runtime` 里给设置面板显示
    /// （文件保持原样，不被覆盖）。
    fn from_environment() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .expect("HOME 未设置，无法定位 $HOME");
        let (settings, config_error) = match Settings::load_or_create(&config_path(&home)) {
            Ok(settings) => (settings, None),
            Err(e) => {
                eprintln!("config-sync: {e}；本次用默认设置启动");
                (Settings::default(), Some(e))
            }
        };
        AppState {
            runtime: Mutex::new(Runtime::new(&home, settings, config_error)),
            watcher: Mutex::new(None),
            home,
        }
    }

    fn runtime(&self) -> MutexGuard<'_, Runtime> {
        self.runtime.lock().expect("设置状态被毒化")
    }

    /// 按当前源目录（重新）建立文件监视；监视不起来不阻塞启动，只是不能自动刷新。
    fn watch(&self, app: &AppHandle) {
        let source_dir = self.runtime().source_dir.clone();
        let watcher = match watch::start(app.clone(), &source_dir) {
            Ok(watcher) => Some(watcher),
            Err(e) => {
                eprintln!("config-sync: {e}；清单不会随编辑器保存自动刷新");
                None
            }
        };
        *self.watcher.lock().expect("监视状态被毒化") = watcher;
    }
}

/// `~/Library/Application Support/config-sync/config.toml`。
fn config_path(home: &Path) -> PathBuf {
    home.join("Library/Application Support/config-sync/config.toml")
}

#[tauri::command]
fn inventory(state: State<'_, AppState>) -> Result<Vec<InventoryEntry>, CoreError> {
    state.runtime().core.inventory()
}

#[tauri::command]
fn repo_status(state: State<'_, AppState>) -> Result<RepoStatus, CoreError> {
    state.runtime().core.repo_status()
}

/// 点开一行时的 unified diff；比较哪两侧由 Core 按该行状态决定。
#[tauri::command]
fn diff(state: State<'_, AppState>, target: String) -> Result<FileDiff, CoreError> {
    state.runtime().core.diff(Path::new(&target))
}

/// 源目录里是否已经有仓库：false 时前端进「输入远端地址」。
#[tauri::command]
fn has_repo(state: State<'_, AppState>) -> bool {
    state.runtime().core.has_repo()
}

/// 「编辑」一行：打开源目录里的副本，拉起即返回；保存后的刷新靠 fs watch。
#[tauri::command]
fn edit(state: State<'_, AppState>, target: String) -> Result<(), CoreError> {
    state.runtime().core.edit(Path::new(&target))
}

#[tauri::command]
fn source_dir(state: State<'_, AppState>) -> String {
    state.runtime().source_dir.display().to_string()
}

#[tauri::command]
fn clone_remote(state: State<'_, AppState>, remote: String) -> Result<(), CoreError> {
    state.runtime().core.clone(&remote)
}

/// 把路径（文件或整棵子树）收进源目录；每条路径各自成败，逐条返回。
#[tauri::command]
fn add(state: State<'_, AppState>, paths: Vec<PathBuf>) -> Result<Vec<AddOutcome>, CoreError> {
    state.runtime().core.add(&paths)
}

#[derive(Serialize)]
struct SettingsView {
    settings: Settings,
    /// 启动时读 `config.toml` 失败的原因（人话），没坏时为 null。
    config_error: Option<String>,
    /// 源目录里 `origin` 的地址；还不是仓库或没配 origin 时为 null。
    remote_url: Option<String>,
}

/// 设置面板要显示的全部内容：三字段 + 当前远端地址 + 配置读坏的原因。
#[tauri::command]
fn settings_view(state: State<'_, AppState>) -> SettingsView {
    let runtime = state.runtime();
    SettingsView {
        settings: runtime.settings.clone(),
        config_error: runtime.config_error.clone(),
        remote_url: runtime.core.remote_url().ok(),
    }
}

/// 校验 → 落盘 → 用新设置重建 Core。只写 `config.toml`，不做任何 git 操作。
#[tauri::command]
fn save_settings(
    state: State<'_, AppState>,
    app: AppHandle,
    settings: Settings,
) -> Result<(), CoreError> {
    let io = |message: String| CoreError { kind: ErrorKind::Io, message, stderr: None };
    let settings = settings.trimmed();
    settings.validate(&state.home).map_err(io)?;
    settings.save(&config_path(&state.home)).map_err(io)?;
    *state.runtime() = Runtime::new(&state.home, settings, None);
    // 源目录可能换了个地方，监视器跟着换。
    state.watch(&app);
    Ok(())
}

/// 拉取：`fetch` + `merge --ff-only`，只更新源目录，不动任何目标文件。
#[tauri::command]
fn pull(state: State<'_, AppState>) -> Result<(), CoreError> {
    state.runtime().core.pull()
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::from_environment())
        .setup(|app| {
            app.state::<AppState>().watch(app.handle());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            inventory,
            repo_status,
            diff,
            edit,
            has_repo,
            source_dir,
            clone_remote,
            add,
            pull,
            settings_view,
            save_settings
        ])
        .run(tauri::generate_context!())
        .expect("config-sync 启动失败");
}
