use sb_core::size::Allocated;
use sb_core::tree::{DirState, NodeId, Tree, flags};
use sb_scan::fake::FakeFs;
use sb_scan::{
    EntryKind, ErrorKind, OmissionReason, Progress, RootKind, Scan, ScanConfig, ScanState,
};
use std::path::Path;
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn config() -> ScanConfig {
    ScanConfig {
        workers: 3,
        progress_interval: Duration::from_millis(10),
        sample_limit: 2,
    }
}

/// Scans to completion and keeps the tree for inspection.
fn run(fs: &FakeFs) -> (Progress, Arc<std::sync::RwLock<Tree>>) {
    let s = Scan::start(fs.root(), fs.clone(), config(), |_| {});
    let tree = s.tree();
    (s.wait(), tree)
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
fn links_mounts_and_special_files_are_skipped_and_counted() {
    let fs = FakeFs::new();
    fs.file("real.bin", 100, 4096)
        .other("link", EntryKind::Symlink)
        .other("junction", EntryKind::MountPoint)
        .other("sub/mnt", EntryKind::MountPoint)
        .other("fifo", EntryKind::Special)
        .other("weird", EntryKind::UnknownReparse)
        .other("vanished", EntryKind::Failed(ErrorKind::Vanished));
    let (progress, _) = run(&fs);
    let o = &progress.omissions;

    assert_eq!(progress.state, ScanState::CompleteWithOmissions);
    assert_eq!(progress.files, 1);
    assert_eq!(o.count(OmissionReason::Symlink), 1);
    assert_eq!(o.count(OmissionReason::MountPoint), 2);
    assert_eq!(o.count(OmissionReason::SpecialFile), 1);
    assert_eq!(o.count(OmissionReason::UnknownReparse), 1);
    assert_eq!(o.count(OmissionReason::Vanished), 1);
    assert_eq!(o.errors(), 1);
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
    let deadline = Instant::now() + Duration::from_secs(5);
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
    let deadline = Instant::now() + Duration::from_secs(5);
    while s.progress().files < 1 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    let tree = s.tree();
    let started = Instant::now();
    s.cancel();
    let progress = s.wait();
    let took = started.elapsed();

    assert_eq!(progress.state, ScanState::Cancelled);
    assert!(took < Duration::from_millis(250), "cancel took {took:?}");
    assert_eq!(progress.files, 1, "discovered data retained");
    let tree = tree.read().unwrap();
    assert_eq!(tree.dir_state(find(&tree, "blocked")), DirState::NotScanned);
    drop(tree);
    gate.release();
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
    let deadline = Instant::now() + Duration::from_secs(5);
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
