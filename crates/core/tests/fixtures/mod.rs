//! 真 git 测试夹具。
//!
//! 每个夹具在临时目录里建齐三样东西：
//!
//! ```text
//! <tmp>/home/       临时 $HOME（目标文件的根）
//! <tmp>/source/     临时源目录，一个真 git 工作区，origin 指向下面的裸仓库
//! <tmp>/remote.git  临时裸仓库，以 file:// 路径充当远端
//! ```
//!
//! 只用系统 `git` 与真文件系统：不 mock、不用内存文件系统。

#![allow(dead_code)] // 夹具是给后续所有票据共用的测试库，单个测试文件未必用到全部助手。

use std::path::{Path, PathBuf};
use std::process::Command;

use config_sync_core::{Config, Core};

pub struct Fixture {
    // 持有 TempDir 保证退出时删除临时树。
    _tmp: tempfile::TempDir,
    pub home: PathBuf,
    pub source_dir: PathBuf,
    pub remote: PathBuf,
}

impl Fixture {
    pub fn new() -> Self {
        let tmp = tempfile::tempdir().expect("创建临时目录");
        let root = tmp.path();
        let home = root.join("home");
        let source_dir = root.join("source");
        let remote = root.join("remote.git");
        std::fs::create_dir_all(&home).expect("创建临时 home");
        std::fs::create_dir_all(&source_dir).expect("创建临时源目录");

        git(None, root, &["init", "--bare", "--initial-branch=main", path(&remote)]);
        git(None, &source_dir, &["init", "--initial-branch=main"]);
        // 仓库内固定身份与签名开关：测试不受用户全局 gitconfig 影响。
        git(None, &source_dir, &["config", "user.name", "config-sync test"]);
        git(None, &source_dir, &["config", "user.email", "test@config-sync.invalid"]);
        git(None, &source_dir, &["config", "commit.gpgsign", "false"]);
        git(None, &source_dir, &["remote", "add", "origin", &file_url(&remote)]);

        Fixture { _tmp: tmp, home, source_dir, remote }
    }

    /// 用夹具的三个路径构造 Core：`(home, source_dir, config)`。
    pub fn core(&self) -> Core {
        Core::new(&self.home, &self.source_dir, Config::default())
    }

    /// 临时树的根（放第二份工作副本之类的东西，不污染 home 与 source）。
    pub fn tmp_root(&self) -> &Path {
        self.home.parent().expect("临时 home 必有父目录")
    }

    /// 临时树根下的路径。
    pub fn path(&self, rel_path: &str) -> PathBuf {
        self.tmp_root().join(rel_path)
    }

    /// 以 `file://` 形式暴露远端裸仓库，供第二次 clone 使用。
    pub fn remote_url(&self) -> String {
        file_url(&self.remote)
    }

    /// 在任意目录里跑真 `git`（例如第二份工作副本）。
    pub fn git_at(&self, dir: &Path, args: &[&str]) -> String {
        git(None, dir, args)
    }

    /// 在源目录里跑真 `git`，返回裁剪过的 stdout；非零退出直接 panic 并带上 stderr。
    pub fn git(&self, args: &[&str]) -> String {
        git(None, &self.source_dir, args)
    }

    /// 在远端裸仓库里跑真 `git`（断言推送结果用）。
    pub fn git_remote(&self, args: &[&str]) -> String {
        git(None, &self.remote, args)
    }

    /// 源目录当前 HEAD 的 commit sha。
    pub fn head_sha(&self) -> String {
        self.git(&["rev-parse", "HEAD"])
    }

    /// 源目录里是否有未提交改动（含未跟踪文件）。
    pub fn is_dirty(&self) -> bool {
        !self.git(&["status", "--porcelain"]).is_empty()
    }

    /// 在源目录（git 工作区）里写文件，自动建父目录。返回绝对路径。
    pub fn write_source(&self, rel_path: &str, content: &str) -> PathBuf {
        write(&self.source_dir.join(rel_path), content)
    }

    /// 在临时 $HOME 里写文件，自动建父目录。返回绝对路径。
    pub fn write_home(&self, rel_path: &str, content: &str) -> PathBuf {
        write(&self.home.join(rel_path), content)
    }

    /// 读临时 $HOME 里的文件内容。
    pub fn read_home(&self, rel_path: &str) -> String {
        std::fs::read_to_string(self.home.join(rel_path)).expect("读取 home 内的文件")
    }

    /// 提交源目录里所有改动（`git add -A` + commit），返回 sha。
    pub fn commit_all(&self, message: &str) -> String {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-m", message]);
        self.head_sha()
    }

    /// 推送当前分支到远端（建立上游跟踪）。
    pub fn push(&self) {
        self.git(&["push", "-u", "origin", "main"]);
    }

    /// 远端仓库的第二份工作副本（懒建，用来让远端领先）。
    pub fn other(&self) -> PathBuf {
        let dir = self.path("other");
        if !dir.exists() {
            self.git_at(self.tmp_root(), &["clone", &self.remote_url(), "other"]);
            self.git_at(&dir, &["config", "user.email", "test@config-sync.invalid"]);
            self.git_at(&dir, &["config", "user.name", "config-sync test"]);
        }
        dir
    }

    /// 让远端在 `rel_path` 上多一个提交，源目录只 `fetch`（不合并）。
    /// 前提：源目录已经把 `main` 推上去过。
    pub fn advance_remote(&self, rel_path: &str, content: &str) {
        let other = self.other();
        write(&other.join(rel_path), content);
        self.git_at(&other, &["add", "-A"]);
        self.git_at(&other, &["commit", "-m", "config-sync: 1 files"]);
        self.git_at(&other, &["push", "origin", "main"]);
        self.git(&["fetch", "origin"]);
    }

    /// 在源目录里跑真 git，容忍非零退出（制造真冲突的 merge 用），返回是否成功。
    pub fn git_try(&self, args: &[&str]) -> bool {
        Command::new("git")
            .arg("-C")
            .arg(&self.source_dir)
            .args(args)
            .output()
            .expect("启动系统 git")
            .status
            .success()
    }
}

fn path(p: &Path) -> &str {
    p.to_str().expect("临时路径必须是合法 UTF-8")
}

fn file_url(p: &Path) -> String {
    format!("file://{}", p.display())
}

fn write(path: &Path, content: &str) -> PathBuf {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("建父目录");
    }
    std::fs::write(path, content).expect("写文件");
    path.to_path_buf()
}

fn git(cwd: Option<&Path>, dir: &Path, args: &[&str]) -> String {
    let mut cmd = Command::new("git");
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    cmd.arg("-C").arg(dir).args(args);
    let out = cmd.output().expect("启动系统 git");
    assert!(
        out.status.success(),
        "git {args:?} 失败（{}）：{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim_end().to_string()
}
