//! Core 的行为测试。全部通过夹具（临时 home + 临时源目录 + 临时裸远端 + 真 git）驱动。

mod fixtures;

use config_sync_core::{Core, InventoryEntry, RepoStatus, Status};

fn core() -> (fixtures::Fixture, Core) {
    let f = fixtures::Fixture::new();
    let core = f.core();
    (f, core)
}

#[test]
fn empty_source_dir_yields_empty_inventory_and_clean_status_bar() {
    let (_f, core) = core();
    assert_eq!(core.inventory().unwrap(), Vec::<InventoryEntry>::new());

    let status = core.repo_status().unwrap();
    assert_eq!(status.branch, "main");
    assert_eq!((status.ahead, status.behind, status.dirty), (0, 0, 0));
}

#[test]
fn inventory_mirrors_home_subtree_and_compares_content() {
    let (f, core) = core();
    // 还没有受管文件时，目标文件存在也不该出现在清单里。
    f.write_home(".zshrc", "export A=1\n");
    assert_eq!(core.inventory().unwrap(), Vec::<InventoryEntry>::new());

    let source = f.write_source("home/.zshrc", "export A=1\n");
    let missing_target = f.write_source("home/.config/nvim/init.lua", "-- 空\n");
    let target = f.write_home(".zshrc", "export A=1\n");

    let entries = core.inventory().unwrap();
    assert_eq!(
        entries,
        vec![
            // 排序在 `home/.zshrc` 之前的子路径先出现，与遍历顺序无关。
            InventoryEntry {
                source: missing_target,
                target: f.home.join(".config/nvim/init.lua"),
                status: Status::TargetMissing,
            },
            InventoryEntry { source, target, status: Status::InSync },
        ]
    );

    // 目标文件被改过 → 待应用。
    f.write_home(".zshrc", "export A=2\n");
    assert_eq!(core.inventory().unwrap()[1].status, Status::Pending);
}

#[test]
fn repo_status_follows_real_git_state_and_the_bare_remote() {
    let (f, core) = core();

    f.write_source("home/.zshrc", "export A=1\n");
    assert_eq!(
        core.repo_status().unwrap(),
        RepoStatus { branch: "main".into(), ahead: 0, behind: 0, dirty: 1 }
    );

    f.commit_all("config-sync: 1 files");
    f.push();
    assert_eq!(core.repo_status().unwrap().dirty, 0);
    // 伪远端真的收到了提交：裸仓库的 main 与源目录 HEAD 相同。
    assert_eq!(f.git_remote(&["rev-parse", "main"]), f.head_sha());

    // 第二份工作副本往远端再推一个提交 → fetch 之后 behind == 1。
    let other = f.path("other");
    f.git_at(f.tmp_root(), &["clone", &f.remote_url(), other.to_str().unwrap()]);
    f.git_at(&other, &["config", "user.email", "test@config-sync.invalid"]);
    f.git_at(&other, &["config", "user.name", "config-sync test"]);
    std::fs::write(other.join("home/.zshrc"), "export A=3\n").unwrap();
    f.git_at(&other, &["commit", "-am", "config-sync: 1 files"]);
    f.git_at(&other, &["push", "origin", "main"]);

    f.git(&["fetch", "origin"]);
    let status = core.repo_status().unwrap();
    assert_eq!((status.ahead, status.behind, status.dirty), (0, 1, 0));
}
