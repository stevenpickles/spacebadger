<script lang="ts">
  // The files and folders merged into one "small items" region, largest
  // first. Stays open while items in it are selected, so they can be
  // stepped through; loads more on request.
  import { smallItems } from "./api";
  import { formatBytes, formatCount, plural } from "./format";
  import type { Metric } from "./protocol/Metric";
  import type { SmallItem } from "./protocol/SmallItem";
  import { clickPick, listRange, type Pick } from "./selection";

  interface Props {
    generation: number;
    folder: number;
    folderName: string;
    count: number;
    metric: Metric;
    search: number | null;
    /** Scan revision; a change refreshes the loaded items. */
    revision: number;
    selected: Set<number>;
    /** Where Shift ranges start. */
    anchor: number | null;
    onpick: (pick: Pick) => void;
    /** The Delete key. */
    ondelete: () => void;
    onopen: (node: number) => void;
    onclose: () => void;
  }

  let {
    generation,
    folder,
    folderName,
    count,
    metric,
    search,
    revision,
    selected,
    anchor,
    onpick,
    ondelete,
    onopen,
    onclose,
  }: Props = $props();

  const PAGE = 200;

  let items = $state.raw<SmallItem[]>([]);
  let total = $state(0);
  let empty = $state(0);
  let wanted = $state(PAGE);
  let error = $state<string | null>(null);

  // Loads the first `wanted` items a page at a time (the backend caps page
  // size), and reloads them when the scan changes.
  $effect(() => {
    const base = { generation, folder, count, metric, search, limit: PAGE };
    void revision;
    const pages = Array.from({ length: Math.ceil(wanted / PAGE) }, (_, i) =>
      smallItems({ ...base, offset: i * PAGE }),
    );
    Promise.all(pages).then(
      (loaded) => {
        const last = loaded[loaded.length - 1];
        if (!last || last.generation !== generation || last.folder !== folder) return;
        items = loaded.flatMap((p) => p.items);
        total = last.total;
        empty = last.empty;
        error = null;
      },
      (e: unknown) => (error = String(e)),
    );
  });

  function onRowClick(e: MouseEvent, index: number, item: SmallItem) {
    const from = anchor === null ? -1 : items.findIndex((i) => i.node === anchor);
    const range = e.shiftKey && from >= 0 ? listRange(items, from, index, (i) => i.node) : null;
    onpick(clickPick({ node: item.node, other: false }, e, range));
  }

  function onKey(e: KeyboardEvent) {
    if (e.key !== "Delete") return;
    ondelete();
    e.preventDefault();
  }
</script>

<section class="small" aria-label="Small items in {folderName}">
  <header>
    <div>
      <strong>{plural(total, "small item")}</strong>
      <span class="muted">in {folderName}</span>
    </div>
    <button type="button" class="close" onclick={onclose} aria-label="Close small items">✕</button>
  </header>
  <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
  <ul onkeydown={onKey}>
    {#each items as item, index (item.node)}
      <li class:selected={selected.has(item.node)}>
        <button
          type="button"
          class="row"
          aria-pressed={selected.has(item.node)}
          onclick={(e) => onRowClick(e, index, item)}
        >
          <span class="name" title={item.name}>{item.name}</span>
          {#if item.folder}<span class="muted kind">folder</span>{/if}
          <span class="size">{formatBytes(item.weight)}</span>
        </button>
        {#if item.folder}
          <button type="button" class="open" onclick={() => onopen(item.node)}>Open</button>
        {/if}
      </li>
    {/each}
  </ul>
  {#if items.length < total}
    <button type="button" class="more" onclick={() => (wanted += PAGE)}>
      Show {formatCount(Math.min(PAGE, total - items.length))} more
    </button>
  {/if}
  {#if empty > 0}
    <p class="muted note">
      {plural(empty, "other item")} here {empty === 1 ? "has" : "have"} no {metric} size, so the map
      can't show {empty === 1 ? "it" : "them"}. The filter finds files by name.
    </p>
  {/if}
  {#if error}
    <p class="error" role="alert">Couldn't list the items: {error}</p>
  {/if}
</section>

<style>
  .small {
    display: flex;
    flex-direction: column;
    min-height: 0;
    flex: 1;
    border-top: 1px solid var(--line);
  }
  header {
    display: flex;
    justify-content: space-between;
    align-items: flex-start;
    gap: 8px;
    padding: 8px 12px;
    font-size: 12px;
  }
  header strong {
    display: block;
    font-size: 13px;
  }
  .close {
    border: none;
    background: none;
    padding: 0 4px;
    color: inherit;
  }
  ul {
    flex: 1;
    min-height: 80px;
    margin: 0;
    padding: 0;
    overflow-y: auto;
    list-style: none;
    border-top: 1px solid var(--line);
  }
  li {
    display: flex;
    align-items: center;
    border-bottom: 1px solid var(--line);
  }
  li.selected {
    background: var(--selected);
    font-weight: 600;
  }
  .row {
    flex: 1;
    min-width: 0;
    display: flex;
    justify-content: space-between;
    gap: 8px;
    padding: 5px 12px;
    border: none;
    border-radius: 0;
    background: none;
    text-align: left;
    font-weight: inherit;
  }
  .row:hover {
    background: var(--hover);
  }
  .name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .kind {
    margin-left: auto;
    font-size: 11px;
  }
  .size {
    flex: none;
    font-variant-numeric: tabular-nums;
  }
  .open {
    margin-right: 8px;
    padding: 1px 8px;
    font-size: 12px;
  }
  .more {
    margin: 6px 12px;
  }
  .muted {
    color: var(--muted);
  }
  .note {
    margin: 6px 12px;
    font-size: 12px;
  }
  .error {
    margin: 6px 12px;
    color: var(--danger);
  }
</style>
