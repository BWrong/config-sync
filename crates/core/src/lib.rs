//! config-sync 的 Core：全部能力的唯一对外表面。
//!
//! 构造时注入所有环境事实（home、source_dir、config），内部不读环境变量、不读 `$HOME`。
//! `#[tauri::command]` 只做转发，逻辑都在这里。

mod add;
mod config;
mod error;
mod git;
mod inventory;
mod paths;
mod state;

pub use add::{AddOutcome, AddResult};
pub use config::Config;
pub use error::{CoreError, ErrorKind};
pub use inventory::{FileDiff, InventoryEntry, RepoStatus, Status};

use std::path::{Path, PathBuf};

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

    /// 源目录里是否已经有一个 git 仓库。
    ///
    /// 首次启动时外壳用它决定是进地址输入还是直接进清单。
    pub fn has_repo(&self) -> bool {
        git::run(&self.source_dir, &["rev-parse", "--git-dir"]).is_ok()
    }

    /// 把远端 clone 到源目录。
    ///
    /// 只建源目录里的文件，绝不写任何目标文件：clone 完停在清单，由用户决定何时应用。
    /// 源目录非空时拒绝（避免把仓库 clone 进已有的东西里，也不需要覆盖确认）。
    pub fn clone(&self, remote_url: &str) -> Result<(), CoreError> {
        let remote = remote_url.trim();
        if remote.is_empty() {
            return Err(CoreError::io("请填写远端仓库地址"));
        }

        if self.source_dir.exists() {
            let mut entries = std::fs::read_dir(&self.source_dir).map_err(|e| {
                CoreError::io(format!("读取 {} 失败：{e}", self.source_dir.display()))
            })?;
            if entries.next().is_some() {
                return Err(CoreError::io(format!(
                    "源目录 {} 不是空的，不能 clone 到里面；请换个源目录，或先清空它",
                    self.source_dir.display()
                )));
            }
        } else {
            std::fs::create_dir_all(&self.source_dir).map_err(|e| {
                CoreError::io(format!("创建源目录 {} 失败：{e}", self.source_dir.display()))
            })?;
        }

        // `-C` 已经切进源目录，目标路径用 `.`。
        git::run(&self.source_dir, &["clone", remote, "."]).map_err(|e| CoreError {
            message: format!("从 {remote} clone 到 {} 失败", self.source_dir.display()),
            ..e
        })?;
        Ok(())
    }

    /// 源目录里 `origin` 的地址；设置面板用它显示"当前对着哪个远端"。
    pub fn remote_url(&self) -> Result<String, CoreError> {
        git::run(&self.source_dir, &["remote", "get-url", "origin"])
    }

    /// 受管文件的完整列表。完全没有受管文件时返回空集合（不是错误）。
    pub fn inventory(&self) -> Result<Vec<InventoryEntry>, CoreError> {
        let mirror_root = self.source_dir.join(paths::MIRROR_ROOT);
        let facts = state::GitFacts::read(&self.source_dir)?;
        let mut entries = Vec::new();

        for rel in paths::managed_relative_paths(&mirror_root)? {
            let source = mirror_root.join(&rel);
            let target = self.home.join(&rel);
            let status = state::derive(self.disk(rel.as_path())?, &repo_path(&rel), &facts);
            entries.push(InventoryEntry { source, target, status });
        }

        Ok(entries)
    }

    /// 一行的 unified diff。比较哪两侧由该行当前状态决定：
    ///
    /// | 状态 | 旧侧 | 新侧 |
    /// |---|---|---|
    /// | 待提交 / 冲突 | `HEAD:<路径>` 已提交的内容 | 源文件当前内容 |
    /// | 待拉取 | `HEAD:<路径>` | `@{u}:<路径>`（拉取会带进来的改动） |
    /// | 其余 | 目标文件 | 源文件（应用会写成这样） |
    pub fn diff(&self, target: &Path) -> Result<FileDiff, CoreError> {
        let rel = target.strip_prefix(&self.home).map_err(|_| {
            CoreError::io(format!(
                "{} 不在 {} 下，没有对应的受管文件",
                target.display(),
                self.home.display()
            ))
        })?;
        let source = self.source_dir.join(paths::MIRROR_ROOT).join(rel);
        let repo_path = repo_path(rel);
        let repo = repo_path.display().to_string();
        let facts = state::GitFacts::read(&self.source_dir)?;
        let status = state::derive(self.disk(rel)?, &repo_path, &facts);

        let (old_label, old, new_label, new) = match status {
            Status::PendingCommit | Status::Conflict => (
                format!("HEAD:{repo}"),
                revision_text(&self.source_dir, "HEAD", &repo_path),
                source.display().to_string(),
                read_text(&source),
            ),
            Status::PendingPull => (
                format!("HEAD:{repo}"),
                revision_text(&self.source_dir, "HEAD", &repo_path),
                format!("@{{u}}:{repo}"),
                revision_text(&self.source_dir, "@{u}", &repo_path),
            ),
            _ => (
                target.display().to_string(),
                read_text(target),
                source.display().to_string(),
                read_text(&source),
            ),
        };

        Ok(FileDiff {
            target: target.to_path_buf(),
            status,
            text: unified_diff(&old_label, &new_label, &old, &new),
        })
    }

    /// 目标文件与源文件的关系；源文件是受管文件本身，读不到是真错误。
    fn disk(&self, rel: &Path) -> Result<state::Disk, CoreError> {
        let target = self.home.join(rel);
        let target_bytes = match std::fs::read(&target) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(state::Disk::Missing),
            Err(e) => {
                return Err(CoreError::io(format!("读取 {} 失败：{e}", target.display())))
            }
        };

        let source = self.source_dir.join(paths::MIRROR_ROOT).join(rel);
        let source_bytes = std::fs::read(&source)
            .map_err(|e| CoreError::io(format!("读取 {} 失败：{e}", source.display())))?;

        Ok(if source_bytes == target_bytes { state::Disk::Same } else { state::Disk::Differs })
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

        let (behind, ahead) = git::ahead_behind(&self.source_dir);

        Ok(RepoStatus { branch, ahead, behind, dirty })
    }
}

/// 源目录里一条受管文件相对仓库根的路径（`home/<rel>`）；git 的输出都以它为键。
fn repo_path(rel: &Path) -> PathBuf {
    Path::new(paths::MIRROR_ROOT).join(rel)
}

/// 读文件成文本；不存在（目标缺失、新文件没有 HEAD 版本）按空内容。
fn read_text(path: &Path) -> String {
    std::fs::read(path).map(|bytes| String::from_utf8_lossy(&bytes).into_owned()).unwrap_or_default()
}

/// `git show <rev>:<路径>`；该版本里没有这个文件时按空内容（新增/删除的一侧）。
fn revision_text(source_dir: &Path, rev: &str, repo_path: &Path) -> String {
    git::run(source_dir, &["show", &format!("{rev}:{}", repo_path.display())]).unwrap_or_default()
}

/// 两侧相同就是空串（前端显示「没有差异」），否则是带 `---/+++` 头的 unified diff。
fn unified_diff(old_label: &str, new_label: &str, old: &str, new: &str) -> String {
    if old == new {
        return String::new();
    }
    similar::TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header(old_label, new_label)
        .to_string()
}
