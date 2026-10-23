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
