# Treemap painting while the pointer moves

Date: 2026-10-14. Branch `feature/37/treemap-paint`. AMD Ryzen 9 5900X, Windows 11 Pro 10.0.26200, WebView2. Release builds (`npx tauri build --no-bundle`).

An architecture review found that every pointer move over the map replaced the hover object, which redrew the whole map (tiles, selection, labels with their `measureText` fitting) even when the pointer stayed on one tile. It asked to measure this first.

## How it was measured

The map's diagnostics overlay (`` ` `` with the map focused) now traces draws while it is open, or always with `localStorage.sbTrace = "1"` or `?sbTrace` in the URL: whole-map draw time, the label pass, hover-layer draw time, `measureText` calls per draw, and counts of pointer moves, whole-map draws, and hover draws (with draws per second). The counters restart when the overlay opens. A build of `origin/dev` with the same tracing gave the baseline.

Scan of `C:\Windows` (221,897 files, 173,325 folders), window client 2560×1600 physical pixels at 125% scaling, so the map is about 1,470×1,110 CSS pixels with every tile the layout gives. Pointer moves were posted to the webview every 16 ms (Chromium delivered about 30 a second):

- **Within one tile**: 240 moves inside `MEMORY.DMP` (a 4.47 GB tile).
- **Across tiles**: 240 moves left to right across WinSxS's small tiles.
- **Selection**: ten clicks on different tiles.

Posted moves are followed by a `mouseleave` when the real cursor isn't over the window, which made the first runs count a leave per move. The window was moved so the cursor (fixed at screen 0,0 in this session) lay over the map.

## Results

| Case | Measure | Before | After |
|---|---|---|---|
| Within one tile | Whole-map draws per move | 1 (64 of 64; 111 of 111) | 0 (0 of 110) |
| | Hover-layer draws | — | 0 |
| | Main-thread drawing | 3.8 ms × ~31/s ≈ 12% of a core | none; the tooltip moves |
| Across tiles | Whole-map draws per move | 1 (124 of 124) | 0 |
| | Hover-layer draws | — | 27 for 126 moves (once per tile entered), p50 < 0.05 ms, max 0.1 ms |
| Every whole-map draw | Draw time | p50 3.7–3.8 ms, p95 4.3–4.6 ms | unchanged (clicks: p50 3.7–4.2 ms, p95 4.9–5.0 ms) |
| | Label pass | p50 1.3–1.4 ms | p50 1.2–1.4 ms |
| | `measureText` calls | 416 | 416 |
| Selection clicks | Whole-map draws | 10 | 10 |

### Label cache: tried and dropped

Caching text widths and fitted labels (keyed by font, text, and room) cut `measureText` calls to 0 per draw once warm, but the label pass stayed at p50 1.1–1.2 ms against 1.2–1.4 ms without it: `fillText` and clipping dominate, not measuring. With no measurable gain, it was removed. Hover no longer redraws labels at all, so they are drawn only on layout, selection, color, theme, and size changes.

## What changed

- **Hovered tile apart from pointer position.** `hoverIndex` (which tile) and `pointer` (where, for the tooltip) are separate state. A move within one tile only moves the tooltip.
- **Three stacked canvases.** The bottom one has tiles and selection tints and frames; the middle one only the hover highlight; the top one the focus ring and labels. That keeps the drawing order of the single canvas (selection, hover, focus ring, labels), so layered they composite the same. A change of hovered tile clears and redraws only the middle canvas. Both are drawn in one animation frame, each only when its inputs changed.
- The canvases sit in one wrapper that is dimmed while another view loads, so the dimming applies to the composite as before.

## Visual check

Screenshots of the same states before and after, compared pixel by pixel over the map: a selected tile with another hovered, the selected tile hovered (focus ring and label above the highlight), a keyboard move with its tooltip, a folder opened by double-click, and the window resized afterwards. Every difference is a channel value off by 1 (compositing rounding: 94 to 2,164 pixels per screenshot), except in the resized case's status bar, which shows the scan time. Click, double-click, and context-menu hit testing, Ctrl/Shift selection, and keyboard handling weren't touched: they still read the layout through `hitTest` and the selection props.

## Limits

- The two extra canvases cost memory: each is the map's size at the device pixel ratio (about 10 MB each here), mostly transparent.
- A whole-map draw (layout, selection, colors, theme, size) still costs the same; most of it is filling tiles.
- The dark theme wasn't switched in the real app, to avoid changing the system setting; the palette, which the theme sets, is an input of both layers' redraws.
