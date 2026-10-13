<script lang="ts">
  import { formatBytes, formatCount, formatElapsed, plural } from "./format";
  import type { ScanStatus } from "./protocol/ScanStatus";

  interface Props {
    status: ScanStatus | null;
    /** Elapsed time, advanced locally between updates while scanning. */
    elapsedMs: number;
    showOmissions: boolean;
    ontoggleomissions: () => void;
    oncancel: () => void;
  }

  let { status, elapsedMs, showOmissions, ontoggleomissions, oncancel }: Props = $props();

  const scanning = $derived(status?.phase.kind === "scanning");

  const phase = $derived.by(() => {
    switch (status?.phase.kind) {
      case undefined:
        return "Ready";
      case "scanning":
        return "Scanning…";
      case "complete":
      case "completeWithOmissions":
        return "Complete";
      case "cancelled":
        return "Cancelled: partial result";
      case "failed":
        return "Failed";
    }
  });

  const skipped = $derived(
    status?.omissions.filter((o) => o.policy).reduce((n, o) => n + o.count, 0) ?? 0,
  );
  const unreadable = $derived(
    status?.omissions.filter((o) => !o.policy).reduce((n, o) => n + o.count, 0) ?? 0,
  );
  const omissionsTitle = $derived(
    [
      unreadable > 0 ? `${plural(unreadable, "item")} couldn't be read` : "",
      skipped > 0 ? `${plural(skipped, "item")} skipped on purpose` : "",
    ]
      .filter(Boolean)
      .join(", "),
  );
</script>

<!-- Not a live region: these counters change every update. The app
     announces phase changes separately. -->
<footer>
  <strong class:busy={scanning}>{phase}</strong>
  {#if scanning}
    <button type="button" class="link" onclick={oncancel}>Cancel</button>
  {/if}
  {#if status}
    <span>{plural(status.files, "file")}</span>
    <span>{plural(status.dirs, "folder")}</span>
    <span title="Total size of the files found so far">{formatBytes(status.logical)}</span>
    <span title="Time spent scanning">{formatElapsed(elapsedMs)}</span>
    {#if scanning}
      <span title="Folders found but not read yet">{formatCount(status.pendingDirs)} folders to go</span>
    {/if}
    {#if skipped + unreadable > 0}
      <button
        type="button"
        class="link"
        aria-expanded={showOmissions}
        title="{omissionsTitle}. Show what wasn't counted."
        onclick={ontoggleomissions}
      >
        {formatCount(skipped + unreadable)} not counted
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
    font-size: var(--fs-sm);
    font-variant-numeric: tabular-nums;
  }
  .busy {
    color: var(--accent);
  }
</style>
