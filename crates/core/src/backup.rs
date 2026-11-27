//! 应用前的快照，以及「撤销这次应用」。
//!
//! 每次 `execute_apply` 真要写盘时，先把目标文件的原文复制到
//! `~/.config/config-sync/backups/<id>/files/<相对 $HOME 的路径>`（保留目录结构），
//! 再在同一目录写 `manifest.json`：这次动了哪些路径、原来存不存在、原文件权限、
//! 写下去的内容指纹、以及每一项的结果（写成 / 没写成和原因）。
//!
//! 撤销同样分两阶段：`plan_restore_backup` 只出计划与 diff 预览（走 `#27` 的同一套闸门），
//! `restore_backup` 执行**同一份**计划。目标文件在应用之后又被改过（用户手改、删掉）时
//! 计划里就是 `Skip` + `conflict`，绝不覆盖这些新改动。

use std::path::{Component, Path, PathBuf};

use chrono::{Local, NaiveDateTime};
use serde::{Deserialize, Serialize};

use crate::apply::{digest, line_count, relative_to_home, replace_file};
use crate::{Core, CoreError, DIFF_MAX_BYTES, diff_gate_reason, too_large_reason, unified_diff};

/// 备份根目录（app 配置目录下）：不在源仓库里，因此不会被 git 跟踪、不会被推送。
pub(crate) const BACKUP_ROOT: &str = ".config/config-sync/backups";
/// 保留策略：无论如何留最近这么多次应用。
const KEEP_LATEST: usize = 10;
/// 保留策略：超过这个天数的、又不在「最近 10 次」里的备份会被清掉。
const KEEP_DAYS: i64 = 30;

const MANIFEST: &str = "manifest.json";
const FILES: &str = "files";

/// 一次应用的清单：撤销只看它。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupManifest {
    /// 目录名，也是 `list_backups` 里的 id。
    pub id: String,
    pub created_at: String,
    pub entries: Vec<BackupEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupEntry {
    /// 这次应用写到的目标（`$HOME` 下的真实路径）。
    pub target: PathBuf,
    /// 原文在备份目录里的位置（`files/<rel>`）；原来不存在时是 `None`。
    pub file: Option<PathBuf>,
    /// 应用之前目标文件是否存在。
    pub existed: bool,
    /// 应用之前目标文件的权限位（unix mode）。
    pub mode: Option<u32>,
    /// 应用写下去的内容指纹；没写成时为 `None`。撤销靠它判断「目标还是那次写的样子吗」。
    pub written_digest: Option<String>,
    /// 这一项写成了没有。没写成的项目标文件原样不动，不用撤销。
    pub result: BackupResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BackupResult {
    Written,
    Failed { message: String },
}

/// 设置页 / 应用结果页用的备份摘要（不带清单正文）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BackupSummary {
    pub id: String,
    pub created_at: String,
    /// 写成几项（撤销时要恢复的）。
    pub written: usize,
    /// 没写成几项（撤销时不管）。
    pub failed: usize,
}

/// 撤销计划：预览展示它，执行也执行它。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestorePlan {
    pub backup: String,
    pub created_at: String,
    pub items: Vec<RestoreItem>,
}

impl RestorePlan {
    /// 真正会被写盘（恢复 / 删除）的项数。
    pub fn writes(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.action != RestoreAction::Skip)
            .count()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreItem {
    pub target: PathBuf,
    pub action: RestoreAction,
    /// 这个动作的原因，直接给界面显示。
    pub reason: String,
    /// 恢复会带来的改动（目标 → 备份原文）；跳过项为空串。
    pub diff: String,
    pub diff_omitted: bool,
    pub diff_reason: String,
    /// 目标在应用之后又被改过：跳过并提示，不覆盖用户的新改动。
    pub conflict: bool,
    /// 原文在备份目录里的位置（删除项为 `None`）。
    pub file: Option<PathBuf>,
    /// 原文的权限位，恢复时照它写回。
    pub mode: Option<u32>,
    /// 出计划时目标内容的指纹：执行前再比一次，变了就失败，不覆盖。
    #[serde(default)]
    pub expected_digest: Option<String>,
    /// 备份原文的指纹：执行时再读一次也比一次。
    #[serde(default)]
    pub backup_digest: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RestoreAction {
    /// 用备份原文覆盖现在的目标。
    Restore,
    /// 那次应用新建的文件：删掉就等于回到从前。
    Delete,
    /// 不动（应用没写成 / 目标被改过 / 备份缺文件）。
    Skip,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RestoreOutcome {
    pub target: PathBuf,
    pub result: RestoreResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum RestoreResult {
    Restored,
    Deleted,
    Failed { message: String },
}

/// 备份里的一份原文：复制之前目标的样子。
pub(crate) struct Origin {
    file: Option<PathBuf>,
    existed: bool,
    mode: Option<u32>,
}

/// 一次应用写盘前的快照目录。`execute_apply` 每写一项前 `save`，最后 `commit` 落清单。
pub(crate) struct Snapshot {
    dir: PathBuf,
    manifest: BackupManifest,
}

impl Snapshot {
    /// 建 `backups/<id>/`（0700）。备份目录落在源仓库里时直接拒绝：备份可能含密钥。
    pub(crate) fn create(home: &Path, source_dir: &Path) -> Result<Snapshot, CoreError> {
        let root = home.join(BACKUP_ROOT);
        if root.starts_with(source_dir) {
            return Err(CoreError::io(format!(
                "备份目录 {} 落在源目录 {} 里，备份会被 git 跟踪并推送（里面可能有密钥）；\
                 先换个源目录再应用，一个文件都没有写",
                root.display(),
                source_dir.display()
            )));
        }
        create_private_dir(&root).map_err(|e| {
            CoreError::io(format!(
                "创建备份目录 {} 失败：{e}；一个文件都没有写",
                root.display()
            ))
        })?;

        let now = Local::now();
        let id = unique_id(&root, &now.format("%Y%m%d-%H%M%S").to_string());
        let dir = root.join(&id);
        create_private_dir(&dir).map_err(|e| {
            CoreError::io(format!(
                "创建备份目录 {} 失败：{e}；一个文件都没有写",
                dir.display()
            ))
        })?;

        Ok(Snapshot {
            dir,
            manifest: BackupManifest {
                id,
                created_at: now.to_rfc3339(),
                entries: Vec::new(),
            },
        })
    }

    /// 写盘之前：把目标文件的原文复制进快照目录。失败时这一项就不该写。
    pub(crate) fn save(&self, home: &Path, target: &Path) -> Result<Origin, String> {
        let rel = relative_to_home(target, home).map_err(|error| error.message)?;
        let meta = match std::fs::metadata(target) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Origin {
                    file: None,
                    existed: false,
                    mode: None,
                });
            }
            Err(e) => return Err(format!("读取 {} 失败：{e}；没有写它", target.display())),
        };
        if !meta.is_file() {
            return Err(format!("{} 不是普通文件，没有写它", target.display()));
        }

        let file = Path::new(FILES).join(&rel);
        let dest = self.dir.join(&file);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                format!(
                    "创建备份目录 {} 失败：{e}；没有写 {}",
                    parent.display(),
                    target.display()
                )
            })?;
        }
        // `fs::copy` 连权限一起复制，撤销时原文的权限才恢复得回来。
        std::fs::copy(target, &dest).map_err(|e| {
            format!(
                "把 {} 备份到 {} 失败：{e}；没有写它",
                target.display(),
                dest.display()
            )
        })?;
        Ok(Origin {
            file: Some(file),
            existed: true,
            mode: mode_of(&meta),
        })
    }

    fn push(
        &mut self,
        target: &Path,
        origin: Option<Origin>,
        written_digest: Option<String>,
        result: BackupResult,
    ) {
        self.manifest.entries.push(BackupEntry {
            target: target.to_path_buf(),
            file: origin.as_ref().and_then(|origin| origin.file.clone()),
            existed: origin.as_ref().is_some_and(|origin| origin.existed),
            mode: origin.and_then(|origin| origin.mode),
            written_digest,
            result,
        });
    }

    pub(crate) fn record_written(&mut self, target: &Path, origin: Origin, written_digest: String) {
        self.push(
            target,
            Some(origin),
            Some(written_digest),
            BackupResult::Written,
        );
    }

    /// 没写成的一项：`origin` 为 `None` 表示连备份都没拿到（目标文件没被碰过）。
    pub(crate) fn record_failed(&mut self, target: &Path, origin: Option<Origin>, message: String) {
        self.push(target, origin, None, BackupResult::Failed { message });
    }

    /// 落清单并按保留策略清旧备份；清单写失败只算这次的备份不可撤销（返回错误给调用方）。
    pub(crate) fn commit(self) -> Result<BackupSummary, CoreError> {
        let path = self.dir.join(MANIFEST);
        let json = serde_json::to_string_pretty(&self.manifest)
            .map_err(|e| CoreError::io(format!("生成备份清单失败：{e}")))?;
        std::fs::write(&path, json)
            .map_err(|e| CoreError::io(format!("写备份清单 {} 失败：{e}", path.display())))?;

        // 保留策略是「还好」，清不掉不影响这次应用的结果。
        let _ = prune(
            self.dir.parent().unwrap_or(&self.dir),
            KEEP_LATEST,
            KEEP_DAYS,
        );

        Ok(BackupSummary {
            written: self
                .manifest
                .entries
                .iter()
                .filter(|entry| matches!(entry.result, BackupResult::Written))
                .count(),
            failed: self
                .manifest
                .entries
                .iter()
                .filter(|entry| matches!(entry.result, BackupResult::Failed { .. }))
                .count(),
            id: self.manifest.id,
            created_at: self.manifest.created_at,
        })
    }
}

impl Core {
    /// 备份根目录。
    fn backup_root(&self) -> PathBuf {
        self.home.join(BACKUP_ROOT)
    }

    /// 现有备份，新的在前。清单读不出来的目录（不是我们写的 / 写坏的）不列。
    pub fn list_backups(&self) -> Result<Vec<BackupSummary>, CoreError> {
        let root = self.backup_root();
        let entries = match std::fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(CoreError::io(format!(
                    "读取备份目录 {} 失败：{e}",
                    root.display()
                )));
            }
        };
        let mut backups: Vec<BackupSummary> = entries
            .flatten()
            .filter(|entry| is_backup_dir(&entry.path()))
            .filter_map(|entry| {
                self.read_manifest(&entry.file_name().to_string_lossy())
                    .ok()
            })
            .map(|manifest| BackupSummary {
                written: manifest
                    .entries
                    .iter()
                    .filter(|entry| matches!(entry.result, BackupResult::Written))
                    .count(),
                failed: manifest
                    .entries
                    .iter()
                    .filter(|entry| matches!(entry.result, BackupResult::Failed { .. }))
                    .count(),
                id: manifest.id,
                created_at: manifest.created_at,
            })
            .collect();
        backups.sort_by(|a, b| b.id.cmp(&a.id));
        Ok(backups)
    }

    /// 出一份撤销计划：只读磁盘与备份，一个字节都不写。
    ///
    /// 应用之后目标又被改过 / 被删掉的项是 `Skip` + `conflict`，界面上要提示而不是覆盖。
    pub fn plan_restore_backup(&self, id: &str) -> Result<RestorePlan, CoreError> {
        let manifest = self.read_manifest(id)?;
        let dir = self.backup_root().join(&manifest.id);
        let mut items = Vec::new();

        for entry in &manifest.entries {
            let target = &entry.target;
            let current = match std::fs::read(target) {
                Ok(bytes) => Some(bytes),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => {
                    return Err(CoreError::io(format!(
                        "读取 {} 失败：{e}",
                        target.display()
                    )));
                }
            };
            let current_digest = current.as_deref().map(digest);
            let written_digest = entry.written_digest.as_deref();
            // 「目标还是那次应用写下的样子」：撤销的安全前提。
            let untouched =
                current_digest.as_deref().is_some() && current_digest.as_deref() == written_digest;

            let mut item = RestoreItem {
                target: target.clone(),
                action: RestoreAction::Skip,
                reason: String::new(),
                diff: String::new(),
                diff_omitted: false,
                diff_reason: String::new(),
                conflict: false,
                file: entry.file.clone(),
                mode: entry.mode,
                expected_digest: current_digest,
                backup_digest: None,
            };

            if !matches!(entry.result, BackupResult::Written) {
                item.reason = "这次应用没有写成这一项，目标文件原样未动，不需要恢复".to_string();
                items.push(item);
                continue;
            }

            if !entry.existed {
                // 那次是新建：目标还是写下的样子 → 删掉它；否则是用户自己又改过。
                match (&current, untouched) {
                    (None, _) => item.reason = "应用新建的文件已经不在了，不用恢复".to_string(),
                    (Some(_), true) => {
                        item.action = RestoreAction::Delete;
                        item.reason = "这个文件是那次应用新建的，撤销会把它删掉".to_string();
                        item.file = None;
                        let old = String::from_utf8_lossy(current.as_deref().unwrap_or_default());
                        item.diff = unified_diff(
                            &target.display().to_string(),
                            &format!("{}（删除）", target.display()),
                            &old,
                            "",
                        );
                    }
                    (Some(_), false) => {
                        item.conflict = true;
                        item.reason = "这个文件在应用之后又被改过，撤销会丢掉这些改动；请手动处理"
                            .to_string();
                    }
                }
                items.push(item);
                continue;
            }

            // 那次是覆盖：备份里应该有原文。
            let backup_bytes = match entry.file.as_ref() {
                Some(file) => match std::fs::read(dir.join(file)) {
                    Ok(bytes) => Some(bytes),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                    Err(e) => {
                        return Err(CoreError::io(format!(
                            "读取备份文件 {} 失败：{e}",
                            dir.join(file).display()
                        )));
                    }
                },
                None => None,
            };
            let Some(backup_bytes) = backup_bytes else {
                item.reason = "备份里缺这个文件的原文，没法恢复；只能手动处理".to_string();
                items.push(item);
                continue;
            };

            match (&current, untouched) {
                (None, _) => {
                    item.conflict = true;
                    item.reason =
                        "这个文件在应用之后被删掉了，恢复会把它重新建出来；请手动处理".to_string();
                }
                (Some(_), false) => {
                    item.conflict = true;
                    item.reason =
                        "这个文件在应用之后又被改过，恢复会覆盖这些改动；请手动处理".to_string();
                }
                (Some(_), true) => {
                    item.action = RestoreAction::Restore;
                    item.reason = "恢复到应用之前的内容与权限".to_string();
                    item.backup_digest = Some(digest(&backup_bytes));
                    let (diff, omitted, reason) = gated_diff(
                        current.as_deref(),
                        &backup_bytes,
                        &target.display().to_string(),
                        &dir.join(entry.file.as_ref().unwrap()).display().to_string(),
                    );
                    item.diff = diff;
                    item.diff_omitted = omitted;
                    item.diff_reason = reason;
                }
            }
            items.push(item);
        }

        Ok(RestorePlan {
            backup: manifest.id,
            created_at: manifest.created_at,
            items,
        })
    }

    /// 执行撤销计划。跳过项不写；每项写盘前再比一次指纹，目标在这之间又变过就失败而不是覆盖。
    pub fn restore_backup(&self, plan: &RestorePlan) -> Result<Vec<RestoreOutcome>, CoreError> {
        let dir = self.backup_root().join(self.checked_id(&plan.backup)?);
        let mut outcomes = Vec::new();
        for item in &plan.items {
            if item.action == RestoreAction::Skip {
                continue;
            }
            let result = match self.restore_item(&dir, item) {
                Ok(result) => result,
                Err(message) => RestoreResult::Failed { message },
            };
            outcomes.push(RestoreOutcome {
                target: item.target.clone(),
                result,
            });
        }
        Ok(outcomes)
    }

    fn restore_item(&self, dir: &Path, item: &RestoreItem) -> Result<RestoreResult, String> {
        let current = std::fs::read(&item.target).ok().as_deref().map(digest);
        if current != item.expected_digest {
            return Err(format!(
                "{} 在预览之后又变过，撤销会覆盖新内容；没有动它，请重新预览",
                item.target.display()
            ));
        }

        match item.action {
            RestoreAction::Delete => match std::fs::remove_file(&item.target) {
                Ok(()) => Ok(RestoreResult::Deleted),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(RestoreResult::Deleted),
                Err(e) => Err(format!("删除 {} 失败：{e}", item.target.display())),
            },
            RestoreAction::Restore => {
                let file = item.file.as_ref().ok_or_else(|| {
                    format!("备份里没有 {} 的原文位置，没法恢复", item.target.display())
                })?;
                let path = dir.join(file);
                let bytes = std::fs::read(&path)
                    .map_err(|e| format!("读取备份 {} 失败：{e}", path.display()))?;
                if Some(digest(&bytes)) != item.backup_digest {
                    return Err(format!(
                        "备份 {} 在预览之后被改过，恢复的就不是确认过的那份；没有写 {}",
                        path.display(),
                        item.target.display()
                    ));
                }
                let permissions = permissions_from_mode(item.mode)
                    .or_else(|| std::fs::metadata(&path).ok().map(|meta| meta.permissions()));
                replace_file(&item.target, &bytes, permissions)?;
                Ok(RestoreResult::Restored)
            }
            RestoreAction::Skip => Ok(RestoreResult::Restored),
        }
    }

    fn read_manifest(&self, id: &str) -> Result<BackupManifest, CoreError> {
        let dir = self.backup_root().join(self.checked_id(id)?);
        let path = dir.join(MANIFEST);
        let json = std::fs::read_to_string(&path).map_err(|e| {
            CoreError::io(format!(
                "读取备份清单 {} 失败：{e}；先看看 `config-sync` 的备份目录还在不在",
                path.display()
            ))
        })?;
        serde_json::from_str(&json)
            .map_err(|e| CoreError::io(format!("备份清单 {} 读不懂：{e}", path.display())))
    }

    /// 备份 id 只能是 `backups/` 下的一级目录名，不接受 `..` / 路径分隔符。
    fn checked_id<'a>(&self, id: &'a str) -> Result<&'a str, CoreError> {
        let mut components = Path::new(id).components();
        match (components.next(), components.next()) {
            (Some(Component::Normal(_)), None) if !id.trim().is_empty() => Ok(id),
            _ => Err(CoreError::io(format!(
                "备份 id「{id}」不是一个备份目录名；用 list_backups 里的 id"
            ))),
        }
    }
}

/// 与 `apply` 同一套 diff 闸门（`#27`）：超字节 / 超行数就不算 diff，只给原因。
fn gated_diff(
    old_bytes: Option<&[u8]>,
    new_bytes: &[u8],
    old_label: &str,
    new_label: &str,
) -> (String, bool, String) {
    let widest = old_bytes.map_or(new_bytes.len(), |old| old.len().max(new_bytes.len()));
    if widest > DIFF_MAX_BYTES {
        let lines = old_bytes.map(|old| line_count(old).max(line_count(new_bytes)));
        return (String::new(), true, too_large_reason(widest, lines));
    }
    let old = old_bytes
        .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        .unwrap_or_default();
    let new = String::from_utf8_lossy(new_bytes);
    match diff_gate_reason(&old, &new) {
        Some(reason) => (String::new(), true, reason),
        None => (
            unified_diff(old_label, new_label, &old, &new),
            false,
            String::new(),
        ),
    }
}

fn is_backup_dir(dir: &Path) -> bool {
    dir.is_dir() && dir.join(MANIFEST).is_file()
}

/// `backups/<id>` 一级目录 0700：里面有密钥，别人不能翻。
#[cfg(unix)]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

#[cfg(unix)]
fn mode_of(meta: &std::fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(meta.permissions().mode() & 0o7777)
}

#[cfg(not(unix))]
fn mode_of(_meta: &std::fs::Metadata) -> Option<u32> {
    None
}

#[cfg(unix)]
fn permissions_from_mode(mode: Option<u32>) -> Option<std::fs::Permissions> {
    use std::os::unix::fs::PermissionsExt;
    mode.map(std::fs::Permissions::from_mode)
}

#[cfg(not(unix))]
fn permissions_from_mode(_mode: Option<u32>) -> Option<std::fs::Permissions> {
    None
}

/// 同一秒里连着应用两次：id 加 `-1`、`-2` 后缀，不覆盖上一次的快照。
fn unique_id(root: &Path, base: &str) -> String {
    if !root.join(base).exists() {
        return base.to_string();
    }
    (1..)
        .map(|n| format!("{base}-{n}"))
        .find(|id| !root.join(id).exists())
        .unwrap_or_else(|| base.to_string())
}

/// 保留策略：留最近 `keep_latest` 次，且任何一次最多留 `keep_days` 天（两者取更严的）。
/// 只动带 `manifest.json` 的一级目录，别人的东西不碰。失败就算了，不影响应用结果。
fn prune(root: &Path, keep_latest: usize, keep_days: i64) -> std::io::Result<()> {
    let mut ids: Vec<String> = std::fs::read_dir(root)?
        .flatten()
        .filter(|entry| is_backup_dir(&entry.path()))
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    ids.sort_by(|a, b| b.cmp(a));

    let now = Local::now().naive_local();
    for (index, id) in ids.into_iter().enumerate() {
        let too_old = parse_id(&id).is_some_and(|created| (now - created).num_days() > keep_days);
        if index < keep_latest && !too_old {
            continue;
        }
        std::fs::remove_dir_all(root.join(id))?;
    }
    Ok(())
}

/// 从目录名 `YYYYMMDD-HHMMSS[-N]` 取时间；取不出来就不按年龄删。
fn parse_id(id: &str) -> Option<NaiveDateTime> {
    let head: String = id.chars().take(15).collect();
    NaiveDateTime::parse_from_str(&head, "%Y%m%d-%H%M%S").ok()
}
