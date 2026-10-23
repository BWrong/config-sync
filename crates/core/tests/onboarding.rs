//! 首次使用向导的候选清单：只经 Core 观察（候选集合 + 勾选后走 `add` 的最终效果）。

mod fixtures;

use std::path::PathBuf;

use config_sync_core::{AddResult, Candidate, Status};

fn candidates(f: &fixtures::Fixture) -> Vec<Candidate> {
    f.core().onboarding_candidates()
}

#[test]
fn candidates_are_a_fixed_list_under_home_marked_existing_or_missing() {
    let f = fixtures::Fixture::new();
    f.write_home(".zshrc", "export A=1\n");

    let candidates = candidates(&f);
    let paths: Vec<&str> = candidates.iter().map(|c| c.path.as_str()).collect();

    // 固定的一组常见配置文件，与磁盘内容无关（存在与否只体现在 exists 上）。
    for expected in ["~/.zshrc", "~/.gitconfig", "~/.config/nvim/init.lua"] {
        assert!(paths.contains(&expected), "候选里应有 {expected}：{paths:?}");
    }

    let zshrc = candidates.iter().find(|c| c.path == "~/.zshrc").unwrap();
    assert!(zshrc.exists, "本机有 .zshrc，应标成存在");
    let gitconfig = candidates.iter().find(|c| c.path == "~/.gitconfig").unwrap();
    assert!(!gitconfig.exists, "本机没有 .gitconfig，应标成不存在");

    // 候选都在 $HOME 内，交给 `add` 的是 `~/…` 形式：没有绝对路径，也没有 `..`。
    for candidate in &candidates {
        assert!(candidate.path.starts_with("~/"), "{}", candidate.path);
        assert!(!candidate.path.contains(".."), "{}", candidate.path);
    }
}

#[test]
fn sensitive_files_are_never_offered_as_candidates() {
    let f = fixtures::Fixture::new();
    // 真的建出来：历史记录与私钥即使存在也不该被引导进仓库。
    f.write_home(".zsh_history", ": 1700000000:0;ls\n");
    f.write_home(".bash_history", "ls\n");
    f.write_home(".ssh/id_rsa", "-----BEGIN OPENSSH PRIVATE KEY-----\n");
    f.write_home(".ssh/id_ed25519", "-----BEGIN OPENSSH PRIVATE KEY-----\n");
    f.write_home("credentials", "aws\n");
    f.write_home(".config/gh/token", "ghp_x\n");
    f.write_home(".ssh/config", "Host github.com\n");

    let candidates = candidates(&f);
    let offered: Vec<&str> = candidates.iter().map(|c| c.path.as_str()).collect();

    for forbidden in ["history", "id_rsa", "id_ed25519", "credential", "token", "secret"] {
        assert!(
            !offered.iter().any(|path| path.contains(forbidden)),
            "{forbidden} 不能出现在候选里：{offered:?}"
        );
    }

    // `.ssh/config` 不是私钥，存在就该出现：过滤没有误伤整棵 .ssh。
    let ssh_config = candidates.iter().find(|c| c.path == "~/.ssh/config").expect("缺 ~/.ssh/config");
    assert!(ssh_config.exists, "{ssh_config:?}");
}

#[test]
fn checked_candidates_go_through_add_and_show_up_as_managed_rows() {
    let f = fixtures::Fixture::new();
    f.write_home(".zshrc", "export A=1\n");
    f.write_home(".gitconfig", "[user]\n\tname = me\n");
    let core = f.core();

    let picked: Vec<PathBuf> = core
        .onboarding_candidates()
        .into_iter()
        .filter(|candidate| candidate.exists)
        .map(|candidate| PathBuf::from(candidate.path))
        .collect();
    assert_eq!(picked.len(), 2, "只有真存在的两个文件可勾选：{picked:?}");

    let outcomes = core.add(&picked).unwrap();
    assert!(outcomes.iter().all(|o| o.result == AddResult::Added), "{outcomes:?}");

    // 走的是同一条 add：逐字节复制进镜像位置，$HOME 里的文件不被改写。
    assert_eq!(std::fs::read_to_string(f.source_dir.join("home/.zshrc")).unwrap(), "export A=1\n");
    assert_eq!(f.read_home(".zshrc"), "export A=1\n");

    let entries = core.inventory().unwrap();
    assert_eq!(
        entries.iter().map(|e| e.target.clone()).collect::<Vec<_>>(),
        vec![f.home.join(".gitconfig"), f.home.join(".zshrc")],
        "{entries:?}"
    );
    assert!(entries.iter().all(|e| e.status == Status::PendingCommit), "{entries:?}");
}
