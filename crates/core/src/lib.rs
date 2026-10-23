//! config-sync 的 Core：全部能力的唯一对外表面。
//!
//! 构造时注入所有环境事实（home、source_dir、config），内部不读环境变量、不读 `$HOME`。
//! `#[tauri::command]` 只做转发，逻辑都在这里。

mod config;
mod error;
mod git;
mod inventory;
mod paths;

pub use config::Config;
pub use error::{CoreError, ErrorKind};
pub use inventory::{InventoryEntry, RepoStatus, Status};

use std::path::PathBuf;

pub struct Core {
    home: PathBuf,
    source_dir: PathBuf,
    #[allow(dead_code)] // 后续票据（设置、编辑）读它；本票不需要。
    config: Config,
}

impl Core {
    /// `home` 是镜像布局的根（目标文件根），`source_dir` 是 git 工作区。
    pub fn new(home: impl Into<PathBuf>, source_dir: impl Into<PathBuf>, config: Config) -> Self {
        Core { home: home.into(), source_dir: source_dir.into(), config }
    }

    /// 受管文件的完整列表。完全没有受管文件时返回空集合（不是错误）。
    pub fn inventory(&self) -> Result<Vec<InventoryEntry>, CoreError> {
        let mirror_root = self.source_dir.join(paths::MIRROR_ROOT);
        let mut entries = Vec::new();

        for rel in paths::managed_relative_paths(&mirror_root)? {
            let source = mirror_root.join(&rel);
            let target = self.home.join(&rel);
            let status = match std::fs::read(&target) {
                Ok(target_bytes) => {
                    let source_bytes = std::fs::read(&source).map_err(|e| {
                        CoreError::io(format!("读取 {} 失败：{e}", source.display()))
                    })?;
                    if source_bytes == target_bytes { Status::InSync } else { Status::Pending }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Status::TargetMissing,
                Err(e) => {
                    return Err(CoreError::io(format!("读取 {} 失败：{e}", target.display())))
                }
            };
            entries.push(InventoryEntry { source, target, status });
        }

        Ok(entries)
    }

    /// 顶部状态条：分支、ahead/behind、未提交改动数。
    pub fn repo_status(&self) -> Result<RepoStatus, CoreError> {
        let branch = git::run(&self.source_dir, &["branch", "--show-current"])?;
        if branch.is_empty() {
            return Err(CoreError {
                kind: ErrorKind::Git,
                message: "源目录当前不在任何分支上（detached HEAD）".to_string(),
                stderr: None,
            });
        }

        let dirty = git::run(&self.source_dir, &["status", "--porcelain"])?
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count() as u32;

        // 没有上游时 ahead/behind 无意义，按 0 处理（首个远端由后续票据建立）。
        let (behind, ahead) = match git::run(
            &self.source_dir,
            &["rev-list", "--left-right", "--count", "@{u}...HEAD"],
        ) {
            Ok(counts) => parse_ahead_behind(&counts),
            Err(_) => (0, 0),
        };

        Ok(RepoStatus { branch, ahead, behind, dirty })
    }
}

/// `git rev-list --left-right --count @{u}...HEAD` 的输出是 `<behind>\t<ahead>`。
fn parse_ahead_behind(counts: &str) -> (u32, u32) {
    let mut parts = counts.split_whitespace().map(|n| n.parse::<u32>().unwrap_or(0));
    let behind = parts.next().unwrap_or(0);
    let ahead = parts.next().unwrap_or(0);
    (behind, ahead)
}
