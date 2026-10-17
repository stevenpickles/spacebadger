// The Git quick filters' view of a query: which of All files, Git
// repositories, and Linked worktrees it amounts to, and the query with one of
// them chosen. Pure, so it can be checked on its own. A query is a list of
// `;`-separated terms (see completion.ts); a Git term is `git:value`, maybe
// after a `!`, read as sb-core's search reads it.

export type GitValue = "repo" | "worktree" | "metadata";
export type GitScope = "all" | "repo" | "worktree" | "custom";

/** sb-core's GitSet::from_keyword: the first keyword the typed value
 * starts, ignoring case, names the kind. */
const KEYWORDS: [string, GitValue][] = [
  ["repository", "repo"],
  ["repositories", "repo"],
  ["worktree", "worktree"],
  ["worktrees", "worktree"],
  ["metadata", "metadata"],
];

export interface GitTerm {
  negated: boolean;
  /** `null` for a value the filter rejects; `undefined` while it's empty
   * (still being typed, which the filter ignores). */
  value: GitValue | null | undefined;
}

/** The Git term a `;`-separated segment is, or `null` for any other term.
 * As in sb-core: trimmed, a `!` and the spaces after it, then `git:` in
 * any case, then the value, trimmed. */
export function gitTerm(segment: string): GitTerm | null {
  const m = /^(!?)\s*git:(.*)$/i.exec(segment.trim());
  if (!m) return null;
  const typed = m[2]!.trim();
  const negated = m[1] === "!";
  if (typed === "") return { negated, value: undefined };
  const lower = typed.replace(/[A-Z]/g, (c) => c.toLowerCase());
  const found = KEYWORDS.find(([keyword]) => keyword.startsWith(lower));
  return { negated, value: found ? found[1] : null };
}

/** Whether excluding `excluded` leaves nothing of `value`: `!git:repo`
 * leaves out worktrees and metadata too, as everything Git holds. */
function covers(excluded: GitValue, value: GitValue): boolean {
  return excluded === value || excluded === "repo";
}

/** The non-empty terms, trimmed, in order. */
function segments(query: string): string[] {
  return query
    .split(";")
    .map((s) => s.trim())
    .filter((s) => s !== "");
}

/** Whether the query has a Git term, even one still being typed. */
export function hasGitTerms(query: string): boolean {
  return segments(query).some((s) => gitTerm(s) !== null);
}

/**
 * Which quick filter the query amounts to: "all" without Git terms (empty
 * ones don't count, as the filter ignores them); "repo" or "worktree" when
 * that's its one positive Git term, beside exclusions that narrow it
 * without leaving it out entirely (`git:repo; !git:metadata`); else
 * "custom", including any term the filter rejects.
 */
export function gitScope(query: string): GitScope {
  const terms = segments(query)
    .map(gitTerm)
    .filter((t): t is GitTerm => t !== null && t.value !== undefined);
  if (terms.length === 0) return "all";
  if (terms.some((t) => t.value === null)) return "custom";
  const positive = terms.filter((t) => !t.negated);
  if (positive.length !== 1) return "custom";
  const only = positive[0]!.value as GitValue;
  if (only === "metadata") return "custom";
  if (terms.some((t) => t.negated && covers(t.value as GitValue, only))) return "custom";
  return only;
}

/**
 * The query with a quick filter chosen. Other terms stay as typed, in order,
 * joined with "; ", and so do Git terms the filter rejects, so their error
 * still shows. "all" drops every other Git term. "repo" and "worktree"
 * replace the positive Git terms with theirs, at the end, and keep the
 * exclusions that don't leave it out entirely.
 */
export function withGitScope(query: string, scope: "all" | "repo" | "worktree"): string {
  const kept = segments(query).filter((s) => {
    const t = gitTerm(s);
    if (t === null || t.value === null) return true;
    if (t.value === undefined || scope === "all") return false;
    return t.negated && !covers(t.value, scope);
  });
  if (scope !== "all") kept.push(`git:${scope}`);
  return kept.join("; ");
}
