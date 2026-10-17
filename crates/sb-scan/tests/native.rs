//! Scans real temporary directories with the platform's native adapter.

use sb_core::git::{GitClass, RootKind as GitKind};
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
        remove_junctions(&self.0);
        let _ = std::fs::remove_dir_all(&self.0);
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
fn local_folders_use_the_default_worker_count() {
    let local = std::env::temp_dir();
    assert!(!sb_scan::native::is_remote(&local));
    assert_eq!(
        sb_scan::native::scan_config(&local).workers,
        ScanConfig::default().workers
    );
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
