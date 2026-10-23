use std::path::Path;
use std::process::Command;

use crate::CoreError;

/// 唯一调用 git 的地方。
///
/// argv 数组传参（不拼 shell 字符串）、始终显式 `-C <源目录>`、同时捕获
/// stdout/stderr 与退出码。凭据、代理、host key 全部交给 git 自己。
pub(crate) fn run(source_dir: &Path, args: &[&str]) -> Result<String, CoreError> {
    let out = Command::new("git")
        .arg("-C")
        .arg(source_dir)
        .args(args)
        .output()
        .map_err(|e| CoreError::io(format!("无法启动系统 git：{e}")))?;

    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    } else {
        Err(CoreError::git(
            format!("git {} 执行失败", args.join(" ")),
            String::from_utf8_lossy(&out.stderr).trim_end().to_string(),
        ))
    }
}

/// 上游的领先/落后计数，返回 `(behind, ahead)`。
///
/// `git rev-list --left-right --count @{u}...HEAD` 的输出是 `<behind>\t<ahead>`；
/// 没有上游时按 `(0, 0)`（首个远端由拉取那张票建立）。
pub(crate) fn ahead_behind(source_dir: &Path) -> (u32, u32) {
    match run(source_dir, &["rev-list", "--left-right", "--count", "@{u}...HEAD"]) {
        Ok(counts) => {
            let mut parts = counts.split_whitespace().map(|n| n.parse::<u32>().unwrap_or(0));
            (parts.next().unwrap_or(0), parts.next().unwrap_or(0))
        }
        Err(_) => (0, 0),
    }
}
