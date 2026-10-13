// Display formatting. Sizes use binary units (KiB, MiB, GiB) consistently.

const UNITS = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];

export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < UNITS.length - 1) {
    value /= 1024;
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
