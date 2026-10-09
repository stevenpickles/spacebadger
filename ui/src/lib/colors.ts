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
