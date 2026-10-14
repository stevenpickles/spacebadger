// The scan being shown: which one, its latest status, and starting or
// cancelling scans. A newer scan (a higher generation) always replaces the
// one shown, whether it's reported by a status event or by the command
// that started it; what else starts over is up to the owner's `onadopt`.
import { onScanStatus, scanCancel, scanStatus } from "../api";
import type { ScanStarted } from "../protocol/ScanStarted";
import type { ScanStatus } from "../protocol/ScanStatus";

/** A newer scan replaced the one shown; `previous` was its last status. */
type OnAdopt = (previous: ScanStatus | null) => void;

export class ScanController {
  generation = $state<number | null>(null);
  status = $state.raw<ScanStatus | null>(null);
  #statusAt = $state(0);
  #now = $state(performance.now());
  #onerror: (message: string) => void;
  #onadopt: OnAdopt = () => {};

  readonly scanning = $derived(this.status?.phase.kind === "scanning");
  readonly finished = $derived(this.status !== null && this.status.phase.kind !== "scanning");
  /** Only a completed scan is saved, and only it can be compared. */
  readonly completed = $derived(
    this.status?.phase.kind === "complete" || this.status?.phase.kind === "completeWithOmissions",
  );
  readonly elapsedMs = $derived(
    this.status
      ? this.status.elapsedMs + (this.scanning ? Math.max(0, this.#now - this.#statusAt) : 0)
      : 0,
  );

  constructor(onerror: (message: string) => void) {
    this.#onerror = onerror;
  }

  /**
   * Starts following the backend's scans, calling `onadopt` whenever a
   * newer one is shown. Call during component setup, once everything
   * `onadopt` uses is in place: status may arrive as soon as it mounts.
   */
  follow(onadopt: OnAdopt) {
    this.#onadopt = onadopt;

    $effect(() => {
      const unlisten = onScanStatus((s) => this.receive(s));
      scanStatus().then((s) => s && this.receive(s), () => {});
      return () => void unlisten.then((f) => f());
    });

    // Local clock so elapsed time keeps moving between coalesced updates.
    $effect(() => {
      if (!this.scanning) return;
      const timer = setInterval(() => (this.#now = performance.now()), 250);
      return () => clearInterval(timer);
    });
  }

  /** Switches to a newer scan; earlier ones are ignored. */
  adopt(next: number) {
    if (this.generation !== null && next <= this.generation) return;
    const previous = this.status;
    this.generation = next;
    this.status = null;
    this.#onadopt(previous);
  }

  receive(s: ScanStatus) {
    this.adopt(s.generation);
    if (s.generation !== this.generation) return;
    // Coalesced events can arrive out of order; a final status always counts.
    if (this.status && s.revision < this.status.revision && s.phase.kind === "scanning") return;
    this.status = s;
    this.#statusAt = performance.now();
  }

  /** Runs a command that may start a scan, such as the folder picker. */
  async run(action: () => Promise<ScanStarted | null>) {
    try {
      const started = await action();
      if (started) this.adopt(started.generation);
    } catch (e) {
      this.#onerror(String(e));
    }
  }

  cancel() {
    if (this.generation !== null) scanCancel(this.generation).catch((e: unknown) => this.#onerror(String(e)));
  }
}
