//! config-sync 的 Core：全部能力的唯一对外表面。
//!
//! 构造时注入所有环境事实（home、source_dir、config），内部不读环境变量、不读 `$HOME`。
//! `#[tauri::command]` 只做转发，逻辑都在这里。

mod add;
mod apply;
mod config;
mod editor;
mod error;
mod git;
mod inventory;
mod onboarding;
mod paths;
mod push;
pub mod remove;
pub mod resolve;
mod state;

pub use add::{AddOutcome, AddResult};
pub use apply::{
    ApplyAction, ApplyItem, ApplyOutcome, ApplyPlan, ApplyResult, ApplySummary, SkipReason,
};
pub use config::Config;
pub use editor::Attempt;
pub use error::{CoreError, ErrorKind};
pub use inventory::{FileDiff, InventoryEntry, Refresh, RepoStatus, Status};
pub use onboarding::Candidate;
pub use push::PushOutcome;
pub use remove::{RemoveOutcome, RemoveResult};
pub use resolve::Side;

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

pub struct Core {
    home: PathBuf,
    source_dir: PathBuf,
    config: Config,
}

impl Core {
    /// `home` 是镜像布局的根（目标文件根），`source_dir` 是 git 工作区。
    pub fn new(home: impl Into<PathBuf>, source_dir: impl Into<PathBuf>, config: Config) -> Self {
        Core {
            home: home.into(),
            source_dir: source_dir.into(),
            config,
        }
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
                CoreError::io(format!(
                    "创建源目录 {} 失败：{e}",
                    self.source_dir.display()
                ))
            })?;
        }

        // `-C` 已经切进源目录，目标路径用 `.`。
        git::run(&self.source_dir, &["clone", remote, "."]).map_err(|e| CoreError {
            message: format!("从 {remote} clone 到 {} 失败", self.source_dir.display()),
            ..e
        })?;
        Ok(())
    }

    /// 拉取：`fetch` 之后只允许 `merge --ff-only`。
    ///
    /// 只更新源目录里的仓库，**绝不写任何目标文件**；拉完清单里相关行会变成待应用。
    /// 任何会生成 merge commit 的路径都不存在：不能快进就直接拒绝（附带 git 原始 stderr），
    /// 源目录保持原样。
    pub fn pull(&self) -> Result<(), CoreError> {
        let remote_url = self.remote_url().map_err(|e| CoreError {
            message: "源目录没有配置远端，无法拉取".to_string(),
            ..e
        })?;

        let upstream = git::run(
            &self.source_dir,
            &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
        )
        .map_err(|e| CoreError {
            message: "当前分支没有上游分支，无法拉取；先推送一次建立跟踪".to_string(),
            ..e
        })?;

        git::run(&self.source_dir, &["fetch", "origin"]).map_err(|e| CoreError {
            message: format!("拉取失败：无法从远端 {remote_url} 取回最新提交"),
            ..e
        })?;

        git::run(&self.source_dir, &["merge", "--ff-only", &upstream]).map_err(|e| CoreError {
            message: format!(
                "拉取被拒：无法快进到 {upstream}（本机有远端没有的提交，或工作区改动挡住了合并），\
                 需要手动处理；源目录保持原样，没有产生 merge commit"
            ),
            ..e
        })?;

        Ok(())
    }

    /// 源目录里 `origin` 的地址；设置面板用它显示"当前对着哪个远端"。
    pub fn remote_url(&self) -> Result<String, CoreError> {
        git::run(&self.source_dir, &["remote", "get-url", "origin"])
    }

    /// 写入或替换 origin 的地址；不动分支、提交和工作区。
    pub fn set_remote(&self, url: &str) -> Result<(), CoreError> {
        let url = url.trim();
        if url.is_empty() {
            return Err(CoreError::io("请填写远端仓库地址"));
        }
        let remotes = git::run(&self.source_dir, &["remote"])?;
        let verb = if remotes.lines().any(|line| line.trim() == "origin") {
            "set-url"
        } else {
            "add"
        };
        git::run(&self.source_dir, &["remote", verb, "origin", url])?;
        Ok(())
    }

    /// 受管文件的完整列表。完全没有受管文件时返回空集合（不是错误）。
    pub fn inventory(&self) -> Result<Vec<InventoryEntry>, CoreError> {
        let facts = state::GitFacts::read(&self.source_dir)?;
        self.entries(&facts)
    }

    /// 顶部状态条：分支、ahead/behind、未提交改动数。
    pub fn repo_status(&self) -> Result<RepoStatus, CoreError> {
        self.status(&state::GitFacts::read(&self.source_dir)?)
    }

    /// 一次刷新要的两半（清单 + 状态条），同一份 git 事实算出来：
    /// 前端原来分两次 IPC 各跑一遍 git，现在一次 IPC、一份 `git status`（#19）。
    pub fn refresh(&self) -> Result<Refresh, CoreError> {
        let facts = state::GitFacts::read(&self.source_dir)?;
        let entries = self.entries(&facts)?;
        let status = self.status(&facts)?;
        Ok(Refresh { entries, status })
    }

    /// 清单主体：全部受管路径 × 磁盘比较 × git 事实。
    fn entries(&self, facts: &state::GitFacts) -> Result<Vec<InventoryEntry>, CoreError> {
        let mirror_root = self.source_dir.join(paths::MIRROR_ROOT);
        let mut entries = Vec::new();

        let (mut rels, skipped_symlinks) = paths::managed_relative_paths(&mirror_root)?;
        if skipped_symlinks > 0 {
            // C2：符号链接被跳过 → 提示而非无声丢失；清单仍返回（用户可自行处理）。
            eprintln!("config-sync: 跳过 {skipped_symlinks} 个符号链接（v1 不支持符号链接语义）");
        }
        // 删掉的文件在磁盘上已经没有了，但 git 里还挂着这桩删除（未提交，或已提交未推送）：
        // 不列出来的话，清单上根本看不见「待提交」和「待推送」的删除。
        let mut deleted = HashSet::new();
        // `managed_relative_paths` 已经排好序：查重用 HashSet，补齐的行事后按序插入，
        // 不再 O(D×N) 线性找 + 整体重排（#20）。
        let known: HashSet<&Path> = rels.iter().map(PathBuf::as_path).collect();
        for path in facts.dirty_paths().chain(facts.push_deleted_paths()) {
            let Ok(rel) = path.strip_prefix(paths::MIRROR_ROOT) else {
                continue;
            };
            // 磁盘上真没有才算删除；符号链接在这里也不收（C2 的跳过口径一致）。
            if std::fs::symlink_metadata(mirror_root.join(rel)).is_ok() {
                continue;
            }
            if known.contains(rel) {
                continue;
            }
            deleted.insert(rel.to_path_buf());
        }
        for rel in &deleted {
            if let Err(at) = rels.binary_search(rel) {
                rels.insert(at, rel.clone());
            }
        }

        for rel in rels {
            let source = mirror_root.join(&rel);
            let target = self.home.join(&rel);
            let gone = deleted.contains(&rel);
            // 已经删掉的行没有源文件可比，磁盘侧按「无可比对象」算；状态由 git 事实定。
            let disk = if gone {
                state::Disk::Missing
            } else {
                self.disk(rel.as_path())?
            };
            let status = state::derive(disk, &repo_path(&rel), facts);
            entries.push(InventoryEntry {
                source,
                target,
                status,
                deleted: gone,
            });
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
        // 源文件已经被删（待提交的删除）时没有可比的一侧，磁盘侧按「无可比对象」算。
        let disk = if source.is_file() {
            self.disk(rel)?
        } else {
            state::Disk::Missing
        };
        let status = state::derive(disk, &repo_path, &facts);

        let (old_label, old, new_label, new) = match status {
            Status::PendingCommit | Status::Conflict => (
                format!("HEAD:{repo}"),
                Ok(revision_text(&self.source_dir, "HEAD", &repo_path)),
                source.display().to_string(),
                diff_side(&source),
            ),
            Status::PendingPull => (
                format!("HEAD:{repo}"),
                Ok(revision_text(&self.source_dir, "HEAD", &repo_path)),
                format!("@{{u}}:{repo}"),
                Ok(revision_text(&self.source_dir, "@{u}", &repo_path)),
            ),
            _ => (
                target.display().to_string(),
                diff_side(target),
                source.display().to_string(),
                diff_side(&source),
            ),
        };

        let done = |text: String, omitted: bool, truncated: bool, reason: String| FileDiff {
            target: target.to_path_buf(),
            status,
            text,
            omitted,
            reason,
            truncated,
        };

        let (old, new) = match (old, new) {
            (Ok(old), Ok(new)) => (old, new),
            // 读不进就别假装没差异：明说「无法在此预览」，并留编辑器里的退路（#17 S2）。
            (Err(why), _) | (_, Err(why)) => {
                return Ok(done(
                    String::new(),
                    true,
                    false,
                    format!("{DIFF_UNREADABLE}：{why}；请在编辑器里对比这两个文件"),
                ));
            }
        };

        if old == new {
            return Ok(done(String::new(), false, false, String::new()));
        }

        // 显式点开的 diff 走按需闸门：比预览宽得多，超过也只截断、不整体拒绝（#17 S2）。
        let bytes = old.len().max(new.len());
        // 两侧行数可能不同（多删 / 多增），reason 里两个都要报：尾窗口各取各的最后 5000 行，
        // 行数不等时会多出一段看着像删除的内容，读者得能自己解释掉它（第 4 轮）。
        let (old_lines, new_lines) = (old.lines().count(), new.lines().count());
        let lines = old_lines.max(new_lines);
        if bytes <= DIFF_ONDEMAND_MAX_BYTES && lines <= DIFF_ONDEMAND_MAX_LINES {
            return Ok(done(
                unified_diff(&old_label, &new_label, &old, &new),
                false,
                false,
                String::new(),
            ));
        }
        // 先算全文再截不可行：12 万行的全量 diff 要两分多钟（第 2 轮 B1）。
        // 只对比头/尾两个窗口，窗口自己有字节与行数上限，单行大文件也压得住。
        let head_old = head_window(&old);
        let head_new = head_window(&new);
        let head_diff = unified_diff(&old_label, &new_label, &head_old, &head_new);
        if !head_diff.is_empty() {
            let reason = if head_old.ends_with('\n') && head_new.ends_with('\n') {
                format!(
                    "已截断：每侧只对比了前 {DIFF_TRUNCATE_LINES} 行，完整内容请在编辑器里对比原文件"
                )
            } else {
                format!(
                    "已截断：每侧只对比了前 {} KB 字节（有超长行，按字节切），完整内容请在编辑器里对比原文件",
                    DIFF_WINDOW_BYTES / 1024
                )
            };
            return Ok(done(head_diff, false, true, reason));
        }
        // 差异全在头部窗口之外：给末尾窗口，别让用户点开看到空白（第 2 轮 N1）。
        // 尾窗口的 hunk 头是窗口内相对行号，得加回跳过的行数，不然和正文对不上（第 3 轮 B2）。
        let (tail_old, skipped_old) = tail_window(&old);
        let (tail_new, skipped_new) = tail_window(&new);
        let tail_diff = offset_hunk_headers(
            &unified_diff(&old_label, &new_label, &tail_old, &tail_new),
            skipped_old,
            skipped_new,
        );
        if tail_diff.is_empty() {
            // 头尾窗口都没差异：直说中间那段没对比，别让用户以为“没差异”。
            // 两侧行数不等时「中间」只能报个上界，所以写「最多」（第 4 轮）。
            let skipped = lines.saturating_sub(2 * DIFF_TRUNCATE_LINES);
            return Ok(done(
                tail_diff,
                false,
                true,
                format!(
                    "已截断：头尾各 {DIFF_TRUNCATE_LINES} 行内都没有差异，中间最多 {skipped} 行没有对比\
                     （左侧 {old_lines} 行 / 右侧 {new_lines} 行），完整内容请在编辑器里对比原文件"
                ),
            ));
        }
        Ok(done(
            tail_diff,
            false,
            true,
            format!(
                "已截断：前 {DIFF_TRUNCATE_LINES} 行内没有差异，只对比了末尾 {DIFF_TRUNCATE_LINES} 行\
                 （左侧 {old_lines} 行 / 右侧 {new_lines} 行），完整内容请在编辑器里对比原文件"
            ),
        ))
    }

    /// 打开一行的源文件（受管副本，绝不是 `$HOME` 里的目标文件）要用的尝试序列；不执行进程。
    ///
    /// 源文件不在磁盘上（清单过期或被删）时直接报错，不把空路径交给编辑器。
    pub fn edit_plan(&self, target: &Path) -> Result<Vec<Attempt>, CoreError> {
        let rel = target.strip_prefix(&self.home).map_err(|_| {
            CoreError::io(format!(
                "{} 不在 {} 下，没有对应的受管文件",
                target.display(),
                self.home.display()
            ))
        })?;
        let source = self.source_dir.join(paths::MIRROR_ROOT).join(rel);
        if !source.is_file() {
            return Err(CoreError::io(format!(
                "源文件 {} 不存在（清单可能已过期），不打开空路径；请重新读取清单",
                source.display()
            )));
        }
        editor::plan(&self.config.editor_command, &source)
    }

    /// 用 `editor_command`（或系统 `open`）打开源文件，拉起即返回，不等编辑器退出；
    /// 保存后的清单刷新靠外壳的 fs watch。
    pub fn edit(&self, target: &Path) -> Result<(), CoreError> {
        let mut last = None;
        for attempt in self.edit_plan(target)? {
            match attempt.run() {
                Ok(()) => return Ok(()),
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| CoreError::io("没有可用的打开方式")))
    }

    /// 在 Finder 里打开源目录；目录不存在时报错，不弹空白窗口。
    pub fn open_source_dir(&self) -> Result<(), CoreError> {
        editor::reveal(&self.source_dir)
    }

    /// 目标文件与源文件的关系；源文件是受管文件本身，读不到是真错误。
    /// 把 `$HOME` 下的路径换算成镜像内相对路径；`$HOME` 之外（或带 `..` 这类穿透成分）返回 `None`。
    pub(crate) fn mirror_relative_path(&self, path: &Path) -> Option<PathBuf> {
        // 粘贴进来的 `~/…` 按注入的 $HOME 展开（不读环境变量）。
        let expanded = match path.strip_prefix("~") {
            Ok(rest) => self.home.join(rest),
            Err(_) => path.to_path_buf(),
        };
        if let Some(rel) = inside(&expanded, &self.home) {
            return Some(rel);
        }
        // 选择器与终端常给出带符号链接的路径（macOS 上 /tmp、/var 都是软链）：按真实路径再试一次。
        inside(
            &expanded.canonicalize().ok()?,
            &self.home.canonicalize().ok()?,
        )
    }

    /// 先比长度：不相等直接不同，相等才读内容（#20）。
    fn disk(&self, rel: &Path) -> Result<state::Disk, CoreError> {
        let target = self.home.join(rel);
        let target_len = match std::fs::metadata(&target) {
            Ok(meta) if meta.is_file() => meta.len(),
            // 目录等非普通文件：走原来的读错误路径，错误类型与文案不变（#20 N3）。
            Ok(_) => std::fs::read(&target)
                .map(|bytes| bytes.len() as u64)
                .map_err(|e| CoreError::io(format!("读取 {} 失败：{e}", target.display())))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(state::Disk::Missing),
            Err(e) => {
                return Err(CoreError::io(format!(
                    "读取 {} 失败：{e}",
                    target.display()
                )));
            }
        };

        let source = self.source_dir.join(paths::MIRROR_ROOT).join(rel);
        let source_len = std::fs::metadata(&source)
            .map_err(|e| CoreError::io(format!("读取 {} 失败：{e}", source.display())))?
            .len();
        if source_len != target_len {
            return Ok(state::Disk::Differs);
        }

        let target_bytes = std::fs::read(&target)
            .map_err(|e| CoreError::io(format!("读取 {} 失败：{e}", target.display())))?;
        let source_bytes = std::fs::read(&source)
            .map_err(|e| CoreError::io(format!("读取 {} 失败：{e}", source.display())))?;

        Ok(if source_bytes == target_bytes {
            state::Disk::Same
        } else {
            state::Disk::Differs
        })
    }

    /// 顶部状态条：分支、ahead/behind、未提交改动数。
    /// 状态条那一半：分支、ahead/behind、未提交改动数，全部取自同一份 git 事实。
    fn status(&self, facts: &state::GitFacts) -> Result<RepoStatus, CoreError> {
        let branch = facts.branch();
        if branch.is_empty() {
            return Err(CoreError {
                kind: ErrorKind::Git,
                message: "源目录当前不在任何分支上（detached HEAD）".to_string(),
                stderr: None,
            });
        }

        let (ahead, behind) = facts.ahead_behind();

        Ok(RepoStatus {
            branch: branch.to_string(),
            ahead,
            behind,
            dirty: facts.dirty_count(),
        })
    }
}

/// 源目录里一条受管文件相对仓库根的路径（`home/<rel>`）；git 的输出都以它为键。
fn repo_path(rel: &Path) -> PathBuf {
    Path::new(paths::MIRROR_ROOT).join(rel)
}

/// `path` 相对 `home` 的纯普通路径；不在 `home` 下或带 `..`/`.` 这类穿透成分时返回 `None`。
fn inside(path: &Path, home: &Path) -> Option<PathBuf> {
    let rel = path.strip_prefix(home).ok()?;
    // `..` 会让 join 出来的源路径穿出 home/，一律不收。
    if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return None;
    }
    Some(rel.to_path_buf())
}

/// 读文件成文本；不存在（目标缺失、新文件没有 HEAD 版本）按空内容。
/// 显式 diff 的一侧：读不进就给出原因，而不是假装空文件。
///
/// 只有「读不进」（读取失败、非 UTF-8、超过硬上限）才走这条错误路；
/// 文件不存在仍按空内容算，因为新增/删除的一侧本来就没有内容（#17 S2）。
fn diff_side(path: &Path) -> Result<String, String> {
    let len = match std::fs::metadata(path) {
        Ok(meta) => meta.len(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(_) => 0,
    };
    if len > DIFF_HARD_MAX_BYTES {
        return Err(format!(
            "文件有 {} MB，超过 {} MB 的硬上限，没有读进内存",
            len.div_ceil(1024 * 1024),
            DIFF_HARD_MAX_BYTES / (1024 * 1024)
        ));
    }
    let bytes =
        std::fs::read(path).map_err(|err| format!("读取 {} 失败：{err}", path.display()))?;
    String::from_utf8(bytes).map_err(|_| format!("{} 不是 UTF-8 文本", path.display()))
}

/// 头部窗口：最多 `DIFF_TRUNCATE_LINES` 行、`DIFF_WINDOW_BYTES` 字节，先到先算。
///
/// 单行大文件（压缩的 JSON、min.js、整条证书链）没有换行可切，只能按字节切在这一行里，
/// 落点退到 char 边界，不切半个 UTF-8 字符（第 2 轮 B1）。
fn head_window(text: &str) -> String {
    let mut kept = String::new();
    for (index, line) in text.lines().enumerate() {
        if index == DIFF_TRUNCATE_LINES {
            break;
        }
        if kept.len() + line.len() + 1 > DIFF_WINDOW_BYTES {
            if kept.is_empty() {
                let at = floor_char_boundary(line, DIFF_WINDOW_BYTES);
                kept.push_str(&line[..at]);
            }
            break;
        }
        kept.push_str(line);
        kept.push('\n');
    }
    kept
}

/// 尾部窗口：差异全在头部窗口之外时用，上限同上（第 2 轮 N1）。
/// 一并回带「跳过了多少行」：hunk 头要把这段偏移加回去（第 3 轮 B2）。
fn tail_window(text: &str) -> (String, usize) {
    let mut kept: Vec<&str> = Vec::new();
    let mut bytes = 0;
    for line in text.lines().rev().take(DIFF_TRUNCATE_LINES) {
        let extra = line.len() + 1;
        if bytes + extra > DIFF_WINDOW_BYTES {
            if kept.is_empty() {
                let from = floor_char_boundary(line, line.len().saturating_sub(DIFF_WINDOW_BYTES));
                kept.push(&line[from..]);
            }
            break;
        }
        kept.push(line);
        bytes += extra;
    }
    let total = text.lines().count();
    let window: String = kept.iter().rev().map(|line| format!("{line}\n")).collect();
    (window, total - kept.len())
}

/// 尾窗口的 hunk 头是窗口内相对行号，加回跳过的行数才对得上正文（`@@ -4997` → `@@ -119997`，第 3 轮 B2）。
fn offset_hunk_headers(diff: &str, old_offset: usize, new_offset: usize) -> String {
    diff.lines()
        .map(|line| {
            let shifted = line.strip_prefix("@@ -").and_then(|rest| {
                let (old, rest) = rest.split_once(" +")?;
                let (new, tail) = rest.split_once(' ')?;
                Some(format!(
                    "@@ -{} +{} {tail}",
                    shift_range(old, old_offset)?,
                    shift_range(new, new_offset)?
                ))
            });
            format!("{}\n", shifted.unwrap_or_else(|| line.to_string()))
        })
        .collect()
}

/// `4997` 或 `4997,4` → 起始行加上偏移，行数不变。
fn shift_range(range: &str, offset: usize) -> Option<String> {
    match range.split_once(',') {
        Some((start, count)) => Some(format!("{},{count}", start.parse::<usize>().ok()? + offset)),
        None => Some((range.parse::<usize>().ok()? + offset).to_string()),
    }
}

/// 不超过 `at` 的 char 边界。
fn floor_char_boundary(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// `git show <rev>:<路径>`；该版本里没有这个文件时按空内容（新增/删除的一侧）。
fn revision_text(source_dir: &Path, rev: &str, repo_path: &Path) -> String {
    git::run(
        source_dir,
        &["show", &format!("{rev}:{}", repo_path.display())],
    )
    .unwrap_or_default()
}

/// 预览侧的闸门：清单里每一项都要算 diff，所以压得低，超了就不预生成（#17）。
pub(crate) const DIFF_MAX_BYTES: usize = 256 * 1024;
const DIFF_MAX_LINES: usize = 5000;

/// 显式「点击查看」侧的闸门：用户主动点开一个文件，比预览宽两个数量级。
/// 超过它也不算全量，只对比头/尾窗口并说明截断（#17 S2）。
const DIFF_ONDEMAND_MAX_BYTES: usize = 4 * 1024 * 1024;
const DIFF_ONDEMAND_MAX_LINES: usize = 100_000;

/// 超过按需闸门时只对比这两段窗口，每侧各有行数与字节上限（#17 S2）。
/// 不算全量再截：12 万行的全量 diff 要两分多钟，比不截还糟（第 2 轮 B1）。
const DIFF_TRUNCATE_LINES: usize = 5000;
const DIFF_WINDOW_BYTES: usize = 256 * 1024;

/// 显式 diff 的硬上限：再大就不读进内存，直接说「无法在此预览」。
const DIFF_HARD_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// 超闸门时那句原因的固定开头（不靠它判类型，只用来一眼认出）。
pub const DIFF_TOO_LARGE: &str = "差异过大";

/// 读不进（非 UTF-8 / 超硬上限 / IO 失败）时那句原因的固定开头。
pub const DIFF_UNREADABLE: &str = "无法在此预览";

/// 超闸门的原因。`lines` 为 `None` 表示那一侧压根没读（超字节闸就不读，见 #17 N4）。
fn too_large_reason(bytes: usize, lines: Option<usize>) -> String {
    let limit = DIFF_MAX_BYTES / 1024;
    match lines {
        Some(lines) => format!(
            "{DIFF_TOO_LARGE}：最大的一侧 {} KB / {lines} 行，超过 {limit} KB / {DIFF_MAX_LINES} 行的上限，\
             没有生成 diff；请在编辑器里对比这两个文件",
            bytes.div_ceil(1024),
        ),
        None => format!(
            "{DIFF_TOO_LARGE}：最大的一侧 {} KB，已超过 {limit} KB 的上限，\
             超过上限的那一侧没有读入内存；请在编辑器里对比这两个文件",
            bytes.div_ceil(1024),
        ),
    }
}

/// 字节与行数各自取两侧的最大值再判：字节最宽的那侧不一定是行最多的那侧（#17 B1）。
fn diff_gate_reason(old: &str, new: &str) -> Option<String> {
    let bytes = old.len().max(new.len());
    let lines = old.lines().count().max(new.lines().count());
    if bytes <= DIFF_MAX_BYTES && lines <= DIFF_MAX_LINES {
        return None;
    }
    Some(too_large_reason(bytes, Some(lines)))
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
