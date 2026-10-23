//! 冲突解决：按文件二选一（本机版本 / 远端版本），然后重新提交。
//!
//! 不做三方合并 UI，不打开外部合并工具。一个动作一个后果：只解决这一个文件。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{CoreError, Core, Status, git, state};

/// 解决冲突时保留哪一侧。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    /// 保留本机版本。
    Ours,
    /// 采用远端版本。
    Theirs,
}

impl Side {
    fn verb(&self) -> &'static str {
        match self {
            Side::Ours => "ours",
            Side::Theirs => "theirs",
        }
    }
}

impl Core {
    /// 按文件解决一个冲突：先把远端取到最新，再把这一侧写进源目录并重新提交。
    ///
    /// 只动 `target` 这一个文件；其它冲突行、其它未提交改动都不受影响。
    /// 解决后仓库是干净的（改动已经提交），清单里这一行回落为待提交。
    ///
    /// 两种冲突形态的语义：
    /// - 真未合并（`UU`/`AA`/`DD`，索引里有 stage 2/3）：`checkout --ours/--theirs` 才按
    ///   「本机/远端」取；
    /// - 「本地未提交改动 ∧ 远端也改了同一路径」（本产品的主形态，索引里没有 stage）：
    ///   ours = 保留工作区内容直接提交；theirs = `checkout @{u} --` 把远端版本取进来。
    #[allow(clippy::too_many_lines)] // 分形态 + 收尾，逐行都直接对应语义。
    pub fn resolve_conflict(
        &self,
        target: &Path,
        side: Side,
    ) -> Result<(), CoreError> {
        let rel = target.strip_prefix(&self.home).map_err(|_| {
            CoreError::io(format!(
                "{} 不在 {} 下，没有对应的受管文件",
                target.display(),
                self.home.display()
            ))
        })?;
        let repo_path = repo_path(rel);
        let repo = repo_path.display().to_string();

        // 先取远端最新，让「采用远端版本」的对象是当下最新的，而不是上次 fetch 的。
        // git 的其余调用不要求网络，这里失败就带原始 stderr 停下，源目录保持原样。
        git::run(&self.source_dir, &["fetch", "origin"]).map_err(|e| CoreError {
            message: format!(
                "拉取失败：无法从远端取回最新提交（网络或远端地址问题）；冲突未解决，源目录保持原样"
            ),
            ..e
        })?;

        // 冲突判定与清单同源：这条路径必须是六态里的「冲突」。
        let facts = state::GitFacts::read(&self.source_dir)?;
        let status = state::derive(self.disk(rel)?, &repo_path, &facts);
        if status != Status::Conflict {
            let what = match status {
                Status::TargetMissing => "目标文件不存在，谈不上冲突".to_string(),
                Status::Conflict => unreachable!(),
                _ => format!("当前{}，不冲突", status_label(status)),
            };
            return Err(CoreError::io(format!(
                "{} {}，不能按文件解决；请重新读取清单",
                target.display(),
                what
            )));
        }

        if facts.is_unmerged(&repo_path) {
            // 真未合并：git 的标准语义，--ours/--theirs 各取本机/远端。
            git::run(
                &self.source_dir,
                &["checkout", &format!("--{}", side.verb()), "--", &repo],
            )
            .map_err(|e| CoreError { message: format!("无法取出{}版本：{e}", side.verb()), ..e })?;
        } else if side == Side::Theirs {
            // 主形态：「采用远端版本」把远端侧的版本取进工作区，覆盖掉本机未提交的改动。
            git::run(&self.source_dir, &["checkout", "@{u}", "--", &repo]).map_err(|e| {
                CoreError { message: "无法从远端取出该文件的版本".to_string(), ..e }
            })?;
        }
        // 主形态下的 ours：工作区的本机版本就是我们要的，直接进入提交。

        // 收尾：只暂存这一个文件，重新提交。提交信息里路径用仓库内的 `home/<rel>`。
        git::run(&self.source_dir, &["add", "--", &repo])
            .map_err(|e| CoreError { message: format!("暂存 {} 失败：{e}", target.display()), ..e })?;
        git::run(
            &self.source_dir,
            &["commit", "-m", &format!("config-sync: resolve conflict: {repo} ({})", side.verb())],
        )
        .map_err(|e| CoreError { message: format!("提交冲突解决结果失败：{e}"), ..e })?;

        Ok(())
    }
}

/// 源目录里一条受管文件相对仓库根的路径（`home/<rel>`）。
fn repo_path(rel: &Path) -> PathBuf {
    Path::new(crate::paths::MIRROR_ROOT).join(rel)
}

fn status_label(status: Status) -> &'static str {
    match status {
        Status::Conflict => "冲突",
        Status::PendingCommit => "待提交",
        Status::PendingPull => "待拉取",
        Status::TargetMissing => "目标缺失",
        Status::InSync => "一致",
        Status::Pending => "待应用",
    }
}