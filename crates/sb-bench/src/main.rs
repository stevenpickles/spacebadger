//! Scale benchmark: builds synthetic trees of the given sizes and times the
//! operations the interface depends on, without a window or a filesystem.
//!
//! ```text
//! cargo run --release -p sb-bench -- [--nodes N]... [--seed S]
//! ```
//!
//! Without `--nodes` it runs 1,000,000 and 5,000,000 nodes. Each size is
//! measured in a fresh child process so peak memory belongs to that size
//! alone. Real-disk scans are measured with the `sb-scan` `scan` example.

mod memory;
mod synth;

use sb_core::layout::{self, LayoutParams, Metric, OrderCache, RectKind};
use sb_core::search::{Matcher, Search};
use sb_core::tree::{FileSizes, NodeId, Tree};
use sb_protocol::wire;
use std::process::Command;
use std::time::{Duration, Instant};

const DEFAULT_SIZES: [usize; 2] = [1_000_000, 5_000_000];

fn main() {
    let mut sizes = Vec::new();
    let mut seed = 1u64;
    let mut child = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || {
            args.next()
                .and_then(|v| v.replace('_', "").parse::<u64>().ok())
                .unwrap_or_else(|| panic!("{arg} needs a number"))
        };
        match arg.as_str() {
            "--nodes" => sizes.push(value() as usize),
            "--seed" => seed = value(),
            "--child" => child = true,
            _ => panic!("unknown argument {arg}; usage: sb-bench [--nodes N]... [--seed S]"),
        }
    }
    if sizes.is_empty() {
        sizes.extend(DEFAULT_SIZES);
    }
    if child || sizes.len() == 1 {
        for &n in &sizes {
            run(n, seed);
        }
        return;
    }
    print_machine();
    // One process per size, so each peak-memory figure stands alone.
    let exe = std::env::current_exe().expect("own executable path");
    for n in sizes {
        let status = Command::new(&exe)
            .args([
                "--child",
                "--nodes",
                &n.to_string(),
                "--seed",
                &seed.to_string(),
            ])
            .status()
            .expect("run benchmark child");
        assert!(status.success(), "benchmark for {n} nodes failed");
    }
}

fn print_machine() {
    println!(
        "machine: {} {}, {} logical CPUs",
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::thread::available_parallelism().map_or(0, |n| n.get())
    );
}

fn ms(d: Duration) -> String {
    format!("{:.1} ms", d.as_secs_f64() * 1000.0)
}

fn mib(bytes: u64) -> String {
    format!("{:.0} MiB", bytes as f64 / (1u64 << 20) as f64)
}

/// Median of `runs` timings of `f`, plus its last result.
fn time<T>(runs: usize, mut f: impl FnMut() -> T) -> (Duration, T) {
    let mut times = Vec::with_capacity(runs);
    let mut out = None;
    for _ in 0..runs {
        let t = Instant::now();
        out = Some(f());
        times.push(t.elapsed());
    }
    times.sort();
    (times[runs / 2], out.expect("at least one run"))
}

fn run(nodes: usize, seed: u64) {
    println!("\n== {nodes} nodes (seed {seed}) ==");
    let before = memory::current();
    let t = Instant::now();
    let g = synth::generate(nodes, seed);
    let built = t.elapsed();
    let tree = g.tree;
    let after = memory::current();
    println!(
        "tree:    {} files, {} folders, names avg {:.1} bytes; built in {}",
        g.files,
        g.folders,
        g.name_bytes as f64 / tree.len() as f64,
        ms(built)
    );
    println!(
        "memory:  tree estimate {}; process grew by {}",
        mib(tree.memory_bytes() as u64),
        after
            .zip(before)
            .map_or("unknown".into(), |(a, b)| mib(a.saturating_sub(b)))
    );

    let params = LayoutParams::new(1920.0, 1080.0);
    let mut cache = OrderCache::default();
    let (first, l) = time(1, || {
        layout::layout(
            &tree,
            NodeId::ROOT,
            Metric::Allocated,
            None,
            &params,
            &mut cache,
            false,
        )
    });
    let (stable, _) = time(5, || {
        layout::layout(
            &tree,
            NodeId::ROOT,
            Metric::Allocated,
            None,
            &params,
            &mut cache,
            true,
        )
    });
    let (sorted, _) = time(5, || {
        layout::layout(
            &tree,
            NodeId::ROOT,
            Metric::Allocated,
            None,
            &params,
            &mut cache,
            false,
        )
    });
    println!(
        "layout:  root 1920x1080 {} rects{}; first {}, stable repeat {}, final sort {}",
        l.rects.len(),
        if l.truncated { " (budget reached)" } else { "" },
        ms(first),
        ms(stable),
        ms(sorted)
    );
    let (logical, _) = time(5, || {
        layout::layout(
            &tree,
            NodeId::ROOT,
            Metric::Logical,
            None,
            &params,
            &mut OrderCache::default(),
            false,
        )
    });
    println!("         logical metric, fresh cache {}", ms(logical));

    let (encode, bytes) = time(5, || encode(&tree, &l));
    println!(
        "wire:    root layout {} KiB, encoded in {}",
        bytes.len() / 1024,
        ms(encode)
    );

    // The folder with the most direct children stresses sorting and small items.
    let widest = (0..tree.len() as u32)
        .map(NodeId::from_index)
        .filter(|&n| tree.is_dir(n))
        .max_by_key(|&n| tree.children(n).count())
        .expect("tree has folders");
    let width = tree.children(widest).count();
    let (wide, wl) = time(5, || {
        layout::layout(
            &tree,
            widest,
            Metric::Allocated,
            None,
            &params,
            &mut OrderCache::default(),
            false,
        )
    });
    let other = wl
        .rects
        .iter()
        .find(|r| r.kind == RectKind::OtherSmall && r.node == widest)
        .map_or(0, |r| r.count as usize);
    let (small, _) = time(5, || {
        layout::small_children(&tree, widest, Metric::Allocated, None, other)
    });
    println!(
        "widest:  folder with {width} children: layout {}, listing {other} small items {}",
        ms(wide),
        ms(small)
    );

    for query in [".pdf", "e", "no file has this name"] {
        let (searched, s) = time(3, || {
            let mut s = Search::new(Matcher::new(query).expect("non-empty"));
            s.catch_up(&tree, usize::MAX);
            s
        });
        let (filtered, fl) = time(3, || {
            layout::layout(
                &tree,
                NodeId::ROOT,
                Metric::Allocated,
                Some(&s),
                &params,
                &mut OrderCache::default(),
                false,
            )
        });
        let (sort, _) = time(3, || {
            let mut files = s.matches().to_vec();
            files.sort_unstable_by_key(|&f| {
                (
                    std::cmp::Reverse(Metric::Allocated.weight(&tree, f)),
                    f.index(),
                )
            });
            files
        });
        println!(
            "search:  {query:?} {} matches in {}; filtered layout {} ({} rects); sort results {}",
            s.matches().len(),
            ms(searched),
            ms(filtered),
            fl.rects.len(),
            ms(sort)
        );
    }

    let mut tree = tree;
    let mut search = Search::new(Matcher::new(".pdf").expect("non-empty"));
    search.catch_up(&tree, usize::MAX);
    let grown = grow(&mut tree, 50_000, seed);
    let (catch_up, _) = time(1, || search.catch_up(&tree, usize::MAX));
    println!(
        "stream:  {grown} new nodes, search catch-up {}",
        ms(catch_up)
    );

    if let Some(peak) = memory::peak() {
        println!("peak:    process working set {}", mib(peak));
    }
}

/// Adds `count` files under existing folders, as a running scan would.
fn grow(tree: &mut Tree, count: usize, seed: u64) -> usize {
    let mut rng = synth::Rng::new(seed ^ 0x5eed);
    let folders: Vec<NodeId> = (0..tree.len() as u32)
        .map(NodeId::from_index)
        .filter(|&n| tree.is_dir(n))
        .collect();
    for i in 0..count {
        let dir = folders[rng.below(folders.len() as u64) as usize];
        let name = format!("late_{i}.pdf");
        tree.add_file(
            dir,
            name.as_ref(),
            FileSizes {
                logical: 4096,
                allocated: sb_core::size::Allocated::known(4096),
            },
            0,
        )
        .expect("within capacity");
    }
    count
}

fn encode(tree: &Tree, l: &layout::Layout) -> Vec<u8> {
    let rects: Vec<wire::Rect> = l
        .rects
        .iter()
        .map(|r| wire::Rect {
            x: r.x,
            y: r.y,
            w: r.w,
            h: r.h,
            node: r.node.index() as u32,
            count: r.count,
            weight: r.weight,
            depth: r.depth,
            kind: r.kind as u8,
            flags: r.flags,
            file_type: 0,
        })
        .collect();
    // Label every large-enough rectangle, as the desktop session does.
    let names: Vec<(u32, String)> = l
        .rects
        .iter()
        .enumerate()
        .filter(|(_, r)| r.kind != RectKind::OtherSmall && r.w >= 40.0 && r.h >= 14.0)
        .map(|(i, r)| (i as u32, tree.name(r.node).to_string_lossy().into_owned()))
        .collect();
    let labels: Vec<(u32, &str)> = names.iter().map(|(i, s)| (*i, s.as_str())).collect();
    let header = wire::Header {
        generation: 1,
        revision: 1,
        request: 1,
        view: 0,
        total: l.total,
        flags: 0,
    };
    wire::encode(&header, &rects, &labels)
}
