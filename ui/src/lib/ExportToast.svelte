<script lang="ts">
  // An export's progress, then where it was saved, with a way to show the
  // file. Stays until closed or replaced by the next export.
  import { formatCount, plural } from "./format";
  import type { ExportReport } from "./protocol/ExportReport";
  import PathText from "./PathText.svelte";

  interface Props {
    /** Rows written so far while exporting; `null` before the file is chosen. */
    rows: number | null;
    /** Set once the export is done. */
    report: ExportReport | null;
    /** "Explorer", "Finder", or "file manager". */
    fileManager: string;
    onreveal: () => void;
    onclose: () => void;
  }

  let { rows, report, fileManager, onreveal, onclose }: Props = $props();

  const done = $derived.by(() => {
    if (!report) return "";
    if (report.format === "csv") return `Exported ${plural(report.rows, "row")} to`;
    return "Saved the report to";
  });
</script>

<div class="toast" role="status">
  {#if report}
    <div class="text">
      <span>{done}</span>
      <span class="selectable path"><PathText path={report.path} /></span>
    </div>
    <div class="actions">
      <button type="button" onclick={onreveal}>Show in {fileManager}</button>
      <button type="button" class="icon-button" aria-label="Dismiss" onclick={onclose}>✕</button>
    </div>
  {:else}
    <div class="text">
      <span>Exporting…{rows !== null && rows > 0 ? ` ${formatCount(rows)} so far` : ""}</span>
    </div>
  {/if}
</div>

<style>
  .toast {
    position: fixed;
    right: 16px;
    bottom: 40px;
    z-index: 30;
    display: flex;
    align-items: center;
    gap: var(--space-3);
    max-width: min(560px, calc(100vw - 32px));
    padding: 8px 8px 8px 12px;
    border: 1px solid var(--line);
    border-radius: 6px;
    background: var(--bg);
    box-shadow: 0 6px 20px rgb(0 0 0 / 0.2);
    font-size: var(--fs-sm);
  }
  .text {
    display: flex;
    flex-direction: column;
    min-width: 0;
  }
  .path {
    overflow-wrap: anywhere;
    color: var(--muted);
  }
  .actions {
    display: flex;
    flex: none;
    align-items: center;
    gap: var(--space-1);
  }
</style>
