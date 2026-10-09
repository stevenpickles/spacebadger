//! Scans a root with the native adapter and prints a summary.
//!
//! ```text
//! cargo run --release -p sb-scan --example scan -- <root> [--workers N] [--cancel-after-ms N]
//! ```
//!
//! After a full scan it also times treemap layouts of the root at 1920×1080:
//! a first (sorting) layout and a repeat that reuses the sibling order.

use sb_core::layout::{self, LayoutParams, Metric, OrderCache};
use sb_core::tree::NodeId;
use sb_scan::{NativeFs, Scan, ScanConfig};
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn gib(bytes: u64) -> String {
    format!("{:.2} GiB", bytes as f64 / (1u64 << 30) as f64)
}

fn main() {
    let mut args = std::env::args_os().skip(1);
    let root = PathBuf::from(
        args.next()
            .expect("usage: scan <root> [--workers N] [--cancel-after-ms N]"),
    );
    let mut config = ScanConfig::default();
    let mut cancel_after = None;
    while let Some(flag) = args.next() {
        let value: u64 = args
            .next()
            .and_then(|v| v.into_string().ok())
            .and_then(|v| v.parse().ok())
            .expect("flag needs a number");
        match flag.to_str() {
            Some("--workers") => config.workers = value as usize,
            Some("--cancel-after-ms") => cancel_after = Some(Duration::from_millis(value)),
            _ => panic!("unknown flag {flag:?}"),
        }
    }

    let started = Instant::now();
    let first_batch = std::sync::Mutex::new(None::<Duration>);
    let last_print = std::sync::Mutex::new(Instant::now());
    let first_batch = std::sync::Arc::new(first_batch);
    let first_sink = std::sync::Arc::clone(&first_batch);
    let workers = config.workers;
    let scan = Scan::start(root.clone(), NativeFs::new(), config, move |p| {
        if p.files > 0 {
            first_sink.lock().unwrap().get_or_insert(started.elapsed());
        }
        let mut last = last_print.lock().unwrap();
        if last.elapsed() >= Duration::from_secs(1) {
            eprintln!(
                "  {:>6.1}s  {:>10} files  {:>8} dirs  {:>12} allocated  {:>7} pending",
                p.elapsed.as_secs_f64(),
                p.files,
                p.dirs,
                gib(p.allocated),
                p.pending_dirs
            );
            *last = Instant::now();
        }
    });
    if let Some(after) = cancel_after {
        std::thread::sleep(after);
        let requested = Instant::now();
        scan.cancel();
        let tree = scan.tree();
        let p = scan.wait();
        println!("cancel acknowledged in {:?}", requested.elapsed());
        drop(tree);
        print_summary(&root, workers, &p, None);
        return;
    }
    let tree = scan.tree();
    let p = scan.wait();
    let tree = tree.read().unwrap();
    print_summary(&root, workers, &p, *first_batch.lock().unwrap());

    println!(
        "tree nodes:   {} (~{} MiB)",
        tree.len(),
        tree.memory_bytes() >> 20
    );
    let params = LayoutParams::new(1920.0, 1080.0);
    let mut cache = OrderCache::default();
    for (label, stable) in [("first layout", false), ("repeat layout", true)] {
        let t = Instant::now();
        let l = layout::layout(
            &tree,
            NodeId::ROOT,
            Metric::Allocated,
            &params,
            &mut cache,
            stable,
        );
        println!(
            "{label}: {:?}, {} rects{}",
            t.elapsed(),
            l.rects.len(),
            if l.truncated { " (budget reached)" } else { "" }
        );
    }
    let mut top: Vec<NodeId> = tree.children(NodeId::ROOT).collect();
    top.sort_by_key(|&n| std::cmp::Reverse(tree.allocated(n)));
    println!("largest entries at the root (allocated / logical):");
    for n in top.into_iter().take(10) {
        println!(
            "  {:>12}  {:>12}  {}{}",
            gib(tree.allocated(n)),
            gib(tree.logical(n)),
            tree.name(n).to_string_lossy(),
            if tree.is_dir(n) { "\\" } else { "" }
        );
    }
}

fn print_summary(
    root: &std::path::Path,
    workers: usize,
    p: &sb_scan::Progress,
    first: Option<Duration>,
) {
    let secs = p.elapsed.as_secs_f64().max(1e-9);
    println!("root:         {}", root.display());
    println!("state:        {:?}", p.state);
    println!("workers:      {workers}");
    println!("elapsed:      {:.2}s", secs);
    if let Some(first) = first {
        println!("first files:  {first:?}");
    }
    println!("files:        {} ({:.0}/s)", p.files, p.files as f64 / secs);
    println!("dirs:         {}", p.dirs);
    println!("logical:      {}", gib(p.logical));
    println!("allocated:    {}", gib(p.allocated));
    println!("unknown alloc files: {}", p.unknown_allocation_files);
    println!("hard-link aliases:   {}", p.hardlink_aliases);
    println!("omissions:");
    for (reason, count) in p.omissions.iter() {
        let sample = p.omissions.samples(reason).first();
        println!(
            "  {:<16} {:>8}  e.g. {}",
            format!("{reason:?}"),
            count,
            sample.map_or(String::new(), |s| match &s.detail {
                Some(detail) => format!("{} ({detail})", s.path.display()),
                None => s.path.display().to_string(),
            })
        );
    }
}
