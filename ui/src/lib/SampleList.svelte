<script lang="ts">
  // The largest few of a set of selected items, then "…and N more". Used by
  // the details panel and the delete dialog.
  import { exactBytes, formatBytes, formatCount } from "./format";
  import type { SelectedItem } from "./protocol/SelectedItem";

  interface Props {
    samples: SelectedItem[];
    /** How many items the samples stand for. */
    total: number;
    /** Show each item's full path as a second line. */
    paths?: boolean;
  }

  let { samples, total, paths = false }: Props = $props();
</script>

<ul class="samples" class:paths>
  {#each samples as item (item.node)}
    <li>
      <div class="line">
        <span class="row-name" title={paths ? undefined : item.path}>{item.name}{item.folder ? "/" : ""}</span>
        <span class="row-size" title={exactBytes(item.logical)}>{formatBytes(item.logical)}</span>
      </div>
      {#if paths}
        <div class="path muted selectable">{item.path}</div>
      {/if}
    </li>
  {/each}
  {#if total > samples.length}
    <li class="muted">…and {formatCount(total - samples.length)} more</li>
  {/if}
</ul>

<style>
  .samples {
    margin: 0;
    padding: 0;
    list-style: none;
    font-size: var(--fs-sm);
  }
  .line {
    display: flex;
    justify-content: space-between;
    gap: var(--space-2);
  }
  .paths li + li {
    margin-top: var(--space-1);
  }
  .path {
    font-size: var(--fs-xs);
    overflow-wrap: anywhere;
  }
</style>
