<script lang="ts">
  // Matching files, largest first. Rows are fetched a page at a time as they
  // scroll into view; the list refetches what's visible when the scan or the
  // search changes.
  import { announce } from "./announce.svelte";
  import { errorText, searchResults } from "./api";
  import { exactBytes, formatBytes, formatCount, percent, plural } from "./format";
  import ItemList from "./ItemList.svelte";
  import type { Metric } from "./protocol/Metric";
  import type { SearchRow } from "./protocol/SearchRow";
  import type { SearchSummary } from "./protocol/SearchSummary";
  import type { Pick } from "./selection";

  interface Props {
    generation: number;
    summary: SearchSummary;
    /** The size measure rows are ordered by; the interface uses file size. */
    metric?: Metric;
    /** Scan revision; a change refetches the visible rows. */
    revision: number;
    scanning: boolean;
    /** Unfiltered size of the scan, for comparison. */
    scanned: number;
    selected: Set<number>;
    /** Where Shift ranges start. */
    anchor: number | null;
    onpick: (pick: Pick) => void;
    /** The Delete key. */
    ondelete: () => void;
    /** Open the containing folder in the map and select the file. */
    onshow: (row: SearchRow) => void;
    onmenu: (row: SearchRow, x: number, y: number) => void;
  }

  let {
    generation,
    summary,
    metric = "logical",
    revision,
    scanning,
    scanned,
    selected,
    anchor,
    onpick,
    ondelete,
    onshow,
    onmenu,
  }: Props = $props();

  const uid = $props.id();
  const ROW = 44;
  const PAGE = 100;
  /** Ranges longer than this ask before loading and selecting. */
  const CONFIRM = 10_000;

  let list: ReturnType<typeof ItemList<SearchRow>> | undefined = $state();
  let rows = $state.raw(new Map<number, SearchRow>());
  let total = $state(0);
  let first = $state(0);
  let last = $state(0);
  let error = $state<string | null>(null);
  /** Bumped by Retry, so failed pages are fetched again. */
  let attempt = $state(0);
  /** A long Shift range waiting for an answer. */
  let ask = $state<{ count: number; answer: (yes: boolean) => void } | null>(null);
  /** Rows loaded so far for a Shift range. */
  let progress = $state<{ done: number; count: number } | null>(null);

  // A different search or scan starts at the top.
  let shown = "";
  $effect(() => {
    const key = `${generation}/${summary.search}/${metric}`;
    if (key === shown) return;
    shown = key;
    rows = new Map();
    total = summary.files;
    error = null;
    list?.reset();
  });

  // Fetch the pages covering the visible rows; refetch on every revision.
  let loaded = new Set<string>();
  $effect(() => {
    const want = { generation, search: summary.search, metric };
    const tag = `${generation}/${summary.search}/${metric}/${revision}/${attempt}`;
    for (let p = Math.floor(first / PAGE); p * PAGE < Math.max(last, 1); p++) {
      const key = `${tag}/${p}`;
      if (loaded.has(key)) continue;
      loaded.add(key);
      searchResults({ ...want, offset: p * PAGE, limit: PAGE }).then(
        (page) => {
          if (
            page.generation !== generation ||
            page.search !== summary.search ||
            want.metric !== metric
          ) {
            return;
          }
          const next = new Map(rows);
          page.rows.forEach((r, i) => next.set(page.offset + i, r));
          rows = next;
          total = page.total;
          error = null;
        },
        (e: unknown) => {
          loaded.delete(key);
          if (want.search === summary.search) error = errorText(e);
        },
      );
    }
    if (loaded.size > 200) loaded = new Set([...loaded].slice(-50));
  });

  // Say the total once per search, and again once the scan has finished.
  let said = "";
  $effect(() => {
    const key = `${generation}/${summary.search}/${scanning}`;
    if (key === said) return;
    said = key;
    const files = plural(summary.files, "matching file");
    announce(scanning ? `${files} so far` : files);
  });

  function indexOf(node: number): number {
    for (const [i, r] of rows) if (r.node === node) return i;
    return -1;
  }

  /** Asks inline whether to go on with a long range. */
  function confirmRange(count: number, signal: AbortSignal): Promise<boolean> {
    return new Promise((resolve) => {
      const answer = (yes: boolean) => {
        ask = null;
        signal.removeEventListener("abort", stop);
        resolve(yes);
        list?.focus();
      };
      const stop = () => answer(false);
      signal.addEventListener("abort", stop);
      ask = { count, answer };
    });
  }

  /** Matching files from row `from` to row `to`, loading rows not yet seen. */
  async function rangeNodes(from: number, to: number, signal: AbortSignal): Promise<number[] | null> {
    const lo = Math.min(from, to);
    const hi = Math.max(from, to);
    const count = hi - lo + 1;
    if (count > CONFIRM && !(await confirmRange(count, signal))) return null;
    const want = { generation, search: summary.search, metric };
    const found = new Map(rows);
    try {
      for (let i = lo; i <= hi; i++) {
        if (signal.aborted) return null;
        if (found.has(i)) continue;
        progress = { done: i - lo, count };
        const page = await searchResults({ ...want, offset: i, limit: Math.min(500, hi - i + 1) });
        if (page.generation !== generation || page.search !== summary.search) return null;
        page.rows.forEach((r, k) => found.set(page.offset + k, r));
        if (page.rows.length === 0) break;
      }
    } catch (e) {
      error = errorText(e);
      return null;
    } finally {
      progress = null;
    }
    const nodes: number[] = [];
    for (let i = lo; i <= hi; i++) {
      const r = found.get(i);
      if (r) nodes.push(r.node);
    }
    return nodes;
  }

  function retry() {
    error = null;
    attempt++;
  }

  const sep = (folder: string) => (folder.includes("\\") ? "\\" : "/");

  function fullPath(r: SearchRow): string {
    return r.folder ? `${r.folder}${sep(r.folder)}${r.name}` : `${r.name} (in the scan root)`;
  }

  function sizeTitle(r: SearchRow): string {
    const note = r.alias ? ". A hard link: another name for a file also counted at its first name" : "";
    return `${exactBytes(r.logical)}${note}`;
  }
</script>

<section class="results" aria-label="Search results">
  <header>
    <div class="panel-head">
      <h3>{plural(summary.files, "matching file")}</h3>
      <span class="muted small">· largest first</span>
    </div>
    <p class="small">
      {formatBytes(summary.logical)}{#if scanned > 0}
        · {percent(summary.logical, scanned)} of {formatBytes(scanned)} scanned{/if}
    </p>
    {#if scanning}
      <p class="small note">Still scanning: more matches may appear.</p>
    {/if}
  </header>
  {#if ask}
    <div class="bar small" role="group" aria-label="Confirm selection">
      <span id="{uid}-question">Select {formatCount(ask.count)} items?</span>
      <!-- svelte-ignore a11y_autofocus -->
      <button type="button" autofocus aria-describedby="{uid}-question" onclick={() => ask?.answer(true)}>
        Select
      </button>
      <button type="button" onclick={() => ask?.answer(false)}>Cancel</button>
    </div>
  {:else if progress}
    <p class="bar small muted">Loading {formatCount(progress.done)} of {formatCount(progress.count)} items…</p>
  {/if}
  <ItemList
    bind:this={list}
    {total}
    item={(i) => rows.get(i)}
    node={(r) => r.node}
    label="Matching files, largest first"
    rowHeight={ROW}
    title={fullPath}
    {selected}
    {anchor}
    {indexOf}
    range={rangeNodes}
    onvisible={(f, l) => {
      first = f;
      last = l;
    }}
    {onpick}
    onopen={(r) => onshow(r)}
    {onmenu}
    {ondelete}
  >
    {#snippet main(r)}
      <span class="row-name name">{r.name}</span>
      <span class="folder"><span dir="ltr">{r.folder || "Scan root"}</span></span>
    {/snippet}
    {#snippet size(r)}
      <span title={sizeTitle(r)}>{formatBytes(r.logical)}</span>
    {/snippet}
    {#snippet empty()}
      <p class="muted">
        {scanning ? "No matching files found yet." : `No file names match “${summary.query}”.`}
        The filter matches file names, not folders or paths.
      </p>
    {/snippet}
  </ItemList>
  {#if error}
    <p class="error small failed" role="alert">
      Couldn't load results: {error}
      <button type="button" class="link" onclick={retry}>Retry</button>
    </p>
  {/if}
</section>

<style>
  .results {
    display: flex;
    flex-direction: column;
    flex: 1;
    min-height: 0;
  }
  header {
    flex: none;
    padding-bottom: var(--space-2);
    border-bottom: 1px solid var(--line);
  }
  header p {
    margin: 0;
    padding: 0 var(--space-3);
  }
  .note {
    color: var(--accent);
  }
  .bar {
    flex: none;
    display: flex;
    align-items: center;
    gap: var(--space-2);
    margin: 0;
    padding: var(--space-1) var(--space-3);
    border-bottom: 1px solid var(--line);
  }
  .name {
    display: block;
  }
  /* Long folder paths lose their start, keeping the distinctive end. */
  .folder {
    display: block;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    direction: rtl;
    text-align: left;
    font-size: var(--fs-xs);
    color: var(--muted);
  }
  .failed {
    flex: none;
    margin: 0;
    padding: var(--space-2) var(--space-3);
  }
</style>
