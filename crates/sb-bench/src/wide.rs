//! Very wide folders: one folder holding hundreds of thousands to millions
//! of entries, scanned through the real engine. Measures what a single huge
//! listing costs: peak memory, how long the tree stays locked against
//! readers (a thread takes the read lock every millisecond, as the interface
//! does for layouts), and how soon a cancel is acknowledged once the
//! listing's entries are being added to the tree.

use crate::{memory, mib, ms};
use sb_scan::{
    Entry, EntryKind, FsAdapter, Identity, Measured, NativeFs, RootKind, Scan, ScanConfig,
    ScanError,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime};

/// A synthetic folder of `entries` entries at the root: files with
/// distinct sizes and IDs, and one subfolder (empty) per thousand entries.
struct WideFs {
    entries: usize,
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
            cloud: false,
            modified: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
        }
    }
}

impl FsAdapter for WideFs {
    fn open_root(&self, _root: &Path) -> Result<RootKind, ScanError> {
        Ok(RootKind::Directory)
    }

    fn read_dir(&self, dir: &Path) -> Result<Vec<Entry>, ScanError> {
        if dir != Path::new(ROOT) {
            return Ok(Vec::new());
        }
        Ok((0..self.entries).map(Self::entry).collect())
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

    fn read_dir(&self, dir: &Path) -> Result<Vec<Entry>, ScanError> {
        let result = self.inner.read_dir(dir);
        if result.as_ref().is_ok_and(|e| !e.is_empty()) {
            self.delivered.store(true, Ordering::Relaxed);
        }
        result
    }

    fn measure_file(&self, file: &Path) -> Result<Measured, ScanError> {
        self.inner.measure_file(file)
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

fn run_case<A: FsAdapter, F: Fn() -> A>(label: &str, root: PathBuf, adapter: F) {
    println!("\n== {label} ==");
    let config = ScanConfig::default();
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
        "scan:    {:?}, {} files, {} folders, {} nodes in {}",
        p.state,
        p.files,
        p.dirs,
        nodes,
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
        config,
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
        || WideFs { entries },
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
