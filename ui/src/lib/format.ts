// Display formatting.

/**
 * Sizes read KB, MB, GB, as the system's file manager counts them: Windows
 * Explorer takes 1 KB as 1,024 bytes, macOS Finder and Linux file managers
 * as 1,000. So a size here matches the one shown next to it there.
 */
export const KILO = /Windows/i.test(navigator.userAgent) ? 1024 : 1000;

const UNITS = ["B", "KB", "MB", "GB", "TB", "PB"];

export function formatBytes(bytes: number): string {
  if (bytes < KILO) return `${bytes} B`;
  let value = bytes;
  let unit = 0;
  while (value >= KILO && unit < UNITS.length - 1) {
    value /= KILO;
    unit++;
  }
  // 999.6 KB would round to "1000 KB"; it reads "1.00 MB" instead.
  if (Math.round(value) >= KILO && unit < UNITS.length - 1) {
    value /= KILO;
    unit++;
  }
  const digits = value >= 100 ? 0 : value >= 10 ? 1 : 2;
  return `${value.toFixed(digits)} ${UNITS[unit]}`;
}

/** Exact byte count for details, e.g. "1,234,567 bytes". */
export function exactBytes(bytes: number): string {
  return `${bytes.toLocaleString()} ${bytes === 1 ? "byte" : "bytes"}`;
}

export function formatCount(n: number): string {
  return n.toLocaleString();
}

export function formatElapsed(ms: number): string {
  const s = Math.floor(ms / 1000);
  if (s < 60) return `${(ms / 1000).toFixed(1)} s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}:${String(s % 60).padStart(2, "0")}`;
  return `${Math.floor(m / 60)}:${String(m % 60).padStart(2, "0")}:${String(s % 60).padStart(2, "0")}`;
}

export function plural(n: number, one: string, many = `${one}s`): string {
  return `${formatCount(n)} ${n === 1 ? one : many}`;
}

/**
 * What a set of selected items holds, e.g. "4 files and 1 folder (2 files
 * inside)". `files` counts every file, including those inside the folders.
 */
export function describeItems(items: number, folders: number, files: number): string {
  const loose = items - folders;
  const parts: string[] = [];
  if (loose > 0) parts.push(plural(loose, "file"));
  if (folders > 0) parts.push(`${plural(folders, "folder")} (${plural(files - loose, "file")} inside)`);
  return parts.join(" and ");
}

/** `part` as a share of `whole`, e.g. "12.5%"; tiny non-zero shares read "<0.1%". */
export function percent(part: number, whole: number): string {
  if (whole <= 0) return "0%";
  const p = (part / whole) * 100;
  if (p > 0 && p < 0.1) return "<0.1%";
  // Nearly all isn't all: 99.6% stays "99.6%", never rounding up to 100%.
  if (p >= 99.5 && part < whole) return `${Math.min(99.9, Math.floor(p * 10) / 10)}%`;
  return `${p >= 10 ? Math.round(p) : p.toFixed(1)}%`;
}

/** Whether the app runs on macOS, where ⌘ replaces Ctrl in key names. */
export const isMac = navigator.userAgent.includes("Mac");

/** A key combination named for this platform: "Ctrl+A", or "⌘A" on macOS. */
export function shortcut(keys: string): string {
  if (!isMac) return keys;
  const mac: Record<string, string> = { Ctrl: "⌘", Alt: "⌥", Shift: "⇧" };
  return keys
    .split("+")
    .map((k) => mac[k] ?? k)
    .join("");
}
