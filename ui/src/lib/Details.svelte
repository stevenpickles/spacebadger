<script lang="ts">
  import { describeItems, exactBytes, formatBytes, plural } from "./format";
  import type { FolderState } from "./protocol/FolderState";
  import type { NodeDetails } from "./protocol/NodeDetails";
  import type { SelectionSummary } from "./protocol/SelectionSummary";
  import type { Selection } from "./selection";

  interface Props {
    selection: Selection | null;
    details: NodeDetails | null;
    /** Weight and count of a selected "other small items" region. */
    other: { weight: number; items: number } | null;
    /** Totals when several items are selected. */
    multi: SelectionSummary | null;
    view: number;
    /** "Explorer", "Finder", or "file manager". */
    fileManager: string;
    /** "Recycle Bin" or "Trash". */
    trashName: string;
    /** "Ctrl", or "⌘" on macOS. */
    modKey: string;
    /** Why deleting isn't available now, if it isn't. */
    deleteBlocked: string | null;
    onopen: (node: number) => void;
    onreveal: (node: number) => void;
    oncopy: (node: number) => void;
    ondelete: (permanent: boolean) => void;
  }

  let {
    selection,
    details,
    other,
    multi,
    view,
    fileManager,
    trashName,
    modKey,
    deleteBlocked,
    onopen,
    onreveal,
    oncopy,
    ondelete,
  }: Props = $props();


  const folderStates: Record<FolderState, string> = {
    pending: "Still being scanned",
    listed: "Scanned",
    failed: "Couldn't be read; contents unknown",
    notScanned: "Not scanned (scan cancelled)",
  };
</script>

{#snippet deleteActions()}
  <div class="actions delete">
    <button type="button" disabled={!!deleteBlocked} title={deleteBlocked} onclick={() => ondelete(false)}>
      Move to {trashName}
    </button>
    <button
      type="button"
      class="danger"
      disabled={!!deleteBlocked}
      title={deleteBlocked}
      onclick={() => ondelete(true)}
    >
      Delete permanently…
    </button>
  </div>
{/snippet}

<section class="details" aria-label="Selected item" aria-live="polite">
  {#if multi && multi.items > 1}
    <h2>{plural(multi.items, "item")} selected</h2>
    <p class="path">{describeItems(multi.items, multi.folders, multi.files)}</p>
    <dl>
      <dt>Allocated</dt>
      <dd>{formatBytes(multi.allocated)} <span class="muted">({exactBytes(multi.allocated)})</span></dd>
      <dt>Logical</dt>
      <dd>{formatBytes(multi.logical)} <span class="muted">({exactBytes(multi.logical)})</span></dd>
      {#if multi.unknownAllocationFiles > 0}
        <dt>Unknown</dt>
        <dd>{plural(multi.unknownAllocationFiles, "file")} with unknown allocation</dd>
      {/if}
    </dl>
    <ul class="samples">
      {#each multi.samples as item (item.node)}
        <li>
          <span class="name" title={item.path}>{item.name}{item.folder ? "/" : ""}</span>
          <span class="size">{item.allocated === null ? "unknown" : formatBytes(item.allocated)}</span>
        </li>
      {/each}
      {#if multi.items > multi.samples.length}
        <li class="muted">and {plural(multi.items - multi.samples.length, "more item")}</li>
      {/if}
    </ul>
    <p class="muted hint">{modKey}+click adds or removes items; Shift+click selects a range. Escape clears.</p>
    {@render deleteActions()}
  {:else if !selection}
    <p class="muted">Select an item in the map to see its details.</p>
  {:else if selection.other}
    <h2>{other ? plural(other.items, "small item") : "Small items"}</h2>
    {#if details}
      <p class="path">in {details.path}</p>
    {/if}
    {#if other}
      <dl>
        <dt>Combined</dt>
        <dd>{formatBytes(other.weight)} <span class="muted">({exactBytes(other.weight)})</span></dd>
      </dl>
    {/if}
    <p class="muted">They are too small to draw at this size, so they are listed below.</p>
    <div class="actions">
      {#if selection.node !== view}
        <button type="button" onclick={() => onopen(selection.node)}>Open folder in map</button>
      {/if}
      <button type="button" onclick={() => onreveal(selection.node)}>Show folder in {fileManager}</button>
    </div>
  {:else if details}
    <h2>{details.name}</h2>
    <p class="path">{details.path}</p>
    <dl>
      <dt>Type</dt>
      <dd>{details.folder ? "Folder" : "File"}</dd>
      <dt>Allocated</dt>
      <dd>
        {#if details.allocated === null}
          Unknown
        {:else}
          {formatBytes(details.allocated)}
          <span class="muted">({exactBytes(details.allocated)})</span>
        {/if}
      </dd>
      <dt>Logical</dt>
      <dd>
        {formatBytes(details.logical)} <span class="muted">({exactBytes(details.logical)})</span>
      </dd>
      {#if details.folder}
        <dt>Files</dt>
        <dd>{details.files.toLocaleString()}</dd>
        <dt>Status</dt>
        <dd>{folderStates[details.folder]}</dd>
      {/if}
      {#if details.folder && details.unknownAllocationFiles > 0}
        <dt>Unknown</dt>
        <dd>{plural(details.unknownAllocationFiles, "file")} with unknown allocation, not in the map</dd>
      {/if}
    </dl>
    {#if details.cloud}
      <p class="note">Cloud file: only bytes stored on this device count as allocated.</p>
    {/if}
    {#if details.aliasOf}
      <p class="note">Hard link: its allocation is counted at {details.aliasOf}.</p>
    {:else if details.hardlinked}
      <p class="note">Hard-linked: its allocation is counted here, once.</p>
    {/if}
    <div class="actions">
      {#if details.folder && details.node !== view}
        <button type="button" onclick={() => onopen(details.node)}>Open folder in map</button>
      {/if}
      <button type="button" onclick={() => onreveal(details.node)}>Show in {fileManager}</button>
      <button type="button" onclick={() => oncopy(details.node)}>Copy path</button>
    </div>
    {#if details.node !== 0 && details.node !== view}
      {@render deleteActions()}
    {/if}
  {:else}
    <p class="muted">Loading…</p>
  {/if}
</section>

<style>
  .details {
    padding: 12px;
    overflow-wrap: anywhere;
  }
  h2 {
    margin: 0 0 4px;
    font-size: 15px;
  }
  .path {
    margin: 0 0 10px;
    font-size: 12px;
    color: var(--muted);
    user-select: text;
  }
  dl {
    display: grid;
    grid-template-columns: auto 1fr;
    gap: 4px 10px;
    margin: 0 0 10px;
  }
  dt {
    color: var(--muted);
  }
  dd {
    margin: 0;
  }
  .note {
    margin: 0 0 8px;
    font-size: 12px;
  }
  .muted {
    color: var(--muted);
  }
  .actions {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
  }
  .actions.delete {
    margin-top: 8px;
  }
  .danger {
    color: var(--danger);
  }
  .samples {
    margin: 0 0 8px;
    padding: 0;
    list-style: none;
    font-size: 12px;
  }
  .samples li {
    display: flex;
    justify-content: space-between;
    gap: 8px;
  }
  .samples .name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .samples .size {
    flex: none;
    font-variant-numeric: tabular-nums;
  }
  .hint {
    margin: 0 0 4px;
    font-size: 12px;
  }
</style>
