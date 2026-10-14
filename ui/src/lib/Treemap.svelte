<script lang="ts">
  import PathText from "./PathText.svelte";
  // Canvas treemap. Pulls a layout from the backend whenever its inputs
  // change; at most one request is in flight, and changes that arrive
  // meanwhile are folded into one follow-up request (natural backpressure).
  import { announce } from "./announce.svelte";
  import { changeMarks, nodeDetails, requestLayout } from "./api";
  import {
    changeColor,
    depthColor,
    typeModeFolderColor,
    otherSmallColor,
    isDarkFill,
    typeColor,
    type ColorMode,
  } from "./colors";
  import { MARK_UNKNOWN } from "./compare";
  import { formatBytes, formatCount, percent, plural, shortcut } from "./format";
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
  import { cssVar, theme } from "./theme.svelte";
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
    /** The earlier scan the "change" colors compare with. */
    compare?: string | null;
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
    compare = null,
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

  const helpId = $props.id();
  let container: HTMLDivElement;
  // Three stacked canvases, composited as if drawn in one, so hovering
  // never repaints the map: tiles and selection, then the hover highlight,
  // then the focus ring and labels (which stay above the highlight).
  let canvas: HTMLCanvasElement;
  let hoverCanvas: HTMLCanvasElement;
  let topCanvas: HTMLCanvasElement;
  let width = $state(0);
  let height = $state(0);
  let layout = $state.raw<DecodedLayout | null>(null);
  let error = $state<string | null>(null);
  /** The rectangle under the pointer, or -1. Only a change of rectangle
   * redraws the map; moves within one only move the tooltip. */
  let hoverIndex = $state(-1);
  /** Where the pointer is over the map, for the tooltip. */
  let pointer = $state<{ x: number; y: number } | null>(null);
  /** Details for the tooltip's item, fetched shortly after it changes. */
  let tipDetails = $state.raw<NodeDetails | null>(null);
  /** The tooltip follows the keyboard selection until the mouse moves. */
  let keyTip = $state(false);
  let tipWidth = $state(0);
  let tipHeight = $state(0);
  const dark = $derived(theme.dark);

  /** Canvas colors from the app.css tokens, re-read when the theme changes. */
  const palette = $derived.by(() => {
    void theme.dark;
    return {
      gap: cssVar("--map-gap"),
      textDark: cssVar("--map-text-dark"),
      textLight: cssVar("--map-text-light"),
      incomplete: cssVar("--incomplete"),
      accent: cssVar("--accent"),
      // Labels are --fs-xs (11px) at the default 14px root size and scale with it.
      fontSize: Math.round((parseFloat(getComputedStyle(document.documentElement).fontSize) || 14) * (11 / 14)),
    };
  });

  // ---- diagnostics (toggle with ` while the map has focus) ---------------

  const stats = { layout: new Samples(), draw: new Samples(), input: new Samples() };
  /** When the last click or key arrived, until the next frame is drawn. */
  let inputAt: number | null = null;
  let showPerf = $state(false);
  let perfText = $state("");

  /** Paint tracing: on with the overlay, or with `localStorage.sbTrace = "1"`
   * or `?sbTrace` in the URL. Off, it costs a branch per label. */
  const traceFlag = (() => {
    try {
      return localStorage.getItem("sbTrace") === "1" || new URLSearchParams(location.search).has("sbTrace");
    } catch {
      return false;
    }
  })();
  const trace = {
    on: traceFlag,
    /** Since tracing started: whole-map draws, their label passes, hover draws. */
    full: new Samples(),
    labels: new Samples(),
    hover: new Samples(),
    measures: new Samples(),
    /** measureText calls in the draw under way. */
    measuring: 0,
    draws: 0,
    hoverDraws: 0,
    moves: 0,
    /** When recent draws happened, for draws per second. */
    recent: [] as number[],
  };

  /** Label text widths and fitted labels for the current font. Both depend
   * only on the font, the text, and (for fits) the room, so they carry over
   * from draw to draw and across layouts. */
  const textCache = { font: "", widths: new Map<string, number>(), fits: new Map<string, string>() };
  const TEXT_CACHE_LIMIT = 50_000;

  /** The width of `text` in the label font: cached, counted while tracing. */
  function measure(ctx: CanvasRenderingContext2D, text: string): number {
    const known = textCache.widths.get(text);
    if (known !== undefined) return known;
    if (trace.on) trace.measuring++;
    const width = ctx.measureText(text).width;
    if (textCache.widths.size >= TEXT_CACHE_LIMIT) textCache.widths.clear();
    textCache.widths.set(text, width);
    return width;
  }

  $effect(() => {
    trace.on = traceFlag || showPerf;
    if (!showPerf) return;
    if (!traceFlag) {
      // Count from when the overlay opens.
      trace.full = new Samples();
      trace.labels = new Samples();
      trace.hover = new Samples();
      trace.measures = new Samples();
      trace.draws = trace.hoverDraws = trace.moves = 0;
    }
    const update = () => {
      const now = performance.now();
      const perSecond = trace.recent.filter((t) => now - t <= 1000).length;
      perfText = [
        `layout round trip: ${stats.layout.summary()}`,
        `draw: ${stats.draw.summary()}`,
        `traced draw: ${trace.full.summary()}`,
        `labels: ${trace.labels.summary()}`,
        `hover draw: ${trace.hover.summary()}`,
        `measureText per draw: ${trace.measures.summary().replace(" ms", "")}`,
        `draws/s: ${perSecond} · draws ${trace.draws} · hover draws ${trace.hoverDraws} · moves ${trace.moves}`,
        `input to frame: ${stats.input.summary()}`,
      ].join("\n");
    };
    update();
    const timer = setInterval(update, 500);
    return () => clearInterval(timer);
  });

  // ---- layout requests -------------------------------------------------

  /** What the drawn layout shows; differs from the props while a new one loads. */
  let shown = $state<{ generation: number; view: number; search: number | null } | null>(null);
  /** The map is dimmed while it shows another view or search than asked for.
   * Scan revisions refresh in place without dimming. */
  const stale = $derived(
    shown !== null &&
      (shown.generation !== generation || shown.view !== view || shown.search !== search),
  );

  /** Change marks for the drawn layout's rectangles, in change mode. */
  let marks = $state.raw<{ layout: DecodedLayout; compare: string; bytes: Uint8Array } | null>(null);
  const changeMode = $derived(colors === "change" && compare !== null);

  async function fetchMarks(l: DecodedLayout, cmp: string): Promise<Uint8Array> {
    const nodes = Array.from(l.node.subarray(0, l.count));
    return new Uint8Array(await changeMarks({ generation: l.generation, snapshot: cmp, nodes }));
  }

  // Switching to change colors, or to another comparison, marks the map
  // already drawn; new layouts come with their marks (see fetchLayout).
  $effect(() => {
    const l = layout;
    const cmp = changeMode ? compare : null;
    if (!l || cmp === null || (marks?.layout === l && marks.compare === cmp)) return;
    fetchMarks(l, cmp).then(
      (bytes) => {
        if (layout === l && compare === cmp) marks = { layout: l, compare: cmp, bytes };
      },
      () => {},
    );
  });

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
          // In change mode the marks arrive with the layout, so tiles never
          // flash in neutral colors first.
          const cmp = changeMode ? compare : null;
          let replyMarks: Uint8Array | null = null;
          if (cmp !== null) replyMarks = await fetchMarks(reply, cmp).catch(() => null);
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
            if (replyMarks && cmp !== null) marks = { layout: reply, compare: cmp, bytes: replyMarks };
            shown = { generation: want.generation, view: want.view, search: want.search };
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

  // ---- sizing ----------------------------------------------------------

  $effect(() => {
    const observer = new ResizeObserver(([entry]) => {
      if (!entry) return;
      width = Math.floor(entry.contentRect.width);
      height = Math.floor(entry.contentRect.height);
    });
    observer.observe(container);
    return () => observer.disconnect();
  });

  // ---- drawing ---------------------------------------------------------

  function fillFor(l: DecodedLayout, i: number): string {
    const kind = l.kind[i];
    if (kind === KIND_OTHER) return otherSmallColor(dark);
    if (colors === "change") {
      const m = marks?.layout === l && marks.compare === compare ? marks.bytes[i]! : MARK_UNKNOWN;
      return changeColor(m, kind === KIND_FOLDER, dark);
    }
    if (colors === "type") {
      return kind === KIND_FOLDER ? typeModeFolderColor(dark) : typeColor(l.ftype[i]!, dark);
    }
    return depthColor(l.depth[i]!, kind === KIND_FOLDER, dark);
  }

  /** `text`, cut with an ellipsis to fit `max` pixels; cached. */
  function fit(ctx: CanvasRenderingContext2D, text: string, max: number): string {
    if (max <= 8) return "";
    const key = `${max}\u0000${text}`;
    let fitted = textCache.fits.get(key);
    if (fitted === undefined) {
      fitted = cut(ctx, text, max);
      if (textCache.fits.size >= TEXT_CACHE_LIMIT) textCache.fits.clear();
      textCache.fits.set(key, fitted);
    }
    return fitted;
  }

  function cut(ctx: CanvasRenderingContext2D, text: string, max: number): string {
    if (measure(ctx, text) <= max) return text;
    let lo = 0;
    let hi = text.length;
    while (lo < hi) {
      const mid = (lo + hi + 1) >> 1;
      if (measure(ctx, text.slice(0, mid) + "…") <= max) lo = mid;
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
    trace.measuring = 0;
    paint();
    const now = performance.now();
    stats.draw.add(now - started);
    if (trace.on) {
      trace.draws++;
      trace.full.add(now - started);
      trace.measures.add(trace.measuring);
      trace.recent.push(now);
      while (trace.recent.length > 0 && now - trace.recent[0]! > 1000) trace.recent.shift();
    }
    if (inputAt !== null) {
      stats.input.add(now - inputAt);
      inputAt = null;
    }
  }

  /** A cleared 2D context for `c`, sized to the map at the device pixel ratio. */
  function cleared(c: HTMLCanvasElement | undefined): CanvasRenderingContext2D | null {
    const ctx = c?.getContext("2d");
    if (!c || !ctx) return null;
    const dpr = window.devicePixelRatio || 1;
    if (c.width !== Math.round(width * dpr) || c.height !== Math.round(height * dpr)) {
      c.width = Math.round(width * dpr);
      c.height = Math.round(height * dpr);
    }
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, width, height);
    return ctx;
  }

  /** Tiles and selection on the bottom canvas; focus ring and labels on top. */
  function paint() {
    const ctx = cleared(canvas);
    const top = cleared(topCanvas);
    if (!ctx || !top) return;
    const l = layout;
    if (!l) return;

    const p = palette;
    const font = `${p.fontSize}px system-ui, -apple-system, 'Segoe UI', sans-serif`;
    if (textCache.font !== font) {
      textCache.font = font;
      textCache.widths.clear();
      textCache.fits.clear();
    }
    top.font = font;
    top.textBaseline = "alphabetic";
    const gap = p.gap;

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
        ctx.strokeStyle = p.incomplete;
        ctx.lineWidth = 1;
        ctx.setLineDash([3, 3]);
        ctx.strokeRect(x + 1, y + 1, w - 2, h - 2);
        ctx.setLineDash([]);
      }
    }

    // Selection: a light accent tint (labels stay readable) under an accent
    // frame edged in dark and light, so it can't pass for a blue depth fill.
    const ink = dark ? p.textLight : p.textDark;
    const paper = dark ? p.textDark : p.textLight;
    const tint = dark ? 0.24 : 0.18;
    const si = selectedIndex(l, selection);
    const pickedAt = (i: number) =>
      l.kind[i] === KIND_OTHER ? i === si : selected.has(l.node[i]!);

    for (let i = 0; i < l.count; i++) {
      // Small-items regions are never in `selected`; they count as picked
      // while they hold the focus.
      if (!pickedAt(i)) continue;
      const x = l.x[i]!;
      const y = l.y[i]!;
      const w = l.w[i]!;
      const h = l.h[i]!;
      ctx.globalAlpha = tint;
      ctx.fillStyle = p.accent;
      ctx.fillRect(x, y, w, h);
      ctx.globalAlpha = 1;
      if (w > 8 && h > 8) {
        band(ctx, x, y, w, h, 0, 1, ink);
        band(ctx, x, y, w, h, 1, 2, p.accent);
        band(ctx, x, y, w, h, 3, 1, paper);
      } else if (w > 2 && h > 2) {
        band(ctx, x, y, w, h, 0, 1, p.accent);
      }
    }

    // The hover highlight goes between the canvases (see paintHover).

    // The focused item: a heavy dark-and-light ring, keeping the accent
    // inside it when the item is also picked.
    if (si >= 0) {
      const x = l.x[si]!;
      const y = l.y[si]!;
      const w = l.w[si]!;
      const h = l.h[si]!;
      band(top, x, y, w, h, 0, 3, ink);
      band(top, x, y, w, h, 3, 1, paper);
      if (pickedAt(si) && w > 14 && h > 14) band(top, x, y, w, h, 4, 2, p.accent);
    }

    // Labels go last, so tints and frames never hide them.
    // Labels go inside the frames of selected and focused tiles.
    const labelsAt = trace.on ? performance.now() : 0;
    for (let i = 0; i < l.count; i++) drawLabel(top, l, i, i === si ? 7 : pickedAt(i) ? 5 : 4);
    if (trace.on) trace.labels.add(performance.now() - labelsAt);
  }

  /** Hover: a slight lighten and a 2px line, quieter than the selection.
   * Above the selection, below the focus ring and labels. */
  function paintHover() {
    const ctx = cleared(hoverCanvas);
    const l = layout;
    const hi = hoverIndex;
    if (!ctx || !l || hi < 0 || hi >= l.count) return;
    const p = palette;
    ctx.globalAlpha = dark ? 0.1 : 0.22;
    ctx.fillStyle = p.textLight;
    ctx.fillRect(l.x[hi]!, l.y[hi]!, l.w[hi]!, l.h[hi]!);
    ctx.globalAlpha = 0.7;
    band(ctx, l.x[hi]!, l.y[hi]!, l.w[hi]!, l.h[hi]!, 0, 2, dark ? p.textLight : p.textDark);
    ctx.globalAlpha = 1;
  }

  function drawLabel(ctx: CanvasRenderingContext2D, l: DecodedLayout, i: number, pad: number) {
    const p = palette;
    const x = l.x[i]! + pad - 4;
    const y = l.y[i]! + pad - 4;
    const w = l.w[i]! - 2 * (pad - 4);
    const h = l.h[i]! - 2 * (pad - 4);
    const kind = l.kind[i]!;
    // Baselines of the first and second label lines, from the tile top.
    const line1 = p.fontSize + 1;
    const line2 = line1 + p.fontSize + 2;
    // Tiles shorter than this get no label at all.
    if (h < Math.max(14, p.fontSize + 3) || w < 16) return;
    const name = l.labels.get(i);
    const header = kind === KIND_FOLDER && (l.rflags[i]! & RF_HEADER) !== 0;
    if (kind !== KIND_OTHER && (name === undefined || (kind === KIND_FOLDER && !header))) return;

    ctx.save();
    ctx.beginPath();
    ctx.rect(x, y, w, h);
    ctx.clip();
    // Dark or light text, whichever contrasts with this tile.
    ctx.fillStyle = isDarkFill(fillFor(l, i)) ? p.textLight : p.textDark;
    const room = w - 8;
    if (kind === KIND_OTHER) {
      // Shorter wordings as the region narrows.
      const n = formatCount(l.items[i]!);
      const text = [plural(l.items[i]!, "small item"), `${n} small`, n].find(
        (t) => measure(ctx, t) <= room,
      );
      if (text) ctx.fillText(text, x + 4, y + line1);
    } else {
      // The size in full, or none where it doesn't fit.
      const size = formatBytes(l.weight[i]!);
      const sizeWidth = measure(ctx, size);
      if (kind === KIND_FILE && h >= line2 + 3) {
        // Tall enough for two lines: name, then size.
        label(ctx, name!, x + 4, y + line1, room);
        if (sizeWidth <= room) ctx.fillText(size, x + 4, y + line2);
      } else if (room >= sizeWidth + 30) {
        // One line: the size right-aligned, the name cut to fit before it.
        label(ctx, name!, x + 4, y + line1, room - sizeWidth - 6);
        ctx.fillText(size, x + w - 4 - sizeWidth, y + line1);
      } else if (sizeWidth <= room) {
        // The size matters most, so it wins when both don't fit.
        ctx.fillText(size, x + 4, y + line1);
      }
    }
    ctx.restore();
  }

  /** Strokes a band `width` px wide, `inset` px inside the rectangle. */
  function band(
    ctx: CanvasRenderingContext2D,
    x: number,
    y: number,
    w: number,
    h: number,
    inset: number,
    width: number,
    color: string,
  ) {
    const o = inset + width / 2;
    if (w <= 2 * o || h <= 2 * o) return;
    ctx.lineWidth = width;
    ctx.strokeStyle = color;
    ctx.strokeRect(x + o, y + o, w - 2 * o, h - 2 * o);
  }

  // Both layers are drawn in one animation frame, each only when its
  // inputs changed.
  let frame = 0;
  let mapDirty = false;
  let hoverDirty = false;

  function flush() {
    frame = 0;
    if (mapDirty) {
      mapDirty = false;
      draw();
    }
    if (hoverDirty) {
      hoverDirty = false;
      const started = performance.now();
      paintHover();
      if (trace.on) {
        trace.hoverDraws++;
        trace.hover.add(performance.now() - started);
      }
    }
  }

  function schedule() {
    if (frame === 0) frame = requestAnimationFrame(flush);
  }

  $effect(() => {
    void [layout, marks, selection, selected, palette, colors, width, height];
    mapDirty = true;
    schedule();
  });

  $effect(() => {
    void [layout, hoverIndex, palette, width, height];
    hoverDirty = true;
    schedule();
  });

  $effect(() => () => {
    cancelAnimationFrame(frame);
    frame = 0;
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

  function onMove(e: MouseEvent) {
    const l = layout;
    if (!l) return;
    const [x, y] = point(e);
    const index = hitTest(l, x, y);
    if (trace.on) trace.moves++;
    hoverIndex = index;
    pointer = index >= 0 ? { x, y } : null;
    keyTip = false;
  }

  function onLeave() {
    hoverIndex = -1;
    pointer = null;
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

  /** Says what rectangle `i` is: "name, size, 12% of parent, folder". */
  function announceAt(i: number) {
    const l = layout;
    if (!l) return;
    const size = formatBytes(l.weight[i]!);
    const share = shareOfParent(l, i);
    const say = (name: string, kind: string) =>
      announce([name, size, share, kind].filter(Boolean).join(", "));
    const kind = l.kind[i];
    if (kind === KIND_OTHER) {
      say(plural(l.items[i]!, "small item"), "merged");
      return;
    }
    const what = kind === KIND_FOLDER ? "folder" : "file";
    const name = l.labels.get(i);
    if (name !== undefined) {
      say(name, what);
      return;
    }
    // Small tiles carry no label; ask for the name.
    const node = l.node[i]!;
    const gen = generation;
    nodeDetails(gen, node).then(
      (d) => {
        if (selection?.node === node && !selection.other && generation === gen) say(d.name, what);
      },
      () => {},
    );
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
      if (i >= 0) {
        onpick(only(i));
        keyTip = true;
        hoverIndex = -1;
        pointer = null;
        announceAt(i);
      }
    } else if (e.key === "Enter") {
      open(selection);
    } else if (e.key === "Backspace" || (e.key === "ArrowUp" && e.altKey)) {
      onup();
    } else if (e.key === "Escape") {
      keyTip = false;
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

  /** Zoom-in over what double-click opens: folders and small items. */
  const cursor = $derived.by(() => {
    const l = layout;
    const i = hoverIndex;
    if (!l || i < 0 || i >= l.count) return "default";
    return l.kind[i] !== KIND_FILE && l.node[i] !== view ? "zoom-in" : "default";
  });

  // ---- tooltip ---------------------------------------------------------

  /** The rectangle the tooltip describes: hovered, or keyboard-selected. */
  const tipIndex = $derived.by(() => {
    const l = layout;
    if (!l) return -1;
    if (hoverIndex >= 0) return hoverIndex < l.count ? hoverIndex : -1;
    return keyTip ? selectedIndex(l, selection) : -1;
  });

  /** The node whose details the tooltip needs; -1 for none or small items. */
  const tipNode = $derived(
    tipIndex >= 0 && layout && layout.kind[tipIndex] !== KIND_OTHER ? layout.node[tipIndex]! : -1,
  );

  $effect(() => {
    const node = tipNode;
    const gen = generation;
    tipDetails = null;
    if (node < 0) return;
    const timer = setTimeout(() => {
      nodeDetails(gen, node).then(
        (d) => {
          if (d.node === tipNode && d.generation === generation) tipDetails = d;
        },
        () => {},
      );
    }, 120);
    return () => clearTimeout(timer);
  });

  /** Size of the folder rectangle `i` is drawn in, or 0 if not drawn. */
  function parentWeight(l: DecodedLayout, i: number): number {
    const parent = l.parent[i]!;
    if (parent === l.view) return l.total;
    // Parents come before their children.
    for (let j = i - 1; j >= 0; j--) {
      if (l.node[j] === parent && l.kind[j] === KIND_FOLDER) return l.weight[j]!;
    }
    return 0;
  }

  /** "12% of parent" for rectangle `i`, or `null` when unknown. */
  function shareOfParent(l: DecodedLayout, i: number): string | null {
    const whole = parentWeight(l, i);
    return whole > 0 ? `${percent(l.weight[i]!, whole)} of parent` : null;
  }

  /**
   * What the tooltip shows. Every line it will have is there from the
   * start; `null` marks one still waiting for details, drawn as a
   * placeholder so the tooltip keeps its size when they arrive.
   */
  const tooltip = $derived.by(() => {
    const l = layout;
    const i = tipIndex;
    if (!l || i < 0) return null;
    const share = shareOfParent(l, i);
    const size = [formatBytes(l.weight[i]!), share].filter(Boolean).join(" · ");
    if (l.kind[i] === KIND_OTHER) {
      return {
        title: plural(l.items[i]!, "small item"),
        path: undefined,
        lines: [`${size} combined`, "Too small to draw individually"],
      };
    }
    const d = tipDetails;
    const folder = l.kind[i] === KIND_FOLDER;
    const lines: (string | null)[] = [size];
    if (folder) lines.push(d ? plural(d.files, "file") : null);
    if (folder && l.rflags[i]! & RF_INCOMPLETE) lines.push("Not fully scanned");
    if (colors === "type" && !folder) lines.push(`Type: ${FILE_TYPES[l.ftype[i]!]}`);
    return { title: d?.name ?? l.labels.get(i) ?? "", path: d ? d.path : null, lines };
  });

  /**
   * Where the tooltip may go: right of `right` or left of `left`, below
   * `below` or above `above`, whichever fits inside the map.
   */
  const tipAnchor = $derived.by(() => {
    const l = layout;
    if (!l || tipIndex < 0) return null;
    if (hoverIndex >= 0 && pointer) {
      const { x, y } = pointer;
      return { right: x + 12, left: x - 12, below: y + 12, above: y - 12 };
    }
    // Keyboard: under small tiles, just inside the top of big ones.
    const x = l.x[tipIndex]!;
    const y = l.y[tipIndex]!;
    return {
      right: x + 8,
      left: x + 8,
      below: Math.min(y + l.h[tipIndex]! + 4, y + 24),
      above: y - 4,
    };
  });

  const tipPosition = $derived.by(() => {
    const a = tipAnchor;
    if (!a) return null;
    const clamp = (v: number, max: number) => Math.max(4, Math.min(v, max - 4));
    let left = a.right;
    if (left + tipWidth > width - 4) left = a.left - tipWidth;
    let top = a.below;
    if (top + tipHeight > height - 4) top = a.above - tipHeight;
    return { left: clamp(left, width - tipWidth), top: clamp(top, height - tipHeight) };
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
  aria-label="Treemap"
  aria-describedby={helpId}
  style:cursor
  onmousemove={onMove}
  onmouseleave={onLeave}
  onclick={onClick}
  ondblclick={onDblClick}
  oncontextmenu={onContextMenu}
  onkeydown={onKey}
  onblur={() => (keyTip = false)}
>
  <p id={helpId} class="sr-only">
    Arrow keys move the selection. {shortcut("Ctrl")}+click or Shift+click selects several items,
    and {shortcut("Ctrl+A")} selects everything in view. Enter opens a folder, Backspace goes up.
    Delete moves the selection to the Recycle Bin once deleting is allowed. Shift+F10 shows
    actions. Escape clears the selection.
  </p>
  <!-- Dimmed as one, as a single canvas was. -->
  <div class="layers" class:stale style:width="{width}px" style:height="{height}px" aria-hidden="true">
    <canvas bind:this={canvas} style:width="{width}px" style:height="{height}px"></canvas>
    <canvas bind:this={hoverCanvas} style:width="{width}px" style:height="{height}px"></canvas>
    <canvas bind:this={topCanvas} style:width="{width}px" style:height="{height}px"></canvas>
  </div>
  {#if tooltip && tipPosition}
    <div
      class="tooltip"
      aria-hidden="true"
      bind:clientWidth={tipWidth}
      bind:clientHeight={tipHeight}
      style:left="{tipPosition.left}px"
      style:top="{tipPosition.top}px"
      style:visibility={tipHeight > 0 ? "visible" : "hidden"}
    >
      <strong class="title">{tooltip.title || "\u00a0"}</strong>
      {#if tooltip.path !== undefined}
        <div class="path selectable" class:pending={tooltip.path === null}>{#if tooltip.path}<PathText path={tooltip.path} />{/if}</div>
      {/if}
      {#each tooltip.lines as line, i (i)}
        <div class="line" class:pending={line === null}>{line ?? ""}</div>
      {/each}
    </div>
  {/if}
  {#if error}
    <p class="error selectable" role="alert">Couldn't draw the map: {error}</p>
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
  .layers {
    position: relative;
    transition: opacity 0.15s;
  }
  .layers.stale {
    opacity: 0.6;
  }
  canvas {
    display: block;
    position: absolute;
    left: 0;
    top: 0;
  }
  .tooltip {
    position: absolute;
    /* A fixed width, so details arriving later never resize it. */
    box-sizing: border-box;
    width: 300px;
    max-width: calc(100% - 8px);
    padding: 6px 8px;
    border-radius: var(--radius);
    background: var(--panel);
    color: var(--fg);
    border: 1px solid var(--line);
    box-shadow: 0 2px 8px rgb(0 0 0 / 0.25);
    pointer-events: none;
    font-size: var(--fs-sm);
    line-height: 1.35;
  }
  .tooltip .title {
    display: block;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .tooltip .line {
    min-height: 1.35em;
  }
  /* Paths take up to two lines, reserved from the start. */
  .tooltip .path {
    display: -webkit-box;
    -webkit-box-orient: vertical;
    -webkit-line-clamp: 2;
    line-clamp: 2;
    height: 2.7em;
    overflow: hidden;
    overflow-wrap: anywhere;
    color: var(--muted);
  }
  .tooltip .pending::before {
    content: "";
    display: inline-block;
    width: 60%;
    height: 0.75em;
    border-radius: 2px;
    background: var(--line);
  }
  .tooltip .line.pending::before {
    width: 30%;
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
  /* A solid chip, readable over any tile. */
  .error {
    position: absolute;
    left: var(--space-2);
    bottom: var(--space-2);
    max-width: calc(100% - 2 * var(--space-2));
    box-sizing: border-box;
    margin: 0;
    padding: var(--space-1) var(--space-2);
    border: 1px solid var(--danger);
    border-radius: var(--radius);
    background: var(--danger-bg);
    color: var(--danger);
    font-size: var(--fs-sm);
    overflow-wrap: anywhere;
  }
</style>
