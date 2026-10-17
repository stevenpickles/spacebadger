// Decoder for the binary layout reply described in crates/sb-protocol/src/wire.rs.
import { measure } from "./metrics";

export const KIND_FILE = 0;
export const KIND_FOLDER = 1;
export const KIND_OTHER = 2;

// Mirrors sb_core::layout::rect_flags.
export const RF_HEADER = 1 << 0;
export const RF_EXPANDED = 1 << 1;
export const RF_PENDING = 1 << 2;
export const RF_INCOMPLETE = 1 << 3;
export const RF_CLOUD = 1 << 4;
export const RF_HARDLINKED = 1 << 5;
const RF_GIT_SHIFT = 6;

/** Mirrors sb_core::git::GitClass, in value order. */
export const GIT_OTHER = 0;
export const GIT_CHECKOUT = 1;
export const GIT_WORKTREE = 2;
export const GIT_METADATA = 3;

/** The Git class a rectangle's flags carry in bits 6-7. */
export const gitClassOf = (rflags: number): number => (rflags >> RF_GIT_SHIFT) & 3;

/** A folder's Git root badge (its `ftype`), as sb_protocol::wire::git_badge:
 * the word on its map label, the kind in full, and the Git class of its
 * content. Index 0 is no root. */
export const GIT_BADGES: readonly { short: string; name: string; cls: number }[] = [
  { short: "", name: "", cls: GIT_OTHER },
  { short: "repo", name: "Repository", cls: GIT_CHECKOUT },
  { short: "worktree", name: "Linked worktree", cls: GIT_WORKTREE },
  { short: "submodule", name: "Submodule", cls: GIT_CHECKOUT },
  { short: "repo", name: "Repository with a separate Git directory", cls: GIT_CHECKOUT },
  { short: "bare", name: "Bare repository", cls: GIT_METADATA },
  { short: "git", name: "Git directory", cls: GIT_METADATA },
  { short: "git?", name: "Unresolved Git metadata", cls: GIT_OTHER },
];
export const GIT_BADGE_UNRESOLVED = 7;
/** The Git class each badge's content has, by badge. */
export const GIT_BADGE_CLASS = GIT_BADGES.map((b) => b.cls);

/** Mirrors sb_core::filetype::FileType, in value order. */
export const FILE_TYPES = [
  "Other",
  "Video",
  "Audio",
  "Images",
  "Documents",
  "Archives",
  "Disk images",
  "Programs",
  "Code",
] as const;

export const LAYOUT_TRUNCATED = 1 << 0;
export const LAYOUT_FINAL = 1 << 1;
export const LAYOUT_PROVISIONAL = 1 << 2;

/** A rectangle's byte 40: part of its area is estimated from an earlier
 * scan (sb_protocol::wire::RECT_PROVISIONAL). */
const RECT_PROVISIONAL = 1 << 0;

const MAGIC = 0x314c4253; // "SBL1" read as little-endian u32
const FORMAT_VERSION = 5;
const RECT_LEN = 44;

/** Rectangles as parallel typed arrays, parent before child. */
export interface DecodedLayout {
  generation: number;
  revision: number;
  request: number;
  view: number;
  total: number;
  flags: number;
  /** When the scan the estimated areas come from finished, in seconds since
   * the Unix epoch; 0 when nothing is estimated. */
  estimatedFrom: number;
  count: number;
  x: Float32Array;
  y: Float32Array;
  w: Float32Array;
  h: Float32Array;
  node: Uint32Array;
  items: Uint32Array;
  weight: Float64Array;
  depth: Uint8Array;
  kind: Uint8Array;
  rflags: Uint8Array;
  /** File type for files (see FILE_TYPES); Git root badge for folders
   * (sb_protocol::wire::git_badge, 0 for none); 0 for other small items. */
  ftype: Uint8Array;
  /** The folder whose contents each rectangle is part of. Siblings are
   * consecutive, largest first. */
  parent: Uint32Array;
  /** 1 where part of the area stands for folders not yet listed, sized from
   * an earlier scan; the weight never counts them. */
  provisional: Uint8Array;
  labels: Map<number, string>;
}

export function decodeLayout(buffer: ArrayBuffer): DecodedLayout {
  return measure("decode layout", () => decode(buffer));
}

function decode(buffer: ArrayBuffer): DecodedLayout {
  const v = new DataView(buffer);
  if (v.getUint32(0, true) !== MAGIC) throw new Error("not a layout reply");
  const version = v.getUint16(4, true);
  if (version !== FORMAT_VERSION) throw new Error(`layout format ${version} is not supported`);
  const headerLen = v.getUint16(6, true);
  const count = v.getUint32(40, true);
  const labelCount = v.getUint32(44, true);
  const out: DecodedLayout = {
    generation: Number(v.getBigUint64(8, true)),
    revision: Number(v.getBigUint64(16, true)),
    request: v.getUint32(24, true),
    view: v.getUint32(28, true),
    total: Number(v.getBigUint64(32, true)),
    flags: v.getUint32(48, true),
    estimatedFrom: v.getUint32(52, true),
    count,
    x: new Float32Array(count),
    y: new Float32Array(count),
    w: new Float32Array(count),
    h: new Float32Array(count),
    node: new Uint32Array(count),
    items: new Uint32Array(count),
    weight: new Float64Array(count),
    depth: new Uint8Array(count),
    kind: new Uint8Array(count),
    rflags: new Uint8Array(count),
    ftype: new Uint8Array(count),
    parent: new Uint32Array(count),
    provisional: new Uint8Array(count),
    labels: new Map(),
  };
  let o = headerLen;
  for (let i = 0; i < count; i++, o += RECT_LEN) {
    out.x[i] = v.getFloat32(o, true);
    out.y[i] = v.getFloat32(o + 4, true);
    out.w[i] = v.getFloat32(o + 8, true);
    out.h[i] = v.getFloat32(o + 12, true);
    out.node[i] = v.getUint32(o + 16, true);
    out.items[i] = v.getUint32(o + 20, true);
    out.weight[i] = Number(v.getBigUint64(o + 24, true));
    out.depth[i] = v.getUint8(o + 32);
    out.kind[i] = v.getUint8(o + 33);
    out.rflags[i] = v.getUint8(o + 34);
    out.ftype[i] = v.getUint8(o + 35);
    out.parent[i] = v.getUint32(o + 36, true);
    out.provisional[i] = v.getUint8(o + 40) & RECT_PROVISIONAL ? 1 : 0;
  }
  const text = new TextDecoder();
  const bytes = new Uint8Array(buffer);
  for (let i = 0; i < labelCount; i++) {
    const index = v.getUint32(o, true);
    const len = v.getUint16(o + 4, true);
    out.labels.set(index, text.decode(bytes.subarray(o + 6, o + 6 + len)));
    o += 6 + len;
  }
  return out;
}

/** Index of the deepest rectangle containing the point, or -1. */
export function hitTest(l: DecodedLayout, px: number, py: number): number {
  for (let i = l.count - 1; i >= 0; i--) {
    const x = l.x[i]!;
    const y = l.y[i]!;
    if (px >= x && py >= y && px < x + l.w[i]! && py < y + l.h[i]!) return i;
  }
  return -1;
}
