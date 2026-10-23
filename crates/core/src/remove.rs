//! 删除：把文件从源目录的镜像位置移除（`home/<相对路径>`）。
//!
//! 只动源目录里的副本，**绝不碰 `$HOME` 下的原文件**。git 状态留给
//! 「待提交」，跟添加对称：添加产生未跟踪文件，删除产生已删除文件。

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::{Core, CoreError, paths};

/// 一条输入路径的结果。「删除」是批量的，每条各自成败，不因一条被拒而丢掉整批。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RemoveOutcome {
    /// 目标路径（`$HOME` 下的镜像绝对路径）。
    pub target: PathBuf,
    pub result: RemoveResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum RemoveResult {
    /// 已从源目录移除。
    Removed,
    /// 源目录里没有这个文件，什么都没动。
    NotManaged,
    /// 没过校验，什么都没删；`reason` 是给人看的原因。
    Rejected { reason: String },
}

impl Core {
    /// 把一个或多个路径从源目录移除（目录会整棵子树移除）。
    ///
    /// 只删源目录 `home/` 镜像里的副本；`$HOME` 下的原文件保持原样。
    pub fn remove(&self, targets: &[PathBuf]) -> Result<Vec<RemoveOutcome>, CoreError> {
        let mut outcomes = Vec::new();
        for target in targets {
            self.remove_one(target, &mut outcomes)?;
        }
        Ok(outcomes)
    }

    fn remove_one(&self, input: &Path, out: &mut Vec<RemoveOutcome>) -> Result<(), CoreError> {
        let Some(rel) = self.mirror_relative_path(input) else {
            out.push(RemoveOutcome {
                target: input.to_path_buf(),
                result: RemoveResult::Rejected {
                    reason: format!(
                        "{} 不在 {} 下：镜像布局只能表达 $HOME 之内的文件（home/ 与 $HOME 逐路径对应）",
                        input.display(),
                        self.home.display()
                    ),
                },
            });
            return Ok(());
        };

        let target = self.home.join(&rel);
        let source = self.source_dir.join(paths::MIRROR_ROOT).join(&rel);

        if !source.exists() {
            out.push(RemoveOutcome {
                target,
                result: RemoveResult::NotManaged,
            });
            return Ok(());
        }

        if source.is_dir() {
            std::fs::remove_dir_all(&source).map_err(|e| {
                CoreError::io(format!("删除 {} 失败：{e}", source.display()))
            })?;
        } else {
            std::fs::remove_file(&source).map_err(|e| {
                CoreError::io(format!("删除 {} 失败：{e}", source.display()))
            })?;
        }
        prune_empty_parents(source.parent());

        out.push(RemoveOutcome { target, result: RemoveResult::Removed });
        Ok(())
    }
}

/// 从 `dir` 向上清理空目录，直到遇见镜像根 `home/`（该层本身不删，也不向上）。
fn prune_empty_parents(mut dir: Option<&Path>) {
    while let Some(d) = dir {
        if d.file_name().map(|n| n != paths::MIRROR_ROOT).unwrap_or(false) {
            let Ok(mut entries) = std::fs::read_dir(d) else { return };
            if entries.next().is_some() {
                return; // 非空，停。
            }
            let _ = std::fs::remove_dir(d);
        }
        dir = d.parent();
    }
}