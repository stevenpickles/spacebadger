//! The order folders are listed in while the tree has a prior scan (see
//! `ScanConfig::list_by_prior`), on the in-memory filesystem: only the
//! order changes, never what the scan finds.

use sb_core::git::RootKind as GitKind;
use sb_core::prior::Prior;
use sb_core::snapshot::Snapshot;
use sb_core::tree::{DirState, NodeId, Tree};
use sb_scan::fake::FakeFs;
use sb_scan::{
    BATCH_ENTRIES, EntryKind, ErrorKind, GIT_GRACE, OmissionReason, Progress, Scan, ScanConfig,
    ScanState,
};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime};

const HANG: Duration = Duration::from_secs(60);

/// When the prior scan finished, in milliseconds since the Unix epoch.
const PRIOR_MS: u64 = 1_000_000;

fn config(workers: usize) -> ScanConfig {
    ScanConfig {
        workers,
        progress_interval: Duration::from_millis(10),
        sample_limit: 2,
        write_holds: None,
        git_grace: GIT_GRACE,
        exact_hard_links: false,
        list_by_prior: true,
    }
}

fn scan(
    fs: &FakeFs,
    config: ScanConfig,
    prior: Option<&Arc<Prior>>,
) -> (Progress, Arc<RwLock<Tree>>) {
    let s = Scan::start(fs.root(), fs.clone(), config, |_| {});
    let tree = s.tree();
    if let Some(prior) = prior {
        tree.write().unwrap().set_prior(Arc::clone(prior));
    }
    (s.wait(), tree)
}

/// A snapshot of a scan of `fs` as it is now, as the prior of a rescan.
fn prior_of(fs: &FakeFs) -> Arc<Prior> {
    let (progress, tree) = scan(fs, config(2), None);
    assert!(progress.state.is_finished());
    let snap = Snapshot::capture(&tree.read().unwrap(), PRIOR_MS, true);
    Arc::new(Prior::new(snap))
}

fn find(tree: &Tree, path: &str) -> NodeId {
    let mut node = NodeId::ROOT;
    for part in path.split('/').filter(|p| !p.is_empty()) {
        node = tree
            .children(node)
            .find(|&c| tree.name(c) == part)
            .unwrap_or_else(|| panic!("{path}: missing {part}"));
    }
    node
}

/// Folders a..f under the root, d the biggest, as scanned before; then c
/// is new and e has had an entry added since (a later modified time).
fn six_folders() -> (FakeFs, Arc<Prior>) {
    let fs = FakeFs::new();
    for (dir, size) in [
        ("a", 20),
        ("b", 300_000),
        ("d", 10_000_000),
        ("e", 100_000),
        ("f", 50),
    ] {
        fs.file(&format!("{dir}/data"), size, size);
    }
    let prior = prior_of(&fs);
    fs.file("c/data", 5, 5);
    fs.file("e/added", 1, 1);
    let later = SystemTime::UNIX_EPOCH + Duration::from_millis(PRIOR_MS + 1_000);
    fs.set_modified("e", later);
    (fs, prior)
}

/// Rescans with one worker, with every folder under the root queued before
/// any is listed: the root's listing waits, once its first batch (with all
/// six folders) is in, until they're all queued. Returns the order of the
/// listings after the root's.
fn listing_order(fs: &FakeFs, config: ScanConfig, prior: Option<&Arc<Prior>>) -> Vec<String> {
    for i in 0..2 * BATCH_ENTRIES {
        fs.file(&format!("z{i:05}"), 1, 1);
    }
    let before = fs.listings().len();
    let start = fs.gate_dir("");
    let first_batch = fs.gate_dir_after("", 2 * BATCH_ENTRIES);
    let s = Scan::start(fs.root(), fs.clone(), config, |_| {});
    if let Some(prior) = prior {
        s.tree().write().unwrap().set_prior(Arc::clone(prior));
    }
    assert!(start.entered(HANG), "the root's listing never started");
    start.release();
    assert!(first_batch.entered(HANG), "the first batch never arrived");
    let deadline = Instant::now() + HANG;
    while s.progress().pending_dirs < 7 {
        assert!(Instant::now() < deadline, "the folders were never queued");
        std::thread::sleep(Duration::from_millis(2));
    }
    first_batch.release();
    let progress = s.wait();
    assert_eq!(progress.state, ScanState::Complete);
    let listings = &fs.listings()[before..];
    assert_eq!(listings[0], fs.root());
    listings[1..]
        .iter()
        .map(|p| p.strip_prefix(fs.root()).unwrap().display().to_string())
        .collect()
}

#[test]
fn with_a_prior_changed_folders_go_first_then_big_ones_then_the_rest_in_order() {
    let (fs, prior) = six_folders();
    // e changed (and held 1 %), c is new; then d (96 %) and b (3 %); a and
    // f held under 0.01 % and keep their places.
    assert_eq!(
        listing_order(&fs, config(1), Some(&prior)),
        ["e", "c", "d", "b", "a", "f"]
    );
}

#[test]
fn a_prior_given_late_reorders_the_folders_already_queued() {
    let (fs, prior) = six_folders();
    // Listed first (its name sorts first) and held twice: as it starts,
    // until the six folders are queued and the prior is given; and once
    // its first batch is in, until that batch has been applied, which is
    // when the scan notices the prior.
    for i in 0..2 * BATCH_ENTRIES + 1 {
        fs.file(&format!("0held/f{i:05}"), 1, 1);
    }
    let start = fs.gate_dir("0held");
    let first_batch = fs.gate_dir_after("0held", 2 * BATCH_ENTRIES);
    let before = fs.listings().len();
    let s = Scan::start(fs.root(), fs.clone(), config(1), |_| {});
    assert!(start.entered(HANG), "0held was never listed");
    let deadline = Instant::now() + HANG;
    while s.progress().pending_dirs < 7 {
        assert!(Instant::now() < deadline, "the folders were never queued");
        std::thread::sleep(Duration::from_millis(2));
    }
    s.tree().write().unwrap().set_prior(Arc::clone(&prior));
    start.release();
    assert!(first_batch.entered(HANG), "the first batch never arrived");
    while (s.progress().files as usize) < BATCH_ENTRIES {
        assert!(
            Instant::now() < deadline,
            "the first batch was never applied"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    first_batch.release();
    assert_eq!(s.wait().state, ScanState::Complete);
    let order: Vec<String> = fs.listings()[before + 2..]
        .iter()
        .map(|p| p.strip_prefix(fs.root()).unwrap().display().to_string())
        .collect();
    // Their modified times are gone by then, so e is just big.
    assert_eq!(order, ["c", "d", "b", "e", "a", "f"]);
}

#[test]
fn without_a_prior_folders_are_listed_in_the_order_found() {
    let (fs, _) = six_folders();
    assert_eq!(
        listing_order(&fs, config(1), None),
        ["a", "b", "c", "d", "e", "f"]
    );
}

#[test]
fn ordering_by_the_prior_can_be_turned_off() {
    let (fs, prior) = six_folders();
    let config = ScanConfig {
        list_by_prior: false,
        ..config(1)
    };
    assert_eq!(
        listing_order(&fs, config, Some(&prior)),
        ["a", "b", "c", "d", "e", "f"]
    );
}

/// Everything a scan reports about every path, whatever the nodes' order.
fn everything(progress: &Progress, tree: &Tree) -> (String, BTreeMap<PathBuf, String>) {
    let totals = format!(
        "{:?} files={} dirs={} logical={} allocated={} unknown={} aliases={} git={} omissions={:?}",
        progress.state,
        progress.files,
        progress.dirs,
        progress.logical,
        progress.allocated,
        progress.unknown_allocation_files,
        progress.hardlink_aliases,
        progress.git_roots,
        progress.omissions.iter().collect::<Vec<_>>()
    );
    let mut nodes = BTreeMap::new();
    for i in 0..tree.len() as u32 {
        let n = NodeId::from_index(i);
        let owner = tree.alias_owner(n).map(|o| tree.path(o));
        let state = tree.is_dir(n).then(|| tree.dir_state(n));
        let git = tree.git_entry(n).map(|r| format!("{:?}", r.kind));
        nodes.insert(
            tree.path(n),
            format!(
                "{} {:?} {} {state:?} {owner:?} {git:?} {}",
                tree.logical(n),
                tree.allocated(n),
                tree.file_count(n),
                tree.flags(n)
            ),
        );
    }
    (totals, nodes)
}

#[test]
fn the_order_never_changes_what_is_found() {
    let fs = FakeFs::new();
    for d in 0..12 {
        for s in 0..4 {
            for f in 0..3 {
                let size = 1u64 << ((d * 7 + s * 3 + f) % 30);
                fs.file(
                    &format!("d{d}/s{s}/f{f}"),
                    size,
                    size.next_multiple_of(4096),
                );
            }
        }
    }
    // Hard links across folders, a repository, omissions.
    for at in ["d1/link", "d7/s2/link", "d11/link", "new/link"] {
        fs.linked_file(at, 9_000, 12_288, 77);
    }
    fs.text("proj/.git/HEAD", "ref: refs/heads/main\n")
        .text(
            "proj/.git/config",
            "[core]\n\trepositoryformatversion = 0\n",
        )
        .file("proj/.git/objects/pack/pack-1.pack", 5000, 8192)
        .dir("proj/.git/refs/heads")
        .file("proj/src/main.rs", 100, 4096);
    fs.other("d3/link", EntryKind::Symlink);
    fs.fail_dir("d5/s1", ErrorKind::PermissionDenied);
    let prior = prior_of(&fs);
    // Changes since: a new folder, a grown one, one removed.
    fs.file("new/big", 50_000_000, 50_003_968);
    fs.file("d2/s0/grown", 7_000_000, 7_000_064);
    fs.set_modified(
        "d2/s0",
        SystemTime::UNIX_EPOCH + Duration::from_millis(PRIOR_MS + 1),
    );

    let (p, t) = scan(&fs, config(8), None);
    let reference = everything(&p, &t.read().unwrap());
    assert_eq!(p.state, ScanState::CompleteWithOmissions);
    assert_eq!(p.omissions.count(OmissionReason::Symlink), 1);
    assert_eq!(p.omissions.count(OmissionReason::PermissionDenied), 1);
    assert_eq!(p.hardlink_aliases, 3);
    for round in 0..6 {
        let config = ScanConfig {
            list_by_prior: round % 2 == 0,
            ..config(1 + round * 3)
        };
        let (p, t) = scan(&fs, config, Some(&prior));
        let t = t.read().unwrap();
        assert_eq!(everything(&p, &t), reference, "round {round}");
        let proj = find(&t, "proj");
        assert_eq!(
            t.git_entry(proj).map(|r| r.kind),
            Some(GitKind::Repository),
            "the probe ran"
        );
        assert!(!t.is_provisional(NodeId::ROOT), "nothing left estimated");
    }
}

#[test]
fn a_cancel_with_a_prior_stops_at_once_and_leaves_nothing_pending() {
    let (fs, prior) = six_folders();
    let gate = fs.gate_dir("d");
    let s = Scan::start(fs.root(), fs.clone(), config(1), |_| {});
    let tree = s.tree();
    tree.write().unwrap().set_prior(Arc::clone(&prior));
    assert!(gate.entered(HANG), "d was never listed");
    s.cancel();
    let started = Instant::now();
    let progress = s.wait();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(progress.state, ScanState::Cancelled);
    gate.release();
    let tree = tree.read().unwrap();
    for i in 0..tree.len() as u32 {
        let n = NodeId::from_index(i);
        if tree.is_dir(n) {
            assert_ne!(tree.dir_state(n), DirState::Pending);
        }
    }
    assert_eq!(tree.dir_state(find(&tree, "d")), DirState::NotScanned);
    assert!(!tree.is_provisional(NodeId::ROOT));
}

#[test]
fn running_out_of_capacity_with_a_prior_stops_the_scan_cleanly() {
    let (fs, prior) = six_folders();
    // A name longer than the tree stores, in the folder listed first.
    fs.file(&format!("e/{}", "n".repeat(70_000)), 1, 1);
    let (progress, tree) = scan(&fs, config(1), Some(&prior));
    match &progress.state {
        ScanState::Failed(why) => assert!(why.contains("capacity"), "{why}"),
        other => panic!("{other:?}"),
    }
    let tree = tree.read().unwrap();
    for i in 0..tree.len() as u32 {
        let n = NodeId::from_index(i);
        if tree.is_dir(n) {
            assert_ne!(tree.dir_state(n), DirState::Pending);
        }
    }
    assert!(!tree.is_provisional(NodeId::ROOT));
}
