//! Finding duplicate files by their contents, on request.
//!
//! This is the one place SpaceBadger reads what's inside files, and only
//! when the user asks. Candidates come from the scan tree: files of at
//! least a minimum size that share their size with another candidate.
//! A hard-linked file counts once, since deleting one of its names frees
//! nothing, and cloud files are left out so nothing is downloaded.
//!
//! Each size group is split by a hash of the first and last 64 KiB of its
//! files (the whole file when that's all there is), and what's left is
//! confirmed by a hash of the whole content. Hashes are BLAKE3, so equal
//! hashes mean equal contents in practice. Groups with the most space at
//! stake are read first, by a small pool of readers. Opening a file is
//! slow (virus scanners look at each one first), so several are opened at
//! once, but only a couple of large files are read whole at a time:
//! spinning disks and network shares read fastest a stream or two at a time.

use sb_core::layout::{self, Metric};
use sb_core::search::Search;
use sb_core::tree::{NodeId, Tree, flags};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// Bytes hashed from each end of a file to tell same-size files apart.
pub const SAMPLE: u64 = 64 * 1024;

/// Bytes read per call when hashing whole files.
const BUFFER: usize = 1 << 20;

/// Files open at once.
pub const READERS: usize = 6;

/// Whole files larger than this are read at most [`STREAMS`] at a time.
const LARGE: u64 = 8 << 20;

/// Large files read whole at once. More only helps fast SSDs, and costs
/// spinning disks and network shares a lot.
const STREAMS: usize = 2;

/// Why a candidate file wasn't read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Skip {
    /// A cloud file, which reading could download.
    Cloud,
    /// Gone since the scan.
    Vanished,
    /// Changed since the scan: another size, or no longer a plain file.
    Changed,
    /// Stored behind a reparse point (deduplicated, tiered, or
    /// virtualized), so reading it raw wouldn't give its contents.
    Special,
    /// Another program has it open and won't share it.
    InUse,
    /// No permission to read it.
    Denied,
    /// Reading failed for another reason.
    Unreadable,
}

impl Skip {
    /// The reason for a failed open or read.
    pub fn of(err: &io::Error) -> Self {
        // ERROR_SHARING_VIOLATION and ERROR_LOCK_VIOLATION.
        if cfg!(windows) && matches!(err.raw_os_error(), Some(32 | 33)) {
            return Self::InUse;
        }
        match err.kind() {
            io::ErrorKind::NotFound => Self::Vanished,
            io::ErrorKind::PermissionDenied => Self::Denied,
            io::ErrorKind::ResourceBusy | io::ErrorKind::ExecutableFileBusy => Self::InUse,
            _ => Self::Unreadable,
        }
    }
}

/// Opens files to read their contents. The native implementation never
/// follows links and never recalls cloud content; tests use a fake.
pub trait Contents: Sync {
    type File: Read + Seek;

    /// Opens the file at `path`, which the scan measured at `len` bytes.
    /// Refuses one that is no longer a plain file of that length.
    fn open(&self, path: &Path, len: u64) -> Result<Self::File, Skip>;
}

#[derive(Debug, Clone)]
pub struct Candidate {
    pub node: NodeId,
    pub path: PathBuf,
}

/// Candidates of one size.
#[derive(Debug, Clone)]
pub struct SizeGroup {
    pub size: u64,
    pub files: Vec<Candidate>,
}

/// What [`run`] will read.
#[derive(Debug, Default)]
pub struct Plan {
    /// Groups of two or more same-size files, most space at stake first.
    pub groups: Vec<SizeGroup>,
    /// Cloud files left out that had the size of another candidate.
    pub cloud: Vec<NodeId>,
    /// Bytes in the groups, which is the most [`run`] can read.
    pub bytes: u64,
}

impl Plan {
    pub fn files(&self) -> usize {
        self.groups.iter().map(|g| g.files.len()).sum()
    }
}

/// The files under `folder` (matching `filter`, when given) that might
/// have duplicates: at least `min_size` bytes and never empty, with the
/// same size as another.
pub fn plan(tree: &Tree, folder: NodeId, filter: Option<&Search>, min_size: u64) -> Plan {
    let (files, _empty) = layout::files_under(tree, folder, Metric::Logical, filter);
    let min_size = min_size.max(1);
    // A hard-linked file counts once, by one of its names among the
    // candidates: the name the scan found first if it's one of them, else
    // the first other name that is. Two names of one file are never a
    // group, since deleting one frees nothing.
    let mut names: HashMap<NodeId, NodeId> = HashMap::new();
    for f in files {
        if tree.logical(f) < min_size || tree.is_removed(f) {
            continue;
        }
        let file = tree.alias_owner(f).unwrap_or(f);
        let name = names.entry(file).or_insert(f);
        if f == file {
            *name = f;
        }
    }
    let mut by_size: HashMap<u64, Vec<NodeId>> = HashMap::new();
    for f in names.into_values() {
        by_size.entry(tree.logical(f)).or_default().push(f);
    }
    let mut plan = Plan::default();
    for (size, mut files) in by_size {
        if files.len() < 2 {
            continue;
        }
        // Cloud files are counted where they might have had a duplicate.
        files.retain(|&f| {
            let cloud = tree.flags(f) & flags::CLOUD != 0;
            if cloud {
                plan.cloud.push(f);
            }
            !cloud
        });
        if files.len() < 2 {
            continue;
        }
        files.sort_unstable();
        plan.bytes += size * files.len() as u64;
        plan.groups.push(SizeGroup {
            size,
            files: files
                .into_iter()
                .map(|node| Candidate {
                    node,
                    path: tree.path(node),
                })
                .collect(),
        });
    }
    plan.cloud.sort_unstable();
    plan.groups.sort_by(|a, b| {
        let at_stake = |g: &SizeGroup| g.size * (g.files.len() as u64 - 1);
        at_stake(b).cmp(&at_stake(a)).then(b.size.cmp(&a.size))
    });
    plan
}

/// Files with the same contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub size: u64,
    /// In the order of the plan.
    pub files: Vec<NodeId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Found(Group),
    Skipped(NodeId, Skip),
}

/// How far [`run`] has got, readable while it runs.
#[derive(Debug, Default)]
pub struct Counters {
    /// Bytes of the plan dealt with: files confirmed, ruled out, or
    /// skipped count in full. Reaches [`Plan::bytes`] when done.
    pub done: AtomicU64,
    /// Bytes actually read.
    pub read: AtomicU64,
}

/// Reads the planned files with `readers` threads and reports each group of
/// duplicates as soon as it's confirmed, and each file it couldn't read.
/// Work is handed out a file at a time, so one large group keeps every
/// reader busy. Returns `false` if `cancel` was set first; groups already
/// reported stand.
pub fn run<C: Contents>(
    plan: &Plan,
    contents: &C,
    readers: usize,
    cancel: &AtomicBool,
    counters: &Counters,
    on_event: impl Fn(Event) + Sync,
) -> bool {
    let work = Work::new(plan);
    let readers = readers.clamp(1, plan.files().max(1));
    std::thread::scope(|s| {
        for _ in 0..readers {
            s.spawn(|| {
                let mut reader = Reader {
                    contents,
                    cancel,
                    counters,
                    buf: vec![0; BUFFER],
                };
                while let Some(task) = work.next(plan, cancel) {
                    // Hands the task back even if reading panics, so the
                    // other readers don't wait for it forever.
                    let mut taken = Taken {
                        work: &work,
                        stream: task.streams(plan),
                        more: Vec::new(),
                    };
                    let result = reader.perform(plan, &task);
                    work.complete(plan, task, result, counters, &on_event, &mut taken.more);
                }
            });
        }
    });
    !cancel.load(Ordering::Relaxed)
}

struct Cancelled;

type Digest = [u8; 32];

/// One file to hash.
enum Task {
    /// Its first and last [`SAMPLE`] bytes, or all of a small file.
    Sample { group: usize, file: usize },
    /// All of it, after its sample matched another's.
    Whole { bucket: Arc<Bucket>, file: usize },
}

impl Task {
    /// Whether it reads a large file whole, which takes one of the
    /// [`STREAMS`].
    fn streams(&self, plan: &Plan) -> bool {
        matches!(self, Task::Whole { bucket, .. } if plan.groups[bucket.group].size > LARGE)
    }
}

/// A task a reader has taken; dropping it returns its place in the queue
/// along with any whole-file reads it led to.
struct Taken<'a> {
    work: &'a Work,
    stream: bool,
    more: Vec<(Arc<Bucket>, usize)>,
}

impl Drop for Taken<'_> {
    fn drop(&mut self) {
        // Runs while unwinding too, so a poisoned lock is still used.
        let mut q = self.work.queue.lock().unwrap_or_else(|e| e.into_inner());
        q.busy -= 1;
        if self.stream {
            q.streams -= 1;
        }
        q.whole.extend(self.more.drain(..));
        self.work.changed.notify_all();
    }
}

/// Files of one group whose samples matched, being read whole.
struct Bucket {
    group: usize,
    results: Mutex<Results>,
}

/// Digests of a set of files, and how many are still being read.
struct Results {
    left: usize,
    /// Digest and index of the file in its group.
    digests: Vec<(Digest, usize)>,
}

impl Results {
    fn new(left: usize) -> Self {
        Self {
            left,
            digests: Vec::new(),
        }
    }
}

/// What's left to read, shared by the readers.
struct Work {
    queue: Mutex<Queue>,
    changed: Condvar,
    /// Each group's sample digests.
    samples: Vec<Mutex<Results>>,
}

struct Queue {
    /// Whole-file reads, taken before further samples so that groups are
    /// confirmed early.
    whole: VecDeque<(Arc<Bucket>, usize)>,
    /// The next sample to take: group and file.
    next: (usize, usize),
    /// Tasks taken but not completed, which may add whole-file reads.
    busy: usize,
    /// Large files being read whole, at most [`STREAMS`].
    streams: usize,
}

impl Work {
    fn new(plan: &Plan) -> Self {
        Self {
            queue: Mutex::new(Queue {
                whole: VecDeque::new(),
                next: (0, 0),
                busy: 0,
                streams: 0,
            }),
            changed: Condvar::new(),
            samples: plan
                .groups
                .iter()
                .map(|g| Mutex::new(Results::new(g.files.len())))
                .collect(),
        }
    }

    /// The next file to hash, waiting while others may still add work;
    /// `None` when everything is done or `cancel` is set.
    fn next(&self, plan: &Plan, cancel: &AtomicBool) -> Option<Task> {
        let large = |b: &Bucket| plan.groups[b.group].size > LARGE;
        let mut q = self.queue.lock().unwrap();
        loop {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            let streams_free = q.streams < STREAMS;
            if let Some(i) = q.whole.iter().position(|(b, _)| streams_free || !large(b)) {
                let (bucket, file) = q.whole.remove(i).expect("position is in range");
                if large(&bucket) {
                    q.streams += 1;
                }
                q.busy += 1;
                return Some(Task::Whole { bucket, file });
            }
            let (group, file) = q.next;
            if let Some(g) = plan.groups.get(group) {
                q.next = if file + 1 < g.files.len() {
                    (group, file + 1)
                } else {
                    (group + 1, 0)
                };
                q.busy += 1;
                return Some(Task::Sample { group, file });
            }
            if q.busy == 0 && q.whole.is_empty() {
                self.changed.notify_all();
                return None;
            }
            q = self
                .changed
                .wait_timeout(q, Duration::from_millis(50))
                .unwrap()
                .0;
        }
    }

    /// Records a task's result; the last result for a set of files decides
    /// what happens to them next, which may add whole-file reads to `more`.
    /// A cancelled task is dropped.
    fn complete(
        &self,
        plan: &Plan,
        task: Task,
        result: Result<Result<Digest, Skip>, Cancelled>,
        counters: &Counters,
        on_event: &impl Fn(Event),
        more: &mut Vec<(Arc<Bucket>, usize)>,
    ) {
        let (group, file) = match &task {
            Task::Sample { group, file } => (*group, *file),
            Task::Whole { bucket, file } => (bucket.group, *file),
        };
        let g = &plan.groups[group];
        let finish = |files: usize| {
            counters
                .done
                .fetch_add(g.size * files as u64, Ordering::Relaxed);
        };
        if let Ok(result) = result {
            let results = match &task {
                Task::Sample { .. } => &self.samples[group],
                Task::Whole { bucket, .. } => &bucket.results,
            };
            let mut r = results.lock().unwrap();
            let skipped = match result {
                Ok(digest) => {
                    r.digests.push((digest, file));
                    None
                }
                Err(why) => Some(why),
            };
            r.left -= 1;
            let all = (r.left == 0).then(|| std::mem::take(&mut r.digests));
            drop(r);
            if let Some(why) = skipped {
                finish(1);
                on_event(Event::Skipped(g.files[file].node, why));
            }
            for same in all.map(alike).unwrap_or_default() {
                match &task {
                    // Small files were read whole by the sample.
                    Task::Sample { .. } if same.len() >= 2 && g.size > 2 * SAMPLE => {
                        let bucket = Arc::new(Bucket {
                            group,
                            results: Mutex::new(Results::new(same.len())),
                        });
                        more.extend(same.into_iter().map(|f| (Arc::clone(&bucket), f)));
                    }
                    _ => {
                        finish(same.len());
                        if same.len() >= 2 {
                            on_event(Event::Found(Group {
                                size: g.size,
                                files: same.iter().map(|&f| g.files[f].node).collect(),
                            }));
                        }
                    }
                }
            }
        }
    }
}

/// Files with equal digests, each set in plan order.
fn alike(digests: Vec<(Digest, usize)>) -> Vec<Vec<usize>> {
    let mut by_digest: BTreeMap<Digest, Vec<usize>> = BTreeMap::new();
    for (digest, file) in digests {
        by_digest.entry(digest).or_default().push(file);
    }
    by_digest
        .into_values()
        .map(|mut files| {
            files.sort_unstable();
            files
        })
        .collect()
}

struct Reader<'a, C> {
    contents: &'a C,
    cancel: &'a AtomicBool,
    counters: &'a Counters,
    buf: Vec<u8>,
}

impl<C: Contents> Reader<'_, C> {
    fn perform(&mut self, plan: &Plan, task: &Task) -> Result<Result<Digest, Skip>, Cancelled> {
        match task {
            Task::Sample { group, file } => {
                let g = &plan.groups[*group];
                self.digest(&g.files[*file], g.size, g.size > 2 * SAMPLE)
            }
            Task::Whole { bucket, file } => {
                let g = &plan.groups[bucket.group];
                self.digest(&g.files[*file], g.size, false)
            }
        }
    }

    /// Hashes the file's first and last [`SAMPLE`] bytes, or all of it.
    fn digest(
        &mut self,
        c: &Candidate,
        size: u64,
        sample: bool,
    ) -> Result<Result<Digest, Skip>, Cancelled> {
        let mut file = match self.contents.open(&c.path, size) {
            Ok(file) => file,
            Err(why) => return Ok(Err(why)),
        };
        let ranges = if sample {
            [(0, SAMPLE), (size - SAMPLE, SAMPLE)]
        } else {
            [(0, size), (size, 0)]
        };
        let mut hasher = blake3::Hasher::new();
        for (start, len) in ranges {
            if let Err(why) = self.hash_range(&mut file, start, len, &mut hasher)? {
                return Ok(Err(why));
            }
        }
        // Both ways end at the scanned size; more there means it grew.
        match file.read(&mut self.buf[..1]) {
            Ok(0) => Ok(Ok(*hasher.finalize().as_bytes())),
            Ok(_) => Ok(Err(Skip::Changed)),
            Err(e) => Ok(Err(Skip::of(&e))),
        }
    }

    fn hash_range(
        &mut self,
        file: &mut C::File,
        start: u64,
        len: u64,
        hasher: &mut blake3::Hasher,
    ) -> Result<Result<(), Skip>, Cancelled> {
        if let Err(e) = file.seek(SeekFrom::Start(start)) {
            return Ok(Err(Skip::of(&e)));
        }
        let mut left = len;
        while left > 0 {
            if self.cancel.load(Ordering::Relaxed) {
                return Err(Cancelled);
            }
            let n = left.min(self.buf.len() as u64) as usize;
            match file.read_exact(&mut self.buf[..n]) {
                Ok(()) => {}
                // Shorter than scanned.
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    return Ok(Err(Skip::Changed));
                }
                Err(e) => return Ok(Err(Skip::of(&e))),
            }
            hasher.update(&self.buf[..n]);
            self.counters.read.fetch_add(n as u64, Ordering::Relaxed);
            left -= n as u64;
        }
        Ok(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sb_core::search::Matcher;
    use sb_core::size::Allocated;
    use sb_core::tree::FileSizes;
    use std::io::Cursor;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;

    /// File contents by path; a missing path has vanished.
    #[derive(Default)]
    struct FakeContents {
        files: HashMap<PathBuf, Result<Vec<u8>, Skip>>,
        opened: Mutex<Vec<PathBuf>>,
        /// Sets the flag after this many opens, to cancel mid-run.
        cancel_after: Option<(usize, &'static AtomicBool)>,
    }

    impl Contents for FakeContents {
        type File = Cursor<Vec<u8>>;

        fn open(&self, path: &Path, len: u64) -> Result<Self::File, Skip> {
            let mut opened = self.opened.lock().unwrap();
            opened.push(path.to_owned());
            if let Some((after, flag)) = self.cancel_after
                && opened.len() >= after
            {
                flag.store(true, Ordering::Relaxed);
            }
            match self.files.get(path) {
                None => Err(Skip::Vanished),
                Some(Err(why)) => Err(*why),
                Some(Ok(bytes)) if bytes.len() as u64 != len => Err(Skip::Changed),
                Some(Ok(bytes)) => Ok(Cursor::new(bytes.clone())),
            }
        }
    }

    struct Fixture {
        tree: Tree,
        contents: FakeContents,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                tree: Tree::new("/r"),
                contents: FakeContents::default(),
            }
        }

        /// Adds a file with these contents to the tree and the fake disk.
        fn file(&mut self, name: &str, bytes: Vec<u8>) -> NodeId {
            self.file_flagged(name, bytes, 0)
        }

        fn file_flagged(&mut self, name: &str, bytes: Vec<u8>, extra: u8) -> NodeId {
            let len = bytes.len() as u64;
            let node = self
                .tree
                .add_file(
                    NodeId::ROOT,
                    name.as_ref(),
                    FileSizes {
                        logical: len,
                        allocated: Allocated::known(len),
                    },
                    None,
                    extra,
                )
                .unwrap();
            self.contents.files.insert(self.tree.path(node), Ok(bytes));
            node
        }

        fn run(&self, min_size: u64) -> (Vec<Group>, Vec<(NodeId, Skip)>, bool) {
            let plan = plan(&self.tree, NodeId::ROOT, None, min_size);
            let cancel = AtomicBool::new(false);
            self.run_plan(&plan, &cancel)
        }

        fn run_plan(
            &self,
            plan: &Plan,
            cancel: &AtomicBool,
        ) -> (Vec<Group>, Vec<(NodeId, Skip)>, bool) {
            let events = Mutex::new(Vec::new());
            let counters = Counters::default();
            let done = run(plan, &self.contents, 2, cancel, &counters, |e| {
                events.lock().unwrap().push(e)
            });
            if done {
                assert_eq!(counters.done.load(Ordering::Relaxed), plan.bytes);
            }
            let mut groups = Vec::new();
            let mut skipped = Vec::new();
            for e in events.into_inner().unwrap() {
                match e {
                    Event::Found(g) => groups.push(g),
                    Event::Skipped(n, why) => skipped.push((n, why)),
                }
            }
            groups.sort_by_key(|g| g.files[0]);
            skipped.sort();
            (groups, skipped, done)
        }
    }

    fn bytes(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u8).wrapping_mul(31) ^ seed)
            .collect()
    }

    const BIG: usize = 3 * SAMPLE as usize;

    #[test]
    fn groups_by_size_and_drops_unique_sizes() {
        let mut f = Fixture::new();
        let a = f.file("a", bytes(5000, 1));
        let b = f.file("b", bytes(5000, 1));
        let c = f.file("c", bytes(5000, 2));
        f.file("lonely", bytes(7000, 1));
        f.file("empty1", Vec::new());
        f.file("empty2", Vec::new());
        let plan = plan(&f.tree, NodeId::ROOT, None, 0);
        assert_eq!(
            plan.groups.len(),
            1,
            "unique sizes and empty files are left out"
        );
        assert_eq!(plan.groups[0].files.len(), 3);
        assert_eq!(plan.bytes, 15_000);

        let (groups, skipped, done) = f.run(0);
        assert!(done && skipped.is_empty());
        assert_eq!(
            groups,
            [Group {
                size: 5000,
                files: vec![a, b]
            }]
        );
        let _ = c;
        // Only the small files' samples were read: they are the whole file.
        assert_eq!(f.contents.opened.lock().unwrap().len(), 3);
    }

    #[test]
    fn honours_the_minimum_size() {
        let mut f = Fixture::new();
        f.file("a", bytes(500, 1));
        f.file("b", bytes(500, 1));
        let big = f.file("c", bytes(5000, 1));
        let big2 = f.file("d", bytes(5000, 1));
        let (groups, _, _) = f.run(1000);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].files, [big, big2]);
    }

    #[test]
    fn samples_split_groups_and_full_hashes_confirm() {
        let mut f = Fixture::new();
        let same1 = f.file("same1", bytes(BIG, 1));
        let same2 = f.file("same2", bytes(BIG, 1));
        // Same size, different start: split by the sample, never read whole.
        f.file("other", bytes(BIG, 9));
        // Same start and end, different middle: only the full hash tells.
        let mut middle = bytes(BIG, 1);
        middle[BIG / 2] ^= 0xFF;
        let mid = f.file("middle", middle);
        // Same start, different end.
        let mut tail = bytes(BIG, 1);
        tail[BIG - 1] ^= 0xFF;
        f.file("tail", tail);

        let (groups, skipped, done) = f.run(0);
        assert!(done && skipped.is_empty());
        assert_eq!(
            groups,
            [Group {
                size: BIG as u64,
                files: vec![same1, same2]
            }]
        );
        let opened = f.contents.opened.lock().unwrap();
        // Five samples, then whole reads of the three that sampled alike.
        assert_eq!(opened.len(), 8);
        assert_eq!(
            opened.iter().filter(|p| p.ends_with("middle")).count(),
            2,
            "{mid:?} was sampled and then read whole"
        );
        assert_eq!(opened.iter().filter(|p| p.ends_with("other")).count(), 1);
        assert_eq!(opened.iter().filter(|p| p.ends_with("tail")).count(), 1);
    }

    #[test]
    fn hard_link_names_count_once() {
        let mut f = Fixture::new();
        let owner = f.file("owner", bytes(5000, 1));
        let alias = f.file("alias", bytes(5000, 1));
        f.tree.mark_alias(alias, owner);
        // Two names for one file are not duplicates.
        assert!(plan(&f.tree, NodeId::ROOT, None, 0).groups.is_empty());

        let copy = f.file("copy", bytes(5000, 1));
        let (groups, _, _) = f.run(0);
        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0].files,
            [owner, copy],
            "one name stands for the file"
        );

        // Where the first name is filtered out, another name stands for the
        // file, so the separate copy still has its partner.
        let mut search = Search::new(Matcher::new("!owner").unwrap());
        search.catch_up(&f.tree, usize::MAX);
        assert_eq!(search.totals(NodeId::ROOT).files, 2);
        let plan = plan(&f.tree, NodeId::ROOT, Some(&search), 0);
        assert_eq!(plan.groups.len(), 1);
        let nodes: Vec<_> = plan.groups[0].files.iter().map(|c| c.node).collect();
        assert_eq!(nodes, [alias, copy]);
    }

    #[test]
    fn cloud_files_are_never_opened() {
        let mut f = Fixture::new();
        let local = f.file("local", bytes(5000, 1));
        let cloud = f.file_flagged("cloud", bytes(5000, 1), flags::CLOUD);
        let cloud2 = f.file_flagged("cloud2", bytes(5000, 1), flags::CLOUD);
        f.file_flagged("alone", bytes(6000, 1), flags::CLOUD);

        let plan = plan(&f.tree, NodeId::ROOT, None, 0);
        assert!(plan.groups.is_empty(), "one local file is left");
        assert_eq!(
            plan.cloud,
            [cloud, cloud2],
            "a cloud file of a unique size isn't counted"
        );
        let (groups, _, _) = f.run(0);
        assert!(groups.is_empty());
        assert!(f.contents.opened.lock().unwrap().is_empty());
        let _ = local;
    }

    #[test]
    fn unreadable_files_are_skipped_not_errors() {
        let mut f = Fixture::new();
        let a = f.file("a", bytes(BIG, 1));
        let b = f.file("b", bytes(BIG, 1));
        let gone = f.file("gone", bytes(BIG, 1));
        let locked = f.file("locked", bytes(BIG, 1));
        let shrunk = f.file("shrunk", bytes(BIG, 1));
        f.contents.files.remove(&f.tree.path(gone));
        f.contents
            .files
            .insert(f.tree.path(locked), Err(Skip::InUse));
        f.contents
            .files
            .insert(f.tree.path(shrunk), Ok(bytes(BIG - 1, 1)));

        let (groups, skipped, done) = f.run(0);
        assert!(done);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].files, [a, b]);
        assert_eq!(
            skipped,
            [
                (gone, Skip::Vanished),
                (locked, Skip::InUse),
                (shrunk, Skip::Changed)
            ]
        );
    }

    #[test]
    fn a_file_that_grew_is_skipped() {
        let mut f = Fixture::new();
        let a = f.file("a", bytes(5000, 1));
        let b = f.file("b", bytes(5000, 1));
        // The fake checks length on open; grow it behind its back.
        struct Growing(FakeContents);
        impl Contents for Growing {
            type File = Cursor<Vec<u8>>;
            fn open(&self, path: &Path, len: u64) -> Result<Self::File, Skip> {
                let mut file = self.0.open(path, len)?;
                if path.ends_with("b") {
                    file.get_mut().push(0);
                }
                Ok(file)
            }
        }
        let plan = plan(&f.tree, NodeId::ROOT, None, 0);
        let events = Mutex::new(Vec::new());
        let contents = Growing(std::mem::take(&mut f.contents));
        run(
            &plan,
            &contents,
            1,
            &AtomicBool::new(false),
            &Counters::default(),
            |e| events.lock().unwrap().push(e),
        );
        assert_eq!(
            events.into_inner().unwrap(),
            [Event::Skipped(b, Skip::Changed)]
        );
        let _ = a;
    }

    #[test]
    fn filter_limits_the_candidates() {
        let mut f = Fixture::new();
        let v1 = f.file("one.mp4", bytes(5000, 1));
        let v2 = f.file("two.mp4", bytes(5000, 1));
        f.file("one.txt", bytes(5000, 1));
        let mut search = Search::new(Matcher::new("*.mp4").unwrap());
        search.catch_up(&f.tree, usize::MAX);
        let plan = plan(&f.tree, NodeId::ROOT, Some(&search), 0);
        assert_eq!(plan.groups.len(), 1);
        let nodes: Vec<_> = plan.groups[0].files.iter().map(|c| c.node).collect();
        assert_eq!(nodes, [v1, v2]);
    }

    #[test]
    fn removed_files_are_left_out() {
        let mut f = Fixture::new();
        f.file("a", bytes(5000, 1));
        let b = f.file("b", bytes(5000, 1));
        f.tree.remove(b);
        assert!(plan(&f.tree, NodeId::ROOT, None, 0).groups.is_empty());
    }

    #[test]
    fn most_space_at_stake_goes_first() {
        let mut f = Fixture::new();
        for i in 0..2 {
            f.file(&format!("big{i}"), bytes(9000, 1));
        }
        for i in 0..4 {
            f.file(&format!("many{i}"), bytes(4000, 1));
        }
        let plan = plan(&f.tree, NodeId::ROOT, None, 0);
        let sizes: Vec<_> = plan.groups.iter().map(|g| g.size).collect();
        assert_eq!(sizes, [4000, 9000], "3 × 4000 beats 1 × 9000");
    }

    #[test]
    fn cancelling_stops_reading_and_keeps_what_was_found() {
        static CANCEL: AtomicBool = AtomicBool::new(false);
        let mut f = Fixture::new();
        // The first group read holds the largest files.
        let a = f.file("a", bytes(BIG * 2, 1));
        let b = f.file("b", bytes(BIG * 2, 1));
        for i in 0..10 {
            f.file(&format!("small{i}"), bytes(1000, 1));
        }
        let plan = plan(&f.tree, NodeId::ROOT, None, 0);
        assert_eq!(plan.groups[0].size, BIG as u64 * 2);
        // Cancel when the first file of the second group is opened.
        f.contents.cancel_after = Some((5, &CANCEL));
        let events = Mutex::new(Vec::new());
        let done = run(&plan, &f.contents, 1, &CANCEL, &Counters::default(), |e| {
            events.lock().unwrap().push(e)
        });
        assert!(!done);
        assert_eq!(
            events.into_inner().unwrap(),
            [Event::Found(Group {
                size: BIG as u64 * 2,
                files: vec![a, b]
            })]
        );
        assert_eq!(f.contents.opened.lock().unwrap().len(), 5, "nothing after");
    }

    #[test]
    fn one_large_group_keeps_every_reader_busy() {
        /// Holds each open a moment and counts how many overlap.
        struct Slow {
            inner: FakeContents,
            open: Mutex<(usize, usize)>,
        }
        impl Contents for Slow {
            type File = Cursor<Vec<u8>>;
            fn open(&self, path: &Path, len: u64) -> Result<Self::File, Skip> {
                {
                    let mut open = self.open.lock().unwrap();
                    open.0 += 1;
                    open.1 = open.1.max(open.0);
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
                self.open.lock().unwrap().0 -= 1;
                self.inner.open(path, len)
            }
        }
        let mut f = Fixture::new();
        let files: Vec<_> = (0..8)
            .map(|i| f.file(&format!("copy{i}"), bytes(BIG, 1)))
            .collect();
        let plan = plan(&f.tree, NodeId::ROOT, None, 0);
        assert_eq!(plan.groups.len(), 1);
        let contents = Slow {
            inner: std::mem::take(&mut f.contents),
            open: Mutex::new((0, 0)),
        };
        let events = Mutex::new(Vec::new());
        let counters = Counters::default();
        let done = run(
            &plan,
            &contents,
            4,
            &AtomicBool::new(false),
            &counters,
            |e| events.lock().unwrap().push(e),
        );
        assert!(done);
        assert_eq!(counters.done.load(Ordering::Relaxed), plan.bytes);
        assert_eq!(
            events.into_inner().unwrap(),
            [Event::Found(Group {
                size: BIG as u64,
                files
            })]
        );
        assert!(
            contents.open.lock().unwrap().1 > 1,
            "the group's files were read in parallel"
        );
    }

    #[test]
    fn a_reader_that_panics_lets_the_others_finish() {
        struct Panics(FakeContents);
        impl Contents for Panics {
            type File = Cursor<Vec<u8>>;
            fn open(&self, path: &Path, len: u64) -> Result<Self::File, Skip> {
                assert!(!path.ends_with("bad"), "reading failed badly");
                self.0.open(path, len)
            }
        }
        let mut f = Fixture::new();
        f.file("bad", bytes(5000, 1));
        f.file("good", bytes(5000, 1));
        for i in 0..6 {
            f.file(&format!("other{i}"), bytes(7000, 1));
        }
        let plan = plan(&f.tree, NodeId::ROOT, None, 0);
        let contents = Panics(std::mem::take(&mut f.contents));
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let found = AtomicUsize::new(0);
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run(
                    &plan,
                    &contents,
                    3,
                    &AtomicBool::new(false),
                    &Counters::default(),
                    |e| {
                        if matches!(e, Event::Found(_)) {
                            found.fetch_add(1, Ordering::Relaxed);
                        }
                    },
                )
            }));
            let _ = tx.send((outcome.is_err(), found.load(Ordering::Relaxed)));
        });
        let (panicked, found) = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("run returned");
        assert!(panicked, "the panic reaches the caller");
        assert_eq!(found, 1, "the other group was still read");
    }
}
