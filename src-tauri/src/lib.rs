//! config-sync 的 Tauri 2 外壳。
//!
//! 这里的 `#[tauri::command]` 只做转发：所有逻辑在 `config-sync-core` 里。
//! 环境事实（`$HOME`、`config.toml`）由外壳读取后注入 Core，Core 自己绝不读。

mod onboarding;
mod settings;
mod watch;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};

use config_sync_core::{
    AddOutcome, ApplyOutcome, ApplyPlan, ApplySummary, BackupSummary, Candidate, Core, CoreError,
    ErrorKind, FileDiff, PushOutcome, Refresh, RemoveOutcome, RestoreOutcome, RestorePlan,
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
    /// 当前配置快照。`save_settings` 整体换掉它，其余命令只在锁里取一份 `Arc` 就走。
    ///
    /// 长任务（push / apply / clone）**不**持这把锁，所以刷新不会被它们排住（#31）。
    runtime: RwLock<Arc<Runtime>>,
    /// 写命令之间的互斥门闩：apply / push / clone / … 全程持有，读命令一律不碰。
    ///
    /// 写命令改的是磁盘上的仓库，`Runtime` 自己（路径 + 配置）在构造后不再变，
    /// 所以「写与写互斥」得单独一把锁，而不是把 `runtime` 的写锁当门闩用。
    write_gate: Mutex<()>,
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
            runtime: RwLock::new(Arc::new(Runtime::new(&home, settings, config_error))),
            write_gate: Mutex::new(()),
            watcher: Mutex::new(None),
            home,
        }
    }

    /// 当前配置快照：锁只在这几纳秒里被持着，拿到 `Arc` 就走。
    ///
    /// 中毒说明有命令持锁时 panic 过。这里留一行日志、拿回最后写入的内容继续，
    /// 不让编程错误悄悄退化成「没有仓库」（#31）。
    fn runtime(&self) -> Arc<Runtime> {
        match self.runtime.read() {
            Ok(runtime) => Arc::clone(&runtime),
            Err(poisoned) => {
                eprintln!(
                    "config-sync: 配置快照锁被毒化（有命令持锁时 panic），先按最后写入的配置继续"
                );
                Arc::clone(&poisoned.into_inner())
            }
        }
    }

    /// 写命令的互斥门闩：guard 得活在命令的整个作用域里（`let _gate = …`）。
    fn write_gate(&self) -> MutexGuard<'_, ()> {
        match self.write_gate.lock() {
            Ok(gate) => gate,
            Err(poisoned) => {
                eprintln!("config-sync: 写命令门闩被毒化（有一条写命令 panic 过），仍然串行继续");
                poisoned.into_inner()
            }
        }
    }

    /// 写命令的统一入口：先拿互斥门闩，再取配置快照，整段活儿都在门闩里跑。
    ///
    /// 读命令**不**走这里：它们只取一份快照（`runtime()`），不会被长任务排住（#31）。
    ///
    /// 「写」不止是改源目录里的仓库：`edit` 也走这里——它要么拉起编辑器（默认 `open`，
    /// 或者 `editor_command` 的编辑器分支 `spawn` 即返回，持锁极短），要么（`open` 等退出码
    /// 那支 / 需要回退时）等几十毫秒的进程退出。它动的是目标文件，跟 apply 落在同一批路径上，
    /// 所以同样排进写队列，而不是当读命令放行。
    fn write<T>(&self, work: impl FnOnce(&WriteScope<'_>) -> T) -> T {
        let scope = WriteScope {
            state: self,
            _gate: self.write_gate(),
        };
        work(&scope)
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

/// 握着写门闩的句柄：拿到它才说明这条命令排在写队列里。
///
/// 存在的理由是 `set_runtime`：换配置快照会换掉整份 `Runtime`，必须与写命令串行。
/// 把它做成只有 `write()` 造得出来的句柄上的方法，而不是 `AppState` 上的一个普通 `fn`，
/// 将来就没法绕过门闩直接换（#34）。
struct WriteScope<'a> {
    state: &'a AppState,
    _gate: MutexGuard<'a, ()>,
}

impl WriteScope<'_> {
    fn runtime(&self) -> Arc<Runtime> {
        self.state.runtime()
    }

    /// 换配置快照（只有 `save_settings` 用）。
    fn set_runtime(&self, runtime: Runtime) {
        match self.state.runtime.write() {
            Ok(mut slot) => *slot = Arc::new(runtime),
            Err(poisoned) => {
                eprintln!("config-sync: 配置快照锁被毒化（有命令持锁时 panic），仍然换上新配置");
                *poisoned.into_inner() = Arc::new(runtime);
            }
        }
    }
}

/// `~/.config/config-sync/config.toml`。
fn config_path(home: &Path) -> PathBuf {
    settings::app_dir(home).join("config.toml")
}

/// 把命令里的阻塞工作挪到 tokio 的 blocking 池（#18）。
///
/// Tauri 的同步命令跑在主线程，core 里一个 `git` 子进程或一次大文件比较就是整窗冻结。
/// 这里让主线程只等一个 `JoinHandle`：git 与磁盘的等待都在别的线程上。锁粒度见
/// `AppState`：读命令只取一份快照，写命令另有一把互斥门闩（#31）。
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
            .write(|scope| scope.runtime().core.edit(Path::new(&target)))
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
        state.write(|scope| scope.runtime().core.clone(&remote))?;
        // 克隆出来的 `home/` 现在才存在：重建监视器，别再盯着含 `.git` 的整棵树。
        state.watch(&app);
        Ok(())
    })
    .await
}

/// 写入或替换源目录仓库的 origin 地址。只动 git 配置，不提交、不写 $HOME。
#[tauri::command]
async fn set_remote(app: AppHandle, remote: String) -> Result<(), CoreError> {
    blocking(move || {
        app.state::<AppState>()
            .write(|scope| scope.runtime().core.set_remote(&remote))
    })
    .await
}

/// 把路径（文件或整棵子树）收进源目录；每条路径各自成败，逐条返回。
#[tauri::command]
async fn add(app: AppHandle, paths: Vec<PathBuf>) -> Result<Vec<AddOutcome>, CoreError> {
    blocking(move || {
        app.state::<AppState>()
            .write(|scope| scope.runtime().core.add(&paths))
    })
    .await
}

/// 把路径（文件或整棵子树）从源目录移除；只动源目录里的副本，不碰 `$HOME` 原文件。
#[tauri::command]
async fn remove(app: AppHandle, targets: Vec<PathBuf>) -> Result<Vec<RemoveOutcome>, CoreError> {
    blocking(move || {
        app.state::<AppState>()
            .write(|scope| scope.runtime().core.remove(&targets))
    })
    .await
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
    blocking(move || {
        app.state::<AppState>()
            .write(|scope| scope.runtime().core.execute_apply(&plan))
    })
    .await
}

/// 现有备份（新的在前）：应用结果页拿最新一份做「撤销这次应用」。
#[tauri::command]
async fn list_backups(app: AppHandle) -> Result<Vec<BackupSummary>, CoreError> {
    blocking(move || app.state::<AppState>().runtime().core.list_backups()).await
}

/// 撤销第一步：出一份恢复计划 + diff 预览（走 `#27` 的同一套闸门），一个字节都不写。
#[tauri::command]
async fn plan_restore_backup(app: AppHandle, id: String) -> Result<RestorePlan, CoreError> {
    blocking(move || {
        app.state::<AppState>()
            .runtime()
            .core
            .plan_restore_backup(&id)
    })
    .await
}

/// 撤销第二步：执行预览里那一份计划，不重新判断动作。
#[tauri::command]
async fn restore_backup(
    app: AppHandle,
    plan: RestorePlan,
) -> Result<Vec<RestoreOutcome>, CoreError> {
    blocking(move || {
        app.state::<AppState>()
            .write(|scope| scope.runtime().core.restore_backup(&plan))
    })
    .await
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
        // 换配置也是写操作：跟 apply / push 排在同一个队列里，不跟它们抢源目录。
        state.write(|scope| {
            let io = |message: String| CoreError {
                kind: ErrorKind::Io,
                message,
                stderr: None,
            };
            let settings = settings.trimmed();
            settings.validate(&state.home).map_err(io)?;
            settings.save(&config_path(&state.home)).map_err(io)?;
            scope.set_runtime(Runtime::new(&state.home, settings, None));
            // 源目录可能换了个地方，监视器跟着换。
            state.watch(&app);
            Ok(())
        })
    })
    .await
}

/// 拉取：`fetch` + `merge --ff-only`，只更新源目录，不动任何目标文件。
#[tauri::command]
async fn pull(app: AppHandle) -> Result<(), CoreError> {
    blocking(move || {
        app.state::<AppState>()
            .write(|scope| scope.runtime().core.pull())
    })
    .await
}

/// 推送：硬拦 → `git add -A` → 自动提交 → `git push`，全在 Core 的一个方法里。
#[tauri::command]
async fn commit_and_push(app: AppHandle) -> Result<PushOutcome, CoreError> {
    blocking(move || {
        app.state::<AppState>()
            .write(|scope| scope.runtime().core.commit_and_push())
    })
    .await
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
        app.state::<AppState>().write(|scope| {
            scope
                .runtime()
                .core
                .resolve_conflict(Path::new(&target), side)
        })
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
            list_backups,
            plan_restore_backup,
            restore_backup,
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
    use super::{AppState, Runtime, blocking};
    use crate::settings::Settings;
    use std::path::Path;
    use std::sync::{Arc, Mutex, RwLock};
    use std::thread::ThreadId;
    use std::time::{Duration, Instant};

    /// 直接构造 `AppState`（字段都是本模块的），不再读 `$HOME`。
    fn app_state(root: &Path, source_dir: &Path) -> AppState {
        let settings = Settings {
            source_dir: source_dir.display().to_string(),
            ..Settings::default()
        };
        AppState {
            home: root.to_path_buf(),
            runtime: RwLock::new(Arc::new(Runtime::new(root, settings, None))),
            write_gate: Mutex::new(()),
            watcher: Mutex::new(None),
        }
    }

    fn git_ok(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("跑不起系统 git");
        assert!(
            out.status.success(),
            "git {args:?} 失败：{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// 一个最小的 `AppState`：源目录是真 git 仓库（`refresh` 要跑 `git status`）。
    fn state_with_repo(root: &Path) -> AppState {
        let source = root.join("source");
        std::fs::create_dir_all(source.join("home")).unwrap();
        git_ok(&source, &["init", "-q", "-b", "main"]);
        git_ok(
            &source,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "init",
            ],
        );
        app_state(root, &source)
    }

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

    /// #31 验收 1：写命令持着门闩 400ms 期间，`refresh()` 必须立刻返回。
    ///
    /// 改前（所有命令共用一把 `Mutex<Runtime>`）这里是 391.1ms：读命令排在整段写命令
    /// 后面。现在读命令只取一份快照，门闩完全不碰它。
    #[test]
    fn refresh_returns_while_a_long_write_holds_the_gate() {
        let root = tempfile::tempdir().unwrap();
        let state = Arc::new(state_with_repo(root.path()));

        let writer = {
            let state = Arc::clone(&state);
            std::thread::spawn(move || {
                // 写命令就是这条路径：`write()` 里拿门闩 + 取快照，然后在门闩里干长活儿。
                state.write(|_| std::thread::sleep(Duration::from_millis(400)))
            })
        };
        // 让写命令先开跑，模拟「推送已经在跑」的那一刻。
        std::thread::sleep(Duration::from_millis(50));

        let started = Instant::now();
        let refreshed = state.runtime().core.refresh();
        let waited = started.elapsed();

        writer.join().unwrap();
        assert!(refreshed.is_ok(), "refresh 失败：{:?}", refreshed.err());
        assert!(
            waited < Duration::from_millis(100),
            "刷新等了 {waited:?}，仍被写命令排住"
        );
    }

    /// #31 验收 2：两个写命令并发时严格互斥——第二个必须等第一个放掉门闩。
    ///
    /// `apply` / `push` / `clone` / `save_settings` 全都走 `write()`，所以这里验的就是
    /// 它们共用的那条路。
    #[test]
    fn two_writers_are_mutually_exclusive() {
        let root = tempfile::tempdir().unwrap();
        let state = Arc::new(state_with_repo(root.path()));
        let epoch = Instant::now();
        // 每段写命令的 [开始, 结束]；两张表的先后由开始时刻决定。
        let spans: Arc<Mutex<Vec<(Duration, Duration)>>> = Arc::new(Mutex::new(Vec::new()));

        let mut handles = Vec::new();
        for hold in [Duration::from_millis(200), Duration::from_millis(100)] {
            let state = Arc::clone(&state);
            let spans = Arc::clone(&spans);
            handles.push(std::thread::spawn(move || {
                state.write(|_| {
                    let start = epoch.elapsed();
                    std::thread::sleep(hold);
                    spans.lock().unwrap().push((start, epoch.elapsed()));
                })
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }

        let mut spans = spans.lock().unwrap().clone();
        spans.sort_by_key(|(start, _)| *start);
        let [(_, first_end), (second_start, _)] = spans[..] else {
            panic!("两段写命令没都跑完：{spans:?}");
        };
        assert!(
            second_start >= first_end,
            "第二个写命令在第一段还没放门闩时就开始跑了：{spans:?}"
        );
    }

    /// #31 验收 3：锁中毒是编程错误，必须留日志并按最后写入的状态继续，
    /// 而不是被 `blocking_or(false, …)` 静默吞成「没有仓库」。
    #[test]
    fn poisoned_snapshot_lock_still_serves_the_real_state() {
        let root = tempfile::tempdir().unwrap();
        let state = Arc::new(state_with_repo(root.path()));

        // 让一条「命令」在持锁时 panic：这就是 std 锁中毒的来源。
        let poisoner = {
            let state = Arc::clone(&state);
            std::thread::spawn(move || {
                let _guard = state.runtime.write().unwrap();
                panic!("模拟命令持锁时 panic");
            })
        };
        assert!(poisoner.join().is_err());
        assert!(
            state.runtime.is_poisoned(),
            "锁没被毒化，这条断言就失去意义了"
        );

        // 读路径不受影响：拿回的是最后写入的那份配置，不是空 / 默认值。
        assert_eq!(
            state.runtime().source_dir,
            root.path().join("source"),
            "中毒后读到了错的配置"
        );
        // 写路径照旧串行，且能把新配置写进去。
        state
            .write(|scope| scope.set_runtime(Runtime::new(root.path(), Settings::default(), None)));
        assert_eq!(
            state.runtime().settings,
            Settings::default(),
            "中毒后写不进新配置"
        );
    }

    /// #34 验收：写门闩中毒后写命令仍严格互斥，读命令照常取快照。
    ///
    /// 中毒只有一种恢复方式是对的：`into_inner()` 拿回**同一把**锁，后面照旧排队。
    /// 「毒了就放行」（换一把没人持的锁 / 干脆不锁）会让两段写命令同时跑，
    /// 下面 `spans` 就会相交 → 红。改这段恢复逻辑时请反向验证一次。
    #[test]
    fn poisoned_write_gate_still_serialises_writers() {
        let root = tempfile::tempdir().unwrap();
        let state = Arc::new(state_with_repo(root.path()));

        // 一条写命令持门闩时 panic：这就是门闩中毒的来源。
        let poisoner = {
            let state = Arc::clone(&state);
            std::thread::spawn(move || {
                let _gate = state.write_gate();
                panic!("模拟写命令持门闩时 panic");
            })
        };
        assert!(poisoner.join().is_err());
        assert!(
            state.write_gate.is_poisoned(),
            "门闩没被毒化，这条断言就失去意义了"
        );

        // 读命令不受影响：取快照 + 真跑一次 `git status`。
        assert_eq!(
            state.runtime().source_dir,
            root.path().join("source"),
            "中毒后读到了错的配置"
        );
        assert!(
            state.runtime().core.refresh().is_ok(),
            "中毒后读命令跑不起来"
        );

        // 两段写命令并发：门闩中毒也得让它们排队。
        let epoch = Instant::now();
        let spans: Arc<Mutex<Vec<(Duration, Duration)>>> = Arc::new(Mutex::new(Vec::new()));
        let mut handles = Vec::new();
        for hold in [Duration::from_millis(200), Duration::from_millis(100)] {
            let state = Arc::clone(&state);
            let spans = Arc::clone(&spans);
            handles.push(std::thread::spawn(move || {
                state.write(|_| {
                    let start = epoch.elapsed();
                    std::thread::sleep(hold);
                    spans.lock().unwrap().push((start, epoch.elapsed()));
                })
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }

        let mut spans = spans.lock().unwrap().clone();
        assert_eq!(spans.len(), 2, "两段写命令没都跑完：{spans:?}");
        spans.sort_by_key(|(start, _)| *start);
        let [(_, first_end), (second_start, _)] = spans[..] else {
            unreachable!("刚断言过是两段");
        };
        println!("门闩中毒后两段写命令的区间：{spans:?}");
        assert!(
            second_start >= first_end,
            "门闩中毒后两段写命令重叠了：{spans:?}"
        );
    }

    /// #31 验收 4 的 release 实测：大仓库推送期间 `refresh()` 的耗时。
    ///
    /// 手动跑（要造几千个文件、推一次真仓库）：
    /// `cargo test --release -p config-sync-app bench_refresh_during_push -- --ignored --nocapture`
    ///
    /// 只打印数字，不断言墙钟阈值：机器快慢不该让这条测试红。
    #[test]
    #[ignore = "release 实测，手动跑"]
    fn bench_refresh_during_push() {
        const FILES: usize = 10000;
        let root = tempfile::tempdir().unwrap();
        let remote = root.path().join("remote.git");
        let source = root.path().join("source");
        std::fs::create_dir_all(&source).unwrap();
        git_ok(
            root.path(),
            &["init", "--bare", "-q", remote.to_str().unwrap()],
        );
        git_ok(&source, &["init", "-q", "-b", "main"]);
        git_ok(&source, &["config", "user.name", "bench"]);
        git_ok(&source, &["config", "user.email", "bench@example.com"]);
        for i in 0..FILES {
            let dir = source.join(format!("home/.config/bench/d{}", i % 50));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join(format!("f{i}.conf")),
                format!("key{i} = value{i}\n").repeat(8),
            )
            .unwrap();
        }
        git_ok(&source, &["add", "-A"]);
        git_ok(&source, &["commit", "-q", "-m", "init"]);
        git_ok(
            &source,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git_ok(&source, &["push", "-q", "-u", "origin", "main"]);

        let state = Arc::new(app_state(root.path(), &source));
        let idle = median_refresh(&state, 5);

        // 改一批文件，让这次推送真有活可干（commit + push 3000 个文件）。
        for i in 0..FILES {
            let dir = source.join(format!("home/.config/bench/d{}", i % 50));
            std::fs::write(
                dir.join(format!("f{i}.conf")),
                format!("key{i} = value{i} (updated)\n").repeat(8),
            )
            .unwrap();
        }

        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writer = {
            let state = Arc::clone(&state);
            let done = Arc::clone(&done);
            std::thread::spawn(move || {
                let started = Instant::now();
                let outcome = state.write(|scope| scope.runtime().core.commit_and_push());
                let took = started.elapsed();
                done.store(true, std::sync::atomic::Ordering::SeqCst);
                (outcome, took)
            })
        };

        std::thread::sleep(Duration::from_millis(20));
        let mut samples = Vec::new();
        while !done.load(std::sync::atomic::Ordering::SeqCst) {
            let started = Instant::now();
            let outcome = state.runtime().core.refresh();
            samples.push(started.elapsed());
            assert!(outcome.is_ok(), "refresh 失败：{:?}", outcome.err());
            std::thread::sleep(Duration::from_millis(10));
        }
        let (pushed, push_took) = writer.join().unwrap();
        assert!(pushed.is_ok(), "推送失败：{:?}", pushed.err());

        samples.sort();
        let median = samples[samples.len() / 2];
        let slow = samples
            .iter()
            .filter(|s| **s > Duration::from_millis(100))
            .count();
        println!(
            "\n#31 bench：{FILES} 个文件 / {:.1} KB",
            dir_size(&source.join("home")) as f64 / 1024.0
        );
        println!("推送本身 {push_took:?}；空闲 refresh 中位数 {idle:?}");
        println!(
            "推送期间 refresh：采样 {} 次，中位数 {median:?}，最大 {:?}，>100ms 的 {slow} 次",
            samples.len(),
            samples.last().unwrap(),
        );
    }

    fn median_refresh(state: &AppState, rounds: usize) -> Duration {
        let mut samples: Vec<Duration> = (0..rounds)
            .map(|_| {
                let started = Instant::now();
                state.runtime().core.refresh().unwrap();
                started.elapsed()
            })
            .collect();
        samples.sort();
        samples[samples.len() / 2]
    }

    fn dir_size(dir: &Path) -> u64 {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return 0;
        };
        entries
            .map(|entry| {
                let entry = entry.unwrap();
                match entry.file_type().unwrap().is_dir() {
                    true => dir_size(&entry.path()),
                    false => entry.metadata().unwrap().len(),
                }
            })
            .sum()
    }
}
