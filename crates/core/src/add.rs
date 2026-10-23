//! 添加：把 `$HOME` 下的真实文件复制进源目录的镜像位置。
//!
//! 只复制，不动 git（新文件在源目录里是未跟踪 → 清单显示「待提交」），也不写任何目标文件。

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::{Core, CoreError, paths};

/// 一条输入路径的结果。「添加」是可批量的，所以每条各自成败，不因一条被拒而丢掉整批。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AddOutcome {
    /// 被添加的路径（目录输入展开成其中每个文件）；被拒时就是输入的那个路径。
    pub target: PathBuf,
    pub result: AddResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum AddResult {
    /// 已复制进源目录。
    Added,
    /// 已在清单中且与磁盘内容相同：不覆盖，也不产生第二行。
    AlreadyManaged,
    /// 已在清单中，但磁盘上的目标文件与源文件内容不同：不覆盖，只提示。
    AlreadyManagedDiffers,
    /// 没过校验，什么都没复制；`reason` 是给人看的原因。
    Rejected { reason: String },
}

impl Core {
    /// 把一个或多个路径收进源目录。目录会整棵子树递归收进来。
    ///
    /// 路径推导：目标路径去掉 `$HOME` 前缀即镜像位置（`home/<相对路径>`），
    /// `$HOME` 之外的路径无法用镜像布局表达，逐条拒绝。
    pub fn add(&self, paths: &[PathBuf]) -> Result<Vec<AddOutcome>, CoreError> {
        let mut outcomes = Vec::new();
        for path in paths {
            self.add_one(path, &mut outcomes)?;
        }
        Ok(outcomes)
    }

    fn add_one(&self, input: &Path, out: &mut Vec<AddOutcome>) -> Result<(), CoreError> {
        let Some(rel) = self.mirror_relative_path(input) else {
            out.push(AddOutcome {
                target: input.to_path_buf(),
                result: AddResult::Rejected {
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
        let meta = match std::fs::metadata(&target) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                out.push(AddOutcome {
                    target,
                    result: AddResult::Rejected { reason: format!("{} 不存在", input.display()) },
                });
                return Ok(());
            }
            Err(e) => {
                return Err(CoreError::io(format!("读取 {} 失败：{e}", target.display())))
            }
        };

        if meta.is_dir() {
            let mut files = Vec::new();
            collect_regular_files(&target, &mut files)?;
            files.sort();
            for file in files {
                // 目录在 $HOME 下，其中的文件也一定在 $HOME 下。
                let rel = file.strip_prefix(&self.home).expect("目录内的文件都在 $HOME 下");
                self.copy_into_mirror(rel, out)?;
            }
        } else if meta.is_file() {
            self.copy_into_mirror(&rel, out)?;
        } else {
            out.push(AddOutcome {
                target,
                result: AddResult::Rejected {
                    reason: format!("{} 不是普通文件，v1 只托管普通文件", input.display()),
                },
            });
        }
        Ok(())
    }

    /// 单个文件的复制；源文件已存在时只报告，绝不覆盖。
    fn copy_into_mirror(&self, rel: &Path, out: &mut Vec<AddOutcome>) -> Result<(), CoreError> {
        let target = self.home.join(rel);
        let source = self.source_dir.join(paths::MIRROR_ROOT).join(rel);

        if source.exists() {
            let same = match (std::fs::read(&target), std::fs::read(&source)) {
                (Ok(target_bytes), Ok(source_bytes)) => target_bytes == source_bytes,
                // 目标读不到（例如已被删除）就谈不上内容不同。
                _ => true,
            };
            let result =
                if same { AddResult::AlreadyManaged } else { AddResult::AlreadyManagedDiffers };
            out.push(AddOutcome { target, result });
            return Ok(());
        }

        if let Some(parent) = source.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                CoreError::io(format!("创建 {} 失败：{e}", parent.display()))
            })?;
        }
        std::fs::copy(&target, &source).map_err(|e| {
            CoreError::io(format!("复制 {} 到 {} 失败：{e}", target.display(), source.display()))
        })?;
        out.push(AddOutcome { target, result: AddResult::Added });
        Ok(())
    }
}

/// 递归收集普通文件。符号链接按指向的内容算（不递归进符号链接目录，避免环），
/// 管道、套接字、断链等非普通文件直接跳过。
fn collect_regular_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), CoreError> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| CoreError::io(format!("读取 {} 失败：{e}", dir.display())))?;

    for entry in entries {
        let entry = entry.map_err(|e| CoreError::io(format!("读取 {} 失败：{e}", dir.display())))?;
        let path = entry.path();
        let Ok(meta) = std::fs::metadata(&path) else { continue };
        if meta.is_dir() {
            let is_symlink = entry.file_type().map(|t| t.is_symlink()).unwrap_or(true);
            if !is_symlink {
                collect_regular_files(&path, out)?;
            }
        } else if meta.is_file() {
            out.push(path);
        }
    }
    Ok(())
}
