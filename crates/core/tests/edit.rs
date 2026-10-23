//! 编辑集成的行为测试：只测「打开什么、用什么 argv」，不真的拉起编辑器。
//!
//! 编辑对象永远是源目录里的受管副本（`home/<rel>`），不是 `$HOME` 里的目标文件；
//! 真正的进程拉起留给人工作冒烟。

mod fixtures;

use config_sync_core::{Config, Core, ErrorKind};
use fixtures::Fixture;

fn core_with(f: &Fixture, editor_command: &str) -> Core {
    Core::new(
        &f.home,
        &f.source_dir,
        Config { branch: "main".to_string(), editor_command: editor_command.to_string() },
    )
}

fn text(plan: &[config_sync_core::Attempt]) -> Vec<(String, Vec<String>, bool)> {
    plan.iter().map(|a| (a.program.clone(), a.args.clone(), a.wait)).collect()
}

#[test]
fn placeholder_becomes_the_source_path_and_a_path_with_spaces_stays_one_argument() {
    let f = Fixture::new();
    let source = f.write_source("home/.config/my app/config.toml", "x");
    let target = f.home.join(".config/my app/config.toml");
    let core = core_with(&f, "code -w --goto {}");

    let plan = core.edit_plan(&target).expect("编辑器命令能解析");

    // 只试一次：编辑器进程会一直活着，不等它退出。
    assert_eq!(
        text(&plan),
        vec![(
            "code".to_string(),
            vec!["-w".to_string(), "--goto".to_string(), source.display().to_string()],
            false
        )]
    );
    assert_ne!(source, target, "编辑的是源目录里的副本，不是 $HOME 里的目标文件");
}

#[test]
fn empty_editor_command_uses_open_and_falls_back_to_the_text_editor() {
    let f = Fixture::new();
    let source = f.write_source("home/.zshrc", "export A=1\n");
    let target = f.home.join(".zshrc");
    let core = core_with(&f, "");

    let plan = core.edit_plan(&target).expect("空命令 = 系统默认方式");

    // 第一步 `open`：系统关联 app；它非零退出（无扩展名文件判不出类型时就是）才试第二步。
    assert_eq!(
        text(&plan),
        vec![
            ("open".to_string(), vec![source.display().to_string()], true),
            (
                "open".to_string(),
                vec!["-t".to_string(), source.display().to_string()],
                true
            ),
        ]
    );
}

#[test]
fn editor_command_without_the_placeholder_is_a_config_error() {
    let f = Fixture::new();
    let source = f.write_source("home/.zshrc", "export A=1\n");
    let core = core_with(&f, "code -w");

    let error = core.edit_plan(&f.home.join(".zshrc")).expect_err("缺 {} 拿不到路径");

    assert_eq!(error.kind, ErrorKind::Io);
    assert!(error.message.contains("{}"), "{}", error.message);
    assert!(source.exists(), "解析失败不该动任何文件");
}

#[test]
fn editor_command_that_is_only_a_placeholder_is_rejected() {
    let f = Fixture::new();
    f.write_source("home/.zshrc", "export A=1\n");
    let core = core_with(&f, "{}");

    let error = core.edit_plan(&f.home.join(".zshrc")).expect_err("没有程序名可执行");
    assert!(error.message.contains("程序名"), "{}", error.message);
}

#[test]
fn missing_source_file_is_reported_instead_of_opening_an_empty_path() {
    let f = Fixture::new();
    let core = core_with(&f, "");

    // `$HOME` 里有、源目录里没有：清单已过期，不能把空路径交给编辑器。
    f.write_home(".zshrc", "export A=1\n");
    let error = core.edit_plan(&f.home.join(".zshrc")).expect_err("源文件不存在");

    assert!(error.message.contains("不存在"), "{}", error.message);
}

#[test]
fn paths_outside_home_are_rejected() {
    let f = Fixture::new();
    let core = core_with(&f, "");

    let error = core.edit_plan(&f.path("elsewhere/.zshrc")).expect_err("不在 $HOME 下没有受管文件");

    assert!(error.message.contains("不在"), "{}", error.message);
}
