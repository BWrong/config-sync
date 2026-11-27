//! 应用前快照与撤销的行为测试：真 git、真文件、真权限，断言覆盖 3 个文件后能一键还原。

mod fixtures;

use std::os::unix::fs::PermissionsExt;

use config_sync_core::{ApplyAction, Core, RestoreAction, RestoreResult};

fn backup_root(core_home: &std::path::Path) -> std::path::PathBuf {
    core_home.join(".config/config-sync/backups")
}

fn mode_of(path: &std::path::Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn item_of<'a>(
    plan: &'a config_sync_core::ApplyPlan,
    target: &std::path::Path,
) -> &'a config_sync_core::ApplyItem {
    plan.items
        .iter()
        .find(|item| item.target == target)
        .expect("计划里有这个目标")
}

fn latest_id(core: &Core) -> String {
    core.list_backups()
        .unwrap()
        .first()
        .expect("至少有一份备份")
        .id
        .clone()
}

#[test]
fn undo_restores_three_overwritten_files_with_their_contents_and_modes() {
    let f = fixtures::Fixture::new();
    let core = f.core();

    // 三份原文，权限各不相同：撤销后要一模一样。
    let originals = [
        (".zshrc", "export A=1\n", 0o644u32),
        (".gitconfig", "name = me\n", 0o600),
        (".local/bin/tool", "#!/bin/sh\necho hi\n", 0o750),
    ];
    for (rel, content, _) in originals {
        f.write_source(&format!("home/{rel}"), content);
    }
    f.commit_all("config-sync: 3 files");
    f.push();
    for (rel, content, mode) in originals {
        let target = f.write_home(rel, content);
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode)).unwrap();
        f.write_source(&format!("home/{rel}"), &format!("{content}# 源目录改过\n"));
    }
    f.commit_all("config-sync: 3 files");
    f.push();

    let plan = core.plan_apply(None).unwrap();
    assert_eq!(plan.summary().overwrite, 3);
    let outcomes = core.execute_apply(&plan).unwrap();
    assert_eq!(outcomes.len(), 3, "{outcomes:?}");

    // 备份目录：0700、在 $HOME 下，不在源仓库里，也就不会被推送。
    let root = backup_root(&f.home);
    assert!(root.is_dir(), "{} 应该被建出来", root.display());
    assert_eq!(mode_of(&root), 0o700, "备份根目录必须是 0700");
    assert!(!root.starts_with(&f.source_dir), "备份不许落在源仓库里");
    assert_eq!(
        f.git(&["status", "--porcelain"]),
        "",
        "源目录不该因为备份变脏（备份没进仓库）"
    );

    // 清单：这次动了 3 个路径，都写成了。
    let backups = core.list_backups().unwrap();
    assert_eq!(backups.len(), 1, "{backups:?}");
    assert_eq!((backups[0].written, backups[0].failed), (3, 0));
    let id = backups[0].id.clone();
    assert_eq!(mode_of(&root.join(&id)), 0o700, "单次备份目录也必须是 0700");

    // 撤销预览：3 项都是恢复，diff 里能看见原文。
    let restore = core.plan_restore_backup(&id).unwrap();
    assert_eq!(restore.writes(), 3, "{restore:?}");
    for item in &restore.items {
        assert_eq!(item.action, RestoreAction::Restore, "{item:?}");
        assert!(!item.conflict);
        assert!(!item.reason.is_empty());
    }
    let zshrc = restore
        .items
        .iter()
        .find(|item| item.target.ends_with(".zshrc"))
        .unwrap();
    assert!(zshrc.diff.contains("export A=1"), "diff = {}", zshrc.diff);
    assert!(
        zshrc.diff.contains("-# 源目录改过"),
        "撤销的 diff 是「现在的目标 → 备份原文」：{}",
        zshrc.diff
    );

    let outcomes = core.restore_backup(&restore).unwrap();
    assert_eq!(outcomes.len(), 3, "{outcomes:?}");
    for (rel, content, mode) in originals {
        let target = f.home.join(rel);
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            content,
            "{rel} 的内容没恢复"
        );
        assert_eq!(mode_of(&target), mode, "{rel} 的权限没恢复");
    }
}

#[test]
fn undo_deletes_files_the_apply_created_and_restores_the_rest() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    let existing = f.write_home(".zshrc", "export OLD=1\n");
    f.write_source("home/.zshrc", "export NEW=1\n");
    f.write_source("home/.tmux.conf", "set -g mouse on\n");
    f.commit_all("config-sync: 2 files");
    f.push();

    let plan = core.plan_apply(None).unwrap();
    assert_eq!(
        item_of(&plan, &f.home.join(".tmux.conf")).action,
        ApplyAction::Create
    );
    core.execute_apply(&plan).unwrap();

    let restore = core.plan_restore_backup(&latest_id(&core)).unwrap();
    let created = restore
        .items
        .iter()
        .find(|item| item.target.ends_with(".tmux.conf"))
        .unwrap();
    assert_eq!(created.action, RestoreAction::Delete, "{created:?}");
    assert!(created.reason.contains("删掉"), "{}", created.reason);

    core.restore_backup(&restore).unwrap();
    assert!(
        !f.home.join(".tmux.conf").exists(),
        "应用新建的文件应被删掉"
    );
    assert_eq!(f.read_home(".zshrc"), "export OLD=1\n");
    assert_eq!(existing, f.home.join(".zshrc"));
}

#[test]
fn restore_refuses_to_clobber_changes_the_user_made_after_the_apply() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    f.write_source("home/.zshrc", "export SOURCE=1\n");
    f.write_source("home/.gitconfig", "name = me\n");
    f.commit_all("config-sync: 2 files");
    f.push();
    f.write_home(".zshrc", "export MINE=1\n");
    f.write_home(".gitconfig", "name = mine\n");

    let plan = core.plan_apply(None).unwrap();
    core.execute_apply(&plan).unwrap();

    // 应用之后用户又手改了一个文件。
    let hand_edited = f.write_home(".zshrc", "export HAND_EDITED=1\n");

    let restore = core.plan_restore_backup(&latest_id(&core)).unwrap();
    let conflict = restore
        .items
        .iter()
        .find(|item| item.target == hand_edited)
        .unwrap();
    assert_eq!(conflict.action, RestoreAction::Skip, "{conflict:?}");
    assert!(conflict.conflict, "要标成冲突，界面才提示得出来");
    assert!(conflict.reason.contains("改过"), "{}", conflict.reason);
    assert_eq!(restore.writes(), 1, "只该恢复没人动过的那一份");

    let outcomes = core.restore_backup(&restore).unwrap();
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].result, RestoreResult::Restored);
    // 用户的新改动一个字节都没动。
    assert_eq!(f.read_home(".zshrc"), "export HAND_EDITED=1\n");
    // 撤销回的是应用之前的原文，不是源目录里那份。
    assert_eq!(f.read_home(".gitconfig"), "name = mine\n");
}

#[test]
fn a_partial_failure_lands_in_the_manifest_and_only_the_written_items_are_undone() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    f.write_source("home/a.conf", "A=source\n");
    f.write_source("home/b.conf", "B=source\n");
    f.commit_all("config-sync: 2 files");
    f.push();
    f.write_home("a.conf", "A=original\n");
    f.write_home("b.conf", "B=original\n");

    let plan = core.plan_apply(None).unwrap();
    // 预览之后把 b 的源文件删掉：这一项必然失败，a 照常写。
    std::fs::remove_file(f.source_dir.join("home/b.conf")).unwrap();
    let outcomes = core.execute_apply(&plan).unwrap();
    assert_eq!(outcomes.len(), 2, "{outcomes:?}");

    let backups = core.list_backups().unwrap();
    assert_eq!(
        (backups[0].written, backups[0].failed),
        (1, 1),
        "{backups:?}"
    );
    assert_eq!(
        f.read_home("b.conf"),
        "B=original\n",
        "失败项的目标原样未动"
    );

    let restore = core.plan_restore_backup(&backups[0].id).unwrap();
    let failed = restore
        .items
        .iter()
        .find(|item| item.target.ends_with("b.conf"))
        .unwrap();
    assert_eq!(failed.action, RestoreAction::Skip, "{failed:?}");
    assert!(failed.reason.contains("没有写成"), "{}", failed.reason);

    core.restore_backup(&restore).unwrap();
    assert_eq!(f.read_home("a.conf"), "A=original\n");
}

#[test]
fn backups_are_pruned_to_the_latest_ten() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    f.write_source("home/.zshrc", "v0\n");
    f.commit_all("config-sync: 1 files");
    f.push();

    for round in 1..=12 {
        f.write_source("home/.zshrc", &format!("v{round}\n"));
        f.commit_all("config-sync: 1 files");
        f.push();
        f.write_home(".zshrc", "old\n");
        let plan = core.plan_apply(None).unwrap();
        core.execute_apply(&plan).unwrap();
    }

    let backups = core.list_backups().unwrap();
    assert_eq!(backups.len(), 10, "保留策略：最近 10 次，不无限增长");
    assert!(backups[0].id > backups[9].id, "新的在前：{backups:?}");
}

#[test]
fn a_backup_dir_inside_the_source_repo_is_refused_before_anything_is_written() {
    // $HOME 就是源目录：备份会落进仓库，必须拒绝而不是把密钥推上去。
    let f = fixtures::Fixture::new();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    let core = Core::new(
        &f.source_dir,
        &f.source_dir,
        config_sync_core::Config::default(),
    );

    let plan = core.plan_apply(None).unwrap();
    let error = core.execute_apply(&plan).unwrap_err();
    assert!(error.message.contains("落在源目录"), "{}", error.message);
    assert!(
        !f.source_dir.join(".config/config-sync/backups").exists(),
        "拒绝时不该留下任何备份目录"
    );
}

#[test]
fn restore_rechecks_the_target_between_preview_and_execution() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    f.write_source("home/.zshrc", "export SOURCE=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();
    f.write_home(".zshrc", "export OLD=1\n");

    let plan = core.plan_apply(None).unwrap();
    core.execute_apply(&plan).unwrap();

    let restore = core.plan_restore_backup(&latest_id(&core)).unwrap();
    // 预览之后又改了一次：执行时要失败，而不是把这一下覆盖掉。
    f.write_home(".zshrc", "export LATE=1\n");

    let outcomes = core.restore_backup(&restore).unwrap();
    assert_eq!(outcomes.len(), 1);
    assert!(
        matches!(&outcomes[0].result, RestoreResult::Failed { message } if message.contains("又变过")),
        "{outcomes:?}"
    );
    assert_eq!(f.read_home(".zshrc"), "export LATE=1\n");
}
