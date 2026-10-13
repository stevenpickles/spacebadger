//! Snapshots of completed scans, kept in the app's data folder so a later
//! scan of the same folder can be compared with them.
//!
//! Each scan root has its own folder, named by a hash of the root's path
//! (ignoring case on Windows), holding `<finished ms>.sbsnap` files in the
//! format of `sb_core::snapshot`. A snapshot is written to a temporary file
//! and renamed into place, so a crash leaves either the whole file or none.
//! After each save, only the newest [`KEEP_PER_ROOT`] per root are kept,
//! and the oldest of all go until the total fits [`MAX_TOTAL_BYTES`].

use sb_core::snapshot::{self, Snapshot, SnapshotHeader};
use std::fs::{self, File};
use std::io::{self, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Snapshots kept per scan root.
pub const KEEP_PER_ROOT: usize = 10;

/// Room for all snapshots together.
pub const MAX_TOTAL_BYTES: u64 = 512 << 20;

const EXTENSION: &str = "sbsnap";
const TEMP_EXTENSION: &str = "tmp";

/// Temporary files older than this are left over from a crash.
const STALE_TEMP: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
    keep_per_root: usize,
    max_total: u64,
}

impl Store {
    pub fn new(dir: PathBuf) -> Self {
        Self::with_limits(dir, KEEP_PER_ROOT, MAX_TOTAL_BYTES)
    }

    pub fn with_limits(dir: PathBuf, keep_per_root: usize, max_total: u64) -> Self {
        Self {
            dir,
            keep_per_root: keep_per_root.max(1),
            max_total,
        }
    }

    fn root_dir(&self, root: &Path) -> PathBuf {
        self.dir
            .join(format!("{:016x}", fnv64(root_key(root).as_bytes())))
    }

    /// Writes `snap` of `root` and prunes old snapshots; returns the new
    /// snapshot's id.
    pub fn save(&self, root: &Path, snap: &Snapshot) -> io::Result<String> {
        let dir = self.root_dir(root);
        fs::create_dir_all(&dir)?;
        let bytes = snap.encode();
        let base = snap.header().created_ms.to_string();
        let mut id = base.clone();
        let mut n = 1;
        while dir.join(format!("{id}.{EXTENSION}")).exists() {
            id = format!("{base}-{n}");
            n += 1;
        }
        let temp = dir.join(format!("{id}.{TEMP_EXTENSION}"));
        let path = dir.join(format!("{id}.{EXTENSION}"));
        let written = (|| {
            let mut file = File::create(&temp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temp, &path)
        })();
        if let Err(e) = written {
            let _ = fs::remove_file(&temp);
            return Err(e);
        }
        self.prune(&path);
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

    /// Keeps the newest snapshots per root, then removes the oldest of all
    /// until they fit the total. `keep` (the one just written) always stays.
    fn prune(&self, keep: &Path) {
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
            let mut files: Vec<(u64, PathBuf, u64)> = snapshot_files(&dir)
                .into_iter()
                .map(|(id, path)| {
                    let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                    (created_of(&id), path, size)
                })
                .collect();
            files.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
            for (i, file) in files.into_iter().enumerate() {
                if i >= self.keep_per_root && file.1 != keep {
                    let _ = fs::remove_file(&file.1);
                } else {
                    all.push(file);
                }
            }
            // Only removes a folder that's empty.
            let _ = fs::remove_dir(&dir);
        }
        all.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        let mut total: u64 = all.iter().map(|f| f.2).sum();
        for (_, path, size) in all {
            if total <= self.max_total {
                break;
            }
            if path != keep && fs::remove_file(&path).is_ok() {
                total -= size;
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

fn remove_stale_temps(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().is_none_or(|x| x != TEMP_EXTENSION) {
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

/// The root as compared: its path without trailing separators, and on
/// Windows in one case, as the file system compares it.
fn root_key(root: &Path) -> String {
    let text = root.to_string_lossy();
    let trimmed = text.trim_end_matches(['/', '\\']);
    // "C:\" and "/" keep their separator.
    let text = if trimmed.is_empty() || trimmed.ends_with(':') {
        &text
    } else {
        trimmed
    };
    if snapshot::CASE_INSENSITIVE {
        text.to_uppercase()
    } else {
        text.to_owned()
    }
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
        let first = store.save(a, &snap("/data/a", 1000, 1)).unwrap();
        let second = store.save(a, &snap("/data/a", 2000, 2)).unwrap();
        store.save(b, &snap("/data/b", 1500, 3)).unwrap();
        let again = store.save(a, &snap("/data/a", 2000, 4)).unwrap();
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
    fn keeps_the_newest_per_root_and_fits_the_total() {
        let tmp = temp("prune");
        let store = Store::with_limits(tmp.0.clone(), 3, u64::MAX);
        let a = Path::new("/data/a");
        for t in 1..=5 {
            store.save(a, &snap("/data/a", t * 1000, t)).unwrap();
        }
        let ids: Vec<_> = store.list(a).into_iter().map(|(id, _)| id).collect();
        assert_eq!(ids, ["5000", "4000", "3000"]);

        // A total that fits about two snapshots: the oldest go first, from
        // any root, but never the one just written.
        let size = fs::metadata(store.root_dir(a).join("5000.sbsnap"))
            .unwrap()
            .len();
        let tight = Store::with_limits(tmp.0.clone(), 3, size * 2 + size / 2);
        let b = Path::new("/data/b");
        tight.save(b, &snap("/data/b", 6000, 6)).unwrap();
        let left: Vec<_> = tight.list(a).into_iter().map(|(id, _)| id).collect();
        assert_eq!(left, ["5000"]);
        assert_eq!(tight.list(b).len(), 1);

        let tiny = Store::with_limits(tmp.0.clone(), 3, 1);
        tiny.save(a, &snap("/data/a", 7000, 7)).unwrap();
        let left: Vec<_> = tiny.list(a).into_iter().map(|(id, _)| id).collect();
        assert_eq!(left, ["7000"], "the new one stays even over the limit");
        assert!(tiny.list(b).is_empty());
    }

    #[test]
    fn damaged_files_are_skipped_or_reported() {
        let tmp = temp("damaged");
        let store = Store::new(tmp.0.clone());
        let a = Path::new("/data/a");
        let id = store.save(a, &snap("/data/a", 1000, 1)).unwrap();
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
    fn root_keys_ignore_trailing_separators() {
        assert_eq!(
            root_key(Path::new("/data/a/")),
            root_key(Path::new("/data/a"))
        );
        assert_eq!(root_key(Path::new("/")), "/");
        if cfg!(windows) {
            assert_eq!(root_key(Path::new(r"C:\")), r"C:\");
            assert_eq!(
                root_key(Path::new(r"c:\Users")),
                root_key(Path::new(r"C:\USERS\"))
            );
        }
    }
}
