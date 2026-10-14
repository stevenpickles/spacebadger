//! Saving a snapshot when a scan completes, and comparing the scan with an
//! earlier one ("what grew?").

use super::{MAX_PAGE, Session, display_path, node};
use crate::metrics::locked;
use crate::snapshots::Store;
use sb_core::compare::{Change, Comparison, Item, change_level};
use sb_core::snapshot::{self, Snapshot};
use sb_core::tree::{NodeId, Tree};
use sb_protocol as proto;
use sb_scan::ScanState;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock, TryLockError, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The error of a quick comparison query (an item's change, the map's
/// change marks) while another request matches the comparison again after
/// a delete. Rather than hold a worker of the query runner for as long as
/// that takes, it gives up; the delete announces its change once the
/// comparison is matched, and the interface asks again then.
pub const UPDATING: &str = "The comparison is being updated.";

/// How a request waits while another matches the comparison again.
#[derive(Clone, Copy, PartialEq)]
enum Wait {
    /// Gives up with [`UPDATING`] (quick queries).
    No,
    /// Waits, giving up if superseded (lists, which have to answer).
    Cooperatively,
    /// Waits (the delete matching it before announcing its change).
    Blocking,
}

/// Saves a snapshot of the scan once it completes, on its own thread so
/// neither the scanner nor the interface waits for the disk.
pub(super) struct Saver {
    pub store: Arc<Store>,
    pub root: PathBuf,
    /// Set once the scan has started.
    pub tree: Arc<OnceLock<Weak<RwLock<Tree>>>>,
    /// The id of the snapshot saved for this scan, left out of the earlier
    /// scans it can be compared with.
    pub saved: Arc<Mutex<Option<String>>>,
    /// A snapshot in use for comparing, which saving mustn't prune.
    pub protect: Option<String>,
}

/// An earlier scan in use for comparing, handed from one scan of a root to
/// the next, so a rescan keeps comparing with it even if its file is pruned.
#[derive(Clone)]
pub struct Carried {
    info: proto::SnapshotInfo,
    snapshot: Arc<Snapshot>,
}

impl Carried {
    pub fn id(&self) -> &str {
        &self.info.id
    }
}

impl Saver {
    /// Called with every progress update; acts on the one that completes
    /// the scan. Cancelled and failed scans are partial, so they're never
    /// saved.
    pub fn progress(&self, state: &ScanState) {
        if !matches!(
            state,
            ScanState::Complete | ScanState::CompleteWithOmissions
        ) {
            return;
        }
        let store = Arc::clone(&self.store);
        let root = self.root.clone();
        let tree = Arc::clone(&self.tree);
        let saved = Arc::clone(&self.saved);
        let protect = self.protect.clone();
        let spawned = std::thread::Builder::new()
            .name("sb-snapshot".into())
            .spawn(move || {
                // Gone if a newer scan replaced this one meanwhile.
                let Some(tree) = tree.wait().upgrade() else {
                    return;
                };
                let started = Instant::now();
                let snap = {
                    let Ok(tree) = tree.read() else {
                        return;
                    };
                    Snapshot::capture(&tree, now_ms(), snapshot::CASE_INSENSITIVE)
                };
                drop(tree);
                let captured = started.elapsed();
                match store.save(&root, &snap, protect.as_deref()) {
                    Ok(id) => {
                        if cfg!(debug_assertions) {
                            eprintln!(
                                "snapshot {id}: {} entries, captured in {captured:?}, saved in {:?}",
                                snap.len(),
                                started.elapsed()
                            );
                        }
                        *saved.lock().unwrap() = Some(id);
                    }
                    Err(e) => eprintln!("couldn't save a snapshot of {}: {e}", root.display()),
                }
            });
        if let Err(e) = spawned {
            eprintln!("couldn't start saving a snapshot: {e}");
        }
    }
}

pub(super) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The comparison that's on.
pub(super) struct Active {
    info: proto::SnapshotInfo,
    snapshot: Arc<Snapshot>,
    /// Matched for scan revision `revision`; rebuilt when the tree changes.
    revision: u64,
    comparison: Arc<Comparison>,
    /// The changes last listed: folder, files only, revision, and the list.
    changes: Option<(u32, bool, u64, Arc<Vec<Change>>)>,
}

/// What a request works with, taken from [`Active`] so the lock isn't held
/// while it's used.
struct Current {
    snapshot: Arc<Snapshot>,
    comparison: Arc<Comparison>,
    revision: u64,
}

impl Session {
    /// Earlier scans of this root that this one can be compared with,
    /// newest first.
    pub fn snapshots(&self) -> Vec<proto::SnapshotInfo> {
        let Some(store) = &self.snapshots else {
            return Vec::new();
        };
        let own = self.saved_snapshot.lock().unwrap().clone();
        store
            .list(&self.root)
            .into_iter()
            .filter(|(id, h)| Some(id) != own.as_ref() && h.created_ms < self.started_ms)
            .map(|(id, h)| info(id, &h))
            .collect()
    }

    /// Compares with snapshot `id`, or turns comparing off with `None`.
    /// Only a completed scan compares: a partial one would show everything
    /// it didn't reach as removed. Reading and matching take a while on a
    /// large tree; other requests keep being answered meanwhile.
    pub fn compare_set(&self, id: Option<&str>) -> Result<Option<proto::CompareSummary>, String> {
        let Some(id) = id else {
            *self
                .compare
                .lock()
                .map_err(|_| "comparing is unavailable")? = None;
            return Ok(None);
        };
        if !matches!(
            self.scan.progress().state,
            ScanState::Complete | ScanState::CompleteWithOmissions
        ) {
            return Err("Comparing is available once the scan completes.".into());
        }
        let store = self
            .snapshots
            .as_ref()
            .ok_or("Earlier scans aren't saved.")?;
        // The scan carried over from before a rescan is kept in memory.
        let snapshot = match &self.carried {
            Some(c) if c.info.id == id => Arc::clone(&c.snapshot),
            _ => Arc::new(store.load(&self.root, id)?),
        };
        let header = snapshot.header().clone();
        let (comparison, revision) = {
            let tree = self.scan.tree();
            let tree = locked("tree", || tree.read()).map_err(|_| "scan data is unavailable")?;
            let revision = self.scan.progress().revision;
            (Arc::new(Comparison::new(&tree, &snapshot)), revision)
        };
        let info = info(id.to_owned(), &header);
        *self
            .compare
            .lock()
            .map_err(|_| "comparing is unavailable")? = Some(Active {
            info: info.clone(),
            snapshot,
            revision,
            comparison,
            changes: None,
        });
        Ok(Some(proto::CompareSummary {
            generation: self.generation(),
            snapshot: info,
            threshold: header.threshold,
        }))
    }

    /// The earlier scan being compared with, to carry over to a rescan.
    pub fn carry(&self) -> Option<Carried> {
        let compare = self.compare.lock().ok()?;
        compare.as_ref().map(|a| Carried {
            info: a.info.clone(),
            snapshot: Arc::clone(&a.snapshot),
        })
    }

    /// How `id` changed since the snapshot.
    pub fn compare_item(&self, snapshot: &str, id: u32) -> Result<proto::ItemChange, String> {
        let tree = self.scan.tree();
        let tree = locked("tree", || tree.read()).map_err(|_| "scan data is unavailable")?;
        let cur = self.current(snapshot, &tree, Wait::No)?;
        let n = node(&tree, id)?;
        let change = cur.comparison.change(&tree, &cur.snapshot, n);
        Ok(proto::ItemChange {
            generation: self.generation(),
            node: id,
            kind: change.map_or(proto::ChangeKind::Unknown, |c| kind(&c)),
            before: change.and_then(|c| c.before),
            before_below: change.and_then(|c| c.below),
            after: tree.logical(n),
        })
    }

    /// One byte per node for the "Change" colors (see
    /// [`proto::change_mark`]).
    pub fn change_marks(&self, req: &proto::ChangeMarksRequest) -> Result<Vec<u8>, String> {
        use proto::change_mark as mark;
        let tree = self.scan.tree();
        let tree = locked("tree", || tree.read()).map_err(|_| "scan data is unavailable")?;
        let cur = self.current(&req.snapshot, &tree, Wait::No)?;
        Ok(req
            .nodes
            .iter()
            .map(|&id| {
                let n = NodeId::from_index(id);
                if !tree.contains(n) || tree.is_removed(n) {
                    return mark::UNKNOWN;
                }
                let Some(c) = cur.comparison.change(&tree, &cur.snapshot, n) else {
                    return mark::UNKNOWN;
                };
                if c.is_new() {
                    return mark::NEW;
                }
                let before = c.before.or(c.below).unwrap_or(0);
                let after = c.after.unwrap_or(0);
                if c.delta() == 0 {
                    mark::UNCHANGED
                } else if c.delta() > 0 {
                    mark::GROWN + change_level(before, after)
                } else {
                    mark::SHRUNK + change_level(before, after)
                }
            })
            .collect())
    }

    /// A page of the biggest changes under a folder, at any depth,
    /// including items removed since.
    pub fn changes(&self, req: &proto::ChangesRequest) -> Result<proto::ChangesPage, String> {
        let tree = self.scan.tree();
        let tree = locked("tree", || tree.read()).map_err(|_| "scan data is unavailable")?;
        let cur = self.current(&req.snapshot, &tree, Wait::Cooperatively)?;
        let folder = node(&tree, req.folder)?;
        let cached = {
            let compare = self
                .compare
                .lock()
                .map_err(|_| "comparing is unavailable")?;
            compare
                .as_ref()
                .and_then(|a| a.changes.as_ref())
                .filter(|(f, files, rev, _)| {
                    *f == req.folder && *files == req.files_only && *rev == cur.revision
                })
                .map(|c| Arc::clone(&c.3))
        };
        let list = match cached {
            Some(list) => list,
            None => {
                let list = Arc::new(cur.comparison.changes_under(
                    &tree,
                    &cur.snapshot,
                    folder,
                    req.files_only,
                ));
                let mut compare = self
                    .compare
                    .lock()
                    .map_err(|_| "comparing is unavailable")?;
                if let Some(a) = compare.as_mut().filter(|a| a.info.id == req.snapshot) {
                    a.changes = Some((req.folder, req.files_only, cur.revision, Arc::clone(&list)));
                }
                list
            }
        };
        let total = list.len();
        let start = (req.offset as usize).min(total);
        let end = start
            .saturating_add(req.limit.min(MAX_PAGE) as usize)
            .min(total);
        let base = tree.path(folder);
        let folder_entry = match cur.comparison.status(folder) {
            sb_core::compare::Status::Was(e) => Some(e),
            _ => None,
        };
        let snap = &cur.snapshot;
        let rows = list[start..end]
            .iter()
            .map(|c| match c.item {
                Item::Current(n) => {
                    let parent = tree.parent(n).unwrap_or(NodeId::ROOT);
                    let path = tree.path(parent);
                    let sub = path.strip_prefix(&base).unwrap_or(&path);
                    proto::ChangeRow {
                        node: Some(n.index() as u32),
                        parent: Some(parent.index() as u32),
                        name: tree.name(n).to_string_lossy().into_owned(),
                        folder: tree.is_dir(n),
                        subfolder: display_path(sub),
                        kind: kind(c),
                        before: c.before,
                        before_below: c.below,
                        after: c.after,
                    }
                }
                Item::Removed(e) => {
                    let mut names = folder_entry
                        .map(|f| Comparison::snapshot_path(snap, f, e))
                        .unwrap_or_default();
                    names.pop();
                    let sub: PathBuf = names.iter().collect();
                    proto::ChangeRow {
                        node: None,
                        parent: None,
                        name: snap.display_name(e),
                        folder: snap.is_dir(e),
                        subfolder: display_path(Path::new(&sub)),
                        kind: proto::ChangeKind::Removed,
                        before: c.before,
                        before_below: None,
                        after: None,
                    }
                }
            })
            .collect();
        Ok(proto::ChangesPage {
            generation: self.generation(),
            folder: req.folder,
            offset: start as u32,
            total: total as u32,
            rows,
        })
    }

    /// The comparison with snapshot `id`, matched against `tree` (read
    /// locked by the caller). When the tree changed since (items deleted),
    /// it's matched again without holding the comparison's lock, so other
    /// requests aren't kept waiting, and swapped in. Requests asking
    /// meanwhile wait as `wait` says.
    fn current(&self, id: &str, tree: &Tree, wait: Wait) -> Result<Current, String> {
        let revision = self.scan.progress().revision;
        if let Some(current) = self.matched_for(id, revision)? {
            return Ok(current);
        }
        // One request matches again; others asking meanwhile wait for it
        // and use its result, or give up.
        let _one = loop {
            let taken = match wait {
                Wait::Blocking => self.rematching.lock().map_err(|_| ()),
                _ => match self.rematching.try_lock() {
                    Ok(one) => Ok(one),
                    Err(TryLockError::Poisoned(_)) => Err(()),
                    Err(TryLockError::WouldBlock) if wait == Wait::No => {
                        return Err(UPDATING.to_owned());
                    }
                    Err(TryLockError::WouldBlock) => {
                        crate::runner::check()?;
                        std::thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                },
            };
            break taken.map_err(|()| "comparing is unavailable")?;
        };
        if let Some(current) = self.matched_for(id, revision)? {
            return Ok(current);
        }
        let snapshot = {
            let compare = self
                .compare
                .lock()
                .map_err(|_| "comparing is unavailable")?;
            let active = compare
                .as_ref()
                .filter(|a| a.info.id == id)
                .ok_or_else(gone)?;
            Arc::clone(&active.snapshot)
        };
        let comparison = Arc::new(Comparison::new(tree, &snapshot));
        let mut compare = self
            .compare
            .lock()
            .map_err(|_| "comparing is unavailable")?;
        let active = compare
            .as_mut()
            .filter(|a| a.info.id == id)
            .ok_or_else(gone)?;
        active.comparison = Arc::clone(&comparison);
        active.revision = revision;
        Ok(Current {
            snapshot,
            comparison,
            revision,
        })
    }

    /// Matches the comparison that's on again after the tree changed, so
    /// the requests that follow find it ready instead of the first of them,
    /// such as a tooltip, doing it.
    pub(super) fn rematch(&self) {
        let Some(id) = self
            .compare
            .lock()
            .ok()
            .and_then(|c| c.as_ref().map(|a| a.info.id.clone()))
        else {
            return;
        };
        let tree = self.scan.tree();
        if let Ok(tree) = locked("tree", || tree.read()) {
            // Gone means comparing was turned off meanwhile.
            let _ = self.current(&id, &tree, Wait::Blocking);
        }
    }

    /// The comparison with snapshot `id` if it's matched for `revision`.
    fn matched_for(&self, id: &str, revision: u64) -> Result<Option<Current>, String> {
        let compare = self
            .compare
            .lock()
            .map_err(|_| "comparing is unavailable")?;
        let active = compare
            .as_ref()
            .filter(|a| a.info.id == id)
            .ok_or_else(gone)?;
        Ok((active.revision == revision).then(|| Current {
            snapshot: Arc::clone(&active.snapshot),
            comparison: Arc::clone(&active.comparison),
            revision,
        }))
    }
}

fn gone() -> String {
    "That comparison is no longer on.".to_owned()
}

fn info(id: String, h: &snapshot::SnapshotHeader) -> proto::SnapshotInfo {
    proto::SnapshotInfo {
        id,
        created_ms: h.created_ms,
        logical: h.logical,
        files: h.files,
    }
}

fn kind(c: &Change) -> proto::ChangeKind {
    if c.after.is_none() {
        return proto::ChangeKind::Removed;
    }
    if c.is_new() {
        return proto::ChangeKind::New;
    }
    match c.delta() {
        d if d > 0 => proto::ChangeKind::Grown,
        d if d < 0 => proto::ChangeKind::Shrunk,
        _ => proto::ChangeKind::Unchanged,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    struct TempDir(PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp(name: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!("sb-compare-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    const MB: usize = 1 << 20;

    /// Scans `root` to the end and waits for its snapshot.
    fn scan(root: &Path, store: &Arc<Store>) -> Session {
        scan_carrying(root, store, None)
    }

    fn scan_carrying(root: &Path, store: &Arc<Store>, carried: Option<Carried>) -> Session {
        let session =
            Session::start_with(root.to_owned(), Some(Arc::clone(store)), carried, |_| {});
        let deadline = Instant::now() + Duration::from_secs(20);
        while session.saved_snapshot.lock().unwrap().is_none() {
            assert!(Instant::now() < deadline, "no snapshot saved");
            std::thread::sleep(Duration::from_millis(5));
        }
        session
    }

    fn find(session: &Session, path: &str) -> u32 {
        let tree = session.scan.tree();
        let tree = tree.read().unwrap();
        let mut n = NodeId::ROOT;
        for part in path.split('/') {
            n = tree.children(n).find(|&c| tree.name(c) == part).unwrap();
        }
        n.index() as u32
    }

    #[test]
    fn saves_a_snapshot_and_compares_the_next_scan_with_it() {
        let tmp = temp("rescan");
        let data = tmp.0.join("data");
        std::fs::create_dir_all(data.join("logs")).unwrap();
        std::fs::write(data.join("logs").join("app.log"), vec![0; 2 * MB]).unwrap();
        std::fs::write(data.join("old.bin"), vec![0; 3 * MB]).unwrap();
        std::fs::write(data.join("tiny.txt"), b"x").unwrap();
        std::fs::write(data.join("grows.txt"), b"x").unwrap();
        let store = Arc::new(Store::new(tmp.0.join("store")));

        let first = scan(&data, &store);
        assert!(
            first.snapshots().is_empty(),
            "its own snapshot isn't offered"
        );
        drop(first);

        std::fs::write(data.join("logs").join("app.log"), vec![0; 5 * MB]).unwrap();
        std::fs::remove_file(data.join("old.bin")).unwrap();
        std::fs::write(data.join("new.iso"), vec![0; MB]).unwrap();
        std::fs::write(data.join("grows.txt"), vec![0; 3 * MB]).unwrap();
        let second = scan(&data, &store);
        let earlier = second.snapshots();
        assert_eq!(earlier.len(), 1);
        assert_eq!(earlier[0].logical, 5 * MB as u64 + 2);

        let id = earlier[0].id.clone();
        let summary = second.compare_set(Some(&id)).unwrap().unwrap();
        assert_eq!(summary.snapshot.id, id);

        let log = find(&second, "logs/app.log");
        let change = second.compare_item(&id, log).unwrap();
        assert_eq!(change.kind, proto::ChangeKind::Grown);
        assert_eq!(change.before, Some(2 * MB as u64));
        let tiny = find(&second, "tiny.txt");
        assert_eq!(
            second.compare_item(&id, tiny).unwrap().kind,
            proto::ChangeKind::Unknown
        );
        // Small then, big now: grown by at least the difference.
        let grows = second
            .compare_item(&id, find(&second, "grows.txt"))
            .unwrap();
        assert_eq!(grows.kind, proto::ChangeKind::Grown);
        assert_eq!((grows.before, grows.before_below), (None, Some(MB as u64)));
        let new = find(&second, "new.iso");
        let marks = second
            .change_marks(&proto::ChangeMarksRequest {
                generation: second.generation(),
                snapshot: id.clone(),
                nodes: vec![log, new, tiny, 0, u32::MAX],
            })
            .unwrap();
        use proto::change_mark as mark;
        assert_eq!(
            marks,
            [
                mark::GROWN + 4,
                mark::NEW,
                mark::UNKNOWN,
                mark::GROWN + 3,
                mark::UNKNOWN
            ]
        );

        let page = second
            .changes(&proto::ChangesRequest {
                generation: second.generation(),
                snapshot: id.clone(),
                folder: 0,
                files_only: false,
                offset: 0,
                limit: 10,
            })
            .unwrap();
        let rows: Vec<_> = page
            .rows
            .iter()
            .map(|r| (r.name.as_str(), r.kind, r.node.is_some()))
            .collect();
        assert_eq!(
            rows,
            [
                ("logs", proto::ChangeKind::Grown, true),
                ("app.log", proto::ChangeKind::Grown, true),
                ("old.bin", proto::ChangeKind::Removed, false),
                ("grows.txt", proto::ChangeKind::Grown, true),
                ("new.iso", proto::ChangeKind::New, true),
            ]
        );
        assert_eq!(page.rows[1].subfolder, "logs");
        assert!(
            second.compare_item("123", log).is_err(),
            "not the comparison that's on"
        );

        // After a delete, the scan is matched again (once, by whichever
        // request comes first); quick queries asking meanwhile don't wait.
        let logs = find(&second, "logs");
        second
            .scan
            .edit_finished(|t| t.remove(NodeId::from_index(log)))
            .unwrap();
        let results: Vec<_> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..4)
                .map(|_| s.spawn(|| second.compare_item(&id, logs)))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert!(results.iter().any(Result::is_ok));
        for c in results {
            match c {
                Ok(c) => assert_eq!((c.kind, c.after), (proto::ChangeKind::Shrunk, 0)),
                Err(e) => assert_eq!(e, UPDATING),
            }
        }
        let c = second.compare_item(&id, logs).unwrap();
        assert_eq!((c.kind, c.after), (proto::ChangeKind::Shrunk, 0));

        // While another request matches it again, quick queries give up at
        // once, and lists wait unless superseded.
        let tiny = NodeId::from_index(find(&second, "tiny.txt"));
        let (_, progress) = second.scan.edit_finished(|t| t.remove(tiny)).unwrap();
        {
            let _matching = second.rematching.lock().unwrap();
            assert_eq!(second.compare_item(&id, logs).unwrap_err(), UPDATING);
            let marks = second.change_marks(&proto::ChangeMarksRequest {
                generation: second.generation(),
                snapshot: id.clone(),
                nodes: vec![logs],
            });
            assert_eq!(marks.unwrap_err(), UPDATING);
            let changes = proto::ChangesRequest {
                generation: second.generation(),
                snapshot: id.clone(),
                folder: 0,
                files_only: false,
                offset: 0,
                limit: 10,
            };
            let stopped = crate::runner::as_superseded(|| second.changes(&changes));
            assert_eq!(stopped.unwrap_err(), proto::SUPERSEDED);
        }
        second.rematch();
        assert!(
            second
                .matched_for(&id, progress.revision)
                .unwrap()
                .is_some()
        );
        // A delete matches it again itself, before announcing the change.
        let grows = NodeId::from_index(find(&second, "grows.txt"));
        let (_, progress) = second.scan.edit_finished(|t| t.remove(grows)).unwrap();
        assert!(
            second
                .matched_for(&id, progress.revision)
                .unwrap()
                .is_none()
        );
        second.rematch();
        assert!(
            second
                .matched_for(&id, progress.revision)
                .unwrap()
                .is_some()
        );

        // A rescan carries the earlier scan over, even once its file is gone.
        let carried = second.carry().expect("comparing");
        assert_eq!(carried.id(), id);
        for (file_id, _) in store.list(&data) {
            if file_id == id {
                let dir = tmp.0.join("store");
                for d in std::fs::read_dir(&dir).unwrap().flatten() {
                    let _ = std::fs::remove_file(d.path().join(format!("{id}.sbsnap")));
                }
            }
        }
        assert!(store.list(&data).iter().all(|(f, _)| *f != id));
        let third = scan_carrying(&data, &store, Some(carried));
        assert!(third.compare_set(Some(&id)).unwrap().is_some());
        let log = find(&third, "logs/app.log");
        assert_eq!(
            third.compare_item(&id, log).unwrap().before,
            Some(2 * MB as u64)
        );

        second.compare_set(None).unwrap();
        assert!(second.compare_item(&id, log).is_err());
        assert!(second.carry().is_none());
    }
}
