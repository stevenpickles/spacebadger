//! Interaction latency while scanning: a synthetic scan runs while the
//! queries the interface makes are issued at its rates, through the async
//! runtime the commands use, and each request's latency is split into
//! queue, lock waits and holds, search catch-up, ranking, layout, and
//! encoding (see [`crate::metrics`]). Ignored by default; run with
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
//! Results are recorded in `docs/validation/interaction-latency.md`.

use super::Session;
use crate::metrics::{self, Kind, Request};
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
}

/// One request as the harness saw it.
struct Sample {
    command: &'static str,
    /// From asking to the reply, as the interface would wait (without IPC).
    latency: Duration,
    scanning: bool,
    request: Option<Request>,
}

/// Runs `work` as a request asked for now: on a runtime worker, like the
/// `async` commands, or on a blocking thread with `blocking`. Returns the
/// latency and the request's breakdown.
fn ask<R: Send + 'static>(
    command: &'static str,
    blocking: bool,
    work: impl FnOnce() -> R + Send + 'static,
) -> (Duration, Option<Request>) {
    let asked = Instant::now();
    let run = move || {
        let timing = metrics::command_queued(command, asked);
        let _ = work();
        timing.finish()
    };
    let request = if blocking {
        tauri::async_runtime::block_on(tauri::async_runtime::spawn_blocking(run))
    } else {
        tauri::async_runtime::block_on(tauri::async_runtime::spawn(async move { run() }))
    }
    .expect("request panicked");
    (asked.elapsed(), request)
}

/// Asks for `make()` every `every`, never with two in flight, like the
/// interface's coalescing, until `stop`.
fn stream(
    session: Arc<Session>,
    stop: Arc<AtomicBool>,
    samples: Arc<Mutex<Vec<Sample>>>,
    command: &'static str,
    every: Duration,
    blocking: bool,
    work: impl Fn(&Session, &mut Rng) + Send + Sync + 'static,
) -> std::thread::JoinHandle<()> {
    let work = Arc::new(work);
    std::thread::spawn(move || {
        let mut rng = Rng::new(fnv(command));
        let mut next = Instant::now();
        while !stop.load(Ordering::Relaxed) {
            let scanning = !session.scan.progress().state.is_finished();
            let (s, w, seed) = (Arc::clone(&session), Arc::clone(&work), rng.next());
            let (latency, request) = ask(command, blocking, move || {
                w(&s, &mut Rng::new(seed));
            });
            samples.lock().unwrap().push(Sample {
                command,
                latency,
                scanning,
                request,
            });
            next += every;
            let now = Instant::now();
            if next > now {
                std::thread::sleep(next - now);
            } else {
                next = now;
            }
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

fn run(scenario: &Scenario) -> (Vec<Sample>, Duration, usize) {
    let fs = SynthFs::from_env();
    let query = std::env::var("SB_LOAD_QUERY").unwrap_or_else(|_| "*.log".to_owned());
    metrics::reset();
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
        let (latency, request) = ask("search_set", true, move || {
            if let Ok(Some(summary)) = s.search_set(&query) {
                into.store(summary.search, Ordering::Relaxed);
            }
        });
        samples.lock().unwrap().push(Sample {
            command: "search_set",
            latency,
            scanning,
            request,
        });
        search_id.store(found.load(Ordering::Relaxed), Ordering::Relaxed);
    };
    if scenario.filter == Filter::AtStart {
        apply(query.clone());
    }
    let current = |id: &AtomicU32| Some(id.load(Ordering::Relaxed)).filter(|&i| i != 0);
    let spawn = |command, every, blocking, work| {
        stream(
            Arc::clone(&session),
            Arc::clone(&stop),
            Arc::clone(&samples),
            command,
            every,
            blocking,
            work,
        )
    };
    let size = |s: &Session| {
        let p = s.scan.progress();
        (p.files + p.dirs + 1).max(1)
    };
    let id = || Arc::clone(&search_id);
    let mut threads = vec![
        spawn(
            "layout",
            STATUS_EVERY,
            false,
            Box::new({
                let id = id();
                move |s: &Session, _: &mut Rng| {
                    let search = current(&id);
                    let _ = s.layout(&proto::LayoutRequest {
                        generation,
                        request: 0,
                        view: 0,
                        width: 1600.0,
                        height: 1000.0,
                        metric: proto::Metric::Logical,
                        search,
                    });
                }
            }) as Box<dyn Fn(&Session, &mut Rng) + Send + Sync>,
        ),
        spawn(
            "search_summary",
            STATUS_EVERY,
            false,
            Box::new({
                let id = id();
                move |s: &Session, _: &mut Rng| {
                    if let Some(search) = current(&id) {
                        let _ = s.search_summary(search);
                    }
                }
            }),
        ),
        // The details of the selected folder, refreshed with each status.
        spawn(
            "node_details",
            STATUS_EVERY,
            false,
            Box::new({
                let id = id();
                move |s: &Session, _: &mut Rng| {
                    let _ = s.details(1, current(&id));
                }
            }),
        ),
        // Tooltips for whatever the pointer is over.
        spawn(
            "tooltip",
            HOVER_EVERY,
            false,
            Box::new(move |s: &Session, rng: &mut Rng| {
                let _ = s.details(rng.below(size(s)) as u32, None);
            }),
        ),
    ];
    threads.push(match scenario.contents {
        Contents::Items => spawn(
            "small_items",
            STATUS_EVERY,
            false,
            Box::new({
                let id = id();
                move |s: &Session, _: &mut Rng| {
                    let _ = s.small_items(&proto::SmallItemsRequest {
                        generation,
                        folder: 0,
                        metric: proto::Metric::Logical,
                        search: current(&id),
                        count: u32::MAX,
                        offset: 0,
                        limit: 200,
                    });
                }
            }),
        ),
        Contents::Files => spawn(
            "folder_files",
            STATUS_EVERY,
            true,
            Box::new({
                let id = id();
                move |s: &Session, _: &mut Rng| {
                    let _ = s.folder_files(&proto::FolderFilesRequest {
                        generation,
                        folder: 0,
                        metric: proto::Metric::Logical,
                        search: current(&id),
                        offset: 0,
                        limit: 200,
                    });
                }
            }),
        ),
    });
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

/// Markdown tables: latency per command, then the mean of each part over
/// the requests at or above the p95 and p99 latency.
fn report(name: &str, samples: &[Sample], scanning: bool) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let phase = if scanning { "while scanning" } else { "after" };
    let _ = writeln!(out, "\n#### {name}, {phase}\n");
    let _ = writeln!(
        out,
        "| request | n | p50 | p95 | p99 | max | tail | {} | backlog |",
        PARTS.iter().map(|p| p.0).collect::<Vec<_>>().join(" | ")
    );
    let _ = writeln!(out, "|{}", "---|".repeat(7 + PARTS.len() + 1));
    let mut commands: Vec<&str> = samples.iter().map(|s| s.command).collect();
    commands.sort();
    commands.dedup();
    for command in commands {
        let mut mine: Vec<&Sample> = samples
            .iter()
            .filter(|s| s.command == command && s.scanning == scanning)
            .collect();
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
                    "{command} | {} | {} | {} | {} | {}",
                    mine.len(),
                    ms(p50),
                    ms(p95),
                    ms(p99),
                    ms(max)
                )
            } else {
                " | | | | | ".to_owned()
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
        },
        Scenario {
            name: "Filter on, Contents with files only",
            filter: Filter::AtStart,
            contents: Contents::Files,
        },
        Scenario {
            name: "No filter, Contents with folders",
            filter: Filter::Off,
            contents: Contents::Items,
        },
        Scenario {
            name: "Filter typed 5 s into the scan, Contents with folders",
            filter: Filter::After(Duration::from_secs(5)),
            contents: Contents::Items,
        },
    ];
    let cores = std::thread::available_parallelism().map_or(0, |n| n.get());
    println!(
        "cores: {cores}, scan workers: {}",
        ScanConfig::default().workers
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
