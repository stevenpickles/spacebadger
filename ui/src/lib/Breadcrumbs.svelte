<script lang="ts">
  import type { Crumb } from "./protocol/Crumb";

  interface Props {
    crumbs: Crumb[];
    onnavigate: (node: number) => void;
  }

  let { crumbs, onnavigate }: Props = $props();
</script>

<nav aria-label="Current folder">
  <ol>
    {#each crumbs as crumb, i (crumb.node)}
      <li>
        {#if i === crumbs.length - 1}
          <span aria-current="location">{crumb.name}</span>
        {:else}
          <button type="button" onclick={() => onnavigate(crumb.node)}>{crumb.name}</button>
          <span class="sep" aria-hidden="true">›</span>
        {/if}
      </li>
    {/each}
  </ol>
</nav>

<style>
  nav {
    min-width: 0;
    overflow: hidden;
  }
  ol {
    display: flex;
    align-items: center;
    gap: 2px;
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
  button {
    border: none;
    background: none;
    color: var(--link);
    padding: 2px 4px;
    border-radius: 3px;
    cursor: pointer;
    font: inherit;
  }
  button:hover {
    background: var(--hover);
  }
  span[aria-current] {
    padding: 2px 4px;
    font-weight: 600;
  }
  .sep {
    color: var(--muted);
  }
</style>
