import { invoke } from "@tauri-apps/api/core";
import "./styles.css";

/** 与 Core 的 `InventoryEntry.status` 一一对应；中文文案只活在前端。 */
type Status =
  | "Conflict"
  | "PendingCommit"
  | "PendingPull"
  | "TargetMissing"
  | "InSync"
  | "Pending";

interface InventoryEntry {
  source: string;
  target: string;
  status: Status;
}

interface FileDiff {
  target: string;
  status: Status;
  /** unified diff 文本，空串 = 两侧没有差异。 */
  text: string;
}

interface RepoStatus {
  branch: string;
  ahead: number;
  behind: number;
  dirty: number;
}

interface CoreError {
  kind: "Git" | "Io";
  message: string;
  stderr: string | null;
}

interface Settings {
  source_dir: string;
  editor_command: string;
  branch: string;
}

interface SettingsView {
  settings: Settings;
  config_error: string | null;
  remote_url: string | null;
}

const STATUS_LABEL: Record<Status, string> = {
  Conflict: "冲突",
  PendingCommit: "待提交",
  PendingPull: "待拉取",
  TargetMissing: "目标缺失",
  InSync: "一致",
  Pending: "待应用",
};

const app = document.querySelector<HTMLDivElement>("#app")!;

function statusBar(status: RepoStatus): string {
  return `
    <header class="bar">
      <span class="brand">config-sync</span>
      <span class="chips">
        <span class="chip">分支 <b>${escapeHtml(status.branch)}</b></span>
        <span class="chip">↑${status.ahead} ↓${status.behind}</span>
        <span class="chip">未提交 <b>${status.dirty}</b></span>
      </span>
      <button class="nav" data-settings>设置</button>
    </header>`;
}

function rows(entries: InventoryEntry[]): string {
  return entries
    .map(
      (entry) => `
      <li class="row" data-target="${escapeHtml(entry.target)}">
        <button type="button" class="row-head" aria-expanded="false">
          <span class="paths">
            <code>${escapeHtml(entry.source)}</code>
            <span class="arrow">↔</span>
            <code>${escapeHtml(entry.target)}</code>
          </span>
          <span class="status status-${entry.status}">${STATUS_LABEL[entry.status]}</span>
        </button>
        <div class="diff"></div>
      </li>`,
    )
    .join("");
}

/** 点行展开该文件的 diff；diff 由 Core 生成，这里只按行着色。 */
async function toggleDiff(row: HTMLLIElement): Promise<void> {
  const head = row.querySelector<HTMLButtonElement>(".row-head")!;
  const slot = row.querySelector<HTMLDivElement>(".diff")!;
  if (row.classList.contains("open")) {
    row.classList.remove("open");
    head.setAttribute("aria-expanded", "false");
    return;
  }

  row.classList.add("open");
  head.setAttribute("aria-expanded", "true");
  if (slot.dataset.loaded === "true") return;

  slot.dataset.loaded = "true";
  slot.innerHTML = `<p class="diff-hint">正在读取差异…</p>`;
  try {
    const diff = await invoke<FileDiff>("diff", { target: row.dataset.target });
    slot.innerHTML = diff.text
      ? `<pre class="diff-text">${diffLines(diff.text)}</pre>`
      : `<p class="diff-hint">两侧内容相同，没有差异。</p>`;
  } catch (error) {
    slot.innerHTML = errorPanel(error);
  }
}

function diffLines(text: string): string {
  return text
    .split("\n")
    .map((line) => {
      const kind = /^(\+\+\+|---)/.test(line)
        ? "meta"
        : line.startsWith("@@")
          ? "hunk"
          : line.startsWith("+")
            ? "add"
            : line.startsWith("-")
              ? "del"
              : "ctx";
      return `<span class="diff-line diff-${kind}">${escapeHtml(line) || " "}</span>`;
    })
    .join("");
}

function errorPanel(error: unknown): string {
  const e = error as Partial<CoreError>;
  const detail = e.stderr ? `<pre>${escapeHtml(e.stderr)}</pre>` : "";
  return `<div class="error"><p>${escapeHtml(e.message ?? String(error))}</p>${detail}</div>`;
}

/** 顶部全局动作「拉取」：只更新源目录，不动任何目标文件；拉完重新推导清单。 */
async function pull(): Promise<void> {
  const button = app.querySelector<HTMLButtonElement>("[data-pull]")!;
  const slot = app.querySelector<HTMLDivElement>(".action-error")!;
  button.disabled = true;
  button.textContent = "正在拉取…";
  try {
    await invoke("pull");
    await renderInventory();
  } catch (error) {
    // 没有远端 / 没有上游 / 网络失败 / 无法快进：message 分类，stderr 原样展示。
    slot.innerHTML = errorPanel(error);
    button.disabled = false;
    button.textContent = "拉取";
  }
}

async function renderInventory(): Promise<void> {
  try {
    const [entries, status] = await Promise.all([
      invoke<InventoryEntry[]>("inventory"),
      invoke<RepoStatus>("repo_status"),
    ]);
    app.innerHTML = `
      ${statusBar(status)}
      <div class="toolbar">
        <button type="button" data-pull>拉取</button>
      </div>
      <div class="action-error"></div>
      <main class="inventory">
        ${
          entries.length === 0
            ? `<p class="empty">清单为空<small>源目录里还没有任何受管文件。</small></p>`
            : `<ul class="rows">${rows(entries)}</ul>`
        }
      </main>`;

    app.querySelector<HTMLUListElement>(".rows")?.addEventListener("click", (event) => {
      const head = (event.target as HTMLElement).closest(".row-head");
      const row = head?.closest<HTMLLIElement>(".row");
      if (row) void toggleDiff(row);
    });
    app.querySelector<HTMLButtonElement>("[data-pull]")?.addEventListener("click", () => void pull());
    bindSettingsEntry();
  } catch (error) {
    renderError(error);
  }
}

/** 错误面板 + 一个回到设置的入口：源目录指错时人不用重启应用。 */
function renderError(error: unknown): void {
  app.innerHTML = `
    ${errorPanel(error)}
    <div class="actions"><button class="nav" data-settings>设置</button></div>`;
  bindSettingsEntry();
}

function bindSettingsEntry(): void {
  app.querySelectorAll<HTMLButtonElement>("[data-settings]").forEach((button) => {
    button.addEventListener("click", () => void renderSettings());
  });
}

/** 设置面板：三字段 + 当前远端地址。保存只落盘，随后按新源目录重读清单。 */
async function renderSettings(): Promise<void> {
  let view: SettingsView;
  try {
    view = await invoke<SettingsView>("settings_view");
  } catch (error) {
    renderError(error);
    return;
  }

  const remote = view.remote_url
    ? `<code>${escapeHtml(view.remote_url)}</code>`
    : `<span class="muted">源目录还不是 git 仓库，或没有配置 origin</span>`;
  app.innerHTML = `
    <header class="bar">
      <span class="brand">config-sync</span>
      <span class="chips"><span class="chip">设置</span></span>
      <button class="nav" data-back>返回清单</button>
    </header>
    <main class="settings">
      ${
        view.config_error
          ? errorPanel({ message: `启动时读取设置失败，这次用默认值启动：${view.config_error}` })
          : ""
      }
      <form class="settings-form">
        <label>源目录
          <input name="source_dir" value="${escapeHtml(view.settings.source_dir)}"
            autocomplete="off" spellcheck="false" />
          <small>受管文件副本所在的 git 仓库，支持 <code>~</code> 开头。换它不会移动或删除旧目录里的任何文件。</small>
        </label>
        <label>编辑器命令
          <input name="editor_command" value="${escapeHtml(view.settings.editor_command)}"
            placeholder="code -w {}" autocomplete="off" spellcheck="false" />
          <small>留空表示用系统默认方式打开；非空时必须含 <code>{}</code> 作为文件路径占位。</small>
        </label>
        <label>分支
          <input name="branch" value="${escapeHtml(view.settings.branch)}"
            autocomplete="off" spellcheck="false" />
        </label>
        <p class="hint">当前远端：${remote}</p>
        <div class="actions">
          <button type="submit">保存</button>
          <button type="button" class="nav" data-back>取消</button>
        </div>
        <div class="settings-error"></div>
      </form>
    </main>`;

  const form = app.querySelector<HTMLFormElement>(".settings-form")!;
  const slot = app.querySelector<HTMLDivElement>(".settings-error")!;
  const submit = form.querySelector<HTMLButtonElement>("button[type=submit]")!;
  const field = (name: string) => form.querySelector<HTMLInputElement>(`input[name="${name}"]`)!;

  app.querySelectorAll<HTMLButtonElement>("[data-back]").forEach((button) => {
    button.addEventListener("click", () => void renderInventory());
  });
  field("source_dir").focus();

  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    const settings: Settings = {
      source_dir: field("source_dir").value,
      editor_command: field("editor_command").value,
      branch: field("branch").value,
    };

    slot.innerHTML = "";
    submit.disabled = true;
    submit.textContent = "正在保存…";
    try {
      await invoke("save_settings", { settings });
    } catch (error) {
      // 校验或落盘失败：停在表单上，人话 + 原始 stderr 都在 errorPanel 里。
      slot.innerHTML = errorPanel(error);
      submit.disabled = false;
      submit.textContent = "保存";
      return;
    }

    // 配置已落盘：按新源目录重读清单；新目录不是仓库时，错误面板里就是 git 的原话。
    await renderInventory();
  });
}

/** 首次启动或源目录还不是仓库：先要地址，clone 完停在清单。 */
function renderOnboarding(sourceDir: string): void {
  app.innerHTML = `
    <main class="onboarding">
      <h1>输入远端仓库地址</h1>
      <p class="hint">
        远端会 clone 到 <code>${escapeHtml(sourceDir)}</code>，clone 完停在清单，
        不会改动你 <code>$HOME</code> 下的任何文件。
      </p>
      <form class="clone-form">
        <input
          type="text"
          name="remote"
          placeholder="git@github.com:you/dotfiles.git"
          autocomplete="off"
          spellcheck="false"
        />
        <button type="submit">clone</button>
      </form>
      <div class="clone-error"></div>
      <div class="actions"><button class="nav" data-settings>设置</button></div>
    </main>`;

  const form = app.querySelector<HTMLFormElement>(".clone-form")!;
  const input = form.querySelector<HTMLInputElement>("input")!;
  const button = form.querySelector<HTMLButtonElement>("button")!;
  const slot = app.querySelector<HTMLDivElement>(".clone-error")!;
  input.focus();
  bindSettingsEntry();

  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    const remote = input.value.trim();
    if (!remote) {
      slot.innerHTML = errorPanel({ message: "请填写远端仓库地址" });
      input.focus();
      return;
    }

    slot.innerHTML = "";
    button.disabled = true;
    button.textContent = "正在 clone…";
    try {
      await invoke("clone_remote", { remote });
      await renderInventory();
    } catch (error) {
      // git 的原始 stderr 原样留在 errorPanel 里，改地址可以重试。
      slot.innerHTML = errorPanel(error);
      button.disabled = false;
      button.textContent = "clone";
      input.focus();
    }
  });
}

async function boot(): Promise<void> {
  try {
    const [hasRepo, sourceDir] = await Promise.all([
      invoke<boolean>("has_repo"),
      invoke<string>("source_dir"),
    ]);
    if (!hasRepo) {
      renderOnboarding(sourceDir);
      return;
    }
    await renderInventory();
    // 没有刷新按钮（fs watch 在 v0.3）：从终端切回来时重新推导一遍。
    window.addEventListener("focus", () => void renderInventory());
  } catch (error) {
    renderError(error);
  }
}

function escapeHtml(text: string): string {
  return text.replace(
    /[&<>"']/g,
    (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]!,
  );
}

app.innerHTML = `<p class="empty">正在读取清单…</p>`;
void boot();
