// Treemap colors. Depth mode tints each nesting level; file-type mode colors
// files by FILE_TYPES and draws folders in one indigo, a hue no type uses.

import { FILE_TYPES } from "./layoutWire";

export type ColorMode = "depth" | "type";

const DEPTH_HUES = [212, 28, 145, 270, 188, 340, 50, 105];

export function depthColor(depth: number, folder: boolean, dark: boolean): string {
  const hue = DEPTH_HUES[(depth - 1) % DEPTH_HUES.length];
  if (folder) return dark ? `hsl(${hue} 28% 30%)` : `hsl(${hue} 30% 62%)`;
  return dark ? `hsl(${hue} 38% 42%)` : `hsl(${hue} 45% 80%)`;
}

/** Hue per file type, by FILE_TYPES index; `null` for Other. */
const TYPE_HUES: (number | null)[] = [null, 0, 280, 130, 212, 32, 330, 52, 180];

export function typeColor(type: number, dark: boolean): string {
  const hue = TYPE_HUES[type] ?? null;
  // Other is a warm off-white so it isn't mistaken for small items.
  if (hue === null) return dark ? "hsl(40 10% 44%)" : "hsl(40 25% 90%)";
  return dark ? `hsl(${hue} 45% 42%)` : `hsl(${hue} 65% 74%)`;
}

export function typeModeFolderColor(dark: boolean): string {
  return dark ? "hsl(245 25% 33%)" : "hsl(245 32% 68%)";
}

export function otherSmallColor(dark: boolean): string {
  return dark ? "#4b505b" : "#c4c8cf";
}

export const TYPE_LEGEND = FILE_TYPES.map((label, type) => ({ label, type })).filter(
  (t) => t.type !== 0,
).concat({ label: FILE_TYPES[0], type: 0 });

const darkFills = new Map<string, boolean>();

/**
 * Whether light text reads better than dark text on `color`, one of the
 * "hsl(h s% l%)" or "#rrggbb" fills above. Compares WCAG relative luminance
 * with the point where black and white text have equal contrast.
 */
export function isDarkFill(color: string): boolean {
  let dark = darkFills.get(color);
  if (dark === undefined) {
    const rgb = parseColor(color);
    dark = rgb ? luminance(rgb) < 0.179 : false;
    darkFills.set(color, dark);
  }
  return dark;
}

type Rgb = [number, number, number];

/** Channels in 0..1, or `null` for a format the fills above don't use. */
function parseColor(color: string): Rgb | null {
  const hex = /^#([0-9a-f]{2})([0-9a-f]{2})([0-9a-f]{2})$/i.exec(color);
  if (hex) return [hex[1]!, hex[2]!, hex[3]!].map((c) => parseInt(c, 16) / 255) as Rgb;
  const hsl = /^hsl\(([\d.]+)\s+([\d.]+)%\s+([\d.]+)%\)$/.exec(color);
  if (!hsl) return null;
  const h = Number(hsl[1]);
  const s = Number(hsl[2]) / 100;
  const l = Number(hsl[3]) / 100;
  const a = s * Math.min(l, 1 - l);
  const f = (n: number) => {
    const k = (n + h / 30) % 12;
    return l - a * Math.max(-1, Math.min(k - 3, 9 - k, 1));
  };
  return [f(0), f(8), f(4)];
}

function luminance([r, g, b]: Rgb): number {
  const lin = (c: number) => (c <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4);
  return 0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b);
}
