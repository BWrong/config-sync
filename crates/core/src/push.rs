//! 推送：硬拦 → `git add -A` → 自动提交 → `git push`，三步在同一个方法里。
//!
//! 硬拦在 `git add` 之前跑，看的是「工作区 ∪ 暂存区」：命中任一条规则就整次拒绝，
//! 一个文件都不进暂存区，也没有 override 通道（见 DESIGN.md）。

use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;
use serde::Serialize;

use crate::{Core, CoreError, git, state};

/// 命中硬拦时固定的解释；没有「强行继续」。
const ENCRYPTION_HINT: &str = "私钥/凭据类文件需要先加密才能同步，加密在 v0.3 提供。";

/// 9 条路径规则。`**/` 前缀表示「仓库内任意层级」。
const PATH_RULES: &[&str] = &[
    "**/id_rsa",
    "**/id_ed25519*",
    "**/.ssh/id_*",
    "**/*.pem",
    "**/*.p12",
    "**/.netrc",
    "**/*_history",
    "**/credentials*",
    "**/.aws/credentials",
];

/// 6 条内容规则（正则）。与 `crates/core/tests/push.rs` 的表一一对应。
const CONTENT_RULES: &[&str] = &[
    r"-----BEGIN [A-Z ]*PRIVATE KEY-----",
    "ghp_",
    "github_pat_",
    "AKIA",
    r"xox[baprs]-",
    "sk-",
];

static CONTENT_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    CONTENT_RULES.iter().map(|rule| Regex::new(rule).expect("内容规则是合法正则")).collect()
});

/// 一次推送的结果，供状态条显示。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PushOutcome {
    /// 这次是否产生了自动提交；false = 没有需要推送的改动。
    pub committed: bool,
    /// 本次提交涉及的文件数。
    pub files: u32,
    /// 固定格式的提交 message；没提交时是空串。
    pub message: String,
    /// 推送目标的远端地址。
    pub remote_url: String,
}

impl Core {
    /// 推送 = 硬拦 → `git add -A` → `git commit -m "<固定格式>"` → `git push`。
    ///
    /// 命中硬拦时整次拒绝：不暂存、不提交、不推送，错误里点名规则与文件。
    /// 没有可提交的改动时返回 `committed: false`，不报错也不空提交。
    /// 提交成功而 push 失败是可接受的终态：错误里如实说明「已提交」并带上 git 原始 stderr。
    pub fn commit_and_push(&self) -> Result<PushOutcome, CoreError> {
        let hits = scan(&self.source_dir)?;
        if !hits.is_empty() {
            return Err(blocked_error(&hits));
        }

        if !has_changes(&self.source_dir)? {
            return Ok(self.outcome(false, 0, String::new()));
        }

        git::run(&self.source_dir, &["add", "-A"])?;
        let files = staged_file_count(&self.source_dir)?;
        let message =
            format!("config-sync: {files} files ({})", chrono::Local::now().format("%Y-%m-%d %H:%M"));
        git::run(&self.source_dir, &["commit", "-m", &message])
            .map_err(|e| CoreError { message: format!("自动提交「{message}」失败"), ..e })?;

        // 第一次推送没有上游，顺手建立跟踪（拉取那边靠它判断能否 fetch）。
        let branch = git::run(&self.source_dir, &["branch", "--show-current"])?;
        let push: Vec<String> = if git::run(&self.source_dir, &["rev-parse", "--abbrev-ref", "@{u}"]).is_ok() {
            vec!["push".to_string()]
        } else {
            vec!["push".to_string(), "-u".to_string(), "origin".to_string(), branch]
        };
        let args: Vec<&str> = push.iter().map(String::as_str).collect();
        git::run(&self.source_dir, &args).map_err(|e| CoreError {
            message: format!("已自动提交「{message}」，但推送失败：远端没有接受这个提交"),
            ..e
        })?;

        Ok(self.outcome(true, files, message))
    }

    fn outcome(&self, committed: bool, files: u32, message: String) -> PushOutcome {
        PushOutcome { committed, files, message, remote_url: self.remote_url().unwrap_or_default() }
    }
}

/// 仓库里是否有未提交改动（含未跟踪、含暂存）。
fn has_changes(source_dir: &Path) -> Result<bool, CoreError> {
    let porcelain = git::run(
        source_dir,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )?;
    Ok(!porcelain.is_empty())
}

/// `git add -A` 之后暂存区里的文件数 = 本次提交涉及的文件数。
fn staged_file_count(source_dir: &Path) -> Result<u32, CoreError> {
    let names = git::run(source_dir, &["diff", "--cached", "--name-only"])?;
    Ok(names.lines().filter(|line| !line.is_empty()).count() as u32)
}

/// 一条命中：规则原文 + 命中的仓库内路径 + 规则种类。
struct Hit {
    kind: &'static str,
    rule: &'static str,
    path: String,
}

/// 在「工作区 ∪ 暂存区」上跑 9 条路径规则与 6 条内容规则。
fn scan(source_dir: &Path) -> Result<Vec<Hit>, CoreError> {
    let facts = state::GitFacts::read(source_dir)?;
    let mut paths: Vec<_> = facts.dirty_paths().cloned().collect();
    paths.sort();

    let mut hits: Vec<Hit> = Vec::new();
    for path in &paths {
        let repo_path = path.to_string_lossy().replace('\\', "/");
        let mut push_hit = |kind: &'static str, rule: &'static str| {
            // 同一规则在同一文件上只报一次（暂存区与工作区各扫一遍）。
            if !hits.iter().any(|hit| hit.rule == rule && hit.path == repo_path) {
                hits.push(Hit { kind, rule, path: repo_path.clone() });
            }
        };

        for rule in PATH_RULES {
            if path_rule_matches(rule, &repo_path) {
                push_hit("路径", rule);
            }
        }

        for text in contents(source_dir, path) {
            for (rule, pattern) in CONTENT_RULES.iter().zip(CONTENT_PATTERNS.iter()) {
                if pattern.is_match(&text) {
                    push_hit("内容", rule);
                }
            }
        }
    }
    Ok(hits)
}

/// 一个路径在暂存区与工作区里的文本内容（二进制按有损 UTF-8 读，不影响正则）。
fn contents(source_dir: &Path, repo_path: &Path) -> Vec<String> {
    let worktree = std::fs::read(source_dir.join(repo_path))
        .ok()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
    // 未跟踪的文件不在暂存区，`git show :<路径>` 会失败，按「没有这份内容」处理。
    let staged =
        git::run(source_dir, &["show", &format!(":{}", repo_path.display())]).ok();
    worktree.into_iter().chain(staged).collect()
}

/// `**/<尾部>` 的匹配：尾部可以落在路径的任意一段之后。
///
/// shortcut: 规则表只用到单个 `*`，多 `*` 的通配要换成真 glob 库。
fn path_rule_matches(rule: &str, repo_path: &str) -> bool {
    let tail = rule.strip_prefix("**/").unwrap_or(rule);
    let mut candidate = repo_path.trim_start_matches("./");
    loop {
        if glob_segment_matches(tail, candidate) {
            return true;
        }
        match candidate.split_once('/') {
            Some((_, rest)) => candidate = rest,
            None => return false,
        }
    }
}

fn glob_segment_matches(pattern: &str, text: &str) -> bool {
    match pattern.split_once('*') {
        Some((prefix, suffix)) => {
            text.len() >= prefix.len() + suffix.len()
                && text.starts_with(prefix)
                && text.ends_with(suffix)
        }
        None => text == pattern,
    }
}

fn blocked_error(hits: &[Hit]) -> CoreError {
    let names: Vec<String> = hits
        .iter()
        .map(|hit| format!("{}规则「{}」→ {}", hit.kind, hit.rule, hit.path))
        .collect();
    CoreError::io(format!(
        "推送被拒绝：命中 {} 条安全规则，没有提交也没有推送。{ENCRYPTION_HINT}命中：{}",
        names.len(),
        names.join("；")
    ))
}
