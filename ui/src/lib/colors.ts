// Treemap colors. Depth mode tints each nesting level; file-type mode colors
// files by FILE_TYPES and draws folders in one muted slate; Git mode colors
// by what Git makes of the content.

import { MARK_GROWN, MARK_LEVELS, MARK_NEW, MARK_SHRUNK, MARK_UNCHANGED } from "./compare";
import {
  FILE_TYPES,
  GIT_BADGE_CLASS,
  GIT_BADGE_UNRESOLVED,
  GIT_CHECKOUT,
  GIT_METADATA,
  GIT_OTHER,
  GIT_WORKTREE,
} from "./layoutWire";

/** "change" is offered only while comparing with an earlier scan. */
export type ColorMode = "depth" | "type" | "change" | "git";

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
  // Other is a mid beige: visible against the background, and warmer than
  // the cool grey of small items.
  if (hue === null) return dark ? "hsl(36 18% 46%)" : "hsl(36 24% 70%)";
  return dark ? `hsl(${hue} 45% 42%)` : `hsl(${hue} 65% 74%)`;
}

/** Folders are containers, so a quiet slate that leaves color to files. */
export function typeModeFolderColor(dark: boolean): string {
  return dark ? "hsl(225 10% 24%)" : "hsl(225 9% 60%)";
}

export function otherSmallColor(dark: boolean): string {
  return dark ? "#4b505b" : "#c4c8cf";
}

const GROWN_HUE = 16;
const SHRUNK_HUE = 208;
const NEW_HUE = 285;

/**
 * Change mode, by change mark (see compare.ts): grown items warm and
 * shrunk ones cool, deeper the more they changed relative to their earlier
 * size; new items purple; unchanged ones a neutral beige-grey, and items
 * whose earlier size isn't known paler still. Folders take the same color,
 * muted, so files carry the contrast.
 */
export function changeColor(mark: number, folder: boolean, dark: boolean): string {
  const muted = folder ? 0.6 : 1;
  const shade = folder ? 6 : 0;
  let hue: number;
  let s: number;
  let l: number;
  if (mark >= MARK_GROWN && mark < MARK_SHRUNK + MARK_LEVELS) {
    const grown = mark < MARK_SHRUNK;
    const t = (mark - (grown ? MARK_GROWN : MARK_SHRUNK)) / (MARK_LEVELS - 1);
    hue = grown ? GROWN_HUE : SHRUNK_HUE;
    s = dark ? 35 + 30 * t : 55 + 30 * t;
    l = dark ? 28 + 18 * t : 84 - 28 * t;
  } else if (mark === MARK_NEW) {
    hue = NEW_HUE;
    s = dark ? 40 : 50;
    l = dark ? 48 : 66;
  } else {
    hue = 40;
    const known = mark === MARK_UNCHANGED;
    s = known ? 8 : 6;
    l = dark ? (known ? 34 : 25) : known ? 82 : 91;
  }
  return `hsl(${hue} ${Math.round(s * muted)}% ${l - shade}%)`;
}

/** Hue per Git class, by GIT_* value; `null` for other content. */
const GIT_HUES: (number | null)[] = [null, 205, 265, 35];

/**
 * Git mode, by Git class (see layoutWire.ts): checkout content a muted
 * blue-teal, linked-worktree content a muted violet, Git's own data a
 * low-saturation warm grey (also hatched on the map), and everything else
 * neutral grey. Folders take the same color, muted, so files carry the
 * contrast.
 */
export function gitColor(cls: number, folder: boolean, dark: boolean): string {
  const hue = GIT_HUES[cls] ?? null;
  if (hue === null) {
    if (folder) return dark ? "hsl(220 4% 25%)" : "hsl(220 4% 66%)";
    return dark ? "hsl(220 4% 34%)" : "hsl(220 5% 83%)";
  }
  const s = cls === GIT_METADATA ? (dark ? 20 : 26) : dark ? 34 : 46;
  if (folder) return dark ? `hsl(${hue} ${Math.round(s * 0.7)}% 27%)` : `hsl(${hue} ${Math.round(s * 0.7)}% 64%)`;
  return dark ? `hsl(${hue} ${s}% 40%)` : `hsl(${hue} ${s}% 78%)`;
}

/**
 * The thin accent and badge of a repository or worktree root folder, by
 * badge (see GIT_BADGES): its kind's hue, deeper than the fills so it reads
 * in every color mode; unresolved roots in an amber like "not fully
 * scanned".
 */
export function gitAccent(badge: number, dark: boolean): string {
  if (badge === GIT_BADGE_UNRESOLVED) return dark ? "hsl(40 65% 58%)" : "hsl(36 90% 33%)";
  const hue = GIT_HUES[GIT_BADGE_CLASS[badge] ?? GIT_OTHER] ?? 220;
  return dark ? `hsl(${hue} 50% 66%)` : `hsl(${hue} 55% 36%)`;
}

/** The legend's rows for Git mode. */
export const GIT_LEGEND = [
  { label: "Repository content", cls: GIT_CHECKOUT },
  { label: "Linked worktree content", cls: GIT_WORKTREE },
  { label: "Git metadata", cls: GIT_METADATA },
  { label: "Other", cls: GIT_OTHER },
];

/** The legend's rows for change mode: grown and shrunk as ramps. */
export const CHANGE_LEGEND = {
  grown: [0, 2, 4].map((level) => MARK_GROWN + level),
  shrunk: [0, 2, 4].map((level) => MARK_SHRUNK + level),
};

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
