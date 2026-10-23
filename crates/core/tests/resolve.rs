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

/// 造一个只远端领先、本地没改的干净受管文件（待拉取，非冲突）。
fn make_clean_remote_ahead(f: &fixtures::Fixture) {
    f.write_source("home/.vimrc", "set nocompatible\n");
    f.commit_all("config-sync: 1 files");
    f.push();
    f.write_home(".vimrc", "set nocompatible\n");
    f.advance_remote("home/.vimrc", "set nocompatible\n\" REMOTE\n");
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
    // 解决后远端仍领先（fetch 已取回），这一行从冲突回落为待拉取。
    assert_eq!(only_status(&core), Status::PendingPull, "解决后状态回落待拉取");

    assert_eq!(f.read_home(".zshrc"), "export A=1\n");
    assert_eq!(
        read_source(&f, "home/.zshrc"),
        "export A=1\nexport LOCAL=7\n",
        "源目录应是本机未提交的版本"
    );
    assert!(!f.is_dirty(), "解决后源目录必须干净");

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
    assert_eq!(only_status(&core), Status::Pending, "解决后源/目标不同：远端版尚未应用，目标文件还是旧版 → 待应用");

    assert_eq!(f.read_home(".zshrc"), "export A=1\n");
    assert_eq!(
        read_source(&f, "home/.zshrc"),
        "export A=1\nexport REMOTE=9\n",
        "源目录应是远端版本"
    );
    assert!(!f.is_dirty(), "解决后源目录必须干净");
    // 远端版已写进源目录但也已提交，目标文件还是旧版：待应用（不是待提交）。
    assert_eq!(only_status(&core), Status::Pending, "解决后状态回落待应用");

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

    // 其它行不受影响：.vimrc 的源目录内容没被碰（只 fetch 到 refs，工作区原样），
    // 远端领先仍未解决（fetch 已把它推到 2 commits behind）→ 待拉取。
    let entries = core.inventory().expect("读取清单");
    let vimrc = entries.iter().find(|e| e.target.ends_with(".vimrc")).unwrap();
    assert_eq!(vimrc.status, Status::PendingPull, "其它行不受影响");
    // .vimrc 内容:advance_remote 在源目录 fetch 了远端,但工作区文件保持本地提交的版本
    // （只 fetch 不 merge）；源目录内容不是 REMOTE 版,而是本地提交的 "set nocompatible\n"
    assert_eq!(
        read_source(&f, "home/.vimrc"),
        "set nocompatible\n",
        ".vimrc 工作区不被解决动作碰,保持本地提交版"
    );
    // 但远端已领先,解决 .zshrc 时 fetch 把未来拉取内容取回 → 远端 ref 指向含 REMOTE 的 commit
    // 若 .vimrc 有未来内容,那在远端 commit 里;工作区仍是本地版,此处验证工作区未被写。
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
}

#[test]
fn resolving_a_file_kept_clean_that_is_only_ahead_on_remote_is_a_human_error() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    // 远端领先但本地没改 → 待拉取，不是冲突，不能按文件解决。
    make_clean_remote_ahead(&f);
    let entries = core.inventory().expect("读取清单");
    let vimrc = entries.iter().find(|e| e.target.ends_with(".vimrc")).unwrap();
    assert_eq!(vimrc.status, Status::PendingPull, "待拉取行不该是冲突: {entries:?}");

    let error = core.resolve_conflict(&f.home.join(".vimrc"), Side::Ours).unwrap_err();
    assert_eq!(error.kind, ErrorKind::Io, "{error:?}");
    assert!(error.message.contains("不冲突"), "message = {}", error.message);
}

#[test]
fn resolving_a_missing_file_is_a_human_error() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    make_conflict(&f);

        let error = core.resolve_conflict(&f.home.join(".gitconfig"), Side::Ours).unwrap_err();
        assert_eq!(error.kind, ErrorKind::Io, "{error:?}");
        assert!(error.message.contains("不存在"), "message = {}", error.message);
        // fetch 前校验先拦下：不存在的文件不触发网络调用，源目录保持 make_conflict 留下的真实冲突形态。
        assert!(f.is_dirty(), "冲突形态本身是带未提交改动的，不该被额外清理");
}

#[test]
fn resolving_a_path_outside_home_is_a_human_error() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    make_conflict(&f);

    // 路径在 $HOME 外：最早被拦下，不触发 fetch。
    let error = core.resolve_conflict(Path::new("/etc/hosts"), Side::Ours).unwrap_err();
    assert_eq!(error.kind, ErrorKind::Io, "{error:?}");
    assert!(error.message.contains("不在"), "message = {}", error.message);
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
    assert!(
        stderr.contains("does not appear to be a git repository"),
        "stderr = {stderr}"
    );
    // fetch 失败前用户已有未提交改动（make_conflict 的形态即是）：失败后那些改动都还在，
    // 没被 fetch 弄丢、没留下任何 merge/索引痕迹。
    assert!(
        !f.git(&["status", "--porcelain"]).lines().any(|l| !l.trim().is_empty() && !l.trim_start().starts_with(" M ") && !l.trim_start().starts_with("M ")),
        "fetch 失败不应引入新的索引/未合并条目"
    );
    assert!(f.git(&["status", "--porcelain"]).contains("home/.zshrc"), "原有改动必须保留");
}

fn read_source(f: &fixtures::Fixture, rel: &str) -> String {
    std::fs::read_to_string(f.source_dir.join(rel)).expect("读取源文件")
}