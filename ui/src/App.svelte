<script lang="ts">
  import {
    appInfo,
    EXPECTED_PROTOCOL_VERSION,
    fileManagerName,
    nodeDetails,
    onScanStatus,
    scanCancel,
    scanChoose,
    scanRefresh,
    scanStatus,
    reveal,
    searchSet,
    searchSummary,
    volumeInfo,
  } from "./lib/api";
  import Breadcrumbs from "./lib/Breadcrumbs.svelte";
  import type { ColorMode } from "./lib/colors";
  import Legend from "./lib/Legend.svelte";
  import ContextMenu, { type MenuItem } from "./lib/ContextMenu.svelte";
  import Details from "./lib/Details.svelte";
  import { plural } from "./lib/format";
  import { KIND_OTHER, type DecodedLayout } from "./lib/layoutWire";
  import Omissions from "./lib/Omissions.svelte";
  import type { Metric } from "./lib/protocol/Metric";
  import type { NodeDetails } from "./lib/protocol/NodeDetails";
  import type { ScanStarted } from "./lib/protocol/ScanStarted";
  import type { ScanStatus } from "./lib/protocol/ScanStatus";
  import type { SearchRow } from "./lib/protocol/SearchRow";
  import type { SearchSummary } from "./lib/protocol/SearchSummary";
  import SearchResults from "./lib/SearchResults.svelte";
  import SmallItems from "./lib/SmallItems.svelte";
  import type { MenuRequest, Selection } from "./lib/selection";
  import StatusBar from "./lib/StatusBar.svelte";
  import Treemap from "./lib/Treemap.svelte";
  import type { VolumeInfo } from "./lib/protocol/VolumeInfo";
  import VolumeStrip from "./lib/VolumeStrip.svelte";

  const ROOT = 0;

  let generation = $state<number | null>(null);
  let status = $state.raw<ScanStatus | null>(null);
  let statusAt = $state(0);
  let now = $state(performance.now());
  let view = $state(ROOT);
  let selection = $state<Selection | null>(null);
  let metric = $state<Metric>("allocated");
  let colors = $state<ColorMode>(savedColors());
  let viewDetails = $state.raw<NodeDetails | null>(null);
  let selectedDetails = $state.raw<NodeDetails | null>(null);
  let layout = $state.raw<DecodedLayout | null>(null);
  let showOmissions = $state(false);
  let problem = $state<string | null>(null);
  let privileged = $state(false);
  let os = $state("");
  let volume = $state.raw<VolumeInfo | null>(null);
  let showVolume = $state(saved("volume") === "on");
  let fileManager = $state(fileManagerName(""));
  let menu = $state.raw<{ x: number; y: number; items: MenuItem[] } | null>(null);
  /** What's typed in the filter box. */
  let query = $state("");
  /** The search the map and results show; `null` when unfiltered. */
  let search = $state.raw<SearchSummary | null>(null);
  let searching = $state(false);
  let notice = $state<string | null>(null);

  const scanning = $derived(status?.phase.kind === "scanning");
  const elapsedMs = $derived(
    status ? status.elapsedMs + (scanning ? Math.max(0, now - statusAt) : 0) : 0,
  );

  appInfo().then(
    (info) => {
      privileged = info.privilegedAccess;
      os = info.os;
      fileManager = fileManagerName(info.os);
      if (info.protocolVersion !== EXPECTED_PROTOCOL_VERSION) {
        problem = `The backend speaks protocol ${info.protocolVersion}, but this interface expects ${EXPECTED_PROTOCOL_VERSION}.`;
      }
    },
    (e: unknown) => (problem = `Backend unavailable: ${String(e)}`),
  );

  // Display choices are per-viewer preferences; storage may be unavailable,
  // and then the defaults apply.
  function saved(key: string): string | null {
    try {
      return localStorage.getItem(key);
    } catch {
      return null;
    }
  }

  function savedColors(): ColorMode {
    return saved("colors") === "type" ? "type" : "depth";
  }

  function remember(key: string, value: string) {
    try {
      localStorage.setItem(key, value);
    } catch {
      // Not remembered.
    }
  }

  $effect(() => remember("colors", colors));
  $effect(() => remember("volume", showVolume ? "on" : "off"));

  /** Switches to a newer scan; view and selection start over at its root. */
  function adopt(next: number) {
    if (generation !== null && next <= generation) return;
    generation = next;
    status = null;
    view = ROOT;
    selection = null;
    viewDetails = null;
    selectedDetails = null;
    layout = null;
    menu = null;
    search = null;
    volume = null;
    // Keep the filter across Refresh and new roots.
    if (query) applySearch(query);
  }

  function receive(s: ScanStatus) {
    adopt(s.generation);
    if (s.generation !== generation) return;
    if (status && s.revision < status.revision && s.phase.kind === "scanning") return;
    status = s;
    statusAt = performance.now();
  }

  $effect(() => {
    const unlisten = onScanStatus(receive);
    scanStatus().then((s) => s && receive(s), () => {});
    return () => void unlisten.then((f) => f());
  });

  // Local clock so elapsed time keeps moving between coalesced updates.
  $effect(() => {
    if (!scanning) return;
    const timer = setInterval(() => (now = performance.now()), 250);
    return () => clearInterval(timer);
  });

  async function run(action: () => Promise<ScanStarted | null>) {
    try {
      const started = await action();
      if (started) adopt(started.generation);
    } catch (e) {
      problem = String(e);
    }
  }

  function cancel() {
    if (generation !== null) scanCancel(generation).catch((e: unknown) => (problem = String(e)));
  }

  // Volume capacity, read when a scan starts and again when it ends, since
  // free space may have changed meanwhile.
  const finished = $derived(status !== null && status.phase.kind !== "scanning");
  $effect(() => {
    const gen = generation;
    void finished;
    if (gen === null) return;
    volumeInfo(gen).then(
      (v) => {
        if (gen === generation) volume = v;
      },
      () => {},
    );
  });

  // ---- filename search ------------------------------------------------

  let searchTimer: ReturnType<typeof setTimeout> | undefined;
  /** Counts requests so only the newest reply is used. */
  let searchSeq = 0;

  function onQueryInput() {
    clearTimeout(searchTimer);
    searchTimer = setTimeout(() => applySearch(query), 250);
  }

  function clearQuery() {
    query = "";
    clearTimeout(searchTimer);
    applySearch("");
  }

  async function applySearch(text: string) {
    const gen = generation;
    if (gen === null) return;
    const seq = ++searchSeq;
    searching = text !== "";
    try {
      const summary = await searchSet(gen, text);
      if (seq !== searchSeq || gen !== generation) return;
      search = summary;
      menu = null;
    } catch (e) {
      // A newer search replaced this one; its reply will arrive instead.
      if (seq === searchSeq && gen === generation) problem = `Search failed: ${String(e)}`;
    } finally {
      if (seq === searchSeq) searching = false;
    }
  }

  const searchId = $derived(search?.search);

  // Matching totals follow the scan.
  $effect(() => {
    const gen = generation;
    const id = searchId;
    void status?.revision;
    if (gen === null || id === undefined) return;
    searchSummary(gen, id).then(
      (s) => {
        if (gen === generation && search?.search === s.search) search = s;
      },
      () => {},
    );
  });

  /** Opens a result's folder in the map with the file selected. */
  function showInMap(row: SearchRow) {
    view = row.parent;
    selection = { node: row.node, other: false };
  }

  function resultItems(row: SearchRow): MenuItem[] {
    return [
      { label: "Show in map", hint: "Enter", action: () => showInMap(row) },
      {
        label: `Show in ${fileManager}`,
        action: () => showInFileManager(row.node),
        separator: true,
      },
      { label: "Copy path", action: () => copyPath(row.node) },
    ];
  }

  // Breadcrumbs follow the view.
  $effect(() => {
    const gen = generation;
    const node = view;
    if (gen === null) return;
    nodeDetails(gen, node).then(
      (d) => {
        if (gen === generation && node === view) viewDetails = d;
      },
      () => {},
    );
  });

  // Selected item details refresh as the scan updates its sizes.
  $effect(() => {
    const gen = generation;
    const sel = selection;
    void status?.revision;
    if (gen === null || !sel) {
      selectedDetails = null;
      return;
    }
    nodeDetails(gen, sel.node).then(
      (d) => {
        if (gen === generation && selection?.node === sel.node) selectedDetails = d;
      },
      () => {},
    );
  });

  const otherInfo = $derived.by(() => {
    const l = layout;
    if (!selection?.other || !l) return null;
    for (let i = 0; i < l.count; i++) {
      if (l.kind[i] === KIND_OTHER && l.node[i] === selection.node) {
        return { weight: l.weight[i]!, items: l.items[i]! };
      }
    }
    return null;
  });

  // The small-items list stays open while its items are selected, until it's
  // closed or the map it came from changes.
  let smallOf = $state<{ folder: number; count: number; name: string } | null>(null);

  $effect(() => {
    const sel = selection;
    const info = otherInfo;
    if (!sel?.other || !info) return;
    const name = selectedDetails?.node === sel.node ? selectedDetails.name : (smallOf?.name ?? "");
    if (smallOf?.folder !== sel.node || smallOf.count !== info.items || smallOf.name !== name) {
      smallOf = { folder: sel.node, count: info.items, name };
    }
  });

  $effect(() => {
    void [generation, view, metric, searchId];
    smallOf = null;
  });

  function openFolder(node: number) {
    view = node;
    selection = null;
  }

  function goUp() {
    const crumbs = viewDetails?.ancestors;
    if (!crumbs || crumbs.length < 2) return;
    const from = view;
    view = crumbs[crumbs.length - 2]!.node;
    selection = { node: from, other: false };
  }

  function showInFileManager(node: number) {
    if (generation === null) return;
    reveal(generation, node).catch((e: unknown) => (problem = String(e)));
  }

  let noticeTimer: ReturnType<typeof setTimeout> | undefined;

  function flash(text: string) {
    notice = text;
    clearTimeout(noticeTimer);
    noticeTimer = setTimeout(() => (notice = null), 1800);
  }

  async function copyPath(node: number) {
    if (generation === null) return;
    try {
      const d = await nodeDetails(generation, node);
      await navigator.clipboard.writeText(d.path);
      flash("Path copied");
    } catch (e) {
      problem = `Couldn't copy the path: ${String(e)}`;
    }
  }

  function menuItems(r: MenuRequest): MenuItem[] {
    // Empty space stands for the folder being viewed.
    const node = r.selection?.node ?? view;
    const kind = r.kind ?? "folder";
    const items: MenuItem[] = [];
    if (kind !== "file" && node !== view) {
      const label = kind === "other" ? "Open folder in map" : "Open in map";
      items.push({ label, hint: "Enter", action: () => openFolder(node) });
    }
    if (view !== ROOT) items.push({ label: "Up one level", hint: "Backspace", action: goUp });
    items.push({
      label: kind === "other" ? `Show folder in ${fileManager}` : `Show in ${fileManager}`,
      action: () => showInFileManager(node),
      separator: true,
    });
    if (kind !== "other") items.push({ label: "Copy path", action: () => copyPath(node) });
    return items;
  }

  // Release builds replace the webview's own menu (Back, Reload, Inspect)
  // everywhere except where text can be copied or edited.
  $effect(() => {
    if (!import.meta.env.PROD) return;
    const suppress = (e: MouseEvent) => {
      const t = e.target as HTMLElement;
      if (t.closest("input, textarea") || !document.getSelection()?.isCollapsed) return;
      e.preventDefault();
    };
    document.addEventListener("contextmenu", suppress);
    return () => document.removeEventListener("contextmenu", suppress);
  });

  const empty = $derived.by((): { text: string; toRoot?: boolean } | null => {
    if (!layout || layout.total > 0 || !status) return null;
    if (status.phase.kind === "failed") return null;
    const what = metric === "allocated" ? "allocated space" : "data";
    if (search) {
      if (search.files === 0) {
        const text = `No file names contain “${search.query}”`;
        return { text: scanning ? `${text} yet.` : `${text}.`, toRoot: view !== ROOT };
      }
      if (view !== ROOT) {
        return { text: `No matching files with measurable ${what} in this folder.`, toRoot: true };
      }
      return { text: `The matching files have no measurable ${what}. They're in the result list.` };
    }
    if (scanning) return { text: `Scanning… no measurable ${what} found here yet.` };
    let text = `Nothing here has measurable ${what}.`;
    if (metric === "allocated" && (viewDetails?.unknownAllocationFiles ?? 0) > 0) {
      text += ` ${plural(viewDetails!.unknownAllocationFiles, "file")} have unknown allocation; try Logical size.`;
    }
    return { text };
  });
</script>

<div class="app">
  <div class="toolbar" role="toolbar" aria-label="Scan controls">
    <button type="button" class="primary" onclick={() => run(scanChoose)}>Choose folder or drive…</button>
    <button type="button" onclick={() => run(scanRefresh)} disabled={generation === null}>Refresh</button>
    <button type="button" onclick={cancel} disabled={!scanning}>Cancel</button>
    <div class="metric" role="radiogroup" aria-label="Size measure">
      <label><input type="radio" bind:group={metric} value="allocated" /> Allocated</label>
      <label><input type="radio" bind:group={metric} value="logical" /> Logical</label>
    </div>
    <label
      class="check"
      title={volume?.isRoot
        ? "Show the volume's capacity, free space, and space not attributed to files"
        : "Available when a whole drive or volume is scanned"}
    >
      <input type="checkbox" bind:checked={showVolume} disabled={!volume?.isRoot} /> Volume overview
    </label>
    <div class="metric" role="radiogroup" aria-label="Colors">
      <label><input type="radio" bind:group={colors} value="depth" /> Depth colors</label>
      <label><input type="radio" bind:group={colors} value="type" /> File type colors</label>
    </div>
    <div class="filter">
      <input
        type="search"
        placeholder="Filter by file name"
        aria-label="Filter by file name"
        bind:value={query}
        oninput={onQueryInput}
        onkeydown={(e) => e.key === "Escape" && clearQuery()}
        disabled={generation === null}
      />
      {#if searching}<span class="muted" role="status">Searching…</span>{/if}
    </div>
    {#if privileged}
      <span class="badge" title="Started with administrator or root rights: protected folders are scanned too.">
        Administrator access
      </span>
    {/if}
  </div>

  {#if problem}
    <div class="banner error" role="alert">
      {problem}
      <button type="button" onclick={() => (problem = null)} aria-label="Dismiss">✕</button>
    </div>
  {/if}
  {#if status?.phase.kind === "cancelled"}
    <div class="banner warn" role="status">
      Partial result: the scan was cancelled before every folder was read, so sizes shown are lower
      bounds. Select a folder to see whether it was scanned.
    </div>
  {:else if status?.phase.kind === "failed"}
    <div class="banner error" role="alert">{status.phase.message}</div>
  {/if}

  {#if showVolume && volume?.isRoot && status}
    {#if metric === "allocated" && !search}
      <VolumeStrip {volume} {status} {os} onshowomissions={() => (showOmissions = true)} />
    {:else}
      <p class="volume-note">
        The volume overview compares allocated space, so it's hidden with Logical size or a filter.
      </p>
    {/if}
  {/if}

  <div class="crumbs">
    {#if viewDetails}
      <Breadcrumbs crumbs={viewDetails.ancestors} onnavigate={openFolder} />
    {:else if status}
      <span>{status.root}</span>
    {/if}
  </div>

  <div class="body">
    <div class="mapcol">
      <main class="map">
        {#if generation === null}
          <div class="placeholder">
            <p>Choose a folder or drive to see what's using its space.</p>
            <button type="button" class="primary" onclick={() => run(scanChoose)}>Choose folder or drive…</button>
          </div>
        {:else}
          <Treemap
            {generation}
            {view}
            {metric}
            {colors}
            search={search?.search ?? null}
            revision={status?.revision ?? 0}
            {selection}
            onselect={(s) => (selection = s)}
            onopen={openFolder}
            onup={goUp}
            onlayout={(l) => (layout = l)}
            onmenu={(r) => (menu = { x: r.x, y: r.y, items: menuItems(r) })}
          />
          {#if empty}
            <div class="placeholder overlay">
              <p>{empty.text}</p>
              {#if empty.toRoot}
                <button type="button" onclick={() => openFolder(ROOT)}>Go to the scan root</button>
              {/if}
            </div>
          {/if}
        {/if}
      </main>
      {#if colors === "type" && generation !== null}
        <Legend />
      {/if}
    </div>
    <aside>
      <Details
        {selection}
        details={selectedDetails}
        other={otherInfo}
        {view}
        {fileManager}
        onopen={openFolder}
        onreveal={showInFileManager}
        oncopy={copyPath}
      />
      {#if showOmissions && status}
        <Omissions groups={status.omissions} onclose={() => (showOmissions = false)} />
      {/if}
      {#if smallOf && generation !== null}
        {#key smallOf.folder}
          <SmallItems
            {generation}
            folder={smallOf.folder}
            folderName={smallOf.name}
            count={smallOf.count}
            {metric}
            search={searchId ?? null}
            revision={status?.revision ?? 0}
            selected={selection && !selection.other ? selection.node : null}
            onselect={(node) => (selection = { node, other: false })}
            onopen={openFolder}
            onclose={() => (smallOf = null)}
          />
        {/key}
      {/if}
      {#if search && generation !== null}
        <SearchResults
          {generation}
          summary={search}
          {metric}
          revision={status?.revision ?? 0}
          {scanning}
          scanned={metric === "allocated" ? (status?.allocated ?? 0) : (status?.logical ?? 0)}
          selected={selection && !selection.other ? selection.node : null}
          onselect={(node) => (selection = { node, other: false })}
          onshow={showInMap}
          onmenu={(row, x, y) => (menu = { x, y, items: resultItems(row) })}
        />
      {/if}
    </aside>
  </div>

  <StatusBar
    {status}
    {elapsedMs}
    {metric}
    {showOmissions}
    ontoggleomissions={() => (showOmissions = !showOmissions)}
  />

  {#if menu}
    <ContextMenu
      x={menu.x}
      y={menu.y}
      label="Actions"
      items={menu.items}
      onclose={() => (menu = null)}
    />
  {/if}
  <div class="notice" role="status">{#if notice}<span>{notice}</span>{/if}</div>
</div>

<style>
  .app {
    display: flex;
    flex-direction: column;
    height: 100vh;
  }
  .toolbar {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 8px;
    padding: 8px 12px;
    border-bottom: 1px solid var(--line);
    background: var(--panel);
  }
  .metric {
    display: flex;
    gap: 10px;
    margin-left: 12px;
  }
  .metric label {
    display: flex;
    align-items: center;
    gap: 4px;
    cursor: pointer;
  }
  .filter {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-left: 12px;
  }
  .filter input {
    width: 220px;
    font: inherit;
    padding: 4px 8px;
    border: 1px solid var(--line);
    border-radius: 4px;
    background: var(--bg);
    color: var(--fg);
  }
  .muted {
    color: var(--muted);
    font-size: 12px;
  }
  .check {
    display: flex;
    align-items: center;
    gap: 4px;
    margin-left: 12px;
    cursor: pointer;
  }
  .volume-note {
    margin: 0;
    padding: 4px 12px;
    border-bottom: 1px solid var(--line);
    color: var(--muted);
    font-size: 12px;
  }
  .badge {
    margin-left: auto;
    padding: 2px 8px;
    border-radius: 10px;
    background: var(--warn-bg);
    font-size: 12px;
  }
  .crumbs {
    padding: 4px 8px;
    min-height: 24px;
    border-bottom: 1px solid var(--line);
  }
  .body {
    flex: 1;
    display: grid;
    grid-template-columns: 1fr 300px;
    min-height: 0;
  }
  .mapcol {
    display: flex;
    flex-direction: column;
    min-width: 0;
    min-height: 0;
  }
  .map {
    flex: 1;
    position: relative;
    min-width: 0;
    min-height: 0;
    padding: 0;
  }
  aside {
    display: flex;
    flex-direction: column;
    min-height: 0;
    border-left: 1px solid var(--line);
    background: var(--panel);
    overflow-y: auto;
  }
  .placeholder {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 8px;
    height: 100%;
    color: var(--muted);
    text-align: center;
    padding: 24px;
    box-sizing: border-box;
  }
  .overlay {
    position: absolute;
    inset: 0;
    pointer-events: none;
  }
  .overlay button {
    pointer-events: auto;
  }
  .banner {
    display: flex;
    justify-content: space-between;
    gap: 8px;
    padding: 6px 12px;
    border-bottom: 1px solid var(--line);
  }
  .banner button {
    border: none;
    background: none;
    color: inherit;
    cursor: pointer;
  }
  .notice {
    position: fixed;
    bottom: 40px;
    left: 50%;
    transform: translateX(-50%);
    pointer-events: none;
  }
  .notice span {
    display: block;
    padding: 6px 12px;
    border-radius: 4px;
    background: var(--fg);
    color: var(--bg);
    font-size: 12px;
  }
  .warn {
    background: var(--warn-bg);
  }
  .error {
    background: var(--danger-bg);
  }
</style>
