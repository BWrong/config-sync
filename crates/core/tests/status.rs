//! 清单六态的行为测试：每种状态都用真 git（临时 home + 临时源目录 + 临时裸远端）造出来。

mod fixtures;

use config_sync_core::{Core, Status};

/// 清单里唯一那一行的状态。
fn only_status(core: &Core, context: &str) -> Status {
    let entries = core.inventory().expect("读取清单");
    assert_eq!(entries.len(), 1, "{context}：清单应只有一行，实际 {entries:?}");
    entries[0].status
}

#[test]
fn in_sync_when_source_and_target_agree_and_the_repo_is_clean() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();
    f.write_home(".zshrc", "export A=1\n");

    assert_eq!(only_status(&core, "干净且内容相同"), Status::InSync);
}

#[test]
fn target_missing_when_the_source_side_is_committed_and_clean() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    f.write_source("home/.config/nvim/init.lua", "-- 空\n");
    f.commit_all("config-sync: 1 files");
    f.push();

    assert_eq!(only_status(&core, "目标不存在"), Status::TargetMissing);
}

#[test]
fn pending_when_the_target_differs_and_the_source_side_is_clean() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();
    f.write_home(".zshrc", "export A=2\n");

    assert_eq!(only_status(&core, "目标被本地改过"), Status::Pending);
}

#[test]
fn pending_commit_when_the_source_file_has_uncommitted_changes() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();
    // 目标与已提交版本一致，只有源目录多了一段没提交的改动。
    f.write_home(".zshrc", "export A=1\n");
    f.write_source("home/.zshrc", "export A=1\nexport B=2\n");

    assert_eq!(only_status(&core, "源目录有未提交改动"), Status::PendingCommit);
}

#[test]
fn pending_pull_when_the_remote_changed_a_path_the_worktree_did_not() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();
    f.write_home(".zshrc", "export A=1\n");
    f.advance_remote("home/.zshrc", "export A=1\nexport B=9\n");

    assert_eq!(only_status(&core, "远端领先且改了同一路径"), Status::PendingPull);
}

#[test]
fn conflict_when_a_local_uncommitted_change_meets_a_remote_change() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();
    f.write_home(".zshrc", "export A=1\n");
    f.advance_remote("home/.zshrc", "export A=1\nexport REMOTE=9\n");
    f.write_source("home/.zshrc", "export A=1\nexport LOCAL=7\n");

    assert_eq!(only_status(&core, "两边都改了同一路径"), Status::Conflict);
}

#[test]
fn conflict_when_a_real_merge_left_the_path_unmerged() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();
    f.write_home(".zshrc", "export A=1\n");
    f.advance_remote("home/.zshrc", "export A=1\nexport REMOTE=9\n");
    f.write_source("home/.zshrc", "export A=1\nexport LOCAL=7\n");
    f.commit_all("config-sync: 1 files");

    assert!(!f.git_try(&["merge", "origin/main"]), "这次 merge 必须冲突");
    assert_eq!(only_status(&core, "merge 留下未合并路径"), Status::Conflict);
}

/// 表驱动：(本地未提交改动 × 远端是否改了同一路径 × 目标文件) → 状态。
#[derive(Debug, Clone, Copy)]
enum Target {
    Missing,
    Same,
    Differs,
}

const CASES: &[(bool, bool, Target, Status)] = &[
    (false, false, Target::Missing, Status::TargetMissing),
    (false, false, Target::Same, Status::InSync),
    (false, false, Target::Differs, Status::Pending),
    (true, false, Target::Missing, Status::PendingCommit),
    (true, false, Target::Same, Status::PendingCommit),
    (true, false, Target::Differs, Status::PendingCommit),
    (false, true, Target::Missing, Status::PendingPull),
    (false, true, Target::Same, Status::PendingPull),
    (false, true, Target::Differs, Status::PendingPull),
    (true, true, Target::Missing, Status::Conflict),
    (true, true, Target::Same, Status::Conflict),
    (true, true, Target::Differs, Status::Conflict),
];

#[test]
fn status_table_covers_git_state_target_presence_and_remote_lead() {
    for &(dirty, remote_ahead, target, expected) in CASES {
        let f = fixtures::Fixture::new();
        let core = f.core();
        f.write_source("home/.zshrc", "export A=1\n");
        f.commit_all("config-sync: 1 files");
        f.push();

        let source_content = if dirty { "export A=1\nexport LOCAL=7\n" } else { "export A=1\n" };
        if dirty {
            f.write_source("home/.zshrc", source_content);
        }
        if remote_ahead {
            f.advance_remote("home/.zshrc", "export A=1\nexport REMOTE=9\n");
        }
        match target {
            Target::Missing => {}
            Target::Same => {
                f.write_home(".zshrc", source_content);
            }
            Target::Differs => {
                f.write_home(".zshrc", "export A=1\nexport DISK=3\n");
            }
        }

        let context = format!("dirty={dirty} remote_ahead={remote_ahead} target={target:?}");
        assert_eq!(only_status(&core, &context), expected, "{context}");
    }
}

#[test]
fn status_follows_a_commit_made_outside_the_app_without_restarting() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();
    f.write_home(".zshrc", "export A=1\n");
    f.write_source("home/.zshrc", "export A=1\nexport B=2\n");
    assert_eq!(only_status(&core, "改动还没提交"), Status::PendingCommit);

    // 用户切到终端用系统 git 提交，界面这边只是再读一次清单。
    f.git(&["add", "-A"]);
    f.git(&["commit", "-m", "config-sync: 1 files"]);
    assert_eq!(only_status(&core, "外部提交之后"), Status::Pending);

    // 外部把提交推上去、远端再前进一步，也只是重新推导。
    f.write_home(".zshrc", "export A=1\nexport B=2\n");
    assert_eq!(only_status(&core, "外部提交且目标已跟上"), Status::InSync);
}

#[test]
fn status_bar_counts_match_git_status_after_the_remote_moves() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();
    f.advance_remote("home/.zshrc", "export A=1\nexport B=9\n");
    f.write_source("home/.gitconfig", "[user]\n\tname = 测试\n");

    let status = core.repo_status().unwrap();
    assert_eq!(status.branch, "main");
    assert_eq!((status.ahead, status.behind), (0, 1));
    assert_eq!(status.dirty, f.git(&["status", "--porcelain"]).lines().count() as u32);
}
