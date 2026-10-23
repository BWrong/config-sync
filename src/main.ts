import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
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

/** 与 Core 的 `PushOutcome` 一一对应。 */
interface PushOutcome {
  /** false = 没有需要推送的改动。 */
  committed: boolean;
  files: number;
  message: string;
  remote_url: string;
}

interface CoreError {
  kind: "Git" | "Io";
  message: string;
  stderr: string | null;
}

/** 与 Core 的 `AddResult` 一一对应：单元变体是字符串，`Rejected` 带原因。 */
type AddResult =
  | "Added"
  | "AlreadyManaged"
  | "AlreadyManagedDiffers"
  | { Rejected: { reason: string } };

interface AddOutcome {
  /** 被添加的路径；目录会展开成其中每个文件。 */
  target: string;
  result: AddResult;
}

/** 与 Core 的 `ApplyAction` 一一对应；中文文案只活在前端。 */
type ApplyAction = "Create" | "Overwrite" | "Skip";

interface ApplyItem {
  /** 会被写到的目标路径。 */
  target: string;
  action: ApplyAction;
  reason: string;
  /** 应用会带来的改动（目标 → 源），空串 = 没有差异。 */
  diff: string;
}

interface ApplyPlan {
  items: ApplyItem[];
}

interface ApplySummary {
  create: number;
  overwrite: number;
  skip: number;
}

/** `plan_apply` 回来的一整份计划 + 摘要；确认时把同一份 `plan` 原样送回去执行。 */
interface ApplyPreview {
  plan: ApplyPlan;
  summary: ApplySummary;
}

/** 与 Core 的 `ApplyResult` 一一对应：`Failed` 带人话原因。 */
type ApplyResult = "Written" | { Failed: { message: string } };

interface ApplyOutcome {
  target: string;
  result: ApplyResult;
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

/** 向导里的一行候选；`path` 是 `~/…` 形式，勾选后直接交给 `add`。 */
interface Candidate {
  path: string;
  exists: boolean;
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

/** 上一次添加的结果（或报错）。放进 renderInventory 的模板里，重渲染不会丢。 */
let addReportHtml = "";

/** 上一次推送的结果（或硬拦/失败报错），同上。 */
let pushReportHtml = "";

/** 待确认的应用计划；非 null 时界面显示预览页，确认后执行的就是这一份。 */
let applyPlan: ApplyPlan | null = null;
let applySummary: ApplySummary | null = null;

/** 上一次应用的结果（或报错），同样放进清单模板里。 */
let applyReportHtml = "";

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
        <button type="button" class="row-edit" data-edit title="打开源目录里的副本">编辑</button>
        <div class="diff"></div>
      </li>`,
    )
    .join("");
}

/** 「编辑」：用系统打开方式（或设置里的编辑器命令）打开源目录里的副本。
 *  拉起即返回，保存后的状态变化由 fs watch 推过来，这里不轮询。 */
async function editRow(row: HTMLLIElement, button: HTMLButtonElement): Promise<void> {
  const slot = row.querySelector<HTMLDivElement>(".diff")!;
  button.disabled = true;
  try {
    await invoke("edit", { target: row.dataset.target });
  } catch (error) {
    // 编辑器命令配错、源文件已被删：把原因摆在这一行里，别让人以为点了没反应。
    row.classList.add("open");
    slot.dataset.loaded = "false";
    slot.innerHTML = errorPanel(error);
  } finally {
    button.disabled = false;
  }
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
  pushReportHtml = "";
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

async function pushChanges(): Promise<void> {
  const button = app.querySelector<HTMLButtonElement>("[data-push]")!;
  button.disabled = true;
  button.textContent = "正在推送…";
  try {
    const outcome = await invoke<PushOutcome>("commit_and_push");
    pushReportHtml = outcome.committed
      ? `<p class="add-summary">已推送 ${outcome.files} 个文件到 <code>${escapeHtml(
          outcome.remote_url,
        )}</code>：${escapeHtml(outcome.message)}</p>`
      : `<p class="add-summary">没有需要推送的改动。</p>`;
  } catch (error) {
    pushReportHtml = errorPanel(error);
  }
  // 无论成败都重读：成功后 ahead 归零、清单转「一致」；失败时状态条也要反映已提交的现状。
  await renderInventory();
}

const ACTION_LABEL: Record<ApplyAction, string> = { Create: "新建", Overwrite: "覆盖", Skip: "跳过" };

/** 「应用」第一步：向 Core 要一份完整计划，只预览，一个字节都不写。 */
async function startApply(): Promise<void> {
  const button = app.querySelector<HTMLButtonElement>("[data-apply]");
  if (button) {
    button.disabled = true;
    button.textContent = "正在计算…";
  }
  try {
    const preview = await invoke<ApplyPreview>("plan_apply", { targets: null });
    applyPlan = preview.plan;
    applySummary = preview.summary;
    applyReportHtml = "";
    await renderInventory();
  } catch (error) {
    app.querySelector<HTMLDivElement>(".action-error")!.innerHTML = errorPanel(error);
    if (button) {
      button.disabled = false;
      button.textContent = "应用";
    }
  }
}

/** 预览页：每个受管文件一条，带动作、原因与 diff；确认后写盘。 */
function renderApplyPreview(): void {
  const items = applyPlan!.items;
  const summary = applySummary!;
  const writes = summary.create + summary.overwrite;
  app.innerHTML = `
    <header class="bar">
      <span class="brand">config-sync</span>
      <span class="chips">
        <span class="chip">新建 <b>${summary.create}</b></span>
        <span class="chip">覆盖 <b>${summary.overwrite}</b></span>
        <span class="chip">跳过 <b>${summary.skip}</b></span>
      </span>
      <button class="nav" data-settings>设置</button>
    </header>
    <main class="inventory apply-preview">
      <h2>应用预览</h2>
      <p class="apply-hint">确认后会把下面 ${writes} 个文件写到 $HOME；跳过的项不动。</p>
      <div class="action-error"></div>
      ${
        items.length === 0
          ? `<p class="empty">清单为空<small>源目录里还没有任何受管文件。</small></p>`
          : `<ul class="apply-items">${items.map(applyItemHtml).join("")}</ul>`
      }
      <div class="apply-actions">
        <button type="button" class="confirm-apply" ${writes === 0 ? "disabled" : ""}>确认应用</button>
        <button type="button" class="cancel-apply">取消</button>
      </div>
    </main>`;

  app.querySelector<HTMLButtonElement>(".confirm-apply")?.addEventListener("click", () => void confirmApply());
  app.querySelector<HTMLButtonElement>(".cancel-apply")?.addEventListener("click", () => void cancelApply());
  bindSettingsEntry();
}

function applyItemHtml(item: ApplyItem): string {
  const diff = item.diff ? `<pre class="apply-diff">${diffLines(item.diff)}</pre>` : "";
  return `
    <li class="apply-item apply-${item.action}">
      <div class="apply-head">
        <code>${escapeHtml(item.target)}</code>
        <span class="status">${ACTION_LABEL[item.action]}</span>
      </div>
      <p class="apply-reason">${escapeHtml(item.reason)}</p>
      ${diff}
    </li>`;
}

/** 「应用」第二步：把预览里那一份计划原样送回去执行，不重新计算动作。 */
async function confirmApply(): Promise<void> {
  if (!applyPlan) return;
  const button = app.querySelector<HTMLButtonElement>(".confirm-apply")!;
  button.disabled = true;
  button.textContent = "正在写…";
  try {
    applyReportHtml = applyReport(await invoke<ApplyOutcome[]>("execute_apply", { plan: applyPlan }));
  } catch (error) {
    applyReportHtml = errorPanel(error);
  }
  applyPlan = null;
  applySummary = null;
  await renderInventory();
}

async function cancelApply(): Promise<void> {
  applyPlan = null;
  applySummary = null;
  await renderInventory();
}

/** 应用结果的汇总：写成功的计数 + 没写成的逐条原因。 */
function applyReport(outcomes: ApplyOutcome[]): string {
  const written = outcomes.filter((outcome) => outcome.result === "Written").length;
  const failed = outcomes.length - written;
  const notes = outcomes.flatMap((outcome) => {
    const result = outcome.result;
    if (result === "Written") return [];
    return [
      `<li><code>${escapeHtml(outcome.target)}</code> 没有写：${escapeHtml(
        result.Failed.message,
      )}</li>`,
    ];
  });
  return `<p class="apply-summary">已写 ${written} 个文件${failed ? ` · ${failed} 个没有写` : ""}</p>${notes.length ? `<ul class="apply-notes">${notes.join("")}</ul>` : ""}`;
}

async function renderInventory(): Promise<void> {
  try {
    // 有未确认的计划时，整个页面就是预览页：这一屏不写盘。
    if (applyPlan) {
      renderApplyPreview();
      return;
    }
    const [entries, status] = await Promise.all([
      invoke<InventoryEntry[]>("inventory"),
      invoke<RepoStatus>("repo_status"),
    ]);
    app.innerHTML = `
      ${statusBar(status)}
      <div class="toolbar">
        <button type="button" data-pull>拉取</button>
        <button type="button" data-push>推送</button>
        <button type="button" data-apply>应用</button>
      </div>
      <div class="action-error">${pushReportHtml}</div>
      <main class="inventory">
        <div class="add-bar">
          <button type="button" class="add-files">添加文件…</button>
          <details class="add-paste">
            <summary>粘贴路径</summary>
            <form class="add-paths">
              <textarea name="paths" rows="3" spellcheck="false"
                placeholder="一行一个路径，文件或整棵目录；支持 ~ 开头"></textarea>
              <button type="submit">添加</button>
            </form>
          </details>
        </div>
        <div class="add-report">${addReportHtml}</div>
        <div class="apply-report">${applyReportHtml}</div>
        ${
          entries.length === 0
            ? `<p class="empty">清单为空<small>源目录里还没有任何受管文件。</small></p>`
            : `<ul class="rows">${rows(entries)}</ul>`
        }
      </main>`;

    app.querySelector<HTMLUListElement>(".rows")?.addEventListener("click", (event) => {
      const target = event.target as HTMLElement;
      const edit = target.closest<HTMLButtonElement>("[data-edit]");
      const row = target.closest<HTMLLIElement>(".row");
      if (!row) return;
      if (edit) {
        void editRow(row, edit);
      } else if (target.closest(".row-head")) {
        void toggleDiff(row);
      }
    });
    app.querySelector<HTMLButtonElement>("[data-pull]")?.addEventListener("click", () => void pull());
    app.querySelector<HTMLButtonElement>("[data-push]")?.addEventListener("click", () =>
      void pushChanges(),
    );
    app.querySelector<HTMLButtonElement>("[data-apply]")?.addEventListener("click", () => void startApply());
    bindSettingsEntry();
  } catch (error) {
    renderError(error);
  }
}

/** 添加结果的汇总：新增的行已经在清单里，这里只补「已在清单中 / 没添加」的说明。 */
function addReport(outcomes: AddOutcome[]): string {
  const count = (match: (result: AddResult) => boolean) =>
    outcomes.filter((outcome) => match(outcome.result)).length;
  const added = count((result) => result === "Added");
  const managed = count((result) => result === "AlreadyManaged" || result === "AlreadyManagedDiffers");

  const notes = outcomes.flatMap((outcome) => {
    const result = outcome.result;
    const path = `<code>${escapeHtml(outcome.target)}</code>`;
    if (result === "Added") return [];
    if (result === "AlreadyManaged") return [`<li>${path} 已在清单中，没有重复添加。</li>`];
    if (result === "AlreadyManagedDiffers") {
      return [`<li>${path} 已在清单中，但磁盘上的内容与源文件不同；没有覆盖，展开该行可看差异。</li>`];
    }
    return [`<li>${path} 没有添加：${escapeHtml(result.Rejected.reason)}</li>`];
  });

  return `<p class="add-summary">添加 ${added} 个文件 · 已在清单中 ${managed} 个 · 未添加 ${
    outcomes.length - added - managed
  } 个</p>${notes.length ? `<ul class="add-notes">${notes.join("")}</ul>` : ""}`;
}

/** 添加后清单立即重读：新行出现、状态由 Core 重算，不自动应用也不推送。 */
async function addPaths(paths: string[]): Promise<void> {
  try {
    addReportHtml = addReport(await invoke<AddOutcome[]>("add", { paths }));
  } catch (error) {
    addReportHtml = errorPanel(error);
  }
  await renderInventory();
}

async function pickFiles(): Promise<void> {
  try {
    const selected = await open({ multiple: true, title: "选择要添加的文件" });
    if (!selected) return;
    await addPaths(Array.isArray(selected) ? selected : [selected]);
  } catch (error) {
    addReportHtml = errorPanel(error);
    await renderInventory();
  }
}

async function submitPasted(form: HTMLFormElement): Promise<void> {
  const textarea = form.querySelector<HTMLTextAreaElement>("textarea")!;
  const paths = textarea.value
    .split("\n")
    .map((line) => line.trim())
    .filter(Boolean);
  if (paths.length === 0) {
    addReportHtml = `<p class="add-summary">先粘贴至少一个路径（文件或整棵目录）。</p>`;
    await renderInventory();
    return;
  }
  textarea.value = "";
  await addPaths(paths);
}

/** 错误面板 + 一个回到设置的入口：源目录指错时人不用重启应用。 */function renderError(error: unknown): void {
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

    // 配置已落盘：按新源目录重读。新目录不是仓库时回到「输入远端地址」，而不是错误面板。
    window.location.reload();
  });
}

/** 首次使用向导：固定的候选勾选，勾中的一次批量添加。只出现一次，跳过也算走过。
 *  私钥与历史记录不在候选里；文案说明要先自己加密。 */
async function renderWizard(): Promise<void> {
  let candidates: Candidate[];
  try {
    candidates = await invoke<Candidate[]>("onboarding_candidates");
  } catch (error) {
    renderError(error);
    return;
  }

  const items = candidates
    .map(
      (candidate) => `
      <li class="${candidate.exists ? "" : "missing"}">
        <label>
          <input type="checkbox" name="candidate" value="${escapeHtml(candidate.path)}"
            ${candidate.exists ? "" : "disabled"} />
          <code>${escapeHtml(candidate.path)}</code>
          <span class="presence">${candidate.exists ? "存在" : "不存在"}</span>
        </label>
      </li>`,
    )
    .join("");

  app.innerHTML = `
    <main class="onboarding wizard">
      <h1>挑几个配置文件托管</h1>
      <p class="hint">
        勾选的文件会被复制进源目录的 git 仓库，成为受管文件；这一步不写你 <code>$HOME</code> 里的任何文件。
        这个列表只在首次使用时出现一次，以后要加别的文件走清单页的「添加文件…」。
      </p>
      <p class="hint">
        私钥需要先用 <code>age</code> 自己加密后再添加（加密在 v0.3 提供）；
        向导不会推荐历史记录、私钥或凭据类文件。
      </p>
      <form class="wizard-form">
        <ul class="candidates">${items}</ul>
        <div class="actions">
          <button type="submit">添加选中的文件</button>
          <button type="button" class="nav" data-skip>跳过</button>
        </div>
        <div class="wizard-error"></div>
      </form>
    </main>`;

  const form = app.querySelector<HTMLFormElement>(".wizard-form")!;
  const slot = app.querySelector<HTMLDivElement>(".wizard-error")!;
  const submit = form.querySelector<HTMLButtonElement>("button[type=submit]")!;
  const skip = app.querySelector<HTMLButtonElement>("[data-skip]")!;

  const finish = async (paths: string[]): Promise<void> => {
    slot.innerHTML = "";
    submit.disabled = true;
    skip.disabled = true;
    if (paths.length > 0) {
      try {
        addReportHtml = addReport(await invoke<AddOutcome[]>("add", { paths }));
      } catch (error) {
        // 添加失败：停在向导上可以重试，清单还没被碰过。
        slot.innerHTML = errorPanel(error);
        submit.disabled = false;
        skip.disabled = false;
        return;
      }
    }
    try {
      await invoke("finish_onboarding");
    } catch (error) {
      // 标记没写住只是下次启动再问一次，不该把人挡在清单外面。
      addReportHtml = errorPanel(error);
    }
    await boot();
  };

  form.addEventListener("submit", (event) => {
    event.preventDefault();
    const paths = Array.from(
      form.querySelectorAll<HTMLInputElement>('input[name="candidate"]:checked'),
    ).map((input) => input.value);
    if (paths.length === 0) {
      slot.innerHTML = `<p class="add-summary">先勾选至少一个文件，或点「跳过」。</p>`;
      return;
    }
    void finish(paths);
  });
  skip.addEventListener("click", () => void finish([]));
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
      await boot();
    } catch (error) {
      // git 的原始 stderr 原样留在 errorPanel 里，改地址可以重试。
      slot.innerHTML = errorPanel(error);
      button.disabled = false;
      button.textContent = "clone";
      input.focus();
    }
  });
}

/** 清单页的刷新来源：fs watch 事件 + 从终端切回来。只绑一次（clone / 向导之后会再次 boot）。 */
let listening = false;
async function startListening(): Promise<void> {
  if (listening) return;
  listening = true;
  // 编辑器保存、外部改动源目录都会推一条；没有刷新按钮。
  await listen("inventory-changed", () => void renderInventory());
  // 源目录之外的 git 改动（在终端里 commit）没有监视事件，从终端切回来时补一次。
  window.addEventListener("focus", () => void renderInventory());
}

async function boot(): Promise<void> {
  try {
    const [hasRepo, sourceDir, wizardDone] = await Promise.all([
      invoke<boolean>("has_repo"),
      invoke<string>("source_dir"),
      invoke<boolean>("onboarding_done"),
    ]);
    if (!hasRepo) {
      renderOnboarding(sourceDir);
      return;
    }
    // 向导只出现一次：标记落在 app 配置目录里，跳过也会落。
    if (!wizardDone) {
      await renderWizard();
      return;
    }
    await renderInventory();
    await startListening();
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

// 添加控件在每次重渲染后都是新的，所以用事件委派挂在 #app 上，只绑一次。
app.addEventListener("click", (event) => {
  if ((event.target as HTMLElement).closest(".add-files")) void pickFiles();
});
app.addEventListener("submit", (event) => {
  const form = (event.target as HTMLElement).closest<HTMLFormElement>(".add-paths");
  if (!form) return;
  event.preventDefault();
  void submitPasted(form);
});

app.innerHTML = `<p class="empty">正在读取清单…</p>`;
void boot();
