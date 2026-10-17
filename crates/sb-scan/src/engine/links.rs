//! Hard-link bookkeeping for the aggregator: counting a file with several
//! names once, and correcting names whose listings are stale.
//!
//! NTFS keeps a copy of a file's sizes and times in each of its names'
//! directory entries and updates only the entry of the name a change went
//! through, so the other names' entries go stale until those names are
//! opened. Where the adapter gives a [`LinkStamp`], a later name's listed
//! sizes and stamp are compared with its first name's: equal ones are
//! taken as listed (both could only be stale if the file was last changed
//! through a name the scan never lists), and a file whose names disagree is
//! re-measured once, by its first name, after every folder is listed, with
//! the result applied to all its names; so is a file whose stamp says its
//! listings can be wrong under every name alike. Without a stamp, or with
//! [`ScanConfig::exact_hard_links`](super::ScanConfig::exact_hard_links),
//! every later name and (once) its first name are re-measured as they're
//! found.

use crate::adapter::{Entry, LinkStamp};
use sb_core::tree::{NodeId, Tree};
use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};
use std::path::PathBuf;

/// A file ID as a hash map key: two words, so a map entry needs no 16-byte
/// alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct FileKey(u64, u64);

impl FileKey {
    pub(super) fn of(key: u128) -> Self {
        Self(key as u64, (key >> 64) as u64)
    }
}

/// A fast, fixed-key hasher for file IDs: the Fx hash (as the suggestion
/// index uses for names) with a final fold, so the low bits a hash map
/// indexes by depend on every bit of the ID. File IDs on the user's own
/// disk don't need the default hasher's resistance to collision attacks.
#[derive(Default)]
pub(super) struct IdHasher(u64);

pub(super) type IdHash = BuildHasherDefault<IdHasher>;

impl Hasher for IdHasher {
    fn write(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(8) {
            let mut word = [0; 8];
            word[..chunk.len()].copy_from_slice(chunk);
            self.write_u64(u64::from_le_bytes(word));
        }
    }

    fn write_u64(&mut self, word: u64) {
        self.0 = (self.0.rotate_left(5) ^ word).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }

    fn finish(&self) -> u64 {
        self.0 ^ (self.0 >> 32)
    }
}

/// The first name a file with several names was found under: the alias
/// owner, with its listing's stamp to compare later names' listings with.
/// Its listed sizes are the tree's until the file is re-measured. The
/// stamp is kept as fields, with `trusted` in what would be padding, so an
/// entry of the owner map takes 40 bytes with its key (an `Option` of a
/// [`LinkStamp`] would make it 48).
#[derive(Debug, Clone, Copy)]
pub(super) struct Owner {
    pub(super) node: NodeId,
    trusted: bool,
    /// [`NONE`] in both for no stamp.
    written: i64,
    changed: i64,
}

/// No listed stamp: listed times are never negative.
const NONE: i64 = i64::MIN;

impl Owner {
    pub(super) fn new(node: NodeId, stamp: Option<LinkStamp>) -> Self {
        let stamp = stamp.filter(|s| (s.written, s.changed) != (NONE, NONE));
        Self {
            node,
            trusted: stamp.is_some_and(|s| s.trusted),
            written: stamp.map_or(NONE, |s| s.written),
            changed: stamp.map_or(NONE, |s| s.changed),
        }
    }

    fn stamp(&self) -> Option<LinkStamp> {
        ((self.written, self.changed) != (NONE, NONE)).then_some(LinkStamp {
            written: self.written,
            changed: self.changed,
            trusted: self.trusted,
        })
    }
}

/// What the scan keeps to count hard-linked files once and to correct
/// stale listings of their names.
#[derive(Default)]
pub(super) struct Links {
    /// Re-measure every name as it's found.
    exact: bool,
    /// The first name of every file that may have several names, by file
    /// ID; dropped once every folder is listed.
    owners: HashMap<FileKey, Owner, IdHash>,
    /// Owners already queued for a re-measure as they were found.
    remeasured: HashSet<NodeId>,
    /// Every (owner, alias) pair whose listings were compared, so a file
    /// found stale can have all its names corrected.
    aliases: Vec<(NodeId, NodeId)>,
    /// Owners of files whose names' listings disagreed.
    stale: HashSet<NodeId>,
    /// Re-measures after the listings, by the name being measured: all of
    /// the file's names (owner first) and how many have been tried.
    refining: HashMap<NodeId, (Vec<NodeId>, usize)>,
    /// Whether every folder is listed and the re-measures were queued.
    refined: bool,
}

impl Links {
    pub(super) fn new(exact: bool) -> Self {
        Self {
            exact,
            ..Self::default()
        }
    }

    /// The first name found so far of the file with ID `key`.
    pub(super) fn owner(&self, key: u128) -> Option<Owner> {
        self.owners.get(&FileKey::of(key)).copied()
    }

    /// Records first names found in a batch.
    pub(super) fn add_owners(&mut self, owners: impl IntoIterator<Item = (FileKey, Owner)>) {
        self.owners.extend(owners);
    }

    /// Notes that `alias`, just added from `entry`, is another name of
    /// `owner`'s file, under the batch's hold of the write lock (the alias
    /// is already marked in the tree). Pushes the names to re-measure now:
    /// the alias and (once) its owner where listings can't be compared or
    /// every name is to be re-measured; otherwise none, and a file whose
    /// names' listings disagree is left for [`Links::refine`].
    pub(super) fn alias(
        &mut self,
        tree: &Tree,
        alias: NodeId,
        entry: &Entry,
        owner: Owner,
        remeasure: &mut Vec<(NodeId, PathBuf)>,
    ) {
        self.aliases.push((owner.node, alias));
        let (Some(listed), Some(first), false) = (entry.stamp, owner.stamp(), self.exact) else {
            // Listings of other names can be stale (NTFS), so re-measure
            // both sides once.
            remeasure.push((alias, tree.path(alias)));
            if self.remeasured.insert(owner.node) {
                remeasure.push((owner.node, tree.path(owner.node)));
            }
            return;
        };
        let first_allocated =
            (tree.unknown_allocation_files(owner.node) == 0).then(|| tree.allocated(owner.node));
        // An untrusted listing can be wrong under every name alike.
        let agree = listed.trusted
            && listed == first
            && entry.logical == tree.logical(owner.node)
            && entry.allocated == first_allocated;
        if !agree {
            self.stale.insert(owner.node);
        }
    }

    /// Whether [`Links::refine`] has run.
    pub(super) fn refined(&self) -> bool {
        self.refined
    }

    /// Once every folder is listed: forgets the first names and returns one
    /// re-measure per file whose names' listings disagreed, by its first
    /// name's path, whose result [`Links::measured`] applies to every name.
    pub(super) fn refine(&mut self, tree: &Tree) -> Vec<(NodeId, PathBuf)> {
        self.refined = true;
        self.owners = HashMap::default();
        let mut groups: HashMap<NodeId, Vec<NodeId>> = self
            .stale
            .drain()
            .map(|owner| (owner, vec![owner]))
            .collect();
        for (owner, alias) in &self.aliases {
            if let Some(names) = groups.get_mut(owner) {
                names.push(*alias);
            }
        }
        let mut jobs: Vec<(NodeId, PathBuf)> = groups
            .keys()
            .map(|&owner| (owner, tree.path(owner)))
            .collect();
        jobs.sort_unstable_by_key(|job| job.0);
        self.refining = groups
            .into_iter()
            .map(|(owner, names)| (owner, (names, 1)))
            .collect();
        jobs
    }

    /// When the scan ends, after every re-measure was applied: for each
    /// hard-linked file whose owner isn't its first name in [`name_order`],
    /// that name and the owner's other aliases, to hand to
    /// [`Tree::promote_alias`], so which name owns the allocation doesn't
    /// depend on which folder was listed first. Only reads the tree, so it
    /// can run under a read lock.
    pub(super) fn settle(&mut self, tree: &Tree) -> Vec<(NodeId, Vec<NodeId>)> {
        let mut groups: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
        for (owner, alias) in std::mem::take(&mut self.aliases) {
            groups.entry(owner).or_default().push(alias);
        }
        let mut moves = Vec::new();
        for (owner, aliases) in groups {
            let best = aliases.iter().copied().fold(owner, |best, name| {
                if name_order(tree, name, best).is_lt() {
                    name
                } else {
                    best
                }
            });
            if best != owner {
                let others = aliases.into_iter().filter(|&a| a != best).collect();
                moves.push((best, others));
            }
        }
        moves
    }

    /// What a re-measure of `file` that succeeded (`ok`) or failed means:
    /// the names its result applies to, or (`Err`) the next name of the
    /// file to measure instead, when a re-measure after the listings failed
    /// and another name is left to try. A failure otherwise keeps the
    /// listings' values.
    pub(super) fn measured(
        &mut self,
        file: NodeId,
        ok: bool,
    ) -> Result<Vec<NodeId>, Option<NodeId>> {
        let Some((names, tried)) = self.refining.remove(&file) else {
            return if ok { Ok(vec![file]) } else { Err(None) };
        };
        if ok {
            return Ok(names);
        }
        let Some(&next) = names.get(tried) else {
            return Err(None);
        };
        self.refining.insert(next, (names, tried + 1));
        Err(Some(next))
    }
}

/// The order in which names of one file are preferred as its owner: fewer
/// path components first (`System32\x.dll` before
/// `WinSxS\<component>\x.dll`), then path order by component, ignoring
/// ASCII case, then exact bytes. It depends only on the paths, never on
/// listing order, and costs a walk up each name's folders.
fn name_order(tree: &Tree, a: NodeId, b: NodeId) -> std::cmp::Ordering {
    let depth = |mut node: NodeId| {
        let mut depth = 0usize;
        while let Some(parent) = tree.parent(node) {
            depth += 1;
            node = parent;
        }
        depth
    };
    let by_depth = depth(a).cmp(&depth(b));
    if by_depth.is_ne() {
        return by_depth;
    }
    let chain = |node: NodeId| {
        let mut chain = vec![node];
        let mut at = node;
        while let Some(parent) = tree.parent(at) {
            chain.push(parent);
            at = parent;
        }
        chain.reverse();
        chain
    };
    let (ca, cb) = (chain(a), chain(b));
    let bytes = |n: NodeId| tree.name(n).as_encoded_bytes();
    let pairs = || ca.iter().zip(&cb).filter(|(x, y)| x != y);
    ca.len()
        .cmp(&cb.len())
        .then_with(|| {
            pairs()
                .map(|(&x, &y)| {
                    let fold = |n| bytes(n).iter().map(u8::to_ascii_lowercase);
                    fold(x).cmp(fold(y))
                })
                .find(|o| o.is_ne())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .then_with(|| {
            pairs()
                .map(|(&x, &y)| bytes(x).cmp(bytes(y)))
                .find(|o| o.is_ne())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .then(a.cmp(&b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::hash::BuildHasher;

    #[test]
    fn owner_map_entries_stay_small() {
        assert_eq!(size_of::<(FileKey, Owner)>(), 40);
        let none = Owner::new(NodeId::ROOT, None);
        assert_eq!(none.stamp(), None);
        for trusted in [true, false] {
            let stamp = LinkStamp {
                written: 0,
                changed: 1,
                trusted,
            };
            assert_eq!(Owner::new(NodeId::ROOT, Some(stamp)).stamp(), Some(stamp));
        }
    }

    #[test]
    fn file_ids_hash_apart_in_the_low_bits() {
        // NTFS IDs: record numbers in the low 48 bits, a sequence number
        // above them, the high word zero.
        let hasher = IdHash::default();
        let mut buckets = HashSet::new();
        for record in 0..4096u128 {
            let key = FileKey::of(record | (3 << 48));
            buckets.insert(hasher.hash_one(key) & 0xFFF);
        }
        assert!(
            buckets.len() > 2400,
            "{} of 4096 buckets used",
            buckets.len()
        );
        assert_ne!(
            hasher.hash_one(FileKey::of(1)),
            hasher.hash_one(FileKey::of(1 << 64))
        );
    }
}
