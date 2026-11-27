//! 推送：硬拦 → `git add -A` → 自动提交 → `git push`。
//!
//! 硬拦看的是「工作区 ∪ 暂存区 ∪ 待推提交（`@{u}..HEAD`）」：命中任一条规则就整次拒绝，
//! 一个文件都不进暂存区，也没有 override 通道（见 DESIGN.md）。已提交但没推送的提交同样
//! 要扫——这次 `git push` 会把它们一起送到远端，不扫就等于没有硬拦。
//!
//! 待推提交按「区间里出现过的每一个 blob 版本」扫（`rev-list --objects`），而不是按
//! `base..HEAD` 的最终 diff：提交 A 加进密钥、提交 B 把文件删掉时，最终 diff 与 HEAD 树里
//! 都没有它，可 A 的 blob 照样会被 push 送出去。为此区间有上限（见 `MAX_*` 常量），超限
//! 整次中止并说明是哪一项——扫不完就推等于把硬拦关掉，不静默放行。
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

// 待推提交的扫描上限。这几个数远超「本机配置仓库」的日常量级，顶到上限说明该先手动推一次；
// 超限一律 fail-closed（整次中止），绝不「太大的跳过」——跳过就等于大文件里藏密钥照推。
/// 未推送提交数上限。
const MAX_UNPUSHED_COMMITS: u32 = 1000;
/// 待扫 blob 版本数上限（按 oid 去重后）。
const MAX_UNPUSHED_BLOBS: usize = 10_000;
/// 待扫内容累计上限。
const MAX_SCAN_BYTES: u64 = 256 * 1024 * 1024;
/// 单个 blob 版本上限（在 `cat-file --batch-check` 拿到 size 时就判，不必读内容）。
const MAX_BLOB_BYTES: u64 = 16 * 1024 * 1024;

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
        let hits = scan(&self.source_dir, &facts, target.scan_base.as_deref(), target.ahead)?;
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
        // （手动推过但没建跟踪），两者都不存在才整树。`ahead`、历史扫描区间、路径规则都
        // 从这一个函数取基准——三处各算各的就会在历史扫描里把 B2 原样复刻一遍。
        let scan_base = push_base(source_dir, &branch, upstream.as_deref());

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

/// 待推区间与硬拦扫描的唯一基准：上游优先，其次远端同名分支，都没有才是 `None`（整棵 HEAD 树）。
fn push_base(source_dir: &Path, branch: &str, upstream: Option<&str>) -> Option<String> {
    let origin_branch = format!("origin/{branch}");
    [upstream, Some(origin_branch.as_str())]
        .into_iter()
        .flatten()
        .find(|rev| rev_exists(source_dir, rev))
        .map(str::to_string)
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
    ahead: u32,
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

    // 已提交未推送的提交：这次 push 会把它们送出去。扫的是区间里出现过的每一个 blob 版本
    // （含之后被删掉的路径）——按最终 diff 扫会漏掉「加了又删」的那个密钥 blob。
    let pending = pending_blobs(source_dir, base, ahead)?;
    for blob in &pending {
        check_path_rules(&mut hits, &mut seen, &blob.path);
    }
    let pending_specs: Vec<String> = pending.iter().map(|blob| blob.oid.clone()).collect();
    let pending_contents = cat_file_batch(source_dir, &pending_specs)?;
    for blob in &pending {
        if let Some(text) = pending_contents.get(&blob.oid) {
            check_content_rules(&mut hits, &mut seen, &blob.path, text);
        }
    }

    Ok(hits)
}

/// 待推区间里的一个 blob 版本：仓库内路径 + blob sha（同一份内容只算一次）。
struct PendingBlob {
    path: String,
    oid: String,
}

/// `base..HEAD` 里出现过的每个 blob 版本，按 oid 去重。
///
/// `rev-list --objects` 列的是「有了 base 还缺的对象」，所以区间里每个提交的树都会被展开：
/// 提交 A 加进的 blob 即使被提交 B 删掉也在这份名单里。不扫 `--all` 也不扫整仓历史：
/// 只扫这次真要推出去的部分，成本才有界。
fn pending_blobs(
    source_dir: &Path,
    base: Option<&str>,
    ahead: u32,
) -> Result<Vec<PendingBlob>, CoreError> {
    if ahead > MAX_UNPUSHED_COMMITS {
        return Err(over_limit(
            "未推送提交数".to_string(),
            format!("{ahead} 个"),
            format!("{MAX_UNPUSHED_COMMITS} 个"),
            "请先手动 `git push` 一次，把已有提交送上远端再试",
        ));
    }
    if ahead == 0 {
        return Ok(Vec::new());
    }

    let range = match base {
        Some(base) => format!("{base}..HEAD"),
        None => "HEAD".to_string(),
    };
    let listed = git::run(source_dir, &["rev-list", "--objects", &range])?;
    let objects = parse_objects(&listed);
    // 路径要从 `log --raw -z` 拿：`rev-list --objects` 遇到带换行的路径会把后半截截掉，
    // `-z` 的 raw 输出才是 NUL 结尾的完整路径。
    let raw = git::run(
        source_dir,
        &["log", "--raw", "-z", "--no-abbrev", "-r", "--pretty=format:%H", &range],
    )?;
    let paths = parse_raw_paths(&raw);

    let mut oids: Vec<String> = Vec::new();
    let mut seen_oid: HashSet<&str> = HashSet::new();
    for (oid, _) in &objects {
        if seen_oid.insert(oid) {
            oids.push(oid.clone());
        }
    }
    // 类型与大小一次问完：非 blob（commit/tree、子模块）没内容可扫，大 blob 在拿内容之前就拦下。
    let metas = cat_file_meta(source_dir, &oids)?;

    let mut blobs: Vec<PendingBlob> = Vec::new();
    let mut seen_blob: HashSet<String> = HashSet::new();
    let mut total: u64 = 0;
    for (oid, listed_path) in objects {
        let Some((kind, size)) = metas.get(&oid) else { continue };
        if kind != "blob" || !seen_blob.insert(oid.clone()) {
            continue;
        }
        // 优先用 raw diff 给的完整路径；没有 diff 记录（如 merge 带进来的内容）才退回 rev-list 的。
        let path = paths.get(&oid).cloned().unwrap_or(listed_path);
        let path = if path.is_empty() { "（路径未知）".to_string() } else { path };
        if *size > MAX_BLOB_BYTES {
            return Err(over_limit(
                format!("单个文件版本「{path}」占"),
                human_bytes(*size),
                human_bytes(MAX_BLOB_BYTES),
                "把这个提交里的它移出仓库，或先手动 `git push` 推掉这个提交再试",
            ));
        }
        total += size;
        blobs.push(PendingBlob { path, oid });
    }

    if blobs.len() > MAX_UNPUSHED_BLOBS {
        return Err(over_limit(
            "待扫描的文件版本数".to_string(),
            format!("{} 个", blobs.len()),
            format!("{MAX_UNPUSHED_BLOBS} 个"),
            "请先手动 `git push` 一次，把已有提交送上远端再试",
        ));
    }
    if total > MAX_SCAN_BYTES {
        return Err(over_limit(
            "待扫描内容总大小".to_string(),
            human_bytes(total),
            human_bytes(MAX_SCAN_BYTES),
            "请先手动 `git push` 一次，把已有提交送上远端再试",
        ));
    }
    Ok(blobs)
}

/// `git rev-list --objects` 的输出：每行 `<oid>[ <path>]`。
///
/// oid 永远在行首，所以 blob 名单完整；但路径只作兜底（带换行的路径会被截断，
/// 完整路径由 `parse_raw_paths` 提供）。
fn parse_objects(raw: &str) -> Vec<(String, String)> {
    let mut objects: Vec<(String, String)> = Vec::new();
    for line in raw.lines() {
        match line.split_once(' ') {
            Some((oid, path)) if is_oid(oid) => objects.push((oid.to_string(), path.to_string())),
            None if is_oid(line) => objects.push((line.to_string(), String::new())),
            _ => {
                if let Some((_, path)) = objects.last_mut() {
                    path.push('\n');
                    path.push_str(line);
                }
            }
        }
    }
    objects
}

fn is_oid(text: &str) -> bool {
    (text.len() == 40 || text.len() == 64) && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// `git log --raw -z` 的输出：每条 diff 是 `:旧模式 新模式 旧sha 新sha 状态\0路径\0`，
/// 重命名/复制后面还跟一条旧路径。只收「这条提交新引入的」blob（新 sha 非全 0）：
/// 被删掉的那个 blob 由当初引入它的提交负责，同一次 push 里会被扫到。
fn parse_raw_paths(raw: &str) -> HashMap<String, String> {
    let mut path_of: HashMap<String, String> = HashMap::new();
    // (新 sha, 还要跳过的路径数)：重命名/复制先跟一条旧路径。
    let mut pending: Option<(String, usize)> = None;
    for token in raw.split('\0') {
        // 提交 sha 头部与它的第一条 diff 记录在同一个 NUL 段里，拆掉头部。
        let token = match token.split_once('\n') {
            Some((head, rest)) if is_oid(head) && (rest.is_empty() || rest.starts_with(':')) => rest,
            _ => token,
        };
        if let Some((sha, skip)) = pending.as_mut() {
            if *skip > 0 {
                *skip -= 1;
                continue;
            }
            path_of.entry(sha.clone()).or_insert_with(|| token.to_string());
            pending = None;
            continue;
        }
        let Some(fields) = token.strip_prefix(':') else { continue };
        let fields: Vec<&str> = fields.split(' ').collect();
        let (Some(new_sha), Some(status)) = (fields.get(3), fields.get(4)) else { continue };
        if !is_oid(new_sha) || new_sha.bytes().all(|byte| byte == b'0') {
            continue;
        }
        let skip = usize::from(status.starts_with('R') || status.starts_with('C'));
        pending = Some((new_sha.to_string(), skip));
    }
    path_of
}

/// `cat-file --batch-check` 问一批对象：oid → (类型, 字节数)。
///
/// 每行都解析不了就整次报错；`missing` 是 `parse_objects` 的解析残留（假 oid），
/// 它对应的对象不存在、没有内容可扫，不算漏扫。
fn cat_file_meta(
    source_dir: &Path,
    specs: &[String],
) -> Result<HashMap<String, (String, u64)>, CoreError> {
    if specs.is_empty() {
        return Ok(HashMap::new());
    }
    let out = cat_file(source_dir, "--batch-check", specs)?;
    let text = String::from_utf8_lossy(&out);
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() != specs.len() {
        return Err(batch_parse_error(&format!(
            "{} 条查询只回了 {} 行",
            specs.len(),
            lines.len()
        )));
    }
    let mut metas = HashMap::with_capacity(specs.len());
    for (spec, line) in specs.iter().zip(lines) {
        let mut fields = line.split(' ');
        let oid = fields.next().unwrap_or_default();
        let kind = fields.next().unwrap_or_default();
        if oid != spec {
            return Err(batch_parse_error(&format!("查询「{spec}」回了「{oid}」")));
        }
        if kind == "missing" {
            continue;
        }
        let size = fields.next().and_then(|size| size.parse::<u64>().ok());
        let size = size.ok_or_else(|| batch_parse_error(spec))?;
        metas.insert(oid.to_string(), (kind.to_string(), size));
    }
    Ok(metas)
}

/// 超限文案固定含四项：超了哪一项、当前值、上限、下一步该干什么。
fn over_limit(what: String, current: String, cap: String, advice: &str) -> CoreError {
    CoreError::io(format!(
        "推送被拒绝：待推送内容超过安全扫描上限，为免漏扫这次推送已中止。{what} {current}，上限 {cap}：{advice}。"
    ))
}

fn human_bytes(bytes: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    if bytes as f64 >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB)
    } else {
        format!("{bytes} 字节")
    }
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

/// `ls-files -s` 的 `-z` 输出：每条是 `<元信息>\t<路径>`，元信息里第 `sha_field`
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
    let out = cat_file(source_dir, "--batch", specs)?;
    parse_batch(&out, specs)
}

/// 一次 `git cat-file <flag>` 把查询串喂完，返回原始 stdout。
fn cat_file(source_dir: &Path, flag: &str, specs: &[String]) -> Result<Vec<u8>, CoreError> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(source_dir)
        .args(["cat-file", flag])
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
            format!("git cat-file {flag} 执行失败"),
            String::from_utf8_lossy(&out.stderr).trim_end().to_string(),
        ));
    }
    Ok(out.stdout)
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
