<script lang="ts" module>
  export interface MenuItem {
    label: string;
    /** Keyboard shortcut shown beside the label. */
    hint?: string;
    action: () => void;
    /** Draw a separator above this item. */
    separator?: boolean;
  }
</script>

<script lang="ts">
  // A small popup menu at a point in the window. Arrow keys move between
  // items, Enter activates, and Escape, Tab, or a click elsewhere closes it.
  // Focus returns to whatever had it when the menu opened.

  interface Props {
    /** Window (client) coordinates. */
    x: number;
    y: number;
    label: string;
    items: MenuItem[];
    onclose: () => void;
  }

  let { x, y, label, items, onclose }: Props = $props();

  let menu: HTMLDivElement;
  let left = $state(0);
  let top = $state(0);
  const returnFocus = document.activeElement as HTMLElement | null;

  function buttons(): HTMLButtonElement[] {
    return [...menu.querySelectorAll<HTMLButtonElement>("button")];
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
    buttons()[0]?.focus();

    const onPointer = (e: PointerEvent) => {
      if (!menu.contains(e.target as Node)) close();
    };
    const onDismiss = () => close();
    window.addEventListener("pointerdown", onPointer, true);
    window.addEventListener("resize", onDismiss);
    window.addEventListener("blur", onDismiss);
    return () => {
      window.removeEventListener("pointerdown", onPointer, true);
      window.removeEventListener("resize", onDismiss);
      window.removeEventListener("blur", onDismiss);
    };
  });

  function activate(item: MenuItem) {
    close();
    item.action();
  }

  function onKey(e: KeyboardEvent) {
    const all = buttons();
    const at = all.indexOf(document.activeElement as HTMLButtonElement);
    let next = -1;
    if (e.key === "ArrowDown") next = (at + 1) % all.length;
    else if (e.key === "ArrowUp") next = (at - 1 + all.length) % all.length;
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
  aria-label={label}
  bind:this={menu}
  style:left="{left}px"
  style:top="{top}px"
  onkeydown={onKey}
  oncontextmenu={(e) => e.preventDefault()}
>
  {#each items as item, i (i)}
    {#if item.separator && i > 0}
      <div class="separator" role="separator"></div>
    {/if}
    <button
      type="button"
      role="menuitem"
      tabindex="-1"
      aria-keyshortcuts={item.hint}
      onclick={() => activate(item)}
    >
      <span>{item.label}</span>
      {#if item.hint}<kbd aria-hidden="true">{item.hint}</kbd>{/if}
    </button>
  {/each}
</div>

<style>
  .menu {
    position: fixed;
    z-index: 10;
    min-width: 200px;
    padding: 4px;
    border: 1px solid var(--line);
    border-radius: 6px;
    background: var(--panel);
    box-shadow: 0 4px 16px rgb(0 0 0 / 0.25);
    font-size: 13px;
  }
  .menu:focus {
    outline: none;
  }
  button {
    display: flex;
    justify-content: space-between;
    gap: 24px;
    width: 100%;
    padding: 5px 10px;
    border: none;
    border-radius: 4px;
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
  kbd {
    color: var(--muted);
    font: inherit;
  }
  .separator {
    height: 1px;
    margin: 4px 6px;
    background: var(--line);
  }
</style>
