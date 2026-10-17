//! Scan engine: a bounded worker pool lists directories while a single
//! aggregator thread owns all writes to the [`Tree`].
//!
//! - Workers only call the adapter; they never touch the tree.
//! - Listings arrive in batches of at most
//!   [`BATCH_ENTRIES`](crate::adapter::BATCH_ENTRIES) entries through a
//!   bounded channel, so slow aggregation applies backpressure to listing
//!   instead of buffering without limit, and memory stays bounded however
//!   wide a directory is. The aggregator takes the tree's write lock once
//!   per batch, so readers wait at most one batch.
//! - A directory becomes [`DirState::Listed`] only after its last batch; one
//!   whose listing fails part way keeps the entries already added and is
//!   marked as having unreadable entries.
//! - Progress is coalesced and published at most once per interval.
//! - Cancellation is a flag checked by workers before each job and between
//!   batches, and by the aggregator between batches and at least every
//!   [`CANCEL_POLL`]; the aggregator stops without waiting for OS calls
//!   already in flight, so a blocked network listing cannot delay it.
//! - Git detection (see [`crate::git`]) adds jobs, not listings: the
//!   aggregator spots candidates by name in the batches it applies, a worker
//!   confirms each with a few small reads, and the aggregator writes what it
//!   found into the tree as a new revision. Those jobs count as outstanding
//!   work, so a scan completes only once they're done, or once they've gone
//!   [`ScanConfig::git_grace`] without an answer after the last listing; a
//!   cancel leaves the unconfirmed candidates as not checked.
//! - A file seen under a second name is counted once, as an alias of its
//!   first name. Names whose listings may be stale are re-measured: by
//!   default only a file whose names' listings disagree, once, after every
//!   folder is listed (the scan is still running then); see [`links`]. A
//!   scan that stops before then (cancelled, failed, or out of tree
//!   capacity) keeps the listed sizes of such names, as it keeps
//!   everything else it found. Once complete, each file's allocation goes
//!   to a name chosen by path, not by listing order.
//! - Folders are listed in the order found, except while the tree has a
//!   prior scan to estimate from: then the folders whose estimates are
//!   least to be trusted, and then the biggest, go first, so the map's
//!   estimated areas become exact soonest (see [`listing_priority`]).

use crate::adapter::{
    BATCH_ENTRIES, Entry, EntryKind, ErrorKind, FsAdapter, Identity, Measured, RootKind, ScanError,
};
use crate::git::{self, Finding, Plan, Scope, Signals, Spot};
use links::{FileKey, IdHash, Links, Owner};
use sb_core::git::{GitRoot, Problem, RootKind as GitKind};
use sb_core::size::Allocated;
use sb_core::tree::{DirState, FileSizes, NodeId, Tree, fingerprint, flags};
use std::collections::hash_map::Entry as MapEntry;
use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::ops::{ControlFlow, Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{Arc, Condvar, Mutex, RwLock, RwLockWriteGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod links;

const CANCEL_POLL: Duration = Duration::from_millis(25);

/// The share of the root, in millionths, an unchanged folder must have held
/// in the prior scan for its listing to go ahead of the folders found
/// before it (see [`listing_priority`]): 0.01 %, an area of about 130 px²
/// on a full-screen map. Smaller folders hardly move the map, and keeping
/// them in order of discovery keeps the queue's ordered part small.
pub const BIG_SHARE: u32 = 100;

/// Added to the priority of a folder the prior scan can't vouch for: more
/// than any share (at most a million).
const CHANGED: u32 = 2_000_000;

/// The priority of a `.git` folder's listing: in the lane served first (see
/// [`Queue`]), since a Git probe waits for it.
const GIT_DIR: u32 = u32::MAX;

/// Where folder `dir`, just found with modified time `modified`, is listed
/// while the tree has a prior scan that finished at `since` (milliseconds
/// since the Unix epoch). Measured on rescans, the map's error comes from
/// what the prior doesn't know: folders new since (no estimate) and folders
/// whose entries changed since (a later modified time: entries added,
/// removed or renamed in them). So those go first, biggest first, then the
/// unchanged folders that held at least [`BIG_SHARE`] of the root, biggest
/// first, then the rest in order found (0). Changes inside a file don't
/// touch its folder's time, so a file that grew in place is found in turn.
///
/// Modified times only order listings, never change what's found, so
/// inexact ones only cost time: the snapshot's time is when its scan
/// finished, so a folder changed during that scan after it was listed looks
/// unchanged; network shares keep the server's clock; and FAT and exFAT
/// keep local time, so after a change of time zone or daylight saving
/// time, folders on them can look changed or unchanged by up to the shift.
fn listing_priority(tree: &Tree, dir: NodeId, modified: Option<SystemTime>, since: u64) -> u32 {
    let newer = modified
        .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
        .is_some_and(|d| d.as_millis() > u128::from(since));
    match tree.listing_priority(dir) {
        Some(share) if !newer => share,
        // New since, or changed since.
        share => CHANGED.saturating_add(share.unwrap_or(0)),
    }
}

/// How long Git probes may go without an answer once every folder is
/// listed before the scan finishes without them (see
/// [`ScanConfig::git_grace`]).
pub const GIT_GRACE: Duration = Duration::from_secs(5);

static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
pub struct ScanConfig {
    /// Concurrent directory listings. Keep small for network roots.
    pub workers: usize,
    /// Minimum time between progress publications while scanning.
    pub progress_interval: Duration,
    /// Example paths kept per omission reason.
    pub sample_limit: usize,
    /// Told how long each of the scanner's holds of the tree's write lock
    /// lasted, for timings; `None` costs nothing.
    pub write_holds: Option<HoldProbe>,
    /// Once every folder is listed, how long Git probes may go without an
    /// answer (a read stuck on a share that stopped responding) before the
    /// scan finishes without them, leaving their candidates timed out.
    pub git_grace: Duration,
    /// Re-measures every name of a file with several names as it's found,
    /// even when the names' listings agree. Off by default: listings that
    /// agree can only both be stale when the file was last changed through
    /// a name outside the scan (or in a folder that couldn't be listed).
    pub exact_hard_links: bool,
    /// While the tree has a prior scan, lists first the folders it can't
    /// vouch for, then the ones that were big in it (see
    /// [`listing_priority`]). On by default; off lists every folder in the
    /// order found, as without a prior. Only the order changes.
    pub list_by_prior: bool,
}

/// Called after the scanner releases the tree's write lock with how long it
/// held it and how many nodes the tree had then. Runs on the scan thread
/// with no lock held, so it should be quick.
#[derive(Clone)]
pub struct HoldProbe(Arc<dyn Fn(Duration, usize) + Send + Sync>);

impl HoldProbe {
    pub fn new(probe: impl Fn(Duration, usize) + Send + Sync + 'static) -> Self {
        Self(Arc::new(probe))
    }
}

impl std::fmt::Debug for HoldProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HoldProbe")
    }
}

impl Default for ScanConfig {
    fn default() -> Self {
        let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
        Self {
            workers: cores.clamp(2, 8),
            progress_interval: Duration::from_millis(150),
            sample_limit: 50,
            write_holds: None,
            git_grace: GIT_GRACE,
            exact_hard_links: false,
            list_by_prior: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanState {
    Scanning,
    Complete,
    /// Finished, but some entries were skipped or couldn't be measured.
    CompleteWithOmissions,
    /// Stopped by the user; discovered data is retained.
    Cancelled,
    /// Couldn't start or couldn't continue; any discovered data is retained.
    Failed(String),
}

impl ScanState {
    pub fn is_finished(&self) -> bool {
        !matches!(self, Self::Scanning)
    }
}

/// Why something was left out of the totals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum OmissionReason {
    // Skipped by policy.
    Symlink,
    MountPoint,
    SpecialFile,
    UnknownReparse,
    // Errors.
    PermissionDenied,
    Vanished,
    Disconnected,
    Unavailable,
    Other,
}

impl OmissionReason {
    pub fn is_policy(self) -> bool {
        matches!(
            self,
            Self::Symlink | Self::MountPoint | Self::SpecialFile | Self::UnknownReparse
        )
    }

    fn from_error(kind: ErrorKind) -> Self {
        match kind {
            ErrorKind::PermissionDenied => Self::PermissionDenied,
            ErrorKind::Vanished => Self::Vanished,
            ErrorKind::Disconnected => Self::Disconnected,
            ErrorKind::Unavailable => Self::Unavailable,
            ErrorKind::Other => Self::Other,
        }
    }
}

/// One example of an omission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OmissionSample {
    pub path: PathBuf,
    /// The OS error message, when the omission came from an error.
    pub detail: Option<String>,
}

/// Omission counts by reason with a bounded sample of paths per reason.
#[derive(Debug, Clone, Default)]
pub struct Omissions {
    counts: BTreeMap<OmissionReason, u64>,
    samples: BTreeMap<OmissionReason, Vec<OmissionSample>>,
}

impl Omissions {
    fn add(
        &mut self,
        reason: OmissionReason,
        limit: usize,
        sample: impl FnOnce() -> OmissionSample,
    ) {
        *self.counts.entry(reason).or_default() += 1;
        let samples = self.samples.entry(reason).or_default();
        if samples.len() < limit {
            samples.push(sample());
        }
    }

    pub fn count(&self, reason: OmissionReason) -> u64 {
        self.counts.get(&reason).copied().unwrap_or(0)
    }

    pub fn total(&self) -> u64 {
        self.counts.values().sum()
    }

    pub fn errors(&self) -> u64 {
        self.counts
            .iter()
            .filter(|(r, _)| !r.is_policy())
            .map(|(_, c)| c)
            .sum()
    }

    pub fn samples(&self, reason: OmissionReason) -> &[OmissionSample] {
        self.samples.get(&reason).map_or(&[], Vec::as_slice)
    }

    pub fn iter(&self) -> impl Iterator<Item = (OmissionReason, u64)> + '_ {
        self.counts.iter().map(|(r, c)| (*r, *c))
    }
}

/// A coalesced view of scan activity. No percent-complete: total work is
/// unknown until the scan ends.
#[derive(Debug, Clone)]
pub struct Progress {
    pub generation: u64,
    pub state: ScanState,
    pub elapsed: Duration,
    pub files: u64,
    /// Directories discovered below the root.
    pub dirs: u64,
    pub logical: u64,
    pub allocated: u64,
    pub unknown_allocation_files: u64,
    pub hardlink_aliases: u64,
    /// Directories discovered but not yet listed.
    pub pending_dirs: u64,
    /// Increments whenever the tree changes; consumers can skip redraws
    /// when it hasn't moved.
    pub revision: u64,
    pub omissions: Omissions,
    /// Git repositories, worktrees and unresolved candidates found so far
    /// (see `sb_core::git::GitIndex::listed_count`).
    pub git_roots: u32,
}

impl Progress {
    fn new(generation: u64) -> Self {
        Self {
            generation,
            state: ScanState::Scanning,
            elapsed: Duration::ZERO,
            files: 0,
            dirs: 0,
            logical: 0,
            allocated: 0,
            unknown_allocation_files: 0,
            hardlink_aliases: 0,
            pending_dirs: 1,
            revision: 0,
            omissions: Omissions::default(),
            git_roots: 0,
        }
    }

    /// Copies the tree's totals.
    fn count(&mut self, tree: &Tree) {
        let root = NodeId::ROOT;
        self.files = u64::from(tree.file_count(root));
        self.dirs = tree.dir_count() - 1;
        self.logical = tree.logical(root);
        self.allocated = tree.allocated(root);
        self.unknown_allocation_files = u64::from(tree.unknown_allocation_files(root));
        self.hardlink_aliases = tree.alias_count() as u64;
        self.git_roots = tree.git().listed_count();
    }
}

struct Shared {
    cancel: AtomicBool,
    progress: Mutex<Progress>,
}

/// A running or finished scan of one root. Dropping it cancels the scan.
pub struct Scan {
    generation: u64,
    tree: Arc<RwLock<Tree>>,
    shared: Arc<Shared>,
    aggregator: Option<JoinHandle<()>>,
}

impl Scan {
    /// Starts scanning `root` in the background. Returns immediately; root
    /// validation happens on the scan thread and is reported via progress.
    pub fn start<A: FsAdapter>(
        root: PathBuf,
        adapter: A,
        config: ScanConfig,
        on_progress: impl Fn(&Progress) + Send + 'static,
    ) -> Self {
        let generation = NEXT_GENERATION.fetch_add(1, Ordering::Relaxed);
        let tree = Arc::new(RwLock::new(Tree::new(root.clone())));
        let shared = Arc::new(Shared {
            cancel: AtomicBool::new(false),
            progress: Mutex::new(Progress::new(generation)),
        });
        let aggregator = {
            let tree = Arc::clone(&tree);
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("sb-scan-aggregator".into())
                .spawn(move || {
                    Aggregator::new(
                        root,
                        Arc::new(adapter),
                        config,
                        tree,
                        shared,
                        Box::new(on_progress),
                    )
                    .run()
                })
                .expect("spawn scan aggregator")
        };
        Self {
            generation,
            tree,
            shared,
            aggregator: Some(aggregator),
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn tree(&self) -> Arc<RwLock<Tree>> {
        Arc::clone(&self.tree)
    }

    pub fn progress(&self) -> Progress {
        self.shared.progress.lock().unwrap().clone()
    }

    /// Applies `edit` to the tree of a finished scan, such as removing
    /// deleted items, and records it as a new revision. Returns `None`
    /// without calling `edit` while the scan is running, because the scanner
    /// may still be adding to the parts being changed.
    pub fn edit_finished<R>(&self, edit: impl FnOnce(&mut Tree) -> R) -> Option<(R, Progress)> {
        if !self.shared.progress.lock().unwrap().state.is_finished() {
            return None;
        }
        // Same lock order as the aggregator: tree, then progress.
        let mut tree = self.tree.write().unwrap();
        let result = edit(&mut tree);
        let mut progress = self.shared.progress.lock().unwrap();
        progress.revision += 1;
        progress.count(&tree);
        Some((result, progress.clone()))
    }

    /// Requests cancellation. Returns immediately; the scan reports
    /// [`ScanState::Cancelled`] within [`CANCEL_POLL`] even if an OS call is
    /// still blocked in a worker.
    pub fn cancel(&self) {
        self.shared.cancel.store(true, Ordering::Relaxed);
    }

    /// Blocks until the aggregator finishes and returns the final progress.
    /// Worker threads blocked in OS calls may outlive this; they exit when
    /// their call returns.
    pub fn wait(mut self) -> Progress {
        if let Some(handle) = self.aggregator.take() {
            let _ = handle.join();
        }
        self.progress()
    }
}

impl Drop for Scan {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// The tree's write lock as the scanner holds it, reporting the hold to
/// [`ScanConfig::write_holds`] once released.
struct Written<'a> {
    /// Always `Some` until dropped.
    guard: Option<RwLockWriteGuard<'a, Tree>>,
    probe: Option<(HoldProbe, Instant)>,
}

fn write<'a>(tree: &'a RwLock<Tree>, probe: Option<&HoldProbe>) -> Written<'a> {
    let guard = tree.write().unwrap();
    Written {
        guard: Some(guard),
        probe: probe.map(|p| (p.clone(), Instant::now())),
    }
}

impl Deref for Written<'_> {
    type Target = Tree;

    fn deref(&self) -> &Tree {
        self.guard.as_ref().expect("held until dropped")
    }
}

impl DerefMut for Written<'_> {
    fn deref_mut(&mut self) -> &mut Tree {
        self.guard.as_mut().expect("held until dropped")
    }
}

impl Drop for Written<'_> {
    fn drop(&mut self) {
        let Some(guard) = self.guard.take() else {
            return;
        };
        let nodes = guard.len();
        let held = self.probe.as_ref().map(|(_, since)| since.elapsed());
        drop(guard);
        if let (Some((probe, _)), Some(held)) = (&self.probe, held) {
            (probe.0)(held, nodes);
        }
    }
}

/// What a directory's finished listing says about Git, given what its
/// batches showed, the listing's error if any, and whether some of it was
/// read: a folder with a Git directory's layout is worth a probe (outside a
/// `.git` folder's own data, where only a linked worktree's admin folder is
/// noted), a folder named `.git` without it leaves its owner unresolved,
/// and otherwise a `.git` file in the folder is followed.
fn listed_git(
    tree: &mut Tree,
    dir: NodeId,
    path: &Path,
    candidate: Candidate,
    error: Option<ScanError>,
    partial: bool,
) -> Option<(PathBuf, Plan)> {
    let dotgit = path
        .file_name()
        .is_some_and(|n| git::name_eq(n.as_encoded_bytes(), b".git"));
    let signals = candidate.signals;
    // The scan root has no owner inside the scan; the ancestor probe finds
    // the repository it belongs to.
    let owner = if dotgit { tree.parent(dir) } else { None };
    if signals.qualifies() && (error.is_none() || partial) {
        if !dotgit && git::inside_dotgit(path) && !signals.is_worktree_admin() {
            return None;
        }
        // A Git directory is probed as one; a `.git` file in it is ignored.
        return Some((
            path.to_path_buf(),
            Plan::AdminDir {
                dir,
                dotgit,
                owner,
                signals,
            },
        ));
    }
    if dotgit {
        if let Some(owner) = owner {
            let problem = match error {
                Some(err) => Problem::Unreadable {
                    kind: err.kind as u8,
                    message: err.message,
                },
                None => Problem::NotGit,
            };
            tree.set_git(GitRoot::unresolved(owner, problem));
        }
        return None;
    }
    let (file, name) = candidate.pointer?;
    tree.set_git(GitRoot::pending(dir));
    Some((path.join(name), Plan::Pointer { file, dir }))
}

/// What a folder's listing has shown of Git so far.
#[derive(Debug, Default)]
struct Candidate {
    signals: Signals,
    /// A `.git` file in it, and its name as listed.
    pointer: Option<(NodeId, OsString)>,
}

/// How a directory's listing ended: its result, and whether any of its
/// entries were added.
struct ListingEnd {
    result: Result<(), ScanError>,
    partial: bool,
}

/// A worker's work; `Git` confirms a Git candidate (see [`git::probe`]).
enum Job {
    /// Lists folder `dir`, last modified at `modified` as its parent's
    /// listing said (kept to order the listing by, see [`Queue::reorder`]).
    List {
        dir: NodeId,
        path: PathBuf,
        modified: Option<SystemTime>,
    },
    Measure {
        file: NodeId,
        path: PathBuf,
    },
    Git {
        path: PathBuf,
        plan: Plan,
    },
}

enum Outcome {
    /// Some of a directory's entries; more batches or its end may follow.
    Batch {
        dir: NodeId,
        path: PathBuf,
        entries: Vec<Entry>,
    },
    /// A directory's listing ended, with its last batch (if any).
    Listed {
        dir: NodeId,
        path: PathBuf,
        entries: Vec<Entry>,
        result: Result<(), ScanError>,
    },
    Measured {
        file: NodeId,
        result: Result<Measured, ScanError>,
    },
    /// What a Git probe found.
    Git { finding: Finding },
    /// The worker saw the cancel flag and didn't run the job.
    Skipped,
    /// The worker saw the cancel flag part way through listing `dir`.
    Stopped { dir: NodeId },
}

/// Workers' jobs in three lanes:
///
/// - Git probes, listings of `.git` folders (which a probe of the folder
///   above waits for), and re-measures: few, small, and wanted soon;
/// - listings of folders a prior puts ahead (see [`listing_priority`]),
///   highest priority first;
/// - other listings, in the order queued.
///
/// While the ordered lane has listings, the first lane is served before
/// it; otherwise the first and last lanes are served together in the order
/// queued. Without a prior the ordered lane stays empty, so the queue is a
/// plain FIFO of every job.
#[derive(Default)]
struct Queue {
    state: Mutex<Jobs>,
    ready: Condvar,
}

#[derive(Default)]
struct Jobs {
    /// Git probes and re-measures, with when each was queued.
    soon: VecDeque<(u64, Job)>,
    big: BinaryHeap<Big>,
    /// Other listings, with when each was queued.
    fifo: VecDeque<(u64, Job)>,
    /// Jobs queued so far: the order they were queued in.
    queued: u64,
    closed: bool,
}

impl Jobs {
    fn seq(&mut self) -> u64 {
        self.queued += 1;
        self.queued
    }

    /// Queues `job`; see [`Queue`].
    fn put(&mut self, job: Job, priority: u32) {
        let seq = self.seq();
        match job {
            Job::Measure { .. } | Job::Git { .. } => self.soon.push_back((seq, job)),
            Job::List { .. } if priority == GIT_DIR => self.soon.push_back((seq, job)),
            Job::List { .. } if priority >= BIG_SHARE => self.big.push(Big { priority, seq, job }),
            Job::List { .. } => self.fifo.push_back((seq, job)),
        }
    }

    fn take(&mut self) -> Option<Job> {
        let soon_first = match (self.soon.front(), self.fifo.front()) {
            (None, _) => false,
            _ if !self.big.is_empty() => true,
            (Some(_), None) => true,
            (Some((s, _)), Some((f, _))) => s < f,
        };
        if soon_first {
            return self.soon.pop_front().map(|(_, job)| job);
        }
        if let Some(big) = self.big.pop() {
            return Some(big.job);
        }
        self.fifo.pop_front().map(|(_, job)| job)
    }
}

/// A listing ahead of the FIFO: ordered by priority, then by when queued.
struct Big {
    priority: u32,
    seq: u64,
    job: Job,
}

impl PartialEq for Big {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for Big {}

impl PartialOrd for Big {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Big {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.priority
            .cmp(&other.priority)
            .then(other.seq.cmp(&self.seq))
    }
}

impl Queue {
    /// Queues `job`: a listing with a priority of [`BIG_SHARE`] or more
    /// goes ahead of the others (see [`Queue`]).
    fn push(&self, job: Job, priority: u32) {
        self.state.lock().unwrap().put(job, priority);
        self.ready.notify_one();
    }

    /// Next job, or `None` once closed.
    fn pop(&self) -> Option<Job> {
        let mut jobs = self.state.lock().unwrap();
        loop {
            if jobs.closed {
                return None;
            }
            if let Some(job) = jobs.take() {
                return Some(job);
            }
            jobs = self.ready.wait(jobs).unwrap();
        }
    }

    fn close(&self) {
        let mut jobs = self.state.lock().unwrap();
        jobs.closed = true;
        self.ready.notify_all();
        jobs.soon.clear();
        jobs.big.clear();
        jobs.fifo.clear();
    }

    /// Moves the listings waiting in order found to which `priority` (of
    /// the folder and its modified time) puts ahead (see [`Queue::push`]),
    /// keeping their order among equals.
    fn reorder(&self, mut priority: impl FnMut(NodeId, Option<SystemTime>) -> u32) {
        let mut jobs = self.state.lock().unwrap();
        let waiting = std::mem::take(&mut jobs.fifo);
        for (seq, job) in waiting {
            let p = match &job {
                Job::List { dir, modified, .. } => priority(*dir, *modified),
                _ => 0,
            };
            if p >= BIG_SHARE {
                jobs.big.push(Big {
                    priority: p,
                    seq,
                    job,
                });
            } else {
                jobs.fifo.push_back((seq, job));
            }
        }
    }
}

struct Aggregator<A> {
    root: PathBuf,
    adapter: Arc<A>,
    /// What Git probes need to know of the scan.
    scope: Arc<Scope>,
    config: ScanConfig,
    tree: Arc<RwLock<Tree>>,
    shared: Arc<Shared>,
    on_progress: Box<dyn Fn(&Progress) + Send>,
    started: Instant,
    queue: Arc<Queue>,
    outstanding: u64,
    pending_dirs: u64,
    links: Links,
    /// Directories with batches applied whose listing hasn't ended yet.
    partial: HashSet<NodeId>,
    /// Git probes queued or running.
    git_jobs: u64,
    /// Since when only Git probes have been left, without an answer.
    git_quiet_since: Option<Instant>,
    /// Git's names seen so far in directories still being listed.
    git_signals: HashMap<NodeId, Candidate>,
    omissions: Omissions,
    revision: u64,
    published_revision: u64,
    last_publish: Instant,
    stop: Option<ScanState>,
    /// Whether the listings queued so far were ordered by a prior.
    ordered: bool,
}

impl<A: FsAdapter> Aggregator<A> {
    fn new(
        root: PathBuf,
        adapter: Arc<A>,
        config: ScanConfig,
        tree: Arc<RwLock<Tree>>,
        shared: Arc<Shared>,
        on_progress: Box<dyn Fn(&Progress) + Send>,
    ) -> Self {
        let now = Instant::now();
        let exact_hard_links = config.exact_hard_links;
        let scope = Arc::new(Scope {
            root: root.clone(),
            style: adapter.path_style(),
        });
        Self {
            root,
            adapter,
            scope,
            config,
            tree,
            shared,
            on_progress,
            started: now,
            queue: Arc::new(Queue::default()),
            outstanding: 0,
            pending_dirs: 0,
            links: Links::new(exact_hard_links),
            partial: HashSet::new(),
            git_jobs: 0,
            git_quiet_since: None,
            git_signals: HashMap::new(),
            omissions: Omissions::default(),
            revision: 0,
            published_revision: 0,
            last_publish: now,
            stop: None,
            ordered: false,
        }
    }

    fn cancelled(&self) -> bool {
        self.shared.cancel.load(Ordering::Relaxed)
    }

    fn run(mut self) {
        let failure = match self.adapter.open_root(&self.root) {
            Ok(RootKind::Directory) => None,
            Ok(RootKind::Link) => Some(
                "The selected folder is a symbolic link, junction, or mount redirection. \
                 Select the folder it points to instead."
                    .to_owned(),
            ),
            Ok(RootKind::NotDirectory) => Some("The selected path is not a folder.".to_owned()),
            Err(err) => Some(format!(
                "The selected folder can't be opened: {}",
                err.message
            )),
        };
        if let Some(message) = failure {
            self.pending_dirs = 0;
            self.finish(ScanState::Failed(message));
            return;
        }

        let (tx, rx) = sync_channel::<Outcome>(self.config.workers * 4);
        for i in 0..self.config.workers.max(1) {
            self.spawn_worker(i, tx.clone());
        }
        drop(tx);

        self.enqueue_list(NodeId::ROOT, self.root.clone(), None, 0);
        self.enqueue_git(self.root.clone(), Plan::Ancestors);
        loop {
            if self.cancelled() {
                self.stop.get_or_insert(ScanState::Cancelled);
            }
            if let Some(state) = self.stop.take() {
                self.abandon_remaining();
                self.finish(state);
                return;
            }
            match rx.recv_timeout(CANCEL_POLL) {
                Ok(outcome) => self.apply(outcome),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    self.stop = Some(ScanState::Failed(
                        "Scan workers stopped unexpectedly.".into(),
                    ));
                    continue;
                }
            }
            if self.stop.is_some() {
                continue;
            }
            // A cancel that arrived while the last outcome was applied may
            // have cut work short; the top of the loop reports it.
            if self.cancelled() {
                continue;
            }
            if self.pending_dirs == 0 && !self.links.refined() {
                self.refine();
            }
            if self.outstanding == 0 || self.git_stalled() {
                self.queue.close();
                if self.outstanding > 0 {
                    // Answers that come later are dropped with the channel.
                    self.unconfirmed(Problem::TimedOut);
                }
                let state = if self.omissions.total() == 0 {
                    ScanState::Complete
                } else {
                    ScanState::CompleteWithOmissions
                };
                self.settle_owners();
                self.finish(state);
                return;
            }
            // Publish the first change at once so a first view can render
            // early; coalesce later changes per interval.
            if self.revision != self.published_revision
                && (self.published_revision == 0
                    || self.last_publish.elapsed() >= self.config.progress_interval)
            {
                self.publish(ScanState::Scanning);
            }
        }
    }

    /// Whether only Git probes are left and none has answered for
    /// [`ScanConfig::git_grace`].
    fn git_stalled(&mut self) -> bool {
        if self.pending_dirs > 0 || self.outstanding != self.git_jobs {
            self.git_quiet_since = None;
            return false;
        }
        let since = *self.git_quiet_since.get_or_insert_with(Instant::now);
        since.elapsed() >= self.config.git_grace
    }

    fn spawn_worker(&self, index: usize, tx: SyncSender<Outcome>) {
        let queue = Arc::clone(&self.queue);
        let adapter = Arc::clone(&self.adapter);
        let shared = Arc::clone(&self.shared);
        let scope = Arc::clone(&self.scope);
        std::thread::Builder::new()
            .name(format!("sb-scan-worker-{index}"))
            .spawn(move || {
                while let Some(job) = queue.pop() {
                    let outcome = if shared.cancel.load(Ordering::Relaxed) {
                        Outcome::Skipped
                    } else {
                        match job {
                            Job::List { dir, path, .. } => {
                                // Each batch goes to the aggregator once the
                                // next is read, so the last one travels with
                                // the listing's end (most directories fit in
                                // one). The cancel flag ends the listing.
                                let mut held = None;
                                let mut stopped = false;
                                let result = adapter.read_dir(&path, &mut |entries| {
                                    if shared.cancel.load(Ordering::Relaxed) {
                                        stopped = true;
                                        return ControlFlow::Break(());
                                    }
                                    if let Some(entries) = held.replace(entries) {
                                        let batch = Outcome::Batch {
                                            dir,
                                            path: path.clone(),
                                            entries,
                                        };
                                        if tx.send(batch).is_err() {
                                            stopped = true;
                                            return ControlFlow::Break(());
                                        }
                                    }
                                    ControlFlow::Continue(())
                                });
                                if stopped {
                                    Outcome::Stopped { dir }
                                } else {
                                    Outcome::Listed {
                                        dir,
                                        path,
                                        entries: held.unwrap_or_default(),
                                        result,
                                    }
                                }
                            }
                            Job::Measure { file, path } => Outcome::Measured {
                                file,
                                result: adapter.measure_file(&path),
                            },
                            Job::Git { path, plan } => Outcome::Git {
                                finding: git::probe(&*adapter, &path, &plan, &scope),
                            },
                        }
                    };
                    if tx.send(outcome).is_err() {
                        break;
                    }
                }
            })
            .expect("spawn scan worker");
    }

    /// Queues `dir`'s listing, ahead of others by `priority` (see
    /// [`listing_priority`]).
    fn enqueue_list(
        &mut self,
        dir: NodeId,
        path: PathBuf,
        modified: Option<SystemTime>,
        priority: u32,
    ) {
        self.outstanding += 1;
        self.pending_dirs += 1;
        let job = Job::List {
            dir,
            path,
            modified,
        };
        self.queue.push(job, priority);
    }

    fn enqueue_measure(&mut self, file: NodeId, path: PathBuf) {
        self.outstanding += 1;
        self.queue.push(Job::Measure { file, path }, 0);
    }

    /// Once the scan is complete, gives each hard-linked file's allocation
    /// to the name a fixed order of paths prefers, whatever was listed
    /// first (see [`links`]): chosen under the read lock, then moved in
    /// holds of the write lock bounded like a batch's. Skipped for a
    /// stopped scan, and given up part way (totals stay exact, owners as
    /// found) when a cancel arrives meanwhile, so a cancel is acknowledged
    /// at once; the scan still reports the state it completed with.
    fn settle_owners(&mut self) {
        let shared = Arc::clone(&self.shared);
        let stop = || shared.cancel.load(Ordering::Relaxed);
        let moves = self.links.settle(&self.tree.read().unwrap(), stop);
        for chunk in moves.chunks(BATCH_ENTRIES) {
            if stop() {
                break;
            }
            let mut tree = write(&self.tree, self.config.write_holds.as_ref());
            for (heir, others) in chunk {
                tree.promote_alias(*heir, others);
            }
        }
    }

    /// Once every folder is listed, queues the hard-link re-measures left
    /// for then (see [`links`]).
    fn refine(&mut self) {
        let jobs = self.links.refine(&self.tree.read().unwrap());
        for (file, path) in jobs {
            self.enqueue_measure(file, path);
        }
    }

    fn enqueue_git(&mut self, path: PathBuf, plan: Plan) {
        self.outstanding += 1;
        self.git_jobs += 1;
        self.queue.push(Job::Git { path, plan }, 0);
    }

    fn apply(&mut self, outcome: Outcome) {
        // A batch is part of a job, not its end.
        if !matches!(outcome, Outcome::Batch { .. }) {
            self.outstanding -= 1;
        }
        match outcome {
            Outcome::Batch { dir, path, entries } => {
                self.partial.insert(dir);
                self.apply_batch(dir, &path, entries, None);
                self.revision += 1;
            }
            Outcome::Listed {
                dir,
                path,
                entries,
                result,
            } => {
                self.pending_dirs -= 1;
                let partial = self.partial.remove(&dir) || !entries.is_empty();
                let end = ListingEnd { result, partial };
                // The last batch and the listing's end share one hold.
                let probe = if entries.is_empty() {
                    if self.stop.is_some() {
                        return;
                    }
                    let tree_lock = Arc::clone(&self.tree);
                    let mut tree = write(&tree_lock, self.config.write_holds.as_ref());
                    self.end_listing(&mut tree, dir, &path, end)
                } else {
                    self.apply_batch(dir, &path, entries, Some(end))
                };
                self.revision += 1;
                if let Some((path, plan)) = probe {
                    self.enqueue_git(path, plan);
                }
            }
            Outcome::Stopped { dir } => {
                // Left pending, so it ends up as not scanned. Work dropped
                // for a cancel means the scan is cancelled, even if this was
                // the last of it.
                self.pending_dirs -= 1;
                self.partial.remove(&dir);
                self.stop.get_or_insert(ScanState::Cancelled);
            }
            Outcome::Measured { file, result } => {
                // One measure applies to every name of a file re-measured
                // after the listings; a failed re-measure tries the file's
                // next name, or keeps the listings' values.
                let todo = self
                    .links
                    .measured(file, result.as_ref().ok().map(|m| m.id));
                if let (Ok(m), false) = (&result, todo.apply.is_empty()) {
                    let sizes = FileSizes {
                        logical: m.logical,
                        allocated: m.allocated.map_or(Allocated::UNKNOWN, Allocated::known),
                    };
                    let mut tree = write(&self.tree, self.config.write_holds.as_ref());
                    for name in todo.apply {
                        tree.update_file(name, sizes);
                    }
                    drop(tree);
                    self.revision += 1;
                }
                if self.stop.is_none() && !todo.remeasure.is_empty() {
                    let paths: Vec<PathBuf> = {
                        let tree = self.tree.read().unwrap();
                        todo.remeasure.iter().map(|&n| tree.path(n)).collect()
                    };
                    for (name, path) in todo.remeasure.into_iter().zip(paths) {
                        self.enqueue_measure(name, path);
                    }
                }
            }
            Outcome::Git { finding } => {
                self.git_jobs -= 1;
                self.git_quiet_since = None;
                if self.stop.is_some() || finding.is_empty() {
                    return;
                }
                let mut tree = write(&self.tree, self.config.write_holds.as_ref());
                for root in finding.roots {
                    tree.set_git(root);
                }
                if let Some((path, root)) = finding.above {
                    tree.set_git_above(path, root);
                }
                drop(tree);
                self.revision += 1;
            }
            Outcome::Skipped => {
                self.stop.get_or_insert(ScanState::Cancelled);
            }
        }
    }

    /// Adds one batch of `dir`'s entries under one hold of the write lock,
    /// ending the listing there too when it's the last batch, then queues
    /// the subdirectories and re-measures it found. Returns the Git probe
    /// the listing's end asks for.
    fn apply_batch(
        &mut self,
        dir: NodeId,
        path: &Path,
        entries: Vec<Entry>,
        end: Option<ListingEnd>,
    ) -> Option<(PathBuf, Plan)> {
        if self.stop.is_some() {
            return None;
        }
        let limit = self.config.sample_limit;
        let mut subdirs = Vec::new();
        let mut remeasure = Vec::new();
        // Hard-link owners are looked up before taking the lock and new
        // owners recorded after releasing it: the map holds an entry per file
        // on Windows, and growing it at millions of entries takes long enough
        // to stall readers. Names sharing a file within this batch are
        // matched under the lock through a map bounded by the batch.
        let known: Vec<Option<Owner>> = entries
            .iter()
            .map(|e| match e.identity {
                Identity::MaybeShared { key } => self.links.owner(key),
                _ => None,
            })
            .collect();
        let mut first_names: HashMap<FileKey, Owner, IdHash> = HashMap::default();
        let tree_lock = Arc::clone(&self.tree);
        let probe = self.config.write_holds.clone();
        let mut tree = write(&tree_lock, probe.as_ref());
        // Listings are ordered by the prior scan only if asked to and while
        // there is one.
        let since = if self.config.list_by_prior {
            tree.prior_created_ms()
        } else {
            None
        };
        // A prior handed over once the scan has begun orders the listings
        // already queued too.
        if let (Some(since), false) = (since, self.ordered) {
            self.ordered = true;
            self.queue
                .reorder(|dir, modified| listing_priority(&tree, dir, modified, since));
        }
        let mut unreadable = false;
        for (entry, known) in entries.into_iter().zip(known) {
            let spot = git::spot(entry.name.as_encoded_bytes(), entry.kind);
            let skipped = match entry.kind {
                EntryKind::File | EntryKind::Directory => None,
                EntryKind::Symlink => Some(OmissionReason::Symlink),
                EntryKind::MountPoint => Some(OmissionReason::MountPoint),
                EntryKind::Special => Some(OmissionReason::SpecialFile),
                EntryKind::UnknownReparse => Some(OmissionReason::UnknownReparse),
                EntryKind::Failed(kind) => Some(OmissionReason::from_error(kind)),
            };
            unreadable |= matches!(entry.kind, EntryKind::Failed(_));
            if let Some(reason) = skipped {
                self.omissions.add(reason, limit, || OmissionSample {
                    path: path.join(&entry.name),
                    detail: None,
                });
                if let Some(spot) = spot {
                    self.spotted(&mut tree, dir, path, spot, &entry, None);
                }
                continue;
            }
            let added = if entry.kind == EntryKind::Directory {
                tree.add_dir(dir, &entry.name).inspect(|&id| {
                    tree.set_id(id, fingerprint(entry.id));
                    let priority = if spot == Some(Spot::DotGit) {
                        GIT_DIR
                    } else {
                        since.map_or(0, |since| {
                            listing_priority(&tree, id, entry.modified, since)
                        })
                    };
                    subdirs.push((id, path.join(&entry.name), entry.modified, priority));
                })
            } else {
                let sizes = FileSizes {
                    logical: entry.logical,
                    allocated: entry.allocated.map_or(Allocated::UNKNOWN, Allocated::known),
                };
                let extra = if entry.cloud { flags::CLOUD } else { 0 };
                tree.add_file(dir, &entry.name, sizes, entry.modified, extra)
                    .inspect(|&id| {
                        tree.set_id(id, fingerprint(entry.id));
                        let Identity::MaybeShared { key } = entry.identity else {
                            return;
                        };
                        let owner = match known {
                            Some(owner) => owner,
                            None => match first_names.entry(FileKey::of(key)) {
                                MapEntry::Vacant(v) => {
                                    v.insert(Owner::new(id, entry.stamp));
                                    return;
                                }
                                MapEntry::Occupied(o) => *o.get(),
                            },
                        };
                        tree.mark_alias(id, owner.node);
                        self.links
                            .alias(&tree, id, &entry, key, owner, &mut remeasure);
                    })
            };
            let Ok(added) = added else {
                self.stop = Some(ScanState::Failed(
                    "The scan stopped because the folder tree exceeded SpaceBadger's capacity. \
                     Results so far are kept."
                        .into(),
                ));
                break;
            };
            if let Some(spot) = spot {
                self.spotted(&mut tree, dir, path, spot, &entry, Some(added));
            }
        }
        if unreadable {
            tree.mark_unreadable_entries(dir);
        }
        let probe = match end {
            Some(end) if self.stop.is_none() => self.end_listing(&mut tree, dir, path, end),
            _ => None,
        };
        drop(tree);
        self.links.add_owners(first_names);
        if self.stop.is_some() {
            return None;
        }
        for (id, path, modified, priority) in subdirs {
            self.enqueue_list(id, path, modified, priority);
        }
        for (id, path) in remeasure {
            self.enqueue_measure(id, path);
        }
        probe
    }

    /// Marks `dir` listed (or failed) once its last batch is in, under the
    /// caller's hold of the write lock, and says what its listing showed of
    /// Git.
    fn end_listing(
        &mut self,
        tree: &mut Tree,
        dir: NodeId,
        path: &Path,
        end: ListingEnd,
    ) -> Option<(PathBuf, Plan)> {
        let ListingEnd { result, partial } = end;
        let candidate = self.git_signals.remove(&dir).unwrap_or_default();
        let mut failed = None;
        match result {
            Ok(()) => tree.set_dir_state(dir, DirState::Listed),
            Err(err) => {
                let reason = OmissionReason::from_error(err.kind);
                self.omissions
                    .add(reason, self.config.sample_limit, || OmissionSample {
                        path: path.to_path_buf(),
                        detail: Some(err.message.clone()),
                    });
                if partial {
                    // Listed in part: the entries already added stay, and
                    // the folder counts as not fully read.
                    tree.set_dir_state(dir, DirState::Listed);
                    tree.mark_unreadable_entries(dir);
                } else {
                    tree.set_dir_state(dir, DirState::Failed);
                    if dir == NodeId::ROOT {
                        self.stop = Some(ScanState::Failed(format!(
                            "The selected folder can't be listed: {}",
                            err.message
                        )));
                    }
                }
                failed = Some(err);
            }
        }
        listed_git(tree, dir, path, candidate, failed, partial)
    }

    /// Stops issuing work, marks every unlisted directory as not scanned and
    /// every Git candidate not yet confirmed as not checked.
    fn abandon_remaining(&mut self) {
        self.queue.close();
        let mut tree = write(&self.tree, self.config.write_holds.as_ref());
        for i in 0..tree.len() as u32 {
            let node = NodeId::from_index(i);
            if tree.is_dir(node) && tree.dir_state(node) == DirState::Pending {
                tree.set_dir_state(node, DirState::NotScanned);
            }
        }
        drop(tree);
        self.unconfirmed(Problem::NotChecked);
    }

    /// Leaves every Git candidate not yet confirmed unresolved with
    /// `problem`.
    fn unconfirmed(&mut self, problem: Problem) {
        let mut tree = write(&self.tree, self.config.write_holds.as_ref());
        let pending: Vec<NodeId> = tree
            .git()
            .roots()
            .filter(|r| r.kind == GitKind::Pending)
            .map(|r| r.node)
            .collect();
        for node in pending {
            tree.set_git(GitRoot::unresolved(node, problem.clone()));
        }
    }

    /// Notes what an entry's name says about Git, under the batch's hold of
    /// the write lock: a `.git` folder makes its folder a candidate, a
    /// `.git` file is noted to be followed once the folder is listed (unless
    /// the folder is a Git directory itself), a `.git` link is never
    /// followed, and Git's other names are signals for the folder. A `.git`
    /// inside a `.git` folder is ignored: it's data, not a repository.
    fn spotted(
        &mut self,
        tree: &mut Tree,
        dir: NodeId,
        path: &Path,
        spot: Spot,
        entry: &Entry,
        added: Option<NodeId>,
    ) {
        if spot != Spot::DotGit {
            if added.is_some() {
                spot.add_to(&mut self.git_signals.entry(dir).or_default().signals);
            }
            return;
        }
        let in_git = path
            .file_name()
            .is_some_and(|n| git::name_eq(n.as_encoded_bytes(), b".git"))
            || git::inside_dotgit(path);
        if in_git {
            return;
        }
        let root = match (entry.kind, added) {
            (EntryKind::Directory, Some(_)) => GitRoot::pending(dir),
            (EntryKind::File, Some(file)) => {
                self.git_signals.entry(dir).or_default().pointer = Some((file, entry.name.clone()));
                return;
            }
            (EntryKind::Symlink | EntryKind::MountPoint | EntryKind::UnknownReparse, _) => {
                GitRoot::unresolved(dir, Problem::Link)
            }
            (EntryKind::Failed(kind), _) => GitRoot::unresolved(
                dir,
                Problem::Unreadable {
                    kind: kind as u8,
                    message: match kind {
                        ErrorKind::PermissionDenied => "access denied",
                        ErrorKind::Vanished => ".git disappeared",
                        ErrorKind::Disconnected => "the device stopped responding",
                        ErrorKind::Unavailable | ErrorKind::Other => ".git couldn't be examined",
                    }
                    .to_owned(),
                },
            ),
            _ => return,
        };
        tree.set_git(root);
    }

    fn finish(&mut self, state: ScanState) {
        self.revision += 1;
        self.publish(state);
    }

    fn publish(&mut self, state: ScanState) {
        let progress = {
            let tree = self.tree.read().unwrap();
            let mut progress = Progress::new(self.shared.progress.lock().unwrap().generation);
            progress.count(&tree);
            Progress {
                pending_dirs: if state.is_finished() {
                    0
                } else {
                    self.pending_dirs
                },
                state,
                elapsed: self.started.elapsed(),
                revision: self.revision,
                omissions: self.omissions.clone(),
                ..progress
            }
        };
        *self.shared.progress.lock().unwrap() = progress.clone();
        self.published_revision = self.revision;
        self.last_publish = Instant::now();
        (self.on_progress)(&progress);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(n: u32) -> Job {
        Job::List {
            dir: NodeId::from_index(n),
            path: PathBuf::from(n.to_string()),
            modified: None,
        }
    }

    fn name(job: Job) -> String {
        match job {
            Job::List { path, .. } | Job::Measure { path, .. } | Job::Git { path, .. } => {
                path.display().to_string()
            }
        }
    }

    #[test]
    fn big_listings_go_first_biggest_first_then_the_rest_in_order() {
        let q = Queue::default();
        q.push(list(1), 0);
        q.push(list(2), BIG_SHARE);
        q.push(list(3), BIG_SHARE - 1);
        q.push(
            Job::Git {
                path: "probe".into(),
                plan: Plan::Ancestors,
            },
            0,
        );
        q.push(list(4), 500_000);
        q.push(list(5), BIG_SHARE);
        q.push(
            Job::Measure {
                file: NodeId::from_index(6),
                path: "6".into(),
            },
            0,
        );
        q.push(list(7), 500_000);
        let order: Vec<String> = (0..8).map(|_| name(q.pop().unwrap())).collect();
        // The Git probe and the re-measure go ahead of the ordered
        // listings; those go highest first, ties to the one queued first;
        // then the rest in the order queued.
        assert_eq!(order, ["probe", "6", "4", "7", "2", "5", "1", "3"]);
    }

    #[test]
    fn folders_the_prior_cant_vouch_for_come_first_then_big_ones() {
        use sb_core::prior::Prior;
        use sb_core::size::Allocated;
        use sb_core::snapshot::Snapshot;
        let sizes = |n| FileSizes {
            logical: n,
            allocated: Allocated::known(n),
        };
        let mut before = Tree::new("/r");
        for (dir, size) in [("big", 999_000), ("small", 10), ("touched", 990)] {
            let d = before.add_dir(NodeId::ROOT, dir.as_ref()).unwrap();
            before
                .add_file(d, "f".as_ref(), sizes(size), None, 0)
                .unwrap();
            before.set_dir_state(d, DirState::Listed);
        }
        let empty = before.add_dir(NodeId::ROOT, "empty".as_ref()).unwrap();
        before.set_dir_state(empty, DirState::Listed);
        before.set_dir_state(NodeId::ROOT, DirState::Listed);
        let since = 5_000;
        let prior = Prior::new(Snapshot::capture(&before, since, true));

        let mut t = Tree::new("/r");
        t.set_prior(Arc::new(prior));
        let at = |ms| Some(UNIX_EPOCH + Duration::from_millis(ms));
        let mut order = |name: &str, modified| {
            let d = t.add_dir(NodeId::ROOT, name.as_ref()).unwrap();
            listing_priority(&t, d, modified, since)
        };
        // Shares of the million bytes the root held.
        assert_eq!(order("big", at(since)), 999_000, "unchanged since");
        assert_eq!(order("small", None), 10, "too small to go ahead");
        assert_eq!(order("touched", at(since + 1)), CHANGED + 990);
        assert_eq!(order("new", at(1)), CHANGED, "nothing to go by");
        assert_eq!(order("empty", at(since)), 0, "unchanged and empty");
    }

    #[test]
    fn without_priorities_the_queue_is_first_in_first_out() {
        let q = Queue::default();
        for n in 0..50 {
            if n % 7 == 3 {
                q.push(
                    Job::Git {
                        path: n.to_string().into(),
                        plan: Plan::Ancestors,
                    },
                    0,
                );
            } else if n % 7 == 5 {
                q.push(
                    Job::Measure {
                        file: NodeId::from_index(n),
                        path: n.to_string().into(),
                    },
                    0,
                );
            } else {
                q.push(list(n), 0);
            }
        }
        for n in 0..50 {
            assert_eq!(name(q.pop().unwrap()), n.to_string());
        }
    }

    #[test]
    fn a_git_probe_goes_ahead_of_many_ordered_listings() {
        let q = Queue::default();
        q.push(list(0), 0);
        for n in 1..1_000 {
            q.push(list(n), 500_000 + n);
        }
        q.push(
            Job::Git {
                path: "probe".into(),
                plan: Plan::Ancestors,
            },
            0,
        );
        q.push(list(1_000), GIT_DIR);
        assert_eq!(name(q.pop().unwrap()), "probe");
        assert_eq!(name(q.pop().unwrap()), "1000", "a .git folder's listing");
        assert_eq!(name(q.pop().unwrap()), "999");
        // Once the ordered listings are done, the rest go in the order
        // queued again.
        for _ in 1..999 {
            q.pop().unwrap();
        }
        q.push(
            Job::Git {
                path: "later".into(),
                plan: Plan::Ancestors,
            },
            0,
        );
        assert_eq!(name(q.pop().unwrap()), "0");
        assert_eq!(name(q.pop().unwrap()), "later");
    }

    #[test]
    fn a_closed_queue_hands_out_nothing_more() {
        let q = Queue::default();
        q.push(list(1), 0);
        q.push(list(2), 1_000);
        q.close();
        assert!(q.pop().is_none());
        // Pushing after closing is harmless.
        q.push(list(3), 1_000);
        assert!(q.pop().is_none());
        // A worker waiting for work wakes up and stops.
        let idle = Arc::new(Queue::default());
        let waiter = {
            let idle = Arc::clone(&idle);
            std::thread::spawn(move || idle.pop().is_none())
        };
        std::thread::sleep(Duration::from_millis(20));
        idle.close();
        assert!(waiter.join().unwrap());
    }
}
