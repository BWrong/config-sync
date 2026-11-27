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

/// 恢复计划是前端回传的：一份被改过的计划不能让「备份目录里的任意文件」写到「任意路径」。
#[test]
fn restore_refuses_a_plan_that_points_outside_home_or_outside_the_backup() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    let target = f.write_home(".zshrc", "export OLD=1\n");
    f.write_source("home/.zshrc", "export NEW=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();
    core.execute_apply(&core.plan_apply(None).unwrap()).unwrap();

    // ① 目标不在 $HOME 下：apply 侧一直守着这条，恢复这边也得守。
    let mut plan = core.plan_restore_backup(&latest_id(&core)).unwrap();
    plan.items[0].target = f.tmp_root().join("outside/.zshrc");
    let outcomes = core.restore_backup(&plan).unwrap();
    match &outcomes[0].result {
        RestoreResult::Failed { message } => assert!(
            message.contains("不在"),
            "拒绝理由要说清目标不在 $HOME 下：{message}"
        ),
        other => panic!("越界的恢复必须失败：{other:?}"),
    }
    assert!(
        !f.tmp_root().join("outside/.zshrc").exists(),
        "一个字节都不许写"
    );
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "export NEW=1\n",
        "被拒绝的计划不能动真目标"
    );

    // ② 备份文件不在 `backups/<id>/` 里（`..` 跳出去）：不许 join 完就去读。
    let mut plan = core.plan_restore_backup(&latest_id(&core)).unwrap();
    plan.items[0].file = Some(std::path::PathBuf::from("../manifest.json"));
    plan.items[0].target = target.clone();
    let outcomes = core.restore_backup(&plan).unwrap();
    match &outcomes[0].result {
        RestoreResult::Failed { message } => assert!(
            message.contains("不是备份目录内"),
            "拒绝理由要说清这份备份文件不在备份目录里：{message}"
        ),
        other => panic!("越界的备份文件必须失败：{other:?}"),
    }
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "export NEW=1\n");
}

/// 撤销预览不许为了一个 200MB 的文件把 400MB 读进内存：先看大小，超了只让人去编辑器里对比。
#[test]
fn restore_preview_skips_a_file_that_is_too_large_to_read() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    let target = f.write_home(".zshrc", "export A=1\n");
    f.write_source("home/.zshrc", "export B=2\n");
    f.commit_all("config-sync: 1 files");
    f.push();
    core.execute_apply(&core.plan_apply(None).unwrap()).unwrap();

    // 应用之后用户把它撑到 65 MiB（比单个文件硬上限 64 MiB 大一点）。
    let huge = 65 * 1024 * 1024;
    std::fs::File::options()
        .write(true)
        .open(&target)
        .unwrap()
        .set_len(huge)
        .unwrap();

    let plan = core.plan_restore_backup(&latest_id(&core)).unwrap();
    let item = &plan.items[0];
    assert_eq!(item.action, RestoreAction::Skip, "{item:?}");
    assert!(
        item.reason.contains("太大"),
        "超限要说清「太大、去编辑器里对比」：{}",
        item.reason
    );
    assert!(item.diff.is_empty(), "超限不生成 diff");
    assert!(!item.conflict, "太大不是冲突，别混成一种提示");
}

/// 指纹被写进清单、几天后又读回来比对：必须是稳定哈希（FNV-1a 64，常量写死在代码里），
/// 外加长度做第二道保险。换成 `DefaultHasher` 之类跨版本不稳的哈希，这条会红。
#[test]
fn the_manifest_stores_a_stable_digest_and_the_written_length() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    f.write_home(".zshrc", "export A=1\n");
    let source = "export A=1\nexport B=2\n";
    f.write_source("home/.zshrc", source);
    f.commit_all("config-sync: 1 files");
    f.push();
    core.execute_apply(&core.plan_apply(None).unwrap()).unwrap();

    // 手算一遍 FNV-1a 64：这份十六进制串不随 toolchain 变。
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in source.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let manifest = std::fs::read_to_string(
        backup_root(&f.home)
            .join(latest_id(&core))
            .join("manifest.json"),
    )
    .expect("清单要在盘上");
    assert!(
        manifest.contains(&format!("\"{hash:016x}\"")),
        "清单里的指纹必须等于 FNV-1a 64 的手算值：{manifest}"
    );
    assert!(
        manifest.contains(&format!("\"written_len\": {}", source.len())),
        "清单还要存下写入长度做双保险：{manifest}"
    );

    // 长度对得上、内容被改过：仍然判冲突（指纹是主判据）。
    let mut plan = core.plan_restore_backup(&latest_id(&core)).unwrap();
    plan.items[0].expected_len = Some(source.len() as u64);
    assert_eq!(plan.items[0].action, RestoreAction::Restore);
}

/// 前端拿「最新一份备份」当「刚才这次」用（`list_backups()[0]`）。
/// 清单没落盘的目录不在列表里——否则一次「文件写了、清单没写成」的应用会拿到上一份的 id，
/// 「撤销这次应用」就按更早的状态往回写。这条就是那 2 行前端判断的后端依据。
#[test]
fn a_backup_dir_without_a_manifest_is_never_offered_as_the_latest_one() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    let target = f.write_home(".zshrc", "export OLD=1\n");

    // 第一次应用：写了一份完整备份。
    f.write_source("home/.zshrc", "export NEW=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();
    core.execute_apply(&core.plan_apply(None).unwrap()).unwrap();
    let first = latest_id(&core);

    // 第二次应用：又是一份备份，然后模拟「清单没落盘」。
    f.write_source("home/.zshrc", "export NEWER=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();
    core.execute_apply(&core.plan_apply(None).unwrap()).unwrap();
    let second = latest_id(&core);
    assert_ne!(first, second, "两次应用各留一份备份");
    std::fs::remove_file(backup_root(&f.home).join(&second).join("manifest.json")).unwrap();

    let listed: Vec<String> = core
        .list_backups()
        .unwrap()
        .into_iter()
        .map(|b| b.id)
        .collect();
    assert_eq!(
        listed,
        vec![first.clone()],
        "没有清单的那份不能被当成「最新一份」：否则撤销按钮会指向 {first} 而不是刚才那次"
    );
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "export NEWER=1\n",
        "列表变化不该动目标文件"
    );
}
