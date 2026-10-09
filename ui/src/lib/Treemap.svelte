<script lang="ts">
  // Canvas treemap. Pulls a layout from the backend whenever its inputs
  // change; at most one request is in flight, and changes that arrive
  // meanwhile are folded into one follow-up request (natural backpressure).
  import { nodeDetails, requestLayout } from "./api";
  import { formatBytes, plural } from "./format";
  import {
    decodeLayout,
    hitTest,
    KIND_FILE,
    KIND_FOLDER,
    KIND_OTHER,
    RF_HEADER,
    RF_INCOMPLETE,
    type DecodedLayout,
  } from "./layoutWire";
  import type { Metric } from "./protocol/Metric";
  import type { NodeDetails } from "./protocol/NodeDetails";
  import { sameSelection, type ItemKind, type MenuRequest, type Selection } from "./selection";

  interface Props {
    generation: number;
    view: number;
    metric: Metric;
    /** Active filename search; only matching files are drawn. */
    search: number | null;
    /** Scan revision; a change triggers a new layout. */
    revision: number;
    selection: Selection | null;
    onselect: (selection: Selection | null) => void;
    /** Double-click or Enter on a folder (or merged small items). */
    onopen: (node: number) => void;
    /** Backspace: go to the parent folder. */
    onup: () => void;
    onlayout?: (layout: DecodedLayout) => void;
    /** Right-click, the Menu key, or Shift+F10. */
    onmenu?: (request: MenuRequest) => void;
  }

  let {
    generation,
    view,
    metric,
    search,
    revision,
    selection,
    onselect,
    onopen,
    onup,
    onlayout,
    onmenu,
  }: Props = $props();

  let container: HTMLDivElement;
  let canvas: HTMLCanvasElement;
  let width = $state(0);
  let height = $state(0);
  let layout = $state.raw<DecodedLayout | null>(null);
  let error = $state<string | null>(null);
  let hover = $state<{ index: number; x: number; y: number } | null>(null);
  let hoverDetails = $state.raw<NodeDetails | null>(null);
  let dark = $state(window.matchMedia("(prefers-color-scheme: dark)").matches);

  // ---- layout requests -------------------------------------------------

  let nextRequest = 1;
  let inFlight = false;
  let again = false;

  $effect(() => {
    // Dependencies that change the layout.
    void [generation, view, metric, search, revision, width, height];
    fetchLayout();
  });

  async function fetchLayout() {
    if (inFlight) {
      again = true;
      return;
    }
    inFlight = true;
    try {
      do {
        again = false;
        if (width < 1 || height < 1) break;
        const want = { generation, view, metric, search };
        const request = nextRequest++;
        try {
          const reply = decodeLayout(
            await requestLayout({ ...want, request, width, height }),
          );
          // Drop replies for a view, metric, search, or scan no longer shown.
          if (
            want.generation === generation &&
            want.view === view &&
            want.metric === metric &&
            want.search === search &&
            reply.generation === generation
          ) {
            layout = reply;
            error = null;
            onlayout?.(reply);
          }
        } catch (e) {
          if (want.generation === generation && want.search === search) error = String(e);
        }
      } while (again);
    } finally {
      inFlight = false;
    }
  }

  // ---- sizing and theme ------------------------------------------------

  $effect(() => {
    const observer = new ResizeObserver(([entry]) => {
      if (!entry) return;
      width = Math.floor(entry.contentRect.width);
      height = Math.floor(entry.contentRect.height);
    });
    observer.observe(container);
    const media = window.matchMedia("(prefers-color-scheme: dark)");
    const onTheme = () => (dark = media.matches);
    media.addEventListener("change", onTheme);
    return () => {
      observer.disconnect();
      media.removeEventListener("change", onTheme);
    };
  });

  // ---- drawing ---------------------------------------------------------

  const HUES = [212, 28, 145, 270, 188, 340, 50, 105];

  function fillFor(l: DecodedLayout, i: number): string {
    const kind = l.kind[i];
    if (kind === KIND_OTHER) return dark ? "#4b505b" : "#c4c8cf";
    const hue = HUES[(l.depth[i]! - 1) % HUES.length];
    if (kind === KIND_FOLDER) return dark ? `hsl(${hue} 28% 30%)` : `hsl(${hue} 30% 62%)`;
    return dark ? `hsl(${hue} 38% 42%)` : `hsl(${hue} 45% 80%)`;
  }

  function fit(ctx: CanvasRenderingContext2D, text: string, max: number): string {
    if (max <= 8) return "";
    if (ctx.measureText(text).width <= max) return text;
    let lo = 0;
    let hi = text.length;
    while (lo < hi) {
      const mid = (lo + hi + 1) >> 1;
      if (ctx.measureText(text.slice(0, mid) + "…").width <= max) lo = mid;
      else hi = mid - 1;
    }
    return lo === 0 ? "" : text.slice(0, lo) + "…";
  }

  function label(ctx: CanvasRenderingContext2D, text: string, x: number, y: number, max: number) {
    const fitted = fit(ctx, text, max);
    if (fitted) ctx.fillText(fitted, x, y);
  }

  function selectedIndex(l: DecodedLayout | null, sel: Selection | null): number {
    if (!l || !sel) return -1;
    for (let i = 0; i < l.count; i++) {
      if (l.node[i] === sel.node && (l.kind[i] === KIND_OTHER) === sel.other) return i;
    }
    return -1;
  }

  function draw() {
    const ctx = canvas?.getContext("2d");
    if (!ctx) return;
    const dpr = window.devicePixelRatio || 1;
    if (canvas.width !== Math.round(width * dpr) || canvas.height !== Math.round(height * dpr)) {
      canvas.width = Math.round(width * dpr);
      canvas.height = Math.round(height * dpr);
    }
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, width, height);
    const l = layout;
    if (!l) return;

    ctx.font = "11px system-ui, -apple-system, 'Segoe UI', sans-serif";
    ctx.textBaseline = "alphabetic";
    const text = dark ? "#eceae6" : "#1b1e25";
    const gap = dark ? "#1b1e25" : "#f6f6f4";

    for (let i = 0; i < l.count; i++) {
      const x = l.x[i]!;
      const y = l.y[i]!;
      const w = l.w[i]!;
      const h = l.h[i]!;
      const kind = l.kind[i]!;
      ctx.fillStyle = gap;
      ctx.fillRect(x, y, w, h);
      ctx.fillStyle = fillFor(l, i);
      if (w > 2 && h > 2) ctx.fillRect(x + 0.5, y + 0.5, w - 1, h - 1);
      else ctx.fillRect(x, y, w, h);

      if (kind === KIND_FOLDER && l.rflags[i]! & RF_INCOMPLETE) {
        ctx.strokeStyle = dark ? "#d9a441" : "#a86a00";
        ctx.setLineDash([3, 3]);
        ctx.strokeRect(x + 1, y + 1, w - 2, h - 2);
        ctx.setLineDash([]);
      }

      ctx.save();
      ctx.beginPath();
      ctx.rect(x, y, w, h);
      ctx.clip();
      ctx.fillStyle = text;
      const name = l.labels.get(i);
      const size = formatBytes(l.weight[i]!);
      if (kind === KIND_FOLDER && l.rflags[i]! & RF_HEADER && name !== undefined) {
        const sizeWidth = ctx.measureText(size).width;
        if (w - 8 > sizeWidth + 40) {
          label(ctx, name, x + 4, y + 12, w - 14 - sizeWidth);
          ctx.fillText(size, x + w - 4 - sizeWidth, y + 12);
        } else {
          label(ctx, name, x + 4, y + 12, w - 8);
        }
      } else if (kind === KIND_FILE && name !== undefined) {
        label(ctx, name, x + 4, y + 12, w - 8);
        if (h >= 28) label(ctx, size, x + 4, y + 25, w - 8);
      } else if (kind === KIND_OTHER && w >= 60 && h >= 14) {
        label(ctx, `${plural(l.items[i]!, "small item")}`, x + 4, y + 12, w - 8);
      }
      ctx.restore();
    }

    const hi = hover ? hover.index : -1;
    if (hi >= 0 && hi < l.count) {
      ctx.strokeStyle = dark ? "rgba(255,255,255,0.7)" : "rgba(0,0,0,0.55)";
      ctx.lineWidth = 1;
      ctx.strokeRect(l.x[hi]! + 0.5, l.y[hi]! + 0.5, l.w[hi]! - 1, l.h[hi]! - 1);
    }
    const si = selectedIndex(l, selection);
    if (si >= 0) {
      ctx.lineWidth = 3;
      ctx.strokeStyle = dark ? "#ffffff" : "#000000";
      ctx.strokeRect(l.x[si]! + 1.5, l.y[si]! + 1.5, Math.max(0, l.w[si]! - 3), Math.max(0, l.h[si]! - 3));
      ctx.lineWidth = 1;
      ctx.strokeStyle = dark ? "#000000" : "#ffffff";
      ctx.strokeRect(l.x[si]! + 3.5, l.y[si]! + 3.5, Math.max(0, l.w[si]! - 7), Math.max(0, l.h[si]! - 7));
    }
  }

  let frame = 0;
  $effect(() => {
    void [layout, selection, hover, dark, width, height];
    cancelAnimationFrame(frame);
    frame = requestAnimationFrame(draw);
    return () => cancelAnimationFrame(frame);
  });

  // ---- interaction -----------------------------------------------------

  function selectionAt(i: number): Selection | null {
    const l = layout;
    if (!l || i < 0) return null;
    return { node: l.node[i]!, other: l.kind[i] === KIND_OTHER };
  }

  function kindAt(i: number): ItemKind | null {
    const kind = i >= 0 ? layout?.kind[i] : undefined;
    if (kind === KIND_FILE) return "file";
    if (kind === KIND_FOLDER) return "folder";
    if (kind === KIND_OTHER) return "other";
    return null;
  }

  function point(e: MouseEvent): [number, number] {
    const r = canvas.getBoundingClientRect();
    return [e.clientX - r.left, e.clientY - r.top];
  }

  let hoverTimer: ReturnType<typeof setTimeout> | undefined;

  function onMove(e: MouseEvent) {
    const l = layout;
    if (!l) return;
    const [x, y] = point(e);
    const index = hitTest(l, x, y);
    const changed = !hover || hover.index !== index;
    hover = index >= 0 ? { index, x, y } : null;
    if (changed) {
      hoverDetails = null;
      clearTimeout(hoverTimer);
      if (index >= 0 && l.kind[index] !== KIND_OTHER) {
        const node = l.node[index]!;
        const gen = generation;
        hoverTimer = setTimeout(() => {
          nodeDetails(gen, node).then(
            (d) => {
              if (hover && layout?.node[hover.index] === d.node && d.generation === generation) {
                hoverDetails = d;
              }
            },
            () => {},
          );
        }, 120);
      }
    }
  }

  function onLeave() {
    clearTimeout(hoverTimer);
    hover = null;
    hoverDetails = null;
  }

  function onClick(e: MouseEvent) {
    const l = layout;
    if (!l) return;
    const [x, y] = point(e);
    const next = selectionAt(hitTest(l, x, y));
    if (!sameSelection(next, selection)) onselect(next);
    container.focus();
  }

  function open(sel: Selection | null) {
    if (!sel) return;
    const l = layout;
    const i = selectedIndex(l, sel);
    // Files never open: double-clicking must not launch anything.
    if (!l || i < 0 || l.kind[i] === KIND_FILE) return;
    if (sel.node !== view) onopen(sel.node);
  }

  function onDblClick(e: MouseEvent) {
    const l = layout;
    if (!l) return;
    const [x, y] = point(e);
    open(selectionAt(hitTest(l, x, y)));
  }

  /** When the menu key or Shift+F10 last opened the menu. */
  let keyMenuAt = -Infinity;

  function onContextMenu(e: MouseEvent) {
    e.preventDefault();
    // Some webviews also send a contextmenu event for the menu key or
    // Shift+F10. Keyboard-made events have no pointer type.
    if ((e as PointerEvent).pointerType === "") {
      if (performance.now() - keyMenuAt > 500) menuFromKeyboard();
      return;
    }
    const l = layout;
    if (!l) return;
    const [x, y] = point(e);
    const i = hitTest(l, x, y);
    const target = selectionAt(i);
    // Right-click selects what it points at, as in file managers.
    if (target && !sameSelection(target, selection)) onselect(target);
    onLeave();
    container.focus();
    onmenu?.({ selection: target, kind: kindAt(i), x: e.clientX, y: e.clientY });
  }

  /** Opens the menu for the selection, near its top-left corner. */
  function menuFromKeyboard() {
    keyMenuAt = performance.now();
    const l = layout;
    const r = canvas.getBoundingClientRect();
    const i = selectedIndex(l, selection);
    if (!l || i < 0) {
      onmenu?.({ selection: null, kind: null, x: r.left + 8, y: r.top + 8 });
      return;
    }
    onmenu?.({
      selection,
      kind: kindAt(i),
      x: r.left + l.x[i]! + Math.min(l.w[i]! / 2, 24),
      y: r.top + l.y[i]! + Math.min(l.h[i]! / 2, 16),
    });
  }

  /** Nearest rectangle at the same depth in the arrow's direction. */
  function neighbour(dx: number, dy: number): number {
    const l = layout;
    if (!l || l.count === 0) return -1;
    const cur = selectedIndex(l, selection);
    if (cur < 0) return 0;
    const cx = l.x[cur]! + l.w[cur]! / 2;
    const cy = l.y[cur]! + l.h[cur]! / 2;
    const depth = l.depth[cur];
    let best = -1;
    let bestScore = Infinity;
    for (let i = 0; i < l.count; i++) {
      if (i === cur || l.depth[i] !== depth) continue;
      const ox = l.x[i]! + l.w[i]! / 2 - cx;
      const oy = l.y[i]! + l.h[i]! / 2 - cy;
      const along = ox * dx + oy * dy;
      if (along <= 0) continue;
      const across = Math.abs(ox * dy - oy * dx);
      const score = along + 2 * across;
      if (score < bestScore) {
        bestScore = score;
        best = i;
      }
    }
    return best;
  }

  function onKey(e: KeyboardEvent) {
    const moves: Record<string, [number, number]> = {
      ArrowLeft: [-1, 0],
      ArrowRight: [1, 0],
      ArrowUp: [0, -1],
      ArrowDown: [0, 1],
    };
    const move = moves[e.key];
    if (move && !e.altKey) {
      const i = neighbour(move[0], move[1]);
      if (i >= 0) onselect(selectionAt(i));
    } else if (e.key === "Enter") {
      open(selection);
    } else if (e.key === "Backspace" || (e.key === "ArrowUp" && e.altKey)) {
      onup();
    } else if (e.key === "Escape") {
      onselect(null);
    } else if (e.key === "ContextMenu" || (e.key === "F10" && e.shiftKey)) {
      menuFromKeyboard();
    } else {
      return;
    }
    e.preventDefault();
  }

  const tooltip = $derived.by(() => {
    const l = layout;
    if (!hover || !l || hover.index >= l.count) return null;
    const i = hover.index;
    if (l.kind[i] === KIND_OTHER) {
      return {
        title: plural(l.items[i]!, "small item"),
        lines: [`${formatBytes(l.weight[i]!)} combined`, "Too small to draw individually"],
      };
    }
    const d = hoverDetails;
    const name = d?.name ?? l.labels.get(i) ?? "";
    const lines: string[] = [];
    if (d) {
      lines.push(d.path);
      const allocated = d.allocated === null ? "unknown" : formatBytes(d.allocated);
      lines.push(`Allocated ${allocated} · Logical ${formatBytes(d.logical)}`);
    } else {
      lines.push(formatBytes(l.weight[i]!));
    }
    return { title: name, lines };
  });
</script>

<!-- role="application" is the ARIA role for a custom keyboard-driven widget;
     Svelte's checker treats it as non-interactive. -->
<!-- svelte-ignore a11y_no_noninteractive_tabindex, a11y_no_noninteractive_element_interactions -->
<div
  class="treemap"
  bind:this={container}
  tabindex="0"
  role="application"
  aria-label="Treemap. Arrow keys move the selection, Enter opens a folder, Backspace goes up, Shift+F10 shows actions, Escape clears the selection."
  onmousemove={onMove}
  onmouseleave={onLeave}
  onclick={onClick}
  ondblclick={onDblClick}
  oncontextmenu={onContextMenu}
  onkeydown={onKey}
>
  <canvas bind:this={canvas} style:width="{width}px" style:height="{height}px" aria-hidden="true"
  ></canvas>
  {#if tooltip && hover}
    <div
      class="tooltip"
      style:left="{Math.min(hover.x + 14, Math.max(0, width - 320))}px"
      style:top="{hover.y + 18 > height - 60 ? hover.y - 64 : hover.y + 18}px"
    >
      <strong>{tooltip.title}</strong>
      {#each tooltip.lines as line, i (i)}
        <div>{line}</div>
      {/each}
    </div>
  {/if}
  {#if error}
    <p class="error" role="alert">Couldn't draw the map: {error}</p>
  {/if}
</div>

<style>
  .treemap {
    position: relative;
    width: 100%;
    height: 100%;
    overflow: hidden;
  }
  .treemap:focus {
    outline: none;
  }
  .treemap:focus-visible {
    box-shadow: inset 0 0 0 2px var(--accent);
  }
  canvas {
    display: block;
  }
  .tooltip {
    position: absolute;
    max-width: 300px;
    padding: 6px 8px;
    border-radius: 4px;
    background: var(--panel);
    color: var(--fg);
    border: 1px solid var(--line);
    box-shadow: 0 2px 8px rgb(0 0 0 / 0.25);
    pointer-events: none;
    font-size: 12px;
    overflow-wrap: anywhere;
  }
  .error {
    position: absolute;
    left: 8px;
    bottom: 8px;
    margin: 0;
    color: var(--danger);
  }
</style>
