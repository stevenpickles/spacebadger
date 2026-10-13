// What the filter's completion offers at the caret. A query is a list of
// `;`-separated terms; only the term the caret ends is completed, and its
// leading `!` is kept. See sb-core's search module for the syntax.

import { FILE_TYPES } from "./layoutWire";

/** `is:` keywords by FILE_TYPES index, as sb-core's FileType::keyword. */
const TYPE_KEYWORDS = ["other", "video", "audio", "image", "document", "archive", "diskimage", "program", "code"];

/** In the order sb-core lists them: Other last. */
export const TYPES = [1, 2, 3, 4, 5, 6, 7, 8, 0].map((type) => ({
  type,
  keyword: TYPE_KEYWORDS[type]!,
  label: FILE_TYPES[type]!,
}));

export type ContextKind = "keyword" | "type" | "folder" | "extension" | "value";

export interface Context {
  kind: ContextKind;
  /** What's typed after the keyword or prefix, e.g. "no" in "dir:no". */
  typed: string;
  /** The term's span in the query, after any `!`, to replace on accept. */
  start: number;
  end: number;
  /** Put before an accepted name: "dir:", "is:", "size:", "older:",
   * "newer:", "*.", or ".". */
  prefix: string;
}

/** The completion context for a caret at `caret`, or `null` when the caret
 * isn't at the end of a term. */
export function contextAt(query: string, caret: number): Context | null {
  const segStart = query.lastIndexOf(";", caret - 1) + 1;
  const next = query.indexOf(";", caret);
  const segEnd = next < 0 ? query.length : next;
  // Only complete at the end of a term.
  if (query.slice(caret, segEnd).trim() !== "") return null;
  let start = segStart;
  while (start < caret && query[start] === " ") start++;
  if (query[start] === "!") {
    start++;
    while (start < caret && query[start] === " ") start++;
  }
  const term = query.slice(start, caret);
  const at = (kind: ContextKind, prefix: string, typed: string): Context => ({
    kind,
    typed,
    start,
    end: caret,
    prefix,
  });
  const keyword = /^(dir|is|size|older|newer):\s*/i.exec(term);
  if (keyword) {
    const typed = term.slice(keyword[0].length);
    const word = keyword[1]!.toLowerCase();
    if (word === "dir") return at("folder", "dir:", typed);
    if (word === "is") return at("type", "is:", typed);
    return at("value", `${word}:`, typed);
  }
  if (/[*?]/.test(term.replace(/^\*\./, ""))) return null;
  if (term.startsWith("*.")) return at("extension", "*.", term.slice(2));
  if (term.startsWith(".") && term.length > 1) return at("extension", ".", term.slice(1));
  return at("keyword", "", term);
}

export interface Keyword {
  /** Inserted as typed: "dir:" or "is:video". */
  text: string;
  describe: string;
  /** A FILE_TYPES index, for a color swatch. */
  type?: number;
}

const DIR: Keyword = { text: "dir:", describe: "files inside folders with this name" };
const IS: Keyword = { text: "is:", describe: "files of a type: video, image, document…" };
/** Keywords that narrow by size or age, each with a short example. */
const RESTRICTIONS: Keyword[] = [
  { text: "size:", describe: "by size: size:>1GB, size:10MB..1GB" },
  { text: "older:", describe: "modified before: older:1y, older:2024-01-01" },
  { text: "newer:", describe: "modified since: newer:30d" },
];

/** Keywords starting with `typed`, ignoring case: "d" offers `dir:`, "is"
 * every type, "s" `size:`. Empty `typed` offers every keyword. */
export function keywords(typed: string): Keyword[] {
  const t = typed.toLowerCase();
  if (t === "") return [DIR, IS, ...RESTRICTIONS];
  const found: Keyword[] = [];
  if (DIR.text.startsWith(t)) found.push(DIR);
  if (IS.text.startsWith(t)) found.push(...types(""));
  found.push(...RESTRICTIONS.filter((k) => k.text.startsWith(t)));
  return found;
}

/** Common values after `size:`, `older:`, and `newer:`, by keyword. */
const VALUES: Record<string, [string, string][]> = {
  "size:": [
    [">100MB", "over 100 MB"],
    [">1GB", "over 1 GB"],
    [">10GB", "over 10 GB"],
    ["<1MB", "under 1 MB"],
    ["10MB..1GB", "from 10 MB to 1 GB"],
  ],
  "older:": [
    ["30d", "modified over 30 days ago"],
    ["6mo", "over 6 months ago"],
    ["1y", "over a year ago"],
    ["5y", "over 5 years ago"],
  ],
  "newer:": [
    ["1d", "modified in the last day"],
    ["7d", "in the last week"],
    ["30d", "in the last 30 days"],
    ["1y", "in the last year"],
  ],
};

/** Example values for `prefix` (e.g. "size:") starting with `typed`. */
export function values(prefix: string, typed: string): Keyword[] {
  const t = typed.toLowerCase().replace(/\s+/g, "");
  return (VALUES[prefix] ?? [])
    .filter(([value]) => value.toLowerCase().startsWith(t))
    .map(([value, describe]) => ({ text: `${prefix}${value}`, describe }));
}

/** Types whose keyword starts with `typed`, as `is:` keywords. */
export function types(typed: string): Keyword[] {
  const t = typed.toLowerCase();
  return TYPES.filter((k) => k.keyword.startsWith(t)).map((k) => ({
    text: `is:${k.keyword}`,
    describe: k.label,
    type: k.type,
  }));
}

/** Folder names and extensions that the filter would read as something
 * else can't be inserted as they are. */
export function insertable(name: string): boolean {
  return !/[;*?]/.test(name) && name.trim() === name && !name.startsWith("!");
}

/** The query with `ctx`'s term replaced by `text`, and where the caret goes. */
export function replaceTerm(query: string, ctx: Context, text: string): { query: string; caret: number } {
  const before = query.slice(0, ctx.start);
  return { query: before + text + query.slice(ctx.end), caret: before.length + text.length };
}
