// The Git quick filters' view of a query: which of All files, Git
// repositories, and Linked worktrees it amounts to, and the query with one of
// them chosen. Pure, so it can be checked on its own. A query is a list of
// `;`-separated terms (see completion.ts); a Git term is `git:value`, maybe
// after a `!`, with the values sb-core's search accepts.

export type GitValue = "repo" | "worktree" | "metadata";
export type GitScope = "all" | "repo" | "worktree" | "custom";

/** Every spelling sb-core accepts for each value; unique prefixes too. */
const SPELLINGS: [GitValue, string[]][] = [
  ["repo", ["repo", "repository", "repositories"]],
  ["worktree", ["worktree", "worktrees"]],
  ["metadata", ["metadata", "meta"]],
];

export interface GitTerm {
  negated: boolean;
  /** `null` for a value the filter won't take; `undefined` while it's empty
   * (still being typed, which the filter ignores). */
  value: GitValue | null | undefined;
}

/** The Git term a `;`-separated segment is, or `null` for any other term. */
export function gitTerm(segment: string): GitTerm | null {
  const m = /^\s*(!?)\s*git:\s*(.*?)\s*$/i.exec(segment);
  if (!m) return null;
  const typed = m[2]!.toLowerCase();
  if (typed === "") return { negated: m[1] === "!", value: undefined };
  const found = SPELLINGS.filter(([, words]) => words.some((w) => w.startsWith(typed)));
  return { negated: m[1] === "!", value: found.length === 1 ? found[0]![0] : null };
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
 * that's its one positive Git term, beside any exclusions such as
 * `!git:metadata`, which narrow it like other terms do; else "custom".
 */
export function gitScope(query: string): GitScope {
  const terms = segments(query)
    .map(gitTerm)
    .filter((t): t is GitTerm => t !== null && t.value !== undefined);
  if (terms.length === 0) return "all";
  const positive = terms.filter((t) => !t.negated);
  if (terms.some((t) => t.value === null) || positive.length !== 1) return "custom";
  const only = positive[0]!.value;
  if (only === "metadata") return "custom";
  // `git:repo; !git:repo` matches nothing: that's not "Git repositories".
  if (terms.some((t) => t.negated && t.value === only)) return "custom";
  return only as "repo" | "worktree";
}

/**
 * The query with a quick filter chosen. Other terms stay as typed, in order,
 * joined with "; ". "all" drops every Git term. "repo" and "worktree" replace
 * the positive Git terms with theirs, at the end, and keep exclusions other
 * than of the value chosen.
 */
export function withGitScope(query: string, scope: "all" | "repo" | "worktree"): string {
  const kept = segments(query).filter((s) => {
    const t = gitTerm(s);
    if (t === null) return true;
    return scope !== "all" && t.negated && t.value !== scope;
  });
  if (scope !== "all") kept.push(`git:${scope}`);
  return kept.join("; ");
}
