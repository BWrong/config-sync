//! `config.toml` 的读写。
//!
//! 文件固定在 `~/.config/config-sync/config.toml`，不进 git。
//! 磁盘结构留在外壳：Core 只收到 `Config { branch, editor_command }` 与解析后的 `source_dir`，
//! 所以 `source_dir` 只在这里出现一次，Core 不读 env、不读 `$HOME`。

use std::path::{Path, PathBuf};

use config_sync_core::Config;
use serde::{Deserialize, Serialize};

/// 默认源目录；`~` 在 `resolved_source_dir` 里展开。
pub const DEFAULT_SOURCE_DIR: &str = "~/.local/share/config-sync";

/// app 自身的配置目录：`config.toml` 与向导标记都放这里，都不进 git。
pub fn app_dir(home: &Path) -> PathBuf {
    home.join(".config/config-sync")
}

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

    /// 去掉三字段两端的空白；用户粘贴路径时常见。
    pub fn trimmed(self) -> Settings {
        Settings {
            source_dir: self.source_dir.trim().to_string(),
            editor_command: self.editor_command.trim().to_string(),
            branch: self.branch.trim().to_string(),
        }
    }

    /// 保存前的校验。只检查「这份设置能不能指过去」，**不**做任何 git 操作。
    ///
    /// 空 `editor_command` 是合法的（用系统默认方式打开）；非空时必须带 `{}`，
    /// 否则编辑器拿不到文件路径。
    pub fn validate(&self, home: &Path) -> Result<(), String> {
        let raw = self.source_dir.trim();
        if raw.is_empty() {
            return Err("源目录不能为空".to_string());
        }
        if !raw.starts_with('~') && !Path::new(raw).is_absolute() {
            return Err(format!("源目录 {raw} 需要是绝对路径或以 ~ 开头"));
        }

        let resolved = self.resolved_source_dir(home);
        match std::fs::metadata(&resolved) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Err(format!("源目录 {} 不是目录", resolved.display())),
            Err(_) => {
                return Err(format!(
                    "源目录 {} 不存在，请先创建它（或到清单页用远端地址 clone 到那里）",
                    resolved.display()
                ));
            }
        }

        if self.branch.trim().is_empty() {
            return Err("分支不能为空".to_string());
        }

        let editor = self.editor_command.trim();
        if !editor.is_empty() && !editor.contains("{}") {
            return Err(format!(
                "编辑器命令 {editor} 里没有 {{}}，拿不到要打开的文件路径；写成 \"{editor} {{}}\" 这种形式，留空则用系统默认方式打开"
            ));
        }

        Ok(())
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
        let path = dir.path().join(".config/config-sync/config.toml");
        (dir, path)
    }

    #[test]
    fn first_run_writes_defaults_and_reads_them_back() {
        let (_dir, path) = temp_config_path();
        assert!(!path.exists());

        let loaded = Settings::load_or_create(&path).expect("首次启动应成功");

        assert_eq!(loaded, Settings::default());
        assert_eq!(loaded.source_dir, "~/.local/share/config-sync");
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
            ("~/.local/share/config-sync", "/Users/someone/.local/share/config-sync"),
            ("~", "/Users/someone"),
            ("~/dotfiles/config", "/Users/someone/dotfiles/config"),
            ("/opt/config-sync", "/opt/config-sync"),
        ] {
            let settings = Settings { source_dir: raw.to_string(), ..Settings::default() };
            assert_eq!(settings.resolved_source_dir(home), PathBuf::from(expected), "{raw}");
        }
    }

    #[test]
    fn source_dir_must_be_an_existing_directory() {
        let home = tempfile::tempdir().expect("临时 home");

        let missing = Settings { source_dir: "~/missing".to_string(), ..Settings::default() };
        let err = missing.validate(home.path()).expect_err("不存在的源目录必须报错");
        assert!(err.contains("不存在"), "{err}");
        assert!(err.contains("missing"), "错误里要有解析后的路径：{err}");

        let file = home.path().join("a-file");
        std::fs::write(&file, "x").expect("写文件");
        let not_a_dir =
            Settings { source_dir: file.display().to_string(), ..Settings::default() };
        let err = not_a_dir.validate(home.path()).expect_err("文件不是源目录");
        assert!(err.contains("不是目录"), "{err}");

        let dir = home.path().join("dotfiles");
        std::fs::create_dir(&dir).expect("建目录");
        let ok = Settings { source_dir: "~/dotfiles".to_string(), ..Settings::default() };
        assert_eq!(ok.validate(home.path()), Ok(()));
    }

    #[test]
    fn editor_command_is_optional_but_non_empty_must_carry_the_placeholder() {
        let home = tempfile::tempdir().expect("临时 home");

        let base = Settings { source_dir: "~".to_string(), ..Settings::default() };

        let empty = base.clone();
        assert_eq!(empty.validate(home.path()), Ok(()), "留空表示用系统默认方式打开");

        let with_placeholder = Settings { editor_command: "code -w {}".to_string(), ..base.clone() };
        assert_eq!(with_placeholder.validate(home.path()), Ok(()));

        let no_placeholder = Settings { editor_command: "code -w".to_string(), ..base };
        let err = no_placeholder.validate(home.path()).expect_err("没有 {} 收不到路径");
        assert!(err.contains("{}"), "{err}");
    }

    #[test]
    fn validate_rejects_empty_or_relative_source_dir_and_empty_branch() {
        let home = tempfile::tempdir().expect("临时 home");

        let empty = Settings { source_dir: "  ".to_string(), ..Settings::default() };
        assert!(empty.validate(home.path()).is_err(), "空源目录");

        let relative = Settings { source_dir: "dotfiles".to_string(), ..Settings::default() };
        let err = relative.validate(home.path()).expect_err("相对路径不接受");
        assert!(err.contains("绝对路径"), "{err}");

        let no_branch =
            Settings { source_dir: "~".to_string(), branch: " ".to_string(), ..Settings::default() };
        let err = no_branch.validate(home.path()).expect_err("空分支");
        assert!(err.contains("分支"), "{err}");
    }

    #[test]
    fn trimmed_normalizes_whitespace_without_touching_content() {
        let settings = Settings {
            source_dir: "  ~/dotfiles ".to_string(),
            editor_command: "  code -w {} ".to_string(),
            branch: " trunk ".to_string(),
        };

        assert_eq!(
            settings.trimmed(),
            Settings {
                source_dir: "~/dotfiles".to_string(),
                editor_command: "code -w {}".to_string(),
                branch: "trunk".to_string(),
            }
        );
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
