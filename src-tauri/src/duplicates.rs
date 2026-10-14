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
    self, Confirm, ConfirmProgress, Confirmed, Contents, Counters, Digest, Event, Fingerprint,
    Group, Held, Plan, Skip,
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
    /// Per group: a selected file that would go, the size, and copies that
    /// would stay with their fingerprints as compared.
    on_disk: Vec<(String, u64, Vec<Staying>)>,
}

/// A copy that would stay: its path, and its fingerprint as compared.
type Staying = (PathBuf, Fingerprint);

/// One group's files a delete would take, and the files that would stay.
struct Split {
    /// Files selected themselves, with their fingerprints as compared.
    selected: Vec<(NodeId, Fingerprint)>,
    /// Files going with a selected folder.
    inside: Vec<NodeId>,
    staying: Vec<(NodeId, Fingerprint)>,
}

/// The selected items, to tell whether a file is selected itself or goes
/// with a selected folder.
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

    fn in_folder(&self, tree: &Tree, n: NodeId) -> bool {
        let mut cur = tree.parent(n);
        while let Some(c) = cur {
            if self.0.contains(&c) {
                return true;
            }
            cur = tree.parent(c);
        }
        false
    }

    /// Splits a group's files still in the scan.
    fn split(&self, tree: &Tree, g: &Group) -> Split {
        let mut split = Split {
            selected: Vec::new(),
            inside: Vec::new(),
            staying: Vec::new(),
        };
        for (&n, &print) in g.files.iter().zip(&g.prints) {
            if !tree.contains(n) || tree.is_removed(n) {
                continue;
            }
            if self.0.contains(&n) {
                split.selected.push((n, print));
            } else if self.in_folder(tree, n) {
                split.inside.push(n);
            } else {
                split.staying.push((n, print));
            }
        }
        split
    }
}

impl Job {
    /// Starts checking which groups deleting `selected` (with everything in
    /// selected folders) would leave with no copy, and which selected files
    /// the delete may keep because the copies that would stay are gone or
    /// changed since they were compared (by fingerprint; contents are read
    /// only by [`Job::retained`]). Groups already down to one file count
    /// too.
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
            if split.staying.is_empty() {
                let going = split.selected.first().map(|&(n, _)| n);
                if let Some(example) = going.or(split.inside.first().copied()) {
                    pending.check.all_selected += 1;
                    pending.example(display_path(&tree.path(example)));
                }
                continue;
            }
            // Files going with a folder go regardless; only files selected
            // themselves are kept when no copy staying is confirmed.
            let Some(&(example, _)) = split.selected.first() else {
                continue;
            };
            pending.check.to_recheck += 1;
            if pending.on_disk.len() < CHECK_GROUPS {
                let copies = split
                    .staying
                    .iter()
                    .take(CHECK_COPIES)
                    .map(|&(n, print)| (tree.path(n), print))
                    .collect();
                pending
                    .on_disk
                    .push((display_path(&tree.path(example)), g.size, copies));
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

/// Why a selected file of a group of duplicates is kept rather than
/// deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unconfirmed {
    /// The file itself changed since it was compared.
    NoLongerMatches,
    /// The file itself couldn't be looked at to tell.
    Unchecked,
    /// Every copy that would stay is gone or has changed since compared.
    Changed,
    /// Some copy that would stay couldn't be read or held to tell.
    Unknown,
    /// Checking was stopped first.
    Stopped,
}

impl Unconfirmed {
    /// The outcome reported for the file called `name`.
    pub fn message(self, name: &str) -> String {
        let why = match self {
            Self::NoLongerMatches => {
                "it no longer matches its copies: it changed since they were compared"
            }
            Self::Unchecked => "couldn't check that it still matches its copies",
            Self::Changed => {
                "its other copies have changed or gone since they were compared, so it may be the last one"
            }
            Self::Unknown => {
                "couldn't confirm that another copy still exists: the other copies couldn't be read or held"
            }
            Self::Stopped => "checking that another copy still exists was stopped",
        };
        format!("Kept “{name}”: {why}.")
    }
}

/// Selected files to keep, and why.
pub type Verdicts = HashMap<NodeId, Unconfirmed>;

/// What [`Retained::verify`] found: the files to keep, and the confirmed
/// copies, held unchanged until this is dropped.
pub struct Verified {
    pub verdicts: Verdicts,
    _held: Vec<Held>,
}

impl Verified {
    /// Keeping these files, with no copies held.
    pub fn keeping(verdicts: Verdicts) -> Self {
        Self {
            verdicts,
            _held: Vec::new(),
        }
    }
}

/// The copies a delete must confirm before moving anything, read without
/// holding any lock.
pub struct Retained {
    groups: Vec<Retaining>,
}

/// One group with files selected themselves while others stay.
struct Retaining {
    /// The selected files, with their paths and fingerprints as compared.
    selected: Vec<(NodeId, PathBuf, Fingerprint)>,
    size: u64,
    digest: Digest,
    /// The copies that would stay.
    staying: Vec<Staying>,
}

impl Job {
    /// The groups with files selected themselves (as from the Duplicates
    /// tab) while others stay, for [`Retained::verify`]. Files going with a
    /// selected folder go regardless, and groups losing every file aren't
    /// included: the dialog warned of those, and the user confirmed.
    pub fn retained(&self, tree: &Tree, selected: &[u32]) -> Retained {
        let items = Items::new(tree, selected);
        let found = self.found.lock().unwrap();
        let groups = found
            .groups
            .iter()
            .filter_map(|g| {
                let split = items.split(tree, g);
                if split.selected.is_empty() || split.staying.is_empty() {
                    return None;
                }
                let paths = |files: &[(NodeId, Fingerprint)]| {
                    files
                        .iter()
                        .map(|&(n, print)| (n, tree.path(n), print))
                        .collect::<Vec<_>>()
                };
                Some(Retaining {
                    selected: paths(&split.selected),
                    size: g.size,
                    digest: g.digest,
                    staying: paths(&split.staying)
                        .into_iter()
                        .map(|(_, path, print)| (path, print))
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

    /// Every selected file of these groups, kept for `why`.
    pub fn keep_all(&self, why: Unconfirmed) -> Verdicts {
        self.groups
            .iter()
            .flat_map(|g| g.selected.iter().map(move |&(n, _, _)| (n, why)))
            .collect()
    }

    /// Looks at each selected file, which must be unchanged since it was
    /// compared, then holds and reads the copies that would stay (see
    /// [`duplicates::confirm`]) until one per group is confirmed unchanged
    /// with the group's contents. Selected files of a group with none
    /// confirmed are kept: an unreadable copy or a stop leaves it unknown,
    /// which is never taken as proof.
    pub fn verify(
        self,
        contents: &impl Contents,
        cancel: &AtomicBool,
        progress: &ConfirmProgress,
    ) -> Verified {
        let mut verdicts = Verdicts::new();
        let mut confirming = Vec::new();
        let mut waiting = Vec::new();
        for g in self.groups {
            let mut still = Vec::new();
            for (n, path, print) in g.selected {
                match contents.examine(&path, g.size) {
                    Ok(now) if print.unchanged(&now) => still.push(n),
                    // Gone already: the delete says so.
                    Err(Skip::Vanished) => {}
                    Ok(_) | Err(Skip::Changed | Skip::Changing) => {
                        verdicts.insert(n, Unconfirmed::NoLongerMatches);
                    }
                    Err(_) => {
                        verdicts.insert(n, Unconfirmed::Unchecked);
                    }
                }
            }
            if !still.is_empty() {
                waiting.push(still);
                confirming.push(Confirm {
                    size: g.size,
                    digest: g.digest,
                    copies: g.staying,
                });
            }
        }
        let found =
            duplicates::confirm(contents, &confirming, duplicates::READERS, cancel, progress);
        let mut held = Vec::new();
        for (confirmed, files) in found.into_iter().zip(waiting) {
            let why = match confirmed {
                Confirmed::Copy(_, h) => {
                    held.push(h);
                    continue;
                }
                Confirmed::Changed => Unconfirmed::Changed,
                Confirmed::Unknown => Unconfirmed::Unknown,
                Confirmed::Cancelled => Unconfirmed::Stopped,
            };
            for n in files {
                verdicts.insert(n, why);
            }
        }
        Verified {
            verdicts,
            _held: held,
        }
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
    fn selected_files_are_examined_and_a_stop_keeps_them() {
        let dir = std::env::temp_dir().join(format!("sb-retained-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let contents = sb_scan::native::NativeContents;
        let file = |name: &str| {
            let path = dir.join(name);
            std::fs::write(&path, vec![7; 4096]).unwrap();
            let print = contents.examine(&path, 4096).unwrap();
            (path, print)
        };
        let (kept, kept_print) = file("kept");
        let (mine, mine_print) = file("mine");
        let (edited, edited_print) = file("edited");
        let (gone, gone_print) = file("gone");
        std::fs::write(&edited, vec![8; 4096]).unwrap();
        let f = std::fs::File::options().write(true).open(&edited).unwrap();
        f.set_modified(SystemTime::UNIX_EPOCH).unwrap();
        drop(f);
        std::fs::remove_file(&gone).unwrap();
        let n = NodeId::from_index;
        let retained = Retained {
            groups: vec![Retaining {
                selected: vec![
                    (n(1), mine, mine_print),
                    (n(2), edited, edited_print),
                    (n(3), gone, gone_print),
                ],
                size: 4096,
                digest: [0; 32],
                staying: vec![(kept, kept_print)],
            }],
        };
        let stop = AtomicBool::new(true);
        let verified = retained.verify(&contents, &stop, &ConfirmProgress::default());
        assert_eq!(
            verified.verdicts,
            Verdicts::from([
                (n(1), Unconfirmed::Stopped),
                (n(2), Unconfirmed::NoLongerMatches),
            ]),
            "one gone already is left to the delete"
        );
        assert_eq!(
            Unconfirmed::NoLongerMatches.message("b.bin"),
            "Kept “b.bin”: it no longer matches its copies: it changed since they were compared."
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// One group of 200,000 identical small files found among 20,000 groups
    /// of two: how long a status (sent every 250 ms while reading) and a
    /// list take, and how much the list sends. Run by hand:
    /// `cargo test --release -p spacebadger duplicates_at_scale -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn duplicates_at_scale() {
        const HUGE: usize = 200_000;
        const PAIRS: usize = 20_000;
        let mut tree = Tree::new("/r");
        let sizes = |logical| FileSizes {
            logical,
            allocated: Allocated::known(logical.next_multiple_of(4096)),
        };
        let generated = tree.add_dir(NodeId::ROOT, "generated".as_ref()).unwrap();
        let mut folder = generated;
        let mut huge = Vec::with_capacity(HUGE);
        for i in 0..HUGE {
            if i % 1000 == 0 {
                let name = format!("batch-{:03}", i / 1000);
                folder = tree.add_dir(generated, name.as_ref()).unwrap();
            }
            let name = format!("part-{i:06}.tmp");
            huge.push(
                tree.add_file(folder, name.as_ref(), sizes(1500), None, 0)
                    .unwrap(),
            );
        }
        let photos = tree.add_dir(NodeId::ROOT, "photos".as_ref()).unwrap();
        let backup = tree.add_dir(NodeId::ROOT, "backup".as_ref()).unwrap();
        let mut pairs = Vec::with_capacity(PAIRS);
        for i in 0..PAIRS {
            let name = format!("IMG_{i:05}.jpg");
            let size = sizes(2_000_000 + i as u64);
            let a = tree.add_file(photos, name.as_ref(), size, None, 0).unwrap();
            let b = tree.add_file(backup, name.as_ref(), size, None, 0).unwrap();
            pairs.push((size.logical, [a, b]));
        }
        let found = |size, files: Vec<NodeId>| {
            Event::Found(Group {
                size,
                digest: [0; 32],
                prints: (0..files.len())
                    .map(|i| print(i as u128, Some(1)))
                    .collect(),
                files,
            })
        };
        let scope = Scope {
            generation: 1,
            folder: 0,
            path: "/r".into(),
            query: None,
            min_size: 1,
        };
        let job = Job::new(1, scope, Plan::default());
        for (size, files) in pairs {
            job.record(found(size, files.to_vec()));
        }
        job.record(found(1500, huge));

        let time = |what: &str, runs: u32, f: &dyn Fn() -> usize| {
            let start = Instant::now();
            let mut out = 0;
            for _ in 0..runs {
                out = f();
            }
            let each = start.elapsed() / runs;
            println!("{what}: {each:.1?} each, {out}");
        };
        time("status", 5, &|| job.status(&tree, 0).groups as usize);
        time("list", 3, &|| {
            let list = job.list(&tree, 0);
            let files: usize = list.groups.iter().map(|g| g.files.len()).sum();
            let json = serde_json::to_vec(&list).unwrap().len();
            println!(
                "  list: {} groups, {files} files, {json} bytes as JSON",
                list.groups.len()
            );
            files
        });
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
