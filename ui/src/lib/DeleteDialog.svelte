<script lang="ts" module>
  export interface DeleteResult {
    /** Items moved (or already gone). */
    moved: number;
    /** Items asked for. */
    total: number;
    /** Size of the moved items, in bytes. */
    size: number;
    failures: { path: string; message: string }[];
  }
</script>

<script lang="ts">
  import PathText from "./PathText.svelte";
  // Confirms moving items to the Recycle Bin or Trash, saying exactly what
  // it covers. The confirm button stays disabled until the word "delete" is
  // typed and a short countdown has run, so a reflexive Enter or click can't
  // go through. When the move is done the dialog stays open to report what
  // moved and what couldn't.
  import { untrack } from "svelte";
  import { describeItems, exactBytes, formatBytes, plural } from "./format";
  import type { DuplicatesCheck } from "./protocol/DuplicatesCheck";
  import type { DuplicatesRecheck } from "./protocol/DuplicatesRecheck";
  import type { SelectionSummary } from "./protocol/SelectionSummary";
  import SampleList from "./SampleList.svelte";

  interface Props {
    summary: SelectionSummary;
    /** "Recycle Bin" or "Trash". */
    trashName: string;
    /** A filename filter is active, so folders hold more than is drawn. */
    filtered: boolean;
    /** Running as administrator or root. */
    privileged: boolean;
    /** Groups of duplicates the delete would leave with no copy. */
    duplicates?: DuplicatesCheck | "unknown" | null;
    /** The delete is running. */
    busy: boolean;
    /** While it runs, how far it has got reading the copies of duplicates
     * that would stay, before moving anything. */
    recheck?: DuplicatesRecheck | null;
    /** Stops that reading; files whose copy isn't confirmed are kept. */
    onstoprecheck?: () => void;
    /** Set once the delete has finished: the dialog then shows the outcome. */
    result?: DeleteResult | null;
    onconfirm: () => void;
    oncancel: () => void;
    /** Closes the result screen. */
    onclose?: () => void;
  }

  let {
    summary,
    trashName,
    filtered,
    privileged,
    duplicates = null,
    busy,
    recheck = null,
    onstoprecheck,
    result = null,
    onconfirm,
    oncancel,
    onclose,
  }: Props = $props();

  /** What has to be typed. */
  const WORD = "delete";
  /** Seconds before the confirm button can be used. */
  const WAIT = 3;

  const id = $props.id();

  let typed = $state("");
  let left = $state(WAIT);
  let dialog: HTMLDialogElement;
  let input = $state<HTMLInputElement>();
  let closeButton = $state<HTMLButtonElement>();
  const returnFocus = document.activeElement as HTMLElement | null;
  /** Unmounted: a late close event must not act. */
  let gone = false;

  const matches = $derived(typed.trim().toLowerCase() === WORD);
  const ready = $derived(matches && left === 0 && !busy);
  /** Reading copies of duplicates that would stay, before moving. */
  const rechecking = $derived(busy && recheck !== null && !recheck.finished);
  let stopped = $state(false);
  const missing = $derived.by(() => {
    // The field's label already asks for the word; this adds the wait.
    if (rechecking && recheck) {
      if (stopped) return "Stopping…";
      return `Checking the copies that stay: ${formatBytes(recheck.doneBytes)} of ${formatBytes(recheck.totalBytes)}`;
    }
    if (busy) return "Moving…";
    if (left > 0) return `You can confirm in ${left} s`;
    return matches ? "" : `Type ${WORD} above`;
  });

  const what = $derived(
    summary.items === 1 && summary.samples[0] ? `“${summary.samples[0].name}”` : plural(summary.items, "item"),
  );
  const contents = $derived(describeItems(summary.items, summary.folders, summary.files));

  const checked = $derived(typeof duplicates === "object" ? duplicates : null);
  const lostGroups = $derived(checked ? checked.allSelected + checked.keptGone : 0);
  /** Groups keeping copies that look unchanged, read again before moving. */
  const recheckGroups = $derived(checked ? checked.toRecheck - checked.keptGone - checked.unchecked : 0);
  /** Some groups of duplicates weren't checked; say so. */
  const unchecked = $derived(duplicates === "unknown" || (checked?.unchecked ?? 0) > 0);

  /** Every warning paragraph, for aria-describedby. */
  const describedBy = $derived(
    [
      `${id}-what`,
      lostGroups > 0 ? `${id}-duplicates` : "",
      recheckGroups > 0 ? `${id}-recheck` : "",
      unchecked ? `${id}-unchecked` : "",
      summary.folders > 0 ? `${id}-folders` : "",
      `${id}-skip`,
      privileged ? `${id}-admin` : "",
    ]
      .filter(Boolean)
      .join(" "),
  );

  $effect(() => {
    dialog.showModal();
    untrack(() => input?.focus());
    const timer = setInterval(() => {
      left = Math.max(0, left - 1);
      if (left === 0) clearInterval(timer);
    }, 1000);
    return () => {
      clearInterval(timer);
      gone = true;
      returnFocus?.focus();
    };
  });

  // While the move runs every control is disabled; keep focus in the dialog.
  $effect(() => {
    if (busy && !result) dialog.focus();
  });

  // The result screen's only control is Close.
  $effect(() => {
    if (result) closeButton?.focus();
  });

  function confirm() {
    if (ready) onconfirm();
  }

  function dismiss() {
    if (result) onclose?.();
    else if (!busy) oncancel();
  }

  /** Escape: the parent decides when the dialog goes away. */
  function onCancel(e: Event) {
    e.preventDefault();
    dismiss();
  }

  /** The browser may close the dialog anyway (repeated Escape). */
  function onClosed() {
    if (gone) return;
    if (busy && !result) dialog.showModal();
    else dismiss();
  }

  function onKey(e: KeyboardEvent) {
    // Keys mustn't reach the map or lists behind the dialog.
    e.stopPropagation();
    if (e.key === "Enter" && e.target === input) {
      e.preventDefault();
      confirm();
    }
  }
</script>

<dialog
  bind:this={dialog}
  role="alertdialog"
  aria-labelledby="{id}-title"
  aria-describedby={result ? `${id}-result` : describedBy}
  tabindex="-1"
  onkeydown={onKey}
  oncancel={onCancel}
  onclose={onClosed}
>
  {#if result}
    {@const all = result.failures.length === 0 && result.moved === result.total}
    <h2 id="{id}-title">
      {#if all}
        Moved {plural(result.moved, "item")} ({formatBytes(result.size)})
      {:else}
        Moved {result.moved.toLocaleString()} of {plural(result.total, "item")} to the {trashName}
      {/if}
    </h2>
    <p id="{id}-result">
      {#if all}
        They're in the {trashName}. The space is freed when it's emptied.
      {:else if result.failures.length > 0}
        {plural(result.failures.length, "item")} couldn't be moved and {result.failures.length === 1 ? "is" : "are"}
        still where {result.failures.length === 1 ? "it was" : "they were"}:
      {:else}
        The rest were no longer there.
      {/if}
    </p>
    {#if result.failures.length > 0}
      <ul class="failures">
        {#each result.failures as failure, i (i)}
          <li>
            <div class="selectable path"><PathText path={failure.path} /></div>
            <div class="muted small selectable">{failure.message}</div>
          </li>
        {/each}
      </ul>
    {/if}
    <div class="buttons">
      <button type="button" bind:this={closeButton} onclick={() => onclose?.()}>Close</button>
    </div>
  {:else}
    <h2 id="{id}-title">Move {what} to the {trashName}?</h2>
    <p id="{id}-what">
      {contents}. Frees about <span title={exactBytes(summary.logical)}>{formatBytes(summary.logical)}</span> when the
      {trashName} is emptied.
    </p>
    <div class="samples">
      <SampleList samples={summary.samples} total={summary.items} paths />
    </div>
    {#if checked && lostGroups > 0}
      <div class="warning" id="{id}-duplicates">
        {#if checked.allSelected > 0}
          <p>
            This deletes every copy in {plural(checked.allSelected, "group")} of duplicates. Nothing of
            {checked.allSelected === 1 ? "it" : "them"} would remain outside the {trashName}.
          </p>
        {/if}
        {#if checked.keptGone > 0}
          <p>
            In {plural(checked.keptGone, "group")} of duplicates, the copies left out are gone or changed since they
            were compared, so the files selected from {checked.keptGone === 1 ? "it" : "them"} will be kept, not moved.
          </p>
        {/if}
        <ul class="small">
          {#each checked.examples as path (path)}
            <li class="selectable"><PathText {path} /></li>
          {/each}
        </ul>
      </div>
    {/if}
    {#if recheckGroups > 0}
      <p class="muted small" id="{id}-recheck">
        The copies left out of {plural(recheckGroups, "group")} of duplicates look unchanged since they were compared,
        but that's provisional: they're read again before anything moves, and a file whose copy can't be confirmed is
        kept.
      </p>
    {/if}
    {#if unchecked}
      <p class="muted small" id="{id}-unchecked">
        Couldn't check {lostGroups > 0 ? "every" : "whether any"} group of duplicates in time. Copies left out are
        still read before anything moves, and a file whose copy can't be confirmed is kept, but make sure you keep a
        copy of each group you select from entirely.
      </p>
    {/if}
    {#if summary.folders > 0}
      <p class="warning" id="{id}-folders">
        Folders go with everything in them{filtered ? ", including files the filter hides" : ""}.
      </p>
    {/if}
    <p class="muted small" id="{id}-skip">
      Items that can't go to the {trashName} — on network or removable drives, or too large — are skipped and left
      where they are.
    </p>
    {#if privileged}
      <p class="warning" id="{id}-admin">SpaceBadger has administrator rights, so protected files can be moved too.</p>
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
      <span class="check" class:on={matches} aria-hidden="true">✓</span>
      {#if matches}<span class="sr-only">Matches</span>{/if}
    </label>
    <div class="buttons">
      <!-- Always present, so the dialog doesn't change height as it counts down. -->
      <span class="muted small hint" role={busy ? "status" : undefined}>{missing}</span>
      {#if rechecking}
        <button
          type="button"
          disabled={stopped}
          title="Files whose copy isn't confirmed yet are kept; the rest still move"
          onclick={() => {
            stopped = true;
            onstoprecheck?.();
          }}>Stop checking</button
        >
      {:else}
        <button type="button" disabled={busy} onclick={oncancel}>Cancel</button>
      {/if}
      <button type="button" class="danger filled" disabled={!ready} onclick={confirm}>
        Move to the {trashName}
      </button>
    </div>
  {/if}
</dialog>

<style>
  dialog {
    width: min(480px, calc(100vw - 32px));
    max-height: calc(100vh - 32px);
    overflow-y: auto;
    box-sizing: border-box;
    padding: 16px 20px;
    border: 1px solid var(--line);
    border-radius: 8px;
    background: var(--panel);
    color: var(--fg);
    box-shadow: 0 8px 32px rgb(0 0 0 / 0.35);
    overflow-wrap: anywhere;
  }
  dialog::backdrop {
    background: rgb(0 0 0 / 0.35);
  }
  dialog:focus {
    outline: none;
  }
  h2 {
    margin: 0 0 var(--space-2);
    font-size: 16px;
  }
  p {
    margin: 0 0 10px;
  }
  .samples,
  .failures {
    margin: 0 0 var(--space-3);
    padding: 6px 10px;
    border: 1px solid var(--line);
    border-radius: var(--radius);
    background: var(--bg);
  }
  .samples {
    max-height: 200px;
    overflow-y: auto;
  }
  .failures {
    max-height: 240px;
    overflow-y: auto;
    list-style: none;
    font-size: var(--fs-sm);
  }
  .failures li + li {
    margin-top: 6px;
  }
  .warning {
    color: var(--danger);
    font-weight: 600;
  }
  div.warning {
    margin: 1em 0;
  }
  div.warning p {
    margin: 0;
  }
  div.warning ul {
    margin: 4px 0 0;
    padding-left: 1.2em;
    font-weight: normal;
  }
  .type {
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }
  .type input {
    flex: 1;
    user-select: text;
  }
  .check {
    width: 1em;
    color: var(--accent);
    font-weight: 700;
    visibility: hidden;
  }
  .check.on {
    visibility: visible;
  }
  .buttons {
    display: flex;
    justify-content: flex-end;
    align-items: center;
    gap: var(--space-2);
    margin-top: 14px;
  }
  .hint {
    margin-right: auto;
  }
</style>
