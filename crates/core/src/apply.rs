//! 应用：把源目录里的副本写到 `$HOME` 里的目标文件。
//!
//! 两阶段：`plan_apply` 只出计划（纯数据，不写盘），`execute_apply` 执行**同一份**计划。
//! 动作在计划里定下后不再重算；写盘只做两道防越界的检查（目标在 `$HOME` 下、源文件还在且没变过）。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::{Core, CoreError, Status, paths, read_text, state, unified_diff};

/// 一份完整的应用计划。预览展示它，写盘也执行它。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyPlan {
    pub items: Vec<ApplyItem>,
}

impl ApplyPlan {
    /// 批量摘要：新建 / 覆盖 / 跳过各几项。口径与 `execute_apply` 一致（跳过项不写盘）。
    pub fn summary(&self) -> ApplySummary {
        let count = |action| self.items.iter().filter(|item| item.action == action).count();
        ApplySummary {
            create: count(ApplyAction::Create),
            overwrite: count(ApplyAction::Overwrite),
            skip: count(ApplyAction::Skip),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ApplySummary {
    pub create: usize,
    pub overwrite: usize,
    pub skip: usize,
}

/// 计划里的一项：写哪个目标、做什么、为什么、会变成什么样。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyItem {
    /// 会被写到的真实路径（`$HOME` 下）。
    pub target: PathBuf,
    pub action: ApplyAction,
    /// 这个动作的原因，直接给界面显示；跳过项也一定有。
    pub reason: String,
    /// 应用会带来的改动（目标 → 源），unified diff；没有改动时是空串。
    pub diff: String,
    /// 出计划时源文件内容的指纹：写盘前比对，源文件在这期间变过就不写。
    pub digest: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApplyAction {
    /// 目标不存在，新建。
    Create,
    /// 目标存在且内容不同，覆盖（保留目标原有权限）。
    Overwrite,
    /// 不写：内容已一致、源目录里没有它，或源侧还要先提交 / 解决冲突。
    Skip,
}

/// 一项计划的执行结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApplyOutcome {
    pub target: PathBuf,
    pub result: ApplyResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum ApplyResult {
    Written,
    Failed { message: String },
}

impl Core {
    /// 出一份应用计划。`targets` 为 `None` 时是清单里的全部受管文件。
    ///
    /// 只读磁盘与 git，一个字节都不写；跳过的项也一定出现在计划里并带原因，不悄悄消失。
    pub fn plan_apply(&self, targets: Option<&[PathBuf]>) -> Result<ApplyPlan, CoreError> {
        let mirror_root = self.source_dir.join(paths::MIRROR_ROOT);
        let facts = state::GitFacts::read(&self.source_dir)?;
        let mut items = Vec::new();

        match targets {
            Some(targets) => {
                for target in targets {
                    let rel = relative_to_home(target, &self.home)?;
                    items.push(self.plan_item(&rel, &mirror_root, &facts)?);
                }
            }
            None => {
                for rel in paths::managed_relative_paths(&mirror_root)? {
                    items.push(self.plan_item(&rel, &mirror_root, &facts)?);
                }
            }
        }

        Ok(ApplyPlan { items })
    }

    /// 执行计划里的每一项（跳过项不写）。
    ///
    /// 边界行为：单项失败不中断整批，其余项照常写；每个目标文件的写入是原子的
    /// （同目录临时文件 + rename），失败项的目标文件保持原样，也不留临时文件。
    pub fn execute_apply(&self, plan: &ApplyPlan) -> Result<Vec<ApplyOutcome>, CoreError> {
        let mut outcomes = Vec::new();
        for item in &plan.items {
            if item.action == ApplyAction::Skip {
                continue;
            }
            let result = match self.write_item(item) {
                Ok(()) => ApplyResult::Written,
                Err(message) => ApplyResult::Failed { message },
            };
            outcomes.push(ApplyOutcome { target: item.target.clone(), result });
        }
        Ok(outcomes)
    }

    fn plan_item(
        &self,
        rel: &Path,
        mirror_root: &Path,
        facts: &state::GitFacts,
    ) -> Result<ApplyItem, CoreError> {
        let source = mirror_root.join(rel);
        let target = self.home.join(rel);

        // 源目录里没有这个文件：目标已存在也绝不覆盖（DESIGN 关键行为 1）。
        if !source.is_file() {
            return Ok(ApplyItem {
                target,
                action: ApplyAction::Skip,
                reason: format!(
                    "源目录里没有这个文件（{}），目标已存在也不会被覆盖",
                    source.display()
                ),
                diff: String::new(),
                digest: 0,
            });
        }

        let repo_path = Path::new(paths::MIRROR_ROOT).join(rel);
        let disk = self.disk(rel)?;
        let (action, reason) = match state::derive(disk, &repo_path, facts) {
            // 源侧内容还没定下来：先让用户处理提交 / 冲突，应用不动目标文件。
            Status::PendingCommit => (
                ApplyAction::Skip,
                "源目录里这个文件还有未提交的改动；先推送，再应用".to_string(),
            ),
            Status::Conflict => (
                ApplyAction::Skip,
                "这个文件在源目录与远端都有改动（冲突）；先解决冲突，再应用".to_string(),
            ),
            _ => match disk {
                state::Disk::Same => (ApplyAction::Skip, "内容已一致，不用写".to_string()),
                state::Disk::Missing => {
                    (ApplyAction::Create, "目标文件不存在，会新建".to_string())
                }
                state::Disk::Differs => (
                    ApplyAction::Overwrite,
                    "目标文件存在且内容不同，会覆盖（保留它原来的权限）".to_string(),
                ),
            },
        };

        let bytes = std::fs::read(&source)
            .map_err(|e| CoreError::io(format!("读取 {} 失败：{e}", source.display())))?;
        Ok(ApplyItem {
            diff: unified_diff(
                &target.display().to_string(),
                &source.display().to_string(),
                &read_text(&target),
                &String::from_utf8_lossy(&bytes),
            ),
            digest: digest(&bytes),
            target,
            action,
            reason,
        })
    }

    /// 写一项：补建父目录 → 同目录临时文件 → rename 到位。
    ///
    /// 临时文件保证编辑器之类的进程不会读到半截内容；权限上新建沿用源文件（可执行位不丢），
    /// 覆盖沿用目标文件原来的权限。
    fn write_item(&self, item: &ApplyItem) -> Result<(), String> {
        let rel = relative_to_home(&item.target, &self.home).map_err(|e| e.message)?;
        let source = self.source_dir.join(paths::MIRROR_ROOT).join(&rel);
        let target = &item.target;

        let bytes = std::fs::read(&source)
            .map_err(|e| format!("读取源文件 {} 失败：{e}；没有写 {}", source.display(), target.display()))?;
        if digest(&bytes) != item.digest {
            return Err(format!(
                "源文件 {} 在预览之后改过了，写下去的内容不是确认过的那份；没有写 {}，请重新预览",
                source.display(),
                target.display()
            ));
        }

        let parent = target
            .parent()
            .ok_or_else(|| format!("{} 没有父目录，没有写", target.display()))?;
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("创建 {} 失败：{e}；没有写 {}", parent.display(), target.display()))?;

        let temp = parent.join(format!(
            ".{}.config-sync-{}-{}.tmp",
            target.file_name().unwrap_or_default().to_string_lossy(),
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));

        let written = std::fs::copy(&source, &temp).and_then(|_| {
            // 覆盖已有文件时，保留它原来的权限（用户可能自己收紧过）。
            if let Ok(existing) = std::fs::metadata(target) {
                std::fs::set_permissions(&temp, existing.permissions())?;
            }
            std::fs::rename(&temp, target)
        });
        written.map_err(|e| {
            // rename 之前失败：目标文件从没被碰过，把临时文件清掉。
            let _ = std::fs::remove_file(&temp);
            format!("写 {} 失败：{e}", target.display())
        })
    }
}

/// 临时文件名里的序号，让同一进程内的多次写不会撞名。
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

/// 目标路径必须严格落在 `$HOME` 下（不能带 `..` / `.` 这类穿透成分）。
fn relative_to_home(target: &Path, home: &Path) -> Result<PathBuf, CoreError> {
    let rel = target.strip_prefix(home).map_err(|_| {
        CoreError::io(format!(
            "{} 不在 {} 下，没有对应的受管文件",
            target.display(),
            home.display()
        ))
    })?;
    if rel.components().any(|component| !matches!(component, Component::Normal(_))) {
        return Err(CoreError::io(format!(
            "{} 不是 $HOME 下的普通路径，拒绝写盘",
            target.display()
        )));
    }
    Ok(rel.to_path_buf())
}

/// 源文件内容的指纹，只为「预览之后源文件变过没有」这一次比对服务。
fn digest(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}
