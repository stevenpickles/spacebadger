//! Correctness-probe CLI (milestone 1): reports length, allocation by each
//! candidate API, identity, link count, reparse tag or flags, and hydration
//! state for the given paths without reading file contents.
//!
//! ```text
//! sb-probe file [--open-plain] [--settle-ms N] <path>...
//! sb-probe walk [--open] [--compressed] [--max-files N] [--samples N] <dir>
//! ```
//!
//! Output is JSON on stdout so results can be pasted into validation reports.

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

file   Probe individual paths with every candidate metadata API, snapshotting
       placeholder/hydration state between steps.
         --open-plain  open handles without FILE_FLAG_OPEN_NO_RECALL (Windows)
         --settle-ms   wait before the final snapshot (default 1500)
walk   Enumerate a tree via directory listings only (no link traversal) and
       summarize sizes, link/reparse categories, and placeholder states.
         --open        also open each file for attributes and compare
         --compressed  also call the compressed/allocated-size API and compare
         --max-files   stop after N files
         --samples     mismatch samples to keep per category (default 20)";

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
    match run(std::env::args_os().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("sb-probe: {message}\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
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
