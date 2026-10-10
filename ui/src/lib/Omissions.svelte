<script lang="ts">
  import { formatCount } from "./format";
  import type { OmissionGroup } from "./protocol/OmissionGroup";
  import type { OmissionReason } from "./protocol/OmissionReason";

  interface Props {
    groups: OmissionGroup[];
    onclose: () => void;
  }

  let { groups, onclose }: Props = $props();

  const reasons: Record<OmissionReason, string> = {
    symlink: "Symbolic links (not followed)",
    mountPoint: "Junctions and other mounted filesystems (not followed)",
    specialFile: "Sockets, devices, and other special files",
    unknownReparse: "Unrecognized redirections (not followed)",
    permissionDenied: "Permission denied",
    vanished: "Removed during the scan",
    disconnected: "Device or share disconnected",
    unavailable: "Metadata unavailable",
    other: "Other errors",
  };
</script>

<section aria-label="Omitted items">
  <header>
    <h2>Left out of the totals</h2>
    <button type="button" onclick={onclose} aria-label="Close omitted items">✕</button>
  </header>
  {#each [...groups].sort((a, b) => Number(a.policy) - Number(b.policy)) as group (group.reason)}
    <details>
      <summary>
        {reasons[group.reason]}: {formatCount(group.count)}
        {#if group.policy}<span class="muted">(by design)</span>{/if}
      </summary>
      <ul>
        {#each group.samples as sample, i (i)}
          <li>
            <span class="path">{sample.path}</span>
            {#if sample.detail}<span class="muted"> {sample.detail}</span>{/if}
          </li>
        {/each}
        {#if group.count > group.samples.length}
          <li class="muted">…and {formatCount(group.count - group.samples.length)} more</li>
        {/if}
      </ul>
    </details>
  {/each}
</section>

<style>
  section {
    padding: 12px;
    border-top: 1px solid var(--line);
    overflow-wrap: anywhere;
  }
  header {
    display: flex;
    justify-content: space-between;
    align-items: center;
  }
  h2 {
    margin: 0 0 6px;
    font-size: 14px;
  }
  header button {
    border: none;
    background: none;
    color: var(--fg);
    cursor: pointer;
  }
  summary {
    cursor: pointer;
    padding: 3px 0;
  }
  ul {
    margin: 2px 0 6px;
    padding-left: 18px;
    font-size: 12px;
  }
  .path {
    user-select: text;
  }
  .muted {
    color: var(--muted);
  }
</style>
