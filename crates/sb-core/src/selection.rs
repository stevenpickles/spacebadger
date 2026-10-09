//! A set of items chosen in the interface, reduced to what an action on
//! them would touch.

use crate::tree::{NodeId, Tree};
use std::cmp::Reverse;
use std::collections::HashSet;

/// Selected items with nested and stale entries dropped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resolved {
    /// Items not inside another selected folder, largest allocation first.
    pub items: Vec<NodeId>,
    /// Entries dropped because a selected folder already contains them.
    pub nested: usize,
    /// Entries that aren't in the tree: unknown, removed, or the root.
    pub missing: usize,
    /// How many of `items` are folders.
    pub folders: usize,
    /// Files in `items`, counting each folder's contents.
    pub files: u64,
    pub logical: u64,
    pub allocated: u64,
    /// Files in `items` whose allocation is unknown.
    pub unknown: u64,
}

/// Reduces `ids` to the outermost selected items. The scan root is never
/// included: it stands for the whole scan, not an item in it.
pub fn resolve(tree: &Tree, ids: &[u32]) -> Resolved {
    let mut out = Resolved::default();
    let mut chosen = HashSet::with_capacity(ids.len());
    for &id in ids {
        let node = NodeId::from_index(id);
        if node == NodeId::ROOT || !tree.contains(node) || tree.is_removed(node) {
            out.missing += 1;
        } else {
            chosen.insert(node);
        }
    }
    let duplicates = ids.len() - out.missing - chosen.len();
    for &node in &chosen {
        let mut up = tree.parent(node);
        let inside = loop {
            match up {
                Some(p) if chosen.contains(&p) => break true,
                Some(p) => up = tree.parent(p),
                None => break false,
            }
        };
        if inside {
            out.nested += 1;
        } else {
            out.items.push(node);
        }
    }
    out.nested += duplicates;
    out.items
        .sort_unstable_by_key(|&n| (Reverse(tree.allocated(n)), n.index()));
    for &n in &out.items {
        out.folders += usize::from(tree.is_dir(n));
        out.files += u64::from(tree.file_count(n));
        out.logical += tree.logical(n);
        out.allocated += tree.allocated(n);
        out.unknown += u64::from(tree.unknown_allocation_files(n));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::size::Allocated;
    use crate::tree::FileSizes;

    fn sizes(logical: u64, allocated: u64) -> FileSizes {
        FileSizes {
            logical,
            allocated: Allocated::known(allocated),
        }
    }

    fn id(n: NodeId) -> u32 {
        n.index() as u32
    }

    #[test]
    fn keeps_outermost_items_and_totals_them() {
        let mut t = Tree::new("/r");
        let a = t.add_dir(NodeId::ROOT, "a".as_ref()).unwrap();
        let b = t.add_dir(a, "b".as_ref()).unwrap();
        let x = t.add_file(b, "x".as_ref(), sizes(10, 4096), 0).unwrap();
        let y = t.add_file(a, "y".as_ref(), sizes(20, 8192), 0).unwrap();
        let z = t
            .add_file(NodeId::ROOT, "z".as_ref(), sizes(1, 512), 0)
            .unwrap();

        let r = resolve(&t, &[id(x), id(z), id(a), id(b), id(z)]);
        assert_eq!(r.items, [a, z]);
        assert_eq!(r.nested, 3, "x and b are inside a; z twice");
        assert_eq!(r.missing, 0);
        assert_eq!(r.folders, 1);
        assert_eq!(r.files, 3);
        assert_eq!((r.logical, r.allocated), (31, 12_800));

        let r = resolve(&t, &[id(y), id(x)]);
        assert_eq!(r.items, [y, x]);
        assert_eq!((r.nested, r.folders, r.files), (0, 0, 2));
    }

    #[test]
    fn skips_the_root_and_unknown_or_removed_items() {
        let mut t = Tree::new("/r");
        let f = t
            .add_file(NodeId::ROOT, "f".as_ref(), sizes(1, 1), 0)
            .unwrap();
        let g = t
            .add_file(NodeId::ROOT, "g".as_ref(), sizes(1, 1), 0)
            .unwrap();
        t.remove(g);
        let r = resolve(&t, &[0, id(f), id(g), 999]);
        assert_eq!(r.items, [f]);
        assert_eq!(r.missing, 3);
        assert!(resolve(&t, &[]).items.is_empty());
    }
}
