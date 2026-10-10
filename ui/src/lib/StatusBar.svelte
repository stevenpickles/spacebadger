<script lang="ts">
  import { formatBytes, formatCount, formatElapsed, plural } from "./format";
  import type { Metric } from "./protocol/Metric";
  import type { ScanStatus } from "./protocol/ScanStatus";

  interface Props {
    status: ScanStatus | null;
    /** Elapsed time, advanced locally between updates while scanning. */
    elapsedMs: number;
    metric: Metric;
    showOmissions: boolean;
    ontoggleomissions: () => void;
  }

  let { status, elapsedMs, metric, showOmissions, ontoggleomissions }: Props = $props();

  const phase = $derived.by(() => {
    switch (status?.phase.kind) {
      case undefined:
        return "Ready";
      case "scanning":
        return "Scanning…";
      case "complete":
        return "Complete";
      case "completeWithOmissions":
        return "Complete with omissions";
      case "cancelled":
        return "Cancelled: partial result";
      case "failed":
        return "Failed";
    }
  });

  const skipped = $derived(
    status?.omissions.filter((o) => o.policy).reduce((n, o) => n + o.count, 0) ?? 0,
  );
  const errors = $derived(
    status?.omissions.filter((o) => !o.policy).reduce((n, o) => n + o.count, 0) ?? 0,
  );
</script>

<footer role="status">
  <strong class:busy={status?.phase.kind === "scanning"}>{phase}</strong>
  {#if status}
    <span>{plural(status.files, "file")}</span>
    <span>{plural(status.dirs, "folder")}</span>
    <span title="Allocated space found so far">{formatBytes(status.allocated)} allocated</span>
    {#if metric === "logical"}
      <span>{formatBytes(status.logical)} logical</span>
    {/if}
    <span>{formatElapsed(elapsedMs)}</span>
    {#if status.phase.kind === "scanning"}
      <span>{formatCount(status.pendingDirs)} folders queued</span>
    {/if}
    {#if status.unknownAllocationFiles > 0}
      <span>{plural(status.unknownAllocationFiles, "file")} with unknown allocation</span>
    {/if}
    {#if skipped + errors > 0}
      <button type="button" aria-expanded={showOmissions} onclick={ontoggleomissions}>
        {[errors > 0 ? plural(errors, "error") : "", skipped > 0 ? `${formatCount(skipped)} skipped` : ""]
          .filter(Boolean)
          .join(", ")}
      </button>
    {/if}
  {/if}
</footer>

<style>
  footer {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 4px 14px;
    padding: 5px 12px;
    border-top: 1px solid var(--line);
    background: var(--panel);
    font-size: 12px;
    font-variant-numeric: tabular-nums;
  }
  .busy {
    color: var(--accent);
  }
  button {
    border: none;
    background: none;
    color: var(--link);
    padding: 0;
    font: inherit;
    cursor: pointer;
    text-decoration: underline;
  }
</style>
