# config-sync

把 `$HOME` 下零散的配置文件托管在一个 git 仓库里（默认 `~/.local/share/config-sync`），并在多台机器之间搬运。
界面是一张**清单**：每个受管文件一行，显示源↔目标路径与状态。

技术栈与行为规则见 [DESIGN.md](./DESIGN.md)，术语见 [GLOSSARY.md](./GLOSSARY.md)，
镜像布局的理由见 [docs/adr/0001-mirror-directory-layout.md](./docs/adr/0001-mirror-directory-layout.md)，
出 Windows 版要动的地方见 [docs/windows.md](./docs/windows.md)。

## 目录

```
crates/core/          Rust Core（库）：清单、git 子进程、路径推导、六态判定、增删改应用推送拉取。唯一测试 seam
  src/                lib.rs（Core 门面）+ add / apply / config / editor / error / git /
                      inventory / onboarding / paths / push / remove / resolve / state
  tests/fixtures/     真 git 测试夹具（临时 home + 临时源目录 + 临时裸远端）
  tests/*.rs          按功能一块：add / apply / clone / diff / edit / inventory /
                      onboarding / pull / push / remove / resolve / status
src-tauri/            Tauri 2 外壳：#[tauri::command] 只做转发
  src/settings.rs     config.toml 的读写与 `~` 展开
  src/watch.rs        源目录 fs watch（300ms 防抖）→ 前端重读清单
  src/onboarding.rs   首次向导的完成标记
src/                  前端（Vite + TypeScript，无 UI 框架）
```

## 配置

`~/.config/config-sync/config.toml`，**不进 git**；首次启动自动写入默认值：

```toml
source_dir = "~/.local/share/config-sync"
editor_command = ""        # 空 = 用系统关联 app；支持 `{}` 占位符
branch = "main"
```

远端地址不存在这里：它属于源目录仓库的 git 配置（`remote.origin.url`），在设置界面里直接改，保存时走 `git remote set-url`（没有 origin 就 `remote add`）。

首次启动（源目录里还没有仓库）会要求输入远端地址并 clone；clone 完停在清单，不写任何目标文件。

## 跑测试

```sh
cargo test                    # Core + 外壳，整个 workspace
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

## 打包 macOS

```sh
npm run tauri build           # → target/release/bundle/{macos/ConfigSync.app, dmg/ConfigSync_<版本>_aarch64.dmg}
```

只装了 Command Line Tools（没有完整 Xcode）也能出包；打包出的 `.app` 双击即可启动。
直接运行 `target/debug/config-sync-app`（不经 `.app`）在 macOS 上可能得到空白窗口，
开发请走 `npm run tauri dev`。

## 发版

版本号有两个地方，必须一致：`package.json` 与 `src-tauri/tauri.conf.json` 的 `version`。

```sh
# 1. 改版本号（两处一起），提交
git add package.json src-tauri/tauri.conf.json
git commit -m "chore(release): 🏷️ 升到 v0.2.0"

# 2. 打 tag 并推——tag 名是 v + 完整版本号，对不上 workflow 会直接失败
git push origin main
git tag v0.2.0 && git push origin v0.2.0
```

推 tag 之后 `.github/workflows/release.yml` 会：跑 `cargo test --workspace`（红了就不出包）
→ 打 `ConfigSync_<版本>_aarch64.dmg` → 建一个**草稿** Release（更新日志由 GitHub 生成，
dmg 作为附件，正文末尾追加安装说明）。

最后一步是人工的：

```sh
gh release view v0.2.0 --json body -q .body   # 看一眼日志
gh release edit v0.2.0 --draft=false         # 或者到网页上点 Publish
```

自动日志靠 PR 标题抓，直接推 main 的 commit 抓不到；要重发就先
`gh release delete v0.2.0 --yes` 删掉草稿、删掉 tag，再重新打。
完整步骤见 `.github/workflows/release.yml`。

包目前**没有签名也没有公证**，首次打开会被 Gatekeeper 拦一次，Release 正文里已经写了
两种绕法。要用户双击即开，需要 Apple Developer Program（$99/年）的 Developer ID，
届时把构建步骤换成 `tauri-apps/tauri-action` 并配上签名 secrets 即可。

## 约定（后续实现者必读）

- **Core 是唯一 seam**：逻辑全在 `crates/core`，`#[tauri::command]` 只有三行转发。
- **Core 不读环境**：`Core::new(home, source_dir, config)` 注入一切；`$HOME` 由外壳读。
- **git 只走子进程**：`crates/core/src/git.rs` 是唯一入口，argv 数组 + 显式 `-C <源目录>`。
- **磁盘配置留在外壳**：`config.toml`（含 `source_dir`）只在 `src-tauri/src/settings.rs` 出现；
  注入 Core 的仍是 `Config { branch, editor_command }` 与解析后的 `source_dir`。
- **界面文案**用「清单 / 应用 / 推送 / 拉取 / 冲突」，不出现「同步」。
