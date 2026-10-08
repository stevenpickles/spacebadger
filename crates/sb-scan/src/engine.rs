//! Scan engine: a bounded worker pool lists directories while a single
//! aggregator thread owns all writes to the [`Tree`].
//!
//! - Workers only call the adapter; they never touch the tree.
//! - Results flow through a bounded channel, so slow aggregation applies
//!   backpressure to listing instead of buffering without limit.
//! - Progress is coalesced and published at most once per interval.
//! - Cancellation is a flag checked by workers before each job and by the
//!   aggregator at least every [`CANCEL_POLL`]; the aggregator stops without
//!   waiting for OS calls already in flight, so a blocked network listing
//!   cannot delay it.

use crate::adapter::{
    Entry, EntryKind, ErrorKind, FsAdapter, Identity, Measured, RootKind, ScanError,
};
use sb_core::size::Allocated;
use sb_core::tree::{DirState, FileSizes, NodeId, Tree, flags};
use std::collections::hash_map::Entry as MapEntry;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const CANCEL_POLL: Duration = Duration::from_millis(25);

static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
pub struct ScanConfig {
    /// Concurrent directory listings. Keep small for network roots.
    pub workers: usize,
    /// Minimum time between progress publications while scanning.
    pub progress_interval: Duration,
    /// Example paths kept per omission reason.
    pub sample_limit: usize,
}

impl Default for ScanConfig {
    fn default() -> Self {
        let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
        Self {
            workers: cores.clamp(2, 8),
            progress_interval: Duration::from_millis(150),
            sample_limit: 50,
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

/// Omission counts by reason with a bounded sample of paths per reason.
#[derive(Debug, Clone, Default)]
pub struct Omissions {
    counts: BTreeMap<OmissionReason, u64>,
    samples: BTreeMap<OmissionReason, Vec<PathBuf>>,
}

impl Omissions {
    fn add(&mut self, reason: OmissionReason, limit: usize, path: impl FnOnce() -> PathBuf) {
        *self.counts.entry(reason).or_default() += 1;
        let samples = self.samples.entry(reason).or_default();
        if samples.len() < limit {
            samples.push(path());
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

    pub fn samples(&self, reason: OmissionReason) -> &[PathBuf] {
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
        }
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

enum Job {
    List { dir: NodeId, path: PathBuf },
    Measure { file: NodeId, path: PathBuf },
}

enum Outcome {
    Listed {
        dir: NodeId,
        path: PathBuf,
        result: Result<Vec<Entry>, ScanError>,
    },
    Measured {
        file: NodeId,
        result: Result<Measured, ScanError>,
    },
    /// The worker saw the cancel flag and didn't run the job.
    Skipped,
}

#[derive(Default)]
struct Queue {
    state: Mutex<(VecDeque<Job>, bool)>,
    ready: Condvar,
}

impl Queue {
    fn push(&self, job: Job) {
        self.state.lock().unwrap().0.push_back(job);
        self.ready.notify_one();
    }

    /// Next job, or `None` once closed.
    fn pop(&self) -> Option<Job> {
        let mut state = self.state.lock().unwrap();
        loop {
            if state.1 {
                return None;
            }
            if let Some(job) = state.0.pop_front() {
                return Some(job);
            }
            state = self.ready.wait(state).unwrap();
        }
    }

    fn close(&self) -> Vec<Job> {
        let mut state = self.state.lock().unwrap();
        state.1 = true;
        self.ready.notify_all();
        state.0.drain(..).collect()
    }
}

struct Aggregator<A> {
    root: PathBuf,
    adapter: Arc<A>,
    config: ScanConfig,
    tree: Arc<RwLock<Tree>>,
    shared: Arc<Shared>,
    on_progress: Box<dyn Fn(&Progress) + Send>,
    started: Instant,
    queue: Arc<Queue>,
    outstanding: u64,
    pending_dirs: u64,
    owners: HashMap<u128, NodeId>,
    remeasured: HashSet<NodeId>,
    omissions: Omissions,
    revision: u64,
    published_revision: u64,
    last_publish: Instant,
    stop: Option<ScanState>,
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
        Self {
            root,
            adapter,
            config,
            tree,
            shared,
            on_progress,
            started: now,
            queue: Arc::new(Queue::default()),
            outstanding: 0,
            pending_dirs: 0,
            owners: HashMap::new(),
            remeasured: HashSet::new(),
            omissions: Omissions::default(),
            revision: 0,
            published_revision: 0,
            last_publish: now,
            stop: None,
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

        self.enqueue_list(NodeId::ROOT, self.root.clone());
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
            if self.outstanding == 0 {
                self.queue.close();
                let state = if self.omissions.total() == 0 {
                    ScanState::Complete
                } else {
                    ScanState::CompleteWithOmissions
                };
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

    fn spawn_worker(&self, index: usize, tx: SyncSender<Outcome>) {
        let queue = Arc::clone(&self.queue);
        let adapter = Arc::clone(&self.adapter);
        let shared = Arc::clone(&self.shared);
        std::thread::Builder::new()
            .name(format!("sb-scan-worker-{index}"))
            .spawn(move || {
                while let Some(job) = queue.pop() {
                    let outcome = if shared.cancel.load(Ordering::Relaxed) {
                        Outcome::Skipped
                    } else {
                        match job {
                            Job::List { dir, path } => {
                                let result = adapter.read_dir(&path);
                                Outcome::Listed { dir, path, result }
                            }
                            Job::Measure { file, path } => Outcome::Measured {
                                file,
                                result: adapter.measure_file(&path),
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

    fn enqueue_list(&mut self, dir: NodeId, path: PathBuf) {
        self.outstanding += 1;
        self.pending_dirs += 1;
        self.queue.push(Job::List { dir, path });
    }

    fn enqueue_measure(&mut self, file: NodeId, path: PathBuf) {
        self.outstanding += 1;
        self.queue.push(Job::Measure { file, path });
    }

    fn apply(&mut self, outcome: Outcome) {
        self.outstanding -= 1;
        match outcome {
            Outcome::Listed { dir, path, result } => {
                self.pending_dirs -= 1;
                match result {
                    Ok(entries) => self.apply_listing(dir, &path, entries),
                    Err(err) => {
                        let reason = OmissionReason::from_error(err.kind);
                        self.omissions
                            .add(reason, self.config.sample_limit, || path.clone());
                        self.tree
                            .write()
                            .unwrap()
                            .set_dir_state(dir, DirState::Failed);
                        if dir == NodeId::ROOT {
                            self.stop = Some(ScanState::Failed(format!(
                                "The selected folder can't be listed: {}",
                                err.message
                            )));
                        }
                    }
                }
                self.revision += 1;
            }
            Outcome::Measured { file, result } => {
                // A failed re-measure keeps the listing's values.
                if let Ok(m) = result {
                    let sizes = FileSizes {
                        logical: m.logical,
                        allocated: m.allocated.map_or(Allocated::UNKNOWN, Allocated::known),
                    };
                    self.tree.write().unwrap().update_file(file, sizes);
                    self.revision += 1;
                }
            }
            Outcome::Skipped => {}
        }
    }

    fn apply_listing(&mut self, dir: NodeId, path: &Path, entries: Vec<Entry>) {
        let limit = self.config.sample_limit;
        let mut subdirs = Vec::new();
        let mut remeasure = Vec::new();
        let tree_lock = Arc::clone(&self.tree);
        let mut tree = tree_lock.write().unwrap();
        for entry in entries {
            let skipped = match entry.kind {
                EntryKind::File | EntryKind::Directory => None,
                EntryKind::Symlink => Some(OmissionReason::Symlink),
                EntryKind::MountPoint => Some(OmissionReason::MountPoint),
                EntryKind::Special => Some(OmissionReason::SpecialFile),
                EntryKind::UnknownReparse => Some(OmissionReason::UnknownReparse),
                EntryKind::Failed(kind) => Some(OmissionReason::from_error(kind)),
            };
            if let Some(reason) = skipped {
                self.omissions.add(reason, limit, || path.join(&entry.name));
                continue;
            }
            let added = if entry.kind == EntryKind::Directory {
                tree.add_dir(dir, &entry.name).map(|id| {
                    subdirs.push((id, path.join(&entry.name)));
                })
            } else {
                let sizes = FileSizes {
                    logical: entry.logical,
                    allocated: entry.allocated.map_or(Allocated::UNKNOWN, Allocated::known),
                };
                let extra = if entry.cloud { flags::CLOUD } else { 0 };
                tree.add_file(dir, &entry.name, sizes, extra).map(|id| {
                    if let Identity::MaybeShared { key } = entry.identity {
                        match self.owners.entry(key) {
                            MapEntry::Vacant(v) => {
                                v.insert(id);
                            }
                            MapEntry::Occupied(o) => {
                                let owner = *o.get();
                                tree.mark_alias(id, owner);
                                // Listings of other names can be stale (NTFS),
                                // so re-measure both sides once.
                                remeasure.push(id);
                                if self.remeasured.insert(owner) {
                                    remeasure.push(owner);
                                }
                            }
                        }
                    }
                })
            };
            if added.is_err() {
                self.stop = Some(ScanState::Failed(
                    "The scan stopped because the folder tree exceeded SpaceBadger's capacity. \
                     Results so far are kept."
                        .into(),
                ));
                break;
            }
        }
        tree.set_dir_state(dir, DirState::Listed);
        let remeasure: Vec<_> = remeasure.into_iter().map(|n| (n, tree.path(n))).collect();
        drop(tree);
        if self.stop.is_some() {
            return;
        }
        for (id, path) in subdirs {
            self.enqueue_list(id, path);
        }
        for (id, path) in remeasure {
            self.enqueue_measure(id, path);
        }
    }

    /// Stops issuing work and marks every unlisted directory as not scanned.
    fn abandon_remaining(&mut self) {
        self.queue.close();
        let mut tree = self.tree.write().unwrap();
        for i in 0..tree.len() as u32 {
            let node = NodeId::from_index(i);
            if tree.is_dir(node) && tree.dir_state(node) == DirState::Pending {
                tree.set_dir_state(node, DirState::NotScanned);
            }
        }
    }

    fn finish(&mut self, state: ScanState) {
        self.revision += 1;
        self.publish(state);
    }

    fn publish(&mut self, state: ScanState) {
        let progress = {
            let tree = self.tree.read().unwrap();
            let root = NodeId::ROOT;
            Progress {
                generation: self.shared.progress.lock().unwrap().generation,
                pending_dirs: if state.is_finished() {
                    0
                } else {
                    self.pending_dirs
                },
                state,
                elapsed: self.started.elapsed(),
                files: u64::from(tree.file_count(root)),
                dirs: tree.dir_count() - 1,
                logical: tree.logical(root),
                allocated: tree.allocated(root),
                unknown_allocation_files: u64::from(tree.unknown_allocation_files(root)),
                hardlink_aliases: tree.alias_count() as u64,
                revision: self.revision,
                omissions: self.omissions.clone(),
            }
        };
        *self.shared.progress.lock().unwrap() = progress.clone();
        self.published_revision = self.revision;
        self.last_publish = Instant::now();
        (self.on_progress)(&progress);
    }
}
