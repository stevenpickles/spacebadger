// Moving items to the Recycle Bin or Trash: the "Allow deleting" switch,
// the confirmation with its duplicates check, the delete itself, and its
// result. Every step checks the scan it started on is still shown.
import { announce } from "../announce.svelte";
import {
  deleteItems,
  duplicatesCheck,
  duplicatesRecheckCancel,
  errorText,
  onDuplicatesRecheck,
  selectionSummary,
  setDeleteAllowed,
} from "../api";
import type { DeleteResult } from "../DeleteDialog.svelte";
import { formatBytes, plural } from "../format";
import type { DeleteReport } from "../protocol/DeleteReport";
import type { DuplicatesCheck } from "../protocol/DuplicatesCheck";
import type { DuplicatesRecheck } from "../protocol/DuplicatesRecheck";
import type { SelectionSummary } from "../protocol/SelectionSummary";
import type { ScanController } from "./scan.svelte";

/** How long the delete dialog waits for the duplicates check, which
 * looks at files on disk and can be slow on a network share. */
const DUPLICATES_CHECK_WAIT = 2000;

interface Inputs {
  scan: ScanController;
  /** What a delete applies to now. */
  targets: () => number[];
  /** The duplicate search shown, whose groups the delete is checked against. */
  duplicatesJob: () => number | undefined;
  /** "Recycle Bin" or "Trash". */
  trash: () => string;
}

interface Hooks {
  /** A short message, e.g. why deleting isn't possible yet. */
  onnotice: (text: string, ms?: number) => void;
  onerror: (message: string) => void;
  /** Items were moved; the scan already left them out. */
  ondeleted: (report: DeleteReport) => void;
}

export class DeletionController {
  #scan: ScanController;
  #inputs: Inputs;
  #hooks: Hooks;

  /** Deleting is turned on. Never saved: every start begins with it off. */
  allowed = $state(false);
  confirm = $state.raw<{
    nodes: number[];
    summary: SelectionSummary;
    /** Groups of duplicates the delete would leave with no copy. */
    duplicates: DuplicatesCheck | "unknown" | null;
  } | null>(null);
  running = $state(false);
  /** How far the running delete has got reading copies of duplicates
   * that would stay, before moving anything. */
  recheck = $state.raw<DuplicatesRecheck | null>(null);
  /** What the last delete did, shown in the dialog until it's closed. */
  result = $state.raw<DeleteResult | null>(null);
  /** Counts completed deletes, which change free space. */
  count = $state(0);

  /** Call during component setup: it listens for recheck progress while mounted. */
  constructor(inputs: Inputs, hooks: Hooks) {
    this.#scan = inputs.scan;
    this.#inputs = inputs;
    this.#hooks = hooks;

    $effect(() => {
      const unlisten = onDuplicatesRecheck((p) => {
        if (p.generation === this.#scan.generation && this.running) this.recheck = p;
      });
      return () => void unlisten.then((f) => f());
    });
  }

  /** Why deleting isn't possible now, if it isn't. */
  get blocked(): string | null {
    const scan = this.#scan;
    return !scan.status || scan.scanning ? "Deleting is available once the scan finishes or is cancelled." : null;
  }

  async setAllowed(on: boolean) {
    try {
      this.allowed = await setDeleteAllowed(on);
    } catch (e) {
      this.allowed = false;
      this.#hooks.onerror(`Couldn't change the delete setting: ${String(e)}`);
    }
  }

  /** Whether deleting `nodes` loses every copy of some duplicates;
   * "unknown" if that couldn't be told in time. */
  #checkDuplicates(gen: number, job: number, nodes: number[]): Promise<DuplicatesCheck | "unknown" | null> {
    const late = new Promise<"unknown">((resolve) => setTimeout(() => resolve("unknown"), DUPLICATES_CHECK_WAIT));
    const check = duplicatesCheck({ generation: gen, job, nodes }).catch(() => "unknown" as const);
    return Promise.race([check, late]);
  }

  /** Opens the confirmation for the current targets. */
  async ask() {
    const gen = this.#scan.generation;
    const nodes = this.#inputs.targets();
    if (gen === null || nodes.length === 0 || this.confirm) return;
    if (!this.allowed) {
      const text = `Turn on “Allow deleting” in the toolbar to move items to the ${this.#inputs.trash()}.`;
      this.#hooks.onnotice(text, 3000);
      return;
    }
    if (this.blocked) {
      this.#hooks.onnotice(this.blocked);
      return;
    }
    try {
      // Duplicates found may be deleted all at once, or the copy meant to
      // stay may have gone meanwhile; the dialog says so.
      const job = this.#inputs.duplicatesJob();
      const [summary, duplicates] = await Promise.all([
        selectionSummary({ generation: gen, nodes }),
        job === undefined ? null : this.#checkDuplicates(gen, job, nodes),
      ]);
      if (gen === this.#scan.generation && summary.items > 0) this.confirm = { nodes, summary, duplicates };
    } catch (e) {
      this.#hooks.onerror(`Couldn't prepare the delete: ${String(e)}`);
    }
  }

  async confirmed() {
    const c = this.confirm;
    const gen = this.#scan.generation;
    if (!c || gen === null || this.running) return;
    this.running = true;
    this.recheck = null;
    try {
      const report = await deleteItems({ generation: gen, nodes: c.nodes });
      // The dialog stays open on its result screen until closed.
      if (gen === this.#scan.generation) this.#done(report);
      else this.confirm = null;
    } catch (e) {
      this.confirm = null;
      this.#hooks.onerror(`Couldn't move to the ${this.#inputs.trash()}: ${errorText(e)}`);
    } finally {
      this.running = false;
      this.recheck = null;
    }
  }

  /** Stops reading the copies that would stay; those not confirmed are kept. */
  stopRecheck() {
    const gen = this.#scan.generation;
    if (gen !== null) duplicatesRecheckCancel(gen).catch(() => {});
  }

  /** Closes the confirmation, or the result after a delete. */
  close() {
    this.confirm = null;
    this.result = null;
  }

  #done(r: DeleteReport) {
    const trash = this.#inputs.trash();
    this.count++;
    this.#hooks.ondeleted(r);
    const total = r.recycled.length + r.others.length;
    this.result = { total, recycled: r.recycled.length, size: r.logical, others: r.others };
    const count = (o: DeleteReport["others"][number]["outcome"]) => r.others.filter((i) => i.outcome === o).length;
    const notes = [
      [count("uncertain"), "not confirmed in the " + trash],
      [count("kept"), "kept as possibly the last copy"],
      [count("changed"), "changed since the scan"],
      [count("failed"), "couldn't be moved"],
      [count("alreadyMissing"), "no longer there"],
    ] as const;
    const rest = notes.filter(([n]) => n > 0).map(([n, what]) => `${n} ${what}`);
    announce(
      rest.length > 0
        ? `Moved ${r.recycled.length} of ${total} items to the ${trash}; ${rest.join(", ")}`
        : `Moved ${plural(r.recycled.length, "item")} (${formatBytes(r.logical)}) to the ${trash}`,
    );
  }
}
