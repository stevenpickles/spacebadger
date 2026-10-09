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
  } from "./lib/api";
  import Breadcrumbs from "./lib/Breadcrumbs.svelte";
  import ContextMenu, { type MenuItem } from "./lib/ContextMenu.svelte";
  import Details from "./lib/Details.svelte";
  import { plural } from "./lib/format";
  import { KIND_OTHER, type DecodedLayout } from "./lib/layoutWire";
  import Omissions from "./lib/Omissions.svelte";
  import type { Metric } from "./lib/protocol/Metric";
  import type { NodeDetails } from "./lib/protocol/NodeDetails";
  import type { ScanStarted } from "./lib/protocol/ScanStarted";
  import type { ScanStatus } from "./lib/protocol/ScanStatus";
  import type { MenuRequest, Selection } from "./lib/selection";
  import StatusBar from "./lib/StatusBar.svelte";
  import Treemap from "./lib/Treemap.svelte";

  const ROOT = 0;

  let generation = $state<number | null>(null);
  let status = $state.raw<ScanStatus | null>(null);
  let statusAt = $state(0);
  let now = $state(performance.now());
  let view = $state(ROOT);
  let selection = $state<Selection | null>(null);
  let metric = $state<Metric>("allocated");
  let viewDetails = $state.raw<NodeDetails | null>(null);
  let selectedDetails = $state.raw<NodeDetails | null>(null);
  let layout = $state.raw<DecodedLayout | null>(null);
  let showOmissions = $state(false);
  let problem = $state<string | null>(null);
  let privileged = $state(false);
  let fileManager = $state(fileManagerName(""));
  let menu = $state.raw<MenuRequest | null>(null);
  let notice = $state<string | null>(null);

  const scanning = $derived(status?.phase.kind === "scanning");
  const elapsedMs = $derived(
    status ? status.elapsedMs + (scanning ? Math.max(0, now - statusAt) : 0) : 0,
  );

  appInfo().then(
    (info) => {
      privileged = info.privilegedAccess;
      fileManager = fileManagerName(info.os);
      if (info.protocolVersion !== EXPECTED_PROTOCOL_VERSION) {
        problem = `The backend speaks protocol ${info.protocolVersion}, but this interface expects ${EXPECTED_PROTOCOL_VERSION}.`;
      }
    },
    (e: unknown) => (problem = `Backend unavailable: ${String(e)}`),
  );

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

  const empty = $derived.by(() => {
    if (!layout || layout.total > 0 || !status) return null;
    if (status.phase.kind === "failed") return null;
    const what = metric === "allocated" ? "allocated space" : "data";
    if (scanning) return `Scanning… no measurable ${what} found here yet.`;
    let text = `Nothing here has measurable ${what}.`;
    if (metric === "allocated" && (viewDetails?.unknownAllocationFiles ?? 0) > 0) {
      text += ` ${plural(viewDetails!.unknownAllocationFiles, "file")} have unknown allocation; try Logical size.`;
    }
    return text;
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

  <div class="crumbs">
    {#if viewDetails}
      <Breadcrumbs crumbs={viewDetails.ancestors} onnavigate={openFolder} />
    {:else if status}
      <span>{status.root}</span>
    {/if}
  </div>

  <div class="body">
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
          revision={status?.revision ?? 0}
          {selection}
          onselect={(s) => (selection = s)}
          onopen={openFolder}
          onup={goUp}
          onlayout={(l) => (layout = l)}
          onmenu={(r) => (menu = r)}
        />
        {#if empty}
          <div class="placeholder overlay"><p>{empty}</p></div>
        {/if}
      {/if}
    </main>
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
      items={menuItems(menu)}
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
  .map {
    position: relative;
    min-width: 0;
    min-height: 0;
    padding: 0;
  }
  aside {
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
