# Filter patterns

Date: 2026-10-09. Branch `feature/12/filter-patterns`. Windows 11 Pro 10.0.26200, release build.

The filter box used to match one case-insensitive literal substring. The brief left glob syntax out of the first version; it was added at the user's request.

## Syntax

- Patterns are separated by `;` and trimmed; empty ones are ignored.
- A pattern with `*` (any run of characters) or `?` (one character) must match the whole name: `*.txt` matches `a.TXT` but not `a.txt.bak`.
- A pattern without wildcards matches names containing it, as before: `.pdf`, `annual report`.
- A leading `!` excludes. A name is shown when it matches any included pattern (or there are none) and no excluded one, so `!*.tmp` alone shows everything else.
- Matching ignores case and looks only at file names, never folders or paths.
- A query with no patterns (`" ; "`, `"!"`) clears the filter.
- A literal `;`, `*`, `?`, or leading `!` can't be searched for directly; `?` can stand in for it (`a?b` finds `a;b`).

## Implementation

All matching is in `sb_core::search::Matcher`; nothing else changed, including the protocol.

Patterns are lowercased once. Patterns with only `*` are matched by their pieces: a prefix, a suffix, and the middle pieces found in order. Patterns with `?` use a general wildcard matcher that backtracks only to the last star. ASCII names against ASCII patterns are matched bytewise without allocating. A test checks the piece matcher against the general one on every name up to four characters from a mixed-case, non-ASCII alphabet.

## Speed

`sb-bench` on synthetic trees (search over every node, release build):

| Query | 1M nodes | 5M nodes |
|---|---|---|
| `.pdf` | 35.8 ms | 182.9 ms |
| `e` (60% of files match) | 52.6 ms | 275.5 ms |
| `*.jpg; *.png; *.pdf` | 33.9 ms | 176.6 ms |
| `*e*; !*.tmp; !*.log` | 63.5 ms | 333.6 ms |

Before the piece matcher, `*.jpg; *.png; *.pdf` took 87.5 ms and 453.8 ms.

## In the app

On a folder with 12 files in `photos`, `logs`, and `docs`:

| Query | Shown |
|---|---|
| `*.jpg; *.png` | `scan.png`, `IMG_0002.JPG`, `IMG_0001.jpg` |
| `*.log; !debug*` | `app.log` only (not `debug-1.log`) |
| `!*.tmp; !*.bin` | the 10 files other than `cache.tmp` and `other.bin` |
| `report` | `report.docx`, `annual report.pdf` |
| `IMG_????.jpg` | both `IMG_` files |
| ` ; ` | no filter; the whole folder |
| `*.xyz` | none: "No file names match “*.xyz”." |

The map, result list, and match totals agreed in each case.
