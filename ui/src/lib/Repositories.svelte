<script lang="ts" module>
  import type { GitHead } from "./protocol/GitHead";
  import type { GitRootKind } from "./protocol/GitRootKind";
  import type { GitRootRow } from "./protocol/GitRootRow";

  /** Each kind's badge, as the map draws it (see GIT_BADGES). */
  export const KIND_BADGE: Record<GitRootKind, number> = {
    repository: 1,
    linkedWorktree: 2,
    submodule: 3,
    separateGitDir: 4,
    bare: 5,
    gitDir: 6,
    unresolved: 7,
  };

  /** What HEAD names, as a phrase: "main", "detached at 1a2b3c4d5e6f". */
  export function describeHead(head: GitHead): string {
    if (head.kind === "branch") return head.name;
    if (head.kind === "ref") return head.name;
    return `detached at ${head.commit}`;
  }

  /** What a root's link to another repository is, by kind. */
  export function repositoryLabel(row: GitRootRow): string {
    if (row.kind === "linkedWorktree") return "Main repository";
    if (row.kind === "submodule") return "Superproject";
    return "Git directory";
  }
</script>

<script lang="ts">
  // The Git repositories, linked worktrees, and unresolved candidates in the
  // scan, largest first. Selecting one selects its folder in the map and the
  // details; Enter or double-click opens it (the repository above the scan
  // root stands for the scan root, node 0). Roots are few, so they come in
  // one request (the backend caps it at 500), asked again as the scan and
  // filter change.
  import { announce } from "./announce.svelte";
  import { errorText, gitRoots } from "./api";
  import { gitAccent } from "./colors";
  import { exactBytes, formatBytes, plural } from "./format";
  import ItemList from "./ItemList.svelte";
  import { GIT_BADGES } from "./layoutWire";
  import type { GitRootsPage } from "./protocol/GitRootsPage";
  import type { Metric } from "./protocol/Metric";
  import { latest, queryKey } from "./request";
  import type { Pick } from "./selection";
  import { theme } from "./theme.svelte";

  interface Props {
    generation: number;
    /** The current search, for matching sizes; `null` unfiltered. */
    search: number | null;
    /** The size measure rows are ordered by; the interface uses file size. */
    metric?: Metric;
    /** Scan revision; a change asks again. */
    revision: number;
    /** A change asks again too (the filter caught up). */
    refresh?: number;
    selected: Set<number>;
    /** Where Shift ranges start. */
    anchor: number | null;
    onpick: (pick: Pick) => void;
    /** The Delete key. */
    ondelete: () => void;
    /** Open a root's folder in the map. */
    onopen: (node: number) => void;
  }

  let {
    generation,
    search,
    metric = "logical",
    revision,
    refresh = 0,
    selected,
    anchor,
    onpick,
    ondelete,
    onopen,
  }: Props = $props();

  const ROW = 54;
  const LIMIT = 500;

  let page = $state.raw<GitRootsPage | null>(null);
  let error = $state<string | null>(null);
  /** Bumped by Retry. */
  let attempt = $state(0);
  const roots = latest();

  $effect(() => {
    const want = { generation, metric, search, offset: 0, limit: LIMIT };
    const rev = revision;
    void [refresh, attempt];
    roots.run(
      queryKey(want.generation, want.metric, want.search),
      rev,
      (lane) => gitRoots(want, lane),
      (p) => {
        if (p.generation !== generation) return;
        page = p;
        error = null;
      },
      (e) => (error = errorText(e)),
    );
  });

  // A different scan or filter starts over, so a list of the old one isn't
  // shown as if it matched.
  let shown = "";
  $effect(() => {
    const key = `${generation}/${search}/${metric}`;
    if (key === shown) return;
    shown = key;
    page = null;
    error = null;
  });

  const rows = $derived(page?.rows ?? []);
  const largest = $derived(rows.reduce((m, r) => Math.max(m, r.size), 0));
  const filtered = $derived(search !== null);

  // Said once the scan has found them all.
  let said = "";
  $effect(() => {
    const p = page;
    if (!p || p.discovering) return;
    const key = `${generation}/${p.total}`;
    if (key === said) return;
    said = key;
    announce(plural(p.total, "Git repository or worktree", "Git repositories and worktrees"));
  });

  function indexOf(node: number): number {
    return rows.findIndex((r) => r.node === node);
  }

  /** The second line: how the root relates to others, or what's wrong. */
  function facts(r: GitRootRow): string {
    if (r.problem) return r.problem;
    const parts: string[] = [];
    if (r.head) parts.push(describeHead(r.head));
    if (r.outside) parts.push("above the scan root");
    // Two rows can stand for node 0: the repository above the scan root and
    // the scan root's own; `parentOutside` says which one is meant.
    const outside = (r as GitRootRow & { parentOutside?: boolean }).parentOutside ?? false;
    const parent = r.parent === null ? undefined : rows.find((p) => p.node === r.parent && p.outside === outside);
    if (parent) parts.push(`inside ${parent.name}`);
    if (r.kind === "linkedWorktree" && r.repository) parts.push(`main repo: ${r.repository.path}`);
    if (r.nested > 0) parts.push(`includes ${plural(r.nested, "nested root")}`);
    if (r.partial) parts.push("partly scanned");
    return parts.join(" · ");
  }

  function counts(worktrees: number, unresolved: number): string {
    const parts = [];
    if (worktrees > 0) parts.push(plural(worktrees, "linked worktree"));
    if (unresolved > 0) parts.push(`${unresolved.toLocaleString()} unresolved`);
    parts.push("largest first");
    return `· ${parts.join(" · ")}`;
  }

  function rowLabel(r: GitRootRow): string {
    const kind = GIT_BADGES[KIND_BADGE[r.kind]]!.name;
    const match = r.matching !== null ? `, ${formatBytes(r.matching)} matching` : "";
    return `${kind}: ${r.path}, ${formatBytes(r.size)} scanned${match}`;
  }

  function sizeTitle(r: GitRootRow): string {
    const lines = [`Scanned: ${exactBytes(r.size)}`];
    if (r.nested > 0) lines.push(`Of that, in nested repositories: ${formatBytes(r.size - r.own)}`);
    if (r.metadata > 0) lines.push(`Git metadata: ${formatBytes(r.metadata)}`);
    if (r.matching !== null) lines.push(`Matching the filter: ${exactBytes(r.matching)}`);
    return lines.join("\n");
  }
</script>

<section class="repos" aria-label="Git repositories">
  <header>
    {#if page}
      {@const t = page.totals}
      <div class="panel-head">
        <h3>{plural(t.roots, "repository", "repositories")}</h3>
        <span class="muted small">{counts(t.worktrees, t.unresolved)}</span>
      </div>
      <p class="small">
        <span title="Everything in any repository or worktree, nested ones counted once"
          >{formatBytes(t.union)} scanned, counted once</span
        >{#if t.metadata > 0}{" · "}<span title="Git's own data, where it was found">Git data {formatBytes(t.metadata)}</span
          >{/if}{#if filtered && t.matching !== null}{" · "}<strong>{formatBytes(t.matching)} match</strong>{/if}
      </p>
      {#if page.discovering}
        <p class="small note">Still scanning: more repositories may be found.</p>
      {/if}
      {#if page.total > rows.length}
        <p class="small muted">The largest {rows.length.toLocaleString()} of {page.total.toLocaleString()}.</p>
      {/if}
    {:else if !error}
      <div class="panel-head"><h3>Git repositories</h3></div>
      <p class="small muted">Loading…</p>
    {/if}
  </header>
  {#if page}
    <ItemList
      {indexOf}
      total={rows.length}
      item={(i) => rows[i]}
      node={(r) => r.node}
      label="Git repositories and worktrees, largest first"
      rowHeight={ROW}
      title={rowLabel}
      {selected}
      {anchor}
      {onpick}
      onopen={(r) => onopen(r.node)}
      {ondelete}
    >
      {#snippet main(r)}
        {@const more = facts(r)}
        {@const badge = KIND_BADGE[r.kind]}
        <span class="first">
          <span class="badge" style:--kind={gitAccent(badge, theme.dark)} title={GIT_BADGES[badge]!.name}
            >{GIT_BADGES[badge]!.short}</span
          >
          <span class="row-name">{r.name}</span>
        </span>
        <span class="line path"><bdi>{r.path}</bdi></span>
        {#if more}<span class="line" class:problem={r.problem !== null}>{more}</span>{/if}
      {/snippet}
      {#snippet size(r)}
        <span class="sizes" title={sizeTitle(r)}>
          <span>{formatBytes(r.size)}</span>
          {#if r.matching !== null}<span class="match">{formatBytes(r.matching)} match</span>{/if}
          <span class="share" aria-hidden="true"
            ><span style:width="{largest > 0 ? Math.max(2, (r.size / largest) * 100) : 0}%"></span></span
          >
        </span>
      {/snippet}
      {#snippet empty()}
        <p class="muted">
          {page?.discovering ? "No Git repositories found yet." : "No Git repositories or worktrees in this scan."}
        </p>
      {/snippet}
    </ItemList>
  {/if}
  {#if error}
    <p class="error small failed" role="alert">
      Couldn't list repositories: {error}
      <button
        type="button"
        class="link"
        onclick={() => {
          error = null;
          attempt++;
        }}>Retry</button
      >
    </p>
  {/if}
</section>

<style>
  .repos {
    display: flex;
    flex-direction: column;
    flex: 1;
    min-height: 0;
  }
  header {
    flex: none;
    padding-bottom: var(--space-2);
    border-bottom: 1px solid var(--line);
  }
  /* In a narrow panel the counts wrap under the heading, not into it. */
  header .panel-head {
    flex-wrap: wrap;
    row-gap: 0;
  }
  header h3 {
    white-space: nowrap;
  }
  header p {
    margin: 0;
    padding: 0 var(--space-3);
  }
  .note {
    color: var(--accent);
  }
  .first {
    display: flex;
    align-items: center;
    gap: 6px;
    min-width: 0;
  }
  .line {
    display: block;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    font-size: var(--fs-xs);
    color: var(--muted);
  }
  /* Paths lose their start rather than the folder names at the end. */
  .path {
    direction: rtl;
    text-align: left;
  }
  /* Stronger than the muted lines, without a color that might not contrast. */
  .problem {
    color: var(--fg);
  }
  /* A text pill in the kind's hue: the word carries the meaning, the color
     only echoes the map's accent. */
  .badge {
    flex: none;
    padding: 0 5px;
    border: 1px solid var(--kind);
    border-left-width: 3px;
    border-radius: var(--radius);
    color: var(--fg);
    font-size: var(--fs-xs);
    line-height: 14px;
  }
  .sizes {
    display: flex;
    flex-direction: column;
    align-items: flex-end;
    gap: 1px;
  }
  .match {
    font-size: var(--fs-xs);
    font-weight: 600;
  }
  /* Each root's size against the largest. */
  .share {
    display: block;
    width: 100%;
    height: 3px;
    margin-top: 2px;
    border-radius: 2px;
    background: var(--line);
  }
  .share span {
    display: block;
    height: 100%;
    border-radius: 2px;
    background: var(--muted);
  }
  .failed {
    flex: none;
    margin: 0;
    padding: var(--space-2) var(--space-3);
  }
</style>
