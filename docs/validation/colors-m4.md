# Milestone 4 — color modes

Date: 2026-10-09. Branch `feature/7/color-modes`.

## What exists

- **Depth colors** (default): each nesting level gets its own hue; folders are darker than files.
- **File type colors:** files are colored by type and folders are indigo, a hue no type uses. A legend under the map names every color, so no type has to be guessed from color alone.
  - Types come from the extension only (`sb-core::filetype`), case-insensitively: video, audio (including `.m4b` audiobooks), images, documents, archives, disk images, programs, code, and other.
  - `.ts` is shared by TypeScript and MPEG transport streams; files of 1 MiB or more count as video.
  - The type travels in the previously reserved byte of each rectangle in the layout wire format, which is now version 2.
  - Hovering a file in this mode adds its type to the tooltip.
- The choice is a toolbar radio group and is remembered on the device in local storage. Without storage, depth colors apply.

## Verified on Windows

Debug build, `C:\Users\Steven\Downloads` (28,278 files), switched with UI Automation and checked with screenshots:

| Check | Result |
|---|---|
| Switch to file type colors | Ubuntu `.iso` files pink (disk images), KiCad `.exe` installers yellow (programs); legend shown |
| MasterClass lessons (`.ts`, 115–610 MiB) | Video after the size rule; Code before it |
| `.run` installer (no listed extension) | Off-white "Other", distinct from gray small items |
| Relaunch | File type colors still selected |

## Known gaps

- The legend lists colors only; it doesn't show bytes per type.
- Extensions not in the table are "Other"; the table is deliberately short.
