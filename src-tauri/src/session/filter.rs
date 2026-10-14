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

/// Nodes the job examines per hold of the locks, about 1 ms: the longest a
/// request waits for it.
const CATCH_UP_STEP: usize = 25_000;

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
