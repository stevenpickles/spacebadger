//! Scans real temporary directories with the platform's native adapter.

use sb_core::git::{GitClass, Problem, RootKind as GitKind};
use sb_core::tree::{NodeId, Tree, flags};
use sb_scan::{
    BATCH_ENTRIES, EntryKind, ErrorKind, FsAdapter, NativeFs, OmissionReason, Progress, Scan,
    ScanConfig, ScanState, refused,
};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("sb-scan-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        #[cfg(windows)]
        remove_junctions(&verbatim(&self.0));
        let _ = std::fs::remove_dir_all(verbatim(&self.0));
    }
}

/// Junctions must be removed as directories without recursing into targets.
#[cfg(windows)]
fn remove_junctions(dir: &Path) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let meta = std::fs::symlink_metadata(entry.path()).unwrap();
        if meta.is_symlink()
            || meta.file_type().is_dir() && std::fs::read_link(entry.path()).is_ok()
        {
            let _ = std::fs::remove_dir(entry.path());
        } else if meta.is_dir() {
            remove_junctions(&entry.path());
        }
    }
}

/// `path` in the `\\?\` form on Windows, where names ending in a dot or a
/// space survive only that way; as it is elsewhere.
fn verbatim(path: &Path) -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(format!(r"\\?\{}", path.display()))
    } else {
        path.to_path_buf()
    }
}

fn write(path: &Path, bytes: usize) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, vec![0x5a; bytes]).unwrap();
}

#[cfg(windows)]
fn make_dir_link(link: &Path, target: &Path) {
    // cmd parses "/x" as a switch, so use native separators only.
    let native = |p: &Path| p.as_os_str().to_string_lossy().replace('/', "\\");
    let output = std::process::Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(native(link))
        .arg(native(target))
        .output()
        .unwrap();
    assert!(output.status.success(), "mklink /J failed: {output:?}");
}

#[cfg(unix)]
fn make_dir_link(link: &Path, target: &Path) {
    std::os::unix::fs::symlink(target, link).unwrap();
}

fn scan(root: &Path) -> (Progress, Arc<RwLock<Tree>>) {
    let s = Scan::start(
        root.to_path_buf(),
        NativeFs::new(),
        ScanConfig::default(),
        |_| {},
    );
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
fn scans_a_real_tree_without_following_links() {
    let tmp = TempDir::new("tree");
    let root = tmp.0.join("root");
    write(&root.join("a/one.bin"), 1_048_577);
    write(&root.join("a/b/two.bin"), 4096);
    write(&root.join("empty.txt"), 0);
    write(&root.join("ünïcode name.txt"), 10);
    write(&tmp.0.join("outside/big.bin"), 2_000_000);
    std::fs::hard_link(root.join("a/one.bin"), root.join("a/b/one-alias.bin")).unwrap();
    make_dir_link(&root.join("link-out"), &tmp.0.join("outside"));
    make_dir_link(&root.join("a/loop"), &root);
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(root.join("a/one.bin"), root.join("file-link")).unwrap();
        assert!(
            std::process::Command::new("mkfifo")
                .arg(root.join("fifo"))
                .status()
                .unwrap()
                .success()
        );
    }

    let (progress, tree) = scan(&root);
    let tree = tree.read().unwrap();

    assert_eq!(
        progress.state,
        ScanState::CompleteWithOmissions,
        "{progress:?}"
    );
    assert_eq!(progress.files, 5);
    assert_eq!(progress.dirs, 2);
    assert_eq!(
        progress.logical,
        2 * 1_048_577 + 4096 + 10,
        "alias logical counted per entry"
    );
    assert_eq!(progress.hardlink_aliases, 1);
    assert_eq!(progress.unknown_allocation_files, 0);

    let o = &progress.omissions;
    let links = o.count(OmissionReason::Symlink) + o.count(OmissionReason::MountPoint);
    #[cfg(windows)]
    assert_eq!(links, 2, "two junctions");
    #[cfg(unix)]
    {
        assert_eq!(links, 3, "two dir symlinks and a file symlink");
        assert_eq!(o.count(OmissionReason::SpecialFile), 1, "fifo");
    }
    assert_eq!(o.errors(), 0);

    // Allocation for the hard-linked file is counted once.
    let one = find(&tree, "a/one.bin");
    let alias = find(&tree, "a/b/one-alias.bin");
    let (owner, alias) = if tree.alias_owner(alias).is_some() {
        (one, alias)
    } else {
        (alias, one)
    };
    assert_eq!(tree.alias_owner(alias), Some(owner));
    assert_ne!(tree.flags(owner) & flags::HARDLINKED, 0);
    let owner_alloc = tree.file_allocated(owner).get().unwrap();
    assert!(owner_alloc >= 1_048_577, "owner allocation {owner_alloc}");
    let two = tree
        .file_allocated(find(&tree, "a/b/two.bin"))
        .get()
        .unwrap();
    let uni = tree
        .file_allocated(find(&tree, "ünïcode name.txt"))
        .get()
        .unwrap();
    assert_eq!(progress.allocated, owner_alloc + two + uni);
    assert_eq!(
        tree.path(find(&tree, "ünïcode name.txt")),
        root.join("ünïcode name.txt")
    );
}

/// The native adapter, noting every file it re-measures.
#[cfg(windows)]
struct Counted {
    inner: NativeFs,
    measures: Arc<std::sync::Mutex<Vec<PathBuf>>>,
}

#[cfg(windows)]
impl FsAdapter for Counted {
    fn open_root(&self, root: &Path) -> Result<sb_scan::RootKind, sb_scan::ScanError> {
        self.inner.open_root(root)
    }

    fn read_dir(&self, dir: &Path, sink: &mut sb_scan::Sink<'_>) -> Result<(), sb_scan::ScanError> {
        self.inner.read_dir(dir, sink)
    }

    fn measure_file(&self, file: &Path) -> Result<sb_scan::Measured, sb_scan::ScanError> {
        self.measures.lock().unwrap().push(file.to_path_buf());
        self.inner.measure_file(file)
    }
}

/// Scans `root` natively, with every hard-linked name re-measured or not;
/// returns what it found and the files it re-measured.
#[cfg(windows)]
fn scan_counted(
    root: &Path,
    exact_hard_links: bool,
) -> (Progress, Arc<RwLock<Tree>>, Vec<PathBuf>) {
    let measures = Arc::default();
    let adapter = Counted {
        inner: NativeFs::new(),
        measures: Arc::clone(&measures),
    };
    let config = ScanConfig {
        exact_hard_links,
        ..ScanConfig::default()
    };
    let s = Scan::start(root.to_path_buf(), adapter, config, |_| {});
    let tree = s.tree();
    let progress = s.wait();
    let measures = std::mem::take(&mut *measures.lock().unwrap());
    (progress, tree, measures)
}

/// NTFS updates the directory entry of only the name a file was written
/// through, so the other name's listing is stale: the scan sees the two
/// names disagree, re-measures the file once, and gets both right.
#[cfg(windows)]
#[test]
fn a_hard_link_written_through_another_name_is_remeasured_once() {
    use std::io::Write;
    let tmp = TempDir::new("stale-link");
    let root = tmp.0.join("root");
    write(&root.join("x/a.bin"), 5000);
    std::fs::create_dir_all(root.join("y")).unwrap();
    std::fs::hard_link(root.join("x/a.bin"), root.join("y/b.bin")).unwrap();
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(root.join("x/a.bin"))
        .unwrap();
    file.write_all(&[1; 10_000]).unwrap();
    drop(file);
    let fs = NativeFs::new();
    let mut stale = None;
    fs.read_dir(&root.join("y"), &mut |batch| {
        stale = batch.first().map(|e| e.logical);
        ControlFlow::Continue(())
    })
    .unwrap();
    assert_eq!(stale, Some(5000), "y/b.bin's listing is stale");

    let (progress, tree, measures) = scan_counted(&root, false);
    assert_eq!(progress.state, ScanState::Complete);
    assert_eq!(
        measures.len(),
        1,
        "one re-measure for the file: {measures:?}"
    );
    let tree = tree.read().unwrap();
    let (a, b) = (find(&tree, "x/a.bin"), find(&tree, "y/b.bin"));
    let owner = if tree.alias_owner(b) == Some(a) { a } else { b };
    assert_eq!(measures[0], tree.path(owner), "measured by its first name");
    assert_eq!(progress.hardlink_aliases, 1);
    assert_eq!((tree.logical(a), tree.logical(b)), (15_000, 15_000));
    assert_eq!(progress.logical, 30_000);
    assert_eq!(progress.allocated, 16_384, "allocated once");
}

/// Names whose listings agree are taken as listed; asked to, the scan
/// re-measures every name as it did before.
#[cfg(windows)]
#[test]
fn hard_links_whose_listings_agree_are_not_remeasured() {
    let tmp = TempDir::new("fresh-link");
    let root = tmp.0.join("root");
    write(&root.join("x/a.bin"), 5000);
    std::fs::create_dir_all(root.join("y")).unwrap();
    // Making a link updates the entries of the name linked from and the new
    // name alike.
    std::fs::hard_link(root.join("x/a.bin"), root.join("y/b.bin")).unwrap();

    let (progress, tree, measures) = scan_counted(&root, false);
    assert_eq!(measures, Vec::<PathBuf>::new());
    let (exact, exact_tree, exact_measures) = scan_counted(&root, true);
    assert_eq!(exact_measures.len(), 2, "the alias and its owner");
    for (progress, tree) in [(progress, tree), (exact, exact_tree)] {
        assert_eq!(progress.state, ScanState::Complete);
        assert_eq!(progress.hardlink_aliases, 1);
        assert_eq!(progress.logical, 10_000);
        assert_eq!(progress.allocated, 8192, "allocated once");
        let tree = tree.read().unwrap();
        assert_eq!(tree.logical(find(&tree, "y/b.bin")), 5000);
    }

    // A second link changes the file's change time, which b.bin's entry
    // misses: one re-measure at most, and the same sizes.
    std::fs::hard_link(root.join("x/a.bin"), root.join("y/c.bin")).unwrap();
    let (progress, _, measures) = scan_counted(&root, false);
    assert!(measures.len() <= 1, "{measures:?}");
    assert_eq!(progress.hardlink_aliases, 2);
    assert_eq!(progress.logical, 15_000);
    assert_eq!(progress.allocated, 8192);
}

/// A WOF-compressed file lists 0 allocated under every name, so agreeing
/// listings prove nothing: its names are re-measured as before.
#[cfg(windows)]
#[test]
fn compressed_hard_links_are_remeasured_though_their_listings_agree() {
    let tmp = TempDir::new("wof-link");
    let root = tmp.0.join("root");
    let file = root.join("x/a.bin");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, b"compressible ".repeat(20_000)).unwrap();
    let compact = std::process::Command::new("compact")
        .args(["/c", "/exe:xpress4k"])
        .arg(&file)
        .output()
        .unwrap();
    assert!(compact.status.success(), "compact failed: {compact:?}");
    std::fs::create_dir_all(root.join("y")).unwrap();
    std::fs::hard_link(&file, root.join("y/b.bin")).unwrap();
    let measured = NativeFs::new().measure_file(&file).unwrap();

    let (progress, _, measures) = scan_counted(&root, false);
    assert_eq!(measures.len(), 2, "the alias and its owner: {measures:?}");
    assert_eq!(progress.hardlink_aliases, 1);
    assert_eq!(Some(progress.allocated), measured.allocated);
    assert_eq!(progress.logical, 2 * 260_000);
}

#[test]
fn rejects_a_link_as_the_root() {
    let tmp = TempDir::new("linkroot");
    std::fs::create_dir(tmp.0.join("real")).unwrap();
    make_dir_link(&tmp.0.join("link"), &tmp.0.join("real"));
    let (progress, _) = scan(&tmp.0.join("link"));
    assert!(
        matches!(progress.state, ScanState::Failed(ref m) if m.contains("symbolic link")),
        "{:?}",
        progress.state
    );
}

#[test]
fn missing_root_fails() {
    let tmp = TempDir::new("missing");
    let (progress, _) = scan(&tmp.0.join("does-not-exist"));
    assert!(
        matches!(progress.state, ScanState::Failed(_)),
        "{:?}",
        progress.state
    );
}

#[test]
fn fixed_disks_without_seeks_use_more_workers() {
    let local = std::env::temp_dir();
    assert!(!sb_scan::native::is_remote(&local));
    let config = sb_scan::native::scan_config(&local);
    let default = ScanConfig::default();
    if cfg!(windows) {
        assert!(sb_scan::native::is_local_fixed(&local));
        let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
        let (low, high) = sb_scan::native::LOCAL_WORKERS;
        let expected = if sb_scan::native::no_seek_penalty(&local) {
            cores.clamp(low, high)
        } else {
            default.workers
        };
        assert_eq!(config.workers, expected);
    } else {
        assert_eq!(config.workers, default.workers);
    }
}

#[test]
#[cfg(any(windows, target_os = "linux"))]
fn reports_volume_capacity_and_whether_the_root_is_the_volume_top() {
    let folder = TempDir::new("volume");
    let inside = sb_scan::native::volume_info(&folder.0).expect("volume info");
    assert!(!inside.is_root, "a temporary folder is inside its volume");
    assert!(inside.capacity > 0 && inside.free <= inside.capacity);

    let top = folder.0.ancestors().last().expect("path has a root");
    let volume = sb_scan::native::volume_info(top).expect("volume info");
    assert!(volume.is_root, "{} is a volume's top folder", top.display());
    assert_eq!(volume.capacity, inside.capacity);
}

#[test]
fn records_modified_times_from_the_listing() {
    let tmp = TempDir::new("mtime");
    write(&tmp.0.join("old.txt"), 3);
    write(&tmp.0.join("sub/new.txt"), 3);
    let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
    std::fs::File::options()
        .write(true)
        .open(tmp.0.join("old.txt"))
        .unwrap()
        .set_modified(old)
        .unwrap();
    let new = std::fs::metadata(tmp.0.join("sub/new.txt"))
        .unwrap()
        .modified()
        .unwrap();
    let (_, tree) = scan(&tmp.0);
    let tree = tree.read().unwrap();
    assert_eq!(tree.modified(find(&tree, "old.txt")), Some(1_000_000_000));
    let secs = new.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    assert_eq!(tree.modified(find(&tree, "sub/new.txt")), Some(secs as u32));
    assert_eq!(tree.modified(find(&tree, "sub")), None, "folders have none");
}

#[test]
fn wide_folders_are_listed_in_bounded_batches() {
    let tmp = TempDir::new("wide");
    let n = 2 * BATCH_ENTRIES + 7;
    for i in 0..n {
        std::fs::File::create(tmp.0.join(format!("f{i:05}"))).unwrap();
    }
    let fs = NativeFs::new();
    fs.open_root(&tmp.0).unwrap();

    let mut sizes = Vec::new();
    let mut names = std::collections::HashSet::new();
    fs.read_dir(&tmp.0, &mut |batch| {
        sizes.push(batch.len());
        names.extend(batch.into_iter().map(|e| e.name));
        ControlFlow::Continue(())
    })
    .unwrap();
    assert!(sizes.iter().all(|&s| s <= BATCH_ENTRIES), "{sizes:?}");
    assert_eq!(sizes.iter().sum::<usize>(), n);
    assert_eq!(names.len(), n, "each entry once, no . or ..");

    let mut batches = 0;
    fs.read_dir(&tmp.0, &mut |_| {
        batches += 1;
        ControlFlow::Break(())
    })
    .unwrap();
    assert_eq!(batches, 1, "a listing stops when asked");
}

#[test]
fn reads_small_files_only_when_plain_and_never_through_links() {
    let tmp = TempDir::new("readsmall");
    let head = tmp.0.join("HEAD");
    std::fs::write(&head, b"ref: refs/heads/main\n").unwrap();
    write(&tmp.0.join("big"), 5000);
    std::fs::create_dir(tmp.0.join("folder")).unwrap();
    let fs = NativeFs::new();
    fs.open_root(&tmp.0).unwrap();

    assert_eq!(
        fs.read_small(&head, 4096).unwrap(),
        b"ref: refs/heads/main\n"
    );
    let too_large = fs.read_small(&tmp.0.join("big"), 4096).unwrap_err();
    assert_eq!(too_large.message, refused::TOO_LARGE);
    assert_eq!(fs.read_small(&tmp.0.join("big"), 5000).unwrap().len(), 5000);
    assert_eq!(
        fs.read_small(&tmp.0.join("gone"), 4096).unwrap_err().kind,
        ErrorKind::Vanished
    );
    assert_eq!(
        fs.read_small(&tmp.0.join("folder"), 4096)
            .unwrap_err()
            .message,
        refused::NOT_FILE
    );
    assert_eq!(fs.stat_kind(&head).unwrap(), EntryKind::File);
    assert_eq!(
        fs.stat_kind(&tmp.0.join("folder")).unwrap(),
        EntryKind::Directory
    );

    make_dir_link(&tmp.0.join("dir-link"), &tmp.0.join("folder"));
    let err = fs.read_small(&tmp.0.join("dir-link"), 4096).unwrap_err();
    assert_eq!(err.message, refused::LINK, "{err:?}");
    assert!(matches!(
        fs.stat_kind(&tmp.0.join("dir-link")).unwrap(),
        EntryKind::Symlink | EntryKind::MountPoint
    ));

    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(&head, tmp.0.join("file-link"));
    // Making a symbolic link needs a privilege or developer mode.
    #[cfg(windows)]
    let linked = std::os::windows::fs::symlink_file(&head, tmp.0.join("file-link"));
    match linked {
        Ok(()) => {
            let err = fs.read_small(&tmp.0.join("file-link"), 4096).unwrap_err();
            assert_eq!(err.message, refused::LINK, "{err:?}");
            assert_eq!(
                fs.stat_kind(&tmp.0.join("file-link")).unwrap(),
                EntryKind::Symlink
            );
        }
        Err(err) => eprintln!("skipped file symlink: {err}"),
    }

    // A FIFO named HEAD is refused without waiting for a writer.
    #[cfg(unix)]
    {
        let fifo = tmp.0.join("fifo/HEAD");
        std::fs::create_dir(tmp.0.join("fifo")).unwrap();
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let err = fs.read_small(&fifo, 4096).unwrap_err();
        assert_eq!(err.message, refused::NOT_FILE, "{err:?}");
        assert_eq!(fs.stat_kind(&fifo).unwrap(), EntryKind::Special);
    }
}

#[test]
fn finds_repositories_written_by_hand() {
    let tmp = TempDir::new("git");
    let root = tmp.0.join("root");
    let git = root.join("main/.git");
    write(&root.join("main/src/a.rs"), 10);
    std::fs::create_dir_all(git.join("objects/pack")).unwrap();
    std::fs::create_dir_all(git.join("refs/heads")).unwrap();
    std::fs::write(git.join("HEAD"), b"ref: refs/heads/main\n").unwrap();
    std::fs::write(git.join("config"), b"[core]\n\tbare = false\n").unwrap();
    let admin = git.join("worktrees/wt");
    std::fs::create_dir_all(&admin).unwrap();
    std::fs::write(admin.join("HEAD"), b"ref: refs/heads/topic\n").unwrap();
    std::fs::write(admin.join("commondir"), b"../..\n").unwrap();
    let wt = root.join("wt");
    std::fs::write(
        admin.join("gitdir"),
        format!("{}\n", wt.join(".git").display()),
    )
    .unwrap();
    write(&wt.join("b.rs"), 10);
    std::fs::write(wt.join(".git"), b"gitdir: ../main/.git/worktrees/wt\r\n").unwrap();

    let (progress, tree) = scan(&root);
    let tree = tree.read().unwrap();
    assert_eq!(progress.state, ScanState::Complete, "{progress:?}");
    assert_eq!(progress.git_roots, 2);
    let kind = |p: &str| tree.git_entry(find(&tree, p)).map(|r| r.kind);
    assert_eq!(kind("main"), Some(GitKind::Repository));
    assert_eq!(kind("wt"), Some(GitKind::LinkedWorktree));
    assert_eq!(kind("main/.git/worktrees/wt"), Some(GitKind::WorktreeAdmin));
    let wt_entry = tree.git_entry(find(&tree, "wt")).unwrap();
    assert_eq!(wt_entry.recorded_at, None, "{wt_entry:?}");
    assert_eq!(tree.git_class(find(&tree, "wt/b.rs")), GitClass::Worktree);
}

#[test]
fn never_reads_metadata_through_a_link_in_its_path() {
    let tmp = TempDir::new("gitlink");
    let real = tmp.0.join("elsewhere/real.git");
    std::fs::create_dir_all(real.join("objects")).unwrap();
    std::fs::create_dir_all(real.join("refs")).unwrap();
    std::fs::write(real.join("HEAD"), b"ref: refs/heads/main\n").unwrap();
    let root = tmp.0.join("root");
    write(&root.join("wt/a.txt"), 1);
    std::fs::write(root.join("wt/.git"), b"gitdir: ../lnk/real.git\n").unwrap();
    make_dir_link(&root.join("lnk"), &tmp.0.join("elsewhere"));

    let (_, tree) = scan(&root);
    let tree = tree.read().unwrap();
    let wt = tree.git_entry(find(&tree, "wt")).expect("an entry");
    assert_eq!(wt.problem, Some(Problem::NotRead), "{wt:?}");
}
#[test]
fn records_an_id_for_every_file_and_folder() {
    let tmp = TempDir::new("ids");
    write(&tmp.0.join("a.txt"), 3);
    write(&tmp.0.join("sub/b.txt"), 3);
    let (_, tree) = scan(&tmp.0);
    let tree = tree.read().unwrap();
    let ids: Vec<_> = ["a.txt", "sub", "sub/b.txt"]
        .iter()
        .map(|p| tree.id(find(&tree, p)).expect("an id"))
        .collect();
    assert!(ids[0] != ids[1] && ids[1] != ids[2] && ids[0] != ids[2]);
}

#[test]
fn lists_folders_whose_names_end_in_a_dot_or_a_space() {
    let tmp = TempDir::new("odd-names");
    // Names Windows only keeps through `\\?\` paths.
    let odd = verbatim(&tmp.0);
    write(&odd.join("foo.").join("x.bin"), 3);
    write(&odd.join("foo.").join("inner").join("y.bin"), 4);
    write(&odd.join("bar ").join("z.bin"), 5);
    write(&odd.join("foo").join("w.bin"), 6);
    let (progress, tree) = scan(&tmp.0);
    assert_eq!(
        progress.state,
        ScanState::Complete,
        "{:?}",
        progress.omissions
    );
    assert_eq!(progress.files, 4);
    let tree = tree.read().unwrap();
    for (path, size) in [
        ("foo./x.bin", 3),
        ("foo./inner/y.bin", 4),
        ("bar /z.bin", 5),
        ("foo/w.bin", 6),
    ] {
        assert_eq!(tree.logical(find(&tree, path)), size, "{path}");
    }
}
