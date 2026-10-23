//! `remove` 的行为测试：真文件系统 + 真 git，只经 Core 观察。

mod fixtures;

use std::path::Path;

use config_sync_core::{Core, RemoveOutcome, RemoveResult};

fn core() -> (fixtures::Fixture, Core) {
    let f = fixtures::Fixture::new();
    let core = f.core();
    (f, core)
}

fn removed(target: &Path) -> RemoveOutcome {
    RemoveOutcome { target: target.to_path_buf(), result: RemoveResult::Removed }
}

#[test]
fn removing_a_managed_file_deletes_the_mirror_copy_and_leaves_home_alone() {
    let (f, core) = core();
    let zshrc = f.write_home(".zshrc", "export A=1\n");
    core.add(&[zshrc.clone()]).unwrap();
    f.git(&["add", "-A"]);
    f.git(&["commit", "-m", "add .zshrc"]);

    assert_eq!(core.remove(&[zshrc.clone()]).unwrap(), vec![removed(&zshrc)]);

    // 源目录里的副本没了，$HOME 下的原文件一个字节都没动。
    assert!(!f.source_dir.join("home/.zshrc").exists());
    assert_eq!(std::fs::read_to_string(&zshrc).unwrap(), "export A=1\n");
    // 删除在 git 里表现为待提交的删除；这一行仍留在清单里，否则用户看不见等着推送的删除。
    assert!(f.is_dirty());
    let entries = core.inventory().unwrap();
    assert_eq!(entries.len(), 1);
    assert!(entries[0].deleted);
    assert_eq!(entries[0].target, zshrc);
}

#[test]
fn a_file_that_was_never_added_is_reported_as_not_managed() {
    let (f, core) = core();
    let ghost = f.write_home(".nope", "x\n");

    assert_eq!(
        core.remove(&[ghost.clone()]).unwrap(),
        vec![RemoveOutcome { target: ghost, result: RemoveResult::NotManaged }]
    );
}

#[test]
fn paths_outside_home_are_rejected_and_the_rest_of_the_batch_still_runs() {
    let (f, core) = core();
    let managed = f.write_home(".zshrc", "export A=1\n");
    core.add(&[managed.clone()]).unwrap();
    let outside = f.path("outside/.zshrc");
    std::fs::create_dir_all(outside.parent().unwrap()).unwrap();
    std::fs::write(&outside, "x\n").unwrap();

    let outcomes = core.remove(&[outside.clone(), managed.clone()]).unwrap();
    assert_eq!(outcomes.len(), 2);

    match &outcomes[0] {
        RemoveOutcome { target, result: RemoveResult::Rejected { reason } } => {
            assert_eq!(target, &outside);
            assert!(reason.contains("镜像"), "错误文案要说明镜像布局表达不了该位置：{reason}");
            assert!(reason.contains(f.home.to_str().unwrap()), "错误文案要指出 $HOME：{reason}");
        }
        other => panic!("$HOME 之外的路径必须被拒绝，实际是 {other:?}"),
    }
    assert_eq!(outcomes[1], removed(&managed));
    // 源目录之外的文件一个字节都没被删。
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "x\n");
}

#[test]
fn removing_a_directory_takes_the_whole_subtree() {
    let (f, core) = core();
    f.write_home(".config/nvim/init.lua", "-- init\n");
    f.write_home(".config/nvim/lua/plugins/local.lua", "-- local\n");
    core.add(&[f.home.join(".config/nvim")]).unwrap();

    let outcomes = core.remove(&[f.home.join(".config/nvim")]).unwrap();

    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].result, RemoveResult::Removed);
    assert!(!f.source_dir.join("home/.config/nvim").exists());
    // $HOME 下的文件都还在。
    assert!(f.home.join(".config/nvim/init.lua").exists());
    assert!(f.home.join(".config/nvim/lua/plugins/local.lua").exists());
}

#[test]
fn emptying_a_directory_prunes_the_empty_parents_but_keeps_the_mirror_root() {
    let (f, core) = core();
    let nested = f.write_home(".config/a/b/c.conf", "x\n");
    core.add(&[nested.clone()]).unwrap();

    core.remove(&[nested]).unwrap();

    // 空父目录一路清掉，镜像根 `home/` 本身留着。
    assert!(!f.source_dir.join("home/.config").exists());
    assert!(f.source_dir.join("home").is_dir(), "镜像根不能删");
}
