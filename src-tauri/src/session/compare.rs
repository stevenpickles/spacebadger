//! Saving a snapshot when a scan completes, and comparing the scan with an
//! earlier one ("what grew?").

use super::{MAX_PAGE, Session, display_path, node};
use crate::snapshots::Store;
use sb_core::compare::{Change, Comparison, Item, change_level};
use sb_core::snapshot::{self, Snapshot};
use sb_core::tree::{NodeId, Tree};
use sb_protocol as proto;
use sb_scan::ScanState;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

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
                match store.save(&root, &snap) {
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
    threshold: u64,
    snapshot: Snapshot,
    /// Matched for this scan revision; rebuilt when the tree changes.
    revision: u64,
    comparison: Comparison,
    /// The changes last listed: folder, files only, and the list.
    changes: Option<(u32, bool, Vec<Change>)>,
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
    /// it didn't reach as removed.
    pub fn compare_set(&self, id: Option<&str>) -> Result<Option<proto::CompareSummary>, String> {
        let Some(id) = id else {
            *self
                .compare
                .lock()
                .map_err(|_| "comparing is unavailable")? = None;
            return Ok(None);
        };
        let progress = self.scan.progress();
        if !matches!(
            progress.state,
            ScanState::Complete | ScanState::CompleteWithOmissions
        ) {
            return Err("Comparing is available once the scan completes.".into());
        }
        let store = self
            .snapshots
            .as_ref()
            .ok_or("Earlier scans aren't saved.")?;
        let snapshot = store.load(&self.root, id)?;
        let header = snapshot.header().clone();
        let mut compare = self
            .compare
            .lock()
            .map_err(|_| "comparing is unavailable")?;
        let tree = self.scan.tree();
        let tree = tree.read().map_err(|_| "scan data is unavailable")?;
        let comparison = Comparison::new(&tree, &snapshot);
        let active = compare.insert(Active {
            info: info(id.to_owned(), &header),
            threshold: header.threshold,
            snapshot,
            revision: progress.revision,
            comparison,
            changes: None,
        });
        Ok(Some(proto::CompareSummary {
            generation: self.generation(),
            snapshot: active.info.clone(),
            threshold: active.threshold,
        }))
    }

    /// How `id` changed since the snapshot.
    pub fn compare_item(&self, snapshot: &str, id: u32) -> Result<proto::ItemChange, String> {
        let mut compare = self
            .compare
            .lock()
            .map_err(|_| "comparing is unavailable")?;
        let tree = self.scan.tree();
        let tree = tree.read().map_err(|_| "scan data is unavailable")?;
        let active = self.current_comparison(&mut compare, snapshot, &tree)?;
        let n = node(&tree, id)?;
        let change = active.comparison.change(&tree, &active.snapshot, n);
        Ok(proto::ItemChange {
            generation: self.generation(),
            node: id,
            kind: change.map_or(proto::ChangeKind::Unknown, |c| kind(&c)),
            before: change.and_then(|c| c.before),
            after: tree.logical(n),
        })
    }

    /// One byte per node for the "Change" colors (see
    /// [`proto::change_mark`]).
    pub fn change_marks(&self, req: &proto::ChangeMarksRequest) -> Result<Vec<u8>, String> {
        use proto::change_mark as mark;
        let mut compare = self
            .compare
            .lock()
            .map_err(|_| "comparing is unavailable")?;
        let tree = self.scan.tree();
        let tree = tree.read().map_err(|_| "scan data is unavailable")?;
        let active = self.current_comparison(&mut compare, &req.snapshot, &tree)?;
        Ok(req
            .nodes
            .iter()
            .map(|&id| {
                let n = NodeId::from_index(id);
                if !tree.contains(n) || tree.is_removed(n) {
                    return mark::UNKNOWN;
                }
                match active.comparison.change(&tree, &active.snapshot, n) {
                    None => mark::UNKNOWN,
                    Some(Change { before: None, .. }) => mark::NEW,
                    Some(Change {
                        before: Some(b),
                        after: Some(a),
                        ..
                    }) if a != b => {
                        let level = change_level(b, a);
                        if a > b {
                            mark::GROWN + level
                        } else {
                            mark::SHRUNK + level
                        }
                    }
                    Some(_) => mark::UNCHANGED,
                }
            })
            .collect())
    }

    /// A page of the biggest changes under a folder, at any depth,
    /// including items removed since.
    pub fn changes(&self, req: &proto::ChangesRequest) -> Result<proto::ChangesPage, String> {
        let mut compare = self
            .compare
            .lock()
            .map_err(|_| "comparing is unavailable")?;
        let tree = self.scan.tree();
        let tree = tree.read().map_err(|_| "scan data is unavailable")?;
        let active = self.current_comparison(&mut compare, &req.snapshot, &tree)?;
        let folder = node(&tree, req.folder)?;
        let cached = matches!(&active.changes, Some((f, files, _)) if *f == req.folder && *files == req.files_only);
        if !cached {
            let list =
                active
                    .comparison
                    .changes_under(&tree, &active.snapshot, folder, req.files_only);
            active.changes = Some((req.folder, req.files_only, list));
        }
        let list = &active.changes.as_ref().expect("just listed").2;
        let total = list.len();
        let start = (req.offset as usize).min(total);
        let end = start
            .saturating_add(req.limit.min(MAX_PAGE) as usize)
            .min(total);
        let base = tree.path(folder);
        let folder_entry = match active.comparison.status(folder) {
            sb_core::compare::Status::Was(e) => Some(e),
            _ => None,
        };
        let snap = &active.snapshot;
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

    /// The comparison if it is with snapshot `id`, matched again if the
    /// tree changed (items deleted) since.
    fn current_comparison<'a>(
        &self,
        compare: &'a mut Option<Active>,
        id: &str,
        tree: &Tree,
    ) -> Result<&'a mut Active, String> {
        let active = compare
            .as_mut()
            .filter(|a| a.info.id == id)
            .ok_or("That comparison is no longer on.")?;
        let revision = self.scan.progress().revision;
        if active.revision != revision {
            active.comparison = Comparison::new(tree, &active.snapshot);
            active.revision = revision;
            active.changes = None;
        }
        Ok(active)
    }
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
    match (c.before, c.after) {
        (None, _) => proto::ChangeKind::New,
        (_, None) => proto::ChangeKind::Removed,
        (Some(b), Some(a)) if a > b => proto::ChangeKind::Grown,
        (Some(b), Some(a)) if a < b => proto::ChangeKind::Shrunk,
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
        let session = Session::start_with(root.to_owned(), Some(Arc::clone(store)), |_| {});
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
        let second = scan(&data, &store);
        let earlier = second.snapshots();
        assert_eq!(earlier.len(), 1);
        assert_eq!(earlier[0].logical, 5 * MB as u64 + 1);

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
                mark::GROWN + 1,
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
                ("new.iso", proto::ChangeKind::New, true),
            ]
        );
        assert_eq!(page.rows[1].subfolder, "logs");
        assert!(
            second.compare_item("123", log).is_err(),
            "not the comparison that's on"
        );

        second.compare_set(None).unwrap();
        assert!(second.compare_item(&id, log).is_err());
    }
}
