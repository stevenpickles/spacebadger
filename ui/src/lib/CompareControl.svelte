<script lang="ts">
  // The toolbar's "Compare…" control: lists the earlier scans of this
  // folder (saved when each scan completed), newest first, and turns
  // comparing with one on or off.
  import { formatDay, formatDelta, formatWhen } from "./compare";
  import { formatBytes, plural } from "./format";
  import type { SnapshotInfo } from "./protocol/SnapshotInfo";

  interface Props {
    snapshots: SnapshotInfo[];
    /** The earlier scan compared with, if comparing. */
    current: SnapshotInfo | null;
    /** Logical bytes in the current scan, for each scan's difference. */
    now: number;
    /** Turning a comparison on is under way. */
    busy: boolean;
    onchoose: (snapshot: SnapshotInfo) => void;
    onstop: () => void;
  }

  let { snapshots, current, now, busy, onchoose, onstop }: Props = $props();

  const listId = $props.id();

  /** Each scan's time, with seconds where two would otherwise read alike. */
  const labels = $derived.by(() => {
    const plain = snapshots.map((s) => formatWhen(s.createdMs));
    return snapshots.map((s, i) =>
      plain.indexOf(plain[i]!) !== plain.lastIndexOf(plain[i]!) ? formatWhen(s.createdMs, true) : plain[i]!,
    );
  });
  const currentWhen = $derived.by(() => {
    if (!current) return "";
    const i = snapshots.findIndex((s) => s.id === current.id);
    return i >= 0 ? labels[i]! : formatWhen(current.createdMs);
  });
  let open = $state(false);
  let root: HTMLDivElement;

  function choose(s: SnapshotInfo) {
    open = false;
    onchoose(s);
  }

  function onWindowPointer(e: PointerEvent) {
    if (!root.contains(e.target as Node)) open = false;
  }

  function onKey(e: KeyboardEvent) {
    if (e.key === "Escape" && open) {
      e.stopPropagation();
      open = false;
      root.querySelector<HTMLButtonElement>("button")?.focus();
    }
  }
</script>

<svelte:window onpointerdown={open ? onWindowPointer : undefined} />

<!-- svelte-ignore a11y_no_static_element_interactions -->
<div class="compare" bind:this={root} onkeydown={onKey}>
  <button
    type="button"
    class:on={current !== null}
    aria-expanded={open}
    aria-controls={listId}
    disabled={busy}
    aria-label={current ? `Comparing with the scan of ${currentWhen}` : undefined}
    title={current
      ? `Comparing with the scan of ${currentWhen}. Click to choose another earlier scan or stop comparing`
      : "Compare this scan with an earlier scan of the same folder: what grew, shrank, or is new"}
    onclick={() => (open = !open)}
  >
    {#if busy}Comparing…{:else if current}{formatDay(current.createdMs)}{:else}Compare…{/if}
  </button>
  {#if open}
    <div class="popover" id={listId} role="group" aria-label="Earlier scans">
      {#if current}
        <button type="button" class="stop" onclick={() => ((open = false), onstop())}>Stop comparing</button>
      {/if}
      <p class="muted small">Compare with an earlier scan of this folder:</p>
      <ul>
        {#each snapshots as s, i (s.id)}
          <li>
            <button type="button" class="pick" class:chosen={s.id === current?.id} onclick={() => choose(s)}>
              <span class="when">{labels[i]}</span>
              <span class="muted small">
                {formatBytes(s.logical)} in {plural(s.files, "file")} · {formatDelta(s.logical, now)} since
              </span>
            </button>
          </li>
        {/each}
      </ul>
    </div>
  {/if}
</div>

<style>
  .compare {
    position: relative;
    display: flex;
    align-items: center;
    gap: 2px;
  }
  button.on {
    border-color: var(--accent);
    background: var(--selected);
  }
  .popover {
    position: absolute;
    top: calc(100% + 6px);
    left: 0;
    z-index: 20;
    width: 300px;
    max-height: 60vh;
    overflow-y: auto;
    padding: var(--space-2);
    border: 1px solid var(--line);
    border-radius: 6px;
    background: var(--bg);
    box-shadow: 0 6px 20px rgb(0 0 0 / 0.2);
  }
  .popover p {
    margin: 0 0 var(--space-2);
  }
  ul {
    margin: 0;
    padding: 0;
    list-style: none;
  }
  .pick {
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    width: 100%;
    padding: var(--space-1) var(--space-2);
    border: none;
    border-radius: var(--radius);
    background: none;
    text-align: left;
  }
  .pick:hover,
  .pick:focus-visible {
    background: var(--hover);
  }
  .pick.chosen {
    background: var(--selected);
  }
  .when {
    font-weight: 600;
  }
  .stop {
    margin-bottom: var(--space-2);
  }
</style>
