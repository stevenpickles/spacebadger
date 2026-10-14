//! Snapshots of completed scans, kept in the app's local data folder so a
//! later scan of the same folder can be compared with them.
//!
//! Each scan root has its own folder, named by a hash of the root's
//! absolute path (compared as the file system compares names), holding
//! `<finished ms>.sbsnap` files in the format of `sb_core::snapshot`. A
//! snapshot is written to a new temporary file (see [`crate::atomic_file`])
//! and renamed into place, so a crash leaves either the whole file or none,
//! and saves at once never write into each other's files. Temporary files a
//! crash left behind are removed by a later prune once an hour old.
//!
//! After each save, a root keeps its newest [`KEEP_NEWEST`] snapshots, the
//! first of every other day (UTC), and always its first, so rescanning a
//! folder many times doesn't push out the scan worth comparing with. Then,
//! while all snapshots together exceed [`MAX_TOTAL_BYTES`], the oldest go,
//! the daily ones before the newest and a root's first last of all.

use crate::atomic_file::{self, TempFile};
use sb_core::snapshot::{self, Snapshot, SnapshotHeader};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, BufReader, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

/// The newest snapshots kept per scan root.
pub const KEEP_NEWEST: usize = 5;

/// Room for all snapshots together.
pub const MAX_TOTAL_BYTES: u64 = 256 << 20;

const DAY_MS: u64 = 24 * 60 * 60 * 1000;
const EXTENSION: &str = "sbsnap";
/// Temporary files of earlier versions.
const OLD_TEMP_EXTENSION: &str = "tmp";

/// Temporary files older than this are left over from a crash.
const STALE_TEMP: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
    keep_newest: usize,
    max_total: u64,
}

/// A snapshot file found while pruning.
struct Found {
    created: u64,
    path: PathBuf,
    size: u64,
    /// Removed last when over the total: 0 a daily one, 1 one of the
    /// newest, 2 the root's first.
    rank: u8,
}

impl Store {
    pub fn new(dir: PathBuf) -> Self {
        Self::with_limits(dir, KEEP_NEWEST, MAX_TOTAL_BYTES)
    }

    pub fn with_limits(dir: PathBuf, keep_newest: usize, max_total: u64) -> Self {
        Self {
            dir,
            keep_newest: keep_newest.max(1),
            max_total,
        }
    }

    fn root_dir(&self, root: &Path) -> PathBuf {
        self.dir
            .join(format!("{:016x}", fnv64(root_key(root).as_bytes())))
    }

    /// Writes `snap` of `root` and prunes old snapshots, except `protect`
    /// (one of `root`'s, in use for comparing); returns the new snapshot's
    /// id.
    pub fn save(&self, root: &Path, snap: &Snapshot, protect: Option<&str>) -> io::Result<String> {
        // Encoded first, so the folder exists as briefly as possible before
        // the file in it.
        let bytes = snap.encode();
        let dir = self.root_dir(root);
        let base = snap.header().created_ms.to_string();
        // Another save's prune may remove the folder while it's empty, just
        // after it was made here; then it's made again.
        let mut retried = false;
        let (id, temp) = loop {
            fs::create_dir_all(&dir)?;
            let mut id = base.clone();
            let mut n = 1;
            while dir.join(format!("{id}.{EXTENSION}")).exists() {
                id = format!("{base}-{n}");
                n += 1;
            }
            match TempFile::new_in(&dir, &format!("{id}.{EXTENSION}")) {
                Ok(temp) => break (id, temp),
                Err(e) if e.kind() == io::ErrorKind::NotFound && !retried => retried = true,
                Err(e) => return Err(e),
            }
        };
        let path = dir.join(format!("{id}.{EXTENSION}"));
        // Removed when dropped unless persisted.
        temp.file().write_all(&bytes)?;
        temp.persist(&path)?;
        let mut keep = vec![path];
        keep.extend(protect.map(|p| dir.join(format!("{p}.{EXTENSION}"))));
        self.prune(&keep);
        Ok(id)
    }

    /// Readable snapshots of `root`, newest first. Unreadable files are
    /// skipped.
    pub fn list(&self, root: &Path) -> Vec<(String, SnapshotHeader)> {
        let key = root_key(root);
        let mut found: Vec<(String, SnapshotHeader)> = snapshot_files(&self.root_dir(root))
            .into_iter()
            .filter_map(|(id, path)| {
                let mut reader = BufReader::new(File::open(path).ok()?);
                let header = snapshot::read_header(&mut reader).ok()?;
                // A hash collision between two roots would show the other's.
                (root_key(Path::new(&header.root)) == key).then_some((id, header))
            })
            .collect();
        found.sort_by_key(|f| std::cmp::Reverse(f.1.created_ms));
        found
    }

    /// Reads snapshot `id` of `root`.
    pub fn load(&self, root: &Path, id: &str) -> Result<Snapshot, String> {
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_digit() || c == '-') {
            return Err(format!("unknown snapshot {id}"));
        }
        let path = self.root_dir(root).join(format!("{id}.{EXTENSION}"));
        let bytes = fs::read(&path).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => "That earlier scan is no longer saved.".to_owned(),
            _ => format!("Couldn't read the earlier scan: {e}"),
        })?;
        Snapshot::decode(&bytes).map_err(|e| format!("Couldn't read the earlier scan: {e}."))
    }

    /// Applies the retention rules in the module notes. `keep` (the one
    /// just written, and one in use) always stays.
    fn prune(&self, keep: &[PathBuf]) {
        let Ok(roots) = fs::read_dir(&self.dir) else {
            return;
        };
        let mut all = Vec::new();
        for root in roots.flatten() {
            let dir = root.path();
            if !dir.is_dir() {
                continue;
            }
            remove_stale_temps(&dir);
            let mut files: Vec<Found> = snapshot_files(&dir)
                .into_iter()
                .map(|(id, path)| Found {
                    created: created_of(&id),
                    size: fs::metadata(&path).map(|m| m.len()).unwrap_or(0),
                    path,
                    rank: 0,
                })
                .collect();
            // Oldest first, so each day's first comes before the rest of it.
            files.sort_by(|a, b| a.created.cmp(&b.created).then_with(|| a.path.cmp(&b.path)));
            let newest_from = files.len().saturating_sub(self.keep_newest);
            let mut days = HashSet::new();
            for (i, mut f) in files.into_iter().enumerate() {
                let first_of_day = days.insert(f.created / DAY_MS);
                f.rank = if i == 0 {
                    2
                } else if i >= newest_from {
                    1
                } else {
                    0
                };
                if f.rank > 0 || first_of_day || keep.contains(&f.path) {
                    all.push(f);
                } else {
                    let _ = fs::remove_file(&f.path);
                }
            }
            // Only removes a folder that's empty.
            let _ = fs::remove_dir(&dir);
        }
        all.sort_by(|a, b| {
            (a.rank, a.created)
                .cmp(&(b.rank, b.created))
                .then_with(|| a.path.cmp(&b.path))
        });
        let mut total: u64 = all.iter().map(|f| f.size).sum();
        for f in all {
            if total <= self.max_total {
                break;
            }
            if !keep.contains(&f.path) && fs::remove_file(&f.path).is_ok() {
                total -= f.size;
            }
        }
    }
}

/// `(id, path)` of the snapshot files in `dir`.
fn snapshot_files(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if path.extension()? != EXTENSION {
                return None;
            }
            let id = path.file_stem()?.to_str()?.to_owned();
            Some((id, path))
        })
        .collect()
}

/// Removes temporary files in `dir` too old to be still being written.
fn remove_stale_temps(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        let temp = path
            .extension()
            .is_some_and(|x| x == atomic_file::EXTENSION || x == OLD_TEMP_EXTENSION);
        if !temp {
            continue;
        }
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .is_some_and(|age| age > STALE_TEMP);
        if old {
            let _ = fs::remove_file(path);
        }
    }
}

/// The finish time in an id (`1760000000000` or `1760000000000-1`).
fn created_of(id: &str) -> u64 {
    id.split('-')
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// The root as compared: its path with one kind of separator (on Windows,
/// where `/` and `\` both work), without a `\\?\` prefix, with `.` and
/// `..` resolved by name, without trailing separators, and folded as names
/// are compared where case doesn't matter.
fn root_key(root: &Path) -> String {
    let mut text = root.to_string_lossy().into_owned();
    if cfg!(windows) {
        text = text.replace('/', "\\");
        if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
            text = format!(r"\\{rest}");
        } else if let Some(rest) = text.strip_prefix(r"\\?\") {
            text = rest.to_owned();
        }
    }
    let mut parts: Vec<Component> = Vec::new();
    for c in Path::new(&text).components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir if matches!(parts.last(), Some(Component::Normal(_))) => {
                parts.pop();
            }
            // Above the root is the root.
            Component::ParentDir
                if matches!(
                    parts.last(),
                    Some(Component::RootDir | Component::Prefix(_))
                ) => {}
            c => parts.push(c),
        }
    }
    let normal: PathBuf = parts.iter().collect();
    let text = normal.to_string_lossy();
    let mut buf = Vec::new();
    let folded = snapshot::fold(text.as_bytes(), snapshot::CASE_INSENSITIVE, &mut buf);
    String::from_utf8_lossy(folded).into_owned()
}

fn fnv64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use sb_core::size::Allocated;
    use sb_core::tree::{FileSizes, NodeId, Tree};

    struct TempDir(PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn temp(name: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!("sb-snapshots-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        TempDir(dir)
    }

    fn snap(root: &str, created_ms: u64, logical: u64) -> Snapshot {
        let mut t = Tree::new(root);
        t.add_file(
            NodeId::ROOT,
            "f".as_ref(),
            FileSizes {
                logical,
                allocated: Allocated::known(logical),
            },
            None,
            0,
        )
        .unwrap();
        Snapshot::capture(&t, created_ms, snapshot::CASE_INSENSITIVE)
    }

    #[test]
    fn saves_lists_and_loads_per_root() {
        let tmp = temp("list");
        let store = Store::new(tmp.0.clone());
        let a = Path::new("/data/a");
        let b = Path::new("/data/b");
        let first = store.save(a, &snap("/data/a", 1000, 1), None).unwrap();
        let second = store.save(a, &snap("/data/a", 2000, 2), None).unwrap();
        store.save(b, &snap("/data/b", 1500, 3), None).unwrap();
        let again = store.save(a, &snap("/data/a", 2000, 4), None).unwrap();
        assert_eq!(again, "2000-1", "same time, distinct file");

        let listed: Vec<_> = store
            .list(a)
            .into_iter()
            .map(|(id, h)| (id, h.logical))
            .collect();
        assert_eq!(listed[0].0.split('-').next(), Some("2000"));
        assert_eq!(listed.len(), 3);
        assert_eq!(listed[2], (first.clone(), 1));
        assert_eq!(store.list(Path::new("/data/a/")).len(), 3, "trailing slash");
        assert_eq!(store.list(b).len(), 1);
        assert!(store.list(Path::new("/elsewhere")).is_empty());

        assert_eq!(store.load(a, &second).unwrap().header().logical, 2);
        assert!(store.load(a, "999").is_err());
        assert!(store.load(a, "../x").is_err());
        assert!(store.load(b, &first).is_err(), "another root's snapshot");
        let no_temps = snapshot_files(&store.root_dir(a))
            .iter()
            .all(|(_, p)| p.extension().unwrap() == EXTENSION);
        assert!(no_temps);
    }

    #[test]
    fn keeps_the_newest_each_days_first_and_the_first() {
        let tmp = temp("prune");
        let store = Store::with_limits(tmp.0.clone(), 3, u64::MAX);
        let a = Path::new("/data/a");
        let d = DAY_MS;
        let times = [
            1000,
            2000,
            3000,
            d + 1000,
            d + 2000,
            2 * d + 1,
            2 * d + 2,
            2 * d + 3,
            2 * d + 4,
            2 * d + 5,
        ];
        for t in times {
            store.save(a, &snap("/data/a", t, 1), None).unwrap();
        }
        let kept = |s: &Store| -> Vec<u64> {
            let mut ids: Vec<u64> = s.list(a).into_iter().map(|(_, h)| h.created_ms).collect();
            ids.sort_unstable();
            ids
        };
        assert_eq!(
            kept(&store),
            [1000, d + 1000, 2 * d + 1, 2 * d + 3, 2 * d + 4, 2 * d + 5],
            "the first, each day's first, and the newest three"
        );

        // Over the total: daily ones go first, then the newest; the
        // root's first scan is kept longest.
        let size = fs::metadata(store.root_dir(a).join("1000.sbsnap"))
            .unwrap()
            .len();
        let tight = Store::with_limits(tmp.0.clone(), 3, size * 4 + size / 2);
        tight.save(a, &snap("/data/a", 2 * d + 6, 1), None).unwrap();
        assert_eq!(kept(&tight), [1000, 2 * d + 4, 2 * d + 5, 2 * d + 6]);

        let b = Path::new("/data/b");
        tight.save(b, &snap("/data/b", 3 * d, 1), None).unwrap();
        assert_eq!(kept(&tight), [1000, 2 * d + 5, 2 * d + 6], "from any root");
        assert_eq!(tight.list(b).len(), 1);

        let tiny = Store::with_limits(tmp.0.clone(), 3, 1);
        tiny.save(a, &snap("/data/a", 2 * d + 7, 1), None).unwrap();
        assert_eq!(
            kept(&tiny),
            [2 * d + 7],
            "the new one stays even over the limit"
        );
        assert!(tiny.list(b).is_empty());
    }

    #[test]
    fn a_snapshot_in_use_is_never_pruned() {
        let tmp = temp("protect");
        let store = Store::with_limits(tmp.0.clone(), 1, 1);
        let a = Path::new("/data/a");
        store.save(a, &snap("/data/a", 1000, 1), None).unwrap();
        store.save(a, &snap("/data/a", 2000, 1), None).unwrap();
        store
            .save(a, &snap("/data/a", 3000, 1), Some("2000"))
            .unwrap();
        let mut ids: Vec<_> = store.list(a).into_iter().map(|(id, _)| id).collect();
        ids.sort();
        assert_eq!(ids, ["2000", "3000"], "over the total, but in use");
    }

    #[test]
    fn save_makes_the_folder_again_if_it_vanishes() {
        let tmp = temp("vanish");
        let store = Store::new(tmp.0.clone());
        let a = Path::new("/data/a");
        store.save(a, &snap("/data/a", 1000, 1), None).unwrap();
        // As if another save's prune removed the folder while empty.
        fs::remove_dir_all(store.root_dir(a)).unwrap();
        store.save(a, &snap("/data/a", 2000, 1), None).unwrap();
        assert_eq!(store.list(a).len(), 1);
    }

    #[test]
    fn saves_through_a_temporary_file_and_clears_old_ones() {
        let tmp = temp("temps");
        let store = Store::new(tmp.0.clone());
        let a = Path::new("/data/a");
        store.save(a, &snap("/data/a", 1000, 1), None).unwrap();
        let dir = store.root_dir(a);
        // As if left by a crash, or being written by another save now.
        let stale = dir.join(".2000.sbsnap.1-0.partial");
        let old = dir.join("3000.tmp");
        let fresh = dir.join(".4000.sbsnap.1-0.partial");
        // A fixed name the new snapshot's temporary file might once have had.
        let taken = dir.join(".5000.sbsnap.partial");
        let hour_ago = SystemTime::now() - STALE_TEMP - Duration::from_secs(60);
        for (path, aged) in [
            (&stale, true),
            (&old, true),
            (&fresh, false),
            (&taken, false),
        ] {
            let mut f = File::create(path).unwrap();
            f.write_all(b"not ours").unwrap();
            if aged {
                f.set_modified(hour_ago).unwrap();
            }
        }
        store.save(a, &snap("/data/a", 5000, 1), None).unwrap();
        let mut left: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                ".4000.sbsnap.1-0.partial",
                ".5000.sbsnap.partial",
                "1000.sbsnap",
                "5000.sbsnap"
            ]
        );
        assert_eq!(fs::read(&taken).unwrap(), b"not ours");
        assert_eq!(fs::read(&fresh).unwrap(), b"not ours");
    }

    #[test]
    fn damaged_files_are_skipped_or_reported() {
        let tmp = temp("damaged");
        let store = Store::new(tmp.0.clone());
        let a = Path::new("/data/a");
        let id = store.save(a, &snap("/data/a", 1000, 1), None).unwrap();
        let path = store.root_dir(a).join(format!("{id}.sbsnap"));
        let bytes = fs::read(&path).unwrap();
        fs::write(&path, &bytes[..bytes.len() - 3]).unwrap();
        let err = store.load(a, &id).unwrap_err();
        assert!(err.starts_with("Couldn't read the earlier scan"), "{err}");
        // Its header is intact, so it's still listed.
        assert_eq!(store.list(a).len(), 1);
        fs::write(&path, b"junk").unwrap();
        assert!(store.list(a).is_empty());
        assert!(store.load(a, &id).is_err());
    }

    #[test]
    fn root_keys_are_normalized() {
        assert_eq!(
            root_key(Path::new("/data/a/")),
            root_key(Path::new("/data/a"))
        );
        assert_eq!(
            root_key(Path::new("/data/./b/../a/.")),
            root_key(Path::new("/data/a")),
            "`.` and `..` resolved by name"
        );
        assert_eq!(
            root_key(Path::new("/")).len(),
            1,
            "the root keeps its separator"
        );
        if cfg!(windows) {
            assert_eq!(root_key(Path::new(r"C:\")), r"C:\");
            assert_eq!(
                root_key(Path::new(r"c:\Users")),
                root_key(Path::new(r"C:\USERS\"))
            );
            assert_eq!(
                root_key(Path::new("C:/Users/me")),
                root_key(Path::new(r"C:\Users\me"))
            );
            assert_eq!(
                root_key(Path::new(r"\\?\C:\Users\me")),
                root_key(Path::new(r"C:\Users\me"))
            );
            assert_eq!(
                root_key(Path::new(r"\\?\UNC\server\share\x")),
                root_key(Path::new(r"\\server\share\x"))
            );
            assert_eq!(root_key(Path::new(r"C:\..")), root_key(Path::new(r"C:\")));
        }
        if snapshot::CASE_INSENSITIVE {
            // Folded like names, one character at a time: `ß` isn't `SS`.
            assert_ne!(
                root_key(Path::new("/data/stra\u{df}e")),
                root_key(Path::new("/data/STRASSE"))
            );
        }
    }
}
