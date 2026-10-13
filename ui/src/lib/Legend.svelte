<script lang="ts">
  import { typeModeFolderColor, otherSmallColor, TYPE_LEGEND, typeColor } from "./colors";
  import { theme } from "./theme.svelte";

  const dark = $derived(theme.dark);
</script>

<ul class="legend" aria-label="File type colors">
  {#each TYPE_LEGEND as t (t.type)}
    <li><span class="swatch" style:background={typeColor(t.type, dark)}></span>{t.label}</li>
  {/each}
  <li><span class="swatch" style:background={typeModeFolderColor(dark)}></span>Folders</li>
  <li><span class="swatch" style:background={otherSmallColor(dark)}></span>Small items</li>
</ul>

<style>
  .legend {
    display: flex;
    flex-wrap: wrap;
    gap: 2px 14px;
    margin: 0;
    padding: 4px 8px;
    list-style: none;
    border-top: 1px solid var(--line);
    background: var(--panel);
    font-size: 12px;
  }
  li {
    display: flex;
    align-items: center;
    gap: 5px;
  }
  .swatch {
    width: 12px;
    height: 12px;
    border-radius: 2px;
    border: 1px solid var(--line);
  }
</style>
