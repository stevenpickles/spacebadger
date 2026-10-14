//! Opt-in timings of interactive requests, to tell apart what makes them
//! slow: waiting for a runtime thread, waiting for a lock, holding it, and
//! the work done under it (search catch-up, ranking, layout, encoding).
//!
//! Off unless `SPACEBADGER_TRACE` is set: to a file to write the report to
//! at exit, or to `1` for the standard error stream. Release builds on
//! Windows have no console (`windows_subsystem = "windows"`), so they need a
//! file. Empty or `0` is off. When off, every helper costs one atomic load
//! and the guards carry no timestamps. Timings are kept per command in log-linear
//! histograms (8 buckets per doubling, so percentiles are within 12.5%),
//! with the slowest requests of each command kept whole, so the report says
//! where a slow request spent its time.
//!
//! A request is a [`Command`] guard on the thread doing its work; locks
//! taken through [`locked`] and stages timed with [`span`] on that thread
//! count towards it. The session's methods are synchronous, so a request
//! never moves between threads while a guard is alive.

#[cfg(test)]
use std::cell::Cell;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// 0 until read from the environment, then 1 off or 2 on.
static STATE: AtomicU8 = AtomicU8::new(0);

static STORE: Mutex<Store> = Mutex::new(Store::new());

/// Slowest requests kept per command.
const SLOWEST: usize = 5;

/// Whether timings are being kept.
pub fn enabled() -> bool {
    #[cfg(test)]
    if ON_THIS_THREAD.with(Cell::get) {
        return true;
    }
    match STATE.load(Ordering::Relaxed) {
        2 => true,
        1 => false,
        _ => {
            let on =
                std::env::var_os("SPACEBADGER_TRACE").is_some_and(|v| !v.is_empty() && v != "0");
            STATE.store(if on { 2 } else { 1 }, Ordering::Relaxed);
            on
        }
    }
}

/// Keeps timings regardless of the environment, on every thread. Only for
/// the ignored load test, which runs alone and on runtime threads; other
/// tests use [`enable_on_this_thread`], so tests running in parallel keep
/// the off path.
#[cfg(test)]
pub fn enable() {
    STATE.store(2, Ordering::Relaxed);
}

#[cfg(test)]
thread_local! {
    static ON_THIS_THREAD: Cell<bool> = const { Cell::new(false) };
}

/// Keeps timings on the calling thread until the guard is dropped.
#[cfg(test)]
pub fn enable_on_this_thread() -> impl Drop {
    struct Off;
    impl Drop for Off {
        fn drop(&mut self) {
            ON_THIS_THREAD.with(|on| on.set(false));
        }
    }
    ON_THIS_THREAD.with(|on| on.set(true));
    Off
}

fn store() -> MutexGuard<'static, Store> {
    STORE.lock().unwrap_or_else(|e| e.into_inner())
}

/// What a time or count is of: a stage, or waiting for or holding a lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// From being asked for to starting, on the async runtime.
    Queue,
    /// The whole request on its thread, from start to finish.
    Work,
    /// One stage of the work.
    Stage,
    /// Waiting to take a lock.
    Wait,
    /// Holding a lock.
    Hold,
    /// A count, such as nodes a search had to catch up on; not a time.
    Count,
}

impl Kind {
    fn suffix(self) -> &'static str {
        match self {
            Self::Queue | Self::Work | Self::Stage | Self::Count => "",
            Self::Wait => " wait",
            Self::Hold => " hold",
        }
    }
}

/// One finished request: its times, with each stage summed.
#[derive(Debug, Clone)]
pub struct Request {
    pub command: &'static str,
    pub queue: Option<Duration>,
    pub work: Duration,
    /// Per stage or lock, the total time (or count) within this request.
    pub parts: Vec<(&'static str, Kind, u64)>,
}

impl Request {
    /// Total nanoseconds (or count) of one part; 0 if it never happened.
    #[cfg(test)]
    pub fn part(&self, name: &str, kind: Kind) -> u64 {
        self.parts
            .iter()
            .filter(|p| p.0 == name && p.1 == kind)
            .map(|p| p.2)
            .sum()
    }
}

/// The request being worked on by this thread.
struct Current {
    command: &'static str,
    parts: Vec<(&'static str, Kind, u64)>,
}

thread_local! {
    static CURRENT: RefCell<Vec<Current>> = const { RefCell::new(Vec::new()) };
}

/// Notes a time or count against the current request (and command), or
/// against `-` outside any.
pub fn record(name: &'static str, kind: Kind, value: u64) {
    if !enabled() {
        return;
    }
    let command = CURRENT.with_borrow_mut(|stack| match stack.last_mut() {
        Some(cur) => {
            match cur.parts.iter_mut().find(|p| p.0 == name && p.1 == kind) {
                Some(p) => p.2 += value,
                None => cur.parts.push((name, kind, value)),
            }
            cur.command
        }
        None => "-",
    });
    store().add(command, name, kind, value);
}

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// Notes a count, such as a backlog's size, against the current request.
pub fn count(name: &'static str, value: usize) {
    record(name, Kind::Count, value as u64);
}

/// A request being worked on; finishing or dropping it records it.
pub struct Command {
    started: Option<Instant>,
    queue: Option<Duration>,
}

/// Starts timing a request on this thread.
pub fn command(name: &'static str) -> Command {
    start(name, None)
}

/// Starts timing a request asked for at `asked`, recording the wait since.
pub fn command_queued(name: &'static str, asked: Instant) -> Command {
    start(name, Some(asked))
}

fn start(name: &'static str, asked: Option<Instant>) -> Command {
    if !enabled() {
        return Command {
            started: None,
            queue: None,
        };
    }
    let now = Instant::now();
    CURRENT.with_borrow_mut(|stack| {
        stack.push(Current {
            command: name,
            parts: Vec::new(),
        })
    });
    let queue = asked.map(|a| now.saturating_duration_since(a));
    if let Some(q) = queue {
        record(name, Kind::Queue, nanos(q));
    }
    Command {
        started: Some(now),
        queue,
    }
}

impl Command {
    /// Records the request and returns it; `None` when timings are off.
    #[cfg(test)]
    pub fn finish(mut self) -> Option<Request> {
        self.end()
    }

    fn end(&mut self) -> Option<Request> {
        let started = self.started.take()?;
        let work = started.elapsed();
        let cur = CURRENT.with_borrow_mut(Vec::pop)?;
        let request = Request {
            command: cur.command,
            queue: self.queue,
            work,
            parts: cur.parts,
        };
        let mut store = store();
        store.add(request.command, request.command, Kind::Work, nanos(work));
        store.finished(&request);
        Some(request)
    }
}

impl Drop for Command {
    fn drop(&mut self) {
        self.end();
    }
}

/// Runs a request's work on this thread, timed as `name`.
pub fn run<R>(name: &'static str, work: impl FnOnce() -> R) -> R {
    let _command = command(name);
    work()
}

/// Runs a request's work on a blocking thread, timed as `name`, with the
/// wait for a thread recorded as its queue time.
pub async fn blocking<R: Send + 'static>(
    name: &'static str,
    work: impl FnOnce() -> R + Send + 'static,
) -> Result<R, String> {
    let asked = enabled().then(Instant::now);
    tauri::async_runtime::spawn_blocking(move || {
        let _command = match asked {
            Some(at) => command_queued(name, at),
            None => command(name),
        };
        work()
    })
    .await
    .map_err(|e| e.to_string())
}

/// Times a stage of the current request until dropped.
pub struct Span {
    name: &'static str,
    started: Option<Instant>,
}

pub fn span(name: &'static str) -> Span {
    Span {
        name,
        started: enabled().then(Instant::now),
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        if let Some(t) = self.started {
            record(self.name, Kind::Stage, nanos(t.elapsed()));
        }
    }
}

/// A lock guard that records how long it was held when dropped. Derefs to
/// what the guard derefs to, so it stands in for the guard. The lock is
/// released before the time is recorded, so recording never extends it.
pub struct Held<G> {
    /// Always `Some` until dropped.
    guard: Option<G>,
    lock: &'static str,
    since: Option<Instant>,
}

/// Takes a lock through `take` (such as `|| mutex.lock()`), recording the
/// wait for it as `lock`, and later how long it's held.
pub fn locked<G, E>(lock: &'static str, take: impl FnOnce() -> Result<G, E>) -> Result<Held<G>, E> {
    if !enabled() {
        return take().map(|guard| Held {
            guard: Some(guard),
            lock,
            since: None,
        });
    }
    let asked = Instant::now();
    let guard = take()?;
    record(lock, Kind::Wait, nanos(asked.elapsed()));
    // The hold is counted from here, so it leaves out recording the wait.
    Ok(Held {
        guard: Some(guard),
        lock,
        since: Some(Instant::now()),
    })
}

impl<G: Deref> Deref for Held<G> {
    type Target = G::Target;

    fn deref(&self) -> &Self::Target {
        self.guard.as_ref().expect("held until dropped")
    }
}

impl<G: DerefMut> DerefMut for Held<G> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.guard.as_mut().expect("held until dropped")
    }
}

impl<G> Drop for Held<G> {
    fn drop(&mut self) {
        let held = self.since.map(|t| t.elapsed());
        drop(self.guard.take());
        if let Some(held) = held {
            record(self.lock, Kind::Hold, nanos(held));
        }
    }
}

/// Buckets 0–15 hold exact values, then 8 per doubling.
const BUCKETS: usize = 16 + 60 * 8;

fn bucket(v: u64) -> usize {
    if v < 16 {
        return v as usize;
    }
    let exp = 63 - v.leading_zeros() as usize;
    let sub = ((v >> (exp - 3)) & 7) as usize;
    16 + (exp - 4) * 8 + sub
}

/// The largest value that falls in bucket `i`.
fn bucket_top(i: usize) -> u64 {
    if i < 16 {
        return i as u64;
    }
    let exp = (i - 16) / 8 + 4;
    let sub = ((i - 16) % 8) as u64;
    let base = (8 + sub) << (exp - 3);
    base.saturating_add((1u64 << (exp - 3)) - 1)
}

#[derive(Clone)]
pub struct Histogram {
    counts: Box<[u64; BUCKETS]>,
    count: u64,
    sum: u128,
    max: u64,
}

impl Histogram {
    fn new() -> Self {
        Self {
            counts: Box::new([0; BUCKETS]),
            count: 0,
            sum: 0,
            max: 0,
        }
    }

    fn add(&mut self, v: u64) {
        self.counts[bucket(v)] += 1;
        self.count += 1;
        self.sum += u128::from(v);
        self.max = self.max.max(v);
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    /// The value below which a fraction `q` of the samples fall, rounded up
    /// to its bucket's top (never above the largest seen).
    pub fn quantile(&self, q: f64) -> u64 {
        if self.count == 0 {
            return 0;
        }
        let rank = ((q * self.count as f64).ceil() as u64).clamp(1, self.count);
        let mut seen = 0;
        for (i, &c) in self.counts.iter().enumerate() {
            seen += c;
            if seen >= rank {
                return bucket_top(i).min(self.max);
            }
        }
        self.max
    }

    pub fn max(&self) -> u64 {
        self.max
    }

    pub fn mean(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.sum as f64 / self.count as f64
        }
    }
}

type Key = (&'static str, &'static str, Kind);

struct Store {
    histograms: BTreeMap<Key, Histogram>,
    slowest: BTreeMap<&'static str, Vec<Request>>,
}

impl Store {
    const fn new() -> Self {
        Self {
            histograms: BTreeMap::new(),
            slowest: BTreeMap::new(),
        }
    }

    fn add(&mut self, command: &'static str, name: &'static str, kind: Kind, value: u64) {
        self.histograms
            .entry((command, name, kind))
            .or_insert_with(Histogram::new)
            .add(value);
    }

    fn finished(&mut self, request: &Request) {
        let kept = self.slowest.entry(request.command).or_default();
        if kept.len() < SLOWEST || kept.last().is_some_and(|r| r.work < request.work) {
            kept.push(request.clone());
            kept.sort_by_key(|r| std::cmp::Reverse(r.work));
            kept.truncate(SLOWEST);
        }
    }
}

/// Forgets every timing so far.
pub fn reset() {
    let mut store = store();
    store.histograms.clear();
    store.slowest.clear();
}

/// A copy of one histogram, if anything was recorded for it.
#[cfg(test)]
pub fn histogram(command: &'static str, name: &'static str, kind: Kind) -> Option<Histogram> {
    store().histograms.get(&(command, name, kind)).cloned()
}

fn ms(ns: u64) -> String {
    format!("{:.2}", ns as f64 / 1e6)
}

/// Every histogram as a table (times in ms), then the slowest requests of
/// each command with where their time went. `None` when timings are off.
pub fn report() -> Option<String> {
    if !enabled() {
        return None;
    }
    use std::fmt::Write;
    let store = store();
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:<18} {:<22} {:>8} {:>9} {:>9} {:>9} {:>9} {:>9}",
        "command", "part", "count", "mean", "p50", "p95", "p99", "max"
    );
    for (&(command, name, kind), h) in &store.histograms {
        let part = match kind {
            Kind::Queue => "queue".to_owned(),
            Kind::Work => "work".to_owned(),
            Kind::Count => format!("{name} (count)"),
            _ => format!("{name}{}", kind.suffix()),
        };
        let value = |v: u64| match kind {
            Kind::Count => v.to_string(),
            _ => ms(v),
        };
        let mean = match kind {
            Kind::Count => format!("{:.0}", h.mean()),
            _ => format!("{:.2}", h.mean() / 1e6),
        };
        let _ = writeln!(
            out,
            "{command:<18} {part:<22} {:>8} {mean:>9} {:>9} {:>9} {:>9} {:>9}",
            h.count(),
            value(h.quantile(0.5)),
            value(h.quantile(0.95)),
            value(h.quantile(0.99)),
            value(h.max()),
        );
    }
    let _ = writeln!(out, "\nslowest requests (ms):");
    for (command, kept) in &store.slowest {
        for r in kept {
            let _ = writeln!(out, "{command:<18} {}", describe(r));
        }
    }
    Some(out)
}

/// One request's time and its parts, such as
/// `work 12.30 · queue 0.01 · tree wait 8.20 · catch-up 3.10`.
pub fn describe(r: &Request) -> String {
    let mut text = format!("work {}", ms(nanos(r.work)));
    if let Some(q) = r.queue {
        text += &format!(" · queue {}", ms(nanos(q)));
    }
    for &(name, kind, v) in &r.parts {
        if kind == Kind::Count {
            text += &format!(" · {name} {v}");
        } else if kind != Kind::Queue {
            text += &format!(" · {name}{} {}", kind.suffix(), ms(v));
        }
    }
    text
}

/// Writes the report where `SPACEBADGER_TRACE` says: a file, or with `1`
/// the standard error stream (lost in Windows release builds, which have no
/// console).
pub fn write_report() {
    let Some(text) = report() else {
        return;
    };
    let target = std::env::var_os("SPACEBADGER_TRACE").unwrap_or_default();
    if target == "1" {
        eprintln!("{text}");
        return;
    }
    let path = std::path::PathBuf::from(target);
    if let Err(e) = std::fs::write(&path, text) {
        eprintln!(
            "Couldn't write the timing report to {}: {e}",
            path.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_cover_every_value_in_order() {
        let mut last = 0;
        for v in (0..5000).chain([u64::MAX / 2, u64::MAX]) {
            let b = bucket(v);
            assert!(b < BUCKETS);
            assert!(b >= last);
            assert!(bucket_top(b) >= v, "{v} above its bucket's top");
            last = b;
        }
        for i in 16..BUCKETS - 1 {
            assert_eq!(bucket(bucket_top(i)), i);
            assert_eq!(bucket(bucket_top(i) + 1), i + 1);
        }
    }

    #[test]
    fn quantiles_are_within_a_bucket() {
        let mut h = Histogram::new();
        for v in 1..=1000u64 {
            h.add(v * 1000);
        }
        let p50 = h.quantile(0.5) as f64;
        assert!((500_000.0..=500_000.0 * 1.125).contains(&p50), "{p50}");
        let p99 = h.quantile(0.99) as f64;
        assert!((990_000.0..=990_000.0 * 1.125).contains(&p99), "{p99}");
        assert_eq!(h.quantile(1.0), 1_000_000);
        assert_eq!(h.max(), 1_000_000);
    }

    #[test]
    fn requests_collect_their_locks_and_stages() {
        let _on = enable_on_this_thread();
        let lock = Mutex::new(5);
        let request = {
            let command = command("metrics-test");
            {
                let held = locked("number", || lock.lock()).unwrap();
                assert_eq!(*held, 5);
                let _stage = span("think");
                std::thread::sleep(Duration::from_millis(2));
            }
            count("backlog", 7);
            count("backlog", 3);
            command.finish().unwrap()
        };
        assert_eq!(request.command, "metrics-test");
        assert!(request.part("number", Kind::Hold) >= 2_000_000);
        assert!(request.part("think", Kind::Stage) >= 2_000_000);
        assert!(request.part("number", Kind::Wait) < request.part("number", Kind::Hold));
        assert_eq!(request.part("backlog", Kind::Count), 10);
        assert!(request.work >= Duration::from_millis(2));
        let h = histogram("metrics-test", "backlog", Kind::Count).unwrap();
        assert_eq!((h.count(), h.max()), (2, 7));
        assert!(report().unwrap().contains("metrics-test"));
    }
}
