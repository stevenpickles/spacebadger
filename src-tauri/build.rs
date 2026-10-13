//! Besides the Tauri build steps, works out the version shown in the window
//! title (`SPACEBADGER_BUILD`):
//!
//! - `v0.1.0` for a release: the commit is tagged `v` plus the Cargo.toml
//!   version and nothing tracked has changed;
//! - `v0.1.0+dev.d1ce4c8` for any other commit, with `.dirty` appended when
//!   tracked files have uncommitted changes;
//! - `v0.1.0+dev` when git isn't available (a source archive).

use std::path::Path;
use std::process::Command;

fn main() {
    let version = env!("CARGO_PKG_VERSION");
    println!(
        "cargo:rustc-env=SPACEBADGER_BUILD={}",
        build_version(version)
    );
    tauri_build::build()
}

fn build_version(version: &str) -> String {
    let Some(commit) = git(&["rev-parse", "--short=7", "HEAD"]) else {
        return format!("v{version}+dev");
    };
    watch_git();
    let tag = format!("v{version}");
    let tagged =
        git(&["tag", "--points-at", "HEAD"]).is_some_and(|tags| tags.lines().any(|t| t == tag));
    let dirty =
        git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
    match (tagged, dirty) {
        (true, false) => tag,
        (_, false) => format!("{tag}+dev.{commit}"),
        (_, true) => format!("{tag}+dev.{commit}.dirty"),
    }
}

/// Reruns this script when the commit, the tags, or the working tree change.
fn watch_git() {
    let mut paths = vec!["HEAD", "index", "packed-refs", "refs/tags"]
        .into_iter()
        .filter_map(|p| git(&["rev-parse", "--git-path", p]))
        .collect::<Vec<_>>();
    if let Some(branch) = git(&["symbolic-ref", "-q", "HEAD"]) {
        paths.extend(git(&["rev-parse", "--git-path", &branch]));
    }
    // Edits that haven't been staged don't touch git's files.
    paths.extend(
        [
            "../ui/src",
            "../ui/index.html",
            "../crates",
            "src",
            "Cargo.toml",
            "tauri.conf.json",
        ]
        .map(String::from),
    );
    for path in paths {
        if Path::new(&path).exists() {
            println!("cargo:rerun-if-changed={path}");
        }
    }
}

/// The trimmed output of a git command, or `None` if it failed.
fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}
