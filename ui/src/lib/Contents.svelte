<script lang="ts" module>
  /** A row of the contents list: a file or folder in the viewed folder, or
   * with folders hidden, a file anywhere under it. */
  export interface ContentsRow {
    node: number;
    name: string;
    folder: boolean;
    weight: number;
    /** With folders hidden: the containing folder, and its path relative to
     * the viewed folder ("" when directly in it). */
    parent: number | null;
    subfolder: string | null;
  }
</script>

<script lang="ts">
  // What's in the folder the map shows, largest first. With "Folders"
  // checked: the same files and folders as the map, including those merged
  // into small-items regions. Unchecked: the files anywhere under the
  // folder, each with the subfolder it's in. Rows are fetched a page at a
  // time as they scroll into view; the list refetches what's visible when
  // the scan changes, and starts over for a new folder, filter, or mode.
  //
  // Rankings move as the scan goes on, so pages fetched at different times
  // can overlap or leave gaps. The rows shown are one set, bound to the
  // revision it was fetched for: a newer revision refetches every visible
  // page together and replaces the set whole, one refresh at a time with
  // only the newest waiting, and replies for a set that's been replaced
  // are dropped.
  import { onDestroy, tick, untrack } from "svelte";
  import { errorText, folderFiles, smallItems } from "./api";
  import { exactBytes, formatBytes, percent, plural } from "./format";
  import ItemList from "./ItemList.svelte";
  import { Coalescer, latest, queryKey, type Ticket } from "./request";
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
    /** List folders too; otherwise only files, from every level. */
    folders: boolean;
    onfolders: (on: boolean) => void;
    selected: Set<number>;
    /** Where Shift ranges start. */
    anchor: number | null;
    onpick: (pick: Pick) => void;
    /** The Delete key. */
    ondelete: () => void;
    /** Open a folder in the map. */
    onopen: (node: number) => void;
    /** Show a file in a subfolder in the map: open its folder, select it. */
    onshow: (node: number, parent: number) => void;
    /** Right-click, the Menu key, or Shift+F10; window coordinates. */
    onmenu?: (row: ContentsRow, x: number, y: number) => void;
  }

  let {
    generation,
    folder,
    folderName,
    folderSize,
    search,
    revision,
    folders,
    onfolders,
    selected,
    anchor,
    onpick,
    ondelete,
    onopen,
    onshow,
    onmenu,
  }: Props = $props();

  const PAGE = 200;
  /** Asks the small-items request for every child rather than the smallest. */
  const ALL = 0xffff_ffff;

  let list: ReturnType<typeof ItemList<ContentsRow>> | undefined = $state();
  let rows = $state.raw(new Map<number, ContentsRow>());
  /** Rows in all; `null` until the first page arrives. */
  let total = $state<number | null>(null);
  let empty = $state(0);
  let first = $state(0);
  let last = $state(0);
  let error = $state<string | null>(null);
  /** Bumped by Retry, so failed pages are fetched again. */
  let attempt = $state(0);

  const name = $derived(folderName || "this folder");

  interface Page {
    folder: number;
    offset: number;
    total: number;
    empty: number;
    rows: ContentsRow[];
  }

  function fetchPage(offset: number): Promise<Page & { generation: number }> {
    const base = { generation, folder, metric: "logical" as const, search, offset, limit: PAGE };
    if (folders) {
      return smallItems({ ...base, count: ALL }).then((p) => ({
        ...p,
        rows: p.items.map((it) => ({ ...it, parent: null, subfolder: null })),
      }));
    }
    return folderFiles(base).then((p) => ({
      ...p,
      rows: p.files.map((f) => ({ ...f, folder: false })),
    }));
  }

  /** The first page of a new list scrolls to an item already selected. */
  let revealed = false;

  /** The set of rows shown. Starting a set (a fresh list or a refresh)
   * supersedes replies for the one before, and teardown drops them all. */
  const sets = latest();
  /** The set rows are added to as they scroll into view, and the revision
   * it belongs to; `null` before the first. */
  let shownSet = $state.raw<{ ticket: Ticket; revision: number } | null>(null);
  /** Pages of the shown set fetched or on their way. */
  let loaded = new Set<number>();
  /** A refresh is fetching the next set; pages wait for it. */
  let refreshing = $state(false);
  const refreshes = new Coalescer();
  onDestroy(() => refreshes.dispose());

  /** Everything that decides which rows a set holds and in what order. A
   * reply that says which revision it ranked can be checked against it
   * with `sets.accepts`. */
  const setKey = (rev: number) => queryKey(generation, folder, search, folders, rev);

  /** The pages covering the visible rows. */
  function visiblePages(): number[] {
    const pages: number[] = [];
    for (let p = Math.floor(first / PAGE); p * PAGE < Math.max(last, 1); p++) pages.push(p);
    return pages;
  }

  // A different folder, filter, mode, or scan starts a new set at the top.
  let shown = "";
  $effect(() => {
    const key = queryKey(generation, folder, search, folders);
    if (key === shown) return;
    shown = key;
    rows = new Map();
    total = null;
    empty = 0;
    error = null;
    revealed = false;
    const rev = untrack(() => revision);
    refreshes.clear();
    loaded = new Set();
    shownSet = { ticket: sets.begin(setKey(rev)), revision: rev };
    list?.reset();
  });

  // Fetch the pages of the shown set that come into view.
  $effect(() => {
    const set = shownSet;
    void attempt;
    if (!set || refreshing) return;
    for (const p of visiblePages()) {
      if (loaded.has(p)) continue;
      loaded.add(p);
      fetchPage(p * PAGE).then(
        (page) => {
          if (sets.current(set.ticket)) addPage(page);
        },
        (e: unknown) => {
          if (!sets.current(set.ticket)) return;
          loaded.delete(p);
          error = errorText(e);
        },
      );
    }
  });

  function addPage(page: Page) {
    const next = new Map(rows);
    page.rows.forEach((r, i) => next.set(page.offset + i, r));
    // Rows past a shrunken end would linger otherwise.
    for (const i of next.keys()) if (i >= page.total) next.delete(i);
    rows = next;
    total = page.total;
    empty = page.empty;
    error = null;
    revealOnce();
  }

  /** Scrolls to an item already selected, the first time rows arrive. */
  function revealOnce() {
    if (revealed || rows.size === 0) return;
    revealed = true;
    let at = -1;
    for (const [i, r] of rows) if (selected.has(r.node) && (at < 0 || i < at)) at = i;
    // After the list has grown to its new length.
    if (at >= 0) void tick().then(() => list?.scrollToRow(at));
  }

  // A newer revision refetches what's visible as one set.
  $effect(() => {
    const rev = revision;
    const set = untrack(() => shownSet);
    if (!set || set.revision === rev) return;
    // The refresh reads what it needs when it starts; nothing it reads
    // should rerun this.
    untrack(() => refreshes.run(refresh));
  });

  /** Pages a refresh fetches at most besides the visible ones. */
  const KEEP_PAGES = 10;

  /** Fetches the visible pages for the newest revision and replaces the
   * rows with them, all at once. Pages loaded earlier come along while
   * there are few, so ranges and Select all still reach them; otherwise
   * rows out of view are dropped and fetched again when they come into
   * view. */
  async function refresh() {
    const rev = revision;
    if (shownSet?.revision === rev) return;
    const ticket = sets.begin(setKey(rev));
    const visible = visiblePages();
    const pages = loaded.size <= KEEP_PAGES ? [...new Set([...loaded, ...visible])].sort((a, b) => a - b) : visible;
    refreshing = true;
    try {
      const got = await Promise.all(pages.map((p) => fetchPage(p * PAGE)));
      if (!sets.current(ticket)) return;
      const next = new Map<number, ContentsRow>();
      for (const page of got) page.rows.forEach((r, i) => next.set(page.offset + i, r));
      const end = got[got.length - 1];
      if (end) {
        for (const i of next.keys()) if (i >= end.total) next.delete(i);
        total = end.total;
        empty = end.empty;
      }
      rows = next;
      error = null;
      revealOnce();
      loaded = new Set(pages);
      shownSet = { ticket, revision: rev };
    } catch (e) {
      if (!sets.current(ticket)) return;
      // The rows shown stay as they were; pages coming into view are
      // fetched for the new revision.
      error = errorText(e);
      loaded = new Set();
      shownSet = { ticket, revision: rev };
    } finally {
      refreshing = false;
    }
  }

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
    loaded = new Set();
    attempt++;
  }

  function open(r: ContentsRow) {
    if (r.folder) onopen(r.node);
    else if (r.parent !== null && r.parent !== folder) onshow(r.node, r.parent);
  }

  function rowTitle(r: ContentsRow): string {
    if (r.folder) return `${r.name} (folder; double-click or Enter opens it)`;
    if (r.subfolder) return `${r.subfolder}${sep(r.subfolder)}${r.name} (Enter shows it in the map)`;
    return r.name;
  }

  const sep = (path: string) => (path.includes("\\") ? "\\" : "/");

  function sizeTitle(r: ContentsRow): string {
    const share = folderSize > 0 ? `, ${percent(r.weight, folderSize)} of ${name}` : "";
    return `${exactBytes(r.weight)}${share}`;
  }

  const what = $derived(folders ? "item" : "file");
</script>

<!-- Only needed while files and folders are mixed. -->
{#snippet kindCell(r: ContentsRow)}
  {r.folder ? "folder" : "file"}
{/snippet}

<section class="contents" aria-label="Contents of {name}">
  <header class="panel-head">
    <h3>Contents</h3>
    {#if total !== null}
      <span class="muted small count">{plural(total, what)} · largest first</span>
    {/if}
    <label class="toggle small" title="Uncheck to list only files, from every folder under this one">
      <input type="checkbox" checked={folders} onchange={(e) => onfolders(e.currentTarget.checked)} />
      Folders
    </label>
  </header>
  {#key folders}
    <ItemList
      bind:this={list}
      total={total ?? 0}
      item={(i) => rows.get(i)}
      node={(r) => r.node}
      label={folders ? `Contents of ${name}, largest first` : `Files under ${name}, largest first`}
      rowHeight={folders ? 30 : 44}
      title={rowTitle}
      {selected}
      {anchor}
      {indexOf}
      onvisible={(f, l) => {
        first = f;
        last = l;
      }}
      {onpick}
      onopen={open}
      {onmenu}
      {ondelete}
      kind={folders ? kindCell : undefined}
    >
      {#snippet main(r)}
        <span class="row-name name">{r.name}</span>
        {#if r.subfolder !== null}
          <span class="row-name subfolder">{r.subfolder || "Directly in this folder"}</span>
        {/if}
      {/snippet}
      {#snippet size(r)}
        <span title={sizeTitle(r)}>{formatBytes(r.weight)}</span>
      {/snippet}
      {#snippet empty()}
        <p class="muted">
          {#if total === null}Loading…{:else if folders}Nothing here has any size.{:else}No files with any size
            here.{/if}
        </p>
      {/snippet}
    </ItemList>
  {/key}
  {#if empty > 0 || error}
    <footer>
      {#if empty > 0}
        <p class="muted small">
          {plural(empty, `other ${what}`)}
          {folders ? "here" : "under this folder"}
          {empty === 1 ? "has" : "have"} no size, so {empty === 1 ? "it isn't" : "they aren't"} listed. Use the filter
          to find them by name.
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
  .count {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .toggle {
    display: flex;
    flex: none;
    align-items: center;
    gap: var(--space-1);
    margin-left: auto;
  }
  .name,
  .subfolder {
    display: block;
  }
  .subfolder {
    font-size: var(--fs-xs);
    color: var(--muted);
  }
  footer {
    flex: none;
  }
  footer p {
    margin: var(--space-2) var(--space-3);
  }
</style>
