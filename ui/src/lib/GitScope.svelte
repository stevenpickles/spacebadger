<script lang="ts">
  // Quick filters beside the filter box: All files, Git repositories, and
  // Linked worktrees, as a compact segmented control. They follow the query
  // (none is pressed when its Git terms are something else, such as
  // git:metadata), and choosing one rewrites only its Git terms.
  import { gitScope, withGitScope } from "./gitQuery";

  interface Props {
    query: string;
    /** A quick filter was chosen: the query has changed, search now. */
    onapply: (scope: "all" | "repo" | "worktree") => void;
  }

  let { query = $bindable(), onapply }: Props = $props();

  const scope = $derived(gitScope(query));

  const choices = [
    { id: "all", text: "All", label: "All files" },
    { id: "repo", text: "Repos", label: "Git repositories" },
    { id: "worktree", text: "Worktrees", label: "Linked worktrees" },
  ] as const;

  function choose(id: "all" | "repo" | "worktree") {
    query = withGitScope(query, id);
    onapply(id);
  }
</script>

<div class="scope" role="group" aria-label="Git quick filters">
  {#each choices as c (c.id)}
    <button
      type="button"
      aria-pressed={scope === c.id}
      aria-label={c.label}
      title={scope === "custom" && c.id === "all" ? `${c.label} (the filter has other Git terms)` : c.label}
      onclick={() => choose(c.id)}>{c.text}</button
    >
  {/each}
</div>

<style>
  /* One bordered strip with the pressed segment filled, like a switch. */
  .scope {
    display: inline-flex;
    flex: none;
    border: 1px solid var(--line);
    border-radius: var(--radius);
    overflow: hidden;
  }
  button {
    height: 24px;
    padding: 0 8px;
    border: none;
    border-left: 1px solid var(--line);
    border-radius: 0;
    background: var(--bg);
    color: var(--muted);
    font-size: var(--fs-sm);
    white-space: nowrap;
  }
  button:first-child {
    border-left: none;
  }
  button[aria-pressed="true"] {
    background: var(--selected);
    color: var(--fg);
    font-weight: 600;
  }
  button:focus-visible {
    outline-offset: -2px;
  }
  @media (max-width: 900px) {
    button {
      padding: 0 5px;
    }
  }
</style>
