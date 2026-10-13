<script lang="ts" module>
  export interface MenuItem {
    label: string;
    /** Keyboard shortcut shown beside the label, e.g. "Del" or "Ctrl+C". */
    hint?: string;
    /** The shortcut as aria-keyshortcuts names, e.g. "Delete"; derived from
     * `hint` when left out. */
    keys?: string;
    action: () => void;
    /** Draw a separator above this item. */
    separator?: boolean;
    /** Shown but not available; `title` says why. */
    disabled?: boolean;
    title?: string;
    /** Destructive: drawn in the danger color. */
    danger?: boolean;
  }

  /** Display names that differ from the key names aria-keyshortcuts uses. */
  const KEY_NAMES: Record<string, string> = {
    Ctrl: "Control",
    Del: "Delete",
    Esc: "Escape",
    Cmd: "Meta",
    "⌘": "Meta",
    "⌥": "Alt",
    "⇧": "Shift",
    Option: "Alt",
  };

  const VALID_KEYS = new Set([
    "Control",
    "Alt",
    "Shift",
    "Meta",
    "Delete",
    "Escape",
    "Enter",
    "Backspace",
    "Tab",
    "Space",
    "Insert",
    "Home",
    "End",
    "PageUp",
    "PageDown",
    "ArrowUp",
    "ArrowDown",
    "ArrowLeft",
    "ArrowRight",
  ]);

  /** `hint` as a valid aria-keyshortcuts value, or undefined if it isn't one. */
  function keyShortcut(hint: string | undefined): string | undefined {
    if (!hint) return undefined;
    const keys = hint.split("+").map((k) => KEY_NAMES[k.trim()] ?? k.trim());
    const valid = keys.every((k) => VALID_KEYS.has(k) || /^F([1-9]|1[0-9]|2[0-4])$/.test(k) || /^\S$/.test(k));
    return valid ? keys.join("+") : undefined;
  }
</script>

<script lang="ts">
  import { tick } from "svelte";

  // A small popup menu at a point in the window. Arrow keys move between
  // items, including unavailable ones (which say why), Enter activates, and
  // Escape, Tab, scrolling, or a click elsewhere closes it. Focus returns to
  // whatever had it when the menu opened.

  interface Props {
    /** Window (client) coordinates. */
    x: number;
    y: number;
    label: string;
    /** What the menu acts on, e.g. "foo.txt" or "3 selected items". */
    heading?: string;
    items: MenuItem[];
    onclose: () => void;
  }

  let { x, y, label, heading, items, onclose }: Props = $props();

  const id = $props.id();

  let menu: HTMLDivElement;
  let left = $state(0);
  let top = $state(0);
  /** Hidden until it has been measured and placed. */
  let placed = $state(false);
  const returnFocus = document.activeElement as HTMLElement | null;

  function buttons(): HTMLButtonElement[] {
    return [...menu.querySelectorAll<HTMLButtonElement>("button[role=menuitem]")];
  }

  function close() {
    onclose();
    returnFocus?.focus();
  }

  $effect(() => {
    // Keep the whole menu inside the window.
    const r = menu.getBoundingClientRect();
    left = Math.max(4, Math.min(x, window.innerWidth - r.width - 4));
    top = Math.max(4, Math.min(y, window.innerHeight - r.height - 4));
    placed = true;
    // Hidden elements can't take focus, so wait until it's shown.
    tick().then(() => {
      const first = buttons().find((b) => b.getAttribute("aria-disabled") !== "true") ?? buttons()[0];
      (first ?? menu).focus();
    });

    const onPointer = (e: PointerEvent) => {
      if (!menu.contains(e.target as Node)) close();
    };
    const onScroll = (e: Event) => {
      if (!menu.contains(e.target as Node)) close();
    };
    const onDismiss = () => close();
    window.addEventListener("pointerdown", onPointer, true);
    window.addEventListener("wheel", onScroll, { capture: true, passive: true });
    window.addEventListener("scroll", onScroll, true);
    window.addEventListener("resize", onDismiss);
    window.addEventListener("blur", onDismiss);
    return () => {
      window.removeEventListener("pointerdown", onPointer, true);
      window.removeEventListener("wheel", onScroll, { capture: true });
      window.removeEventListener("scroll", onScroll, true);
      window.removeEventListener("resize", onDismiss);
      window.removeEventListener("blur", onDismiss);
    };
  });

  function activate(item: MenuItem) {
    if (item.disabled) return;
    close();
    item.action();
  }

  function onKey(e: KeyboardEvent) {
    const all = buttons();
    const at = all.indexOf(document.activeElement as HTMLButtonElement);
    let next = -1;
    if (e.key === "ArrowDown") next = (at + 1) % all.length;
    else if (e.key === "ArrowUp") next = at < 0 ? all.length - 1 : (at - 1 + all.length) % all.length;
    else if (e.key === "Home") next = 0;
    else if (e.key === "End") next = all.length - 1;
    else if (e.key === "Escape" || e.key === "Tab") close();
    else return;
    if (next >= 0) all[next]?.focus();
    e.preventDefault();
    e.stopPropagation();
  }
</script>

<div
  class="menu"
  role="menu"
  tabindex="-1"
  aria-label={heading ? `${label}: ${heading}` : label}
  bind:this={menu}
  style:left="{left}px"
  style:top="{top}px"
  style:visibility={placed ? null : "hidden"}
  onkeydown={onKey}
  oncontextmenu={(e) => e.preventDefault()}
>
  {#if heading}
    <div class="heading" aria-hidden="true">{heading}</div>
  {/if}
  {#each items as item, i (i)}
    {#if item.separator && (i > 0 || heading)}
      <div class="separator" role="separator"></div>
    {/if}
    <button
      type="button"
      role="menuitem"
      tabindex="-1"
      aria-keyshortcuts={item.keys ?? keyShortcut(item.hint)}
      aria-disabled={item.disabled ? "true" : undefined}
      aria-labelledby="{id}-{i}"
      aria-describedby={item.disabled && item.title ? `${id}-${i}-why` : undefined}
      class:danger={item.danger}
      class:unavailable={item.disabled}
      title={item.disabled ? undefined : item.title}
      onclick={() => activate(item)}
    >
      <span class="line">
        <span id="{id}-{i}">{item.label}</span>
        {#if item.hint}<kbd aria-hidden="true">{item.hint}</kbd>{/if}
      </span>
      {#if item.disabled && item.title}
        <span class="reason" id="{id}-{i}-why">{item.title}</span>
      {/if}
    </button>
  {/each}
</div>

<style>
  .menu {
    position: fixed;
    z-index: 10;
    min-width: 200px;
    max-width: min(360px, calc(100vw - 8px));
    max-height: calc(100vh - 8px);
    overflow-y: auto;
    box-sizing: border-box;
    padding: var(--space-1);
    border: 1px solid var(--line);
    border-radius: 6px;
    background: var(--panel);
    box-shadow: 0 4px 16px rgb(0 0 0 / 0.25);
    font-size: 13px;
  }
  .menu:focus {
    outline: none;
  }
  .heading {
    padding: var(--space-1) 10px;
    color: var(--muted);
    font-size: var(--fs-sm);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  button {
    display: flex;
    flex-direction: column;
    gap: 2px;
    width: 100%;
    padding: 5px 10px;
    border: none;
    border-radius: var(--radius);
    background: none;
    text-align: left;
  }
  button:hover,
  button:focus {
    background: var(--hover);
    outline: none;
  }
  button:focus-visible {
    box-shadow: inset 0 0 0 2px var(--accent);
  }
  .line {
    display: flex;
    justify-content: space-between;
    gap: 24px;
    width: 100%;
  }
  button.danger {
    color: var(--danger);
  }
  button.unavailable {
    color: var(--muted);
    cursor: default;
  }
  .reason {
    font-size: var(--fs-xs);
    color: var(--muted);
  }
  kbd {
    color: var(--muted);
    font: inherit;
  }
  .separator {
    height: 1px;
    margin: var(--space-1) 6px;
    background: var(--line);
  }
</style>
