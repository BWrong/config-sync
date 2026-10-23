mod fixtures;

use config_sync_core::{Config, Core, ErrorKind, Status};

#[test]
fn clone_lands_a_repo_in_an_empty_source_dir_and_inventory_lists_its_files() {
    let f = fixtures::Fixture::new();
    // 先在「另一台机器」上把两份配置提交并推送到裸远端。
    f.write_source("home/.zshrc", "export PATH=/usr/local/bin:$PATH\n");
    f.write_source("home/.config/nvim/init.lua", "vim.o.number = true\n");
    f.commit_all("seed");
    f.push();

    // 新机器：源目录还不存在，$HOME 里也没有这些文件。
    let fresh_source = f.path("fresh-source");
    let core = Core::new(&f.home, &fresh_source, Config::default());
    assert!(!core.has_repo(), "空源目录不该被当成已有仓库");

    core.clone(&f.remote_url()).expect("从裸远端 clone 应成功");

    // 源目录是一个合法 git 仓库，origin 指回原远端。
    assert_eq!(f.git_at(&fresh_source, &["rev-parse", "--is-inside-work-tree"]), "true");
    assert_eq!(f.git_at(&fresh_source, &["remote", "get-url", "origin"]), f.remote_url());
    assert!(core.has_repo());

    // 清单直接来自 clone 下来的 home/，且没有写入任何真实文件。
    let entries = core.inventory().expect("清单");
    assert_eq!(entries.len(), 2);
    assert!(entries.iter().all(|e| e.status == Status::TargetMissing));
    assert!(!f.home.join(".zshrc").exists(), "clone 不得写盘");
    assert!(!f.home.join(".config").exists(), "clone 不得写盘");
}

#[test]
fn clone_refuses_a_source_dir_that_already_has_files() {
    let f = fixtures::Fixture::new();
    f.write_source("home/.zshrc", "x\n");
    f.commit_all("seed");
    f.push();

    let err = f.core().clone(&f.remote_url()).expect_err("非空源目录必须拒绝");

    assert_eq!(err.kind, ErrorKind::Io);
    assert!(err.message.contains("源目录"), "{}", err.message);
    // 原有仓库一个字节都没动。
    assert_eq!(f.git(&["remote", "get-url", "origin"]), f.remote_url());
}

#[test]
fn clone_failure_carries_the_raw_git_stderr() {
    let f = fixtures::Fixture::new();
    let core = Core::new(&f.home, f.path("fresh-source"), Config::default());
    let missing = format!("file://{}", f.path("no-such-remote.git").display());

    let err = core.clone(&missing).expect_err("远端不存在必须失败");

    assert_eq!(err.kind, ErrorKind::Git);
    let stderr = err.stderr.expect("必须带 git 原始 stderr");
    assert!(stderr.contains("does not appear to be a git repository"), "{stderr}");
    assert!(!f.path("fresh-source").join(".git").exists());
}

#[test]
fn empty_remote_address_is_rejected_without_touching_the_disk() {
    let f = fixtures::Fixture::new();
    let source = f.path("fresh-source");

    let err = Core::new(&f.home, &source, Config::default())
        .clone("   ")
        .expect_err("空地址必须拒绝");

    assert_eq!(err.kind, ErrorKind::Io);
    assert!(!source.exists(), "校验失败不该建目录");
}

#[test]
fn remote_url_reports_origin_and_keeps_gits_own_words_when_there_is_no_repo() {
    let f = fixtures::Fixture::new();
    assert_eq!(f.core().remote_url().expect("origin 地址"), f.remote_url());

    let plain = f.path("plain");
    std::fs::create_dir_all(&plain).expect("建普通目录");
    let err = Core::new(&f.home, &plain, Config::default())
        .remote_url()
        .expect_err("不是仓库就没有远端");
    assert_eq!(err.kind, ErrorKind::Git);
    assert!(!err.stderr.unwrap_or_default().trim().is_empty(), "git 的原话要带回来");
}
