//! Git detection during a scan. Never runs `git`: the scan spots candidates
//! by name in listings it applies anyway (a `.git` entry; `HEAD`, `objects`
//! and `refs` side by side), and a worker then confirms each with a few
//! capped reads through [`FsAdapter::read_small`] and
//! [`FsAdapter::stat_kind`], which never follow a link or download a cloud
//! file. What a candidate turns out to be is a [`Finding`] the scan writes
//! into the tree; see [`sb_core::git`] for the model.
//!
//! Every class needs a valid `HEAD` plus structure (`objects` and `refs`
//! folders, or a `commondir`); a folder merely named `.git`, `worktrees` or
//! `.codex` is never taken for a repository. Git directories outside the
//! scan are only read, never listed or counted.

use crate::adapter::{EntryKind, ErrorKind, FsAdapter, ScanError, refused};
use sb_core::git::paths::{self, Style};
use sb_core::git::{
    GitRoot, Head, Problem, RootKind, parse_config_core, parse_head, parse_path_file, parse_pointer,
};
use sb_core::tree::NodeId;
use std::path::{Component, Path, PathBuf};

/// Most bytes read of `HEAD`, a `.git` file, `commondir` or `gitdir`.
pub const SMALL_CAP: usize = 4 * 1024;
/// Most bytes read of `config`.
pub const CONFIG_CAP: usize = 64 * 1024;
/// Most folders above the scan root looked at for a repository the scan is
/// inside of.
pub const ANCESTOR_LIMIT: usize = 64;

/// Whether names compare ignoring case, as the platform's filesystems
/// usually do.
const FOLD_CASE: bool = cfg!(any(windows, target_os = "macos"));

/// Whether the entry name `name` is `literal` (ASCII, compared ignoring
/// case on Windows and macOS).
#[inline]
pub fn name_eq(name: &[u8], literal: &[u8]) -> bool {
    name.len() == literal.len()
        && if FOLD_CASE {
            name.eq_ignore_ascii_case(literal)
        } else {
            name == literal
        }
}

/// Git's names a listing can show in a folder, as bits of [`Signals`].
mod bit {
    pub const HEAD: u8 = 1;
    pub const CONFIG: u8 = 2;
    pub const COMMONDIR: u8 = 4;
    pub const GITDIR: u8 = 8;
    pub const OBJECTS: u8 = 16;
    pub const REFS: u8 = 32;
}

/// What a listing showed of a Git directory's layout in one folder.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Signals(u8);

impl Signals {
    fn has(self, bit: u8) -> bool {
        self.0 & bit != 0
    }

    /// `HEAD` with `objects` and `refs`, or with `commondir` (a linked
    /// worktree's Git directory): worth confirming.
    pub fn qualifies(self) -> bool {
        const STRUCTURE: u8 = bit::OBJECTS | bit::REFS;
        self.has(bit::HEAD) && (self.0 & STRUCTURE == STRUCTURE || self.has(bit::COMMONDIR))
    }

    /// Has `commondir`: a linked worktree's Git directory.
    pub fn is_worktree_admin(self) -> bool {
        self.has(bit::COMMONDIR)
    }
}

/// What an entry's name says about Git.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Spot {
    /// A `.git` entry of any kind.
    DotGit,
    /// One of a Git directory's files or folders.
    Signal(u8),
}

impl Spot {
    /// Adds a signal to a folder's.
    pub fn add_to(self, signals: &mut Signals) {
        if let Self::Signal(b) = self {
            signals.0 |= b;
        }
    }
}

/// What the entry `name` of kind `kind` says about Git, if anything. Byte
/// compares on the name only, cheap enough for every entry of a scan.
#[inline]
pub fn spot(name: &[u8], kind: EntryKind) -> Option<Spot> {
    if !(4..=9).contains(&name.len()) {
        return None;
    }
    let file = kind == EntryKind::File;
    let dir = kind == EntryKind::Directory;
    let signal = |b: u8, ok: bool| ok.then_some(Spot::Signal(b));
    match name.len() {
        4 if name_eq(name, b".git") => Some(Spot::DotGit),
        4 if name_eq(name, b"HEAD") => signal(bit::HEAD, file),
        4 if name_eq(name, b"refs") => signal(bit::REFS, dir),
        6 if name_eq(name, b"config") => signal(bit::CONFIG, file),
        6 if name_eq(name, b"gitdir") => signal(bit::GITDIR, file),
        7 if name_eq(name, b"objects") => signal(bit::OBJECTS, dir),
        9 if name_eq(name, b"commondir") => signal(bit::COMMONDIR, file),
        _ => None,
    }
}

/// Whether `path` lies inside a folder named `.git` (not counting its own
/// name), where any Git directory is that repository's own data (its
/// submodules' and worktrees' Git directories).
pub fn inside_dotgit(path: &Path) -> bool {
    let mut names = path.components().filter_map(|c| match c {
        Component::Normal(n) => Some(n.as_encoded_bytes()),
        _ => None,
    });
    names.next_back();
    names.any(|n| name_eq(n, b".git"))
}

/// What a worker is asked to confirm.
#[derive(Debug, Clone)]
pub enum Plan {
    /// A folder whose listing showed `HEAD` with `objects` and `refs` or
    /// with `commondir`. `owner` is the folder holding it when it's named
    /// `.git` inside the scan.
    AdminDir {
        dir: NodeId,
        dotgit: bool,
        owner: Option<NodeId>,
        signals: Signals,
    },
    /// The `.git` file `file` in the folder `dir`.
    Pointer { file: NodeId, dir: NodeId },
    /// The folders above the scan root, for a repository the scan is
    /// inside of.
    Ancestors,
}

/// What a probe found: entries to write into the tree.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Finding {
    pub roots: Vec<GitRoot>,
    pub above: Option<(PathBuf, GitRoot)>,
}

impl Finding {
    pub fn is_empty(&self) -> bool {
        self.roots.is_empty() && self.above.is_none()
    }
}

/// Confirms the candidate at `path` (the folder for [`Plan::AdminDir`] and
/// [`Plan::Ancestors`], the `.git` file for [`Plan::Pointer`]). All its I/O
/// goes through `fs`; `style` says how to read paths found in metadata.
pub fn probe<A: FsAdapter + ?Sized>(fs: &A, path: &Path, plan: &Plan, style: Style) -> Finding {
    match *plan {
        Plan::AdminDir {
            dir,
            dotgit,
            owner,
            signals,
        } => admin_dir(fs, path, dir, dotgit, owner, signals, style),
        Plan::Pointer { file, dir } => {
            let Some(folder) = path.parent() else {
                return Finding::default();
            };
            let root = pointer(fs, folder, dir, style);
            let mut roots = Vec::with_capacity(2);
            // The `.git` file is metadata once its repository is known.
            if root.kind != RootKind::Unresolved {
                roots.push(GitRoot {
                    git_dir: root.git_dir.clone(),
                    ..GitRoot::new(file, RootKind::DotGit)
                });
            }
            roots.push(root);
            Finding { roots, above: None }
        }
        Plan::Ancestors => Finding {
            roots: Vec::new(),
            above: ancestors(fs, path, style),
        },
    }
}

/// Why a read failed, as the problem shown.
fn problem(err: ScanError) -> Problem {
    match err.message.as_str() {
        refused::LINK => Problem::Link,
        refused::CLOUD => Problem::Cloud,
        refused::TOO_LARGE => Problem::TooLarge,
        _ => Problem::Unreadable {
            kind: err.kind as u8,
            message: err.message,
        },
    }
}

/// Reads and parses `HEAD`: `Ok(None)` when it isn't Git's.
fn read_head<A: FsAdapter + ?Sized>(fs: &A, head: &Path) -> Result<Option<Head>, ScanError> {
    fs.read_small(head, SMALL_CAP).map(|b| parse_head(&b))
}

/// Reads a one-path file (`commondir`, `gitdir`) relative to `base`:
/// `Ok(None)` when absent, `Err` when unreadable or not a path.
fn read_path<A: FsAdapter + ?Sized>(
    fs: &A,
    base: &Path,
    name: &str,
    style: Style,
) -> Result<Option<PathBuf>, Problem> {
    match fs.read_small(&base.join(name), SMALL_CAP) {
        Ok(bytes) => match parse_path_file(&bytes) {
            Some(raw) => Ok(Some(paths::resolve(base, raw, style))),
            None => Err(Problem::Malformed(match name {
                "commondir" => "commondir doesn't name a folder",
                _ => "gitdir doesn't name a file",
            })),
        },
        Err(err) if err.kind == ErrorKind::Vanished => Ok(None),
        Err(err) => Err(problem(err)),
    }
}

/// `core.bare` and `core.worktree`, or nothing known when `config` can't be
/// read.
fn read_config<A: FsAdapter + ?Sized>(
    fs: &A,
    dir: &Path,
    style: Style,
) -> (Option<bool>, Option<PathBuf>) {
    match fs.read_small(&dir.join("config"), CONFIG_CAP) {
        Ok(bytes) => {
            let core = parse_config_core(&bytes);
            let worktree = core.worktree.map(|w| paths::resolve(dir, &w, style));
            (core.bare, worktree)
        }
        Err(_) => (None, None),
    }
}

/// Whether `path` is a folder (not through a link).
fn is_dir<A: FsAdapter + ?Sized>(fs: &A, path: &Path) -> bool {
    matches!(fs.stat_kind(path), Ok(EntryKind::Directory))
}

/// A folder's `HEAD`, when it has a valid one and `objects` and `refs`
/// folders: a Git directory found without a listing (above the scan).
fn git_dir_head<A: FsAdapter + ?Sized>(fs: &A, dir: &Path) -> Option<Head> {
    let head = read_head(fs, &dir.join("HEAD")).ok()??;
    (is_dir(fs, &dir.join("objects")) && is_dir(fs, &dir.join("refs"))).then_some(head)
}

/// A worktree's folder, from the path to its `.git` file.
fn checkout_of(dotgit: PathBuf) -> PathBuf {
    let named = dotgit
        .file_name()
        .is_some_and(|n| name_eq(n.as_encoded_bytes(), b".git"));
    match dotgit.parent() {
        Some(parent) if named => parent.to_path_buf(),
        _ => dotgit,
    }
}

fn admin_dir<A: FsAdapter + ?Sized>(
    fs: &A,
    path: &Path,
    dir: NodeId,
    dotgit: bool,
    owner: Option<NodeId>,
    signals: Signals,
    style: Style,
) -> Finding {
    let single = |root| Finding {
        roots: vec![root],
        above: None,
    };
    let head = match read_head(fs, &path.join("HEAD")) {
        Ok(Some(head)) => head,
        // Only a folder named `.git` claims to be Git's; anything else with
        // a stray `HEAD` is left alone.
        failed => {
            return match owner {
                Some(owner) if dotgit => single(GitRoot::unresolved(
                    owner,
                    failed.map_or_else(problem, |_| Problem::NotGit),
                )),
                _ => Finding::default(),
            };
        }
    };
    let (bare, worktree) = if signals.has(bit::CONFIG) {
        read_config(fs, path, style)
    } else {
        (None, None)
    };
    let git_dir = Some(path.to_path_buf());
    if dotgit {
        let mut roots = vec![GitRoot {
            head: Some(head.clone()),
            git_dir: git_dir.clone(),
            ..GitRoot::new(dir, RootKind::DotGit)
        }];
        if let Some(owner) = owner {
            roots.push(GitRoot {
                head: Some(head),
                git_dir,
                worktree,
                ..GitRoot::new(owner, RootKind::Repository)
            });
        }
        return Finding { roots, above: None };
    }
    if signals.has(bit::COMMONDIR) {
        // A linked worktree's administrative folder: information only.
        let common_dir = read_path(fs, path, "commondir", style).ok().flatten();
        let checkout = if signals.has(bit::GITDIR) {
            read_path(fs, path, "gitdir", style)
                .ok()
                .flatten()
                .map(checkout_of)
        } else {
            None
        };
        return single(GitRoot {
            head: Some(head),
            git_dir,
            common_dir,
            worktree: checkout,
            ..GitRoot::new(dir, RootKind::WorktreeAdmin)
        });
    }
    if in_parent_modules(fs, path) {
        // A submodule's Git directory inside a bare repository (or Git
        // directory): that repository's own data, not a repository of its
        // own.
        return Finding::default();
    }
    let kind = if bare == Some(true) {
        RootKind::Bare
    } else {
        RootKind::GitDir
    };
    single(GitRoot {
        head: Some(head),
        git_dir,
        worktree,
        ..GitRoot::new(dir, kind)
    })
}

/// Whether `dir` lies in `<G>/modules/` of a Git directory G (checked by
/// reading, so a folder that just happens to be named `modules` doesn't
/// count).
fn in_parent_modules<A: FsAdapter + ?Sized>(fs: &A, dir: &Path) -> bool {
    dir.ancestors().skip(1).any(|a| {
        a.file_name()
            .is_some_and(|n| name_eq(n.as_encoded_bytes(), b"modules"))
            && a.parent().is_some_and(|g| git_dir_head(fs, g).is_some())
    })
}

/// Follows the `.git` file in `folder`, which is node `node` (the scan root
/// for a folder above the scan): a linked worktree, submodule or separate
/// Git directory, or unresolved with the reason.
fn pointer<A: FsAdapter + ?Sized>(fs: &A, folder: &Path, node: NodeId, style: Style) -> GitRoot {
    pointer_target(fs, folder, node, style).unwrap_or_else(|p| GitRoot::unresolved(node, p))
}

fn pointer_target<A: FsAdapter + ?Sized>(
    fs: &A,
    folder: &Path,
    node: NodeId,
    style: Style,
) -> Result<GitRoot, Problem> {
    let dotgit = folder.join(".git");
    let bytes = fs.read_small(&dotgit, SMALL_CAP).map_err(problem)?;
    let target = paths::resolve(folder, parse_pointer(&bytes)?, style);
    let head = match read_head(fs, &target.join("HEAD")) {
        Ok(Some(head)) => head,
        Ok(None) => return Err(Problem::Malformed("the Git directory's HEAD isn't valid")),
        Err(err) if err.kind == ErrorKind::Vanished => return Err(Problem::TargetMissing(target)),
        Err(err) => return Err(problem(err)),
    };
    if let Some(common) = read_path(fs, &target, "commondir", style)? {
        // Where the worktree's Git directory says the worktree is; noted
        // only when that's elsewhere (moved, or a copy).
        let recorded_at = read_path(fs, &target, "gitdir", style)
            .ok()
            .flatten()
            .map(checkout_of)
            .filter(|at| paths::relative_to(folder, at, style).is_none_or(|r| !r.is_empty()));
        return Ok(GitRoot {
            head: Some(head),
            git_dir: Some(target),
            common_dir: Some(common),
            recorded_at,
            ..GitRoot::new(node, RootKind::LinkedWorktree)
        });
    }
    if !(is_dir(fs, &target.join("objects")) && is_dir(fs, &target.join("refs"))) {
        return Err(Problem::Malformed(
            "the Git directory has no objects or refs",
        ));
    }
    let kind = if in_modules(&target) {
        RootKind::Submodule
    } else {
        RootKind::SeparateGitDir
    };
    Ok(GitRoot {
        head: Some(head),
        git_dir: Some(target),
        ..GitRoot::new(node, kind)
    })
}

/// Whether a Git directory lies in a superproject's `.git/modules`.
fn in_modules(git_dir: &Path) -> bool {
    let names: Vec<&[u8]> = git_dir
        .components()
        .filter_map(|c| match c {
            Component::Normal(n) => Some(n.as_encoded_bytes()),
            _ => None,
        })
        .collect();
    let Some(dotgit) = names.iter().position(|n| name_eq(n, b".git")) else {
        return false;
    };
    // `modules` after the `.git`, with the submodule's name below it.
    names[dotgit + 1..names.len().saturating_sub(1).max(dotgit + 1)]
        .iter()
        .any(|n| name_eq(n, b"modules"))
}

/// The repository the scan root is inside of: the nearest of up to
/// [`ANCESTOR_LIMIT`] folders above it that is a `.git` folder, holds a
/// `.git` folder or file, or is a bare repository or Git directory. Like
/// `git`, doesn't stop at mount points.
fn ancestors<A: FsAdapter + ?Sized>(
    fs: &A,
    root: &Path,
    style: Style,
) -> Option<(PathBuf, GitRoot)> {
    root.ancestors()
        .skip(1)
        .take(ANCESTOR_LIMIT)
        .find_map(|level| ancestor_at(fs, level, style).map(|r| (level.to_path_buf(), r)))
}

fn ancestor_at<A: FsAdapter + ?Sized>(fs: &A, level: &Path, style: Style) -> Option<GitRoot> {
    let node = NodeId::ROOT;
    let named_dotgit = level
        .file_name()
        .is_some_and(|n| name_eq(n.as_encoded_bytes(), b".git"));
    if named_dotgit && let Some(head) = git_dir_head(fs, level) {
        return Some(GitRoot {
            head: Some(head),
            git_dir: Some(level.to_path_buf()),
            ..GitRoot::new(node, RootKind::DotGit)
        });
    }
    let dotgit = level.join(".git");
    match fs.stat_kind(&dotgit) {
        Ok(EntryKind::Directory) => {
            return Some(match git_dir_head(fs, &dotgit) {
                Some(head) => GitRoot {
                    head: Some(head),
                    worktree: read_config(fs, &dotgit, style).1,
                    git_dir: Some(dotgit),
                    ..GitRoot::new(node, RootKind::Repository)
                },
                None => GitRoot::unresolved(node, Problem::NotGit),
            });
        }
        Ok(EntryKind::File) => return Some(pointer(fs, level, node, style)),
        Ok(EntryKind::Symlink | EntryKind::MountPoint | EntryKind::UnknownReparse) => {
            return Some(GitRoot::unresolved(node, Problem::Link));
        }
        _ => {}
    }
    let head = git_dir_head(fs, level)?;
    let (bare, worktree) = read_config(fs, level, style);
    Some(GitRoot {
        head: Some(head),
        git_dir: Some(level.to_path_buf()),
        worktree,
        ..GitRoot::new(
            node,
            if bare == Some(true) {
                RootKind::Bare
            } else {
                RootKind::GitDir
            },
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spots_gits_names_by_kind() {
        let (f, d) = (EntryKind::File, EntryKind::Directory);
        assert_eq!(spot(b".git", f), Some(Spot::DotGit));
        assert_eq!(spot(b".git", EntryKind::Symlink), Some(Spot::DotGit));
        assert_eq!(spot(b"HEAD", f), Some(Spot::Signal(bit::HEAD)));
        assert_eq!(spot(b"HEAD", d), None, "a folder named HEAD isn't Git's");
        assert_eq!(spot(b"objects", d), Some(Spot::Signal(bit::OBJECTS)));
        assert_eq!(spot(b"objects", f), None);
        assert_eq!(spot(b"commondir", f), Some(Spot::Signal(bit::COMMONDIR)));
        assert_eq!(spot(b".gitignore", f), None);
        assert_eq!(spot(b"worktrees", d), None);
        assert_eq!(spot(b".codex", d), None);
        assert_eq!(spot(b"git", d), None);
        assert_eq!(spot(b".GIT", d).is_some(), FOLD_CASE);
        assert_eq!(spot(b"head", f).is_some(), FOLD_CASE);
    }

    #[test]
    fn signals_qualify_with_head_and_structure_only() {
        let of = |names: &[(&[u8], EntryKind)]| {
            let mut s = Signals::default();
            for (n, k) in names {
                if let Some(spot) = spot(n, *k) {
                    spot.add_to(&mut s);
                }
            }
            s.qualifies()
        };
        let (f, d) = (EntryKind::File, EntryKind::Directory);
        assert!(of(&[(b"HEAD", f), (b"objects", d), (b"refs", d)]));
        assert!(of(&[(b"HEAD", f), (b"commondir", f)]));
        assert!(!of(&[(b"HEAD", f), (b"objects", d)]));
        assert!(!of(&[(b"objects", d), (b"refs", d), (b"config", f)]));
        assert!(!of(&[(b"HEAD", f), (b"config", f), (b"gitdir", f)]));
    }

    #[test]
    fn knows_whats_inside_a_dotgit_folder() {
        assert!(inside_dotgit(Path::new("/r/.git/modules/sub")));
        assert!(!inside_dotgit(Path::new("/r/.git")));
        assert!(!inside_dotgit(Path::new("/r/sub")));
        assert!(!inside_dotgit(Path::new("/r/.github/x")));
    }

    #[test]
    fn submodule_git_dirs_are_under_dotgit_modules() {
        assert!(in_modules(Path::new("/r/.git/modules/sub")));
        assert!(in_modules(Path::new("/r/.git/modules/a/modules/b")));
        assert!(!in_modules(Path::new("/r/.git/modules")));
        assert!(!in_modules(Path::new("/r/modules/sub")));
        assert!(!in_modules(Path::new("/elsewhere/sub.git")));
    }
}
