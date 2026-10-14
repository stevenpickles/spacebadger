<script lang="ts">
  // Groups of files with the same contents, most reclaimable space first,
  // each a header row followed by its files (shortest path first) unless
  // folded. Header rows stand for no item: picks on them are dropped, and
  // Enter or a click on the header folds the group. The list is fetched
  // again whenever the search finds more or items are deleted.
  import { untrack } from "svelte";
  import { announce } from "./announce.svelte";
  import { duplicatesList, errorText } from "./api";
  import { exactBytes, formatBytes, formatCount, formatElapsed, KILO, percent, plural } from "./format";
  import ItemList from "./ItemList.svelte";
  import type { DuplicateFile } from "./protocol/DuplicateFile";
  import type { DuplicateGroup } from "./protocol/DuplicateGroup";
  import type { DuplicatesList } from "./protocol/DuplicatesList";
  import type { DuplicatesStatus } from "./protocol/DuplicatesStatus";
  import type { Pick } from "./selection";

  interface Props {
    generation: number;
    status: DuplicatesStatus;
    /** Smallest file size looked at; changing it searches again. */
    minSize: number;
    /** Searching again needs a finished scan. */
    canSearch: boolean;
    selected: Set<number>;
    /** Where Shift ranges start. */
    anchor: number | null;
    onpick: (pick: Pick) => void;
    /** The Delete key. */
    ondelete: () => void;
    /** Open the file's folder in the map with the file selected. */
    onshow: (file: DuplicateFile) => void;
    onmenu: (file: DuplicateFile, x: number, y: number) => void;
    /** Search again, with this smallest size. */
    onsearch: (minSize: number) => void;
    oncancel: () => void;
    onclose: () => void;
    /** Clears the selection, when the keyboard moves onto a group header. */
    onclear: () => void;
  }

  let {
    generation,
    status,
    minSize,
    canSearch,
    selected,
    anchor,
    onpick,
    ondelete,
    onshow,
    onmenu,
    onsearch,
    oncancel,
    onclose,
    onclear,
  }: Props = $props();

  const SIZES = [
    { value: KILO, label: "1 KB" },
    { value: KILO ** 2, label: "1 MB" },
    { value: 10 * KILO ** 2, label: "10 MB" },
    { value: 100 * KILO ** 2, label: "100 MB" },
  ];

  type Row =
    | { kind: "group"; group: DuplicateGroup; index: number; key: number; open: boolean }
    | { kind: "file"; group: DuplicateGroup; file: DuplicateFile; first: boolean };

  let list = $state.raw<DuplicatesList | null>(null);
  let error = $state<string | null>(null);
  /** Folded groups, by their first file's node. */
  let folded = $state.raw(new Set<number>());

  // Fetch the groups when the search finds more or items are deleted; one
  // request at a time, and once more if anything changed meanwhile.
  let fetching = false;
  let stale = false;
  // Status updates are new objects; only these numbers say what changed.
  const job = $derived(status.job);
  const version = $derived(status.version);
  $effect(() => {
    void [generation, job, version];
    stale = true;
    if (!fetching) untrack(() => void fetchList());
  });

  async function fetchList() {
    fetching = true;
    while (stale) {
      stale = false;
      const gen = generation;
      const job = status.job;
      try {
        const next = await duplicatesList(gen, job);
        if (gen === generation && job === status.job) {
          list = next;
          error = null;
        }
      } catch (e) {
        if (gen === generation && job === status.job) error = errorText(e);
      }
    }
    fetching = false;
  }

  const groups = $derived(list?.job === status.job ? list.groups : []);

  const rows = $derived.by(() => {
    const out: Row[] = [];
    groups.forEach((group, index) => {
      const key = group.files[0]!.node;
      const open = !folded.has(key);
      out.push({ kind: "group", group, index, key, open });
      if (open) group.files.forEach((file, i) => out.push({ kind: "file", group, file, first: i === 0 }));
    });
    return out;
  });

  const running = $derived(status.phase === "running");
  const shownTotal = $derived(list?.job === status.job ? list.total : 0);

  // Say how it went once, when the search ends.
  let said = "";
  $effect(() => {
    const key = `${generation}/${status.job}/${status.phase}`;
    if (status.phase === "running" || key === said) return;
    said = key;
    const found =
      status.groups === 0
        ? "No duplicates found"
        : `${plural(status.groups, "group")} of duplicates, ${formatBytes(status.reclaimable)} of content in extra copies`;
    announce(status.phase === "cancelled" ? `Stopped. ${found} so far` : found);
  });

  /** Header rows have no node; they get negative numbers no item has. */
  function rowNode(r: Row): number {
    return r.kind === "group" ? -1 - r.index : r.file.node;
  }

  function toggle(key: number) {
    const next = new Set(folded);
    if (!next.delete(key)) next.add(key);
    folded = next;
  }

  /** Passes picks on files on. A header stands for no item: moving onto
   * one clears the selection, so Delete never acts on a file out of
   * sight, and a range keeps only the files in it. */
  function pick(p: Pick) {
    const nodes = p.nodes.filter((n) => n >= 0);
    if (p.focus.node >= 0) {
      onpick({ ...p, nodes });
    } else if (p.keepAnchor && nodes.length > 0) {
      onpick({ ...p, focus: { node: nodes[nodes.length - 1]!, other: false }, nodes });
    } else if (p.mode === "replace") {
      onclear();
    }
  }

  /** Selects every file but the first (shortest path) of each group. */
  function selectExtras() {
    const nodes = groups.flatMap((g) => g.files.slice(1).map((f) => f.node));
    if (nodes.length === 0) return;
    onpick({ focus: { node: nodes[0]!, other: false }, nodes, mode: "replace", keepAnchor: false });
    const size = groups.reduce((sum, g) => sum + g.size * (g.files.length - 1), 0);
    announce(`Selected ${plural(nodes.length, "file")}, ${formatBytes(size)} of content`);
  }

  function rowTitle(r: Row): string {
    if (r.kind === "group") {
      return `${plural(r.group.files.length, "file")} of ${exactBytes(r.group.size)} with the same contents. Enter ${r.open ? "folds" : "unfolds"} the group.`;
    }
    const notes = [
      r.first ? "shortest path in its group" : "",
      r.file.hardlinked ? "has other hard-link names, so deleting this one alone may free nothing" : "",
    ].filter(Boolean);
    return notes.length > 0 ? `${r.file.path} (${notes.join("; ")})` : r.file.path;
  }

  const sep = (path: string) => (path.includes("\\") ? "\\" : "/");

  function nameOf(path: string): string {
    return path.slice(path.lastIndexOf(sep(path)) + 1);
  }

  /** A file's folder as its start and its last two folders, so the middle
   * can be cut, as in the result list. */
  function splitFolder(path: string): [string, string] {
    const s = sep(path);
    const parts = path.split(s).slice(0, -1);
    if (parts.length <= 3) return ["", parts.join(s)];
    return [parts.slice(0, -2).join(s), s + parts.slice(-2).join(s)];
  }

  const skipped = $derived.by(() => {
    const s = status.skipped;
    return [
      s.cloud > 0 ? `${plural(s.cloud, "cloud file")} (reading could download ${s.cloud === 1 ? "it" : "them"})` : "",
      s.inUse > 0 ? `${formatCount(s.inUse)} in use by another program` : "",
      s.denied > 0 ? `${formatCount(s.denied)} without permission to read` : "",
      s.changed > 0 ? `${formatCount(s.changed)} changed since the scan` : "",
      s.changing > 0 ? `${formatCount(s.changing)} written to while being read` : "",
      s.special > 0
        ? `${formatCount(s.special)} stored in a special way (deduplicated, tiered, or virtualized), which reading raw would get wrong`
        : "",
      s.vanished > 0 ? `${formatCount(s.vanished)} gone since the scan` : "",
      s.unreadable > 0 ? `${formatCount(s.unreadable)} unreadable` : "",
    ].filter(Boolean);
  });

  /** What deleting the extra copies might free on disk, which can be far
   * less than their content: sparse or compressed files, other hard-link
   * names, and shared storage. */
  const onDisk = $derived.by(() => {
    if (status.onDiskUnknown === 0) return `up to ${formatBytes(status.reclaimableOnDisk)} on disk`;
    if (status.onDiskUnknown === status.groups) return "space on disk unknown";
    return `up to ${formatBytes(status.reclaimableOnDisk)} on disk in ${plural(status.groups - status.onDiskUnknown, "group")}; unknown for the rest`;
  });
  const onDiskTitle =
    "Deleting may free less than the content: sparse and compressed files take less space, a file with other hard-link names frees nothing until every name goes, and storage shared by block cloning or deduplication stays in use. Unknown where a copy's allocation is unknown or it has other names.";

  function groupTitle(g: DuplicateGroup): string {
    const content = `${exactBytes(g.size * (g.files.length - 1))} of content in the copies beyond the first`;
    const disk = g.onDisk === null ? "space on disk unknown" : `up to ${exactBytes(g.onDisk)} on disk`;
    return `${content}; ${disk}`;
  }

  const skippedCount = $derived(Object.values(status.skipped).reduce((a, b) => a + b, 0));
</script>

<section class="duplicates" aria-label="Duplicate files">
  <header>
    <div class="panel-head">
      <h3>Duplicates</h3>
      <span class="muted small where" title={status.path}>in {nameOf(status.path) || status.path}</span>
      <button type="button" class="icon-button close" onclick={onclose} aria-label="Close duplicates" title="Close">
        ✕
      </button>
    </div>
    {#if status.query}
      <p class="small muted">Found among files matching <code>{status.query}</code></p>
    {/if}
    <p class="small summary">
      {#if status.groups > 0}
        <strong title={exactBytes(status.reclaimable)}>{formatBytes(status.reclaimable)}</strong> of content in extra
        copies, in {plural(status.groups, "group")} ({formatCount(status.files)} files)
        <span class="muted" title={onDiskTitle}>· {onDisk}</span>
      {:else if running}
        Looking for files with the same contents…
      {:else}
        No duplicates found.
      {/if}
    </p>
    {#if running}
      <div class="progress small">
        <div class="bar" aria-hidden="true">
          <span style:width="{status.totalBytes > 0 ? (status.doneBytes / status.totalBytes) * 100 : 0}%"></span>
        </div>
        <span class="muted" title="Bytes of same-size files checked so far">
          {formatBytes(status.doneBytes)} of {formatBytes(status.totalBytes)} · {percent(
            status.doneBytes,
            status.totalBytes,
          )}
        </span>
        <button type="button" onclick={oncancel}>Stop</button>
      </div>
    {:else if status.phase === "cancelled"}
      <p class="small note">Stopped early: these are the groups found so far.</p>
    {:else if status.readBytes > 0}
      <p class="small muted">
        Read {formatBytes(status.readBytes)} of {plural(status.candidates, "same-size file")} in {formatElapsed(
          status.elapsedMs,
        )}.
      </p>
    {/if}
    <div class="controls small">
      <label title="Smaller files are left out. Changing this looks again.">
        At least
        <select
          value={minSize}
          disabled={!canSearch}
          onchange={(e) => onsearch(Number(e.currentTarget.value))}
        >
          {#each SIZES as s (s.value)}
            <option value={s.value}>{s.label}</option>
          {/each}
        </select>
      </label>
      {#if !running}
        <button
          type="button"
          disabled={!canSearch}
          title="Look again in the same folder, with the filter as it is now"
          onclick={() => onsearch(minSize)}>Look again</button
        >
      {/if}
      <button
        type="button"
        class="extras"
        disabled={groups.length === 0}
        title="Select every file except the one with the shortest path in each group. Nothing is deleted."
        onclick={selectExtras}>Select all but the first</button
      >
    </div>
  </header>
  <ItemList
    total={rows.length}
    item={(i) => rows[i]}
    node={rowNode}
    label="Groups of duplicate files, most reclaimable space first"
    rowHeight={40}
    title={rowTitle}
    {selected}
    {anchor}
    onpick={pick}
    onopen={(r) => (r.kind === "group" ? toggle(r.key) : onshow(r.file))}
    onmenu={(r, x, y) => r.kind === "file" && onmenu(r.file, x, y)}
    {ondelete}
  >
    {#snippet main(r)}
      {#if r.kind === "group"}
        <button
          type="button"
          class="group"
          tabindex="-1"
          aria-expanded={r.open}
          onclick={(e) => {
            e.stopPropagation();
            toggle(r.key);
          }}
          ondblclick={(e) => e.stopPropagation()}
        >
          <span class="caret" aria-hidden="true">{r.open ? "▾" : "▸"}</span>
          <span class="row-name">
            <strong>{formatCount(r.group.files.length)} copies</strong> of {nameOf(r.group.files[0]!.path)}
          </span>
          <span class="row-name each muted">{formatBytes(r.group.size)} each</span>
        </button>
      {:else}
        {@const [head, tail] = splitFolder(r.file.path)}
        <span class="file">
          <span class="row-name name">{nameOf(r.file.path)}{#if r.first}<span class="first">first</span>{/if}{#if r.file.hardlinked}<span class="first">has other names</span
            >{/if}</span>
          <span class="folder"
            >{#if head}<span class="head">{head}</span>{/if}<span class="tail">{tail}</span></span
          >
        </span>
      {/if}
    {/snippet}
    {#snippet size(r)}
      {#if r.kind === "group"}
        <span title={groupTitle(r.group)}
          >{formatBytes(r.group.size * (r.group.files.length - 1))}</span
        >
      {:else}
        <span class="muted" title={exactBytes(r.group.size)}>{formatBytes(r.group.size)}</span>
      {/if}
    {/snippet}
    {#snippet empty()}
      <p class="muted">
        {#if running}
          Groups appear here as they're confirmed, largest first.
        {:else if status.candidates === 0}
          No two files here of at least {formatBytes(minSize)} have the same size, so none can be
          duplicates.
        {:else}
          Files with the same size were found, but their contents differ.
        {/if}
      </p>
    {/snippet}
  </ItemList>
  {#if skippedCount > 0 || error || shownTotal > groups.length}
    <footer class="small">
      {#if shownTotal > groups.length}
        <p class="muted">
          Showing the {formatCount(groups.length)} groups with the most reclaimable space of {formatCount(shownTotal)}.
        </p>
      {/if}
      {#if skippedCount > 0}
        <p class="muted">Not read: {skipped.join("; ")}.</p>
      {/if}
      {#if error}
        <p class="error" role="alert">
          Couldn't list the duplicates: {error}
          <button type="button" class="link" onclick={() => void fetchList()}>Retry</button>
        </p>
      {/if}
    </footer>
  {/if}
</section>

<style>
  .duplicates {
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
  .where {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .close {
    flex: none;
    margin-left: auto;
  }
  .note {
    color: var(--accent);
  }
  .progress {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: var(--space-1) var(--space-3) 0;
  }
  .progress .bar {
    position: relative;
    flex: 1;
    height: 4px;
    overflow: hidden;
    border-radius: 2px;
    background: var(--line);
  }
  .progress .bar span {
    position: absolute;
    inset: 0 auto 0 0;
    background: var(--accent);
    transition: width 0.3s;
  }
  .controls {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-2);
    padding: var(--space-2) var(--space-3) 0;
  }
  .controls label {
    display: flex;
    align-items: center;
    gap: var(--space-1);
  }
  .extras {
    margin-left: auto;
  }
  .group {
    display: grid;
    grid-template-columns: auto minmax(0, 1fr);
    column-gap: var(--space-1);
    width: 100%;
    padding: 0;
    border: none;
    background: none;
    color: inherit;
    font: inherit;
    text-align: left;
  }
  .group .each {
    grid-column: 2;
    font-size: var(--fs-xs);
  }
  .caret {
    grid-row: span 2;
    align-self: center;
    width: 1em;
    color: var(--muted);
  }
  .file {
    display: block;
    padding-left: 1.5em;
  }
  .name {
    display: block;
  }
  /* Long folder paths lose their middle, as in the result list. */
  .folder {
    display: flex;
    min-width: 0;
    white-space: nowrap;
    font-size: var(--fs-xs);
    color: var(--muted);
  }
  .folder .head {
    flex: 0 1 auto;
    min-width: 2em;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .folder .tail {
    flex: 0 0 auto;
    max-width: calc(100% - 2em);
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .first {
    margin-left: var(--space-2);
    color: var(--muted);
    font-size: var(--fs-xs);
  }
  footer {
    flex: none;
  }
  footer p {
    margin: var(--space-2) var(--space-3);
  }
</style>
