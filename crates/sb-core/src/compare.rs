//! Compares a scan with a [`Snapshot`] of an earlier scan of the same root.
//!
//! Items are matched by their path relative to the root, one folder at a
//! time: each current folder's children are looked up by name among its
//! earlier counterpart's children. A [`Comparison`] keeps the result as one
//! number per current node, so asking about any node afterwards is a lookup.
//!
//! An item that isn't found was added since, unless it is a file that was
//! below the snapshot's file threshold then: those are recorded only by
//! name hash, so the comparison knows the file existed but not its size,
//! and says nothing about it. A renamed item counts as removed under its old
//! name and new under its new one.

use crate::snapshot::{Snapshot, fold, name_hash};
use crate::tree::{NodeId, Tree};
use std::cmp::Reverse;

const NEW: u32 = u32::MAX - 1;
const UNKNOWN: u32 = u32::MAX;

/// Where a current node stands against the snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// The same item, as snapshot entry `n`.
    Was(u32),
    /// Not there before.
    New,
    /// Its earlier size isn't known: a file that was below the threshold,
    /// or a node the comparison doesn't cover.
    Unknown,
}

/// An item whose size changed, for the list of changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Item {
    /// In the current scan.
    Current(NodeId),
    /// Gone since: a snapshot entry with no current counterpart.
    Removed(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Change {
    pub item: Item,
    /// Logical bytes then; `None` if new.
    pub before: Option<u64>,
    /// Logical bytes now; `None` if removed.
    pub after: Option<u64>,
}

impl Change {
    /// Bytes gained (positive) or lost.
    pub fn delta(&self) -> i128 {
        i128::from(self.after.unwrap_or(0)) - i128::from(self.before.unwrap_or(0))
    }
}

/// How much a size changed relative to where it was, from 0 (under 5%) to
/// 4 (at least doubled, or emptied).
pub fn change_level(before: u64, after: u64) -> u8 {
    let ratio = before.abs_diff(after) as f64 / before.max(1) as f64;
    match ratio {
        r if r < 0.05 => 0,
        r if r < 0.2 => 1,
        r if r < 0.5 => 2,
        r if r < 1.0 => 3,
        _ => 4,
    }
}

/// A scan matched against a snapshot.
#[derive(Debug, Clone)]
pub struct Comparison {
    /// Per current node: its snapshot entry, [`NEW`], or [`UNKNOWN`].
    matched: Vec<u32>,
    /// Per snapshot entry: whether a current node matched it.
    seen: Vec<bool>,
    /// Snapshot children by parent, as ranges of `children`.
    child_start: Vec<u32>,
    children: Vec<u32>,
}

impl Comparison {
    /// Matches every node of `tree` with `snap`, comparing names as the
    /// snapshot was taken (ignoring case on Windows).
    pub fn new(tree: &Tree, snap: &Snapshot) -> Self {
        let insensitive = snap.header().case_insensitive;
        let n = snap.len();
        // Children by parent (counting sort, so entries stay in order).
        let mut child_start = vec![0u32; n + 1];
        for e in 1..n as u32 {
            if let Some(p) = snap.parent(e) {
                child_start[p as usize + 1] += 1;
            }
        }
        for i in 0..n {
            child_start[i + 1] += child_start[i];
        }
        let mut fill = child_start.clone();
        let mut children = vec![0u32; child_start[n] as usize];
        for e in 1..n as u32 {
            if let Some(p) = snap.parent(e) {
                children[fill[p as usize] as usize] = e;
                fill[p as usize] += 1;
            }
        }

        // Folded names of the snapshot's entries, when case is ignored.
        let mut keys = Vec::new();
        let mut key_start = Vec::new();
        if insensitive {
            key_start.reserve(n + 1);
            let mut buf = Vec::new();
            for e in 0..n as u32 {
                key_start.push(keys.len());
                let folded = fold(snap.name(e), true, &mut buf);
                keys.extend_from_slice(folded);
            }
            key_start.push(keys.len());
        }
        let key = |e: u32| -> &[u8] {
            if insensitive {
                &keys[key_start[e as usize]..key_start[e as usize + 1]]
            } else {
                snap.name(e)
            }
        };

        let mut matched = vec![UNKNOWN; tree.len()];
        let mut seen = vec![false; n];
        matched[NodeId::ROOT.index()] = 0;
        seen[0] = true;
        let mut pairs = vec![(NodeId::ROOT, 0u32)];
        let mut added = Vec::new();
        let mut buf = Vec::new();
        while let Some((dir, entry)) = pairs.pop() {
            let range =
                child_start[entry as usize] as usize..child_start[entry as usize + 1] as usize;
            let kids = &mut children[range];
            kids.sort_unstable_by(|&a, &b| key(a).cmp(key(b)));
            for c in tree.children(dir) {
                let name = fold(tree.name(c).as_encoded_bytes(), insensitive, &mut buf);
                let found = kids
                    .binary_search_by(|&k| key(k).cmp(name))
                    .ok()
                    .map(|i| kids[i]);
                let is_dir = tree.is_dir(c);
                matched[c.index()] = match found {
                    // Two current names folded to one earlier name: only
                    // the first can be that item.
                    Some(s) if seen[s as usize] => UNKNOWN,
                    Some(s) if snap.is_dir(s) == is_dir => {
                        seen[s as usize] = true;
                        if is_dir {
                            pairs.push((c, s));
                        }
                        s
                    }
                    // A folder where a file was, or the reverse.
                    Some(_) => NEW,
                    None if !is_dir && snap.had_small_file(entry, name_hash(name)) => UNKNOWN,
                    None => NEW,
                };
                if is_dir && matched[c.index()] == NEW {
                    added.push(c);
                }
            }
        }
        // Everything in a new folder is new too.
        while let Some(dir) = added.pop() {
            for c in tree.children(dir) {
                matched[c.index()] = NEW;
                if tree.is_dir(c) {
                    added.push(c);
                }
            }
        }
        Self {
            matched,
            seen,
            child_start,
            children,
        }
    }

    pub fn status(&self, node: NodeId) -> Status {
        match self.matched.get(node.index()).copied().unwrap_or(UNKNOWN) {
            NEW => Status::New,
            UNKNOWN => Status::Unknown,
            entry => Status::Was(entry),
        }
    }

    /// The change at `node`, or `None` if its earlier size isn't known.
    /// Unchanged items give a change with equal sizes.
    pub fn change(&self, tree: &Tree, snap: &Snapshot, node: NodeId) -> Option<Change> {
        let before = match self.status(node) {
            Status::Was(e) => Some(snap.logical(e)),
            Status::New => None,
            Status::Unknown => return None,
        };
        Some(Change {
            item: Item::Current(node),
            before,
            after: Some(tree.logical(node)),
        })
    }

    fn snapshot_children(&self, entry: u32) -> &[u32] {
        let e = entry as usize;
        &self.children[self.child_start[e] as usize..self.child_start[e + 1] as usize]
    }

    /// Every item under `folder`, at any depth, whose size changed: grown,
    /// shrunk, new, or removed (from the snapshot), largest change first.
    /// With `files_only`, folders are left out. Items of unknown earlier
    /// size and empty new or removed items are left out.
    pub fn changes_under(
        &self,
        tree: &Tree,
        snap: &Snapshot,
        folder: NodeId,
        files_only: bool,
    ) -> Vec<Change> {
        let mut out = Vec::new();
        let mut stack: Vec<NodeId> = tree.children(folder).collect();
        while let Some(n) = stack.pop() {
            let is_dir = tree.is_dir(n);
            if is_dir {
                stack.extend(tree.children(n));
                if files_only {
                    continue;
                }
            }
            if let Some(c) = self.change(tree, snap, n)
                && c.delta() != 0
            {
                out.push(c);
            }
        }
        if let Status::Was(entry) = self.status(folder) {
            let mut stack = self.snapshot_children(entry).to_vec();
            while let Some(e) = stack.pop() {
                let is_dir = snap.is_dir(e);
                if is_dir {
                    stack.extend_from_slice(self.snapshot_children(e));
                }
                if self.seen[e as usize] || (files_only && is_dir) || snap.logical(e) == 0 {
                    continue;
                }
                out.push(Change {
                    item: Item::Removed(e),
                    before: Some(snap.logical(e)),
                    after: None,
                });
            }
        }
        // Equal changes: items that were there before, then new ones, then
        // removed ones; each in scan order, so pages stay put.
        out.sort_unstable_by_key(|c| {
            let (removed, id) = match c.item {
                Item::Current(n) => (false, n.index() as u32),
                Item::Removed(e) => (true, e),
            };
            (
                Reverse(c.delta().unsigned_abs()),
                c.before.is_none(),
                removed,
                id,
            )
        });
        out
    }

    /// The path of snapshot entry `entry` below `ancestor` (an ancestor
    /// entry), as display names; empty when `entry` is `ancestor`.
    pub fn snapshot_path(snap: &Snapshot, ancestor: u32, entry: u32) -> Vec<String> {
        let mut names = Vec::new();
        let mut cur = Some(entry);
        while let Some(e) = cur {
            if e == ancestor {
                break;
            }
            names.push(snap.display_name(e));
            cur = snap.parent(e);
        }
        names.reverse();
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::size::Allocated;
    use crate::snapshot::FILE_THRESHOLD;
    use crate::tree::FileSizes;

    const MB: u64 = 1 << 20;

    fn sizes(logical: u64) -> FileSizes {
        FileSizes {
            logical,
            allocated: Allocated::known(logical),
        }
    }

    fn find(tree: &Tree, path: &str) -> NodeId {
        let mut n = NodeId::ROOT;
        for part in path.split('/') {
            n = tree
                .children(n)
                .find(|&c| tree.name(c) == part)
                .unwrap_or_else(|| panic!("no {path}"));
        }
        n
    }

    /// Before: docs/{report.pdf 3 MB, notes.txt 1 KB}, media/{a.mkv 10 MB,
    /// old.mkv 4 MB}, tmp/{cache.bin 2 MB}, small.log 100 B.
    fn before() -> Tree {
        let mut t = Tree::new("/data");
        let docs = t.add_dir(NodeId::ROOT, "docs".as_ref()).unwrap();
        let media = t.add_dir(NodeId::ROOT, "media".as_ref()).unwrap();
        let tmp = t.add_dir(NodeId::ROOT, "tmp".as_ref()).unwrap();
        t.add_file(docs, "report.pdf".as_ref(), sizes(3 * MB), 0)
            .unwrap();
        t.add_file(docs, "notes.txt".as_ref(), sizes(1024), 0)
            .unwrap();
        t.add_file(media, "a.mkv".as_ref(), sizes(10 * MB), 0)
            .unwrap();
        t.add_file(media, "old.mkv".as_ref(), sizes(4 * MB), 0)
            .unwrap();
        t.add_file(tmp, "cache.bin".as_ref(), sizes(2 * MB), 0)
            .unwrap();
        t.add_file(NodeId::ROOT, "small.log".as_ref(), sizes(100), 0)
            .unwrap();
        t
    }

    /// After: report.pdf unchanged, notes.txt grew to 5 MB, a.mkv grew to
    /// 12 MB, old.mkv renamed to older.mkv, tmp removed, new/{b.iso 8 MB},
    /// small.log still small, fresh.txt new and small.
    fn after() -> Tree {
        let mut t = Tree::new("/data");
        let docs = t.add_dir(NodeId::ROOT, "docs".as_ref()).unwrap();
        let media = t.add_dir(NodeId::ROOT, "media".as_ref()).unwrap();
        let new = t.add_dir(NodeId::ROOT, "new".as_ref()).unwrap();
        t.add_file(docs, "report.pdf".as_ref(), sizes(3 * MB), 0)
            .unwrap();
        t.add_file(docs, "notes.txt".as_ref(), sizes(5 * MB), 0)
            .unwrap();
        t.add_file(media, "a.mkv".as_ref(), sizes(12 * MB), 0)
            .unwrap();
        t.add_file(media, "older.mkv".as_ref(), sizes(4 * MB), 0)
            .unwrap();
        t.add_file(new, "b.iso".as_ref(), sizes(8 * MB), 0).unwrap();
        t.add_file(NodeId::ROOT, "small.log".as_ref(), sizes(200), 0)
            .unwrap();
        t.add_file(NodeId::ROOT, "fresh.txt".as_ref(), sizes(10), 0)
            .unwrap();
        t
    }

    #[test]
    fn matches_grown_new_removed_and_renamed_items() {
        let snap = Snapshot::capture(&before(), 0, false);
        let tree = after();
        let cmp = Comparison::new(&tree, &snap);

        let c = |p: &str| cmp.change(&tree, &snap, find(&tree, p));
        let root = cmp.change(&tree, &snap, NodeId::ROOT).unwrap();
        assert_eq!(root.before, Some(19 * MB + 1124));
        assert_eq!(root.after, Some(32 * MB + 210));

        assert_eq!(c("docs/report.pdf").unwrap().delta(), 0);
        assert_eq!(c("media/a.mkv").unwrap().before, Some(10 * MB));
        assert_eq!(c("new").unwrap().before, None, "new folder");
        assert_eq!(
            cmp.status(find(&tree, "new/b.iso")),
            Status::New,
            "inside a new folder"
        );
        assert_eq!(
            cmp.status(find(&tree, "media/older.mkv")),
            Status::New,
            "a rename is new under the new name"
        );
        assert_eq!(cmp.status(find(&tree, "fresh.txt")), Status::New);
        // Below the threshold before: existed, size unknown, say nothing.
        assert_eq!(c("small.log"), None);
        assert_eq!(c("docs/notes.txt"), None, "grew past the threshold");
        const { assert!(1024 < FILE_THRESHOLD) };

        let changes = cmp.changes_under(&tree, &snap, NodeId::ROOT, false);
        let sizes: Vec<u128> = changes.iter().map(|c| c.delta().unsigned_abs()).collect();
        assert!(sizes.windows(2).all(|w| w[0] >= w[1]), "largest first");
        let mut described: Vec<_> = changes
            .iter()
            .map(|c| {
                let name = match c.item {
                    Item::Current(n) => tree.name(n).to_string_lossy().into_owned(),
                    Item::Removed(e) => format!("-{}", snap.display_name(e)),
                };
                (name, c.delta())
            })
            .collect();
        described.sort();
        let mb = |n: i128| n * MB as i128;
        let expected: Vec<(String, i128)> = [
            ("-cache.bin", mb(-2)),
            ("-old.mkv", mb(-4)),
            ("-tmp", mb(-2)),
            ("a.mkv", mb(2)),
            ("b.iso", mb(8)),
            ("docs", mb(5) - 1024),
            ("fresh.txt", 10),
            ("media", mb(2)),
            ("new", mb(8)),
            ("older.mkv", mb(4)),
        ]
        .into_iter()
        .map(|(n, d)| (n.to_owned(), d))
        .collect();
        assert_eq!(described, expected);

        let files = cmp.changes_under(&tree, &snap, NodeId::ROOT, true);
        assert!(files.iter().all(|c| match c.item {
            Item::Current(n) => !tree.is_dir(n),
            Item::Removed(e) => !snap.is_dir(e),
        }));
        assert_eq!(files.len(), 6, "four files now, two gone");

        let media = find(&tree, "media");
        let under = cmp.changes_under(&tree, &snap, media, false);
        assert_eq!(under.len(), 3, "a.mkv, old.mkv, older.mkv");
        let Item::Removed(old) = under.iter().find(|c| c.after.is_none()).unwrap().item else {
            unreachable!()
        };
        let Status::Was(media_entry) = cmp.status(media) else {
            unreachable!()
        };
        assert_eq!(
            Comparison::snapshot_path(&snap, 0, old),
            ["media", "old.mkv"]
        );
        assert_eq!(
            Comparison::snapshot_path(&snap, media_entry, old),
            ["old.mkv"]
        );
    }

    #[test]
    fn case_matters_only_where_the_snapshot_says_so() {
        let mut t = Tree::new("/data");
        let d = t.add_dir(NodeId::ROOT, "Photos".as_ref()).unwrap();
        t.add_file(d, "IMG.JPG".as_ref(), sizes(2 * MB), 0).unwrap();
        t.add_file(d, "Small.txt".as_ref(), sizes(5), 0).unwrap();
        let mut renamed = Tree::new("/data");
        let d = renamed.add_dir(NodeId::ROOT, "photos".as_ref()).unwrap();
        renamed
            .add_file(d, "img.jpg".as_ref(), sizes(3 * MB), 0)
            .unwrap();
        renamed
            .add_file(d, "small.TXT".as_ref(), sizes(5), 0)
            .unwrap();

        let insensitive = Snapshot::capture(&t, 0, true);
        let cmp = Comparison::new(&renamed, &insensitive);
        let img = find(&renamed, "photos/img.jpg");
        assert_eq!(cmp.status(img), Status::Was(2));
        assert_eq!(
            cmp.change(&renamed, &insensitive, img).unwrap().delta(),
            MB as i128
        );
        assert_eq!(
            cmp.status(find(&renamed, "photos/small.TXT")),
            Status::Unknown
        );
        assert!(
            cmp.changes_under(&renamed, &insensitive, NodeId::ROOT, false)
                .iter()
                .all(|c| c.after.is_some()),
            "nothing removed"
        );

        let sensitive = Snapshot::capture(&t, 0, false);
        let cmp = Comparison::new(&renamed, &sensitive);
        assert_eq!(cmp.status(find(&renamed, "photos")), Status::New);
        assert_eq!(cmp.status(img), Status::New);
        let removed = cmp
            .changes_under(&renamed, &sensitive, NodeId::ROOT, false)
            .iter()
            .filter(|c| c.after.is_none())
            .count();
        assert_eq!(removed, 2, "Photos and IMG.JPG");
    }

    #[test]
    fn a_folder_replacing_a_file_is_new() {
        let mut t = Tree::new("/data");
        t.add_file(NodeId::ROOT, "x".as_ref(), sizes(2 * MB), 0)
            .unwrap();
        let mut now = Tree::new("/data");
        let x = now.add_dir(NodeId::ROOT, "x".as_ref()).unwrap();
        let snap = Snapshot::capture(&t, 0, false);
        let cmp = Comparison::new(&now, &snap);
        assert_eq!(cmp.status(x), Status::New);
        let changes = cmp.changes_under(&now, &snap, NodeId::ROOT, false);
        assert_eq!(changes.len(), 1, "the empty new folder isn't a change");
        assert_eq!(changes[0].item, Item::Removed(1));
    }

    #[test]
    fn deleted_nodes_drop_out_when_recomputed() {
        let snap = Snapshot::capture(&before(), 0, false);
        let mut tree = before();
        let media = find(&tree, "media");
        assert!(
            Comparison::new(&tree, &snap)
                .changes_under(&tree, &snap, NodeId::ROOT, false)
                .is_empty()
        );
        tree.remove(media);
        let cmp = Comparison::new(&tree, &snap);
        let changes = cmp.changes_under(&tree, &snap, NodeId::ROOT, false);
        assert_eq!(changes[0].delta(), -(14 * MB as i128));
        assert_eq!(cmp.status(media), Status::Unknown);
    }

    #[test]
    fn levels_follow_the_relative_change() {
        assert_eq!(change_level(100, 101), 0);
        assert_eq!(change_level(100, 110), 1);
        assert_eq!(change_level(100, 70), 2);
        assert_eq!(change_level(100, 160), 3);
        assert_eq!(change_level(100, 300), 4);
        assert_eq!(change_level(100, 0), 4);
        assert_eq!(change_level(0, 5), 4);
    }
}
