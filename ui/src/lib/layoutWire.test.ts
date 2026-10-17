import { describe, expect, it } from "vitest";
import { decodeLayout, hitTest, isFiller, KIND_FOLDER, KIND_OTHER, LAYOUT_FINAL, LAYOUT_PROVISIONAL } from "./layoutWire";

interface RectSpec {
  x: number;
  y: number;
  w: number;
  h: number;
  node: number;
  count: number;
  weight: number;
  kind: number;
  more: number;
}

/** A layout reply laid out as sb_protocol::wire::encode writes format 5. */
function encode(flags: number, estimatedFrom: number, rects: RectSpec[], labels: [number, string][]): ArrayBuffer {
  const text = new TextEncoder();
  const encoded = labels.map(([i, s]) => [i, text.encode(s)] as const);
  const size = 56 + rects.length * 44 + encoded.reduce((n, [, b]) => n + 6 + b.length, 0);
  const buf = new ArrayBuffer(size);
  const v = new DataView(buf);
  v.setUint32(0, 0x314c4253, true);
  v.setUint16(4, 5, true);
  v.setUint16(6, 56, true);
  v.setBigUint64(8, 3n, true);
  v.setBigUint64(16, 9n, true);
  v.setUint32(24, 7, true);
  v.setUint32(28, 0, true);
  v.setBigUint64(32, 1000n, true);
  v.setUint32(40, rects.length, true);
  v.setUint32(44, encoded.length, true);
  v.setUint32(48, flags, true);
  v.setUint32(52, estimatedFrom, true);
  rects.forEach((r, i) => {
    const o = 56 + i * 44;
    v.setFloat32(o, r.x, true);
    v.setFloat32(o + 4, r.y, true);
    v.setFloat32(o + 8, r.w, true);
    v.setFloat32(o + 12, r.h, true);
    v.setUint32(o + 16, r.node, true);
    v.setUint32(o + 20, r.count, true);
    v.setBigUint64(o + 24, BigInt(r.weight), true);
    v.setUint8(o + 32, 1);
    v.setUint8(o + 33, r.kind);
    v.setUint8(o + 34, 0);
    v.setUint8(o + 35, 0);
    v.setUint32(o + 36, 0, true);
    v.setUint8(o + 40, r.more);
  });
  let o = 56 + rects.length * 44;
  const bytes = new Uint8Array(buf);
  for (const [i, b] of encoded) {
    v.setUint32(o, i, true);
    v.setUint16(o + 4, b.length, true);
    bytes.set(b, o + 6);
    o += 6 + b.length;
  }
  return buf;
}

describe("decodeLayout", () => {
  it("reads format 5's provisional byte and the estimate's time", () => {
    const l = decodeLayout(
      encode(
        LAYOUT_PROVISIONAL,
        1_792_000_000,
        [
          { x: 0, y: 0, w: 60, h: 100, node: 4, count: 1, weight: 0, kind: KIND_FOLDER, more: 1 },
          { x: 60, y: 0, w: 30, h: 100, node: 5, count: 1, weight: 900, kind: KIND_FOLDER, more: 0 },
          { x: 90, y: 0, w: 10, h: 100, node: 0, count: 0, weight: 0, kind: KIND_OTHER, more: 1 },
        ],
        [[1, "docs"]],
      ),
    );
    expect(l.flags & LAYOUT_PROVISIONAL).toBe(LAYOUT_PROVISIONAL);
    expect(l.estimatedFrom).toBe(1_792_000_000);
    expect(l.total).toBe(1000);
    expect(Array.from(l.provisional)).toEqual([1, 0, 1]);
    expect(Array.from(l.weight)).toEqual([0, 900, 0]);
    expect(l.labels.get(1)).toBe("docs");
    // The filler is no item: the point over it hits nothing of its own.
    expect([0, 1, 2].map((i) => isFiller(l, i))).toEqual([false, false, true]);
    expect(hitTest(l, 95, 50)).toBe(-1);
    expect(hitTest(l, 10, 50)).toBe(0);
  });

  it("reads a layout with nothing estimated", () => {
    const l = decodeLayout(
      encode(LAYOUT_FINAL, 0, [{ x: 0, y: 0, w: 10, h: 10, node: 1, count: 1, weight: 5, kind: KIND_FOLDER, more: 0 }], []),
    );
    expect(l.flags & LAYOUT_PROVISIONAL).toBe(0);
    expect(l.estimatedFrom).toBe(0);
    expect(Array.from(l.provisional)).toEqual([0]);
  });

  it("refuses other formats", () => {
    const buf = encode(0, 0, [], []);
    new DataView(buf).setUint16(4, 4, true);
    expect(() => decodeLayout(buf)).toThrow(/format 4/);
  });
});
