//! 设置里的 `branch` 在 clone / 拉取 / 推送三条路径上都必须真正生效；
//! 当前分支与设置不一致时行为有定义：干净才切，脏就拒绝并说清楚，绝不静默丢改动。
//!
//! 唯一 seam 是 `Core`，全部用真 git（临时 home + 临时源目录 + 临时裸远端）。

mod fixtures;

use config_sync_core::{Config, Core, CoreError};

/// 用指定分支构造 Core（`Fixture::core()` 固定用默认的 main）。
fn core_on(f: &fixtures::Fixture, branch: &str) -> Core {
    Core::new(
        &f.home,
        &f.source_dir,
        Config {
            branch: branch.to_string(),
            editor_command: String::new(),
        },
    )
}

/// git 失败：人话 + 原始 stderr 都要在。
fn failure(error: CoreError) -> String {
    assert!(
        error
            .stderr
            .as_deref()
            .is_some_and(|stderr| !stderr.is_empty()),
        "git 的原始 stderr 必须被保留：{error:?}"
    );
    format!("{} / {:?}", error.message, error.stderr)
}

/// 我们自己的拒绝（切分支前发现工作区脏）：没有 stderr，只看 message。
fn refusal(error: CoreError) -> String {
    error.message
}

/// 让远端多出一个 `dev` 分支，内容来自源目录。
fn seed_remote_dev(f: &fixtures::Fixture) {
    f.git(&["checkout", "-q", "-b", "dev"]);
    f.write_source("home/.zshrc", "export DEV=1\n");
    f.commit_all("config-sync: 1 files");
    f.git(&["push", "-q", "origin", "dev"]);
    f.git(&["checkout", "-q", "main"]);
    f.git(&["branch", "-q", "-D", "dev"]);
}

#[test]
fn clone_checks_out_the_configured_branch() {
    let f = fixtures::Fixture::new();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();
    seed_remote_dev(&f);

    let target = f.path("clone");
    let core = Core::new(
        &f.home,
        &target,
        Config {
            branch: "dev".to_string(),
            editor_command: String::new(),
        },
    );
    core.clone(&f.remote_url())
        .expect("clone 应按设置的分支落地");

    assert_eq!(
        f.git_at(&target, &["branch", "--show-current"]),
        "dev",
        "clone 完必须停在设置的分支上，而不是远端默认分支"
    );
    assert_eq!(
        f.git_at(&target, &["show", "HEAD:home/.zshrc"]),
        "export DEV=1",
        "拿到的必须是这个分支的内容"
    );
}

#[test]
fn clone_reports_a_configured_branch_the_remote_does_not_have() {
    let f = fixtures::Fixture::new();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();

    let core = Core::new(
        &f.home,
        f.path("clone"),
        Config {
            branch: "nope".to_string(),
            editor_command: String::new(),
        },
    );
    let text = failure(
        core.clone(&f.remote_url())
            .expect_err("远端没有的分支必须明确拒绝"),
    );
    assert!(text.contains("nope"), "要点名是哪个分支：{text}");
}

#[test]
fn pull_switches_to_the_configured_branch_then_fast_forwards() {
    let f = fixtures::Fixture::new();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();
    seed_remote_dev(&f);

    // 远端 dev 再前进一格（另一台机器）。
    let other = f.other();
    f.git_at(&other, &["checkout", "-q", "dev"]);
    std::fs::write(other.join("home/.zshrc"), "export DEV=2\n").expect("写 second worktree");
    f.git_at(&other, &["add", "-A"]);
    f.git_at(&other, &["commit", "-m", "config-sync: 1 files"]);
    f.git_at(&other, &["push", "-q", "origin", "dev"]);
    let remote_dev = f.git_remote(&["rev-parse", "dev"]);

    let core = core_on(&f, "dev");
    assert_eq!(
        f.git(&["branch", "--show-current"]),
        "main",
        "前提：在 main 上"
    );

    core.pull().expect("设置的分支只存在于远端时也要能拉");

    assert_eq!(f.git(&["branch", "--show-current"]), "dev");
    assert_eq!(f.head_sha(), remote_dev, "必须快进到远端 dev");
    assert!(!f.is_dirty(), "拉取后源目录应干净");
    assert!(!f.home.join(".zshrc").exists(), "拉取不写 $HOME");
}

#[test]
fn pull_refuses_to_switch_branches_with_uncommitted_changes() {
    let f = fixtures::Fixture::new();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();
    f.git(&["branch", "dev"]);
    f.write_source("home/.zshrc", "export DIRTY=1\n");

    let text = refusal(
        core_on(&f, "dev")
            .pull()
            .expect_err("工作区脏时必须拒绝切分支"),
    );

    assert!(text.contains("main") && text.contains("dev"), "{text}");
    assert!(text.contains("未提交改动"), "{text}");
    assert!(text.contains("没有丢任何改动"), "{text}");
    assert_eq!(f.git(&["branch", "--show-current"]), "main", "分支不许动");
    assert_eq!(
        std::fs::read_to_string(f.source_dir.join("home/.zshrc")).expect("读源目录文件"),
        "export DIRTY=1\n",
        "未提交改动必须原样留着"
    );
}

#[test]
fn push_sends_head_to_the_configured_branch_not_to_its_upstream() {
    let f = fixtures::Fixture::new();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();

    // dev 从同一提交起家，上游却故意指向 origin/main。
    f.git(&["branch", "dev"]);
    f.git(&["push", "-q", "origin", "dev"]);
    f.git(&["checkout", "-q", "dev"]);
    f.git(&["branch", "--set-upstream-to=origin/main", "dev"]);
    let main_before = f.git_remote(&["rev-parse", "main"]);

    f.write_source("home/.zshrc", "export DEV=1\n");
    let core = core_on(&f, "dev");
    let outcome = core.commit_and_push().expect("推送应当成功");

    assert!(outcome.committed, "{outcome:?}");
    assert_eq!(
        f.git_remote(&["rev-parse", "dev"]),
        f.head_sha(),
        "设置的分支必须收到提交"
    );
    assert_eq!(
        f.git_remote(&["rev-parse", "main"]),
        main_before,
        "上游那个分支不该被动"
    );

    // 再点一次：目标是 dev，它已经跟上，不许谎报还有待推提交。
    let again = core.commit_and_push().expect("再点一次不该报错");
    assert!(!again.committed, "{again:?}");
    assert_eq!(again.existing_commits, 0, "{again:?}");
}

/// 上游与设置的分支不一致时，扫描基准必须是**目标分支**的远端跟踪 ref：
/// 密钥提交已经在 `origin/main` 上（因此 `main..HEAD` 是空的），但这次要送出去的是
/// `origin/dev` 收到的那一截——只按上游扫就会静默推出没扫过的内容。
#[test]
fn push_scans_the_commits_the_configured_branch_will_send() {
    let f = fixtures::Fixture::new();
    f.write_source("home/.zshrc", "export A=1\n");
    let first = f.commit_all("local: one");
    f.push();

    let other = f.other();
    f.git_at(&other, &["checkout", "-q", "main"]);
    // 远端 dev 停在第一个提交上。
    f.git_at(
        &other,
        &["push", "-q", "origin", &format!("{first}:refs/heads/dev")],
    );
    // main 上前进一步，并且这一格里带着密钥（绕过 app 直接推，模拟「已经推上去过」）。
    std::fs::write(
        other.join("home/id_rsa"),
        "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXk\n",
    )
    .expect("写密钥");
    f.git_at(&other, &["add", "-A"]);
    f.git_at(&other, &["commit", "-m", "local: key"]);
    f.git_at(&other, &["push", "-q", "origin", "main"]);
    f.git(&["fetch", "-q", "origin"]);

    let dev_before = f.git_remote(&["rev-parse", "dev"]);
    let main_before = f.git_remote(&["rev-parse", "main"]);

    // 本地 dev 指向 main 的最新提交，上游也指向 origin/main：`main..HEAD` 是空的。
    f.git(&["checkout", "-q", "-b", "dev", "origin/main"]);
    f.git(&["branch", "--set-upstream-to=origin/main", "dev"]);
    f.write_source("home/.zshrc", "export DEV=1\n");
    f.commit_all("local: dev move");

    let error = core_on(&f, "dev")
        .commit_and_push()
        .expect_err("目标分支要送出的提交里有密钥，必须拒绝");

    assert!(
        error.message.contains("id_rsa"),
        "要点名命中的路径：{}",
        error.message
    );
    assert_eq!(
        f.git_remote(&["rev-parse", "dev"]),
        dev_before,
        "被拒时远端 dev 不许动"
    );
    assert_eq!(
        f.git_remote(&["rev-parse", "main"]),
        main_before,
        "被拒时远端 main 不许动"
    );
}

#[test]
fn push_refuses_to_switch_branches_with_uncommitted_changes() {
    let f = fixtures::Fixture::new();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();
    f.git(&["branch", "dev"]);
    f.write_source("home/.zshrc", "export DIRTY=1\n");
    let remote_before = f.git_remote(&["rev-parse", "main"]);

    let text = refusal(
        core_on(&f, "dev")
            .commit_and_push()
            .expect_err("工作区脏时必须拒绝切分支"),
    );

    assert!(
        text.contains("未提交改动") && text.contains("没有丢任何改动"),
        "{text}"
    );
    assert_eq!(f.git(&["branch", "--show-current"]), "main");
    assert!(f.is_dirty(), "未提交改动必须原样留着");
    assert_eq!(
        f.git_remote(&["rev-parse", "main"]),
        remote_before,
        "远端不该收到任何东西"
    );
}
