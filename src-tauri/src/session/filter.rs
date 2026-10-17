//! The active filter, kept caught up with the scan off the interactive path.
//!
//! A search overlay ([`Search`]) has to examine every node the scanner adds.
//! Requests used to do that themselves, catching up on everything scanned
//! since the previous request while holding the search's lock and the
//! tree's, and the layout kept the search's lock through computing and
//! encoding; so a tooltip could wait for a large backlog or a whole layout.
//! Now a background job advances the search after each scan status, in
//! bounded steps that release both locks in between, and requests only read
//! it, under a read lock, so layouts, details, totals, and tooltips run side
//! by side. While requests read, the job backs off and tries again, and
//! only after 20 tries in a row (requests overlapping without a gap) does it
//! queue for the lock, so a request waits for one step at most, plus
//! whatever request the job queued behind.
//!
//! While scanning, a request can see the search as of an earlier status
//! than the tree it reads, usually by a status interval at most; the
//! search's [`ActiveSearch::revision`] says which. Under filtered requests
//! that overlap without a gap, the job gets in only about once per 20 ms
//! (one forced step after its 20 tries), so a backlog can build up while
//! scanning. Once the scan has finished, a request first brings the search
//! fully up to date, whatever the job hadn't reached, so final results are
//! exactly what catching up in the request gave.

use super::Session;
use crate::metrics::{self, Held, locked};
use sb_core::search::Search;
use sb_core::tree::Tree;
use sb_scan::Progress;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, RwLock, RwLockReadGuard, TryLockError, Weak};
use std::time::Duration;

/// Nodes the job examines per hold of the locks, about 0.4 ms (40 ns a
/// node): the longest a request should wait for it.
const CATCH_UP_STEP: usize = 10_000;

/// How long the job waits before trying again while requests read the search.
const BUSY_RETRY: Duration = Duration::from_millis(1);

/// Tries in a row the job makes while requests read the search before it
/// waits for its turn instead, so overlapping requests can't keep the
/// search from catching up indefinitely.
const BUSY_TRIES: u32 = 20;

/// How long after catching up with a status that requests read behind the
/// job waits for the next one before saying the filter caught up (see
/// [`Filter::on_caught_up`]): while the scan keeps reporting, its next
/// status brings the requests anyway.
const QUIET: Duration = Duration::from_millis(400);

/// Called with the search and the scan revision it caught up with.
type CaughtUp = Box<dyn Fn(u32, u64) + Send + Sync>;

pub(super) type SearchGuard<'a> = Held<RwLockReadGuard<'a, Option<ActiveSearch>>>;
pub(super) type TreeGuard<'a> = Held<RwLockReadGuard<'a, Tree>>;

pub(super) struct ActiveSearch {
    pub id: u32,
    pub search: Search,
    /// The scan revision the search was last fully caught up with. While
    /// scanning, the tree read alongside may be further on.
    pub revision: u64,
}

impl ActiveSearch {
    /// Examines everything left in `tree`, which is at scan revision
    /// `revision`.
    pub fn catch_up(&mut self, tree: &Tree, revision: u64) {
        if metrics::enabled() {
            metrics::count("catch-up", self.search.backlog(tree));
        }
        let _catch_up = metrics::span("catch-up");
        self.search.catch_up(tree, usize::MAX);
        self.revision = revision;
    }
}

/// The active search, shared with the job that keeps it caught up.
pub(super) struct Filter {
    pub active: RwLock<Option<ActiveSearch>>,
    /// Whether the job is running; without it, requests catch up themselves.
    background: bool,
    /// Steps the job may still take, for tests of what requests see
    /// meanwhile; `usize::MAX` is no limit.
    #[cfg(test)]
    steps_left: std::sync::atomic::AtomicUsize,
    wake: Mutex<Wake>,
    woken: Condvar,
    /// The latest scan revision the job was asked to catch up with.
    nudged: AtomicU64,
    /// A request read the search behind `nudged` while scanning.
    read_behind: AtomicBool,
    caught_up: OnceLock<CaughtUp>,
}

#[derive(Default)]
struct Wake {
    /// The latest scan revision to catch up with, while the job is due.
    due: Option<u64>,
    stop: bool,
}

/// What one step of the job did.
enum Step {
    /// The search (its number, and the revision it reflects now) is caught
    /// up, or there's none.
    Done(Option<(u32, u64)>),
    More,
    /// A request is reading the search.
    Busy,
    /// The scan or the session is gone.
    Gone,
}

impl Filter {
    /// No search yet, and the job that will keep one caught up with `tree`
    /// started, waiting for [`Filter::nudge`].
    pub fn start(tree: Arc<OnceLock<Weak<RwLock<Tree>>>>) -> Arc<Self> {
        // The job needs the shared state before it can start, and the state
        // records whether it did, so it's built twice on failure.
        let new = |background| {
            Arc::new(Self {
                active: RwLock::new(None),
                background,
                #[cfg(test)]
                steps_left: std::sync::atomic::AtomicUsize::new(usize::MAX),
                wake: Mutex::new(Wake::default()),
                woken: Condvar::new(),
                nudged: AtomicU64::new(0),
                read_behind: AtomicBool::new(false),
                caught_up: OnceLock::new(),
            })
        };
        let filter = new(true);
        let job = Arc::clone(&filter);
        match std::thread::Builder::new()
            .name("sb-search".into())
            .spawn(move || job.run(&tree))
        {
            Ok(_) => filter,
            Err(e) => {
                eprintln!("couldn't start keeping the filter up to date: {e}");
                new(false)
            }
        }
    }

    /// Has the job catch the search up with scan revision `revision`, which
    /// the tree has reached.
    pub fn nudge(&self, revision: u64) {
        self.nudged.fetch_max(revision, Ordering::SeqCst);
        let mut wake = self.wake.lock().unwrap_or_else(|e| e.into_inner());
        wake.due = Some(wake.due.map_or(revision, |d| d.max(revision)));
        self.woken.notify_one();
    }

    /// Has `f` called when the job catches up with a status after requests
    /// read the search behind it, and the scan then stays quiet for
    /// [`QUIET`]. Otherwise what those requests showed would stay behind
    /// until the scan reports again, which a stalled listing (a slow share)
    /// can put off for long.
    pub fn on_caught_up(&self, f: impl Fn(u32, u64) + Send + Sync + 'static) {
        let _ = self.caught_up.set(Box::new(f));
    }

    /// Ends the job.
    pub fn stop(&self) {
        self.wake.lock().unwrap_or_else(|e| e.into_inner()).stop = true;
        self.woken.notify_one();
    }

    fn stopped(&self) -> bool {
        self.wake.lock().unwrap_or_else(|e| e.into_inner()).stop
    }

    /// `tree` is set once the scan has started, before it reports.
    fn run(&self, tree: &OnceLock<Weak<RwLock<Tree>>>) {
        let mut scan = None;
        // Requests read behind, and the job hasn't caught up with every
        // status since.
        let mut owed = false;
        // Caught up with every status after requests read behind, to be
        // announced once the scan is quiet.
        let mut announce: Option<(u32, u64)> = None;
        loop {
            let revision = {
                let mut wake = self.wake.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    if wake.stop {
                        return;
                    }
                    if let Some(revision) = wake.due.take() {
                        // A newer status brings requests of its own.
                        if announce.is_some_and(|(_, caught)| revision > caught) {
                            announce = None;
                        }
                        break revision;
                    }
                    let Some((search, revision)) = announce else {
                        wake = self.woken.wait(wake).unwrap_or_else(|e| e.into_inner());
                        continue;
                    };
                    let (next, waited) = self
                        .woken
                        .wait_timeout(wake, QUIET)
                        .unwrap_or_else(|e| e.into_inner());
                    wake = next;
                    if waited.timed_out() && wake.due.is_none() && !wake.stop {
                        announce = None;
                        if let Some(f) = self.caught_up.get() {
                            f(search, revision);
                        }
                    }
                }
            };
            let tree = scan.get_or_insert_with(|| tree.wait().clone());
            let mut busy = 0;
            // One timed request per round, from its first step, so the
            // report counts rounds rather than every step.
            let mut timing = None;
            loop {
                match self.step(tree, revision, busy >= BUSY_TRIES, &mut timing) {
                    Step::Done(caught) => {
                        // A request that read behind did so before this
                        // step's write lock, so its flag is set by now.
                        owed |= self.read_behind.swap(false, Ordering::SeqCst);
                        // Statuses that came while it was busy are still
                        // due; announce once it has caught up with all.
                        let latest = self.nudged.load(Ordering::SeqCst);
                        if owed && caught.is_none_or(|(_, revision)| revision >= latest) {
                            owed = false;
                            announce = caught;
                        }
                        break;
                    }
                    Step::Gone => return,
                    // Between steps the scanner can take the tree's lock:
                    // if it's waiting, the next read waits behind it.
                    Step::More => {
                        busy = 0;
                        std::thread::yield_now();
                    }
                    Step::Busy => {
                        busy += 1;
                        std::thread::sleep(BUSY_RETRY);
                    }
                }
                if self.stopped() {
                    return;
                }
            }
        }
    }

    /// Examines up to [`CATCH_UP_STEP`] nodes. Normally the tree is
    /// read-locked first, which may wait for the scanner, and the search
    /// only tried, so requests never wait while the job waits; that's also
    /// why taking them in the opposite order to requests can't deadlock.
    /// After requests kept it off for [`BUSY_TRIES`] tries, it `wait`s for
    /// the search's write lock instead, taking the locks in the requests'
    /// order (search, then tree), so new requests queue behind it and the
    /// search catches up a step.
    fn step(
        &self,
        tree: &Weak<RwLock<Tree>>,
        revision: u64,
        wait: bool,
        timing: &mut Option<metrics::Command>,
    ) -> Step {
        let Some(tree) = tree.upgrade() else {
            return Step::Gone;
        };
        let (mut active, tree) = if wait {
            let Ok(active) = self.active.write() else {
                return Step::Gone;
            };
            let Ok(tree) = tree.read() else {
                return Step::Gone;
            };
            (active, tree)
        } else {
            let Ok(tree) = tree.read() else {
                return Step::Gone;
            };
            match self.active.try_write() {
                Ok(active) => (active, tree),
                Err(TryLockError::WouldBlock) => return Step::Busy,
                Err(TryLockError::Poisoned(_)) => return Step::Gone,
            }
        };
        // Checked under the lock, so a test that sets it and then reads
        // the search knows no step comes in between.
        #[cfg(test)]
        if self.steps_left.load(Ordering::SeqCst) == 0 {
            return Step::Busy;
        }
        let Some(active) = active.as_mut() else {
            return Step::Done(None);
        };
        if timing.is_none() {
            *timing = Some(metrics::command("search_catch_up"));
            if metrics::enabled() {
                metrics::count("catch-up", active.search.backlog(&tree));
            }
        }
        #[cfg(test)]
        let _ = self
            .steps_left
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n != usize::MAX).then(|| n.saturating_sub(1))
            });
        let _catch_up = metrics::span("catch-up");
        if active.search.catch_up(&tree, CATCH_UP_STEP) {
            active.revision = active.revision.max(revision);
            Step::Done(Some((active.id, active.revision)))
        } else {
            Step::More
        }
    }
}

impl Session {
    /// Read-locks the search (if `filtered`) and then `tree`, and reads the
    /// scan's progress with the tree's lock held. Once the scan has
    /// finished, a search behind the tree is first brought up to date.
    pub(super) fn read_locked<'a>(
        &'a self,
        tree: &'a RwLock<Tree>,
        filtered: bool,
    ) -> Result<(Option<SearchGuard<'a>>, TreeGuard<'a>, Progress), String> {
        loop {
            let search = if filtered {
                Some(
                    locked("search", || self.filter.active.read())
                        .map_err(|_| "search is unavailable")?,
                )
            } else {
                None
            };
            let held = locked("tree", || tree.read()).map_err(|_| "scan data is unavailable")?;
            let progress = self.scan.progress();
            let active = search.as_deref().and_then(Option::as_ref);
            let finished = progress.state.is_finished();
            let behind = (finished || !self.filter.background)
                && active.is_some_and(|a| a.revision != progress.revision);
            if !finished
                && active.is_some_and(|a| a.revision < self.filter.nudged.load(Ordering::SeqCst))
            {
                self.filter.read_behind.store(true, Ordering::SeqCst);
            }
            if !behind {
                return Ok((search, held, progress));
            }
            // Caught up exclusively, then read again: a delete may edit the
            // tree in between, which the next pass notices.
            drop(held);
            drop(search);
            self.settle()?;
        }
    }

    /// Brings the search up to date with the tree now.
    fn settle(&self) -> Result<(), String> {
        let mut active =
            locked("search", || self.filter.active.write()).map_err(|_| "search is unavailable")?;
        let tree = self.scan.tree();
        let tree = locked("tree", || tree.read()).map_err(|_| "scan data is unavailable")?;
        // Read with the tree's lock held, so it describes the tree.
        let revision = self.scan.progress().revision;
        if let Some(active) = active.as_mut().filter(|a| a.revision != revision) {
            active.catch_up(&tree, revision);
        }
        Ok(())
    }
}

/// The active search, read-locked by [`Session::read_locked`], if it is `id`.
pub(super) fn current<'a>(
    search: &'a Option<SearchGuard<'_>>,
    id: u32,
) -> Result<&'a ActiveSearch, String> {
    search
        .as_deref()
        .and_then(Option::as_ref)
        .filter(|a| a.id == id)
        .ok_or_else(|| format!("search {id} is no longer current"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sb_core::search::Matcher;
    use sb_core::tree::{DirState, NodeId};
    use sb_protocol as proto;
    use sb_scan::ScanConfig;
    use sb_scan::fake::{FakeFs, Gate};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Instant;

    /// Sets the flag when dropped, including while unwinding.
    struct Stop<'a>(&'a AtomicBool);

    impl Drop for Stop<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }

    fn until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting until {what}");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// A scan of `a` (300 files, a third of them logs), listed at once,
    /// with `b` (30,000 files, more than a step, a tenth of them logs) and
    /// `c` listed once their gates are released.
    fn scan() -> (Session, Gate, Gate) {
        let fs = FakeFs::new();
        for i in 0..300 {
            let ext = if i % 3 == 0 { "log" } else { "txt" };
            fs.file(&format!("a/{i}.{ext}"), 10 + i, 4096);
        }
        for i in 0..30_000 {
            let ext = if i % 10 == 0 { "log" } else { "txt" };
            fs.file(&format!("b/{i}.{ext}"), i, 4096);
        }
        fs.file("c/last.log", 7, 4096);
        let b = fs.gate_dir("b");
        let c = fs.gate_dir("c");
        // Enough workers that the two waiting at the gates leave some free.
        let config = ScanConfig {
            workers: 4,
            progress_interval: Duration::from_millis(1),
            ..ScanConfig::default()
        };
        let session = Session::start_on(fs.root(), fs, config, None, None, |_| {});
        until("a is listed", || listed(&session, "a"));
        (session, b, c)
    }

    /// Whether the folder `name` in the root has been listed.
    fn listed(session: &Session, name: &str) -> bool {
        let tree = session.scan.tree();
        let tree = tree.read().unwrap();
        tree.children(NodeId::ROOT)
            .any(|c| tree.name(c) == name && tree.dir_state(c) == DirState::Listed)
    }

    fn finished(session: &Session) -> bool {
        session.scan.progress().state.is_finished()
    }

    /// Files and bytes matching `query` in the tree as it is now, searched
    /// afresh.
    fn fresh(session: &Session, query: &str) -> (u64, u64) {
        let tree = session.scan.tree();
        let tree = tree.read().unwrap();
        let mut search = Search::new(Matcher::new(query).unwrap());
        assert!(search.catch_up(&tree, usize::MAX));
        let t = search.totals(NodeId::ROOT);
        (u64::from(t.files), t.logical)
    }

    fn totals(s: &proto::SearchSummary) -> (u64, u64) {
        (s.files, s.logical)
    }

    #[test]
    fn requests_read_the_filter_while_a_job_keeps_it_caught_up() {
        let (session, b, c) = scan();
        let id = session.search_set("*.log").unwrap().unwrap().search;
        // While the job is held off, requests read the search as it was,
        // whole, saying which revision.
        session.filter.steps_left.store(0, Ordering::SeqCst);
        let first = session.search_summary(id).unwrap();
        assert_eq!(totals(&first), fresh(&session, "*.log"));
        assert_eq!(first.files, 100);
        b.release();
        until("b is listed", || listed(&session, "b"));
        until("a newer status", || {
            session.scan.progress().revision > first.revision
        });
        assert_eq!(
            session.search_summary(id).unwrap(),
            first,
            "requests don't catch up while scanning"
        );
        session
            .filter
            .steps_left
            .store(usize::MAX, Ordering::SeqCst);

        // The revision is a lower bound: a round asked for at an earlier
        // status can finish the work before the round for the newest one
        // records it.
        until("the job catches up, at a newer revision", || {
            let s = session.search_summary(id).unwrap();
            totals(&s) == fresh(&session, "*.log") && s.revision > first.revision
        });
        assert!(!finished(&session), "caught up while scanning");
        assert_eq!(session.search_summary(id).unwrap().files, 3_100);

        c.release();
        until("the scan finishes", || finished(&session));
        let last = session.search_summary(id).unwrap();
        assert_eq!(totals(&last), fresh(&session, "*.log"));
        assert_eq!(last.files, 3_101);
        assert_eq!(
            last.revision,
            session.scan.progress().revision,
            "up to date once the scan has finished"
        );
    }

    #[test]
    fn reads_are_never_torn_while_the_job_catches_up() {
        let (session, b, c) = scan();
        let id = session.search_set("*.log").unwrap().unwrap().search;
        let stop = AtomicBool::new(false);
        let reading = AtomicUsize::new(0);
        let reads = std::thread::scope(|s| {
            // Stops the readers however this ends, so a failure below
            // isn't followed by the scope waiting for them forever.
            let _stop = Stop(&stop);
            let readers: Vec<_> = (0..4)
                .map(|_| {
                    s.spawn(|| {
                        let (mut reads, mut revision) = (0u64, 0);
                        while !stop.load(Ordering::Relaxed) {
                            let tree = session.scan.tree();
                            let (search, tree, _) = session.read_locked(&tree, true).unwrap();
                            let active = current(&search, id).unwrap();
                            // Each step leaves every folder's totals the sum
                            // of its contents'.
                            let root = active.search.totals(NodeId::ROOT);
                            let (files, logical) = tree
                                .children(NodeId::ROOT)
                                .map(|c| active.search.totals(c))
                                .fold((0, 0), |(f, l), t| (f + t.files, l + t.logical));
                            assert_eq!((root.files, root.logical), (files, logical));
                            assert!(active.revision >= revision, "never goes back");
                            revision = active.revision;
                            if reads == 0 {
                                reading.fetch_add(1, Ordering::SeqCst);
                            }
                            reads += 1;
                            drop((search, tree));
                            // Back to back, but leaving the other threads
                            // the processor on a machine with few cores.
                            std::thread::yield_now();
                        }
                        reads
                    })
                })
                .collect();
            until("every reader reads", || reading.load(Ordering::SeqCst) == 4);
            b.release();
            until("b is listed", || listed(&session, "b"));
            until("the job catches up", || {
                totals(&session.search_summary(id).unwrap()) == fresh(&session, "*.log")
            });
            c.release();
            until("the scan finishes", || finished(&session));
            stop.store(true, Ordering::Relaxed);
            readers
                .into_iter()
                .map(|r| r.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert!(reads.iter().all(|&n| n > 0), "every reader read: {reads:?}");
        assert_eq!(
            totals(&session.search_summary(id).unwrap()),
            fresh(&session, "*.log")
        );
    }

    #[test]
    fn says_when_the_filter_caught_up_after_requests_read_behind_and_the_scan_stalled() {
        let (session, b, _c) = scan();
        let (sink, updates) = std::sync::mpsc::channel();
        session.on_filter_updated(move |u| {
            let _ = sink.send(u);
        });
        let id = session.search_set("*.log").unwrap().unwrap().search;
        let held = &session.filter.steps_left;
        held.store(0, Ordering::SeqCst);
        b.release();
        // The scan reports nothing more once `b` is in, since `c` is never
        // listed: wait for the status saying so (only `c` left to list),
        // which the job is nudged with just after it's published. Sampling
        // for a quiet spell instead misses a status the scanner coalesced
        // and publishes late, and a newer status rightly drops the
        // announcement, as it brings requests of its own.
        until("the last status reaches the job", || {
            let p = session.scan.progress();
            p.pending_dirs == 1 && session.filter.nudged.load(Ordering::SeqCst) == p.revision
        });
        let last = session.scan.progress().revision;
        // Answered behind that status.
        let behind = session.search_summary(id).unwrap();
        assert!(behind.revision < last);
        assert_eq!(behind.files, 100);
        assert!(
            session.filter.read_behind.load(Ordering::SeqCst),
            "the request is noted as read behind"
        );
        held.store(usize::MAX, Ordering::SeqCst);
        // Due once the job catches up (milliseconds) and then `QUIET`.
        let u = updates
            .recv_timeout(Duration::from_secs(10))
            .unwrap_or_else(|_| {
                panic!(
                    "no update announced; the scan went from revision {last} to {}",
                    session.scan.progress().revision
                )
            });
        let revision = session.scan.progress().revision;
        assert_eq!(revision, last, "the scan stayed quiet");
        assert_eq!(
            (u.generation, u.search, u.revision),
            (session.generation(), id, revision)
        );
        let now = session.search_summary(id).unwrap();
        assert_eq!((now.files, now.revision), (3_100, revision));
        // Read up to date, nothing more is announced.
        assert!(updates.recv_timeout(QUIET * 2).is_err());
    }

    #[test]
    fn replacing_or_clearing_the_filter_drops_what_was_kept_for_it() {
        let (session, _b, _c) = scan();
        let fill = |id| {
            let generation = session.generation();
            session
                .layout(&proto::LayoutRequest {
                    generation,
                    request: 0,
                    view: 0,
                    metric: proto::Metric::Logical,
                    width: 800.0,
                    height: 600.0,
                    search: Some(id),
                })
                .unwrap();
            session
                .search_results(&proto::SearchResultsRequest {
                    generation,
                    search: id,
                    metric: proto::Metric::Logical,
                    offset: 0,
                    limit: 10,
                })
                .unwrap();
            assert!(session.search_order.lock().unwrap().is_some());
            assert!(session.search_ranked.lock().unwrap().is_some());
        };
        let kept = || {
            session.search_order.lock().unwrap().is_some()
                || session.search_ranked.lock().unwrap().is_some()
        };
        fill(session.search_set("*.log").unwrap().unwrap().search);
        let txt = session.search_set("*.txt").unwrap().unwrap().search;
        assert!(!kept(), "replaced");
        fill(txt);
        assert!(session.search_set("").unwrap().is_none());
        assert!(!kept(), "cleared");
    }

    #[test]
    fn a_new_filter_replaces_the_one_being_caught_up() {
        let (session, b, c) = scan();
        let held = &session.filter.steps_left;
        held.store(0, Ordering::SeqCst);
        let old = session.search_set("*.log").unwrap().unwrap().search;
        b.release();
        until("b is listed", || listed(&session, "b"));
        // One step into b's 30,000 files, the job is held mid-way.
        held.store(1, Ordering::SeqCst);
        until("the job takes a step", || held.load(Ordering::SeqCst) == 0);
        let partial = session.search_summary(old).unwrap().files;
        assert!(
            100 < partial && partial < 3_100,
            "partly caught up: {partial}"
        );

        let new = session.search_set("*.txt").unwrap().unwrap().search;
        assert!(new > old);
        assert!(session.search_summary(old).is_err(), "no longer current");
        assert_eq!(
            session.details(0, Some(old)).unwrap().matching,
            None,
            "a replaced search gives no totals"
        );
        held.store(usize::MAX, Ordering::SeqCst);
        until("the job catches the new one up", || {
            totals(&session.search_summary(new).unwrap()) == fresh(&session, "*.txt")
        });
        c.release();
        until("the scan finishes", || finished(&session));
        assert_eq!(
            totals(&session.search_summary(new).unwrap()),
            fresh(&session, "*.txt")
        );
        assert_eq!(session.search_summary(new).unwrap().files, 27_200);
        assert!(session.search_summary(old).is_err());
        assert!(session.search_set("").unwrap().is_none());
        assert!(session.search_summary(new).is_err(), "cleared");
    }

    /// A scan of repository `a` (300 files) and `b` (30,000 files, more
    /// than a step), `b` listed once its gate is released; `d`, empty, and
    /// `c` listed once theirs are. Nothing is classified until a test
    /// records Git entries itself, as the scanner's probes would.
    fn repos() -> (Session, Gate, Gate, Gate) {
        let fs = FakeFs::new();
        for i in 0..300 {
            fs.file(&format!("a/src/{i}.rs"), 10 + i, 4096);
        }
        fs.file("a/.git/HEAD", 23, 4096);
        for i in 0..30_000 {
            fs.file(&format!("b/{i}.txt"), i, 4096);
        }
        fs.file("b/.git/HEAD", 23, 4096);
        fs.dir("d");
        fs.file("c/last.txt", 7, 4096);
        let b = fs.gate_dir("b");
        let c = fs.gate_dir("c");
        let d = fs.gate_dir("d");
        let config = ScanConfig {
            workers: 5,
            progress_interval: Duration::from_millis(1),
            ..ScanConfig::default()
        };
        let session = Session::start_on(fs.root(), fs, config, None, None, |_| {});
        until("a's files are in", || {
            let tree = session.scan.tree();
            let tree = tree.read().unwrap();
            tree.children(NodeId::ROOT).any(|a| {
                tree.name(a) == "a"
                    && tree
                        .children(a)
                        .all(|c| tree.dir_state(c) == DirState::Listed)
                    && tree.file_count(a) == 301
            })
        });
        (session, b, c, d)
    }

    /// Records `folder` (in the root) and its `.git` folder as a repository
    /// under the tree's write lock, as the scanner does with a probe's
    /// finding; the scan's next status carries it.
    fn found_repository(session: &Session, folder: &str) {
        use sb_core::git::{GitRoot, RootKind};
        let tree = session.scan.tree();
        let mut tree = tree.write().unwrap();
        let repo = tree
            .children(NodeId::ROOT)
            .find(|&c| tree.name(c) == folder)
            .unwrap();
        let dot = tree.children(repo).find(|&c| tree.name(c) == ".git");
        tree.set_git(GitRoot::new(repo, RootKind::Repository));
        if let Some(dot) = dot {
            tree.set_git(GitRoot::new(dot, RootKind::DotGit));
        }
    }

    #[test]
    fn repositories_found_while_scanning_reclassify_files_already_filtered() {
        let (session, b, c, d) = repos();
        let query = "git:repo; !git:metadata";
        let id = session.search_set(query).unwrap().unwrap().search;
        assert_eq!(session.search_summary(id).unwrap().files, 0);
        let stop = AtomicBool::new(false);
        std::thread::scope(|s| {
            let _stop = Stop(&stop);
            // Every step leaves every folder's totals the sum of its
            // contents', a rewalk's included.
            let reader = s.spawn(|| {
                let mut reads = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    let tree = session.scan.tree();
                    let (search, tree, _) = session.read_locked(&tree, true).unwrap();
                    let active = current(&search, id).unwrap();
                    let root = active.search.totals(NodeId::ROOT);
                    let (files, logical) = tree
                        .children(NodeId::ROOT)
                        .map(|c| active.search.totals(c))
                        .fold((0, 0), |(f, l), t| (f + t.files, l + t.logical));
                    assert_eq!((root.files, root.logical), (files, logical));
                    reads += 1;
                    drop((search, tree));
                    std::thread::yield_now();
                }
                reads
            });

            // a was examined before it was known to be a repository.
            found_repository(&session, "a");
            b.release();
            until("b is listed", || listed(&session, "b"));
            until("a's files join", || {
                totals(&session.search_summary(id).unwrap()) == fresh(&session, query)
            });
            assert_eq!(session.search_summary(id).unwrap().files, 300);

            // b, 30,000 files examined over several steps, is found to be a
            // repository; the next status (d listed, nothing new) carries it.
            found_repository(&session, "b");
            d.release();
            until("d is listed", || listed(&session, "d"));
            until("b's files join", || {
                totals(&session.search_summary(id).unwrap()) == fresh(&session, query)
            });
            assert_eq!(session.search_summary(id).unwrap().files, 30_300);
            assert!(!finished(&session), "caught up while scanning");

            c.release();
            until("the scan finishes", || finished(&session));
            stop.store(true, Ordering::Relaxed);
            assert!(reader.join().unwrap() > 0);
        });
        let last = session.search_summary(id).unwrap();
        assert_eq!(totals(&last), fresh(&session, query));
        assert_eq!(last.revision, session.scan.progress().revision);
    }

    #[test]
    fn says_when_the_filter_caught_up_after_only_git_data_changed() {
        let (session, b, _c, d) = repos();
        b.release();
        until("b is listed", || listed(&session, "b"));
        let (sink, updates) = std::sync::mpsc::channel();
        session.on_filter_updated(move |u| {
            let _ = sink.send(u);
        });
        let id = session.search_set("git:repo").unwrap().unwrap().search;
        let held = &session.filter.steps_left;
        held.store(0, Ordering::SeqCst);
        // b becomes a repository, and the status after it adds nothing
        // else: d is empty. Then the scan stalls on c.
        found_repository(&session, "b");
        d.release();
        until("the last status reaches the job", || {
            let p = session.scan.progress();
            listed(&session, "d")
                && p.pending_dirs == 1
                && session.filter.nudged.load(Ordering::SeqCst) == p.revision
        });
        let last = session.scan.progress().revision;
        let behind = session.search_summary(id).unwrap();
        assert!(behind.revision < last);
        assert_eq!(behind.files, 0);
        assert!(session.filter.read_behind.load(Ordering::SeqCst));
        held.store(usize::MAX, Ordering::SeqCst);
        let u = updates
            .recv_timeout(Duration::from_secs(10))
            .expect("an update is announced");
        assert_eq!(
            session.scan.progress().revision,
            last,
            "the scan stayed quiet"
        );
        assert_eq!((u.search, u.revision), (id, last));
        let now = session.search_summary(id).unwrap();
        assert_eq!((now.files, now.revision), (30_001, last));
        assert_eq!(totals(&now), fresh(&session, "git:repo"));
    }

    #[test]
    fn the_completion_index_is_rebuilt_in_the_background_while_scanning() {
        let (session, b, c) = scan();
        let txt = || {
            session
                .suggest(&proto::SuggestRequest {
                    generation: session.generation(),
                    kind: proto::SuggestKind::Extension,
                    typed: "tx".to_owned(),
                    limit: 10,
                })
                .unwrap()[0]
                .count
        };
        assert_eq!(txt(), 200, "built by the first request");
        b.release();
        until("b is listed", || listed(&session, "b"));
        assert_eq!(txt(), 200, "the earlier index answers meanwhile");
        // A rebuild stopped as the session goes leaves the index as it was
        // and can be started again.
        session.names_cancel.store(true, Ordering::SeqCst);
        session.refresh_names(session.scan.progress().revision);
        until("stopped", || !session.names_building.load(Ordering::SeqCst));
        assert_eq!(txt(), 200);
        session.names_cancel.store(false, Ordering::SeqCst);
        session.refresh_names(session.scan.progress().revision);
        until("rebuilt", || {
            !session.names_building.load(Ordering::SeqCst) && txt() == 27_200
        });
        c.release();
        until("the scan finishes", || finished(&session));
        assert_eq!(txt(), 27_200);
        let names = session.names.lock().unwrap();
        assert_eq!(
            names.as_ref().unwrap().revision,
            session.scan.progress().revision,
            "built again for the finished scan"
        );
    }
}
