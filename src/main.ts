import { invoke } from "@tauri-apps/api/core";
import "./styles.css";

/** 与 Core 的 `InInventoryEntry.status` 一一对应；中文文案只活在前端。 */
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

async function render(): Promise<void> {
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

function escapeHtml(text: string): string {
  return text.replace(
    /[&<>"']/g,
    (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]!,
  );
}

app.innerHTML = `<p class="empty">正在读取清单…</p>`;
void render();
