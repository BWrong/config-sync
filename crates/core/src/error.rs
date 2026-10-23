use std::fmt;

use serde::Serialize;

/// 结构化错误：种类 + 人话说明 + git 原始 stderr。
///
/// 外壳必须把 `stderr` 原样展示给用户，不得吞掉。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CoreError {
    pub kind: ErrorKind,
    /// 中文说明，直接可展示。
    pub message: String,
    /// git 子进程的原始 stderr（非 git 错误时为 `None`）。
    pub stderr: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ErrorKind {
    /// 系统 git 调用失败。
    Git,
    /// 文件系统操作失败。
    Io,
}

impl CoreError {
    pub(crate) fn io(message: impl Into<String>) -> Self {
        CoreError { kind: ErrorKind::Io, message: message.into(), stderr: None }
    }

    pub(crate) fn git(message: impl Into<String>, stderr: impl Into<String>) -> Self {
        CoreError { kind: ErrorKind::Git, message: message.into(), stderr: Some(stderr.into()) }
    }
}

impl fmt::Display for CoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.stderr {
            Some(stderr) => write!(f, "{}: {stderr}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for CoreError {}
