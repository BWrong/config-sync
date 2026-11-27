//! 推送：硬拦 → `git add -A` → 自动提交 → `git push`。
//!
//! 硬拦看的是「工作区 ∪ 暂存区 ∪ 待推提交（`@{u}..HEAD`）」：命中任一条规则就整次拒绝，
//! 一个文件都不进暂存区，也没有 override 通道（见 DESIGN.md）。已提交但没推送的提交同样
//! 要扫——这次 `git push` 会把它们一起送到远端，不扫就等于没有硬拦。
//!
//! 工作区干净但 `ahead > 0` 时照常推送（只推已有提交）；没有上游时用
//! `push -u origin <branch>` 建立跟踪。暂存区与待推提交的内容一次 `git cat-file --batch`
//! 取完，文件再多也只是一个进程。

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::LazyLock;

use regex::Regex;
use serde::Serialize;

use crate::{Core, CoreError, ErrorKind, git, state};

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
    // 避开 `task-`/`disk-` 之类普通文本的误伤：需要 16+ 位密钥体才算命中。
    r"sk-[A-Za-z0-9_-]{16,}",
];

static CONTENT_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    CONTENT_RULES.iter().map(|rule| Regex::new(rule).expect("内容规则是合法正则")).collect()
});

/// 一次推送的结果，供状态条显示。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PushOutcome {
    /// 这次是否产生了自动提交；false = 没有产生新提交。
    pub committed: bool,
    /// 本次提交涉及的文件数；没提交时是 0。
    pub files: u32,
    /// 固定格式的提交 message；没提交时是空串。
    pub message: String,
    /// 推送目标的远端地址。
    pub remote_url: String,
    /// 推送前就已在本地、这次顺手推出去的提交数（不含本次自动提交）；
    /// `0` 且 `committed == false` 才是真的「没有需要推送的改动」。
    pub existing_commits: u32,
}

impl Core {
    /// 推送 = 硬拦 → `git add -A` → `git commit -m "<固定格式>"` → `git push`。
    ///
    /// 命中硬拦时整次拒绝：不暂存、不提交、不推送，错误里点名规则与文件。
    /// 没有可提交的改动、`ahead` 也是 0 时返回 `committed: false`，不报错也不空提交；
    /// 只有 `ahead > 0` 时照常推送（`committed: false` + `existing_commits > 0`）。
    /// 没有上游时第一次推送顺手 `-u origin <branch>` 建立跟踪。
    /// 提交成功而 push 失败是可接受的终态：错误里如实说明「已提交」并带上 git 原始 stderr。
    pub fn commit_and_push(&self) -> Result<PushOutcome, CoreError> {
        // 一份 GitFacts 同时喂给硬拦与「有没有改动」，不重复跑 `git status`。
        let facts = state::GitFacts::read(&self.source_dir)?;
        let target = PushTarget::read(&self.source_dir)?;
        let hits = scan(&self.source_dir, &facts, target.scan_base.as_deref())?;
        if !hits.is_empty() {
            return Err(blocked_error(&hits));
        }

        let dirty = facts.dirty_paths().next().is_some();
        // 工作区干净、也没有待推提交：真的没事可做（空仓库同样落在这里）。
        if !dirty && target.ahead == 0 {
            return Ok(self.outcome(false, 0, String::new(), 0));
        }

        let mut committed = false;
        let mut files = 0;
        let mut message = String::new();
        if dirty {
            git::run(&self.source_dir, &["add", "-A"])?;
            files = staged_file_count(&self.source_dir)?;
            message = format!(
                "config-sync: {files} files ({})",
                chrono::Local::now().format("%Y-%m-%d %H:%M")
            );
            git::run(&self.source_dir, &["commit", "-m", &message])
                .map_err(|e| CoreError { message: format!("自动提交「{message}」失败"), ..e })?;
            committed = true;
        }

        // 第一次推送没有上游，顺手建立跟踪（拉取那边靠它判断能否 fetch）。
        let push: Vec<&str> = match target.upstream {
            Some(_) => vec!["push"],
            None => vec!["push", "-u", "origin", &target.branch],
        };
        git::run(&self.source_dir, &push).map_err(|e| CoreError {
            message: if committed {
                format!("已自动提交「{message}」，但推送失败：远端没有接受这个提交")
            } else {
                format!("推送失败：远端没有接受本机已有的 {} 个提交", target.ahead)
            },
            ..e
        })?;

        Ok(self.outcome(committed, files, message, target.ahead))
    }

    fn outcome(
        &self,
        committed: bool,
        files: u32,
        message: String,
        existing_commits: u32,
    ) -> PushOutcome {
        PushOutcome {
            committed,
            files,
            message,
            remote_url: self.remote_url().unwrap_or_default(),
            existing_commits,
        }
    }
}

/// 推送目标的现状：当前分支、上游、待推提交数，以及硬拦扫待推提交时的比较基准。
struct PushTarget {
    branch: String,
    /// 上游跟踪分支（如 `origin/main`）；`None` = 还没建立跟踪。
    upstream: Option<String>,
    /// 推送前已有、尚未推到远端的提交数。
    ahead: u32,
    /// `ahead` 那些提交从哪算起；`None` = 没有可比基准，整个 HEAD 树都要扫。
    scan_base: Option<String>,
}

impl PushTarget {
    fn read(source_dir: &Path) -> Result<Self, CoreError> {
        // 一次 `for-each-ref` 同时拿到当前分支与它的上游（`*` 标出当前分支）。
        let refs = git::run(
            source_dir,
            &[
                "for-each-ref",
                "--format=%(HEAD)%09%(refname:short)%09%(upstream:short)",
                "refs/heads/",
            ],
        )?;
        let found = refs.lines().find_map(|line| {
            let mut fields = line.split('\t');
            if fields.next() != Some("*") {
                return None;
            }
            let branch = fields.next()?.to_string();
            let upstream = fields.next().filter(|up| !up.is_empty()).map(str::to_string);
            Some((branch, upstream))
        });
        let (branch, upstream) = match found {
            Some(found) => found,
            // 空仓库（还没有第一个提交）时 `refs/heads` 是空的，只能单独问一次分支名。
            None => {
                let branch = git::run(source_dir, &["branch", "--show-current"])?;
                if branch.is_empty() {
                    return Err(CoreError {
                        kind: ErrorKind::Git,
                        message: "源目录当前不在任何分支上（detached HEAD）".to_string(),
                        stderr: None,
                    });
                }
                (branch, None)
            }
        };

        // 比较基准：上游优先；没有上游（或上游的 tracking ref 被删）时用远端同名分支
        // （手动推过但没建跟踪），两者都不存在才整树。
        let origin_branch = format!("origin/{branch}");
        let scan_base = [upstream.as_deref(), Some(origin_branch.as_str())]
            .into_iter()
            .flatten()
            .find(|rev| rev_exists(source_dir, rev))
            .map(str::to_string);

        // `ahead` 与 `scan_base` 必须同一个基准：远端已有同名分支且已同步时，ahead 得是 0
        // （否则会谎报「推了 N 个已有提交」，还永远走网络）。
        // 基准算不出来（如上游配置在、tracking ref 被删）就退回整树计数，宁可不漏推。
        let ahead = scan_base
            .as_deref()
            .and_then(|base| count_commits(source_dir, &format!("{base}..HEAD")))
            .or_else(|| count_commits(source_dir, "HEAD"))
            .unwrap_or(0);

        Ok(PushTarget { branch, upstream, ahead, scan_base })
    }
}

fn rev_exists(source_dir: &Path, rev: &str) -> bool {
    git::run(source_dir, &["rev-parse", "--verify", "--quiet", rev]).is_ok()
}

fn count_commits(source_dir: &Path, rev: &str) -> Option<u32> {
    git::run(source_dir, &["rev-list", "--count", rev]).ok()?.trim().parse().ok()
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

/// 在「工作区 ∪ 暂存区 ∪ 待推提交」上跑 9 条路径规则与 6 条内容规则。
fn scan(
    source_dir: &Path,
    facts: &state::GitFacts,
    base: Option<&str>,
) -> Result<Vec<Hit>, CoreError> {
    let mut paths: Vec<_> = facts.dirty_paths().cloned().collect();
    paths.sort();

    let mut hits: Vec<Hit> = Vec::new();
    let mut seen: HashSet<(&'static str, String)> = HashSet::new();

    // 暂存区版本：先把路径换成 blob sha，再一次 batch 取完。查询串里只出现 sha——
    // 路径带换行/引号都不可能把一次查询切成两条，响应与查询也就永远一一对应。
    let dirty_names: HashSet<String> = paths.iter().map(|path| repo_str(path)).collect();
    let staged_blobs = index_blobs(source_dir, &dirty_names)?;
    let index_specs: Vec<String> = staged_blobs.iter().map(|(_, sha)| sha.clone()).collect();
    let index = cat_file_batch(source_dir, &index_specs)?;
    for (path, sha) in &staged_blobs {
        if let Some(text) = index.get(sha) {
            check_content_rules(&mut hits, &mut seen, path, text);
        }
    }

    for path in &paths {
        let repo_path = repo_str(path);
        check_path_rules(&mut hits, &mut seen, &repo_path);
        // 工作区版本读磁盘；二进制按有损 UTF-8 读，不影响正则。
        if let Ok(bytes) = std::fs::read(source_dir.join(path)) {
            check_content_rules(&mut hits, &mut seen, &repo_path, &String::from_utf8_lossy(&bytes));
        }
    }

    // 已提交未推送的提交：这次 push 会把它们送出去，内容按 HEAD 里的版本扫。
    let ahead = ahead_paths(source_dir, base)?;
    for path in &ahead {
        check_path_rules(&mut hits, &mut seen, path);
    }
    let head_blobs = tree_blobs(source_dir, &ahead)?;
    let head_specs: Vec<String> = head_blobs.iter().map(|(_, sha)| sha.clone()).collect();
    let head = cat_file_batch(source_dir, &head_specs)?;
    for (path, sha) in &head_blobs {
        if let Some(text) = head.get(sha) {
            check_content_rules(&mut hits, &mut seen, path, text);
        }
    }

    Ok(hits)
}

fn repo_str(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// 索引里这些路径的 blob sha（`wanted` 为空则不查）。冲突多阶段时同一路径会有多条，逐个扫。
fn index_blobs(
    source_dir: &Path,
    wanted: &HashSet<String>,
) -> Result<Vec<(String, String)>, CoreError> {
    if wanted.is_empty() {
        return Ok(Vec::new());
    }
    let raw = git::run(source_dir, &["ls-files", "-s", "-z"])?;
    Ok(parse_meta_lines(&raw, 1)
        .into_iter()
        .filter(|(path, _)| wanted.contains(path))
        .collect())
}

/// `HEAD` 树里这些路径的 blob sha。
fn tree_blobs(source_dir: &Path, paths: &[String]) -> Result<Vec<(String, String)>, CoreError> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let mut args = vec!["ls-tree", "-r", "-z", "HEAD", "--"];
    args.extend(paths.iter().map(String::as_str));
    let raw = git::run(source_dir, &args)?;
    Ok(parse_meta_lines(&raw, 2))
}

/// `ls-files -s` / `ls-tree` 的 `-z` 输出：每条是 `<元信息>\t<路径>`，元信息里第 `sha_field`
/// 个空格分隔字段是 blob sha。路径里有换行也无所谓，两者的分隔是 NUL。
fn parse_meta_lines(raw: &str, sha_field: usize) -> Vec<(String, String)> {
    raw.split('\0')
        .filter_map(|line| {
            let (meta, path) = line.split_once('\t')?;
            let sha = meta.split(' ').nth(sha_field)?.to_string();
            Some((path.to_string(), sha))
        })
        .collect()
}

fn check_path_rules(hits: &mut Vec<Hit>, seen: &mut HashSet<(&'static str, String)>, path: &str) {
    for rule in PATH_RULES {
        if path_rule_matches(rule, path) {
            record(hits, seen, "路径", rule, path);
        }
    }
}

fn check_content_rules(
    hits: &mut Vec<Hit>,
    seen: &mut HashSet<(&'static str, String)>,
    path: &str,
    text: &str,
) {
    for (rule, pattern) in CONTENT_RULES.iter().zip(CONTENT_PATTERNS.iter()) {
        if pattern.is_match(text) {
            record(hits, seen, "内容", rule, path);
        }
    }
}

/// 同一规则在同一文件上只报一次（暂存区、工作区、待推提交可能各扫一遍）。
fn record(
    hits: &mut Vec<Hit>,
    seen: &mut HashSet<(&'static str, String)>,
    kind: &'static str,
    rule: &'static str,
    path: &str,
) {
    if seen.insert((rule, path.to_string())) {
        hits.push(Hit { kind, rule, path: path.to_string() });
    }
}

/// `base..HEAD` 里被改过的路径（删掉的不算，没有内容可扫）；
/// 没有基准时退回整个 HEAD 树——首次推送会把树里的一切都送出去。
fn ahead_paths(source_dir: &Path, base: Option<&str>) -> Result<Vec<String>, CoreError> {
    let raw = match base {
        Some(base) => git::run(
            source_dir,
            &["diff", "--name-only", "-z", "--diff-filter=d", &format!("{base}..HEAD")],
        )?,
        None => {
            // 还没有第一个提交的空仓库：没有历史可扫。
            if git::run(source_dir, &["rev-parse", "--verify", "--quiet", "HEAD"]).is_err() {
                return Ok(Vec::new());
            }
            git::run(source_dir, &["ls-tree", "-r", "--name-only", "-z", "HEAD"])?
        }
    };
    Ok(raw.split('\0').filter(|path| !path.is_empty()).map(str::to_string).collect())
}

/// 一次 `git cat-file --batch` 取一批对象的内容，键是查询串（这里是 blob sha）。
///
/// 一个进程覆盖任意多个文件：推送耗时不再随 dirty 文件数线性增长（#16）。
fn cat_file_batch(
    source_dir: &Path,
    specs: &[String],
) -> Result<HashMap<String, String>, CoreError> {
    if specs.is_empty() {
        return Ok(HashMap::new());
    }
    let mut child = Command::new("git")
        .arg("-C")
        .arg(source_dir)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| CoreError::io(format!("无法启动系统 git：{e}")))?;

    // 查询串是 blob sha（`[0-9a-f]{40,64}`），不含空白与换行，`\n` 分隔不会有歧义。
    let input = format!("{}\n", specs.join("\n"));
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| CoreError::io("git cat-file 的 stdin 没有打开"))?;
    // 写端放后台：大仓库里输出可能超过管道缓冲，边写边等读端会互锁。
    let writer = std::thread::spawn(move || -> std::io::Result<()> {
        stdin.write_all(input.as_bytes())?;
        stdin.flush()
    });

    let out = child
        .wait_with_output()
        .map_err(|e| CoreError::io(format!("读取 git cat-file 的输出失败：{e}")))?;
    // 查询只写了一半 = git 那边少扫了文件，不能当「没命中」：写入与线程错误都整次失败。
    writer
        .join()
        .map_err(|_| CoreError::io("写入 git cat-file 的查询时线程崩溃"))?
        .map_err(|e| CoreError::io(format!("写入 git cat-file 的查询失败：{e}")))?;
    if !out.status.success() {
        return Err(CoreError::git(
            "git cat-file --batch 执行失败",
            String::from_utf8_lossy(&out.stderr).trim_end().to_string(),
        ));
    }
    parse_batch(&out.stdout, specs)
}

/// `--batch` 的输出按查询顺序一组：`<sha> <type> <size>\n<内容>\n`。
///
/// 查询串只包含索引 / HEAD 里确实存在的 blob sha，所以每一组都必须解析成功；
/// 解析不了就整次报错，不猜偏移、也不把剩余项当「没内容」（那是静默漏扫）。
fn parse_batch(bytes: &[u8], specs: &[String]) -> Result<HashMap<String, String>, CoreError> {
    let mut contents = HashMap::with_capacity(specs.len());
    let mut pos = 0;
    for spec in specs {
        let header_end = bytes[pos..]
            .iter()
            .position(|byte| *byte == b'\n')
            .ok_or_else(|| batch_parse_error(spec))?;
        let header = String::from_utf8_lossy(&bytes[pos..pos + header_end]);
        pos += header_end + 1;
        let size = header
            .rsplit_once(' ')
            .and_then(|(_, size)| size.parse::<usize>().ok())
            .ok_or_else(|| batch_parse_error(spec))?;
        let end = pos.checked_add(size).filter(|end| *end <= bytes.len());
        let end = end.ok_or_else(|| batch_parse_error(spec))?;
        contents.insert(spec.clone(), String::from_utf8_lossy(&bytes[pos..end]).into_owned());
        pos = end + 1; // 内容后面跟一字节分隔符。
    }
    Ok(contents)
}

fn batch_parse_error(spec: &str) -> CoreError {
    CoreError::io(format!(
        "无法解析 git cat-file 的输出（查询「{spec}」），为免漏扫这次推送中止"
    ))
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
