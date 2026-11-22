//! `sb-probe mft <volume> [--compare <root>]`: the elevated raw-MFT read
//! (design idea 2.3, phase 3 prototype). Reads the volume's master file
//! table once, read-only, builds the record table with bottom-up totals,
//! lists the records the USN journal says changed during the run, and,
//! with `--compare`, checks it node by node against a walk of the same
//! root with the production listing adapter.
//!
//! The gate and how to run it: `docs/validation/mft-probe.md`.

// Only the comparison rules (and their tests) build off Windows.
#![cfg_attr(not(windows), allow(dead_code))]

pub mod compare;
#[cfg(windows)]
mod volume;

use std::path::PathBuf;

/// Options for `sb-probe mft`.
pub struct MftOpts {
    pub volume: String,
    pub compare: Option<PathBuf>,
    /// Parser threads.
    pub threads: usize,
    /// Read size in MiB (4-16).
    pub chunk_mib: u64,
    /// Also treat records the journal changed this many seconds before the
    /// read started as changed: the raw read sees the table as the lazy
    /// writer last flushed it.
    pub lag_secs: u64,
    pub samples: usize,
    /// Matched nodes to print with their record values.
    pub show: usize,
}

/// Why `sb-probe mft` stopped.
pub enum Failure {
    /// Not elevated, not NTFS, not Windows: nothing was read.
    Refused(String),
    /// A read or query failed part way.
    Failed(String),
}

/// The drive letter of `C`, `C:`, `C:\` or `\\.\C:`.
pub fn drive_letter(volume: &str) -> Option<char> {
    let v = volume.strip_prefix(r"\\.\").unwrap_or(volume);
    let v = v.strip_suffix('\\').unwrap_or(v);
    let v = v.strip_suffix(':').unwrap_or(v);
    let mut chars = v.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_alphabetic() => Some(c.to_ascii_uppercase()),
        _ => None,
    }
}

/// Runs the probe; returns the JSON report and whether a comparison ran
/// and failed.
pub fn run(opts: &MftOpts) -> Result<(String, bool), Failure> {
    let letter = drive_letter(&opts.volume).ok_or_else(|| {
        Failure::Refused(format!(
            "{:?} is not a drive (expected a letter such as C:)",
            opts.volume
        ))
    })?;
    #[cfg(windows)]
    return volume::run(letter, opts);
    #[cfg(not(windows))]
    {
        let _ = letter;
        Err(Failure::Refused(
            "reading the master file table needs Windows and NTFS".into(),
        ))
    }
}

#[cfg(test)]
mod tests;
