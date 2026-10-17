//! Correctness-probe CLI (milestone 1): reports length, allocation by each
//! candidate API, identity, link count, reparse tag or flags, and hydration
//! state for the given paths without reading file contents.
//!
//! ```text
//! sb-probe file [--open-plain] [--settle-ms N] <path>...
//! sb-probe walk [--open] [--compressed] [--max-files N] [--samples N] <dir>
//! sb-probe mft <volume> [--compare <root>] [--threads N] [--chunk-mib N]
//!                       [--lag-secs N] [--samples N] [--show N]
//! ```
//!
//! Output is JSON on stdout so results can be pasted into validation reports.

mod mft;
mod report;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
use unix as platform;
#[cfg(windows)]
use windows as platform;

use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "usage:
  sb-probe file [--open-plain] [--settle-ms N] <path>...
  sb-probe walk [--open] [--compressed] [--max-files N] [--samples N] <dir>
  sb-probe mft <volume> [--compare <root>] [--threads N] [--chunk-mib N]
                        [--lag-secs N] [--samples N] [--show N]

file   Probe individual paths with every candidate metadata API, snapshotting
       placeholder/hydration state between steps.
         --open-plain  open handles without FILE_FLAG_OPEN_NO_RECALL (Windows)
         --settle-ms   wait before the final snapshot (default 1500)
walk   Enumerate a tree via directory listings only (no link traversal) and
       summarize sizes, link/reparse categories, and placeholder states.
         --open        also open each file for attributes and compare
         --compressed  also call the compressed/allocated-size API and compare
         --max-files   stop after N files
         --samples     mismatch samples to keep per category (default 20)
mft    Read an NTFS volume's master file table directly (Windows, elevated
       only; read-only) and report record counts, totals, and timing.
         --compare     also walk <root> on that volume with the production
                       listing adapter and compare node by node; exits 1
                       when they disagree
         --threads     parser threads (default: cores, at most 8)
         --chunk-mib   read size, 4-16 MiB (default 8)
         --lag-secs    also allow records the USN journal changed this long
                       before the read (default 60)
         --samples     difference samples per category (default 20)
         --show        also print the first N compared nodes with the
                       record's values (default 0)";

/// Options for `sb-probe file`.
pub struct FileOpts {
    pub open_plain: bool,
    pub settle_ms: u64,
}

/// Options for `sb-probe walk`.
pub struct WalkOpts {
    pub open: bool,
    pub compressed: bool,
    pub max_files: Option<u64>,
    pub samples: usize,
}

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.first().and_then(|a| a.to_str()) == Some("mft") {
        return run_mft(&args[1..]);
    }
    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => usage_error(&message),
    }
}

fn usage_error(message: &str) -> ExitCode {
    eprintln!("sb-probe: {message}\n\n{USAGE}");
    ExitCode::from(2)
}

/// `sb-probe mft`: exit 0 when read (and, with `--compare`, in agreement),
/// 1 when the comparison disagrees or a read fails, 2 for bad usage, and 3
/// when refused (not Windows, not NTFS, not elevated) without reading.
fn run_mft(args: &[std::ffi::OsString]) -> ExitCode {
    let opts = match mft_opts(args) {
        Ok(opts) => opts,
        Err(message) => return usage_error(&message),
    };
    match mft::run(&opts) {
        Ok((json, disagreed)) => {
            println!("{json}");
            if disagreed {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(mft::Failure::Refused(message)) => {
            eprintln!("sb-probe mft: refused: {message}");
            ExitCode::from(3)
        }
        Err(mft::Failure::Failed(message)) => {
            eprintln!("sb-probe mft: failed: {message}");
            ExitCode::FAILURE
        }
    }
}

fn mft_opts(args: &[std::ffi::OsString]) -> Result<mft::MftOpts, String> {
    let mut volume = None;
    let mut opts = mft::MftOpts {
        volume: String::new(),
        compare: None,
        threads: std::thread::available_parallelism().map_or(4, |n| n.get().min(8)),
        chunk_mib: 8,
        lag_secs: 60,
        samples: 20,
        show: 0,
    };
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let text = arg.to_str().ok_or("arguments must be valid Unicode")?;
        let mut number = || -> Result<u64, String> {
            let value = rest
                .next()
                .and_then(|v| v.to_str())
                .ok_or(format!("{text} needs a value"))?;
            value
                .parse()
                .map_err(|_| format!("{text}: not a number: {value}"))
        };
        match text {
            "--compare" => {
                let root = rest.next().ok_or("--compare needs a folder")?;
                opts.compare = Some(PathBuf::from(root));
            }
            "--threads" => opts.threads = number()?.clamp(1, 64) as usize,
            "--chunk-mib" => opts.chunk_mib = number()?.clamp(4, 16),
            "--lag-secs" => opts.lag_secs = number()?,
            "--samples" => opts.samples = number()? as usize,
            "--show" => opts.show = number()? as usize,
            flag if flag.starts_with("--") => return Err(format!("unknown option {flag}")),
            _ if volume.is_none() => volume = Some(text.to_owned()),
            _ => return Err("mft takes one volume".into()),
        }
    }
    opts.volume = volume.ok_or("no volume given")?;
    Ok(opts)
}

fn run(args: Vec<std::ffi::OsString>) -> Result<(), String> {
    let mut args = args.into_iter();
    let command = args.next().ok_or("missing command")?;
    let mut flags = Vec::new();
    let mut paths = Vec::new();
    let mut rest = args.peekable();
    while let Some(arg) = rest.next() {
        match arg.to_str() {
            Some(flag @ ("--settle-ms" | "--max-files" | "--samples")) => {
                let value = rest
                    .next()
                    .and_then(|v| v.into_string().ok())
                    .ok_or(format!("{flag} needs a value"))?;
                let value: u64 = value
                    .parse()
                    .map_err(|_| format!("{flag}: not a number: {value}"))?;
                flags.push((flag.to_owned(), Some(value)));
            }
            Some(flag) if flag.starts_with("--") => flags.push((flag.to_owned(), None)),
            _ => paths.push(PathBuf::from(arg)),
        }
    }
    let has = |name: &str| flags.iter().any(|(f, _)| f == name);
    let value = |name: &str| flags.iter().find(|(f, _)| f == name).and_then(|(_, v)| *v);
    let known: &[&str] = match command.to_str() {
        Some("file") => &["--open-plain", "--settle-ms"],
        Some("walk") => &["--open", "--compressed", "--max-files", "--samples"],
        _ => return Err(format!("unknown command {command:?}")),
    };
    if let Some((unknown, _)) = flags.iter().find(|(f, _)| !known.contains(&f.as_str())) {
        return Err(format!("unknown option {unknown}"));
    }
    if paths.is_empty() {
        return Err("no paths given".into());
    }

    let output = if command == "file" {
        let opts = FileOpts {
            open_plain: has("--open-plain"),
            settle_ms: value("--settle-ms").unwrap_or(1500),
        };
        let reports: Vec<_> = paths
            .iter()
            .map(|p| platform::probe_file(p, &opts))
            .collect();
        serde_json::to_string_pretty(&reports)
    } else {
        let [dir] = paths.as_slice() else {
            return Err("walk takes exactly one directory".into());
        };
        let opts = WalkOpts {
            open: has("--open"),
            compressed: has("--compressed"),
            max_files: value("--max-files"),
            samples: value("--samples").map_or(20, |v| v as usize),
        };
        serde_json::to_string_pretty(&platform::walk(dir, &opts))
    };
    println!("{}", output.map_err(|e| e.to_string())?);
    Ok(())
}
