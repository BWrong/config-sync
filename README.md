# config-sync

把 `$HOME` 下零散的配置文件托管在一个 git 仓库里（默认 `~/.config-sync`），并在多台机器之间搬运。
界面是一张**清单**：每个受管文件一行，显示源↔目标路径与状态。

技术栈与行为规则见 [DESIGN.md](./DESIGN.md)，术语见 [GLOSSARY.md](./GLOSSARY.md)，
镜像布局的理由见 [docs/adr/0001-mirror-directory-layout.md](./docs/adr/0001-mirror-directory-layout.md)。

## 目录

```
crates/core/          Rust Core（库）：清单、git 子进程、路径推导、clone。唯一测试 seam
  src/                lib.rs / config.rs / error.rs / git.rs / inventory.rs / paths.rs
  tests/fixtures/     真 git 测试夹具（临时 home + 临时源目录 + 临时裸远端）
  tests/inventory.rs  Core 的行为测试
  tests/clone.rs      clone 的行为测试
src-tauri/            Tauri 2 外壳：#[tauri::command] 只做转发
  src/settings.rs     config.toml 的读写与 `~` 展开
src/                  前端（Vite + TypeScript，无 UI 框架）
```

## 配置

`~/Library/Application Support/config-sync/config.toml`，**不进 git**；首次启动自动写入默认值：

```toml
source_dir = "~/.config-sync"
editor_command = ""        # 空 = 用系统关联 app；支持 `{}` 占位符
branch = "main"
```

首次启动（源目录里还没有仓库）会要求输入远端地址并 clone；clone 完停在清单，不写任何目标文件。

## 跑测试

```sh
cargo test                    # 只跑 Core（workspace 的 default-members）
cargo test -p config-sync-app # 外壳里的 config.toml 读写
cargo check --workspace       # Core + 外壳一起检查
```

测试不用 fake git、不用内存文件系统：每个用例在临时目录里建真 git 仓库与裸远端。
需要系统 `git` 在 `PATH` 上。

## 开发模式

```sh
npm install
npm run tauri dev             # 起 Vite（5173）+ 编译并按 devUrl 启动窗口
```

前端单独构建：

```sh
npm run build                 # tsc --noEmit + vite build → dist/
```

## 打包 macOS .app

```sh
npm run tauri build           # → target/release/bundle/macos/config-sync.app
```

只装了 Command Line Tools（没有完整 Xcode）也能出包；打包出的 `.app` 双击即可启动。
直接运行 `target/debug/config-sync-app`（不经 `.app`）在 macOS 上可能得到空白窗口，
开发请走 `npm run tauri dev`。

## 约定（后续实现者必读）

- **Core 是唯一 seam**：逻辑全在 `crates/core`，`#[tauri::command]` 只有三行转发。
- **Core 不读环境**：`Core::new(home, source_dir, config)` 注入一切；`$HOME` 由外壳读。
- **git 只走子进程**：`crates/core/src/git.rs` 是唯一入口，argv 数组 + 显式 `-C <源目录>`。
- **磁盘配置留在外壳**：`config.toml`（含 `source_dir`）只在 `src-tauri/src/settings.rs` 出现；
  注入 Core 的仍是 `Config { branch, editor_command }` 与解析后的 `source_dir`。
- **界面文案**用「清单 / 应用 / 推送 / 拉取 / 冲突」，不出现「同步」。
