<script lang="ts">
  // What's in the folder the map shows, largest first: the same files and
  // folders as the map, including those merged into small-items regions.
  // Rows are fetched a page at a time as they scroll into view; the list
  // refetches what's visible when the scan changes, and starts over for a
  // new folder or filter.
  import { errorText, smallItems } from "./api";
  import { exactBytes, formatBytes, percent, plural } from "./format";
  import ItemList from "./ItemList.svelte";
  import type { SmallItem } from "./protocol/SmallItem";
  import type { Pick } from "./selection";

  interface Props {
    generation: number;
    /** The folder the map shows. */
    folder: number;
    folderName: string;
    /** The folder's size, for each item's share; 0 if unknown. */
    folderSize: number;
    search: number | null;
    /** Scan revision; a change refetches the visible rows. */
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
  }

  let {
    generation,
    folder,
    folderName,
    folderSize,
    search,
    revision,
    selected,
    anchor,
    onpick,
    ondelete,
    onopen,
    onmenu,
  }: Props = $props();

  const PAGE = 200;
  /** Asks the small-items request for every child rather than the smallest. */
  const ALL = 0xffff_ffff;

  let list: ReturnType<typeof ItemList<SmallItem>> | undefined = $state();
  let rows = $state.raw(new Map<number, SmallItem>());
  /** Items in the folder; `null` until the first page arrives. */
  let total = $state<number | null>(null);
  let empty = $state(0);
  let first = $state(0);
  let last = $state(0);
  let error = $state<string | null>(null);
  /** Bumped by Retry, so failed pages are fetched again. */
  let attempt = $state(0);

  const name = $derived(folderName || "this folder");

  // A different folder, filter, or scan starts at the top.
  let shown = "";
  $effect(() => {
    const key = `${generation}/${folder}/${search}`;
    if (key === shown) return;
    shown = key;
    rows = new Map();
    total = null;
    empty = 0;
    error = null;
    list?.reset();
  });

  // Fetch the pages covering the visible rows; refetch on every revision.
  let loaded = new Set<string>();
  $effect(() => {
    const want = { generation, folder, count: ALL, metric: "logical" as const, search };
    const tag = `${generation}/${folder}/${search}/${revision}/${attempt}`;
    for (let p = Math.floor(first / PAGE); p * PAGE < Math.max(last, 1); p++) {
      const key = `${tag}/${p}`;
      if (loaded.has(key)) continue;
      loaded.add(key);
      smallItems({ ...want, offset: p * PAGE, limit: PAGE }).then(
        (page) => {
          if (page.generation !== generation || page.folder !== folder || want.search !== search) return;
          const next = new Map(rows);
          page.items.forEach((it, i) => next.set(page.offset + i, it));
          // Rows past a shrunken end would linger otherwise.
          for (const i of next.keys()) if (i >= page.total) next.delete(i);
          rows = next;
          total = page.total;
          empty = page.empty;
          error = null;
        },
        (e: unknown) => {
          loaded.delete(key);
          if (want.folder === folder && want.search === search) error = errorText(e);
        },
      );
    }
    if (loaded.size > 200) loaded = new Set([...loaded].slice(-50));
  });

  function indexOf(node: number): number {
    for (const [i, r] of rows) if (r.node === node) return i;
    return -1;
  }

  export function focus() {
    list?.focus();
  }

  /** Brings an item selected elsewhere (the map) into view, if it's loaded. */
  export function reveal(node: number) {
    list?.scrollToRow(indexOf(node));
  }

  function retry() {
    error = null;
    attempt++;
  }

  function sizeTitle(it: SmallItem): string {
    const share = folderSize > 0 ? `, ${percent(it.weight, folderSize)} of ${name}` : "";
    return `${exactBytes(it.weight)}${share}`;
  }
</script>

<section class="contents" aria-label="Contents of {name}">
  <header class="panel-head">
    <h3>Contents</h3>
    {#if total !== null}
      <span class="muted small">{plural(total, "item")} · largest first</span>
    {/if}
  </header>
  <ItemList
    bind:this={list}
    total={total ?? 0}
    item={(i) => rows.get(i)}
    node={(it) => it.node}
    label="Contents of {name}, largest first"
    rowHeight={30}
    title={(it) => (it.folder ? `${it.name} (folder; double-click or Enter opens it)` : it.name)}
    {selected}
    {anchor}
    {indexOf}
    onvisible={(f, l) => {
      first = f;
      last = l;
    }}
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
      <span title={sizeTitle(it)}>{formatBytes(it.weight)}</span>
    {/snippet}
    {#snippet empty()}
      <p class="muted">{total === null ? "Loading…" : "Nothing here has any size."}</p>
    {/snippet}
  </ItemList>
  {#if empty > 0 || error}
    <footer>
      {#if empty > 0}
        <p class="muted small">
          {plural(empty, "other item")} here {empty === 1 ? "has" : "have"} no size, so {empty === 1
            ? "it isn't"
            : "they aren't"} listed. Use the filter to find them by name.
        </p>
      {/if}
      {#if error}
        <p class="error small" role="alert">
          Couldn't list the contents: {error}
          <button type="button" class="link" onclick={retry}>Retry</button>
        </p>
      {/if}
    </footer>
  {/if}
</section>

<style>
  .contents {
    display: flex;
    flex-direction: column;
    flex: 1;
    min-height: 0;
  }
  header {
    flex: none;
    align-items: baseline;
    gap: var(--space-2);
    border-bottom: 1px solid var(--line);
  }
  .name {
    display: block;
  }
  footer {
    flex: none;
  }
  footer p {
    margin: var(--space-2) var(--space-3);
  }
</style>
