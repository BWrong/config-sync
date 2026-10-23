//! 推送的行为测试：9 条路径规则 + 6 条内容规则逐条硬拦、正常推送、无可提交改动、push 失败。
//!
//! 唯一 seam 是 `Core`，全部用真 git（临时 home + 临时源目录 + 临时裸远端）。

mod fixtures;

use config_sync_core::{Core, CoreError, ErrorKind, PushOutcome};

/// `(规则原文, 命中路径, 近似但不该命中的路径)`；路径都是仓库内相对路径。
const PATH_RULES: &[(&str, &str, &str)] = &[
    ("**/id_rsa", "home/.ssh/id_rsa", "home/id_rsa_backup"),
    ("**/id_ed25519*", "home/.ssh/id_ed25519.pub", "home/keys/not_id_ed25519"),
    ("**/.ssh/id_*", "home/.ssh/id_ecdsa", "home/config/id_custom"),
    ("**/*.pem", "home/certs/server.pem", "home/certs/server.pemx"),
    ("**/*.p12", "home/certs/dev.p12", "home/certs/dev.p12x"),
    ("**/.netrc", "home/.netrc", "home/.netrc2"),
    ("**/*_history", "home/.zsh_history", "home/history"),
    ("**/credentials*", "home/.config/gcloud/credentials.json", "home/.config/gcloud/credential.json"),
    ("**/.aws/credentials", "home/.aws/credentials", "home/.aws/config"),
];

/// `(规则原文, 命中内容, 近似但不该命中的内容)`。
const CONTENT_RULES: &[(&str, &str, &str)] = &[
    (
        "-----BEGIN [A-Z ]*PRIVATE KEY-----",
        "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXk\n",
        "-----BEGIN PUBLIC KEY-----\nMIIBIjAN\n",
    ),
    ("ghp_", "GITHUB_TOKEN=ghp_0123456789abcdef\n", "GITHUB_TOKEN=ghpx_0123456789abcdef\n"),
    ("github_pat_", "token=github_pat_11ABCDEFG\n", "token=github_patX11ABCDEFG\n"),
    ("AKIA", "aws_access_key_id = AKIAIOSFODNN7EXAMPLE\n", "aws_access_key_id = AKIBIOSFODNN7EXAMPLE\n"),
    (r"xox[baprs]-", "slack=xoxb-1234567890-abcdef\n", "slack=xoxz-1234567890-abcdef\n"),
    ("sk-", "openai=sk-proj-abcdef123456\n", "openai=sk_proj_abcdef123456\n"),
];

fn head_message(f: &fixtures::Fixture) -> String {
    f.git(&["log", "-1", "--pretty=%s"])
}

/// 固定格式 `config-sync: N files (YYYY-MM-DD HH:MM)`；时间只校验形状（时区由系统给）。
fn assert_message_shape(message: &str, files: usize) {
    let prefix = format!("config-sync: {files} files (");
    let rest = message.strip_prefix(&prefix).unwrap_or_else(|| panic!("message 前缀不对：{message}"));
    let stamp = rest.strip_suffix(')').unwrap_or_else(|| panic!("message 缺右括号：{message}"));
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
    assert!(error.message.contains("推送被拒绝"), "message = {}", error.message);
    assert!(
        error.message.contains("加密在 v0.3 提供"),
        "文案必须固定指向 v0.3 加密：{}",
        error.message
    );
    assert_eq!(f.git(&["diff", "--cached", "--name-only"]), "", "命中后一个文件都不许进暂存区");
    assert!(!f.git_try(&["rev-parse", "--verify", "HEAD"]), "命中后不许留下提交");
    assert!(!f.remote.join("refs/heads/main").exists(), "命中后远端不该收到任何东西");
    error
}

#[test]
fn every_path_rule_blocks_the_whole_push_and_names_the_rule_and_file() {
    for (rule, hit, _) in PATH_RULES {
        let f = fixtures::Fixture::new();
        f.write_source(hit, "hello\n");
        let error = assert_blocked(&f, &f.core());
        assert!(error.message.contains(rule), "规则 {rule} 没被点名：{}", error.message);
        assert!(error.message.contains(hit), "文件 {hit} 没被点名：{}", error.message);
    }
}

#[test]
fn every_content_rule_blocks_the_whole_push_and_names_the_rule_and_file() {
    for (rule, hit, _) in CONTENT_RULES {
        let f = fixtures::Fixture::new();
        f.write_source("home/.zshrc", hit);
        let error = assert_blocked(&f, &f.core());
        assert!(error.message.contains(rule), "规则 {rule} 没被点名：{}", error.message);
        assert!(error.message.contains("home/.zshrc"), "文件没被点名：{}", error.message);
    }
}

#[test]
fn near_misses_are_not_blocked_and_the_push_goes_through() {
    let f = fixtures::Fixture::new();
    for (_, _, near_miss) in PATH_RULES {
        f.write_source(near_miss, "hello\n");
    }
    let near_miss_contents: String =
        CONTENT_RULES.iter().map(|(_, _, near_miss)| format!("{near_miss}")).collect();
    f.write_source("home/.zshrc", &near_miss_contents);

    let outcome = f.core().commit_and_push().expect("近似样例不该被拦");

    assert_eq!(outcome.files as usize, PATH_RULES.len() + 1);
    assert_eq!(f.git_remote(&["rev-parse", "main"]), f.head_sha(), "远端必须收到这次提交");
}

/// 硬拦看的是「工作区 ∪ 暂存区」：内容只在暂存区里也必须拦。
#[test]
fn a_secret_that_only_lives_in_the_index_is_blocked_too() {
    let f = fixtures::Fixture::new();
    f.write_source("home/.zshrc", "GITHUB_TOKEN=ghp_0123456789abcdef\n");
    f.git(&["add", "-A"]);
    f.write_source("home/.zshrc", "clean now\n");

    let error = f.core().commit_and_push().expect_err("暂存区里的密钥也必须拦");
    assert!(error.message.contains("ghp_"), "message = {}", error.message);
    assert!(!f.git_try(&["rev-parse", "--verify", "HEAD"]), "不许留下提交");
    assert!(!f.remote.join("refs/heads/main").exists(), "远端不该收到任何东西");
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
    assert_eq!(outcome.remote_url, f.remote_url());
    assert_message_shape(&outcome.message, 3);
    assert_eq!(head_message(&f), outcome.message, "本地提交 message 必须一致");
    // 裸远端真的收到这个提交（第一次推送顺带建立上游跟踪）。
    assert_eq!(f.git_remote(&["rev-parse", "main"]), f.head_sha());
    assert_eq!(f.git_remote(&["log", "-1", "--pretty=%s"]), outcome.message);
    assert!(!f.is_dirty(), "提交后源目录必须干净");

    let status = core.repo_status().expect("读取状态条");
    assert_eq!((status.ahead, status.behind, status.dirty), (0, 0, 0), "推送后 ahead 归零");

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

    let outcome = f.core().commit_and_push().expect("没有可提交的改动不是错误");

    assert!(!outcome.committed, "{outcome:?}");
    assert_eq!(outcome.files, 0);
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
    assert!(error.message.contains("已自动提交"), "提交成功必须如实说：{}", error.message);
    let stderr = error.stderr.expect("必须保留 git 原始 stderr");
    assert!(
        stderr.contains("rejected") || stderr.contains("non-fast-forward"),
        "stderr = {stderr}"
    );
    // 本地提交留下了（可接受的终态），远端一个字节没动。
    assert!(head_message(&f).starts_with("config-sync: 1 files ("), "本地提交应保留");
    assert!(!f.is_dirty());
    assert_eq!(f.git_remote(&["rev-parse", "main"]), remote_before);
}
