//! The real scan engine on a real folder, timed for comparison with the
//! floor ([`crate::floor`]): wall time, the scanner's write-lock holds
//! (through [`HoldProbe`]), and CPU for the whole process and per role
//! (aggregator, workers, other), from the names `sb-scan` gives its threads.

use crate::cpu::{Cpu, Threads, print_roles, without_watcher};
use crate::ms;
use sb_scan::{HoldProbe, NativeFs, Scan, ScanConfig, ScanState};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Write-lock holds: how many, how long in total, the longest.
#[derive(Default)]
pub struct Holds {
    pub count: u64,
    pub total: Duration,
    pub longest: Duration,
}

impl Holds {
    /// A probe adding every hold to `holds`.
    pub fn probe(holds: &Arc<Mutex<Self>>) -> HoldProbe {
        let holds = Arc::clone(holds);
        HoldProbe::new(move |held, _| {
            let mut h = holds.lock().unwrap();
            h.count += 1;
            h.total += held;
            h.longest = h.longest.max(held);
        })
    }
}

/// Scans `root` once with `workers` workers and prints the measurements.
pub fn run(root: &Path, workers: usize) {
    println!("\n== engine {}: {workers} workers ==", root.display());
    let holds = Arc::new(Mutex::new(Holds::default()));
    let config = ScanConfig {
        workers,
        write_holds: Some(Holds::probe(&holds)),
        ..ScanConfig::default()
    };
    let threads = Threads::watch();
    let before = Cpu::process();
    let started = Instant::now();
    let scan = Scan::start(root.to_path_buf(), NativeFs::new(), config, |_| {});
    let tree = scan.tree();
    let p = scan.wait();
    let wall = started.elapsed();
    let process = Cpu::process().zip(before).map(|(a, b)| a.since(b));
    let roles = threads.finish();
    let cpu = process.map(|p| without_watcher(p, roles.as_ref()));
    let nodes = tree.read().unwrap().len();
    println!(
        "scan:    {:?}: {} files, {} folders, {} nodes, {} hard-link aliases, {} Git roots, {} omissions in {}",
        p.state,
        p.files,
        p.dirs,
        nodes,
        p.hardlink_aliases,
        p.git_roots,
        p.omissions.total(),
        ms(wall)
    );
    println!(
        "rate:    {:.0} entries/s",
        (p.files + p.dirs) as f64 / wall.as_secs_f64()
    );
    let h = holds.lock().unwrap();
    println!(
        "writes:  {} holds, {} held in total ({:.0}% of the scan), longest {}",
        h.count,
        ms(h.total),
        h.total.as_secs_f64() / wall.as_secs_f64() * 100.0,
        ms(h.longest)
    );
    if let Some(cpu) = cpu {
        println!(
            "cpu:     process kernel {}, user {} ({:.1} cores busy; without the thread watcher)",
            ms(cpu.kernel),
            ms(cpu.user),
            cpu.total().as_secs_f64() / wall.as_secs_f64()
        );
    }
    if let Some(roles) = &roles {
        print_roles(roles);
    }
    let role = |name: &str| {
        roles
            .as_ref()
            .and_then(|r| r.get(name))
            .map_or(-1.0, |(c, _)| c.total().as_secs_f64() * 1e3)
    };
    let (k, u) = cpu.map_or((-1.0, -1.0), |c| {
        (c.kernel.as_secs_f64() * 1e3, c.user.as_secs_f64() * 1e3)
    });
    println!(
        "result:  engine wall_ms={:.1} entries={} dirs={} holds={} kernel_ms={k:.0} user_ms={u:.0} \
         aggregator_ms={:.0} workers_ms={:.0} state={}",
        wall.as_secs_f64() * 1e3,
        p.files + p.dirs + p.omissions.total(),
        p.dirs,
        h.count,
        role("aggregator"),
        role("workers"),
        state_name(&p.state)
    );
}

/// A scan state as one word, for the `result:` line.
pub fn state_name(state: &ScanState) -> &'static str {
    match state {
        ScanState::Scanning => "Scanning",
        ScanState::Complete => "Complete",
        ScanState::CompleteWithOmissions => "CompleteWithOmissions",
        ScanState::Cancelled => "Cancelled",
        ScanState::Failed(_) => "Failed",
    }
}
