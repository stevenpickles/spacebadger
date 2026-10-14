<script lang="ts">
  import { untrack } from "svelte";
  import {
    appInfo,
    duplicatesCancel,
    duplicatesStart,
    duplicatesStatus,
    errorText,
    EXPECTED_PROTOCOL_VERSION,
    exportSave,
    fileManagerName,
    nodeDetails,
    onDuplicatesStatus,
    onExportProgress,
    scanChoose,
    scanRefresh,
    restartElevated,
    reveal,
    revealExport,
    searchSet,
    searchSummary,
    selectionSummary,
    trashName,
    volumeInfo,
  } from "./lib/api";
  import Breadcrumbs from "./lib/Breadcrumbs.svelte";
  import Changes from "./lib/Changes.svelte";
  import CompareControl from "./lib/CompareControl.svelte";
  import type { ColorMode } from "./lib/colors";
  import Legend from "./lib/Legend.svelte";
  import ContextMenu, { type MenuItem } from "./lib/ContextMenu.svelte";
  import Contents, { type ContentsRow } from "./lib/Contents.svelte";
  import { ComparisonController } from "./lib/controllers/comparison.svelte";
  import { DeletionController } from "./lib/controllers/deletion.svelte";
  import { ScanController } from "./lib/controllers/scan.svelte";
  import DeleteDialog from "./lib/DeleteDialog.svelte";
  import Details from "./lib/Details.svelte";
  import Duplicates from "./lib/Duplicates.svelte";
  import ExportDialog, { exportFileName, localTime, type ExportChoice } from "./lib/ExportDialog.svelte";
  import ExportToast from "./lib/ExportToast.svelte";
  import FilterBox from "./lib/FilterBox.svelte";
  import Announcer from "./lib/Announcer.svelte";
  import { announce } from "./lib/announce.svelte";
  import { formatBytes, KILO, plural } from "./lib/format";
  import { KIND_OTHER, type DecodedLayout } from "./lib/layoutWire";
  import Omissions from "./lib/Omissions.svelte";
  import type { DeleteReport } from "./lib/protocol/DeleteReport";
  import type { DuplicateFile } from "./lib/protocol/DuplicateFile";
  import type { DuplicatesStatus } from "./lib/protocol/DuplicatesStatus";
  import type { ExportReport } from "./lib/protocol/ExportReport";
  import type { Metric } from "./lib/protocol/Metric";
  import type { NodeDetails } from "./lib/protocol/NodeDetails";
  import type { ScanStatus } from "./lib/protocol/ScanStatus";
  import type { SearchRow } from "./lib/protocol/SearchRow";
  import type { SmallItem } from "./lib/protocol/SmallItem";
  import type { SearchSummary } from "./lib/protocol/SearchSummary";
  import type { SelectionSummary } from "./lib/protocol/SelectionSummary";
  import SearchResults from "./lib/SearchResults.svelte";
  import Shortcuts from "./lib/Shortcuts.svelte";
  import SmallItems from "./lib/SmallItems.svelte";
  import { latest, queryKey } from "./lib/request";
  import { applyPick, type MenuRequest, type Pick, type Selection } from "./lib/selection";
  import StatusBar from "./lib/StatusBar.svelte";
  import Treemap from "./lib/Treemap.svelte";
  import type { VolumeInfo } from "./lib/protocol/VolumeInfo";
  import VolumeStrip from "./lib/VolumeStrip.svelte";

  const ROOT = 0;
  type PanelTab = "contents" | "changes" | "results" | "small" | "duplicates" | "omissions";
  const FILTER_HELP: [string, string][] = [
    ["report", "names containing “report”"],
    ["*.jpg", "names ending in .jpg (* is any text, ? one character)"],
    ["*.jpg; *.png", "either pattern (separate patterns with ;)"],
    ["!*.tmp", "leave out names ending in .tmp"],
    ["dir:cache", "files inside folders named like “cache”, at any depth"],
    ["*.js; !dir:node_modules", "JavaScript files, except inside node_modules"],
    ["is:video", "videos; also audio, image, document, archive, diskimage, program, code, other"],
    ["is:image; dir:Photos", "images inside Photos folders"],
    ["size:>500MB", "files over 500 MB; also <, >=, <="],
    ["size:10MB..1GB", "from 10 MB to 1 GB, both included"],
    ["older:1y", "modified over a year ago; also h, d, w, mo"],
    ["newer:2024-06-30", "modified on or after a date"],
    ["*.iso; size:>1GB; older:1y", "disk images over 1 GB, untouched for a year"],
  ];

  const scan = new ScanController({ onadopt: adopted, onerror: (message) => (problem = message) });
  const generation = $derived(scan.generation);
  const status = $derived(scan.status);
  let view = $state(ROOT);
  /** The focused item: its details are shown and arrow keys start there. */
  let selection = $state<Selection | null>(null);
  /** Selected files and folders, which actions such as delete apply to. */
  let picked = $state.raw<number[]>([]);
  const pickedSet = $derived(new Set(picked));
  /** Where Shift ranges start. */
  let anchor = $state<number | null>(null);
  /** Totals when several items are selected. */
  let multi = $state.raw<SelectionSummary | null>(null);
  /** The map, lists, and totals measure file size; on-disk size appears
   * only in the volume overview, which compares against disk usage. */
  const metric: Metric = "logical";
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
  let trash = $state(trashName(""));
  const deletion = new DeletionController(
    { scan, targets: deleteTargets, duplicatesJob: () => dupes?.job, trash: () => trash },
    { onnotice: flash, onerror: (message) => (problem = message), ondeleted: afterDelete },
  );
  const allowDelete = $derived(deletion.allowed);
  const confirmDelete = $derived(deletion.confirm);
  const deleting = $derived(deletion.running);
  const askDelete = () => deletion.ask();
  let detailsError = $state<string | null>(null);
  let detailsRetry = $state(0);
  let multiUpdating = $state(false);
  let menu = $state.raw<{ x: number; y: number; items: MenuItem[]; heading?: string } | null>(null);
  /** What's typed in the filter box. */
  let query = $state("");
  /** The search the map and results show; `null` when unfiltered. */
  let search = $state.raw<SearchSummary | null>(null);
  let searching = $state(false);
  /** Why the typed filter can't be used, e.g. an unknown is: type. */
  let filterError = $state<string | null>(null);
  let notice = $state<string | null>(null);
  let showShortcuts = $state(false);
  let showFilterHelp = $state(false);
  /** Waiting on the restart question or the Windows elevation prompt. */
  let elevating = $state(false);
  /** The list chosen below the details; the panel falls back to one that's open. */
  let tab = $state<PanelTab | null>(null);
  /** The latest search for duplicate files in this scan, while its tab is open. */
  let dupes = $state.raw<DuplicatesStatus | null>(null);
  /** Smallest file size the duplicate search looks at. */
  let dupesMinSize = $state(Math.max(KILO, Number(saved("duplicates-min-size")) || KILO ** 2));

  const scanning = $derived(scan.scanning);
  /** Restarting with administrator rights is offered on Windows only. */
  const canElevate = $derived(os === "windows" && !privileged);

  async function elevate() {
    if (elevating) return;
    elevating = true;
    try {
      // On success this window closes; otherwise the user said no.
      await restartElevated();
    } catch (e) {
      problem = errorText(e);
    } finally {
      elevating = false;
    }
  }
  const deleteBlocked = $derived(deletion.blocked);

  appInfo().then(
    (info) => {
      privileged = info.privilegedAccess;
      os = info.os;
      fileManager = fileManagerName(info.os);
      trash = trashName(info.os);
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

  // Change colors last only as long as the comparison, so aren't remembered.
  $effect(() => {
    if (colors !== "change") remember("colors", colors);
  });
  $effect(() => remember("volume", showVolume ? "on" : "off"));

  /** The contents list shows folders too; off, it lists files from every level. */
  let contentsFolders = $state(saved("contents-folders") !== "off");
  $effect(() => remember("contents-folders", contentsFolders ? "on" : "off"));

  /** A newer scan is shown; view and selection start over at its root. */
  function adopted(previous: ScanStatus | null) {
    view = ROOT;
    pick(null);
    viewDetails = null;
    selectedDetails = null;
    layout = null;
    menu = null;
    search = null;
    volume = null;
    dupes = null;
    compare.restart(previous);
    // Keep the filter across Refresh and new roots.
    if (query) applySearch(query);
  }

  // Volume capacity, read when a scan starts and again when it ends, since
  // free space may have changed meanwhile.
  const finished = $derived(scan.finished);
  $effect(() => {
    const gen = generation;
    void [finished, deletion.count];
    if (gen === null) return;
    volumeInfo(gen).then(
      (v) => {
        if (gen === generation) volume = v;
      },
      () => {},
    );
  });

  // ---- comparing with an earlier scan -------------------------------------

  const compare = new ComparisonController(
    { scan, selected: () => (selection && !selection.other ? selection.node : null), view: () => view },
    {
      onstart: (_, again) => {
        if (colors !== "change") colorsBefore = colors;
        colors = "change";
        if (!again) tab = "changes";
      },
      onend: () => {
        if (colors === "change") colors = colorsBefore;
      },
      onerror: (message) => (problem = message),
    },
  );
  const snapshots = $derived(compare.snapshots);
  const comparison = $derived(compare.comparison);
  const comparing = $derived(compare.busy);
  const selectedChange = $derived(compare.selectedChange);
  const viewChange = $derived(compare.viewChange);
  const compareId = $derived(compare.id);
  const since = $derived(compare.since);
  /** The colors to go back to when comparing stops. */
  let colorsBefore: ColorMode = "depth";
  /** List folders in the Changes tab, not only files. */
  let changesFolders = $state(true);

  // ---- filename search ------------------------------------------------

  let searchTimer: ReturnType<typeof setTimeout> | undefined;
  /** Only the newest search's reply is used. */
  const searches = latest();

  function onQueryInput() {
    clearTimeout(searchTimer);
    searchTimer = setTimeout(() => applySearch(query), 250);
  }

  function clearQuery() {
    query = "";
    clearTimeout(searchTimer);
    applySearch("");
  }

  function applyNow() {
    clearTimeout(searchTimer);
    applySearch(query);
  }

  async function applySearch(text: string) {
    const gen = generation;
    if (gen === null) return;
    const t = searches.begin(queryKey(gen, text));
    searching = text !== "";
    try {
      const summary = await searchSet(gen, text);
      if (!searches.current(t) || gen !== generation) return;
      search = summary;
      filterError = null;
      menu = null;
    } catch (e) {
      // A newer search replaced this one; its reply will arrive instead.
      // The one that failed is the query as typed, e.g. an unknown type.
      if (searches.current(t) && gen === generation) filterError = errorText(e);
    } finally {
      if (searches.current(t)) searching = false;
    }
  }

  const searchId = $derived(search?.search);

  // Matching totals follow the scan; an earlier revision's reply arriving
  // late is dropped.
  const searchTotals = latest();
  $effect(() => {
    const gen = generation;
    const id = searchId;
    const revision = status?.revision;
    if (gen === null || id === undefined) {
      searchTotals.cancel();
      return;
    }
    searchTotals.run(
      queryKey(gen, revision, id),
      () => searchSummary(gen, id),
      (s) => {
        if (search?.search === s.search) search = s;
      },
    );
  });

  /** Opens a result's folder in the map with the file selected. */
  function showInMap(row: { node: number; parent: number }) {
    view = row.parent;
    selectOnly(row.node);
  }

  function contentsMenu(r: ContentsRow): MenuItem[] {
    const parent = r.parent;
    if (r.folder || parent === null || parent === view) return smallItemMenu(r);
    return [
      { label: "Show in map", hint: "Enter", action: () => showInMap({ node: r.node, parent }) },
      { label: `Show in ${fileManager}`, action: () => showInFileManager(r.node), separator: true },
      { label: "Copy path", action: () => copyPath(r.node) },
      ...deleteItemsFor(r.node),
    ];
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
      ...deleteItemsFor(row.node),
    ];
  }

  function smallItemMenu(it: SmallItem): MenuItem[] {
    const items: MenuItem[] = [];
    if (it.folder) items.push({ label: "Open in map", hint: "Enter", action: () => openFolder(it.node) });
    items.push(
      { label: `Show in ${fileManager}`, action: () => showInFileManager(it.node), separator: it.folder },
      { label: "Copy path", action: () => copyPath(it.node) },
    );
    if (it.folder) items.push(findDuplicatesItem(it.node));
    if (it.folder) items.push(...exportItemsFor(it.node));
    items.push(...deleteItemsFor(it.node));
    return items;
  }

  function duplicateMenu(f: DuplicateFile): MenuItem[] {
    return [
      { label: "Show in map", hint: "Enter", action: () => showInMap(f) },
      { label: `Show in ${fileManager}`, action: () => showInFileManager(f.node), separator: true },
      { label: "Copy path", action: () => copyPath(f.node) },
      ...deleteItemsFor(f.node),
    ];
  }

  // ---- duplicate files ----------------------------------------------------

  const duplicatesBlocked = "Duplicates can be found once the scan finishes or is cancelled.";

  /** Looks for files with the same contents under `folder`, matching the
   * filter if one is on. Only ever started by the user: it reads the files. */
  async function findDuplicates(folder: number, minSize = dupesMinSize) {
    const gen = generation;
    if (gen === null) return;
    if (scanning) {
      flash(duplicatesBlocked);
      return;
    }
    dupesMinSize = minSize;
    remember("duplicates-min-size", String(minSize));
    try {
      const started = await duplicatesStart({ generation: gen, folder, search: searchId ?? null, minSize });
      if (gen !== generation) return;
      dupes = started;
      tab = "duplicates";
    } catch (e) {
      problem = `Couldn't look for duplicates: ${errorText(e)}`;
    }
  }

  function findDuplicatesItem(folder: number): MenuItem {
    return {
      label: "Find duplicates here",
      action: () => findDuplicates(folder),
      separator: true,
      disabled: scanning,
      title: scanning ? duplicatesBlocked : undefined,
    };
  }

  function stopDuplicates() {
    if (generation !== null && dupes) duplicatesCancel(generation, dupes.job).catch(() => {});
  }

  function closeDuplicates() {
    if (dupes?.phase === "running") stopDuplicates();
    dupes = null;
  }

  $effect(() => {
    const unlisten = onDuplicatesStatus((s) => {
      if (s.generation === generation && s.job === dupes?.job) dupes = s;
    });
    return () => void unlisten.then((f) => f());
  });

  /** The duplicate search shown, as a plain number so that status updates
   * (new objects) don't rerun what depends on it. */
  const dupesJob = $derived(dupes?.job);

  // Deleting items takes them out of the groups.
  $effect(() => {
    const gen = generation;
    const job = dupesJob;
    void status?.revision;
    if (gen === null || job === undefined) return;
    duplicatesStatus(gen, job).then(
      (s) => {
        if (gen === generation && s.job === dupes?.job) dupes = s;
      },
      () => {},
    );
  });

  // ---- export -----------------------------------------------------------

  /** The folder the export dialog is open for. */
  let exportOf = $state.raw<{ node: number; name: string } | null>(null);
  /** An export is running: rows written so far, `null` until the file is chosen. */
  let exporting = $state<{ rows: number | null } | null>(null);
  /** The last finished export, shown until dismissed. */
  let exported = $state.raw<ExportReport | null>(null);

  async function askExport(node: number) {
    const gen = generation;
    if (gen === null || exporting) return;
    try {
      const d = await nodeDetails(gen, node);
      if (gen === generation) exportOf = { node, name: d.name };
    } catch (e) {
      problem = `Couldn't export: ${errorText(e)}`;
    }
  }

  async function runExport(choice: ExportChoice) {
    const target = exportOf;
    const gen = generation;
    exportOf = null;
    if (!target || gen === null || exporting) return;
    exporting = { rows: null };
    exported = null;
    // Listening before asking, so even a small export's progress is seen.
    let unlisten: (() => void) | undefined;
    try {
      unlisten = await onExportProgress((p) => {
        if (p.generation === gen && exporting) exporting = { rows: p.rows };
      });
      const report = await exportSave({
        generation: gen,
        folder: target.node,
        scope: choice.scope,
        format: choice.format,
        metric,
        search: searchId ?? null,
        fileName: exportFileName(target.name, choice.format),
        exportedAt: localTime(),
      });
      if (report) {
        exported = report;
        announce(
          report.format === "csv"
            ? `Exported ${plural(report.rows, "row")} to ${report.path}`
            : `Saved the report to ${report.path}`,
        );
      }
    } catch (e) {
      problem = `Couldn't export: ${errorText(e)}`;
    } finally {
      exporting = null;
      unlisten?.();
    }
  }

  /** Where focus goes when the export dialog closes and the control that
   * opened it is gone or disabled. */
  function focusMap() {
    document.querySelector<HTMLElement>(".map .treemap")?.focus();
  }

  function showExport() {
    revealExport().catch((e: unknown) => (problem = errorText(e)));
  }

  // ---- selection and delete -------------------------------------------

  function pick(p: Pick | null) {
    if (!p) {
      selection = null;
      picked = [];
      anchor = null;
      return;
    }
    selection = p.focus;
    picked = applyPick(picked, p);
    if (!p.keepAnchor && !p.focus.other) anchor = p.focus.node;
  }

  function selectOnly(node: number) {
    pick({ focus: { node, other: false }, nodes: [node], mode: "replace", keepAnchor: false });
  }

  // A different scan or filter starts with nothing selected; the focused
  // item may no longer be drawn.
  $effect(() => {
    void [generation, searchId];
    untrack(() => pick(null));
  });

  // Totals for several selected items follow the scan; an earlier
  // revision's reply arriving late is dropped.
  const multiRequest = latest();
  $effect(() => {
    const gen = generation;
    const nodes = picked;
    const revision = status?.revision;
    if (gen === null || nodes.length < 2) {
      multiRequest.cancel();
      multi = null;
      multiUpdating = false;
      return;
    }
    multiUpdating = true;
    multiRequest.run(
      queryKey(gen, revision, ...nodes),
      () => selectionSummary({ generation: gen, nodes }),
      (s) => {
        multi = s;
        multiUpdating = false;
      },
      (e) => {
        multiUpdating = false;
        problem = `Couldn't total the selection: ${errorText(e)}`;
      },
    );
  });

  async function copyPaths() {
    const gen = generation;
    const nodes = picked;
    if (gen === null || nodes.length === 0) return;
    try {
      const all = await Promise.all(nodes.map((n) => nodeDetails(gen, n)));
      await navigator.clipboard.writeText(all.map((d) => d.path).join("\n"));
      flash(`${plural(nodes.length, "path")} copied`);
    } catch (e) {
      problem = `Couldn't copy the paths: ${errorText(e)}`;
    }
  }

  /** What delete applies to: the selected items, else the focused one. */
  function deleteTargets(): number[] {
    if (picked.length > 0) return picked;
    return selection && !selection.other ? [selection.node] : [];
  }

  /** Menu entry to delete `node`, or the selection it belongs to; shown
   * unavailable, with the reason, until deleting is turned on. */
  function deleteItemsFor(node: number): MenuItem[] {
    const count = pickedSet.has(node) ? picked.length : 1;
    const what = count > 1 ? `${count} items ` : "";
    const reason = !allowDelete ? "Turn on “Allow deleting” in the toolbar first." : deleteBlocked;
    return [
      {
        label: `Move ${what}to the ${trash}…`,
        hint: "Del",
        action: askDelete,
        separator: true,
        danger: true,
        disabled: !!reason,
        title: reason ?? undefined,
      },
    ];
  }

  /** Menu entry to export a folder; unavailable while an export runs. */
  function exportItemsFor(node: number): MenuItem[] {
    return [
      {
        label: "Export…",
        action: () => askExport(node),
        disabled: exporting !== null,
        title: exporting ? "An export is running." : undefined,
      },
    ];
  }

  /** What a menu acts on, named in its heading. */
  function menuHeading(node: number, name: string | undefined): string | undefined {
    if (pickedSet.has(node) && picked.length > 1) return `${picked.length} selected items`;
    return name;
  }

  /** Leaves a folder deleted with the items; what stayed stays selected. */
  function afterDelete(r: DeleteReport) {
    const gone = new Set(r.removed);
    // Leave a folder that was deleted along with the items.
    const crumbs = viewDetails?.ancestors ?? [];
    const at = crumbs.findIndex((c) => gone.has(c.node));
    if (at > 0) openFolder(crumbs[at - 1]!.node);
    // Items left where they were (changed or failed) stay selected.
    const kept = r.others.filter((o) => !gone.has(o.node)).map((o) => o.node);
    if (kept.length > 0) {
      pick({ focus: { node: kept[0]!, other: false }, nodes: kept, mode: "replace", keepAnchor: false });
    } else {
      pick(null);
    }
  }

  /** What a folder adds up to in the map: its matching files when filtered. */
  const shownSize = (d: NodeDetails) => d.matching?.logical ?? d.logical;

  // Each of the details below takes only its newest reply: a reply to an
  // earlier filter, revision, or item (A, then B, then A again) is dropped,
  // however late it arrives.
  const viewDetailsRequest = latest();
  const selectedDetailsRequest = latest();
  const parentSizeRequest = latest();
  const viewParentSizeRequest = latest();

  // Breadcrumbs follow the view; its details also refresh as the scan
  // updates sizes and the filter changes, since they're shown when nothing
  // is selected.
  $effect(() => {
    const gen = generation;
    const node = view;
    const filter = searchId ?? null;
    const revision = status?.revision;
    if (gen === null) {
      viewDetailsRequest.cancel();
      return;
    }
    viewDetailsRequest.run(
      queryKey(gen, revision, node, filter),
      () => nodeDetails(gen, node, filter),
      (d) => {
        if (d.generation === gen && d.node === node) viewDetails = d;
      },
    );
  });

  // Selected item details refresh as the scan updates its sizes.
  $effect(() => {
    const gen = generation;
    const sel = selection;
    const filter = searchId ?? null;
    const revision = status?.revision;
    void detailsRetry;
    detailsError = null;
    if (gen === null || !sel) {
      selectedDetailsRequest.cancel();
      selectedDetails = null;
      return;
    }
    const node = sel.node;
    selectedDetailsRequest.run(
      queryKey(gen, revision, node, filter),
      () => nodeDetails(gen, node, filter),
      (d) => {
        if (d.generation === gen && d.node === node) selectedDetails = d;
      },
      (e) => {
        selectedDetails = null;
        detailsError = errorText(e);
      },
    );
  });

  // The selected item's share of its folder.
  let parentSize = $state<number | null>(null);
  $effect(() => {
    const gen = generation;
    const d = selectedDetails;
    const parent = d?.ancestors.at(-2)?.node;
    const filter = searchId ?? null;
    parentSize = null;
    parentSizeRequest.cancel();
    if (gen === null || !d || parent === undefined || selection?.other) return;
    if (parent === view && viewDetails?.node === view) {
      parentSize = shownSize(viewDetails);
      return;
    }
    parentSizeRequest.run(
      queryKey(gen, d.node, parent, filter),
      () => nodeDetails(gen, parent, filter),
      (p) => (parentSize = shownSize(p)),
    );
  });

  // The viewed folder's parent, for its share when nothing is selected.
  let viewParentSize = $state<number | null>(null);
  $effect(() => {
    const gen = generation;
    const d = viewDetails;
    const parent = d?.ancestors.at(-2)?.node;
    const filter = searchId ?? null;
    viewParentSize = null;
    viewParentSizeRequest.cancel();
    if (gen === null || !d || parent === undefined) return;
    viewParentSizeRequest.run(
      queryKey(gen, d.node, parent, filter),
      () => nodeDetails(gen, parent, filter),
      (p) => (viewParentSize = shownSize(p)),
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
    void [generation, view, searchId];
    smallOf = null;
  });

  // Selecting something outside the list's folder closes it.
  $effect(() => {
    const sel = selection;
    const d = selectedDetails;
    if (!smallOf || !sel || sel.other || d?.node !== sel.node) return;
    const parent = d.ancestors[d.ancestors.length - 2]?.node;
    if (parent !== smallOf.folder) smallOf = null;
  });

  function openFolder(node: number) {
    view = node;
    pick(null);
  }

  function goUp() {
    const crumbs = viewDetails?.ancestors;
    if (!crumbs || crumbs.length < 2) return;
    const from = view;
    view = crumbs[crumbs.length - 2]!.node;
    selectOnly(from);
  }

  function showInFileManager(node: number) {
    if (generation === null) return;
    reveal(generation, node).catch((e: unknown) => (problem = String(e)));
  }

  let noticeTimer: ReturnType<typeof setTimeout> | undefined;

  function flash(text: string, ms = 1800) {
    notice = text;
    announce(text);
    clearTimeout(noticeTimer);
    noticeTimer = setTimeout(() => (notice = null), ms);
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
    if (kind === "folder") items.push(findDuplicatesItem(node));
    if (kind === "folder") items.push(...exportItemsFor(node));
    if (kind !== "other" && node !== view && node !== ROOT) items.push(...deleteItemsFor(node));
    return items;
  }

  function treemapHeading(r: MenuRequest, name: string | undefined): string | undefined {
    const node = r.selection?.node ?? view;
    if (r.kind === "other") return name ? `Small items in ${name}` : "Small items";
    return menuHeading(node, name);
  }

  function openTreemapMenu(r: MenuRequest) {
    const node = r.selection?.node ?? view;
    const known = [selectedDetails, viewDetails].find((d) => d?.node === node);
    const opened = { x: r.x, y: r.y, items: menuItems(r), heading: treemapHeading(r, known?.name) };
    menu = opened;
    // An item right-clicked before it was selected: name it once known.
    const gen = generation;
    if (known || gen === null) return;
    nodeDetails(gen, node).then(
      (d) => {
        if (menu === opened) menu = { ...opened, heading: treemapHeading(r, d.name) };
      },
      () => {},
    );
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
    if (search) {
      if (search.files === 0) {
        const text = `No files match “${search.query}”`;
        return { text: scanning ? `${text} yet.` : `${text}.`, toRoot: view !== ROOT };
      }
      if (view !== ROOT) {
        return { text: "The matching files in this folder are all empty.", toRoot: true };
      }
      return { text: "The matching files are all empty. They're in the result list." };
    }
    if (scanning) return { text: "Scanning… no data found here yet." };
    return { text: "Everything here is empty." };
  });

  // ---- side panel lists -------------------------------------------------

  const tabs = $derived(
    [
      generation !== null ? { id: "contents" as const, label: "Contents" } : null,
      comparison && generation !== null ? { id: "changes" as const, label: "Changes" } : null,
      search && generation !== null ? { id: "results" as const, label: "Results" } : null,
      smallOf && generation !== null ? { id: "small" as const, label: "Small items" } : null,
      dupes && generation !== null ? { id: "duplicates" as const, label: "Duplicates" } : null,
      showOmissions && status ? { id: "omissions" as const, label: "Not counted" } : null,
    ].filter((t) => t !== null),
  );
  const activeTab = $derived(tabs.find((t) => t.id === tab)?.id ?? tabs[0]?.id ?? null);

  // A list that opens comes to the front.
  $effect(() => {
    if (search) untrack(() => (tab = "results"));
  });
  $effect(() => {
    if (smallOf?.folder !== undefined) untrack(() => (tab = "small"));
  });
  $effect(() => {
    if (showOmissions) untrack(() => (tab = "omissions"));
  });

  let contents: ReturnType<typeof Contents> | undefined = $state();
  /** A folder opened from the contents list: the new list takes the focus. */
  let refocusContents = false;

  $effect(() => {
    if (contents && refocusContents) {
      refocusContents = false;
      contents.focus();
    }
  });

  // An item selected in the map scrolls into view in the contents list.
  $effect(() => {
    const sel = selection;
    if (sel && !sel.other) untrack(() => contents?.reveal(sel.node));
  });

  function onTabKey(e: KeyboardEvent) {
    const at = tabs.findIndex((t) => t.id === activeTab);
    const step = e.key === "ArrowRight" ? 1 : e.key === "ArrowLeft" ? -1 : 0;
    if (!step || at < 0) return;
    e.preventDefault();
    const next = tabs[(at + step + tabs.length) % tabs.length]!;
    tab = next.id;
    document.getElementById(`tab-${next.id}`)?.focus();
  }

  // ---- announcements and global keys -------------------------------------

  // Phase changes are announced once; the status bar's counters never are.
  let announcedPhase = "";
  $effect(() => {
    const s = status;
    const key = s ? `${s.generation}:${s.phase.kind}` : "";
    if (!s || key === announcedPhase) return;
    announcedPhase = key;
    if (s.phase.kind === "scanning") announce(`Scanning ${s.root}`);
    else if (s.phase.kind === "failed") announce("Scan failed");
    else if (s.phase.kind === "cancelled") announce("Scan cancelled; partial result");
    else announce(`Scan complete: ${plural(s.files, "file")}, ${formatBytes(s.logical)}`);
  });

  function onWindowKey(e: KeyboardEvent) {
    const t = e.target as HTMLElement;
    if (e.key !== "?" || t.closest("input, textarea, dialog")) return;
    e.preventDefault();
    showShortcuts = true;
  }

  // Scan progress: a whole-volume scan can compare against used space.
  const progress = $derived.by(() => {
    if (!scanning || !status) return null;
    if (!volume?.isRoot) return { value: null };
    const used = volume.capacity - volume.free;
    return used > 0 ? { value: Math.min(1, status.allocated / used) } : { value: null };
  });

  function closeFilterHelp(e: PointerEvent) {
    if (!(e.target as HTMLElement).closest(".filter-help")) showFilterHelp = false;
  }
</script>

<svelte:window onkeydown={onWindowKey} onpointerdown={showFilterHelp ? closeFilterHelp : undefined} />

<div class="app">
  <div class="toolbar">
    <div class="group">
      <button type="button" class="primary" onclick={() => scan.run(scanChoose)}>Choose folder or drive…</button>
      {#if generation !== null}
        <!-- One place for both, so the toolbar stays on one row: a scan can
             be cancelled while it runs and repeated once it's done. -->
        {#if scanning}
          <button type="button" onclick={() => scan.cancel()}>Cancel</button>
        {:else}
          <button
            type="button"
            onclick={() => scan.run(scanRefresh)}
            title={status ? `Scan ${status.root} again` : "Scan again"}>Rescan</button
          >
        {/if}
        {#if snapshots.length > 0 || comparison}
          <CompareControl
            {snapshots}
            current={comparison?.snapshot ?? null}
            now={status?.logical ?? 0}
            busy={comparing}
            onchoose={(s) => compare.start(s)}
            onstop={() => compare.stop()}
          />
        {/if}
      {/if}
    </div>
    {#if generation !== null}
      <!-- A list rather than radio buttons keeps the toolbar on one row. -->
      <label
        class="group colors"
        title={comparison ? `Change colors items by how much they changed since ${since}` : undefined}
      >
        Color by
        <select bind:value={colors}>
          <option value="depth">Depth</option>
          <option value="type">File type</option>
          {#if comparison}<option value="change">Change</option>{/if}
        </select>
      </label>
      {#if volume?.isRoot}
        <label class="group check" title="Show the drive's capacity, free space, and used space no file accounts for">
          <input type="checkbox" bind:checked={showVolume} /> Volume overview
        </label>
      {/if}
      <div class="group filter">
        <FilterBox
          bind:value={query}
          {searching}
          {generation}
          oninput={onQueryInput}
          onapply={applyNow}
          onclear={clearQuery}
        />
        <div class="filter-help">
          <button
            type="button"
            class="icon-button help"
            aria-label="Filter syntax"
            aria-expanded={showFilterHelp}
            onclick={() => (showFilterHelp = !showFilterHelp)}>?</button
          >
          {#if showFilterHelp}
            <div class="popover" role="note">
              <p>
                Matches file names, ignoring case. Start a pattern with <code>dir:</code> to match folder names, or
                <code>is:</code> for a file type. <code>size:</code>, <code>older:</code>, and <code>newer:</code> narrow
                the matches by size and modified date. Press ↓ for suggestions.
              </p>
              <dl>
                {#each FILTER_HELP as [pattern, meaning] (pattern)}
                  <dt><code>{pattern}</code></dt>
                  <dd>{meaning}</dd>
                {/each}
              </dl>
            </div>
          {/if}
        </div>
      </div>
    {/if}
    <div class="end">
      {#if privileged}
        <span class="badge" title="Started with administrator or root rights: protected folders are scanned too.">
          Administrator
        </span>
      {:else if canElevate}
        <button
          type="button"
          disabled={elevating || deleting}
          title="Reopen SpaceBadger with administrator rights, so folders that need them are scanned too. Windows asks for permission first."
          onclick={elevate}
        >
          Restart as administrator…
        </button>
      {/if}
      <label
        class="check allow"
        class:on={allowDelete}
        title="Lets you move items to the {trash}: select them, then press Delete or right-click. Turns itself off when SpaceBadger closes."
      >
        <input
          type="checkbox"
          checked={allowDelete}
          onchange={(e) => deletion.setAllowed(e.currentTarget.checked)}
        />
        Allow deleting
      </label>
      <button
        type="button"
        class="icon-button help"
        aria-label="Keyboard and mouse shortcuts"
        title="Keyboard and mouse shortcuts (?)"
        onclick={() => (showShortcuts = true)}>?</button
      >
    </div>
  </div>
  <div class="progress" class:on={progress !== null} aria-hidden="true">
    {#if progress}
      {#if progress.value === null}
        <span class="indeterminate"></span>
      {:else}
        <span style:width="{progress.value * 100}%"></span>
      {/if}
    {/if}
  </div>

  <div class="banners">
    {#if problem}
      <div class="banner error" role="alert">
        <span class="selectable">{problem}</span>
        <button type="button" class="icon-button" onclick={() => (problem = null)} aria-label="Dismiss">✕</button>
      </div>
    {/if}
    {#if status?.phase.kind === "cancelled"}
      <div class="banner warn">
        <span>
          Partial result: the scan was cancelled before every folder was read, so sizes are lower bounds.
          Select a folder to see whether it was scanned.
        </span>
      </div>
    {:else if status?.phase.kind === "failed"}
      <div class="banner error" role="alert"><span class="selectable">{status.phase.message}</span></div>
    {/if}
  </div>

  {#if showVolume && volume?.isRoot && status}
    {#if !search}
      <VolumeStrip {volume} {status} {os} onshowomissions={() => (showOmissions = true)} />
    {:else}
      <p class="volume-note">The volume overview is hidden while a filter is on.</p>
    {/if}
  {/if}

  {#if generation !== null}
    <div class="crumbs">
      {#if viewDetails}
        <Breadcrumbs crumbs={viewDetails.ancestors} onnavigate={openFolder} onup={goUp} />
      {:else if status}
        <span class="muted">{status.root}</span>
      {/if}
      {#if filterError}
        <span class="filtered invalid" role="alert">{filterError}</span>
      {:else if search}
        <span class="filtered">
          Showing files matching <code>{search.query}</code>
          <button type="button" class="icon-button" aria-label="Clear the filter" title="Clear the filter" onclick={clearQuery}
            >✕</button
          >
        </span>
      {/if}
      <!-- Actions on the folder in the map sit beside its path; the toolbar
           is full at 1280 px. -->
      <button
        type="button"
        class="folder-action"
        class:alone={!filterError && !search}
        disabled={scanning}
        title={scanning
          ? duplicatesBlocked
          : "Look for files with the same contents in the folder in the map, matching the filter if one is on. This reads the files; cloud files are skipped."}
        onclick={() => findDuplicates(view)}>Find duplicates</button
      >
      <button
        type="button"
        class="folder-action"
        disabled={exporting !== null}
        title="Save the folder in the map as a CSV file or a text report"
        onclick={() => askExport(view)}>Export…</button
      >
    </div>
  {/if}

  <div class="body">
    <main class="map">
      {#if generation === null}
        <div class="placeholder">
          <h1>See what's using your disk space</h1>
          <p>Choose a folder or a whole drive. The map fills in while it scans: bigger boxes are bigger files.</p>
          <button type="button" class="primary" onclick={() => scan.run(scanChoose)}>Choose folder or drive…</button>
          <p class="small">
            Press <kbd>?</kbd> for keyboard and mouse shortcuts.
          </p>
        </div>
      {:else}
        <Treemap
          {generation}
          {view}
          {metric}
          {colors}
          compare={compareId}
          search={search?.search ?? null}
          revision={status?.revision ?? 0}
          {selection}
          selected={pickedSet}
          {anchor}
          onpick={pick}
          ondelete={askDelete}
          onopen={openFolder}
          onup={goUp}
          onlayout={(l) => (layout = l)}
          onmenu={openTreemapMenu}
        />
        {#if empty}
          <div class="placeholder overlay">
            <p>{empty.text}</p>
            {#if empty.toRoot}
              <button type="button" onclick={() => openFolder(ROOT)}>Go to the scan root</button>
            {/if}
          </div>
        {/if}
        <Legend mode={colors} {since} filtered={search !== null} />
      {/if}
    </main>
    <aside>
      <div class="details">
        <Details
          {selection}
          details={selectedDetails}
          other={otherInfo}
          {multi}
          {view}
          {fileManager}
          trashName={trash}
          {parentSize}
          viewed={viewDetails?.node === view ? viewDetails : null}
          viewedParentSize={viewParentSize}
          error={detailsError}
          onretry={() => detailsRetry++}
          updating={multiUpdating}
          oncopypaths={copyPaths}
          canDelete={allowDelete}
          {deleteBlocked}
          onopen={openFolder}
          onreveal={showInFileManager}
          oncopy={copyPath}
          ondelete={askDelete}
          change={selectedChange}
          viewedChange={viewChange}
          {since}
        />
      </div>
      {#if activeTab && tabs.length > 1}
        <div class="tabs" role="tablist" aria-label="Lists">
          {#each tabs as t (t.id)}
            <button
              type="button"
              role="tab"
              id="tab-{t.id}"
              aria-selected={t.id === activeTab}
              aria-controls="panel-list"
              tabindex={t.id === activeTab ? 0 : -1}
              onclick={() => (tab = t.id)}
              onkeydown={onTabKey}>{t.label}</button
            >
          {/each}
        </div>
      {/if}
      {#if activeTab}
        <!-- With one list there are no tabs, so no tab panel either. -->
        <div
          class="list"
          id="panel-list"
          role={tabs.length > 1 ? "tabpanel" : undefined}
          aria-labelledby={tabs.length > 1 ? `tab-${activeTab}` : undefined}
        >
          {#if activeTab === "contents" && generation !== null}
            {#key view}
              <Contents
                bind:this={contents}
                {generation}
                folder={view}
                folderName={viewDetails?.node === view ? viewDetails.name : ""}
                folderSize={viewDetails?.node === view ? shownSize(viewDetails) : 0}
                search={searchId ?? null}
                revision={status?.revision ?? 0}
                folders={contentsFolders}
                onfolders={(on) => (contentsFolders = on)}
                selected={pickedSet}
                {anchor}
                onpick={pick}
                ondelete={askDelete}
                onopen={(node) => {
                  refocusContents = true;
                  openFolder(node);
                }}
                onshow={(node, parent) => {
                  refocusContents = true;
                  showInMap({ node, parent });
                }}
                onmenu={(r, x, y) => (menu = { x, y, items: contentsMenu(r), heading: menuHeading(r.node, r.name) })}
              />
            {/key}
          {:else if activeTab === "changes" && comparison && generation !== null}
            {#key `${view}:${comparison.snapshot.id}`}
              <Changes
                {generation}
                snapshot={comparison.snapshot.id}
                {since}
                folder={view}
                folderName={viewDetails?.node === view ? viewDetails.name : ""}
                revision={status?.revision ?? 0}
                folders={changesFolders}
                onfolders={(on) => (changesFolders = on)}
                selected={pickedSet}
                {anchor}
                onpick={pick}
                onclear={() => pick(null)}
                ondelete={askDelete}
                onopen={openFolder}
                onshow={(node, parent) => showInMap({ node, parent })}
              />
            {/key}
          {:else if activeTab === "omissions" && status}
            <Omissions
              groups={status.omissions}
              autofocus
              onclose={() => (showOmissions = false)}
              onelevate={canElevate && !elevating && !deleting ? elevate : undefined}
            />
          {:else if activeTab === "small" && smallOf && generation !== null}
            {#key smallOf.folder}
              <SmallItems
                {generation}
                folder={smallOf.folder}
                folderName={smallOf.name}
                count={smallOf.count}
                search={searchId ?? null}
                revision={status?.revision ?? 0}
                selected={pickedSet}
                {anchor}
                onpick={pick}
                ondelete={askDelete}
                onopen={openFolder}
                onclose={() => (smallOf = null)}
                onmenu={(it, x, y) => (menu = { x, y, items: smallItemMenu(it), heading: menuHeading(it.node, it.name) })}
              />
            {/key}
          {:else if activeTab === "duplicates" && dupes && generation !== null}
            <Duplicates
              {generation}
              status={dupes}
              minSize={dupesMinSize}
              canSearch={!scanning}
              selected={pickedSet}
              {anchor}
              onpick={pick}
              ondelete={askDelete}
              onshow={showInMap}
              onmenu={(f, x, y) => (menu = { x, y, items: duplicateMenu(f), heading: menuHeading(f.node, f.path.split(/[\\/]/).pop()) })}
              onsearch={(min) => dupes && findDuplicates(dupes.folder, min)}
              oncancel={stopDuplicates}
              onclose={closeDuplicates}
              onclear={() => pick(null)}
            />
          {:else if activeTab === "results" && search && generation !== null}
            <SearchResults
              {generation}
              summary={search}
              revision={status?.revision ?? 0}
              {scanning}
              scanned={status?.logical ?? 0}
              selected={pickedSet}
              {anchor}
              onpick={pick}
              ondelete={askDelete}
              onshow={showInMap}
              onmenu={(row, x, y) => (menu = { x, y, items: resultItems(row), heading: menuHeading(row.node, row.name) })}
            />
          {/if}
        </div>
      {/if}
    </aside>
  </div>

  <StatusBar
    {status}
    elapsedMs={scan.elapsedMs}
    {showOmissions}
    ontoggleomissions={() => (showOmissions = !showOmissions)}
    oncancel={() => scan.cancel()}
  />

  {#if menu}
    <ContextMenu
      x={menu.x}
      y={menu.y}
      label="Actions"
      heading={menu.heading}
      items={menu.items}
      onclose={() => (menu = null)}
    />
  {/if}
  {#if confirmDelete}
    <DeleteDialog
      summary={confirmDelete.summary}
      trashName={trash}
      filtered={search !== null}
      {privileged}
      duplicates={confirmDelete.duplicates}
      busy={deleting}
      recheck={deletion.recheck}
      onstoprecheck={() => deletion.stopRecheck()}
      result={deletion.result}
      onconfirm={() => deletion.confirmed()}
      oncancel={() => (deletion.confirm = null)}
      onclose={() => deletion.close()}
    />
  {/if}
  {#if exportOf}
    <ExportDialog
      folderName={exportOf.name}
      filter={search?.query ?? null}
      onexport={runExport}
      oncancel={() => (exportOf = null)}
      fallbackFocus={focusMap}
    />
  {/if}
  <!-- Progress shows once the file is chosen, not while the save dialog is open. -->
  {#if exporting ? exporting.rows !== null : exported}
    <ExportToast
      rows={exporting?.rows ?? null}
      report={exporting ? null : exported}
      {fileManager}
      onreveal={showExport}
      onclose={() => (exported = null)}
    />
  {/if}
  {#if showShortcuts}
    <Shortcuts trashName={trash} onclose={() => (showShortcuts = false)} />
  {/if}
  <div class="notice" aria-hidden="true">{#if notice}<span>{notice}</span>{/if}</div>
  <Announcer />
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
    gap: var(--space-2) 0;
    padding: var(--space-2) var(--space-3);
    background: var(--panel);
  }
  /* Control groups, divided by a rule. */
  .group {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    margin: 0;
    padding: 0 var(--space-3);
    border: none;
    border-left: 1px solid var(--line);
  }
  .group:first-child {
    padding-left: 0;
    border-left: none;
  }
  .colors {
    color: var(--muted);
  }
  .colors select {
    color: var(--fg);
  }
  .check {
    display: flex;
    align-items: center;
    gap: 4px;
    cursor: pointer;
  }
  /* The group that wraps to a second row first: a rule there would
     dangle at the row's start. */
  .filter {
    border-left: none;
    flex: 1 1 150px;
    max-width: 420px;
    min-width: 150px;
  }
  .filter-help {
    position: relative;
  }
  .popover {
    position: absolute;
    top: calc(100% + 6px);
    right: 0;
    z-index: 20;
    width: 320px;
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--line);
    border-radius: 6px;
    background: var(--bg);
    box-shadow: 0 6px 20px rgb(0 0 0 / 0.2);
    font-size: var(--fs-sm);
  }
  .popover p {
    margin: 0 0 var(--space-2);
  }
  .popover dl {
    display: grid;
    grid-template-columns: max-content 1fr;
    gap: 4px 10px;
    margin: 0;
  }
  .popover dd {
    margin: 0;
  }
  .end {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    margin-left: auto;
    padding-left: var(--space-3);
  }
  .allow {
    padding: 1px 6px;
    border-radius: var(--radius);
  }
  .allow.on {
    background: var(--danger-bg);
    color: var(--danger);
    font-weight: 600;
  }
  .help {
    border: 1px solid var(--line);
    border-radius: 50%;
    font-weight: 600;
  }
  .badge {
    padding: 2px 8px;
    border-radius: 10px;
    background: var(--warn-bg);
    font-size: var(--fs-sm);
  }
  /* Thin bar under the toolbar while scanning. */
  .progress {
    position: relative;
    height: 2px;
    overflow: hidden;
    border-bottom: 1px solid var(--line);
    background: var(--panel);
  }
  .progress span {
    position: absolute;
    inset: 0 auto 0 0;
    background: var(--accent);
    transition: width 0.3s;
  }
  .progress .indeterminate {
    width: 30%;
    animation: slide 1.4s ease-in-out infinite;
  }
  @keyframes slide {
    from {
      left: -30%;
    }
    to {
      left: 100%;
    }
  }
  .banners {
    max-height: 30vh;
    overflow-y: auto;
  }
  .banner {
    display: flex;
    justify-content: space-between;
    align-items: flex-start;
    gap: 8px;
    white-space: pre-line;
    padding: 6px 12px;
    border-bottom: 1px solid var(--line);
  }
  .volume-note {
    margin: 0;
    padding: 4px 12px;
    border-bottom: 1px solid var(--line);
    color: var(--muted);
    font-size: var(--fs-sm);
  }
  .crumbs {
    display: flex;
    align-items: center;
    padding: 2px 8px;
    min-height: 28px;
    border-bottom: 1px solid var(--line);
  }
  .crumbs .folder-action {
    flex: none;
    margin-left: var(--space-2);
    padding: 1px 8px;
    font-size: var(--fs-sm);
  }
  .crumbs .folder-action.alone {
    margin-left: auto;
  }
  .filtered {
    display: flex;
    flex: none;
    align-items: center;
    gap: 4px;
    margin-left: auto;
    padding: 0 2px 0 8px;
    border-radius: 12px;
    background: var(--selected);
    font-size: var(--fs-sm);
  }
  .filtered.invalid {
    padding: 2px 10px;
    background: none;
    color: var(--danger);
  }
  .body {
    flex: 1;
    display: grid;
    grid-template-columns: minmax(0, 1fr) clamp(260px, 28%, 380px);
    min-height: 0;
  }
  .map {
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
  }
  /* A fixed share of the panel, so the list below doesn't jump as the
     selection changes; long details scroll. */
  .details {
    flex: none;
    height: clamp(200px, 45%, 480px);
    overflow-y: auto;
    border-bottom: 1px solid var(--line);
  }
  .tabs {
    display: flex;
    flex: none;
    gap: 2px;
    padding: 0 var(--space-2);
    border-bottom: 1px solid var(--line);
  }
  .tabs button {
    padding: 6px 10px;
    border: none;
    border-bottom: 2px solid transparent;
    border-radius: 0;
    background: none;
    color: var(--muted);
  }
  .tabs button[aria-selected="true"] {
    border-bottom-color: var(--accent);
    color: var(--fg);
    font-weight: 600;
  }
  .list {
    display: flex;
    flex-direction: column;
    flex: 1;
    min-height: 0;
  }
  /* Narrow windows put the side panel under the map. */
  @media (max-width: 800px) {
    .body {
      grid-template-columns: minmax(0, 1fr);
      grid-template-rows: minmax(0, 1fr) minmax(0, 40%);
    }
    /* One scroll area: the details at full height, then the list. */
    aside {
      border-left: none;
      border-top: 1px solid var(--line);
      overflow-y: auto;
    }
    .details {
      height: auto;
      overflow: visible;
    }
    .list {
      flex: none;
      height: 280px;
    }
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
  .placeholder h1 {
    margin: 0;
    font-size: 20px;
    color: var(--fg);
  }
  .placeholder p {
    max-width: 420px;
    margin: 0;
  }
  .placeholder kbd {
    padding: 0 5px;
    border: 1px solid var(--line);
    border-radius: var(--radius);
    font: inherit;
  }
  .overlay {
    position: absolute;
    inset: 0;
    pointer-events: none;
  }
  .overlay button {
    pointer-events: auto;
  }
  .notice {
    position: fixed;
    bottom: 48px;
    left: 50%;
    transform: translateX(-50%);
    pointer-events: none;
  }
  .notice span {
    display: block;
    padding: 6px 12px;
    border-radius: var(--radius);
    background: var(--fg);
    color: var(--bg);
    font-size: var(--fs-sm);
  }
  .warn {
    background: var(--warn-bg);
  }
  .error {
    background: var(--danger-bg);
    color: inherit;
  }
</style>
