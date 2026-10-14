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
//! by side. The job never waits for the search's lock: when a request is
//! reading, it backs off and tries again, so a request waits at most for one
//! step.
//!
//! While scanning, a request can see the search as of an earlier status
//! than the tree it reads, by a status interval at most; the search's
//! [`ActiveSearch::revision`] says which. Once the scan has finished, a
//! request first brings the search fully up to date (what the job hadn't
//! reached yet, a step or so), so final results are exactly what catching
//! up in the request gave.

use super::Session;
use crate::metrics::{self, Held, locked};
use sb_core::search::Search;
use sb_core::tree::Tree;
use sb_scan::Progress;
use std::sync::{Arc, Condvar, Mutex, OnceLock, RwLock, RwLockReadGuard, TryLockError, Weak};
use std::time::Duration;

/// Nodes the job examines per hold of the locks, about 0.4 ms (40 ns a
/// node): the longest a request should wait for it.
const CATCH_UP_STEP: usize = 10_000;

/// How long the job waits before trying again while requests read the search.
const BUSY_RETRY: Duration = Duration::from_millis(1);

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
    wake: Mutex<Wake>,
    woken: Condvar,
}

#[derive(Default)]
struct Wake {
    /// The latest scan revision to catch up with, while the job is due.
    due: Option<u64>,
    stop: bool,
}

/// What one step of the job did.
enum Step {
    /// The search is caught up, or there's none.
    Done,
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
                wake: Mutex::new(Wake::default()),
                woken: Condvar::new(),
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
        let mut wake = self.wake.lock().unwrap_or_else(|e| e.into_inner());
        wake.due = Some(wake.due.map_or(revision, |d| d.max(revision)));
        self.woken.notify_one();
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
        loop {
            let revision = {
                let mut wake = self.wake.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    if wake.stop {
                        return;
                    }
                    if let Some(revision) = wake.due.take() {
                        break revision;
                    }
                    wake = self.woken.wait(wake).unwrap_or_else(|e| e.into_inner());
                }
            };
            let tree = scan.get_or_insert_with(|| tree.wait().clone());
            loop {
                match self.step(tree, revision) {
                    Step::Done => break,
                    Step::Gone => return,
                    // Between steps the scanner can take the tree's lock:
                    // if it's waiting, the next read waits behind it.
                    Step::More => std::thread::yield_now(),
                    Step::Busy => std::thread::sleep(BUSY_RETRY),
                }
                if self.stopped() {
                    return;
                }
            }
        }
    }

    /// Examines up to [`CATCH_UP_STEP`] nodes. The tree is read-locked
    /// first, which may wait for the scanner, and the search only tried, so
    /// requests never wait while the job waits; that's also why taking them
    /// in the opposite order to requests can't deadlock.
    fn step(&self, tree: &Weak<RwLock<Tree>>, revision: u64) -> Step {
        let Some(tree) = tree.upgrade() else {
            return Step::Gone;
        };
        let Ok(tree) = tree.read() else {
            return Step::Gone;
        };
        let mut active = match self.active.try_write() {
            Ok(active) => active,
            Err(TryLockError::WouldBlock) => return Step::Busy,
            Err(TryLockError::Poisoned(_)) => return Step::Gone,
        };
        let Some(active) = active.as_mut() else {
            return Step::Done;
        };
        let _timing = metrics::command("search_catch_up");
        if metrics::enabled() {
            metrics::count("catch-up", active.search.backlog(&tree));
        }
        let _catch_up = metrics::span("catch-up");
        if active.search.catch_up(&tree, CATCH_UP_STEP) {
            active.revision = active.revision.max(revision);
            Step::Done
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
            let behind = (progress.state.is_finished() || !self.filter.background)
                && search
                    .as_deref()
                    .and_then(Option::as_ref)
                    .is_some_and(|a| a.revision != progress.revision);
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
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Instant;

    fn until(what: &str, done: impl Fn() -> bool) {
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

    /// Asked on another thread, so the test can hold the search's lock.
    fn summary_elsewhere(session: &Session, id: u32) -> proto::SearchSummary {
        std::thread::scope(|s| s.spawn(|| session.search_summary(id).unwrap()).join()).unwrap()
    }

    #[test]
    fn requests_read_the_filter_while_a_job_keeps_it_caught_up() {
        let (session, b, c) = scan();
        let id = session.search_set("*.log").unwrap().unwrap().search;
        // While a request reads the search, the job stays off it, and
        // other requests read it as it was, whole, saying which revision.
        let reading = session.filter.active.read().unwrap();
        let first = summary_elsewhere(&session, id);
        assert_eq!(totals(&first), fresh(&session, "*.log"));
        assert_eq!(first.files, 100);
        b.release();
        until("b is listed", || listed(&session, "b"));
        until("a newer status", || {
            session.scan.progress().revision > first.revision
        });
        assert_eq!(
            summary_elsewhere(&session, id),
            first,
            "requests don't catch up while scanning"
        );
        drop(reading);

        until("the job catches up", || {
            totals(&session.search_summary(id).unwrap()) == fresh(&session, "*.log")
        });
        assert!(!finished(&session), "caught up while scanning");
        let caught = session.search_summary(id).unwrap();
        assert_eq!(caught.files, 3_100);
        assert!(caught.revision > first.revision, "a newer revision");

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
        let reads = std::thread::scope(|s| {
            let readers: Vec<_> = (0..4)
                .map(|_| {
                    s.spawn(|| {
                        let (mut reads, mut revision) = (0, 0);
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
                            reads += 1;
                        }
                        reads
                    })
                })
                .collect();
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
    fn a_new_filter_replaces_the_one_being_caught_up() {
        let (session, b, c) = scan();
        let old = session.search_set("*.log").unwrap().unwrap().search;
        b.release();
        let new = session.search_set("*.txt").unwrap().unwrap().search;
        assert!(new > old);
        assert!(session.search_summary(old).is_err(), "no longer current");
        assert_eq!(
            session.details(0, Some(old)).unwrap().matching,
            None,
            "a replaced search gives no totals"
        );
        until("b is listed", || listed(&session, "b"));
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
