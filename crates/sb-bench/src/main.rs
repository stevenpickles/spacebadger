//! Scale benchmark: builds synthetic trees of the given sizes and times the
//! operations the interface depends on, without a window or a filesystem.
//!
//! ```text
//! cargo run --release -p sb-bench -- [--nodes N]... [--seed S]
//! cargo run --release -p sb-bench -- [--wide N]... [--wide-slow N]... [--wide-dir PATH]...
//! cargo run --release -p sb-bench -- [--lod N]...
//! ```
//!
//! Without arguments it runs 1,000,000 and 5,000,000 nodes. `--wide` scans a
//! synthetic folder of N entries through the scan engine, `--wide-slow` the
//! same folder listed slower than the engine adds it, and `--wide-dir` a
//! real folder with the native adapter (see [`wide`]). `--lod` lays out a
//! folder of N equal small files and a mixed tree of N nodes (see [`lod`]). Each case is measured
//! in a fresh child process so peak memory belongs to that case alone.
//! Real-disk scans are otherwise measured with the `sb-scan` `scan` example.

mod cpu;
mod lod;
mod memory;
mod synth;
mod wide;

use sb_core::layout::{self, LayoutParams, Metric, OrderCache, RectKind};
use sb_core::ranking::{self, Scope, Walk};
use sb_core::search::{Matcher, Ranked, Search};
use sb_core::suggest::NameIndex;
use sb_core::tree::{FileSizes, NodeId, Tree};
use sb_protocol::wire;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

#[global_allocator]
static ALLOC: lod::Counting = lod::Counting;

const DEFAULT_SIZES: [usize; 2] = [1_000_000, 5_000_000];

/// One measurement, run in its own process.
enum Case {
    Nodes(usize),
    Wide(usize),
    WideSlow(usize),
    WideDir(PathBuf),
    Lod(usize),
}

impl Case {
    fn args(&self) -> [String; 2] {
        match self {
            Self::Nodes(n) => ["--nodes".into(), n.to_string()],
            Self::Wide(n) => ["--wide".into(), n.to_string()],
            Self::WideSlow(n) => ["--wide-slow".into(), n.to_string()],
            Self::WideDir(p) => ["--wide-dir".into(), p.display().to_string()],
            Self::Lod(n) => ["--lod".into(), n.to_string()],
        }
    }
}

fn main() {
    let mut cases = Vec::new();
    let mut seed = 1u64;
    let mut child = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut text = || args.next().unwrap_or_else(|| panic!("{arg} needs a value"));
        let mut value = || {
            text()
                .replace('_', "")
                .parse::<u64>()
                .unwrap_or_else(|_| panic!("{arg} needs a number"))
        };
        match arg.as_str() {
            "--nodes" => cases.push(Case::Nodes(value() as usize)),
            "--wide" => cases.push(Case::Wide(value() as usize)),
            "--wide-slow" => cases.push(Case::WideSlow(value() as usize)),
            "--wide-dir" => cases.push(Case::WideDir(PathBuf::from(text()))),
            "--lod" => cases.push(Case::Lod(value() as usize)),
            "--seed" => seed = value(),
            "--child" => child = true,
            _ => panic!(
                "unknown argument {arg}; usage: sb-bench [--nodes N]... [--wide N]... \
                 [--wide-slow N]... [--wide-dir PATH]... [--lod N]... [--seed S]"
            ),
        }
    }
    if cases.is_empty() {
        cases.extend(DEFAULT_SIZES.map(Case::Nodes));
    }
    if child || cases.len() == 1 {
        for case in &cases {
            match case {
                Case::Nodes(n) => run(*n, seed),
                Case::Wide(n) => wide::synthetic(*n),
                Case::WideSlow(n) => wide::synthetic_slow(*n),
                Case::WideDir(p) => wide::real(p),
                Case::Lod(n) => lod::run(*n, seed),
            }
        }
        return;
    }
    print_machine();
    // One process per case, so each peak-memory figure stands alone.
    let exe = std::env::current_exe().expect("own executable path");
    for case in cases {
        let [flag, value] = case.args();
        let status = Command::new(&exe)
            .args(["--child", &flag, &value, "--seed", &seed.to_string()])
            .status()
            .expect("run benchmark child");
        assert!(status.success(), "benchmark {flag} {value} failed");
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

    // What the Contents list asks for: the largest files anywhere under the
    // root, and the root's own children, each in one walk.
    let rank = |scope: Scope, want: usize| {
        let key = ranking::Key {
            scope,
            folder: NodeId::ROOT,
            metric: Metric::Allocated,
            search: None,
        };
        let basis = ranking::Basis {
            revision: 1,
            finished: true,
            signature: ranking::Signature::of(&tree, NodeId::ROOT, None),
            at: Instant::now(),
        };
        let mut walk = Walk::new(key, basis, want);
        walk.step(&tree, None, usize::MAX);
        walk.finish().items().len()
    };
    let (files, ranked) = time(5, || rank(Scope::Files, ranking::MIN_RANKED));
    let (children, listed) = time(5, || rank(Scope::Children, usize::MAX));
    println!(
        "ranking: top {ranked} files under the root {}; root's {listed} children {}",
        ms(files),
        ms(children)
    );

    let (indexed, index) = time(3, || NameIndex::build(&tree));
    let (asked, found) = time(3, || index.folders("ab", 8).len());
    println!(
        "suggest: name index {}; folder suggestions for \"ab\" ({found}) {}",
        ms(indexed),
        ms(asked)
    );

    for query in [
        ".pdf",
        "e",
        "no file has this name",
        "*.jpg; *.png; *.pdf",
        "*e*; !*.tmp; !*.log",
        "dir:a",
        "*.pdf; !dir:e",
        "size:>100MB",
        "*.pdf; older:1y",
    ] {
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
        // What the result list asks for: the first page after a change.
        let (page, _) = time(3, || {
            Ranked::new(&s, Metric::Allocated).top(&tree, 100).len()
        });
        println!(
            "search:  {query:?} {} matches in {}; filtered layout {} ({} rects); full sort {}, first page {}",
            s.matches().len(),
            ms(searched),
            ms(filtered),
            fl.rects.len(),
            ms(sort),
            ms(page)
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
            None,
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
            parent: r.parent(tree).index() as u32,
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
