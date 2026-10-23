//! `Core::diff` 的行为测试：断言 unified diff 的关键行，以及比较的两侧选对没有。

mod fixtures;

use std::path::Path;

use config_sync_core::Status;

#[test]
fn diff_between_target_and_source_for_a_pending_row() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    let source = f.write_source("home/.zshrc", "export A=1\nexport B=2\n");
    f.commit_all("config-sync: 1 files");
    f.push();
    let target = f.write_home(".zshrc", "export A=1\n");

    let diff = core.diff(&target).unwrap();
    assert_eq!(diff.status, Status::Pending);
    assert_eq!(diff.target, target);

    let text = &diff.text;
    assert!(
        text.starts_with(&format!("--- {}\n+++ {}\n", target.display(), source.display())),
        "{text}"
    );
    assert!(text.contains("@@"), "{text}");
    // 旧侧是磁盘上的目标文件，新侧是源目录（应用会写成这样）。
    assert!(text.contains(" export A=1"), "{text}");
    assert!(text.contains("+export B=2"), "{text}");
}

#[test]
fn diff_uses_head_as_the_old_side_when_the_source_has_uncommitted_changes() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();
    let target = f.write_home(".zshrc", "export A=1\n");
    f.write_source("home/.zshrc", "export A=1\nexport B=3\n");

    let diff = core.diff(&target).unwrap();
    assert_eq!(diff.status, Status::PendingCommit);

    let text = &diff.text;
    assert!(text.contains("--- HEAD:home/.zshrc"), "{text}");
    assert!(text.contains("-export A=1"), "{text}");
    assert!(text.contains("+export A=1"), "{text}");
    assert!(text.contains("+export B=3"), "{text}");
}

#[test]
fn diff_shows_the_incoming_change_for_a_pending_pull_row() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();
    let target = f.write_home(".zshrc", "export A=1\n");
    f.advance_remote("home/.zshrc", "export A=1\nexport REMOTE=9\n");

    let diff = core.diff(&target).unwrap();
    assert_eq!(diff.status, Status::PendingPull);

    let text = &diff.text;
    assert!(text.contains("--- HEAD:home/.zshrc"), "{text}");
    assert!(text.contains("+++ @{u}:home/.zshrc"), "{text}");
    assert!(text.contains("+export REMOTE=9"), "{text}");
}

#[test]
fn diff_is_empty_when_both_sides_are_identical_and_all_added_when_the_target_is_missing() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();
    let missing_target = f.home.join(".zshrc");

    let added = core.diff(&missing_target).unwrap();
    assert_eq!(added.status, Status::TargetMissing);
    assert!(added.text.contains("+export A=1"), "{}", added.text);

    f.write_home(".zshrc", "export A=1\n");
    let same = core.diff(&missing_target).unwrap();
    assert_eq!(same.status, Status::InSync);
    assert_eq!(same.text, "");
}

#[test]
fn diff_of_a_path_outside_home_is_an_error() {
    let f = fixtures::Fixture::new();
    let core = f.core();

    let error = core.diff(Path::new("/etc/hosts")).unwrap_err();
    assert!(error.message.contains("不在"), "{}", error.message);
}
