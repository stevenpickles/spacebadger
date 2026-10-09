# Milestone 6 — selection and delete

Date: 2026-10-09. Branch `feature/11/select-delete`. Windows 11 Pro 10.0.26200, NTFS, release build, not elevated.

The interface was driven by posting mouse and key messages to the webview window. Posted mouse messages carry Ctrl and Shift in `wParam`, so Ctrl+click and Shift+click were exercised for real; posted key messages can't carry modifiers, so Ctrl+A, Ctrl+Space, and Shift+Delete were not (Delete was).

## Map

Fixture folder: `big1.bin` 40 MiB, `big2.bin` 30 MiB, `big3.bin` 20 MiB, `mid.bin` 10 MiB (read-only), `folderA` (15 + 5 MiB), `folderB` (8 MiB).

| Action | Result |
|---|---|
| Click `big1`, Ctrl+click `big3` | Both tinted; details panel: "2 items selected", 60.0 MiB |
| Click `big1`, Shift+click `mid` | The five items between them in size order: `big1`, `big2`, `big3`, `folderA`, `mid` (120 MiB); `folderB` not included |
| Ctrl+click `folderA` | Removed from the selection: 4 files, 100 MiB |
| Delete key | Dialog "Move 4 items to the Recycle Bin?", listing all four, focus on the confirm button |
| Enter | All four in the Recycle Bin (checked through the shell's Recycle Bin folder, with their original location), including the read-only file. Map and status bar: 3 files, 2 folders, 28.0 MiB. Notice "Moved 4 items (100 MiB) to the Recycle Bin" |
| Right-click `b1.bin` → Delete permanently… | Dialog with the warning "This can't be undone", focus on Cancel |
| Enter | Cancelled; the file is still on disk |
| Select `folderA` → details → Delete permanently… → confirm | Folder and contents gone from disk and not in the Recycle Bin; status bar 1 file, 1 folder, 8.00 MiB |
| Another process opens `b1.bin` without sharing; Move to Recycle Bin | Not deleted. Banner: "Couldn't delete 1 item: …b1.bin: Couldn't move it to the Recycle Bin: the move was cancelled or blocked. It may be in use." No Windows dialog appeared; the file stayed selected |
| Right-click during a `C:\` scan | Both delete entries disabled in the menu and in the details panel |

## Result list

Fixture: 400 `.log` files from 3.13 MiB down to 8 KiB, filter `log`.

| Action | Result |
|---|---|
| Click the first row, scroll to row 273 with the mouse wheel, Shift+click row 276 | 277 items selected (rows 0–276), 567 MiB, including rows that hadn't been loaded |
| Details → Delete permanently… → confirm | 277 files gone from disk; 123 matches remain; status bar 124 files |

## Automated tests

- `sb-core`: removing a folder takes its sizes off every ancestor and unlinks it; removing the owner of a hard link hands its allocation to a remaining alias; removed files leave search matches and totals; a selection resolves to its outermost items with totals.
- `sb-scan`: a finished scan can be edited as a new revision, and edits are refused while scanning.
- Desktop app: permanent deletion of files and folders (including a read-only file inside a folder), items that have become a different kind are left alone, a folder holding a junction (Windows) or symlink (Unix) loses the link but not its target, and an end-to-end session test (scan, summarize, delete, status update, search totals, already-gone items).
- Linux (Docker, `rust:1.97` with the webview packages): `cargo clippy --workspace --all-targets` clean and `cargo test --workspace` passing, including the symlink test and the session test.

## Not covered

- The Trash on macOS and Linux: the code builds and the permanent-delete tests run on Linux, but nothing was moved to a Linux or macOS trash.
- Windows' prompt for items that can't go to the Recycle Bin (too large, or on a volume without one, such as most network shares). The shell is asked to warn before deleting permanently; this was not triggered.
- Ctrl+A, Ctrl+Space, and Shift+Delete (see above). Shift+Delete calls the same handler as the Delete key with `permanent` set.
- Elevated deletes of protected files.

## Known limitations

- Deleting waits for the scan to finish or be cancelled: the scanner may still be adding items to a folder being deleted.
- Recycled items still use space until the Recycle Bin or Trash is emptied. They leave the map at once, but on a whole-drive scan the volume overview then counts them as "not attributed" until the next scan.
- A large delete shows "Deleting…" until it ends; there's no progress count and it can't be cancelled.
- Deleting one name of a hard-linked file frees nothing while another name remains; the map moves the allocation to the remaining name.
