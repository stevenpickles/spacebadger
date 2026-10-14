//! Interaction latency while scanning: a synthetic scan runs while the
//! queries the interface makes are issued at its rates, through the query
//! runner the commands use (each in its class and lane), and each request's
//! latency is split into queue, lock waits and holds, search catch-up,
//! ranking, layout, and encoding (see [`crate::metrics`]). Ignored by
//! default; run with
//!
//! ```text
//! cargo test -p spacebadger --release --lib interaction_latency -- --ignored --nocapture
//! ```
//!
//! The tree's shape and the scan's pace can be changed with `SB_LOAD_DEPTH`
//! (folder levels, 8), `SB_LOAD_FAN` (subfolders 2 to 1 + this, 6),
//! `SB_LOAD_FILES` (mean files per folder, 25), `SB_LOAD_WIDE` (files
//! directly in the root, which the Contents list shows, 20,000),
//! `SB_LOAD_DELAY_US` (per listing, 400), and `SB_LOAD_QUERY` (`*.log`).
//! `SB_LOAD_RUNNER=0` issues them as before the runner: inline on the async
//! runtime's workers, or on its blocking threads.
//! Results are recorded in `docs/validation/interaction-latency.md`.

use super::Session;
use crate::metrics::{self, Kind, Request};
use crate::runner::{Class, Runner};
use sb_protocol as proto;
use sb_scan::{
    Entry, EntryKind, FsAdapter, Identity, Measured, RootKind, ScanConfig, ScanError, Sink, batched,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

const ROOT: &str = "/load";

/// The interface refreshes on every scan status, sent every 150 ms.
const STATUS_EVERY: Duration = Duration::from_millis(150);

/// A tooltip is asked for about this often while the pointer moves.
const HOVER_EVERY: Duration = Duration::from_millis(50);

/// A pointer sweeping across small tiles asks for a tooltip this often.
const SWEEP_HOVER_EVERY: Duration = Duration::from_millis(5);

/// A map being resized asks for a layout this often: faster than a layout
/// takes, as several resize events a frame or a slower machine make it.
const SWEEP_RESIZE_EVERY: Duration = Duration::from_millis(4);

/// How long to go on after the scan finishes, for the settled numbers.
const AFTER: Duration = Duration::from_secs(3);

fn env(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.replace('_', "").parse().ok())
        .unwrap_or(default)
}

/// xorshift64*, seeded per folder so every run lists the same tree.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }

    fn unit(&mut self) -> f64 {
        ((self.next() >> 11) + 1) as f64 / (1u64 << 53) as f64
    }
}

fn fnv(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
    })
}

const SYLLABLES: &[&str] = &[
    "ba", "ko", "ri", "net", "ser", "data", "img", "log", "app", "lib", "core", "min", "doc",
    "test", "cache", "temp", "user", "proj", "src", "build", "re", "lo", "ta", "vi",
];

/// Extensions with their weights; `log` is about one file in twenty.
const EXTENSIONS: &[(&str, u64)] = &[
    ("dll", 6),
    ("txt", 6),
    ("json", 5),
    ("js", 8),
    ("png", 8),
    ("jpg", 6),
    ("log", 3),
    ("dat", 4),
    ("mp4", 1),
    ("zip", 1),
    ("tmp", 2),
    ("h", 4),
    ("py", 4),
];

/// A deterministic tree listed slowly enough that the scan lasts a while.
struct SynthFs {
    depth: usize,
    fan: u64,
    files: u64,
    wide: u64,
    delay: Duration,
}

impl SynthFs {
    fn from_env() -> Self {
        Self {
            depth: env("SB_LOAD_DEPTH", 8) as usize,
            fan: env("SB_LOAD_FAN", 6),
            files: env("SB_LOAD_FILES", 25),
            wide: env("SB_LOAD_WIDE", 20_000),
            delay: Duration::from_micros(env("SB_LOAD_DELAY_US", 400)),
        }
    }

    fn name(rng: &mut Rng, i: u64) -> String {
        let a = SYLLABLES[rng.below(SYLLABLES.len() as u64) as usize];
        let b = SYLLABLES[rng.below(SYLLABLES.len() as u64) as usize];
        format!("{a}{b}_{i}")
    }

    fn extension(rng: &mut Rng) -> &'static str {
        let total: u64 = EXTENSIONS.iter().map(|e| e.1).sum();
        let mut pick = rng.below(total);
        for &(ext, weight) in EXTENSIONS {
            if pick < weight {
                return ext;
            }
            pick -= weight;
        }
        "bin"
    }
}

impl FsAdapter for SynthFs {
    fn open_root(&self, _root: &Path) -> Result<RootKind, ScanError> {
        Ok(RootKind::Directory)
    }

    fn read_dir(&self, dir: &Path, sink: &mut Sink<'_>) -> Result<(), ScanError> {
        std::thread::sleep(self.delay);
        let depth = dir.strip_prefix(ROOT).map_or(0, |p| p.components().count());
        let seed = fnv(&dir.to_string_lossy());
        let mut rng = Rng::new(seed);
        let (files, dirs) = if depth == 0 {
            (self.wide, self.fan)
        } else {
            // Mostly small folders and a few large ones, averaging `files`.
            let files = (self.files as f64 * 0.3 / rng.unit().powf(0.7)) as u64;
            let dirs = if depth < self.depth {
                2 + rng.below(self.fan - 1)
            } else {
                0
            };
            (files.min(20_000), dirs)
        };
        let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        batched(sink, |out| {
            for i in 0..dirs {
                let entry = Entry {
                    name: Self::name(&mut rng, i).into(),
                    kind: EntryKind::Directory,
                    logical: 0,
                    allocated: None,
                    identity: Identity::Unique,
                    id: u128::from(seed ^ i),
                    cloud: false,
                    modified: Some(modified),
                };
                if out.push(entry).is_break() {
                    return Ok(());
                }
            }
            for i in 0..files {
                let ext = Self::extension(&mut rng);
                // Heavy-tailed sizes from 512 bytes up to 50 GB.
                let logical = (512.0 / rng.unit().powf(1.0 / 1.1)).min(5e10) as u64;
                let entry = Entry {
                    name: format!("{}.{ext}", Self::name(&mut rng, i)).into(),
                    kind: EntryKind::File,
                    logical,
                    allocated: Some(logical.div_ceil(4096) * 4096),
                    identity: Identity::Unique,
                    id: u128::from(seed ^ (i << 20)),
                    cloud: false,
                    modified: Some(modified),
                };
                if out.push(entry).is_break() {
                    return Ok(());
                }
            }
            Ok(())
        })
    }

    fn measure_file(&self, _file: &Path) -> Result<Measured, ScanError> {
        Ok(Measured {
            logical: 0,
            allocated: Some(0),
        })
    }
}

/// What the Contents list shows for the viewed folder.
#[derive(Clone, Copy, PartialEq)]
enum Contents {
    /// "Folders" checked: the folder's own items (`small_items`).
    Items,
    /// Unchecked: the files anywhere under it (`folder_files`).
    Files,
}

/// When the filter is typed.
#[derive(Clone, Copy, PartialEq)]
enum Filter {
    Off,
    /// As the scan starts, so requests catch it up as the scan goes.
    AtStart,
    /// This long into the scan, so it starts with a large tree to examine.
    After(Duration),
}

struct Scenario {
    name: &'static str,
    filter: Filter,
    contents: Contents,
    /// Adds a pointer sweeping across tiles and a map being resized, whose
    /// requests supersede each other.
    sweep: bool,
}

/// One request as the harness saw it.
struct Sample {
    command: &'static str,
    /// From asking to the reply, as the interface would wait (without IPC).
    latency: Duration,
    scanning: bool,
    /// Turned away or stopped because a newer request of its lane came.
    superseded: bool,
    request: Option<Request>,
}

/// How a kind of request is made, as the app makes it.
#[derive(Clone, Copy)]
struct How {
    /// Its class on the query runner.
    class: Class,
    /// Without the runner (`SB_LOAD_RUNNER=0`, as before it): on a blocking
    /// thread rather than a runtime worker.
    blocking: bool,
    /// Asked for again before the reply, each time superseding the one
    /// before (a pointer sweeping across tiles, the map resized or
    /// navigated quickly); otherwise each waits for the previous reply, like
    /// the interface's coalescing.
    overlap: bool,
}

type Work = Arc<dyn Fn(&Session, &mut Rng) -> Result<(), String> + Send + Sync>;

/// Runs `work` as a request asked for now, numbered `seq` in the lane named
/// after `command`: on `runner` like the app, or without one on a runtime
/// worker (the `async` commands before the runner) or a blocking thread.
/// Returns the latency, the request's breakdown, and whether it was
/// superseded.
fn ask(
    runner: Option<&Runner>,
    command: &'static str,
    how: How,
    seq: u64,
    work: impl FnOnce() -> Result<(), String> + Send + 'static,
) -> (Duration, Option<Request>, bool) {
    let asked = Instant::now();
    let (outcome, request) = match runner {
        Some(runner) => {
            let lane = proto::Lane {
                key: command.to_owned(),
                seq,
            };
            runner
                .submit_timed(how.class, command, Some(lane), work)
                .wait()
                .unwrap_or_else(|e| (Err(e), None))
        }
        None => {
            let run = move || {
                let timing = metrics::command_queued(command, asked);
                let outcome = work();
                (outcome, timing.finish())
            };
            if how.blocking {
                tauri::async_runtime::block_on(tauri::async_runtime::spawn_blocking(run))
            } else {
                tauri::async_runtime::block_on(tauri::async_runtime::spawn(async move { run() }))
            }
            .expect("request panicked")
        }
    };
    let superseded = outcome.as_ref().is_err_and(|e| e == proto::SUPERSEDED);
    (asked.elapsed(), request, superseded)
}

/// Asks for `work` every `every` until `stop`, as `how` says.
#[allow(clippy::too_many_arguments)]
fn stream(
    session: Arc<Session>,
    runner: Option<Arc<Runner>>,
    stop: Arc<AtomicBool>,
    samples: Arc<Mutex<Vec<Sample>>>,
    command: &'static str,
    every: Duration,
    how: How,
    work: Work,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut rng = Rng::new(fnv(command));
        let mut next = Instant::now();
        let mut seq = 0;
        let mut overlapping = Vec::new();
        while !stop.load(Ordering::Relaxed) {
            let scanning = !session.scan.progress().state.is_finished();
            let (s, w, r, seed) = (
                Arc::clone(&session),
                Arc::clone(&work),
                runner.clone(),
                rng.next(),
            );
            let samples_to = Arc::clone(&samples);
            if how.overlap {
                seq += 1;
            }
            let one = move || {
                let (latency, request, superseded) =
                    ask(r.as_deref(), command, how, seq, move || {
                        w(&s, &mut Rng::new(seed))
                    });
                samples_to.lock().unwrap().push(Sample {
                    command,
                    latency,
                    scanning,
                    superseded,
                    request,
                });
            };
            if how.overlap {
                overlapping.push(std::thread::spawn(one));
                overlapping.retain(|t: &std::thread::JoinHandle<()>| !t.is_finished());
            } else {
                one();
            }
            next += every;
            let now = Instant::now();
            if next > now {
                std::thread::sleep(next - now);
            } else {
                next = now;
            }
        }
        for t in overlapping {
            t.join().unwrap();
        }
    })
}

/// A session scanning a synthetic tree of `depth` folder levels at full
/// speed, for tests that need more than a handful of files.
pub(super) fn synthetic(depth: usize, wide: u64) -> Session {
    let fs = SynthFs {
        depth,
        fan: 4,
        files: 25,
        wide,
        delay: Duration::ZERO,
    };
    Session::start_on(
        PathBuf::from(ROOT),
        fs,
        ScanConfig::default(),
        None,
        None,
        |_| {},
    )
}

/// Whether requests go through the query runner, as in the app; set
/// `SB_LOAD_RUNNER=0` for how they ran before it.
fn via_runner() -> bool {
    env("SB_LOAD_RUNNER", 1) != 0
}

fn run(scenario: &Scenario) -> (Vec<Sample>, Duration, usize) {
    let fs = SynthFs::from_env();
    let query = std::env::var("SB_LOAD_QUERY").unwrap_or_else(|_| "*.log".to_owned());
    metrics::reset();
    let runner = via_runner().then(|| Arc::new(Runner::default()));
    let started = Instant::now();
    let session = Arc::new(Session::start_on(
        PathBuf::from(ROOT),
        fs,
        ScanConfig::default(),
        None,
        None,
        |_| {},
    ));
    let generation = session.generation();
    let stop = Arc::new(AtomicBool::new(false));
    let samples = Arc::new(Mutex::new(Vec::new()));
    // The current search, or 0 for none, as the interface knows it.
    let search_id = Arc::new(AtomicU32::new(0));
    let apply = |query: String| {
        let s = Arc::clone(&session);
        let scanning = !s.scan.progress().state.is_finished();
        let found = Arc::new(AtomicU32::new(0));
        let into = Arc::clone(&found);
        let how = How {
            class: Class::View,
            blocking: true,
            overlap: false,
        };
        let (latency, request, superseded) =
            ask(runner.as_deref(), "search_set", how, 0, move || {
                if let Some(summary) = s.search_set(&query)? {
                    into.store(summary.search, Ordering::Relaxed);
                }
                Ok(())
            });
        samples.lock().unwrap().push(Sample {
            command: "search_set",
            latency,
            scanning,
            superseded,
            request,
        });
        search_id.store(found.load(Ordering::Relaxed), Ordering::Relaxed);
    };
    if scenario.filter == Filter::AtStart {
        apply(query.clone());
    }
    let current = |id: &AtomicU32| Some(id.load(Ordering::Relaxed)).filter(|&i| i != 0);
    let spawn = |command, every, how, work| {
        stream(
            Arc::clone(&session),
            runner.clone(),
            Arc::clone(&stop),
            Arc::clone(&samples),
            command,
            every,
            how,
            work,
        )
    };
    let waits = |class, blocking| How {
        class,
        blocking,
        overlap: false,
    };
    let overlaps = |class| How {
        class,
        blocking: false,
        overlap: true,
    };
    let size = |s: &Session| {
        let p = s.scan.progress();
        (p.files + p.dirs + 1).max(1)
    };
    let id = || Arc::clone(&search_id);
    let layout = move |id: Arc<AtomicU32>, width: f32| {
        move |s: &Session, rng: &mut Rng| {
            s.layout(&proto::LayoutRequest {
                generation,
                request: 0,
                view: 0,
                // A resize drag: a few pixels narrower or wider each time.
                width: width - rng.below(64) as f32,
                height: 1000.0,
                metric: proto::Metric::Logical,
                search: current(&id),
            })
            .map(drop)
        }
    };
    let mut threads = vec![
        spawn(
            "layout",
            STATUS_EVERY,
            waits(Class::View, false),
            Arc::new(layout(id(), 1600.0)) as Work,
        ),
        spawn(
            "search_summary",
            STATUS_EVERY,
            waits(Class::Interactive, false),
            Arc::new({
                let id = id();
                move |s: &Session, _: &mut Rng| match current(&id) {
                    Some(search) => s.search_summary(search).map(drop),
                    None => Ok(()),
                }
            }),
        ),
        // The details of the selected folder, refreshed with each status.
        spawn(
            "node_details",
            STATUS_EVERY,
            waits(Class::Interactive, false),
            Arc::new({
                let id = id();
                move |s: &Session, _: &mut Rng| s.details(1, current(&id)).map(drop)
            }),
        ),
        // Tooltips for whatever the pointer is over.
        spawn(
            "tooltip",
            HOVER_EVERY,
            waits(Class::Interactive, false),
            Arc::new(move |s: &Session, rng: &mut Rng| {
                s.details(rng.below(size(s)) as u32, None).map(drop)
            }),
        ),
    ];
    threads.push(match scenario.contents {
        Contents::Items => spawn(
            "small_items",
            STATUS_EVERY,
            waits(Class::Lists, false),
            Arc::new({
                let id = id();
                move |s: &Session, _: &mut Rng| {
                    s.small_items(&proto::SmallItemsRequest {
                        generation,
                        folder: 0,
                        metric: proto::Metric::Logical,
                        search: current(&id),
                        count: u32::MAX,
                        offset: 0,
                        limit: 200,
                    })
                    .map(drop)
                }
            }),
        ),
        Contents::Files => spawn(
            "folder_files",
            STATUS_EVERY,
            waits(Class::Lists, true),
            Arc::new({
                let id = id();
                move |s: &Session, _: &mut Rng| {
                    s.folder_files(&proto::FolderFilesRequest {
                        generation,
                        folder: 0,
                        metric: proto::Metric::Logical,
                        search: current(&id),
                        offset: 0,
                        limit: 200,
                    })
                    .map(drop)
                }
            }),
        ),
    });
    if scenario.sweep {
        // A pointer sweeping across small tiles: a tooltip for each, the
        // newest superseding those not answered yet.
        threads.push(spawn(
            "hover",
            SWEEP_HOVER_EVERY,
            overlaps(Class::Interactive),
            Arc::new(move |s: &Session, rng: &mut Rng| {
                s.details(rng.below(size(s)) as u32, None).map(drop)
            }),
        ));
        // A second map being resized: each size supersedes the last.
        threads.push(spawn(
            "resize",
            SWEEP_RESIZE_EVERY,
            overlaps(Class::View),
            Arc::new(layout(id(), 1200.0)),
        ));
    }
    if let Filter::After(delay) = scenario.filter {
        std::thread::sleep(delay);
        apply(query);
    }
    while !session.scan.progress().state.is_finished() {
        std::thread::sleep(Duration::from_millis(20));
    }
    let scan_time = started.elapsed();
    std::thread::sleep(AFTER);
    stop.store(true, Ordering::Relaxed);
    for t in threads {
        t.join().unwrap();
    }
    if let Some(runner) = runner {
        assert!(runner.shutdown(Duration::from_secs(5)), "the runner hung");
    }
    let nodes = session.scan.tree().read().unwrap().len();
    let samples = std::mem::take(&mut *samples.lock().unwrap());
    (samples, scan_time, nodes)
}

fn ms(d: Duration) -> String {
    format!("{:.1}", d.as_secs_f64() * 1e3)
}

fn ms_ns(ns: f64) -> String {
    format!("{:.1}", ns / 1e6)
}

/// The value at quantile `q` of sorted `values`.
fn at<T: Copy>(sorted: &[T], q: f64) -> T {
    let i = ((q * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len()) - 1;
    sorted[i]
}

/// The parts a request's time is split into, in the order reported.
const PARTS: &[(&str, &str, Kind)] = &[
    ("queue", "", Kind::Queue),
    ("search wait", "search", Kind::Wait),
    ("tree wait", "tree", Kind::Wait),
    ("order wait", "order", Kind::Wait),
    ("rankings wait", "rankings", Kind::Wait),
    ("catch-up", "catch-up", Kind::Stage),
    ("compute", "compute", Kind::Stage),
    ("rank", "rank", Kind::Stage),
    ("encode", "encode", Kind::Stage),
    ("tree hold", "tree", Kind::Hold),
    ("search hold", "search", Kind::Hold),
];

fn part(r: &Request, name: &str, kind: Kind) -> u64 {
    match kind {
        Kind::Queue => r.queue.map_or(0, |q| q.as_nanos() as u64),
        _ => r.part(name, kind),
    }
}

/// Markdown tables: latency per command (of the requests answered; those
/// superseded are only counted), then the mean of each part over the
/// requests at or above the p95 and p99 latency.
fn report(name: &str, samples: &[Sample], scanning: bool) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let phase = if scanning { "while scanning" } else { "after" };
    let _ = writeln!(out, "\n#### {name}, {phase}\n");
    let _ = writeln!(
        out,
        "| request | n | superseded | p50 | p95 | p99 | max | tail | {} | backlog |",
        PARTS.iter().map(|p| p.0).collect::<Vec<_>>().join(" | ")
    );
    let _ = writeln!(out, "|{}", "---|".repeat(8 + PARTS.len() + 1));
    let mut commands: Vec<&str> = samples.iter().map(|s| s.command).collect();
    commands.sort();
    commands.dedup();
    for command in commands {
        let (superseded, mut mine): (Vec<&Sample>, Vec<&Sample>) = samples
            .iter()
            .filter(|s| s.command == command && s.scanning == scanning)
            .partition(|s| s.superseded);
        if mine.is_empty() {
            continue;
        }
        mine.sort_by_key(|s| s.latency);
        let lat: Vec<Duration> = mine.iter().map(|s| s.latency).collect();
        let (p50, p95, p99, max) = (
            at(&lat, 0.5),
            at(&lat, 0.95),
            at(&lat, 0.99),
            *lat.last().unwrap(),
        );
        for (label, floor) in [("≥p95", p95), ("≥p99", p99)] {
            let tail: Vec<&Request> = mine
                .iter()
                .filter(|s| s.latency >= floor)
                .filter_map(|s| s.request.as_ref())
                .collect();
            let mean = |f: &dyn Fn(&Request) -> u64| {
                tail.iter().map(|r| f(r) as f64).sum::<f64>() / tail.len().max(1) as f64
            };
            let parts: Vec<String> = PARTS
                .iter()
                .map(|&(_, n, k)| ms_ns(mean(&|r| part(r, n, k))))
                .collect();
            let backlog = mean(&|r| r.part("catch-up", Kind::Count));
            let head = if label == "≥p95" {
                format!(
                    "{command} | {} | {} | {} | {} | {} | {}",
                    mine.len(),
                    superseded.len(),
                    ms(p50),
                    ms(p95),
                    ms(p99),
                    ms(max)
                )
            } else {
                " | | | | | | ".to_owned()
            };
            let _ = writeln!(
                out,
                "| {head} | {label} | {} | {backlog:.0} |",
                parts.join(" | ")
            );
        }
    }
    out
}

#[test]
#[ignore = "takes about a minute; run in release, see the module docs"]
fn interaction_latency_while_scanning() {
    metrics::enable();
    let scenarios = [
        Scenario {
            name: "Filter on, Contents with folders",
            filter: Filter::AtStart,
            contents: Contents::Items,
            sweep: false,
        },
        Scenario {
            name: "Filter on, Contents with files only",
            filter: Filter::AtStart,
            contents: Contents::Files,
            sweep: false,
        },
        Scenario {
            name: "No filter, Contents with folders",
            filter: Filter::Off,
            contents: Contents::Items,
            sweep: false,
        },
        Scenario {
            name: "Filter typed 5 s into the scan, Contents with folders",
            filter: Filter::After(Duration::from_secs(5)),
            contents: Contents::Items,
            sweep: false,
        },
        Scenario {
            name: "Filter on, Contents with folders, pointer sweeping and a map resized",
            filter: Filter::AtStart,
            contents: Contents::Items,
            sweep: true,
        },
    ];
    let cores = std::thread::available_parallelism().map_or(0, |n| n.get());
    println!(
        "cores: {cores}, scan workers: {}, query runner: {}",
        ScanConfig::default().workers,
        if via_runner() {
            format!("{} threads", crate::runner::default_threads())
        } else {
            "off".to_owned()
        }
    );
    for scenario in &scenarios {
        let (samples, scan_time, nodes) = run(scenario);
        println!(
            "\n### {} ({nodes} nodes scanned in {})",
            scenario.name,
            ms(scan_time)
        );
        println!("{}", report(scenario.name, &samples, true));
        println!("{}", report(scenario.name, &samples, false));
        println!("\n```text\n{}```", metrics::report().unwrap_or_default());
        assert!(
            samples.iter().any(|s| s.scanning),
            "the scan ended before any request"
        );
    }
}
