# Milestone 9 — interface pass

Date: 2026-10-13. Branch `feature/14/ux-review`. Windows 11 Pro 10.0.26200, NTFS, 200% display scaling, release build, not elevated.

The interface was driven the same way as in milestone 6: screenshots with `PrintWindow` of the webview window, mouse and key messages posted to it. Posted hover and double-click only register while the real cursor is over the window, so the window was moved under it for those checks. Before-and-after screenshots of the same states were compared at the default 1280×800 and at 800×550 and 1900×1150 (logical pixels).

## What changed

- **One size.** The interface measures file size only; the Allocated/Logical switch is gone. The volume overview still compares on-disk usage, and the protocol still offers both.
- **Toolbar.** Grouped into scan actions, view options (with a visible "Color by" label), the filter (syntax popover, inline spinner), and Allow deleting with a shortcuts button pinned right. View options and the filter appear once there is a scan; Volume overview only when a whole drive was scanned.
- **Navigation.** An Up button beside the breadcrumbs; deep paths collapse to the root, "…", and the last three folders. A chip beside them shows an active filter and clears it. `?` opens a shortcuts sheet.
- **Side panel.** Details stay on top at a fixed share of the panel, so the list below doesn't move as the selection changes. Below them, the contents of the folder in the map, largest first (every item, including those merged into small-items regions); selecting a row selects it in the map and vice versa, and Enter opens a folder with the focus staying in the list. Results, small items, and what wasn't counted join as tabs when open; with one list there is no tab strip. Below 800 px wide the panel stacks under the map and scrolls as one.
- **Treemap.** Colors come from theme tokens; labels pick dark or light text per tile, keep the size (compact, e.g. `310M`, on narrow tiles), and sit inside selection frames. Selection, focus, and hover are distinct; the tooltip stays near the pointer and shows the share of the parent folder; the map dims while a new view loads. The legend is a foldable card for both color modes, folded until opened.
- **Lists.** Results and small items share one keyboard-driven list (arrows, Home/End, Shift ranges, Ctrl+Space, Enter, context menu). Result folders are cut in the middle, keeping the start and the last two folders.
- **Details, menus, dialogs.** Details show the share of the parent folder, a Retry when loading fails, and why delete is unavailable. Menus name their target and show the delete entry unavailable, with the reason, until deleting is allowed. The delete dialog is a native modal that lists full paths, keeps its guards (typed `delete`, 3-second countdown, Recycle Bin or Trash only), and ends on a result screen listing every item that couldn't be moved.
- **Plain words.** "Not counted" replaces errors/skipped/omissions, grouped as "Couldn't read" and "Skipped on purpose", each with what to do about it.
- **Screen readers.** One polite live region announces scan phases, search totals, finished actions, and the item reached with the arrow keys; the status bar and details are no longer live regions.

## Results

| Check | Before | After |
|---|---|---|
| Toolbar at 1280 wide | Wrapped, "Allow deleting" alone on a second row | One row |
| Start screen | Empty path bar, two Choose buttons, a ticked disabled checkbox | Heading, one Choose button, shortcuts hint |
| Tile labels | Size cut off first ("libsp… 310 …") | Size kept; compact on narrow tiles |
| Tooltip | Far from the pointer, reflowed when details arrived | 12 px from the pointer, flips at edges, fixed size |
| Filter, then select a result, then clear | Hidden file stayed in the details; selected tile lost its label | Selection cleared with the filter; labels intact |
| Result paths | Cut at the end; "0%" for tiny shares | Cut in the middle; "<0.1%" |
| 800×550 | Fixed 300 px panel squeezed the map | Panel under the map; details not clipped |
| Type colors | Purple folders dominated; "Other" blended into the background | Muted folders; beige "Other" |
| Right-click | (no heading) | Heading names the item; first item focused; Escape closes |
| Side panel after a scan | Details hints, nothing below | Contents of the viewed folder, largest first; list stays in place when an item is selected |
| Delete two dummy files | Toast plus banner | Full paths listed; ✓ when `delete` matches; "Moved 2 items (12.7 KiB)" result screen; map updated |

Type check (`npm run check`) and `npm run build` pass; no Rust changed.

## Not done

- Folder header bars still take a row each in deep trees; their height comes from the Rust layout.
- The legend folds and unfolds only from its arrow button, not its heading.
- The last fixes (backslash wrapping in paths, whole last two folders in result paths, the toolbar rule on a wrapped row) passed the type check but weren't re-screenshotted.
