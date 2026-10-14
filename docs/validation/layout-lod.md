# Layout level of detail

Date: 2026-10-14. Branch `feature/35/layout-lod`. Same machine as [milestone 5](scale-m5.md): AMD Ryzen 9 5900X (24 threads), 64 GiB RAM, Windows 11 Pro 10.0.26200. Release builds.

An architecture review found that `fill` collected and sorted every child of a folder before merging the ones too small to draw, and remembered every child's rank in the order cache. A folder of millions of equal small files draws as one "other small items" region yet was sorted, and its millions of ranks cached, on every layout. The cache was bounded by folder count (100,000), not by how many children it held.

## Change

Whether a child is merged depends only on its own weight: its area, weight × rect area ÷ folder weight, is below `min_area`. It doesn't depend on order, squarifying, or other siblings, so `fill` now decides it per child in one pass, summing the weight and count of the merged ones, and orders only the rest, which can't number more than the area over `min_area` (about 170,000 at 1920 × 1080, 690,000 at 4K). The comparison is the same expression as before, so the boundary case (area exactly `min_area`, drawn) is unchanged. No partial selection is needed: every child above the threshold is drawn, and the rectangle budget applies between folders, as before.

The order cache remembers only the drawn children, as a sorted array of (child, rank) per folder, 8 bytes per child, and holds at most 2^20 children (8 MiB) in all, each folder's own entry counting as four. Past that it forgets the least recently used folders, down to three quarters of the limit so evictions are rare. A folder with nothing drawn on its own isn't remembered, and one with more drawn children than the whole budget is sorted afresh each time. Node IDs are never reused within a scan, and each scan and each filter has its own cache, as before.

## Benchmark

```text
cargo run --release -p sb-bench -- --lod 2000000 --lod 5000000
```

`--lod N` builds a folder of N files of 4 KiB beside eight large files, and a synthetic mixed tree of N nodes, and lays them out at 1920 × 1080. Cache memory is measured exactly with a counting allocator: the live heap bytes added by layouts into one cache, with the layouts themselves dropped. "2,001 views" lays out the root and then the 2,000 folders with the most children into one cache, as a user browsing would. Two runs each, alternating before and after; ranges where they differ.

| Case | Measure | Before | After |
|---|---|---|---|
| 2M equal files | Final layout (fresh cache) | 168–174 ms | 3.5–3.6 ms |
| | Stable relayout (while scanning) | 196–298 ms | 3.1–3.4 ms |
| | Order cache | 36 MiB | 9 children, under 1 KiB |
| 5M equal files | Final layout (fresh cache) | 523–675 ms | 8.1–8.7 ms |
| | Stable relayout | 0.93–1.88 s | 8.3–8.5 ms |
| | Order cache | 72 MiB | 9 children, under 1 KiB |
| Mixed tree, 2M nodes | Final layout | 19–22 ms | 1.4–1.6 ms |
| | Order cache after one layout | 4 MiB | 88 KiB (2,858 children) |
| | 2,001 views | 62–89 ms, cache 13 MiB | 28–31 ms, cache 4 MiB (447,796 children) |
| Mixed tree, 5M nodes | Final layout | 27–33 ms | 2.2–4.3 ms |
| | Order cache after one layout | 7 MiB | 163 KiB (4,086 children) |
| | 2,001 views | 161–187 ms, cache 27 MiB | 44–64 ms, cache 6 MiB (746,225 children) |

What remains per layout is reading each child's weight once; for the 5M folder that's the 8 ms above.

## Same output

`crates/sb-core/src/layout.rs` keeps the previous implementation as a test-only reference (`tests::reference`) and compares whole layouts, every rectangle's position, size, flags, weight, and count, for equality:

- `final_layouts_match_the_reference`: 40 random trees, each with nested folders in every state (pending, failed, not scanned, listed, partly read), cloud and hard-linked flags, unknown allocations, zero sizes, many ties, and wide folders of 2,000–5,000 equal files; each with no filter and five filters (patterns, exclusions, `dir:`, size, and one matching nothing), both metrics, and random views and parameters (including empty and tiny views, `min_area` of 0 to 300, and budgets that truncate). For each, a fresh stable layout and two final layouts must match. A final layout sorts afresh, so it is identical by construction: filtering a sorted list keeps its order.
- `stable_layouts_match_the_reference_when_every_child_is_drawn`: 30 random trees grown step by step, as a scan does, with stable layouts in between and a final one at the end, must match at every step when no child is small.
- `a_child_of_exactly_the_minimum_area_is_drawn_as_before` and `a_wide_folder_of_equal_files_orders_only_what_is_drawn` (200,000 equal files: identical output, and the cache holds the root's two children).

Changing the threshold by 1% or turning `<` into `<=` fails these tests, as does dropping the stable order or not remembering new children.

The map has no color input: change colors are applied by the interface to the rectangles it receives, so they're covered by the rectangles matching.

## What changed on purpose

While scanning, the reorder hysteresis used to compare every sibling, drawn or not, in their remembered order. It now compares drawn siblings only. Before, a new tiny file 1.5 times the size of the last small one known, or two tiny files swapping order, re-sorted the whole folder, visible siblings included; now only drawn siblings outgrowing their predecessor do. A child that was too small to draw in the previous layout has no remembered rank and is placed as a new one, after the known ones and subject to the same hysteresis. Final layouts, and the first layout of any folder, are unaffected. `small_children_changing_order_does_not_reorder_the_drawn_ones` covers this.

## Not covered

- `small_children` (the list of merged items in the details) still sorts all of a folder's children, but only when asked, for one folder.
- Each layout still reads every child's weight in each folder it fills; a folder of 5M files costs about 8 ms.
