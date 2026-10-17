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
//! [`Tree::set_prior`](crate::tree::Tree::set_prior)).

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
    /// only the tree (which keeps them) and the layout's provisional weight
    /// may. Totals, exports, comparisons, searches, selections, rankings
    /// and Git summaries live in this crate too, so they're checked here.
    #[test]
    fn only_the_layout_reads_estimates() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        // Spelled in two parts so this test doesn't count itself.
        let needle = ["pending", "_estimate("].concat();
        let mut readers = Vec::new();
        for entry in std::fs::read_dir(&src).unwrap() {
            let path = entry.unwrap().path();
            let text = std::fs::read_to_string(&path).unwrap();
            let uses = text.matches(needle.as_str()).count();
            if uses > 0 {
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                readers.push((name, uses));
            }
        }
        readers.sort();
        // tree.rs defines and maintains it; layout.rs reads it once, in
        // `Metric::estimate`.
        assert_eq!(readers.len(), 2, "{readers:?}");
        assert_eq!(readers[0].0, "layout.rs");
        assert_eq!(readers[0].1, 1, "one read in the layout");
        assert_eq!(readers[1].0, "tree.rs");
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
