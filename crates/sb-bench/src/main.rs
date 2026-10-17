//! Scale benchmark: builds synthetic trees of the given sizes and times the
//! operations the interface depends on, without a window or a filesystem;
//! and measures real-folder scans against the filesystem's own floor.
//!
//! ```text
//! cargo run --release -p sb-bench -- [--nodes N]... [--seed S]
//! cargo run --release -p sb-bench -- [--wide N]... [--wide-slow N]... [--wide-dir PATH]...
//! cargo run --release -p sb-bench -- [--lod N]...
//! cargo run --release -p sb-bench -- --floor PATH [--threads N]... [--strategy S]... [--runs R] [--no-warmup]
//! cargo run --release -p sb-bench -- --engine PATH [--threads N]... [--strategy S] [--runs R] [--no-warmup]
//! cargo run --release -p sb-bench -- --provisional PATH [--snapshot FILE] [--rounds N]
//! ```
//!
//! Without arguments it runs 1,000,000 and 5,000,000 nodes. `--wide` scans a
//! synthetic folder of N entries through the scan engine, `--wide-slow` the
//! same folder listed slower than the engine adds it, and `--wide-dir` a
//! real folder with the native adapter (see [`wide`]). `--lod` lays out a
//! folder of N equal small files and a mixed tree of N nodes (see [`lod`]).
//!
//! `--floor` lists a real folder with N threads and nothing else, the
//! syscall floor a scan can't beat (see [`floor`]); `--strategy` is
//! `current` (the adapter's calls, the default), or on Windows `ntdll` or
//! `relative`. `--engine` scans the folder with the real engine and lists
//! it with the floor (the first `--strategy` only), alternating, `--runs`
//! times each after one discarded warm-up pair (skipped with
//! `--no-warmup`), and reports the engine/floor wall and CPU ratios with
//! CPU per thread role (see [`engine`]). It exits with an error if, in any
//! run, the two counted more than 0.5 % different entries, the scan didn't
//! finish, or the floor couldn't list more than 1 % of the folders.
//! `--threads` defaults to the engine's default worker count and may be
//! repeated; `--runs` (default 1) repeats each measurement and reports its
//! spread. Each case and each run is measured in a fresh child process, so
//! peak memory belongs to that case alone. Method and numbers:
//! `docs/validation/scan-floor.md`.
//!
//! `--provisional` rescans a real folder and measures how soon its map,
//! estimated from a snapshot, matches the final one (see [`provisional`]);
//! it runs in this process, on its own.

mod cpu;
mod engine;
mod floor;
mod lod;
mod memory;
mod provisional;
mod synth;
mod wide;

use floor::Strategy;
use sb_core::layout::{self, LayoutParams, Metric, OrderCache, RectKind};
use sb_core::ranking::{self, Scope, Walk};
use sb_core::search::{Matcher, Ranked, Search};
use sb_core::suggest::NameIndex;
use sb_core::tree::{FileSizes, NodeId, Tree};
use sb_protocol::wire;
use std::collections::HashMap;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[global_allocator]
static ALLOC: lod::Counting = lod::Counting;

const DEFAULT_SIZES: [usize; 2] = [1_000_000, 5_000_000];

/// A root from the command line, made absolute once as the app does, so
/// the scan builds child paths verbatim (names ending in a dot or space).
fn real_root(text: String) -> PathBuf {
    let root = PathBuf::from(text);
    std::path::absolute(&root).unwrap_or(root)
}

/// One measurement, run in its own process.
enum Case {
    Nodes(usize),
    Wide(usize),
    WideSlow(usize),
    WideDir(PathBuf),
    Lod(usize),
    /// The floor, for each strategy and thread count.
    Floor(PathBuf),
    /// The engine against the floor, for each thread count.
    Engine(PathBuf),
}

impl Case {
    fn args(&self) -> [String; 2] {
        match self {
            Self::Nodes(n) => ["--nodes".into(), n.to_string()],
            Self::Wide(n) => ["--wide".into(), n.to_string()],
            Self::WideSlow(n) => ["--wide-slow".into(), n.to_string()],
            Self::WideDir(p) => ["--wide-dir".into(), p.display().to_string()],
            Self::Lod(n) => ["--lod".into(), n.to_string()],
            Self::Floor(p) => ["--floor".into(), p.display().to_string()],
            Self::Engine(p) => ["--engine".into(), p.display().to_string()],
        }
    }
}

/// Options shared by the floor and engine cases.
struct Ruler {
    threads: Vec<usize>,
    strategies: Vec<Strategy>,
    runs: usize,
    /// Run (and discard) one floor run, or floor and engine pair, first.
    warmup: bool,
}

const USAGE: &str = "usage: sb-bench [--nodes N]... [--seed S] [--wide N]... [--wide-slow N]... \
                     [--wide-dir PATH]... [--lod N]... [--floor PATH]... [--engine PATH]... \
                     [--threads N]... [--strategy current|ntdll|relative]... [--runs R] [--no-warmup] \n                     [--provisional PATH [--snapshot FILE] [--rounds N]]";

/// Below this much floor CPU time, Windows' 15.6 ms CPU clock makes the CPU
/// ratio too coarse to compare.
const MIN_FLOOR_CPU_MS: f64 = 1500.0;

fn main() {
    let mut cases = Vec::new();
    let mut seed = 1u64;
    let mut child = false;
    let mut ruler = Ruler {
        threads: Vec::new(),
        strategies: Vec::new(),
        runs: 1,
        warmup: true,
    };
    let mut provisional = None;
    let mut snapshot = None;
    let mut rounds = 3;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut text = || {
            args.next()
                .unwrap_or_else(|| panic!("{arg} needs a value\n{USAGE}"))
        };
        let mut value = || {
            text()
                .replace('_', "")
                .parse::<u64>()
                .unwrap_or_else(|_| panic!("{arg} needs a number\n{USAGE}"))
        };
        match arg.as_str() {
            "--nodes" => cases.push(Case::Nodes(value() as usize)),
            "--wide" => cases.push(Case::Wide(value() as usize)),
            "--wide-slow" => cases.push(Case::WideSlow(value() as usize)),
            "--wide-dir" => cases.push(Case::WideDir(real_root(text()))),
            "--lod" => cases.push(Case::Lod(value() as usize)),
            "--floor" => cases.push(Case::Floor(real_root(text()))),
            "--engine" => cases.push(Case::Engine(real_root(text()))),
            "--threads" => ruler.threads.push((value() as usize).max(1)),
            "--runs" => ruler.runs = (value() as usize).max(1),
            "--no-warmup" => ruler.warmup = false,
            "--strategy" => {
                let name = text();
                let strategy = Strategy::parse(&name)
                    .unwrap_or_else(|| panic!("unknown strategy {name}\n{USAGE}"));
                assert!(
                    strategy.available(),
                    "strategy {name} is only available on Windows"
                );
                ruler.strategies.push(strategy);
            }
            "--provisional" => provisional = Some(PathBuf::from(text())),
            "--snapshot" => snapshot = Some(PathBuf::from(text())),
            "--rounds" => rounds = value() as usize,
            "--seed" => seed = value(),
            "--child" => child = true,
            _ => panic!("unknown argument {arg}\n{USAGE}"),
        }
    }
    if let Some(root) = provisional {
        provisional::run(&root, snapshot.as_deref(), rounds);
        return;
    }
    if cases.is_empty() {
        cases.extend(DEFAULT_SIZES.map(Case::Nodes));
    }
    if ruler.threads.is_empty() {
        ruler.threads.push(sb_scan::ScanConfig::default().workers);
    }
    if ruler.strategies.is_empty() {
        ruler.strategies.push(Strategy::Current);
    }
    let ruled = |c: &Case| matches!(c, Case::Floor(_) | Case::Engine(_));
    if child || (cases.len() == 1 && !ruled(&cases[0])) {
        for case in &cases {
            match case {
                Case::Nodes(n) => run(*n, seed),
                Case::Wide(n) => wide::synthetic(*n),
                Case::WideSlow(n) => wide::synthetic_slow(*n),
                Case::WideDir(p) => wide::real(p),
                Case::Lod(n) => lod::run(*n, seed),
                Case::Floor(p) => floor::run(p, ruler.threads[0], ruler.strategies[0]),
                Case::Engine(p) => engine::run(p, ruler.threads[0]),
            }
        }
        return;
    }
    print_machine();
    if ruler.strategies.len() > 1 && cases.iter().any(|c| matches!(c, Case::Engine(_))) {
        println!(
            "warning: --engine compares with one floor strategy only, the first given ({})",
            ruler.strategies[0].name()
        );
    }
    // One process per case (and per run), so each figure stands alone.
    let mut failures = Vec::new();
    for case in cases {
        match &case {
            Case::Floor(root) => floors(root, &ruler),
            Case::Engine(root) => failures.extend(engines(root, &ruler)),
            _ => {
                let [flag, value] = case.args();
                spawn(&[flag, value, "--seed".into(), seed.to_string()]);
            }
        }
    }
    if !failures.is_empty() {
        eprintln!("\nerror: the floor and the engine didn't measure the same tree:");
        for failure in &failures {
            eprintln!("  {failure}");
        }
        std::process::exit(1);
    }
}

/// The `key=value` pairs of a child's `result:` line.
type Reported = HashMap<String, String>;

/// Runs this benchmark in a child process with `args`, echoing its output,
/// and returns the `key=value` pairs of its `result:` line, if any.
fn spawn(args: &[String]) -> Reported {
    let exe = std::env::current_exe().expect("own executable path");
    let mut child = Command::new(&exe)
        .arg("--child")
        .args(args)
        .stdout(Stdio::piped())
        .spawn()
        .expect("run benchmark child");
    let mut result = HashMap::new();
    let out = std::io::BufReader::new(child.stdout.take().expect("piped stdout"));
    for line in out.lines() {
        let line = line.expect("child output");
        println!("{line}");
        if line.starts_with("result:") {
            result = parse_result(&line);
        }
    }
    let status = child.wait().expect("wait for benchmark child");
    assert!(status.success(), "benchmark {} failed", args.join(" "));
    result
}

/// The `key=value` pairs of a `result:` line.
fn parse_result(line: &str) -> Reported {
    line.split_whitespace()
        .filter_map(|t| t.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// A number a child reported.
fn num(result: &Reported, key: &str) -> Option<f64> {
    result.get(key)?.parse().ok()
}

fn floor_args(root: &Path, threads: usize, strategy: Strategy) -> [String; 6] {
    [
        "--floor".into(),
        root.display().to_string(),
        "--threads".into(),
        threads.to_string(),
        "--strategy".into(),
        strategy.name().into(),
    ]
}

/// The floor of `root` for each strategy and thread count, `runs` times.
fn floors(root: &Path, ruler: &Ruler) {
    for &strategy in &ruler.strategies {
        for &threads in &ruler.threads {
            if ruler.warmup {
                println!("\n-- warm-up run (discarded) --");
                spawn(&floor_args(root, threads, strategy));
            }
            let results: Vec<_> = (0..ruler.runs)
                .map(|_| spawn(&floor_args(root, threads, strategy)))
                .collect();
            if ruler.runs > 1 {
                let walls: Vec<f64> = results.iter().map(wall).collect();
                let cpus: Vec<f64> = results.iter().filter_map(cpu_ms).collect();
                let s = Spread::of(&walls);
                println!(
                    "\n== summary: floor {}, {}, {threads} threads, {} runs ==",
                    root.display(),
                    strategy.name(),
                    ruler.runs
                );
                println!("floor:   wall {}", s.describe_ms());
                if !cpus.is_empty() {
                    println!("floor:   cpu {}", Spread::of(&cpus).describe_ms());
                }
                println!(
                    "summary: floor strategy={} threads={threads} runs={} min_ms={:.1} median_ms={:.1} \
                     max_ms={:.1} spread_pct={:.1}",
                    strategy.name(),
                    ruler.runs,
                    s.min,
                    s.median,
                    s.max,
                    s.pct()
                );
            }
        }
    }
}

/// Most the floor's and the engine's entry counts may differ, as a share
/// of the floor's: files created or deleted between the two runs.
const MAX_ENTRY_DIFFERENCE: f64 = 0.005;
/// Most folders the floor may fail to list, as a share of all folders.
const MAX_FLOOR_ERRORS: f64 = 0.01;

/// Why one floor run and the engine run after it can't be compared: they
/// listed different trees, the scan didn't finish, or the floor couldn't
/// list too much of it. Empty when they can.
fn coverage(floor: &Reported, engine: &Reported) -> Vec<String> {
    let mut problems = Vec::new();
    let (fe, ee) = (
        num(floor, "entries").unwrap_or(0.0),
        num(engine, "entries").unwrap_or(0.0),
    );
    if (fe - ee).abs() > fe.max(1.0) * MAX_ENTRY_DIFFERENCE {
        problems.push(format!(
            "entries differ by more than {:.1}%: floor {fe:.0}, engine {ee:.0} (files + folders + omissions)",
            MAX_ENTRY_DIFFERENCE * 100.0
        ));
    }
    // Omissions (links, mount points, unreadable folders) are part of
    // every real tree, so a scan that finished with them counts as
    // complete; one that was cancelled or failed doesn't.
    let state = engine.get("state").map_or("unknown", String::as_str);
    if !matches!(state, "Complete" | "CompleteWithOmissions") {
        problems.push(format!("the scan ended {state}, not complete"));
    }
    let errors = num(floor, "errors").unwrap_or(0.0);
    let folders = num(floor, "folders").unwrap_or(1.0).max(1.0);
    if errors / folders > MAX_FLOOR_ERRORS {
        problems.push(format!(
            "the floor couldn't list {errors:.0} of {folders:.0} folders (more than {:.0}%)",
            MAX_FLOOR_ERRORS * 100.0
        ));
    }
    problems
}

/// The engine against the floor (the first strategy) for each thread
/// count, alternating floor and engine runs so drifting load hits both.
/// Returns why runs couldn't be compared, if any.
fn engines(root: &Path, ruler: &Ruler) -> Vec<String> {
    let strategy = ruler.strategies[0];
    let mut failures = Vec::new();
    for &threads in &ruler.threads {
        let failed_before = failures.len();
        let engine_args = [
            "--engine".into(),
            root.display().to_string(),
            "--threads".into(),
            threads.to_string(),
        ];
        if ruler.warmup {
            println!("\n-- warm-up pair (discarded) --");
            spawn(&floor_args(root, threads, strategy));
            spawn(&engine_args);
        }
        // Wall times and CPU times (kernel + user) of the floor and the
        // engine, and their ratios, per run.
        let (mut walls, mut cpus) = ([vec![], vec![]], [vec![], vec![]]);
        let (mut wall_ratios, mut cpu_ratios) = (vec![], vec![]);
        let mut last = (Reported::new(), Reported::new());
        for run in 1..=ruler.runs {
            let f = spawn(&floor_args(root, threads, strategy));
            let e = spawn(&engine_args);
            for problem in coverage(&f, &e) {
                failures.push(format!(
                    "{}, {threads} threads, run {run}: {problem}",
                    root.display()
                ));
            }
            walls[0].push(wall(&f));
            walls[1].push(wall(&e));
            wall_ratios.push(wall(&e) / wall(&f));
            if let (Some(cf), Some(ce)) = (cpu_ms(&f), cpu_ms(&e)) {
                cpus[0].push(cf);
                cpus[1].push(ce);
                if cf > 0.0 {
                    cpu_ratios.push(ce / cf);
                }
            }
            last = (f, e);
        }
        println!(
            "\n== summary: engine vs floor {}, {threads} threads, floor strategy {}, {} runs alternating ==",
            root.display(),
            strategy.name(),
            ruler.runs
        );
        let (f, e, r) = (
            Spread::of(&walls[0]),
            Spread::of(&walls[1]),
            Spread::of(&wall_ratios),
        );
        println!("floor:   wall {}", f.describe_ms());
        println!("engine:  wall {}", e.describe_ms());
        println!("ratio:   wall {}", describe_ratios(&wall_ratios));
        // Load from other processes only ever adds time, so on a busy
        // machine the fastest run of each is the best estimate of both.
        let best = e.min / f.min;
        println!(
            "ratio:   fastest engine / fastest floor {best:.2}; median engine / median floor {:.2}",
            e.median / f.median
        );
        let cpu = if cpus[0].is_empty() {
            println!("ratio:   cpu not reported on this platform");
            None
        } else if cpu_ratios.is_empty() || Spread::of(&cpus[0]).median < MIN_FLOOR_CPU_MS {
            println!(
                "warning: the floor used under {:.1} s of CPU; the CPU ratio is too coarse to compare and is left out",
                MIN_FLOOR_CPU_MS / 1000.0
            );
            None
        } else {
            let (cf, ce) = (Spread::of(&cpus[0]), Spread::of(&cpus[1]));
            println!("floor:   cpu {}", cf.describe_ms());
            println!("engine:  cpu {}", ce.describe_ms());
            println!(
                "ratio:   cpu over {} of {} runs: {}",
                cpu_ratios.len(),
                ruler.runs,
                describe_ratios(&cpu_ratios)
            );
            Some(Spread::of(&cpu_ratios))
        };
        let (lf, le) = &last;
        let entries = (
            num(lf, "entries").unwrap_or(0.0),
            num(le, "entries").unwrap_or(0.0),
        );
        let floor_errors = num(lf, "errors").unwrap_or(0.0);
        let state = le.get("state").map_or("unknown", String::as_str);
        println!(
            "entries: floor {:.0}, engine {:.0} (files + folders + omissions); floor errors {floor_errors:.0}; \
             scan {state} (last run)",
            entries.0, entries.1
        );
        let mut line = format!(
            "summary: engine_vs_floor strategy={} threads={threads} runs={} floor_median_ms={:.1} \
             engine_median_ms={:.1} floor_min_ms={:.1} engine_min_ms={:.1} ratio_of_mins={best:.3} \
             ratio_min={:.3} ratio_median={:.3} ratio_max={:.3} ratio_spread_pct={:.1} \
             entries_floor={:.0} entries_engine={:.0} floor_errors={floor_errors:.0} engine_state={state}",
            strategy.name(),
            ruler.runs,
            f.median,
            e.median,
            f.min,
            e.min,
            r.min,
            r.median,
            r.max,
            r.pct(),
            entries.0,
            entries.1
        );
        if let Some(c) = cpu {
            line += &format!(
                " cpu_runs={} cpu_ratio_median={:.3} cpu_ratio_spread_pct={:.1}",
                cpu_ratios.len(),
                c.median,
                c.pct()
            );
        }
        line += if failures.len() == failed_before {
            " coverage=ok"
        } else {
            " coverage=failed"
        };
        println!("{line}");
    }
    failures
}

/// Each run's ratio, then their min, median, max and spread.
fn describe_ratios(ratios: &[f64]) -> String {
    let r = Spread::of(ratios);
    let each: Vec<String> = ratios.iter().map(|r| format!("{r:.2}")).collect();
    format!(
        "engine/floor per run {}; min {:.2}, median {:.2}, max {:.2}; spread {:.1}% of median",
        each.join(", "),
        r.min,
        r.median,
        r.max,
        r.pct()
    )
}

/// Process CPU time (kernel + user) a child reported, if it could.
fn cpu_ms(result: &Reported) -> Option<f64> {
    let (k, u) = (num(result, "kernel_ms")?, num(result, "user_ms")?);
    // -1 means the platform doesn't report it.
    (k >= 0.0 && u >= 0.0).then_some(k + u)
}

fn wall(result: &Reported) -> f64 {
    num(result, "wall_ms").expect("the child reported no wall time")
}

/// Minimum, median and maximum of some measurements.
#[derive(Debug, PartialEq)]
struct Spread {
    min: f64,
    median: f64,
    max: f64,
}

impl Spread {
    fn of(values: &[f64]) -> Self {
        let mut v = values.to_vec();
        v.sort_by(f64::total_cmp);
        let n = v.len();
        assert!(n > 0, "no measurements");
        let median = if n % 2 == 1 {
            v[n / 2]
        } else {
            (v[n / 2 - 1] + v[n / 2]) / 2.0
        };
        Self {
            min: v[0],
            median,
            max: v[n - 1],
        }
    }

    /// Max minus min, as a percentage of the median.
    fn pct(&self) -> f64 {
        (self.max - self.min) / self.median * 100.0
    }

    fn describe_ms(&self) -> String {
        format!(
            "min {:.1} ms, median {:.1} ms, max {:.1} ms; spread {:.1}% of median",
            self.min,
            self.median,
            self.max,
            self.pct()
        )
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
            more_flags: 0,
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
        estimated_from: 0,
    };
    wire::encode(&header, &rects, &labels)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn results_parse_as_pairs() {
        let r = parse_result("result:  engine wall_ms=812.5 entries=585000 state=Complete");
        assert_eq!(num(&r, "wall_ms"), Some(812.5));
        assert_eq!(num(&r, "entries"), Some(585_000.0));
        assert_eq!(r["state"], "Complete");
        assert_eq!(num(&r, "state"), None);
        assert_eq!(r.len(), 3);
    }

    #[test]
    fn coverage_needs_the_same_tree_a_finished_scan_and_few_floor_errors() {
        let floor = parse_result("result: floor entries=100000 folders=1000 errors=5");
        let engine = |entries: u32, state: &str| {
            parse_result(&format!("result: engine entries={entries} state={state}"))
        };
        assert!(coverage(&floor, &engine(100_400, "CompleteWithOmissions")).is_empty());
        assert!(coverage(&floor, &engine(99_600, "Complete")).is_empty());
        assert_eq!(coverage(&floor, &engine(100_600, "Complete")).len(), 1);
        assert_eq!(coverage(&floor, &engine(100_000, "Cancelled")).len(), 1);
        assert_eq!(coverage(&floor, &engine(100_000, "Failed")).len(), 1);
        let unreadable = parse_result("result: floor entries=100000 folders=1000 errors=11");
        assert_eq!(coverage(&unreadable, &engine(100_000, "Complete")).len(), 1);
        assert_eq!(coverage(&unreadable, &engine(1, "Cancelled")).len(), 3);
    }

    #[test]
    fn spread_is_range_over_median() {
        let s = Spread::of(&[110.0, 100.0, 90.0, 105.0, 95.0]);
        assert_eq!(
            s,
            Spread {
                min: 90.0,
                median: 100.0,
                max: 110.0
            }
        );
        assert!((s.pct() - 20.0).abs() < 1e-9);
        assert_eq!(Spread::of(&[1.0, 3.0]).median, 2.0);
    }
}
