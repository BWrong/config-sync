//! 编辑集成：把 `editor_command` / 系统 `open` 变成一个可执行的 argv 序列。
//!
//! 只构造 argv 与尝试顺序，进程拉起见 `Attempt::run`（那部分不进测试：会真的弹出编辑器）。

use std::path::Path;

use crate::CoreError;

/// 一次拉起尝试：程序 + argv（`args` 不含程序名本身）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt {
    pub program: String,
    pub args: Vec<String>,
    /// true = 等它退出、用退出码判成败（`open` 是启动器，立刻返回）；
    /// false = 拉起即算成功（编辑器进程会一直活着，不等）。
    pub wait: bool,
}

impl Attempt {
    /// 真正拉起进程。编辑器不等退出；`open` 等退出码，好决定要不要回退。
    pub(crate) fn run(&self) -> Result<(), CoreError> {
        let mut command = std::process::Command::new(&self.program);
        command.args(&self.args);
        let started = |e: std::io::Error| {
            CoreError::io(format!("启动 {} 失败：{e}", self.program))
        };

        if !self.wait {
            command.spawn().map_err(started)?;
            return Ok(());
        }

        let output = command.output().map_err(started)?;
        if output.status.success() {
            return Ok(());
        }
        Err(CoreError::io(format!(
            "{} {} 没有成功（{}）",
            self.program,
            self.args.join(" "),
            output.status
        )))
    }
}

/// 在 Finder 里打开一个目录（源目录的「打开目录」按钮）。
///
/// 目录不存在就报错，不拉起一个空的 Finder 窗口。
pub fn reveal(dir: &Path) -> Result<(), CoreError> {
    if !dir.is_dir() {
        return Err(CoreError::io(format!(
            "目录 {} 不存在；源目录还没建好，先克隆或添加一个文件",
            dir.display()
        )));
    }
    std::process::Command::new("open")
        .arg(dir)
        .spawn()
        .map_err(|e| CoreError::io(format!("打开 {} 失败：{e}", dir.display())))?;
    Ok(())
}

/// 打开 `path` 的尝试序列，按顺序试到成功为止。
///
/// - `editor_command` 非空：按空白切词，含 `{}` 的词里把占位换成路径。
///   命令里没有 `{}` 就拿不到路径，是配置错误（设置面板已先拦一次，这里兜底）。
/// - 空：`open <path>`，它非零退出再 `open -t <path>` —— 无扩展名文件（`.zshrc`）
///   系统判不出类型时 `open` 会失败，`-t` 强制用文本编辑器，不弹「选择应用」。
pub fn plan(editor_command: &str, path: &Path) -> Result<Vec<Attempt>, CoreError> {
    let target = path.display().to_string();
    let editor = editor_command.trim();
    if editor.is_empty() {
        return Ok(vec![
            Attempt { program: "open".to_string(), args: vec![target.clone()], wait: true },
            Attempt { program: "open".to_string(), args: vec!["-t".to_string(), target], wait: true },
        ]);
    }

    let mut words: Vec<String> = editor.split_whitespace().map(str::to_string).collect();
    let mut replaced = false;
    for word in &mut words {
        if word.contains("{}") {
            *word = word.replace("{}", &target);
            replaced = true;
        }
    }
    if !replaced {
        return Err(CoreError::io(format!(
            "编辑器命令 {editor} 里没有 {{}}，拿不到要打开的文件路径；写成 \"{editor} {{}}\" 这种形式，留空则用系统默认方式打开"
        )));
    }
    if words.len() == 1 {
        return Err(CoreError::io(format!(
            "编辑器命令 {editor} 里只有占位符，没有要执行的程序名；写成 \"code {{{{}}}}\" 这种形式"
        )));
    }

    let program = words.remove(0);
    Ok(vec![Attempt { program, args: words, wait: false }])
}
