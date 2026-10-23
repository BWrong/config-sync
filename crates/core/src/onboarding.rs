//! 首次使用向导的候选清单。
//!
//! 固定的一组常见配置文件，逐个标出本机是否存在；勾选后由外壳原样交给 `add`，
//! 所以这里只管「推荐哪些路径」，复制与路径校验仍然只有 `add` 一条路。
//! 候选只在向导里出现一次，不是常驻推荐。

use std::path::Path;

use serde::Serialize;

use crate::Core;

/// 固定候选（相对 `$HOME` 的镜像路径），按这个顺序显示。
///
/// 刻意不收历史记录与凭据类文件：`is_sensitive` 会再拦一道，即使以后有人往这里加。
const CANDIDATES: &[&str] = &[
    ".zshrc",
    ".zshenv",
    ".gitconfig",
    ".config/nvim/init.lua",
    ".config/starship.toml",
    ".config/gh/config.yml",
    ".ssh/config",
    ".tmux.conf",
    ".vimrc",
];

/// 绝不推荐给用户的路径：历史记录、`~/.ssh/id_*` 私钥、含凭据字样的文件。
fn is_sensitive(rel: &Path) -> bool {
    let text = rel.to_string_lossy().to_lowercase();
    let name = rel.file_name().map(|name| name.to_string_lossy().to_lowercase()).unwrap_or_default();
    let under_ssh = rel.components().any(|component| component.as_os_str() == ".ssh");

    name.ends_with("_history")
        || (under_ssh && name.starts_with("id_"))
        || ["credential", "token", "secret"].iter().any(|needle| text.contains(needle))
}

/// 向导里的一行候选。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Candidate {
    /// `~/…` 形式，同时用于显示与交给 `add`（Core 按注入的 `$HOME` 展开）。
    pub path: String,
    /// 本机是否真的存在这个文件；不存在的只能显示、不能勾选。
    pub exists: bool,
}

impl Core {
    /// 首次使用向导的候选：固定清单，逐个标出本机是否存在。
    pub fn onboarding_candidates(&self) -> Vec<Candidate> {
        CANDIDATES
            .iter()
            .filter(|rel| !is_sensitive(Path::new(rel)))
            .map(|rel| Candidate {
                path: format!("~/{rel}"),
                exists: self.home.join(rel).is_file(),
            })
            .collect()
    }
}
