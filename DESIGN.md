# config-sync 设计

术语见 [GLOSSARY.md](./GLOSSARY.md)；布局取舍见 [docs/adr/0001-mirror-directory-layout.md](./docs/adr/0001-mirror-directory-layout.md)。

## 定位

把 `$HOME` 下零散的配置文件集中到一个 git 仓库托管，并在多台机器之间搬运。核心价值是**看得见状态**：打开就知道哪些文件两边不一致。

**v1 明确不做**：模板与变量、加密、符号链接语义、目录权限编码、三方合并 UI、内置文本编辑器、拖拽、守护进程自动同步、多用户/多 `$HOME`。私钥类文件的同步要等加密（v0.3+），v1 直接拦。

## 技术栈

| 项 | 选择 | 理由 |
|---|---|---|
| 外壳 | Tauri 2（Rust 内核 + web 前端） | 产物 10–15MB；本机 Rust 1.97 + Node 24 + CLT 已就绪；不写死平台，将来可出 Windows |
| 平台 | 先只出 macOS arm64 | 现有工具链足够，无需安装 Xcode；出 Windows 版要动的改动见 [docs/windows.md](./docs/windows.md) |
| git | **shell 调用系统 `git`** | SSH key / credential helper / 代理 / host key 全部复用，不自建凭据存储 |
| 前端 | 纯 TypeScript + Vite，无 UI 框架 | 只有清单、diff、设置三类视图，框架带不来收益；`npm run build` 只有 `tsc --noEmit` + `vite build` |

凭据、代理、host key 校验一律不由本应用接管；git 报什么错，就原样解释给用户。

## 磁盘布局

**源目录**（默认 `~/.local/share/config-sync`，设置里可改）：

```
~/.local/share/config-sync/   ← git 工作区 = 源目录
├── .git/
└── home/                ← 镜像 $HOME
    ├── .zshrc           → $HOME/.zshrc
    ├── .gitconfig       → $HOME/.gitconfig
    └── .config/nvim/…   → $HOME/.config/nvim/…
```

`home/` 之外的任何路径都不参与同步（app 自身的元数据以后可以放这里，v1 不需要）。

远端地址不在 `config.toml` 里：它属于源目录仓库的 git 配置（`remote.origin.url`），设置界面直接读写它。

**app 配置**：`~/.config/config-sync/config.toml`，**不进 git**。

```toml
source_dir = "~/.local/share/config-sync"
editor_command = ""        # 空 = 用 `open`；支持 `{}` 占位符，如 "code -w {}"
branch = "main"
```

## 清单状态

每行 = 一个受管文件的 源路径 ↔ 目标路径 + 状态。状态由两两比较推导：

| 状态 | 含义 | 可执行动作 |
|---|---|---|
| `一致` | 源与目标内容相同 | 编辑、移除 |
| `目标缺失` | 源有，目标不存在 | 应用、移除 |
| `待应用` | 两边不同，且源侧无未提交改动 | 应用、编辑、移除 |
| `待提交` | 真实文件被改过，源目录落后（`git status` 有改动） | 推送（自动提交）、编辑、移除 |
| `待拉取` | 远端有新提交（fetch 后 ahead/behind 非 0 且可快进） | 拉取 |
| `冲突` | 源与远端同一文件都有改动，无法快进 | 保留本机 / 采用远端 |

状态划分的锚点是 **`git status`（源目录内部）+ 目标文件与源文件的逐文件比较**，不额外维护一份自己的元数据库——避免出现与 git 并存的第二份真相。

删除受管文件不会让行消失：源目录里已经删掉、git 里还有记录的行仍然列出，状态是「待提交」，路径划掉，直到推送把这次删除提交掉。这样删了几个文件，界面上仍能看见"还有东西要推"。

## 界面

**清单为主**：主视图就是上表那张列表；顶部一条远端状态（分支、ahead/behind、未提交改动数），本机与远端不一致时再加一行提示，直接说该推送还是该拉取；工具栏四个动作（添加文件 / 拉取 / 推送 / 应用）；点行展开 diff（源 ↔ 目标，或 HEAD ↔ 源）。

每行左边是源目录里的副本（`home/…`），右边是它落到 `$HOME` 的位置（`~/…`），源目录的长前缀不重复占位。状态同时用图标和颜色表示，文字名放在悬停提示里，清单底部给一条常驻图例——六种图标不看图例认不全。行内右侧是操作（编辑 / 移除）。六个原始功能全部归位为：行内操作（编辑、移除、应用）+ 工具栏（添加、拉取、推送）+ 设置。

## 关键行为规则

1. **应用 = 先看再写**。点"应用"先出 diff 预览，确认后才写盘；不做静默覆盖。预览里不写的项折在"跳过 N 个"下，各带一个原因：内容已一致、源目录里没有这个副本、源目录还有未提交改动、源与远端冲突。目标文件已存在但仓库里没有它时，拒绝写。
2. **添加**：只从文件选择器来（不接受粘贴路径），路径相对 `$HOME` 推导源目录位置。首次使用时向导可列出候选（`.zshrc`、`.gitconfig`、`.config/*`、`.ssh/config` 等）勾选，**但不做成常驻推荐**——避免把 history、私钥塞进仓库。
   **移除**与它对称：只删源目录里的副本，`$HOME` 下的原文件保持原样，删除作为待提交的改动留在 git 里。
3. **推送前硬拦**（命中即拒绝，无"强行继续"）：
   - 路径黑名单：`**/id_rsa`、`**/id_ed25519*`、`**/.ssh/id_*`、`**/*.pem`、`**/*.p12`、`**/.netrc`、`**/*_history`、`**/credentials*`、`**/.aws/credentials`
   - 内容正则：`-----BEGIN [A-Z ]*PRIVATE KEY-----`、`ghp_`、`github_pat_`、`AKIA`、`xox[baprs]-`、`sk-[A-Za-z0-9_-]{16,}`（16 位以上才算命中，避免误伤普通文本）
   - 文案固定为："私钥/凭据类文件需要先加密才能同步，加密在 v0.3 提供。" 理由：推到公开仓库的密钥不可撤销，force push 也救不回缓存与 fork。
4. **拉取一律 `--ff-only`**：不允许产生静默 merge commit。冲突时按文件二选一（`git checkout --ours/--theirs` 后重新提交）。
5. **clone 后不自动应用**，与"拉取不自动应用"同一条规则。新机器上清单本身就是最好的说明。
6. **编辑**：默认 `open <目标文件>`（系统关联 app）；无关联或返回非零时回退 `open -t`；`editor_command` 可覆盖。编辑器拉起即算成功，不等它退出。另有 fs watch 盯着源目录的 `home/` 子树，保存后 300ms 内合并事件、推一次刷新，界面上不设"刷新"按钮。

## 里程碑

**v0.1 只读闭环**：设置仓库地址 + clone；清单视图（状态 + diff）；添加；应用（预览→写盘）。
完成即"能在新机器上把配置铺回来"，且只有 仓库→磁盘 一个方向。

**v0.2 写入闭环**：推送（含安全扫描）、拉取（`--ff-only`）、冲突按文件二选一。核心功能全齐。

**v0.3 顺手部**：编辑集成（`open` + fs watch）、设置界面（源目录、远端地址、编辑器命令、分支）、欢迎向导的候选勾选、加密（让私钥类文件可同步）。

## 未拍板

- 产品名：**`ConfigSync`**（显示名，大驼峰）。内部标识保持小写连字符：crate `config-sync-core` / `config-sync-app`、`package.json` 的 `name`、bundle identifier `com.bwrong.config-sync`、提交信息前缀 `config-sync:`。

已定的（原列在此处）：自动提交的 message 为 `config-sync: N files (YYYY-MM-DD HH:MM)`；前端是纯 TypeScript + Vite；diff 在 Core 里用 `similar` 生成 unified 格式，不调 `git diff --no-index`。
