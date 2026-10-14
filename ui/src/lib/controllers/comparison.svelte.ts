// Comparing with an earlier scan: the scans offered, the comparison that's
// on, and how the selected item and the viewed folder changed. A rescan of
// the same folder takes the comparison up again once it completes.
import { untrack } from "svelte";
import { announce } from "../announce.svelte";
import { compareItem, compareSet, compareSnapshots, errorText } from "../api";
import { formatWhen } from "../compare";
import type { CompareSummary } from "../protocol/CompareSummary";
import type { ItemChange } from "../protocol/ItemChange";
import type { ScanStatus } from "../protocol/ScanStatus";
import type { SnapshotInfo } from "../protocol/SnapshotInfo";
import { latest, queryKey } from "../request";
import type { ScanController } from "./scan.svelte";

interface Inputs {
  scan: ScanController;
  /** The item whose change the details show, if any. */
  selected: () => number | null;
  /** The folder in view. */
  view: () => number;
}

interface Hooks {
  /** A comparison started; `again` when taken up after a rescan. */
  onstart: (summary: CompareSummary, again: boolean) => void;
  /** The comparison ended, by the user or a new scan. */
  onend: () => void;
  onerror: (message: string) => void;
}

export class ComparisonController {
  /** Earlier scans of this root, newest first, offered once a scan completes. */
  snapshots = $state.raw<SnapshotInfo[]>([]);
  /** The comparison that's on. Nothing about it is remembered across restarts. */
  comparison = $state.raw<CompareSummary | null>(null);
  busy = $state(false);
  selectedChange = $state.raw<ItemChange | null>(null);
  viewChange = $state.raw<ItemChange | null>(null);
  readonly id = $derived(this.comparison?.snapshot.id ?? null);
  readonly since = $derived(this.comparison ? formatWhen(this.comparison.snapshot.createdMs) : "");

  /** The comparison to take up again after a rescan of the same folder. */
  #carried: { snapshot: SnapshotInfo; root: string } | null = null;
  #scan: ScanController;
  #hooks: Hooks;

  /** Call during component setup. */
  constructor({ scan, selected, view }: Inputs, hooks: Hooks) {
    this.#scan = scan;
    this.#hooks = hooks;

    $effect(() => {
      const gen = scan.generation;
      if (gen === null || !scan.completed) return;
      compareSnapshots(gen).then(
        (list) => {
          if (gen === scan.generation) this.snapshots = list;
        },
        () => {},
      );
    });

    $effect(() => {
      const gen = scan.generation;
      const status = scan.status;
      if (gen === null || !scan.completed || !status) return;
      const root = status.root;
      untrack(() => {
        const c = this.#carried;
        this.#carried = null;
        // Quietly: the earlier scan may have been pruned meanwhile.
        if (c && c.root === root) void this.start(c.snapshot, true);
      });
    });

    // How the selected item and the viewed folder changed, for the details.
    // Only the newest request counts: selecting A then B must not show A's
    // change beside B when A's reply comes last.
    const selectedRequest = latest();
    const viewRequest = latest();
    $effect(() => {
      const gen = scan.generation;
      const cmp = this.id;
      const node = selected();
      const revision = scan.status?.revision;
      if (gen === null || cmp === null || node === null) {
        selectedRequest.cancel();
        this.selectedChange = null;
        return;
      }
      selectedRequest.run(
        queryKey(gen, revision, cmp, node),
        () => compareItem(gen, cmp, node),
        (c) => {
          if (c.generation === gen && c.node === node) this.selectedChange = c;
        },
      );
    });
    $effect(() => {
      const gen = scan.generation;
      const cmp = this.id;
      const node = view();
      const revision = scan.status?.revision;
      if (gen === null || cmp === null) {
        viewRequest.cancel();
        this.viewChange = null;
        return;
      }
      viewRequest.run(
        queryKey(gen, revision, cmp, node),
        () => compareItem(gen, cmp, node),
        (c) => {
          if (c.generation === gen && c.node === node) this.viewChange = c;
        },
      );
    });
  }

  async start(s: SnapshotInfo, again = false) {
    const gen = this.#scan.generation;
    if (gen === null || this.busy) return;
    this.busy = true;
    try {
      const summary = await compareSet(gen, s.id);
      if (gen !== this.#scan.generation || !summary) return;
      this.comparison = summary;
      this.#hooks.onstart(summary, again);
      announce(`Comparing with the scan of ${formatWhen(s.createdMs)}`);
    } catch (e) {
      if (!again) this.#hooks.onerror(`Couldn't compare: ${errorText(e)}`);
    } finally {
      this.busy = false;
    }
  }

  stop() {
    const gen = this.#scan.generation;
    if (gen !== null) compareSet(gen, null).catch(() => {});
    this.#end();
  }

  /** A newer scan replaced the one compared, whose last status was
   * `previous`; a rescan of the same folder compares again once complete. */
  restart(previous: ScanStatus | null) {
    this.#carried = this.comparison && previous ? { snapshot: this.comparison.snapshot, root: previous.root } : null;
    this.snapshots = [];
    this.#end();
  }

  #end() {
    this.comparison = null;
    this.selectedChange = null;
    this.viewChange = null;
    this.#hooks.onend();
  }
}
