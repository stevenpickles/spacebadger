//! A bounded pool of threads for the query commands, apart from the async
//! runtime, the scanner, and the session's background jobs (search
//! catch-up, the completion index).
//!
//! `async` alone doesn't make a query cooperative: its synchronous work and
//! lock waits would hold a runtime worker, and unbounded `spawn_blocking`
//! would only queue more obsolete work. Here every query names a [`Class`],
//! and an idle worker always takes the oldest job of the most urgent class
//! waiting. One worker is kept for [`Class::Interactive`], so a tooltip never
//! waits behind layouts and lists however many are running, and at most one
//! [`Class::Background`] job runs at a time.
//!
//! A query may name a [`Lane`]; a newer request in its lane supersedes the
//! older ones. Waiting ones are turned away at once with
//! [`proto::SUPERSEDED`], and running ones are asked to stop: long work
//! calls [`check`] between steps and gives up with the same error. Each
//! class's queue is bounded too; when it's full the oldest waiting job is
//! turned away with [`QUEUE_FULL`]. The interface coalesces its requests,
//! so in practice the bound is never reached.

use sb_protocol::{self as proto, Lane};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::task::{Context, Poll};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tauri::async_runtime::{Receiver, channel};

use crate::metrics;

/// The error of a job turned away because its class's queue was full.
pub const QUEUE_FULL: &str = "too many requests are waiting; try again";

/// The error of a job turned away because the app is closing.
pub const SHUT_DOWN: &str = "the app is closing";

/// How urgent a query is, most urgent first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Class {
    /// Tooltips, details, the filter's totals, a selection's totals: small
    /// answers the interface waits on while the pointer moves.
    Interactive,
    /// What the map draws: layouts and their change marks.
    View,
    /// Pages of lists, which may rank a whole folder first.
    Lists,
    /// Optional extras such as the filter's completion, which may index the
    /// whole tree first.
    Background,
}

impl Class {
    const ALL: [Self; 4] = [Self::Interactive, Self::View, Self::Lists, Self::Background];

    /// Jobs kept waiting in this class; past this the oldest is turned away.
    /// The interface has a handful of each in flight at most.
    fn cap(self) -> usize {
        match self {
            Self::Interactive => 64,
            Self::View => 16,
            Self::Lists => 64,
            Self::Background => 8,
        }
    }
}

/// Threads in the pool: half the hardware threads, at least 3 (one kept
/// for interactive queries, two for the rest) and at most 6, since the
/// scanner and the session's jobs need the others.
pub fn default_threads() -> usize {
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    (cores / 2).clamp(3, 6)
}

/// A query waiting for a worker.
struct Job {
    name: &'static str,
    class: Class,
    lane: Option<Lane>,
    cancel: Arc<AtomicBool>,
    /// Runs the query and sends its outcome with `None`, or sends the error
    /// given instead without running it.
    task: Box<dyn FnOnce(Option<&'static str>) + Send>,
}

/// A query a worker is running.
struct Running {
    id: u64,
    class: Class,
    lane: Option<Lane>,
    cancel: Arc<AtomicBool>,
}

#[derive(Default)]
struct State {
    queues: [VecDeque<Job>; 4],
    running: Vec<Running>,
    next_id: u64,
    stopping: bool,
}

impl State {
    /// Whether any job of `lane`'s key, waiting or running, is newer.
    fn newer_than(&self, lane: &Lane) -> bool {
        let newer = |l: &Option<Lane>| {
            l.as_ref()
                .is_some_and(|l| l.key == lane.key && l.seq > lane.seq)
        };
        self.queues.iter().flatten().any(|j| newer(&j.lane))
            || self.running.iter().any(|r| newer(&r.lane))
    }

    /// Takes the jobs of `lane`'s key older than it out of the queues and
    /// asks the running ones to stop.
    fn supersede(&mut self, lane: &Lane, refused: &mut Vec<(Job, &'static str)>) {
        let older = |l: &Option<Lane>| {
            l.as_ref()
                .is_some_and(|l| l.key == lane.key && l.seq < lane.seq)
        };
        for queue in &mut self.queues {
            let mut i = 0;
            while i < queue.len() {
                if older(&queue[i].lane) {
                    let job = queue.remove(i).expect("in range");
                    refused.push((job, proto::SUPERSEDED));
                } else {
                    i += 1;
                }
            }
        }
        for r in &self.running {
            if older(&r.lane) {
                r.cancel.store(true, Ordering::Relaxed);
            }
        }
    }

    fn busy(&self, class: Class) -> usize {
        self.running.iter().filter(|r| r.class == class).count()
    }

    /// The next job a worker may start: the oldest of the most urgent class
    /// waiting, leaving one of `threads` for interactive queries and running
    /// one background job at a time.
    fn pick(&mut self, threads: usize) -> Option<Job> {
        let others = self.running.len() - self.busy(Class::Interactive);
        for class in Class::ALL {
            let allowed = match class {
                Class::Interactive => true,
                Class::Background => others + 1 < threads && self.busy(class) == 0,
                Class::View | Class::Lists => others + 1 < threads,
            };
            if allowed && let Some(job) = self.queues[class as usize].pop_front() {
                return Some(job);
            }
        }
        None
    }
}

struct Shared {
    state: Mutex<State>,
    /// Signalled when a job is queued, a worker frees up, or on shutdown.
    ready: Condvar,
    threads: usize,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        // Jobs run outside the lock and never panic inside it.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The query runner; dropping it shuts it down.
pub struct Runner {
    shared: Arc<Shared>,
    workers: Mutex<Vec<JoinHandle<()>>>,
}

impl Default for Runner {
    fn default() -> Self {
        Self::new(default_threads())
    }
}

impl Runner {
    /// A runner with `threads` workers (at least 2).
    pub fn new(threads: usize) -> Self {
        let threads = threads.max(2);
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            ready: Condvar::new(),
            threads,
        });
        let workers = (0..threads)
            .filter_map(|i| {
                let shared = Arc::clone(&shared);
                std::thread::Builder::new()
                    .name(format!("sb-query-{i}"))
                    .spawn(move || work(&shared))
                    .ok()
            })
            .collect();
        Self {
            shared,
            workers: Mutex::new(workers),
        }
    }

    /// Queues `query` as the request `name` (for the timings) and returns
    /// its outcome when done. A panic in it fails just this request.
    pub fn submit<R: Send + 'static>(
        &self,
        class: Class,
        name: &'static str,
        lane: Option<Lane>,
        query: impl FnOnce() -> Result<R, String> + Send + 'static,
    ) -> Pending<R> {
        self.queue(class, name, lane, move |asked| {
            let _timing = metrics::command_queued(name, asked);
            query()
        })
    }

    /// [`Runner::submit`], also returning the request's timings (`None`
    /// when they aren't kept), for the load test.
    #[cfg(test)]
    pub fn submit_timed<R: Send + 'static>(
        &self,
        class: Class,
        name: &'static str,
        lane: Option<Lane>,
        query: impl FnOnce() -> Result<R, String> + Send + 'static,
    ) -> Pending<(Result<R, String>, Option<metrics::Request>)> {
        self.queue(class, name, lane, move |asked| {
            let timing = metrics::command_queued(name, asked);
            let outcome = query();
            Ok((outcome, timing.finish()))
        })
    }

    /// Queues `run`, which is given the time it was asked for.
    fn queue<R: Send + 'static>(
        &self,
        class: Class,
        name: &'static str,
        lane: Option<Lane>,
        run: impl FnOnce(Instant) -> Result<R, String> + Send + 'static,
    ) -> Pending<R> {
        let (tx, rx) = channel(1);
        let asked = Instant::now();
        let task = move |refused: Option<&'static str>| {
            let outcome = match refused {
                Some(why) => Err(why.to_owned()),
                None => catch_unwind(AssertUnwindSafe(|| run(asked)))
                    .unwrap_or_else(|_| Err(format!("{name} failed unexpectedly"))),
            };
            // The command may have gone; nobody's waiting then.
            let _ = tx.try_send(outcome);
        };
        let job = Job {
            name,
            class,
            lane,
            cancel: Arc::new(AtomicBool::new(false)),
            task: Box::new(task),
        };
        let mut refused = Vec::new();
        {
            let mut state = self.shared.lock();
            if state.stopping {
                refused.push((job, SHUT_DOWN));
            } else if job.lane.as_ref().is_some_and(|l| state.newer_than(l)) {
                // Overtaken on the way here by a newer request of its lane.
                refused.push((job, proto::SUPERSEDED));
            } else {
                if let Some(lane) = &job.lane {
                    state.supersede(lane, &mut refused);
                }
                let queue = &mut state.queues[class as usize];
                if queue.len() >= class.cap()
                    && let Some(oldest) = queue.pop_front()
                {
                    refused.push((oldest, QUEUE_FULL));
                }
                queue.push_back(job);
                self.shared.ready.notify_all();
            }
        }
        refuse(refused);
        Pending { rx }
    }

    /// Turns away the waiting jobs, asks the running ones to stop, and
    /// waits up to `timeout` for the workers to finish. Workers still busy
    /// then are left to finish on their own (the process is ending anyway).
    /// Returns whether every worker was joined. Later jobs are turned away.
    pub fn shutdown(&self, timeout: Duration) -> bool {
        let mut refused = Vec::new();
        {
            let mut state = self.shared.lock();
            state.stopping = true;
            for queue in &mut state.queues {
                refused.extend(queue.drain(..).map(|j| (j, SHUT_DOWN)));
            }
            for r in &state.running {
                r.cancel.store(true, Ordering::Relaxed);
            }
            self.shared.ready.notify_all();
        }
        refuse(refused);
        let workers = std::mem::take(&mut *self.workers.lock().unwrap_or_else(|e| e.into_inner()));
        let deadline = Instant::now() + timeout;
        let mut joined = true;
        for worker in workers {
            while !worker.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            if worker.is_finished() {
                let _ = worker.join();
            } else {
                joined = false;
            }
        }
        joined
    }

    /// Jobs waiting and running, for tests.
    #[cfg(test)]
    fn load(&self) -> (usize, usize) {
        let state = self.shared.lock();
        (
            state.queues.iter().map(VecDeque::len).sum(),
            state.running.len(),
        )
    }
}

impl Drop for Runner {
    fn drop(&mut self) {
        self.shutdown(Duration::from_secs(1));
    }
}

/// Sends each job its error, outside the runner's lock.
fn refuse(jobs: Vec<(Job, &'static str)>) {
    for (job, why) in jobs {
        metrics::tally(
            job.name,
            if why == proto::SUPERSEDED {
                "superseded"
            } else {
                "refused"
            },
            1,
        );
        (job.task)(Some(why));
    }
}

thread_local! {
    /// The stop flag of the job this worker is running.
    static CANCEL: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) };
}

/// Whether the job running on this thread was superseded (or the app is
/// closing); always `false` outside the runner.
pub fn superseded() -> bool {
    CANCEL.with_borrow(|c| c.as_ref().is_some_and(|c| c.load(Ordering::Relaxed)))
}

/// Fails with [`proto::SUPERSEDED`] if the job running on this thread was
/// superseded; for long queries to call between steps.
pub fn check() -> Result<(), String> {
    if superseded() {
        Err(proto::SUPERSEDED.to_owned())
    } else {
        Ok(())
    }
}

/// Runs `f` on this thread as a job already superseded, for testing that
/// queries stop.
#[cfg(test)]
pub fn as_superseded<R>(f: impl FnOnce() -> R) -> R {
    CANCEL.with_borrow_mut(|c| *c = Some(Arc::new(AtomicBool::new(true))));
    let out = f();
    CANCEL.with_borrow_mut(|c| *c = None);
    out
}

/// A worker: runs jobs until shut down.
fn work(shared: &Shared) {
    loop {
        let (job, id) = {
            let mut state = shared.lock();
            loop {
                if state.stopping {
                    return;
                }
                if let Some(job) = state.pick(shared.threads) {
                    let id = state.next_id;
                    state.next_id += 1;
                    state.running.push(Running {
                        id,
                        class: job.class,
                        lane: job.lane.clone(),
                        cancel: Arc::clone(&job.cancel),
                    });
                    break (job, id);
                }
                state = shared.ready.wait(state).unwrap_or_else(|e| e.into_inner());
            }
        };
        CANCEL.with_borrow_mut(|c| *c = Some(Arc::clone(&job.cancel)));
        (job.task)(None);
        CANCEL.with_borrow_mut(|c| *c = None);
        let mut state = shared.lock();
        state.running.retain(|r| r.id != id);
        // A freed worker may let a waiting job of another class start.
        shared.ready.notify_all();
    }
}

/// The outcome of a submitted query, to `await`.
pub struct Pending<R> {
    rx: Receiver<Result<R, String>>,
}

impl<R> Pending<R> {
    /// Blocks until the outcome arrives. Not for async code; for tests.
    #[cfg(test)]
    pub fn wait(mut self) -> Result<R, String> {
        self.rx
            .blocking_recv()
            .unwrap_or_else(|| Err(SHUT_DOWN.to_owned()))
    }
}

impl<R> Future for Pending<R> {
    type Output = Result<R, String>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.get_mut()
            .rx
            .poll_recv(cx)
            .map(|r| r.unwrap_or_else(|| Err(SHUT_DOWN.to_owned())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn lane(key: &str, seq: u64) -> Option<Lane> {
        Some(Lane {
            key: key.to_owned(),
            seq,
        })
    }

    /// A job that signals when it starts and then waits to be let go, so
    /// tests can fill workers deterministically.
    struct Gate {
        started: mpsc::Receiver<()>,
        release: mpsc::Sender<()>,
    }

    fn gated(runner: &Runner, class: Class, lane: Option<Lane>) -> (Gate, Pending<u32>) {
        let (started_tx, started) = mpsc::channel();
        let (release, release_rx) = mpsc::channel::<()>();
        let pending = runner.submit(class, "gated", lane, move || {
            let _ = started_tx.send(());
            let _ = release_rx.recv();
            Ok(1)
        });
        (Gate { started, release }, pending)
    }

    fn started(gate: &Gate) {
        gate.started
            .recv_timeout(Duration::from_secs(5))
            .expect("the job should start");
    }

    /// Waits until the runner has `waiting` jobs queued and `running` running.
    fn until(runner: &Runner, waiting: usize, running: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while runner.load() != (waiting, running) {
            assert!(Instant::now() < deadline, "load is {:?}", runner.load());
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn runs_queries_and_passes_errors_on() {
        let runner = Runner::new(2);
        assert_eq!(
            runner.submit(Class::Lists, "t", None, || Ok(5)).wait(),
            Ok(5)
        );
        let failed: Result<u32, _> = runner
            .submit(Class::Lists, "t", None, || Err("no such folder".to_owned()))
            .wait();
        assert_eq!(failed, Err("no such folder".to_owned()));
        let awaited =
            tauri::async_runtime::block_on(runner.submit(Class::View, "t", None, || Ok("x")));
        assert_eq!(awaited, Ok("x"));
    }

    #[test]
    fn a_panic_fails_only_its_request() {
        let runner = Runner::new(2);
        let panicked: Result<u32, _> = runner
            .submit(Class::Interactive, "boom", None, || panic!("bad node"))
            .wait();
        assert_eq!(panicked, Err("boom failed unexpectedly".to_owned()));
        // Both workers are still there: two jobs can run at once.
        let (a, pa) = gated(&runner, Class::Interactive, None);
        let (b, pb) = gated(&runner, Class::Interactive, None);
        started(&a);
        started(&b);
        a.release.send(()).unwrap();
        b.release.send(()).unwrap();
        assert_eq!((pa.wait(), pb.wait()), (Ok(1), Ok(1)));
    }

    #[test]
    fn the_most_urgent_class_goes_first() {
        let runner = Runner::new(2);
        // One worker busy with a list; the other is kept for interactive
        // queries, so everything else waits for the first.
        let (busy, first) = gated(&runner, Class::Lists, None);
        started(&busy);
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut pending = Vec::new();
        for (class, name) in [
            (Class::Background, "background"),
            (Class::Lists, "list"),
            (Class::View, "layout"),
        ] {
            let order = Arc::clone(&order);
            pending.push(runner.submit(class, name, None, move || {
                order.lock().unwrap().push(name);
                Ok(0)
            }));
        }
        until(&runner, 3, 1);
        // An interactive query doesn't wait behind any of them.
        let tooltip = runner.submit(Class::Interactive, "tooltip", None, || Ok(7));
        assert_eq!(tooltip.wait(), Ok(7));
        assert!(order.lock().unwrap().is_empty());
        busy.release.send(()).unwrap();
        assert_eq!(first.wait(), Ok(1));
        for p in pending {
            assert_eq!(p.wait(), Ok(0));
        }
        assert_eq!(*order.lock().unwrap(), ["layout", "list", "background"]);
    }

    #[test]
    fn interactive_queries_never_wait_behind_others() {
        let runner = Runner::new(3);
        let (a, pa) = gated(&runner, Class::View, None);
        let (b, pb) = gated(&runner, Class::Lists, None);
        started(&a);
        started(&b);
        // Every worker but the one kept is busy: more lists wait...
        let list = runner.submit(Class::Lists, "list", None, || Ok(2));
        until(&runner, 1, 2);
        // ...while tooltips go straight through.
        for _ in 0..10 {
            let tip = runner.submit(Class::Interactive, "tooltip", None, || Ok(3));
            assert_eq!(tip.wait(), Ok(3));
        }
        a.release.send(()).unwrap();
        b.release.send(()).unwrap();
        assert_eq!((pa.wait(), pb.wait(), list.wait()), (Ok(1), Ok(1), Ok(2)));
    }

    #[test]
    fn one_background_job_at_a_time() {
        let runner = Runner::new(4);
        let (a, pa) = gated(&runner, Class::Background, None);
        started(&a);
        let second = runner.submit(Class::Background, "bg", None, || Ok(2));
        until(&runner, 1, 1);
        a.release.send(()).unwrap();
        assert_eq!((pa.wait(), second.wait()), (Ok(1), Ok(2)));
    }

    #[test]
    fn a_newer_request_supersedes_waiting_ones() {
        let runner = Runner::new(2);
        let (busy, first) = gated(&runner, Class::View, None);
        started(&busy);
        let old = runner.submit(Class::View, "layout", lane("map", 1), || Ok(1));
        let older_page = runner.submit(Class::Lists, "page", lane("map", 1), || Ok(1));
        let other = runner.submit(Class::View, "layout", lane("other", 1), || Ok(1));
        // Same request number: pages of one list don't supersede each other.
        let page_a = runner.submit(Class::Lists, "page", lane("list", 4), || Ok(4));
        let page_b = runner.submit(Class::Lists, "page", lane("list", 4), || Ok(4));
        let new = runner.submit(Class::View, "layout", lane("map", 2), || Ok(2));
        assert_eq!(old.wait(), Err(proto::SUPERSEDED.to_owned()));
        assert_eq!(older_page.wait(), Err(proto::SUPERSEDED.to_owned()));
        // One that arrives after a newer one of its lane is turned away too.
        let late = runner.submit(Class::View, "layout", lane("map", 1), || Ok(1));
        assert_eq!(late.wait(), Err(proto::SUPERSEDED.to_owned()));
        busy.release.send(()).unwrap();
        assert_eq!(first.wait(), Ok(1));
        assert_eq!(new.wait(), Ok(2));
        assert_eq!(other.wait(), Ok(1));
        assert_eq!((page_a.wait(), page_b.wait()), (Ok(4), Ok(4)));
    }

    #[test]
    fn a_newer_request_stops_a_running_one_that_checks() {
        let runner = Runner::new(2);
        let (started_tx, started_rx) = mpsc::channel();
        let running = runner.submit(Class::View, "layout", lane("map", 1), move || {
            let _ = started_tx.send(());
            let deadline = Instant::now() + Duration::from_secs(5);
            // A walk in steps, checking between them.
            while Instant::now() < deadline {
                check()?;
                std::thread::sleep(Duration::from_millis(1));
            }
            Ok(1)
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let newer = runner.submit(Class::View, "layout", lane("map", 2), || Ok(2));
        assert_eq!(running.wait(), Err(proto::SUPERSEDED.to_owned()));
        assert_eq!(newer.wait(), Ok(2));
        // The flag belonged to that job; the worker's next one runs freely.
        assert_eq!(
            runner
                .submit(Class::View, "t", None, || Ok(superseded()))
                .wait(),
            Ok(false)
        );
        assert!(!superseded(), "outside the runner nothing is superseded");
    }

    #[test]
    fn a_full_queue_turns_away_its_oldest() {
        let runner = Runner::new(2);
        let (busy, first) = gated(&runner, Class::View, None);
        started(&busy);
        let cap = Class::View.cap();
        let waiting: Vec<_> = (0..cap)
            .map(|i| runner.submit(Class::View, "layout", None, move || Ok(i)))
            .collect();
        let extra = runner.submit(Class::View, "layout", None, move || Ok(cap));
        until(&runner, cap, 1);
        // Other classes have their own room.
        assert_eq!(
            runner
                .submit(Class::Interactive, "t", None, || Ok(0))
                .wait(),
            Ok(0)
        );
        busy.release.send(()).unwrap();
        assert_eq!(first.wait(), Ok(1));
        let outcomes: Vec<_> = waiting.into_iter().map(Pending::wait).collect();
        assert_eq!(outcomes[0], Err(QUEUE_FULL.to_owned()));
        assert!(
            outcomes[1..]
                .iter()
                .enumerate()
                .all(|(i, o)| *o == Ok(i + 1))
        );
        assert_eq!(extra.wait(), Ok(cap));
    }

    #[test]
    fn shutdown_turns_away_waiting_jobs_and_joins_the_workers() {
        let runner = Runner::new(2);
        let (started_tx, started_rx) = mpsc::channel();
        // A running job asked to stop by the shutdown.
        let running = runner.submit(Class::View, "layout", None, move || {
            let _ = started_tx.send(());
            while !superseded() {
                std::thread::sleep(Duration::from_millis(1));
            }
            check().map(|()| 1)
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let waiting = runner.submit(Class::View, "layout", None, || Ok(2));
        until(&runner, 1, 1);
        assert!(runner.shutdown(Duration::from_secs(5)));
        assert_eq!(waiting.wait(), Err(SHUT_DOWN.to_owned()));
        assert_eq!(running.wait(), Err(proto::SUPERSEDED.to_owned()));
        let after = runner.submit(Class::Interactive, "t", None, || Ok(3));
        assert_eq!(after.wait(), Err(SHUT_DOWN.to_owned()));
    }

    #[test]
    fn shutdown_gives_up_on_a_stuck_job_after_its_timeout() {
        let runner = Runner::new(2);
        let (gate, pending) = gated(&runner, Class::Lists, None);
        started(&gate);
        let asked = Instant::now();
        assert!(!runner.shutdown(Duration::from_millis(50)));
        assert!(asked.elapsed() < Duration::from_secs(2));
        // Let it finish so the thread ends with the test.
        gate.release.send(()).unwrap();
        assert_eq!(pending.wait(), Ok(1));
        // Dropping after a shutdown doesn't wait again or hang.
        drop(runner);
    }
}
