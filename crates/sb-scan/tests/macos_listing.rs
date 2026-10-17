//! The macOS `getattrlistbulk` listing against the `readdir` + `lstat` one
//! on the same trees: every entry must come out the same, on APFS and on
//! HFS+, exFAT and FAT disk images.
#![cfg(target_os = "macos")]

use sb_scan::native::{Fallbacks, Listing};
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

/// A 64 MB disk image attached at a folder, detached when dropped, also
/// when a test fails. Declare images after the `TempDir` holding them, and
/// an image mounted inside another after it, so they detach first.
struct Image {
    mount: PathBuf,
}

impl Image {
    fn attach(filesystem: &str, volume: &str, images: &Path, mount: &Path) -> Self {
        std::fs::create_dir_all(images).unwrap();
        std::fs::create_dir_all(mount).unwrap();
        let dmg = images.join(format!("{volume}.dmg"));
        run(Command::new("hdiutil")
            .args(["create", "-quiet", "-size", "64m", "-fs", filesystem])
            .args(["-volname", volume])
            .arg(&dmg));
        run(Command::new("hdiutil")
            .args(["attach", "-quiet", "-nobrowse", "-noverify", "-mountpoint"])
            .arg(mount)
            .arg(&dmg));
        Self {
            mount: mount.to_path_buf(),
        }
    }
}

impl Drop for Image {
    fn drop(&mut self) {
        let _ = Command::new("hdiutil")
            .args(["detach", "-quiet", "-force"])
            .arg(&self.mount)
            .status();
    }
}

fn run(cmd: &mut Command) {
    let status = cmd.status().unwrap();
    assert!(status.success(), "{cmd:?}: {status}");
}

fn stderr(text: &str) {
    std::io::Write::write_all(&mut std::io::stderr(), text.as_bytes()).unwrap();
}

/// What one listing says about every path below `root`: its entry, or the
/// error listing it gave.
type Seen = BTreeMap<PathBuf, Result<Entry, String>>;

fn walk(listing: Listing, root: &Path) -> (Seen, Fallbacks) {
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
    (seen, fs.fallbacks())
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

/// Both listings of `root` agree; the bulk one's entries and fallbacks.
fn assert_same(root: &Path) -> (Seen, Fallbacks) {
    let (old, _) = walk(Listing::Readdir, root);
    let (new, fallbacks) = walk(Listing::Bulk, root);
    let diff = differences(&old, &new);
    assert!(
        diff.is_empty(),
        "{root:?}: {} differences:\n{}",
        diff.len(),
        diff.join("\n")
    );
    (new, fallbacks)
}

/// Builds a fixture. On APFS every step must work; elsewhere what the
/// filesystem can't hold is skipped and noted.
struct Builder {
    strict: bool,
    skipped: Vec<String>,
}

impl Builder {
    fn ok<T>(&mut self, what: &str, result: std::io::Result<T>) -> Option<T> {
        match result {
            Ok(value) => Some(value),
            Err(err) if !self.strict => {
                self.skipped.push(format!("{what}: {err}"));
                None
            }
            Err(err) => panic!("{what}: {err}"),
        }
    }

    fn write(&mut self, path: &Path, bytes: usize) {
        let result = std::fs::create_dir_all(path.parent().unwrap())
            .and_then(|()| std::fs::write(path, vec![0x5a; bytes]));
        self.ok(&format!("write {path:?}"), result);
    }

    fn run(&mut self, cmd: &mut Command) {
        let result = cmd.status().and_then(|status| {
            if status.success() {
                Ok(())
            } else {
                Err(std::io::Error::other(status.to_string()))
            }
        });
        self.ok(&format!("{cmd:?}"), result);
    }
}

/// A tree with every kind of entry the listings must agree on, and the
/// socket in it, kept open; with `strict`, every step must work.
fn fixture(root: &Path, strict: bool) -> Option<std::os::unix::net::UnixListener> {
    let mut b = Builder {
        strict,
        skipped: Vec::new(),
    };
    for size in [0, 1, 511, 512, 4095, 4096, 4097, 65_537, 1 << 20] {
        b.write(&root.join(format!("sizes/{size}.bin")), size);
    }
    let sparse =
        std::fs::File::create(root.join("sizes/sparse.bin")).and_then(|f| f.set_len(64 << 20));
    b.ok("sparse file", sparse);
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
        let path = root.join("times").join(name);
        b.write(&path, 3);
        let set = std::fs::File::options()
            .write(true)
            .open(&path)
            .and_then(|f| f.set_modified(time));
        b.ok(&format!("time of {name}"), set);
    }
    // A resource fork counts in the allocation.
    b.write(&root.join("forks/with-rsrc"), 100);
    let fork = std::fs::write(
        root.join("forks/with-rsrc/..namedfork/rsrc"),
        vec![1; 10_000],
    );
    b.ok("resource fork", fork);
    // A compressed copy: `st_size` is the uncompressed length.
    let plain = std::fs::write(
        root.join("forks/plain.txt"),
        "compress me ".repeat(20_000).as_bytes(),
    );
    b.ok("plain.txt", plain);
    b.run(
        Command::new("ditto")
            .arg("--hfsCompression")
            .arg(root.join("forks/plain.txt"))
            .arg(root.join("forks/compressed.txt")),
    );

    let empty = std::fs::create_dir_all(root.join("dirs/empty"));
    b.ok("empty folder", empty);
    let mut deep = root.join("dirs/deep");
    while deep.as_os_str().len() < 900 {
        deep.push("d0123456789abcdef");
    }
    b.write(&deep.join("bottom.txt"), 7);

    let target = root.join("links/target.txt");
    b.write(&target, 5);
    let links = root.join("links");
    use std::os::unix::fs::symlink;
    b.ok("link to file", symlink(&target, links.join("to-file")));
    b.ok(
        "link to folder",
        symlink(root.join("dirs"), links.join("to-dir")),
    );
    b.ok(
        "dangling link",
        symlink(root.join("nowhere"), links.join("dangling")),
    );
    b.ok(
        "relative link",
        symlink("target.txt", links.join("relative")),
    );
    b.ok(
        "hard link",
        std::fs::hard_link(&target, links.join("hard-1")),
    );
    b.ok(
        "hard link",
        std::fs::hard_link(&target, root.join("dirs/hard-2")),
    );

    b.run(Command::new("mkfifo").arg(root.join("fifo")));
    let socket = std::os::unix::net::UnixListener::bind(root.join("sock"));
    let socket = b.ok("socket", socket);

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
        b.write(&names.join(name), name.len());
    }
    let dir = std::fs::create_dir_all(names.join("ünïcode dir"));
    b.ok("unicode folder", dir);

    // More entries than one 64 KiB buffer holds.
    for i in 0..3000 {
        b.write(&root.join(format!("wide/entry-number-{i:05}.dat")), i % 7);
    }

    // Unreadable: a folder neither listing can list, and a file whose
    // metadata both still read.
    b.write(&root.join("locked/inside.txt"), 1);
    b.write(&root.join("no-read.txt"), 9);
    b.run(
        Command::new("chmod")
            .arg("000")
            .arg(root.join("locked"))
            .arg(root.join("no-read.txt")),
    );
    if !b.skipped.is_empty() {
        stderr(&format!(
            "fixture at {root:?} skipped:\n  {}\n",
            b.skipped.join("\n  ")
        ));
    }
    socket
}

#[test]
fn bulk_listing_matches_readdir_and_lstat() {
    let tmp = TempDir::new("parity");
    let root = tmp.0.join("root");
    std::fs::create_dir(&root).unwrap();
    let _socket = fixture(&root, true);
    // A mount point under an APFS folder.
    let _mounted = Image::attach(
        "HFS+",
        "sbmounted",
        &tmp.0.join("images"),
        &root.join("mnt"),
    );

    let (seen, fallbacks) = assert_same(&root);
    assert_eq!(
        fallbacks,
        Fallbacks {
            folders: 0,
            entries: 1
        },
        "only the mount point is lstat'ed"
    );

    let entry = |rel: &str| match &seen[&root.join(rel)] {
        Ok(entry) => entry.clone(),
        Err(err) => panic!("{rel}: {err}"),
    };
    assert_eq!(entry("mnt").kind, EntryKind::MountPoint);
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
            .filter(|p| p.parent() == Some(&root.join("wide")))
            .count(),
        3000
    );
}

#[test]
fn bulk_listing_matches_readdir_on_other_filesystems() {
    let tmp = TempDir::new("volumes");
    let images = tmp.0.join("images");
    let mut report = String::from("bulk listing on disk images (both listings agree):\n");
    for (filesystem, volume) in [("HFS+", "sbhfs"), ("ExFAT", "sbexfat"), ("MS-DOS", "SBFAT")] {
        let image = Image::attach(
            filesystem,
            volume,
            &images,
            &tmp.0.join(format!("mnt-{volume}")),
        );
        let root = image.mount.join("root");
        std::fs::create_dir(&root).unwrap();
        let _socket = fixture(&root, false);
        let (seen, fallbacks) = assert_same(&root);
        report += &format!("  {filesystem}: {} entries, {fallbacks:?}\n", seen.len());
    }

    // A mount point under a non-APFS folder: a FAT image inside an HFS+ one.
    let outer = Image::attach("HFS+", "sbouter", &images, &tmp.0.join("mnt-outer"));
    let root = outer.mount.join("root");
    std::fs::create_dir(&root).unwrap();
    let _socket = fixture(&root, false);
    let inner = Image::attach("MS-DOS", "SBINNER", &images, &root.join("inner"));
    std::fs::write(inner.mount.join("on-inner.txt"), b"x").unwrap();
    let (seen, fallbacks) = assert_same(&root);
    report += &format!("  FAT inside HFS+: {} entries, {fallbacks:?}\n", seen.len());
    stderr(&report);
    match &seen[&root.join("inner")] {
        Ok(entry) => assert_eq!(entry.kind, EntryKind::MountPoint),
        Err(err) => panic!("inner: {err}"),
    }
    assert!(!seen.contains_key(&root.join("inner/on-inner.txt")));
}

#[test]
fn bulk_listing_matches_readdir_on_system_folders() {
    // Real, varied folders the runner has: compressed system files,
    // bundles, links. None holds firmlinks or mount points.
    for dir in ["/usr/bin", "/usr/lib", "/System/Library/CoreServices"] {
        let dir = Path::new(dir);
        if dir.is_dir() {
            let (old, _) = list_under(Listing::Readdir, dir, dir);
            let (new, fallbacks) = list_under(Listing::Bulk, dir, dir);
            let diff = differences(&old.unwrap(), &new.unwrap());
            assert!(diff.is_empty(), "{dir:?}:\n{}", diff.join("\n"));
            assert_eq!(fallbacks, Fallbacks::default(), "{dir:?}");
        }
    }
}

/// `dir`'s entries in a scan of `root`, without descending.
fn list_under(listing: Listing, root: &Path, dir: &Path) -> (Result<Seen, String>, Fallbacks) {
    let fs = NativeFs::with_listing(listing);
    fs.open_root(root).unwrap();
    let mut seen = Seen::new();
    let listed = fs.read_dir(dir, &mut |batch| {
        for entry in batch {
            seen.insert(dir.join(&entry.name), Ok(entry));
        }
        ControlFlow::Continue(())
    });
    (
        listed.map(|()| seen).map_err(|e| format!("{e}")),
        fs.fallbacks(),
    )
}

#[test]
fn firmlinks_and_mount_points_are_treated_alike() {
    // Both listings must agree on what a scan of `/` meets: firmlinked
    // data-volume folders (`/Users`), what lies below them, real mount
    // points (`/dev`, `/System/Volumes/*`) and the special folders at the
    // top. The policy itself is unchanged here; what it amounts to is
    // printed for the record.
    let root = Path::new("/");
    let mut report = String::from("what a scan of / meets (both listings agree):\n");
    for dir in [
        "/",
        "/Users",
        "/Applications",
        "/System/Volumes",
        "/.nofollow",
        "/.resolve",
        "/.vol",
    ] {
        let dir = Path::new(dir);
        let (old, _) = list_under(Listing::Readdir, root, dir);
        let (new, fallbacks) = list_under(Listing::Bulk, root, dir);
        assert_eq!(fallbacks.folders, 0, "{dir:?} fell back to readdir");
        let (old, new) = match (old, new) {
            (Ok(old), Ok(new)) => (old, new),
            (old, new) => {
                assert_eq!(old.is_err(), new.is_err(), "{dir:?}: {old:?} / {new:?}");
                report += &format!("  {dir:?}: not listed: {:?}\n", new.err());
                continue;
            }
        };
        let diff = differences(&old, &new);
        assert!(diff.is_empty(), "{dir:?}:\n{}", diff.join("\n"));
        let mut kinds: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (path, entry) in &new {
            let kind = format!("{:?}", entry.as_ref().map(|e| e.kind));
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            kinds.entry(kind).or_default().push(name.into_owned());
        }
        report += &format!(
            "  {dir:?}: {} entries ({} lstat'ed)\n",
            new.len(),
            fallbacks.entries
        );
        for (kind, mut names) in kinds {
            let more = names.len().saturating_sub(12);
            names.truncate(12);
            report += &format!("    {kind}: {}", names.join(", "));
            if more > 0 {
                report += &format!(" (+{more} more)");
            }
            report += "\n";
        }
    }
    for path in [
        "/",
        "/Users",
        "/Users/runner",
        "/System/Volumes/Data",
        "/.nofollow",
    ] {
        if let Ok(meta) = std::fs::symlink_metadata(path) {
            use std::os::macos::fs::MetadataExt;
            report += &format!(
                "  lstat {path:?}: dev {:#x}, ino {}, flags {:#x}\n",
                meta.st_dev(),
                meta.st_ino(),
                meta.st_flags()
            );
        }
    }
    stderr(&report);
    let top = list_under(Listing::Bulk, root, root).0.unwrap();
    if let Some(Ok(dev)) = top.get(Path::new("/dev")) {
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
    let _socket = fixture(&root, true);
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
    let (_, fallbacks) = assert_same(&root);
    assert_eq!(fallbacks, Fallbacks::default());
    let mut walks: BTreeMap<&str, Vec<Duration>> = BTreeMap::new();
    let mut scans: BTreeMap<&str, Vec<Duration>> = BTreeMap::new();
    for _ in 0..7 {
        for (name, listing) in [("readdir", Listing::Readdir), ("bulk", Listing::Bulk)] {
            let start = Instant::now();
            let (seen, _) = walk(listing, &root);
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
    stderr(&report);
}
