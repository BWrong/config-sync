use std::path::Path;
use std::process::Command;

use crate::CoreError;

/// 唯一调用 git 的地方。
///
/// argv 数组传参（不拼 shell 字符串）、始终显式 `-C <源目录>`、同时捕获
/// stdout/stderr 与退出码。凭据、代理、host key 全部交给 git 自己。
pub(crate) fn run(source_dir: &Path, args: &[&str]) -> Result<String, CoreError> {
    #[cfg(test)]
    GIT_RUNS.with(|runs| runs.set(runs.get() + 1));
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

// #42-4 的取证用：每 spawn 一次 git 就自增，单元测试据此数进程。
// 按线程计数：同一个测试二进制里并行跑着的其它用例也会起 git，共用一个计数就没法下断言。
#[cfg(test)]
thread_local! {
    static GIT_RUNS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn git_runs() -> usize {
    GIT_RUNS.with(std::cell::Cell::get)
}

#[cfg(test)]
pub(crate) fn reset_git_runs() {
    GIT_RUNS.with(|runs| runs.set(0));
}
