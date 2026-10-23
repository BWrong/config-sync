//! `config.toml` 的读写。
//!
//! 文件固定在 `~/Library/Application Support/config-sync/config.toml`，不进 git。
//! 磁盘结构留在外壳：Core 只收到 `Config { branch, editor_command }` 与解析后的 `source_dir`，
//! 所以 `source_dir` 只在这里出现一次，Core 不读 env、不读 `$HOME`。

use std::path::{Path, PathBuf};

use config_sync_core::Config;
use serde::{Deserialize, Serialize};

/// 默认源目录；`~` 在 `resolved_source_dir` 里展开。
pub const DEFAULT_SOURCE_DIR: &str = "~/.config-sync";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    pub source_dir: String,
    /// 空 = 用系统关联 app 打开。
    pub editor_command: String,
    pub branch: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            source_dir: DEFAULT_SOURCE_DIR.to_string(),
            editor_command: String::new(),
            branch: "main".to_string(),
        }
    }
}

impl Settings {
    /// 读 `config.toml`；文件不存在时写入默认值（首次启动）。
    ///
    /// 解析失败返回人话错误，**不**覆盖磁盘上那份文件——坏文件留给人看。
    pub fn load_or_create(path: &Path) -> Result<Settings, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text)
                .map_err(|e| format!("{} 解析失败：{e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let settings = Settings::default();
                settings.save(path)?;
                Ok(settings)
            }
            Err(e) => Err(format!("读取 {} 失败：{e}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let write = || -> Result<(), String> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("创建 {} 失败：{e}", parent.display()))?;
            }
            let text = toml::to_string(self).map_err(|e| format!("序列化配置失败：{e}"))?;
            std::fs::write(path, text).map_err(|e| format!("写入 {} 失败：{e}", path.display()))
        };
        write()
    }

    /// `~` / `~/x` 展开成 `home`；其它路径原样返回。
    pub fn resolved_source_dir(&self, home: &Path) -> PathBuf {
        match self.source_dir.strip_prefix("~") {
            Some(rest) => home.join(rest.trim_start_matches('/')),
            None => PathBuf::from(&self.source_dir),
        }
    }

    /// 注入 Core 的那份设置（不含 `source_dir`）。
    pub fn core_config(&self) -> Config {
        Config { branch: self.branch.clone(), editor_command: self.editor_command.clone() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_config_path() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("创建临时目录");
        let path = dir.path().join("Application Support/config-sync/config.toml");
        (dir, path)
    }

    #[test]
    fn first_run_writes_defaults_and_reads_them_back() {
        let (_dir, path) = temp_config_path();
        assert!(!path.exists());

        let loaded = Settings::load_or_create(&path).expect("首次启动应成功");

        assert_eq!(loaded, Settings::default());
        assert_eq!(loaded.source_dir, "~/.config-sync");
        assert!(path.exists(), "首次启动要把默认 config.toml 落盘");
        assert_eq!(Settings::load_or_create(&path).expect("读回"), Settings::default());
    }

    #[test]
    fn custom_settings_round_trip_through_disk() {
        let (_dir, path) = temp_config_path();
        let settings = Settings {
            source_dir: "~/dotfiles".to_string(),
            editor_command: "code -w {}".to_string(),
            branch: "trunk".to_string(),
        };

        settings.save(&path).expect("写入");

        let text = std::fs::read_to_string(&path).expect("读盘");
        assert!(text.contains(r#"source_dir = "~/dotfiles""#), "{text}");
        assert!(text.contains(r#"branch = "trunk""#), "{text}");
        assert_eq!(Settings::load_or_create(&path).expect("读回"), settings);
        assert_eq!(
            settings.core_config(),
            Config { branch: "trunk".to_string(), editor_command: "code -w {}".to_string() }
        );
    }

    #[test]
    fn tilde_expands_against_home_and_absolute_paths_stay_put() {
        let home = Path::new("/Users/someone");
        for (raw, expected) in [
            ("~/.config-sync", "/Users/someone/.config-sync"),
            ("~", "/Users/someone"),
            ("~/dotfiles/config", "/Users/someone/dotfiles/config"),
            ("/opt/config-sync", "/opt/config-sync"),
        ] {
            let settings = Settings { source_dir: raw.to_string(), ..Settings::default() };
            assert_eq!(settings.resolved_source_dir(home), PathBuf::from(expected), "{raw}");
        }
    }

    #[test]
    fn broken_toml_reports_an_error_and_leaves_the_file_alone() {
        let (_dir, path) = temp_config_path();
        std::fs::create_dir_all(path.parent().expect("父目录")).expect("建父目录");
        std::fs::write(&path, "source_dir = [unclosed\n").expect("写坏文件");

        let err = Settings::load_or_create(&path).expect_err("坏 toml 必须报错");

        assert!(err.contains("config.toml"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&path).expect("读盘"),
            "source_dir = [unclosed\n",
            "不能覆盖用户已有的文件"
        );
    }
}
