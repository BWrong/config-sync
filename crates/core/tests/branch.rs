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

/// 状态条与推送必须说同一件事：设置的分支是 `trunk`、HEAD 在 `main` 时，
/// 状态条要标出设置的分支，ahead/behind 也要按 `trunk` 的远端跟踪 ref 算。
/// 按当前分支的 `@{u}` 算出来的数字会和点下推送之后的结果对不上。
#[test]
fn the_status_bar_counts_against_the_configured_branch_and_says_so() {
    let f = fixtures::Fixture::new();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();
    // 远端有一个 trunk，且比 HEAD 多两个提交；本地 main 与自己的上游是齐的。
    let other = f.other();
    f.git_at(&other, &["checkout", "-q", "-b", "trunk"]);
    for content in ["export T1=1\n", "export T2=2\n"] {
        std::fs::write(other.join("home/.zshrc"), content).unwrap();
        f.git_at(&other, &["add", "-A"]);
        f.git_at(&other, &["commit", "-m", "config-sync: 1 files"]);
    }
    f.git_at(&other, &["push", "origin", "trunk"]);
    f.git(&[
        "fetch",
        "-q",
        "origin",
        "+refs/heads/trunk:refs/remotes/origin/trunk",
    ]);
    // 本地也有一条 trunk，停在远端的前一个提交上：落后的就是那两个。
    let base = f.head_sha();
    f.git(&["branch", "trunk", &base]);
    f.git(&["branch", "--set-upstream-to=origin/trunk", "trunk"]);
    assert_eq!(
        f.git(&["rev-list", "--count", "HEAD..origin/main"]),
        "0",
        "当前分支自己与上游是齐的：按 @{{u}} 算会得到 0/0"
    );

    let status = core_on(&f, "trunk").repo_status().unwrap();

    assert_eq!(status.branch, "main");
    assert_eq!(
        status.setting.as_deref(),
        Some("trunk"),
        "设置的分支与当前分支不一致时必须标出来：{status:?}"
    );
    assert_eq!(
        (status.ahead, status.behind),
        (0, 2),
        "ahead/behind 要对着设置分支 trunk 自己的 ref 算，而不是当前分支的 @{{u}}：{status:?}"
    );
}

/// 状态条的数字必须等于「按下推送真会送出多少」。
/// 反例：本地 trunk 比 origin/trunk 多 3 个提交，HEAD 停在 main 上、main 比 origin/trunk
/// 多 1 个——按 `<base>..HEAD` 算会显示 1，而推送切到 trunk 之后真送 3 个。
#[test]
fn the_status_bar_counts_what_a_push_would_really_send() {
    let f = fixtures::Fixture::new();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();

    f.git(&["switch", "-q", "-c", "trunk"]);
    f.git(&["push", "-q", "-u", "origin", "trunk"]);
    for index in 0..3 {
        f.write_source(&format!("home/trunk-{index}"), "x\n");
        f.commit_all(&format!("config-sync: trunk {index}"));
    }
    f.git(&["switch", "-q", "main"]);
    f.write_source("home/main-only", "y\n");
    f.commit_all("config-sync: main only");

    assert_eq!(
        f.git(&["rev-list", "--count", "origin/trunk..HEAD"]),
        "1",
        "按 HEAD 算只有 main 上那 1 条，trunk 上的 3 条会被吞掉——这正是要避免的口径"
    );

    let core = core_on(&f, "trunk");
    let status = core.repo_status().unwrap();
    assert_eq!(status.branch, "main");
    assert_eq!(status.setting.as_deref(), Some("trunk"));
    assert_eq!(
        status.ahead, 3,
        "设置分支 trunk 有 3 个未推提交：{status:?}"
    );

    let outcome = core.commit_and_push().unwrap();
    assert_eq!(
        status.ahead, outcome.existing_commits,
        "状态条上的 ahead 必须等于推送真送出的提交数"
    );
}

/// 推送把 HEAD 切到设置的分支时，结果里要带上「从哪儿切到哪儿」：
/// 否则用户刚在 main 上看到的待推提交，在「没有需要推送的改动」之后就像凭空消失了。
#[test]
fn push_reports_the_branch_it_switched_from() {
    let f = fixtures::Fixture::new();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();
    f.git(&["push", "-q", "origin", "main:trunk"]);
    f.git(&[
        "fetch",
        "-q",
        "origin",
        "+refs/heads/trunk:refs/remotes/origin/trunk",
    ]);
    // 本地也有一条 trunk，停在远端的前一个提交上：落后的就是那两个。
    let base = f.head_sha();
    f.git(&["branch", "trunk", &base]);
    f.git(&["branch", "--set-upstream-to=origin/trunk", "trunk"]);
    // 本地 main 上多一个没推的提交：状态条（按 @{u}）会显示 ahead 1。
    f.write_source("home/.zshrc", "export A=1\nexport LOCAL=2\n");
    f.commit_all("config-sync: 1 files");

    let outcome = core_on(&f, "trunk")
        .commit_and_push()
        .expect("干净的工作区应该能切到设置的分支再推送");

    assert_eq!(f.git(&["branch", "--show-current"]), "trunk");
    assert_eq!(
        outcome.switch_note.as_deref(),
        Some("已从 main 切到 trunk 再推送"),
        "切过分支就必须说出来：{outcome:?}"
    );
    assert!(!outcome.committed);
    assert_eq!(outcome.existing_commits, 0, "trunk 上确实没有待推提交");
    assert_eq!(
        f.git(&["rev-list", "--count", "origin/main..main"]),
        "1",
        "main 上那个提交还在本地，不该被抹掉"
    );
}

/// 远端不叫 `origin` 时，拉取要认上游配置的远端名（与推送同源），
/// 而不是硬编码一个不存在的 `origin`。
#[test]
fn pull_follows_the_upstream_remote_name_not_a_hardcoded_origin() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();
    f.git(&["remote", "rename", "origin", "fork"]);

    let other = f.other();
    std::fs::write(other.join("home/.zshrc"), "export B=9\n").unwrap();
    f.git_at(&other, &["add", "-A"]);
    f.git_at(&other, &["commit", "-m", "config-sync: 1 files"]);
    f.git_at(&other, &["push", "origin", "main"]);
    let remote_head = f.git_remote(&["rev-parse", "main"]);

    core.pull()
        .expect("远端改名为 fork 之后，拉取也要跟着上游的远端名走");

    assert_eq!(f.head_sha(), remote_head, "远端的新提交要落下来");
    assert!(!f.is_dirty(), "快进之后工作区必须是干净的");

    // 推送结果里显示的地址也得是这个远端，不能还去问一个不存在的 origin。
    let outcome = core
        .commit_and_push()
        .expect("远端改名之后推送也要对着 fork");
    assert_eq!(
        outcome.remote_url,
        f.remote_url(),
        "结果里要显示真正推送去的那个远端"
    );
}
