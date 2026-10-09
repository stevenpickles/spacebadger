<script lang="ts">
  // Confirms moving items to the Recycle Bin or Trash, saying exactly what
  // it covers. The confirm button stays disabled until the word "delete" is
  // typed and a short countdown has run, so a reflexive Enter or click can't
  // go through.
  import { describeItems, formatBytes, plural } from "./format";
  import type { SelectionSummary } from "./protocol/SelectionSummary";

  interface Props {
    summary: SelectionSummary;
    /** "Recycle Bin" or "Trash". */
    trashName: string;
    /** A filename filter is active, so folders hold more than is drawn. */
    filtered: boolean;
    /** Running as administrator or root. */
    privileged: boolean;
    /** The delete is running. */
    busy: boolean;
    onconfirm: () => void;
    oncancel: () => void;
  }

  let { summary, trashName, filtered, privileged, busy, onconfirm, oncancel }: Props = $props();

  /** What has to be typed. */
  const WORD = "delete";
  /** Seconds before the confirm button can be used. */
  const WAIT = 3;

  let typed = $state("");
  let left = $state(WAIT);
  let input: HTMLInputElement;
  let cancelButton: HTMLButtonElement;
  let confirmButton: HTMLButtonElement;
  const returnFocus = document.activeElement as HTMLElement | null;

  const ready = $derived(typed.trim().toLowerCase() === WORD && left === 0 && !busy);

  const what = $derived(
    summary.items === 1 && summary.samples[0] ? `“${summary.samples[0].name}”` : plural(summary.items, "item"),
  );
  const contents = $derived(describeItems(summary.items, summary.folders, summary.files));

  $effect(() => {
    input.focus();
    const timer = setInterval(() => {
      left = Math.max(0, left - 1);
      if (left === 0) clearInterval(timer);
    }, 1000);
    return () => {
      clearInterval(timer);
      returnFocus?.focus();
    };
  });

  function confirm() {
    if (ready) onconfirm();
  }

  function onKey(e: KeyboardEvent) {
    if (e.key === "Escape") {
      if (!busy) oncancel();
    } else if (e.key === "Enter" && e.target === input) {
      confirm();
    } else if (e.key === "Tab") {
      // Keep focus inside the dialog.
      const order: HTMLElement[] = [input, cancelButton, confirmButton].filter((el) => !el.disabled);
      const at = order.indexOf(document.activeElement as HTMLElement);
      const next = order[(at + (e.shiftKey ? order.length - 1 : 1)) % order.length];
      next?.focus();
    } else {
      // Keys mustn't reach the map or lists behind the dialog.
      e.stopPropagation();
      return;
    }
    e.preventDefault();
    e.stopPropagation();
  }
</script>

<div class="backdrop">
  <div
    class="dialog"
    role="alertdialog"
    aria-modal="true"
    aria-labelledby="delete-title"
    aria-describedby="delete-what"
    tabindex="-1"
    onkeydown={onKey}
  >
    <h2 id="delete-title">Move {what} to the {trashName}?</h2>
    <p id="delete-what">
      {contents}, {formatBytes(summary.allocated)} allocated ({formatBytes(summary.logical)} logical).
    </p>
    <ul>
      {#each summary.samples as item (item.node)}
        <li title={item.path}>
          <span class="name">{item.name}{item.folder ? "/" : ""}</span>
          <span class="size">{item.allocated === null ? "unknown" : formatBytes(item.allocated)}</span>
        </li>
      {/each}
      {#if summary.items > summary.samples.length}
        <li class="muted">and {plural(summary.items - summary.samples.length, "more item")}</li>
      {/if}
    </ul>
    {#if summary.folders > 0}
      <p class="warning">
        Folders go with everything in them{filtered ? ", including files the filter hides" : ""}.
      </p>
    {/if}
    <p class="muted">
      Items that can't go to the {trashName} (on network or removable drives, or too large for it) are left where
      they are; nothing is deleted permanently. The space is freed when the {trashName} is emptied.
    </p>
    {#if privileged}
      <p class="warning">SpaceBadger has administrator rights, so protected files can be moved too.</p>
    {/if}
    <label class="type">
      Type <strong>{WORD}</strong> to confirm
      <input
        type="text"
        bind:this={input}
        bind:value={typed}
        disabled={busy}
        autocomplete="off"
        spellcheck="false"
      />
    </label>
    <div class="buttons">
      {#if busy}<span class="muted" role="status">Moving…</span>{/if}
      <button type="button" bind:this={cancelButton} disabled={busy} onclick={oncancel}>Cancel</button>
      <button type="button" class="confirm" bind:this={confirmButton} disabled={!ready} onclick={confirm}>
        {left > 0 ? `Move to ${trashName} (${left})` : `Move to ${trashName}`}
      </button>
    </div>
  </div>
</div>

<style>
  .backdrop {
    position: fixed;
    inset: 0;
    z-index: 20;
    display: flex;
    align-items: center;
    justify-content: center;
    background: rgb(0 0 0 / 0.35);
  }
  .dialog {
    width: min(480px, calc(100vw - 32px));
    max-height: calc(100vh - 32px);
    overflow-y: auto;
    box-sizing: border-box;
    padding: 16px 20px;
    border: 1px solid var(--line);
    border-radius: 8px;
    background: var(--panel);
    box-shadow: 0 8px 32px rgb(0 0 0 / 0.35);
    overflow-wrap: anywhere;
  }
  .dialog:focus {
    outline: none;
  }
  h2 {
    margin: 0 0 8px;
    font-size: 16px;
  }
  p {
    margin: 0 0 10px;
  }
  ul {
    margin: 0 0 12px;
    padding: 6px 10px;
    list-style: none;
    border: 1px solid var(--line);
    border-radius: 4px;
    background: var(--bg);
    font-size: 12px;
  }
  li {
    display: flex;
    justify-content: space-between;
    gap: 12px;
    padding: 1px 0;
  }
  .name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .size {
    flex: none;
    font-variant-numeric: tabular-nums;
  }
  .muted {
    color: var(--muted);
    font-size: 12px;
  }
  .warning {
    color: var(--danger);
    font-weight: 600;
  }
  .type {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .type input {
    flex: 1;
    font: inherit;
    padding: 4px 8px;
    border: 1px solid var(--line);
    border-radius: 4px;
    background: var(--bg);
    color: var(--fg);
    user-select: text;
  }
  .buttons {
    display: flex;
    justify-content: flex-end;
    align-items: center;
    gap: 8px;
    margin-top: 14px;
  }
  .buttons .muted {
    margin-right: auto;
  }
  .confirm {
    background: var(--danger);
    border-color: var(--danger);
    color: var(--bg);
  }
</style>
