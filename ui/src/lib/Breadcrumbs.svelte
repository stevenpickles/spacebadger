<script lang="ts">
  import type { Crumb } from "./protocol/Crumb";

  interface Props {
    crumbs: Crumb[];
    onnavigate: (node: number) => void;
    onup: () => void;
  }

  let { crumbs, onnavigate, onup }: Props = $props();

  /** Deep paths show the root, "…", and the last few folders until expanded. */
  const KEEP_END = 3;
  let expanded = $state(false);
  let list = $state<HTMLOListElement>();

  const collapsed = $derived(!expanded && crumbs.length > KEEP_END + 2);
  const shown = $derived(
    collapsed
      ? [{ crumb: crumbs[0]!, i: 0 }, ...crumbs.slice(-KEEP_END).map((crumb, k) => ({ crumb, i: crumbs.length - KEEP_END + k }))]
      : crumbs.map((crumb, i) => ({ crumb, i })),
  );

  // A different folder collapses again, and the current folder stays in view.
  $effect(() => {
    void crumbs;
    expanded = false;
  });
  $effect(() => {
    void shown;
    list?.scrollTo({ left: list.scrollWidth });
  });
</script>

<nav aria-label="Current folder">
  <button
    type="button"
    class="icon-button up"
    onclick={onup}
    disabled={crumbs.length < 2}
    title="Up one level (Backspace)"
    aria-label="Up one level"
  >
    ↑
  </button>
  <ol bind:this={list}>
    {#each shown as { crumb, i }, k (crumb.node)}
      <li>
        {#if i === crumbs.length - 1}
          <span aria-current="location">{crumb.name}</span>
        {:else}
          <button type="button" class="crumb" onclick={() => onnavigate(crumb.node)}>{crumb.name}</button>
          <span class="sep" aria-hidden="true">›</span>
        {/if}
      </li>
      {#if collapsed && k === 0}
        <li>
          <button
            type="button"
            class="crumb"
            aria-label="Show {crumbs.length - KEEP_END - 1} hidden folders"
            onclick={() => (expanded = true)}>…</button
          >
          <span class="sep" aria-hidden="true">›</span>
        </li>
      {/if}
    {/each}
  </ol>
</nav>

<style>
  nav {
    display: flex;
    align-items: center;
    gap: var(--space-1);
    min-width: 0;
    overflow: hidden;
  }
  .up {
    flex: none;
    font-size: 15px;
  }
  ol {
    display: flex;
    align-items: center;
    gap: 2px;
    min-width: 0;
    margin: 0;
    padding: 0;
    list-style: none;
    white-space: nowrap;
    overflow-x: auto;
    scrollbar-width: thin;
  }
  li {
    display: flex;
    align-items: center;
    gap: 2px;
  }
  button.crumb {
    border: none;
    background: none;
    color: var(--link);
    padding: 2px 4px;
    border-radius: 3px;
  }
  button.crumb:hover {
    background: var(--hover);
    text-decoration: underline;
  }
  span[aria-current] {
    padding: 2px 4px;
    font-weight: 600;
  }
  .sep {
    color: var(--muted);
  }
</style>
