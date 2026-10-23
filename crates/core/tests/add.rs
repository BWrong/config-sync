//! `add` 的行为测试：真文件系统 + 真 git，只经 Core 观察（清单与磁盘上的最终内容）。

mod fixtures;

use std::path::{Path, PathBuf};
use std::process::Command;

use config_sync_core::{AddOutcome, AddResult, Core, Status};

fn core() -> (fixtures::Fixture, Core) {
    let f = fixtures::Fixture::new();
    let core = f.core();
    (f, core)
}

fn added(target: &Path) -> AddOutcome {
    AddOutcome { target: target.to_path_buf(), result: AddResult::Added }
}

#[test]
fn adding_one_file_copies_it_into_the_mirror_and_lands_in_pending_commit() {
    let (f, core) = core();
    let zshrc = f.write_home(".zshrc", "export A=1\n");

    assert_eq!(core.add(&[zshrc.clone()]).unwrap(), vec![added(&zshrc)]);

    // 复制进源目录的 home/ 镜像位置，内容逐字节相同。
    assert_eq!(
        std::fs::read_to_string(f.source_dir.join("home/.zshrc")).unwrap(),
        "export A=1\n"
    );

    let entries = core.inventory().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].target, zshrc);
    // 新收进来的文件在源目录里还是未跟踪 → 待提交。
    assert_eq!(entries[0].status, Status::PendingCommit);
    // 添加不碰 git：没有提交，也没有推送。
    assert!(f.is_dirty());
    assert!(!f.git_try(&["rev-parse", "--verify", "HEAD"]), "添加不该产生提交");
}

#[test]
fn adding_a_directory_takes_the_whole_subtree_and_keeps_relative_paths() {
    let (f, core) = core();
    f.write_home(".config/nvim/init.lua", "-- init\n");
    f.write_home(".config/nvim/lua/plugins/init.lua", "-- plugins\n");
    f.write_home(".config/nvim/lua/plugins/local.lua", "-- local\n");
    let nvim = f.home.join(".config/nvim");

    let outcomes = core.add(&[nvim]).unwrap();
    assert_eq!(outcomes.len(), 3);
    assert!(outcomes.iter().all(|o| o.result == AddResult::Added));

    // 目标集合与源目录里的路径集合逐一对应（路径映射 = 去掉 $HOME 前缀）。
    let home = &f.home;
    let source_root = f.source_dir.join("home");
    let mut pairs: Vec<(PathBuf, PathBuf)> = core
        .inventory()
        .unwrap()
        .into_iter()
        .map(|entry| {
            let rel = entry.target.strip_prefix(home).expect("清单里的目标都在 $HOME 下");
            assert_eq!(entry.source, source_root.join(rel));
            (entry.target, entry.source)
        })
        .collect();
    pairs.sort();

    let expected = [
        ".config/nvim/init.lua",
        ".config/nvim/lua/plugins/init.lua",
        ".config/nvim/lua/plugins/local.lua",
    ]
    .map(|rel| (f.home.join(rel), source_root.join(rel)));
    assert_eq!(pairs, expected);
    assert_eq!(std::fs::read_to_string(source_root.join(".config/nvim/lua/plugins/local.lua")).unwrap(), "-- local\n");
}

#[test]
fn paths_outside_home_are_rejected_for_the_mirror_layout_and_the_rest_of_the_batch_still_lands() {
    let (f, core) = core();
    let outside = f.path("outside/.zshrc");
    std::fs::create_dir_all(outside.parent().unwrap()).unwrap();
    std::fs::write(&outside, "x\n").unwrap();
    let inside = f.write_home(".zshrc", "export A=1\n");

    let outcomes = core.add(&[outside.clone(), inside.clone()]).unwrap();
    assert_eq!(outcomes.len(), 2);

    match &outcomes[0] {
        AddOutcome { target, result: AddResult::Rejected { reason } } => {
            assert_eq!(target, &outside);
            assert!(reason.contains("镜像"), "错误文案要说明镜像布局表达不了该位置：{reason}");
            assert!(reason.contains(f.home.to_str().unwrap()), "错误文案要指出 $HOME：{reason}");
        }
        other => panic!("$HOME 之外的路径必须被拒绝，实际是 {other:?}"),
    }
    assert_eq!(outcomes[1], added(&inside));
    // $HOME 之外的路径一个字节都没被复制进源目录。
    assert!(!f.source_dir.join("home/outside").exists());
}

#[test]
fn an_already_managed_file_is_reported_and_never_overwritten() {
    let (f, core) = core();
    let zshrc = f.write_home(".zshrc", "export A=1\n");
    core.add(&[zshrc.clone()]).unwrap();

    // 内容相同 → 已在清单中。
    assert_eq!(
        core.add(&[zshrc.clone()]).unwrap(),
        vec![AddOutcome { target: zshrc.clone(), result: AddResult::AlreadyManaged }]
    );

    // 磁盘上的目标被改过 → 已在清单中但内容不同，源文件保持原样（不静默覆盖）。
    f.write_home(".zshrc", "export A=2\n");
    assert_eq!(
        core.add(&[zshrc.clone()]).unwrap(),
        vec![AddOutcome { target: zshrc, result: AddResult::AlreadyManagedDiffers }]
    );
    assert_eq!(
        std::fs::read_to_string(f.source_dir.join("home/.zshrc")).unwrap(),
        "export A=1\n"
    );
}

#[test]
fn a_path_that_does_not_exist_is_rejected() {
    let (f, core) = core();
    let ghost = f.home.join(".nope");

    let outcomes = core.add(&[ghost.clone()]).unwrap();
    match &outcomes[0] {
        AddOutcome { target, result: AddResult::Rejected { reason } } => {
            assert_eq!(target, &ghost);
            assert!(reason.contains("不存在"), "要说清文件不存在：{reason}");
        }
        other => panic!("不存在的路径必须被拒绝，实际是 {other:?}"),
    }
}

#[test]
fn a_directory_only_takes_regular_files() {
    let (f, core) = core();
    let apps = f.home.join(".config/apps");
    f.write_home(".config/apps/real.conf", "a=1\n");
    std::os::unix::fs::symlink("/nonexistent/target", apps.join("broken-link")).unwrap();
    std::os::unix::fs::symlink(apps.join("real.conf"), apps.join("link.conf")).unwrap();
    Command::new("mkfifo").arg(apps.join("pipe")).status().expect("mkfifo");

    core.add(&[apps]).unwrap();

    // 管道与断链被跳过；指向普通文件的符号链接按其内容收进来（源目录里只有普通文件）。
    let mut targets: Vec<String> = core
        .inventory()
        .unwrap()
        .iter()
        .map(|entry| entry.target.strip_prefix(&f.home).unwrap().display().to_string())
        .collect();
    targets.sort();
    assert_eq!(targets, [".config/apps/link.conf", ".config/apps/real.conf"]);
    assert_eq!(std::fs::read_to_string(f.source_dir.join("home/.config/apps/link.conf")).unwrap(), "a=1\n");
}

#[test]
fn a_batch_reports_every_path_and_duplicates_collapse_to_one_inventory_row() {
    let (f, core) = core();
    let zshrc = f.write_home(".zshrc", "export A=1\n");
    let gitconfig = f.write_home(".gitconfig", "[user]\n");

    // `~` 开头的粘贴路径按 $HOME 展开；同一条路径重复出现时第二次报「已在清单中」。
    let outcomes = core
        .add(&[zshrc.clone(), PathBuf::from("~/.gitconfig"), zshrc.clone()])
        .unwrap();
    assert_eq!(
        outcomes,
        vec![
            added(&zshrc),
            added(&gitconfig),
            AddOutcome { target: zshrc, result: AddResult::AlreadyManaged },
        ]
    );
    assert_eq!(core.inventory().unwrap().len(), 2);
}

#[test]
fn a_path_that_reaches_home_through_a_symlink_is_still_accepted() {
    let (f, core) = core();
    f.write_home(".gitconfig", "[user]\n\tname = Bywrong\n");
    // 选择器与终端常给出带符号链接的路径（macOS 上 /tmp、/var 都是软链）。
    let link = f.tmp_root().join("home-link");
    std::os::unix::fs::symlink(f.path("home"), &link).expect("建软链");

    let outcomes = core.add(&[link.join(".gitconfig")]).unwrap();

    assert_eq!(outcomes.len(), 1, "{outcomes:?}");
    assert_eq!(outcomes[0].result, AddResult::Added, "{outcomes:?}");
    assert_eq!(
        std::fs::read_to_string(f.source_dir.join("home/.gitconfig")).unwrap(),
        "[user]\n\tname = Bywrong\n"
    );
}
