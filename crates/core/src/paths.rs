use std::path::{Path, PathBuf};

use crate::CoreError;

/// 源目录里参与镜像的子树：`<source_dir>/home` 对应 `$HOME`。
pub(crate) const MIRROR_ROOT: &str = "home";

/// 列出 `home/` 下所有受管文件的相对路径（相对 `home/`），按字典序排序，
/// 保证清单顺序与文件系统遍历顺序无关。
///
/// 只接受普通文件：符号链接语义在 v1 之外（见 DESIGN.md）。但符号链接不能被
/// **静默**跳过——调用方应把 `skipped_symlinks` 呈现给用户，否则 `ln -s` 管理的
/// 文件会无声地不出现在清单里（C2）。
pub(crate) fn managed_relative_paths(mirror_root: &Path) -> Result<(Vec<PathBuf>, usize), CoreError> {
    let mut out = Vec::new();
    let mut skipped_symlinks = 0usize;
    walk(mirror_root, mirror_root, &mut out, &mut skipped_symlinks)?;
    out.sort();
    Ok((out, skipped_symlinks))
}

fn walk(
    dir: &Path,
    base: &Path,
    out: &mut Vec<PathBuf>,
    skipped_symlinks: &mut usize,
) -> Result<(), CoreError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        // `home/` 还不存在 = 还没有任何受管文件，不是错误。
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(CoreError::io(format!("读取 {} 失败：{e}", dir.display()))),
    };

    for entry in entries {
        let entry = entry.map_err(|e| CoreError::io(format!("读取 {} 失败：{e}", dir.display())))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|e| CoreError::io(format!("读取 {} 类型失败：{e}", path.display())))?;

        if file_type.is_symlink() {
            // v1 不做符号链接语义：计数并跳过，不让它无声消失。
            *skipped_symlinks += 1;
        } else if file_type.is_dir() {
            walk(&path, base, out, skipped_symlinks)?;
        } else if file_type.is_file() {
            if let Ok(rel) = path.strip_prefix(base) {
                out.push(rel.to_path_buf());
            }
        }
    }
    Ok(())
}
