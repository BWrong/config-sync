# 出 Windows 版要动的地方

[DESIGN.md](../DESIGN.md) 的定位是"先只出 macOS arm64"。这份清单是决定要出 Windows 版之后的改动目录，按"改完哪一步能干什么"分三档：**能启动 → 数据可信 → 能发版**。

每条形如「现象 → 位置 → 改法」。方框是进度，不是承诺。

## 第一档：不改就跑不起来

- [ ] **`$HOME` 来源**
  现象：Windows 默认没有 `HOME`（只有 Git Bash 会设），`AppState::from_environment()` 里那句 `.expect("HOME 未设置，无法定位 $HOME")` 直接 panic——启动第一个炸点。
  位置：`src-tauri/src/lib.rs`
  改法：`USERPROFILE` 兜底，或引入 `dirs` crate 统一取家目录。

- [ ] **默认目录是 XDG 风格**
  现象：`DEFAULT_SOURCE_DIR = "~/.local/share/config-sync"` 与 `app_dir() = home/.config/config-sync` 在 Windows 上不合惯例。
  位置：`src-tauri/src/settings.rs`
  改法：`cfg(windows)` 分叉成 `%LOCALAPPDATA%\config-sync`（源目录）与 `%APPDATA%\config-sync`（app 配置）。`settings.rs` 里那批测试的路径断言要跟着分平台。

- [ ] **打开文件的命令是 macOS 专用**
  现象：`open <path>` 与回退 `open -t <path>` 在 Windows 上不存在，编辑按钮直接报"启动 open 失败"。
  位置：`crates/core/src/editor.rs` 的 `plan()`
  改法：Windows 分支用 `cmd /C start "" <path>`（`""` 必须留，否则路径会被当成窗口标题），回退 `notepad`。`editor_command` 那条路径不用动。

- [ ] **前端路径缩写全部失效**
  现象：`mirrorRel()` 用 `${sourceDir}/home/` 做 `startsWith`，后端给的是 `C:\Users\x\...`，比较失败后 `home/…` ↔ `~/…` 两列退化成整条绝对路径。**不报错，静默退化**，是最容易被漏掉的一条。
  位置：`src/main.ts` 的 `mirrorRel()` / `shortTarget()`
  改法：优先在后端序列化时统一成正斜杠（`inventory` / `apply` 的路径字段），前端不动；或者在 `escapeHtml` 前统一 `replace(/\\/g, "/")`。

## 第二档：能跑，但会出错

- [ ] **`git show :<路径>` 的分隔符**
  位置：`crates/core/src/push.rs` 的 `contents()`
  改法：`repo_path.display()` 在 Windows 给反斜杠，git 内部路径永远用 `/`。改成 `to_string_lossy().replace('\\', "/")`。

- [ ] **推送硬拦的路径规则静默失效**（安全相关，必须有测试）
  位置：`crates/core/src/push.rs` 的 `path_rule_matches()` / `glob_segment_matches()`
  现状：按 `'/'` 切段，而输入来自 `git status --porcelain -z`（git 稳定给 `/`），逻辑本身没错。
  改法：保证进函数前不经过 `PathBuf::display()`；补一条用反斜杠路径喂进去的测试，钉住"必须命中"。

- [ ] **盘符与大小写**
  现象：Windows 路径大小写不敏感、`C:` 与 `c:` 不同，`strip_prefix` 会把同一文件判成"不在 $HOME 下"。
  位置：`crates/core/src/lib.rs` 的 `mirror_relative_path()`
  改法：先 canonicalize 再 strip，或做大小写不敏感比较。

- [ ] **写盘方式**
  现象：`copy → set_permissions → rename` 在目标文件被编辑器占用时 `rename` 会 `ACCESS_DENIED`（Unix 的 rename 不怕这个）。
  位置：`crates/core/src/apply.rs` 的写盘函数
  改法：错误文案要点明"关掉占用它的程序再试"；`set_permissions` 在 Windows 只有只读位有意义，需要 `cfg` 保留或跳过。

- [ ] **fs watch 的行为差异**
  位置：`src-tauri/src/watch.rs`
  说明：`notify` 本身跨平台，但 Windows 编辑器保存多是 rename-replace，事件类型与粒度不同，300ms 防抖要在一台真机上复测。

- [ ] **长路径**
  超过 260 字符需要 `\\?\` 前缀，或在 app manifest 里开 `longPathAware`。

## 第三档：能发版

- [ ] **向导候选在 Windows 上全是"不存在"**
  位置：`crates/core/src/onboarding.rs` 的 `CANDIDATES`
  改法：按平台给候选——`_vimrc`、`Documents\PowerShell\Microsoft.PowerShell_profile.ps1`、`AppData\Roaming\nvim\init.lua`、`AppData\Roaming\starship.toml`。同时 `is_sensitive()` 的 `*_history` 认不出 `ConsoleHost_history.txt`。

- [ ] **打包配置只出 macOS**
  位置：`src-tauri/tauri.conf.json`
  改法：`bundle.targets` 加 `"nsis"`（或 `"msi"`），图标补 `icons/icon.ico`。

- [ ] **测试夹具里的 unix 假设**
  位置：`crates/core/tests/add.rs`（用 `mkfifo` 造管道，需要 `#[cfg(unix)]`）、`crates/core/tests/fixtures/mod.rs` 的 `file_url()`（Windows 上要是 `file:///C:/...`，不能是 `file://C:\...`）。

- [ ] **CI**
  加 `windows-latest` 跑 `cargo test --workspace`。上面每一条都属于"不跑就发现不了"的类型。

## 已经不用改的

Tauri 2 本身跨平台；`crates/core/src/git.rs` 走 argv 数组、不拼 shell（没有引号地狱）；镜像布局用 `PathBuf` 而非字符串拼接；七态判定、diff、resolve、remove 全是纯路径逻辑；代码里没有直接调用 `PermissionsExt` / `libc` / `std::os::unix`。

## 顺序与预估

| 档 | 目标 | 估时 | 风险 |
|---|---|---|---|
| 一 | 能启动、清单能显示 | 半天到一天 | 低 |
| 二 | 写盘与硬拦可信 | 一到两天 | **中高**：路径规则与盘符大小写是静默失效 |
| 三 | 能装、能发 | 一天 | 低 |

合计 3–4 天。建议先一次性做完第一档，用一台 Windows 跑 `cargo test --workspace`，剩下的问题会自己浮出来；再动 editor / apply / watch 这些行为差异。

## 验收

1. `cargo test --workspace` 在 Windows 上全绿（当前 89 个）。
2. `npm run tauri dev` 手点五条主路径：clone → 添加 → 应用 → 推送 → 拉取，外加冲突行的两个按钮。
3. 故意让一个目标文件被别的程序占用，确认应用失败时给的是人话而不是 `os error 32`。
4. 故意把 `.ssh/id_rsa` 放进源目录，确认推送仍被硬拦（这条错一次就是泄密）。
