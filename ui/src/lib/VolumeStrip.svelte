<script lang="ts">
  // Whole-volume capacity, kept apart from the treemap: files the scan
  // measured, used space no measured file accounts for, and free space. The
  // middle block is only called "not attributed" once a scan has completed;
  // before that it still includes files not scanned yet.
  import { exactBytes, formatBytes, formatCount } from "./format";
  import type { ScanStatus } from "./protocol/ScanStatus";
  import type { VolumeInfo } from "./protocol/VolumeInfo";

  interface Props {
    volume: VolumeInfo;
    status: ScanStatus;
    /** "windows", "macos", or "linux". */
    os: string;
    onshowomissions: () => void;
  }

  let { volume, status, os, onshowomissions }: Props = $props();

  let explain = $state(false);

  const used = $derived(Math.max(0, volume.capacity - volume.free));
  const files = $derived(Math.min(status.allocated, used));
  const gap = $derived(used - files);
  const complete = $derived(
    status.phase.kind === "complete" || status.phase.kind === "completeWithOmissions",
  );
  const gapLabel = $derived.by(() => {
    if (complete) return "Not attributed to files";
    if (status.phase.kind === "scanning") return "Not scanned yet";
    return "Not scanned";
  });
  const unreadable = $derived(
    status.omissions.filter((o) => !o.policy).reduce((n, o) => n + o.count, 0),
  );
  const measured = $derived(complete ? Math.min(volume.metadata ?? 0, gap) : 0);
  const ntfs = $derived(volume.filesystem === "NTFS");

  function pct(bytes: number): string {
    if (volume.capacity <= 0) return "0%";
    const p = (bytes / volume.capacity) * 100;
    return p > 0 && p < 0.1 ? "<0.1%" : `${p.toFixed(1)}%`;
  }

  function width(bytes: number): string {
    return volume.capacity > 0 ? `${(bytes / volume.capacity) * 100}%` : "0";
  }
</script>

<section class="volume" aria-label="Volume overview">
  <div class="bar" aria-hidden="true">
    <div class="files" style:width={width(files)}></div>
    <button
      type="button"
      class="gap"
      class:pending={!complete}
      style:width={width(gap)}
      tabindex="-1"
      title={gapLabel}
      onclick={() => (explain = !explain)}
    ></button>
    <div class="free" style:width={width(volume.free)}></div>
  </div>
  <ul class="key">
    <li title={exactBytes(files)}>
      <span class="swatch files"></span>Files found {formatBytes(files)} ({pct(files)})
    </li>
    <li>
      <span class="swatch gap" class:pending={!complete}></span>
      <button type="button" class="link" aria-expanded={explain} onclick={() => (explain = !explain)}>
        {gapLabel} {formatBytes(gap)} ({pct(gap)})
      </button>
    </li>
    <li title={exactBytes(volume.free)}>
      <span class="swatch free"></span>Free {formatBytes(volume.free)} ({pct(volume.free)})
    </li>
    <li class="muted">
      of {formatBytes(volume.capacity)}{volume.filesystem ? ` ${volume.filesystem}` : ""}
    </li>
  </ul>
  {#if explain}
    <div class="explain">
      {#if !complete}
        <p>
          {status.phase.kind === "scanning"
            ? "The scan is still running, so this block includes files not found yet."
            : "The scan didn't finish, so this block includes files that were never scanned."}
          Once a scan completes, it shows the used space that no scanned file accounts for.
        </p>
      {:else}
        <p>
          The volume reports {formatBytes(used)} used, but the files SpaceBadger could measure take
          {formatBytes(files)}. The difference isn't spread over folders in the map. Likely sources:
        </p>
        <ul>
          {#if ntfs}
            <li>
              NTFS metadata (the master file table and journal):
              {#if volume.metadata !== null}
                master file table {formatBytes(volume.metadata)}.
              {:else}
                not measured; start SpaceBadger as administrator to measure the master file table.
              {/if}
            </li>
          {:else}
            <li>Filesystem structures and blocks the filesystem reserves for itself.</li>
          {/if}
          {#if unreadable > 0}
            <li>
              {formatCount(unreadable)} items couldn't be read, so their contents aren't counted.
              <button type="button" class="link" onclick={onshowomissions}>Show them</button>
            </li>
          {/if}
          {#if os === "windows"}
            <li>
              Restore points and shadow copies in System Volume Information, which only an
              administrator can measure.
            </li>
            <li>Alternate data streams, which SpaceBadger doesn't measure.</li>
          {:else}
            <li>Snapshots, which aren't part of the folder tree.</li>
          {/if}
        </ul>
        {#if measured > 0}
          <p class="muted">
            Measured: {formatBytes(measured)}. Unexplained: {formatBytes(gap - measured)}.
          </p>
        {/if}
      {/if}
    </div>
  {/if}
</section>

<style>
  .volume {
    padding: 6px 12px;
    border-bottom: 1px solid var(--line);
    font-size: 12px;
  }
  .bar {
    display: flex;
    height: 14px;
    border: 1px solid var(--line);
    border-radius: 3px;
    overflow: hidden;
    background: var(--bg);
  }
  .bar > * {
    flex: none;
    min-width: 0;
    height: 100%;
  }
  .files {
    background: var(--accent);
  }
  .gap {
    padding: 0;
    border: none;
    border-radius: 0;
    background: var(--gap);
    cursor: pointer;
  }
  .gap.pending {
    background: repeating-linear-gradient(
      45deg,
      var(--gap),
      var(--gap) 4px,
      transparent 4px,
      transparent 8px
    );
  }
  .free {
    background: var(--bg);
  }
  .key {
    display: flex;
    flex-wrap: wrap;
    gap: 2px 16px;
    margin: 4px 0 0;
    padding: 0;
    list-style: none;
    font-variant-numeric: tabular-nums;
  }
  .key li {
    display: flex;
    align-items: center;
    gap: 5px;
  }
  .swatch {
    display: inline-block;
    width: 11px;
    height: 11px;
    border: 1px solid var(--line);
    border-radius: 2px;
  }
  .swatch.files {
    background: var(--accent);
  }
  .swatch.gap {
    background: var(--gap);
  }
  .swatch.gap.pending {
    background: repeating-linear-gradient(45deg, var(--gap), var(--gap) 2px, transparent 2px, transparent 4px);
  }
  .swatch.free {
    background: var(--bg);
  }
  .link {
    border: none;
    background: none;
    padding: 0;
    color: var(--link);
    font: inherit;
    text-decoration: underline;
    cursor: pointer;
  }
  .muted {
    color: var(--muted);
  }
  .explain {
    max-width: 760px;
    margin-top: 6px;
  }
  .explain p {
    margin: 0 0 4px;
  }
  .explain ul {
    margin: 0 0 4px;
    padding-left: 18px;
  }
</style>
