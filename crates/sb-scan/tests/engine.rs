use sb_core::size::Allocated;
use sb_core::tree::{DirState, NodeId, Tree, fingerprint, flags};
use sb_scan::fake::{FakeFs, Gate};
use sb_scan::{
    BATCH_ENTRIES, EntryKind, ErrorKind, GIT_GRACE, HoldProbe, OmissionReason, Progress, RootKind,
    Scan, ScanConfig, ScanState,
};
use std::path::Path;
use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

fn config() -> ScanConfig {
    ScanConfig {
        workers: 3,
        progress_interval: Duration::from_millis(10),
        sample_limit: 2,
        write_holds: None,
        git_grace: GIT_GRACE,
        ..ScanConfig::default()
    }
}

/// Scans to completion and keeps the tree for inspection.
fn run(fs: &FakeFs) -> (Progress, Arc<std::sync::RwLock<Tree>>) {
    let s = Scan::start(fs.root(), fs.clone(), config(), |_| {});
    let tree = s.tree();
    (s.wait(), tree)
}

/// How long a test waits for something that should happen at once before
/// it fails instead of hanging; far more than any delay a loaded machine
/// adds.
const HANG: Duration = Duration::from_secs(60);

/// Holds a gated listing in its OS call until [`Returned::finish`], or for
/// [`HANG`] if that never comes, so a scan that waits for the call ends the
/// test with a failure rather than a hang.
struct Returned {
    finished: Sender<()>,
    watchdog: JoinHandle<bool>,
}

impl Returned {
    /// Releases the listing; whether it was still held until now.
    fn finish(self) -> bool {
        let _ = self.finished.send(());
        self.watchdog.join().unwrap()
    }
}

fn release_after_returning(gate: Gate) -> Returned {
    let (finished, rx) = channel();
    let watchdog = std::thread::spawn(move || {
        let held = !matches!(rx.recv_timeout(HANG), Err(RecvTimeoutError::Timeout));
        gate.release();
        held
    });
    Returned { finished, watchdog }
}

fn find(tree: &Tree, path: &str) -> NodeId {
    let mut node = NodeId::ROOT;
    for part in path.split('/') {
        node = tree
            .children(node)
            .find(|&c| tree.name(c) == part)
            .unwrap_or_else(|| panic!("{path}: missing {part}"));
    }
    node
}

#[test]
fn reports_each_write_lock_hold() {
    let fs = FakeFs::new();
    for d in 0..20 {
        fs.file(&format!("d{d}/f"), 1, 1);
    }
    let holds = Arc::new(Mutex::new(Vec::new()));
    let probe = {
        let holds = Arc::clone(&holds);
        HoldProbe::new(move |held, nodes| holds.lock().unwrap().push((held, nodes)))
    };
    let s = Scan::start(
        fs.root(),
        fs.clone(),
        ScanConfig {
            write_holds: Some(probe),
            ..config()
        },
        |_| {},
    );
    let tree = s.tree();
    s.wait();
    let holds = holds.lock().unwrap();
    // One batch and one end of listing per folder, root included.
    assert!(holds.len() >= 21, "{} holds", holds.len());
    let sizes: Vec<usize> = holds.iter().map(|h| h.1).collect();
    assert!(sizes.is_sorted(), "the tree only grows during a scan");
    assert_eq!(sizes.last(), Some(&tree.read().unwrap().len()));
}

#[test]
fn fixture_tree_has_correct_hierarchy_and_weights() {
    let fs = FakeFs::new();
    fs.file("a/x.bin", 1000, 4096)
        .file("a/b/y.bin", 1_000_000, 1_003_520)
        .file("a/b/empty", 0, 0)
        .file("z.txt", 10, 4096)
        .dir("emptydir");
    let (progress, tree) = run(&fs);
    let tree = tree.read().unwrap();

    assert_eq!(progress.state, ScanState::Complete);
    assert_eq!(progress.files, 4);
    assert_eq!(progress.dirs, 3);
    assert_eq!(progress.logical, 1_001_010);
    assert_eq!(progress.allocated, 1_011_712);
    assert_eq!(progress.pending_dirs, 0);

    let a = find(&tree, "a");
    let b = find(&tree, "a/b");
    assert_eq!(tree.logical(a), 1_001_000);
    assert_eq!(tree.allocated(a), 1_007_616);
    assert_eq!(tree.logical(b), 1_000_000);
    assert_eq!(tree.file_count(b), 2);
    assert_eq!(tree.dir_state(b), DirState::Listed);
    assert_eq!(tree.dir_state(find(&tree, "emptydir")), DirState::Listed);
    assert_eq!(
        tree.path(find(&tree, "a/b/y.bin")),
        Path::new("/fake/a/b/y.bin")
    );
}

#[test]
fn cloud_files_count_local_allocation_only() {
    let fs = FakeFs::new();
    fs.cloud_file("online-only.mp4", 5_000_000_000, 0)
        .cloud_file("partial.mp4", 1_000_000, 65_536)
        .file_unknown_allocation("unknown.bin", 777);
    let (progress, tree) = run(&fs);
    let tree = tree.read().unwrap();

    assert_eq!(progress.allocated, 65_536);
    assert_eq!(progress.logical, 5_001_000_777);
    assert_eq!(progress.unknown_allocation_files, 1);
    let unknown = find(&tree, "unknown.bin");
    assert_eq!(tree.file_allocated(unknown), Allocated::UNKNOWN);
    assert_ne!(tree.flags(find(&tree, "online-only.mp4")) & flags::CLOUD, 0);
}

#[test]
fn hard_links_count_allocation_once_and_remeasure() {
    let fs = FakeFs::new();
    fs.linked_file("a/one.bin", 262_144, 262_144, 42)
        .linked_file("b/two.bin", 262_144, 262_144, 42)
        // The authoritative size differs from the (stale) listing.
        .set_measured("a/one.bin", 1_048_576, 1_048_576)
        .set_measured("b/two.bin", 1_048_576, 1_048_576);
    let (progress, tree) = run(&fs);
    let tree = tree.read().unwrap();

    assert_eq!(progress.hardlink_aliases, 1);
    assert_eq!(progress.allocated, 1_048_576, "allocation counted once");
    assert_eq!(progress.logical, 2 * 1_048_576, "logical counts each name");
    let (one, two) = (find(&tree, "a/one.bin"), find(&tree, "b/two.bin"));
    let (owner, alias) = if tree.alias_owner(two) == Some(one) {
        (one, two)
    } else {
        (two, one)
    };
    assert_eq!(tree.alias_owner(alias), Some(owner));
    assert_eq!(tree.allocated(tree.parent(alias).unwrap()), 0);
}

#[test]
fn records_each_entrys_object_id() {
    let fs = FakeFs::new();
    fs.linked_file("a/one.bin", 1, 1, 42).file("b.bin", 1, 1);
    let (_, tree) = run(&fs);
    let tree = tree.read().unwrap();
    assert_eq!(tree.id(find(&tree, "a/one.bin")), Some(fingerprint(42)));
    assert_eq!(tree.id(find(&tree, "b.bin")), None, "the listing gave none");
}

#[test]
fn hard_links_within_one_listing_count_allocation_once() {
    let fs = FakeFs::new();
    fs.linked_file("a/one.bin", 4096, 4096, 9)
        .linked_file("a/two.bin", 4096, 4096, 9)
        .linked_file("a/three.bin", 4096, 4096, 9)
        .linked_file("b/four.bin", 4096, 4096, 9);
    let (progress, tree) = run(&fs);
    let tree = tree.read().unwrap();

    assert_eq!(progress.hardlink_aliases, 3);
    assert_eq!(progress.allocated, 4096);
    // One name owns the allocation and the others point at it.
    let names = ["a/one.bin", "a/two.bin", "a/three.bin", "b/four.bin"].map(|p| find(&tree, p));
    let owners: Vec<_> = names
        .iter()
        .filter(|&&n| tree.alias_owner(n).is_none())
        .collect();
    assert_eq!(owners.len(), 1);
    for &n in &names {
        assert!(tree.alias_owner(n).is_none_or(|o| o == *owners[0]));
    }
}

#[test]
fn links_mounts_and_special_files_are_skipped_and_counted() {
    let fs = FakeFs::new();
    fs.file("real.bin", 100, 4096)
        .other("link", EntryKind::Symlink)
        .other("junction", EntryKind::MountPoint)
        .other("sub/mnt", EntryKind::MountPoint)
        .other("fifo", EntryKind::Special)
        .other("weird", EntryKind::UnknownReparse)
        .other("vanished", EntryKind::Failed(ErrorKind::Vanished));
    let (progress, tree) = run(&fs);
    let o = &progress.omissions;

    assert_eq!(progress.state, ScanState::CompleteWithOmissions);
    assert_eq!(progress.files, 1);
    assert_eq!(o.count(OmissionReason::Symlink), 1);
    assert_eq!(o.count(OmissionReason::MountPoint), 2);
    assert_eq!(o.count(OmissionReason::SpecialFile), 1);
    assert_eq!(o.count(OmissionReason::UnknownReparse), 1);
    assert_eq!(o.count(OmissionReason::Vanished), 1);
    assert_eq!(o.errors(), 1);
    // The entry that couldn't be read may be missing from the listing.
    let tree = tree.read().unwrap();
    assert!(!tree.fully_listed(NodeId::ROOT));
    assert!(tree.fully_listed(find(&tree, "sub")));
    let symlinks: Vec<_> = o
        .samples(OmissionReason::Symlink)
        .iter()
        .map(|s| s.path.as_path())
        .collect();
    assert_eq!(symlinks, [Path::new("/fake/link")]);
}

#[test]
fn errors_are_reported_and_traversal_continues() {
    let fs = FakeFs::new();
    fs.file("ok/a.bin", 10, 4096)
        .fail_dir("denied", ErrorKind::PermissionDenied)
        .fail_dir("gone", ErrorKind::Disconnected)
        .fail_dir("x/denied2", ErrorKind::PermissionDenied)
        .fail_dir("y/denied3", ErrorKind::PermissionDenied)
        .file("ok/b.bin", 20, 4096);
    let (progress, tree) = run(&fs);
    let tree = tree.read().unwrap();
    let o = &progress.omissions;

    assert_eq!(progress.state, ScanState::CompleteWithOmissions);
    assert_eq!(progress.files, 2);
    assert_eq!(o.count(OmissionReason::PermissionDenied), 3);
    assert_eq!(
        o.samples(OmissionReason::PermissionDenied).len(),
        2,
        "bounded samples"
    );
    assert_eq!(o.count(OmissionReason::Disconnected), 1);
    let gone = &o.samples(OmissionReason::Disconnected)[0];
    assert_eq!(gone.path, Path::new("/fake/gone"));
    assert_eq!(gone.detail.as_deref(), Some("injected"));
    assert_eq!(tree.dir_state(find(&tree, "denied")), DirState::Failed);
}

#[test]
fn link_root_is_rejected_clearly() {
    let fs = FakeFs::new();
    fs.set_root_kind(RootKind::Link);
    let (progress, _) = run(&fs);
    let ScanState::Failed(message) = progress.state else {
        panic!("expected failure, got {:?}", progress.state);
    };
    assert!(message.contains("symbolic link"), "{message}");
}

#[test]
fn unreadable_root_fails_instead_of_completing() {
    let fs = FakeFs::new();
    fs.fail_dir("", ErrorKind::PermissionDenied);
    let (progress, _) = run(&fs);
    assert!(
        matches!(progress.state, ScanState::Failed(_)),
        "{:?}",
        progress.state
    );
}

#[test]
fn partial_results_are_visible_before_completion() {
    let fs = FakeFs::new();
    for i in 0..20 {
        fs.file(&format!("early/f{i}"), 100, 4096);
    }
    let gate = fs.gate_dir("slow");
    fs.file("slow/late.bin", 1, 4096);

    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let s = Scan::start(fs.root(), fs.clone(), config(), move |p| {
        sink.lock().unwrap().push((p.state.clone(), p.files));
    });
    let deadline = Instant::now() + HANG;
    while s.progress().files < 20 {
        assert!(Instant::now() < deadline, "partial results never appeared");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(s.progress().state, ScanState::Scanning);
    assert!(s.progress().pending_dirs >= 1);
    {
        let tree = s.tree();
        let tree = tree.read().unwrap();
        assert_eq!(tree.file_count(find(&tree, "early")), 20);
    }
    gate.release();
    let done = s.wait();
    assert_eq!(done.state, ScanState::Complete);
    assert_eq!(done.files, 21);
    let seen = seen.lock().unwrap();
    assert!(
        seen.iter()
            .any(|(s, f)| *s == ScanState::Scanning && *f == 20)
    );
    assert_eq!(seen.last().unwrap().0, ScanState::Complete);
}

#[test]
fn cancel_is_prompt_while_an_os_call_is_blocked_and_keeps_partial_data() {
    let fs = FakeFs::new();
    fs.file("done/a.bin", 10, 4096);
    let gate = fs.gate_dir("blocked");
    fs.file("blocked/never.bin", 1, 4096);
    fs.file("blocked/sub/never2.bin", 1, 4096);

    let s = Scan::start(fs.root(), fs.clone(), config(), |_| {});
    assert!(gate.entered(HANG), "the listing never reached the gate");
    let deadline = Instant::now() + HANG;
    while s.progress().files < 1 {
        assert!(Instant::now() < deadline, "the first file never arrived");
        std::thread::sleep(Duration::from_millis(5));
    }
    let tree = s.tree();
    let returned = release_after_returning(gate);
    s.cancel();
    let progress = s.wait();

    assert_eq!(progress.state, ScanState::Cancelled);
    assert_eq!(progress.files, 1, "discovered data retained");
    let tree = tree.read().unwrap();
    assert_eq!(tree.dir_state(find(&tree, "blocked")), DirState::NotScanned);
    drop(tree);
    assert!(returned.finish(), "cancel waited for the blocked OS call");
}

#[test]
fn generations_are_unique_and_increasing() {
    let fs = FakeFs::new();
    let a = Scan::start(fs.root(), fs.clone(), config(), |_| {});
    let b = Scan::start(fs.root(), fs.clone(), config(), |_| {});
    assert!(b.generation() > a.generation());
    let (pa, pb) = (a.wait(), b.wait());
    assert_ne!(pa.generation, pb.generation);
}

#[test]
fn progress_events_are_coalesced() {
    let fs = FakeFs::new();
    for d in 0..200 {
        fs.file(&format!("d{d}/f"), 1, 1);
    }
    let (tx, rx) = channel();
    let s = Scan::start(
        fs.root(),
        fs.clone(),
        ScanConfig {
            progress_interval: Duration::from_secs(60),
            ..config()
        },
        move |p| tx.send(p.state.clone()).unwrap(),
    );
    s.wait();
    let events: Vec<_> = rx.try_iter().collect();
    assert_eq!(
        events,
        [ScanState::Scanning, ScanState::Complete],
        "the first change and the final state, none per directory"
    );
}

#[test]
fn finished_scans_can_be_edited_as_a_new_revision() {
    let fs = FakeFs::new();
    fs.file("keep/a.bin", 10, 4096)
        .file("gone/b.bin", 20, 8192)
        .file("gone/sub/c.bin", 30, 4096);
    let gate = fs.gate_dir("keep");
    let s = Scan::start(fs.root(), fs.clone(), config(), |_| {});
    assert!(s.edit_finished(|_| ()).is_none(), "no edits while scanning");
    gate.release();
    let deadline = Instant::now() + HANG;
    while !s.progress().state.is_finished() {
        assert!(Instant::now() < deadline, "scan never finished");
        std::thread::sleep(Duration::from_millis(5));
    }
    let before = s.progress();
    let (removed, after) = s
        .edit_finished(|tree| {
            let gone = find(tree, "gone");
            tree.remove(gone)
        })
        .expect("finished scans can be edited");
    assert!(removed);
    assert_eq!(after.revision, before.revision + 1);
    assert_eq!(after.state, ScanState::Complete);
    assert_eq!((after.files, after.dirs), (1, 1));
    assert_eq!((after.logical, after.allocated), (10, 4096));
    assert_eq!(s.progress().revision, after.revision);
}

#[test]
fn modified_times_reach_the_tree() {
    let fs = FakeFs::new();
    let when = std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    fs.file("a/dated.txt", 1, 4096)
        .set_modified("a/dated.txt", when)
        .file("undated.txt", 1, 4096);
    let (_, tree) = run(&fs);
    let tree = tree.read().unwrap();
    assert_eq!(
        tree.modified(find(&tree, "a/dated.txt")),
        Some(1_700_000_000)
    );
    assert_eq!(tree.modified(find(&tree, "undated.txt")), None);
}

/// Adds `count` 100-byte files named `<dir>/f00000`... to `fs`.
fn wide(fs: &FakeFs, dir: &str, count: usize) {
    for i in 0..count {
        fs.file(&format!("{dir}/f{i:05}"), 100, 4096);
    }
}

#[test]
fn wide_directories_arrive_in_batches_with_links_and_omissions_intact() {
    let fs = FakeFs::new();
    let n = 3 * BATCH_ENTRIES + 5;
    wide(&fs, "wide", n);
    // Names sort around the files, so these land in the first and last
    // batches and in between.
    fs.linked_file("wide/a_link.bin", 8192, 8192, 7)
        .linked_file("wide/z_link.bin", 8192, 8192, 7)
        .file("wide/m_sub/inner.bin", 1, 4096)
        .other("wide/b_symlink", EntryKind::Symlink)
        .other("wide/g_symlink", EntryKind::Symlink)
        .other("wide/y_symlink", EntryKind::Symlink);
    let (progress, tree) = run(&fs);
    let tree = tree.read().unwrap();

    assert_eq!(progress.state, ScanState::CompleteWithOmissions);
    assert_eq!(progress.pending_dirs, 0);
    let w = find(&tree, "wide");
    assert_eq!(tree.dir_state(w), DirState::Listed);
    assert!(tree.fully_listed(w), "symlinks are skipped, not unreadable");
    assert_eq!(tree.file_count(w) as usize, n + 3);
    assert_eq!(progress.omissions.count(OmissionReason::Symlink), 3);
    assert_eq!(progress.hardlink_aliases, 1, "linked across batches");
    assert_eq!(
        progress.allocated,
        n as u64 * 4096 + 8192 + 4096,
        "the linked file's allocation counted once"
    );
    let sub = find(&tree, "wide/m_sub");
    assert!(tree.fully_listed(sub));
    assert_eq!(tree.file_count(sub), 1);
}

#[test]
fn a_listing_that_fails_part_way_keeps_what_was_read() {
    let fs = FakeFs::new();
    wide(&fs, "wide", BATCH_ENTRIES + 100);
    fs.file("wide/a_sub/x.bin", 1, 4096)
        .fail_dir_after("wide", BATCH_ENTRIES + 50, ErrorKind::Disconnected)
        .file("other/y.bin", 1, 4096);
    let (progress, tree) = run(&fs);
    let tree = tree.read().unwrap();
    let o = &progress.omissions;

    assert_eq!(progress.state, ScanState::CompleteWithOmissions);
    assert_eq!(o.count(OmissionReason::Disconnected), 1);
    assert_eq!(
        o.samples(OmissionReason::Disconnected)[0].path,
        Path::new("/fake/wide")
    );
    let w = find(&tree, "wide");
    // The first entries (the subfolder, then files) are kept; the folder
    // is listed but not fully read, so comparisons treat it as partial.
    assert_eq!(tree.dir_state(w), DirState::Listed);
    assert!(!tree.fully_listed(w));
    assert_eq!(tree.file_count(w) as usize, BATCH_ENTRIES + 50);
    let sub = find(&tree, "wide/a_sub");
    assert!(
        tree.fully_listed(sub),
        "subfolders found before the error are scanned"
    );
    assert!(tree.fully_listed(find(&tree, "other")));
}

#[test]
fn a_root_that_fails_part_way_is_scanned_in_part() {
    let fs = FakeFs::new();
    fs.file("a.bin", 1, 4096)
        .file("b/c.bin", 1, 4096)
        .file("d.bin", 1, 4096)
        .fail_dir_after("", 2, ErrorKind::Disconnected);
    let (progress, tree) = run(&fs);
    let tree = tree.read().unwrap();

    assert_eq!(progress.state, ScanState::CompleteWithOmissions);
    assert_eq!(progress.files, 2, "a.bin and b/c.bin");
    assert!(!tree.fully_listed(NodeId::ROOT));
    assert!(tree.fully_listed(find(&tree, "b")));
}

#[test]
fn cancel_during_a_wide_listing_is_prompt_and_keeps_applied_batches() {
    let fs = FakeFs::new();
    wide(&fs, "wide", 3 * BATCH_ENTRIES);
    // The listing blocks once two batches are read; the first has reached
    // the aggregator, the second travels on with the next.
    let gate = fs.gate_dir_after("wide", 2 * BATCH_ENTRIES);

    let s = Scan::start(fs.root(), fs.clone(), config(), |_| {});
    assert!(gate.entered(HANG), "the listing never reached the gate");
    let deadline = Instant::now() + HANG;
    while (s.progress().files as usize) < BATCH_ENTRIES {
        assert!(Instant::now() < deadline, "the first batch never arrived");
        std::thread::sleep(Duration::from_millis(5));
    }
    let tree = s.tree();
    {
        let tree = tree.read().unwrap();
        let w = find(&tree, "wide");
        assert_eq!(tree.dir_state(w), DirState::Pending, "not listed yet");
        assert_eq!(tree.file_count(w) as usize, BATCH_ENTRIES);
    }
    let returned = release_after_returning(gate);
    s.cancel();
    let progress = s.wait();
    assert!(returned.finish(), "cancel waited for the blocked listing");

    assert_eq!(progress.state, ScanState::Cancelled);
    assert_eq!(progress.files as usize, BATCH_ENTRIES, "applied batch kept");
    let tree = tree.read().unwrap();
    assert_eq!(tree.dir_state(find(&tree, "wide")), DirState::NotScanned);
}

#[test]
fn a_cancel_that_stops_the_last_listing_is_reported_as_cancelled() {
    let fs = FakeFs::new();
    wide(&fs, "wide", 3 * BATCH_ENTRIES);
    let gate = fs.gate_dir_after("wide", 2 * BATCH_ENTRIES);

    let s = Scan::start(fs.root(), fs.clone(), config(), |_| {});
    let deadline = Instant::now() + HANG;
    while (s.progress().files as usize) < BATCH_ENTRIES {
        assert!(Instant::now() < deadline, "the first batch never arrived");
        std::thread::sleep(Duration::from_millis(5));
    }
    let tree = s.tree();
    s.cancel();
    // The listing sees the cancel and stops while it's the only work left,
    // before the scan has noticed the cancel itself.
    gate.release();
    let progress = s.wait();

    assert_eq!(progress.state, ScanState::Cancelled);
    let tree = tree.read().unwrap();
    assert_eq!(tree.dir_state(find(&tree, "wide")), DirState::NotScanned);
}

/// No folder is left pending once a scan has finished, and a cancelled scan
/// never reports that it completed, wherever the cancel lands.
#[test]
fn cancels_at_any_moment_leave_a_consistent_tree() {
    let fs = FakeFs::new();
    wide(&fs, "wide", 2 * BATCH_ENTRIES + 10);
    for d in 0..40 {
        for f in 0..20 {
            fs.file(&format!("d{d}/s{}/f{f}", d % 3), 10, 4096);
        }
    }
    // Deterministic spread of delays, from before the first listing to
    // after the last.
    let mut seed = 0x9e37_79b9_u64;
    for _ in 0..40 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let delay = Duration::from_micros(seed % 20_000);
        let s = Scan::start(fs.root(), fs.clone(), config(), |_| {});
        let tree = s.tree();
        std::thread::sleep(delay);
        s.cancel();
        let progress = s.wait();
        assert!(
            matches!(
                progress.state,
                ScanState::Cancelled | ScanState::Complete | ScanState::CompleteWithOmissions
            ),
            "{:?}",
            progress.state
        );
        let tree = tree.read().unwrap();
        let mut unscanned = 0;
        for i in 0..tree.len() as u32 {
            let node = NodeId::from_index(i);
            if tree.is_dir(node) {
                let state = tree.dir_state(node);
                assert_ne!(state, DirState::Pending, "after {delay:?}");
                unscanned += usize::from(state == DirState::NotScanned);
            }
        }
        if progress.state != ScanState::Cancelled {
            assert_eq!(unscanned, 0, "a completed scan listed everything");
            assert_eq!(progress.files as usize, 2 * BATCH_ENTRIES + 10 + 800);
        }
    }
}
