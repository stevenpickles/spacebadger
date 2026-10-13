<script lang="ts" generics="T">
  // A multi-select list (role=listbox) shared by the search results and the
  // small items. Only the rows in view are rendered, and rows the parent
  // hasn't loaded yet show as placeholders. Clicks and keys select as in the
  // map (see selection.ts): Ctrl or ⌘ toggles, Shift selects a range from
  // the anchor, Ctrl+A selects everything.
  import { untrack, type Snippet } from "svelte";
  import { clickPick, focusPick, rangePick, replacePick, togglePick, toggles, type Pick } from "./selection";

  interface Props {
    /** Rows in all, loaded or not. */
    total: number;
    /** Row `index`, or `undefined` while it loads. */
    item: (index: number) => T | undefined;
    /** The node a row stands for. */
    node: (item: T) => number;
    /** Accessible name of the list. */
    label: string;
    /** Row height in pixels. */
    rowHeight: number;
    /** The row's tooltip, usually its full path. */
    title: (item: T) => string;
    selected: Set<number>;
    /** Where Shift ranges start. */
    anchor: number | null;
    /** The name column. */
    main: Snippet<[T]>;
    /** The size column. */
    size: Snippet<[T]>;
    /** An optional kind badge column before the name. */
    kind?: Snippet<[T]>;
    /** An optional button column after the size. */
    action?: Snippet<[T]>;
    /** Shown in place of the rows when there are none. */
    empty?: Snippet;
    /** The index of a loaded row, or -1. Defaults to searching the loaded rows. */
    indexOf?: (node: number) => number;
    /**
     * The nodes of rows `from` to `to`, inclusive, for Shift ranges and
     * Ctrl+A. It may load rows first; `null` gives up (asked to stop, or
     * `signal` aborted by a newer click). Defaults to the loaded rows.
     */
    range?: (from: number, to: number, signal: AbortSignal) => Promise<number[] | null>;
    /** The rendered rows, `first` up to (not including) `last`, as they change. */
    onvisible?: (first: number, last: number) => void;
    onpick: (pick: Pick) => void;
    /** Enter or double-click. */
    onopen: (item: T, index: number) => void;
    /** Right-click, the Menu key, or Shift+F10; window coordinates. */
    onmenu?: (item: T, x: number, y: number) => void;
    /** The Delete key. */
    ondelete: () => void;
  }

  let {
    total,
    item,
    node,
    label,
    rowHeight,
    title,
    selected,
    anchor,
    main,
    size,
    kind,
    action,
    empty,
    indexOf,
    range,
    onvisible,
    onpick,
    onopen,
    onmenu,
    ondelete,
  }: Props = $props();

  const uid = $props.id();

  let list: HTMLDivElement;
  let scrollTop = $state(0);
  let viewport = $state(0);
  let active = $state(0);
  let focused = $state(false);
  /** A keyboard move onto a row still loading; it's selected once loaded. */
  let pending = $state<{ index: number; extend: boolean } | null>(null);
  /** The Shift range or Ctrl+A in progress, aborted by any newer move. */
  let ranging: AbortController | null = null;

  const first = $derived(Math.max(0, Math.floor(scrollTop / rowHeight) - 5));
  const last = $derived(Math.min(total, Math.ceil((scrollTop + viewport) / rowHeight) + 5));
  // Screen readers can only follow the focus to a row that exists.
  const activeId = $derived(total > 0 && active >= first && active < last ? `${uid}-${active}` : undefined);
  const columns = $derived(
    [kind ? "4.5em" : "", "minmax(0, 1fr)", "6.5em", action ? "4.5em" : ""].filter(Boolean).join(" "),
  );

  $effect(() => {
    const f = first;
    const l = last;
    untrack(() => onvisible?.(f, l));
  });

  // Keep the active row inside the list when it shrinks.
  $effect(() => {
    if (active > 0 && active >= total) active = Math.max(0, total - 1);
  });

  // Select the row a key moved to once it has loaded.
  $effect(() => {
    if (!pending) return;
    const row = item(pending.index);
    if (row === undefined) return;
    const { index, extend } = pending;
    untrack(() => {
      pending = null;
      void pickAt(index, extend ? "extend" : "replace");
    });
  });

  $effect(() => {
    const observer = new ResizeObserver(([entry]) => {
      if (entry) viewport = entry.contentRect.height;
    });
    observer.observe(list);
    return () => observer.disconnect();
  });

  /** Back to the top with nothing active, for a new list. */
  export function reset() {
    active = 0;
    pending = null;
    ranging?.abort();
    ranging = null;
    if (list) list.scrollTop = 0;
    scrollTop = 0;
  }

  /** Moves the keyboard focus to the list, e.g. after an inline question. */
  export function focus() {
    list?.focus();
  }

  /** Scrolls row `index` into view and makes it the active row, e.g. for an
   * item selected in the map. The selection is left alone. */
  export function scrollToRow(index: number) {
    if (index < 0 || index >= total || !list) return;
    active = index;
    reveal(index);
  }

  /** Starts a new range request, giving up any earlier one. */
  function newRange(): AbortSignal {
    ranging?.abort();
    ranging = new AbortController();
    return ranging.signal;
  }

  function cancelRange() {
    ranging?.abort();
    ranging = null;
  }

  async function loadedRange(from: number, to: number): Promise<number[]> {
    const nodes: number[] = [];
    for (let i = Math.min(from, to); i <= Math.max(from, to); i++) {
      const row = item(i);
      if (row !== undefined) nodes.push(node(row));
    }
    return nodes;
  }

  function rows(from: number, to: number): Promise<number[] | null> {
    const signal = newRange();
    const nodes = range ? range(from, to, signal) : loadedRange(from, to);
    return nodes.then((n) => (signal.aborted ? null : n));
  }

  function findLoaded(target: number): number {
    for (let i = 0; i < total; i++) {
      const row = item(i);
      if (row !== undefined && node(row) === target) return i;
    }
    return -1;
  }

  /**
   * The nodes from the anchor to row `to`: `undefined` when the anchor
   * isn't in the list (treat as a plain click), `null` when given up.
   */
  async function anchorRange(to: number): Promise<number[] | null | undefined> {
    if (anchor === null) return undefined;
    const from = indexOf ? indexOf(anchor) : findLoaded(anchor);
    if (from < 0) return undefined;
    return rows(from, to);
  }

  type How = "replace" | "extend" | "focus";

  async function pickAt(index: number, how: How) {
    const row = item(index);
    if (row === undefined) return;
    const n = node(row);
    if (how === "focus") {
      cancelRange();
      onpick(focusPick(n));
      return;
    }
    if (how === "extend") {
      const nodes = await anchorRange(index);
      if (nodes === null) return;
      if (nodes !== undefined) {
        onpick(rangePick(n, nodes));
        return;
      }
    }
    cancelRange();
    onpick(replacePick(n));
  }

  function reveal(index: number) {
    const top = index * rowHeight;
    if (top < list.scrollTop) list.scrollTop = top;
    else if (top + rowHeight > list.scrollTop + viewport) list.scrollTop = top + rowHeight - viewport;
    // Render the row now, so the active descendant exists.
    scrollTop = list.scrollTop;
  }

  function moveTo(index: number, how: How) {
    if (total === 0) return;
    active = Math.max(0, Math.min(total - 1, index));
    reveal(active);
    pending = null;
    if (item(active) !== undefined) void pickAt(active, how);
    else if (how !== "focus") pending = { index: active, extend: how === "extend" };
  }

  async function selectAll() {
    if (total === 0) return;
    pending = null;
    const nodes = await rows(0, total - 1);
    if (!nodes || nodes.length === 0) return;
    const at = item(active);
    onpick(rangePick(at === undefined ? nodes[0]! : node(at), nodes));
  }

  function toggleActive() {
    const row = item(active);
    if (row === undefined) return;
    cancelRange();
    onpick(togglePick(node(row)));
  }

  /** Selects the row a menu is for, unless it's part of the selection. */
  function menuPick(row: T) {
    const n = node(row);
    cancelRange();
    onpick(selected.has(n) ? focusPick(n) : replacePick(n));
  }

  function menuAtActive() {
    const row = item(active);
    if (row === undefined || !onmenu) return;
    reveal(active);
    menuPick(row);
    const r = list.getBoundingClientRect();
    onmenu(row, r.left + 24, r.top + (active + 0.5) * rowHeight - list.scrollTop);
  }

  function onKey(e: KeyboardEvent) {
    const mod = toggles(e);
    const how: How = e.shiftKey ? "extend" : mod ? "focus" : "replace";
    const page = Math.max(1, Math.floor(viewport / rowHeight) - 1);
    switch (e.key) {
      case "ArrowDown":
        moveTo(active + 1, how);
        break;
      case "ArrowUp":
        moveTo(active - 1, how);
        break;
      case "PageDown":
        moveTo(active + page, how);
        break;
      case "PageUp":
        moveTo(active - page, how);
        break;
      case "Home":
        moveTo(0, how);
        break;
      case "End":
        moveTo(total - 1, how);
        break;
      case " ":
        if (mod) toggleActive();
        else void pickAt(active, e.shiftKey ? "extend" : "replace");
        break;
      case "Enter": {
        const row = item(active);
        if (row !== undefined) onopen(row, active);
        break;
      }
      case "Delete":
        ondelete();
        break;
      case "a":
      case "A":
        if (!mod || e.shiftKey || e.altKey) return;
        void selectAll();
        break;
      case "ContextMenu":
        menuAtActive();
        break;
      case "F10":
        if (!e.shiftKey) return;
        menuAtActive();
        break;
      default:
        return;
    }
    e.preventDefault();
  }

  async function onRowClick(e: MouseEvent, index: number) {
    active = index;
    pending = null;
    list.focus();
    const row = item(index);
    if (row === undefined) return;
    const n = node(row);
    if (e.shiftKey) {
      const nodes = await anchorRange(index);
      if (nodes === null) return;
      if (nodes !== undefined) {
        onpick(rangePick(n, nodes, toggles(e)));
        return;
      }
    }
    cancelRange();
    onpick(clickPick({ node: n, other: false }, e, null));
  }

  function onPlaceholderClick(index: number) {
    active = index;
    pending = null;
    list.focus();
  }

  function onRowMenu(e: MouseEvent, index: number) {
    e.preventDefault();
    const row = item(index);
    if (row === undefined || !onmenu) return;
    active = index;
    pending = null;
    list.focus();
    menuPick(row);
    onmenu(row, e.clientX, e.clientY);
  }
</script>

{#if total === 0 && empty}
  <div class="empty">{@render empty()}</div>
{/if}
<div
  class="list"
  bind:this={list}
  role="listbox"
  tabindex="0"
  aria-label={label}
  aria-multiselectable="true"
  aria-activedescendant={activeId}
  style:--columns={columns}
  style:--row-height="{rowHeight}px"
  onscroll={() => (scrollTop = list.scrollTop)}
  onkeydown={onKey}
  onfocus={() => (focused = true)}
  onblur={() => (focused = false)}
>
  <div class="spacer" style:height="{total * rowHeight}px">
    {#each { length: last - first } as _, k (first + k)}
      {@const i = first + k}
      {@const row = item(i)}
      {#if row !== undefined}
        {@const picked = selected.has(node(row))}
        <!-- The list holds the focus and points at rows with aria-activedescendant. -->
        <!-- svelte-ignore a11y_click_events_have_key_events, a11y_interactive_supports_focus -->
        <div
          id="{uid}-{i}"
          class="row"
          class:active={i === active && focused}
          class:selected={picked}
          role="option"
          aria-selected={picked}
          aria-setsize={total}
          aria-posinset={i + 1}
          title={title(row)}
          style:top="{i * rowHeight}px"
          onclick={(e) => onRowClick(e, i)}
          ondblclick={() => onopen(row, i)}
          oncontextmenu={(e) => onRowMenu(e, i)}
        >
          {#if kind}<span class="kind">{@render kind(row)}</span>{/if}
          <span class="main">{@render main(row)}</span>
          <span class="size row-size">{@render size(row)}</span>
          {#if action}<span class="action">{@render action(row)}</span>{/if}
        </div>
      {:else}
        <!-- svelte-ignore a11y_click_events_have_key_events, a11y_interactive_supports_focus -->
        <div
          id="{uid}-{i}"
          class="row placeholder"
          class:active={i === active && focused}
          role="option"
          aria-selected="false"
          aria-busy="true"
          aria-label="Loading"
          aria-setsize={total}
          aria-posinset={i + 1}
          style:top="{i * rowHeight}px"
          onclick={() => onPlaceholderClick(i)}
        >
          {#if kind}<span class="bar short"></span>{/if}
          <span class="bar"></span>
          <span class="bar short"></span>
          {#if action}<span></span>{/if}
        </div>
      {/if}
    {/each}
  </div>
</div>

<style>
  .list {
    position: relative;
    flex: 1;
    min-height: 0;
    overflow-y: auto;
  }
  .list:focus {
    outline: none;
  }
  .spacer {
    position: relative;
  }
  .row {
    position: absolute;
    left: 0;
    right: 0;
    height: var(--row-height);
    box-sizing: border-box;
    display: grid;
    grid-template-columns: var(--columns);
    align-items: center;
    column-gap: var(--space-2);
    padding: 0 var(--space-3);
    border-bottom: 1px solid var(--line);
    cursor: default;
  }
  .row:hover {
    background: var(--hover);
  }
  /* The accent bar marks selected rows; the outline marks the active one. */
  .row.selected {
    background: var(--selected);
    box-shadow: inset 3px 0 0 var(--accent);
  }
  .row.active {
    outline: 2px solid var(--accent);
    outline-offset: -2px;
  }
  .kind {
    font-size: var(--fs-xs);
    color: var(--muted);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .main {
    min-width: 0;
  }
  .action {
    display: flex;
    justify-content: flex-end;
  }
  .bar {
    height: 0.7em;
    border-radius: var(--radius);
    background: var(--line);
    opacity: 0.6;
  }
  .bar.short {
    width: 60%;
    justify-self: end;
  }
  .empty {
    flex: none;
    padding: var(--space-3);
  }
</style>
