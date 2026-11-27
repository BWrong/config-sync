//! config-sync 的 Core：全部能力的唯一对外表面。
//!
//! 构造时注入所有环境事实（home、source_dir、config），内部不读环境变量、不读 `$HOME`。
//! `#[tauri::command]` 只做转发，逻辑都在这里。

mod add;
mod apply;
mod backup;
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
    ApplyAction, ApplyExecution, ApplyItem, ApplyOutcome, ApplyPlan, ApplyResult, ApplyStep,
    ApplySummary, SkipReason,
};
pub use backup::{
    BackupEntry, BackupManifest, BackupResult, BackupSummary, RestoreAction, RestoreItem,
    RestoreOutcome, RestorePlan, RestoreResult,
};
pub use config::Config;
pub use editor::Attempt;
pub use error::{CoreError, ErrorKind};
pub use inventory::{FileDiff, InventoryEntry, Refresh, RepoStatus, Status};
pub use onboarding::Candidate;
pub use push::PushOutcome;
pub use remove::{RemoveOutcome, RemoveResult};
pub use resolve::Side;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::SystemTime;

pub struct Core {
    home: PathBuf,
    source_dir: PathBuf,
    config: Config,
    caches: Caches,
}

/// 一侧文件在某一刻的「廉价身份」：长度 + mtime + ctime + inode。
///
/// ctime 是关键：用户态改不回去，`cp -p` / `rsync -a` / `tar -xpf` / 显式 `set_modified`
/// 这类「连 mtime 一起还原」的恢复都会动它，所以只保留 mtime 骗不过这一关（#42-1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    len: u64,
    mtime: SystemTime,
    ctime: i64,
    ctime_nsec: i64,
    ino: u64,
}

/// 上次逐行比较磁盘时的两侧身份与结论：两侧都没动就不读内容（#42-1）。
struct DiskStamp {
    target: Stamp,
    source: Stamp,
    disk: state::Disk,
}

/// 计划阶段读到的源侧内容，执行阶段若源文件没变就直接用它，不再读一遍（#42-2）。
struct PlannedSource {
    len: u64,
    mtime: SystemTime,
    bytes: Vec<u8>,
}

/// 上限：计划里攒下的源内容最多这么多字节，超过就不进缓存（执行阶段照旧读盘）。
const PLAN_CACHE_MAX_BYTES: usize = 8 * 1024 * 1024;

/// 「设置分支 ≠ 当前分支」时 ahead/behind 用的 ref 事实，一次 `for-each-ref` 出齐（#42-4）。
#[derive(Clone, Default, PartialEq, Eq)]
struct SettingRefs {
    /// `refs/heads/<setting>` 的 sha；本地还没这条分支时 `None`。
    local: Option<String>,
    /// 各远端下同名分支的 sha：`refs/remotes/<remote>/<setting>`。
    remotes: BTreeMap<String, String>,
}

impl SettingRefs {
    fn read(source_dir: &Path, setting: &str) -> Result<SettingRefs, CoreError> {
        let out = git::run(
            source_dir,
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                &format!("refs/heads/{setting}"),
                &format!("refs/remotes/*/{setting}"),
            ],
        )?;
        let local_ref = format!("refs/heads/{setting}");
        let remote_suffix = format!("/{setting}");
        let mut refs = SettingRefs::default();
        for line in out.lines() {
            let Some((name, sha)) = line.split_once(' ') else {
                continue;
            };
            if name == local_ref {
                refs.local = Some(sha.to_string());
            } else if let Some(remote) = name
                .strip_prefix("refs/remotes/")
                .and_then(|rest| rest.strip_suffix(&remote_suffix))
            {
                refs.remotes.insert(remote.to_string(), sha.to_string());
            }
        }
        Ok(refs)
    }
}

/// 设置分支 ≠ 当前分支时算出来的 ahead/behind，连同一份 ref 事实当键：ref 一个都没动就
/// 直接复用，不再起 git（#42-4）。
#[derive(Clone, PartialEq, Eq)]
struct ForkStatus {
    setting: String,
    refs: SettingRefs,
    ahead: u32,
    behind: u32,
}

/// Core 的加速旁路：只省 IO 与进程，不参与任何结论推导。
struct Caches {
    inner: Mutex<CacheData>,
}

#[derive(Default)]
struct CacheData {
    disk: HashMap<PathBuf, DiskStamp>,
    planned: HashMap<PathBuf, PlannedSource>,
    planned_bytes: usize,
    fork_status: Option<ForkStatus>,
    facts: Option<FactsCache>,
}

/// 上次读到的 git 事实，连同读它那一刻的 `.git` 身份（#42-a）。
struct FactsCache {
    stamp: Vec<(PathBuf, Option<Stamp>)>,
    facts: Arc<state::GitFacts>,
}

impl Caches {
    fn new() -> Self {
        Caches {
            inner: Mutex::new(CacheData::default()),
        }
    }

    /// 中毒（有线程持锁时 panic）就清空重来：缓存丢了只是慢一点，不能把读状态一起拖垮。
    fn lock(&self) -> MutexGuard<'_, CacheData> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                let mut guard = poisoned.into_inner();
                *guard = CacheData::default();
                guard
            }
        }
    }
}

/// `rev` 能不能解析成一个提交/引用（不存在时静默为 false）。
fn rev_exists(source_dir: &Path, rev: &str) -> bool {
    git::run(source_dir, &["rev-parse", "--verify", "--quiet", rev]).is_ok()
}

/// 设置分支对应的远端名：clone / 拉取 / 推送三条路径共用这一个来源。
///
/// 优先用这个分支上游配置的远端名；没有上游、或上游指向本机分支（`remote = .`）时
/// 回落 `origin`；连 `origin` 都没有而仓库只有一个远端，就用那一个——否则远端改成别的
/// 名字之后，拉取还在问一个不存在的 `origin`。三条路径同一个远端，才不会各推各的。
pub(crate) fn branch_remote(source_dir: &Path, branch: &str) -> String {
    let local_ref = format!("refs/heads/{branch}");
    let upstream = git::run(
        source_dir,
        &[
            "for-each-ref",
            "--format=%(upstream:remotename)",
            &local_ref,
        ],
    )
    .unwrap_or_default();
    let upstream = upstream.trim();
    if !upstream.is_empty() && upstream != "." {
        return upstream.to_string();
    }

    let remotes = git::run(source_dir, &["remote"]).unwrap_or_default();
    let mut names: Vec<&str> = remotes
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .collect();
    if names.contains(&"origin") || names.len() != 1 {
        "origin".to_string()
    } else {
        names.remove(0).to_string()
    }
}

impl Core {
    /// `home` 是镜像布局的根（目标文件根），`source_dir` 是 git 工作区。
    pub fn new(home: impl Into<PathBuf>, source_dir: impl Into<PathBuf>, config: Config) -> Self {
        Core {
            home: home.into(),
            source_dir: source_dir.into(),
            config,
            caches: Caches::new(),
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

        // `-C` 已经切进源目录，目标路径用 `.`。分支来自设置：clone 完就停在那个分支上，
        // 而不是停在远端的默认分支——否则第一次拉取/推送会把改动落到另一个分支。
        let branch = self.branch()?;
        git::run(&self.source_dir, &["clone", "--branch", branch, remote, "."]).map_err(|e| {
            CoreError {
                message: format!(
                    "从 {remote} clone 到 {} 失败（分支 {branch} 是设置里指定的，远端没有它会直接报错）",
                    self.source_dir.display()
                ),
                ..e
            }
        })?;
        Ok(())
    }

    /// 设置里的分支：clone / 拉取 / 推送三条路径唯一的来源。
    fn branch(&self) -> Result<&str, CoreError> {
        let branch = self.config.branch.trim();
        if branch.is_empty() {
            return Err(CoreError::io(
                "设置里的分支是空的：clone / 拉取 / 推送不知道用哪个分支；去设置里填一个（比如 main）",
            ));
        }
        Ok(branch)
    }

    /// 保证源目录的 HEAD 就在设置的分支上，返回那个分支名，以及切之前在哪里（没切是 `None`）。
    ///
    /// 当前分支与设置不一致时：工作区干净就切过去（本地有就 `switch`，只有远端有就
    /// `switch -c --track`）；有未提交改动就拒绝并说清楚——切分支会把这些改动带走，
    /// 撞上冲突还会半途失败，宁可停下。拒绝时一个字节都不动，没有丢任何改动。
    fn ensure_branch(&self) -> Result<(String, Option<String>), CoreError> {
        let branch = self.branch()?;
        if !self.has_repo() {
            return Err(CoreError::io(format!(
                "源目录 {} 还不是 git 仓库，先 clone",
                self.source_dir.display()
            )));
        }

        let current = git::run(&self.source_dir, &["branch", "--show-current"])?;
        if current == branch {
            return Ok((branch.to_string(), None));
        }
        if current.is_empty() {
            return Err(CoreError::io(format!(
                "源目录不在任何分支上（detached HEAD），设置的分支是 {branch}；\
                 先手动切回 {branch} 再重试"
            )));
        }

        let dirty = git::run(&self.source_dir, &["status", "--porcelain"])?;
        let dirty_count = dirty.lines().filter(|line| !line.is_empty()).count();
        if dirty_count > 0 {
            return Err(CoreError::io(format!(
                "源目录当前在 {current} 上，设置的分支是 {branch}，还有 {dirty_count} 个未提交改动：\
                 这时切分支会把改动带走或覆盖，先提交（推送）或撤掉它们再重试；没有丢任何改动"
            )));
        }

        let local = format!("refs/heads/{branch}");
        // 远端名与推送同源：远端不叫 origin 时这里也得问同一个地方。
        let remote = branch_remote(&self.source_dir, branch);
        let tracking = format!("{remote}/{branch}");
        if rev_exists(&self.source_dir, &local) {
            git::run(&self.source_dir, &["switch", branch])?;
        } else if rev_exists(&self.source_dir, &tracking) {
            git::run(
                &self.source_dir,
                &["switch", "-c", branch, "--track", &tracking],
            )?;
        } else {
            return Err(CoreError::io(format!(
                "源目录当前在 {current} 上，设置的分支 {branch} 本地和 {remote} 上都没有；\
                 先确认分支名，或拉取一次让它出现"
            )));
        }
        Ok((branch.to_string(), Some(current)))
    }

    /// 拉取：`fetch` 之后只允许 `merge --ff-only`。
    ///
    /// 只更新源目录里的仓库，**绝不写任何目标文件**；拉完清单里相关行会变成待应用。
    /// 任何会生成 merge commit 的路径都不存在：不能快进就直接拒绝（附带 git 原始 stderr），
    /// 源目录保持原样。
    pub fn pull(&self) -> Result<(), CoreError> {
        // 分支来自设置，不需要先有上游：`fetch` 显式给出 refspec，源目录没有
        // `remote.origin.fetch`（clone 之外建的仓库）也不会「fetch 完什么都没更新」。
        // 远端名与推送同源（上游配置 - 回落 origin）：三条路径必须对着同一个远端，
        // 否则远端不叫 origin 时，推送能出去、拉取却在问一个不存在的远端。
        let (branch, _switched) = self.ensure_branch()?;
        let remote = branch_remote(&self.source_dir, &branch);
        let remote_url = self.remote_url_of(&remote).map_err(|e| CoreError {
            message: format!("源目录没有配置远端 {remote}，无法拉取"),
            ..e
        })?;
        let fetched = format!("+refs/heads/{branch}:refs/remotes/{remote}/{branch}");

        git::run(&self.source_dir, &["fetch", &remote, &fetched]).map_err(|e| CoreError {
            message: format!(
                "拉取失败：无法从远端 {remote_url} 取回最新提交（远端可能没有 {branch} 分支）"
            ),
            ..e
        })?;

        let upstream = format!("{remote}/{branch}");
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
        self.remote_url_of("origin")
    }

    /// 指定远端名的地址：拉取按上游配置的远端名取地址，不能只看 `origin`。
    fn remote_url_of(&self, remote: &str) -> Result<String, CoreError> {
        git::run(&self.source_dir, &["remote", "get-url", remote])
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

    /// 上次读到的那份 git 事实：`.git` 里决定 `git status` 输出的那些文件一个都没动就直接复用。
    /// 只给 `diff` 用——点开一行是为了看字，不必为一行字再问一遍「哪里有改动」（#42-a）。
    ///
    /// 失效条件：分支/HEAD、索引、refs、`FETCH_HEAD`、配置、忽略规则里任意一处变了，或者
    /// 那份事实压根还没读过。工作区内容的改动不落进 `.git`，戳看不出来，所以复用到的只可能是
    /// 「上一次 `refresh` 那一刻的事实」：与用户正看着的那一行同源，不会互相打脸。
    /// 要「现在到底有没有未提交改动」的调用方必须走 `facts_fresh`。
    fn facts(&self) -> Result<Arc<state::GitFacts>, CoreError> {
        let stamp = git_stamp(&self.source_dir);
        if let Some(cached) = &self.caches.lock().facts
            && Some(&cached.stamp) == stamp.as_ref()
        {
            return Ok(Arc::clone(&cached.facts));
        }
        self.read_facts(stamp)
    }

    /// 重新读一份 git 事实，不看缓存；读完照样把「戳 + 事实」存进去供 `diff` 复用。
    ///
    /// 界面上的「有没有未提交改动」必须现读：在编辑器里改一个受管文件、`git rm`、新建未跟踪文件，
    /// 一个 `.git` 里的字节都不动，光靠戳分不出「改了」和「没改」——拿旧事实就会一直显示 0 处改动，
    /// 直到有别的动作改写 `.git`（#42-a 复审 B1）。
    fn facts_fresh(&self) -> Result<Arc<state::GitFacts>, CoreError> {
        self.read_facts(git_stamp(&self.source_dir))
    }

    /// 读一份新事实。读之前之后戳一致才进缓存：有别的进程正在改仓库就不缓存，宁可下次再慢一次，
    /// 也不留一份来历不明的事实给 `diff`。
    fn read_facts(
        &self,
        stamp: Option<Vec<(PathBuf, Option<Stamp>)>>,
    ) -> Result<Arc<state::GitFacts>, CoreError> {
        let facts = Arc::new(state::GitFacts::read(&self.source_dir)?);
        if let Some(stamp) = stamp
            && git_stamp(&self.source_dir).as_ref() == Some(&stamp)
        {
            self.caches.lock().facts = Some(FactsCache {
                stamp,
                facts: Arc::clone(&facts),
            });
        }
        Ok(facts)
    }

    /// 一次刷新要的两半（清单 + 状态条），同一份 git 事实算出来：
    /// 前端原来分两次 IPC 各跑一遍 git，现在一次 IPC、一份 `git status`（#19）。
    pub fn refresh(&self) -> Result<Refresh, CoreError> {
        let facts = self.facts_fresh()?;
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
        // 补齐的删除行与 `rels` 都排好序、互不相交：一次线性归并，不再逐条 `Vec::insert`
        // 搬尾部（D 条删除时那是 O(D×N) 指针搬移，#42-5）。输出顺序不变。
        if !deleted.is_empty() {
            let mut extra: Vec<PathBuf> = deleted.iter().cloned().collect();
            extra.sort_unstable();
            rels = merge_sorted(rels, extra);
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
        let facts = self.facts()?;
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

    /// 先比长度：不相等直接不同，相等才读内容（#20）。两侧 `(长度, mtime)` 都没变时
    /// 直接沿用上次的结论，一个字节都不读（#42-1）。
    ///
    /// shortcut: 判「没变」看的是 `(长度, mtime, ctime, inode)` 这一组廉价事实，不是内容。
    /// 现实里能骗过它的向量只有「连 ctime 都还原」的那一档：`cp -p` / `rsync -a` /
    /// `tar -xpf` / 显式 `set_modified` 会保留 mtime，但 ctime 用户态改不回去，进程写文件
    /// 也一定把 ctime 推到当刻——所以「保留 mtime 的恢复」这条路是关死的；同一个文件被
    /// 原地等长改写、且改动落在同一个时间粒度内又被下一次刷新前 ctime 抹平，才可能漏。
    /// 漏了也只是跳过读内容，结论仍只有 Same / Differs 两档，下游判等不变。
    fn disk(&self, rel: &Path) -> Result<state::Disk, CoreError> {
        let target = self.home.join(rel);
        let (target_len, target_stamp) = match std::fs::metadata(&target) {
            Ok(meta) if meta.is_file() => (meta.len(), stamp(&meta)),
            // 目录等非普通文件：走原来的读错误路径，错误类型与文案不变（#20 N3）。
            Ok(_) => (
                std::fs::read(&target)
                    .map(|bytes| bytes.len() as u64)
                    .map_err(|e| CoreError::io(format!("读取 {} 失败：{e}", target.display())))?,
                None,
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.caches.lock().disk.remove(rel);
                return Ok(state::Disk::Missing);
            }
            Err(e) => {
                return Err(CoreError::io(format!(
                    "读取 {} 失败：{e}",
                    target.display()
                )));
            }
        };

        let source = self.source_dir.join(paths::MIRROR_ROOT).join(rel);
        let source_meta = std::fs::metadata(&source)
            .map_err(|e| CoreError::io(format!("读取 {} 失败：{e}", source.display())))?;
        if source_meta.len() != target_len {
            self.caches.lock().disk.remove(rel);
            return Ok(state::Disk::Differs);
        }

        let (Some(target_stamp), Some(source_stamp)) = (target_stamp, stamp(&source_meta)) else {
            // 拿不到 mtime：不进缓存，老实读内容。
            return read_both(&target, &source);
        };
        if let Some(cached) = self.caches.lock().disk.get(rel)
            && cached.target == target_stamp
            && cached.source == source_stamp
        {
            return Ok(cached.disk);
        }

        let disk = read_both(&target, &source)?;
        self.caches.lock().disk.insert(
            rel.to_path_buf(),
            DiskStamp {
                target: target_stamp,
                source: source_stamp,
                disk,
            },
        );
        Ok(disk)
    }

    /// 顶部状态条：分支、ahead/behind、未提交改动数。
    ///
    /// 设置的分支就是「推送会送到哪里」：两者不一致时状态条要说清（`setting` 非空），
    /// ahead/behind 也按**目标分支**的远端跟踪 ref 算——按当前分支的 `@{u}` 算出来的数
    /// 会和点下推送后的结果对不上（同一个会话里两个数字互相打脸）。
    fn status(&self, facts: &state::GitFacts) -> Result<RepoStatus, CoreError> {
        let branch = facts.branch();
        if branch.is_empty() {
            return Err(CoreError {
                kind: ErrorKind::Git,
                message: "源目录当前不在任何分支上（detached HEAD）".to_string(),
                stderr: None,
            });
        }

        let setting = self.config.branch.trim().to_string();
        let dirty = facts.dirty_count();
        if setting.is_empty() || setting == branch {
            let (ahead, behind) = facts.ahead_behind();
            return Ok(RepoStatus {
                branch: branch.to_string(),
                ahead,
                behind,
                dirty,
                setting: None,
            });
        }

        // 状态条上的数字必须等于「按下推送真会送出多少」：推送从设置分支自己的本地 ref
        // 出发、落到它的远端跟踪 ref，所以区间两端都用设置分支，不用 HEAD（HEAD 可能停在
        // 别的分支上，那样数出来既不是当前分支也不是设置分支的）。
        // 每次刷新都要这几个数，而刷新是 fs 事件驱动的：先花一个 `for-each-ref` 拿全 ref
        // 事实，ref 一个都没动就复用上次的数，一次 git 都不再起（#42-4）。读不出来（仓库坏掉
        // 之类）就当没有这些 ref：这条路上的 git 调用以前也不让状态条整条报错。
        let refs = SettingRefs::read(&self.source_dir, &setting).unwrap_or_default();
        let cached = self.caches.lock().fork_status.clone();
        let (ahead, behind) = match cached {
            Some(c) if c.setting == setting && c.refs == refs => (c.ahead, c.behind),
            _ => {
                let remote = branch_remote(&self.source_dir, &setting);
                let count = |rev: String| push::count_commits(&self.source_dir, &rev).unwrap_or(0);
                let local = refs.local.is_some();
                let numbers = match (refs.remotes.contains_key(&remote), local) {
                    (true, true) => {
                        let base = format!("{remote}/{setting}");
                        // 一次数两个方向：left = 远端独有 = behind，right = 本地独有 = ahead。
                        push::count_between(&self.source_dir, &base, &setting).unwrap_or((0, 0))
                    }
                    // 本地还没有这个分支：推送会先按远端建它（或新建一条空分支），没有待推的
                    // 东西，也谈不上落后；状态条上的「还没切过去」就是说的这一档。
                    (true, false) => (0, 0),
                    (false, _) => (count(setting.clone()), 0),
                };
                self.caches.lock().fork_status = Some(ForkStatus {
                    setting: setting.clone(),
                    refs,
                    ahead: numbers.0,
                    behind: numbers.1,
                });
                numbers
            }
        };

        Ok(RepoStatus {
            branch: branch.to_string(),
            ahead,
            behind,
            dirty,
            setting: Some(setting),
        })
    }
}

/// 两侧 `(长度, mtime)`：拿不到 mtime（少见）就 `None`，调用方退回老实读内容。
fn stamp(meta: &std::fs::Metadata) -> Option<Stamp> {
    meta.modified().ok().map(|mtime| Stamp {
        len: meta.len(),
        mtime,
        ctime: meta.ctime(),
        ctime_nsec: meta.ctime_nsec(),
        ino: meta.ino(),
    })
}

/// `.git` 里决定 `git status` 输出的那些文件，以及它们此刻的身份（#42-a）。
///
/// 工作区内容不在里面：改一个受管文件不会动 `.git` 一个字节，那一路归清单自己比。
/// 文件不存在记作 `None`，后来出现了也算变了。`.git` 不是目录（worktree / submodule
/// 的 gitdir 文件）就返回 `None`：说不清就不用缓存，只慢一点，不会错。
fn git_stamp(source_dir: &Path) -> Option<Vec<(PathBuf, Option<Stamp>)>> {
    let git_dir = source_dir.join(".git");
    if !git_dir.is_dir() {
        return None;
    }
    let mut files = vec![
        git_dir.join("HEAD"),
        git_dir.join("index"),
        git_dir.join("packed-refs"),
        git_dir.join("FETCH_HEAD"),
        git_dir.join("config"),
        git_dir.join("info").join("exclude"),
        source_dir.join(".gitignore"),
    ];
    if !collect_files(&git_dir.join("refs"), &mut files) {
        return None;
    }
    files.sort();
    Some(
        files
            .into_iter()
            .map(|path| {
                let stamp = std::fs::symlink_metadata(&path)
                    .ok()
                    .and_then(|meta| stamp(&meta));
                (path, stamp)
            })
            .collect(),
    )
}

/// 把 `dir` 下的普通文件全收进来（`refs` 里分支、标签各占一个文件）。
///
/// 条目读不出类型就返回 `false`，让整份戳作废：漏一个 ref 却声明「没变」是静默错，
/// 整份不缓存只是多读一遍（#49 复审）。
fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return true;
    };
    for entry in entries.flatten() {
        // 不跟随符号链接：`refs` 里不该有，有也不该顺着它绕出去（`file_type` 不解析链接）。
        let Ok(kind) = entry.file_type() else {
            return false;
        };
        let path = entry.path();
        if kind.is_dir() {
            if !collect_files(&path, out) {
                return false;
            }
        } else {
            out.push(path);
        }
    }
    true
}

/// 两侧长度已确认相等时，读内容比字节（错误文案与判等口径不变）。
fn read_both(target: &Path, source: &Path) -> Result<state::Disk, CoreError> {
    #[cfg(test)]
    DISK_READS.fetch_add(2, std::sync::atomic::Ordering::Relaxed);
    let target_bytes = std::fs::read(target)
        .map_err(|e| CoreError::io(format!("读取 {} 失败：{e}", target.display())))?;
    let source_bytes = std::fs::read(source)
        .map_err(|e| CoreError::io(format!("读取 {} 失败：{e}", source.display())))?;
    Ok(if source_bytes == target_bytes {
        state::Disk::Same
    } else {
        state::Disk::Differs
    })
}

/// #42-1 的取证用：`read_both` 每读一次内容就自增，单元测试据此断言缓存真的省掉了读。
#[cfg(test)]
static DISK_READS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// 两条已排序、互不相交的路径表线性归并；`PathBuf` 的 `Ord` 与 `sort_unstable` 同一个口径。
fn merge_sorted(left: Vec<PathBuf>, right: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut merged = Vec::with_capacity(left.len() + right.len());
    let mut right = right.into_iter().peekable();
    for path in left {
        while right.peek().is_some_and(|next| next < &path) {
            merged.extend(right.next());
        }
        merged.push(path);
    }
    merged.extend(right);
    merged
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::*;
    use crate::apply::EXECUTE_SOURCE_READS;
    use crate::git;

    /// 一个真 git 工作区：`main` 上一次提交，里面一条受管文件 `home/profile`。
    fn repo(root: &Path) -> PathBuf {
        let source_dir = root.join("source");
        std::fs::create_dir_all(&source_dir).unwrap();
        for args in [
            &["init", "--initial-branch=main"][..],
            &["config", "user.name", "config-sync test"][..],
            &["config", "user.email", "test@config-sync.invalid"][..],
            &["config", "commit.gpgsign", "false"][..],
        ] {
            git::run(&source_dir, args).unwrap();
        }
        let mirror = source_dir.join(paths::MIRROR_ROOT);
        std::fs::create_dir_all(&mirror).unwrap();
        std::fs::write(mirror.join("profile"), "export A=1\n").unwrap();
        git::run(&source_dir, &["add", "-A"]).unwrap();
        git::run(&source_dir, &["commit", "-m", "init"]).unwrap();
        source_dir
    }

    fn home(root: &Path) -> PathBuf {
        let home = root.join("home-dir");
        std::fs::create_dir_all(&home).unwrap();
        home
    }

    #[test]
    fn merge_sorted_keeps_one_ascending_order() {
        let p = |name: &str| PathBuf::from(name);
        let all = ["a", "b", "c", "d", "e"].map(p);
        assert_eq!(
            merge_sorted(vec![p("a"), p("c"), p("e")], vec![p("b"), p("d")]),
            all
        );
        // 一侧为空、两侧首尾交错、以及一条都不重合的边角。
        assert_eq!(merge_sorted(Vec::new(), all.to_vec()), all);
        assert_eq!(merge_sorted(all.to_vec(), Vec::new()), all);
        assert_eq!(
            merge_sorted(vec![p("z")], vec![p("a")]),
            vec![p("a"), p("z")]
        );
    }

    /// #42-1：两侧 `(长度, mtime)` 都没变就不再读内容；变了立刻失效。
    #[test]
    fn unchanged_metadata_skips_rereading_both_sides() {
        let tmp = tempfile::tempdir().unwrap();
        let home = home(tmp.path());
        let core = Core::new(&home, repo(tmp.path()), Config::default());
        let rel = Path::new("profile");
        std::fs::write(home.join(rel), "export A=1\n").unwrap();

        assert_eq!(core.disk(rel).unwrap(), state::Disk::Same);
        let first = DISK_READS.load(Ordering::Relaxed);
        assert!(first >= 2, "第一次必须读两侧内容，实际读了 {first} 次");

        assert_eq!(core.disk(rel).unwrap(), state::Disk::Same);
        assert_eq!(
            DISK_READS.load(Ordering::Relaxed),
            first,
            "两侧身份 (长度, mtime, ctime, inode) 都没变就不该再读内容"
        );

        // 等长改写（只有时间戳变）：短路必须失效，判等仍按内容算。
        std::fs::write(home.join(rel), "export B=1\n").unwrap();
        assert_eq!(core.disk(rel).unwrap(), state::Disk::Differs);
        assert_eq!(
            DISK_READS.load(Ordering::Relaxed),
            first + 2,
            "mtime 变了就得重读两侧"
        );

        // 「保留 mtime 的恢复」这一档：`cp -p` / `rsync -a` / `tar -xpf` 干的就是这件事。
        // 长度一样、mtime 还被显式改回原值，只有 ctime 会在用户态之外被推走——短路必须失效，
        // 否则用旧备份覆盖回去会被当成「没变」。
        let stamp = std::fs::metadata(home.join(rel))
            .unwrap()
            .modified()
            .unwrap();
        std::fs::write(home.join(rel), "export A=1\n").unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(home.join(rel))
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(stamp))
            .unwrap();
        assert_eq!(
            std::fs::metadata(home.join(rel))
                .unwrap()
                .modified()
                .unwrap(),
            stamp,
            "mtime 已经还原成原值，这一档只能靠 ctime 认出来"
        );
        assert_eq!(
            core.disk(rel).unwrap(),
            state::Disk::Same,
            "mtime 还原但内容确实换回了源侧那份，按内容判仍是 Same"
        );
        assert_eq!(
            DISK_READS.load(Ordering::Relaxed),
            first + 4,
            "ctime 变了就必须重读，不许吃缓存"
        );
    }

    /// #42-2：出计划时读过的源内容，执行阶段源没变就不再读第二遍；变了必须重读并拒绝写。
    #[test]
    fn the_execute_phase_reuses_the_source_bytes_read_by_the_plan() {
        let tmp = tempfile::tempdir().unwrap();
        let home = home(tmp.path());
        let source_dir = repo(tmp.path());
        let core = Core::new(&home, &source_dir, Config::default());

        let plan = core.plan_apply(None).unwrap();
        assert_eq!(plan.items.len(), 1);
        assert_eq!(plan.items[0].action, ApplyAction::Create);

        EXECUTE_SOURCE_READS.store(0, Ordering::Relaxed);
        let outcomes = core.execute_apply(&plan).unwrap();
        assert!(matches!(outcomes[0].result, ApplyResult::Written));
        assert_eq!(
            EXECUTE_SOURCE_READS.load(Ordering::Relaxed),
            0,
            "源文件没变时执行阶段不该再读一遍"
        );
        assert_eq!(
            std::fs::read(home.join("profile")).unwrap(),
            b"export A=1\n"
        );

        // 预览之后源文件被改了：内存里那份不能用，必须重读并报「改过了」，目标不动。
        std::fs::write(home.join("profile"), "export Z=9\n").unwrap();
        let plan = core.plan_apply(None).unwrap();
        assert_eq!(plan.items[0].action, ApplyAction::Overwrite);
        std::fs::write(
            source_dir.join(paths::MIRROR_ROOT).join("profile"),
            "export SNEAKY=1\n",
        )
        .unwrap();
        EXECUTE_SOURCE_READS.store(0, Ordering::Relaxed);
        let outcomes = core.execute_apply(&plan).unwrap();
        assert_eq!(
            EXECUTE_SOURCE_READS.load(Ordering::Relaxed),
            1,
            "源变了就必须重读一遍"
        );
        assert!(matches!(
            &outcomes[0].result,
            ApplyResult::Failed { message } if message.contains("改过")
        ));
        assert_eq!(
            std::fs::read(home.join("profile")).unwrap(),
            b"export Z=9\n",
            "改过的源不许写下去"
        );
    }

    /// #42-4：设置分支 ≠ 当前分支时，ref 没动就只起一次 `for-each-ref`。
    #[test]
    fn a_settled_setting_branch_costs_one_git_process_per_refresh() {
        let tmp = tempfile::tempdir().unwrap();
        let home = home(tmp.path());
        let source_dir = repo(tmp.path());
        // 本地有 release、远端也有一份（同一个提交，ahead/behind 都是 0）。
        git::run(&source_dir, &["branch", "release"]).unwrap();
        git::run(
            &source_dir,
            &["update-ref", "refs/remotes/origin/release", "HEAD"],
        )
        .unwrap();

        // 基线：设置分支 == 当前分支，状态条只花 `git status` 那一次。
        let baseline = {
            let core = Core::new(&home, &source_dir, Config::default());
            git::reset_git_runs();
            core.repo_status().unwrap();
            git::git_runs()
        };

        let core = Core::new(
            &home,
            &source_dir,
            Config {
                branch: "release".to_string(),
                ..Config::default()
            },
        );
        git::reset_git_runs();
        let cold = core.repo_status().unwrap();
        let cold_runs = git::git_runs();
        assert_eq!(cold.setting.as_deref(), Some("release"));

        git::reset_git_runs();
        let warm = core.repo_status().unwrap();
        let warm_runs = git::git_runs();

        assert_eq!(warm, cold, "缓存不许改变结论");
        assert_eq!(
            warm_runs,
            baseline + 1,
            "ref 没动时只该起一次 for-each-ref（基线 {baseline} 次）"
        );
        assert!(
            cold_runs > warm_runs,
            "冷启动还要解析远端名并计数：{cold_runs} 次 vs 复用时的 {warm_runs} 次"
        );
        // 冷启动比基线多的那一截，正是 #42-4 要消掉的那串：远端名（1-2 次）+ 分支计数。
        assert!(
            cold_runs - baseline >= 4,
            "冷启动 {cold_runs} 次 vs 基线 {baseline} 次"
        );
    }

    /// #42-a：刷新刚读过的 git 事实，点开一行 diff 直接复用，不再起一个 `git status`。
    #[test]
    fn a_row_diff_reuses_the_facts_refresh_already_read() {
        let tmp = tempfile::tempdir().unwrap();
        let home = home(tmp.path());
        let core = Core::new(&home, repo(tmp.path()), Config::default());
        let target = home.join("profile");
        std::fs::write(&target, "export A=1\n").unwrap();

        // 改前：diff 自己再读一遍 git 事实，凭空多一个进程。
        git::reset_git_runs();
        let cold = core.diff(&target).unwrap();
        let cold_runs = git::git_runs();

        core.refresh().unwrap();
        git::reset_git_runs();
        let warm = core.diff(&target).unwrap();
        let warm_runs = git::git_runs();

        assert_eq!(warm, cold, "复用缓存的 facts 不许改变 diff 的结论");
        assert_eq!(cold_runs, 1, "改前每点开一行就是一次 git status");
        assert_eq!(
            warm_runs, 0,
            "刷新之后点开一行：{cold_runs} 个 git 进程 → {warm_runs} 个"
        );
    }

    /// #42-a：索引/ref 一动，缓存的事实立刻作废，结论跟着变。
    #[test]
    fn a_staged_change_invalidates_the_reused_facts() {
        let tmp = tempfile::tempdir().unwrap();
        let home = home(tmp.path());
        let source_dir = repo(tmp.path());
        let core = Core::new(&home, &source_dir, Config::default());
        let target = home.join("profile");
        std::fs::write(&target, "export A=1\n").unwrap();
        assert_eq!(core.refresh().unwrap().entries[0].status, Status::InSync);

        // 源目录里改内容再暂存：`.git/index` 变了，缓存必须当场作废。
        std::fs::write(
            source_dir.join(paths::MIRROR_ROOT).join("profile"),
            "export A=2\n",
        )
        .unwrap();
        git::run(&source_dir, &["add", "-A"]).unwrap();

        let diff = core.diff(&target).unwrap();
        assert_eq!(diff.status, Status::PendingCommit, "暂存了就是待提交");
        assert!(
            diff.text.contains("HEAD:home/profile"),
            "待提交比的是 HEAD 与源文件：{}",
            diff.text
        );

        // 重新读到的那份也进了缓存：接着再点一行不再重读 facts，只剩取 diff 侧内容那一次 git。
        git::reset_git_runs();
        let again = core.diff(&target).unwrap();
        assert_eq!(again, diff, "同一份事实，结论不变");
        assert_eq!(git::git_runs(), 1, "第二次点开只剩读 HEAD 内容的那一次");
    }

    /// #42-a 复审 B1：工作区改了但不碰 `.git`，下一次 `refresh` 也必须看得见。
    ///
    /// 拿旧的 git 事实算清单，就会一直说「0 处未提交改动」、那一行还挂在已同步上，
    /// 直到有别的动作改写 `.git`——界面与推送各自看到的世界不一致。
    #[test]
    fn a_worktree_edit_reaches_the_next_refresh() {
        let tmp = tempfile::tempdir().unwrap();
        let home = home(tmp.path());
        let source_dir = repo(tmp.path());
        let core = Core::new(&home, &source_dir, Config::default());
        std::fs::write(home.join("profile"), "export A=1\n").unwrap();
        let cold = core.refresh().unwrap();
        assert_eq!(cold.status.dirty, 0);
        assert_eq!(cold.entries[0].status, Status::InSync);

        // 编辑器里改源目录的副本：只动工作区，不 add、不 commit，`.git` 一个字节不动。
        std::fs::write(
            source_dir.join(paths::MIRROR_ROOT).join("profile"),
            "export A=2\n",
        )
        .unwrap();

        let warm = core.refresh().unwrap();
        assert_eq!(warm.status.dirty, 1, "工作区改了就得算一处未提交改动");
        assert_eq!(
            warm.entries[0].status,
            Status::PendingCommit,
            "没暂存也已经是待提交改动"
        );
    }
}
