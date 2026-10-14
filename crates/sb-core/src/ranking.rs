//! Ranked lists for the Contents view: a folder's children, or the files
//! anywhere under it, largest first, kept between requests.
//!
//! The list is asked for again on every scan status while a scan runs, and
//! ranking the files under a large folder visits its whole subtree. So a
//! [`Ranking`] is kept per folder, metric, and filter ([`Key`]), with what
//! the tree was like when it was made (its [`Basis`]), and
//! [`Ranking::current`] decides whether it can be served again: after the
//! scan, only at the same revision, so a final list is exactly what ranking
//! afresh gives and never has a deleted item in it; while scanning, as long
//! as the folder's own totals are unchanged, so changes elsewhere in the
//! tree leave it alone, and otherwise for [`REFRESH`] after it was made.
//!
//! A [`Walk`] visits the folder a bounded number of nodes at a time, so the
//! caller can let go of the tree's lock between steps and the scanner can
//! write meanwhile. For the files under a folder it keeps only the largest
//! asked for so far (the requested prefix, at least [`MIN_RANKED`]), so the
//! memory a ranking takes follows how far the list was scrolled; a deeper
//! page walks again for more. The few rankings kept are bounded by
//! [`Rankings`].

use crate::layout::Metric;
use crate::search::Search;
use crate::tree::{NodeId, Tree};
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// While scanning, a ranking of a folder that has changed is made again at
/// most this often.
pub const REFRESH: Duration = Duration::from_millis(400);

/// Fewest files under a folder ranked at once, so scrolling doesn't walk
/// again often.
pub const MIN_RANKED: usize = 1_000;

/// Upper bound on items kept across all rankings and walks, 16 bytes each,
/// a ranking counting as [`ENTRY_COST`] more. The ranking being used is
/// kept even when it alone is larger.
pub const RANKING_ITEMS: usize = 1 << 20;

/// Most rankings kept at once.
const MAX_RANKINGS: usize = 16;

/// A ranking's own cost, in items.
const ENTRY_COST: usize = 16;

/// Numbers walks, so the one being finished can be told from later ones.
static WALKS: AtomicU64 = AtomicU64::new(1);

/// What a ranking lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// The folder's own children, files and folders, all of them.
    Children,
    /// Files at any depth under the folder.
    Files,
}

/// What a ranking is of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Key {
    pub scope: Scope,
    pub folder: NodeId,
    pub metric: Metric,
    /// The filter, by its number; its matches decide what's listed.
    pub search: Option<u32>,
}

/// A folder's totals (with a filter, its matching totals). While scanning,
/// the tree only grows and re-measures change sizes, so while these stay
/// the same, nothing under the folder has changed that a ranking lists.
/// Folders without files are the exception: one added doesn't change them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Signature {
    logical: u64,
    allocated: u64,
    unknown: u32,
    files: u32,
}

impl Signature {
    pub fn of(tree: &Tree, folder: NodeId, filter: Option<&Search>) -> Self {
        match filter {
            Some(search) => {
                let t = search.totals(folder);
                Self {
                    logical: t.logical,
                    allocated: t.allocated,
                    unknown: t.unknown,
                    files: t.files,
                }
            }
            None => Self {
                logical: tree.logical(folder),
                allocated: tree.allocated(folder),
                unknown: tree.unknown_allocation_files(folder),
                files: tree.file_count(folder),
            },
        }
    }
}

/// The tree as a ranking (or a request) found it, read with the tree's lock
/// held so the revision describes that tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Basis {
    /// The scan's published revision.
    pub revision: u64,
    /// The scan had finished, so the revision changes with every change.
    /// While scanning, the tree changes between revisions too.
    pub finished: bool,
    pub signature: Signature,
    pub at: Instant,
}

/// Largest first, ties by node ID; smaller is ranked higher.
type Rank = (Reverse<u64>, NodeId);

#[derive(Debug)]
enum Kept {
    /// Everything found, sorted when the walk finishes.
    All(Vec<Rank>),
    /// The largest `want` found, the lowest ranked on top.
    Top(BinaryHeap<Rank>, usize),
}

/// Ranking a folder's items, a bounded number of nodes at a time.
#[derive(Debug)]
pub struct Walk {
    id: u64,
    scope: Scope,
    metric: Metric,
    basis: Basis,
    /// Folders still to be listed.
    folders: Vec<NodeId>,
    /// The next child to look at in the folder being listed.
    next: Option<NodeId>,
    kept: Kept,
    weighted: usize,
    empty: usize,
}

impl Walk {
    /// Starts ranking `key`'s items, keeping the largest `want`
    /// (`usize::MAX` for all).
    pub fn new(key: Key, basis: Basis, want: usize) -> Self {
        Self {
            id: WALKS.fetch_add(1, Ordering::Relaxed),
            scope: key.scope,
            metric: key.metric,
            basis,
            folders: vec![key.folder],
            next: None,
            kept: if want == usize::MAX {
                Kept::All(Vec::new())
            } else {
                Kept::Top(BinaryHeap::with_capacity(want.min(1 << 16)), want)
            },
            weighted: 0,
            empty: 0,
        }
    }

    pub fn basis(&self) -> &Basis {
        &self.basis
    }

    /// Tells this walk (and the ranking it makes) from every other.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// How many items it keeps.
    fn wants(&self) -> usize {
        match self.kept {
            Kept::All(_) => usize::MAX,
            Kept::Top(_, want) => want,
        }
    }

    fn kept(&self) -> usize {
        match &self.kept {
            Kept::All(v) => v.len(),
            Kept::Top(h, _) => h.len(),
        }
    }

    /// Whether it can go on for a request that found the tree as `now`.
    /// Once the scan has finished, the tree may have been edited (items
    /// deleted) whenever the revision changed, and a walk from while
    /// scanning may have missed what was added meanwhile.
    pub fn valid_for(&self, now: &Basis) -> bool {
        self.basis.finished == now.finished
            && (!now.finished || self.basis.revision == now.revision)
    }

    /// Looks at up to `budget` more nodes of `tree`, which must not have had
    /// anything removed since the walk started (see [`Walk::valid_for`]);
    /// it may have grown. Returns whether the walk is done.
    pub fn step(&mut self, tree: &Tree, filter: Option<&Search>, budget: usize) -> bool {
        let mut left = budget;
        loop {
            let Some(c) = self.next else {
                match self.folders.pop() {
                    Some(dir) => {
                        self.next = tree.first_child(dir);
                        continue;
                    }
                    None => return true,
                }
            };
            if left == 0 {
                return false;
            }
            left -= 1;
            self.next = tree.next_sibling(c);
            // With a filter, only what contains matches is listed, and
            // folders without any can be skipped whole.
            if filter.is_some_and(|s| s.totals(c).files == 0) {
                continue;
            }
            if self.scope == Scope::Files && tree.is_dir(c) {
                self.folders.push(c);
                continue;
            }
            match self.metric.weight_in(tree, filter, c) {
                0 => self.empty += 1,
                w => {
                    self.weighted += 1;
                    self.keep((Reverse(w), c));
                }
            }
        }
    }

    fn keep(&mut self, rank: Rank) {
        match &mut self.kept {
            Kept::All(v) => v.push(rank),
            Kept::Top(heap, want) => {
                if heap.len() < *want {
                    heap.push(rank);
                } else if let Some(mut lowest) = heap.peek_mut()
                    && rank < *lowest
                {
                    *lowest = rank;
                }
            }
        }
    }

    /// Ranks what a finished walk found. Takes no lock: call it after
    /// letting go of the tree.
    pub fn finish(self) -> Ranking {
        let ranked = match self.kept {
            Kept::All(mut v) => {
                v.sort_unstable();
                v
            }
            Kept::Top(heap, _) => heap.into_sorted_vec(),
        };
        Ranking {
            walk: self.id,
            basis: self.basis,
            items: ranked.into_iter().map(|(Reverse(w), n)| (n, w)).collect(),
            weighted: self.weighted,
            empty: self.empty,
        }
    }
}

/// A folder's items largest first, with the weights they were ranked by.
#[derive(Debug)]
pub struct Ranking {
    /// The walk that made it.
    walk: u64,
    basis: Basis,
    /// The largest items, in order; all of them unless `weighted` is more.
    items: Vec<(NodeId, u64)>,
    weighted: usize,
    empty: usize,
}

impl Ranking {
    pub fn basis(&self) -> &Basis {
        &self.basis
    }

    /// Whether it was made from a later look at the tree than `other`: a
    /// later revision, after the scan rather than during it, or later.
    pub fn newer_than(&self, other: &Ranking) -> bool {
        let order = |b: &Basis| (b.revision, b.finished, b.at);
        order(&self.basis) > order(&other.basis)
    }

    /// Items with a weight, ranked or not.
    pub fn total(&self) -> usize {
        self.weighted
    }

    /// Other items, with no weight.
    pub fn empty(&self) -> usize {
        self.empty
    }

    /// The largest items ranked, in order.
    pub fn items(&self) -> &[(NodeId, u64)] {
        &self.items
    }

    /// Whether the first `end` items are ranked.
    pub fn covers(&self, end: usize) -> bool {
        end <= self.items.len() || self.items.len() == self.weighted
    }

    /// Whether it can be served to a request that found the tree as `now`:
    /// after the scan, if it was made after the scan from the same
    /// revision; while scanning, if the folder's totals haven't changed
    /// since, or it's younger than `refresh`.
    pub fn current(&self, now: &Basis, refresh: Duration) -> bool {
        if now.finished {
            self.basis.finished && self.basis.revision == now.revision
        } else {
            self.basis.signature == now.signature
                || now.at.saturating_duration_since(self.basis.at) < refresh
        }
    }
}

/// The ranking kept for one key, and the walk making its next one.
#[derive(Debug)]
pub struct Entry {
    key: Key,
    ranking: Option<Ranking>,
    walk: Option<Walk>,
    /// The walk taken to be finished, until its ranking is kept or it's
    /// given up; other requests wait for it rather than walk again.
    finishing: Option<u64>,
    /// Walks started, for tests.
    walks: u64,
    used: u64,
}

impl Entry {
    pub fn ranking(&self) -> Option<&Ranking> {
        self.ranking.as_ref()
    }

    /// The walk to go on with for a request that found the tree as `now`
    /// and needs the first `end` items: the one under way if it's still
    /// valid and keeps enough, else a new one. The files under a folder are
    /// kept to at least [`MIN_RANKED`] and as many as before; a deeper page
    /// at least doubles that.
    pub fn walk(&mut self, now: &Basis, end: usize) -> &mut Walk {
        let want = match self.key.scope {
            Scope::Children => usize::MAX,
            Scope::Files => {
                let had = self.ranking.as_ref().map_or(0, |r| r.items.len());
                let deeper = if end > had { had.saturating_mul(2) } else { 0 };
                end.max(MIN_RANKED).max(had).max(deeper)
            }
        };
        if self
            .walk
            .as_ref()
            .is_some_and(|w| !w.valid_for(now) || w.wants() < want)
        {
            self.walk = None;
        }
        if self.walk.is_none() {
            self.walks += 1;
        }
        self.walk
            .get_or_insert_with(|| Walk::new(self.key, *now, want))
    }

    /// Takes the walk under way to finish it, noting that it's being
    /// finished (see [`Entry::finishing`]) until [`Entry::keep`] is given
    /// its ranking or [`Rankings::give_up`] its id.
    pub fn take_walk(&mut self) -> Option<Walk> {
        let walk = self.walk.take()?;
        self.finishing = Some(walk.id);
        Some(walk)
    }

    /// Whether a walk taken by [`Entry::take_walk`] is still being finished.
    pub fn finishing(&self) -> bool {
        self.finishing.is_some()
    }

    /// Walks started for this entry.
    pub fn walks(&self) -> u64 {
        self.walks
    }

    /// Keeps `ranking` unless the one kept is newer (a request that took
    /// longer finishing an older walk mustn't replace it), and ends its
    /// walk's finishing. Returns whether it was kept.
    pub fn keep(&mut self, ranking: Ranking) -> bool {
        if self.finishing == Some(ranking.walk) {
            self.finishing = None;
        }
        if self.ranking.as_ref().is_some_and(|r| r.newer_than(&ranking)) {
            return false;
        }
        self.ranking = Some(ranking);
        true
    }

    /// Drops the ranking kept.
    pub fn forget(&mut self) {
        self.ranking = None;
    }

    fn cost(&self) -> usize {
        ENTRY_COST
            + self.ranking.as_ref().map_or(0, |r| r.items.len())
            + self.walk.as_ref().map_or(0, Walk::kept)
    }
}

/// The rankings kept, the least recently used dropped first to stay within
/// [`RANKING_ITEMS`].
#[derive(Debug)]
pub struct Rankings {
    entries: Vec<Entry>,
    limit: usize,
    clock: u64,
}

impl Default for Rankings {
    fn default() -> Self {
        Self::with_limit(RANKING_ITEMS)
    }
}

impl Rankings {
    /// Keeps at most `limit` items, besides the ranking in use.
    pub fn with_limit(limit: usize) -> Self {
        Self {
            entries: Vec::new(),
            limit,
            clock: 0,
        }
    }

    /// The entry for `key`, made if there's none. Others are dropped to
    /// make room for what it holds, but not it.
    pub fn entry(&mut self, key: Key) -> &mut Entry {
        self.clock += 1;
        let i = match self.entries.iter().position(|e| e.key == key) {
            Some(i) => i,
            None => {
                self.entries.push(Entry {
                    key,
                    ranking: None,
                    walk: None,
                    finishing: None,
                    walks: 0,
                    used: 0,
                });
                self.entries.len() - 1
            }
        };
        self.entries[i].used = self.clock;
        self.trim(key);
        let i = self
            .entries
            .iter()
            .position(|e| e.key == key)
            .expect("the entry asked for is kept");
        &mut self.entries[i]
    }

    /// Whether a walk for `key` is being finished.
    pub fn finishing(&self, key: Key) -> bool {
        self.entries
            .iter()
            .any(|e| e.key == key && e.finishing.is_some())
    }

    /// Ends the finishing of walk `id` for `key`, if it's still under way,
    /// when its ranking won't be kept.
    pub fn give_up(&mut self, key: Key, id: u64) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.key == key)
            && e.finishing == Some(id)
        {
            e.finishing = None;
        }
    }

    /// Drops the least recently used entries other than `keep` until the
    /// rest fit.
    pub fn trim(&mut self, keep: Key) {
        while self.entries.len() > 1
            && (self.entries.len() > MAX_RANKINGS || self.items() > self.limit)
        {
            let Some(i) = self
                .entries
                .iter()
                .enumerate()
                .filter(|(_, e)| e.key != keep)
                .min_by_key(|(_, e)| e.used)
                .map(|(i, _)| i)
            else {
                break;
            };
            self.entries.swap_remove(i);
        }
    }

    /// Items held, counting each entry's own cost.
    pub fn items(&self) -> usize {
        self.entries.iter().map(Entry::cost).sum()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{files_under, small_children};
    use crate::search::Matcher;
    use crate::size::Allocated;
    use crate::tree::FileSizes;

    /// xorshift64*, so every run builds the same trees.
    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D) % n.max(1)
        }
    }

    /// Adds `n` random items under random folders of `dirs`, with sizes from
    /// a small range so there are plenty of ties (and zeros).
    fn grow(t: &mut Tree, dirs: &mut Vec<NodeId>, rng: &mut Rng, n: usize) {
        for i in 0..n {
            let parent = dirs[rng.below(dirs.len() as u64) as usize];
            if rng.below(5) == 0 {
                let d = t.add_dir(parent, format!("d{i}").as_ref()).unwrap();
                dirs.push(d);
            } else {
                let ext = ["a", "b"][rng.below(2) as usize];
                let logical = rng.below(8);
                let allocated = match rng.below(4) {
                    0 => Allocated::UNKNOWN,
                    _ => Allocated::known(rng.below(6) * 4096),
                };
                let sizes = FileSizes { logical, allocated };
                let name = format!("f{}.{ext}", t.len());
                t.add_file(parent, name.as_ref(), sizes, None, 0).unwrap();
            }
        }
    }

    fn random_tree(seed: u64, n: usize) -> (Tree, Vec<NodeId>) {
        let mut t = Tree::new("/r");
        let mut dirs = vec![NodeId::ROOT];
        grow(&mut t, &mut dirs, &mut Rng(seed), n);
        (t, dirs)
    }

    fn search(query: &str, tree: &Tree) -> Search {
        let mut s = Search::new(Matcher::new(query).unwrap());
        assert!(s.catch_up(tree, usize::MAX));
        s
    }

    fn basis(tree: &Tree, key: Key, filter: Option<&Search>, revision: u64) -> Basis {
        Basis {
            revision,
            finished: true,
            signature: Signature::of(tree, key.folder, filter),
            at: Instant::now(),
        }
    }

    /// Ranking afresh in one go: every item sorted, and the empty count.
    fn reference(tree: &Tree, key: Key, filter: Option<&Search>) -> (Vec<(NodeId, u64)>, usize) {
        match key.scope {
            Scope::Children => small_children(tree, key.folder, key.metric, filter, usize::MAX),
            Scope::Files => {
                let (files, empty) = files_under(tree, key.folder, key.metric, filter);
                let mut all: Vec<_> = files
                    .into_iter()
                    .map(|f| (f, key.metric.weight_in(tree, filter, f)))
                    .collect();
                all.sort_unstable_by_key(|&(n, w)| (Reverse(w), n));
                (all, empty)
            }
        }
    }

    fn walk(tree: &Tree, key: Key, filter: Option<&Search>, want: usize, rng: &mut Rng) -> Ranking {
        let mut w = Walk::new(key, basis(tree, key, filter, 1), want);
        while !w.step(tree, filter, 1 + rng.below(40) as usize) {}
        w.finish()
    }

    #[test]
    fn walked_pages_match_a_full_sort() {
        for seed in 1..=60 {
            let mut rng = Rng(seed * 7919);
            let (t, dirs) = random_tree(seed, 400 + rng.below(400) as usize);
            let a = search("*.a", &t);
            for _ in 0..12 {
                let folder = match rng.below(3) {
                    0 => NodeId::ROOT,
                    _ => dirs[rng.below(dirs.len() as u64) as usize],
                };
                let scope = [Scope::Children, Scope::Files][rng.below(2) as usize];
                let metric = [Metric::Logical, Metric::Allocated][rng.below(2) as usize];
                let filter = (rng.below(2) == 0).then_some(&a);
                let key = Key {
                    scope,
                    folder,
                    metric,
                    search: filter.map(|_| 1),
                };
                let want = match scope {
                    Scope::Children => usize::MAX,
                    Scope::Files => 1 + rng.below(60) as usize,
                };
                let r = walk(&t, key, filter, want, &mut rng);
                let (all, empty) = reference(&t, key, filter);
                assert_eq!(r.total(), all.len(), "seed {seed}");
                assert_eq!(r.empty(), empty, "seed {seed}");
                assert_eq!(r.items(), &all[..want.min(all.len())], "seed {seed}");
                assert!(r.covers(want.min(all.len())));
                assert_eq!(r.covers(want.saturating_add(1)), want >= all.len());
            }
        }
    }

    #[test]
    fn deeper_pages_walk_again_for_more_and_match_a_full_sort() {
        let (t, _) = random_tree(42, 8_000);
        let key = Key {
            scope: Scope::Files,
            folder: NodeId::ROOT,
            metric: Metric::Logical,
            search: None,
        };
        let (all, _) = reference(&t, key, None);
        assert!(all.len() > 3 * MIN_RANKED, "enough files to page through");
        let mut rankings = Rankings::default();
        let now = basis(&t, key, None, 1);
        let mut pages = Vec::new();
        let mut walks = 0;
        for page in 0..all.len().div_ceil(200) {
            let (start, end) = (page * 200, ((page + 1) * 200).min(all.len()));
            let entry = rankings.entry(key);
            if !entry.ranking().is_some_and(|r| r.covers(end)) {
                walks += 1;
                while !entry.walk(&now, end).step(&t, None, 777) {}
                let ranking = entry.take_walk().unwrap().finish();
                entry.keep(ranking);
            }
            let r = entry.ranking().unwrap();
            assert!(r.current(&now, REFRESH));
            pages.extend_from_slice(&r.items()[start..end]);
        }
        assert_eq!(pages, all);
        assert!(walks <= 4, "each walk at least doubles: {walks}");
    }

    #[test]
    fn walks_across_a_growing_tree_list_only_what_exists_and_redo_exactly() {
        for seed in 1..=20 {
            let mut rng = Rng(seed * 104_729);
            let (mut t, mut dirs) = random_tree(seed, 300);
            for scope in [Scope::Children, Scope::Files] {
                let key = Key {
                    scope,
                    folder: NodeId::ROOT,
                    metric: Metric::Logical,
                    search: None,
                };
                let mut now = basis(&t, key, None, 1);
                now.finished = false;
                let mut w = Walk::new(key, now, usize::MAX);
                // The scanner adds to the tree between steps.
                while !w.step(&t, None, 1 + rng.below(30) as usize) {
                    let n = 1 + rng.below(20) as usize;
                    grow(&mut t, &mut dirs, &mut rng, n);
                }
                let r = w.finish();
                let (all, _) = reference(&t, key, None);
                let mut seen = std::collections::HashSet::new();
                for pair in r.items().windows(2) {
                    assert!((Reverse(pair[0].1), pair[0].0) < (Reverse(pair[1].1), pair[1].0));
                }
                // Folders grow meanwhile, so only the nodes are compared.
                for item in r.items() {
                    assert!(seen.insert(item.0), "listed once");
                    assert!(
                        all.iter().any(|a| a.0 == item.0),
                        "seed {seed}: {item:?} is in the tree"
                    );
                }
                // A walk made once nothing changes is exact.
                let again = walk(&t, key, None, usize::MAX, &mut rng);
                assert_eq!(again.items(), &all[..]);
            }
        }
    }

    #[test]
    fn deletes_between_pages_are_never_served() {
        for seed in 1..=30 {
            let mut rng = Rng(seed * 31);
            let (mut t, dirs) = random_tree(seed, 2_500);
            let b = search("*.b", &t);
            let key = Key {
                scope: Scope::Files,
                folder: NodeId::ROOT,
                metric: Metric::Logical,
                search: Some(1),
            };
            let mut rankings = Rankings::default();
            let first = basis(&t, key, Some(&b), 1);
            let entry = rankings.entry(key);
            while !entry.walk(&first, 200).step(&t, Some(&b), 500) {}
            let ranking = entry.take_walk().unwrap().finish();
            entry.keep(ranking);

            // A walk for a second page is under way when items are deleted.
            let entry = rankings.entry(key);
            let deep = entry.ranking().unwrap().items().len() + 1;
            assert!(!entry.walk(&first, deep).step(&t, Some(&b), 100));
            let shown = entry.ranking().unwrap().items()[..200].to_vec();
            for _ in 0..1 + rng.below(4) {
                let gone = match rng.below(2) {
                    0 => shown[rng.below(200) as usize].0,
                    _ => dirs[1 + rng.below(dirs.len() as u64 - 1) as usize],
                };
                t.remove(gone);
            }
            let b = search("*.b", &t);
            let now = basis(&t, key, Some(&b), 2);
            let entry = rankings.entry(key);
            assert!(!entry.ranking().unwrap().current(&now, REFRESH));
            let walk = entry.walk(&now, 400);
            assert_eq!(walk.basis().revision, 2, "the old walk is dropped");
            while !walk.step(&t, Some(&b), 500) {}
            let ranking = entry.take_walk().unwrap().finish();
            let (all, _) = reference(&t, key, Some(&b));
            assert_eq!(ranking.items(), &all[..ranking.items().len()]);
            assert!(ranking.items().iter().all(|&(n, _)| !t.is_removed(n)));
            assert!(ranking.current(&now, REFRESH));
        }
    }

    #[test]
    fn an_older_ranking_finished_late_does_not_replace_a_newer_one() {
        let (t, _) = random_tree(11, 500);
        let key = Key {
            scope: Scope::Files,
            folder: NodeId::ROOT,
            metric: Metric::Logical,
            search: None,
        };
        let start = basis(&t, key, None, 0);
        let made = |revision, finished, after| {
            let b = Basis {
                revision,
                finished,
                at: start.at + Duration::from_millis(after),
                ..start
            };
            let mut w = Walk::new(key, b, 10);
            while !w.step(&t, None, 100) {}
            w
        };
        let mut rankings = Rankings::default();
        let entry = rankings.entry(key);
        // Two requests walk while scanning; the later walk finishes first.
        let older = made(1, false, 0);
        let newer = made(2, false, 5);
        assert!(entry.keep(newer.finish()));
        assert!(!entry.keep(older.finish()), "older revision refused");
        assert_eq!(entry.ranking().unwrap().basis().revision, 2);
        // At one revision, the later look wins, and after the scan beats
        // during it.
        assert!(entry.keep(made(2, false, 9).finish()));
        assert!(!entry.keep(made(2, false, 7).finish()));
        assert!(entry.keep(made(2, true, 0).finish()));
        assert!(!entry.keep(made(2, false, 50).finish()));
        assert!(entry.ranking().unwrap().basis().finished);
        assert!(entry.keep(made(3, true, 0).finish()));
    }

    #[test]
    fn a_walk_being_finished_is_marked_until_kept_or_given_up() {
        let (t, _) = random_tree(12, 300);
        let key = Key {
            scope: Scope::Children,
            folder: NodeId::ROOT,
            metric: Metric::Logical,
            search: None,
        };
        let now = basis(&t, key, None, 1);
        let mut rankings = Rankings::default();
        let entry = rankings.entry(key);
        while !entry.walk(&now, 0).step(&t, None, 25) {}
        // Steps by several requests go on with the one walk.
        assert_eq!(entry.walks(), 1);
        let walk = entry.take_walk().unwrap();
        assert!(entry.finishing() && rankings.finishing(key));
        // Another walk's ranking doesn't end this one's finishing.
        let entry = rankings.entry(key);
        let mut other = Walk::new(key, now, usize::MAX);
        while !other.step(&t, None, 1_000) {}
        entry.keep(other.finish());
        assert!(entry.finishing());
        let id = walk.id();
        entry.keep(walk.finish());
        assert!(!rankings.finishing(key));

        // Given up (say, edited since): the mark goes, and only for its own
        // walk.
        let entry = rankings.entry(key);
        while !entry.walk(&now, 0).step(&t, None, 1_000) {}
        let walk = entry.take_walk().unwrap();
        rankings.give_up(key, id);
        assert!(rankings.finishing(key), "an earlier walk's id");
        rankings.give_up(key, walk.id());
        assert!(!rankings.finishing(key));
        assert_eq!(rankings.entry(key).walks(), 2);
    }

    #[test]
    fn while_scanning_a_ranking_lasts_until_its_folder_changes_and_refresh_passes() {
        let (mut t, mut dirs) = random_tree(9, 200);
        let key = Key {
            scope: Scope::Children,
            folder: NodeId::ROOT,
            metric: Metric::Logical,
            search: None,
        };
        let sub = dirs[1];
        let made = Basis {
            revision: 5,
            finished: false,
            signature: Signature::of(&t, NodeId::ROOT, None),
            at: Instant::now(),
        };
        let mut w = Walk::new(key, made, usize::MAX);
        while !w.step(&t, None, 10) {}
        let r = w.finish();
        let later = |t: &Tree, folder: NodeId, revision: u64, after: Duration| Basis {
            revision,
            finished: false,
            signature: Signature::of(t, folder, None),
            at: made.at + after,
        };

        // Unchanged: kept however long and however many revisions later.
        assert!(r.current(
            &later(&t, NodeId::ROOT, 9, Duration::from_secs(60)),
            REFRESH
        ));
        // Changed: kept only until REFRESH has passed.
        let f = t
            .add_file(sub, "new".as_ref(), sizes(1_000), None, 0)
            .unwrap();
        assert!(r.current(&later(&t, NodeId::ROOT, 9, REFRESH / 2), REFRESH));
        assert!(!r.current(&later(&t, NodeId::ROOT, 9, REFRESH), REFRESH));
        // A re-measure counts as a change.
        let (mut t2, _) = random_tree(9, 200);
        let made2 = Basis {
            signature: Signature::of(&t2, NodeId::ROOT, None),
            ..made
        };
        let file = (0..t2.len() as u32)
            .map(NodeId::from_index)
            .find(|&n| !t2.is_dir(n))
            .unwrap();
        t2.update_file(file, sizes(t2.logical(file) + 1));
        assert_ne!(Signature::of(&t2, NodeId::ROOT, None), made2.signature);

        // A ranking of another folder isn't affected by changes elsewhere.
        let other = dirs
            .iter()
            .copied()
            .find(|&d| d != sub && t.parent(sub) != Some(d) && t.parent(d) == Some(NodeId::ROOT))
            .unwrap_or_else(|| {
                let d = t.add_dir(NodeId::ROOT, "other".as_ref()).unwrap();
                dirs.push(d);
                d
            });
        let before = Signature::of(&t, other, None);
        t.update_file(f, sizes(5));
        assert_eq!(Signature::of(&t, other, None), before);

        // Once the scan has finished, only the same revision will do, and
        // only for a ranking made after the scan.
        let finished = |revision| Basis {
            revision,
            finished: true,
            ..later(&t, NodeId::ROOT, revision, Duration::ZERO)
        };
        assert!(!r.current(&finished(5), REFRESH), "made while scanning");
        let mut w = Walk::new(key, finished(10), usize::MAX);
        assert!(!w.valid_for(&finished(11)));
        assert!(w.valid_for(&finished(10)));
        while !w.step(&t, None, 10) {}
        let r = w.finish();
        assert!(r.current(&finished(10), REFRESH));
        assert!(!r.current(&finished(11), REFRESH));
    }

    fn sizes(logical: u64) -> FileSizes {
        FileSizes {
            logical,
            allocated: Allocated::known(logical),
        }
    }

    #[test]
    fn walks_from_while_scanning_dont_go_on_after_it() {
        let (t, _) = random_tree(3, 100);
        let key = Key {
            scope: Scope::Files,
            folder: NodeId::ROOT,
            metric: Metric::Logical,
            search: None,
        };
        let mut scanning = basis(&t, key, None, 4);
        scanning.finished = false;
        let w = Walk::new(key, scanning, 10);
        assert!(w.valid_for(&Basis {
            revision: 6,
            ..scanning
        }));
        assert!(!w.valid_for(&basis(&t, key, None, 4)), "the scan finished");
    }

    #[test]
    fn rankings_stay_within_their_budget() {
        let (t, dirs) = random_tree(5, 3_000);
        let mut rankings = Rankings::with_limit(1_000);
        let mut last = None;
        for (i, &folder) in dirs.iter().take(40).enumerate() {
            let key = Key {
                scope: Scope::Children,
                folder: if i == 39 { NodeId::ROOT } else { folder },
                metric: Metric::Logical,
                search: None,
            };
            let now = basis(&t, key, None, 1);
            let entry = rankings.entry(key);
            while !entry.walk(&now, 10).step(&t, None, 50) {}
            let ranking = entry.take_walk().unwrap().finish();
            entry.keep(ranking);
            rankings.trim(key);
            assert!(rankings.len() <= MAX_RANKINGS);
            last = Some(key);
        }
        let last = last.unwrap();
        assert!(
            rankings.entry(last).ranking().is_some(),
            "the ranking in use is kept"
        );
        let mut files = Key {
            scope: Scope::Files,
            ..last
        };
        for folder in [dirs[1], dirs[2], dirs[3]] {
            files.folder = folder;
            let now = basis(&t, files, None, 1);
            let entry = rankings.entry(files);
            while !entry.walk(&now, 10).step(&t, None, 50) {}
            let ranking = entry.take_walk().unwrap().finish();
            entry.keep(ranking);
            rankings.trim(files);
            assert!(rankings.items() <= 1_000 || rankings.len() == 1);
        }
    }
}
