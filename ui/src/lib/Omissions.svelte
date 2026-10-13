<script lang="ts">
  // What the scan left out of the totals, split into what it couldn't read
  // and what it skips on purpose, with a plain explanation for each reason.
  // Meant to fill a side-panel tab: it takes the remaining height and
  // scrolls inside.
  import { formatCount, plural } from "./format";
  import type { OmissionGroup } from "./protocol/OmissionGroup";
  import type { OmissionReason } from "./protocol/OmissionReason";

  interface Props {
    groups: OmissionGroup[];
    /** Focus the heading when shown, e.g. when its tab opens. */
    autofocus?: boolean;
    /** Shows a close button when given. */
    onclose?: () => void;
    /** Offers to restart with administrator rights next to the reasons it
     * would help with, when given. */
    onelevate?: () => void;
  }

  let { groups, autofocus = false, onclose, onelevate }: Props = $props();

  const id = $props.id();

  interface Reason {
    title: string;
    /** What it means, in one line. */
    explain: string;
    /** What the user can do about it, if anything. */
    advice?: string;
  }

  const reasons: Record<OmissionReason, Reason> = {
    permissionDenied: {
      title: "Permission denied",
      explain: "The system didn't let SpaceBadger open these.",
      advice: "Running SpaceBadger as administrator usually lets it read these.",
    },
    vanished: {
      title: "Removed during the scan",
      explain: "They were deleted or moved while the scan was running.",
      advice: "Rescan to see the current sizes.",
    },
    disconnected: {
      title: "Drive or share disconnected",
      explain: "The device or network share went away during the scan.",
      advice: "Reconnect it and rescan.",
    },
    unavailable: {
      title: "Size unavailable",
      explain: "The system couldn't report their size.",
    },
    other: {
      title: "Other errors",
      explain: "Reading them failed; the message is shown with each one.",
    },
    symlink: {
      title: "Symbolic links",
      explain: "Links point somewhere else, so they aren't followed; what they point to is counted where it really is.",
    },
    mountPoint: {
      title: "Junctions and mounted drives",
      explain: "These lead into other folders or drives. Following them could count space twice.",
      advice: "Scan that folder or drive on its own to see it.",
    },
    specialFile: {
      title: "Special files",
      explain: "Sockets, devices, and pipes don't hold file data.",
    },
    unknownReparse: {
      title: "Unrecognized redirections",
      explain: "These redirect somewhere in a way SpaceBadger doesn't understand, so it doesn't follow them.",
    },
  };

  const unreadable = $derived(groups.filter((g) => !g.policy));
  const skipped = $derived(groups.filter((g) => g.policy));

  let heading: HTMLHeadingElement;
  /** Index key of the sample just copied, and whether it worked. */
  let copied = $state<{ key: string; ok: boolean } | null>(null);
  let copiedTimer: ReturnType<typeof setTimeout> | undefined;

  /** Moves focus to the heading. */
  export function focus() {
    heading.focus();
  }

  $effect(() => {
    if (autofocus) heading.focus();
    return () => clearTimeout(copiedTimer);
  });

  async function copy(key: string, path: string) {
    let ok = true;
    try {
      await navigator.clipboard.writeText(path);
    } catch {
      ok = false;
    }
    copied = { key, ok };
    clearTimeout(copiedTimer);
    copiedTimer = setTimeout(() => (copied = null), 1500);
  }
</script>

{#snippet group(g: OmissionGroup)}
  {@const r = reasons[g.reason]}
  <div class="group">
    <div class="reason"><strong>{r.title}</strong> <span class="muted">{formatCount(g.count)}</span></div>
    <p class="small">{r.explain}{r.advice ? ` ${r.advice}` : ""}</p>
    {#if g.reason === "permissionDenied" && onelevate}
      <button type="button" class="elevate" onclick={onelevate}>Restart as administrator…</button>
    {/if}
    {#if g.samples.length > 0}
      <details>
        <summary class="small">{g.count > 1 ? "Show paths" : "Show path"}</summary>
        <ul>
          {#each g.samples as sample, i (i)}
            {@const key = `${g.reason}-${i}`}
            <li>
              <span class="path selectable">
                {sample.path}
                {#if sample.detail}<span class="muted"> {sample.detail}</span>{/if}
              </span>
              <button
                type="button"
                class="icon-button"
                title="Copy path"
                aria-label="Copy path"
                onclick={() => copy(key, sample.path)}
              >
                {#if copied?.key === key}{copied.ok ? "✓" : "✕"}{:else}⧉{/if}
              </button>
            </li>
          {/each}
          {#if g.count > g.samples.length}
            <li class="muted">…and {formatCount(g.count - g.samples.length)} more</li>
          {/if}
        </ul>
      </details>
    {/if}
  </div>
{/snippet}

<section class="omissions" aria-labelledby="{id}-title">
  <header class="panel-head">
    <h2 id="{id}-title" tabindex="-1" bind:this={heading}>Not counted</h2>
    {#if onclose}
      <button type="button" class="icon-button close" onclick={onclose} aria-label="Close">✕</button>
    {/if}
  </header>
  <div class="body">
    {#if groups.length === 0}
      <p class="muted">Everything found was counted.</p>
    {:else}
      <p>These weren't counted in the sizes.</p>
      {#if unreadable.length > 0}
        <h3>Couldn't read <span class="muted">{plural(unreadable.reduce((n, g) => n + g.count, 0), "item")}</span></h3>
        {#each unreadable as g (g.reason)}{@render group(g)}{/each}
      {/if}
      {#if skipped.length > 0}
        <h3>Skipped on purpose <span class="muted">{plural(skipped.reduce((n, g) => n + g.count, 0), "item")}</span></h3>
        {#each skipped as g (g.reason)}{@render group(g)}{/each}
      {/if}
    {/if}
  </div>
  <div class="sr-only" role="status">{#if copied}{copied.ok ? "Path copied" : "Couldn't copy the path"}{/if}</div>
</section>

<style>
  .omissions {
    display: flex;
    flex-direction: column;
    flex: 1;
    min-height: 0;
    overflow-wrap: anywhere;
  }
  h2:focus {
    outline: none;
  }
  h2:focus-visible {
    outline: 2px solid var(--accent);
  }
  .close {
    margin-left: auto;
  }
  .body {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    padding: 0 var(--space-3) var(--space-3);
  }
  p {
    margin: 0 0 var(--space-2);
  }
  h3 {
    margin: var(--space-3) 0 var(--space-2);
    font-size: 13px;
    font-weight: 600;
  }
  h3 .muted {
    font-weight: normal;
    font-size: var(--fs-sm);
  }
  .group {
    margin-bottom: 10px;
  }
  .elevate {
    margin-bottom: var(--space-2);
    font-size: var(--fs-sm);
  }
  .reason {
    display: flex;
    justify-content: space-between;
    gap: var(--space-2);
  }
  .reason .muted {
    font-variant-numeric: tabular-nums;
  }
  .group p {
    margin: 2px 0 var(--space-1);
  }
  summary {
    cursor: pointer;
    color: var(--muted);
  }
  ul {
    margin: var(--space-1) 0 0;
    padding: 0;
    list-style: none;
    font-size: var(--fs-sm);
  }
  li {
    display: flex;
    align-items: flex-start;
    gap: var(--space-1);
  }
  li + li {
    margin-top: 2px;
  }
  .path {
    flex: 1;
    min-width: 0;
  }
</style>
