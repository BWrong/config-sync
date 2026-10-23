//! 首次使用向导的「已完成」标记。
//!
//! 标记落在 app 配置目录（`~/.config/config-sync/`），不进 git 仓库：
//! 向导只该出现一次，之后要加文件走清单页的添加入口。勾选添加与直接跳过都算走过。

use std::path::{Path, PathBuf};

/// 标记文件；存在即代表向导已经走过。
const MARKER: &str = "onboarding-done";

pub fn marker_path(home: &Path) -> PathBuf {
    crate::settings::app_dir(home).join(MARKER)
}

pub fn is_done(home: &Path) -> bool {
    marker_path(home).is_file()
}

pub fn mark_done(home: &Path) -> Result<(), String> {
    let path = marker_path(home);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("创建 {} 失败：{e}", parent.display()))?;
    }
    std::fs::write(&path, "done\n").map_err(|e| format!("写入 {} 失败：{e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_round_trips_through_disk_outside_the_source_dir() {
        let home = tempfile::tempdir().expect("临时 home");
        assert!(!is_done(home.path()), "首次启动还没有标记");

        mark_done(home.path()).expect("落盘标记");
        assert!(is_done(home.path()), "落盘后读回");
        mark_done(home.path()).expect("重复落盘不该报错");
        assert!(is_done(home.path()));

        let marker = marker_path(home.path());
        assert!(marker.starts_with(crate::settings::app_dir(home.path())));
        assert!(marker.to_string_lossy().contains(".config/config-sync"));
        assert!(!marker.starts_with(home.path().join(".local/share/config-sync")), "标记不进源目录");
    }

    #[test]
    fn missing_marker_file_means_the_wizard_has_not_run() {
        let home = tempfile::tempdir().expect("临时 home");
        std::fs::create_dir_all(crate::settings::app_dir(home.path())).expect("建 app 目录");
        assert!(!is_done(home.path()), "只有 config.toml 时向导仍算没走过");
    }
}
