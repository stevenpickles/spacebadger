<script lang="ts">
  import { exactBytes, formatBytes, plural } from "./format";
  import type { FolderState } from "./protocol/FolderState";
  import type { NodeDetails } from "./protocol/NodeDetails";
  import type { Selection } from "./selection";

  interface Props {
    selection: Selection | null;
    details: NodeDetails | null;
    /** Weight and count of a selected "other small items" region. */
    other: { weight: number; items: number } | null;
    view: number;
    onopen: (node: number) => void;
  }

  let { selection, details, other, view, onopen }: Props = $props();

  const folderStates: Record<FolderState, string> = {
    pending: "Still being scanned",
    listed: "Scanned",
    failed: "Couldn't be read; contents unknown",
    notScanned: "Not scanned (scan cancelled)",
  };
</script>

<section class="details" aria-label="Selected item" aria-live="polite">
  {#if !selection}
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
    <p class="muted">These items are too small to draw at this size.</p>
    {#if selection.node !== view}
      <button type="button" onclick={() => onopen(selection.node)}>Open folder in map</button>
    {/if}
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
    {#if details.folder && details.node !== view}
      <button type="button" onclick={() => onopen(details.node)}>Open folder in map</button>
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
</style>
