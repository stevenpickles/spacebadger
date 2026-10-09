<script lang="ts">
  // Confirms a delete, saying exactly what it covers. Moving to the trash
  // starts on the confirm button; deleting permanently starts on Cancel.
  import { describeItems, formatBytes, plural } from "./format";
  import type { SelectionSummary } from "./protocol/SelectionSummary";

  interface Props {
    summary: SelectionSummary;
    permanent: boolean;
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

  let { summary, permanent, trashName, filtered, privileged, busy, onconfirm, oncancel }: Props =
    $props();

  let cancelButton: HTMLButtonElement;
  let confirmButton: HTMLButtonElement;
  const returnFocus = document.activeElement as HTMLElement | null;

  const what = $derived(
    summary.items === 1 && summary.samples[0] ? `“${summary.samples[0].name}”` : plural(summary.items, "item"),
  );
  const title = $derived(permanent ? `Permanently delete ${what}?` : `Move ${what} to the ${trashName}?`);

  const contents = $derived(describeItems(summary.items, summary.folders, summary.files));

  $effect(() => {
    (permanent ? cancelButton : confirmButton).focus();
    return () => returnFocus?.focus();
  });

  function onKey(e: KeyboardEvent) {
    if (e.key === "Escape") {
      if (!busy) oncancel();
    } else if (e.key === "Tab") {
      // Keep focus inside the dialog.
      const next = document.activeElement === cancelButton ? confirmButton : cancelButton;
      next.focus();
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
    <h2 id="delete-title">{title}</h2>
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
    {#if permanent}
      <p class="warning">This can't be undone. Nothing is kept in the {trashName}.</p>
    {:else}
      <p class="muted">
        The space is freed when the {trashName} is emptied.
        {#if trashName === "Recycle Bin"}
          If an item can't go to the Recycle Bin, Windows asks before deleting it permanently.
        {/if}
      </p>
    {/if}
    {#if filtered && summary.folders > 0}
      <p class="warning">Folders are deleted with everything in them, including files the filter hides.</p>
    {/if}
    {#if privileged}
      <p class="muted">SpaceBadger has administrator rights, so protected files can be deleted too.</p>
    {/if}
    <div class="buttons">
      {#if busy}<span class="muted" role="status">Deleting…</span>{/if}
      <button type="button" bind:this={cancelButton} disabled={busy} onclick={oncancel}>Cancel</button>
      <button
        type="button"
        class="confirm"
        class:danger={permanent}
        bind:this={confirmButton}
        disabled={busy}
        onclick={onconfirm}
      >
        {permanent ? "Delete permanently" : `Move to ${trashName}`}
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
    background: var(--accent);
    border-color: var(--accent);
    color: var(--bg);
  }
  .confirm.danger {
    background: var(--danger);
    border-color: var(--danger);
  }
</style>
