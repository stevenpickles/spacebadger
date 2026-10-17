//! Compares a scan with a [`Snapshot`] of an earlier scan of the same root.
//!
//! Items are matched by their path relative to the root, one folder at a
//! time: each current folder's children are looked up by name among its
//! earlier counterpart's children. A [`Comparison`] keeps the result as one
//! number per current node, so asking about any node afterwards is a lookup.
//!
//! An item that isn't found was added since, with two exceptions. A file
//! that was below the snapshot's file threshold then is recorded only by
//! name hash, so its earlier size is known only to be under the threshold:
//! once it is over, it counts as grown by at least the difference, and
//! otherwise nothing is said about it. And a folder whose contents weren't
//! read, then or now, can't say what was added or removed inside it, so it
//! and everything under it is left unknown. A renamed item counts as
//! removed under its old name and new under its new one.

use crate::snapshot::{Snapshot, fold, name_hash};
use crate::tree::{DirState, NodeId, Tree};
use std::cmp::Reverse;

const SMALL: u32 = u32::MAX - 2;
const NEW: u32 = u32::MAX - 1;
const UNKNOWN: u32 = u32::MAX;

/// Where a current node stands against the snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// The same item, as snapshot entry `n`.
    Was(u32),
    /// A file that was there but below the snapshot's file threshold.
    WasSmall,
    /// Not there before.
    New,
    /// Its earlier size isn't known: in a folder whose contents weren't
    /// read, then or now, or a node the comparison doesn't cover.
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
    /// Logical bytes then; `None` if new, or if only `below` is known.
    pub before: Option<u64>,
    /// For a file that was below the threshold then: the threshold, which
    /// its earlier size was under.
    pub below: Option<u64>,
    /// Logical bytes now; `None` if removed.
    pub after: Option<u64>,
}

impl Change {
    /// Bytes gained (positive) or lost; for a file that was below the
    /// threshold, the least it can have grown by.
    pub fn delta(&self) -> i128 {
        let before = self.before.or(self.below).unwrap_or(0);
        i128::from(self.after.unwrap_or(0)) - i128::from(before)
    }

    /// Not there before (and not merely small).
    pub fn is_new(&self) -> bool {
        self.before.is_none() && self.below.is_none()
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

/// A snapshot's children by parent, each folder's in order of their names
/// as compared (folded when the snapshot ignores case), so one of them can
/// be found by name with a binary search. Shared by [`Comparison`] and
/// [`Prior`](crate::prior::Prior).
#[derive(Debug, Clone)]
pub(crate) struct ChildIndex {
    /// Children by parent, as ranges of `children`.
    child_start: Vec<u32>,
    children: Vec<u32>,
    /// Folded names of the entries, as ranges of `keys`, when case is
    /// ignored; empty otherwise (the names are compared as they are).
    keys: Vec<u8>,
    key_start: Vec<usize>,
    insensitive: bool,
}

impl ChildIndex {
    /// Children in snapshot order; [`ChildIndex::sort`] orders a folder's
    /// for [`ChildIndex::named`].
    pub(crate) fn new(snap: &Snapshot) -> Self {
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
        Self {
            child_start,
            children,
            keys,
            key_start,
            insensitive,
        }
    }

    pub(crate) fn insensitive(&self) -> bool {
        self.insensitive
    }

    /// Entry `e`'s name as compared.
    pub(crate) fn key<'a>(&'a self, snap: &'a Snapshot, e: u32) -> &'a [u8] {
        if self.insensitive {
            &self.keys[self.key_start[e as usize]..self.key_start[e as usize + 1]]
        } else {
            snap.name(e)
        }
    }

    fn range(&self, entry: u32) -> std::ops::Range<usize> {
        let e = entry as usize;
        self.child_start[e] as usize..self.child_start[e + 1] as usize
    }

    pub(crate) fn children(&self, entry: u32) -> &[u32] {
        &self.children[self.range(entry)]
    }

    /// Orders `entry`'s children by name as compared, exact names breaking
    /// ties.
    pub(crate) fn sort(&mut self, snap: &Snapshot, entry: u32) {
        let range = self.range(entry);
        let mut kids = std::mem::take(&mut self.children);
        kids[range].sort_unstable_by(|&a, &b| {
            self.key(snap, a)
                .cmp(self.key(snap, b))
                .then(snap.name(a).cmp(snap.name(b)))
        });
        self.children = kids;
    }

    /// Where `entry`'s children named `folded` (as compared) are, within
    /// [`ChildIndex::children`]; the folder's children must be sorted.
    pub(crate) fn named(
        &self,
        snap: &Snapshot,
        entry: u32,
        folded: &[u8],
    ) -> std::ops::Range<usize> {
        let kids = self.children(entry);
        let from = kids.partition_point(|&k| self.key(snap, k) < folded);
        let to = from + kids[from..].partition_point(|&k| self.key(snap, k) == folded);
        from..to
    }

    /// Lets the folded names go once every lookup is done.
    fn drop_keys(&mut self) {
        self.keys = Vec::new();
        self.key_start = Vec::new();
    }
}

/// A scan matched against a snapshot.
#[derive(Debug, Clone)]
pub struct Comparison {
    /// Per current node: its snapshot entry, [`SMALL`], [`NEW`], or
    /// [`UNKNOWN`].
    matched: Vec<u32>,
    /// Per snapshot entry: whether it's accounted for (a current node
    /// matched it, or it's inside a folder that couldn't be read now), so
    /// it isn't listed as removed.
    seen: Vec<bool>,
    /// Snapshot children by parent (their folded names dropped).
    index: ChildIndex,
    threshold: u64,
}

impl Comparison {
    /// Matches every node of `tree` with `snap`, comparing names as the
    /// snapshot was taken (ignoring case on Windows and macOS).
    pub fn new(tree: &Tree, snap: &Snapshot) -> Self {
        let n = snap.len();
        let mut index = ChildIndex::new(snap);
        let insensitive = index.insensitive();

        let mut matched = vec![UNKNOWN; tree.len()];
        let mut seen = vec![false; n];
        matched[NodeId::ROOT.index()] = if snap.was_read(0) && tree.fully_listed(NodeId::ROOT) {
            0
        } else {
            UNKNOWN
        };
        seen[0] = true;
        let mut pairs = vec![(NodeId::ROOT, 0u32)];
        let mut added = Vec::new();
        let mut buf = Vec::new();
        let mut kids_now: Vec<NodeId> = Vec::new();
        let mut pending = Vec::new();
        let mut accounted = Vec::new();
        while let Some((dir, entry)) = pairs.pop() {
            index.sort(snap, entry);
            let kids = index.children(entry);
            let read_then = snap.was_read(entry);
            kids_now.clear();
            kids_now.extend(tree.children(dir));
            // Exact names first, so `A` and `a` side by side (a
            // case-sensitive folder) each find their own earlier self;
            // then names that match only ignoring case, where that's
            // unambiguous.
            pending.clear();
            for &c in &kids_now {
                let raw = tree.name(c).as_encoded_bytes();
                let folded = fold(raw, insensitive, &mut buf);
                let std::ops::Range {
                    start: from,
                    end: to,
                } = index.named(snap, entry, folded);
                match kids[from..to]
                    .iter()
                    .find(|&&k| !seen[k as usize] && snap.name(k) == raw)
                {
                    Some(&s) => {
                        seen[s as usize] = true;
                        matched[c.index()] = s;
                    }
                    None => pending.push((c, from, to)),
                }
            }
            for &(c, from, to) in &pending {
                let found = match to - from {
                    0 => None,
                    1 if !seen[kids[from] as usize] => Some(kids[from]),
                    // Taken by an exact match, or several to choose from.
                    _ => {
                        matched[c.index()] = UNKNOWN;
                        continue;
                    }
                };
                matched[c.index()] = match found {
                    Some(s) => {
                        seen[s as usize] = true;
                        s
                    }
                    None if !read_then => UNKNOWN,
                    None if !tree.is_dir(c) && {
                        let folded = fold(tree.name(c).as_encoded_bytes(), insensitive, &mut buf);
                        snap.had_small_file(entry, name_hash(folded))
                    } =>
                    {
                        SMALL
                    }
                    None => NEW,
                };
            }
            // Now settle each match: a folder where a file was (or the
            // reverse) is new, and folders unread then or now are unknown.
            for &c in &kids_now {
                let s = matched[c.index()];
                if s >= SMALL {
                    if s == NEW && tree.is_dir(c) {
                        added.push(c);
                    }
                    continue;
                }
                let is_dir = tree.is_dir(c);
                if snap.is_dir(s) != is_dir {
                    seen[s as usize] = false;
                    matched[c.index()] = NEW;
                    if is_dir {
                        added.push(c);
                    }
                } else if is_dir && tree.dir_state(c) != DirState::Listed {
                    // Unread now: nothing under it can be called removed.
                    matched[c.index()] = UNKNOWN;
                    accounted.push(s);
                    while let Some(e) = accounted.pop() {
                        let e = e as usize;
                        seen[e] = true;
                        accounted.extend_from_slice(index.children(e as u32));
                    }
                } else if is_dir {
                    // Read only in part, then or now: its own total isn't
                    // comparable, but the children found can be.
                    if !snap.was_read(s) || !tree.fully_listed(c) {
                        matched[c.index()] = UNKNOWN;
                    }
                    pairs.push((c, s));
                }
            }
            // Some entries here couldn't be read now: an earlier child not
            // found may be one of them, so it isn't called removed.
            if !tree.fully_listed(dir) {
                accounted.extend(kids.iter().filter(|&&k| !seen[k as usize]));
                while let Some(e) = accounted.pop() {
                    let e = e as usize;
                    seen[e] = true;
                    accounted.extend_from_slice(index.children(e as u32));
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
        index.drop_keys();
        Self {
            matched,
            seen,
            index,
            threshold: snap.header().threshold,
        }
    }

    pub fn status(&self, node: NodeId) -> Status {
        match self.matched.get(node.index()).copied().unwrap_or(UNKNOWN) {
            SMALL => Status::WasSmall,
            NEW => Status::New,
            UNKNOWN => Status::Unknown,
            entry => Status::Was(entry),
        }
    }

    /// The change at `node`, or `None` if its earlier size isn't known (or
    /// it was a small file and still is). Unchanged items give a change
    /// with equal sizes.
    pub fn change(&self, tree: &Tree, snap: &Snapshot, node: NodeId) -> Option<Change> {
        let after = tree.logical(node);
        let (before, below) = match self.status(node) {
            Status::Was(e) => (Some(snap.logical(e)), None),
            Status::WasSmall if after > self.threshold => (None, Some(self.threshold)),
            Status::New => (None, None),
            Status::WasSmall | Status::Unknown => return None,
        };
        Some(Change {
            item: Item::Current(node),
            before,
            below,
            after: Some(after),
        })
    }

    fn snapshot_children(&self, entry: u32) -> &[u32] {
        self.index.children(entry)
    }

    /// The items under `folder`, at any depth, whose size changed: grown,
    /// shrunk, new, or removed (from the snapshot), largest change first.
    /// A new or removed folder stands for everything in it. With
    /// `files_only`, folders are left out and the files in new or removed
    /// folders are listed instead. Items of unknown earlier size and empty
    /// new or removed items are left out.
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
            if tree.is_dir(n) {
                let new = self.status(n) == Status::New;
                if !(new && !files_only) {
                    stack.extend(tree.children(n));
                }
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
                let removed = !self.seen[e as usize];
                if is_dir && (!removed || files_only) {
                    stack.extend_from_slice(self.snapshot_children(e));
                }
                if !removed || (files_only && is_dir) || snap.logical(e) == 0 {
                    continue;
                }
                out.push(Change {
                    item: Item::Removed(e),
                    before: Some(snap.logical(e)),
                    below: None,
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
            (Reverse(c.delta().unsigned_abs()), c.is_new(), removed, id)
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

    /// Marks every folder as read, as after a completed scan.
    fn listed(mut t: Tree) -> Tree {
        for i in 0..t.len() as u32 {
            let n = NodeId::from_index(i);
            if t.is_dir(n) {
                t.set_dir_state(n, DirState::Listed);
            }
        }
        t
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
        t.add_file(docs, "report.pdf".as_ref(), sizes(3 * MB), None, 0)
            .unwrap();
        t.add_file(docs, "notes.txt".as_ref(), sizes(1024), None, 0)
            .unwrap();
        t.add_file(media, "a.mkv".as_ref(), sizes(10 * MB), None, 0)
            .unwrap();
        t.add_file(media, "old.mkv".as_ref(), sizes(4 * MB), None, 0)
            .unwrap();
        t.add_file(tmp, "cache.bin".as_ref(), sizes(2 * MB), None, 0)
            .unwrap();
        t.add_file(NodeId::ROOT, "small.log".as_ref(), sizes(100), None, 0)
            .unwrap();
        listed(t)
    }

    /// After: report.pdf unchanged, notes.txt grew to 5 MB, a.mkv grew to
    /// 12 MB, old.mkv renamed to older.mkv, tmp removed, new/{b.iso 8 MB},
    /// small.log still small, fresh.txt new and small.
    fn after() -> Tree {
        let mut t = Tree::new("/data");
        let docs = t.add_dir(NodeId::ROOT, "docs".as_ref()).unwrap();
        let media = t.add_dir(NodeId::ROOT, "media".as_ref()).unwrap();
        let new = t.add_dir(NodeId::ROOT, "new".as_ref()).unwrap();
        t.add_file(docs, "report.pdf".as_ref(), sizes(3 * MB), None, 0)
            .unwrap();
        t.add_file(docs, "notes.txt".as_ref(), sizes(5 * MB), None, 0)
            .unwrap();
        t.add_file(media, "a.mkv".as_ref(), sizes(12 * MB), None, 0)
            .unwrap();
        t.add_file(media, "older.mkv".as_ref(), sizes(4 * MB), None, 0)
            .unwrap();
        t.add_file(new, "b.iso".as_ref(), sizes(8 * MB), None, 0)
            .unwrap();
        t.add_file(NodeId::ROOT, "small.log".as_ref(), sizes(200), None, 0)
            .unwrap();
        t.add_file(NodeId::ROOT, "fresh.txt".as_ref(), sizes(10), None, 0)
            .unwrap();
        listed(t)
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
        // Below the threshold before and still: size unknown, say nothing.
        assert_eq!(c("small.log"), None);
        assert_eq!(cmp.status(find(&tree, "small.log")), Status::WasSmall);
        // Past it now: grown by at least the difference.
        let notes = c("docs/notes.txt").unwrap();
        const { assert!(1024 < FILE_THRESHOLD) };
        assert_eq!((notes.before, notes.below), (None, Some(FILE_THRESHOLD)));
        assert_eq!(notes.delta(), (4 * MB) as i128);
        assert!(!notes.is_new());

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
        // A new or removed folder stands for what's in it.
        let expected: Vec<(String, i128)> = [
            ("-old.mkv", mb(-4)),
            ("-tmp", mb(-2)),
            ("a.mkv", mb(2)),
            ("docs", mb(5) - 1024),
            ("fresh.txt", 10),
            ("media", mb(2)),
            ("new", mb(8)),
            ("notes.txt", mb(4)),
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
        let mut names: Vec<_> = files
            .iter()
            .map(|c| match c.item {
                Item::Current(n) => tree.name(n).to_string_lossy().into_owned(),
                Item::Removed(e) => format!("-{}", snap.display_name(e)),
            })
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "-cache.bin",
                "-old.mkv",
                "a.mkv",
                "b.iso",
                "fresh.txt",
                "notes.txt",
                "older.mkv"
            ],
            "the files in new and removed folders, not the folders"
        );

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
        t.add_file(d, "IMG.JPG".as_ref(), sizes(2 * MB), None, 0)
            .unwrap();
        t.add_file(d, "Small.txt".as_ref(), sizes(5), None, 0)
            .unwrap();
        let t = listed(t);
        let mut renamed = Tree::new("/data");
        let d = renamed.add_dir(NodeId::ROOT, "photos".as_ref()).unwrap();
        renamed
            .add_file(d, "img.jpg".as_ref(), sizes(3 * MB), None, 0)
            .unwrap();
        renamed
            .add_file(d, "small.TXT".as_ref(), sizes(5), None, 0)
            .unwrap();
        let renamed = listed(renamed);

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
            Status::WasSmall
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
        assert_eq!(removed, 1, "Photos, standing for IMG.JPG too");
    }

    #[test]
    fn a_folder_replacing_a_file_is_new() {
        let mut t = Tree::new("/data");
        t.add_file(NodeId::ROOT, "x".as_ref(), sizes(2 * MB), None, 0)
            .unwrap();
        let t = listed(t);
        let mut now = Tree::new("/data");
        let x = now.add_dir(NodeId::ROOT, "x".as_ref()).unwrap();
        let now = listed(now);
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
    fn folders_unread_then_or_now_are_unknown() {
        let snap = Snapshot::capture(&before(), 0, false);

        // Now unread (say, permission denied): not emptied, nothing removed.
        let mut now = Tree::new("/data");
        let docs = now.add_dir(NodeId::ROOT, "docs".as_ref()).unwrap();
        let media = now.add_dir(NodeId::ROOT, "media".as_ref()).unwrap();
        now.add_file(media, "a.mkv".as_ref(), sizes(10 * MB), None, 0)
            .unwrap();
        now.add_file(media, "old.mkv".as_ref(), sizes(4 * MB), None, 0)
            .unwrap();
        let mut now = listed(now);
        now.set_dir_state(docs, DirState::Failed);
        let cmp = Comparison::new(&now, &snap);
        assert_eq!(cmp.status(docs), Status::Unknown);
        assert_eq!(cmp.change(&now, &snap, docs), None);
        let changes = cmp.changes_under(&now, &snap, NodeId::ROOT, true);
        let removed: Vec<_> = changes
            .iter()
            .filter_map(|c| match c.item {
                Item::Removed(e) => Some(snap.display_name(e)),
                Item::Current(_) => None,
            })
            .collect();
        assert_eq!(removed, ["cache.bin"], "nothing under docs is removed");

        // Unread then: what's in it now isn't new.
        let mut then = Tree::new("/data");
        let media_then = then.add_dir(NodeId::ROOT, "media".as_ref()).unwrap();
        let mut then = listed(then);
        then.set_dir_state(media_then, DirState::Failed);
        let snap = Snapshot::capture(&then, 0, false);
        let now = after();
        let cmp = Comparison::new(&now, &snap);
        let media = find(&now, "media");
        assert_eq!(cmp.status(media), Status::Unknown);
        assert_eq!(cmp.status(find(&now, "media/older.mkv")), Status::Unknown);
        assert!(
            cmp.changes_under(&now, &snap, media, false).is_empty(),
            "no new or removed items in a folder unread then"
        );
    }

    #[test]
    fn entries_that_couldnt_be_read_are_neither_removed_nor_new() {
        let snap = Snapshot::capture(&before(), 0, false);

        // Now `old.mkv` couldn't be read, so it's missing from `media`.
        let mut now = Tree::new("/data");
        let media = now.add_dir(NodeId::ROOT, "media".as_ref()).unwrap();
        now.add_file(media, "a.mkv".as_ref(), sizes(12 * MB), None, 0)
            .unwrap();
        let mut now = listed(now);
        now.mark_unreadable_entries(media);
        let cmp = Comparison::new(&now, &snap);
        assert_eq!(
            cmp.status(media),
            Status::Unknown,
            "its total isn't comparable"
        );
        let a = find(&now, "media/a.mkv");
        assert_eq!(
            cmp.change(&now, &snap, a).unwrap().delta(),
            (2 * MB) as i128
        );
        let removed: Vec<_> = cmp
            .changes_under(&now, &snap, NodeId::ROOT, true)
            .iter()
            .filter_map(|c| match c.item {
                Item::Removed(e) => Some(snap.display_name(e)),
                Item::Current(_) => None,
            })
            .collect();
        assert!(!removed.contains(&"old.mkv".to_owned()), "{removed:?}");

        // Then `media` had an unreadable entry: what's in it now isn't new.
        let mut then = before();
        let media_then = find(&then, "media");
        then.mark_unreadable_entries(media_then);
        let snap = Snapshot::capture(&then, 0, false);
        let now = after();
        let cmp = Comparison::new(&now, &snap);
        assert_eq!(cmp.status(find(&now, "media/older.mkv")), Status::Unknown);
        assert!(matches!(
            cmp.status(find(&now, "media/a.mkv")),
            Status::Was(_)
        ));
    }

    #[test]
    fn exact_names_win_over_names_equal_ignoring_case() {
        // A case-sensitive folder on a case-insensitive system.
        let mut t = Tree::new("/data");
        t.add_file(NodeId::ROOT, "A".as_ref(), sizes(2 * MB), None, 0)
            .unwrap();
        t.add_file(NodeId::ROOT, "a".as_ref(), sizes(3 * MB), None, 0)
            .unwrap();
        let t = listed(t);
        let snap = Snapshot::capture(&t, 0, true);
        let cmp = Comparison::new(&t, &snap);
        assert!(
            cmp.changes_under(&t, &snap, NodeId::ROOT, false).is_empty(),
            "an identical tree has no changes"
        );
        for name in ["A", "a"] {
            let c = cmp.change(&t, &snap, find(&t, name)).unwrap();
            assert_eq!(c.delta(), 0, "{name}");
        }

        // Only `a` is left: it's found exactly, and `A` is removed.
        let mut now = Tree::new("/data");
        now.add_file(NodeId::ROOT, "a".as_ref(), sizes(3 * MB), None, 0)
            .unwrap();
        let now = listed(now);
        let cmp = Comparison::new(&now, &snap);
        assert_eq!(cmp.change(&now, &snap, find(&now, "a")).unwrap().delta(), 0);
        let changes = cmp.changes_under(&now, &snap, NodeId::ROOT, false);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].delta(), -((2 * MB) as i128));

        // A different case only, and unambiguous: still the same file.
        let mut now = Tree::new("/data");
        now.add_file(NodeId::ROOT, "\u{e1}.txt".as_ref(), sizes(3 * MB), None, 0)
            .unwrap();
        let now = listed(now);
        let mut then = Tree::new("/data");
        then.add_file(NodeId::ROOT, "\u{c1}.TXT".as_ref(), sizes(2 * MB), None, 0)
            .unwrap();
        let then = listed(then);
        let cmp = Comparison::new(&now, &Snapshot::capture(&then, 0, true));
        assert_eq!(cmp.status(find(&now, "\u{e1}.txt")), Status::Was(1));
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
