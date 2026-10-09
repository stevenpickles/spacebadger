# Milestone 4 — filename search

Date: 2026-10-09. Branch `feature/6/filename-search`. Same benchmark machine as [`scanner-m2.md`](scanner-m2.md).

## What exists

- **Matching (`sb-core::search`):** a file matches when its basename, including the extension, contains the query as a case-insensitive literal substring.
  - No wildcards or regular expressions: `*.txt` matches only names containing `*.txt`.
  - Folder names, full paths, and file contents are never matched.
  - ASCII names are compared without allocating; other names are lowercased with Unicode rules.
- **Overlay:** a search keeps its own totals (logical, allocated, unknown-allocation count, file count) for matching files and every folder above them.
  - It examines nodes in ID order. New nodes always get larger IDs, so catching up with a running scan only looks at what's new.
  - Files re-measured after they were added (hard-link handling) are replayed from a change log in the tree, so totals stay exact.
  - Hard-link ownership is kept: an alias that matches adds its logical length but no allocation.
- **Map:** the treemap layout accepts the search and sizes every rectangle by matching bytes only; folders without matches are left out. Each search has its own sibling-order cache.
- **Bridge:**
  - `search_set` builds a new search on a blocking thread in steps of 200,000 nodes, releasing the tree's lock between steps. It gives up as soon as a newer query arrives.
  - `search_summary` returns matching totals, caught up with the scan.
  - `search_results` returns pages of matches sorted by the chosen size. The sort is cached until a matching total changes.
  - Layout requests name the search they want; replies for an older search are refused.
- **Interface:**
  - The filter box in the toolbar searches 250 ms after typing stops, and only the newest reply is used.
  - The result list (largest first) only renders the rows on screen and fetches 100 at a time. Each row shows the name, active size, and containing folder relative to the scan root; hovering the size shows both exact sizes.
  - Keyboard: arrow keys, Page Up/Down, and Home/End move through results. Enter or double-click opens the file's folder in the map with the file selected. Right-click or the Menu key offers Show in map, Show in Explorer/Finder/file manager, and Copy path.
  - The header shows matching files and bytes, the share of the scanned total, and "Still scanning: more matches may appear" while scanning.
  - The status bar keeps showing unfiltered totals.
  - Empty states: a folder with no matches offers "Go to the scan root"; a query with no matches says so; matches with no measurable size point to the result list.
  - The current folder and selection are kept when the query changes or is cleared. Refresh and new roots re-apply the query.

## Measurements

Release build, user profile (1.34M files, 1.57M nodes), from the `scan` example:

| Query | Matches | Full search | Filtered root layout |
|---|---|---|---|
| `.dll` | 12,105 | 47 ms | 1.1 ms, 4,015 rects |
| `e` | 869,285 | 69 ms | 5.9 ms, 8,329 rects |
| no match | 0 | 25 ms | under 1 µs |

## Verified on Windows

Driven through UI Automation (setting the filter box) and posted mouse and key messages (see [`desktop-m3.md`](desktop-m3.md)), checked with screenshots, debug build:

| Check | Result |
|---|---|
| `.dll` while the profile was still scanning | Filtered map and 10,228 results within 1.5 s, with "Still scanning: more matches may appear" |
| Same search after the scan completed | Totals caught up to 12,105 files / 8.64 GiB without re-searching |
| Click a result, Down, Enter | Map opened `…\chromium-1234\chrome-win64` with `chrome.dll` selected; breadcrumbs followed |
| Query that matches nothing, debug build | "Searching…" shown while the previous result stays on screen (the final empty state was not captured) |
| Typing `p`, `pd`, `pdf`, `.pdf` 60 ms apart | Only `.pdf` was applied (3,043 matches) |
| `.pdf` inside a folder with no PDFs | "No matching files with measurable allocated space in this folder." with "Go to the scan root" |
| Clear the filter | Full map restored at the same folder with the same selection |
| Refresh with `.pdf` active | New scan filtered as it streamed in |
| Right-click a result | Row and map rectangle selected; menu shows Show in map, Show in Explorer, Copy path |

## Known gaps

- Sorting results re-sorts every match when totals change. With hundreds of thousands of matches during a scan, that is tens of milliseconds per refresh of the visible page.
- Not exercised on Linux or macOS.
