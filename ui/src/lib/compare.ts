// Comparing with an earlier scan: change marks and how changes read.

import { formatBytes } from "./format";
import type { ChangeKind } from "./protocol/ChangeKind";

// Mirrors sb_protocol::change_mark.
export const MARK_UNKNOWN = 0;
export const MARK_UNCHANGED = 1;
export const MARK_NEW = 2;
/** MARK_GROWN + level, level 0 (under 5%) to 4 (doubled or more). */
export const MARK_GROWN = 3;
/** MARK_SHRUNK + level, level 0 (under 5%) to 4 (emptied). */
export const MARK_SHRUNK = 8;
export const MARK_LEVELS = 5;

/**
 * When an earlier scan finished, e.g. "Oct 1, 2:03 PM"; the year shows
 * when it isn't this year, and seconds when asked for, to tell apart scans
 * from the same minute.
 */
export function formatWhen(ms: number, seconds = false): string {
  const d = new Date(ms);
  const options: Intl.DateTimeFormatOptions = { month: "short", day: "numeric", hour: "numeric", minute: "2-digit" };
  if (seconds) options.second = "2-digit";
  if (d.getFullYear() !== new Date().getFullYear()) options.year = "numeric";
  return d.toLocaleString(undefined, options);
}

/** Just the day, e.g. "Oct 1", for tight spaces. */
export function formatDay(ms: number): string {
  const d = new Date(ms);
  const options: Intl.DateTimeFormatOptions = { month: "short", day: "numeric" };
  if (d.getFullYear() !== new Date().getFullYear()) options.year = "numeric";
  return d.toLocaleDateString(undefined, options);
}

/** A signed size change, e.g. "+1.20 GB" or "−300 MB". */
export function formatDelta(before: number, after: number): string {
  if (after === before) return "no change";
  return `${after > before ? "+" : "−"}${formatBytes(Math.abs(after - before))}`;
}

/** What a change looks like in a list: "+1.20 GB", or "≥ +2.00 MB" for a
 * file whose earlier size is only known to be under `below`. */
export function formatChange(before: number | null, below: number | null, after: number | null): string {
  if (before === null && below !== null) return `≥ ${formatDelta(below, after ?? 0)}`;
  return formatDelta(before ?? 0, after ?? 0);
}

/** A change in a sentence for the details panel, or `null` to say nothing. */
export function describeChange(
  kind: ChangeKind,
  before: number | null,
  below: number | null,
  after: number | null,
  since: string,
): { text: string; was?: string } | null {
  switch (kind) {
    case "new":
      return { text: `New since ${since}` };
    case "removed":
      return { text: `Removed since ${since}`, was: before !== null ? `was ${formatBytes(before)}` : undefined };
    case "unchanged":
      return { text: `No change since ${since}` };
    case "grown":
    case "shrunk":
      if (before === null && below !== null) {
        // A file that was below the snapshot's size threshold: only a
        // lower bound on its growth is known.
        return {
          text: `At least ${formatDelta(below, after ?? 0)} since ${since}`,
          was: `was under ${formatBytes(below)}`,
        };
      }
      return {
        text: `${formatDelta(before ?? 0, after ?? 0)} since ${since}`,
        was: before !== null ? `was ${formatBytes(before)}` : undefined,
      };
    default:
      // The earlier size isn't known (a small file then and now, or a
      // folder that couldn't be read), so nothing is said rather than guessed.
      return null;
  }
}
