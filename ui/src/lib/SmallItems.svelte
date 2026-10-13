<script lang="ts">
  // The files and folders merged into one "small items" region, largest
  // first. Stays open while items in it are selected, so they can be
  // stepped through. Loads a page at a time: "Show more" adds the next page,
  // and a scan change reloads the pages already shown.
  import { untrack } from "svelte";
  import { errorText, smallItems } from "./api";
  import { exactBytes, formatBytes, formatCount, plural } from "./format";
  import ItemList from "./ItemList.svelte";
  import type { Metric } from "./protocol/Metric";
  import type { SmallItem } from "./protocol/SmallItem";
  import type { SmallItemsPage } from "./protocol/SmallItemsPage";
  import type { Pick } from "./selection";

  interface Props {
    generation: number;
    folder: number;
    folderName: string;
    count: number;
    /** The size measure the region was drawn with; the interface uses file size. */
    metric?: Metric;
    search: number | null;
    /** Scan revision; a change refreshes the loaded items. */
    revision: number;
    selected: Set<number>;
    /** Where Shift ranges start. */
    anchor: number | null;
    onpick: (pick: Pick) => void;
    /** The Delete key. */
    ondelete: () => void;
    /** Open a folder in the map. */
    onopen: (node: number) => void;
    /** Right-click, the Menu key, or Shift+F10; window coordinates. */
    onmenu?: (item: SmallItem, x: number, y: number) => void;
    onclose: () => void;
  }

  let {
    generation,
    folder,
    folderName,
    count,
    metric = "logical",
    search,
    revision,
    selected,
    anchor,
    onpick,
    ondelete,
    onopen,
    onmenu,
    onclose,
  }: Props = $props();

  const PAGE = 200;

  let pages = $state.raw<SmallItemsPage[]>([]);
  const items = $derived(pages.flatMap((p) => p.items));
  /** Items in the region; `null` until the first page arrives. */
  let total = $state<number | null>(null);
  let empty = $state(0);
  let loading = $state(false);
  let error = $state<string | null>(null);
  /** Bumped by Retry. */
  let attempt = $state(0);
  /** Responses to older requests are dropped. */
  let seq = 0;

  const name = $derived(folderName || "this folder");

  function fetchPages(offset: number, n: number): Promise<SmallItemsPage[]> {
    const base = { generation, folder, count, metric, search, limit: PAGE };
    return Promise.all(Array.from({ length: n }, (_, i) => smallItems({ ...base, offset: offset + i * PAGE })));
  }

  function settle(mine: number, next: SmallItemsPage[]) {
    if (mine !== seq) return;
    const last = next[next.length - 1];
    if (!last || last.generation !== generation || last.folder !== folder) return;
    pages = next;
    total = last.total;
    empty = last.empty;
    error = null;
  }

  function fail(mine: number, e: unknown) {
    if (mine === seq) error = errorText(e);
  }

  // Load the first page, and reload as many pages as are shown when the
  // scan or the region changes.
  $effect(() => {
    void [revision, attempt];
    const n = untrack(() => Math.max(1, pages.length));
    const mine = ++seq;
    loading = true;
    fetchPages(0, n)
      .then((next) => settle(mine, next), (e: unknown) => fail(mine, e))
      .finally(() => {
        if (mine === seq) loading = false;
      });
  });


  function more() {
    if (loading) return;
    const mine = ++seq;
    loading = true;
    fetchPages(pages.length * PAGE, 1)
      .then((next) => settle(mine, [...pages, ...next]), (e: unknown) => fail(mine, e))
      .finally(() => {
        if (mine === seq) loading = false;
      });
  }

  function retry() {
    error = null;
    attempt++;
  }

  const left = $derived(total === null ? 0 : Math.min(PAGE, total - items.length));
</script>

<section class="small-items" aria-label="Small items in {name}">
  <header class="panel-head">
    <div class="title">
      <h3>Small items</h3>
      <span class="muted small">
        {#if total !== null}{plural(total, "item")}{/if} in {name}
      </span>
    </div>
    <button type="button" class="icon-button" onclick={onclose} aria-label="Close small items" title="Close">
      ✕
    </button>
  </header>
  <ItemList
    total={items.length}
    item={(i) => items[i]}
    node={(it) => it.node}
    label="Small items in {name}, largest first"
    rowHeight={30}
    title={(it) => (it.folder ? `${it.name} (folder; Enter opens it)` : it.name)}
    {selected}
    {anchor}
    {onpick}
    onopen={(it) => it.folder && onopen(it.node)}
    {onmenu}
    {ondelete}
  >
    {#snippet kind(it)}
      {it.folder ? "folder" : "file"}
    {/snippet}
    {#snippet main(it)}
      <span class="row-name name">{it.name}</span>
    {/snippet}
    {#snippet size(it)}
      <span title={exactBytes(it.weight)}>{formatBytes(it.weight)}</span>
    {/snippet}
    {#snippet action(it)}
      {#if it.folder}
        <button
          type="button"
          class="open"
          tabindex="-1"
          onclick={(e) => {
            e.stopPropagation();
            onopen(it.node);
          }}
          ondblclick={(e) => e.stopPropagation()}
        >
          Open
        </button>
      {/if}
    {/snippet}
  </ItemList>
  <footer>
    {#if total !== null && items.length < total}
      <button type="button" class="more" disabled={loading} onclick={more}>
        {loading ? "Loading…" : `Show ${formatCount(left)} more`}
      </button>
    {/if}
    {#if empty > 0}
      <p class="muted small">
        {plural(empty, "other item")} here {empty === 1 ? "has" : "have"} no size, so the map can't show
        {empty === 1 ? "it" : "them"}. Use the filter to find them by name.
      </p>
    {/if}
    {#if error}
      <p class="error small" role="alert">
        Couldn't list the items: {error}
        <button type="button" class="link" onclick={retry}>Retry</button>
      </p>
    {/if}
  </footer>
</section>

<style>
  .small-items {
    display: flex;
    flex-direction: column;
    flex: 1;
    min-height: 0;
  }
  header {
    flex: none;
    justify-content: space-between;
    align-items: center;
    border-bottom: 1px solid var(--line);
  }
  .title {
    display: flex;
    align-items: baseline;
    gap: var(--space-2);
    min-width: 0;
  }
  .name {
    display: block;
  }
  .open {
    padding: 0 var(--space-2);
    font-size: var(--fs-sm);
  }
  footer {
    flex: none;
  }
  footer:empty {
    display: none;
  }
  .more {
    margin: var(--space-2) var(--space-3) 0;
  }
  footer p {
    margin: var(--space-2) var(--space-3);
  }
</style>
