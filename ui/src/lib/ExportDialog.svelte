<script lang="ts" module>
  import type { ExportFormat } from "./protocol/ExportFormat";
  import type { ExportScope } from "./protocol/ExportScope";

  export interface ExportChoice {
    scope: ExportScope;
    format: ExportFormat;
  }

  /** Characters no file name may hold on any of the supported systems. */
  const UNSAFE = /[\\/:*?"<>|\u0000-\u001f]+/g;

  const pad = (n: number) => String(n).padStart(2, "0");

  /** Today as yyyy-mm-dd, in local time. */
  export function isoDate(d = new Date()): string {
    return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
  }

  /** Now as yyyy-mm-dd hh:mm, in local time, for a report's header. */
  export function localTime(d = new Date()): string {
    return `${isoDate(d)} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
  }

  /** The name the save dialog suggests, e.g. SpaceBadger-Downloads-2026-10-13.csv.
   * The scan root's name is its full path, so only the last part is used;
   * a drive such as C:\ gives "C". */
  export function exportFileName(folder: string, format: ExportFormat, d = new Date()): string {
    const last = folder.split(/[\\/]/).filter(Boolean).pop() ?? "";
    const safe = last
      .replace(UNSAFE, "-")
      .replace(/^[\s.-]+|[\s.-]+$/g, "")
      .slice(0, 80);
    const ext = format === "csv" ? "csv" : "txt";
    return `SpaceBadger-${safe || "export"}-${isoDate(d)}.${ext}`;
  }
</script>

<script lang="ts">
  // Asks what to export and in which format; the native save dialog
  // follows once Export is chosen. The last choices are remembered on this
  // computer.

  interface Props {
    /** The folder's name; the scan root's is its full path. */
    folderName: string;
    /** The filter query when one is on. */
    filter: string | null;
    onexport: (choice: ExportChoice) => void;
    oncancel: () => void;
    /** Takes the focus on closing when the control that opened the dialog
     * is gone or disabled, as the Export button is while exporting. */
    fallbackFocus?: () => void;
  }

  let { folderName, filter, onexport, oncancel, fallbackFocus }: Props = $props();

  const id = $props.id();

  function saved(key: string): string | null {
    try {
      return localStorage.getItem(key);
    } catch {
      return null;
    }
  }

  function remember(key: string, value: string) {
    try {
      localStorage.setItem(key, value);
    } catch {
      // Not remembered.
    }
  }

  let scope = $state<ExportScope>(saved("export-scope") === "everything" ? "everything" : "contents");
  let format = $state<ExportFormat>(saved("export-format") === "report" ? "report" : "csv");

  let dialog: HTMLDialogElement;
  let first = $state<HTMLInputElement>();
  const returnFocus = document.activeElement as HTMLElement | null;
  let gone = false;

  $effect(() => {
    dialog.showModal();
    first?.focus();
    return () => {
      gone = true;
      // After the update that closed the dialog, which may disable the opener.
      requestAnimationFrame(() => {
        if (returnFocus?.isConnected && !returnFocus.matches(":disabled")) returnFocus.focus();
        else fallbackFocus?.();
      });
    };
  });

  function submit(e: SubmitEvent) {
    e.preventDefault();
    remember("export-scope", scope);
    remember("export-format", format);
    onexport({ scope, format });
  }

  /** Escape: the parent decides when the dialog goes away. */
  function onCancel(e: Event) {
    e.preventDefault();
    oncancel();
  }

  function onClosed() {
    if (!gone) oncancel();
  }
</script>

<dialog
  bind:this={dialog}
  aria-labelledby="{id}-title"
  onkeydown={(e) => e.stopPropagation()}
  oncancel={onCancel}
  onclose={onClosed}
>
  <form onsubmit={submit}>
    <h2 id="{id}-title">Export “{folderName}”</h2>
    <fieldset>
      <legend>Format</legend>
      <label>
        <input type="radio" bind:this={first} bind:group={format} value="csv" />
        <span><strong>CSV</strong> <span class="muted">— one row per file and folder, for spreadsheets</span></span>
      </label>
      <label>
        <input type="radio" bind:group={format} value="report" />
        <span>
          <strong>Text report</strong>
          <span class="muted">— totals and the 50 largest folders and files under this folder</span>
        </span>
      </label>
    </fieldset>
    <fieldset disabled={format === "report"}>
      <legend>What</legend>
      <label>
        <input type="radio" bind:group={scope} value="contents" />
        <span>This folder's contents <span class="muted">— its own files and folders</span></span>
      </label>
      <label>
        <input type="radio" bind:group={scope} value="everything" />
        <span>Everything under this folder <span class="muted">— every file and folder, at any depth</span></span>
      </label>
      {#if format === "report"}
        <p class="muted small">A report always looks at everything under the folder.</p>
      {/if}
    </fieldset>
    {#if filter !== null}
      <p class="filtered" id="{id}-filter">
        The filter <code>{filter}</code> is on: only matching files are exported, and the folders that contain them, with
        the size and file count of their matching files.
      </p>
    {/if}
    <p class="muted small">
      Sizes are file sizes. You choose where to save next. Large folders take a moment; you can keep working meanwhile.
    </p>
    <div class="buttons">
      <button type="button" onclick={oncancel}>Cancel</button>
      <button type="submit" class="primary">Export…</button>
    </div>
  </form>
</dialog>

<style>
  dialog {
    width: min(460px, calc(100vw - 32px));
    max-height: calc(100vh - 32px);
    overflow-y: auto;
    box-sizing: border-box;
    padding: 16px 20px;
    border: 1px solid var(--line);
    border-radius: 8px;
    background: var(--panel);
    color: var(--fg);
    box-shadow: 0 8px 32px rgb(0 0 0 / 0.35);
    overflow-wrap: anywhere;
  }
  dialog::backdrop {
    background: rgb(0 0 0 / 0.35);
  }
  h2 {
    margin: 0 0 var(--space-3);
    font-size: 16px;
  }
  fieldset {
    margin: 0 0 var(--space-3);
    padding: 0;
    border: none;
  }
  fieldset:disabled label {
    opacity: 0.55;
  }
  legend {
    margin-bottom: var(--space-1);
    padding: 0;
    font-weight: 600;
  }
  label {
    display: flex;
    align-items: baseline;
    gap: var(--space-2);
    padding: 3px 0;
    cursor: pointer;
  }
  fieldset p {
    margin: var(--space-1) 0 0;
  }
  p {
    margin: 0 0 10px;
  }
  .filtered {
    padding: 6px 10px;
    border-radius: var(--radius);
    background: var(--selected);
  }
  .buttons {
    display: flex;
    justify-content: flex-end;
    gap: var(--space-2);
    margin-top: 14px;
  }
</style>
