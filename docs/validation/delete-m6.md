# Milestone 6 — selection and delete

Date: 2026-10-09. Branch `feature/11/select-delete`. Windows 11 Pro 10.0.26200, NTFS, release build, not elevated.

The interface was driven by posting mouse and key messages to the webview window. Posted mouse messages carry Ctrl and Shift in `wParam`, so Ctrl+click and Shift+click were exercised for real; posted key messages can't carry modifiers, so Ctrl+A and Ctrl+Space were not (Delete was).

## Protections

Deleting is deliberately hard to do:

1. **Off until allowed.** An "Allow deleting" switch in the toolbar starts off on every launch and is never saved. While it's off, no menu or panel offers a delete, the Delete key only explains how to turn it on, and the backend refuses `delete_items` regardless of what the interface sends.
2. **Typed confirmation.** The dialog lists what will go (count, sizes, the largest items; folders with their contents, including files a filter hides). Its button stays disabled until `delete` is typed and a 3-second countdown has run.
3. **Never permanent.** There is no permanent delete. On Windows, the app drives the shell itself and aborts any item the shell would delete instead of recycling (network and removable drives, items too large for the Recycle Bin, recycling turned off). On macOS and Linux the `trash` crate always keeps a copy in the Trash (on Linux it copies across filesystems before removing the original).
4. **Re-checked per item.** Each item must still be the kind the scan saw and not a link or junction, and counts as deleted only once it's gone from its place.
5. **Not while scanning.**

## Results

| Check | Result |
|---|---|
| Fresh launch | "Allow deleting" unchecked |
| Delete key with it off | Notice: "Turn on “Allow deleting” in the toolbar to move items to the Recycle Bin." Nothing opened |
| Right-click with it off | Menu has only Show in Explorer and Copy path |
| Turn it on, Delete, type `delete` within the countdown, press Enter | Button still disabled ("Move to Recycle Bin (2)"); nothing happened |
| Wait for the countdown, press Enter | `one.bin` in the Recycle Bin; map and status bar updated; notice "Moved “one.bin” (28.6 MiB) to the Recycle Bin" |
| Scan the same folder as `\\localhost\C$\…` (no Recycle Bin on network paths) and confirm a delete of `two.bin` | Refused: "It can't go to the Recycle Bin here (…), and SpaceBadger never deletes permanently." The file is still on disk and stays selected |
| Unit test against `\\localhost\C$` | The shell reported it would delete without recycling; the delete was aborted and the file survived |

Earlier in this milestone, before the protections:

| Check | Result |
|---|---|
| Click, Ctrl+click in the map | Both items tinted; details panel totals them |
| Click `big1`, Shift+click `mid` | The five items between them in size order, same folder only |
| Ctrl+click a selected folder | Removed from the selection |
| Recycle 4 files, one read-only | All four in the Recycle Bin with their original location |
| A file held open without sharing by another process | Not moved; banner "the move was cancelled or blocked. It may be in use."; no Windows dialog; the file stayed selected |
| Result list: click row 0, scroll, Shift+click row 276 | 277 rows selected, including rows not yet loaded |
| Right-click during a `C:\` scan | Delete actions disabled |

## Automated tests

- `sb-core`: removing a folder takes its sizes off every ancestor and unlinks it; removing a hard-link owner hands its allocation to a remaining alias; removed files leave search matches and totals; a selection resolves to its outermost items with totals.
- `sb-scan`: a finished scan can be edited as a new revision; edits are refused while scanning.
- Desktop app: an item counts as deleted only once it's gone; items that became a different kind, symlinks (Unix), and junctions (Windows) are left alone; Windows refuses a delete the shell wouldn't recycle (network path); the shell path conversion; an end-to-end session test (scan, summarize, delete, status update, search totals, already-gone items) with a stand-in for the trash. Moving a real file to the Recycle Bin is an ignored test, run by hand: `cargo test -p spacebadger recycles_a_local_file -- --ignored` (passed).

## Not covered

- Moving items to the Trash on macOS and Linux.
- The Windows refusal for an item too large for the Recycle Bin, or with recycling turned off; only the network-path case was exercised. All three reach the same check.
- Ctrl+A and Ctrl+Space (see above).
- Elevated deletes of protected files.

## Known limitations

- Recycled items still use space until the Recycle Bin or Trash is emptied. They leave the map at once, but on a whole-drive scan the volume overview counts them as "not attributed" until the next scan.
- A large delete shows "Moving…" until it ends; there's no progress count and it can't be cancelled.
- Deleting one name of a hard-linked file frees nothing while another name remains; the map moves the allocation to the remaining name.
