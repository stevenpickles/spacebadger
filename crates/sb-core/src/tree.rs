//! Compact, authoritative scan tree.
//!
//! Nodes live in parallel arrays indexed by [`NodeId`]; names are stored once in
//! a byte arena (native encoding via `OsStr::as_encoded_bytes`) and full paths
//! are rebuilt on demand. Directory sizes are maintained incrementally: adding
//! or updating a file adjusts every ancestor, so readers always see consistent
//! aggregates without a rebuild.

use crate::size::Allocated;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

const NONE: u32 = u32::MAX;

/// Stable identifier of a node within one scan generation. Never reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(u32);

impl NodeId {
    pub const ROOT: Self = Self(0);

    pub const fn index(self) -> usize {
        self.0 as usize
    }

    pub const fn from_index(index: u32) -> Self {
        Self(index)
    }
}

/// Per-node flags.
pub mod flags {
    pub const DIRECTORY: u8 = 1 << 0;
    /// A hard-link alias whose allocation is owned by another node.
    pub const HARDLINK_ALIAS: u8 = 1 << 1;
    /// A file known to have other hard-link names (owner or alias).
    pub const HARDLINKED: u8 = 1 << 2;
    /// A cloud-provider file (placeholder or synced file).
    pub const CLOUD: u8 = 1 << 3;
}

/// Traversal state of a directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DirState {
    /// Discovered but not yet listed.
    Pending = 0,
    /// Listed; its direct children are all present.
    Listed = 1,
    /// Listing failed; contents are unknown.
    Failed = 2,
    /// Not listed because the scan was cancelled.
    NotScanned = 3,
}

impl DirState {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Listed,
            2 => Self::Failed,
            3 => Self::NotScanned,
            _ => Self::Pending,
        }
    }
}

/// Measured sizes of one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileSizes {
    pub logical: u64,
    pub allocated: Allocated,
}

/// The tree would exceed its compact index or arena limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapacityExceeded;

impl std::fmt::Display for CapacityExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("scan tree capacity exceeded")
    }
}

impl std::error::Error for CapacityExceeded {}

#[derive(Debug)]
pub struct Tree {
    root_path: PathBuf,
    parent: Vec<u32>,
    first_child: Vec<u32>,
    next_sibling: Vec<u32>,
    name_start: Vec<u32>,
    name_len: Vec<u16>,
    names: Vec<u8>,
    flags: Vec<u8>,
    dir_state: Vec<u8>,
    /// Files: own logical length. Directories: sum over descendant files.
    logical: Vec<u64>,
    /// Files: own allocation if known and owned, else 0. Directories: sum of
    /// known, owned descendant allocation.
    allocated: Vec<u64>,
    /// Files: 1 if allocation is unknown. Directories: descendant count.
    unknown: Vec<u32>,
    /// Files: 1. Directories: descendant file count.
    files: Vec<u32>,
    /// Hard-link alias → owning node.
    alias_owner: HashMap<u32, u32>,
    dir_count: u64,
}

impl Tree {
    /// Creates a tree whose root node represents `root_path`.
    pub fn new(root_path: impl Into<PathBuf>) -> Self {
        let mut tree = Self {
            root_path: root_path.into(),
            parent: Vec::new(),
            first_child: Vec::new(),
            next_sibling: Vec::new(),
            name_start: Vec::new(),
            name_len: Vec::new(),
            names: Vec::new(),
            flags: Vec::new(),
            dir_state: Vec::new(),
            logical: Vec::new(),
            allocated: Vec::new(),
            unknown: Vec::new(),
            files: Vec::new(),
            alias_owner: HashMap::new(),
            dir_count: 0,
        };
        tree.push(NONE, OsStr::new(""), flags::DIRECTORY)
            .expect("empty tree has capacity for its root");
        tree
    }

    pub fn root_path(&self) -> &Path {
        &self.root_path
    }

    /// Number of nodes, including the root.
    pub fn len(&self) -> usize {
        self.parent.len()
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    pub fn contains(&self, node: NodeId) -> bool {
        node.index() < self.len()
    }

    fn push(
        &mut self,
        parent: u32,
        name: &OsStr,
        node_flags: u8,
    ) -> Result<NodeId, CapacityExceeded> {
        let bytes = name.as_encoded_bytes();
        let id = u32::try_from(self.parent.len()).map_err(|_| CapacityExceeded)?;
        let start = u32::try_from(self.names.len()).map_err(|_| CapacityExceeded)?;
        let len = u16::try_from(bytes.len()).map_err(|_| CapacityExceeded)?;
        if id == NONE || start.checked_add(u32::from(len)).is_none() {
            return Err(CapacityExceeded);
        }
        self.names.extend_from_slice(bytes);
        self.parent.push(parent);
        self.first_child.push(NONE);
        self.next_sibling.push(NONE);
        self.name_start.push(start);
        self.name_len.push(len);
        self.flags.push(node_flags);
        self.dir_state.push(DirState::Pending as u8);
        self.logical.push(0);
        self.allocated.push(0);
        self.unknown.push(0);
        self.files.push(0);
        if parent != NONE {
            let p = parent as usize;
            self.next_sibling[id as usize] = self.first_child[p];
            self.first_child[p] = id;
        }
        if node_flags & flags::DIRECTORY != 0 {
            self.dir_count += 1;
        }
        Ok(NodeId(id))
    }

    /// Adds an empty directory under `parent` in the [`DirState::Pending`] state.
    pub fn add_dir(&mut self, parent: NodeId, name: &OsStr) -> Result<NodeId, CapacityExceeded> {
        debug_assert!(self.is_dir(parent));
        self.push(parent.0, name, flags::DIRECTORY)
    }

    /// Adds a file under `parent` and adds its sizes to every ancestor.
    pub fn add_file(
        &mut self,
        parent: NodeId,
        name: &OsStr,
        sizes: FileSizes,
        extra_flags: u8,
    ) -> Result<NodeId, CapacityExceeded> {
        debug_assert!(self.is_dir(parent));
        let id = self.push(parent.0, name, extra_flags & !flags::DIRECTORY)?;
        let i = id.index();
        self.files[i] = 1;
        self.logical[i] = sizes.logical;
        match sizes.allocated.get() {
            Some(bytes) if extra_flags & flags::HARDLINK_ALIAS == 0 => self.allocated[i] = bytes,
            Some(_) => {}
            None => self.unknown[i] = 1,
        }
        let (logical, allocated, unknown) = (self.logical[i], self.allocated[i], self.unknown[i]);
        self.for_each_ancestor(id, |t, a| {
            t.logical[a] += logical;
            t.allocated[a] += allocated;
            t.unknown[a] += unknown;
            t.files[a] += 1;
        });
        Ok(id)
    }

    /// Replaces a file's sizes (e.g. after an authoritative re-measure),
    /// adjusting ancestors by the difference.
    pub fn update_file(&mut self, node: NodeId, sizes: FileSizes) {
        let i = node.index();
        debug_assert!(!self.is_dir(node));
        let alias = self.flags[i] & flags::HARDLINK_ALIAS != 0;
        let new_logical = sizes.logical;
        let (new_allocated, new_unknown): (u64, u32) = match sizes.allocated.get() {
            Some(bytes) => (if alias { 0 } else { bytes }, 0),
            None => (0, 1),
        };
        let d_logical = new_logical.wrapping_sub(self.logical[i]);
        let d_allocated = new_allocated.wrapping_sub(self.allocated[i]);
        let d_unknown = new_unknown.wrapping_sub(self.unknown[i]);
        self.logical[i] = new_logical;
        self.allocated[i] = new_allocated;
        self.unknown[i] = new_unknown;
        self.for_each_ancestor(node, |t, a| {
            t.logical[a] = t.logical[a].wrapping_add(d_logical);
            t.allocated[a] = t.allocated[a].wrapping_add(d_allocated);
            t.unknown[a] = t.unknown[a].wrapping_add(d_unknown);
        });
    }

    /// Marks `alias` as another name for `owner`'s underlying file. The alias
    /// keeps its logical length but contributes no allocation.
    pub fn mark_alias(&mut self, alias: NodeId, owner: NodeId) {
        let i = alias.index();
        self.flags[owner.index()] |= flags::HARDLINKED;
        if self.flags[i] & flags::HARDLINK_ALIAS != 0 {
            return;
        }
        self.flags[i] |= flags::HARDLINK_ALIAS | flags::HARDLINKED;
        self.alias_owner.insert(alias.0, owner.0);
        let allocated = std::mem::take(&mut self.allocated[i]);
        self.for_each_ancestor(alias, |t, a| t.allocated[a] -= allocated);
    }

    fn for_each_ancestor(&mut self, node: NodeId, mut f: impl FnMut(&mut Self, usize)) {
        let mut p = self.parent[node.index()];
        while p != NONE {
            f(self, p as usize);
            p = self.parent[p as usize];
        }
    }

    pub fn set_dir_state(&mut self, dir: NodeId, state: DirState) {
        debug_assert!(self.is_dir(dir));
        self.dir_state[dir.index()] = state as u8;
    }

    pub fn dir_state(&self, dir: NodeId) -> DirState {
        DirState::from_u8(self.dir_state[dir.index()])
    }

    pub fn parent(&self, node: NodeId) -> Option<NodeId> {
        let p = self.parent[node.index()];
        (p != NONE).then_some(NodeId(p))
    }

    /// Direct children, most recently added first.
    pub fn children(&self, node: NodeId) -> Children<'_> {
        Children {
            tree: self,
            next: self.first_child[node.index()],
        }
    }

    /// The node's own name; empty for the root.
    pub fn name(&self, node: NodeId) -> &OsStr {
        let i = node.index();
        let start = self.name_start[i] as usize;
        let bytes = &self.names[start..start + self.name_len[i] as usize];
        // SAFETY: the bytes were produced by `OsStr::as_encoded_bytes` for a
        // complete `OsStr` on this platform and stored unchanged.
        unsafe { OsStr::from_encoded_bytes_unchecked(bytes) }
    }

    /// Full native path, rebuilt from names.
    pub fn path(&self, node: NodeId) -> PathBuf {
        let mut chain = Vec::new();
        let mut cur = Some(node);
        while let Some(n) = cur {
            if n != NodeId::ROOT {
                chain.push(n);
            }
            cur = self.parent(n);
        }
        let mut path = self.root_path.clone();
        for n in chain.into_iter().rev() {
            path.push(self.name(n));
        }
        path
    }

    pub fn flags(&self, node: NodeId) -> u8 {
        self.flags[node.index()]
    }

    pub fn is_dir(&self, node: NodeId) -> bool {
        self.flags[node.index()] & flags::DIRECTORY != 0
    }

    /// Logical bytes: the file's length, or the sum over descendant files.
    pub fn logical(&self, node: NodeId) -> u64 {
        self.logical[node.index()]
    }

    /// Known allocated bytes counted at this node (excludes unknown
    /// allocations and hard-link aliases).
    pub fn allocated(&self, node: NodeId) -> u64 {
        self.allocated[node.index()]
    }

    /// A file's own allocation, distinguishing unknown from zero. Aliases
    /// report zero because their allocation is owned elsewhere.
    pub fn file_allocated(&self, node: NodeId) -> Allocated {
        let i = node.index();
        if self.unknown[i] != 0 {
            Allocated::UNKNOWN
        } else {
            Allocated::known(self.allocated[i])
        }
    }

    /// Files whose allocation is unknown: 0/1 for a file, a count for a dir.
    pub fn unknown_allocation_files(&self, node: NodeId) -> u32 {
        self.unknown[node.index()]
    }

    /// Files at or under this node.
    pub fn file_count(&self, node: NodeId) -> u32 {
        self.files[node.index()]
    }

    pub fn dir_count(&self) -> u64 {
        self.dir_count
    }

    pub fn alias_owner(&self, alias: NodeId) -> Option<NodeId> {
        self.alias_owner.get(&alias.0).map(|&o| NodeId(o))
    }

    pub fn alias_count(&self) -> usize {
        self.alias_owner.len()
    }

    /// Approximate heap bytes held by the tree.
    pub fn memory_bytes(&self) -> usize {
        let per_node = 4 * 4 + 2 + 2 + 8 * 2 + 4 * 2;
        self.parent.capacity() * per_node + self.names.capacity() + self.alias_owner.capacity() * 16
    }
}

pub struct Children<'a> {
    tree: &'a Tree,
    next: u32,
}

impl Iterator for Children<'_> {
    type Item = NodeId;

    fn next(&mut self) -> Option<NodeId> {
        if self.next == NONE {
            return None;
        }
        let id = NodeId(self.next);
        self.next = self.tree.next_sibling[self.next as usize];
        Some(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sizes(logical: u64, allocated: u64) -> FileSizes {
        FileSizes {
            logical,
            allocated: Allocated::known(allocated),
        }
    }

    fn name(tree: &Tree, node: NodeId) -> String {
        tree.name(node).to_string_lossy().into_owned()
    }

    #[test]
    fn aggregates_both_metrics_up_the_tree() {
        let mut t = Tree::new("/root");
        let a = t.add_dir(NodeId::ROOT, "a".as_ref()).unwrap();
        let b = t.add_dir(a, "b".as_ref()).unwrap();
        t.add_file(a, "x".as_ref(), sizes(10, 4096), 0).unwrap();
        t.add_file(b, "y".as_ref(), sizes(1_000_000, 65_536), 0)
            .unwrap();
        t.add_file(NodeId::ROOT, "z".as_ref(), sizes(5, 0), 0)
            .unwrap();

        assert_eq!(t.logical(b), 1_000_000);
        assert_eq!(t.logical(a), 1_000_010);
        assert_eq!(t.allocated(a), 69_632);
        assert_eq!(t.logical(NodeId::ROOT), 1_000_015);
        assert_eq!(t.allocated(NodeId::ROOT), 69_632);
        assert_eq!(t.file_count(NodeId::ROOT), 3);
        assert_eq!(t.file_count(a), 2);
        assert_eq!(t.dir_count(), 3);
    }

    #[test]
    fn unknown_allocation_is_excluded_and_counted() {
        let mut t = Tree::new("/root");
        let f = t
            .add_file(
                NodeId::ROOT,
                "cloud".as_ref(),
                FileSizes {
                    logical: 500,
                    allocated: Allocated::UNKNOWN,
                },
                flags::CLOUD,
            )
            .unwrap();
        assert_eq!(t.file_allocated(f), Allocated::UNKNOWN);
        assert_eq!(t.allocated(NodeId::ROOT), 0);
        assert_eq!(t.logical(NodeId::ROOT), 500);
        assert_eq!(t.unknown_allocation_files(NodeId::ROOT), 1);

        t.update_file(f, sizes(500, 4096));
        assert_eq!(t.file_allocated(f), Allocated::known(4096));
        assert_eq!(t.allocated(NodeId::ROOT), 4096);
        assert_eq!(t.unknown_allocation_files(NodeId::ROOT), 0);
    }

    #[test]
    fn hard_link_alias_counts_allocation_once() {
        let mut t = Tree::new("/root");
        let d = t.add_dir(NodeId::ROOT, "d".as_ref()).unwrap();
        let owner = t
            .add_file(NodeId::ROOT, "a".as_ref(), sizes(100, 4096), 0)
            .unwrap();
        let alias = t.add_file(d, "b".as_ref(), sizes(100, 4096), 0).unwrap();
        t.mark_alias(alias, owner);

        assert_eq!(t.allocated(NodeId::ROOT), 4096);
        assert_eq!(t.allocated(d), 0);
        assert_eq!(t.logical(NodeId::ROOT), 200, "logical counts each entry");
        assert_eq!(t.alias_owner(alias), Some(owner));
        assert_ne!(t.flags(owner) & flags::HARDLINKED, 0);

        // Re-measuring the alias must not give it allocation.
        t.update_file(alias, sizes(300, 8192));
        assert_eq!(t.allocated(NodeId::ROOT), 4096);
        assert_eq!(t.logical(NodeId::ROOT), 400);
        t.update_file(owner, sizes(300, 8192));
        assert_eq!(t.allocated(NodeId::ROOT), 8192);
    }

    #[test]
    fn rebuilds_paths_and_lists_children() {
        let mut t = Tree::new(PathBuf::from("base"));
        let a = t.add_dir(NodeId::ROOT, "a".as_ref()).unwrap();
        let f = t
            .add_file(a, "ü file.txt".as_ref(), sizes(1, 1), 0)
            .unwrap();
        t.add_file(a, "g".as_ref(), sizes(1, 1), 0).unwrap();
        assert_eq!(t.path(f), Path::new("base").join("a").join("ü file.txt"));
        assert_eq!(t.path(NodeId::ROOT), Path::new("base"));
        let names: Vec<_> = t.children(a).map(|c| name(&t, c)).collect();
        assert_eq!(names, ["g", "ü file.txt"]);
        assert_eq!(t.parent(f), Some(a));
        assert_eq!(t.parent(NodeId::ROOT), None);
    }

    #[test]
    fn dir_state_round_trips() {
        let mut t = Tree::new("/");
        assert_eq!(t.dir_state(NodeId::ROOT), DirState::Pending);
        t.set_dir_state(NodeId::ROOT, DirState::NotScanned);
        assert_eq!(t.dir_state(NodeId::ROOT), DirState::NotScanned);
    }
}
