//! Git detection through the scan engine, on the in-memory filesystem.

use sb_core::git::paths::{Style, normalize};
use sb_core::git::{GitClass, GitRoot, Head, Problem, RootKind, summarize};
use sb_core::layout::Metric;
use sb_core::tree::{NodeId, Tree};
use sb_scan::fake::{FakeFs, Gate};
use sb_scan::git::ANCESTOR_LIMIT;
use sb_scan::{EntryKind, ErrorKind, OmissionReason, Progress, Scan, ScanConfig, ScanState};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

const HANG: Duration = Duration::from_secs(60);

fn config() -> ScanConfig {
    ScanConfig {
        workers: 3,
        progress_interval: Duration::from_millis(10),
        sample_limit: 2,
        write_holds: None,
    }
}

fn run_at(fs: &FakeFs, root: PathBuf) -> (Progress, Arc<RwLock<Tree>>) {
    let s = Scan::start(root, fs.clone(), config(), |_| {});
    let tree = s.tree();
    (s.wait(), tree)
}

fn run(fs: &FakeFs) -> (Progress, Arc<RwLock<Tree>>) {
    run_at(fs, fs.root())
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

fn entry<'t>(tree: &'t Tree, path: &str) -> Option<&'t GitRoot> {
    tree.git_entry(find(tree, path))
}

fn kind(tree: &Tree, path: &str) -> Option<RootKind> {
    entry(tree, path).map(|r| r.kind)
}

fn problem(tree: &Tree, path: &str) -> Option<Problem> {
    entry(tree, path).and_then(|r| r.problem.clone())
}

/// A path as the probe writes it on this platform.
fn native(path: &str) -> PathBuf {
    normalize(Path::new(path), Style::native())
}

fn main_branch() -> Option<Head> {
    Some(Head::Branch("main".into()))
}

/// A Git directory at `at`: `HEAD` on `main`, `objects`, `refs`, and
/// `config` with `extra` in its `[core]` section.
fn git_dir(fs: &FakeFs, at: &str, extra: &str) {
    fs.text(&format!("{at}/HEAD"), "ref: refs/heads/main\n")
        .text(
            &format!("{at}/config"),
            &format!("[core]\n\trepositoryformatversion = 0\n{extra}"),
        )
        .file(&format!("{at}/objects/pack/pack-1.pack"), 5000, 8192)
        .dir(&format!("{at}/refs/heads"));
}

/// An ordinary repository at `at` with one file in its checkout.
fn repo(fs: &FakeFs, at: &str) {
    git_dir(fs, &format!("{at}/.git"), "");
    fs.file(&format!("{at}/src/main.rs"), 100, 4096);
}

#[test]
fn ordinary_repositories_are_found() {
    let fs = FakeFs::new();
    repo(&fs, "proj");
    fs.file("loose.txt", 1, 4096);
    let (progress, tree) = run(&fs);
    let tree = tree.read().unwrap();

    assert_eq!(progress.state, ScanState::Complete);
    assert_eq!(progress.git_roots, 1);
    let root = entry(&tree, "proj").expect("an entry");
    assert_eq!(root.kind, RootKind::Repository);
    assert_eq!(root.head, main_branch());
    assert_eq!(
        root.git_dir.as_deref(),
        Some(native("/fake/proj/.git").as_path())
    );
    assert_eq!(kind(&tree, "proj/.git"), Some(RootKind::DotGit));
    assert_eq!(
        tree.git_class(find(&tree, "proj/src/main.rs")),
        GitClass::Checkout
    );
    assert_eq!(
        tree.git_class(find(&tree, "proj/.git/HEAD")),
        GitClass::Metadata
    );
    assert_eq!(tree.git_class(find(&tree, "loose.txt")), GitClass::Other);
    assert!(tree.git().above().is_none());
}

#[test]
fn nested_repositories_each_get_their_own_entry() {
    let fs = FakeFs::new();
    repo(&fs, "outer");
    repo(&fs, "outer/vendor/inner");
    let (progress, tree) = run(&fs);
    let tree = tree.read().unwrap();
    assert_eq!(progress.git_roots, 2);
    assert_eq!(kind(&tree, "outer"), Some(RootKind::Repository));
    assert_eq!(
        kind(&tree, "outer/vendor/inner"),
        Some(RootKind::Repository)
    );
    let (rows, totals) = summarize(&tree, None, Metric::Logical);
    assert_eq!(rows.len(), 2);
    assert_eq!(
        totals.union,
        tree.logical(find(&tree, "outer")),
        "counted once"
    );
}

/// A main repository at `main` with a linked worktree at `wt`, whose admin
/// folder is `main/.git/worktrees/wt`.
fn worktree(fs: &FakeFs) {
    repo(fs, "main");
    fs.text("main/.git/worktrees/wt/HEAD", "ref: refs/heads/topic\n")
        .text("main/.git/worktrees/wt/commondir", "../..\n")
        .text("main/.git/worktrees/wt/gitdir", "/fake/wt/.git\n")
        .text("wt/.git", "gitdir: ../main/.git/worktrees/wt\n")
        .file("wt/src/main.rs", 100, 4096);
}

#[test]
fn linked_worktrees_are_told_from_their_main_repository() {
    let fs = FakeFs::new();
    worktree(&fs);
    let (progress, tree) = run(&fs);
    let tree = tree.read().unwrap();

    assert_eq!(progress.state, ScanState::Complete);
    assert_eq!(progress.git_roots, 2, "the repository and its worktree");
    assert_eq!(kind(&tree, "main"), Some(RootKind::Repository));
    let wt = entry(&tree, "wt").expect("an entry");
    assert_eq!(wt.kind, RootKind::LinkedWorktree);
    assert_eq!(wt.head, Some(Head::Branch("topic".into())));
    assert_eq!(
        wt.common_dir.as_deref(),
        Some(native("/fake/main/.git").as_path())
    );
    assert_eq!(
        wt.git_dir.as_deref(),
        Some(native("/fake/main/.git/worktrees/wt").as_path())
    );
    assert_eq!(wt.recorded_at, None, "the worktree is where Git says");
    assert_eq!(kind(&tree, "wt/.git"), Some(RootKind::DotGit));
    let admin = entry(&tree, "main/.git/worktrees/wt").expect("an entry");
    assert_eq!(admin.kind, RootKind::WorktreeAdmin);
    assert_eq!(
        admin.worktree.as_deref(),
        Some(native("/fake/wt").as_path())
    );
    assert_eq!(
        tree.git_class(find(&tree, "wt/src/main.rs")),
        GitClass::Worktree
    );
    assert_eq!(
        tree.git_class(find(&tree, "main/.git/worktrees/wt/HEAD")),
        GitClass::Metadata
    );
}

#[test]
fn a_worktrees_git_directory_outside_the_scan_is_read_but_not_scanned() {
    let fs = FakeFs::new();
    fs.file("wt/src/main.rs", 100, 4096)
        .text("wt/.git", "gitdir: /elsewhere/main/.git/worktrees/wt\n")
        .outside_text(
            "/elsewhere/main/.git/worktrees/wt/HEAD",
            "ref: refs/heads/topic\n",
        )
        .outside_text("/elsewhere/main/.git/worktrees/wt/commondir", "../..\n")
        .outside_text(
            "/elsewhere/main/.git/worktrees/wt/gitdir",
            "/fake/moved/.git\n",
        );
    fs.outside_text(
        "/elsewhere/main/.git/objects/huge.pack",
        &"x".repeat(10_000),
    );
    let (progress, tree) = run(&fs);
    let tree = tree.read().unwrap();

    let wt = entry(&tree, "wt").expect("an entry");
    assert_eq!(wt.kind, RootKind::LinkedWorktree);
    assert_eq!(
        wt.common_dir.as_deref(),
        Some(native("/elsewhere/main/.git").as_path())
    );
    assert_eq!(
        wt.recorded_at.as_deref(),
        Some(native("/fake/moved").as_path()),
        "Git recorded the worktree elsewhere"
    );
    // Only what's under the root: the checkout and its `.git` file.
    let wt_bytes = 100 + "gitdir: /elsewhere/main/.git/worktrees/wt\n".len() as u64;
    assert_eq!(progress.logical, wt_bytes);
    assert_eq!(progress.files, 2);
    assert!(
        (0..tree.len() as u32).all(|i| !tree.path(NodeId::from_index(i)).starts_with("/elsewhere")),
        "nothing outside the scan was added"
    );
}

#[test]
fn submodules_and_nested_submodules_are_told_apart() {
    let fs = FakeFs::new();
    repo(&fs, "super");
    git_dir(&fs, "super/.git/modules/lib", "\tworktree = ../../../lib\n");
    git_dir(
        &fs,
        "super/.git/modules/lib/modules/inner",
        "\tworktree = ../../../../../lib/inner\n",
    );
    fs.text("super/lib/.git", "gitdir: ../.git/modules/lib\n")
        .file("super/lib/lib.rs", 10, 4096)
        .text(
            "super/lib/inner/.git",
            "gitdir: ../../.git/modules/lib/modules/inner\n",
        )
        .file("super/lib/inner/inner.rs", 10, 4096);
    // And a linked worktree of the superproject.
    fs.text("super/.git/worktrees/wt/HEAD", "ref: refs/heads/topic\n")
        .text("super/.git/worktrees/wt/commondir", "../..\n")
        .text("super/.git/worktrees/wt/gitdir", "/fake/wt/.git\n")
        .text("wt/.git", "gitdir: ../super/.git/worktrees/wt\n");
    let (progress, tree) = run(&fs);
    let tree = tree.read().unwrap();

    assert_eq!(kind(&tree, "super"), Some(RootKind::Repository));
    let lib = entry(&tree, "super/lib").expect("an entry");
    assert_eq!(lib.kind, RootKind::Submodule);
    assert_eq!(
        lib.git_dir.as_deref(),
        Some(native("/fake/super/.git/modules/lib").as_path())
    );
    assert_eq!(kind(&tree, "super/lib/inner"), Some(RootKind::Submodule));
    assert_eq!(kind(&tree, "wt"), Some(RootKind::LinkedWorktree));
    // The submodules' Git directories are the superproject's data, not
    // repositories of their own.
    assert_eq!(kind(&tree, "super/.git/modules/lib"), None);
    assert_eq!(kind(&tree, "super/.git/modules/lib/modules/inner"), None);
    assert_eq!(
        kind(&tree, "super/.git/worktrees/wt"),
        Some(RootKind::WorktreeAdmin)
    );
    assert_eq!(
        tree.git_class(find(&tree, "super/.git/modules/lib/HEAD")),
        GitClass::Metadata
    );

    assert_eq!(progress.git_roots, 4, "super, lib, inner, wt");
    let (rows, totals) = summarize(&tree, None, Metric::Logical);
    assert_eq!(rows.len(), 4);
    assert_eq!((totals.roots, totals.worktrees), (3, 1));
    assert_eq!(totals.unresolved, 0);
}

#[test]
fn separate_git_directories_inside_and_outside_the_scan() {
    let fs = FakeFs::new();
    git_dir(&fs, "store.git", "\tbare = false\n\tworktree = ../work\n");
    fs.text("work/.git", "gitdir: ../store.git\n")
        .file("work/a.txt", 1, 4096)
        .text("work2/.git", "gitdir: /elsewhere/sep.git\n")
        .outside_text(
            "/elsewhere/sep.git/HEAD",
            "0123456789abcdef0123456789abcdef01234567\n",
        )
        .outside_dir("/elsewhere/sep.git/objects")
        .outside_dir("/elsewhere/sep.git/refs");
    let (progress, tree) = run(&fs);
    let tree = tree.read().unwrap();

    let work = entry(&tree, "work").expect("an entry");
    assert_eq!(work.kind, RootKind::SeparateGitDir);
    assert_eq!(
        work.git_dir.as_deref(),
        Some(native("/fake/store.git").as_path())
    );
    let store = entry(&tree, "store.git").expect("an entry");
    assert_eq!(store.kind, RootKind::GitDir);
    assert_eq!(
        store.worktree.as_deref(),
        Some(native("/fake/work").as_path())
    );
    let work2 = entry(&tree, "work2").expect("an entry");
    assert_eq!(work2.kind, RootKind::SeparateGitDir);
    assert_eq!(work2.head, Some(Head::Detached("0123456789ab".into())));
    assert_eq!(progress.git_roots, 3);
}

#[test]
fn bare_repositories_need_core_bare_and_their_data_isnt_listed_again() {
    let fs = FakeFs::new();
    git_dir(&fs, "b.git", "\tbare = true\n");
    git_dir(&fs, "b.git/modules/sub", "");
    fs.text("b.git/worktrees/w/HEAD", "ref: refs/heads/w\n")
        .text("b.git/worktrees/w/commondir", "../..\n");
    // No config at all: a Git directory, kind unknown.
    fs.text("nc.git/HEAD", "ref: refs/heads/main\n")
        .dir("nc.git/objects")
        .dir("nc.git/refs");
    let (progress, tree) = run(&fs);
    let tree = tree.read().unwrap();

    let bare = entry(&tree, "b.git").expect("an entry");
    assert_eq!(bare.kind, RootKind::Bare);
    assert_eq!(bare.head, main_branch());
    assert_eq!(kind(&tree, "b.git/modules/sub"), None);
    assert_eq!(
        kind(&tree, "b.git/worktrees/w"),
        Some(RootKind::WorktreeAdmin)
    );
    assert_eq!(kind(&tree, "nc.git"), Some(RootKind::GitDir));
    assert_eq!(progress.git_roots, 2);
    assert_eq!(
        tree.git_class(find(&tree, "b.git/objects/pack/pack-1.pack")),
        GitClass::Metadata
    );
}

#[test]
fn names_alone_never_make_a_repository() {
    let fs = FakeFs::new();
    fs.file("a/worktrees/x/notes.txt", 1, 4096)
        .file("b/.codex/session.json", 1, 4096)
        .file("c/.git/readme.txt", 1, 4096)
        .text("d/HEAD", "not a ref\n")
        .dir("d/objects")
        .dir("d/refs")
        .text("e/.git/HEAD", "hello\n")
        .dir("e/.git/objects")
        .dir("e/.git/refs")
        .text("f/HEAD", "ref: refs/heads/main\n")
        .dir("f/objects")
        .file("modules/x/HEAD", 1, 1);
    let (progress, tree) = run(&fs);
    let tree = tree.read().unwrap();

    for path in [
        "a",
        "a/worktrees",
        "a/worktrees/x",
        "b",
        "b/.codex",
        "d",
        "f",
    ] {
        assert_eq!(kind(&tree, path), None, "{path}");
    }
    assert_eq!(problem(&tree, "c"), Some(Problem::NotGit));
    assert_eq!(problem(&tree, "e"), Some(Problem::NotGit));
    assert_eq!(kind(&tree, "c/.git"), None);
    assert_eq!(
        tree.git_class(find(&tree, "c/.git/readme.txt")),
        GitClass::Other
    );
    assert_eq!(progress.git_roots, 2, "the two unresolved folders");
    assert_eq!(progress.state, ScanState::Complete);
}

#[test]
fn broken_git_files_leave_their_folder_unresolved() {
    let fs = FakeFs::new();
    fs.text("malformed/.git", "hello\n")
        .text("large/.git", &format!("gitdir: {}\n", "x".repeat(5000)))
        .text("missing/.git", "gitdir: ../nowhere\n")
        .text("denied/.git", "gitdir: ../store\n")
        .deny_read("denied/.git")
        .cloud_file("cloud/.git", 30, 0)
        .text("nostructure/.git", "gitdir: ../bad\n")
        .text("bad/HEAD", "ref: refs/heads/main\n");
    let (progress, tree) = run(&fs);
    let tree = tree.read().unwrap();

    assert!(matches!(
        problem(&tree, "malformed"),
        Some(Problem::Malformed(_))
    ));
    assert_eq!(problem(&tree, "large"), Some(Problem::TooLarge));
    assert_eq!(
        problem(&tree, "missing"),
        Some(Problem::TargetMissing(native("/fake/nowhere")))
    );
    assert!(matches!(
        problem(&tree, "denied"),
        Some(Problem::Unreadable { kind: 0, .. })
    ));
    assert_eq!(problem(&tree, "cloud"), Some(Problem::Cloud));
    assert!(matches!(
        problem(&tree, "nostructure"),
        Some(Problem::Malformed(_))
    ));
    for dir in [
        "malformed",
        "large",
        "missing",
        "denied",
        "cloud",
        "nostructure",
    ] {
        assert_eq!(kind(&tree, dir), Some(RootKind::Unresolved), "{dir}");
        assert_eq!(kind(&tree, &format!("{dir}/.git")), None, "{dir}/.git");
    }
    assert_eq!(progress.git_roots, 6);
}

#[test]
fn a_git_link_is_never_followed() {
    let fs = FakeFs::new();
    fs.other("linked/.git", EntryKind::Symlink)
        .file("linked/a.txt", 1, 4096)
        .other("other-link", EntryKind::Symlink);
    let (progress, tree) = run(&fs);
    let tree = tree.read().unwrap();
    assert_eq!(problem(&tree, "linked"), Some(Problem::Link));
    assert_eq!(
        progress.omissions.count(OmissionReason::Symlink),
        2,
        "counted as before"
    );
    assert_eq!(progress.files, 1);
    assert_eq!(tree.git_class(find(&tree, "linked/a.txt")), GitClass::Other);
}

#[test]
fn a_git_folder_that_cant_be_listed_is_unresolved() {
    let fs = FakeFs::new();
    fs.fail_dir("r/.git", ErrorKind::PermissionDenied)
        .file("r/a.txt", 1, 4096);
    let (progress, tree) = run(&fs);
    let tree = tree.read().unwrap();
    assert!(matches!(
        problem(&tree, "r"),
        Some(Problem::Unreadable { kind: 0, .. })
    ));
    assert_eq!(
        progress.omissions.count(OmissionReason::PermissionDenied),
        1
    );
}

/// Releases `gate` once the scan returned or after [`HANG`]; whether it was
/// still held until then.
fn release_after_returning(
    gate: Gate,
) -> (std::sync::mpsc::Sender<()>, std::thread::JoinHandle<bool>) {
    let (finished, rx) = channel();
    let watchdog = std::thread::spawn(move || {
        let held = !matches!(rx.recv_timeout(HANG), Err(RecvTimeoutError::Timeout));
        gate.release();
        held
    });
    (finished, watchdog)
}

#[test]
fn cancel_is_prompt_while_a_probe_is_blocked() {
    let fs = FakeFs::new();
    worktree(&fs);
    let gate = fs.gate_read("wt/.git");
    let s = Scan::start(fs.root(), fs.clone(), config(), |_| {});
    assert!(gate.entered(HANG), "the probe never reached the gate");
    let tree = s.tree();
    let (finished, watchdog) = release_after_returning(gate);
    let asked = Instant::now();
    s.cancel();
    let progress = s.wait();
    let took = asked.elapsed();
    let _ = finished.send(());
    assert!(
        watchdog.join().unwrap(),
        "cancel waited for the blocked read"
    );

    assert_eq!(progress.state, ScanState::Cancelled);
    assert!(took < Duration::from_secs(5), "took {took:?}");
    let tree = tree.read().unwrap();
    assert_eq!(problem(&tree, "wt"), Some(Problem::NotChecked));
    assert!(
        tree.git().roots().all(|r| r.kind != RootKind::Pending),
        "nothing left pending"
    );
}

#[test]
fn scans_complete_only_once_probes_are_done() {
    let fs = FakeFs::new();
    worktree(&fs);
    let gate = fs.gate_read("wt/.git");
    let s = Scan::start(fs.root(), fs.clone(), config(), |_| {});
    assert!(gate.entered(HANG), "the probe never reached the gate");
    let deadline = Instant::now() + HANG;
    while s.progress().pending_dirs > 0 || s.progress().git_roots < 1 {
        assert!(Instant::now() < deadline, "the listings never finished");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(s.progress().state, ScanState::Scanning);
    {
        let tree = s.tree();
        let tree = tree.read().unwrap();
        assert_eq!(kind(&tree, "wt"), Some(RootKind::Pending));
    }
    gate.release();
    let tree = s.tree();
    let progress = s.wait();
    assert_eq!(progress.state, ScanState::Complete);
    assert_eq!(progress.git_roots, 2);
    assert_eq!(
        kind(&tree.read().unwrap(), "wt"),
        Some(RootKind::LinkedWorktree)
    );
}

#[test]
fn scans_inside_a_checkout_find_the_repository_above() {
    let fs = FakeFs::new();
    repo(&fs, "repo");
    fs.file("repo/src/lib/x.rs", 1, 4096);

    let (progress, tree) = run_at(&fs, fs.root().join("repo/src"));
    let tree = tree.read().unwrap();
    let (at, above) = tree.git().above().expect("found above");
    assert_eq!(at, Path::new("/fake/repo"));
    assert_eq!(above.kind, RootKind::Repository);
    assert_eq!(above.head, main_branch());
    assert_eq!(tree.git_class(find(&tree, "lib/x.rs")), GitClass::Checkout);
    assert_eq!(progress.git_roots, 1);
    assert_eq!(progress.files, 2, "the metadata above isn't scanned");

    let (_, tree) = run_at(&fs, fs.root().join("repo/.git/objects"));
    let tree = tree.read().unwrap();
    let (at, above) = tree.git().above().expect("found above");
    assert_eq!(at, Path::new("/fake/repo/.git"));
    assert_eq!(above.kind, RootKind::DotGit);
    assert_eq!(
        tree.git_class(find(&tree, "pack/pack-1.pack")),
        GitClass::Metadata
    );
}

#[test]
fn a_scan_of_a_git_folder_itself_is_metadata_of_the_repository_above() {
    let fs = FakeFs::new();
    repo(&fs, "repo");
    let (progress, tree) = run_at(&fs, fs.root().join("repo/.git"));
    let tree = tree.read().unwrap();
    assert_eq!(progress.state, ScanState::Complete);
    assert_eq!(
        tree.git_entry(NodeId::ROOT).map(|r| r.kind),
        Some(RootKind::DotGit)
    );
    let (at, above) = tree.git().above().expect("found above");
    assert_eq!(at, Path::new("/fake/repo"));
    assert_eq!(above.kind, RootKind::Repository);
    assert_eq!(tree.git_class(find(&tree, "HEAD")), GitClass::Metadata);
    assert_eq!(progress.git_roots, 1, "the repository above, once");
}

#[test]
fn the_repository_above_is_looked_for_only_so_far() {
    let fs = FakeFs::new();
    repo(&fs, "repo");
    let near = format!("repo{}", "/d".repeat(ANCESTOR_LIMIT));
    fs.file(&format!("{near}/a.txt"), 1, 1);
    let far = format!("{near}/d");
    fs.file(&format!("{far}/a.txt"), 1, 1);

    // The repository is the 64th folder up from `near`.
    let (_, tree) = run_at(&fs, fs.root().join(&near));
    assert_eq!(
        tree.read().unwrap().git().above().map(|a| a.1.kind),
        Some(RootKind::Repository)
    );
    let (progress, tree) = run_at(&fs, fs.root().join(&far));
    assert!(tree.read().unwrap().git().above().is_none());
    assert_eq!(progress.git_roots, 0);
}
