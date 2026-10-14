// Request identity for replies that arrive out of order. Backend calls run
// concurrently, so a reply can come back after a newer request for the same
// thing, or after the component that asked is gone. Every place that applies
// a reply to state asks a `Latest` first, and none is applied once it's
// disposed.
//
// A request has an identity, every input that decides what is asked (scan,
// node, filter, comparison), and usually the scan revision it was asked at.
// A new identity supersedes everything before it. A newer revision of the
// same identity doesn't: while a scan runs, revisions come faster than some
// replies, so the request in flight is kept, the newest one waits behind it,
// and a reply is applied unless a newer revision's already was.
//
// The backend can drop superseded work too: a request asked with a `Lane`
// supersedes the older ones of its lane still waiting or running, which
// reject with a "superseded" error. Every `Latest` has lanes of its own and
// numbers them so that only requests whose replies it would drop anyway
// are superseded, never the newest one; it drops that error silently.
import { onDestroy } from "svelte";
import { isSuperseded } from "./api";
import type { Lane } from "./protocol/Lane";

/** One request as `Latest.begin` handed it out. */
export interface Ticket {
  readonly seq: number;
  /** Everything the answer depends on; see `queryKey`. */
  readonly key: string;
  /** For the ticket's requests: a newer ticket supersedes them. */
  readonly lane: Lane;
}

// Lane keys start with a prefix of this page load, so after the webview
// reloads, requests of the old page still in the backend never share a lane
// with (and supersede or outrank) the new page's.
const page = Array.from(crypto.getRandomValues(new Uint32Array(2)), (n) => n.toString(36)).join("");
let lanes = 0;

/**
 * A lane of its own for requests whose newest alone matters, such as the
 * tooltip: each `next()` supersedes the requests asked with earlier ones.
 */
export class Lanes {
  readonly #key = `${page}-lane${++lanes}`;
  #seq = 0;

  next(): Lane {
    return { key: this.#key, seq: ++this.#seq };
  }
}

/**
 * The identity of a query: every input that affects its answer apart from
 * the scan revision, such as the scan generation, node, filter, metric, and
 * comparison. Two requests with the same key ask the same question.
 */
export function queryKey(...parts: (string | number | boolean | null | undefined)[]): string {
  return JSON.stringify(parts);
}

export class Latest {
  /** Lanes for `run` (numbered by identity) and `begin` (by ticket). */
  readonly #lane = `${page}-latest${++lanes}`;
  #disposed = false;
  /** The identity asked about; `null` when nothing is. */
  #key: string | null = null;
  /** Counts identities, so going A, B, then A again drops A's first replies. */
  #epoch = 0;
  /** Counts `begin` calls; only the newest ticket is current. */
  #seq = 0;
  /** The newest revision applied for this identity. */
  #applied = -Infinity;
  #inFlight = false;
  #pending: (() => void) | null = null;

  #identify(key: string | null) {
    if (key === this.#key) return;
    this.#key = key;
    this.#epoch++;
    this.#applied = -Infinity;
    this.#inFlight = false;
    this.#pending = null;
  }

  /** Whether `key` is the identity being asked about. */
  follows(key: string): boolean {
    return !this.#disposed && key === this.#key;
  }

  /**
   * Starts a request that supersedes every earlier one, whatever its
   * identity; for requests applied in steps, such as a search.
   */
  begin(key = ""): Ticket {
    this.#identify(key);
    const seq = ++this.#seq;
    return { seq, key, lane: { key: `${this.#lane}b`, seq } };
  }

  /** Whether the reply to `t` should still be applied. */
  current(t: Ticket): boolean {
    return !this.#disposed && t.seq === this.#seq;
  }

  /**
   * Asks `request` about `key` at `revision` and passes the outcome on
   * while `key` is still the identity asked about and no newer revision's
   * reply was applied. One request per identity runs at a time; asking
   * again meanwhile replaces the one waiting. `request` is given the lane
   * to ask with, so a new identity supersedes the old one's in the backend.
   */
  run<T>(
    key: string,
    revision: number,
    request: (lane: Lane) => Promise<T>,
    onvalue: (value: T) => void,
    onerror?: (e: unknown) => void,
  ) {
    if (this.#disposed) return;
    this.#identify(key);
    const epoch = this.#epoch;
    const lane: Lane = { key: `${this.#lane}r`, seq: epoch };
    const settle = (apply: () => void, dropped = false) => {
      if (this.#disposed || epoch !== this.#epoch) return;
      this.#inFlight = false;
      // Only an older identity's requests are superseded, and those never
      // get here; should one anyway, it's dropped like them.
      if (!dropped && revision >= this.#applied) {
        this.#applied = revision;
        apply();
      }
      const next = this.#pending;
      this.#pending = null;
      next?.();
    };
    const start = () => {
      this.#inFlight = true;
      request(lane).then(
        (v) => settle(() => onvalue(v)),
        (e: unknown) => settle(() => onerror?.(e), isSuperseded(e)),
      );
    };
    if (this.#inFlight) this.#pending = start;
    else start();
  }

  /** Nothing is wanted any more, e.g. the thing asked about went away. */
  cancel() {
    this.#seq++;
    this.#identify(null);
  }

  /** For good: the owner is gone. */
  dispose() {
    this.cancel();
    this.#disposed = true;
  }
}

/** A `Latest` that's disposed with the component creating it, so replies
 * arriving after teardown are dropped. Call during component setup. */
export function latest(): Latest {
  const l = new Latest();
  onDestroy(() => l.dispose());
  return l;
}

/**
 * Runs jobs one at a time, keeping only the newest that's waiting: a burst of
 * refreshes becomes the one running plus the latest asked for. A job should
 * read its inputs when it starts, not when it's queued.
 */
export class Coalescer {
  #running = false;
  #pending: (() => Promise<unknown>) | null = null;
  #disposed = false;

  run(job: () => Promise<unknown>) {
    if (this.#disposed) return;
    if (this.#running) {
      this.#pending = job;
      return;
    }
    this.#running = true;
    void job()
      .catch(() => {})
      .finally(() => {
        this.#running = false;
        const next = this.#pending;
        this.#pending = null;
        if (next) this.run(next);
      });
  }

  /** Drops the waiting job; the running one finishes. */
  clear() {
    this.#pending = null;
  }

  dispose() {
    this.clear();
    this.#disposed = true;
  }
}
