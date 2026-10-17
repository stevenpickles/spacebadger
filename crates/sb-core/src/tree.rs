//! Compact, authoritative scan tree.
//!
//! Nodes live in parallel arrays indexed by [`NodeId`], kept in fixed-size
//! chunks so the tree grows without copying what it holds; names are stored
//! once in a chunked byte arena (native encoding via
//! `OsStr::as_encoded_bytes`) and full paths are rebuilt on demand. Directory sizes are maintained incrementally: adding
//! or updating a file adjusts every ancestor, so readers always see consistent
//! aggregates without a rebuild.
//!
//! Nodes can be removed (after the item is deleted from disk). Removed nodes
//! keep their IDs, which are never reused, but are unlinked from their
//! parent and report zero sizes.

use crate::git::{GitClass, GitIndex, GitRoot, Problem, RootKind};
use crate::prior::Prior;
use crate::size::Allocated;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

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
    /// Removed from the tree (deleted from disk) along with its contents.
    pub const REMOVED: u8 = 1 << 4;
    /// A listed folder with entries that couldn't be read (they vanished
    /// or couldn't be measured), so some of its children may be missing.
    pub const UNREADABLE_ENTRIES: u8 = 1 << 5;
    /// Has an entry in the tree's [`GitIndex`](crate::git::GitIndex).
    pub const GIT: u8 = 1 << 6;
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

/// Nodes per chunk of a column. Chunks are allocated whole when the first
/// of their nodes is added.
const CHUNK_BITS: u32 = 14;
const CHUNK: usize = 1 << CHUNK_BITS;
const SLOT: usize = CHUNK - 1;

/// Bytes per chunk of the name arena. Names are at most `u16::MAX` bytes,
/// so one always fits in a fresh chunk.
const NAME_CHUNK_BITS: u32 = 20;
const NAME_CHUNK: usize = 1 << NAME_CHUNK_BITS;

/// One value per node, in chunks of [`CHUNK`] that are never moved or
/// grown, so adding a node never copies the nodes before it: the scanner
/// adds nodes with the tree's write lock held, and copying columns of
/// millions of nodes to grow them would hold it for tens of milliseconds.
/// Node `i` is at slot `i % CHUNK` of chunk `i / CHUNK`. The tree keeps
/// the length; slots past it hold the column's fill value.
struct Column<T> {
    chunks: Vec<Box<[T; CHUNK]>>,
}

impl<T: Copy> Column<T> {
    const fn new() -> Self {
        Self { chunks: Vec::new() }
    }

    /// Adds a chunk with every slot `fill`.
    fn grow(&mut self, fill: T) {
        let chunk: Box<[T; CHUNK]> = vec![fill; CHUNK]
            .into_boxed_slice()
            .try_into()
            .unwrap_or_else(|_| unreachable!("a chunk has CHUNK slots"));
        self.chunks.push(chunk);
    }

    fn bytes(&self) -> usize {
        self.chunks.len() * std::mem::size_of::<[T; CHUNK]>()
    }
}

/// A node index checked to be below the tree's length (see
/// [`Tree::checked`]), so every column has a slot for it.
#[derive(Clone, Copy)]
struct Slot(usize);

#[cold]
#[inline(never)]
fn out_of_range(i: usize) -> ! {
    panic!("node {i} is not in the tree")
}

impl<T> Column<T> {
    /// The value of a checked node. Reads go through here: with the length
    /// checked once, finding the chunk needs no second bounds check, which
    /// keeps sorting by size close to flat columns' speed (with the second
    /// check, the search results' first page measured 2.5 times slower).
    #[inline]
    fn at(&self, slot: Slot) -> &T {
        // SAFETY: a slot is below the tree's length, and every column has
        // a chunk for every node below it (`Tree::push` grows them all).
        unsafe { &self.chunks.get_unchecked(slot.0 >> CHUNK_BITS)[slot.0 & SLOT] }
    }
}

/// Bounds-checked against the chunks, not the tree's length, so only for
/// the tree's own updates of nodes it knows are in it: ones reached through
/// links, or checked with [`Tree::checked`] first.
impl<T> std::ops::Index<usize> for Column<T> {
    type Output = T;

    fn index(&self, i: usize) -> &T {
        &self.chunks[i >> CHUNK_BITS][i & SLOT]
    }
}

impl<T> std::ops::IndexMut<usize> for Column<T> {
    fn index_mut(&mut self, i: usize) -> &mut T {
        &mut self.chunks[i >> CHUNK_BITS][i & SLOT]
    }
}

/// Node names, stored once in chunks of [`NAME_CHUNK`] bytes. A name never
/// spans two chunks, and a chunk is never moved once full, so adding a name
/// copies at most the first chunk while it grows to full size.
#[derive(Default)]
struct Names {
    chunks: Vec<Vec<u8>>,
}

impl Names {
    /// Stores `bytes` and returns where: the chunk above
    /// [`NAME_CHUNK_BITS`], the offset in it below.
    fn push(&mut self, bytes: &[u8]) -> Result<u32, CapacityExceeded> {
        // A name must also start inside the chunk: an empty one at the end
        // of a full chunk would have an offset that carries into the chunk.
        let fits = self
            .chunks
            .last()
            .is_some_and(|c| c.len() < NAME_CHUNK && c.len() + bytes.len() <= NAME_CHUNK);
        if !fits {
            if self.chunks.len() >= 1 << (u32::BITS - NAME_CHUNK_BITS) {
                return Err(CapacityExceeded);
            }
            // Only a small tree's names are in the first chunk alone, so
            // it starts small; later ones are filled.
            let capacity = if self.chunks.is_empty() {
                0
            } else {
                NAME_CHUNK
            };
            self.chunks.push(Vec::with_capacity(capacity));
        }
        let index = self.chunks.len() - 1;
        let chunk = &mut self.chunks[index];
        if chunk.capacity() - chunk.len() < bytes.len() {
            let want = (chunk.capacity() * 2)
                .max(chunk.len() + bytes.len())
                .clamp(256, NAME_CHUNK);
            chunk.reserve_exact(want - chunk.len());
        }
        let start = (index << NAME_CHUNK_BITS | chunk.len()) as u32;
        chunk.extend_from_slice(bytes);
        Ok(start)
    }

    fn get(&self, start: u32, len: u16) -> &[u8] {
        let chunk = &self.chunks[(start >> NAME_CHUNK_BITS) as usize];
        let offset = start as usize & (NAME_CHUNK - 1);
        &chunk[offset..offset + usize::from(len)]
    }

    fn capacity(&self) -> usize {
        self.chunks.iter().map(Vec::capacity).sum()
    }
}

pub struct Tree {
    root_path: PathBuf,
    /// Number of nodes, including the root.
    len: usize,
    parent: Column<u32>,
    first_child: Column<u32>,
    next_sibling: Column<u32>,
    name_start: Column<u32>,
    name_len: Column<u16>,
    names: Names,
    flags: Column<u8>,
    dir_state: Column<u8>,
    /// Files: own logical length. Directories: sum over descendant files.
    logical: Column<u64>,
    /// Files: own allocation if known and owned, else 0. Directories: sum of
    /// known, owned descendant allocation.
    allocated: Column<u64>,
    /// Files: 1 if allocation is unknown. Directories: descendant count.
    unknown: Column<u32>,
    /// Files: 1. Directories: descendant file count.
    files: Column<u32>,
    /// Files: last modified, in [`epoch_seconds`] form. Directories: 0.
    modified: Column<u32>,
    /// The filesystem object the scan found, as a [`fingerprint`]; 0 when
    /// unknown. Lets a delete tell the scanned item from a replacement.
    ids: Column<u64>,
    /// Hard-link alias → owning node.
    alias_owner: HashMap<u32, u32>,
    dir_count: u64,
    /// Files whose sizes changed after they were added (including removed
    /// files), in order. Lets overlays such as search catch up without
    /// rescanning the tree.
    changes: Vec<u32>,
    /// Git repositories, worktrees and metadata found in the scan. A side
    /// table, not a column: only nodes with [`flags::GIT`] have entries.
    git: GitIndex,
    /// The previous scan's sizes for folders still to be listed, while a
    /// rescan runs (see [`Tree::set_prior`]).
    prior: Option<Box<Provisional>>,
}

/// Estimates from a [`Prior`] for the folders not yet listed.
///
/// A folder matched with a snapshot entry when it's created is estimated at
/// that entry's logical size until its listing ends (whatever the outcome:
/// listed, failed, or not scanned). While it's being listed, what's found
/// in it takes over that much of its own estimate: a matched subfolder its
/// estimate, a file its length, never more than is left. So nothing is
/// counted twice, and what's left stands for what hasn't been found yet.
/// `pending[D]` is the sum of the own estimates still left at or under `D`,
/// kept up to date with walks up the tree as file sizes are. Once every
/// folder is listed, every `pending` is 0 again.
struct Provisional {
    prior: Arc<Prior>,
    /// Pending folders matched with a snapshot entry: the entry, whose
    /// children are where the folder's subfolders are looked up as they're
    /// added, and what's left of its own estimate.
    entries: HashMap<u32, Waiting>,
    /// Per node: estimates of the unlisted folders at or under it.
    pending: Column<u64>,
    /// Folded names, reused.
    buf: Vec<u8>,
}

/// A pending folder's snapshot entry and the part of its estimate not yet
/// taken over by what was found in it.
#[derive(Debug, Clone, Copy)]
struct Waiting {
    entry: u32,
    own: u64,
}

/// What [`Tree::match_prior`] found, for [`Tree::apply_prior`].
#[derive(Debug)]
pub struct PriorMatch {
    prior: Arc<Prior>,
    /// The tree's length when matched.
    len: usize,
    /// (pending folder, snapshot entry).
    matched: Vec<(u32, u32)>,
}

/// Estimates taken out of a tree by [`Tree::clear_prior`], to be dropped
/// once its lock is released.
pub struct PriorEstimates(#[allow(dead_code)] Box<Provisional>);

impl std::fmt::Debug for Tree {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tree")
            .field("root_path", &self.root_path)
            .field("len", &self.len)
            .field("dir_count", &self.dir_count)
            .finish_non_exhaustive()
    }
}

impl Tree {
    /// Creates a tree whose root node represents `root_path`.
    pub fn new(root_path: impl Into<PathBuf>) -> Self {
        let mut tree = Self {
            root_path: root_path.into(),
            len: 0,
            parent: Column::new(),
            first_child: Column::new(),
            next_sibling: Column::new(),
            name_start: Column::new(),
            name_len: Column::new(),
            names: Names::default(),
            flags: Column::new(),
            dir_state: Column::new(),
            logical: Column::new(),
            allocated: Column::new(),
            unknown: Column::new(),
            files: Column::new(),
            modified: Column::new(),
            ids: Column::new(),
            alias_owner: HashMap::new(),
            dir_count: 0,
            changes: Vec::new(),
            git: GitIndex::default(),
            prior: None,
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
        self.len
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    pub fn contains(&self, node: NodeId) -> bool {
        node.index() < self.len()
    }

    /// `node` as a [`Slot`]; panics if it isn't in the tree, as indexing
    /// a flat column would.
    #[inline]
    fn checked(&self, node: NodeId) -> Slot {
        let i = node.index();
        if i >= self.len {
            out_of_range(i);
        }
        Slot(i)
    }

    fn push(
        &mut self,
        parent: u32,
        name: &OsStr,
        node_flags: u8,
    ) -> Result<NodeId, CapacityExceeded> {
        let bytes = name.as_encoded_bytes();
        let id = u32::try_from(self.len).map_err(|_| CapacityExceeded)?;
        let len = u16::try_from(bytes.len()).map_err(|_| CapacityExceeded)?;
        if id == NONE {
            return Err(CapacityExceeded);
        }
        let parent_slot = (parent != NONE).then(|| self.checked(NodeId(parent)).0);
        let start = self.names.push(bytes)?;
        let i = id as usize;
        if i & SLOT == 0 {
            // New slots start as an unlinked, empty, pending node.
            self.parent.grow(NONE);
            self.first_child.grow(NONE);
            self.next_sibling.grow(NONE);
            self.name_start.grow(0);
            self.name_len.grow(0);
            self.flags.grow(0);
            self.dir_state.grow(DirState::Pending as u8);
            self.logical.grow(0);
            self.allocated.grow(0);
            self.unknown.grow(0);
            self.files.grow(0);
            self.modified.grow(0);
            self.ids.grow(0);
            if let Some(p) = &mut self.prior {
                p.pending.grow(0);
            }
        }
        self.len += 1;
        // Every slot is set, so nothing written past the end before (which
        // `Tree::checked` refuses) could leak into the new node.
        self.parent[i] = parent;
        self.first_child[i] = NONE;
        self.next_sibling[i] = NONE;
        self.name_start[i] = start;
        self.name_len[i] = len;
        self.flags[i] = node_flags;
        self.dir_state[i] = DirState::Pending as u8;
        self.logical[i] = 0;
        self.allocated[i] = 0;
        self.unknown[i] = 0;
        self.files[i] = 0;
        self.modified[i] = 0;
        self.ids[i] = 0;
        if let Some(p) = &mut self.prior {
            p.pending[i] = 0;
        }
        if let Some(p) = parent_slot {
            self.next_sibling[i] = self.first_child[p];
            self.first_child[p] = id;
        }
        if node_flags & flags::DIRECTORY != 0 {
            self.dir_count += 1;
        }
        Ok(NodeId(id))
    }

    /// Adds an empty directory under `parent` in the [`DirState::Pending`] state.
    pub fn add_dir(&mut self, parent: NodeId, name: &OsStr) -> Result<NodeId, CapacityExceeded> {
        debug_assert!(self.is_dir(parent) && !self.is_removed(parent));
        let id = self.push(parent.0, name, flags::DIRECTORY)?;
        self.estimate_dir(parent, id);
        Ok(id)
    }

    /// Gives new folder `dir` the size its namesake had in the prior scan,
    /// if its parent was matched and is still waiting for its listing to
    /// end (only then can it gain children). That much of the parent's own
    /// estimate is now accounted for by `dir`, so it moves there.
    fn estimate_dir(&mut self, parent: NodeId, dir: NodeId) {
        let Some(p) = self.prior.as_deref_mut() else {
            return;
        };
        let Some(&Waiting {
            entry: parent_entry,
            ..
        }) = p.entries.get(&parent.0)
        else {
            return;
        };
        let i = dir.index();
        let name = self.names.get(self.name_start[i], self.name_len[i]);
        let Some(entry) = p.prior.child_dir(parent_entry, name, &mut p.buf) else {
            return;
        };
        let estimate = p.prior.logical(entry);
        p.entries.insert(
            dir.0,
            Waiting {
                entry,
                own: estimate,
            },
        );
        self.take_own(parent, estimate);
        self.shift_pending(dir, estimate, true);
    }

    /// Takes up to `amount` off the own estimate of pending `dir` (and off
    /// the pending estimate of it and its ancestors): a subfolder or file
    /// found in it now stands for that part of it.
    fn take_own(&mut self, dir: NodeId, amount: u64) {
        let Some(p) = self.prior.as_deref_mut() else {
            return;
        };
        let Some(waiting) = p.entries.get_mut(&dir.0) else {
            return;
        };
        let taken = amount.min(waiting.own);
        waiting.own -= taken;
        self.shift_pending(dir, taken, false);
    }

    /// Adds `amount` to (or takes it from) the pending estimate of `node`
    /// and every ancestor.
    fn shift_pending(&mut self, node: NodeId, amount: u64, add: bool) {
        let Some(p) = self.prior.as_deref_mut() else {
            return;
        };
        if amount == 0 {
            return;
        }
        let mut n = node.0;
        while n != NONE {
            let i = n as usize;
            if add {
                p.pending[i] += amount;
            } else {
                p.pending[i] -= amount;
            }
            n = self.parent[i];
        }
    }

    /// Estimates the folders not yet listed from `prior`, the snapshot of
    /// an earlier scan of the same root, until each one's listing ends.
    /// Replaces any earlier prior. Folders already in the tree are matched
    /// from the root down, so it can come once the scan has begun. The same
    /// as [`Tree::match_prior`] then [`Tree::apply_prior`], which let the
    /// matching be done with only a read lock held.
    ///
    /// The estimates are kept apart from the measured sizes: nothing but
    /// the treemap layout's provisional weight reads them, and they never
    /// change any total, count, or comparison.
    pub fn set_prior(&mut self, prior: Arc<Prior>) {
        let matched = self.match_prior(prior);
        self.apply_prior(matched);
    }

    /// Matches the folders now waiting for their listings with `prior`'s
    /// entries, from the root down through the listed ones, without
    /// changing the tree. Give the result to [`Tree::apply_prior`].
    pub fn match_prior(&self, prior: Arc<Prior>) -> PriorMatch {
        let mut matched = Vec::new();
        let mut buf = Vec::new();
        let mut stack = vec![(NodeId::ROOT, 0u32)];
        while let Some((dir, entry)) = stack.pop() {
            match self.dir_state(dir) {
                DirState::Pending => matched.push((dir.0, entry)),
                DirState::Listed => {}
                DirState::Failed | DirState::NotScanned => continue,
            }
            let mut c = self.first_child[dir.index()];
            while c != NONE {
                let i = c as usize;
                if self.flags[i] & (flags::DIRECTORY | flags::REMOVED) == flags::DIRECTORY {
                    let name = self.names.get(self.name_start[i], self.name_len[i]);
                    if let Some(e) = prior.child_dir(entry, name, &mut buf) {
                        stack.push((NodeId(c), e));
                    }
                }
                c = self.next_sibling[i];
            }
        }
        PriorMatch {
            prior,
            len: self.len,
            matched,
        }
    }

    /// Takes the estimates [`Tree::match_prior`] found, for the folders
    /// still waiting. Nodes added since the match are matched now, in the
    /// order they were added (a parent before its children).
    pub fn apply_prior(&mut self, m: PriorMatch) {
        let mut pending = Column::new();
        for _ in 0..self.parent.chunks.len() {
            pending.grow(0);
        }
        self.prior = Some(Box::new(Provisional {
            prior: m.prior,
            entries: HashMap::new(),
            pending,
            buf: Vec::new(),
        }));
        for (n, entry) in m.matched {
            let node = NodeId(n);
            if self.dir_state(node) != DirState::Pending || self.is_removed(node) {
                continue;
            }
            let p = self.prior.as_deref_mut().expect("just set");
            let estimate = p.prior.logical(entry);
            p.entries.insert(
                n,
                Waiting {
                    entry,
                    own: estimate,
                },
            );
            self.shift_pending(node, estimate, true);
        }
        // What a waiting folder holds already accounts for part of it.
        let mut known = Vec::new();
        if let Some(p) = self.prior.as_deref() {
            for &n in p.entries.keys() {
                let mut c = self.first_child[n as usize];
                while c != NONE {
                    known.push((n, c));
                    c = self.next_sibling[c as usize];
                }
            }
        }
        for (dir, c) in known {
            if c as usize >= m.len {
                continue;
            }
            let i = c as usize;
            let share = if self.flags[i] & flags::DIRECTORY != 0 {
                let waiting = self.prior.as_deref().and_then(|p| p.entries.get(&c));
                match waiting {
                    Some(w) => self
                        .prior
                        .as_deref()
                        .map_or(0, |p| p.prior.logical(w.entry)),
                    None => self.logical[i],
                }
            } else {
                self.logical[i]
            };
            self.take_own(NodeId(dir), share);
        }
        for i in m.len..self.len {
            let node = NodeId(i as u32);
            let parent = NodeId(self.parent[i]);
            if self.flags[i] & flags::REMOVED != 0 {
                continue;
            }
            if self.flags[i] & flags::DIRECTORY != 0 {
                if self.dir_state(node) == DirState::Pending {
                    self.estimate_dir(parent, node);
                } else {
                    self.take_own(parent, self.logical[i]);
                }
            } else {
                self.take_own(parent, self.logical[i]);
            }
        }
    }

    /// Forgets the prior and its estimates. Returns them so the caller can
    /// let their memory go after releasing the tree's lock.
    pub fn clear_prior(&mut self) -> Option<PriorEstimates> {
        self.prior.take().map(PriorEstimates)
    }

    /// When the scan the estimates come from finished (milliseconds since
    /// the Unix epoch), while the tree has a prior.
    pub fn prior_created_ms(&self) -> Option<u64> {
        self.prior.as_ref().map(|p| p.prior.created_ms())
    }

    /// The estimated bytes of the folders at or under `node` still waiting
    /// for their listings. For the treemap's provisional weight only, so
    /// private to this crate: nothing that reports a size may read it.
    pub(crate) fn pending_estimate(&self, node: NodeId) -> u64 {
        match &self.prior {
            Some(p) => *p.pending.at(self.checked(node)),
            None => 0,
        }
    }

    /// Whether part of `node`'s area on the map is estimated from the prior
    /// scan: a folder with unlisted folders at or under it that it knew.
    pub fn is_provisional(&self, node: NodeId) -> bool {
        self.pending_estimate(node) > 0
    }

    /// Adds a file under `parent` and adds its sizes to every ancestor.
    /// `modified` is its last-modified time, if the listing gave one.
    pub fn add_file(
        &mut self,
        parent: NodeId,
        name: &OsStr,
        sizes: FileSizes,
        modified: Option<SystemTime>,
        extra_flags: u8,
    ) -> Result<NodeId, CapacityExceeded> {
        debug_assert!(self.is_dir(parent) && !self.is_removed(parent));
        let id = self.push(parent.0, name, extra_flags & !flags::DIRECTORY)?;
        let i = id.index();
        self.files[i] = 1;
        self.modified[i] = epoch_seconds(modified);
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
        if self.prior.is_some() {
            self.take_own(parent, logical);
        }
        Ok(id)
    }

    /// Replaces a file's sizes (e.g. after an authoritative re-measure),
    /// adjusting ancestors by the difference.
    pub fn update_file(&mut self, node: NodeId, sizes: FileSizes) {
        let i = node.index();
        debug_assert!(!self.is_dir(node));
        if self.is_removed(node) {
            return;
        }
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
        self.changes.push(node.0);
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
        if self.is_removed(alias) || self.is_removed(owner) {
            return;
        }
        self.flags[owner.index()] |= flags::HARDLINKED;
        if self.flags[i] & flags::HARDLINK_ALIAS != 0 {
            return;
        }
        self.flags[i] |= flags::HARDLINK_ALIAS | flags::HARDLINKED;
        self.alias_owner.insert(alias.0, owner.0);
        self.changes.push(alias.0);
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

    /// Sets a folder's state. A folder leaving [`DirState::Pending`] drops
    /// its estimate (see [`Tree::set_prior`]): its listing has ended, so its
    /// own entries now speak for it.
    pub fn set_dir_state(&mut self, dir: NodeId, state: DirState) {
        debug_assert!(self.is_dir(dir));
        let i = self.checked(dir).0;
        let was = self.dir_state[i];
        self.dir_state[i] = state as u8;
        if was == DirState::Pending as u8
            && state != DirState::Pending
            && let Some(p) = self.prior.as_deref_mut()
            && let Some(waiting) = p.entries.remove(&dir.0)
        {
            self.shift_pending(dir, waiting.own, false);
        }
    }

    /// Notes that some of `dir`'s entries couldn't be read (see
    /// [`flags::UNREADABLE_ENTRIES`]).
    pub fn mark_unreadable_entries(&mut self, dir: NodeId) {
        debug_assert!(self.is_dir(dir));
        let i = self.checked(dir).0;
        self.flags[i] |= flags::UNREADABLE_ENTRIES;
    }

    /// Whether `dir` was listed with every entry read, so all its children
    /// are in the tree.
    pub fn fully_listed(&self, dir: NodeId) -> bool {
        self.dir_state(dir) == DirState::Listed
            && *self.flags.at(self.checked(dir)) & flags::UNREADABLE_ENTRIES == 0
    }

    pub fn dir_state(&self, dir: NodeId) -> DirState {
        DirState::from_u8(*self.dir_state.at(self.checked(dir)))
    }

    pub fn parent(&self, node: NodeId) -> Option<NodeId> {
        let p = *self.parent.at(self.checked(node));
        (p != NONE).then_some(NodeId(p))
    }

    /// Direct children, most recently added first.
    pub fn children(&self, node: NodeId) -> Children<'_> {
        Children {
            next_sibling: &self.next_sibling,
            next: *self.first_child.at(self.checked(node)),
            // The root's chunk; every tree has it.
            chunk: (0, &self.next_sibling.chunks[0]),
        }
    }

    /// The first of `node`'s children as [`Tree::children`] lists them.
    pub fn first_child(&self, node: NodeId) -> Option<NodeId> {
        let c = *self.first_child.at(self.checked(node));
        (c != NONE).then_some(NodeId(c))
    }

    /// The child [`Tree::children`] lists after `node`. New children are
    /// added in front, so a walk can stop at a child and go on from it
    /// after the tree has grown, missing only what was added meanwhile.
    pub fn next_sibling(&self, node: NodeId) -> Option<NodeId> {
        let c = *self.next_sibling.at(self.checked(node));
        (c != NONE).then_some(NodeId(c))
    }

    /// The node's own name; empty for the root.
    pub fn name(&self, node: NodeId) -> &OsStr {
        let i = self.checked(node);
        let bytes = self.names.get(*self.name_start.at(i), *self.name_len.at(i));
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
        *self.flags.at(self.checked(node))
    }

    pub fn is_dir(&self, node: NodeId) -> bool {
        *self.flags.at(self.checked(node)) & flags::DIRECTORY != 0
    }

    /// Logical bytes: the file's length, or the sum over descendant files.
    pub fn logical(&self, node: NodeId) -> u64 {
        *self.logical.at(self.checked(node))
    }

    /// Known allocated bytes counted at this node (excludes unknown
    /// allocations and hard-link aliases).
    pub fn allocated(&self, node: NodeId) -> u64 {
        *self.allocated.at(self.checked(node))
    }

    /// A file's own allocation, distinguishing unknown from zero. Aliases
    /// report zero because their allocation is owned elsewhere.
    pub fn file_allocated(&self, node: NodeId) -> Allocated {
        let i = self.checked(node);
        if *self.unknown.at(i) != 0 {
            Allocated::UNKNOWN
        } else {
            Allocated::known(*self.allocated.at(i))
        }
    }

    /// Files whose allocation is unknown: 0/1 for a file, a count for a dir.
    pub fn unknown_allocation_files(&self, node: NodeId) -> u32 {
        *self.unknown.at(self.checked(node))
    }

    /// A file's last-modified time in seconds since the Unix epoch, or
    /// `None` when unknown (and for folders). Any time before 1970 reads as
    /// 1 and any after early 2106 as `u32::MAX`; see [`epoch_seconds`].
    pub fn modified(&self, node: NodeId) -> Option<u32> {
        let secs = *self.modified.at(self.checked(node));
        (secs != 0).then_some(secs)
    }

    /// The [`fingerprint`] of the filesystem object the scan found for
    /// this node, or `None` when the scanner couldn't tell.
    pub fn id(&self, node: NodeId) -> Option<u64> {
        let id = *self.ids.at(self.checked(node));
        (id != 0).then_some(id)
    }

    /// Records the [`fingerprint`] of the node's filesystem object.
    pub fn set_id(&mut self, node: NodeId, id: u64) {
        let i = self.checked(node).0;
        self.ids[i] = id;
    }

    /// Files at or under this node.
    pub fn file_count(&self, node: NodeId) -> u32 {
        *self.files.at(self.checked(node))
    }

    pub fn dir_count(&self) -> u64 {
        self.dir_count
    }

    pub fn alias_owner(&self, alias: NodeId) -> Option<NodeId> {
        self.alias_owner.get(&alias.0).map(|&o| NodeId(o))
    }

    /// Files whose sizes changed after being added, oldest first; removed
    /// files are included. Entries may repeat; new entries are only ever
    /// appended.
    pub fn changed_files(&self) -> &[u32] {
        &self.changes
    }

    /// Whether `node` was removed, directly or with a removed folder.
    pub fn is_removed(&self, node: NodeId) -> bool {
        *self.flags.at(self.checked(node)) & flags::REMOVED != 0
    }

    /// Removes `node` and everything under it, as after deleting it from
    /// disk, and takes its sizes off every ancestor. Returns `false` for the
    /// root or a node already removed.
    ///
    /// A removed file that owned a hard link's allocation hands it to a
    /// remaining alias, since the data stays on disk while any name for it
    /// does.
    pub fn remove(&mut self, node: NodeId) -> bool {
        if node == NodeId::ROOT || !self.contains(node) || self.is_removed(node) {
            return false;
        }
        let i = node.index();
        let (logical, allocated, unknown, files) = (
            self.logical[i],
            self.allocated[i],
            self.unknown[i],
            self.files[i],
        );
        self.for_each_ancestor(node, |t, a| {
            t.logical[a] -= logical;
            t.allocated[a] -= allocated;
            t.unknown[a] -= unknown;
            t.files[a] -= files;
        });
        if let Some(p) = self.parent(node) {
            let pending = self.pending_estimate(node);
            self.shift_pending(p, pending, false);
        }
        self.unlink(node);

        let mut orphaned = Vec::new();
        let mut git = Vec::new();
        let mut stack = vec![node.0];
        while let Some(n) = stack.pop() {
            let j = n as usize;
            let mut c = self.first_child[j];
            while c != NONE {
                stack.push(c);
                c = self.next_sibling[c as usize];
            }
            let node_flags = self.flags[j];
            if node_flags & flags::GIT != 0 {
                git.push(NodeId(n));
            }
            if node_flags & flags::DIRECTORY != 0 {
                self.dir_count -= 1;
            } else {
                self.changes.push(n);
                if node_flags & flags::HARDLINK_ALIAS != 0 {
                    self.alias_owner.remove(&n);
                } else if node_flags & flags::HARDLINKED != 0 && self.unknown[j] == 0 {
                    orphaned.push((n, self.allocated[j]));
                }
            }
            self.flags[j] |= flags::REMOVED;
            if let Some(p) = self.prior.as_deref_mut() {
                p.entries.remove(&n);
                p.pending[j] = 0;
            }
            self.logical[j] = 0;
            self.allocated[j] = 0;
            self.unknown[j] = 0;
            self.files[j] = 0;
        }
        for (owner, allocated) in orphaned {
            self.hand_over(owner, allocated);
        }
        for n in git {
            // A repository whose `.git` (folder or file) went is no longer
            // known to be one.
            if self.git.get(n).is_some_and(|r| r.kind == RootKind::DotGit) {
                let owner = self.parent[n.index()];
                if owner != NONE && !self.is_removed(NodeId(owner)) {
                    self.set_git(GitRoot::unresolved(NodeId(owner), Problem::MetadataRemoved));
                }
            }
            self.git.forget(n);
        }
        true
    }

    /// Detaches `node` from its parent's list of children.
    fn unlink(&mut self, node: NodeId) {
        let parent = self.parent[node.index()] as usize;
        let next = self.next_sibling[node.index()];
        if self.first_child[parent] == node.0 {
            self.first_child[parent] = next;
            return;
        }
        let mut c = self.first_child[parent];
        while c != NONE {
            let after = self.next_sibling[c as usize];
            if after == node.0 {
                self.next_sibling[c as usize] = next;
                return;
            }
            c = after;
        }
    }

    /// Makes a remaining alias of removed `owner` the owner of its
    /// allocation, and points the other aliases at it.
    fn hand_over(&mut self, owner: u32, allocated: u64) {
        let mut aliases: Vec<u32> = self
            .alias_owner
            .iter()
            .filter(|&(_, &o)| o == owner)
            .map(|(&a, _)| a)
            .collect();
        aliases.sort_unstable();
        let Some((&heir, rest)) = aliases.split_first() else {
            return;
        };
        self.alias_owner.remove(&heir);
        for a in rest {
            self.alias_owner.insert(*a, heir);
        }
        let h = heir as usize;
        self.flags[h] &= !flags::HARDLINK_ALIAS;
        if rest.is_empty() {
            self.flags[h] &= !flags::HARDLINKED;
        }
        if self.unknown[h] == 0 {
            self.allocated[h] = allocated;
            self.changes.push(heir);
            self.for_each_ancestor(NodeId(heir), |t, a| t.allocated[a] += allocated);
        }
    }

    /// Makes `alias` the owner of its file's allocation in place of its
    /// owner, which becomes an alias of it, as do `others` (the owner's
    /// other aliases, which the caller knows; any that aren't are left
    /// alone). Totals don't change: the owner's allocation moves as it is
    /// (none when unknown), and each name keeps its own unknown-allocation
    /// mark, as names do from when they're added. Returns `false`, changing
    /// nothing, when `alias` isn't an alias or either name was removed.
    pub fn promote_alias(&mut self, alias: NodeId, others: &[NodeId]) -> bool {
        let Some(owner) = self.alias_owner(alias) else {
            return false;
        };
        let (a, o) = (alias.index(), owner.index());
        if self.is_removed(alias) || self.is_removed(owner) {
            return false;
        }
        let allocated = std::mem::take(&mut self.allocated[o]);
        self.for_each_ancestor(owner, |t, x| t.allocated[x] -= allocated);
        self.allocated[a] = allocated;
        self.for_each_ancestor(alias, |t, x| t.allocated[x] += allocated);
        self.flags[a] &= !flags::HARDLINK_ALIAS;
        self.flags[o] |= flags::HARDLINK_ALIAS | flags::HARDLINKED;
        self.alias_owner.remove(&alias.0);
        self.alias_owner.insert(owner.0, alias.0);
        for other in others {
            if let Some(o2) = self.alias_owner.get_mut(&other.0)
                && *o2 == owner.0
            {
                *o2 = alias.0;
            }
        }
        self.changes.push(alias.0);
        self.changes.push(owner.0);
        true
    }

    pub fn alias_count(&self) -> usize {
        self.alias_owner.len()
    }

    /// Inserts or replaces the Git entry of `root.node`. Its node is logged
    /// in [`GitIndex::changes`] when its class or kind changed, so overlays
    /// can reclassify its subtree. Ignored for a removed node.
    pub fn set_git(&mut self, root: GitRoot) {
        let i = self.checked(root.node).0;
        if self.flags[i] & flags::REMOVED != 0 {
            return;
        }
        self.flags[i] |= flags::GIT;
        self.git.set(root);
    }

    /// Records the repository the scan root is inside of, found at `path`
    /// above it. Logs the root in [`GitIndex::changes`] when it changed.
    pub fn set_git_above(&mut self, path: PathBuf, root: GitRoot) {
        debug_assert_eq!(root.node, NodeId::ROOT);
        self.git.set_above(path, root);
    }

    pub fn git(&self) -> &GitIndex {
        &self.git
    }

    /// The node's own Git entry, if it has one.
    pub fn git_entry(&self, node: NodeId) -> Option<&GitRoot> {
        if *self.flags.at(self.checked(node)) & flags::GIT == 0 {
            return None;
        }
        self.git.get(node)
    }

    /// The class the node's own entry gives its subtree; `None` when it
    /// inherits one from above.
    pub fn git_own(&self, node: NodeId) -> Option<GitClass> {
        self.git_entry(node).and_then(|r| r.kind.class())
    }

    /// The node's Git class: that of the nearest entry at or above it with
    /// one, or of the repository above the scan root. O(depth).
    pub fn git_class(&self, node: NodeId) -> GitClass {
        let mut cur = Some(node);
        while let Some(n) = cur {
            if let Some(class) = self.git_own(n) {
                return class;
            }
            cur = self.parent(n);
        }
        self.git
            .above()
            .and_then(|(_, r)| r.kind.class())
            .unwrap_or_default()
    }

    /// The nearest node at or above `node` with a listed Git entry (a
    /// repository, worktree, or unresolved candidate). `None` when there's
    /// none in the scan; the repository above the scan root, if any, is
    /// [`GitIndex::above`].
    pub fn git_root_of(&self, node: NodeId) -> Option<NodeId> {
        let mut cur = Some(node);
        while let Some(n) = cur {
            if self.git_entry(n).is_some_and(|r| r.kind.listed()) {
                return Some(n);
            }
            cur = self.parent(n);
        }
        None
    }

    /// Approximate heap bytes held by the tree.
    pub fn memory_bytes(&self) -> usize {
        let columns = [
            self.parent.bytes(),
            self.first_child.bytes(),
            self.next_sibling.bytes(),
            self.name_start.bytes(),
            self.name_len.bytes(),
            self.flags.bytes(),
            self.dir_state.bytes(),
            self.logical.bytes(),
            self.allocated.bytes(),
            self.unknown.bytes(),
            self.files.bytes(),
            self.modified.bytes(),
            self.ids.bytes(),
        ];
        columns.iter().sum::<usize>()
            + self.names.capacity()
            + self.alias_owner.capacity() * 16
            + self.changes.capacity() * 4
            + self.git.memory_bytes()
            + self.prior.as_ref().map_or(0, |p| {
                p.pending.bytes()
                    + p.entries.capacity() * std::mem::size_of::<(u32, Waiting)>()
                    + p.prior.memory_bytes()
            })
    }
}

/// A time as whole seconds since the Unix epoch, the way the tree stores
/// it: 0 for unknown, times before 1970 as 1, and times after early 2106
/// (where `u32` runs out) as `u32::MAX`. Four bytes per node is the point;
/// second resolution is plenty for filtering by age.
pub fn epoch_seconds(time: Option<SystemTime>) -> u32 {
    let Some(time) = time else {
        return 0;
    };
    match time.duration_since(UNIX_EPOCH) {
        Ok(since) => u32::try_from(since.as_secs()).unwrap_or(u32::MAX).max(1),
        Err(_) => 1,
    }
}

/// The ID of an object on Linux or macOS: its inode number, with its birth
/// time (whole seconds since the Unix epoch) above it where the filesystem
/// keeps one, so an inode number reused by a new object doesn't match.
pub fn unix_object_id(inode: u64, birth_secs: Option<i64>) -> u128 {
    (u128::from(birth_secs.unwrap_or(0) as u64) << 64) | u128::from(inode)
}

/// A file's birth time in whole seconds since the Unix epoch, where the
/// platform and filesystem keep one (`statx` on Linux, `st_birthtime` on
/// macOS), for [`unix_object_id`].
pub fn birth_secs(meta: &std::fs::Metadata) -> Option<i64> {
    meta.created().ok().map(birth_secs_of)
}

/// [`birth_secs`] of a birth time read some other way (macOS
/// `getattrlistbulk`).
pub fn birth_secs_of(created: SystemTime) -> i64 {
    match created.duration_since(UNIX_EPOCH) {
        Ok(after) => after.as_secs() as i64,
        Err(before) => -(before.duration().as_secs_f64().ceil() as i64),
    }
}

/// Eight bytes standing for a filesystem object's ID (a file ID with
/// Windows, an inode number elsewhere), as kept per node; 0 for none. The
/// fold is exact for IDs that fit 64 bits (NTFS, inodes); a wider ID (ReFS)
/// keeps its low half and mixes in the rest.
pub fn fingerprint(id: u128) -> u64 {
    if id == 0 {
        return 0;
    }
    let folded = (id as u64) ^ ((id >> 64) as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    folded.max(1)
}

pub struct Children<'a> {
    next_sibling: &'a Column<u32>,
    next: u32,
    /// The chunk of sibling links the last child was found in, by index.
    /// Siblings are mostly added together, so the next one is usually in
    /// the same chunk; reusing it keeps walking a long list to one
    /// dependent load per child instead of two (the chunk, then the link).
    chunk: (usize, &'a [u32; CHUNK]),
}

impl Iterator for Children<'_> {
    type Item = NodeId;

    fn next(&mut self) -> Option<NodeId> {
        if self.next == NONE {
            return None;
        }
        let i = self.next as usize;
        if self.chunk.0 != i >> CHUNK_BITS {
            self.chunk = (i >> CHUNK_BITS, &self.next_sibling.chunks[i >> CHUNK_BITS]);
        }
        self.next = self.chunk.1[i & SLOT];
        Some(NodeId(i as u32))
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
        t.add_file(a, "x".as_ref(), sizes(10, 4096), None, 0)
            .unwrap();
        t.add_file(b, "y".as_ref(), sizes(1_000_000, 65_536), None, 0)
            .unwrap();
        t.add_file(NodeId::ROOT, "z".as_ref(), sizes(5, 0), None, 0)
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
                None,
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
            .add_file(NodeId::ROOT, "a".as_ref(), sizes(100, 4096), None, 0)
            .unwrap();
        let alias = t
            .add_file(d, "b".as_ref(), sizes(100, 4096), None, 0)
            .unwrap();
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
        assert_eq!(
            t.changed_files(),
            [
                alias.index() as u32,
                alias.index() as u32,
                owner.index() as u32
            ]
        );
    }

    #[test]
    fn promoting_an_alias_moves_the_allocation_and_the_other_aliases() {
        let mut t = Tree::new("/root");
        let (d, e) = (
            t.add_dir(NodeId::ROOT, "d".as_ref()).unwrap(),
            t.add_dir(NodeId::ROOT, "e".as_ref()).unwrap(),
        );
        let owner = t
            .add_file(d, "a".as_ref(), sizes(100, 4096), None, 0)
            .unwrap();
        let heir = t
            .add_file(e, "b".as_ref(), sizes(100, 4096), None, 0)
            .unwrap();
        let other = t
            .add_file(d, "c".as_ref(), sizes(100, 4096), None, 0)
            .unwrap();
        t.mark_alias(heir, owner);
        t.mark_alias(other, owner);
        assert!(!t.promote_alias(owner, &[]), "not an alias");

        assert!(t.promote_alias(heir, &[other]));
        assert_eq!(t.alias_owner(owner), Some(heir));
        assert_eq!(t.alias_owner(other), Some(heir));
        assert_eq!(t.alias_owner(heir), None);
        assert_eq!(t.alias_count(), 2);
        assert_eq!((t.allocated(d), t.allocated(e)), (0, 4096));
        assert_eq!(t.allocated(NodeId::ROOT), 4096);
        assert_eq!(t.logical(NodeId::ROOT), 300);
        assert_eq!(t.flags(heir) & flags::HARDLINK_ALIAS, 0);
        assert_ne!(t.flags(owner) & flags::HARDLINK_ALIAS, 0);
        // Removing the new owner hands the allocation on as before.
        assert!(t.remove(heir));
        assert_eq!(t.allocated(NodeId::ROOT), 4096);
        assert_eq!(t.alias_count(), 1);
    }

    #[test]
    fn promoting_keeps_totals_when_an_allocation_is_unknown() {
        let unknown = FileSizes {
            logical: 100,
            allocated: Allocated::UNKNOWN,
        };
        for (owner_sizes, alias_sizes) in [
            (unknown, unknown),
            (unknown, sizes(100, 4096)),
            (sizes(100, 4096), unknown),
        ] {
            let mut t = Tree::new("/root");
            let (d, e) = (
                t.add_dir(NodeId::ROOT, "d".as_ref()).unwrap(),
                t.add_dir(NodeId::ROOT, "e".as_ref()).unwrap(),
            );
            let owner = t.add_file(d, "a".as_ref(), owner_sizes, None, 0).unwrap();
            let alias = t.add_file(e, "b".as_ref(), alias_sizes, None, 0).unwrap();
            t.mark_alias(alias, owner);
            let before = (
                t.allocated(NodeId::ROOT),
                t.unknown_allocation_files(NodeId::ROOT),
                t.logical(NodeId::ROOT),
            );
            assert!(t.promote_alias(alias, &[]));
            assert_eq!(t.alias_owner(owner), Some(alias));
            assert_eq!(
                (
                    t.allocated(NodeId::ROOT),
                    t.unknown_allocation_files(NodeId::ROOT),
                    t.logical(NodeId::ROOT),
                ),
                before
            );
            assert_eq!(t.allocated(d), 0);
            assert_eq!(t.allocated(e), before.0);
        }
    }

    #[test]
    fn rebuilds_paths_and_lists_children() {
        let mut t = Tree::new(PathBuf::from("base"));
        let a = t.add_dir(NodeId::ROOT, "a".as_ref()).unwrap();
        let f = t
            .add_file(a, "ü file.txt".as_ref(), sizes(1, 1), None, 0)
            .unwrap();
        t.add_file(a, "g".as_ref(), sizes(1, 1), None, 0).unwrap();
        assert_eq!(t.path(f), Path::new("base").join("a").join("ü file.txt"));
        assert_eq!(t.path(NodeId::ROOT), Path::new("base"));
        let names: Vec<_> = t.children(a).map(|c| name(&t, c)).collect();
        assert_eq!(names, ["g", "ü file.txt"]);
        assert_eq!(t.parent(f), Some(a));
        assert_eq!(t.parent(NodeId::ROOT), None);
    }

    #[test]
    fn removing_a_folder_takes_its_sizes_off_the_ancestors() {
        let mut t = Tree::new("/root");
        let a = t.add_dir(NodeId::ROOT, "a".as_ref()).unwrap();
        let b = t.add_dir(a, "b".as_ref()).unwrap();
        let c = t.add_dir(a, "c".as_ref()).unwrap();
        let x = t
            .add_file(b, "x".as_ref(), sizes(10, 4096), None, 0)
            .unwrap();
        let y = t
            .add_file(
                b,
                "y".as_ref(),
                FileSizes {
                    logical: 7,
                    allocated: Allocated::UNKNOWN,
                },
                None,
                0,
            )
            .unwrap();
        let z = t.add_file(c, "z".as_ref(), sizes(1, 512), None, 0).unwrap();
        let w = t
            .add_file(a, "w".as_ref(), sizes(2, 1024), None, 0)
            .unwrap();

        assert!(t.remove(b));
        assert_eq!(t.logical(a), 3);
        assert_eq!(t.allocated(a), 1536);
        assert_eq!(t.unknown_allocation_files(a), 0);
        assert_eq!(t.file_count(NodeId::ROOT), 2);
        assert_eq!(t.dir_count(), 3);
        let left: Vec<_> = t.children(a).map(|n| name(&t, n)).collect();
        assert_eq!(left, ["w", "c"]);
        for n in [b, x, y] {
            assert!(t.is_removed(n));
            assert_eq!((t.logical(n), t.allocated(n)), (0, 0));
        }
        assert!(!t.is_removed(z));
        let mut changed = t.changed_files().to_vec();
        changed.sort_unstable();
        assert_eq!(changed, [x.index() as u32, y.index() as u32]);

        // Removing again, or removing the root, does nothing.
        assert!(!t.remove(b));
        assert!(!t.remove(x));
        assert!(!t.remove(NodeId::ROOT));

        // A file in the middle of a sibling list.
        assert!(t.remove(w));
        let left: Vec<_> = t.children(a).map(|n| name(&t, n)).collect();
        assert_eq!(left, ["c"]);
        assert_eq!(t.logical(NodeId::ROOT), 1);
        assert_eq!(t.allocated(NodeId::ROOT), 512);
        assert!(t.remove(c));
        assert_eq!(t.children(a).count(), 0);
        assert_eq!(t.allocated(NodeId::ROOT), 0);
        assert_eq!(t.dir_count(), 2);
    }

    #[test]
    fn removing_a_hard_link_owner_hands_allocation_to_an_alias() {
        let mut t = Tree::new("/root");
        let d = t.add_dir(NodeId::ROOT, "d".as_ref()).unwrap();
        let owner = t
            .add_file(NodeId::ROOT, "a".as_ref(), sizes(100, 4096), None, 0)
            .unwrap();
        let first = t
            .add_file(d, "b".as_ref(), sizes(100, 4096), None, 0)
            .unwrap();
        let second = t
            .add_file(d, "c".as_ref(), sizes(100, 4096), None, 0)
            .unwrap();
        t.mark_alias(first, owner);
        t.mark_alias(second, owner);
        assert_eq!(t.allocated(NodeId::ROOT), 4096);

        assert!(t.remove(owner));
        assert_eq!(t.allocated(NodeId::ROOT), 4096, "the data is still on disk");
        assert_eq!(t.allocated(d), 4096);
        assert_eq!(t.alias_owner(first), None);
        assert_eq!(t.alias_owner(second), Some(first));
        assert_eq!(t.alias_count(), 1);
        assert!(t.changed_files().contains(&(first.index() as u32)));

        // Removing the last two names frees it.
        assert!(t.remove(first));
        assert_eq!(t.alias_owner(second), None);
        assert_eq!(
            t.flags(second) & (flags::HARDLINK_ALIAS | flags::HARDLINKED),
            0
        );
        assert_eq!(t.allocated(NodeId::ROOT), 4096);
        assert!(t.remove(second));
        assert_eq!(t.allocated(NodeId::ROOT), 0);
        assert_eq!(t.alias_count(), 0);
    }

    #[test]
    fn removed_nodes_ignore_late_updates() {
        let mut t = Tree::new("/root");
        let f = t
            .add_file(NodeId::ROOT, "f".as_ref(), sizes(10, 4096), None, 0)
            .unwrap();
        t.remove(f);
        t.update_file(f, sizes(20, 8192));
        assert_eq!(t.allocated(NodeId::ROOT), 0);
        assert_eq!(t.logical(f), 0);
    }

    #[test]
    fn stores_modified_times_in_seconds() {
        use std::time::Duration;
        let at = |secs| Some(UNIX_EPOCH + Duration::from_secs(secs));
        assert_eq!(epoch_seconds(None), 0);
        assert_eq!(epoch_seconds(at(1_700_000_000)), 1_700_000_000);
        assert_eq!(epoch_seconds(at(0)), 1, "0 means unknown");
        assert_eq!(
            epoch_seconds(UNIX_EPOCH.checked_sub(Duration::from_secs(86_400))),
            1,
            "before 1970"
        );
        assert_eq!(epoch_seconds(at(1 << 33)), u32::MAX, "after 2106");

        let mut t = Tree::new("/root");
        let f = t
            .add_file(
                NodeId::ROOT,
                "f".as_ref(),
                sizes(1, 1),
                at(1_234_567_890),
                0,
            )
            .unwrap();
        let g = t
            .add_file(NodeId::ROOT, "g".as_ref(), sizes(1, 1), None, 0)
            .unwrap();
        assert_eq!(t.modified(f), Some(1_234_567_890));
        assert_eq!(t.modified(g), None);
        assert_eq!(t.modified(NodeId::ROOT), None);
    }

    #[test]
    fn keeps_each_nodes_object_id() {
        let mut t = Tree::new("/");
        let d = t.add_dir(NodeId::ROOT, "d".as_ref()).unwrap();
        let f = t.add_file(d, "f".as_ref(), sizes(1, 1), None, 0).unwrap();
        assert_eq!((t.id(d), t.id(f)), (None, None));
        t.set_id(f, fingerprint(42));
        assert_eq!(t.id(f), Some(42));
        assert_eq!(t.id(d), None);
    }

    #[test]
    fn fingerprints_keep_64_bit_ids_exact() {
        assert_eq!(fingerprint(0), 0);
        assert_eq!(fingerprint(7), 7);
        assert_eq!(fingerprint(u128::from(u64::MAX)), u64::MAX);
        // Wider IDs differing only in the high half still differ.
        assert_ne!(fingerprint(1 << 64 | 5), fingerprint(2 << 64 | 5));
        assert_ne!(fingerprint(1 << 64 | 5), fingerprint(5));
        assert_ne!(fingerprint(1 << 64), 0);
    }

    #[test]
    fn dir_state_round_trips() {
        let mut t = Tree::new("/");
        assert_eq!(t.dir_state(NodeId::ROOT), DirState::Pending);
        t.set_dir_state(NodeId::ROOT, DirState::NotScanned);
        assert_eq!(t.dir_state(NodeId::ROOT), DirState::NotScanned);
    }

    #[test]
    fn nodes_across_chunks_keep_their_links_and_sizes() {
        let mut t = Tree::new("/");
        // Folders and files alternate, each file in the folder before it,
        // so parents and siblings sit in other chunks than their nodes.
        let mut dirs = vec![NodeId::ROOT];
        let mut files = Vec::new();
        for i in 0..2 * CHUNK + 10 {
            if i % 3 == 0 {
                let parent = dirs[i % dirs.len()];
                dirs.push(t.add_dir(parent, format!("d{i}").as_ref()).unwrap());
            } else {
                let parent = *dirs.last().unwrap();
                let f = t
                    .add_file(
                        parent,
                        format!("f{i}").as_ref(),
                        sizes(i as u64, 1),
                        None,
                        0,
                    )
                    .unwrap();
                t.set_id(f, i as u64 + 1);
                files.push((f, parent, i));
            }
        }
        assert_eq!(t.len(), 2 * CHUNK + 11);
        assert_eq!(t.parent.chunks.len(), 3);
        let mut logical = 0;
        for &(f, parent, i) in &files {
            assert_eq!(name(&t, f), format!("f{i}"));
            assert_eq!(t.parent(f), Some(parent));
            assert_eq!(t.logical(f), i as u64);
            assert_eq!(t.id(f), Some(i as u64 + 1));
            assert!(t.children(parent).any(|c| c == f));
            logical += i as u64;
        }
        assert_eq!(t.logical(NodeId::ROOT), logical);
        assert_eq!(t.file_count(NodeId::ROOT), files.len() as u32);
        assert_eq!(t.dir_count(), dirs.len() as u64);

        // Removing a folder whose contents span chunks.
        let (f, parent, _) = files[files.len() / 2];
        let removed = t.logical(parent);
        assert!(t.remove(parent));
        assert!(t.is_removed(f));
        assert_eq!(t.logical(NodeId::ROOT), logical - removed);
        assert!(!t.children(t.parent(parent).unwrap()).any(|c| c == parent));
    }

    #[test]
    fn names_never_span_name_chunks() {
        let mut t = Tree::new("/");
        let longest = "x".repeat(usize::from(u16::MAX));
        let mut added = Vec::new();
        // Enough long names to fill several chunks, with short ones between.
        for i in 0..4 * NAME_CHUNK / longest.len() {
            let name = format!("{i}{}", &longest[i.to_string().len()..]);
            added.push((t.add_dir(NodeId::ROOT, name.as_ref()).unwrap(), name));
            let short = format!("s{i}");
            added.push((t.add_dir(NodeId::ROOT, short.as_ref()).unwrap(), short));
        }
        assert!(t.names.chunks.len() > 4);
        assert!(t.names.chunks.iter().all(|c| c.len() <= NAME_CHUNK));
        for (node, expected) in &added {
            assert_eq!(&name(&t, *node), expected);
        }
        let too_long = "x".repeat(usize::from(u16::MAX) + 1);
        assert_eq!(
            t.add_dir(NodeId::ROOT, too_long.as_ref()),
            Err(CapacityExceeded)
        );
    }

    #[test]
    fn an_empty_name_after_a_full_name_chunk_starts_a_new_one() {
        let mut t = Tree::new("/");
        let long = "n".repeat(256);
        let filled: Vec<_> = (0..NAME_CHUNK / long.len())
            .map(|_| t.add_dir(NodeId::ROOT, long.as_ref()).unwrap())
            .collect();
        assert_eq!(t.names.chunks[0].len(), NAME_CHUNK, "exactly full");
        let empty = t
            .add_file(NodeId::ROOT, "".as_ref(), sizes(1, 1), None, 0)
            .unwrap();
        // Before any later name, so its chunk must already exist.
        assert_eq!(name(&t, empty), "");
        let after = t.add_dir(NodeId::ROOT, "after".as_ref()).unwrap();
        assert_eq!(name(&t, empty), "");
        assert_eq!(name(&t, after), "after");
        assert_eq!(name(&t, filled[filled.len() - 1]), long);
    }

    #[test]
    fn writes_to_nodes_not_in_the_tree_panic() {
        use std::panic::{AssertUnwindSafe, catch_unwind};
        let mut t = Tree::new("/");
        // Past the end, but inside the root's chunk.
        let ghost = NodeId::from_index(5);
        let writes: [&dyn Fn(&mut Tree); 8] = [
            &|t| t.set_id(ghost, 42),
            &|t| t.set_git(GitRoot::pending(ghost)),
            &|t| t.set_dir_state(ghost, DirState::Listed),
            &|t| t.mark_unreadable_entries(ghost),
            &|t| t.update_file(ghost, sizes(1, 1)),
            &|t| t.mark_alias(ghost, NodeId::ROOT),
            &|t| {
                let _ = t.add_dir(ghost, "d".as_ref());
            },
            &|t| {
                let _ = t.add_file(ghost, "f".as_ref(), sizes(1, 1), None, 0);
            },
        ];
        for (n, write) in writes.iter().enumerate() {
            let result = catch_unwind(AssertUnwindSafe(|| write(&mut t)));
            assert!(result.is_err(), "write {n} went through");
        }
        assert!(!t.remove(ghost));
        assert_eq!(t.len(), 1);

        // Nothing was left behind for the nodes added there later.
        for i in 0..5 {
            t.add_dir(NodeId::ROOT, format!("d{i}").as_ref()).unwrap();
        }
        assert_eq!(t.id(ghost), None);
        assert_eq!(t.dir_state(ghost), DirState::Pending);
        assert_eq!(t.flags(ghost), flags::DIRECTORY);
        assert_eq!(t.children(ghost).count(), 0);
        assert!(t.git().is_empty());
    }

    fn git_changes(t: &Tree) -> Vec<NodeId> {
        t.git()
            .changes()
            .iter()
            .map(|&raw| NodeId::from_index(raw))
            .collect()
    }

    #[test]
    fn set_git_flags_the_node_and_logs_class_changes() {
        let mut t = Tree::new("/s");
        let repo = t.add_dir(NodeId::ROOT, "repo".as_ref()).unwrap();
        let dot = t.add_dir(repo, ".git".as_ref()).unwrap();
        let f = t
            .add_file(repo, "a".as_ref(), sizes(1, 1), None, 0)
            .unwrap();
        assert_eq!(t.git_class(f), GitClass::Other);
        assert_eq!(t.git_own(repo), None);

        t.set_git(GitRoot::pending(repo));
        assert_ne!(t.flags(repo) & flags::GIT, 0);
        assert_eq!(t.git_own(repo), None, "pending inherits");
        assert_eq!(t.git_root_of(f), None);
        t.set_git(GitRoot {
            head: Some(crate::git::Head::Branch("main".into())),
            ..GitRoot::new(repo, RootKind::Repository)
        });
        t.set_git(GitRoot::new(dot, RootKind::DotGit));
        // Same kind, new details: nothing to reclassify.
        t.set_git(GitRoot::new(repo, RootKind::Repository));
        assert_eq!(git_changes(&t), [repo, repo, dot]);
        assert_eq!(t.git().get(repo).unwrap().head, None);
        assert_eq!(t.git().listed_count(), 1);
        assert_eq!(t.git().roots().count(), 2);

        assert_eq!(t.git_class(f), GitClass::Checkout);
        assert_eq!(t.git_class(dot), GitClass::Metadata);
        assert_eq!(t.git_class(NodeId::ROOT), GitClass::Other);
        assert_eq!(t.git_root_of(f), Some(repo));
        assert_eq!(t.git_root_of(dot), Some(repo));
        assert_eq!(t.git_root_of(NodeId::ROOT), None);
        assert_eq!(t.flags(f) & flags::GIT, 0);
    }

    #[test]
    fn nearest_git_entry_wins_and_above_covers_the_rest() {
        let mut t = Tree::new("/s/repo/src");
        let outer = t.add_dir(NodeId::ROOT, "outer".as_ref()).unwrap();
        let wt = t.add_dir(outer, "wt".as_ref()).unwrap();
        let broken = t.add_dir(wt, "broken".as_ref()).unwrap();
        let deep = t
            .add_file(broken, "f".as_ref(), sizes(1, 1), None, 0)
            .unwrap();
        let loose = t
            .add_file(NodeId::ROOT, "g".as_ref(), sizes(1, 1), None, 0)
            .unwrap();
        t.set_git(GitRoot::new(outer, RootKind::Repository));
        t.set_git(GitRoot::new(wt, RootKind::LinkedWorktree));
        t.set_git(GitRoot::unresolved(broken, Problem::Link));
        assert_eq!(t.git_class(deep), GitClass::Worktree);
        assert_eq!(t.git_root_of(deep), Some(broken), "unresolved is listed");
        assert_eq!(t.git_class(loose), GitClass::Other);

        let before = t.git().changes().len();
        t.set_git_above(
            PathBuf::from("/s/repo"),
            GitRoot::new(NodeId::ROOT, RootKind::Repository),
        );
        assert_eq!(t.git_class(loose), GitClass::Checkout);
        assert_eq!(t.git_class(NodeId::ROOT), GitClass::Checkout);
        assert_eq!(t.git_class(deep), GitClass::Worktree);
        assert_eq!(&t.git().changes()[before..], [0]);
        assert_eq!(t.git().above().unwrap().0, Path::new("/s/repo"));
        assert_eq!(t.git().listed_count(), 4);
        // Setting the same again logs nothing.
        t.set_git_above(
            PathBuf::from("/s/repo"),
            GitRoot::new(NodeId::ROOT, RootKind::Repository),
        );
        assert_eq!(t.git().changes().len(), before + 1);
        assert_eq!(t.git().listed_count(), 4);
        assert_eq!(t.git_root_of(loose), None);
    }

    #[test]
    fn removing_a_dot_git_downgrades_its_repository() {
        let mut t = Tree::new("/s");
        let repo = t.add_dir(NodeId::ROOT, "repo".as_ref()).unwrap();
        let dot = t.add_dir(repo, ".git".as_ref()).unwrap();
        let wt = t.add_dir(NodeId::ROOT, "wt".as_ref()).unwrap();
        let pointer = t
            .add_file(wt, ".git".as_ref(), sizes(1, 1), None, 0)
            .unwrap();
        let other = t.add_dir(NodeId::ROOT, "other".as_ref()).unwrap();
        let nested = t.add_dir(other, "nested".as_ref()).unwrap();
        t.set_git(GitRoot::new(repo, RootKind::Repository));
        t.set_git(GitRoot::new(dot, RootKind::DotGit));
        t.set_git(GitRoot::new(wt, RootKind::LinkedWorktree));
        t.set_git(GitRoot::new(pointer, RootKind::DotGit));
        t.set_git(GitRoot::new(nested, RootKind::Repository));
        assert_eq!(t.git().listed_count(), 3);

        let before = t.git().changes().len();
        assert!(t.remove(dot));
        assert!(t.remove(pointer));
        let downgraded = t.git().get(repo).unwrap();
        assert_eq!(downgraded.kind, RootKind::Unresolved);
        assert_eq!(downgraded.problem, Some(Problem::MetadataRemoved));
        assert_eq!(t.git().get(wt).unwrap().kind, RootKind::Unresolved);
        assert_eq!(&git_changes(&t)[before..], [repo, wt]);
        assert_eq!(t.git_class(repo), GitClass::Other);
        assert!(t.git().get(dot).is_none() && t.git().get(pointer).is_none());

        // A repository removed whole leaves nothing behind.
        assert!(t.remove(other));
        assert!(t.git().get(nested).is_none());
        assert_eq!(t.git().listed_count(), 2);
        assert_eq!(t.git().roots().count(), 2);
        // Removed nodes take no new entries.
        t.set_git(GitRoot::new(nested, RootKind::Repository));
        assert!(t.git().get(nested).is_none());
    }

    /// A finished scan of /r: a/{b/(400), c/(100), 10}, d/(50), 1, as a
    /// prior; a later scan of /r then finds what the tests add.
    fn prior() -> Arc<Prior> {
        let mut t = Tree::new("/r");
        let a = t.add_dir(NodeId::ROOT, "a".as_ref()).unwrap();
        let b = t.add_dir(a, "b".as_ref()).unwrap();
        let c = t.add_dir(a, "c".as_ref()).unwrap();
        let d = t.add_dir(NodeId::ROOT, "d".as_ref()).unwrap();
        for (dir, n, size) in [(b, "x", 400), (c, "y", 100), (a, "z", 10), (d, "w", 50)] {
            t.add_file(dir, n.as_ref(), sizes(size, size), None, 0)
                .unwrap();
        }
        t.add_file(NodeId::ROOT, "v".as_ref(), sizes(1, 1), None, 0)
            .unwrap();
        for n in [NodeId::ROOT, a, b, c, d] {
            t.set_dir_state(n, DirState::Listed);
        }
        Arc::new(Prior::new(crate::snapshot::Snapshot::capture(
            &t, 1_000, true,
        )))
    }

    fn pending(t: &Tree, nodes: &[NodeId]) -> Vec<u64> {
        nodes.iter().map(|&n| t.pending_estimate(n)).collect()
    }

    #[test]
    fn folders_carry_their_prior_size_until_listed() {
        let mut t = Tree::new("/r");
        assert_eq!(t.prior_created_ms(), None);
        t.set_prior(prior());
        assert_eq!(t.prior_created_ms(), Some(1_000));
        let root = NodeId::ROOT;
        assert_eq!(pending(&t, &[root]), [561], "the root itself waits");

        // The root's listing: a (matched ignoring case), d, and a new e.
        let a = t.add_dir(root, "A".as_ref()).unwrap();
        let d = t.add_dir(root, "d".as_ref()).unwrap();
        let e = t.add_dir(root, "e".as_ref()).unwrap();
        t.add_file(root, "v".as_ref(), sizes(1, 1), None, 0)
            .unwrap();
        assert_eq!(
            pending(&t, &[root, a, d, e]),
            [560, 510, 50, 0],
            "found things take over the root's own"
        );
        t.set_dir_state(root, DirState::Listed);
        assert_eq!(pending(&t, &[root, a, d]), [560, 510, 50]);
        assert!(t.is_provisional(root) && t.is_provisional(a) && !t.is_provisional(e));

        // a's listing: b, c (now bigger), a file; b's listing.
        let b = t.add_dir(a, "b".as_ref()).unwrap();
        let c = t.add_dir(a, "c".as_ref()).unwrap();
        t.add_file(a, "z".as_ref(), sizes(10, 10), None, 0).unwrap();
        t.set_dir_state(a, DirState::Listed);
        assert_eq!(pending(&t, &[root, a, b, c]), [550, 500, 400, 100]);
        t.add_file(b, "x".as_ref(), sizes(400, 400), None, 0)
            .unwrap();
        t.set_dir_state(b, DirState::Listed);
        assert_eq!(pending(&t, &[root, a, b, c]), [150, 100, 0, 100]);
        // Every outcome ends the wait: failed, and not scanned.
        t.set_dir_state(c, DirState::Failed);
        t.set_dir_state(d, DirState::NotScanned);
        t.set_dir_state(e, DirState::Listed);
        assert_eq!(pending(&t, &[root, a, c, d]), [0, 0, 0, 0]);
        // Measured sizes never saw an estimate.
        assert_eq!(t.logical(root), 411);
        assert_eq!(t.logical(a), 410);

        // A folder under one already listed, and one under a new folder,
        // have nothing to look up.
        let late = t.add_dir(e, "b".as_ref()).unwrap();
        assert_eq!(t.pending_estimate(late), 0);
        t.clear_prior();
        assert_eq!(t.prior_created_ms(), None);
        assert_eq!(t.pending_estimate(root), 0);
    }

    #[test]
    fn a_prior_arriving_mid_scan_estimates_the_folders_still_waiting() {
        let mut t = Tree::new("/r");
        let root = NodeId::ROOT;
        let a = t.add_dir(root, "a".as_ref()).unwrap();
        let d = t.add_dir(root, "d".as_ref()).unwrap();
        t.set_dir_state(root, DirState::Listed);
        let b = t.add_dir(a, "b".as_ref()).unwrap();
        t.set_dir_state(a, DirState::Listed);
        t.set_dir_state(d, DirState::Failed);
        t.set_prior(prior());
        // b waits; d failed; a and the root are listed.
        assert_eq!(pending(&t, &[root, a, b, d]), [400, 400, 400, 0]);
        // c arrives under a, which is listed: nothing to look it up in.
        let c = t.add_dir(a, "c".as_ref()).unwrap();
        assert_eq!(t.pending_estimate(c), 0);
        t.set_dir_state(b, DirState::Listed);
        assert_eq!(pending(&t, &[root, a, b]), [0, 0, 0]);
    }

    #[test]
    fn a_folder_being_listed_hands_its_estimate_to_what_it_finds_once() {
        let mut t = Tree::new("/r");
        t.set_prior(prior());
        let root = NodeId::ROOT;
        let a = t.add_dir(root, "a".as_ref()).unwrap();
        t.set_dir_state(root, DirState::Listed);
        // a (510) is being listed: c is found (100 of it), then a file far
        // bigger than what's left, then b (400), whose estimate finds
        // nothing left of a's own to take over.
        let c = t.add_dir(a, "c".as_ref()).unwrap();
        assert_eq!(pending(&t, &[root, a, c]), [510, 510, 100]);
        t.add_file(a, "huge".as_ref(), sizes(1_000, 1_000), None, 0)
            .unwrap();
        assert_eq!(pending(&t, &[root, a, c]), [100, 100, 100]);
        let b = t.add_dir(a, "b".as_ref()).unwrap();
        assert_eq!(pending(&t, &[root, a, b, c]), [500, 500, 400, 100]);
        // The listing ends with nothing of a's own left to drop.
        t.set_dir_state(a, DirState::Listed);
        assert_eq!(pending(&t, &[root, a]), [500, 500]);
        t.set_dir_state(b, DirState::Listed);
        t.set_dir_state(c, DirState::Listed);
        assert_eq!(pending(&t, &[root, a]), [0, 0]);
    }

    #[test]
    fn a_prior_matched_under_a_read_lock_catches_up_with_what_came_since() {
        let mut t = Tree::new("/r");
        let root = NodeId::ROOT;
        let a = t.add_dir(root, "a".as_ref()).unwrap();
        let d = t.add_dir(root, "d".as_ref()).unwrap();
        t.set_dir_state(root, DirState::Listed);
        // a is part way through its listing when the match is made.
        let c = t.add_dir(a, "c".as_ref()).unwrap();
        let matched = t.match_prior(prior());
        // Then the scan goes on: b in a, a file in a, d listed.
        let b = t.add_dir(a, "b".as_ref()).unwrap();
        t.add_file(a, "z".as_ref(), sizes(10, 10), None, 0).unwrap();
        t.add_file(d, "w".as_ref(), sizes(50, 50), None, 0).unwrap();
        t.set_dir_state(d, DirState::Listed);
        t.apply_prior(matched);
        // a: 510, less c's 100, b's 400 and z's 10: nothing of its own left.
        assert_eq!(pending(&t, &[root, a, b, c, d]), [500, 500, 400, 100, 0]);
        t.set_dir_state(a, DirState::Listed);
        assert_eq!(t.pending_estimate(root), 500);
        assert!(t.clear_prior().is_some());
        assert!(t.clear_prior().is_none());
    }

    #[test]
    fn estimates_grow_into_new_chunks_and_leave_with_removed_folders() {
        let mut t = Tree::new("/r");
        t.set_prior(prior());
        let a = t.add_dir(NodeId::ROOT, "a".as_ref()).unwrap();
        // Enough files to push the folders into later chunks.
        for i in 0..CHUNK + 5 {
            t.add_file(NodeId::ROOT, format!("f{i}").as_ref(), sizes(1, 1), None, 0)
                .unwrap();
        }
        let b = t.add_dir(a, "b".as_ref()).unwrap();
        assert!(b.index() > CHUNK);
        assert_eq!(pending(&t, &[NodeId::ROOT, a, b]), [510, 510, 400]);
        assert!(t.memory_bytes() > 0);
        assert!(t.remove(a));
        assert_eq!(pending(&t, &[NodeId::ROOT, a, b]), [0, 0, 0]);
        t.set_dir_state(NodeId::ROOT, DirState::Listed);
        assert_eq!(t.pending_estimate(NodeId::ROOT), 0);
    }

    #[test]
    fn git_entries_across_chunks() {
        let mut t = Tree::new("/");
        let mut parent = NodeId::ROOT;
        let mut repos = Vec::new();
        for i in 0..2 * CHUNK + 5 {
            let d = t.add_dir(parent, format!("d{i}").as_ref()).unwrap();
            if i % 1000 == 0 {
                t.set_git(GitRoot::new(d, RootKind::Repository));
                repos.push(d);
            }
            if i % 7 == 0 {
                parent = d;
            }
        }
        let leaf = t
            .add_file(parent, "f".as_ref(), sizes(1, 1), None, 0)
            .unwrap();
        assert_eq!(t.git().listed_count(), repos.len() as u32);
        for &r in &repos {
            assert_eq!(t.git_own(r), Some(GitClass::Checkout));
        }
        assert_eq!(t.git_class(leaf), GitClass::Checkout);
        // The nearest is the last repository the leaf's chain passes.
        let root = t.git_root_of(leaf).unwrap();
        assert!(repos.contains(&root));
        let mut cur = t.parent(leaf);
        while let Some(n) = cur {
            if n == root {
                break;
            }
            assert_eq!(t.git_entry(n), None);
            cur = t.parent(n);
        }
        let with = t.memory_bytes();
        assert!(with >= repos.len() * std::mem::size_of::<GitRoot>());
    }

    /// From the review of #62: random earlier scans, random rescans with
    /// the prior matched and applied at random moments, folders removed,
    /// failed, or cut off by a cancel. A folder's pending estimate is never
    /// less than its children's together, and once nothing is waiting every
    /// one is 0.
    #[test]
    fn random_rescans_keep_the_estimates_consistent_and_end_at_zero() {
        let mut seed = 12345u64;
        let mut rnd = |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let mut seen = 0u64;
        for round in 0..120 {
            let mut old = Tree::new("/r");
            let mut dirs = vec![NodeId::ROOT];
            for k in 0..40 {
                let p = dirs[rnd(dirs.len() as u64) as usize];
                if rnd(2) == 0 {
                    let d = old.add_dir(p, format!("d{}", rnd(8)).as_ref()).unwrap();
                    dirs.push(d);
                } else {
                    let s = sizes(rnd(5000) + 1, 1);
                    old.add_file(p, format!("f{k}").as_ref(), s, None, 0)
                        .unwrap();
                }
            }
            for &d in &dirs {
                old.set_dir_state(d, DirState::Listed);
            }
            let snap = crate::snapshot::Snapshot::capture(&old, 1, round % 2 == 0);
            let prior = Arc::new(Prior::new(snap));
            let mut t = Tree::new("/r");
            let mut pending = vec![NodeId::ROOT];
            let mut all = vec![NodeId::ROOT];
            let mut matched = None;
            let when = rnd(60);
            let apply_at = when + rnd(10);
            let mut step = 0;
            while !pending.is_empty() {
                if step == when {
                    matched = Some(t.match_prior(prior.clone()));
                }
                if step == apply_at
                    && let Some(m) = matched.take()
                {
                    t.apply_prior(m);
                }
                step += 1;
                let idx = rnd(pending.len() as u64) as usize;
                let d = pending[idx];
                if t.is_removed(d) {
                    pending.swap_remove(idx);
                    continue;
                }
                match rnd(10) {
                    0..=3 => {
                        let c = t.add_dir(d, format!("d{}", rnd(8)).as_ref()).unwrap();
                        pending.push(c);
                        all.push(c);
                    }
                    4..=6 => {
                        let s = sizes(rnd(5000), 1);
                        t.add_file(d, format!("f{step}").as_ref(), s, None, 0)
                            .unwrap();
                    }
                    7 => {
                        if rnd(4) == 0 && d != NodeId::ROOT {
                            t.remove(d);
                        }
                    }
                    _ => {
                        pending.swap_remove(idx);
                        let state = [DirState::Listed, DirState::Failed, DirState::NotScanned]
                            [rnd(3) as usize];
                        t.set_dir_state(d, state);
                    }
                }
                if rnd(50) == 0 {
                    // A cancel: whatever waits is not scanned.
                    for &n in &pending {
                        if !t.is_removed(n) && t.dir_state(n) == DirState::Pending {
                            t.set_dir_state(n, DirState::NotScanned);
                        }
                    }
                    pending.clear();
                }
                for &n in &all {
                    if !t.is_removed(n) {
                        let below: u64 = t.children(n).map(|c| t.pending_estimate(c)).sum();
                        assert!(t.pending_estimate(n) >= below, "round {round}");
                    }
                }
                if t.pending_estimate(NodeId::ROOT) > 0 {
                    seen += 1;
                }
            }
            if let Some(m) = matched.take() {
                t.apply_prior(m);
            }
            for &n in &all {
                assert_eq!(t.pending_estimate(n), 0, "round {round}");
            }
        }
        assert!(seen > 100, "estimates were in play: {seen} steps");
    }
}
