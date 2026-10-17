//! The macOS `getattrlistbulk` listing against the `readdir` + `lstat` one
//! on the same trees: every entry must come out the same.
#![cfg(target_os = "macos")]

use sb_scan::native::{Listing, readdir_fallbacks};
use sb_scan::{Entry, EntryKind, FsAdapter, NativeFs, OmissionReason, Progress, Scan, ScanConfig};
use std::collections::BTreeMap;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, UNIX_EPOCH};

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("sb-mac-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        // The listings compare `/private/var/...` and `/var/...` alike, but
        // keep paths canonical anyway.
        Self(path.canonicalize().unwrap())
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = Command::new("chmod")
            .arg("-R")
            .arg("u+rwx")
            .arg(&self.0)
            .status();
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write(path: &Path, bytes: usize) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, vec![0x5a; bytes]).unwrap();
}

fn run(cmd: &mut Command) {
    let status = cmd.status().unwrap();
    assert!(status.success(), "{cmd:?}: {status}");
}

/// What one listing says about every path below `root`: its entry, or the
/// error listing it gave.
type Seen = BTreeMap<PathBuf, Result<Entry, String>>;

fn walk(listing: Listing, root: &Path) -> Seen {
    let fs = NativeFs::with_listing(listing);
    fs.open_root(root).unwrap();
    let mut seen = Seen::new();
    let mut queue = vec![root.to_path_buf()];
    while let Some(dir) = queue.pop() {
        let mut entries = Vec::new();
        let listed = fs.read_dir(&dir, &mut |batch| {
            entries.extend(batch);
            ControlFlow::Continue(())
        });
        if let Err(err) = listed {
            seen.insert(dir.join("<listing>"), Err(format!("{:?}", err.kind)));
        }
        for entry in entries {
            let path = dir.join(&entry.name);
            if entry.kind == EntryKind::Directory {
                queue.push(path.clone());
            }
            assert!(
                seen.insert(path.clone(), Ok(entry)).is_none(),
                "{path:?} listed twice"
            );
        }
    }
    seen
}

/// The differences between two listings of the same tree, one line each.
/// Folders' link counts aren't compared: the bulk call has none, and the
/// engine uses only a folder's name, kind and ID.
fn differences(old: &Seen, new: &Seen) -> Vec<String> {
    let mut out = Vec::new();
    for path in old
        .keys()
        .chain(new.keys().filter(|p| !old.contains_key(*p)))
    {
        let (a, b) = match (old.get(path), new.get(path)) {
            (Some(a), Some(b)) => (a, b),
            (a, b) => {
                out.push(format!("{path:?}: readdir {a:?}, bulk {b:?}"));
                continue;
            }
        };
        let (a, b) = match (a, b) {
            (Ok(a), Ok(b)) => (a, b),
            (a, b) => {
                if a.as_ref().err() != b.as_ref().err() {
                    out.push(format!("{path:?}: readdir {a:?}, bulk {b:?}"));
                }
                continue;
            }
        };
        let mut field = |name: &str, x: String, y: String| {
            if x != y {
                out.push(format!("{path:?} {name}: readdir {x}, bulk {y}"));
            }
        };
        field("kind", format!("{:?}", a.kind), format!("{:?}", b.kind));
        field("logical", a.logical.to_string(), b.logical.to_string());
        field(
            "allocated",
            format!("{:?}", a.allocated),
            format!("{:?}", b.allocated),
        );
        if a.kind != EntryKind::Directory {
            field(
                "identity",
                format!("{:?}", a.identity),
                format!("{:?}", b.identity),
            );
        }
        field("id", format!("{:#x}", a.id), format!("{:#x}", b.id));
        field("cloud", a.cloud.to_string(), b.cloud.to_string());
        field(
            "modified",
            format!("{:?}", a.modified),
            format!("{:?}", b.modified),
        );
    }
    out
}

fn assert_same(root: &Path) -> Seen {
    let fallbacks = readdir_fallbacks();
    let old = walk(Listing::Readdir, root);
    let new = walk(Listing::Bulk, root);
    assert_eq!(
        readdir_fallbacks(),
        fallbacks,
        "the bulk listing fell back to readdir"
    );
    let diff = differences(&old, &new);
    assert!(
        diff.is_empty(),
        "{} differences:\n{}",
        diff.len(),
        diff.join("\n")
    );
    new
}

/// A tree with every kind of entry the listings must agree on.
fn fixture(root: &Path) -> std::os::unix::net::UnixListener {
    for size in [0, 1, 511, 512, 4095, 4096, 4097, 65_537, 1 << 20] {
        write(&root.join(format!("sizes/{size}.bin")), size);
    }
    let sparse = std::fs::File::create(root.join("sizes/sparse.bin")).unwrap();
    sparse.set_len(64 << 20).unwrap();
    drop(sparse);
    // Times before the epoch, with nanoseconds, and far ahead.
    for (name, time) in [
        ("old.txt", UNIX_EPOCH - Duration::from_millis(1500)),
        ("epoch.txt", UNIX_EPOCH),
        (
            "nanos.txt",
            UNIX_EPOCH + Duration::new(1_700_000_000, 123_456_789),
        ),
        (
            "future.txt",
            UNIX_EPOCH + Duration::from_secs(4_000_000_000),
        ),
    ] {
        write(&root.join("times").join(name), 3);
        let f = std::fs::File::options()
            .write(true)
            .open(root.join("times").join(name))
            .unwrap();
        f.set_modified(time).unwrap();
    }
    // A resource fork counts in the allocation.
    write(&root.join("forks/with-rsrc"), 100);
    std::fs::write(
        root.join("forks/with-rsrc/..namedfork/rsrc"),
        vec![1; 10_000],
    )
    .unwrap();
    // A compressed copy: `st_size` is the uncompressed length.
    std::fs::write(
        root.join("forks/plain.txt"),
        "compress me ".repeat(20_000).as_bytes(),
    )
    .unwrap();
    run(Command::new("ditto")
        .arg("--hfsCompression")
        .arg(root.join("forks/plain.txt"))
        .arg(root.join("forks/compressed.txt")));

    std::fs::create_dir_all(root.join("dirs/empty")).unwrap();
    let mut deep = root.join("dirs/deep");
    while deep.as_os_str().len() < 900 {
        deep.push("d0123456789abcdef");
    }
    write(&deep.join("bottom.txt"), 7);

    let target = root.join("links/target.txt");
    write(&target, 5);
    let links = root.join("links");
    std::os::unix::fs::symlink(&target, links.join("to-file")).unwrap();
    std::os::unix::fs::symlink(root.join("dirs"), links.join("to-dir")).unwrap();
    std::os::unix::fs::symlink(root.join("nowhere"), links.join("dangling")).unwrap();
    std::os::unix::fs::symlink("target.txt", links.join("relative")).unwrap();
    std::fs::hard_link(&target, links.join("hard-1")).unwrap();
    std::fs::hard_link(&target, root.join("dirs/hard-2")).unwrap();

    run(Command::new("mkfifo").arg(root.join("fifo")));
    let socket = std::os::unix::net::UnixListener::bind(root.join("sock")).unwrap();

    let names = root.join("names");
    for name in [
        "nai\u{308}ve-nfd.txt",
        "caf\u{e9}-nfc.txt",
        "日本語.txt",
        "emoji-😀.txt",
        "line\nbreak",
        " leading space",
        ".hidden",
        "n".repeat(255).as_str(),
    ] {
        write(&names.join(name), name.len());
    }
    std::fs::create_dir_all(names.join("ünïcode dir")).unwrap();

    // More entries than one 64 KiB buffer holds.
    for i in 0..3000 {
        write(&root.join(format!("wide/entry-number-{i:05}.dat")), i % 7);
    }

    // Unreadable: a folder neither listing can list, and a file whose
    // metadata both still read.
    write(&root.join("locked/inside.txt"), 1);
    write(&root.join("no-read.txt"), 9);
    run(Command::new("chmod")
        .arg("000")
        .arg(root.join("locked"))
        .arg(root.join("no-read.txt")));
    socket
}

#[test]
fn bulk_listing_matches_readdir_and_lstat() {
    let tmp = TempDir::new("parity");
    let root = tmp.0.join("root");
    std::fs::create_dir(&root).unwrap();
    let _socket = fixture(&root);

    let seen = assert_same(&root);

    let entry = |rel: &str| match &seen[&root.join(rel)] {
        Ok(entry) => entry.clone(),
        Err(err) => panic!("{rel}: {err}"),
    };
    assert_eq!(entry("fifo").kind, EntryKind::Special);
    assert_eq!(entry("sock").kind, EntryKind::Special);
    for link in ["to-file", "to-dir", "dangling", "relative"] {
        assert_eq!(entry(&format!("links/{link}")).kind, EntryKind::Symlink);
    }
    assert_eq!(entry("dirs/empty").kind, EntryKind::Directory);
    assert_eq!(entry("sizes/1048576.bin").logical, 1 << 20);
    assert_eq!(entry("sizes/0.bin").allocated, Some(0));
    assert_eq!(entry("forks/compressed.txt").logical, 240_000);
    let shared = entry("links/target.txt").identity;
    assert!(matches!(shared, sb_scan::Identity::MaybeShared { .. }));
    assert_eq!(entry("links/hard-1").identity, shared);
    assert_eq!(entry("dirs/hard-2").identity, shared);
    assert_eq!(entry("sizes/1.bin").identity, sb_scan::Identity::Unique);
    assert_eq!(
        entry("times/nanos.txt").modified,
        Some(UNIX_EPOCH + Duration::new(1_700_000_000, 123_456_789))
    );
    assert_eq!(
        entry("times/old.txt").modified,
        Some(UNIX_EPOCH - Duration::from_millis(1500))
    );
    assert!(seen.contains_key(&root.join("locked/<listing>")));
    assert_eq!(
        seen.keys()
            .filter(|p| p.starts_with(root.join("wide")))
            .count(),
        3000
    );
}

#[test]
fn bulk_listing_matches_readdir_on_system_folders() {
    // Real, varied folders the runner has: compressed system files,
    // bundles, links. `/usr/bin` holds no firmlinks or mount points.
    for dir in ["/usr/bin", "/usr/lib", "/System/Library/CoreServices"] {
        let dir = Path::new(dir);
        if dir.is_dir() {
            let old = walk_one(Listing::Readdir, dir);
            let new = walk_one(Listing::Bulk, dir);
            let diff = differences(&old, &new);
            assert!(diff.is_empty(), "{dir:?}:\n{}", diff.join("\n"));
        }
    }
}

/// One folder's entries, without descending.
fn walk_one(listing: Listing, dir: &Path) -> Seen {
    let fs = NativeFs::with_listing(listing);
    fs.open_root(dir).unwrap();
    let mut seen = Seen::new();
    fs.read_dir(dir, &mut |batch| {
        for entry in batch {
            seen.insert(dir.join(&entry.name), Ok(entry));
        }
        ControlFlow::Continue(())
    })
    .unwrap();
    seen
}

#[test]
fn firmlinks_and_mount_points_are_skipped_alike() {
    // Today's policy (unchanged here): from `/`, firmlinked data-volume
    // folders such as `/Users` are on another device and skipped as mount
    // points, as are real mount points such as `/System/Volumes/Data` and
    // `/dev`.
    let old = walk_one(Listing::Readdir, Path::new("/"));
    let new = walk_one(Listing::Bulk, Path::new("/"));
    let diff = differences(&old, &new);
    assert!(diff.is_empty(), "/:\n{}", diff.join("\n"));
    let kinds: Vec<String> = new
        .iter()
        .map(|(p, e)| format!("{p:?}: {:?}", e.as_ref().map(|e| e.kind)))
        .collect();
    std::io::Write::write_all(
        &mut std::io::stderr(),
        format!("entries of / (bulk listing):\n{}\n", kinds.join("\n")).as_bytes(),
    )
    .unwrap();
    if let Some(Ok(users)) = new.get(Path::new("/Users")) {
        assert_eq!(users.kind, EntryKind::MountPoint, "firmlink");
    }
    if let Some(Ok(dev)) = new.get(Path::new("/dev")) {
        assert_eq!(dev.kind, EntryKind::MountPoint);
    }
}

fn scan(listing: Listing, root: &Path) -> Progress {
    Scan::start(
        root.to_path_buf(),
        NativeFs::with_listing(listing),
        ScanConfig::default(),
        |_| {},
    )
    .wait()
}

#[test]
fn scans_total_the_same_either_way() {
    let tmp = TempDir::new("scan");
    let root = tmp.0.join("root");
    std::fs::create_dir(&root).unwrap();
    let _socket = fixture(&root);
    let (a, b) = (scan(Listing::Readdir, &root), scan(Listing::Bulk, &root));
    let totals = |p: &Progress| {
        (
            p.state.clone(),
            p.files,
            p.dirs,
            p.logical,
            p.allocated,
            p.unknown_allocation_files,
            p.hardlink_aliases,
            [
                OmissionReason::Symlink,
                OmissionReason::MountPoint,
                OmissionReason::SpecialFile,
                OmissionReason::PermissionDenied,
            ]
            .map(|r| p.omissions.count(r)),
        )
    };
    assert_eq!(totals(&a), totals(&b));
    assert_eq!(b.hardlink_aliases, 2);
}

/// Informational: listing time of each way on a ~100K-file tree. Run with
/// `cargo test -p sb-scan --release --test macos_listing -- --ignored
/// --nocapture`; CI does on macOS.
#[test]
#[ignore = "timing; prints numbers, asserts nothing about them"]
fn listing_timing() {
    let tmp = TempDir::new("timing");
    let root = tmp.0.join("root");
    let (dirs, per_dir) = (100, 1000);
    let made = Instant::now();
    for d in 0..dirs {
        let dir = root.join(format!("dir-{d:03}"));
        std::fs::create_dir_all(&dir).unwrap();
        for f in 0..per_dir {
            std::fs::write(dir.join(format!("file-{f:04}.dat")), [0u8; 3]).unwrap();
        }
    }
    let files = dirs * per_dir;
    let mut report = format!(
        "macOS listing timing: {files} files in {dirs} folders (made in {:.1?})\n",
        made.elapsed()
    );

    // Warm both, and check they agree on this tree too.
    assert_same(&root);
    let mut walks: BTreeMap<&str, Vec<Duration>> = BTreeMap::new();
    let mut scans: BTreeMap<&str, Vec<Duration>> = BTreeMap::new();
    for _ in 0..7 {
        for (name, listing) in [("readdir", Listing::Readdir), ("bulk", Listing::Bulk)] {
            let start = Instant::now();
            let seen = walk(listing, &root);
            walks.entry(name).or_default().push(start.elapsed());
            assert_eq!(seen.len(), files + dirs);
            let start = Instant::now();
            let progress = scan(listing, &root);
            scans.entry(name).or_default().push(start.elapsed());
            assert_eq!(progress.files, files as u64);
        }
    }
    for (what, times) in [
        ("one thread, listing only", &mut walks),
        ("engine scan, default workers", &mut scans),
    ] {
        for (name, t) in times.iter_mut() {
            t.sort();
            report += &format!(
                "  {what:<30} {name:<8} min {:>9.2?}  median {:>9.2?}  ({} runs, interleaved)\n",
                t[0],
                t[t.len() / 2],
                t.len()
            );
        }
        let ratio = times["readdir"][times["readdir"].len() / 2].as_secs_f64()
            / times["bulk"][times["bulk"].len() / 2].as_secs_f64();
        report += &format!("  {what:<30} readdir/bulk median ratio {ratio:.2}x\n");
    }
    std::io::Write::write_all(&mut std::io::stderr(), report.as_bytes()).unwrap();
}
