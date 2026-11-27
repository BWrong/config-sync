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
use crate::push::human_bytes;
use crate::{
    Core, CoreError, DIFF_HARD_MAX_BYTES, DIFF_MAX_BYTES, diff_gate_reason, too_large_reason,
    unified_diff,
};

/// 备份根目录（app 配置目录下）：不在源仓库里，因此不会被 git 跟踪、不会被推送。
pub(crate) const BACKUP_ROOT: &str = ".config/config-sync/backups";
/// 保留策略：无论如何留最近这么多次应用。
const KEEP_LATEST: usize = 10;
/// 保留策略：超过这个天数的、又不在「最近 10 次」里的备份会被清掉。
const KEEP_DAYS: i64 = 30;

const MANIFEST: &str = "manifest.json";
const FILES: &str = "files";
/// 清单落盘节奏：第一项之后每记这么多项落一次盘（不是每项）。
/// 落一次盘 = 整份清单序列化 + 临时文件 + rename，逐项落就是 O(N²) 写入；
/// 按批落把写入次数压到 `N / 64 + 1`，代价是应用中途崩掉时最后不到 64 项没有记录
/// （见 `Snapshot::push`）。这个「不到 64」窗口的前提是每一批都落盘成功；没落成的批次
/// 记在 `Snapshot::flush_failures` 里，由 `commit` 当警告带出去，实际窗口会更大。
const MANIFEST_FLUSH_EVERY: usize = 64;

/// 一次应用的清单：撤销只看它。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupManifest {
    /// 目录名，也是 `list_backups` 里的 id。
    pub id: String,
    pub created_at: String,
    /// 清单是否落到了 `commit()` 那一次。批次落盘写下的只是半截清单：进程在此之后死掉，
    /// 盘上这一份就停在那个落盘点（`false`），之后写到目标的项撤销不到。
    /// 老清单没有这个字段，默认按完整算（那时只在结束时落一次，有清单就是落完了）。
    #[serde(default = "complete_by_default")]
    pub complete: bool,
    pub entries: Vec<BackupEntry>,
}

fn complete_by_default() -> bool {
    true
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
    /// 应用写下去的内容长度：与指纹一起比对，抵掉哈希撞车（老清单没有这一项时跳过这项检查）。
    #[serde(default)]
    pub written_len: Option<u64>,
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
    /// 清单是否落到了 `commit()` 那一次：`false` 说明进程死在应用中途，这份备份可能不完整。
    pub complete: bool,
}

/// 撤销计划：预览展示它，执行也执行它。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestorePlan {
    pub backup: String,
    pub created_at: String,
    /// 同 `BackupSummary::complete`：`false` 时预览要提示这份备份可能不完整。
    #[serde(default)]
    pub complete: bool,
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
    /// 出计划时目标内容的长度：与指纹一起比对（双保险）。
    #[serde(default)]
    pub expected_len: Option<u64>,
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

/// 一次应用写盘前的快照目录。`execute_apply` 每写一项前 `save`，按批落清单，最后 `commit` 再落一次。
pub(crate) struct Snapshot {
    dir: PathBuf,
    manifest: BackupManifest,
    /// 批次落盘失败的次数：不能吞掉，`commit()` 的警告要能说清这一档。
    flush_failures: usize,
    /// 落盘次数：只有测试看得见（逐项落盘的回归靠它计数）。
    #[cfg(test)]
    flushes: std::cell::Cell<usize>,
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
                complete: false,
                entries: Vec::new(),
            },
            flush_failures: 0,
            #[cfg(test)]
            flushes: std::cell::Cell::new(0),
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
        written_len: Option<u64>,
        result: BackupResult,
    ) {
        self.manifest.entries.push(BackupEntry {
            target: target.to_path_buf(),
            file: origin.as_ref().and_then(|origin| origin.file.clone()),
            existed: origin.as_ref().is_some_and(|origin| origin.existed),
            mode: origin.and_then(|origin| origin.mode),
            written_digest,
            written_len,
            result,
        });
        // 按批落盘：第一项落一次（应用确实写过东西了，清单就该在盘上），之后每
        // `MANIFEST_FLUSH_EVERY` 项落一次，最后 `commit` 必然再落一次。
        // 崩溃窗口：进程死在两次落盘之间时，最后不到 64 项已经写进目标但没记进清单，
        // 这部分撤销不了（备份原文仍在 `files/` 里，可手动恢复）；其余项都可撤销。
        // 这个「不到 64」的前提是这一批落盘成功了：没落成的批次要计数，由 `commit` 带出去。
        let recorded = self.manifest.entries.len();
        if (recorded == 1 || recorded.is_multiple_of(MANIFEST_FLUSH_EVERY))
            && self.write_manifest().is_err()
        {
            self.flush_failures += 1;
        }
    }

    pub(crate) fn record_written(
        &mut self,
        target: &Path,
        origin: Origin,
        written_digest: String,
        written_len: Option<u64>,
    ) {
        self.push(
            target,
            Some(origin),
            Some(written_digest),
            written_len,
            BackupResult::Written,
        );
    }

    /// 没写成的一项：`origin` 为 `None` 表示连备份都没拿到（目标文件没被碰过）。
    pub(crate) fn record_failed(&mut self, target: &Path, origin: Option<Origin>, message: String) {
        self.push(target, origin, None, None, BackupResult::Failed { message });
    }

    fn write_manifest(&self) -> Result<(), String> {
        #[cfg(test)]
        self.flushes.set(self.flushes.get() + 1);
        let path = self.dir.join(MANIFEST);
        let json = serde_json::to_string_pretty(&self.manifest)
            .map_err(|e| format!("生成备份清单失败：{e}"))?;
        // 原子写：崩溃时不会留下一份写到一半的清单（它会盖掉上一次落好的那份）。
        replace_file(&path, json.as_bytes(), None)
    }

    /// 落清单并按保留策略清旧备份。
    ///
    /// 返回值第二项是清单写失败的原因：那次应用已经写完盘了，所以这不叫失败，叫「这次备份
    /// 撤销不了」，由调用方当警告带回去。
    pub(crate) fn commit(mut self) -> (BackupSummary, Option<String>) {
        // 最后一次落盘：只有它落下去了，盘上才是完整清单（`complete` 标记也在这里翻成 true）。
        self.manifest.complete = true;
        let warning = match (self.write_manifest().err(), self.flush_failures) {
            // 最后一次落成了 = 盘上是一份完整清单，中途批次失败没有留下后果：不报。
            // 报了前端会把「有警告」当成「清单没落盘」，白关掉一个本来能用的撤销入口。
            (None, _) => None,
            (Some(error), 0) => Some(error),
            (Some(error), failures) => Some(format!(
                "{error}；另外有 {failures} 次批次落盘失败：清单可能停在更早的落盘点，\
                 未记清单的项会多于 {} 项",
                MANIFEST_FLUSH_EVERY - 1
            )),
        };

        // 保留策略是「还好」，清不掉不影响这次应用的结果。
        let _ = prune(
            self.dir.parent().unwrap_or(&self.dir),
            KEEP_LATEST,
            KEEP_DAYS,
        );

        (
            BackupSummary {
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
                complete: self.manifest.complete,
            },
            warning,
        )
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
                complete: manifest.complete,
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
                expected_digest: None,
                expected_len: None,
                backup_digest: None,
            };

            if !matches!(entry.result, BackupResult::Written) {
                item.reason = "这次应用没有写成这一项，目标文件原样未动，不需要恢复".to_string();
                items.push(item);
                continue;
            }

            // 预览要把目标与备份原文整份读进内存，所有判断（指纹 / diff）都建立在读到的字节上：
            // 先看长度，超硬上限就只说「太大，请在编辑器里对比」，不去读它。
            let target_len = match std::fs::metadata(target) {
                Ok(meta) => Some(meta.len()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => {
                    return Err(CoreError::io(format!(
                        "读取 {} 失败：{e}",
                        target.display()
                    )));
                }
            };
            let backup_path = entry.file.as_ref().map(|file| dir.join(file));
            let backup_len = backup_path
                .as_ref()
                .and_then(|path| std::fs::metadata(path).ok().map(|meta| meta.len()));
            let widest = target_len.into_iter().chain(backup_len).max().unwrap_or(0);
            if widest > DIFF_HARD_MAX_BYTES {
                item.reason = format!(
                    "文件太大（能碰到的那一份有 {}，单个文件上限 {}）：撤销预览不读这么大的内容，\
                     请在编辑器里对比后手动处理",
                    human_bytes(widest),
                    human_bytes(DIFF_HARD_MAX_BYTES)
                );
                items.push(item);
                continue;
            }

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
            let current_len = current.as_ref().map(|bytes| bytes.len() as u64);
            // 「目标还是那次应用写下的样子」：撤销的安全前提。
            let untouched = matches_original(
                current_digest.as_deref(),
                current_len,
                entry.written_digest.as_deref(),
                entry.written_len,
            );
            item.expected_digest = current_digest;
            item.expected_len = current_len;

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
            let backup_bytes = match backup_path.as_ref() {
                Some(path) => match std::fs::read(path) {
                    Ok(bytes) => Some(bytes),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                    Err(e) => {
                        return Err(CoreError::io(format!(
                            "读取备份文件 {} 失败：{e}",
                            path.display()
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
            complete: manifest.complete,
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
        // 计划是前端送回来的：写盘前和 apply 一样守「目标必须在 $HOME 下」这条规矩，
        // 被改过的计划不能把任意路径当目标。
        relative_to_home(&item.target, &self.home).map_err(|error| error.message)?;

        let current = std::fs::read(&item.target).ok();
        // 删除项的目标如果在预览之后已经被删掉，就当已经做完（删除是幂等的，不会碰到别的东西）。
        if current.is_none() && item.action == RestoreAction::Delete {
            return Ok(RestoreResult::Deleted);
        }
        let current_digest = current.as_deref().map(digest);
        let current_len = current.as_ref().map(|bytes| bytes.len() as u64);
        if !matches_original(
            current_digest.as_deref(),
            current_len,
            item.expected_digest.as_deref(),
            item.expected_len,
        ) {
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
                // 备份里的位置只能是备份目录内的相对路径：`..`、绝对路径都不能拿来指到别处。
                if file.as_os_str().is_empty()
                    || file
                        .components()
                        .any(|component| !matches!(component, Component::Normal(_)))
                {
                    return Err(format!(
                        "备份里的路径 {} 不是备份目录内的相对路径；没有写 {}",
                        file.display(),
                        item.target.display()
                    ));
                }
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

/// 「这份内容还是那次写下的样子」：指纹与长度都对得上。
/// 老清单没有长度（`written_len`）时只比指纹，不至于把旧备份一棍子打死。
fn matches_original(
    current_digest: Option<&str>,
    current_len: Option<u64>,
    written_digest: Option<&str>,
    written_len: Option<u64>,
) -> bool {
    let (Some(current_digest), Some(written_digest)) = (current_digest, written_digest) else {
        return false;
    };
    current_digest == written_digest && written_len.is_none_or(|len| current_len == Some(len))
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
/// 只动备份根目录下的一级目录，别人的东西不碰。失败就算了，不影响应用结果。
fn prune(root: &Path, keep_latest: usize, keep_days: i64) -> std::io::Result<()> {
    let mut ids: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if !path.join(MANIFEST).is_file() {
            // 应用写到一半崩掉留下的半截备份：没有清单就撤销不了，按 mtime 过期清掉，
            // 否则它会永远躺在 backups/ 里（上面按 id 排名的逻辑看不见它）。
            if older_than(&entry, keep_days) {
                let _ = std::fs::remove_dir_all(&path);
            }
            continue;
        }
        ids.push(entry.file_name().to_string_lossy().into_owned());
    }
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

/// 目录的 mtime 至少是 `days` 天之前。读不到 mtime 一律当「不够旧」，不动它。
fn older_than(entry: &std::fs::DirEntry, days: i64) -> bool {
    let Ok(modified) = entry.metadata().and_then(|meta| meta.modified()) else {
        return false;
    };
    let Ok(age) = std::time::SystemTime::now().duration_since(modified) else {
        return false;
    };
    age.as_secs() >= (days.max(0) as u64).saturating_mul(86_400)
}

/// 从目录名 `YYYYMMDD-HHMMSS[-N]` 取时间；取不出来就不按年龄删。
fn parse_id(id: &str) -> Option<NaiveDateTime> {
    let head: String = id.chars().take(15).collect();
    NaiveDateTime::parse_from_str(&head, "%Y%m%d-%H%M%S").ok()
}

/// 清单落盘与保留策略的单元测试：只碰临时目录，不需要 git。
#[cfg(test)]
mod tests {
    use super::*;

    /// 一个 home / 一个（跟 home 无关的）源目录，外加一份已写好的目标文件。
    fn world() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let source = tmp.path().join("source");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&source).unwrap();
        let target = home.join(".zshrc");
        std::fs::write(&target, "export A=1\n").unwrap();
        (tmp, home, source, target)
    }

    fn snapshot_with_one_write(home: &Path, source: &Path, target: &Path) -> Snapshot {
        let mut snapshot = Snapshot::create(home, source).unwrap();
        let origin = snapshot.save(home, target).unwrap();
        let len = std::fs::metadata(target).unwrap().len();
        snapshot.record_written(
            target,
            origin,
            crate::apply::digest(b"export A=1\n"),
            Some(len),
        );
        snapshot
    }

    /// 清单写不下去（磁盘满 / 目录被人删了换成了文件）不能变成 Err：
    /// 文件已经写了，调用方还欠界面一句「已写 N 个」，撤销入口也得留着。
    #[test]
    fn a_failed_manifest_write_becomes_a_warning_not_an_error() {
        let (_tmp, home, source, target) = world();
        let snapshot = snapshot_with_one_write(&home, &source, &target);

        // 把备份目录整个换成同名文件：清单再也写不进去。
        std::fs::remove_dir_all(&snapshot.dir).unwrap();
        std::fs::write(&snapshot.dir, b"not a dir").unwrap();

        let (summary, warning) = snapshot.commit();

        assert_eq!(
            (summary.written, summary.failed),
            (1, 0),
            "清单失败不能把已经写好的结果抹掉"
        );
        let warning = warning.expect("清单没写成必须带一条警告回去");
        assert!(
            warning.contains("manifest.json"),
            "警告要说清是哪个清单没写成：{warning}"
        );
    }

    /// 第一项就落盘：应用刚写过东西，清单就该在盘上，进程此时崩掉也还有撤销记录。
    #[test]
    fn the_first_recorded_entry_is_on_disk_before_commit() {
        let (_tmp, home, source, target) = world();
        let snapshot = snapshot_with_one_write(&home, &source, &target);

        let manifest = snapshot.dir.join(MANIFEST);
        let text = std::fs::read_to_string(&manifest).expect("记录一项之后清单就该在盘上");
        assert!(text.contains(".zshrc"), "清单里要有这一项的路径：{text}");
        assert!(
            text.contains("written_len"),
            "清单里要存下写入长度（指纹之外的第二种比对）：{text}"
        );
    }

    /// 落盘节奏：N 项只按批落盘，不是每项一次。逐项落一次 = 整份清单序列化 + rename 各 N 次，
    /// 累计写入量 O(N²)（全部应用上千项时是几百 MB 写放大）。
    #[test]
    fn manifest_flushes_are_batched_not_one_per_entry() {
        let (_tmp, home, source, _target) = world();
        let mut snapshot = Snapshot::create(&home, &source).unwrap();
        let n = MANIFEST_FLUSH_EVERY * 3 + 8;
        for i in 0..n {
            snapshot.record_failed(&home.join(format!(".file-{i}")), None, "没写成".to_string());
        }

        let flushes = snapshot.flushes.get();
        assert!(
            flushes <= n / MANIFEST_FLUSH_EVERY + 1,
            "{n} 项只该按落盘周期落盘，实际落了 {flushes} 次（逐项落盘就是 {n} 次）"
        );
        assert!(
            flushes >= 2,
            "一项都不落盘的话，应用中途崩掉就一点记录都没有：{flushes}"
        );
    }

    /// 崩溃窗口：不 `commit`（进程在应用中途死掉）时，盘上的清单停在最后一个落盘点，
    /// 之后最多 `MANIFEST_FLUSH_EVERY - 1` 项没记进去。交付说明里写的就是这个窗口。
    #[test]
    fn a_crash_mid_apply_leaves_the_manifest_at_the_last_flush_point() {
        let (_tmp, home, source, _target) = world();
        let mut snapshot = Snapshot::create(&home, &source).unwrap();
        let tail = MANIFEST_FLUSH_EVERY / 2;
        let n = MANIFEST_FLUSH_EVERY + tail;
        for i in 0..n {
            snapshot.record_failed(&home.join(format!(".file-{i}")), None, "没写成".to_string());
        }

        // 进程到这里死掉：不再 commit，盘上只剩最后一次落盘的那份。
        let text = std::fs::read_to_string(snapshot.dir.join(MANIFEST)).expect("清单在盘上");
        let on_disk: BackupManifest = serde_json::from_str(&text).unwrap();
        assert_eq!(
            on_disk.entries.len(),
            MANIFEST_FLUSH_EVERY,
            "盘上停在最后一个落盘点"
        );
        assert_eq!(
            n - on_disk.entries.len(),
            tail,
            "丢掉的是落盘点之后的尾巴，不能超过一个落盘周期"
        );
        assert!(
            !on_disk.complete,
            "没有 commit 的清单是半截的：撤销预览要能提示「可能不完整」"
        );
    }

    /// 批次落盘失败不能吞：最后一次也没落成时，警告要说清「另外有几次没落成」。
    #[test]
    fn batch_flush_failures_are_counted_in_the_commit_warning() {
        let (_tmp, home, source, _target) = world();
        let mut snapshot = Snapshot::create(&home, &source).unwrap();
        // 第一项那一批是成功的（清单已经在盘上）。
        snapshot.record_failed(&home.join(".a"), None, "没写成".to_string());

        // 把清单路径换成一个目录：之后每一次落盘（批次与 commit）都 rename 不上去。
        let manifest = snapshot.dir.join(MANIFEST);
        std::fs::remove_file(&manifest).unwrap();
        std::fs::create_dir(&manifest).unwrap();
        for i in 0..MANIFEST_FLUSH_EVERY - 1 {
            snapshot.record_failed(&home.join(format!(".file-{i}")), None, "没写成".to_string());
        }
        assert_eq!(snapshot.flush_failures, 1, "走到一个落盘点，失败一次");

        let (_, warning) = snapshot.commit();

        let warning = warning.expect("最后一次也没落成，必须带一条警告回去");
        assert!(
            warning.contains("1 次批次落盘失败"),
            "警告要说清另外有几次批次落盘失败：{warning}"
        );
    }

    /// 批次失败后最后一次落成了 = 盘上是完整清单，撤销照常可用：这时不该报警告，
    /// 否则前端会把「有警告」当成「清单没落盘」，关掉一个本来能用的撤销入口。
    #[test]
    fn a_recovered_batch_failure_does_not_warn_because_the_last_write_is_complete() {
        let (_tmp, home, source, _target) = world();
        let mut snapshot = Snapshot::create(&home, &source).unwrap();
        snapshot.record_failed(&home.join(".a"), None, "没写成".to_string());

        let manifest = snapshot.dir.join(MANIFEST);
        std::fs::remove_file(&manifest).unwrap();
        std::fs::create_dir(&manifest).unwrap();
        for i in 0..MANIFEST_FLUSH_EVERY - 1 {
            snapshot.record_failed(&home.join(format!(".file-{i}")), None, "没写成".to_string());
        }
        assert_eq!(snapshot.flush_failures, 1);

        // 空间回来了：最后一次落盘成功。
        std::fs::remove_dir(&manifest).unwrap();
        let (summary, warning) = snapshot.commit();

        assert!(warning.is_none(), "最后一次落成了就不该报警告：{warning:?}");
        assert_eq!(summary.written, 0);
        assert!(summary.complete, "commit 落成之后就是一份完整备份");
    }

    /// 应用中途崩掉留下的半截清单（只有批次落盘那几项）：`list_backups` 与撤销预览
    /// 都要能看出「这份备份可能不完整」，否则它和一次正常备份长得一模一样。
    #[test]
    fn a_backup_left_by_a_crash_is_flagged_incomplete() {
        let (_tmp, home, source, target) = world();
        let mut snapshot = Snapshot::create(&home, &source).unwrap();
        let origin = snapshot.save(&home, &target).unwrap();
        let len = std::fs::metadata(&target).unwrap().len();
        snapshot.record_written(
            &target,
            origin,
            crate::apply::digest(b"export A=1\n"),
            Some(len),
        );
        // 进程到这里死掉：不再 commit。
        drop(snapshot);

        let core = Core::new(&home, &source, crate::Config::default());
        let backups = core.list_backups().unwrap();
        assert_eq!(backups.len(), 1, "{backups:?}");
        assert!(!backups[0].complete, "半截清单必须标成不完整");

        let plan = core.plan_restore_backup(&backups[0].id).unwrap();
        assert!(!plan.complete, "撤销预览也要能看到这份备份不完整");
    }

    /// 老清单没有 `complete` 字段：按完整算（那时只在结束时落一次，有清单就是落完了）。
    #[test]
    fn an_old_manifest_without_the_flag_reads_as_complete() {
        let (_tmp, home, source, _target) = world();
        let core = Core::new(&home, &source, crate::Config::default());
        let id = "20200101-000000";
        let dir = home.join(BACKUP_ROOT).join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(MANIFEST),
            format!(r#"{{"id":"{id}","created_at":"x","entries":[]}}"#),
        )
        .unwrap();

        assert!(
            core.read_manifest(id).unwrap().complete,
            "没有标记的老清单不能被当成半截备份"
        );
    }

    /// 结束时必然落一次盘：就算中间一次都没落（或被删了），`commit` 也自己写。
    #[test]
    fn commit_always_writes_the_manifest_at_the_end() {
        let (_tmp, home, source, target) = world();
        let snapshot = snapshot_with_one_write(&home, &source, &target);
        let manifest = snapshot.dir.join(MANIFEST);
        std::fs::remove_file(&manifest).unwrap();

        let (summary, warning) = snapshot.commit();

        assert!(warning.is_none(), "{warning:?}");
        assert_eq!(summary.written, 1);
        assert!(manifest.is_file(), "commit 必须自己再落一次清单");
    }

    /// 没有 `manifest.json` 的半截备份（应用写到一半崩了）也要按 mtime 清掉，
    /// 否则它永远躺在 `backups/` 里、按 id 排名的保留策略看不见它。
    #[test]
    fn prune_removes_manifest_less_dirs_once_they_are_old_enough() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("backups");
        std::fs::create_dir_all(&root).unwrap();

        let orphan = root.join("20200101-000000");
        std::fs::create_dir_all(orphan.join(FILES)).unwrap();
        let fresh = root.join(Local::now().format("%Y%m%d-%H%M%S").to_string());
        std::fs::create_dir_all(&fresh).unwrap();
        std::fs::write(
            fresh.join(MANIFEST),
            format!(
                r#"{{"id":"{}","created_at":"刚刚","entries":[]}}"#,
                fresh.file_name().unwrap().to_string_lossy()
            ),
        )
        .unwrap();

        // 把孤儿目录的 mtime 拨到 90 天前（std 的 set_times，不引第三方库）。
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(90 * 86_400);
        std::fs::File::open(&orphan)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old))
            .unwrap();

        prune(&root, KEEP_LATEST, KEEP_DAYS).unwrap();

        assert!(!orphan.exists(), "没有清单的旧备份要按 mtime 清掉");
        assert!(fresh.exists(), "有清单的新备份不能被顺手删掉");
    }
}
