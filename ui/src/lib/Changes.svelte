<script lang="ts">
  // What changed under the folder the map shows since the earlier scan
  // being compared with: every grown, shrunk, new, or removed item at any
  // depth, largest change first. Removed items aren't in the map, so they
  // can't be selected. Loads a page at a time: "Show more" adds the next
  // page, and a scan change (a delete) reloads the pages already shown.
  import { untrack } from "svelte";
  import { compareChanges, errorText } from "./api";
  import { formatDelta } from "./compare";
  import { exactBytes, formatBytes, formatCount, plural } from "./format";
  import ItemList from "./ItemList.svelte";
  import type { ChangeRow } from "./protocol/ChangeRow";
  import type { ChangesPage } from "./protocol/ChangesPage";
  import type { Pick } from "./selection";

  interface Props {
    generation: number;
    /** The earlier scan compared with. */
    snapshot: string;
    /** When it was, e.g. "Oct 1, 2:03 PM". */
    since: string;
    /** The folder the map shows. */
    folder: number;
    folderName: string;
    /** Scan revision; a change refreshes the loaded rows. */
    revision: number;
    /** List folders too; otherwise only files. */
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
    /** Show a file in the map: open its folder, select it. */
    onshow: (node: number, parent: number) => void;
  }

  let {
    generation,
    snapshot,
    since,
    folder,
    folderName,
    revision,
    folders,
    onfolders,
    selected,
    anchor,
    onpick,
    ondelete,
    onopen,
    onshow,
  }: Props = $props();

  const PAGE = 200;

  /** A row, with an id for the list: its node, or a negative stand-in for
   * a removed item, which has none. */
  type Row = ChangeRow & { id: number };

  let pages = $state.raw<ChangesPage[]>([]);
  const rows = $derived(
    pages.flatMap((p) => p.rows.map((r, i): Row => ({ ...r, id: r.node ?? -(p.offset + i + 1) }))),
  );
  /** Changed items in all; `null` until the first page arrives. */
  let total = $state<number | null>(null);
  let loading = $state(false);
  let error = $state<string | null>(null);
  /** Bumped by Retry. */
  let attempt = $state(0);
  /** Responses to older requests are dropped. */
  let seq = 0;

  const name = $derived(folderName || "this folder");

  function fetchPages(offset: number, n: number): Promise<ChangesPage[]> {
    const base = { generation, snapshot, folder, filesOnly: !folders, limit: PAGE };
    return Promise.all(Array.from({ length: n }, (_, i) => compareChanges({ ...base, offset: offset + i * PAGE })));
  }

  function settle(mine: number, next: ChangesPage[]) {
    if (mine !== seq) return;
    const last = next[next.length - 1];
    if (!last || last.generation !== generation || last.folder !== folder) return;
    pages = next;
    total = last.total;
    error = null;
  }

  function fail(mine: number, e: unknown) {
    if (mine === seq) error = errorText(e);
  }

  // Load the first page, and reload as many pages as are shown when the
  // scan changes.
  $effect(() => {
    void [revision, attempt, folders];
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

  /** Removed items can't be selected: they aren't in the scan. */
  function pick(p: Pick) {
    if (p.focus.node < 0) return;
    onpick({ ...p, nodes: p.nodes.filter((n) => n >= 0) });
  }

  function open(r: Row) {
    if (r.node === null) return;
    if (r.folder) onopen(r.node);
    else if (r.parent !== null) onshow(r.node, r.parent);
  }

  function kindLabel(r: Row): string {
    if (r.kind === "removed") return "removed";
    if (r.kind === "new") return "new";
    return r.folder ? "folder" : "file";
  }

  function rowTitle(r: Row): string {
    const where = r.subfolder ? ` in ${r.subfolder}` : "";
    const what = `${r.folder ? "Folder" : "File"} ${r.name}${where}`;
    if (r.kind === "removed") return `${what}: removed since ${since}`;
    if (r.kind === "new") return `${what}: new since ${since}`;
    const hint = r.folder ? "Enter opens it" : "Enter shows it in the map";
    return `${what}: ${formatBytes(r.before ?? 0)} on ${since}, ${formatBytes(r.after ?? 0)} now (${hint})`;
  }

  function sizeTitle(r: Row): string {
    const before = r.before ?? 0;
    const after = r.after ?? 0;
    const sign = after >= before ? "+" : "−";
    return `${sign}${exactBytes(Math.abs(after - before))}`;
  }

  const left = $derived(total === null ? 0 : Math.min(PAGE, total - rows.length));
  const what = $derived(folders ? "item" : "file");
</script>

{#snippet kindCell(r: Row)}
  <span class="kind {r.kind}">{kindLabel(r)}</span>
{/snippet}

<section class="changes" aria-label="Changes in {name} since {since}">
  <header class="panel-head">
    <h3>Changes</h3>
    <span class="muted small count">
      {#if total !== null}{plural(total, `changed ${what}`)} ·{/if} since {since}
    </span>
    <label class="toggle small" title="Uncheck to list only files">
      <input type="checkbox" checked={folders} onchange={(e) => onfolders(e.currentTarget.checked)} />
      Folders
    </label>
  </header>
  <ItemList
    total={rows.length}
    item={(i) => rows[i]}
    node={(r) => r.id}
    label="Changes in {name} since {since}, largest first"
    rowHeight={44}
    title={rowTitle}
    {selected}
    {anchor}
    onpick={pick}
    onopen={open}
    {ondelete}
    kind={kindCell}
  >
    {#snippet main(r)}
      <span class="row-name name" class:gone={r.kind === "removed"}>{r.name}</span>
      <span class="row-name subfolder">
        {r.folder && (r.kind === "new" || r.kind === "removed") ? "Folder · " : ""}{r.subfolder ||
          "Directly in this folder"}
      </span>
    {/snippet}
    {#snippet size(r)}
      <span class="delta {r.kind}" title={sizeTitle(r)}>{formatDelta(r.before ?? 0, r.after ?? 0)}</span>
    {/snippet}
    {#snippet empty()}
      <p class="muted">
        {#if total === null}Loading…{:else}No size changes in {name} since {since}.{/if}
      </p>
    {/snippet}
  </ItemList>
  <footer>
    {#if total !== null && rows.length < total}
      <button type="button" class="more" disabled={loading} onclick={more}>
        {loading ? "Loading…" : `Show ${formatCount(left)} more`}
      </button>
    {/if}
    {#if error}
      <p class="error small" role="alert">
        Couldn't list the changes: {error}
        <button type="button" class="link" onclick={retry}>Retry</button>
      </p>
    {/if}
  </footer>
</section>

<style>
  .changes {
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
  .gone {
    text-decoration: line-through;
    color: var(--muted);
  }
  .kind.new,
  .delta.new {
    color: var(--change-new);
  }
  .delta.grown {
    color: var(--change-grown);
  }
  .delta.shrunk,
  .delta.removed {
    color: var(--change-shrunk);
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
