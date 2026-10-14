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

The "after" column was measured with the highlight on its own canvas. After review it became an element instead (below), which moves without any canvas drawing at all; the overlay then counts hovered-tile changes instead of hover-layer draws.

### Label cache: tried and dropped

Caching text widths and fitted labels (keyed by font, text, and room) cut `measureText` calls to 0 per draw once warm, but the label pass stayed at p50 1.1–1.2 ms against 1.2–1.4 ms without it: `fillText` and clipping dominate, not measuring. With no measurable gain, it was removed. Hover no longer redraws labels at all, so they are drawn only on layout, selection, color, theme, size, and pixel-ratio changes.

## What changed

- **Hovered tile apart from pointer position.** `hoverIndex` (which tile) and `pointer` (where, for the tooltip) are separate state. A move within one tile only moves the tooltip.
- **Two canvases with the highlight between them.** The bottom canvas has tiles and selection tints and frames; the top one the focus ring and labels. Between them, two absolutely positioned elements at the hovered tile's rectangle draw the highlight: a translucent fill and a 2px border in the ink color. That keeps the single canvas's drawing order (selection, hover, focus ring, labels). A change of hovered tile only moves the elements. A first version used a third full-size canvas for the highlight; the elements save that canvas (up to about 33 MB on a 4K screen).
- The layers sit in one wrapper that is dimmed while another view loads, so the dimming applies to the composite as before.
- **A new layout under a resting pointer** (scan refresh, resize, navigation) is hit-tested again at the pointer's last position, so the highlight and tooltip show the tile now under it. Before, they kept the old index, which named another tile (seen after a resize: the tooltip named `f12db0c.msp` with the pointer on `da45da.msi`). Keyboard moves and the context menu forget the position, as they clear the hover.
- **Device pixel ratio changes** (moving the window to a monitor with other scaling) redraw the canvases at the new ratio: a `(resolution: Xdppx)` media query for the current ratio is watched and replaced each time it changes.

## Visual check

Screenshots of the same states before and after, compared pixel by pixel over the map: a selected tile with another hovered, the selected tile hovered (focus ring and label above the highlight), a keyboard move with its tooltip, a folder opened by double-click, and the window resized afterwards.

- With the third canvas, every difference was a channel value off by 1 (compositing rounding).
- With the highlight as elements (screen now at 150%), the selection, focus ring, labels, and tooltip still differ by at most 1. The highlight's edges differ by up to one device pixel: at a fractional device position the canvas spread the 2px border over four device pixels with soft edges, and the element draws it as three solid ones.

Click, double-click, and context-menu hit testing, Ctrl/Shift selection, and keyboard handling weren't touched: they still read the layout through `hitTest` and the selection props.

## Limits

- The top canvas costs memory: the map's size at the device pixel ratio (about 10 MB here), mostly transparent.
- A whole-map draw (layout, selection, colors, theme, size) still costs the same; most of it is filling tiles.
- The dark theme and a pixel-ratio change weren't tried in the real app: the first would change the system setting, and only one monitor was available. The palette, which the theme sets, feeds the canvas redraw and the highlight's colors.
