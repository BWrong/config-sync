//! `plan_apply` / `execute_apply` 的行为测试：真 git + 真文件系统，只经 Core 观察。
//!
//! 每条都走「先出计划、再执行同一份计划」的两阶段，断言磁盘上的最终结果。

mod fixtures;

use std::path::{Path, PathBuf};

use config_sync_core::{
    ApplyAction, ApplyItem, ApplyOutcome, ApplyPlan, ApplyResult, ApplySummary, DIFF_TOO_LARGE,
    ErrorKind, Status,
};

fn written(target: &Path) -> ApplyOutcome {
    ApplyOutcome {
        target: target.to_path_buf(),
        result: ApplyResult::Written,
        warning: None,
    }
}

fn item<'a>(plan: &'a ApplyPlan, target: &Path) -> &'a ApplyItem {
    plan.items
        .iter()
        .find(|item| item.target == target)
        .unwrap_or_else(|| panic!("计划里没有 {}：{:?}", target.display(), plan.items))
}

/// 源目录里提交并推送过一个文件，返回目标路径（此时目标一定不存在）。
fn committed(f: &fixtures::Fixture, rel: &str, content: &str) -> PathBuf {
    f.write_source(&format!("home/{rel}"), content);
    f.commit_all("config-sync: 1 files");
    f.push();
    f.home.join(rel)
}

#[test]
fn the_plan_classifies_missing_differing_and_identical_targets() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    f.write_source("home/.zshrc", "export A=1\n");
    f.write_source("home/.gitconfig", "name = me\n");
    f.write_source("home/.tmux.conf", "set -g mouse on\n");
    f.commit_all("config-sync: 3 files");
    f.push();

    let missing = f.home.join(".zshrc");
    let differs = f.write_home(".gitconfig", "name = other\n");
    let same = f.write_home(".tmux.conf", "set -g mouse on\n");

    let plan = core.plan_apply(None).unwrap();
    assert_eq!(plan.items.len(), 3);

    let table = [
        (&missing, ApplyAction::Create, "+export A=1"),
        (&differs, ApplyAction::Overwrite, "+name = me"),
        (&same, ApplyAction::Skip, ""),
    ];
    for (target, action, hint) in table {
        let item = item(&plan, target);
        assert_eq!(item.action, action, "{}", target.display());
        assert!(!item.reason.is_empty(), "{} 没有原因", target.display());
        assert!(
            item.diff.contains(hint),
            "{} 的 diff：{}",
            target.display(),
            item.diff
        );
    }
    // 覆盖项同时给出被替换掉的旧内容。
    assert!(
        item(&plan, &differs).diff.contains("-name = other"),
        "{}",
        item(&plan, &differs).diff
    );

    // 批量摘要与动作一一对应。
    assert_eq!(
        plan.summary(),
        ApplySummary {
            create: 1,
            overwrite: 1,
            skip: 1
        }
    );
}

#[test]
fn applying_a_missing_target_creates_it_with_the_source_bytes_and_missing_parent_dirs() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    let target = committed(&f, ".config/nvim/lua/init.lua", "-- 空\n");
    assert!(!f.home.join(".config").exists());

    let plan = core.plan_apply(None).unwrap();
    assert_eq!(item(&plan, &target).action, ApplyAction::Create);

    assert_eq!(core.execute_apply(&plan).unwrap(), vec![written(&target)]);
    assert_eq!(f.read_home(".config/nvim/lua/init.lua"), "-- 空\n");
    assert_eq!(
        std::fs::read(&target).unwrap(),
        std::fs::read(f.source_dir.join("home/.config/nvim/lua/init.lua")).unwrap()
    );
}

#[test]
fn overwriting_replaces_the_old_bytes_completely() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    let target = committed(&f, ".zshrc", "export NEW=1\n");
    f.write_home(
        ".zshrc",
        "export OLD=1\nexport LONG_TAIL=2\n# 比源文件更长的旧内容\n",
    );

    let plan = core.plan_apply(None).unwrap();
    assert_eq!(item(&plan, &target).action, ApplyAction::Overwrite);
    core.execute_apply(&plan).unwrap();

    // 整份替换，旧内容一点不剩。
    assert_eq!(f.read_home(".zshrc"), "export NEW=1\n");
}

#[test]
fn an_identical_file_is_a_skip_item_with_a_reason_and_execution_writes_nothing() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    let target = committed(&f, ".zshrc", "export A=1\n");
    f.write_home(".zshrc", "export A=1\n");

    let plan = core.plan_apply(None).unwrap();
    let skip = item(&plan, &target);
    assert_eq!(skip.action, ApplyAction::Skip);
    assert!(skip.reason.contains("一致"), "{}", skip.reason);
    assert_eq!(skip.diff, "");
    assert_eq!(
        plan.summary(),
        ApplySummary {
            create: 0,
            overwrite: 0,
            skip: 1
        }
    );

    assert!(core.execute_apply(&plan).unwrap().is_empty());
    assert_eq!(f.read_home(".zshrc"), "export A=1\n");
}

#[test]
fn a_target_without_a_source_in_the_repository_is_skipped_instead_of_overwritten() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();
    // 目标存在、源目录里没有这个文件：绝不能被覆盖。
    let orphan = f.write_home(".ssh/config", "Host *\n");

    let plan = core.plan_apply(Some(&[orphan.clone()])).unwrap();
    let skip = item(&plan, &orphan);
    assert_eq!(skip.action, ApplyAction::Skip);
    assert!(skip.reason.contains("源目录里没有"), "{}", skip.reason);

    assert!(core.execute_apply(&plan).unwrap().is_empty());
    assert_eq!(f.read_home(".ssh/config"), "Host *\n");
}

#[test]
fn rows_that_still_need_a_commit_or_a_conflict_decision_are_skipped_with_reasons() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    let uncommitted = committed(&f, ".tmux.conf", "set -g mouse on\n");
    let conflicted = committed(&f, ".zshrc", "export A=1\n");

    // 源侧有未提交改动 → 待提交，不能应用。
    f.write_source("home/.tmux.conf", "set -g mouse off\n");
    // 本地改了、远端也改了同一路径 → 冲突，不能应用。
    f.advance_remote("home/.zshrc", "export A=1\nexport REMOTE=9\n");
    f.write_source("home/.zshrc", "export A=2\n");

    let plan = core.plan_apply(None).unwrap();
    let pending_commit = item(&plan, &uncommitted);
    assert_eq!(pending_commit.action, ApplyAction::Skip);
    assert!(
        pending_commit.reason.contains("提交"),
        "{}",
        pending_commit.reason
    );
    let conflict = item(&plan, &conflicted);
    assert_eq!(conflict.action, ApplyAction::Skip);
    assert!(conflict.reason.contains("冲突"), "{}", conflict.reason);

    assert!(core.execute_apply(&plan).unwrap().is_empty());
    assert!(!uncommitted.exists() && !conflicted.exists());
}

#[test]
fn paths_outside_home_are_refused_by_both_phases() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();
    let outside = f.path("outside.conf");

    let error = core.plan_apply(Some(&[outside.clone()])).unwrap_err();
    assert!(error.message.contains("不在"), "{}", error.message);

    // 手写一份计划也不能写到 $HOME 外。
    let plan = ApplyPlan {
        items: vec![ApplyItem {
            target: outside.clone(),
            source: outside.clone(),
            action: ApplyAction::Overwrite,
            reason: String::new(),
            diff: String::new(),
            diff_omitted: false,
            diff_reason: String::new(),
            digest: String::new(),
            skip_reason: None,
        }],
    };
    let outcomes = core.execute_apply(&plan).unwrap();
    assert_eq!(outcomes.len(), 1);
    assert!(
        matches!(&outcomes[0].result, ApplyResult::Failed { message } if message.contains("不在")),
        "{outcomes:?}"
    );
    assert!(!outside.exists());
}

#[test]
fn a_failed_item_leaves_its_target_untouched_and_the_rest_of_the_batch_still_runs() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    f.write_source("home/a.conf", "A\n");
    f.write_source("home/blocked/b.conf", "B\n");
    let last = committed(&f, "c.conf", "C\n");
    f.write_home("c.conf", "old\n");

    let plan = core.plan_apply(None).unwrap();
    // 计划出完之后才把 home/blocked 变成一个普通文件：这一项在执行时必然建不出父目录。
    f.write_home("blocked", "not a directory\n");

    let outcomes = core.execute_apply(&plan).unwrap();
    assert_eq!(outcomes.len(), 3, "{outcomes:?}");
    assert_eq!(outcomes[0].result, ApplyResult::Written);
    assert!(
        matches!(&outcomes[1].result, ApplyResult::Failed { message } if message.contains("blocked")),
        "{outcomes:?}"
    );
    assert_eq!(outcomes[2], written(&last));

    // 边界行为：失败项的目标原样不动，已写的留着，失败之后的项照常写。
    assert_eq!(f.read_home("a.conf"), "A\n");
    assert_eq!(f.read_home("blocked"), "not a directory\n");
    assert_eq!(f.read_home("c.conf"), "C\n");
    // 不留临时文件。
    let leftovers: Vec<String> = std::fs::read_dir(&f.home)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn a_source_changed_after_the_preview_is_reported_instead_of_written() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    let target = committed(&f, ".zshrc", "export A=1\n");

    let plan = core.plan_apply(None).unwrap();
    assert_eq!(item(&plan, &target).action, ApplyAction::Create);
    // 预览之后源文件又变了：写下去的内容就不是用户确认过的那份。
    f.write_source("home/.zshrc", "export SNEAKY=1\n");

    let outcomes = core.execute_apply(&plan).unwrap();
    assert!(
        matches!(&outcomes[0].result, ApplyResult::Failed { message } if message.contains("改过")),
        "{outcomes:?}"
    );
    assert!(!target.exists());
}

#[test]
fn the_plan_drives_the_write_so_preview_and_result_agree() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    f.write_source("home/created.conf", "new\n");
    f.write_source("home/overwritten.conf", "source\n");
    f.write_source("home/skipped.conf", "same\n");
    f.commit_all("config-sync: 3 files");
    f.push();
    f.write_home("overwritten.conf", "target\n");
    f.write_home("skipped.conf", "same\n");

    let plan = core.plan_apply(None).unwrap();
    let before: Vec<Option<Vec<u8>>> = plan
        .items
        .iter()
        .map(|item| std::fs::read(&item.target).ok())
        .collect();

    core.execute_apply(&plan).unwrap();

    let summary = plan.summary();
    let (mut created, mut overwritten) = (0, 0);
    for (index, planned) in plan.items.iter().enumerate() {
        let rel = planned.target.strip_prefix(&f.home).unwrap();
        let source = std::fs::read(f.source_dir.join("home").join(rel)).unwrap();
        match planned.action {
            ApplyAction::Create => {
                assert_eq!(
                    before[index],
                    None,
                    "{} 计划是新建",
                    planned.target.display()
                );
                assert_eq!(std::fs::read(&planned.target).unwrap(), source);
                created += 1;
            }
            ApplyAction::Overwrite => {
                assert!(
                    before[index].is_some(),
                    "{} 计划是覆盖",
                    planned.target.display()
                );
                assert_eq!(std::fs::read(&planned.target).unwrap(), source);
                overwritten += 1;
            }
            ApplyAction::Skip => {
                assert_eq!(
                    std::fs::read(&planned.target).ok(),
                    before[index],
                    "{} 计划是跳过，磁盘不该变",
                    planned.target.display()
                );
            }
        }
    }
    assert_eq!((created, overwritten), (summary.create, summary.overwrite));
}

#[test]
fn creating_a_target_keeps_the_executable_bit_and_overwriting_keeps_the_targets_mode() {
    use std::os::unix::fs::PermissionsExt;

    let f = fixtures::Fixture::new();
    let core = f.core();
    let script = "#!/bin/sh\necho hi\n";
    let source = f.write_source("home/.local/bin/hello", script);
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o755)).unwrap();
    f.commit_all("config-sync: 1 files");
    f.push();
    let target = f.home.join(".local/bin/hello");

    // 新建：沿用源文件的权限，可执行位不能丢。
    let plan = core.plan_apply(None).unwrap();
    core.execute_apply(&plan).unwrap();
    let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode & 0o111, 0o111, "新建后的权限 {mode:o}");
    assert_eq!(f.read_home(".local/bin/hello"), script);

    // 覆盖：保留目标文件原来的权限（这里用户自己收紧到 600）。
    f.write_source("home/.local/bin/hello", "#!/bin/sh\necho bye\n");
    f.commit_all("config-sync: 1 files");
    f.push();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
    let plan = core.plan_apply(None).unwrap();
    assert_eq!(item(&plan, &target).action, ApplyAction::Overwrite);
    core.execute_apply(&plan).unwrap();
    let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "覆盖后的权限 {mode:o}");
    assert_eq!(f.read_home(".local/bin/hello"), "#!/bin/sh\necho bye\n");
}

#[test]
fn a_target_of_a_different_size_is_judged_without_reading_its_content() {
    use std::os::unix::fs::PermissionsExt;

    let f = fixtures::Fixture::new();
    let core = f.core();
    let target = committed(&f, ".zshrc", "export A=1\n");
    // 长度不同就够判「待应用」：内容读不了也不该影响清单（先比 metadata().len()，见 #20）。
    f.write_home(".zshrc", "export A=1\nexport B=2\n");
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o000)).unwrap();

    let entries = core.inventory().unwrap();
    let row = entries.iter().find(|entry| entry.target == target).unwrap();
    assert_eq!(row.status, Status::Pending, "{row:?}");

    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
}

#[test]
fn skipped_items_carry_no_diff_payload() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    f.write_source("home/.tmux.conf", "set -g mouse on\n");
    f.write_source("home/.zshrc", "export A=1\n");
    f.write_source("home/.bashrc", "export B=1\n");
    f.commit_all("config-sync: 3 files");
    f.push();
    // 一致 → 跳过；待提交 → 跳过；冲突 → 跳过：三种跳过项都不写盘，也就不算 diff。
    f.write_home(".tmux.conf", "set -g mouse on\n");
    f.write_source("home/.zshrc", "export A=2\n");
    f.advance_remote("home/.bashrc", "export B=9\n");
    f.write_source("home/.bashrc", "export B=2\n");

    let plan = core.plan_apply(None).unwrap();
    let skipped: Vec<&ApplyItem> = plan
        .items
        .iter()
        .filter(|item| item.action == ApplyAction::Skip)
        .collect();
    assert_eq!(skipped.len(), 3, "{:?}", plan.items);
    for item in skipped {
        assert_eq!(item.diff, "", "{} 跳过了还带了 diff", item.target.display());
        assert!(!item.diff_omitted, "{}", item.target.display());
        assert!(item.diff_reason.is_empty(), "{}", item.target.display());
    }
}

#[test]
fn files_over_the_diff_gate_are_not_previewed_as_diffs() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    // 超过 5000 行就超过闸门（阈值见 core 的 DIFF_MAX_*）。
    let body: String = (0..6000).map(|i| format!("key_{i:05} = value\n")).collect();
    f.write_source("home/big.conf", &body);
    f.commit_all("config-sync: 1 files");
    f.push();
    let target = f.write_home("big.conf", "old\n");

    let plan = core.plan_apply(None).unwrap();
    let big = item(&plan, &target);
    // 闸门只影响 diff，动作与摘要不变。
    assert_eq!(big.action, ApplyAction::Overwrite);
    assert!(
        big.diff.is_empty(),
        "闸门内不该生成 {} 字节的 diff",
        big.diff.len()
    );
    assert!(big.diff_omitted, "{}", target.display());
    assert_eq!(
        plan.summary(),
        ApplySummary {
            create: 0,
            overwrite: 1,
            skip: 0
        }
    );

    // 原因要能说清「多大 / 多少行 / 上限多少」（#17 N2）。
    assert!(big.diff_reason.contains("行"), "{}", big.diff_reason);
    assert!(
        big.diff_reason.contains(DIFF_TOO_LARGE),
        "{}",
        big.diff_reason
    );

    // 预览不预生成，但显式点开仍要给内容：6000 行没过按需闸门（#17 S2）。
    let diff = core.diff(&target).unwrap();
    assert!(!diff.omitted, "{diff:?}");
    assert!(!diff.truncated, "{diff:?}");
    assert!(diff.text.contains("+key_05999 = value"), "{}", diff.text);
}

#[test]
fn the_line_gate_counts_the_line_heaviest_side_not_the_byte_widest_one() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    // 源：20000 条短行（约 176KB，字节没超闸门）；目标：8 条 32KB 巨行（约 250KB，
    // 字节更宽但只有 8 行）。只按「字节最宽那侧」数行的话，5000 行上限就被绕过了（#17 B1）。
    let source: String = (0..20_000)
        .map(|i| format!("k{i:05}={}\n", i % 9))
        .collect();
    f.write_source("home/lines.conf", &source);
    f.commit_all("config-sync: 1 files");
    f.push();
    let target = f.write_home("lines.conf", &format!("{}\n", "x".repeat(32_001)).repeat(8));
    assert!(
        source.len() < 256 * 1024,
        "源要压在字节闸门内：{}",
        source.len()
    );
    assert!(
        std::fs::metadata(&target).unwrap().len() < 256 * 1024,
        "目标也压在字节闸门内"
    );

    let plan = core.plan_apply(None).unwrap();
    let item = item(&plan, &target);
    assert_eq!(item.action, ApplyAction::Overwrite);
    assert!(
        item.diff.is_empty(),
        "行数超闸门就不能生成 {} 字节的 diff",
        item.diff.len()
    );
    assert!(item.diff_omitted, "行数闸门必须生效");
    assert!(
        item.diff_reason.contains("20000 行"),
        "{}",
        item.diff_reason
    );

    // 单文件 diff 走按需闸门（宽得多）：20000 行没超，照样给 diff（#17 S2）。
    let diff = core.diff(&target).unwrap();
    assert!(!diff.omitted, "{diff:?}");
    assert!(!diff.truncated, "{diff:?}");
    assert!(diff.text.contains("+k19999="), "{}", diff.text.len());
}

#[test]
fn a_directory_where_the_target_should_be_is_a_read_error() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    committed(&f, ".zshrc", "export A=1\n");
    let target = f.home.join(".zshrc");
    std::fs::create_dir(&target).unwrap();

    // 目标是目录：沿用原来的读错误，而不是长度不等就判「不同」（#17 N3）。
    let error = core.inventory().expect_err("目标是目录必须报读失败");
    assert_eq!(error.kind, ErrorKind::Io, "{error:?}");
    assert!(error.message.contains("读取"), "{}", error.message);
}

#[test]
fn an_oversized_target_is_not_read_into_memory() {
    use std::os::unix::fs::PermissionsExt;

    let f = fixtures::Fixture::new();
    let core = f.core();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();
    // 目标比闸门大得多，而且读不了：只有「先量字节、超了就不读」才可能出得了计划（#17 N4）。
    let target = f.write_home(".zshrc", &"x".repeat(300 * 1024));
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o000)).unwrap();

    let plan = core.plan_apply(None).expect("超闸门的目标不该被读进来");
    let big = item(&plan, &target);
    assert_eq!(big.action, ApplyAction::Overwrite);
    assert!(big.diff_omitted, "{}", big.diff_reason);
    // 措辞别过头：源侧是读了的，只说「超过上限的那一侧没有读入内存」（第 2 轮 N3）。
    assert!(
        big.diff_reason.contains("没有读入内存"),
        "{}",
        big.diff_reason
    );

    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
}

#[test]
fn an_oversized_pair_of_equal_size_is_read_but_never_diffed() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    // 两侧等长且都超 256KB：一致性只能靠读内容判断，但 diff 一律不算（#17 N4 + B1）。
    let source = "a".repeat(300 * 1024);
    f.write_source("home/equal.conf", &source);
    f.commit_all("config-sync: 1 files");
    f.push();
    let target = f.write_home("equal.conf", &"b".repeat(300 * 1024));

    let plan = core.plan_apply(None).unwrap();
    let big = item(&plan, &target);
    assert_eq!(big.action, ApplyAction::Overwrite);
    assert!(
        big.diff.is_empty() && big.diff_omitted,
        "{}",
        big.diff_reason
    );
    // 读过就别再说「没有读入内存」：这句得报实际行数。
    assert!(
        !big.diff_reason.contains("没有读入内存"),
        "{}",
        big.diff_reason
    );
}
