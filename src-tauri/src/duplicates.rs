//! A search for duplicate files in one scan, run in the background on
//! request. See `sb_scan::duplicates` for how files are compared; this keeps
//! what was found and reports it, leaving out files deleted since.
//!
//! Before a delete, the copies of each group that would stay are checked:
//! when the dialog opens, by fingerprint only (provisional), and again as
//! the delete starts, reading them whole ([`Job::retained`]). A file whose
//! group has no copy staying that's confirmed unchanged is kept.

use crate::session::display_path;
use sb_core::selection;
use sb_core::tree::{NodeId, Tree, flags};
use sb_protocol as proto;
use sb_scan::duplicates::{
    self, Contents, Counters, Digest, Event, Fingerprint, Group, Plan, Recheck, Skip,
};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// While reading, status updates go out at most this often.
pub const REPORT_EVERY: Duration = Duration::from_millis(250);

/// What to look through, fixed when the search starts.
pub struct Scope {
    pub generation: u64,
    pub folder: u32,
    pub path: String,
    pub query: Option<String>,
    pub min_size: u64,
}

pub struct Job {
    pub id: u32,
    scope: Scope,
    plan: Plan,
    started: Instant,
    cancel: AtomicBool,
    counters: Counters,
    found: Mutex<Found>,
}

struct Found {
    groups: Vec<Group>,
    skipped: proto::DuplicateSkips,
    /// Bumped per group found.
    version: u32,
    phase: proto::DuplicatesPhase,
    elapsed: Option<Duration>,
}

impl Job {
    pub fn new(id: u32, scope: Scope, plan: Plan) -> Self {
        let skipped = proto::DuplicateSkips {
            cloud: plan.cloud.len() as u32,
            ..Default::default()
        };
        Self {
            id,
            scope,
            plan,
            started: Instant::now(),
            cancel: AtomicBool::new(false),
            counters: Counters::default(),
            found: Mutex::new(Found {
                groups: Vec::new(),
                skipped,
                version: 0,
                phase: proto::DuplicatesPhase::Running,
                elapsed: None,
            }),
        }
    }

    /// Stops reading; what was found stays.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn is_running(&self) -> bool {
        self.found.lock().unwrap().phase == proto::DuplicatesPhase::Running
    }

    /// Reads the planned files, recording groups as they're confirmed.
    /// Blocks until done or cancelled.
    /// A reader that panics ends the search as if stopped, keeping what
    /// was found, rather than leaving it running.
    pub fn run(&self, contents: &impl Contents) {
        let complete = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            duplicates::run(
                &self.plan,
                contents,
                duplicates::READERS,
                &self.cancel,
                &self.counters,
                |event| self.record(event),
            )
        }))
        .unwrap_or(false);
        let mut found = self.found.lock().unwrap_or_else(|e| e.into_inner());
        found.phase = if complete {
            proto::DuplicatesPhase::Complete
        } else {
            proto::DuplicatesPhase::Cancelled
        };
        found.elapsed = Some(self.started.elapsed());
    }

    fn record(&self, event: Event) {
        let mut found = self.found.lock().unwrap();
        match event {
            Event::Found(group) => {
                found.groups.push(group);
                found.version += 1;
            }
            Event::Skipped(_, why) => {
                let s = &mut found.skipped;
                let count = match why {
                    Skip::Cloud => &mut s.cloud,
                    Skip::Vanished => &mut s.vanished,
                    Skip::Changed => &mut s.changed,
                    Skip::Changing => &mut s.changing,
                    Skip::Special => &mut s.special,
                    Skip::InUse => &mut s.in_use,
                    Skip::Denied => &mut s.denied,
                    Skip::Unreadable => &mut s.unreadable,
                };
                *count += 1;
            }
        }
    }

    /// The groups with two or more files still in the scan, most
    /// reclaimable space first.
    fn current(&self, tree: &Tree) -> (Vec<Group>, Summary) {
        let found = self.found.lock().unwrap();
        let mut groups: Vec<Group> = found
            .groups
            .iter()
            .filter_map(|g| {
                let (files, prints) = g
                    .files
                    .iter()
                    .zip(&g.prints)
                    .filter(|&(&n, _)| tree.contains(n) && !tree.is_removed(n))
                    .map(|(&n, &print)| (n, print))
                    .unzip::<NodeId, Fingerprint, Vec<_>, Vec<_>>();
                (files.len() >= 2).then_some(Group {
                    size: g.size,
                    digest: g.digest,
                    files,
                    prints,
                })
            })
            .collect();
        groups.sort_by(|a, b| {
            reclaimable(b)
                .cmp(&reclaimable(a))
                .then(b.size.cmp(&a.size))
        });
        let summary = Summary {
            skipped: found.skipped.clone(),
            version: found.version,
            phase: found.phase,
            elapsed: found.elapsed.unwrap_or_else(|| self.started.elapsed()),
        };
        (groups, summary)
    }

    /// How the search is going; `revision` is the scan's, which changes
    /// when items are deleted.
    pub fn status(&self, tree: &Tree, revision: u64) -> proto::DuplicatesStatus {
        let (groups, found) = self.current(tree);
        let on_disk: Vec<Option<u64>> = groups.iter().map(|g| on_disk(tree, g)).collect();
        proto::DuplicatesStatus {
            generation: self.scope.generation,
            job: self.id,
            folder: self.scope.folder,
            path: self.scope.path.clone(),
            query: self.scope.query.clone(),
            min_size: self.scope.min_size,
            phase: found.phase,
            elapsed_ms: found.elapsed.as_millis() as u64,
            candidates: self.plan.files() as u32,
            total_bytes: self.plan.bytes,
            done_bytes: self.counters.done.load(Ordering::Relaxed),
            read_bytes: self.counters.read.load(Ordering::Relaxed),
            groups: groups.len() as u32,
            files: groups.iter().map(|g| g.files.len() as u32).sum(),
            reclaimable: groups.iter().map(reclaimable).sum(),
            reclaimable_on_disk: on_disk.iter().flatten().sum(),
            on_disk_unknown: on_disk.iter().filter(|d| d.is_none()).count() as u32,
            skipped: found.skipped,
            version: found.version.wrapping_add(revision as u32),
        }
    }

    /// The groups found so far, each with its shortest path first.
    pub fn list(&self, tree: &Tree, revision: u64) -> proto::DuplicatesList {
        let (groups, found) = self.current(tree);
        let total = groups.len() as u32;
        let groups = groups
            .into_iter()
            .take(proto::DUPLICATE_GROUPS_LISTED as usize)
            .map(|g| {
                let mut files: Vec<proto::DuplicateFile> = g
                    .files
                    .iter()
                    .zip(&g.prints)
                    .map(|(&n, print)| proto::DuplicateFile {
                        node: n.index() as u32,
                        parent: tree.parent(n).unwrap_or(NodeId::ROOT).index() as u32,
                        path: display_path(&tree.path(n)),
                        hardlinked: has_other_names(tree, n, print),
                    })
                    .collect();
                files.sort_by(|a, b| {
                    let len = |f: &proto::DuplicateFile| f.path.chars().count();
                    len(a).cmp(&len(b)).then_with(|| a.path.cmp(&b.path))
                });
                proto::DuplicateGroup {
                    size: g.size,
                    on_disk: on_disk(tree, &g),
                    files,
                }
            })
            .collect();
        proto::DuplicatesList {
            generation: self.scope.generation,
            job: self.id,
            version: found.version.wrapping_add(revision as u32),
            total,
            groups,
        }
    }
}

/// Most groups whose staying copies one check looks for on disk, and
/// most copies it looks for per group, so a slow share can't hold up the
/// delete dialog for long.
const CHECK_GROUPS: usize = 200;
const CHECK_COPIES: usize = 8;

/// A delete check whose remaining part looks at the disk, done without
/// holding any lock.
pub struct PendingCheck {
    check: proto::DuplicatesCheck,
    /// Per group: a file that would go, the size, and copies that would
    /// stay with their fingerprints as compared.
    on_disk: Vec<(String, u64, Vec<Staying>)>,
}

/// A copy that would stay: its path, and its fingerprint as compared.
type Staying = (PathBuf, Fingerprint);

/// One group's files a delete would take, by the selected item each goes
/// with, and the files that would stay.
struct Split {
    going: Vec<(NodeId, NodeId)>,
    staying: Vec<(NodeId, Fingerprint)>,
}

/// The selected items, to tell which item takes a file: itself or a
/// selected folder above it.
struct Items(HashSet<NodeId>);

impl Items {
    fn new(tree: &Tree, selected: &[u32]) -> Self {
        Self(
            selection::resolve(tree, selected)
                .items
                .into_iter()
                .collect(),
        )
    }

    fn of(&self, tree: &Tree, n: NodeId) -> Option<NodeId> {
        let mut cur = Some(n);
        while let Some(c) = cur {
            if self.0.contains(&c) {
                return Some(c);
            }
            cur = tree.parent(c);
        }
        None
    }

    /// Splits a group's files still in the scan into going and staying.
    fn split(&self, tree: &Tree, g: &Group) -> Split {
        let mut split = Split {
            going: Vec::new(),
            staying: Vec::new(),
        };
        for (&n, &print) in g.files.iter().zip(&g.prints) {
            if !tree.contains(n) || tree.is_removed(n) {
                continue;
            }
            match self.of(tree, n) {
                Some(item) => split.going.push((n, item)),
                None => split.staying.push((n, print)),
            }
        }
        split
    }
}

impl Job {
    /// Starts checking which groups deleting `selected` (with everything in
    /// selected folders) would leave with no copy: every remaining file
    /// selected, or the ones that would stay gone or changed since they
    /// were compared (by fingerprint; contents are read only by
    /// [`Job::retained`]). Groups already down to one file count too.
    pub fn check(&self, tree: &Tree, selected: &[u32]) -> PendingCheck {
        let items = Items::new(tree, selected);
        let mut pending = PendingCheck {
            check: proto::DuplicatesCheck {
                all_selected: 0,
                kept_gone: 0,
                to_recheck: 0,
                unchecked: 0,
                examples: Vec::new(),
            },
            on_disk: Vec::new(),
        };
        let found = self.found.lock().unwrap();
        for g in &found.groups {
            let split = items.split(tree, g);
            let Some(&(example, _)) = split.going.first() else {
                continue;
            };
            let example = display_path(&tree.path(example));
            if split.staying.is_empty() {
                pending.check.all_selected += 1;
                pending.example(example);
                continue;
            }
            pending.check.to_recheck += 1;
            if pending.on_disk.len() < CHECK_GROUPS {
                let copies = split
                    .staying
                    .iter()
                    .take(CHECK_COPIES)
                    .map(|&(n, print)| (tree.path(n), print))
                    .collect();
                pending.on_disk.push((example, g.size, copies));
            } else {
                pending.check.unchecked += 1;
            }
        }
        pending
    }
}

impl PendingCheck {
    fn example(&mut self, path: String) {
        if self.check.examples.len() < proto::DUPLICATE_CHECK_EXAMPLES {
            self.check.examples.push(path);
        }
    }

    /// Looks on disk for the copies that would stay, unchanged since they
    /// were compared: the same file, length, and modified time.
    pub fn finish(mut self, contents: &impl Contents) -> proto::DuplicatesCheck {
        for (example, size, copies) in std::mem::take(&mut self.on_disk) {
            let unchanged = |(path, print): &Staying| {
                contents
                    .examine(path, size)
                    .is_ok_and(|now| print.unchanged(&now))
            };
            if !copies.iter().any(unchanged) {
                self.check.kept_gone += 1;
                self.example(example);
            }
        }
        self.check
    }
}

/// Why a selected item is kept rather than deleted: no copy of some
/// duplicates in it that would stay could be confirmed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unconfirmed {
    /// Every copy that would stay is gone or has changed since compared.
    Changed,
    /// Some copy couldn't be read to tell.
    Unknown,
    /// Checking was stopped first.
    Stopped,
}

impl Unconfirmed {
    /// The outcome reported for the item.
    pub fn message(self) -> &'static str {
        match self {
            Self::Changed => {
                "Kept: the other copies have changed or gone since they were compared, so this may be the last one."
            }
            Self::Unknown => {
                "Kept: couldn't confirm that another copy still exists, since the other copies couldn't be read."
            }
            Self::Stopped => "Kept: checking that another copy still exists was stopped.",
        }
    }
}

/// Selected items to keep, and why.
pub type Verdicts = HashMap<NodeId, Unconfirmed>;

/// The copies a delete must confirm before moving anything, read without
/// holding any lock.
pub struct Retained {
    groups: Vec<Retaining>,
}

/// One group that would lose files and keep others.
struct Retaining {
    /// The selected items taking its files.
    items: Vec<NodeId>,
    size: u64,
    digest: Digest,
    /// The copies that would stay, with their fingerprints as compared.
    staying: Vec<Staying>,
}

impl Job {
    /// The groups deleting `selected` would take files from while others
    /// stay, for [`Retained::verify`]. Groups losing every file aren't
    /// included: the dialog warned of those, and the user confirmed.
    pub fn retained(&self, tree: &Tree, selected: &[u32]) -> Retained {
        let items = Items::new(tree, selected);
        let found = self.found.lock().unwrap();
        let groups = found
            .groups
            .iter()
            .filter_map(|g| {
                let split = items.split(tree, g);
                if split.going.is_empty() || split.staying.is_empty() {
                    return None;
                }
                let mut taking: Vec<NodeId> = split.going.iter().map(|&(_, i)| i).collect();
                taking.sort_unstable();
                taking.dedup();
                Some(Retaining {
                    items: taking,
                    size: g.size,
                    digest: g.digest,
                    staying: split
                        .staying
                        .iter()
                        .map(|&(n, print)| (tree.path(n), print))
                        .collect(),
                })
            })
            .collect();
        Retained { groups }
    }
}

impl Retained {
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// Bytes [`Retained::verify`] reads if the first copy of each group is
    /// confirmed.
    pub fn bytes(&self) -> u64 {
        self.groups.iter().map(|g| g.size).sum()
    }

    /// Every item taking files from these groups, kept for `why`.
    pub fn keep_all(&self, why: Unconfirmed) -> Verdicts {
        self.groups
            .iter()
            .flat_map(|g| g.items.iter().map(move |&i| (i, why)))
            .collect()
    }

    /// Reads the copies that would stay, group by group, until one is
    /// confirmed unchanged with the group's contents. Items taking files
    /// from a group with none confirmed are kept: an unreadable copy or a
    /// stop leaves it unknown, which is never taken as proof.
    pub fn verify(
        self,
        contents: &impl Contents,
        cancel: &AtomicBool,
        counters: &Counters,
    ) -> Verdicts {
        let mut verdicts = Verdicts::new();
        for g in self.groups {
            let mut verdict = Some(Unconfirmed::Changed);
            for (path, print) in &g.staying {
                match duplicates::recheck(contents, path, print, &g.digest, cancel, counters) {
                    Recheck::Same => {
                        verdict = None;
                        break;
                    }
                    Recheck::Changed => {}
                    Recheck::Unknown(_) => verdict = Some(Unconfirmed::Unknown),
                    Recheck::Cancelled => {
                        verdict = Some(Unconfirmed::Stopped);
                        break;
                    }
                }
            }
            if let Some(why) = verdict {
                for item in g.items {
                    verdicts.entry(item).or_insert(why);
                }
            }
        }
        verdicts
    }
}

/// What [`Job::current`] copies out besides the groups.
struct Summary {
    skipped: proto::DuplicateSkips,
    version: u32,
    phase: proto::DuplicatesPhase,
    elapsed: Duration,
}

/// Content in the copies beyond the first: their length.
fn reclaimable(g: &Group) -> u64 {
    g.size * (g.files.len() as u64).saturating_sub(1)
}

/// Whether a file has other hard-link names: in the scan, or by the count
/// read when it was compared.
fn has_other_names(tree: &Tree, n: NodeId, print: &Fingerprint) -> bool {
    tree.flags(n) & flags::HARDLINKED != 0 || print.links.is_some_and(|l| l > 1)
}

/// The most space on disk deleting all but one copy could free: the
/// copies' allocated sizes as scanned, less the smallest (keeping that one
/// frees the most). `None` when a copy's allocation is unknown, or a copy
/// has other names (deleting it may free nothing) or an unknown number of
/// them.
fn on_disk(tree: &Tree, g: &Group) -> Option<u64> {
    let mut sizes = Vec::with_capacity(g.files.len());
    for (&n, print) in g.files.iter().zip(&g.prints) {
        if print.links.is_none() || has_other_names(tree, n, print) {
            return None;
        }
        sizes.push(tree.file_allocated(n).get()?);
    }
    let smallest = sizes.iter().copied().min().unwrap_or(0);
    Some(sizes.iter().sum::<u64>() - smallest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sb_core::size::Allocated;
    use sb_core::tree::FileSizes;
    use std::time::SystemTime;

    fn print(id: u128, links: Option<u32>) -> Fingerprint {
        Fingerprint {
            id: Some((1, id)),
            len: 4096,
            modified: Some(SystemTime::UNIX_EPOCH),
            links,
        }
    }

    fn group(tree: &mut Tree, allocated: &[Allocated]) -> Group {
        let files = allocated
            .iter()
            .enumerate()
            .map(|(i, &allocated)| {
                let sizes = FileSizes {
                    logical: 4096,
                    allocated,
                };
                tree.add_file(NodeId::ROOT, format!("f{i}").as_ref(), sizes, None, 0)
                    .unwrap()
            })
            .collect::<Vec<_>>();
        Group {
            size: 4096,
            digest: [0; 32],
            prints: (0..files.len())
                .map(|i| print(i as u128, Some(1)))
                .collect(),
            files,
        }
    }

    #[test]
    fn stopping_keeps_the_files_not_yet_confirmed() {
        let dir = std::env::temp_dir().join(format!("sb-retained-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("copy");
        std::fs::write(&path, vec![7; 4096]).unwrap();
        let contents = sb_scan::native::NativeContents;
        let print = contents.examine(&path, 4096).unwrap();
        let retained = Retained {
            groups: vec![Retaining {
                items: vec![NodeId::from_index(5)],
                size: 4096,
                digest: [0; 32],
                staying: vec![(path.clone(), print)],
            }],
        };
        let counters = Counters::default();
        let stop = AtomicBool::new(true);
        assert_eq!(
            retained.verify(&contents, &stop, &counters),
            Verdicts::from([(NodeId::from_index(5), Unconfirmed::Stopped)])
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn space_on_disk_is_estimated_only_when_every_copy_is_known() {
        let mut tree = Tree::new("/r");
        let known = Allocated::known;
        let g = group(&mut tree, &[known(4096), known(4096), known(1024)]);
        assert_eq!(reclaimable(&g), 8192, "content in the extra copies");
        assert_eq!(on_disk(&tree, &g), Some(8192), "all but the smallest");

        // Sparse or compressed: little on disk, whatever the length.
        let sparse = group(&mut tree, &[known(0), known(0)]);
        assert_eq!(
            (reclaimable(&sparse), on_disk(&tree, &sparse)),
            (4096, Some(0))
        );

        let unknown = group(&mut tree, &[known(4096), Allocated::UNKNOWN]);
        assert_eq!(on_disk(&tree, &unknown), None);

        // Another name outside the scan, seen when read.
        let mut linked = group(&mut tree, &[known(4096), known(4096)]);
        linked.prints[1].links = Some(2);
        assert_eq!(on_disk(&tree, &linked), None);
        assert!(has_other_names(&tree, linked.files[1], &linked.prints[1]));
        linked.prints[1].links = None;
        assert_eq!(on_disk(&tree, &linked), None, "unknown names");

        // Another name in the scan.
        let aliased = group(&mut tree, &[known(4096), known(4096)]);
        let alias = tree
            .add_file(
                NodeId::ROOT,
                "alias".as_ref(),
                FileSizes {
                    logical: 4096,
                    allocated: known(4096),
                },
                None,
                0,
            )
            .unwrap();
        tree.mark_alias(alias, aliased.files[0]);
        assert_eq!(on_disk(&tree, &aliased), None);
    }
}
