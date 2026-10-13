<script lang="ts">
  import { shortcut } from "./format";

  interface Props {
    trashName: string;
    onclose: () => void;
  }

  let { trashName, onclose }: Props = $props();

  let dialog = $state<HTMLDialogElement>();

  $effect(() => {
    dialog?.showModal();
  });

  const groups = $derived([
    {
      title: "Map",
      keys: [
        ["Click", "Select an item"],
        ["Double-click, Enter", "Open a folder"],
        ["Backspace, Alt+↑", "Go up one level"],
        ["Arrow keys", "Move the selection"],
        ["Escape", "Clear the selection"],
        [`${shortcut("Ctrl")}+click, ${shortcut("Ctrl+Space")}`, "Add or remove an item"],
        ["Shift+click", "Select a range in the same folder"],
        [shortcut("Ctrl+A"), "Select everything in the folder"],
        ["Right-click, Menu, Shift+F10", "Show actions"],
        ["Delete", `Move to the ${trashName} (when deleting is allowed)`],
      ],
    },
    {
      title: "Lists",
      keys: [
        ["↑ ↓, Home, End", "Move through the list"],
        ["Shift+↑ ↓", "Extend the selection"],
        ["Enter, double-click", "Show the item in the map"],
      ],
    },
    {
      title: "Anywhere",
      keys: [
        ["Escape in the filter", "Clear the filter"],
        ["?", "Show this list"],
      ],
    },
  ]);
</script>

<dialog
  bind:this={dialog}
  aria-labelledby="shortcuts-title"
  onclose={onclose}
  onclick={(e) => e.target === dialog && dialog?.close()}
>
  <div class="head">
    <h2 id="shortcuts-title">Keyboard and mouse</h2>
    <button type="button" class="icon-button" aria-label="Close" onclick={() => dialog?.close()}>✕</button>
  </div>
  {#each groups as group (group.title)}
    <h3>{group.title}</h3>
    <dl>
      {#each group.keys as [keys, what] (keys)}
        <dt><kbd>{keys}</kbd></dt>
        <dd>{what}</dd>
      {/each}
    </dl>
  {/each}
  <p class="muted small">Files are never opened. Deleting stays off until you check “Allow deleting”.</p>
</dialog>

<style>
  dialog {
    width: min(520px, calc(100vw - 32px));
    max-height: calc(100vh - 48px);
    padding: var(--space-3) 16px 16px;
    border: 1px solid var(--line);
    border-radius: 6px;
    background: var(--bg);
    color: var(--fg);
    box-shadow: 0 8px 28px rgb(0 0 0 / 0.25);
  }
  dialog::backdrop {
    background: rgb(0 0 0 / 0.35);
  }
  .head {
    display: flex;
    align-items: center;
    justify-content: space-between;
  }
  h2 {
    margin: 0;
    font-size: 16px;
  }
  h3 {
    margin: 14px 0 6px;
    font-size: 13px;
    color: var(--muted);
  }
  dl {
    display: grid;
    /* The same first column in every group, so descriptions line up. */
    grid-template-columns: minmax(0, 15em) 1fr;
    gap: 4px 16px;
    margin: 0;
  }
  dt,
  dd {
    margin: 0;
  }
  kbd {
    font: inherit;
    font-size: var(--fs-sm);
    padding: 1px 6px;
    border: 1px solid var(--line);
    border-radius: var(--radius);
    background: var(--panel);
    white-space: nowrap;
  }
  p {
    margin: 16px 0 0;
  }
</style>
