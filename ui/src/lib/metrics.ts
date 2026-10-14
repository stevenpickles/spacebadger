// Opt-in timings of what the interface waits for: request round trips and
// decoding layouts (painting is left to the map). Kept only when the backend
// keeps its own (`SPACEBADGER_TRACE` is set), so normal runs pay one boolean
// check per call. Read them from the developer tools with
// `await spacebadgerTrace.full()`, which adds the backend's table (queue,
// lock waits and holds, search catch-up, ranking, layout, encoding) so a
// round trip can be split into its causes; `spacebadgerTrace.reset()`
// starts over.
//
// Usage: `timed("layout", promise)` for a round trip, `measure("decode
// layout", () => ...)` for synchronous work, or `record(name, ms)` with a
// time taken elsewhere.

/** Samples kept per name; older ones are dropped first. */
const LIMIT = 20_000;

let enabled = false;
const samples = new Map<string, { values: number[]; next: number; count: number }>();
let backend: ((reset: boolean) => Promise<string | null>) | null = null;

/** Turns timings on (when the backend keeps them) and tells where the
 * backend's report comes from. */
export function enableTrace(on: boolean, backendReport: (reset: boolean) => Promise<string | null>) {
  enabled = on;
  backend = backendReport;
  if (on) (globalThis as { spacebadgerTrace?: unknown }).spacebadgerTrace = { report, reset, full };
}

export function traceEnabled(): boolean {
  return enabled;
}

/** Notes one time, in milliseconds, under `name`. */
export function record(name: string, ms: number) {
  if (!enabled) return;
  let s = samples.get(name);
  if (!s) {
    s = { values: [], next: 0, count: 0 };
    samples.set(name, s);
  }
  if (s.values.length < LIMIT) s.values.push(ms);
  else s.values[s.next] = ms;
  s.next = (s.next + 1) % LIMIT;
  s.count++;
}

/** Times `work` under `name`. */
export function measure<T>(name: string, work: () => T): T {
  if (!enabled) return work();
  const started = performance.now();
  try {
    return work();
  } finally {
    record(name, performance.now() - started);
  }
}

/** Times a request from now until it settles, under `name`. */
export function timed<T>(name: string, request: Promise<T>): Promise<T> {
  if (!enabled) return request;
  const started = performance.now();
  const done = () => record(name, performance.now() - started);
  request.then(done, done);
  return request;
}

/** p50, p95, p99, and max of every name, in ms, as a text table. */
export function report(): string {
  const pad = (s: string, n: number) => s.padStart(n);
  const lines = [`${"name".padEnd(24)}${pad("count", 8)}${pad("p50", 9)}${pad("p95", 9)}${pad("p99", 9)}${pad("max", 9)}`];
  for (const [name, s] of [...samples].sort(([a], [b]) => a.localeCompare(b))) {
    const sorted = [...s.values].sort((a, b) => a - b);
    const n = sorted.length;
    const at = (q: number) => sorted[Math.min(n - 1, Math.max(0, Math.ceil(q * n) - 1))]!;
    const f = (v: number) => pad(v.toFixed(2), 9);
    lines.push(`${name.padEnd(24)}${pad(String(s.count), 8)}${f(at(0.5))}${f(at(0.95))}${f(at(0.99))}${f(sorted[n - 1]!)}`);
  }
  return lines.join("\n");
}

export function reset() {
  samples.clear();
  void backend?.(true);
}

/** This report, then the backend's. */
async function full(): Promise<string> {
  const theirs = (await backend?.(false)) ?? "(backend timings are off)";
  return `interface (ms):\n${report()}\n\nbackend:\n${theirs}`;
}
