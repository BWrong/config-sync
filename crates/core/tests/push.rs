//! 推送的行为测试：9 条路径规则 + 6 条内容规则逐条硬拦、正常推送、无可提交改动、push 失败，
//! ahead-only（工作区干净、只有本地提交）、待推提交里的密钥（含「加了又删」的历史版本与
//! 扫描上限）、无上游的首次推送。
//!
//! 唯一 seam 是 `Core`，全部用真 git（临时 home + 临时源目录 + 临时裸远端）。

mod fixtures;

use config_sync_core::{Core, CoreError, ErrorKind, PushOutcome};

/// `(规则原文, 命中路径, 近似但不该命中的路径)`；路径都是仓库内相对路径。
const PATH_RULES: &[(&str, &str, &str)] = &[
    ("**/id_rsa", "home/.ssh/id_rsa", "home/id_rsa_backup"),
    (
        "**/id_ed25519*",
        "home/.ssh/id_ed25519.pub",
        "home/keys/not_id_ed25519",
    ),
    (
        "**/.ssh/id_*",
        "home/.ssh/id_ecdsa",
        "home/config/id_custom",
    ),
    (
        "**/*.pem",
        "home/certs/server.pem",
        "home/certs/server.pemx",
    ),
    ("**/*.p12", "home/certs/dev.p12", "home/certs/dev.p12x"),
    ("**/.netrc", "home/.netrc", "home/.netrc2"),
    ("**/*_history", "home/.zsh_history", "home/history"),
    (
        "**/credentials*",
        "home/.config/gcloud/credentials.json",
        "home/.config/gcloud/credential.json",
    ),
    (
        "**/.aws/credentials",
        "home/.aws/credentials",
        "home/.aws/config",
    ),
];

/// `(规则原文, 命中内容, 近似但不该命中的内容)`。
const CONTENT_RULES: &[(&str, &str, &str)] = &[
    (
        "-----BEGIN [A-Z ]*PRIVATE KEY-----",
        "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXk\n",
        "-----BEGIN PUBLIC KEY-----\nMIIBIjAN\n",
    ),
    (
        "ghp_",
        "GITHUB_TOKEN=ghp_0123456789abcdef\n",
        "GITHUB_TOKEN=ghpx_0123456789abcdef\n",
    ),
    (
        "github_pat_",
        "token=github_pat_11ABCDEFG\n",
        "token=github_patX11ABCDEFG\n",
    ),
    (
        "AKIA",
        "aws_access_key_id = AKIAIOSFODNN7EXAMPLE\n",
        "aws_access_key_id = AKIBIOSFODNN7EXAMPLE\n",
    ),
    (
        r"xox[baprs]-",
        "slack=xoxb-1234567890-abcdef\n",
        "slack=xoxz-1234567890-abcdef\n",
    ),
    (
        "sk-",
        "openai=sk-proj-abcdef1234567890abcdef\n",
        "openai=sk_proj_abcdef123456\n",
    ),
];

fn head_message(f: &fixtures::Fixture) -> String {
    f.git(&["log", "-1", "--pretty=%s"])
}

/// 固定格式 `config-sync: N files (YYYY-MM-DD HH:MM)`；时间只校验形状（时区由系统给）。
fn assert_message_shape(message: &str, files: usize) {
    let prefix = format!("config-sync: {files} files (");
    let rest = message
        .strip_prefix(&prefix)
        .unwrap_or_else(|| panic!("message 前缀不对：{message}"));
    let stamp = rest
        .strip_suffix(')')
        .unwrap_or_else(|| panic!("message 缺右括号：{message}"));
    assert_eq!(stamp.len(), 16, "时间必须是 YYYY-MM-DD HH:MM：{stamp}");
    for (i, c) in stamp.chars().enumerate() {
        let ok = match i {
            4 | 7 => c == '-',
            10 => c == ' ',
            13 => c == ':',
            _ => c.is_ascii_digit(),
        };
        assert!(ok, "时间第 {i} 位是 {c}：{stamp}");
    }
}

/// 命中硬拦的共用断言：整次拒绝、文案指向 v0.3 加密、暂存区为空、远端什么都没收到。
fn assert_blocked(f: &fixtures::Fixture, core: &Core) -> CoreError {
    let error = core.commit_and_push().expect_err("命中硬拦必须整次拒绝");
    assert!(
        error.message.contains("推送被拒绝"),
        "message = {}",
        error.message
    );
    assert!(
        error.message.contains("加密在 v0.3 提供"),
        "文案必须固定指向 v0.3 加密：{}",
        error.message
    );
    assert_eq!(
        f.git(&["diff", "--cached", "--name-only"]),
        "",
        "命中后一个文件都不许进暂存区"
    );
    assert!(
        !f.git_try(&["rev-parse", "--verify", "HEAD"]),
        "命中后不许留下提交"
    );
    assert!(
        !f.remote.join("refs/heads/main").exists(),
        "命中后远端不该收到任何东西"
    );
    error
}

#[test]
fn every_path_rule_blocks_the_whole_push_and_names_the_rule_and_file() {
    for (rule, hit, _) in PATH_RULES {
        let f = fixtures::Fixture::new();
        f.write_source(hit, "hello\n");
        let error = assert_blocked(&f, &f.core());
        assert!(
            error.message.contains(rule),
            "规则 {rule} 没被点名：{}",
            error.message
        );
        assert!(
            error.message.contains(hit),
            "文件 {hit} 没被点名：{}",
            error.message
        );
    }
}

#[test]
fn every_content_rule_blocks_the_whole_push_and_names_the_rule_and_file() {
    for (rule, hit, _) in CONTENT_RULES {
        let f = fixtures::Fixture::new();
        f.write_source("home/.zshrc", hit);
        let error = assert_blocked(&f, &f.core());
        assert!(
            error.message.contains(rule),
            "规则 {rule} 没被点名：{}",
            error.message
        );
        assert!(
            error.message.contains("home/.zshrc"),
            "文件没被点名：{}",
            error.message
        );
    }
}

#[test]
fn near_misses_are_not_blocked_and_the_push_goes_through() {
    let f = fixtures::Fixture::new();
    for (_, _, near_miss) in PATH_RULES {
        f.write_source(near_miss, "hello\n");
    }
    let near_miss_contents: String = CONTENT_RULES
        .iter()
        .map(|(_, _, near_miss)| format!("{near_miss}"))
        .collect();
    f.write_source("home/.zshrc", &near_miss_contents);

    let outcome = f.core().commit_and_push().expect("近似样例不该被拦");

    assert_eq!(outcome.files as usize, PATH_RULES.len() + 1);
    assert_eq!(
        f.git_remote(&["rev-parse", "main"]),
        f.head_sha(),
        "远端必须收到这次提交"
    );
}

/// 硬拦看的是「工作区 ∪ 暂存区」：内容只在暂存区里也必须拦。
#[test]
fn a_secret_that_only_lives_in_the_index_is_blocked_too() {
    let f = fixtures::Fixture::new();
    f.write_source("home/.zshrc", "GITHUB_TOKEN=ghp_0123456789abcdef\n");
    f.git(&["add", "-A"]);
    f.write_source("home/.zshrc", "clean now\n");

    let error = f
        .core()
        .commit_and_push()
        .expect_err("暂存区里的密钥也必须拦");
    assert!(
        error.message.contains("ghp_"),
        "message = {}",
        error.message
    );
    assert!(
        !f.git_try(&["rev-parse", "--verify", "HEAD"]),
        "不许留下提交"
    );
    assert!(
        !f.remote.join("refs/heads/main").exists(),
        "远端不该收到任何东西"
    );
}

#[test]
fn push_commits_with_the_fixed_message_and_lands_on_the_remote() {
    let f = fixtures::Fixture::new();
    let core = f.core();
    f.write_source("home/.zshrc", "export A=1\n");
    f.write_source("home/.config/nvim/init.lua", "-- init\n");
    f.write_source("home/.tmux.conf", "set -g mouse on\n");
    f.write_home(".zshrc", "export A=1\n");

    let outcome: PushOutcome = core.commit_and_push().expect("有改动时推送应成功");

    assert!(outcome.committed, "有改动时必须自动提交：{outcome:?}");
    assert_eq!(outcome.files, 3);
    assert_eq!(
        outcome.existing_commits, 0,
        "空仓库的第一次推送没有已有提交：{outcome:?}"
    );
    assert_eq!(outcome.remote_url, f.remote_url());
    assert_message_shape(&outcome.message, 3);
    assert_eq!(
        head_message(&f),
        outcome.message,
        "本地提交 message 必须一致"
    );
    // 裸远端真的收到这个提交（第一次推送顺带建立上游跟踪）。
    assert_eq!(f.git_remote(&["rev-parse", "main"]), f.head_sha());
    assert_eq!(f.git_remote(&["log", "-1", "--pretty=%s"]), outcome.message);
    assert!(!f.is_dirty(), "提交后源目录必须干净");

    let status = core.repo_status().expect("读取状态条");
    assert_eq!(
        (status.ahead, status.behind, status.dirty),
        (0, 0, 0),
        "推送后 ahead 归零"
    );

    // 第二次：已有上游时照常推。
    f.write_source("home/.zshrc", "export A=2\n");
    let again = core.commit_and_push().expect("第二次推送也应成功");
    assert_eq!(again.files, 1);
    assert_message_shape(&again.message, 1);
    assert_eq!(f.git_remote(&["rev-parse", "main"]), f.head_sha());
    assert_eq!(f.git_remote(&["show", "main:home/.zshrc"]), "export A=2");
}

#[test]
fn nothing_to_push_is_a_plain_result_not_an_empty_commit() {
    let f = fixtures::Fixture::new();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();
    let before = f.head_sha();

    let outcome = f
        .core()
        .commit_and_push()
        .expect("没有可提交的改动不是错误");

    assert!(!outcome.committed, "{outcome:?}");
    assert_eq!(outcome.files, 0);
    assert_eq!(outcome.existing_commits, 0, "没有待推提交：{outcome:?}");
    assert_eq!(f.head_sha(), before, "不许空提交");
    assert_eq!(f.git(&["rev-list", "--count", "HEAD"]), "1");
    assert!(!f.is_dirty());
}

#[test]
fn a_push_rejected_by_the_remote_is_reported_with_gits_own_words() {
    let f = fixtures::Fixture::new();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();
    f.advance_remote("home/.zshrc", "export A=1\nexport REMOTE=1\n");
    let remote_before = f.git_remote(&["rev-parse", "main"]);
    f.write_source("home/.tmux.conf", "set -g mouse on\n");

    let error = f.core().commit_and_push().expect_err("非快进必须失败");

    assert_eq!(error.kind, ErrorKind::Git, "{error:?}");
    assert!(
        error.message.contains("已自动提交"),
        "提交成功必须如实说：{}",
        error.message
    );
    let stderr = error.stderr.expect("必须保留 git 原始 stderr");
    assert!(
        stderr.contains("rejected") || stderr.contains("non-fast-forward"),
        "stderr = {stderr}"
    );
    // 本地提交留下了（可接受的终态），远端一个字节没动。
    assert!(
        head_message(&f).starts_with("config-sync: 1 files ("),
        "本地提交应保留"
    );
    assert!(!f.is_dirty());
    assert_eq!(f.git_remote(&["rev-parse", "main"]), remote_before);
}

/// 工作区干净、只有本地提交（`ahead > 0`）：照常推送，只推已有提交，不空提交。
#[test]
fn ahead_only_push_sends_existing_commits_without_making_a_new_one() {
    let f = fixtures::Fixture::new();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();
    f.write_source("home/.zshrc", "export A=2\n");
    f.commit_all("local: second");
    f.write_source("home/.tmux.conf", "set -g mouse on\n");
    f.commit_all("local: third");

    let core = f.core();
    let before = f.head_sha();
    assert_eq!(
        core.repo_status().expect("读状态条").ahead,
        2,
        "两个本地提交还没推"
    );

    let outcome = core.commit_and_push().expect("有本地提交时推送应成功");

    assert!(
        !outcome.committed,
        "没有可提交的改动，不许空提交：{outcome:?}"
    );
    assert_eq!(outcome.files, 0);
    assert_eq!(
        outcome.existing_commits, 2,
        "要如实报出推了几个已有提交：{outcome:?}"
    );
    assert_eq!(outcome.remote_url, f.remote_url());
    assert_eq!(f.head_sha(), before, "只推已有提交，不产生新提交");
    assert_eq!(
        f.git_remote(&["rev-parse", "main"]),
        before,
        "远端必须收到这些提交"
    );
    assert_eq!(f.git_remote(&["show", "main:home/.zshrc"]), "export A=2");

    let status = core.repo_status().expect("读状态条");
    assert_eq!(
        (status.ahead, status.dirty),
        (0, 0),
        "推送后 ahead 必须归零"
    );
}

/// 待推提交里的密钥同样硬拦：这些提交会被这次 push 一起送到远端。
#[test]
fn a_secret_in_an_unpushed_commit_blocks_the_push_and_names_the_file() {
    let f = fixtures::Fixture::new();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();
    let remote_before = f.git_remote(&["rev-parse", "main"]);

    f.write_source("home/.zshrc", "GITHUB_TOKEN=ghp_0123456789abcdef\n");
    f.commit_all("local: oops");
    assert!(!f.is_dirty(), "前提：工作区干净，密钥只在已提交的提交里");

    let error = f
        .core()
        .commit_and_push()
        .expect_err("待推提交里的密钥也必须拦");

    assert!(
        error.message.contains("推送被拒绝"),
        "message = {}",
        error.message
    );
    assert!(
        error.message.contains("ghp_"),
        "规则没被点名：{}",
        error.message
    );
    assert!(
        error.message.contains("home/.zshrc"),
        "文件没被点名：{}",
        error.message
    );
    assert_eq!(
        f.git_remote(&["rev-parse", "main"]),
        remote_before,
        "远端不该收到任何东西"
    );
    assert_eq!(
        f.git(&["rev-list", "--count", "@{u}..HEAD"]),
        "1",
        "本地提交原样保留，没有新提交"
    );
}

/// 待推提交里新增的私钥路径也按路径规则拦（内容本身可以完全无害）。
#[test]
fn a_private_key_path_in_an_unpushed_commit_is_blocked_by_the_path_rule() {
    let f = fixtures::Fixture::new();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();
    let remote_before = f.git_remote(&["rev-parse", "main"]);

    f.write_source("home/.ssh/id_rsa", "hello\n");
    f.commit_all("local: key");
    assert!(!f.is_dirty(), "前提：工作区干净");

    let error = f
        .core()
        .commit_and_push()
        .expect_err("待推提交里的私钥路径也必须拦");

    assert!(
        error.message.contains("**/id_rsa"),
        "规则没被点名：{}",
        error.message
    );
    assert!(
        error.message.contains("home/.ssh/id_rsa"),
        "文件没被点名：{}",
        error.message
    );
    assert_eq!(
        f.git_remote(&["rev-parse", "main"]),
        remote_before,
        "远端不该收到任何东西"
    );
}

/// 没有上游、只有本地提交：首次推送用 `-u origin <branch>` 建立跟踪，之后拉取认得上游。
#[test]
fn a_first_push_without_upstream_establishes_tracking() {
    let f = fixtures::Fixture::new();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("local: first");
    let core = f.core();
    assert!(
        !f.git_try(&["rev-parse", "--abbrev-ref", "@{u}"]),
        "前提：还没有上游"
    );

    let outcome = core
        .commit_and_push()
        .expect("没有上游时首次推送应建立跟踪");

    assert!(!outcome.committed, "{outcome:?}");
    assert_eq!(outcome.existing_commits, 1, "{outcome:?}");
    assert_eq!(
        f.git(&["rev-parse", "--abbrev-ref", "@{u}"]),
        "origin/main",
        "必须建立跟踪"
    );
    assert_eq!(f.git_remote(&["rev-parse", "main"]), f.head_sha());
    assert_eq!(core.repo_status().expect("读状态条").ahead, 0);
    core.pull().expect("建立跟踪后拉取必须能识别上游");
}

/// 没有上游、还有未提交改动：先提交再推送，同时建立跟踪。
#[test]
fn a_first_push_with_local_changes_commits_and_sets_tracking() {
    let f = fixtures::Fixture::new();
    f.write_source("home/.zshrc", "export A=1\n");

    let outcome = f
        .core()
        .commit_and_push()
        .expect("首次推送应提交并建立跟踪");

    assert!(outcome.committed, "{outcome:?}");
    assert_eq!(outcome.files, 1);
    assert_eq!(
        outcome.existing_commits, 0,
        "这次推送没有已有提交：{outcome:?}"
    );
    assert_eq!(f.git(&["rev-parse", "--abbrev-ref", "@{u}"]), "origin/main");
    assert_eq!(f.git_remote(&["rev-parse", "main"]), f.head_sha());
    assert_message_shape(&outcome.message, 1);
}

/// 路径里带换行（`cat-file` 查询串的分隔符）：工作区版本照样要拦。
#[test]
fn a_secret_in_a_path_containing_a_newline_is_blocked_in_the_workspace() {
    let f = fixtures::Fixture::new();
    f.write_source("home/bad\nname.txt", "GITHUB_TOKEN=ghp_0123456789abcdef\n");

    let error = assert_blocked(&f, &f.core());

    assert!(
        error.message.contains("ghp_"),
        "规则没被点名：{}",
        error.message
    );
    assert!(
        error.message.contains("home/bad\nname.txt"),
        "文件没被点名：{}",
        error.message
    );
}

/// 同一个路径，密钥只留在暂存区（工作区已经改干净）也必须拦。
#[test]
fn a_secret_in_a_path_containing_a_newline_is_blocked_in_the_index() {
    let f = fixtures::Fixture::new();
    f.write_source("home/bad\nname.txt", "GITHUB_TOKEN=ghp_0123456789abcdef\n");
    f.git(&["add", "-A"]);
    f.write_source("home/bad\nname.txt", "clean now\n");

    let error = f
        .core()
        .commit_and_push()
        .expect_err("暂存区里的密钥也必须拦");

    assert!(
        error.message.contains("推送被拒绝"),
        "message = {}",
        error.message
    );
    assert!(
        error.message.contains("ghp_"),
        "规则没被点名：{}",
        error.message
    );
    assert!(
        error.message.contains("home/bad\nname.txt"),
        "文件没被点名：{}",
        error.message
    );
    assert!(
        !f.git_try(&["rev-parse", "--verify", "HEAD"]),
        "不许留下提交"
    );
    assert!(
        !f.remote.join("refs/heads/main").exists(),
        "远端不该收到任何东西"
    );
}

/// 待推提交里、路径带换行的文件同样扫得到。
#[test]
fn a_secret_in_an_unpushed_commit_under_a_path_with_a_newline_is_blocked() {
    let f = fixtures::Fixture::new();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();
    let remote_before = f.git_remote(&["rev-parse", "main"]);

    f.write_source("home/bad\nname.txt", "GITHUB_TOKEN=ghp_0123456789abcdef\n");
    f.commit_all("local: oops");
    assert!(!f.is_dirty(), "前提：工作区干净，密钥只在已提交的提交里");

    let error = f
        .core()
        .commit_and_push()
        .expect_err("待推提交里的密钥也必须拦");

    assert!(
        error.message.contains("ghp_"),
        "规则没被点名：{}",
        error.message
    );
    assert!(
        error.message.contains("home/bad\nname.txt"),
        "文件没被点名：{}",
        error.message
    );
    assert_eq!(
        f.git_remote(&["rev-parse", "main"]),
        remote_before,
        "远端不该收到任何东西"
    );
}

/// 没有上游、但远端已有同名分支且已同步：ahead 必须是 0，既不谎报「推了已有提交」，
/// 也不为「确认无事可做」跑一次网络（远端已搬走，联网必然失败）。
#[test]
fn a_repo_without_upstream_whose_remote_is_already_in_sync_reports_nothing_and_pushes_nothing() {
    let f = fixtures::Fixture::new();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("local: first");
    // 手动推一次但不建跟踪：本地仍没有上游，远端已有同名分支。
    f.git(&["push", "origin", "main"]);
    assert!(
        !f.git_try(&["rev-parse", "--abbrev-ref", "@{u}"]),
        "前提：仍没有上游"
    );
    std::fs::rename(&f.remote, f.path("remote.offline.git")).expect("把远端挪走");
    let before = f.head_sha();

    let outcome = f.core().commit_and_push().expect("远端已同步时不该报错");

    assert!(!outcome.committed, "{outcome:?}");
    assert_eq!(
        outcome.existing_commits, 0,
        "远端已同步就不许谎报已有提交：{outcome:?}"
    );
    assert_eq!(f.head_sha(), before, "不许空提交");
    assert_eq!(
        f.git(&["rev-parse", "origin/main"]),
        before,
        "远端跟踪分支原样不动"
    );
}

/// 上游配置还在、远端跟踪分支却被删了：算不出 ahead 也不能当成「没有需要推送的改动」。
#[test]
fn a_deleted_remote_tracking_ref_does_not_hide_local_commits() {
    let f = fixtures::Fixture::new();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("local: first");
    f.push();
    f.git(&["update-ref", "-d", "refs/remotes/origin/main"]);
    f.write_source("home/.zshrc", "export A=2\n");
    f.commit_all("local: second");
    assert!(
        !f.is_dirty(),
        "前提：工作区干净，要让推进去只能靠 ahead 判断"
    );

    let outcome = f
        .core()
        .commit_and_push()
        .expect("算不出 ahead 也不能当成没事可做");

    assert!(!outcome.committed, "{outcome:?}");
    assert!(outcome.existing_commits >= 1, "{outcome:?}");
    assert_eq!(
        f.git_remote(&["rev-parse", "main"]),
        f.head_sha(),
        "本地提交必须推上去"
    );
    assert_eq!(
        f.git(&["rev-parse", "origin/main"]),
        f.head_sha(),
        "推送后跟踪分支恢复"
    );
}

/// 用 `git fast-import` 一次性造出大量历史形状（逐个 `git commit` 太慢），
/// 然后 `reset --hard` 让工作区与新的 HEAD 对齐。
fn fast_import(f: &fixtures::Fixture, script: &str) {
    use std::io::Write as _;
    use std::process::{Command, Stdio};
    let mut child = Command::new("git")
        .arg("-C")
        .arg(&f.source_dir)
        .args(["fast-import", "--quiet", "--force"])
        .stdin(Stdio::piped())
        .spawn()
        .expect("启动 git fast-import");
    child
        .stdin
        .take()
        .expect("fast-import 的 stdin")
        .write_all(script.as_bytes())
        .expect("写 fast-import 脚本");
    assert!(
        child.wait().expect("等 fast-import 结束").success(),
        "fast-import 失败"
    );
    f.git(&["reset", "--hard", "-q"]);
}

/// 一个提交只带一个文件，内容各不相同（否则 blob 会被去重成同一个）。
fn inline_commit_script(paths: &[String]) -> String {
    let mut script = String::from(
        "commit refs/heads/main\ncommitter c <c@config-sync.invalid> 1 +0000\ndata 1\nb\n",
    );
    for path in paths {
        let body = format!("{path}\n");
        script.push_str(&format!(
            "M 100644 inline {path}\ndata {}\n{body}",
            body.len()
        ));
    }
    script
}

/// 「加了又删」：提交 A 加进密钥、提交 B 把文件删掉，两者都还没推。
///
/// 这时最终 diff 是空的、HEAD 树里也没有这个路径，可 A 的 blob 照样会随这次 push 出去——
/// 按最终 diff 扫就会把它漏掉（#15 要堵的正是这个洞）。
#[test]
fn a_secret_added_and_then_deleted_in_unpushed_commits_is_still_blocked() {
    let f = fixtures::Fixture::new();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();
    let remote_before = f.git_remote(&["rev-parse", "main"]);

    f.write_source("home/token.txt", "GITHUB_TOKEN=ghp_0123456789abcdef\n");
    f.commit_all("local: add the token");
    std::fs::remove_file(f.source_dir.join("home/token.txt")).expect("删掉刚加的密钥");
    f.commit_all("local: delete it again");
    assert!(!f.is_dirty(), "前提：工作区干净，密钥只在待推提交里");
    assert!(
        !f.git(&["ls-tree", "-r", "--name-only", "HEAD"])
            .contains("home/token.txt"),
        "前提：HEAD 树里已经没有这个路径，最终 diff 里也没有它"
    );

    let error = f
        .core()
        .commit_and_push()
        .expect_err("待推提交里的密钥必须拦，删掉也不行");

    assert!(
        error.message.contains("推送被拒绝"),
        "message = {}",
        error.message
    );
    assert!(
        error.message.contains("ghp_"),
        "规则没被点名：{}",
        error.message
    );
    assert!(
        error.message.contains("home/token.txt"),
        "文件没被点名：{}",
        error.message
    );
    assert_eq!(
        f.git_remote(&["rev-parse", "main"]),
        remote_before,
        "远端不该收到任何东西"
    );
}

/// 加了、删了、又加回来：路径在最终树里，同样必须拦。
#[test]
fn a_secret_deleted_and_then_added_back_in_unpushed_commits_is_blocked() {
    let f = fixtures::Fixture::new();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();
    let remote_before = f.git_remote(&["rev-parse", "main"]);

    f.write_source("home/token.txt", "GITHUB_TOKEN=ghp_0123456789abcdef\n");
    f.commit_all("local: add");
    std::fs::remove_file(f.source_dir.join("home/token.txt")).expect("删掉");
    f.commit_all("local: delete");
    f.write_source("home/token.txt", "GITHUB_TOKEN=ghp_fedcba9876543210\n");
    f.commit_all("local: add back");
    assert!(!f.is_dirty());

    let error = f
        .core()
        .commit_and_push()
        .expect_err("加回来的密钥同样要拦");

    assert!(
        error.message.contains("ghp_"),
        "规则没被点名：{}",
        error.message
    );
    assert!(
        error.message.contains("home/token.txt"),
        "文件没被点名：{}",
        error.message
    );
    assert_eq!(
        f.git_remote(&["rev-parse", "main"]),
        remote_before,
        "远端不该收到任何东西"
    );
}

/// 单个 blob 超过 16 MiB：读内容之前就整次中止，文案点名文件与大小，不「太大就跳过」。
#[test]
fn a_blob_over_the_scan_size_limit_aborts_the_push_and_names_the_file() {
    let f = fixtures::Fixture::new();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();
    let remote_before = f.git_remote(&["rev-parse", "main"]);

    f.write_source("home/big.bin", &"x".repeat(16 * 1024 * 1024 + 1));
    f.commit_all("local: too big");
    assert!(!f.is_dirty());

    let error = f
        .core()
        .commit_and_push()
        .expect_err("超过单 blob 上限必须中止");

    assert!(
        error.message.contains("推送被拒绝"),
        "message = {}",
        error.message
    );
    assert!(
        error.message.contains("home/big.bin"),
        "文件没被点名：{}",
        error.message
    );
    assert!(
        error.message.contains("上限 16.0 MiB"),
        "上限没被点名：{}",
        error.message
    );
    assert!(
        error.message.contains("git push"),
        "得告诉用户下一步：{}",
        error.message
    );
    assert_eq!(
        f.git_remote(&["rev-parse", "main"]),
        remote_before,
        "远端不该收到任何东西"
    );
}

/// 待推提交超过 1000 个：整次中止，文案给出当前值、上限与下一步。
#[test]
fn too_many_unpushed_commits_abort_the_push_with_the_numbers() {
    let f = fixtures::Fixture::new();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();
    let remote_before = f.git_remote(&["rev-parse", "main"]);

    let mut script = String::new();
    for index in 0..1001 {
        let message = format!("c{index}");
        script.push_str(&format!(
            "commit refs/heads/main\ncommitter c <c@config-sync.invalid> {index} +0000\ndata {}\n{message}\n",
            message.len()
        ));
    }
    fast_import(&f, &script);
    assert_eq!(f.git(&["rev-list", "--count", "@{u}..HEAD"]), "1001");

    let error = f
        .core()
        .commit_and_push()
        .expect_err("超过 1000 个待推提交必须中止");

    assert!(
        error.message.contains("未推送提交数"),
        "message = {}",
        error.message
    );
    assert!(
        error.message.contains("1001 个"),
        "当前值没被点名：{}",
        error.message
    );
    assert!(
        error.message.contains("上限 1000 个"),
        "上限没被点名：{}",
        error.message
    );
    assert_eq!(
        f.git_remote(&["rev-parse", "main"]),
        remote_before,
        "远端不该收到任何东西"
    );
}

/// 待扫的 blob 版本超过 10000 个：同样中止，不扫完就推等于把硬拦关掉。
#[test]
fn too_many_unpushed_blobs_abort_the_push_with_the_numbers() {
    let f = fixtures::Fixture::new();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();
    let remote_before = f.git_remote(&["rev-parse", "main"]);

    let paths: Vec<String> = (0..=10_000)
        .map(|index| format!("home/f{index}.txt"))
        .collect();
    fast_import(&f, &inline_commit_script(&paths));

    let error = f
        .core()
        .commit_and_push()
        .expect_err("超过 10000 个 blob 版本必须中止");

    assert!(
        error.message.contains("待扫描的文件版本数"),
        "message = {}",
        error.message
    );
    assert!(
        error.message.contains("10001 个"),
        "当前值没被点名：{}",
        error.message
    );
    assert!(
        error.message.contains("上限 10000 个"),
        "上限没被点名：{}",
        error.message
    );
    assert_eq!(
        f.git_remote(&["rev-parse", "main"]),
        remote_before,
        "远端不该收到任何东西"
    );
}

/// `push.default=matching`（git 2.0 之前的默认值，仍是合法机器配置）下，裸 `git push` 会把
/// 远端已有的同名分支一起推上去；那些分支不在 `@{u}..HEAD` 的扫描范围内，所以推送目标必须
/// 显式写出来，让「送出去的」字面上等于「扫过的」。
#[test]
fn a_matching_push_default_cannot_smuggle_an_unscanned_branch_past_the_scan() {
    let f = fixtures::Fixture::new();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();

    // 远端先有一个干净的 other 分支。
    f.git(&["checkout", "-q", "-b", "other"]);
    f.write_source("home/other.conf", "k = 1\n");
    f.commit_all("local: other");
    f.git(&["push", "-q", "origin", "other"]);
    let other_remote_before = f.git_remote(&["rev-parse", "other"]);

    // 之后在 other 上提交一个密钥，但不推。
    f.write_source("home/token.conf", "GITHUB_TOKEN=ghp_0123456789abcdef\n");
    let secret_sha = f.commit_all("local: secret on other");

    // 回到 main 正常改一个文件，并把 push.default 调成 matching。
    f.git(&["checkout", "-q", "main"]);
    f.write_source("home/.zshrc", "export A=2\n");
    f.git(&["config", "push.default", "matching"]);

    let outcome = f.core().commit_and_push().expect("main 自己的推送应当成功");

    assert!(outcome.committed, "{outcome:?}");
    assert_eq!(
        f.git_remote(&["rev-parse", "main"]),
        f.head_sha(),
        "main 该推上去"
    );
    assert_ne!(
        f.git_remote(&["rev-parse", "other"]),
        secret_sha,
        "含着密钥的 other 分支绝不能被顺手推上去"
    );
    assert_eq!(
        f.git_remote(&["rev-parse", "other"]),
        other_remote_before,
        "other 的远端 ref 不该动"
    );
}

/// 待推区间里有取不到的对象（仓库损坏）：扫不到就不能推，错误里要能看到是哪个 oid。
///
/// 实际触发点在 `git rev-list --objects`（它自己回 `missing blob object '<oid>'`）；
/// `cat_file_meta` 的 `missing` 检查是第二道保险 —— rev-list 万一放行，也绝不能把那个
/// 版本当成「没内容可扫」。
#[test]
fn a_missing_object_in_the_unpushed_range_aborts_the_scan() {
    let f = fixtures::Fixture::new();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();

    let remote_before = f.git_remote(&["rev-parse", "main"]);
    f.write_source("home/lost.conf", "k = 1\n");
    f.commit_all("local: one");

    // 删掉那个 blob 的松散对象文件，制造「rev-list 列了但取不到」。
    let oid = f.git(&["rev-parse", "HEAD:home/lost.conf"]);
    let (dir, file) = oid.split_at(2);
    std::fs::remove_file(f.source_dir.join(".git/objects").join(dir).join(file))
        .expect("删掉松散对象");

    let error = f
        .core()
        .commit_and_push()
        .expect_err("缺对象时必须中止，不能当没内容可扫");

    let text = format!("{} / {:?}", error.message, error.stderr);
    assert!(text.contains(&oid), "必须点名缺失的 oid：{text}");
    assert_eq!(
        f.git_remote(&["rev-parse", "main"]),
        remote_before,
        "缺对象时什么都不能推"
    );
}

/// 本地分支与设置的分支不一致（`git branch -m` 之后很常见）：不静默推、也不静默切过来，
/// 说清两边的名字与未提交改动数就停，改动与远端都不动。
#[test]
fn a_local_branch_that_differs_from_the_setting_is_refused_not_silently_pushed() {
    let f = fixtures::Fixture::new();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();

    // 本地改名 trunk，上游仍是 origin/main，设置里仍写 main。
    f.git(&["branch", "-m", "main", "trunk"]);
    f.git(&["branch", "--set-upstream-to=origin/main", "trunk"]);
    f.write_source("home/.zshrc", "export A=2\n");

    let remote_before = f.git_remote(&["rev-parse", "main"]);
    let head = f.head_sha();

    let error = f
        .core()
        .commit_and_push()
        .expect_err("分支与设置不一致且工作区脏时必须拒绝");

    let text = format!("{} / {:?}", error.message, error.stderr);
    assert!(text.contains("trunk") && text.contains("main"), "{text}");
    assert!(text.contains("未提交改动"), "要说明是脏在哪：{text}");
    assert!(text.contains("没有丢任何改动"), "{text}");
    assert_eq!(
        f.git(&["branch", "--show-current"]),
        "trunk",
        "不许偷偷切分支"
    );
    assert_eq!(f.head_sha(), head, "不许偷偷提交");
    assert!(f.is_dirty(), "改动必须原样留着");
    assert_eq!(
        f.git_remote(&["rev-parse", "main"]),
        remote_before,
        "远端不该收到任何东西"
    );
}

/// 上游指向本地分支（`branch.<b>.remote = .`）时推不到任何远端：推送仍发往 origin 上
/// 设置的那个分支，否则「推送」会在本地空转一场、远端永远收不到东西。
#[test]
fn a_local_branch_upstream_falls_back_to_origin_and_the_configured_branch() {
    let f = fixtures::Fixture::new();
    f.write_source("home/.zshrc", "export A=1\n");
    f.commit_all("config-sync: 1 files");
    f.push();

    f.git(&["branch", "mirror"]);
    f.git(&["branch", "--set-upstream-to=mirror", "main"]);
    let mirror_before = f.git(&["rev-parse", "mirror"]);

    f.write_source("home/.zshrc", "export A=2\n");
    let outcome = f.core().commit_and_push().expect("推送应当成功");

    assert!(outcome.committed, "{outcome:?}");
    assert_eq!(
        f.git_remote(&["rev-parse", "main"]),
        f.head_sha(),
        "远端 main 必须前进"
    );
    assert_eq!(
        f.git(&["rev-parse", "mirror"]),
        mirror_before,
        "本地分支不是推送终点，不该被动"
    );

    let again = f.core().commit_and_push().expect("再点一次不该报错");
    assert_eq!(again.existing_commits, 0, "{again:?}");
}

/// 单个文件版本的内容扫描闸门 = `MAX_BLOB_BYTES`（16 MiB，与待推 blob 一侧同一口径）；
/// 这里造「刚好超过闸门」的文件。
const SCAN_GATE: u64 = 16 * 1024 * 1024 + 1;

/// 造一个刚超过闸门的稀疏文件：只真写开头那段内容，其余靠 `set_len` 拉长，
/// 不真写 16 MiB 字节。内容里有密钥，用来证明闸门确实挡掉了内容扫描。
fn write_oversized(f: &fixtures::Fixture, rel_path: &str, content: &str) {
    use std::io::Write;

    let path = f.source_dir.join(rel_path);
    std::fs::create_dir_all(path.parent().expect("必有父目录")).expect("建父目录");
    let mut file = std::fs::File::create(&path).expect("建大文件");
    file.write_all(content.as_bytes()).expect("写前缀");
    file.set_len(SCAN_GATE).expect("拉长到闸门之上");
}

/// 超过闸门的 dirty 文件不做内容扫描：整份读入 + 全文正则那条路被闸门挡掉。
/// 这是有意的行为变更（见 `push.rs` 的闸门注释与 PR 说明）。
#[test]
fn an_oversized_dirty_file_is_not_scanned_for_content() {
    let f = fixtures::Fixture::new();
    write_oversized(&f, "home/big.log", "GITHUB_TOKEN=ghp_0123456789abcdef\n");

    let outcome = f
        .core()
        .commit_and_push()
        .expect("超大文件不该走内容扫描，也就不该被内容正则拦");

    assert!(outcome.committed, "{outcome:?}");
    assert_eq!(outcome.files, 1);
    assert_eq!(f.git_remote(&["rev-parse", "main"]), f.head_sha());
}

/// 闸门只挡内容规则：路径规则照旧命中，且命中文案里不许出现内容规则——
/// 出现了就说明这台老路还在读整份内容。
#[test]
fn an_oversized_dirty_file_still_hits_path_rules() {
    let f = fixtures::Fixture::new();
    write_oversized(
        &f,
        "home/.ssh/id_rsa",
        "GITHUB_TOKEN=ghp_0123456789abcdef\n",
    );

    let error = assert_blocked(&f, &f.core());
    assert!(
        error.message.contains("**/id_rsa"),
        "路径规则必须照旧命中：{}",
        error.message
    );
    assert!(
        !error.message.contains("ghp_"),
        "超大文件不该进内容扫描：{}",
        error.message
    );
    assert!(
        error.message.contains("命中 2 条安全规则"),
        "只该有路径规则这两条（`**/id_rsa` 与 `**/.ssh/id_*`）：{}",
        error.message
    );
}

/// 暂存区里超过闸门的版本同样不读内容：工作区那份改成小小的一份干净内容，
/// 就只剩暂存区的大版本——闸门必须也把它挡在内容扫描之外。
#[test]
fn an_oversized_staged_version_is_not_scanned_for_content() {
    let f = fixtures::Fixture::new();
    write_oversized(&f, "home/big.log", "GITHUB_TOKEN=ghp_0123456789abcdef\n");
    f.git(&["add", "-A"]);
    f.write_source("home/big.log", "clean now\n");

    let outcome = f
        .core()
        .commit_and_push()
        .expect("暂存区里超过闸门的版本不该被内容正则拦");

    assert!(outcome.committed, "{outcome:?}");
    assert_eq!(outcome.files, 1);
}
