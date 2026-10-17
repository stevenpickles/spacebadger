//! How soon a rescan's map looks right with folders estimated from the last
//! snapshot (`sb_core::prior`): rescans a real folder with the native
//! adapter, lays out its map every 150 ms as the interface would while the
//! scan runs, and compares each layout with the final one.
//!
//! The error at a moment is the symmetric difference in area between the
//! map then and the final map, over the top two levels: for each level,
//! Σ |A ∖ B| + |B ∖ A| over the rectangles of that level (matched by path;
//! one missing from either side counts whole), divided by the two layouts'
//! areas at that level together, so 0 is identical and 1 shares nothing.
//!
//! The same rescan runs three times per round, interleaved and starting
//! with a different one each round: with the prior and folders listed in
//! the order it suggests (as the app scans; see `ScanConfig::list_by_prior`),
//! with the prior and folders listed in the order found, and without the
//! prior, so the last two are the baselines for the order and for the
//! estimates. The snapshot is the file given with `--snapshot`, or else one
//! captured from a first scan (an immediate rescan, the easiest case).

use crate::ms;
use sb_core::layout::{self, Layout, LayoutParams, Metric, OrderCache, RectKind};
use sb_core::prior::Prior;
use sb_core::snapshot::{CASE_INSENSITIVE, Snapshot};
use sb_core::tree::{NodeId, Tree};
use sb_scan::{NativeFs, Scan, ScanConfig, native};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How often the interface asks for a layout while a scan runs.
const TICK: Duration = Duration::from_millis(150);

/// The map's size in CSS pixels: a maximized window at 1920 × 1080.
const WIDTH: f32 = 1500.0;
const HEIGHT: f32 = 860.0;

/// The rectangles of one level of a layout, by path below the view.
type Level = HashMap<String, (f32, f32, f32, f32)>;

pub fn run(root: &Path, snapshot: Option<&Path>, rounds: usize) {
    println!("provisional map: rescanning {}", root.display());
    let snap = match snapshot {
        Some(file) => {
            let bytes = std::fs::read(file).expect("read snapshot");
            let t = Instant::now();
            let snap = Snapshot::decode(&bytes).expect("decode snapshot");
            println!(
                "snapshot {} ({} entries, {:.1} MiB): decoded in {}",
                file.display(),
                snap.len(),
                bytes.len() as f64 / f64::from(1 << 20),
                ms(t.elapsed())
            );
            snap
        }
        None => {
            let (tree, took) = scan(root, None, true, |_, _| {});
            println!("first scan to take a snapshot from: {}", ms(took));
            let tree = tree.read().unwrap();
            Snapshot::capture(&tree, 0, CASE_INSENSITIVE)
        }
    };
    let t = Instant::now();
    let prior = Arc::new(Prior::new(snap));
    println!("prior indexed in {}", ms(t.elapsed()));

    let mut curves: [Vec<Curve>; 3] = Default::default();
    for round in 0..rounds.max(1) {
        for i in 0..VARIANTS.len() {
            let variant = (round + i) % VARIANTS.len();
            let prior = (variant < 2).then(|| Arc::clone(&prior));
            let curve = replay(root, prior, variant == 0);
            println!(
                "round {round} {}: scan {}, {} layouts",
                VARIANTS[variant],
                ms(curve.took),
                curve.points.len()
            );
            curves[variant].push(curve);
        }
    }
    report(&curves);
}

/// Listed in the order the prior suggests, in the order found, and
/// without the prior.
const VARIANTS: [&str; 3] = ["prior order", "found order", "no prior"];

struct Curve {
    took: Duration,
    /// (time since the scan started, level 1 error, level 2 error).
    points: Vec<(Duration, f64, f64)>,
    /// Nothing was estimated once the scan had finished.
    exact_at_end: bool,
}

/// Scans `root`, calling `tick` with the tree every [`TICK`] while it runs.
fn scan(
    root: &Path,
    prior: Option<Arc<Prior>>,
    by_prior: bool,
    mut tick: impl FnMut(&Tree, Duration),
) -> (Arc<std::sync::RwLock<Tree>>, Duration) {
    let started = Instant::now();
    let config = ScanConfig {
        list_by_prior: by_prior,
        ..native::scan_config(root)
    };
    let scan = Scan::start(root.to_path_buf(), NativeFs::new(), config, |_| {});
    let tree = scan.tree();
    if let Some(prior) = prior {
        tree.write().unwrap().set_prior(prior);
    }
    let mut next = started;
    while !scan.progress().state.is_finished() {
        let now = Instant::now();
        if now >= next {
            tick(&tree.read().unwrap(), now - started);
            next += TICK;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let took = started.elapsed();
    let _ = scan.wait();
    (tree, took)
}

fn replay(root: &Path, prior: Option<Arc<Prior>>, by_prior: bool) -> Curve {
    let params = LayoutParams::new(WIDTH, HEIGHT);
    let mut cache = OrderCache::default();
    let mut shots = Vec::new();
    let (tree, took) = scan(root, prior, by_prior, |tree, at| {
        let l = layout::layout(
            tree,
            NodeId::ROOT,
            Metric::Logical,
            None,
            &params,
            &mut cache,
            true,
        );
        shots.push((at, levels(tree, &l)));
    });
    let tree = tree.read().unwrap();
    let last = layout::layout(
        &tree,
        NodeId::ROOT,
        Metric::Logical,
        None,
        &params,
        &mut cache,
        false,
    );
    let exact_at_end =
        !tree.is_provisional(NodeId::ROOT) && last.rects.iter().all(|r| !r.provisional);
    let final_levels = levels(&tree, &last);
    if let Some(at) = std::env::var("SB_PROVISIONAL_DEBUG")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        && let Some((when, l)) = shots.iter().find(|(t, _)| *t >= Duration::from_millis(at))
    {
        for level in 0..2 {
            let mut worst: Vec<(f64, String, f64, f64)> = l[level]
                .iter()
                .map(|(k, &a)| {
                    let b = final_levels[level].get(k).copied();
                    let diff = area(a) - b.map_or(0.0, |b| overlap(a, b));
                    (diff, k.clone(), area(a), b.map_or(0.0, area))
                })
                .chain(
                    final_levels[level]
                        .iter()
                        .filter(|(k, _)| !l[level].contains_key(*k))
                        .map(|(k, &b)| (area(b), k.clone(), 0.0, area(b))),
                )
                .collect();
            worst.sort_by(|a, b| b.0.total_cmp(&a.0));
            println!(
                "  at {} level {}: largest differences (px², then / final)",
                ms(*when),
                level + 1
            );
            for (diff, key, now, last) in worst.iter().take(8) {
                println!("    {diff:>9.0}  {key}  {now:.0} / {last:.0}");
            }
        }
    }
    let points = shots
        .iter()
        .map(|(at, l)| {
            (
                *at,
                difference(&l[0], &final_levels[0]),
                difference(&l[1], &final_levels[1]),
            )
        })
        .collect();
    Curve {
        took,
        points,
        exact_at_end,
    }
}

/// The layout's first two levels, keyed by path below the view; merged
/// small items by their folder's path and a marker.
fn levels(tree: &Tree, l: &Layout) -> [Level; 2] {
    let mut out = [Level::new(), Level::new()];
    for r in &l.rects {
        if r.depth == 0 || r.depth > 2 {
            continue;
        }
        let mut key = path_below_root(tree, r.node);
        if r.kind == RectKind::OtherSmall {
            key.push_str("/<small items>");
        }
        out[usize::from(r.depth - 1)].insert(key, (r.x, r.y, r.w, r.h));
    }
    out
}

fn path_below_root(tree: &Tree, node: NodeId) -> String {
    let mut names = Vec::new();
    let mut cur = Some(node);
    while let Some(n) = cur {
        if n != NodeId::ROOT {
            names.push(tree.name(n).to_string_lossy().into_owned());
        }
        cur = tree.parent(n);
    }
    names.reverse();
    names.join("/")
}

fn area(r: (f32, f32, f32, f32)) -> f64 {
    f64::from(r.2) * f64::from(r.3)
}

fn overlap(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)) -> f64 {
    let w = (a.0 + a.2).min(b.0 + b.2) - a.0.max(b.0);
    let h = (a.1 + a.3).min(b.1 + b.3) - a.1.max(b.1);
    if w <= 0.0 || h <= 0.0 {
        0.0
    } else {
        f64::from(w) * f64::from(h)
    }
}

/// Symmetric difference of two levels over their combined area.
fn difference(now: &Level, last: &Level) -> f64 {
    let mut diff = 0.0;
    let mut total = 0.0;
    for (key, &a) in now {
        total += area(a);
        diff += area(a) - last.get(key).map_or(0.0, |&b| overlap(a, b));
    }
    for (key, &b) in last {
        total += area(b);
        diff += area(b) - now.get(key).map_or(0.0, |&a| overlap(a, b));
    }
    if total == 0.0 { 0.0 } else { diff / total }
}

fn report(curves: &[Vec<Curve>; 3]) {
    println!();
    println!("error vs the final map (level 1 / level 2), median over rounds:");
    print!("{:>8}", "t");
    for v in VARIANTS {
        print!("  {v:>17}");
    }
    println!();
    let longest = curves
        .iter()
        .flatten()
        .map(|c| c.points.len())
        .max()
        .unwrap_or(0);
    let median = |curves: &[Curve], i: usize, level: usize| -> Option<f64> {
        let mut v: Vec<f64> = curves
            .iter()
            .filter_map(|c| c.points.get(i))
            .map(|p| if level == 1 { p.1 } else { p.2 })
            .collect();
        if v.is_empty() {
            return None;
        }
        v.sort_by(f64::total_cmp);
        Some(v[v.len() / 2])
    };
    let cell = |curves: &[Curve], i: usize| match (median(curves, i, 1), median(curves, i, 2)) {
        (Some(a), Some(b)) => format!("{:>7.2}% / {:>6.2}%", a * 100.0, b * 100.0),
        _ => format!("{:>17}", "done"),
    };
    for i in 0..longest {
        print!("{:>6}ms", i as u128 * TICK.as_millis());
        for c in curves {
            print!("  {}", cell(c, i));
        }
        println!();
    }
    let first_under = |curves: &[Curve], limit: f64| -> Vec<String> {
        curves
            .iter()
            .map(|c| {
                c.points
                    .iter()
                    .find(|p| p.1.max(p.2) < limit)
                    .map_or("never".into(), |p| ms(p.0))
            })
            .collect()
    };
    // The area of error left over the scan: the mean error on both levels
    // over the layouts, in error-seconds (lower is better).
    let mean_error = |c: &Curve| -> f64 {
        c.points
            .iter()
            .map(|p| (p.1 + p.2) / 2.0 * TICK.as_secs_f64())
            .sum()
    };
    println!();
    for (v, c) in VARIANTS.iter().zip(curves) {
        let mut took: Vec<f64> = c.iter().map(|c| c.took.as_secs_f64() * 1e3).collect();
        took.sort_by(f64::total_cmp);
        let errors: Vec<String> = c.iter().map(|c| format!("{:.3}", mean_error(c))).collect();
        println!(
            "{v:>11}: first under 5% {:?}, under 2% {:?}; error-seconds {errors:?}; scan ms {took:.0?}",
            first_under(c, 0.05),
            first_under(c, 0.02),
        );
    }
    let gate = curves[0].iter().all(|c| {
        c.points
            .iter()
            .any(|p| p.0 <= Duration::from_millis(500) && p.1.max(p.2) < 0.05)
    });
    let exact = curves.iter().flatten().all(|c| c.exact_at_end);
    println!(
        "gate (< 5% within 0.5 s, every round): {}; nothing estimated at completion: {}",
        if gate { "pass" } else { "FAIL" },
        if exact { "yes" } else { "NO" }
    );
}
