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

/** When an earlier scan finished, e.g. "Oct 1, 2:03 PM"; the year shows
 * when it isn't this year. */
export function formatWhen(ms: number): string {
  const d = new Date(ms);
  const options: Intl.DateTimeFormatOptions = { month: "short", day: "numeric", hour: "numeric", minute: "2-digit" };
  if (d.getFullYear() !== new Date().getFullYear()) options.year = "numeric";
  return d.toLocaleString(undefined, options);
}

/** A signed size change, e.g. "+1.20 GB" or "−300 MB". */
export function formatDelta(before: number, after: number): string {
  if (after === before) return "no change";
  return `${after > before ? "+" : "−"}${formatBytes(Math.abs(after - before))}`;
}

/** A change in a sentence for the details panel, or `null` to say nothing. */
export function describeChange(
  kind: ChangeKind,
  before: number | null,
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
      return {
        text: `${formatDelta(before ?? 0, after ?? 0)} since ${since}`,
        was: before !== null ? `was ${formatBytes(before)}` : undefined,
      };
    default:
      // A file that was below the snapshot's size threshold: its earlier
      // size isn't known, so nothing is said rather than guessed.
      return null;
  }
}
