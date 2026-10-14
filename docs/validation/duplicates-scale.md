# Duplicates at scale

Date: 2026-10-14. Branch `feature/33/duplicate-pages`. Same machine as [milestone 5](scale-m5.md): AMD Ryzen 9 5900X (24 threads), 64 GiB RAM, Samsung 980 PRO NVMe, Windows 11 Pro 10.0.26200, NTFS. Release builds.

A review found that the duplicates list capped the number of groups (5,000) but not their files. One group of a folder's worth of identical generated files was sent whole: every path built and serialized, and a row made for each in the interface. Each status update (every 250 ms while reading) also copied every group with its files still in the scan and sorted them, just to count them.

## Benchmark

```text
cargo test --release -p spacebadger duplicates_at_scale -- --ignored --nocapture
```

The test builds a tree with one group of 200,000 identical 1,500-byte files (in 200 folders of 1,000) and 20,000 groups of two 2 MB photos, records them as found, and times the backend's calls. Reading files isn't part of it: that was already bounded. The list is timed with its JSON serialization, which is what Tauri sends. Three runs before, two after; ranges shown.

| Measure | Before | After |
|---|---|---|
| Status, each (sent every 250 ms while reading) | 23–31 ms | 0.14–0.18 µs |
| Status, first after the groups are found | (same) | 5.5–6.7 ms |
| Status, first after a delete | (same) | 4.7–5.2 ms |
| List and JSON, each | 157–163 ms | 6.5–7.2 ms |
| List, first after the groups are found (orders the large group by path once) | (same) | 102–114 ms |
| List size | 5,000 groups, 209,998 files, 21.3 MB of JSON | 5,000 groups, 10,000 files, 1.2 MB of JSON |
| A page of 1,000 files of the large group, with JSON | (no paging) | 0.48–0.52 ms, 102 KB |
| Select all but the first: extras' nodes, with JSON | (built in the interface from the list) | 3.1–4.3 ms, 204,998 nodes, 1.3 MB |

Before, the interface then made 214,999 rows (a header and a row per file) from the 21 MB list, each time the list was fetched again: on every new group while reading, and after each delete. After, it makes rows only for the files it has fetched: 15,000 at most for the list (5,000 headers and 10,000 files), plus 1,000 per **Show more**, plus a "Show more" row per group with more.

### What was changed

1. **Stable group numbers.** Groups are kept in the order they were found and never renumbered while their search is current, so `duplicate_files` can page a group's files by number. A request for a search that has been replaced, or a scan that has been, gets the existing "no longer current" error.
2. **A file budget.** The list carries each group's file count and at most its first 100 files, within 10,000 in all; each listed group keeps at least its first file, for its name. The rest come 1,000 at a time.
3. **Ordering once.** A group's files are ordered by path (shortest first) when first needed and kept as indexes, 4 bytes per file, rather than building and sorting every path on every list. Deleted files are dropped from the order; nothing else changes it.
4. **Running totals.** Each group's figures (files still in the scan, content in the extra copies, the on-disk estimate) and the totals are added as groups are found, and worked out again only when the scan's revision changes, which is when items are deleted. A list picks its 5,000 groups with a partial select before sorting just those.
5. **Selecting without listing.** "Select all but the first" asks the backend for the extras' node numbers instead of reading them from the list, so it still covers every file of every listed group, shown or not. The selection is still of files selected themselves, so the delete's protection is unchanged: those files are examined, and the copies staying confirmed, before anything moves.

## Desktop app

Release build (`npx tauri build --no-bundle`), on a folder of 1,200 identical 1 MB files and five pairs of 2 MB files, driven by posted window messages:

- The search reads 1.34 GB in about a second. The large group's header says "1,200 copies", its first 100 files are listed, then "Show 1,000 more of 1,100"; clicking it adds the next 1,000 rows, and a further row offers the last 100.
- "Select all but the first", before any more were shown, selected 1,204 files (1,199 + 5), 1.18 GB, and the map highlighted every one; the files fetched afterwards showed as selected.
- Deleting one file from among those fetched by Show more (with Allow deleting on, in the test folder only) removed it from the expanded group without folding it: the files shown were fetched again, and the totals dropped to 1,209 files.
