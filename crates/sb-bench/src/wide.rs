//! Very wide folders: one folder holding hundreds of thousands to millions
//! of entries, scanned through the real engine. Measures what a single huge
//! listing costs: peak memory, how long the tree stays locked against
//! readers (a thread takes the read lock every millisecond, as the interface
//! does for layouts), how long each of the scanner's write-lock holds lasts
//! as the tree grows, and how soon a cancel is acknowledged once the
//! listing's entries are being added to the tree.

use crate::{memory, mib, ms};
use sb_scan::{
    BATCH_ENTRIES, Entry, EntryKind, FsAdapter, HoldProbe, Identity, Measured, NativeFs, RootKind,
    Scan, ScanConfig, ScanError, Sink, batched,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime};

/// A synthetic folder of `entries` entries at the root: files with
/// distinct sizes and IDs, and one subfolder (empty) per thousand entries.
/// `batch_delay` slows the listing down per batch, as a slow disk or share
/// would, so listing rather than adding to the tree is the bottleneck.
struct WideFs {
    entries: usize,
    batch_delay: Duration,
}

const ROOT: &str = "/wide";

impl WideFs {
    fn entry(i: usize) -> Entry {
        let dir = i % 1000 == 999;
        Entry {
            name: format!("item_{i:08}.dat").into(),
            kind: if dir {
                EntryKind::Directory
            } else {
                EntryKind::File
            },
            logical: (i as u64 * 7919) % 1_000_000,
            allocated: Some(((i as u64 * 7919) % 1_000_000).div_ceil(4096) * 4096),
            // Windows listings mark every file as possibly shared.
            identity: Identity::MaybeShared { key: i as u128 + 1 },
            id: i as u128 + 1,
            cloud: false,
            modified: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
        }
    }
}

impl FsAdapter for WideFs {
    fn open_root(&self, _root: &Path) -> Result<RootKind, ScanError> {
        Ok(RootKind::Directory)
    }

    fn read_dir(&self, dir: &Path, sink: &mut Sink<'_>) -> Result<(), ScanError> {
        if dir != Path::new(ROOT) {
            return Ok(());
        }
        batched(sink, |out| {
            for i in 0..self.entries {
                if i % BATCH_ENTRIES == 0 && !self.batch_delay.is_zero() {
                    std::thread::sleep(self.batch_delay);
                }
                if out.push(Self::entry(i)).is_break() {
                    break;
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

/// Wraps an adapter to note when the first listed entries reach the engine.
struct Watched<A> {
    inner: A,
    delivered: Arc<AtomicBool>,
}

impl<A: FsAdapter> FsAdapter for Watched<A> {
    fn open_root(&self, root: &Path) -> Result<RootKind, ScanError> {
        self.inner.open_root(root)
    }

    fn read_dir(&self, dir: &Path, sink: &mut Sink<'_>) -> Result<(), ScanError> {
        self.inner.read_dir(dir, &mut |batch| {
            let flow = sink(batch);
            self.delivered.store(true, Ordering::Relaxed);
            flow
        })
    }

    fn measure_file(&self, file: &Path) -> Result<Measured, ScanError> {
        self.inner.measure_file(file)
    }

    fn read_small(&self, path: &Path, cap: usize) -> Result<Vec<u8>, ScanError> {
        self.inner.read_small(path, cap)
    }

    fn stat_kind(&self, path: &Path) -> Result<EntryKind, ScanError> {
        self.inner.stat_kind(path)
    }
}

/// Longest and 99th-percentile waits for the read lock while `done` is unset.
fn watch_readers(tree: Arc<RwLock<sb_core::tree::Tree>>, done: Arc<AtomicBool>) -> Vec<Duration> {
    let mut waits = Vec::new();
    while !done.load(Ordering::Relaxed) {
        let t = Instant::now();
        let guard = tree.read().unwrap();
        waits.push(t.elapsed());
        drop(guard);
        std::thread::sleep(Duration::from_millis(1));
    }
    waits.sort();
    waits
}

/// The scanner's write-lock holds, longest and 99th percentile, overall and
/// by the tree's size when they ended (in bands doubling from 256K nodes),
/// with the longest few.
fn report_holds(mut holds: Vec<(Duration, usize)>) {
    if holds.is_empty() {
        return;
    }
    let summary = |holds: &mut Vec<Duration>| {
        holds.sort();
        let p99 = holds[((holds.len() - 1) as f64 * 0.99) as usize];
        format!(
            "{} holds, longest {}, p99 {}",
            holds.len(),
            ms(*holds.last().unwrap()),
            ms(p99)
        )
    };
    let mut all: Vec<Duration> = holds.iter().map(|h| h.0).collect();
    println!("writes:  {}", summary(&mut all));
    let largest = holds.iter().map(|h| h.1).max().unwrap_or(0);
    let (mut low, mut high) = (0, 1 << 18);
    while low <= largest {
        let mut band: Vec<Duration> = holds
            .iter()
            .filter(|h| (low..high).contains(&h.1))
            .map(|h| h.0)
            .collect();
        if !band.is_empty() {
            println!(
                "         {:>4}K-{:>4}K nodes: {}",
                low >> 10,
                high >> 10,
                summary(&mut band)
            );
        }
        (low, high) = (high, high * 2);
    }
    holds.sort_by_key(|h| std::cmp::Reverse(h.0));
    let longest: Vec<String> = holds
        .iter()
        .take(5)
        .map(|(held, nodes)| format!("{} at {}K", ms(*held), nodes >> 10))
        .collect();
    println!("         longest: {}", longest.join(", "));
}

fn run_case<A: FsAdapter, F: Fn() -> A>(label: &str, root: PathBuf, adapter: F) {
    println!("\n== {label} ==");
    let holds = Arc::new(Mutex::new(Vec::new()));
    let config = ScanConfig {
        write_holds: Some({
            let holds = Arc::clone(&holds);
            HoldProbe::new(move |held, nodes| holds.lock().unwrap().push((held, nodes)))
        }),
        ..ScanConfig::default()
    };
    let before = memory::current();
    let delivered = Arc::new(AtomicBool::new(false));
    let started = Instant::now();
    let scan = Scan::start(
        root.clone(),
        Watched {
            inner: adapter(),
            delivered: Arc::clone(&delivered),
        },
        config.clone(),
        |_| {},
    );
    let done = Arc::new(AtomicBool::new(false));
    let reader = {
        let (tree, done) = (scan.tree(), Arc::clone(&done));
        std::thread::spawn(move || watch_readers(tree, done))
    };
    let tree = scan.tree();
    let p = scan.wait();
    let elapsed = started.elapsed();
    done.store(true, Ordering::Relaxed);
    let waits = reader.join().unwrap();
    let (nodes, tree_bytes) = {
        let tree = tree.read().unwrap();
        (tree.len(), tree.memory_bytes() as u64)
    };
    drop(tree);
    println!(
        "scan:    {:?}, {} files, {} folders, {} nodes, {} Git roots in {}",
        p.state,
        p.files,
        p.dirs,
        nodes,
        p.git_roots,
        ms(elapsed)
    );
    let pct = |q: f64| waits[((waits.len() - 1) as f64 * q) as usize];
    println!(
        "readers: {} read-lock waits; longest {}, p99 {}, median {}",
        waits.len(),
        ms(*waits.last().unwrap()),
        ms(pct(0.99)),
        ms(pct(0.5))
    );
    report_holds(std::mem::take(&mut *holds.lock().unwrap()));
    println!(
        "memory:  peak working set {}; grew by {} at peak; tree estimate {}",
        memory::peak().map_or("unknown".into(), mib),
        memory::peak()
            .zip(before)
            .map_or("unknown".into(), |(a, b)| mib(a.saturating_sub(b))),
        mib(tree_bytes)
    );

    // Cancel just after the first entries reach the aggregator.
    let delivered = Arc::new(AtomicBool::new(false));
    let scan = Scan::start(
        root,
        Watched {
            inner: adapter(),
            delivered: Arc::clone(&delivered),
        },
        ScanConfig {
            write_holds: None,
            ..config
        },
        |_| {},
    );
    while !delivered.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_micros(200));
    }
    std::thread::sleep(Duration::from_millis(10));
    let requested = Instant::now();
    scan.cancel();
    let p = scan.wait();
    println!(
        "cancel:  {:?} after {} files, acknowledged in {}",
        p.state,
        p.files,
        ms(requested.elapsed())
    );
}

/// A synthetic folder of `entries` entries.
pub fn synthetic(entries: usize) {
    run_case(
        &format!("synthetic folder of {entries} entries"),
        PathBuf::from(ROOT),
        || WideFs {
            entries,
            batch_delay: Duration::ZERO,
        },
    );
}

/// A synthetic folder of `entries` entries listed at 2 ms per batch, slower
/// than the engine adds them.
pub fn synthetic_slow(entries: usize) {
    run_case(
        &format!("synthetic folder of {entries} entries, listed at 2 ms per batch"),
        PathBuf::from(ROOT),
        || WideFs {
            entries,
            batch_delay: Duration::from_millis(2),
        },
    );
}

/// A real folder, scanned with the native adapter.
pub fn real(root: &Path) {
    run_case(
        &format!("real folder {}", root.display()),
        root.to_path_buf(),
        NativeFs::new,
    );
}
