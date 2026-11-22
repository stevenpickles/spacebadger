//! The previous scan of a root, used while it is scanned again to give
//! folders not yet listed a provisional area on the map.
//!
//! A rescan lists every folder again, which on a large volume takes long
//! enough that a map drawn only from what has been listed is mostly empty
//! for a while. A [`Prior`] lets the [`Tree`](crate::tree::Tree) give each
//! folder it creates the size that folder had in the last [`Snapshot`],
//! matched by name the way comparisons match it (see [`crate::compare`]),
//! until the folder's own listing replaces it.
//!
//! Estimates only ever shape the map while a scan runs. Totals, counts,
//! exports, comparisons, searches, selections, Git summaries and deletes
//! read the tree's measured sizes, which estimates never touch; the tree
//! keeps the estimates in a column only the layout reads (see
//! [`Tree::set_prior`](crate::tree::Tree::set_prior)). The scanner may also
//! ask which waiting folders were biggest, as a share of the root, to list
//! them first (see
//! [`Tree::listing_priority`](crate::tree::Tree::listing_priority)).

use crate::compare::ChildIndex;
use crate::snapshot::{Snapshot, fold};

/// A decoded snapshot of the root, indexed for lookups by name.
#[derive(Debug)]
pub struct Prior {
    snap: Snapshot,
    index: ChildIndex,
}

impl Prior {
    /// Indexes `snap` for lookups (sorting every folder's children once).
    pub fn new(snap: Snapshot) -> Self {
        let mut index = ChildIndex::new(&snap);
        index.sort_all(&snap);
        Self { snap, index }
    }

    /// Approximate heap bytes held: the snapshot and its index.
    pub fn memory_bytes(&self) -> usize {
        self.snap.memory_bytes() + self.index.memory_bytes()
    }

    /// When the snapshot's scan finished, in milliseconds since the Unix
    /// epoch.
    pub fn created_ms(&self) -> u64 {
        self.snap.header().created_ms
    }

    /// The folder named `name` (native bytes) among folder `entry`'s
    /// children: the one with exactly that name, else the only one equal to
    /// it ignoring case where the snapshot ignores case. `None` when there
    /// is no such folder (new, renamed, or a file then).
    pub(crate) fn child_dir(&self, entry: u32, name: &[u8], buf: &mut Vec<u8>) -> Option<u32> {
        let folded = fold(name, self.index.insensitive(), buf);
        let range = self.index.named(&self.snap, entry, folded);
        let kids = &self.index.children(entry)[range];
        let found = match kids.iter().find(|&&k| self.snap.name(k) == name) {
            Some(&k) => k,
            None if kids.len() == 1 => kids[0],
            None => return None,
        };
        self.snap.is_dir(found).then_some(found)
    }

    /// Logical bytes the entry had.
    pub(crate) fn logical(&self, entry: u32) -> u64 {
        self.snap.logical(entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::size::Allocated;
    use crate::tree::{DirState, FileSizes, NodeId, Tree};

    fn snap(case_insensitive: bool) -> Snapshot {
        let mut t = Tree::new("/r");
        let a = t.add_dir(NodeId::ROOT, "Alpha".as_ref()).unwrap();
        let b = t.add_dir(a, "beta".as_ref()).unwrap();
        t.add_file(
            b,
            "f".as_ref(),
            FileSizes {
                logical: 300,
                allocated: Allocated::known(300),
            },
            None,
            0,
        )
        .unwrap();
        t.add_file(
            NodeId::ROOT,
            "file".as_ref(),
            FileSizes {
                logical: 5,
                allocated: Allocated::known(5),
            },
            None,
            0,
        )
        .unwrap();
        for n in [NodeId::ROOT, a, b] {
            t.set_dir_state(n, DirState::Listed);
        }
        Snapshot::capture(&t, 1_700_000_000_000, case_insensitive)
    }

    /// Outside the crate the estimates can't be read at all; inside it,
    /// only the tree (which keeps them), the layout's provisional weight,
    /// and the ranking that orders the small items the map merged by that
    /// same weight may. Totals, exports, comparisons, searches, selections
    /// and Git summaries live in this crate too, so they're checked here.
    #[test]
    fn only_the_layout_reads_estimates() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        // Spelled in parts so this test doesn't count itself.
        let readers = |needle: String| {
            let mut found = Vec::new();
            for entry in std::fs::read_dir(&src).unwrap() {
                let path = entry.unwrap().path();
                let text = std::fs::read_to_string(&path).unwrap();
                let uses = text.matches(needle.as_str()).count();
                if uses > 0 {
                    let name = path.file_name().unwrap().to_string_lossy().into_owned();
                    found.push((name, uses));
                }
            }
            found.sort();
            found
        };
        // The column: kept by tree.rs, read once in layout.rs, in
        // `Metric::estimate`.
        let column = readers(["pending", "_estimate("].concat());
        assert_eq!(column.len(), 2, "{column:?}");
        assert_eq!(
            column[0],
            ("layout.rs".to_owned(), 1),
            "one read in the layout"
        );
        assert_eq!(column[1].0, "tree.rs");
        // `Metric::estimate`: the layout's area weight, and the ranking's.
        let names: Vec<String> = readers([".esti", "mate("].concat())
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(names, ["layout.rs", "ranking.rs"]);
        // The scanner's listing order: defined in tree.rs, used by no
        // reader of sizes here, nor anywhere in the app.
        let needle = ["listing_", "priority("].concat();
        let order: Vec<String> = readers(needle.clone())
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(order, ["tree.rs"]);
        let app = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../src-tauri/src");
        let mut dirs = vec![app];
        let mut files = 0;
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    dirs.push(path);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    files += 1;
                    let text = std::fs::read_to_string(&path).unwrap();
                    assert!(!text.contains(&needle), "{}", path.display());
                }
            }
        }
        assert!(files > 0, "the app's sources were found");
    }

    #[test]
    fn finds_folders_by_name_as_the_snapshot_compares_them() {
        let mut buf = Vec::new();
        let p = Prior::new(snap(true));
        assert_eq!(p.created_ms(), 1_700_000_000_000);
        let alpha = p.child_dir(0, b"Alpha", &mut buf).unwrap();
        assert_eq!(p.logical(alpha), 300);
        assert_eq!(p.child_dir(0, b"ALPHA", &mut buf), Some(alpha));
        let beta = p.child_dir(alpha, b"Beta", &mut buf).unwrap();
        assert_eq!(p.logical(beta), 300);
        assert_eq!(p.child_dir(0, b"gamma", &mut buf), None);
        // Small files aren't entries, and big ones aren't folders.
        assert_eq!(p.child_dir(0, b"file", &mut buf), None);

        let exact = Prior::new(snap(false));
        assert!(exact.child_dir(0, b"Alpha", &mut buf).is_some());
        assert_eq!(exact.child_dir(0, b"alpha", &mut buf), None);
    }
}
