//! The Git repositories and linked worktrees the scan found: the list the
//! Repositories tab shows, what the details say about a node's place in
//! Git, and the badge a root folder's rectangle carries.
//!
//! Nothing here touches the disk or runs Git: everything comes from the
//! entries the scanner recorded in the tree. Paths in those entries (a
//! worktree's common directory, a submodule's Git directory) are matched to
//! nodes by name, from the scan root down, only for the rows asked for.

use super::filter::current;
use super::{MAX_PAGE, Session, display_path, metric, node_name};
use sb_core::git::{self, GitClass, GitRoot, Head, RootKind, RootSummary, paths};
use sb_core::tree::{NodeId, Tree, flags};
use sb_protocol as proto;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

impl Session {
    /// A page of the listed Git roots, largest first by the request's
    /// metric, with totals that count each byte once; with `search`, also
    /// what that filter matches in each. Summing up is linear in the
    /// entries times their depth, under the search's and the tree's read
    /// locks; the rows' links are resolved one row at a time, stopping
    /// between rows if this request is superseded.
    pub fn git_roots(&self, req: &proto::GitRootsRequest) -> Result<proto::GitRootsPage, String> {
        let tree_lock = self.scan.tree();
        let (search, tree, progress) = self.read_locked(&tree_lock, req.search.is_some())?;
        crate::runner::check()?;
        let filter = match req.search {
            Some(id) => Some(&current(&search, id)?.search),
            None => None,
        };
        let (summaries, totals) = git::summarize(&tree, filter, metric(req.metric));
        crate::runner::check()?;
        let nested = nested_counts(&tree, &summaries);
        let total = summaries.len();
        let start = (req.offset as usize).min(total);
        let end = start
            .saturating_add(req.limit.min(MAX_PAGE) as usize)
            .min(total);
        let mut rows = Vec::with_capacity(end - start);
        for (i, s) in summaries[start..end].iter().enumerate() {
            crate::runner::check()?;
            let mut row = root_row(&tree, s.node, s.outside);
            row.size = s.size;
            row.own = s.own;
            row.metadata = s.metadata;
            row.matching = s.matching;
            row.files = u64::from(s.files);
            row.parent = s.parent.map(|p| p.index() as u32);
            row.parent_outside = s.parent_outside;
            row.nested = nested[start + i];
            rows.push(row);
        }
        Ok(proto::GitRootsPage {
            generation: self.generation(),
            revision: progress.revision,
            offset: start as u32,
            total: total as u32,
            rows,
            totals: proto::GitTotalsWire {
                roots: totals.roots,
                worktrees: totals.worktrees,
                unresolved: totals.unresolved,
                checkout: totals.checkout,
                worktree: totals.worktree,
                metadata: totals.metadata,
                union: totals.union,
                matching: totals.matching,
            },
            discovering: !progress.state.is_finished(),
        })
    }
}

/// For each row, how many other rows are inside it, found through the
/// folders above each row, unresolved ones included; the row for the
/// repository above the scan root has every other row inside it.
fn nested_counts(tree: &Tree, rows: &[RootSummary]) -> Vec<u32> {
    // Rows are (node, outside): the scan root can have a row of its own
    // besides the one for the repository above it.
    let at: HashMap<(NodeId, bool), usize> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| ((r.node, r.outside), i))
        .collect();
    let outside = at.get(&(NodeId::ROOT, true)).copied();
    let mut nested = vec![0u32; rows.len()];
    for r in rows.iter().filter(|r| !r.outside) {
        let mut cur = tree.parent(r.node);
        while let Some(a) = cur {
            if let Some(&j) = at.get(&(a, false)) {
                nested[j] += 1;
            }
            cur = tree.parent(a);
        }
        if let Some(j) = outside {
            nested[j] += 1;
        }
    }
    nested
}

/// The entry a row stands for.
fn entry_of(tree: &Tree, node: NodeId, outside: bool) -> Option<(&GitRoot, Option<&Path>)> {
    if outside {
        tree.git().above().map(|(path, r)| (r, Some(path)))
    } else {
        tree.git_entry(node).map(|r| (r, None))
    }
}

/// A row for the listed root at `node` (or above the scan root), with
/// everything but the sizes, the parent, and the count of nested rows.
pub(super) fn root_row(tree: &Tree, node: NodeId, outside: bool) -> proto::GitRootRow {
    let entry = entry_of(tree, node, outside);
    let (name, path) = match entry {
        Some((_, Some(above))) => (
            above
                .file_name()
                .map_or_else(|| display_path(above), |n| n.to_string_lossy().into_owned()),
            display_path(above),
        ),
        _ => (node_name(tree, node), display_path(&tree.path(node))),
    };
    let root = entry.map(|(r, _)| r);
    let shown = |p: &Option<PathBuf>| p.as_deref().map(display_path);
    proto::GitRootRow {
        node: node.index() as u32,
        name,
        path,
        kind: root.map_or(proto::GitRootKind::Unresolved, |r| root_kind(r.kind)),
        head: root.and_then(|r| r.head.as_ref()).map(head),
        parent: None,
        parent_outside: false,
        repository: root.and_then(|r| repository(tree, r)),
        size: 0,
        own: 0,
        metadata: 0,
        matching: None,
        files: 0,
        nested: 0,
        problem: root
            .and_then(|r| r.problem.as_ref())
            .map(ToString::to_string),
        recorded_at: root.and_then(|r| shown(&r.recorded_at)),
        worktree: root
            .filter(|r| r.kind != RootKind::LinkedWorktree)
            .and_then(|r| shown(&r.worktree)),
        outside,
        // Above the scan root, most of the repository isn't scanned.
        partial: outside || !tree.fully_listed(node),
    }
}

fn root_kind(kind: RootKind) -> proto::GitRootKind {
    match kind {
        RootKind::Repository => proto::GitRootKind::Repository,
        RootKind::LinkedWorktree => proto::GitRootKind::LinkedWorktree,
        RootKind::Submodule => proto::GitRootKind::Submodule,
        RootKind::SeparateGitDir => proto::GitRootKind::SeparateGitDir,
        RootKind::Bare => proto::GitRootKind::Bare,
        RootKind::GitDir => proto::GitRootKind::GitDir,
        // Not listed; a row never stands for one.
        RootKind::Unresolved | RootKind::Pending | RootKind::DotGit | RootKind::WorktreeAdmin => {
            proto::GitRootKind::Unresolved
        }
    }
}

fn head(h: &Head) -> proto::GitHead {
    match h {
        Head::Branch(name) => proto::GitHead::Branch { name: name.clone() },
        Head::Ref(name) => proto::GitHead::Ref { name: name.clone() },
        Head::Detached(commit) => proto::GitHead::Detached {
            commit: commit.clone(),
        },
    }
}

/// The repository a root belongs to: a linked worktree's main repository
/// (the folder holding its common `.git`, or a bare common directory
/// itself), a submodule's superproject (the folder holding the `.git`
/// whose `modules` hold its Git directory), a separate Git directory.
fn repository(tree: &Tree, root: &GitRoot) -> Option<proto::GitLink> {
    let style = paths::Style::native();
    let target = match root.kind {
        RootKind::LinkedWorktree => {
            let common = root.common_dir.as_deref()?;
            if is_named(common, ".git", style) {
                common.parent()?.to_path_buf()
            } else {
                common.to_path_buf()
            }
        }
        RootKind::Submodule => root
            .git_dir
            .as_deref()?
            .ancestors()
            .find(|a| {
                is_named(a, "modules", style)
                    && a.parent().is_some_and(|g| is_named(g, ".git", style))
            })?
            .parent()?
            .parent()?
            .to_path_buf(),
        RootKind::SeparateGitDir => root.git_dir.clone()?,
        _ => return None,
    };
    Some(proto::GitLink {
        path: display_path(&target),
        node: node_at(tree, &target, style).map(|n| n.index() as u32),
    })
}

fn is_named(path: &Path, name: &str, style: paths::Style) -> bool {
    path.file_name()
        .is_some_and(|n| same_name(n, OsStr::new(name), style))
}

fn same_name(a: &OsStr, b: &OsStr, style: paths::Style) -> bool {
    if style.fold_case {
        a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
    } else {
        a == b
    }
}

/// The node at `path`, found by name from the scan root down; `None` when
/// it's outside the scan or wasn't found (or removed).
fn node_at(tree: &Tree, path: &Path, style: paths::Style) -> Option<NodeId> {
    let parts = paths::relative_to(tree.root_path(), path, style)?;
    let mut cur = NodeId::ROOT;
    for part in &parts {
        cur = tree
            .children(cur)
            .find(|&c| !tree.is_removed(c) && same_name(tree.name(c), part, style))?;
    }
    Some(cur)
}

/// What the details panel says about `n`'s place in Git. O(depth), plus
/// one pass over the entries for a repository's linked worktrees. The
/// root's `size` and `matching` are logical sizes, as the details' own
/// are, `matching` from `filter` when given; `own` and `metadata` aren't
/// summed up here (they're 0; the list has them).
pub(super) fn details(
    tree: &Tree,
    n: NodeId,
    filter: Option<&sb_core::search::Search>,
) -> Option<proto::GitDetails> {
    let git = tree.git();
    if git.is_empty() {
        return None;
    }
    let content = match tree.git_class(n) {
        GitClass::Other => None,
        GitClass::Checkout => Some(proto::GitContent::Checkout),
        GitClass::Worktree => Some(proto::GitContent::Worktree),
        GitClass::Metadata => Some(proto::GitContent::Metadata),
    };
    let found = match tree.git_root_of(n) {
        Some(r) => Some((r, false)),
        None => git
            .above()
            .filter(|(_, r)| r.kind.listed())
            .map(|_| (NodeId::ROOT, true)),
    };
    let root = found.map(|(r, outside)| {
        let mut row = root_row(tree, r, outside);
        row.files = u64::from(tree.file_count(r));
        row.size = tree.logical(r);
        row.matching = filter.map(|s| s.totals(r).logical);
        if let Some((parent, parent_outside)) = (!outside).then(|| enclosing(tree, r)).flatten() {
            row.parent = Some(parent.index() as u32);
            row.parent_outside = parent_outside;
        }
        row
    });
    if content.is_none() && root.is_none() {
        return None;
    }
    let worktrees_registered = found
        .filter(|&(_, outside)| !outside)
        .and_then(|(r, _)| registered_worktrees(tree, r));
    Some(proto::GitDetails {
        content,
        root,
        worktrees_registered,
    })
}

/// The nearest listed root with a class around `node`, and whether it's
/// the repository above the scan root, as `RootSummary::parent` and
/// `parent_outside` give them.
fn enclosing(tree: &Tree, node: NodeId) -> Option<(NodeId, bool)> {
    let mut cur = tree.parent(node);
    while let Some(a) = cur {
        if tree
            .git_entry(a)
            .is_some_and(|r| r.kind.listed() && r.kind.class().is_some())
        {
            return Some((a, false));
        }
        cur = tree.parent(a);
    }
    tree.git()
        .above()
        .filter(|(_, r)| r.kind.listed() && r.kind.class().is_some())
        .map(|_| (NodeId::ROOT, true))
}

/// For a repository whose Git directory is in the scan: the linked
/// worktrees registered there (its `worktrees/<id>` folders), and how many
/// of their checkouts are in the scan too. `None` when none are known.
fn registered_worktrees(tree: &Tree, repo: NodeId) -> Option<(u32, u32)> {
    let style = paths::Style::native();
    let (mut registered, mut here) = (0u32, 0u32);
    for admin in tree.git().roots() {
        if admin.kind != RootKind::WorktreeAdmin || tree.is_removed(admin.node) {
            continue;
        }
        let Some(list) = tree.parent(admin.node) else {
            continue;
        };
        let Some(dir) = tree.parent(list) else {
            continue;
        };
        if !same_name(tree.name(list), OsStr::new("worktrees"), style) {
            continue;
        }
        // `<repo>/.git/worktrees/<id>`, or `<bare>/worktrees/<id>`.
        let owner = match tree.git_entry(dir).map(|r| r.kind) {
            Some(RootKind::DotGit) => tree.parent(dir),
            _ => Some(dir),
        };
        if owner != Some(repo) {
            continue;
        }
        registered += 1;
        let checkout = admin.worktree.as_deref().and_then(|p| {
            // The checkout's folder; the admin folder may name its `.git`.
            let folder = if is_named(p, ".git", style) {
                p.parent()?
            } else {
                p
            };
            node_at(tree, folder, style)
        });
        if checkout.is_some_and(|c| {
            tree.git_entry(c)
                .is_some_and(|r| r.kind == RootKind::LinkedWorktree)
        }) {
            here += 1;
        }
    }
    (registered > 0).then_some((registered, here))
}

/// Byte 35 of a folder's rectangle: the kind of Git root it is, if any.
pub(super) fn badge(tree: &Tree, node: NodeId) -> u8 {
    use proto::wire::git_badge;
    if tree.flags(node) & flags::GIT == 0 {
        return git_badge::NONE;
    }
    match tree.git_entry(node).map(|r| r.kind) {
        Some(RootKind::Repository) => git_badge::REPOSITORY,
        Some(RootKind::LinkedWorktree) => git_badge::LINKED_WORKTREE,
        Some(RootKind::Submodule) => git_badge::SUBMODULE,
        Some(RootKind::SeparateGitDir) => git_badge::SEPARATE_GIT_DIR,
        Some(RootKind::Bare) => git_badge::BARE,
        Some(RootKind::GitDir) => git_badge::GIT_DIR,
        Some(RootKind::Unresolved) => git_badge::UNRESOLVED,
        _ => git_badge::NONE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sb_core::git::Problem;
    use sb_scan::ScanConfig;
    use sb_scan::fake::FakeFs;
    use std::time::{Duration, Instant};

    /// A finished scan of:
    ///
    /// ```text
    /// main/        repository on main: src/a.rs 1000, .git/ (HEAD, objects/p 500,
    ///              worktrees/wt1/HEAD), vendor/sub/ submodule (.git file, x.rs 200)
    /// wt1/         linked worktree of main: .git file, b.rs 300
    /// broken/      unresolved, holding inner/, a repository (.git/HEAD, i.rs 20)
    /// other/c.txt  50
    /// ```
    ///
    /// with the entries detection would record, added as a new revision.
    fn scanned() -> Session {
        let fs = FakeFs::new();
        fs.file("main/src/a.rs", 1000, 1000)
            .file("main/.git/HEAD", 10, 10)
            .file("main/.git/objects/p", 500, 500)
            .file("main/.git/worktrees/wt1/HEAD", 5, 5)
            .file("main/vendor/sub/.git", 3, 3)
            .file("main/vendor/sub/x.rs", 200, 200)
            .file("wt1/.git", 4, 4)
            .file("wt1/b.rs", 300, 300)
            .file("broken/inner/.git/HEAD", 6, 6)
            .file("broken/inner/i.rs", 20, 20)
            .file("other/c.txt", 50, 50);
        let session = Session::start_on(fs.root(), fs, ScanConfig::default(), None, None, |_| {});
        let deadline = Instant::now() + Duration::from_secs(30);
        while !session.scan.progress().state.is_finished() {
            assert!(Instant::now() < deadline, "scan never finished");
            std::thread::sleep(Duration::from_millis(2));
        }
        session
            .scan
            .edit_finished(|t| {
                let native = |t: &Tree, n| paths::normalize(&t.path(n), paths::Style::native());
                let main = at(t, "main");
                let dot = at(t, "main/.git");
                let wt = at(t, "wt1");
                let entries = [
                    GitRoot {
                        head: Some(Head::Branch("main".into())),
                        ..GitRoot::new(main, RootKind::Repository)
                    },
                    GitRoot::new(dot, RootKind::DotGit),
                    GitRoot {
                        worktree: Some(native(t, wt)),
                        ..GitRoot::new(at(t, "main/.git/worktrees/wt1"), RootKind::WorktreeAdmin)
                    },
                    GitRoot {
                        git_dir: Some(native(t, dot).join("modules").join("sub")),
                        ..GitRoot::new(at(t, "main/vendor/sub"), RootKind::Submodule)
                    },
                    GitRoot::new(at(t, "main/vendor/sub/.git"), RootKind::DotGit),
                    GitRoot {
                        head: Some(Head::Detached("0123456789ab".into())),
                        common_dir: Some(native(t, dot)),
                        recorded_at: Some(PathBuf::from("/elsewhere/wt1")),
                        ..GitRoot::new(wt, RootKind::LinkedWorktree)
                    },
                    GitRoot::new(at(t, "wt1/.git"), RootKind::DotGit),
                    GitRoot::unresolved(at(t, "broken"), Problem::NotGit),
                    GitRoot::new(at(t, "broken/inner"), RootKind::Repository),
                    GitRoot::new(at(t, "broken/inner/.git"), RootKind::DotGit),
                ];
                for e in entries {
                    t.set_git(e);
                }
            })
            .unwrap();
        session
    }

    /// The node at `path` under the scan root.
    fn at(tree: &Tree, path: &str) -> NodeId {
        path.split('/').fold(NodeId::ROOT, |n, name| {
            tree.children(n)
                .find(|&c| tree.name(c) == name)
                .unwrap_or_else(|| panic!("no {path}"))
        })
    }

    fn node_of(session: &Session, path: &str) -> u32 {
        if path.is_empty() {
            return 0;
        }
        let tree = session.scan.tree();
        let tree = tree.read().unwrap();
        at(&tree, path).index() as u32
    }

    fn request(session: &Session, offset: u32, limit: u32) -> proto::GitRootsRequest {
        proto::GitRootsRequest {
            generation: session.generation(),
            metric: proto::Metric::Logical,
            search: None,
            offset,
            limit,
        }
    }

    fn row(page: &proto::GitRootsPage, node: u32) -> &proto::GitRootRow {
        page.rows
            .iter()
            .find(|r| r.node == node && !r.outside)
            .unwrap_or_else(|| panic!("no row for {node}"))
    }

    #[test]
    fn lists_roots_largest_first_a_page_at_a_time() {
        let session = scanned();
        let all = session.git_roots(&request(&session, 0, 100)).unwrap();
        assert_eq!(all.total, 5, "main, sub, wt1, broken, inner");
        assert_eq!(
            all.total,
            session.status().git_roots,
            "as the status counts"
        );
        assert!(!all.discovering);
        assert_eq!(all.revision, session.scan.progress().revision);
        let names: Vec<&str> = all.rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["main", "wt1", "sub", "broken", "inner"]);

        let first = session.git_roots(&request(&session, 0, 2)).unwrap();
        let rest = session.git_roots(&request(&session, 2, 2)).unwrap();
        let last = session.git_roots(&request(&session, 4, 2)).unwrap();
        assert_eq!((first.offset, rest.offset, last.offset), (0, 2, 4));
        assert_eq!([first.rows, rest.rows, last.rows].concat(), all.rows);
        let past = session.git_roots(&request(&session, 9, 2)).unwrap();
        assert_eq!((past.offset, past.rows.len(), past.total), (5, 0, 5));

        let main = row(&all, node_of(&session, "main"));
        assert_eq!(main.kind, proto::GitRootKind::Repository);
        assert_eq!(
            main.head,
            Some(proto::GitHead::Branch {
                name: "main".into()
            })
        );
        assert_eq!(main.size, 1000 + 10 + 500 + 5 + 3 + 200);
        assert_eq!(main.own, 1000, "less its .git and the submodule");
        assert_eq!(
            main.metadata, 515,
            "its .git, worktree admin folder included"
        );
        assert_eq!((main.nested, main.parent), (1, None));
        assert_eq!(main.files, 6);
        assert!(!main.partial && !main.outside);
        assert_eq!(main.matching, None);

        let sub = row(&all, node_of(&session, "main/vendor/sub"));
        assert_eq!(sub.kind, proto::GitRootKind::Submodule);
        assert_eq!(sub.parent, Some(main.node));
        let superproject = sub.repository.as_ref().unwrap();
        assert_eq!(superproject.node, Some(main.node), "{superproject:?}");

        let wt = row(&all, node_of(&session, "wt1"));
        assert_eq!(wt.kind, proto::GitRootKind::LinkedWorktree);
        assert_eq!(
            wt.head,
            Some(proto::GitHead::Detached {
                commit: "0123456789ab".into()
            })
        );
        let repo = wt.repository.as_ref().unwrap();
        assert_eq!(repo.node, Some(main.node));
        assert_eq!(
            repo.path,
            display_path(&paths::normalize(
                Path::new(&main.path),
                paths::Style::native()
            )),
            "the folder holding the common .git"
        );
        assert_eq!(
            wt.recorded_at,
            Some(display_path(Path::new("/elsewhere/wt1")))
        );
        assert_eq!(
            (wt.own, wt.metadata),
            (300, 4),
            "shared data counts in main"
        );

        let broken = row(&all, node_of(&session, "broken"));
        assert_eq!(broken.kind, proto::GitRootKind::Unresolved);
        assert_eq!(
            broken.problem.as_deref(),
            Some("folder named .git without Git data")
        );
        assert_eq!(broken.nested, 1, "the repository inside it");
        let inner = row(&all, node_of(&session, "broken/inner"));
        assert_eq!(inner.parent, None, "unresolved rows aren't parents");

        assert_eq!(
            (
                all.totals.roots,
                all.totals.worktrees,
                all.totals.unresolved
            ),
            (3, 1, 1)
        );
        assert_eq!(
            all.totals.union,
            1718 + 304 + 26,
            "main, wt1 and inner, each byte once; broken's own content isn't in Git"
        );
        assert_eq!(all.totals.matching, None);
    }

    #[test]
    fn rows_carry_matching_sizes_for_a_search() {
        let session = scanned();
        let id = session.search_set("*.rs").unwrap().unwrap().search;
        let mut req = request(&session, 0, 100);
        req.search = Some(id);
        let page = session.git_roots(&req).unwrap();
        let main = row(&page, node_of(&session, "main"));
        assert_eq!(main.matching, Some(1200), "a.rs and the submodule's x.rs");
        assert_eq!(row(&page, node_of(&session, "wt1")).matching, Some(300));
        assert_eq!(page.totals.matching, Some(1200 + 300 + 20));
        // Details give the root the same matching total the root's own
        // details give.
        let wt = node_of(&session, "wt1");
        let own = session.details(wt, Some(id)).unwrap();
        let root = own.git.unwrap().root.unwrap();
        assert_eq!(root.matching, Some(own.matching.unwrap().logical));
        assert_eq!(root.matching, Some(300));
        assert_eq!(root.size, 304);
        let file = node_of(&session, "wt1/b.rs");
        let git = session.details(file, Some(id)).unwrap().git.unwrap();
        assert_eq!(git.root.unwrap().matching, Some(300));
        let untold = session.details(file, None).unwrap().git.unwrap();
        assert_eq!(untold.root.unwrap().matching, None);
        // A replaced search gives an error, as the other lists do.
        session.search_set("*.txt").unwrap();
        assert!(session.git_roots(&req).is_err());
    }

    #[test]
    fn the_repository_above_the_scan_root_is_a_row_of_its_own() {
        let session = scanned();
        session
            .scan
            .edit_finished(|t| {
                t.set_git_above(
                    PathBuf::from("/"),
                    GitRoot {
                        head: Some(Head::Branch("trunk".into())),
                        ..GitRoot::new(NodeId::ROOT, RootKind::Repository)
                    },
                )
            })
            .unwrap();
        let page = session.git_roots(&request(&session, 0, 100)).unwrap();
        assert_eq!(page.total, 6);
        assert_eq!(page.total, session.status().git_roots);
        let outside = &page.rows[0];
        assert!(outside.outside, "it holds the whole scan, so it's first");
        assert_eq!(outside.node, 0);
        assert_eq!(outside.kind, proto::GitRootKind::Repository);
        assert_eq!(
            outside.head,
            Some(proto::GitHead::Branch {
                name: "trunk".into()
            })
        );
        assert_eq!(outside.path, display_path(Path::new("/")));
        assert!(outside.partial, "most of it isn't scanned");
        assert_eq!(outside.nested, 5, "every other row");
        let main = row(&page, node_of(&session, "main"));
        assert_eq!((main.parent, main.parent_outside), (Some(0), true));
        let sub = row(&page, node_of(&session, "main/vendor/sub"));
        assert_eq!((sub.parent, sub.parent_outside), (Some(main.node), false));
        let details = session.details(main.node, None).unwrap().git.unwrap();
        let root = details.root.unwrap();
        assert_eq!((root.parent, root.parent_outside), (Some(0), true));
    }

    #[test]
    fn a_superseded_page_stops() {
        let session = scanned();
        let req = request(&session, 0, 100);
        let stopped = crate::runner::as_superseded(|| session.git_roots(&req));
        assert_eq!(stopped.err(), Some(proto::SUPERSEDED.to_owned()));
        assert!(session.git_roots(&req).is_ok());
    }

    #[test]
    fn details_say_where_a_node_is_in_git() {
        let session = scanned();
        let details = |path: &str| session.details(node_of(&session, path), None).unwrap().git;
        let main = node_of(&session, "main");

        let wt = details("wt1/b.rs").unwrap();
        assert_eq!(wt.content, Some(proto::GitContent::Worktree));
        let root = wt.root.unwrap();
        assert_eq!(root.node, node_of(&session, "wt1"));
        assert_eq!(root.kind, proto::GitRootKind::LinkedWorktree);
        assert_eq!(root.repository.unwrap().node, Some(main));
        assert_eq!(root.files, 2);
        assert_eq!(wt.worktrees_registered, None);

        let meta = details("main/.git/objects/p").unwrap();
        assert_eq!(meta.content, Some(proto::GitContent::Metadata));
        assert_eq!(meta.root.as_ref().unwrap().node, main);
        assert_eq!(
            meta.worktrees_registered,
            Some((1, 1)),
            "wt1 is registered in main's .git and in the scan"
        );
        let pointer = details("wt1/.git").unwrap();
        assert_eq!(pointer.content, Some(proto::GitContent::Metadata));

        let nested = details("main/vendor/sub/x.rs").unwrap();
        assert_eq!(nested.content, Some(proto::GitContent::Checkout));
        assert_eq!(nested.root.unwrap().parent, Some(main));

        let unresolved = details("broken").unwrap();
        assert_eq!(unresolved.content, None);
        assert_eq!(
            unresolved.root.unwrap().kind,
            proto::GitRootKind::Unresolved
        );
        assert_eq!(details("other/c.txt"), None, "not in any repository");
        assert_eq!(details(""), None, "the scan root isn't in one");

        session
            .scan
            .edit_finished(|t| {
                t.set_git_above(
                    PathBuf::from("/"),
                    GitRoot::new(NodeId::ROOT, RootKind::Repository),
                )
            })
            .unwrap();
        let root = details("").unwrap();
        assert_eq!(root.content, Some(proto::GitContent::Checkout));
        let above = root.root.unwrap();
        assert!(above.outside && above.node == 0);
        assert!(details("other/c.txt").unwrap().root.unwrap().outside);
    }

    #[test]
    fn root_folders_carry_a_badge_and_rects_their_class() {
        let session = scanned();
        let bytes = session
            .layout(&proto::LayoutRequest {
                generation: session.generation(),
                request: 1,
                view: 0,
                metric: proto::Metric::Logical,
                width: 1600.0,
                height: 1200.0,
                search: None,
            })
            .unwrap();
        let count = u32::from_le_bytes(bytes[40..44].try_into().unwrap()) as usize;
        let rects = &bytes[proto::wire::HEADER_LEN..][..count * proto::wire::RECT_LEN];
        // Folders' (class, badge).
        let folders: HashMap<u32, (u8, u8)> = rects
            .chunks_exact(proto::wire::RECT_LEN)
            .filter(|r| r[33] == 1)
            .map(|r| {
                let node = u32::from_le_bytes(r[16..20].try_into().unwrap());
                (node, (r[34] >> 6, r[35]))
            })
            .collect();
        use proto::wire::git_badge;
        let of = |path: &str| folders.get(&node_of(&session, path)).copied();
        assert_eq!(of("main"), Some((1, git_badge::REPOSITORY)));
        assert_eq!(
            of("main/.git"),
            Some((3, git_badge::NONE)),
            "metadata, no badge"
        );
        assert_eq!(of("main/src"), Some((1, git_badge::NONE)));
        assert_eq!(of("main/vendor/sub"), Some((1, git_badge::SUBMODULE)));
        assert_eq!(of("wt1"), Some((2, git_badge::LINKED_WORKTREE)));
        assert_eq!(of("broken"), Some((0, git_badge::UNRESOLVED)));
        assert_eq!(of("broken/inner"), Some((1, git_badge::REPOSITORY)));
        assert_eq!(of("other"), Some((0, git_badge::NONE)));
    }
}
