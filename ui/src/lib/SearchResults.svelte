<script lang="ts">
  // Matching files, largest first. Only the rows in view are rendered, and
  // rows are fetched a page at a time; the list refetches what's visible
  // when the scan, search, or size measure changes.
  import { searchResults } from "./api";
  import { exactBytes, formatBytes, formatCount, plural } from "./format";
  import type { Metric } from "./protocol/Metric";
  import type { SearchRow } from "./protocol/SearchRow";
  import type { SearchSummary } from "./protocol/SearchSummary";

  interface Props {
    generation: number;
    summary: SearchSummary;
    metric: Metric;
    /** Scan revision; a change refetches the visible rows. */
    revision: number;
    scanning: boolean;
    /** Unfiltered size of the scan, for comparison. */
    scanned: number;
    selected: number | null;
    onselect: (node: number) => void;
    /** Open the containing folder in the map and select the file. */
    onshow: (row: SearchRow) => void;
    onmenu: (row: SearchRow, x: number, y: number) => void;
  }

  let {
    generation,
    summary,
    metric,
    revision,
    scanning,
    scanned,
    selected,
    onselect,
    onshow,
    onmenu,
  }: Props = $props();

  const ROW = 44;
  const PAGE = 100;

  let list: HTMLDivElement;
  let scrollTop = $state(0);
  let viewport = $state(0);
  let rows = $state.raw(new Map<number, SearchRow>());
  let total = $state(0);
  let active = $state(0);
  let error = $state<string | null>(null);

  // A different search, measure, or scan starts at the top.
  let shown = "";
  $effect(() => {
    const key = `${generation}/${summary.search}/${metric}`;
    if (key === shown) return;
    shown = key;
    rows = new Map();
    total = summary.files;
    active = 0;
    if (list) list.scrollTop = 0;
    scrollTop = 0;
  });

  const first = $derived(Math.max(0, Math.floor(scrollTop / ROW) - 5));
  const last = $derived(Math.min(total, Math.ceil((scrollTop + viewport) / ROW) + 5));

  // Fetch the pages covering the visible rows; refetch on every revision.
  let loaded = new Set<string>();
  $effect(() => {
    const want = { generation, search: summary.search, metric };
    const tag = `${generation}/${summary.search}/${metric}/${revision}`;
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
          if (want.search === summary.search) error = String(e);
        },
      );
    }
    if (loaded.size > 200) loaded = new Set([...loaded].slice(-50));
  });

  $effect(() => {
    const observer = new ResizeObserver(([entry]) => {
      if (entry) viewport = entry.contentRect.height;
    });
    observer.observe(list);
    return () => observer.disconnect();
  });

  function size(r: SearchRow): string {
    if (metric === "logical") return formatBytes(r.logical);
    if (r.alias) return "hard link";
    return r.allocated === null ? "unknown" : formatBytes(r.allocated);
  }

  function sizeTitle(r: SearchRow): string {
    const allocated = r.allocated === null ? "unknown" : exactBytes(r.allocated);
    const note = r.alias ? " (counted at another name of this hard-linked file)" : "";
    return `Allocated ${allocated}${note} · Logical ${exactBytes(r.logical)}`;
  }

  function moveTo(index: number) {
    if (total === 0) return;
    active = Math.max(0, Math.min(total - 1, index));
    const top = active * ROW;
    if (top < list.scrollTop) list.scrollTop = top;
    else if (top + ROW > list.scrollTop + viewport) list.scrollTop = top + ROW - viewport;
    const row = rows.get(active);
    if (row) onselect(row.node);
  }

  function onKey(e: KeyboardEvent) {
    const page = Math.max(1, Math.floor(viewport / ROW) - 1);
    const keys: Record<string, () => void> = {
      ArrowDown: () => moveTo(active + 1),
      ArrowUp: () => moveTo(active - 1),
      PageDown: () => moveTo(active + page),
      PageUp: () => moveTo(active - page),
      Home: () => moveTo(0),
      End: () => moveTo(total - 1),
      Enter: () => {
        const row = rows.get(active);
        if (row) onshow(row);
      },
    };
    const action = keys[e.key];
    if (action) {
      action();
      e.preventDefault();
    } else if (e.key === "ContextMenu" || (e.key === "F10" && e.shiftKey)) {
      const row = rows.get(active);
      if (row) {
        const r = list.getBoundingClientRect();
        onmenu(row, r.left + 24, r.top + (active + 0.5) * ROW - list.scrollTop);
      }
      e.preventDefault();
    }
  }

  function onContextMenu(e: MouseEvent, index: number, row: SearchRow) {
    e.preventDefault();
    active = index;
    onselect(row.node);
    list.focus();
    onmenu(row, e.clientX, e.clientY);
  }

  const share = $derived.by(() => {
    const matched = metric === "allocated" ? summary.allocated : summary.logical;
    return scanned > 0 ? Math.round((matched / scanned) * 1000) / 10 : 0;
  });
</script>

<section class="results" aria-label="Search results">
  <header>
    <strong>{plural(summary.files, "matching file")}</strong>
    <span>
      {formatBytes(metric === "allocated" ? summary.allocated : summary.logical)}
      {metric}{scanned > 0 ? ` (${share}% of ${formatBytes(scanned)} scanned)` : ""}
    </span>
    {#if summary.unknownAllocationFiles > 0 && metric === "allocated"}
      <span class="muted">{plural(summary.unknownAllocationFiles, "match")} with unknown allocation</span>
    {/if}
    {#if scanning}
      <span class="note">Still scanning: more matches may appear.</span>
    {/if}
  </header>
  <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
  <div
    class="list"
    bind:this={list}
    role="listbox"
    tabindex="0"
    aria-label="Matching files, largest first. Enter shows the file in the map."
    aria-activedescendant={total > 0 ? `result-${active}` : undefined}
    onscroll={() => (scrollTop = list.scrollTop)}
    onkeydown={onKey}
  >
    <div class="spacer" style:height="{total * ROW}px">
      {#each { length: last - first } as _, k (first + k)}
        {@const i = first + k}
        {@const row = rows.get(i)}
        <div
          id="result-{i}"
          class="row"
          class:active={i === active}
          class:selected={row !== undefined && row.node === selected}
          role="option"
          aria-selected={row !== undefined && row.node === selected}
          tabindex="-1"
          style:top="{i * ROW}px"
          onclick={() => {
            active = i;
            if (row) onselect(row.node);
            list.focus();
          }}
          ondblclick={() => row && onshow(row)}
          oncontextmenu={(e) => row && onContextMenu(e, i, row)}
          onkeydown={() => {}}
        >
          {#if row}
            <div class="line">
              <span class="name" title={row.name}>{row.name}</span>
              <span class="size" title={sizeTitle(row)}>{size(row)}</span>
            </div>
            <div class="folder" title={row.folder || "Scan root"}>{row.folder || "Scan root"}</div>
          {:else}
            <div class="line muted">Loading…</div>
          {/if}
        </div>
      {/each}
    </div>
    {#if total === 0}
      <p class="muted empty">
        {scanning ? "No matching files found yet." : `No file names contain “${summary.query}”.`}
      </p>
    {/if}
  </div>
  {#if error}
    <p class="error" role="alert">Couldn't load results: {error}</p>
  {/if}
  <p class="sr-only" aria-live="polite">{formatCount(summary.files)} matches</p>
</section>

<style>
  .results {
    display: flex;
    flex-direction: column;
    min-height: 0;
    flex: 1;
    border-top: 1px solid var(--line);
  }
  header {
    display: flex;
    flex-direction: column;
    gap: 2px;
    padding: 8px 12px;
    font-size: 12px;
  }
  header strong {
    font-size: 13px;
  }
  .note {
    color: var(--accent);
  }
  .list {
    position: relative;
    flex: 1;
    min-height: 120px;
    overflow-y: auto;
    border-top: 1px solid var(--line);
  }
  .list:focus {
    outline: none;
  }
  .list:focus-visible .row.active {
    box-shadow: inset 0 0 0 2px var(--accent);
  }
  .spacer {
    position: relative;
  }
  .row {
    position: absolute;
    left: 0;
    right: 0;
    height: 44px;
    box-sizing: border-box;
    padding: 4px 12px;
    cursor: default;
    border-bottom: 1px solid var(--line);
  }
  .row:hover {
    background: var(--hover);
  }
  .row.selected {
    background: var(--hover);
    font-weight: 600;
  }
  .line {
    display: flex;
    justify-content: space-between;
    gap: 8px;
  }
  .name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .size {
    flex: none;
    font-variant-numeric: tabular-nums;
  }
  .folder {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    font-size: 11px;
    color: var(--muted);
    font-weight: normal;
  }
  .muted {
    color: var(--muted);
  }
  .empty {
    padding: 12px;
    margin: 0;
  }
  .error {
    margin: 0;
    padding: 6px 12px;
    color: var(--danger);
  }
  .sr-only {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip-path: inset(50%);
  }
</style>
