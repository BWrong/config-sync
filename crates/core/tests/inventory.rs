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
    // 已提交，git 侧干净：状态就只由磁盘内容决定。
    f.commit_all("config-sync: 2 files");

    let entries = core.inventory().unwrap();
    assert_eq!(
        entries,
        vec![
            // 排序在 `home/.zshrc` 之前的子路径先出现，与遍历顺序无关。
            InventoryEntry {
                source: missing_target,
                target: f.home.join(".config/nvim/init.lua"),
                status: Status::TargetMissing,
                deleted: false,
            },
            InventoryEntry {
                source,
                target,
                status: Status::InSync,
                deleted: false
            },
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
        RepoStatus {
            branch: "main".into(),
            ahead: 0,
            behind: 0,
            dirty: 1,
            // 设置的分支就是当前分支：状态条不需要额外标注。
            setting: None,
        }
    );

    f.commit_all("config-sync: 1 files");
    f.push();
    assert_eq!(core.repo_status().unwrap().dirty, 0);
    // 伪远端真的收到了提交：裸仓库的 main 与源目录 HEAD 相同。
    assert_eq!(f.git_remote(&["rev-parse", "main"]), f.head_sha());

    // 第二份工作副本往远端再推一个提交 → fetch 之后 behind == 1。
    let other = f.path("other");
    f.git_at(
        f.tmp_root(),
        &["clone", &f.remote_url(), other.to_str().unwrap()],
    );
    f.git_at(
        &other,
        &["config", "user.email", "test@config-sync.invalid"],
    );
    f.git_at(&other, &["config", "user.name", "config-sync test"]);
    std::fs::write(other.join("home/.zshrc"), "export A=3\n").unwrap();
    f.git_at(&other, &["commit", "-am", "config-sync: 1 files"]);
    f.git_at(&other, &["push", "origin", "main"]);

    f.git(&["fetch", "origin"]);
    let status = core.repo_status().unwrap();
    assert_eq!((status.ahead, status.behind, status.dirty), (0, 1, 0));
}

#[test]
fn a_deleted_mirror_file_stays_listed_as_pending_commit() {
    let (f, core) = core();
    let target = f.write_home(".zshrc", "export A=1\n");
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");

    core.remove(&[target.clone()]).unwrap();

    // 源文件没了，但 git 里那条删除还没提交：清单要能看见它，否则待推送的删除无从下手。
    let entries = core.inventory().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].target, target);
    assert_eq!(entries[0].status, Status::PendingCommit);
    assert!(entries[0].deleted);

    // 展开这一行：HEAD 里的内容对空内容，正好是这个删除本身。
    let diff = core.diff(&target).unwrap();
    assert!(diff.text.contains("-export A=1"), "{}", diff.text);
}

#[test]
/// 排序口径的回归保护（#42-5）。
///
/// 旧的 `binary_search` + `Vec::insert` 本来就升序，这条用例**不**是新旧实现的判别器：
/// #42-5 的毛病是每次插入搬一次尾巴（O(D×N) 的指针搬移），是成本不是顺序。改成排序后
/// 一把线性归并，代价是 O(D log D + N)，顺序语义必须和原来一样——这条用例钉的就是这个
/// 「一样」（含插在两条留下的中间，而不是追加到末尾），性能本身没在这里测。
fn deleted_rows_keep_their_alphabetical_place_between_the_surviving_ones() {
    let (f, core) = core();
    for (rel, content) in [
        (".zshrc", "export Z=1\n"),
        (".zshenv", "export E=1\n"),
        (".zprofile", "export P=1\n"),
    ] {
        f.write_home(rel, content);
        f.write_source(&format!("home/{rel}"), content);
    }
    f.commit_all("config-sync: 3 files");

    // 只删中间那一条：归并后它得回到自己那一格，而不是被追加到末尾。
    core.remove(&[f.home.join(".zshenv")]).unwrap();

    let names: Vec<String> = core
        .inventory()
        .unwrap()
        .into_iter()
        .map(|entry| {
            entry
                .target
                .strip_prefix(&f.home)
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    // `.zshenv` < `.zshrc`：删掉的那条落在两条留下的之间，不是末尾。
    assert_eq!(names, [".zprofile", ".zshenv", ".zshrc"]);
}

#[test]
fn a_deletion_that_is_committed_but_not_pushed_stays_listed() {
    let (f, core) = core();
    let target = f.write_home(".zshrc", "export A=1\n");
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();

    core.remove(&[target.clone()]).unwrap();
    // 删除进了提交，但停在本地：远端此刻还有这个文件。
    f.commit_all("config-sync: 0 files");

    let entries = core.inventory().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].target, target);
    assert_eq!(entries[0].status, Status::PendingPushDelete);
    assert!(entries[0].deleted);

    // 推送之后远端也没有了，这一行从清单消失。
    f.push();
    assert!(core.inventory().unwrap().is_empty());
}

#[test]
fn refresh_carries_inventory_and_status_from_one_read() {
    let (f, core) = core();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();
    // 远端多一个提交（behind=1）、本地多一个未跟踪文件（dirty=1）、目标内容不同。
    f.advance_remote("home/.zshrc", "export A=1\nexport B=9\n");
    f.write_source("home/.gitconfig", "[user]\n\tname = 测试\n");
    f.write_home(".zshrc", "export A=2\n");

    let refresh = core.refresh().unwrap();
    assert_eq!(refresh.entries, core.inventory().unwrap());
    assert_eq!(refresh.status, core.repo_status().unwrap());
    assert_eq!(refresh.status.branch, "main");
    assert_eq!((refresh.status.ahead, refresh.status.behind), (0, 1));
    assert_eq!(refresh.status.dirty, 1);
    assert_eq!(refresh.entries.len(), 2);
}

#[test]
fn renamed_file_is_listed_under_its_new_path_only() {
    let (f, core) = core();
    f.write_source("home/.zshrc", "export A=1\n");
    f.write_home(".zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");

    // v2 的重命名记录（`2 …`）后面紧跟一个原路径字段：解析时按定长切到路径那段，
    // 再跳过原路径。切错一格就会拿 `R100` 之类当路径。
    f.git(&["mv", "home/.zshrc", "home/.zshrc.bak"]);

    let entries = core.inventory().unwrap();
    assert_eq!(entries.len(), 1, "重命名只该剩一行：{entries:?}");
    assert!(
        entries[0].source.ends_with("home/.zshrc.bak"),
        "新路径没解析出来：{:?}",
        entries[0].source
    );
    assert_eq!(
        core.repo_status().unwrap().dirty,
        1,
        "一次重命名算一条未提交改动"
    );
}
