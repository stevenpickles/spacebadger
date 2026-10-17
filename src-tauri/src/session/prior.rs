//! The previous scan of the root, read in the background as a rescan
//! starts, so the map can show the folders not yet listed at the size they
//! had then (see `sb_core::prior`). Only the map's areas use it.
//!
//! The scanner owns the tree's measured sizes; the estimates are kept apart
//! from them, so handing the tree its prior from another thread (under the
//! write lock, like a delete's edits) can't disturb the scan.

use crate::snapshots::Store;
use sb_core::prior::Prior;
use sb_core::tree::Tree;
use sb_scan::ScanState;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, RwLock, Weak};

/// Hands the newest earlier snapshot of the root to the scan's tree, and
/// takes it back when the scan ends.
pub(super) struct Estimates {
    /// Set once the scan has started.
    tree: Arc<OnceLock<Weak<RwLock<Tree>>>>,
    /// Set, with the tree's write lock held, when the scan ends: a prior
    /// read after that isn't given to the tree.
    finished: Arc<AtomicBool>,
}

impl Estimates {
    /// Starts reading the newest snapshot of `root` finished before
    /// `before_ms`, on its own thread so the scan doesn't wait for it, and
    /// gives it to the tree as soon as it's read.
    pub fn start(
        store: Arc<Store>,
        root: PathBuf,
        before_ms: u64,
        tree: Arc<OnceLock<Weak<RwLock<Tree>>>>,
    ) -> Self {
        let finished = Arc::new(AtomicBool::new(false));
        let (cell, done) = (Arc::clone(&tree), Arc::clone(&finished));
        let spawned = std::thread::Builder::new()
            .name("sb-prior".into())
            .spawn(move || {
                let Some((id, _)) = store
                    .list(&root)
                    .into_iter()
                    .find(|(_, h)| h.created_ms < before_ms)
                else {
                    return;
                };
                // An unreadable snapshot just means no estimates.
                let Ok(snap) = store.load(&root, &id) else {
                    return;
                };
                let prior = Arc::new(Prior::new(snap));
                // Gone if a newer scan replaced this one meanwhile.
                let Some(tree) = cell.wait().upgrade() else {
                    return;
                };
                let Ok(mut tree) = tree.write() else {
                    return;
                };
                if !done.load(Ordering::SeqCst) {
                    tree.set_prior(prior);
                }
            });
        if let Err(e) = spawned {
            eprintln!("couldn't start reading the previous scan: {e}");
        }
        Self { tree, finished }
    }

    /// Called on the scanner's thread with every progress update, with no
    /// lock held: once the scan ends, takes the prior away (by then nothing
    /// is estimated, so this only frees its memory).
    pub fn progress(&self, state: &ScanState) {
        if !state.is_finished() || self.finished.load(Ordering::SeqCst) {
            return;
        }
        let Some(tree) = self.tree.get().and_then(Weak::upgrade) else {
            self.finished.store(true, Ordering::SeqCst);
            return;
        };
        if let Ok(mut tree) = tree.write() {
            self.finished.store(true, Ordering::SeqCst);
            tree.clear_prior();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Session, compare::now_ms};
    use crate::snapshots::Store;
    use sb_core::size::Allocated;
    use sb_core::snapshot::{CASE_INSENSITIVE, Snapshot};
    use sb_core::tree::{DirState, FileSizes, NodeId, Tree};
    use sb_protocol as proto;
    use sb_protocol::wire::{HEADER_LEN, LAYOUT_PROVISIONAL, RECT_LEN, RECT_PROVISIONAL};
    use sb_scan::ScanConfig;
    use sb_scan::fake::FakeFs;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    struct TempDir(PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting until {what}");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn node(session: &Session, name: &str) -> u32 {
        let tree = session.scan.tree();
        let tree = tree.read().unwrap();
        tree.children(NodeId::ROOT)
            .find(|&c| tree.name(c) == name)
            .unwrap()
            .index() as u32
    }

    type Rect = (u32, u64, u8);

    /// The layout's header flags, estimate time and total, and each
    /// rectangle's node, weight and more-flags byte.
    fn layout(session: &Session) -> (u32, u32, u64, Vec<Rect>) {
        let bytes = session
            .layout(&proto::LayoutRequest {
                generation: session.generation(),
                request: 1,
                view: 0,
                metric: proto::Metric::Logical,
                width: 800.0,
                height: 600.0,
                search: None,
            })
            .unwrap();
        let word = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        let total = u64::from_le_bytes(bytes[32..40].try_into().unwrap());
        let count = word(40) as usize;
        let rects = bytes[HEADER_LEN..][..count * RECT_LEN]
            .chunks_exact(RECT_LEN)
            .map(|r| {
                (
                    u32::from_le_bytes(r[16..20].try_into().unwrap()),
                    u64::from_le_bytes(r[24..32].try_into().unwrap()),
                    r[40],
                )
            })
            .collect();
        (word(48), word(52), total, rects)
    }

    fn rect(rects: &[Rect], node: u32) -> Rect {
        *rects.iter().find(|r| r.0 == node).unwrap()
    }

    #[test]
    fn a_rescan_draws_unlisted_folders_from_the_last_snapshot_until_listed() {
        let tmp = TempDir(std::env::temp_dir().join(format!("sb-prior-{}", std::process::id())));
        let _ = std::fs::remove_dir_all(&tmp.0);
        let store = Arc::new(Store::new(tmp.0.join("store")));
        let fs = FakeFs::new();
        fs.file("a/x", 100, 4096);
        fs.file("b/y", 200, 4096);
        let gate = fs.gate_dir("b");

        // The last scan of the root found b much bigger.
        let mut earlier = Tree::new(fs.root());
        for (name, bytes) in [("a", 100), ("b", 400_000)] {
            let d = earlier.add_dir(NodeId::ROOT, name.as_ref()).unwrap();
            let sizes = FileSizes {
                logical: bytes,
                allocated: Allocated::known(bytes),
            };
            earlier.add_file(d, "f".as_ref(), sizes, None, 0).unwrap();
            earlier.set_dir_state(d, DirState::Listed);
        }
        earlier.set_dir_state(NodeId::ROOT, DirState::Listed);
        let created_ms = now_ms() - 60_000;
        let snap = Snapshot::capture(&earlier, created_ms, CASE_INSENSITIVE);
        store.save(&fs.root(), &snap, None).unwrap();

        let config = ScanConfig {
            progress_interval: Duration::from_millis(1),
            ..ScanConfig::default()
        };
        let session = Session::start_on(fs.root(), fs, config, Some(store), None, |_| {});
        until("the estimates are in and a is listed", || {
            let tree = session.scan.tree();
            let tree = tree.read().unwrap();
            tree.is_provisional(NodeId::ROOT)
                && tree
                    .children(NodeId::ROOT)
                    .any(|c| tree.name(c) == "a" && tree.dir_state(c) == DirState::Listed)
        });
        let (a, b) = (node(&session, "a"), node(&session, "b"));
        let (flags, from, total, rects) = layout(&session);
        assert_ne!(flags & LAYOUT_PROVISIONAL, 0);
        assert_eq!(u64::from(from), created_ms / 1000);
        assert_eq!(total, 100, "the view's total is measured");
        assert_eq!(rect(&rects, b), (b, 0, RECT_PROVISIONAL));
        assert_eq!(rect(&rects, a), (a, 100, 0));
        let details = |n: u32| session.details(n, None).unwrap();
        assert_eq!(details(b).estimated_from, Some(created_ms));
        assert_eq!(details(a).estimated_from, None);
        assert_eq!(
            (details(0).logical, details(0).estimated_from),
            (100, Some(created_ms))
        );
        until("the status counts a", || session.status().logical >= 100);
        assert_eq!(session.status().logical, 100, "measured only");

        gate.release();
        until("the scan finishes", || {
            session.scan.progress().state.is_finished()
        });
        let (flags, from, total, rects) = layout(&session);
        assert_eq!((flags & LAYOUT_PROVISIONAL, from, total), (0, 0, 300));
        assert!(rects.iter().all(|r| r.2 == 0));
        assert_eq!(rect(&rects, b).1, 200);
        assert_eq!(details(b).estimated_from, None);
        assert_eq!(session.scan.tree().read().unwrap().prior_created_ms(), None);
    }
}
