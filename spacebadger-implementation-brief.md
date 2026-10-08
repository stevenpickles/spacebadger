# SpaceBadger — implementation brief

Prepared October 8, 2026. Confirmed application name: SpaceBadger.

## Instructions for the implementing AI

Build a compiled desktop disk-space explorer for personal use, targeting Windows, Linux, and macOS. Use this document as the product specification. Begin with the implementation plan and the filesystem correctness probes described below, then build in milestones. Do not expand the feature set without a clear reason. Keep the interface responsive throughout scanning, searching, and navigation.

Use Rust for the scanner and backend, with a bundled web interface in a desktop webview. Tauri is the recommended shell, subject to verifying the current supported stable release and platform prerequisites at implementation time. This is a desktop application, not a hosted website. No separate Node.js runtime should be needed by the installed application.

The SpaceMonger reference is a visual inspiration: nested, labeled rectangles make a folder hierarchy and its largest files visible at once. This document is self-contained; the original screenshot is not required to implement it. Reproduce the useful interaction and hierarchy, using an original modern interface.

## Product intent and scope

Help the user answer two questions: “What is consuming disk space?” and “What files and folders are here?”

Confirmed requirements:

- Scan a selected folder recursively or an entire drive/volume.
- Support local disks, external disks, and mounted network shares selected as scan roots.
- Show a nested treemap, with area proportional to the selected size metric.
- Make partial results visible and navigable while scanning millions of files.
- Click to select, double-click a folder to zoom, and navigate with breadcrumbs.
- Filter by filename only. Earlier size and date filter ideas were removed.
- Offer “Show in file manager.”
- Support logical size and allocated disk space; default to allocated space.
- Cloud-only files must not be counted as consuming their full logical size locally. Count their locally allocated bytes, if any.
- Include hidden and system files, subject to the user's existing access permissions.
- Exclude symbolic links, directory junctions, and traversal into other mounted filesystems.
- Live scans and manual refresh only.

Proposed implementation defaults, selected to fill remaining design gaps:

- Folder-depth colors by default, with an optional file-type color mode and legend.
- Optional free-space display at a whole-volume root, off by default.
- A compact details panel and virtualized filename search results make tiny and zero-byte items accessible.
- Case-insensitive literal substring filename matching, with no regex or glob syntax in the first version.
- Search the complete selected scan root; retain ancestors of matching files to preserve hierarchy.

Excluded from the first version: deletion, moving or renaming, opening file contents, duplicate detection, saved scans, historical comparisons, filesystem watching, dedicated cloud/network account integrations, AI content analysis, archive inspection, and following links or crossing mount boundaries.

## Primary workflow and interface

1. Choose a folder or drive/volume using native selection controls.
2. Begin scanning immediately and show status, elapsed time, discovered file/folder counts, and measured bytes.
3. Render the first usable nonempty treemap without waiting for the scan to finish. Offer useful results as soon as metadata is available.
4. Let the user select, drill into known folders, use breadcrumbs, change size modes, and search while the scan continues.
5. On completion, mark results complete, or complete with omissions if errors/skips occurred.
6. Manual Refresh scans the same root again. Cancel retains a clearly marked partial result.

Suggested layout:

```text
[Choose folder/drive] [Refresh] [Cancel] [Allocated / Logical]
[Root > Folder > Current folder]     [Filter filenames...]
+--------------------------------------------------------+
| Nested treemap                       | Selected item   |
|                                      | Path            |
|                                      | Both sizes      |
|                                      | Status          |
|                                      | Show in manager |
+--------------------------------------------------------+
| Scanning / Complete / Partial; counts; elapsed; omissions|
+--------------------------------------------------------+
```

Use visible folder borders and labels where there is sufficient space. Large file labels should show filename and formatted size. Hover or selection reveals full filename, full path, both size values, and measurement/state information. Use GiB/MiB/KiB consistently and expose exact byte values in details.

Single-click selects without zooming; double-click a folder changes the displayed root. Double-clicking a file must not launch it. Breadcrumbs jump to ancestors. Provide keyboard selection/navigation, accessible control labels, and search results that do not require interpreting color or hovering.

Selection and navigation use stable node identifiers. Incoming updates must not reset the current folder, query, or selected item. Labels must not spill into neighboring rectangles. Do not inflate tiny items to misleading minimum areas; aggregate subpixel items into an identifiable “Other small items” region that can be explored. Zero-byte entries remain accessible through details/search even though they have no proportional area.

## Filename search

Match only the file's basename, including extension. Do not match full paths or folder names, and do not inspect file contents. Debounce input and run matching/aggregation away from the UI thread. Maintain results as newly discovered files arrive.

With a query active, show only matching files and their ancestor folders in the treemap; recompute folder weights from matching descendants. Display matching bytes/counts distinctly from the unfiltered scan totals. Retain the currently viewed folder; if it has no matches, show an empty state and offer navigation back to the scan root.

Provide a virtualized result list with filename, parent path, and active size, sorted by size descending, with selection/reveal and a route to the file's containing folder in the map. This is a focused search list, not a second full filesystem-browser feature.

Clearing the query restores the unfiltered view. Searching a partial scan must explicitly say results are still being discovered.

## Disk-space accounting

Store logical and allocated sizes separately as integer byte counts. Missing allocation is an explicit unknown value, never zero and never an automatic logical-size fallback.

| Case | Allocated-space mode | Logical-size mode |
|---|---|---|
| Ordinary file | Platform-reported allocated bytes | Reported file length |
| Sparse/compressed file | Reported allocation, not full length | Reported file length |
| Cloud-only placeholder | Reported local allocation, including any reported overhead; often zero | Reported full length |
| Partially/local cached cloud file | Reported local allocation | Reported full length |
| Allocation cannot be determined | Exclude unknown bytes from proportional area; expose unknown count/details | Use known logical length |

Do not read file contents, generate previews, hash files, request hydration, or download remote payloads. Cloud metadata access must use documented nonhydrating methods where available. Where safe enumeration or measurement is unavailable, report the subtree/file as omitted or unknown; do not try potentially hydrating operations to complete the scan. Verify this behavior per platform/provider rather than assuming all providers behave identically.

Directory map weights are sums of included descendant file weights. Do not double-count folder aggregates as additional storage. Directory metadata, filesystem structures, snapshots, shared extents, reserved blocks, and inaccessible files may prevent file totals from matching volume usage; explain the difference in a compact status/detail message.

Hard links require explicit accounting. When stable file identity is available, count allocation once per underlying file within the scan. Attribute allocated bytes to the first encountered included path, keep that ownership stable during the scan, and annotate other aliases as shared/hard-linked without a second allocation. Logical mode may count each directory entry's length; label that behavior. Search must preserve allocation ownership rather than reassigning bytes when an alias matches. If identity is unavailable, label entry-based counting and its possible duplication. Reflinks/clones and filesystem-level deduplication are outside exact unique-byte accounting in this release.

An unknown-size region must not be given invented proportional area. A partial zero/unknown-only scan shows an informative state with accessible entries, not a blank unexplained map.

## Scan boundaries and errors

- Root selection establishes the permitted scan filesystem. A mounted network share can be the selected root, but nested mounts are excluded.
- Do not follow symlinks, junctions, volume mount points, or equivalent redirections. Exclude their targets from counts and weights; report skipped-link counts.
- On Linux, device identity alone is insufficient for same-device bind mounts; use mount identity/table information as needed. Detect equivalent boundaries on macOS and Windows.
- Treat Windows reparse tags by category. Cloud placeholders can be reparse points too; do not skip every reparse point indiscriminately. Exclude link/mount redirections while measuring supported cloud placeholders safely.
- Do not let path canonicalization silently follow links before enforcing these rules. Reject a link root clearly, allowing the user to select the real folder explicitly.
- Hidden/system entries are eligible. Permission-denied paths are reported and scanning continues. Do not auto-elevate privileges.
- Exclude sockets, devices, FIFOs, and other nonregular data sources from file-weight accounting. Never open them for data.
- Files may disappear, grow, or change during a scan. Handle races without crashing; results describe observations over time, not an atomic filesystem snapshot.
- Handle long paths, Unicode, and native non-Unicode filename representations without losing the original actionable path. Display replacement/escaped forms where required.
- A disconnected external drive or share produces a clear partial/error state. Use bounded worker concurrency and document OS-call cancellation limitations; a blocked remote metadata call must not block the interface.

Aggregate omissions by reason, with a bounded sample of paths to avoid millions of UI error records. Distinguish skipped by policy, permission denied, unavailable metadata, disconnected root, and other scan errors.

## Streaming and performance architecture

Design for millions of files from the outset. Do not create one DOM/SVG element per file or resend the complete filesystem tree with every update.

Recommended components:

- A Rust scan engine with platform-specific metadata and mount/cloud adapters.
- A compact authoritative Rust data model storing node IDs, parent IDs, native names/path information, sizes, allocation quality, file identity, and scan states.
- Incremental ancestor aggregation and filename-search queries in the backend.
- A viewport-driven treemap service with bounded display complexity, level of detail, and small-item aggregation.
- Canvas rendering in the webview, with conventional accessible controls/details/search lists around it. Move expensive layout off the UI thread, using a worker or Rust implementation as appropriate.
- Typed, versioned commands and bounded event batches between backend and interface.

Keep names/relative relationships compact; do not duplicate full absolute paths at every node. Use backpressure and coalesced updates instead of one IPC message per discovered file. Aim for useful batches around 100–250 ms, adapt to load, and prioritize the current viewport. Avoid rescanning or rebuilding the complete tree for every ancestor/query update.

Use stable layout/order during scanning to limit rectangle jumping. Partial sizes may grow and ordering may change, but do not continually reshuffle siblings on every file arrival. A final relayout is acceptable if selection and navigation remain intact.

Every scan has a generation ID; every view/query request has a revision. Discard stale updates after cancellation, refresh, root changes, or newer searches. Cancel stops issuing new work promptly, retains discovered data, and never reports completion incorrectly. When refreshing, retaining the prior result is optional if memory permits; old and new generations must never merge.

Avoid fictitious percent-complete or ETAs before total work is known. Report activity, discovered counts, elapsed time, and known bytes. If resources are exhausted, preserve a partial result and explain the limitation instead of silently dropping files.

## Free space and platform integration

The optional free-space rectangle is available only at a whole-volume root with allocation mode and no filename query. Use the OS volume capacity/free-space values. Keep capacity accounting distinct from the discovered-file treemap, for example in a separate volume overview strip, so partial or unmeasurable content is not mislabeled as free space. Disable the proportional free-space view for logical mode, folder roots, and search results.

“Show in file manager” uses the current platform's supported mechanism: reveal/select a file where supported, or open its containing folder otherwise. For directories, reveal the directory where supported or open it. Revalidate paths before the action and report files that no longer exist. Invoke APIs or pass structured arguments; never concatenate a filename into a shell command.

Use bundled local interface assets and narrow desktop capabilities. No file mutations, telemetry, remote scripts, or hosted service are required. This does not mean mounted network scans are disconnected operations; accessing their metadata can involve the network.

## Suggested project organization

Separate scanner/accounting, platform adapters, treemap/layout, desktop bridge, and interface code. Keep core scanning and aggregation testable without a desktop window. Share typed command/event definitions or generate bindings to prevent backend/frontend drift. Pin chosen toolchain/dependency versions after verification.

Build separate native packages for Windows, Linux, and macOS. Cross-platform support does not imply one executable runs on all operating systems. Document webview prerequisites, build/run/package commands, and any platform-specific limitations. A reasonable initial target matrix is Windows x64, Linux x64, and macOS Apple Silicon, with macOS Intel and other architectures added when tested. This architecture matrix is a proposed default, not a user-confirmed restriction.

## Implementation milestones

1. **Correctness probes:** Validate allocation versus length for ordinary, sparse/compressed, and cloud-placeholder files. Verify metadata-only cloud access and mount/link exclusions on available platforms. Record unsupported cases explicitly.
2. **Scanner core:** Implement scoped traversal, stable identities, both metrics, aggregate states/errors, batching, and cancellation. Exercise the core independently of the UI.
3. **Streaming desktop slice:** Select a root, stream a provisional treemap, select/zoom, use breadcrumbs, and cancel while retaining partial results.
4. **Usability:** Add filename filtering/results, details, reveal integration, manual refresh, color choices, empty/error states, and optional volume overview.
5. **Scale and packaging:** Benchmark millions of nodes, tune memory/IPC/layout behavior, verify native integration, and produce packages plus build instructions.

Do not postpone cloud correctness and mount boundaries until after UI polish. They are central acceptance criteria.

## Acceptance and verification

Functional checks:

- A known fixture tree produces correct hierarchy and additive area weights in each metric.
- Partial map appears before scan completion; folder zoom, breadcrumbs, selection, and search work while additional files arrive.
- Cloud-only files with zero reported allocation add zero allocated bytes. A cloud file with nonzero allocation adds only that amount. Logical mode shows its reported length. Scanning does not change hydration state or read payload data.
- A cloud allocation query failure is shown as unknown, never full logical usage. Test at least one supported real provider on Windows and macOS where available; state which providers/platforms were actually validated.
- Hidden/system files are included where accessible. Permission failures remain visible and do not abort unrelated traversal.
- Symlink loops, Windows junctions, nested volumes, and same-device bind mounts do not traverse beyond the selected root filesystem. A share selected as the root is allowed.
- Sparse files and hard links obey the stated accounting rules; aliases and unknown allocation remain understandable in search/details.
- Filename filtering retains ancestors, excludes nonmatching files, updates incrementally, and clearing it restores the map.
- Cancellation, rapid query changes, and refresh/root switches cannot introduce stale results.
- Zero-byte/empty/unknown-only roots and disconnected devices have understandable states.
- Reveal works with spaces, Unicode, and filenames that resemble shell syntax without executing them.

Performance targets to measure, not unsupported promises:

| Check | Initial target |
|---|---|
| First useful visualization | Within 1 second of the first usable metadata batch on a local healthy filesystem |
| Interaction latency while scanning | Most selection/navigation feedback within 100 ms; report slow operations explicitly |
| Visible redraw | At least 30 fps during typical navigation on the documented benchmark machine |
| Scale | Benchmark 1 million and 5 million nodes; no full-tree transfer/render in the interface |
| Memory | Initial goal under 1 GiB total app memory at 5 million nodes on the documented fixture; measure and explain exceptions |
| Cancellation | Stop scheduling new work within 250 ms; interface responds immediately even when an OS request is pending |

Use both synthetic node data for model/layout scaling and real filesystem fixtures for scanning/accounting. Report hardware, OS, storage type, filename distribution, counts, peak memory across app/webview processes, scan throughput, first-render latency, and interaction latency. Include slow network-share behavior separately. Do not claim native cloud support, reveal integration, or packaging on an OS that was not exercised; identify remaining validation work.

## Completion deliverables for the coding AI

- Source repository with clear modules and dependency lockfiles.
- Working desktop application and native packages for the platforms actually validated.
- README with build/run/package steps and webview prerequisites.
- Meaningful core/accounting tests and a reusable scale benchmark.
- Brief validation report listing measured performance and tested filesystem/cloud cases.
- Clearly documented limitations and remaining platform verification, without adding deferred features.

## Technical references

These support the implementation direction; verify the current documentation and exact API behavior before coding.

- [Tauri architecture](https://tauri.app/concept/architecture/) and [process model](https://tauri.app/concept/process-model/): Rust core, webview interface, and desktop platform architecture.
- [Windows GetCompressedFileSizeW](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-getcompressedfilesizew): storage-size queries including compressed/sparse files; evaluate alongside handle-based metadata for placeholder-safe access.
- [Windows CfGetPlaceholderInfo](https://learn.microsoft.com/en-us/windows/win32/api/cfapi/nf-cfapi-cfgetplaceholderinfo) and [CF_PLACEHOLDER_STANDARD_INFO](https://learn.microsoft.com/en-us/windows/win32/api/cfapi/ns-cfapi-cf_placeholder_standard_info): cloud-placeholder metadata candidates. Validate the required handle-opening flags and local-allocation semantics; do not equate a provider field with uniquely reclaimable disk space without testing.
- [Linux stat/lstat documentation](https://man7.org/linux/man-pages/man2/stat.2.html): non-following metadata, device/inode information, and platform-dependent block accounting. Inspect mount APIs/tables separately to catch same-device mount boundaries.
- [Apple volumeIdentifierKey](https://developer.apple.com/documentation/foundation/urlresourcekey/volumeidentifierkey): a starting point for volume identity. Verify current Apple allocation and File Provider metadata APIs on actual macOS installations.
