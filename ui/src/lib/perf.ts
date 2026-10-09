// Rolling timing samples for the diagnostic overlay (toggle with ` in the map).

export class Samples {
  private values: number[] = [];

  constructor(private readonly limit = 500) {}

  add(ms: number) {
    this.values.push(ms);
    if (this.values.length > this.limit) this.values.shift();
  }

  /** "p50 x · p95 y · max z ms (n)" over the retained samples. */
  summary(): string {
    const n = this.values.length;
    if (n === 0) return "no samples";
    const sorted = [...this.values].sort((a, b) => a - b);
    const at = (q: number) => sorted[Math.min(n - 1, Math.floor(q * n))]!;
    const f = (v: number) => v.toFixed(1);
    return `p50 ${f(at(0.5))} · p95 ${f(at(0.95))} · max ${f(sorted[n - 1]!)} ms (${n})`;
  }
}
