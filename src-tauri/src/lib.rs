//! config-sync 的 Tauri 2 外壳。
//!
//! 这里的 `#[tauri::command]` 只做转发：所有逻辑在 `config-sync-core` 里。
//! 环境事实（`$HOME`、`config.toml`）由外壳读取后注入 Core，Core 自己绝不读。

mod onboarding;
mod settings;
mod watch;

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use config_sync_core::{
    AddOutcome, ApplyOutcome, ApplyPlan, ApplySummary, Candidate, Core, CoreError, ErrorKind,
    FileDiff, PushOutcome, Refresh, RemoveOutcome,
};
use serde::Serialize;
use settings::Settings;
use tauri::{AppHandle, Manager};

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

/// `~/.config/config-sync/config.toml`。
fn config_path(home: &Path) -> PathBuf {
    settings::app_dir(home).join("config.toml")
}

/// 把命令里的阻塞工作挪到 tokio 的 blocking 池（#18）。
///
/// Tauri 的同步命令跑在主线程，core 里一个 `git` 子进程或一次大文件比较就是整窗冻结。
/// 这里让主线程只等一个 `JoinHandle`：git 与磁盘的等待都在别的线程上。命令体本身
/// 一行没改（锁粒度也没动，仍是一把 `Mutex<Runtime>`），变的只是它跑在哪条线程上。
async fn blocking<T, F>(work: F) -> Result<T, CoreError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, CoreError> + Send + 'static,
{
    match tauri::async_runtime::spawn_blocking(work).await {
        Ok(result) => result,
        // 后台线程 panic（含设置锁被毒化）才会到这里：报成 IO 错误而不是再 panic 一次。
        Err(e) => Err(CoreError {
            kind: ErrorKind::Io,
            message: format!("后台任务异常结束：{e}"),
            stderr: None,
        }),
    }
}

/// 同上，给没有 `Result` 返回值的命令用：后台 panic 时给回退值（这些命令本身不会失败）。
///
/// 没有错误通道，回退值会把编程错误伪装成「空状态」（`AppState` 未注册、锁中毒
/// 都会变成「没有仓库」/「源目录为空」，用户直接看到引导页），所以至少留一行日志。
async fn blocking_or<T, F>(what: &'static str, fallback: T, work: F) -> T
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    match tauri::async_runtime::spawn_blocking(work).await {
        Ok(value) => value,
        Err(e) => {
            eprintln!("config-sync: {what} 的后台任务异常结束（{e}），先用回退值继续");
            fallback
        }
    }
}

/// 一次刷新：清单 + 状态条。两半出自同一份 git 事实，所以只跑一次 `git status`（#19）。
#[tauri::command]
async fn refresh(app: AppHandle) -> Result<Refresh, CoreError> {
    blocking(move || app.state::<AppState>().runtime().core.refresh()).await
}

/// 点开一行时的 unified diff；比较哪两侧由 Core 按该行状态决定。
#[tauri::command]
async fn diff(app: AppHandle, target: String) -> Result<FileDiff, CoreError> {
    blocking(move || {
        app.state::<AppState>()
            .runtime()
            .core
            .diff(Path::new(&target))
    })
    .await
}

/// 源目录里是否已经有仓库：false 时前端进「输入远端地址」。
#[tauri::command]
async fn has_repo(app: AppHandle) -> bool {
    blocking_or("has_repo", false, move || {
        app.state::<AppState>().runtime().core.has_repo()
    })
    .await
}

/// 「编辑」一行：打开源目录里的副本，拉起即返回；保存后的刷新靠 fs watch。
#[tauri::command]
async fn edit(app: AppHandle, target: String) -> Result<(), CoreError> {
    blocking(move || {
        app.state::<AppState>()
            .runtime()
            .core
            .edit(Path::new(&target))
    })
    .await
}

#[tauri::command]
async fn source_dir(app: AppHandle) -> String {
    blocking_or("source_dir", String::new(), move || {
        app.state::<AppState>()
            .runtime()
            .source_dir
            .display()
            .to_string()
    })
    .await
}

/// 在 Finder 里打开源目录（顶栏的「打开目录」按钮）。
#[tauri::command]
async fn open_source_dir(app: AppHandle) -> Result<(), CoreError> {
    blocking(move || app.state::<AppState>().runtime().core.open_source_dir()).await
}

#[tauri::command]
async fn clone_remote(app: AppHandle, remote: String) -> Result<(), CoreError> {
    blocking(move || {
        let state = app.state::<AppState>();
        // 语句边界是必需的：这把 guard 必须在 `watch()` 之前释放。`watch()` 内部要取
        // 同一把 `runtime()` 锁，`std::sync::Mutex` 不可重入，拿着 guard 调用就自死锁。
        state.runtime().core.clone(&remote)?;
        // 克隆出来的 `home/` 现在才存在：重建监视器，别再盯着含 `.git` 的整棵树。
        state.watch(&app);
        Ok(())
    })
    .await
}

/// 写入或替换源目录仓库的 origin 地址。只动 git 配置，不提交、不写 $HOME。
#[tauri::command]
async fn set_remote(app: AppHandle, remote: String) -> Result<(), CoreError> {
    blocking(move || app.state::<AppState>().runtime().core.set_remote(&remote)).await
}

/// 把路径（文件或整棵子树）收进源目录；每条路径各自成败，逐条返回。
#[tauri::command]
async fn add(app: AppHandle, paths: Vec<PathBuf>) -> Result<Vec<AddOutcome>, CoreError> {
    blocking(move || app.state::<AppState>().runtime().core.add(&paths)).await
}

/// 把路径（文件或整棵子树）从源目录移除；只动源目录里的副本，不碰 `$HOME` 原文件。
#[tauri::command]
async fn remove(app: AppHandle, targets: Vec<PathBuf>) -> Result<Vec<RemoveOutcome>, CoreError> {
    blocking(move || app.state::<AppState>().runtime().core.remove(&targets)).await
}

/// 首次使用向导的候选：固定的一组常见配置文件 + 本机是否存在；勾选后交给 `add`。
#[tauri::command]
async fn onboarding_candidates(app: AppHandle) -> Vec<Candidate> {
    blocking_or("onboarding_candidates", Vec::new(), move || {
        app.state::<AppState>()
            .runtime()
            .core
            .onboarding_candidates()
    })
    .await
}

/// 向导是否已经走过（勾选或跳过都算）；走过就不再出现。
/// 只 stat 一个标记文件、不取运行时锁，留在主线程也不会被慢命令挡住。
#[tauri::command]
fn onboarding_done(app: AppHandle) -> bool {
    onboarding::is_done(&app.state::<AppState>().home)
}

/// 标记向导已完成：只写 app 配置目录里的标记，不碰源目录，也不进 git。
#[tauri::command]
async fn finish_onboarding(app: AppHandle) -> Result<(), CoreError> {
    blocking(move || {
        onboarding::mark_done(&app.state::<AppState>().home).map_err(|message| CoreError {
            kind: ErrorKind::Io,
            message,
            stderr: None,
        })
    })
    .await
}

#[derive(Serialize)]
struct ApplyPreview {
    plan: ApplyPlan,
    /// 新建 / 覆盖 / 跳过各几项，界面直接显示这一行摘要。
    summary: ApplySummary,
}

/// 「应用」第一步：出一份完整计划（每项动作 + 原因 + diff），一个字节都不写。
#[tauri::command]
async fn plan_apply(
    app: AppHandle,
    targets: Option<Vec<PathBuf>>,
) -> Result<ApplyPreview, CoreError> {
    blocking(move || {
        let plan = app
            .state::<AppState>()
            .runtime()
            .core
            .plan_apply(targets.as_deref())?;
        let summary = plan.summary();
        Ok(ApplyPreview { plan, summary })
    })
    .await
}

/// 「应用」第二步：执行预览里那一份计划，不重新判断动作。
#[tauri::command]
async fn execute_apply(app: AppHandle, plan: ApplyPlan) -> Result<Vec<ApplyOutcome>, CoreError> {
    blocking(move || app.state::<AppState>().runtime().core.execute_apply(&plan)).await
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
async fn settings_view(app: AppHandle) -> Result<SettingsView, CoreError> {
    blocking(move || {
        let state = app.state::<AppState>();
        let runtime = state.runtime();
        Ok(SettingsView {
            settings: runtime.settings.clone(),
            config_error: runtime.config_error.clone(),
            remote_url: runtime.core.remote_url().ok(),
        })
    })
    .await
}

/// 校验 → 落盘 → 用新设置重建 Core。只写 `config.toml`，不做任何 git 操作。
#[tauri::command]
async fn save_settings(app: AppHandle, settings: Settings) -> Result<(), CoreError> {
    blocking(move || {
        let state = app.state::<AppState>();
        let io = |message: String| CoreError {
            kind: ErrorKind::Io,
            message,
            stderr: None,
        };
        let settings = settings.trimmed();
        settings.validate(&state.home).map_err(io)?;
        settings.save(&config_path(&state.home)).map_err(io)?;
        *state.runtime() = Runtime::new(&state.home, settings, None);
        // 上一条赋值是个完整语句，guard 在这一行结束时已经释放：`watch()` 内部要取
        // 同一把 `runtime()` 锁（不可重入），并成一句就自死锁。
        // 源目录可能换了个地方，监视器跟着换。
        state.watch(&app);
        Ok(())
    })
    .await
}

/// 拉取：`fetch` + `merge --ff-only`，只更新源目录，不动任何目标文件。
#[tauri::command]
async fn pull(app: AppHandle) -> Result<(), CoreError> {
    blocking(move || app.state::<AppState>().runtime().core.pull()).await
}

/// 推送：硬拦 → `git add -A` → 自动提交 → `git push`，全在 Core 的一个方法里。
#[tauri::command]
async fn commit_and_push(app: AppHandle) -> Result<PushOutcome, CoreError> {
    blocking(move || app.state::<AppState>().runtime().core.commit_and_push()).await
}

/// 冲突解决：按文件二选一（保留本机 / 采用远端），然后重新提交。
#[tauri::command]
async fn resolve_conflict(app: AppHandle, target: String, side: String) -> Result<(), CoreError> {
    let side = match side.as_str() {
        "ours" => config_sync_core::resolve::Side::Ours,
        "theirs" => config_sync_core::resolve::Side::Theirs,
        other => {
            return Err(CoreError {
                kind: ErrorKind::Io,
                message: format!("未知的解决方向：{other}"),
                stderr: None,
            });
        }
    };
    blocking(move || {
        app.state::<AppState>()
            .runtime()
            .core
            .resolve_conflict(Path::new(&target), side)
    })
    .await
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
            refresh,
            diff,
            edit,
            has_repo,
            source_dir,
            open_source_dir,
            clone_remote,
            set_remote,
            add,
            remove,
            plan_apply,
            execute_apply,
            onboarding_candidates,
            onboarding_done,
            finish_onboarding,
            pull,
            commit_and_push,
            resolve_conflict,
            settings_view,
            save_settings
        ])
        .run(tauri::generate_context!())
        .expect("config-sync 启动失败");
}

#[cfg(test)]
mod tests {
    use super::blocking;
    use std::sync::{Arc, Mutex};
    use std::thread::ThreadId;
    use std::time::{Duration, Instant};

    /// #18 的断言：长任务必须真的跑在另一条线程上，且不拖住其他命令。
    ///
    /// 两件事分开验，因为只看耗时拓不住「假异步」：
    ///
    /// - **线程 id**：`blocking()` 若被改成就地执行闭包（教科书式假异步），闭包会跑在
    ///   调用方（async worker）上，id 相同 → 红。不比线程名：`spawn_blocking` 的线程
    ///   也叫 `tokio-rt-worker`，与 async worker 同名。
    /// - **耗时**：真被挡住是 ≥400ms（慢任务整段长度），350ms 仍然能拓住，又不会
    ///   因为 CI 机器的抖动误报。
    #[test]
    fn a_slow_command_runs_off_thread_and_does_not_delay_another() {
        tauri::async_runtime::block_on(async {
            let caller = std::thread::current().id();
            let slow_thread: Arc<Mutex<Option<ThreadId>>> = Arc::new(Mutex::new(None));
            let fast_thread: Arc<Mutex<Option<ThreadId>>> = Arc::new(Mutex::new(None));

            let slow = tauri::async_runtime::spawn(blocking({
                let slot = Arc::clone(&slow_thread);
                move || {
                    *slot.lock().unwrap() = Some(std::thread::current().id());
                    std::thread::sleep(Duration::from_millis(400));
                    Ok("slow")
                }
            }));
            // 让慢任务先开跑，模拟「推送/预览已经在跑」的那一刻。
            std::thread::sleep(Duration::from_millis(50));

            let started = Instant::now();
            let fast = blocking({
                let slot = Arc::clone(&fast_thread);
                move || {
                    *slot.lock().unwrap() = Some(std::thread::current().id());
                    Ok("fast")
                }
            })
            .await
            .unwrap();
            let waited = started.elapsed();

            assert_eq!(fast, "fast");
            assert_eq!(slow.await.unwrap().unwrap(), "slow");

            assert_ne!(
                slow_thread.lock().unwrap().expect("慢任务的闭包没跑"),
                caller,
                "慢任务跑在调用方线程上了"
            );
            assert_ne!(
                fast_thread.lock().unwrap().expect("快命令的闭包没跑"),
                caller,
                "快命令跑在调用方线程上了（假异步）"
            );
            assert!(
                waited < Duration::from_millis(350),
                "快命令等了 {waited:?}，被慢任务挡住了"
            );
        });
    }
}
