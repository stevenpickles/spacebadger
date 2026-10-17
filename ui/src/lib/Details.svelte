<script lang="ts">
  import PathText from "./PathText.svelte";
  import { gitAccent } from "./colors";
  import { describeChange } from "./compare";
  import { describeItems, exactBytes, formatBytes, formatDate, percent, plural, shortcut } from "./format";
  import { GIT_BADGES } from "./layoutWire";
  import type { FolderState } from "./protocol/FolderState";
  import type { GitContent } from "./protocol/GitContent";
  import type { GitDetails } from "./protocol/GitDetails";
  import { KIND_BADGE, repositoryLabel } from "./Repositories.svelte";
  import { theme } from "./theme.svelte";
  import type { ItemChange } from "./protocol/ItemChange";
  import type { NodeDetails } from "./protocol/NodeDetails";
  import type { SelectionSummary } from "./protocol/SelectionSummary";
  import SampleList from "./SampleList.svelte";
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
    /** Unused; key names now come from `shortcut()`. */
    modKey?: string;
    /** Deleting is turned on for this run of the app. */
    canDelete: boolean;
    /** Why deleting isn't available now, if it isn't. */
    deleteBlocked: string | null;
    /** Size of the selected item's parent folder, for "n% of parent folder". */
    parentSize?: number | null;
    /** The folder the map shows, described when nothing is selected. */
    viewed?: NodeDetails | null;
    /** Size of the viewed folder's parent. */
    viewedParentSize?: number | null;
    /** The multi-selection totals are being recounted; `multi` is stale. */
    updating?: boolean;
    /** Loading the selected item's details failed. */
    error?: string | null;
    onretry?: () => void;
    onopen: (node: number) => void;
    onreveal: (node: number) => void;
    oncopy: (node: number) => void;
    /** Copy the paths of every selected item. */
    oncopypaths?: () => void;
    ondelete: () => void;
    /** While comparing: how the selected item and the viewed folder
     * changed since the earlier scan, and when that was. */
    change?: ItemChange | null;
    viewedChange?: ItemChange | null;
    since?: string;
  }

  let {
    selection,
    details,
    other,
    multi,
    view,
    fileManager,
    trashName,
    canDelete,
    deleteBlocked,
    parentSize = null,
    viewed = null,
    viewedParentSize = null,
    updating = false,
    error = null,
    onretry,
    onopen,
    onreveal,
    oncopy,
    oncopypaths,
    ondelete,
    change = null,
    viewedChange = null,
    since = "",
  }: Props = $props();

  const ROOT = 0;

  const folderStates: Record<FolderState, string> = {
    pending: "Still being scanned",
    listed: "Scanned",
    partial: "Partly read; some of its contents couldn't be read",
    failed: "Couldn't be read; contents unknown",
    notScanned: "Not scanned (scan cancelled)",
  };

  const GIT_CONTENT: Record<GitContent, string> = {
    checkout: "Repository content",
    worktree: "Linked worktree content",
    metadata: "Git metadata",
  };

  const ctrl = shortcut("Ctrl");

  /** Exact size, plus size on disk when it's known. */
  function sizeTitle(logical: number, allocated: number | null): string {
    const lines = [exactBytes(logical)];
    if (allocated !== null) lines.push(`On disk: ${formatBytes(allocated)} (${exactBytes(allocated)})`);
    return lines.join("\n");
  }
</script>

{#snippet deleteAction(label: string)}
  <div class="delete">
    {#if canDelete}
      <button type="button" class="danger" disabled={!!deleteBlocked} onclick={ondelete}>{label}</button>
      {#if deleteBlocked}<p class="muted small reason">{deleteBlocked}</p>{/if}
    {:else}
      <button type="button" class="danger" disabled>{label}</button>
      <p class="muted small reason">Turn on “Allow deleting” in the toolbar to use this.</p>
    {/if}
  </div>
{/snippet}

{#snippet changeRow(d: NodeDetails, c: ItemChange | null)}
  {@const said = c && c.node === d.node ? describeChange(c.kind, c.before, c.beforeBelow, c.after, since) : null}
  {#if said}
    <!-- With a filter on, sizes above are matching files; changes cover all. -->
    <dt title={d.matching ? "Change in all files, not only those matching the filter" : undefined}>
      {d.matching ? "Change (all files)" : "Change"}
    </dt>
    <dd class="change {c?.kind}">
      {said.text}{#if said.was}<span class="muted">{` (${said.was})`}</span>{/if}
    </dd>
  {/if}
{/snippet}

{#snippet gitGroup(d: NodeDetails, g: GitDetails)}
  {@const root = g.root}
  {@const isRoot = root !== null && !root.outside && root.node === d.node}
  <section class="git" aria-label="Git">
    <h3 class="eyebrow">Git</h3>
    {#if root}
      {@const badge = KIND_BADGE[root.kind]}
      {#if !isRoot}
        <p class="root">
          <span class="badge" style:--kind={gitAccent(badge, theme.dark)}>{GIT_BADGES[badge]!.short}</span>
          <span class="selectable">{root.name}</span>
          {#if !root.outside && root.node !== view}
            <button type="button" class="link" onclick={() => onopen(root.node)}>Open</button>
          {/if}
        </p>
        <p class="path selectable">
          {#if root.outside}Above the scan root: {/if}<PathText path={root.path} />
        </p>
      {/if}
      <div class="facts">
        <div>
          <span class="label">Type</span>
          <span>{GIT_BADGES[badge]!.name}</span>
        </div>
        {#if root.head}
          <div title="As named by HEAD">
            <span class="label">{root.head.kind === "detached" ? "HEAD" : "Branch"}</span>
            <span class="selectable"
              >{root.head.kind === "detached" ? `Detached at ${root.head.commit}` : root.head.name}</span
            >
          </div>
        {/if}
        <!-- The root's own sizes are the item's, above, so they agree. -->
        {#if !isRoot}
          <div title="The whole repository or worktree as scanned, nested ones included">
            <span class="label">Root scanned</span>
            <span>{formatBytes(root.size)}</span>
          </div>
          {#if root.matching !== null}
            <div title="Of the root's scanned size, what the filter matches">
              <span class="label">Root matching</span>
              <span>{formatBytes(root.matching)}</span>
            </div>
          {/if}
        {/if}
        {#if !isRoot || g.content}
          <div>
            <span class="label">Content here</span>
            <span>{g.content ? GIT_CONTENT[g.content] : "Not classified"}</span>
          </div>
        {/if}
      </div>
      {#if root.repository}
        <p class="note">
          {repositoryLabel(root)}:
          <span class="selectable"><PathText path={root.repository.path} /></span>
          {#if root.repository.node !== null && root.repository.node !== view}
            {@const target = root.repository.node}
            <button type="button" class="link" onclick={() => onopen(target)}>Open</button>
          {:else if root.repository.node === null}
            <span class="muted">(outside the scan)</span>
          {/if}
        </p>
      {/if}
      {#if root.kind === "linkedWorktree" && root.repository?.node === null}
        <p class="note muted">Its shared Git data is in the main repository, outside the scan, so it isn't counted.</p>
      {:else if root.kind === "linkedWorktree"}
        <p class="note muted">Its shared Git data is counted where it is, in the main repository, not here.</p>
      {/if}
      {#if g.worktreesRegistered}
        {@const [registered, found] = g.worktreesRegistered}
        <p class="note">Linked worktrees: {registered.toLocaleString()} registered, {found.toLocaleString()} in this scan</p>
      {/if}
      {#if root.worktree}
        <p class="note muted">
          Its config sets the working tree to <PathText path={root.worktree} />; sizes stay where the files are.
        </p>
      {/if}
      {#if root.recordedAt}
        <p class="note muted">Its Git data records it at <PathText path={root.recordedAt} />.</p>
      {/if}
      {#if root.problem}
        <p class="note problem">{root.problem}</p>
      {/if}
      {#if root.partial}
        <p class="note muted">Some of it couldn't be read or wasn't scanned, so sizes are lower bounds.</p>
      {/if}
    {:else if g.content}
      <div class="facts">
        <div>
          <span class="label">Content here</span>
          <span>{GIT_CONTENT[g.content]}</span>
        </div>
      </div>
    {/if}
  </section>
{/snippet}

{#snippet item(d: NodeDetails, parent: number | null, c: ItemChange | null)}
  <h2>{d.name}</h2>
  <!-- The scan root's name is already its path. -->
  {#if d.path !== d.name}
    <p class="path selectable"><PathText path={d.path} /></p>
  {/if}
  <dl>
    {#if d.folder && d.matching}
      <!-- Filtered: what the map shows first, then the whole folder. -->
      <dt>Matching</dt>
      <dd title={exactBytes(d.matching.logical)}>
        {formatBytes(d.matching.logical)}
        {#if parent !== null && parent > 0}
          <span class="muted">· {percent(d.matching.logical, parent)} of the parent folder's matches</span>
        {/if}
      </dd>
      <dt>Files</dt>
      <dd>{d.matching.files.toLocaleString()} of {d.files.toLocaleString()} match</dd>
      <dt>All files</dt>
      <dd title={sizeTitle(d.logical, d.allocated)}>{formatBytes(d.logical)}</dd>
    {:else}
      <dt>Size</dt>
      <dd title={sizeTitle(d.logical, d.allocated)}>
        {formatBytes(d.logical)}
        {#if parent !== null && parent > 0}
          <span class="muted">· {percent(d.logical, parent)} of parent folder</span>
        {/if}
      </dd>
    {/if}
    {@render changeRow(d, c)}
    <dt>Type</dt>
    <dd>{d.folder ? "Folder" : "File"}</dd>
    {#if !d.folder && d.modified !== null}
      <dt>Modified</dt>
      <dd>{formatDate(d.modified)}</dd>
    {/if}
    {#if d.folder}
      {#if !d.matching}
        <dt>Files</dt>
        <dd>{d.files.toLocaleString()}</dd>
      {/if}
      <dt>Status</dt>
      <dd>{folderStates[d.folder]}</dd>
    {/if}
  </dl>
  {#if d.estimatedFrom !== null}
    <!-- Only the map's area is estimated; every number here is measured. -->
    <p class="note">
      ≈ Its area on the map is estimated from the scan of {formatDate(d.estimatedFrom / 1000)} until the
      folders in it are scanned. The sizes above are what has been found so far.
    </p>
  {/if}
  {#if d.git && (d.git.content || d.git.root)}
    {@render gitGroup(d, d.git)}
  {/if}
  {#if d.cloud}
    <p class="note">Cloud file: only the part stored on this device takes up space.</p>
  {/if}
  {#if d.aliasOf}
    <p class="note">Hard link: its size is counted once, at {d.aliasOf}.</p>
  {:else if d.hardlinked}
    <p class="note">Hard-linked: its size is counted here, once.</p>
  {/if}
  <div class="actions">
    {#if d.folder && d.node !== view}
      <button type="button" onclick={() => onopen(d.node)}>Open folder in map</button>
    {/if}
    <button type="button" onclick={() => onreveal(d.node)}>Show in {fileManager}</button>
    <button type="button" onclick={() => oncopy(d.node)}>Copy path</button>
  </div>
  {#if d.node === ROOT}
    <p class="muted small reason">This is the scanned folder itself, so it can't be moved to the {trashName} from here.</p>
  {:else if d.node === view}
    <p class="muted small reason">
      This is the folder you're viewing. Go up a level to move it to the {trashName}.
    </p>
  {:else}
    {@render deleteAction(`Move to the ${trashName}…`)}
  {/if}
{/snippet}

<section class="details" aria-label={selection || multi ? "Selected item" : "Folder in view"}>
  {#if multi && multi.items > 1}
    <h2>{plural(multi.items, "item")} selected</h2>
    {#if updating}
      <p class="muted small">Updating…</p>
    {:else}
      <p class="path">{describeItems(multi.items, multi.folders, multi.files)}</p>
      <dl>
        <dt>Size</dt>
        <dd title={sizeTitle(multi.logical, multi.allocated)}>{formatBytes(multi.logical)}</dd>
      </dl>
      <div class="samples">
        <SampleList samples={multi.samples} total={multi.items} />
      </div>
    {/if}
    <p class="muted small hint">
      {ctrl}+click adds or removes items; Shift+click selects a range. Escape clears.
    </p>
    {#if oncopypaths}
      <div class="actions">
        <button type="button" onclick={oncopypaths}>Copy {multi.items.toLocaleString()} paths</button>
      </div>
    {/if}
    {@render deleteAction(`Move ${plural(multi.items, "item")} to the ${trashName}…`)}
  {:else if !selection && viewed}
    <p class="eyebrow">{viewed.matching ? "Viewing · filtered" : "Viewing"}</p>
    {@render item(viewed, viewedParentSize, viewedChange)}
    <p class="muted small hint">
      Click an item in the map for its details; {ctrl}+click or Shift+click selects several.
    </p>
  {:else if !selection}
    <h2>Nothing selected</h2>
    <ul class="muted small tips">
      <li>Click an item in the map to select it.</li>
      <li>Double-click a folder to open it.</li>
      <li>{ctrl}+click or Shift+click to select several.</li>
      <li>Right-click for more actions.</li>
    </ul>
  {:else if selection.other}
    <h2>Small items</h2>
    {#if details}
      <p class="path selectable">in <PathText path={details.path} /></p>
    {/if}
    {#if other}
      <dl>
        <dt>Size</dt>
        <dd title={exactBytes(other.weight)}>
          {formatBytes(other.weight)}
          {#if details && (details.matching?.logical ?? details.logical) > 0}
            <span class="muted">
              · {percent(other.weight, details.matching?.logical ?? details.logical)} of the folder{details.matching
                ? "'s matches"
                : ""}
            </span>
          {/if}
        </dd>
      </dl>
      <p class="muted small">{plural(other.items, "item")} too small to draw one by one at this size.</p>
    {:else}
      <p class="muted small">Items too small to draw one by one at this size.</p>
    {/if}
    <div class="actions">
      {#if selection.node !== view}
        <button type="button" onclick={() => onopen(selection.node)}>Open folder in map</button>
      {/if}
      <button type="button" onclick={() => onreveal(selection.node)}>Show folder in {fileManager}</button>
    </div>
  {:else if details}
    {@render item(details, parentSize, change)}
  {:else if error}
    <p class="error selectable">Couldn't load details: {error}</p>
    {#if onretry}
      <div class="actions">
        <button type="button" onclick={onretry}>Retry</button>
      </div>
    {/if}
  {:else}
    <p class="muted">Loading…</p>
  {/if}
</section>

<style>
  .details {
    padding: var(--space-3);
    overflow-wrap: anywhere;
  }
  h2 {
    margin: 0 0 var(--space-1);
    font-size: 13px;
    font-weight: 600;
  }
  p {
    margin: 0 0 var(--space-2);
  }
  .path {
    font-size: var(--fs-sm);
    color: var(--muted);
  }
  dl {
    display: grid;
    grid-template-columns: auto 1fr;
    gap: var(--space-1) 10px;
    margin: 0 0 10px;
  }
  dt {
    color: var(--muted);
  }
  dd {
    margin: 0;
  }
  .note {
    font-size: var(--fs-sm);
  }
  .change.grown {
    color: var(--change-grown);
  }
  .change.shrunk {
    color: var(--change-shrunk);
  }
  .change.new {
    color: var(--change-new);
  }
  .actions {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
  }
  .delete {
    margin-top: var(--space-2);
  }
  .reason {
    margin: var(--space-1) 0 0;
  }
  .samples {
    margin: 0 0 var(--space-2);
  }
  .eyebrow {
    margin: 0 0 2px;
    font-size: var(--fs-xs);
    font-weight: 600;
    letter-spacing: 0.04em;
    text-transform: uppercase;
    color: var(--muted);
  }
  .hint {
    margin: var(--space-3) 0 0;
  }
  /* Git: a labelled group of facts, two to a row. */
  .git {
    margin: 0 0 10px;
    padding-top: var(--space-2);
    border-top: 1px solid var(--line);
  }
  .git .eyebrow {
    margin-bottom: var(--space-1);
  }
  .root {
    display: flex;
    align-items: center;
    gap: 6px;
    margin-bottom: 2px;
    font-weight: 600;
  }
  .root .link {
    font-weight: normal;
  }
  .facts {
    display: grid;
    grid-template-columns: repeat(2, minmax(0, 1fr));
    gap: var(--space-2) var(--space-3);
    margin: 0 0 var(--space-2);
  }
  .facts > div {
    display: flex;
    flex-direction: column;
    min-width: 0;
  }
  .label {
    font-size: var(--fs-xs);
    letter-spacing: 0.04em;
    text-transform: uppercase;
    color: var(--muted);
  }
  /* The word carries the kind; the color echoes the map's accent. */
  .badge {
    flex: none;
    padding: 0 5px;
    border: 1px solid var(--kind);
    border-left-width: 3px;
    border-radius: var(--radius);
    font-size: var(--fs-xs);
    font-weight: normal;
    line-height: 16px;
  }
  .problem {
    padding-left: 6px;
    border-left: 3px solid var(--incomplete);
  }
  .tips {
    margin: 0;
    padding-left: 18px;
  }
  .tips li + li {
    margin-top: 2px;
  }
</style>
