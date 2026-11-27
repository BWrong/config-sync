//! 六态推导：只用源目录的 git 事实 + 逐文件磁盘比较，不设任何元数据文件或数据库。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::{CoreError, Status, git};

/// 源目录的 git 事实，按「相对仓库根」的路径归集（与 `git status` 输出的路径同一口味）。
pub(crate) struct GitFacts {
    /// 当前分支；detached HEAD 时为空，状态条那边负责报错。
    branch: String,
    /// 上游的领先 / 落后计数；没有上游时都是 0。
    ahead: u32,
    behind: u32,
    /// 未提交改动的路径数，与 `dirty` 出自同一份解析（状态条显示它）。
    dirty_count: u32,
    /// 未提交改动：已暂存 / 已修改 / 未跟踪。
    dirty: HashSet<PathBuf>,
    /// 未合并（`git status` 显示 `UU` / `AA` / `DD` 等）：真冲突。
    unmerged: HashSet<PathBuf>,
    /// 远端相对 HEAD 改过的路径；`behind == 0` 时必为空。
    remote_changed: HashSet<PathBuf>,
    /// 本机已有提交删掉的路径（远端此刻还有它）；`ahead == 0` 时必为空。
    push_deleted: HashSet<PathBuf>,
}

impl GitFacts {
    pub(crate) fn read(source_dir: &Path) -> Result<Self, CoreError> {
        // 一个进程出齐三件事：`--branch` 带分支与 ahead/behind，逐文件改动来自同一份
        // `status`。以前是 `status` + `branch --show-current` + `rev-list` 三个进程（#19）。
        // `-uall` 才有未跟踪目录里的逐个文件，`-z` 才不会被带空格的路径骗到。
        let porcelain = git::run(
            source_dir,
            &[
                "status",
                "--porcelain=v2",
                "--branch",
                "-z",
                "--untracked-files=all",
            ],
        )?;

        let mut branch = String::new();
        let mut ahead = 0;
        let mut behind = 0;
        let mut dirty_count = 0;
        let mut dirty = HashSet::new();
        let mut unmerged = HashSet::new();
        let mut fields = porcelain.split('\0').filter(|field| !field.is_empty());
        while let Some(field) = fields.next() {
            // 头行：`# branch.head main` / `# branch.ab +1 -2`；oid / upstream 这些用不上。
            if let Some(header) = field.strip_prefix("# ") {
                if let Some(name) = header.strip_prefix("branch.head ") {
                    if name != "(detached)" {
                        branch = name.to_string();
                    }
                } else if let Some(counts) = header.strip_prefix("branch.ab ") {
                    let mut parts = counts.split_whitespace();
                    ahead = parts
                        .next()
                        .and_then(|n| n.strip_prefix('+'))
                        .and_then(|n| n.parse().ok())
                        .unwrap_or(0);
                    behind = parts
                        .next()
                        .and_then(|n| n.strip_prefix('-'))
                        .and_then(|n| n.parse().ok())
                        .unwrap_or(0);
                }
                continue;
            }
            // 记录格式：`1 XY … <路径>` / `2 XY … <路径>\0<原路径>`（rename、copy）/
            // `u XY … <路径>`（未合并）/ `? <路径>`。在这儿只关心路径与是否未合并，
            // 前几段定长、都不含空格，所以按空格切到路径那一段为止。
            let (path_field, unmerged_record) = match field.as_bytes().first() {
                Some(b'1') => (field.splitn(9, ' ').nth(8), false),
                Some(b'2') => (field.splitn(10, ' ').nth(9), false),
                Some(b'u') => (field.splitn(11, ' ').nth(10), true),
                Some(b'?') => (field.split_once(' ').map(|rest| rest.1), false),
                _ => (None, false),
            };
            let Some(path) = path_field else {
                continue;
            };
            let path = PathBuf::from(path);
            dirty_count += 1;
            if unmerged_record {
                unmerged.insert(path.clone());
            }
            dirty.insert(path);
            if field.starts_with("2 ") {
                // rename / copy 的原路径是紧跟着的下一个字段。
                fields.next();
            }
        }

        // fetch 之后才有意义的 behind：没有上游或没拉过就一直为 0。
        let remote_changed = if behind == 0 {
            HashSet::new()
        } else {
            git::run(source_dir, &["diff", "--name-only", "HEAD", "@{u}"])?
                .lines()
                .map(PathBuf::from)
                .collect()
        };

        // 已提交但没推送的删除：这些文件在磁盘上早就没了，不进这个集合就再也列不出来。
        let push_deleted = if ahead == 0 {
            HashSet::new()
        } else {
            git::run(
                source_dir,
                &[
                    "diff",
                    "--diff-filter=D",
                    "--name-only",
                    "-z",
                    "@{u}",
                    "HEAD",
                ],
            )?
            .split('\0')
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .collect()
        };

        Ok(GitFacts {
            branch,
            ahead,
            behind,
            dirty_count,
            dirty,
            unmerged,
            remote_changed,
            push_deleted,
        })
    }

    /// 当前分支；detached HEAD 时为空。
    pub(crate) fn branch(&self) -> &str {
        &self.branch
    }

    /// 上游的领先 / 落后计数，与状态条同一份来源。
    pub(crate) fn ahead_behind(&self) -> (u32, u32) {
        (self.ahead, self.behind)
    }

    /// 未提交改动的路径数（状态条那一栏）。
    pub(crate) fn dirty_count(&self) -> u32 {
        self.dirty_count
    }

    /// 未提交改动涉及的仓库内路径（推送前的硬拦扫这些文件）。
    pub(crate) fn dirty_paths(&self) -> impl Iterator<Item = &PathBuf> {
        self.dirty.iter()
    }

    /// 已提交、但还没推送出去的删除涉及的仓库内路径。
    pub(crate) fn push_deleted_paths(&self) -> impl Iterator<Item = &PathBuf> {
        self.push_deleted.iter()
    }

    /// 这条仓库内路径是否处于真未合并（`UU`/`AA`/`DD`）状态。
    pub(crate) fn is_unmerged(&self, repo_path: &Path) -> bool {
        self.unmerged.contains(repo_path)
    }
}

/// 目标文件与源文件的关系（纯磁盘事实）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Disk {
    Missing,
    Same,
    Differs,
}

/// 七态推导。判定顺序即优先级，高者先返回：
///
/// 1. 未合并 → 冲突
/// 2. 本地有未提交改动 **且** 远端也改了同一路径 → 冲突
/// 3. 本地有未提交改动 → 待提交
/// 4. 本机提交里删了这个文件、还没推送 → 待推送删除
/// 5. 远端改了这条路径（behind > 0）→ 待拉取
/// 6. 目标缺失 → 目标缺失
/// 7. 内容相同 → 一致
/// 8. 其余 → 待应用
pub(crate) fn derive(disk: Disk, repo_path: &Path, facts: &GitFacts) -> Status {
    let dirty = facts.dirty.contains(repo_path);
    let remote = facts.remote_changed.contains(repo_path);

    // 冲突 = 「真未合并」或「本地改了同一路径 ∧ 远端也改了它」。判断作纯粹磁盘事实：
    // dirty 与 remote 各自独立，哪个为真就归哪个, 冲突只占二者交集。
    if facts.unmerged.contains(repo_path) || (dirty && remote) {
        Status::Conflict
    } else if dirty {
        Status::PendingCommit
    } else if facts.push_deleted.contains(repo_path) {
        // 源文件已经不在磁盘上，只剩一条待推送的删除：不单独成态就彻底看不见。
        Status::PendingPushDelete
    } else if remote {
        Status::PendingPull
    } else {
        match disk {
            Disk::Missing => Status::TargetMissing,
            Disk::Same => Status::InSync,
            Disk::Differs => Status::Pending,
        }
    }
}
