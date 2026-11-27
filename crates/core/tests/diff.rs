//! `Core::diff` 的行为测试：断言 unified diff 的关键行，以及比较的两侧选对没有。

mod fixtures;

use std::path::Path;

use config_sync_core::{DIFF_UNREADABLE, Status};

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
        text.starts_with(&format!(
            "--- {}\n+++ {}\n",
            target.display(),
            source.display()
        )),
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

#[test]
fn a_diff_over_the_ondemand_gate_is_truncated_not_refused() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    // 12 万行（约 2.4MB）：字节没过 4MB，但行数过了 10 万（按需闸门，见 core 的 DIFF_ONDEMAND_*）。
    let body: String = (0..120_000)
        .map(|i| format!("key_{i:06} = value\n"))
        .collect();
    f.write_source("home/big.conf", &body);
    f.commit_all("config-sync: 1 files");
    f.push();
    let target = f.write_home("big.conf", "old\n");

    let diff = core.diff(&target).unwrap();
    assert!(!diff.omitted, "{diff:?}");
    assert!(diff.truncated, "{diff:?}");
    assert!(
        diff.reason.contains("已截断"),
        "截断要有标记：{}",
        diff.reason
    );
    assert!(
        diff.reason.contains("共 120000 行"),
        "截断要报总行数：{}",
        diff.reason
    );
    // 内容真的给了，但只有前 5000 行的 diff，不是 12 万行。
    assert!(diff.text.contains("+key_004999 = value"), "{}", diff.text);
    assert!(!diff.text.contains("+key_005000 = value"), "{}", diff.text);
    assert!(
        diff.text.lines().count() < 6000,
        "{}",
        diff.text.lines().count()
    );
}

#[test]
fn a_target_that_is_not_utf8_says_it_cannot_be_previewed_here() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();
    let target = f.home.join(".zshrc");
    std::fs::write(&target, [0xff_u8, 0xfe, 0x00, 0x01]).unwrap();

    let diff = core.diff(&target).unwrap();
    assert!(diff.omitted, "{diff:?}");
    assert_eq!(diff.text, "");
    assert!(diff.reason.starts_with(DIFF_UNREADABLE), "{}", diff.reason);
    // 退路要留着：让用户去编辑器里对比。
    assert!(diff.reason.contains("编辑器"), "{}", diff.reason);
}

#[test]
fn a_target_over_the_hard_cap_is_not_read_into_memory() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();
    let target = f.home.join(".zshrc");
    // 超过 64MB 的硬上限：不读进内存，直接说没法在这里预览（#17 S2）。
    std::fs::write(&target, vec![b'a'; 64 * 1024 * 1024 + 1]).unwrap();

    let diff = core.diff(&target).unwrap();
    assert!(diff.omitted, "{diff:?}");
    assert_eq!(diff.text, "");
    assert!(diff.reason.starts_with(DIFF_UNREADABLE), "{}", diff.reason);
    assert!(diff.reason.contains("64 MB"), "{}", diff.reason);
}
