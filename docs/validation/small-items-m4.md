# Milestone 4 — exploring small items

Date: 2026-10-09. Branch `feature/9/small-items`.

## What exists

- **Membership (`sb-core::layout::small_children`):** a small-items region holds its folder's smallest children with a nonzero size, because the layout merges every child below the area threshold. The region's folder and item count therefore identify its members without keeping the layout around.
  - With a filter, only children containing matches count, and sizes are matching bytes.
  - Children with no size in the current measure are counted separately, since the map never draws them.
- **Bridge:** `small_items` returns members largest first, in pages of at most 500.
- **Interface:** selecting a small-items region opens a list under the details.
  - Rows show the name and size, mark folders, and give folders an "Open" button.
  - The list loads 200 at a time with "Show N more".
  - It notes how many other items have no size, and that the filter finds files by name.
  - It stays open while its items are selected, so they can be stepped through. It closes with ✕, or when the view, size measure, filter, or scan changes.

## Verified on Windows

Debug build, `C:\Users\Steven\Downloads` (28,279 files), clicks posted to the webview, checked with screenshots:

| Check | Result |
|---|---|
| Select the root's small-items region | "706 small items", 125 MiB combined; list from 848 KiB down, folders marked with Open |
| Click the first item | Details show the file; the list stays open with it highlighted |
| "Show 200 more" twice | "Show 106 more"; a third click loads the rest and the button goes away |
| Items without size | "3 other items here have no allocated size…" |

The first version asked for 400, then 600 items in one request, and the backend's 500-row cap stopped it at 500. It now loads 200-item pages.

## Known gaps

- Rows can't be reached with arrow keys; each row is a button, so Tab moves through them.
- If the list is closed while the region is still selected, the details still say the items are listed below.
