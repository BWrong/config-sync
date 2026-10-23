use serde::{Deserialize, Serialize};

/// 由外壳注入的设置。磁盘上的 `config.toml`（含 `source_dir`）由外壳读写，
/// Core 只用构造参数里那份 `source_dir`，避免同一事实有两个来源。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// 远端分支名。
    pub branch: String,
    /// 空 = 用系统关联 app 打开。
    pub editor_command: String,
}

impl Default for Config {
    fn default() -> Self {
        Config { branch: "main".to_string(), editor_command: String::new() }
    }
}
