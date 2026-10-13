<script lang="ts">
  // The filter box with completion (the ARIA combobox pattern). The term the
  // caret ends gets suggestions: keywords (`dir:`, `is:`), file types after
  // `is:`, and folder names after `dir:` or extensions after `*.` from the
  // scan. Arrow keys move through them, Enter or Tab accepts, Escape closes
  // the list and, once closed, clears the box. ArrowDown or Ctrl+Space opens
  // the list on an empty term.
  import { tick } from "svelte";
  import { filterSuggest } from "./api";
  import { typeColor } from "./colors";
  import { contextAt, insertable, keywords, replaceTerm, types, type Context } from "./completion";
  import { formatBytes, plural } from "./format";
  import { theme } from "./theme.svelte";

  interface Props {
    value: string;
    /** A search is running. */
    searching: boolean;
    /** The scan to suggest names from; none before a scan. */
    generation: number | null;
    /** Typing: search after a pause. */
    oninput: () => void;
    /** Search now: Enter, or a finished suggestion. */
    onapply: () => void;
    /** Escape with the list closed. */
    onclear: () => void;
  }

  let { value = $bindable(), searching, generation, oninput, onapply, onclear }: Props = $props();

  interface Item {
    /** The term as it will read. */
    insert: string;
    detail: string;
    /** A FILE_TYPES index, for a swatch. */
    type?: number;
    /** Accepting it ends the term, so the search runs; keywords don't. */
    done: boolean;
  }

  const LIMIT = 8;

  let input: HTMLInputElement;
  let caret = $state(0);
  let focused = $state(false);
  /** Escape closed the list; typing opens it again. */
  let dismissed = $state(false);
  /** ArrowDown or Ctrl+Space asked for suggestions on an empty term. */
  let forced = $state(false);
  let active = $state(-1);
  /** Suggestions from the scan, for the context they were asked for. */
  let remote = $state.raw<{ key: string; items: Item[] } | null>(null);
  let seq = 0;

  const ctx = $derived(contextAt(value, caret));
  const key = $derived(ctx ? `${generation}/${ctx.kind}/${ctx.typed.toLowerCase()}` : "");

  const items = $derived.by((): Item[] => {
    if (!ctx) return [];
    switch (ctx.kind) {
      case "keyword":
        if (ctx.typed === "" && !forced) return [];
        return keywords(ctx.typed).map((k) => ({
          insert: k.text,
          detail: k.describe,
          type: k.type,
          done: k.type !== undefined,
        }));
      case "type":
        return types(ctx.typed).map((k) => ({ insert: k.text, detail: k.describe, type: k.type, done: true }));
      default:
        return remote?.key === key ? remote.items : [];
    }
  });

  const open = $derived(focused && !dismissed && items.length > 0);

  // Ask the scan for folder names or extensions, a moment after typing.
  $effect(() => {
    const c = ctx;
    const k = key;
    const gen = generation;
    if (!c || gen === null || (c.kind !== "folder" && c.kind !== "extension")) return;
    const mine = ++seq;
    const timer = setTimeout(() => {
      filterSuggest({ generation: gen, kind: c.kind === "folder" ? "folder" : "extension", typed: c.typed, limit: LIMIT })
        .then((found) => {
          if (mine !== seq) return;
          const what = c.kind === "folder" ? "folder" : "file";
          remote = {
            key: k,
            items: found
              .filter((s) => insertable(s.name))
              .map((s) => ({
                insert: `${c.prefix}${s.name}`,
                detail: `${plural(s.count, what)} · ${formatBytes(s.logical)}`,
                done: true,
              })),
          };
        })
        .catch(() => {
          // Completion is a convenience; the filter works without it.
          if (mine === seq) remote = { key: k, items: [] };
        });
    }, 80);
    return () => clearTimeout(timer);
  });

  // A new list starts with nothing chosen, so Enter keeps meaning "search".
  $effect(() => {
    void items;
    active = -1;
  });

  function track() {
    caret = input.selectionStart ?? value.length;
  }

  function onInput() {
    dismissed = false;
    forced = false;
    track();
    oninput();
  }

  async function accept(item: Item, c: Context) {
    const next = replaceTerm(value, c, item.insert);
    value = next.query;
    await tick();
    input.setSelectionRange(next.caret, next.caret);
    caret = next.caret;
    forced = false;
    if (item.done) {
      dismissed = true;
      onapply();
    } else {
      // A keyword: its suggestions follow.
      dismissed = false;
      oninput();
    }
  }

  function onKey(e: KeyboardEvent) {
    const c = ctx;
    switch (e.key) {
      case "ArrowDown":
        if (!open) {
          dismissed = false;
          forced = true;
        } else {
          active = (active + 1) % items.length;
        }
        break;
      case "ArrowUp":
        if (!open) return;
        active = active <= 0 ? items.length - 1 : active - 1;
        break;
      case "Enter":
        if (open && active >= 0 && c) void accept(items[active]!, c);
        else onapply();
        break;
      case "Tab":
        // Tab completes when there's something to complete; otherwise it
        // moves the focus as usual.
        if (!open || !c || e.shiftKey) return;
        void accept(items[Math.max(0, active)]!, c);
        break;
      case "Escape":
        if (open) dismissed = true;
        else onclear();
        break;
      case " ":
        if (!e.ctrlKey) return;
        dismissed = false;
        forced = true;
        break;
      default:
        return;
    }
    e.preventDefault();
  }
</script>

<div class="box">
  <input
    bind:this={input}
    bind:value
    type="search"
    placeholder="Filter files: *.jpg; is:video; dir:Downloads"
    aria-label="Filter"
    aria-describedby="filter-hint"
    role="combobox"
    aria-autocomplete="list"
    aria-expanded={open}
    aria-controls="filter-suggestions"
    aria-activedescendant={open && active >= 0 ? `filter-suggestion-${active}` : undefined}
    autocomplete="off"
    spellcheck="false"
    oninput={onInput}
    onkeydown={onKey}
    onkeyup={track}
    onclick={track}
    onselect={track}
    onfocus={() => {
      focused = true;
      track();
    }}
    onblur={() => (focused = false)}
  />
  <span class="spinner" class:on={searching} aria-hidden="true"></span>
  <span id="filter-hint" class="sr-only">
    Matches file names. dir: matches folder names, is: file types. Arrow keys show suggestions; Escape clears.
  </span>
  <ul id="filter-suggestions" role="listbox" aria-label="Suggestions" hidden={!open}>
    {#if open}
      {#each items as item, i (item.insert)}
        <!-- Chosen with the pointer without taking the focus from the box. -->
        <li
          id="filter-suggestion-{i}"
          role="option"
          aria-selected={i === active}
          class:active={i === active}
          onpointerdown={(e) => {
            e.preventDefault();
            if (ctx) void accept(item, ctx);
          }}
          onpointermove={() => (active = i)}
        >
          {#if item.type !== undefined}
            <span class="swatch" style:background={typeColor(item.type, theme.dark)} aria-hidden="true"></span>
          {/if}
          <code class="row-name">{item.insert}</code>
          <span class="detail muted">{item.detail}</span>
        </li>
      {/each}
    {/if}
  </ul>
</div>

<style>
  .box {
    position: relative;
    display: flex;
    align-items: center;
    flex: 1;
    min-width: 0;
  }
  input {
    flex: 1;
    min-width: 0;
  }
  .spinner {
    width: 12px;
    height: 12px;
    margin-left: -30px;
    margin-right: 6px;
    border: 2px solid var(--line);
    border-top-color: var(--accent);
    border-radius: 50%;
    visibility: hidden;
    pointer-events: none;
  }
  .spinner.on {
    visibility: visible;
    animation: spin 0.8s linear infinite;
  }
  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }
  ul {
    position: absolute;
    top: calc(100% + 4px);
    left: 0;
    z-index: 25;
    min-width: max(100%, 300px);
    max-height: 320px;
    overflow-y: auto;
    margin: 0;
    padding: 4px 0;
    list-style: none;
    border: 1px solid var(--line);
    border-radius: 6px;
    background: var(--bg);
    box-shadow: 0 6px 20px rgb(0 0 0 / 0.2);
  }
  li {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: 4px var(--space-3);
    cursor: pointer;
  }
  li.active {
    background: var(--selected);
  }
  code {
    flex: 0 1 auto;
    font-size: var(--fs-sm);
  }
  .detail {
    flex: none;
    margin-left: auto;
    padding-left: var(--space-3);
    font-size: var(--fs-sm);
  }
  .swatch {
    flex: none;
    width: 10px;
    height: 10px;
    border-radius: 2px;
  }
</style>
