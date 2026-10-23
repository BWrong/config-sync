import { invoke } from "@tauri-apps/api/core";
import "./styles.css";

/** 与 Core 的 `InventoryEntry.status` 一一对应；中文文案只活在前端。 */
type Status = "InSync" | "TargetMissing" | "Pending";

interface InventoryEntry {
  source: string;
  target: string;
  status: Status;
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

const STATUS_LABEL: Record<Status, string> = {
  InSync: "一致",
  TargetMissing: "目标缺失",
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
    </header>`;
}

function rows(entries: InventoryEntry[]): string {
  return entries
    .map(
      (entry) => `
      <li class="row">
        <span class="paths">
          <code>${escapeHtml(entry.source)}</code>
          <span class="arrow">↔</span>
          <code>${escapeHtml(entry.target)}</code>
        </span>
        <span class="status status-${entry.status}">${STATUS_LABEL[entry.status]}</span>
      </li>`,
    )
    .join("");
}

function errorPanel(error: unknown): string {
  const e = error as Partial<CoreError>;
  const detail = e.stderr ? `<pre>${escapeHtml(e.stderr)}</pre>` : "";
  return `<div class="error"><p>${escapeHtml(e.message ?? String(error))}</p>${detail}</div>`;
}

async function renderInventory(): Promise<void> {
  try {
    const [entries, status] = await Promise.all([
      invoke<InventoryEntry[]>("inventory"),
      invoke<RepoStatus>("repo_status"),
    ]);
    app.innerHTML = `
      ${statusBar(status)}
      <main class="inventory">
        ${
          entries.length === 0
            ? `<p class="empty">清单为空<small>源目录里还没有任何受管文件。</small></p>`
            : `<ul class="rows">${rows(entries)}</ul>`
        }
      </main>`;
  } catch (error) {
    app.innerHTML = `${errorPanel(error)}`;
  }
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
    </main>`;

  const form = app.querySelector<HTMLFormElement>(".clone-form")!;
  const input = form.querySelector<HTMLInputElement>("input")!;
  const button = form.querySelector<HTMLButtonElement>("button")!;
  const slot = app.querySelector<HTMLDivElement>(".clone-error")!;
  input.focus();

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
  } catch (error) {
    app.innerHTML = `${errorPanel(error)}`;
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
