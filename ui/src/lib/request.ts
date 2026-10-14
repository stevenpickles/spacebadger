// Request identity for replies that arrive out of order. Backend calls run
// concurrently, so a reply can come back after a newer request for the same
// thing, or after the component that asked is gone. Every place that applies
// a reply to state asks a `Latest` first: only the newest request it started
// is still wanted, and none once it's disposed.
import { onDestroy } from "svelte";

/** One request as `Latest` handed it out. */
export interface Ticket {
  readonly seq: number;
  /** Everything the answer depends on; see `queryKey`. */
  readonly key: string;
}

/**
 * The identity of a query: every input that affects its answer, such as the
 * scan generation, revision, node, filter, metric, and comparison. Two
 * requests with the same key would get the same answer.
 */
export function queryKey(...parts: (string | number | boolean | null | undefined)[]): string {
  return JSON.stringify(parts);
}

/** Latest request wins: starting one supersedes every earlier one. */
export class Latest {
  #seq = 0;
  #key: string | null = null;
  #disposed = false;

  /** Starts a request; replies to earlier ones are no longer wanted. */
  begin(key = ""): Ticket {
    this.#key = key;
    return { seq: ++this.#seq, key };
  }

  /** Whether the reply to `t` should still be applied. */
  current(t: Ticket): boolean {
    return !this.#disposed && t.seq === this.#seq;
  }

  /** Whether an answer for `key` is what the newest request asked for, for
   * replies that say themselves what they answer (a revision, a token). */
  accepts(key: string): boolean {
    return !this.#disposed && key === this.#key;
  }

  /** Nothing is wanted any more, e.g. the thing asked about went away. */
  cancel() {
    this.#seq++;
    this.#key = null;
  }

  /** For good: the owner is gone. */
  dispose() {
    this.cancel();
    this.#disposed = true;
  }

  /** Starts `request` and passes its outcome on only while it's the newest. */
  run<T>(key: string, request: () => Promise<T>, onvalue: (value: T) => void, onerror?: (e: unknown) => void): Ticket {
    const t = this.begin(key);
    request().then(
      (v) => {
        if (this.current(t)) onvalue(v);
      },
      (e: unknown) => {
        if (this.current(t)) onerror?.(e);
      },
    );
    return t;
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
