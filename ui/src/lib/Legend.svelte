<script lang="ts">
  // What the treemap colors mean: a small card floating in a corner of the
  // map, so switching color modes never resizes the map. It can be folded
  // down to its heading; that choice is remembered per viewer.
  import {
    CHANGE_LEGEND,
    changeColor,
    depthColor,
    gitAccent,
    gitColor,
    GIT_LEGEND,
    typeModeFolderColor,
    otherSmallColor,
    TYPE_LEGEND,
    typeColor,
    type ColorMode,
  } from "./colors";
  import { MARK_NEW, MARK_UNCHANGED, MARK_UNKNOWN } from "./compare";
  import { GIT_METADATA } from "./layoutWire";
  import { theme } from "./theme.svelte";

  interface Props {
    /** Defaults to "type", the only mode the legend was shown for before. */
    mode?: ColorMode;
    /** In change mode: when the earlier scan was, e.g. "Oct 1, 2:03 PM". */
    since?: string;
    /** A name filter is on: tiles show matching files, but changes are
     * of all files. */
    filtered?: boolean;
    /** The map shows areas estimated from the last scan (a rescan is
     * under way). */
    estimated?: boolean;
  }

  let { mode = "type", since = "", filtered = false, estimated = false }: Props = $props();

  const dark = $derived(theme.dark);
  const listId = $props.id();
  const STORE_KEY = "legend-collapsed";

  let collapsed = $state(read());

  function read(): boolean {
    try {
      // Folded until opened once, so it doesn't cover the map by default.
      return localStorage.getItem(STORE_KEY) !== "0";
    } catch {
      return true;
    }
  }

  function toggle() {
    collapsed = !collapsed;
    try {
      localStorage.setItem(STORE_KEY, collapsed ? "1" : "0");
    } catch {
      // Not remembered; it still works for this session.
    }
  }

  const heading = $derived(
    mode === "type"
      ? "File types"
      : mode === "change"
        ? since
          ? `Change since ${since}`
          : "Change"
        : mode === "git"
          ? "Git"
          : "Colors",
  );
</script>

<section class="legend" class:wide={mode === "git"} aria-labelledby="{listId}-heading">
  <div class="head">
    <h2 id="{listId}-heading">{heading}</h2>
    <button
      type="button"
      class="icon-button"
      aria-expanded={!collapsed}
      aria-controls={listId}
      aria-label={collapsed ? "Show color key" : "Hide color key"}
      title={collapsed ? "Show color key" : "Hide color key"}
      onclick={toggle}>{collapsed ? "▴" : "▾"}</button
    >
  </div>
  {#if !collapsed}
    <ul id={listId} class:row={mode === "git"}>
      {#if mode === "type"}
        {#each TYPE_LEGEND as t (t.type)}
          <li><span class="swatch" style:background={typeColor(t.type, dark)}></span>{t.label}</li>
        {/each}
        <li><span class="swatch" style:background={typeModeFolderColor(dark)}></span>Folders</li>
        <li><span class="swatch" style:background={otherSmallColor(dark)}></span>Small items</li>
      {:else if mode === "change"}
        <li>
          <span class="ramp" aria-hidden="true">
            {#each CHANGE_LEGEND.grown as mark (mark)}
              <span class="swatch" style:background={changeColor(mark, false, dark)}></span>
            {/each}
          </span>Grown
        </li>
        <li>
          <span class="ramp" aria-hidden="true">
            {#each CHANGE_LEGEND.shrunk as mark (mark)}
              <span class="swatch" style:background={changeColor(mark, false, dark)}></span>
            {/each}
          </span>Shrunk
        </li>
        <li><span class="swatch" style:background={changeColor(MARK_NEW, false, dark)}></span>New</li>
        <li><span class="swatch" style:background={changeColor(MARK_UNCHANGED, false, dark)}></span>Unchanged</li>
        <li class="wide">
          <span class="swatch" style:background={changeColor(MARK_UNKNOWN, false, dark)}></span>Earlier size unknown
        </li>
        <li class="wide muted">Deeper colors: bigger change for its size</li>
        {#if filtered}
          <li class="wide muted note">Colors show the change in all files, not only matching ones</li>
        {/if}
      {:else if mode === "git"}
        {#each GIT_LEGEND as g (g.cls)}
          <li>
            <span class="swatch" class:hatched={g.cls === GIT_METADATA} style:background-color={gitColor(g.cls, false, dark)}
            ></span>{g.label}
          </li>
        {/each}
        <li><span class="swatch" style:background={otherSmallColor(dark)}></span>Small items</li>
        <li class="wide muted note">
          <span class="pill" aria-hidden="true" style:background={gitAccent(1, dark)}>repo</span> Badges and a top line mark repository and worktree folders
        </li>
      {:else}
        <li class="wide">
          <span class="depths" aria-hidden="true">
            {#each [1, 2, 3, 4] as depth (depth)}
              <span class="swatch" style:background={depthColor(depth, false, dark)}></span>
            {/each}
          </span>Colors show folder depth
        </li>
      {/if}
      {#if estimated}
        <li class="wide"><span class="swatch estimated" class:dark></span>Estimated from last scan</li>
      {/if}
      <li><span class="swatch incomplete"></span>Not fully scanned</li>
      <li><span class="swatch selected" class:dark></span>Selected</li>
    </ul>
  {/if}
</section>

<style>
  .legend {
    position: absolute;
    right: var(--space-2);
    bottom: var(--space-2);
    z-index: 1;
    max-width: min(240px, calc(100% - 2 * var(--space-2)));
    box-sizing: border-box;
    padding: var(--space-1) var(--space-2);
    border: 1px solid var(--line);
    border-radius: var(--radius);
    background: var(--panel);
    box-shadow: 0 2px 8px rgb(0 0 0 / 0.2);
    font-size: var(--fs-xs);
  }
  /* Git mode: one compact row of swatches, wrapping as needed. */
  .legend.wide {
    max-width: min(420px, calc(100% - 2 * var(--space-2)));
  }
  ul.row {
    display: flex;
    flex-wrap: wrap;
  }
  ul.row li.wide {
    flex-basis: 100%;
  }
  /* As the map draws estimated areas: faded toward the background, with
     dense lines the other way from Git metadata's. */
  .swatch.estimated {
    background-color: color-mix(in srgb, var(--muted) 45%, var(--bg));
    background-image: repeating-linear-gradient(
      45deg,
      transparent 0 2px,
      rgb(0 0 0 / 0.25) 2px 3px
    );
  }
  .swatch.estimated.dark {
    background-image: repeating-linear-gradient(
      45deg,
      transparent 0 2px,
      rgb(255 255 255 / 0.3) 2px 3px
    );
  }
  .swatch.hatched {
    background-image: repeating-linear-gradient(
      -45deg,
      transparent 0 2px,
      color-mix(in srgb, var(--fg) 18%, transparent) 2px 3px
    );
  }
  .pill {
    padding: 0 4px;
    border-radius: 2px;
    color: var(--bg);
    font-weight: 600;
    font-size: 9px;
    line-height: 12px;
  }
  .head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-2);
  }
  h2 {
    margin: 0;
    font-size: var(--fs-xs);
    font-weight: 600;
    color: var(--muted);
  }
  ul {
    display: grid;
    grid-template-columns: repeat(2, auto);
    gap: 2px var(--space-3);
    margin: var(--space-1) 0 2px;
    padding: 0;
    list-style: none;
  }
  li {
    display: flex;
    align-items: center;
    gap: 5px;
    white-space: nowrap;
  }
  li.note {
    white-space: normal;
  }
  li.wide {
    grid-column: 1 / -1;
  }
  .depths,
  .ramp {
    display: flex;
    gap: 1px;
  }
  .swatch {
    flex: none;
    box-sizing: border-box;
    width: 12px;
    height: 12px;
    border-radius: 2px;
    border: 1px solid var(--line);
  }
  .swatch.incomplete {
    border: 1px dashed var(--incomplete);
    background: transparent;
  }
  /* Matches the canvas: an accent frame edged in dark, over a light tint. */
  .swatch.selected {
    border: 2px solid var(--accent);
    background: color-mix(in srgb, var(--accent) 22%, transparent);
    box-shadow: 0 0 0 1px var(--map-text-dark);
  }
  .swatch.selected.dark {
    box-shadow: 0 0 0 1px var(--map-text-light);
  }
</style>
