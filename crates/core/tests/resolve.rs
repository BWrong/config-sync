//! 冲突解决的行为测试：真 git 造「两边都改了同一路径」的真实冲突，
//! 断言「用我的 / 用仓库的」只解决这一个文件、重新提交、状态正确回落。

mod fixtures;

use std::path::Path;

use config_sync_core::{Core, ErrorKind, Status, resolve::Side};

/// 造「本地未提交改动 ∧ 远端领先」的真实冲突形态（产品的主形态，非真 merge）。
fn make_conflict(f: &fixtures::Fixture) {
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();
    f.write_home(".zshrc", "export A=1\n");
    f.advance_remote("home/.zshrc", "export A=1\nexport REMOTE=9\n");
    f.write_source("home/.zshrc", "export A=1\nexport LOCAL=7\n");
}

fn only_status(core: &Core) -> Status {
    let entries = core.inventory().expect("读取清单");
    assert_eq!(entries.len(), 1, "清单应只有一行：{entries:?}");
    entries[0].status
}

fn target(f: &fixtures::Fixture) -> std::path::PathBuf {
    f.home.join(".zshrc")
}

#[test]
fn ours_keeps_the_local_version_and_commits() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    make_conflict(&f);
    assert_eq!(only_status(&core), Status::Conflict);

    core.resolve_conflict(&target(&f), Side::Ours).expect("保留本机版本应成功");

    assert_eq!(f.read_home(".zshrc"), "export A=1\n");
    assert_eq!(
        read_source(&f, "home/.zshrc"),
        "export A=1\nexport LOCAL=7\n",
        "源目录应是本机未提交的版本"
    );
    assert!(!f.is_dirty(), "解决后源目录必须干净");
    assert_eq!(only_status(&core), Status::PendingCommit, "解决后状态回落待提交");

    // 提交信息反映了本机版本。
    let log = f.git(&["log", "-1", "--format=%s"]);
    assert!(log.contains("resolve conflict: home/.zshrc"), "log = {log}");
    assert!(log.ends_with("(ours)"), "log = {log}");
}

#[test]
fn theirs_restores_the_remote_version_and_commits() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    make_conflict(&f);
    assert_eq!(only_status(&core), Status::Conflict);

    core.resolve_conflict(&target(&f), Side::Theirs).expect("采用远端版本应成功");

    assert_eq!(f.read_home(".zshrc"), "export A=1\n");
    assert_eq!(
        read_source(&f, "home/.zshrc"),
        "export A=1\nexport REMOTE=9\n",
        "源目录应是远端版本"
    );
    assert!(!f.is_dirty(), "解决后源目录必须干净");
    assert_eq!(only_status(&core), Status::PendingCommit, "解决后状态回落待提交");

    let log = f.git(&["log", "-1", "--format=%s"]);
    assert!(log.ends_with("(theirs)"), "log = {log}");
}

#[test]
fn resolving_one_file_leaves_other_conflicts_and_other_dirty_files_alone() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    f.write_source("home/.zshrc", "export A=1\n");
    f.write_source("home/.vimrc", "set nocompatible\n");
    f.commit_all("config-sync: 2 files");
    f.push();
    f.write_home(".zshrc", "export A=1\n");
    f.write_home(".vimrc", "set nocompatible\n");
    f.advance_remote("home/.zshrc", "export A=1\nexport REMOTE=9\n");
    f.advance_remote("home/.vimrc", "set nocompatible\n\" REMOTE\n");
    f.write_source("home/.zshrc", "export A=1\nexport LOCAL=7\n");

    // 只有 .zshrc 是冲突（.vimrc 本地没改，只是远端领先 → 待拉取）。
    let entries = core.inventory().expect("读取清单");
    let zshrc = entries.iter().find(|e| e.target.ends_with(".zshrc")).unwrap();
    let vimrc = entries.iter().find(|e| e.target.ends_with(".vimrc")).unwrap();
    assert_eq!(zshrc.status, Status::Conflict);
    assert_eq!(vimrc.status, Status::PendingPull);

    core.resolve_conflict(&f.home.join(".zshrc"), Side::Ours).expect("解决 .zshrc");

    // 其它行不受影响：.vimrc 仍是远端领先，源目录 .vimrc 内容没动。
    let entries = core.inventory().expect("读取清单");
    let vimrc = entries.iter().find(|e| e.target.ends_with(".vimrc")).unwrap();
    assert_eq!(vimrc.status, Status::PendingPull, "其它行不受影响");
    assert!(read_source(&f, "home/.vimrc").contains("REMOTE"), ".vimrc 不该被解决动作碰");
}

#[test]
fn resolving_a_non_conflicting_file_is_a_human_error() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    // 一个完全干净一致的文件，不在冲突状态。
    f.write_source("home/.vimrc", "set nocompatible\n");
    f.commit_all("config-sync: 1 files");
    f.push();
    f.write_home(".vimrc", "set nocompatible\n");

    let error = core.resolve_conflict(&f.home.join(".vimrc"), Side::Ours).unwrap_err();
    assert_eq!(error.kind, ErrorKind::Io, "{error:?}");
    assert!(error.message.contains("不冲突"), "message = {}", error.message);
    assert!(!f.is_dirty(), "报错后源目录必须干净");
}

#[test]
fn resolving_a_missing_file_is_a_human_error() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    make_conflict(&f);

    let missing = f.home.join(".gitconfig");
    let error = core.resolve_conflict(&missing, Side::Ours).unwrap_err();
    assert_eq!(error.kind, ErrorKind::Io, "{error:?}");
    assert!(error.message.contains("不存在"), "message = {}", error.message);
    assert!(!f.is_dirty());
}

#[test]
fn resolving_a_path_outside_home_is_a_human_error() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    make_conflict(&f);

    let error = core.resolve_conflict(Path::new("/etc/hosts"), Side::Ours).unwrap_err();
    assert_eq!(error.kind, ErrorKind::Io, "{error:?}");
    assert!(error.message.contains("不在"), "message = {}", error.message);
    assert!(!f.is_dirty());
}

#[test]
fn resolving_a_real_merge_conflict_uses_the_git_checkout_semantics() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();
    f.advance_remote("home/.zshrc", "export A=1\nexport REMOTE=9\n");
    f.write_source("home/.zshrc", "export A=1\nexport LOCAL=7\n");
    f.commit_all("config-sync: 1 files");
    // 真合并留下未合并状态（索引 stage 2/3）。
    assert!(!f.git_try(&["merge", "origin/main"]), "merge 应因路径冲突失败");
    assert_eq!(only_status(&core), Status::Conflict);

    core.resolve_conflict(&target(&f), Side::Theirs).expect("采用远端版本应成功");

    assert_eq!(
        read_source(&f, "home/.zshrc"),
        "export A=1\nexport REMOTE=9\n",
        "真合并下 checkout --theirs 应取远端版本"
    );
    assert!(!f.is_dirty(), "解决后源目录必须干净");
    let log = f.git(&["log", "-1", "--format=%s"]);
    assert!(log.ends_with("(theirs)"), "log = {log}");
}

#[test]
fn a_failed_fetch_carries_the_original_stderr() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    make_conflict(&f);
    let missing = format!("file://{}", f.path("missing.git").display());
    f.git(&["remote", "set-url", "origin", &missing]);

    let error = core.resolve_conflict(&target(&f), Side::Theirs).unwrap_err();
    assert_eq!(error.kind, ErrorKind::Git, "{error:?}");
    assert!(error.message.contains("取回最新提交"), "message = {}", error.message);
    let stderr = error.stderr.unwrap();
    assert!(stderr.contains("does not appear to be a git repository"), "stderr = {stderr}");
    assert!(!f.is_dirty(), "fetch 失败后源目录必须干净");
}

fn read_source(f: &fixtures::Fixture, rel: &str) -> String {
    std::fs::read_to_string(f.source_dir.join(rel)).expect("读取源文件")
}