//! 拉取的行为测试：真 git 造「远端领先」「远端分叉」，断言只动源目录、不碰 $HOME。

mod fixtures;

use config_sync_core::{Core, CoreError, ErrorKind, Status};

fn only_status(core: &Core) -> Status {
    let entries = core.inventory().expect("读取清单");
    assert_eq!(entries.len(), 1, "清单应只有一行：{entries:?}");
    entries[0].status
}

#[test]
fn pull_fast_forwards_and_leaves_the_real_files_alone() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();
    f.write_home(".zshrc", "export A=1\n");

    // 「另一台机器」= 远端的第二份工作副本（真 clone），推一个提交上去。
    f.advance_remote("home/.zshrc", "export A=1\nexport B=2\n");
    assert_eq!(only_status(&core), Status::PendingPull);

    core.pull().expect("远端领先时快进拉取应成功");

    // 源目录与远端一致，工作区和历史里都没有 merge commit。
    assert_eq!(f.head_sha(), f.git_remote(&["rev-parse", "main"]));
    assert!(!f.is_dirty(), "拉取后源目录应干净");
    assert_eq!(f.git(&["rev-list", "--count", "--merges", "HEAD"]), "0");

    // $HOME 一个字节都没动：拉取只更新源目录，应用要用户另行点。
    assert_eq!(f.read_home(".zshrc"), "export A=1\n");
    assert_eq!(only_status(&core), Status::Pending, "拉取后该行应变成待应用");

    // 状态条：拉完 behind 归零。
    let status = core.repo_status().expect("读取状态条");
    assert_eq!((status.ahead, status.behind), (0, 0));
}

/// 失败必须是「人话 + git 原始 stderr」，且能靠 message 区分是哪一类。
fn failure(core: &Core) -> CoreError {
    let error = core.pull().expect_err("这一步必须拒绝拉取");
    assert_eq!(error.kind, ErrorKind::Git, "失败种类：{error:?}");
    assert!(
        error.stderr.as_deref().is_some_and(|stderr| !stderr.is_empty()),
        "git 的原始 stderr 必须被保留：{error:?}"
    );
    error
}

#[test]
fn pull_refuses_to_create_a_merge_commit_and_leaves_the_source_dir_clean() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();

    f.advance_remote("home/.zshrc", "export A=1\nexport REMOTE=1\n");
    // 本机也往前走了一步且没推：与远端分叉。
    f.write_source("home/.tmux.conf", "set -g mouse on\n");
    f.commit_all("config-sync: 1 files");
    let head = f.head_sha();

    let error = failure(&core);
    assert!(error.message.contains("需要手动处理"), "message = {}", error.message);
    let stderr = error.stderr.unwrap();
    assert!(stderr.contains("Not possible to fast-forward"), "stderr = {stderr}");

    assert_eq!(f.head_sha(), head, "被拒后 HEAD 不许动");
    assert!(!f.is_dirty(), "被拒后源目录必须干净");
    assert!(!f.source_dir.join(".git/MERGE_HEAD").exists(), "不许留下半合并状态");
    assert_eq!(f.git(&["rev-list", "--count", "--merges", "HEAD"]), "0");

    // 远端仍领先，.zshrc 那一行保持待拉取（本地新增的 .tmux.conf 也在这个集合里，
    // 是 t03 的 `HEAD..@{u}` 反向差分不分方向导致的，与本票无关）。
    let entries = core.inventory().expect("读取清单");
    let zshrc = entries.iter().find(|e| e.target.ends_with(".zshrc")).expect("有 .zshrc 行");
    assert_eq!(zshrc.status, Status::PendingPull, "远端仍领先，状态不变");
}

#[test]
fn pull_reports_missing_upstream_with_gits_own_words() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    // 没 push 过，因此没有上游分支。

    let error = failure(&core);
    assert!(error.message.contains("上游分支"), "message = {}", error.message);
    let stderr = error.stderr.unwrap();
    assert!(stderr.contains("no upstream configured"), "stderr = {stderr}");
}

#[test]
fn pull_reports_missing_remote_with_gits_own_words() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.git(&["remote", "remove", "origin"]);

    let error = failure(&core);
    assert!(error.message.contains("没有配置远端"), "message = {}", error.message);
    let stderr = error.stderr.unwrap();
    assert!(stderr.contains("No such remote"), "stderr = {stderr}");
}

#[test]
fn pull_reports_an_unreachable_remote_with_the_original_stderr() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();
    let missing = format!("file://{}", f.path("missing.git").display());
    f.git(&["remote", "set-url", "origin", &missing]);

    let error = failure(&core);
    assert!(error.message.contains("取回最新提交"), "message = {}", error.message);
    let stderr = error.stderr.unwrap();
    assert!(stderr.contains("does not appear to be a git repository"), "stderr = {stderr}");
    assert!(!f.is_dirty(), "fetch 失败后源目录必须干净");
}
