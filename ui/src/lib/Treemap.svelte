<script lang="ts">
  // Canvas treemap. Pulls a layout from the backend whenever its inputs
  // change; at most one request is in flight, and changes that arrive
  // meanwhile are folded into one follow-up request (natural backpressure).
  import { nodeDetails, requestLayout } from "./api";
  import {
    depthColor,
    typeModeFolderColor,
    otherSmallColor,
    typeColor,
    type ColorMode,
  } from "./colors";
  import { formatBytes, plural } from "./format";
  import {
    decodeLayout,
    FILE_TYPES,
    hitTest,
    KIND_FILE,
    KIND_FOLDER,
    KIND_OTHER,
    RF_HEADER,
    RF_INCOMPLETE,
    type DecodedLayout,
  } from "./layoutWire";
  import { Samples } from "./perf";
  import type { Metric } from "./protocol/Metric";
  import type { NodeDetails } from "./protocol/NodeDetails";
  import {
    clickPick,
    siblingRange,
    toggles,
    type ItemKind,
    type MenuRequest,
    type Pick,
    type Selection,
  } from "./selection";

  interface Props {
    generation: number;
    view: number;
    metric: Metric;
    colors: ColorMode;
    /** Active filename search; only matching files are drawn. */
    search: number | null;
    /** Scan revision; a change triggers a new layout. */
    revision: number;
    /** The focused item: the one with details, and where arrow keys start. */
    selection: Selection | null;
    /** Selected files and folders (actions apply to these). */
    selected: Set<number>;
    /** Where Shift ranges start. */
    anchor: number | null;
    /** A click or key changed the selection; `null` clears it. */
    onpick: (pick: Pick | null) => void;
    /** The Delete key. */
    ondelete?: () => void;
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
    colors,
    search,
    revision,
    selection,
    selected,
    anchor,
    onpick,
    ondelete,
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

  // ---- diagnostics (toggle with ` while the map has focus) ---------------

  const stats = { layout: new Samples(), draw: new Samples(), input: new Samples() };
  /** When the last click or key arrived, until the next frame is drawn. */
  let inputAt: number | null = null;
  let showPerf = $state(false);
  let perfText = $state("");

  $effect(() => {
    if (!showPerf) return;
    const update = () => {
      perfText = [
        `layout round trip: ${stats.layout.summary()}`,
        `draw: ${stats.draw.summary()}`,
        `input to frame: ${stats.input.summary()}`,
      ].join("\n");
    };
    update();
    const timer = setInterval(update, 500);
    return () => clearInterval(timer);
  });

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
          const started = performance.now();
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
            stats.layout.add(performance.now() - started);
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

  function fillFor(l: DecodedLayout, i: number): string {
    const kind = l.kind[i];
    if (kind === KIND_OTHER) return otherSmallColor(dark);
    if (colors === "type") {
      return kind === KIND_FOLDER ? typeModeFolderColor(dark) : typeColor(l.ftype[i]!, dark);
    }
    return depthColor(l.depth[i]!, kind === KIND_FOLDER, dark);
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
    const started = performance.now();
    paint();
    const now = performance.now();
    stats.draw.add(now - started);
    if (inputAt !== null) {
      stats.input.add(now - inputAt);
      inputAt = null;
    }
  }

  function paint() {
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

    if (selected.size > 0) {
      // Selected items are tinted, with everything drawn inside them.
      ctx.fillStyle = dark ? "rgb(122 167 255 / 0.38)" : "rgb(37 99 201 / 0.3)";
      ctx.strokeStyle = dark ? "#7aa7ff" : "#2563c9";
      ctx.lineWidth = 2;
      for (let i = 0; i < l.count; i++) {
        if (l.kind[i] === KIND_OTHER || !selected.has(l.node[i]!)) continue;
        ctx.fillRect(l.x[i]!, l.y[i]!, l.w[i]!, l.h[i]!);
        if (l.w[i]! > 4 && l.h[i]! > 4) {
          ctx.strokeRect(l.x[i]! + 1, l.y[i]! + 1, l.w[i]! - 2, l.h[i]! - 2);
        }
      }
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
    void [layout, selection, selected, hover, dark, colors, width, height];
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

  /** Replaces the selection with the item at rectangle `i`. */
  function only(i: number): Pick | null {
    const focus = selectionAt(i);
    return focus && { focus, nodes: focus.other ? [] : [focus.node], mode: "replace", keepAnchor: false };
  }

  function onClick(e: MouseEvent) {
    inputAt = performance.now();
    const l = layout;
    if (!l) return;
    const [x, y] = point(e);
    const i = hitTest(l, x, y);
    const focus = selectionAt(i);
    if (focus) onpick(clickPick(focus, e, e.shiftKey ? siblingRange(l, anchor, i) : null));
    else if (!toggles(e) && !e.shiftKey) onpick(null);
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
    // Right-click selects what it points at, as in file managers, but keeps
    // a selection it points into so the menu acts on all of it.
    if (target && !target.other && selected.has(target.node)) {
      onpick({ focus: target, nodes: [], mode: "add", keepAnchor: true });
    } else if (target) {
      onpick(only(i));
    }
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

  /** Selects every file and folder drawn directly in the view. */
  function selectAll() {
    const l = layout;
    if (!l) return;
    const nodes: number[] = [];
    for (let i = 0; i < l.count; i++) {
      if (l.depth[i] === 1 && l.kind[i] !== KIND_OTHER) nodes.push(l.node[i]!);
    }
    if (nodes.length === 0) return;
    const focus = selection ?? { node: nodes[0]!, other: false };
    onpick({ focus, nodes, mode: "replace", keepAnchor: true });
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
    if (e.key === "`") {
      showPerf = !showPerf;
      e.preventDefault();
      return;
    }
    inputAt = performance.now();
    const moves: Record<string, [number, number]> = {
      ArrowLeft: [-1, 0],
      ArrowRight: [1, 0],
      ArrowUp: [0, -1],
      ArrowDown: [0, 1],
    };
    const move = moves[e.key];
    if (move && !e.altKey) {
      const i = neighbour(move[0], move[1]);
      if (i >= 0) onpick(only(i));
    } else if (e.key === "Enter") {
      open(selection);
    } else if (e.key === "Backspace" || (e.key === "ArrowUp" && e.altKey)) {
      onup();
    } else if (e.key === "Escape") {
      onpick(null);
    } else if (e.key.toLowerCase() === "a" && toggles(e)) {
      selectAll();
    } else if (e.key === " " && toggles(e) && selection && !selection.other) {
      onpick({ focus: selection, nodes: [selection.node], mode: "toggle", keepAnchor: false });
    } else if (e.key === "Delete") {
      ondelete?.();
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
    if (colors === "type" && l.kind[i] === KIND_FILE) lines.push(`Type: ${FILE_TYPES[l.ftype[i]!]}`);
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
  aria-label="Treemap. Arrow keys move the selection, Ctrl+click or Shift+click selects several items, Enter opens a folder, Backspace goes up, Delete moves the selection to the trash once deleting is allowed, Shift+F10 shows actions, Escape clears the selection."
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
  {#if showPerf}
    <pre class="perf" aria-label="Timing diagnostics">{perfText}</pre>
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
  .perf {
    position: absolute;
    right: 6px;
    bottom: 6px;
    margin: 0;
    padding: 6px 8px;
    border-radius: 4px;
    background: rgb(0 0 0 / 0.75);
    color: #fff;
    font-size: 11px;
    pointer-events: none;
  }
  .error {
    position: absolute;
    left: 8px;
    bottom: 8px;
    margin: 0;
    color: var(--danger);
  }
</style>
