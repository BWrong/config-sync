//! 源目录的 fs watch：编辑器保存后让前端自己重读清单，界面上就没有「刷新」按钮。
//!
//! 只监视源目录 `home/` 子树（受管副本），**永远不写** `$HOME` 里的目标文件。
//! 事件合并了才发一次 `inventory-changed`，前端收到就重跑 `inventory` + `repo_status`。

use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tauri::{AppHandle, Emitter};

/// 前端监听的事件名。
pub const EVENT: &str = "inventory-changed";

/// 去抖窗口：一次保存 FSEvents 会连报好几条（写临时文件、改名、改属性），
/// 300ms 内的连续事件合并成一次刷新。
const DEBOUNCE: Duration = Duration::from_millis(300);

/// 开始监视 `source_dir/home`（递归）。
///
/// `home/` 还不存在时退而监视整个源目录：首个受管文件落地那次也要刷新。
/// 返回的 watcher 必须一直被持有，drop 即停止监视。
pub fn start(app: AppHandle, source_dir: &Path) -> Result<RecommendedWatcher, String> {
    let home = source_dir.join("home");
    let dir = if home.is_dir() { home } else { source_dir.to_path_buf() };

    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
        if result.is_ok() {
            let _ = tx.send(());
        }
    })
    .map_err(|e| format!("创建文件监视失败：{e}"))?;
    watcher
        .watch(&dir, RecursiveMode::Recursive)
        .map_err(|e| format!("监视 {} 失败：{e}", dir.display()))?;

    std::thread::spawn(move || {
        while rx.recv().is_ok() {
            while rx.recv_timeout(DEBOUNCE).is_ok() {}
            let _ = app.emit(EVENT, ());
        }
    });

    Ok(watcher)
}
