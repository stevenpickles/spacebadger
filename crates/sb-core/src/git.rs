//! Git repositories and linked worktrees found in a scan.
//!
//! The scanner reads a few small metadata files per candidate (never runs
//! `git`) and records what it finds as [`GitRoot`] entries keyed by node in
//! the tree's [`GitIndex`]. A node's [`GitClass`] is never stored per node:
//! it's the class of the nearest entry at or above it that has one, so a
//! late discovery reclassifies a whole subtree by adding one entry.
//!
//! Also here, pure and free of I/O so they can be tested on any platform:
//! parsing of `HEAD`, `.git` pointer files, `commondir`/`gitdir` and the
//! `[core]` section of `config` ([`parse_head`], [`parse_pointer`],
//! [`parse_path_file`], [`parse_config_core`]), lexical path handling
//! ([`paths`]), and size accounting over the entries ([`summarize`]).

use crate::layout::Metric;
use crate::search::Search;
use crate::tree::{NodeId, Tree};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// What a node's content is, as far as Git goes. Fits two bits (see
/// `sb_core::layout::rect_flags::GIT_MASK`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum GitClass {
    /// Not in any detected repository.
    #[default]
    Other = 0,
    /// The checkout of an ordinary repository, a submodule, or a repository
    /// with a separate Git directory.
    Checkout = 1,
    /// The checkout of a linked worktree.
    Worktree = 2,
    /// Git's own data: a `.git` folder or file, a bare repository, a Git
    /// directory kept apart from its checkout.
    Metadata = 3,
}

impl GitClass {
    /// The class with this value; out-of-range values read as `Other`.
    pub fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Checkout,
            2 => Self::Worktree,
            3 => Self::Metadata,
            _ => Self::Other,
        }
    }
}

/// What an entry stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RootKind {
    /// A `.git` folder was seen but hasn't been checked yet.
    Pending,
    /// A folder with a `.git` folder: an ordinary repository's checkout.
    Repository,
    /// A folder whose `.git` file points to a linked worktree's Git
    /// directory (one with a `commondir`).
    LinkedWorktree,
    /// A folder whose `.git` file points into a superproject's
    /// `.git/modules`.
    Submodule,
    /// A folder whose `.git` file points to a Git directory elsewhere.
    SeparateGitDir,
    /// A bare repository (`core.bare = true`).
    Bare,
    /// A Git directory that isn't named `.git` and isn't bare: a separate
    /// or submodule Git directory inside the scan.
    GitDir,
    /// A `.git` folder or file whose repository resolved.
    DotGit,
    /// A linked worktree's administrative folder (`<common>/worktrees/<id>`).
    /// Information only: it inherits its class and isn't listed.
    WorktreeAdmin,
    /// Looked like a repository but couldn't be confirmed; see
    /// [`GitRoot::problem`].
    Unresolved,
}

impl RootKind {
    /// The class this entry gives its subtree; `None` when the subtree
    /// inherits the class from above.
    pub fn class(self) -> Option<GitClass> {
        match self {
            Self::Repository | Self::Submodule | Self::SeparateGitDir => Some(GitClass::Checkout),
            Self::LinkedWorktree => Some(GitClass::Worktree),
            Self::Bare | Self::GitDir | Self::DotGit => Some(GitClass::Metadata),
            Self::Pending | Self::WorktreeAdmin | Self::Unresolved => None,
        }
    }

    /// Whether the entry is shown as a row in the list of repositories.
    pub fn listed(self) -> bool {
        matches!(
            self,
            Self::Repository
                | Self::LinkedWorktree
                | Self::Submodule
                | Self::SeparateGitDir
                | Self::Bare
                | Self::GitDir
                | Self::Unresolved
        )
    }
}

/// What `HEAD` names, as written there.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Head {
    /// A branch, without `refs/heads/`. May not exist yet (an unborn
    /// branch).
    Branch(String),
    /// Some other ref, in full.
    Ref(String),
    /// A commit: the first 12 hex digits of its ID.
    Detached(String),
}

/// Why an entry couldn't be confirmed.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Problem {
    /// Not checked because the scan stopped first.
    NotChecked,
    /// A metadata file couldn't be read. `kind` is the scanner's error
    /// kind, as a number.
    Unreadable { kind: u8, message: String },
    /// A metadata file was larger than any real one.
    TooLarge,
    /// A metadata file didn't have the expected shape.
    Malformed(&'static str),
    /// The Git directory a `.git` file points to isn't there.
    TargetMissing(PathBuf),
    /// A cloud placeholder, which would have to be downloaded to be read.
    Cloud,
    /// A link, which is never followed.
    Link,
    /// A folder named `.git` without Git's data in it.
    NotGit,
    /// The repository's `.git` was deleted from the app.
    MetadataRemoved,
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotChecked => f.write_str("not checked: scan stopped"),
            Self::Unreadable { message, .. } => {
                write!(f, "Git metadata couldn't be read: {message}")
            }
            Self::TooLarge => f.write_str("Git metadata file is too large"),
            Self::Malformed(what) => write!(f, "Git metadata isn't valid: {what}"),
            Self::TargetMissing(path) => {
                write!(f, "Git directory not found: {}", path.display())
            }
            Self::Cloud => f.write_str("Git metadata is a cloud placeholder; not downloaded"),
            Self::Link => f.write_str(".git is a link; not followed"),
            Self::NotGit => f.write_str("folder named .git without Git data"),
            Self::MetadataRemoved => f.write_str("Git metadata was deleted"),
        }
    }
}

/// One detected repository, worktree, or piece of Git metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitRoot {
    pub node: NodeId,
    pub kind: RootKind,
    pub head: Option<Head>,
    /// The Git directory: where a `.git` file points, or the folder itself
    /// for a bare repository or Git directory.
    pub git_dir: Option<PathBuf>,
    /// A linked worktree's common Git directory (the main repository's).
    pub common_dir: Option<PathBuf>,
    /// `core.worktree`, or a worktree admin folder's checkout.
    pub worktree: Option<PathBuf>,
    /// Where a linked worktree's Git directory says the worktree is, when
    /// that isn't where it was found.
    pub recorded_at: Option<PathBuf>,
    pub problem: Option<Problem>,
}

impl GitRoot {
    /// An entry with nothing known beyond its kind.
    pub fn new(node: NodeId, kind: RootKind) -> Self {
        Self {
            node,
            kind,
            head: None,
            git_dir: None,
            common_dir: None,
            worktree: None,
            recorded_at: None,
            problem: None,
        }
    }

    /// A candidate seen but not yet checked.
    pub fn pending(node: NodeId) -> Self {
        Self::new(node, RootKind::Pending)
    }

    /// A candidate that couldn't be confirmed.
    pub fn unresolved(node: NodeId, problem: Problem) -> Self {
        Self {
            problem: Some(problem),
            ..Self::new(node, RootKind::Unresolved)
        }
    }
}

/// The Git entries of a tree, keyed by node, and the repository the scan
/// root is inside of, if any. Kept by [`Tree`]; written through
/// [`Tree::set_git`] and [`Tree::set_git_above`].
#[derive(Debug, Default)]
pub struct GitIndex {
    roots: Vec<GitRoot>,
    /// Node index → index into `roots`.
    by_node: HashMap<u32, u32>,
    /// The repository found above the scan root, and its folder.
    above: Option<(PathBuf, GitRoot)>,
    /// Nodes whose entry changed class or kind, oldest first; the root (0)
    /// also when `above` changes.
    changes: Vec<u32>,
    /// Entries whose kind is [`RootKind::listed`], `above` included.
    listed: u32,
}

impl GitIndex {
    pub fn get(&self, node: NodeId) -> Option<&GitRoot> {
        let &i = self.by_node.get(&(node.index() as u32))?;
        Some(&self.roots[i as usize])
    }

    /// Every entry, in no particular order. Entries of removed nodes are
    /// dropped when they're removed.
    pub fn roots(&self) -> impl Iterator<Item = &GitRoot> {
        self.roots.iter()
    }

    /// The repository the scan root is inside of, and its folder (outside
    /// the scan).
    pub fn above(&self) -> Option<(&Path, &GitRoot)> {
        self.above.as_ref().map(|(p, r)| (p.as_path(), r))
    }

    /// Nodes whose entry changed class or kind, oldest first; may repeat,
    /// and is only ever appended to. The root (0) is also added when
    /// [`GitIndex::above`] changes.
    pub fn changes(&self) -> &[u32] {
        &self.changes
    }

    /// Entries shown in the list of repositories, including the one above
    /// the scan root.
    pub fn listed_count(&self) -> u32 {
        self.listed
    }

    pub fn is_empty(&self) -> bool {
        self.roots.is_empty() && self.above.is_none()
    }

    /// Inserts or replaces `root`'s entry. Returns whether its class or
    /// kind changed (a new entry always counts as changed).
    pub(crate) fn set(&mut self, root: GitRoot) -> bool {
        let raw = root.node.index() as u32;
        let new_kind = root.kind;
        let old_kind = match self.by_node.get(&raw) {
            Some(&i) => {
                let slot = &mut self.roots[i as usize];
                let old = slot.kind;
                *slot = root;
                Some(old)
            }
            None => {
                self.by_node.insert(raw, self.roots.len() as u32);
                self.roots.push(root);
                None
            }
        };
        self.relist(old_kind, Some(new_kind));
        let changed = old_kind != Some(new_kind);
        if changed {
            self.changes.push(raw);
        }
        changed
    }

    /// Sets the repository above the scan root. Returns whether it changed
    /// in a way that can change classes (folder or kind).
    pub(crate) fn set_above(&mut self, path: PathBuf, root: GitRoot) -> bool {
        let old = self.above.as_ref().map(|(p, r)| (p.clone(), r.kind));
        self.relist(old.as_ref().map(|o| o.1), Some(root.kind));
        let changed = old.as_ref() != Some(&(path.clone(), root.kind));
        self.above = Some((path, root));
        if changed {
            self.changes.push(0);
        }
        changed
    }

    /// Drops the entry of a node removed from the tree.
    pub(crate) fn forget(&mut self, node: NodeId) {
        let Some(i) = self.by_node.remove(&(node.index() as u32)) else {
            return;
        };
        let removed = self.roots.swap_remove(i as usize);
        if let Some(moved) = self.roots.get(i as usize) {
            self.by_node.insert(moved.node.index() as u32, i);
        }
        self.relist(Some(removed.kind), None);
    }

    fn relist(&mut self, old: Option<RootKind>, new: Option<RootKind>) {
        if old.is_some_and(RootKind::listed) {
            self.listed -= 1;
        }
        if new.is_some_and(RootKind::listed) {
            self.listed += 1;
        }
    }

    /// Approximate heap bytes held.
    pub(crate) fn memory_bytes(&self) -> usize {
        let paths: usize = self
            .roots
            .iter()
            .chain(self.above.iter().map(|(_, r)| r))
            .map(|r| {
                [&r.git_dir, &r.common_dir, &r.worktree, &r.recorded_at]
                    .into_iter()
                    .flatten()
                    .map(|p| p.capacity())
                    .sum::<usize>()
            })
            .sum();
        self.roots.capacity() * std::mem::size_of::<GitRoot>()
            + self.by_node.capacity() * 12
            + self.changes.capacity() * 4
            + paths
    }
}

/// One listed entry's sizes, as [`summarize`] gives them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootSummary {
    pub node: NodeId,
    /// The whole subtree's weight, nested repositories included: what the
    /// map shows for the folder.
    pub size: u64,
    /// The weight only this entry accounts for: its subtree less the
    /// nested entries that give their own subtrees a class.
    pub own: u64,
    /// Git metadata this entry accounts for: `own` of the `.git`, bare and
    /// Git directories whose nearest listed entry is this one.
    pub metadata: u64,
    /// Weight of the files a filter matches in the subtree, when given one.
    pub matching: Option<u64>,
    /// Files in the subtree.
    pub files: u32,
    /// The nearest listed entry with a class enclosing this one.
    pub parent: Option<NodeId>,
    /// The repository found above the scan root (`node` is the root).
    pub outside: bool,
}

/// Totals over every entry, each byte counted once.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GitTotals {
    /// Listed repositories other than linked worktrees: ordinary,
    /// submodules, separate Git directories, bare, Git directories.
    pub roots: u32,
    pub worktrees: u32,
    pub unresolved: u32,
    /// Checkout content of ordinary repositories, submodules and separate
    /// Git directories.
    pub checkout: u64,
    /// Linked worktree checkouts.
    pub worktree: u64,
    pub metadata: u64,
    /// Everything in any repository: the sum of the above.
    pub union: u64,
    /// Of `union`, what the filter matches, when given one.
    pub matching: Option<u64>,
}

/// The listed entries, largest first, and totals that count each byte
/// once: nested repositories are subtracted from the ones around them, and
/// shared metadata (a linked worktree's objects and admin folder) counts
/// where it is, in the main repository's `.git`.
///
/// O(entries × depth); for the list of repositories, not for every frame.
pub fn summarize(
    tree: &Tree,
    filter: Option<&Search>,
    metric: Metric,
) -> (Vec<RootSummary>, GitTotals) {
    let git = tree.git();
    // The kind each node with a say stands for: its own entry, or for the
    // root without one, the repository above the scan.
    let effective = |n: NodeId| -> Option<(RootKind, bool)> {
        if let Some(r) = tree.git_entry(n)
            && (r.kind.class().is_some() || r.kind.listed())
        {
            return Some((r.kind, false));
        }
        if n == NodeId::ROOT {
            return git.above().map(|(_, r)| (r.kind, true));
        }
        None
    };
    let weight = |n: NodeId| metric.weight(tree, n);
    let matching = |n: NodeId| filter.map_or(0, |s| s.weight(metric, n));

    // Every node with a say: entries with a class (overrides) and listed
    // entries (rows).
    let mut nodes: Vec<(NodeId, RootKind, bool)> = git
        .roots()
        .filter(|r| !tree.is_removed(r.node))
        .filter_map(|r| effective(r.node).map(|(k, o)| (r.node, k, o)))
        .collect();
    if let Some((kind, true)) = effective(NodeId::ROOT) {
        nodes.push((NodeId::ROOT, kind, true));
    }
    let at: HashMap<NodeId, usize> = nodes.iter().enumerate().map(|(i, e)| (e.0, i)).collect();

    let mut own: Vec<u64> = nodes.iter().map(|e| weight(e.0)).collect();
    let mut own_matching: Vec<u64> = nodes.iter().map(|e| matching(e.0)).collect();
    let mut parent: Vec<Option<NodeId>> = vec![None; nodes.len()];
    for (i, &(node, kind, _)) in nodes.iter().enumerate() {
        // An override's whole subtree comes off the nearest override
        // around it, and off any unresolved rows in between.
        let mut taken = kind.class().is_none();
        let mut cur = tree.parent(node);
        while let Some(a) = cur {
            if let Some(&j) = at.get(&a) {
                let k = nodes[j].1;
                if !taken {
                    own[j] = own[j].saturating_sub(weight(node));
                    if k.class().is_some() {
                        own_matching[j] = own_matching[j].saturating_sub(matching(node));
                        taken = true;
                    }
                }
                if parent[i].is_none() && k.listed() && k.class().is_some() {
                    parent[i] = Some(a);
                }
                if taken && parent[i].is_some() {
                    break;
                }
            }
            cur = tree.parent(a);
        }
    }

    let mut totals = GitTotals {
        matching: filter.map(|_| 0),
        ..GitTotals::default()
    };
    let mut metadata: HashMap<NodeId, u64> = HashMap::new();
    for (i, &(node, kind, _)) in nodes.iter().enumerate() {
        match kind.class() {
            Some(GitClass::Checkout) => totals.checkout += own[i],
            Some(GitClass::Worktree) => totals.worktree += own[i],
            Some(GitClass::Metadata) => {
                totals.metadata += own[i];
                // Counted with the nearest listed entry at or above it.
                let mut cur = Some(node);
                while let Some(a) = cur {
                    if at.get(&a).is_some_and(|&j| nodes[j].1.listed()) {
                        *metadata.entry(a).or_default() += own[i];
                        break;
                    }
                    cur = tree.parent(a);
                }
            }
            Some(GitClass::Other) | None => {}
        }
        if kind.class().is_some() {
            totals.union += own[i];
            if let Some(m) = totals.matching.as_mut() {
                *m += own_matching[i];
            }
        }
        if kind.listed() {
            match kind {
                RootKind::LinkedWorktree => totals.worktrees += 1,
                RootKind::Unresolved => totals.unresolved += 1,
                _ => totals.roots += 1,
            }
        }
    }

    let mut rows: Vec<RootSummary> = nodes
        .iter()
        .enumerate()
        .filter(|(_, e)| e.1.listed())
        .map(|(i, &(node, _, outside))| RootSummary {
            node,
            size: weight(node),
            own: own[i],
            metadata: metadata.get(&node).copied().unwrap_or(0),
            matching: filter.map(|s| s.weight(metric, node)),
            files: tree.file_count(node),
            parent: parent[i],
            outside,
        })
        .collect();
    rows.sort_by(|a, b| b.size.cmp(&a.size).then(a.node.cmp(&b.node)));
    (rows, totals)
}

/// Parses `HEAD`: `ref: refs/…` or a commit ID (40 or 64 hex digits).
/// `None` when it's neither, so the folder isn't a Git directory.
pub fn parse_head(bytes: &[u8]) -> Option<Head> {
    let line = first_line(bytes)?;
    if let Some(name) = line.strip_prefix("ref:") {
        let name = name.trim();
        if !name.starts_with("refs/") || name.len() == "refs/".len() || has_space(name) {
            return None;
        }
        return Some(match name.strip_prefix("refs/heads/") {
            Some(branch) if !branch.is_empty() => Head::Branch(branch.to_owned()),
            _ => Head::Ref(name.to_owned()),
        });
    }
    let id = line.trim();
    ((id.len() == 40 || id.len() == 64) && id.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| Head::Detached(id[..12].to_ascii_lowercase()))
}

/// Parses a `.git` file: `gitdir: <path>` on its first line. Returns the
/// path as written (relative to the file's folder unless absolute).
pub fn parse_pointer(bytes: &[u8]) -> Result<&str, Problem> {
    const NOT_POINTER: &str = ".git file doesn't name a Git directory";
    let line = first_line(bytes).ok_or(Problem::Malformed(NOT_POINTER))?;
    let path = line
        .strip_prefix("gitdir:")
        .map(str::trim)
        .ok_or(Problem::Malformed(NOT_POINTER))?;
    if path.is_empty() {
        return Err(Problem::Malformed(NOT_POINTER));
    }
    Ok(path)
}

/// Parses a file holding one path on its first line, as `commondir` and a
/// worktree's `gitdir` do. `None` when it's empty or not text.
pub fn parse_path_file(bytes: &[u8]) -> Option<&str> {
    let line = first_line(bytes)?.trim();
    (!line.is_empty()).then_some(line)
}

/// What [`parse_config_core`] reads from a repository's `config`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CoreConfig {
    /// `core.bare`; `None` when absent or not a boolean.
    pub bare: Option<bool>,
    /// `core.worktree`, as written.
    pub worktree: Option<String>,
}

/// Reads `core.bare` and `core.worktree` from a Git config file: sections
/// and keys ignoring case, the last value winning, quotes and trailing
/// comments removed. `include` and `includeIf` aren't followed.
pub fn parse_config_core(bytes: &[u8]) -> CoreConfig {
    let text = String::from_utf8_lossy(bytes);
    let mut config = CoreConfig::default();
    let mut in_core = false;
    for raw in text.lines() {
        let mut line = raw.trim_start_matches('\u{feff}').trim();
        if let Some(rest) = line.strip_prefix('[') {
            let Some(end) = rest.find(']') else {
                in_core = false;
                continue;
            };
            in_core = rest[..end].trim().eq_ignore_ascii_case("core");
            // A key may follow the header on the same line.
            line = rest[end + 1..].trim();
        }
        if !in_core || line.is_empty() || line.starts_with(['#', ';']) {
            continue;
        }
        let (key, value) = match line.split_once('=') {
            Some((k, v)) => (k.trim(), Some(config_value(v))),
            None => (line, None),
        };
        if key.eq_ignore_ascii_case("bare") {
            // A key without a value is true.
            config.bare = match value.as_deref() {
                None => Some(true),
                Some(v) => config_bool(v),
            };
        } else if key.eq_ignore_ascii_case("worktree") {
            config.worktree = value.filter(|v| !v.is_empty());
        }
    }
    config
}

fn config_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" | "" => Some(false),
        _ => None,
    }
}

/// A config value with quotes, escapes and a trailing comment removed.
fn config_value(raw: &str) -> String {
    let mut out = String::new();
    let mut quoted = false;
    let mut chars = raw.trim().chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => quoted = !quoted,
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => out.push(other),
                None => {}
            },
            '#' | ';' if !quoted => break,
            _ => out.push(c),
        }
    }
    if quoted {
        out
    } else {
        out.trim_end().to_owned()
    }
}

/// The first line of a small text file, without a byte-order mark or line
/// ending. `None` when it isn't UTF-8.
fn first_line(bytes: &[u8]) -> Option<&str> {
    let text = std::str::from_utf8(bytes).ok()?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let line = text.split('\n').next().unwrap_or("");
    Some(line.strip_suffix('\r').unwrap_or(line))
}

fn has_space(text: &str) -> bool {
    text.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// Lexical path handling for metadata paths, which may be written for
/// another platform's conventions (MSYS `/c/…` paths on Windows). Never
/// touches the disk: no `canonicalize`, which would follow links. Each
/// function takes the [`Style`] to apply, so every platform's rules can be
/// tested anywhere.
pub mod paths {
    use std::ffi::{OsStr, OsString};
    use std::path::{Path, PathBuf};

    /// Which platform's path rules apply.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Style {
        /// `\` and `/` separate; drive letters and UNC prefixes.
        pub windows: bool,
        /// Names compare ignoring case.
        pub fold_case: bool,
    }

    impl Style {
        pub const UNIX: Self = Self {
            windows: false,
            fold_case: false,
        };
        pub const MACOS: Self = Self {
            windows: false,
            fold_case: true,
        };
        pub const WINDOWS: Self = Self {
            windows: true,
            fold_case: true,
        };

        /// The rules of the platform the app runs on.
        pub fn native() -> Self {
            Self {
                windows: cfg!(windows),
                fold_case: cfg!(any(windows, target_os = "macos")),
            }
        }
    }

    /// Where a path starts.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Prefix {
        None,
        /// `C:`; the letter upper case.
        Drive(u8),
        /// `\\server\share`.
        Unc(OsString, OsString),
        /// `\\?\…` kept as written, through its first component.
        Verbatim(OsString),
    }

    struct Parsed {
        prefix: Prefix,
        /// Starts at a root (`/`, `\`, or after a UNC share).
        absolute: bool,
        parts: Vec<OsString>,
    }

    fn os(bytes: &[u8]) -> OsString {
        // SAFETY: every slice passed here comes from `as_encoded_bytes`
        // (or a `str`) split only next to ASCII separators or prefix
        // characters, which the encoding allows.
        unsafe { OsStr::from_encoded_bytes_unchecked(bytes) }.to_owned()
    }

    fn is_sep(b: u8, style: Style) -> bool {
        b == b'/' || (style.windows && b == b'\\')
    }

    fn parse(bytes: &[u8], style: Style) -> Parsed {
        let mut prefix = Prefix::None;
        let mut rest = bytes;
        let mut absolute = false;
        if style.windows {
            if let Some(after) = bytes.strip_prefix(br"\\?\") {
                // Verbatim: only `\` separates and nothing is normalized,
                // but `C:` and `UNC\server\share` read as their usual forms
                // so they compare with them.
                let parts: Vec<&[u8]> = after.split(|&b| b == b'\\').collect();
                if let Some(first) = parts.first()
                    && first.len() == 2
                    && first[1] == b':'
                    && first[0].is_ascii_alphabetic()
                {
                    return Parsed {
                        prefix: Prefix::Drive(first[0].to_ascii_uppercase()),
                        absolute: true,
                        parts: parts[1..]
                            .iter()
                            .filter(|p| !p.is_empty())
                            .map(|p| os(p))
                            .collect(),
                    };
                }
                if parts.len() >= 3 && parts[0].eq_ignore_ascii_case(b"UNC") {
                    return Parsed {
                        prefix: Prefix::Unc(os(parts[1]), os(parts[2])),
                        absolute: true,
                        parts: parts[3..]
                            .iter()
                            .filter(|p| !p.is_empty())
                            .map(|p| os(p))
                            .collect(),
                    };
                }
                return Parsed {
                    prefix: Prefix::Verbatim(os(parts.first().copied().unwrap_or(b""))),
                    absolute: true,
                    parts: parts
                        .iter()
                        .skip(1)
                        .filter(|p| !p.is_empty())
                        .map(|p| os(p))
                        .collect(),
                };
            }
            if bytes.len() >= 2
                && is_sep(bytes[0], style)
                && is_sep(bytes[1], style)
                && bytes.get(2).is_none_or(|&b| !is_sep(b, style))
            {
                // `\\server\share`, either separator.
                let mut it = bytes[2..].split(|&b| is_sep(b, style));
                let server = it.next().unwrap_or(b"");
                let share = it.next().unwrap_or(b"");
                if !server.is_empty() && !share.is_empty() {
                    let used = 2 + server.len() + 1 + share.len();
                    return Parsed {
                        prefix: Prefix::Unc(os(server), os(share)),
                        absolute: true,
                        parts: components(&bytes[used.min(bytes.len())..], style),
                    };
                }
            }
            if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
                prefix = Prefix::Drive(bytes[0].to_ascii_uppercase());
                rest = &bytes[2..];
                // `C:x` (relative to the drive's current folder) has no
                // meaning here; it's read as `C:\x`.
                absolute = true;
            }
        }
        if rest.first().is_some_and(|&b| is_sep(b, style)) {
            absolute = true;
        }
        Parsed {
            prefix,
            absolute,
            parts: components(rest, style),
        }
    }

    fn components(bytes: &[u8], style: Style) -> Vec<OsString> {
        bytes
            .split(|&b| is_sep(b, style))
            .filter(|p| !p.is_empty())
            .map(os)
            .collect()
    }

    /// Drops `.` and empty names and applies `..`, never above the root.
    fn fold(parsed: &mut Parsed) {
        if matches!(parsed.prefix, Prefix::Verbatim(_)) {
            return;
        }
        let mut out: Vec<OsString> = Vec::with_capacity(parsed.parts.len());
        for part in parsed.parts.drain(..) {
            match part.as_encoded_bytes() {
                b"." => {}
                b".." => match out.last() {
                    Some(last) if last.as_encoded_bytes() != b".." => {
                        out.pop();
                    }
                    _ if parsed.absolute => {}
                    _ => out.push(part),
                },
                _ => out.push(part),
            }
        }
        parsed.parts = out;
    }

    fn render(parsed: &Parsed, style: Style) -> PathBuf {
        let sep = if style.windows { "\\" } else { "/" };
        let mut out = OsString::new();
        match &parsed.prefix {
            Prefix::None => {}
            Prefix::Drive(letter) => {
                out.push(OsStr::new(
                    std::str::from_utf8(&[*letter, b':']).unwrap_or("C:"),
                ));
            }
            Prefix::Unc(server, share) => {
                out.push(r"\\");
                out.push(server);
                out.push(r"\");
                out.push(share);
            }
            Prefix::Verbatim(first) => {
                out.push(r"\\?\");
                out.push(first);
            }
        }
        if parsed.absolute {
            out.push(sep);
        }
        for (i, part) in parsed.parts.iter().enumerate() {
            if i > 0 {
                out.push(sep);
            }
            out.push(part);
        }
        if out.is_empty() {
            out.push(".");
        }
        PathBuf::from(out)
    }

    /// `path` with `.` and `..` applied by name alone, `..` never climbing
    /// above the root; on Windows with `\` separators and an upper-case
    /// drive letter. `\\?\` paths are kept as written.
    pub fn normalize(path: &Path, style: Style) -> PathBuf {
        let mut parsed = parse(path.as_os_str().as_encoded_bytes(), style);
        fold(&mut parsed);
        render(&parsed, style)
    }

    /// A path as written in a metadata file, relative to `base` unless
    /// absolute, normalized. On Windows, MSYS and Cygwin paths (`/c/x`,
    /// `/cygdrive/c/x`) read as `C:\x`, `//server/share` as a UNC path, and
    /// a path rooted without a drive takes `base`'s.
    pub fn resolve(base: &Path, raw: &str, style: Style) -> PathBuf {
        let raw = raw.trim();
        let converted;
        let raw = if style.windows {
            match msys_drive(raw) {
                Some(path) => {
                    converted = path;
                    converted.as_str()
                }
                None => raw,
            }
        } else {
            raw
        };
        let mut parsed = parse(raw.as_bytes(), style);
        if parsed.prefix == Prefix::None {
            let base = parse(base.as_os_str().as_encoded_bytes(), style);
            if !parsed.absolute {
                let mut parts = base.parts;
                parts.append(&mut parsed.parts);
                parsed.parts = parts;
                parsed.absolute = base.absolute;
            }
            parsed.prefix = base.prefix;
        }
        fold(&mut parsed);
        render(&parsed, style)
    }

    /// `/c/x` or `/cygdrive/c/x` as `C:/x`.
    fn msys_drive(raw: &str) -> Option<String> {
        let rest = raw.strip_prefix("/cygdrive").unwrap_or(raw);
        let rest = rest.strip_prefix('/')?;
        let mut chars = rest.chars();
        let letter = chars.next().filter(char::is_ascii_alphabetic)?;
        let after = chars.as_str();
        if !(after.is_empty() || after.starts_with('/')) {
            return None;
        }
        Some(format!(
            "{}:/{}",
            letter.to_ascii_uppercase(),
            after.trim_start_matches('/')
        ))
    }

    fn same_name(a: &OsStr, b: &OsStr, fold_case: bool) -> bool {
        if !fold_case {
            return a == b;
        }
        match (a.to_str(), b.to_str()) {
            (Some(a), Some(b)) => a == b || a.to_lowercase() == b.to_lowercase(),
            _ => a
                .as_encoded_bytes()
                .eq_ignore_ascii_case(b.as_encoded_bytes()),
        }
    }

    fn same_prefix(a: &Prefix, b: &Prefix) -> bool {
        match (a, b) {
            // Drive letters and UNC names ignore case on any file system.
            (Prefix::Unc(s1, h1), Prefix::Unc(s2, h2)) => {
                same_name(s1, s2, true) && same_name(h1, h2, true)
            }
            (Prefix::Verbatim(x), Prefix::Verbatim(y)) => same_name(x, y, true),
            _ => a == b,
        }
    }

    /// The names leading from `root` to `path`, after normalizing both, or
    /// `None` when `path` isn't at or under `root`. Empty for `root` itself.
    pub fn relative_to(root: &Path, path: &Path, style: Style) -> Option<Vec<OsString>> {
        let mut root = parse(root.as_os_str().as_encoded_bytes(), style);
        let mut path = parse(path.as_os_str().as_encoded_bytes(), style);
        fold(&mut root);
        fold(&mut path);
        if root.absolute != path.absolute || !same_prefix(&root.prefix, &path.prefix) {
            return None;
        }
        if path.parts.len() < root.parts.len()
            || !root
                .parts
                .iter()
                .zip(&path.parts)
                .all(|(a, b)| same_name(a, b, style.fold_case))
        {
            return None;
        }
        Some(path.parts.split_off(root.parts.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::paths::{Style, normalize, relative_to, resolve};
    use super::*;
    use crate::search::Matcher;
    use crate::size::Allocated;
    use crate::tree::FileSizes;
    use std::ffi::OsString;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    fn names(parts: &[&str]) -> Option<Vec<OsString>> {
        Some(parts.iter().map(OsString::from).collect())
    }

    #[test]
    fn normalizes_unix_paths_by_name() {
        let u = Style::UNIX;
        assert_eq!(normalize(&p("/a/./b/../c/"), u), p("/a/c"));
        assert_eq!(
            normalize(&p("/../../x"), u),
            p("/x"),
            "never above the root"
        );
        assert_eq!(normalize(&p("a/../../b"), u), p("../b"));
        assert_eq!(normalize(&p("a/.."), u), p("."));
        assert_eq!(normalize(&p("//srv//x"), u), p("/srv/x"));
        // A backslash is an ordinary character.
        assert_eq!(normalize(&p(r"/a\b/c"), u), p(r"/a\b/c"));
    }

    #[test]
    fn normalizes_windows_paths_by_name() {
        let w = Style::WINDOWS;
        assert_eq!(normalize(&p("C:/x/../y"), w), p(r"C:\y"));
        assert_eq!(normalize(&p(r"c:\a/b\..\..\..\z"), w), p(r"C:\z"));
        assert_eq!(
            normalize(&p(r"\\server\share\a\..\.."), w),
            p(r"\\server\share\")
        );
        assert_eq!(normalize(&p("//server/share/x"), w), p(r"\\server\share\x"));
        assert_eq!(normalize(&p(r"\\?\C:\a\..\b"), w), p(r"C:\b"));
        assert_eq!(
            normalize(&p(r"\\?\GLOBALROOT\Device\x\..\y"), w),
            p(r"\\?\GLOBALROOT\Device\x\..\y"),
            "other verbatim paths are kept as written"
        );
        assert_eq!(normalize(&p(r"a\.\b/c"), w), p(r"a\b\c"));
    }

    #[test]
    fn resolves_relative_and_absolute_targets() {
        let u = Style::UNIX;
        assert_eq!(
            resolve(&p("/w/app"), " ../main/.git/worktrees/app\n", u),
            p("/w/main/.git/worktrees/app")
        );
        assert_eq!(
            resolve(&p("/w/app"), "/srv/repo.git", u),
            p("/srv/repo.git")
        );
        assert_eq!(resolve(&p("/w"), "../../../x", u), p("/x"));
        // MSYS paths mean nothing special elsewhere.
        assert_eq!(resolve(&p("/w"), "/c/Users", u), p("/c/Users"));

        let w = Style::WINDOWS;
        assert_eq!(
            resolve(&p(r"D:\w\app"), "/c/Users/me/x", w),
            p(r"C:\Users\me\x")
        );
        assert_eq!(resolve(&p(r"D:\w"), "/cygdrive/e/r", w), p(r"E:\r"));
        assert_eq!(resolve(&p(r"D:\w"), "/c", w), p(r"C:\"));
        assert_eq!(
            resolve(&p(r"D:\w"), "/cache/x", w),
            p(r"D:\cache\x"),
            "not a drive"
        );
        assert_eq!(resolve(&p(r"D:\w\a"), "../b/.git", w), p(r"D:\w\b\.git"));
        assert_eq!(resolve(&p(r"D:\w"), "C:/x/../y", w), p(r"C:\y"));
        assert_eq!(
            resolve(&p(r"D:\w"), "//server/share/r", w),
            p(r"\\server\share\r")
        );
        assert_eq!(resolve(&p(r"D:\w"), r"\\?\C:\r", w), p(r"C:\r"));
        assert_eq!(
            resolve(&p(r"\\srv\sh\w"), r"..\x/y", w),
            p(r"\\srv\sh\x\y"),
            "mixed separators"
        );
    }

    #[test]
    fn relative_to_compares_by_component() {
        let u = Style::UNIX;
        assert_eq!(relative_to(&p("/s"), &p("/s/a/b"), u), names(&["a", "b"]));
        assert_eq!(relative_to(&p("/s"), &p("/s"), u), names(&[]));
        assert_eq!(
            relative_to(&p("/s"), &p("/sx/a"), u),
            None,
            "not a prefix by text"
        );
        assert_eq!(relative_to(&p("/s"), &p("/S/a"), u), None);
        assert_eq!(relative_to(&p("/s/x"), &p("/s/x/../y"), u), None);
        assert_eq!(
            relative_to(&p("/s"), &p("/S/a"), Style::MACOS),
            names(&["a"])
        );

        let w = Style::WINDOWS;
        assert_eq!(
            relative_to(&p(r"C:\Scan"), &p(r"c:/scan/Repo/.git"), w),
            names(&["Repo", ".git"])
        );
        assert_eq!(relative_to(&p(r"C:\Scan"), &p(r"D:\Scan\x"), w), None);
        assert_eq!(
            relative_to(&p(r"C:\Scan"), &p(r"\\?\C:\Scan\x"), w),
            names(&["x"])
        );
        assert_eq!(
            relative_to(
                &p(r"\\SRV\Share"),
                &p("//srv/share/a"),
                Style {
                    windows: true,
                    fold_case: false
                }
            ),
            names(&["a"]),
            "server and share names ignore case"
        );
        assert_eq!(
            relative_to(
                &p(r"C:\Scan"),
                &p(r"C:\scan\x"),
                Style {
                    windows: true,
                    fold_case: false
                }
            ),
            None
        );
        assert_eq!(
            relative_to(&p("/s/ä"), &p("/s/Ä/x"), Style::MACOS),
            names(&["x"])
        );
    }

    #[test]
    fn parses_head() {
        assert_eq!(
            parse_head(b"ref: refs/heads/main\n"),
            Some(Head::Branch("main".into()))
        );
        assert_eq!(
            parse_head(b"ref: refs/heads/feature/x\r\n"),
            Some(Head::Branch("feature/x".into()))
        );
        assert_eq!(
            parse_head(b"ref:refs/remotes/origin/HEAD"),
            Some(Head::Ref("refs/remotes/origin/HEAD".into()))
        );
        let sha1 = "0123456789ABCDEF0123456789abcdef01234567";
        assert_eq!(
            parse_head(sha1.as_bytes()),
            Some(Head::Detached("0123456789ab".into()))
        );
        let sha256 = "a".repeat(64);
        assert_eq!(
            parse_head(sha256.as_bytes()),
            Some(Head::Detached("a".repeat(12)))
        );
        for bad in [
            &b""[..],
            b"hello",
            b"ref: main",
            b"ref: refs/",
            b"ref: refs/heads/a b",
            b"0123456789",
            b"\xff\xfe",
            &[b'g'; 40],
        ] {
            assert_eq!(parse_head(bad), None, "{:?}", String::from_utf8_lossy(bad));
        }
    }

    #[test]
    fn parses_pointer_and_path_files() {
        assert_eq!(
            parse_pointer(b"gitdir: ../.git/worktrees/x\r\n"),
            Ok("../.git/worktrees/x")
        );
        assert_eq!(
            parse_pointer(b"\xEF\xBB\xBFgitdir:/abs/path\nextra"),
            Ok("/abs/path")
        );
        assert!(matches!(
            parse_pointer(b"gitdir:   \n"),
            Err(Problem::Malformed(_))
        ));
        assert!(matches!(
            parse_pointer(b"not a pointer"),
            Err(Problem::Malformed(_))
        ));
        assert!(matches!(parse_pointer(b"\xff"), Err(Problem::Malformed(_))));
        assert_eq!(parse_path_file(b"../..\n"), Some("../.."));
        assert_eq!(parse_path_file(b"/w/app/.git\r\n"), Some("/w/app/.git"));
        assert_eq!(parse_path_file(b"\n"), None);
    }

    #[test]
    fn parses_core_config() {
        let c = parse_config_core(
            b"[core]\n\trepositoryformatversion = 0\n\tbare = true\n[remote \"origin\"]\n\tbare = false\n",
        );
        assert_eq!(c.bare, Some(true), "other sections don't count");
        for (value, bare) in [
            ("yes", Some(true)),
            ("On", Some(true)),
            ("1", Some(true)),
            ("FALSE", Some(false)),
            ("off", Some(false)),
            ("0", Some(false)),
            ("maybe", None),
            ("true ; comment", Some(true)),
            ("\"false\"", Some(false)),
        ] {
            let text = format!("[CORE]\nBare = {value}\n");
            assert_eq!(parse_config_core(text.as_bytes()).bare, bare, "{value}");
        }
        assert_eq!(parse_config_core(b"[core]\n  bare\n").bare, Some(true));
        assert_eq!(parse_config_core(b"[core] bare = false").bare, Some(false));
        assert_eq!(parse_config_core(b"[core \"x\"]\nbare = true").bare, None);
        assert_eq!(parse_config_core(b"bare = true\n").bare, None, "no section");
        let c = parse_config_core(
            b"[core]\nbare=false\nworktree = \"../my tree\" # where\n[core]\n bare = true\n",
        );
        assert_eq!(c.bare, Some(true), "the last value wins");
        assert_eq!(c.worktree.as_deref(), Some("../my tree"));
        assert_eq!(
            parse_config_core(b"[core]\nworktree = C:\\\\w\n")
                .worktree
                .as_deref(),
            Some(r"C:\w")
        );
        assert_eq!(parse_config_core(b"\xff\xff[core]\n").bare, None);
    }

    #[test]
    fn kinds_map_to_classes() {
        use RootKind::*;
        assert_eq!(Repository.class(), Some(GitClass::Checkout));
        assert_eq!(Submodule.class(), Some(GitClass::Checkout));
        assert_eq!(SeparateGitDir.class(), Some(GitClass::Checkout));
        assert_eq!(LinkedWorktree.class(), Some(GitClass::Worktree));
        for k in [Bare, GitDir, DotGit] {
            assert_eq!(k.class(), Some(GitClass::Metadata));
        }
        for k in [Pending, WorktreeAdmin, Unresolved] {
            assert_eq!(k.class(), None);
        }
        assert!(Unresolved.listed() && Bare.listed() && GitDir.listed());
        assert!(!DotGit.listed() && !Pending.listed() && !WorktreeAdmin.listed());
        for c in [
            GitClass::Other,
            GitClass::Checkout,
            GitClass::Worktree,
            GitClass::Metadata,
        ] {
            assert_eq!(GitClass::from_u8(c as u8), c);
        }
    }

    fn sizes(n: u64) -> FileSizes {
        FileSizes {
            logical: n,
            allocated: Allocated::known(n),
        }
    }

    fn file(t: &mut Tree, dir: NodeId, name: &str, n: u64) -> NodeId {
        t.add_file(dir, name.as_ref(), sizes(n), None, 0).unwrap()
    }

    fn dir(t: &mut Tree, parent: NodeId, name: &str) -> NodeId {
        t.add_dir(parent, name.as_ref()).unwrap()
    }

    fn row(rows: &[RootSummary], node: NodeId) -> RootSummary {
        *rows.iter().find(|r| r.node == node).unwrap()
    }

    #[test]
    fn summarize_counts_nested_repositories_once() {
        let mut t = Tree::new("/s");
        let outer = dir(&mut t, NodeId::ROOT, "outer");
        let dot = dir(&mut t, outer, ".git");
        file(&mut t, dot, "pack", 100);
        file(&mut t, outer, "a.rs", 10);
        let inner = dir(&mut t, outer, "inner");
        let inner_dot = dir(&mut t, inner, ".git");
        file(&mut t, inner_dot, "pack", 50);
        file(&mut t, inner, "b.rs", 5);
        file(&mut t, NodeId::ROOT, "loose", 1000);
        t.set_git(GitRoot::new(outer, RootKind::Repository));
        t.set_git(GitRoot::new(dot, RootKind::DotGit));
        t.set_git(GitRoot::new(inner, RootKind::Repository));
        t.set_git(GitRoot::new(inner_dot, RootKind::DotGit));

        let (rows, totals) = summarize(&t, None, Metric::Logical);
        assert_eq!(
            rows.iter().map(|r| r.node).collect::<Vec<_>>(),
            [outer, inner]
        );
        let o = row(&rows, outer);
        assert_eq!((o.size, o.own, o.metadata, o.files), (165, 10, 100, 4));
        assert_eq!(o.parent, None);
        let i = row(&rows, inner);
        assert_eq!(
            (i.size, i.own, i.metadata, i.parent),
            (55, 5, 50, Some(outer))
        );
        assert_eq!(totals.union, 165);
        assert_eq!(
            totals.union,
            totals.checkout + totals.worktree + totals.metadata
        );
        assert_eq!((totals.checkout, totals.metadata), (15, 150));
        assert_eq!(
            (totals.roots, totals.worktrees, totals.unresolved),
            (2, 0, 0)
        );
        assert_eq!(totals.matching, None);
    }

    #[test]
    fn summarize_counts_shared_metadata_in_the_main_repository() {
        let mut t = Tree::new("/s");
        let main = dir(&mut t, NodeId::ROOT, "main");
        let dot = dir(&mut t, main, ".git");
        file(&mut t, dot, "objects", 400);
        let worktrees = dir(&mut t, dot, "worktrees");
        let admin = dir(&mut t, worktrees, "wt");
        file(&mut t, admin, "HEAD", 1);
        file(&mut t, main, "src.rs", 20);
        let wt = dir(&mut t, NodeId::ROOT, "wt");
        let pointer = file(&mut t, wt, ".git", 2);
        file(&mut t, wt, "src.rs", 30);
        t.set_git(GitRoot::new(main, RootKind::Repository));
        t.set_git(GitRoot::new(dot, RootKind::DotGit));
        t.set_git(GitRoot::new(admin, RootKind::WorktreeAdmin));
        t.set_git(GitRoot::new(wt, RootKind::LinkedWorktree));
        t.set_git(GitRoot::new(pointer, RootKind::DotGit));

        let (rows, totals) = summarize(&t, None, Metric::Logical);
        assert_eq!(rows.len(), 2, "the admin folder isn't listed");
        let m = row(&rows, main);
        assert_eq!((m.own, m.metadata), (20, 401));
        let w = row(&rows, wt);
        assert_eq!((w.own, w.metadata, w.parent), (30, 2, None));
        assert_eq!(
            (totals.checkout, totals.worktree, totals.metadata),
            (20, 30, 403)
        );
        assert_eq!(totals.union, 453);
        assert_eq!((totals.roots, totals.worktrees), (1, 1));
    }

    #[test]
    fn summarize_lists_bare_git_dirs_unresolved_and_above() {
        let mut t = Tree::new("/s/repo/src");
        let bare = dir(&mut t, NodeId::ROOT, "x.git");
        file(&mut t, bare, "pack", 70);
        let gd = dir(&mut t, NodeId::ROOT, "gitdir");
        file(&mut t, gd, "HEAD", 3);
        let broken = dir(&mut t, NodeId::ROOT, "broken");
        file(&mut t, broken, "f", 9);
        let nested = dir(&mut t, broken, "nested");
        file(&mut t, nested, "g", 4);
        file(&mut t, NodeId::ROOT, "main.rs", 11);
        t.set_git(GitRoot::new(bare, RootKind::Bare));
        t.set_git(GitRoot::new(gd, RootKind::GitDir));
        t.set_git(GitRoot::unresolved(broken, Problem::Link));
        t.set_git(GitRoot::new(nested, RootKind::Repository));
        t.set_git_above(
            p("/s/repo"),
            GitRoot::new(NodeId::ROOT, RootKind::Repository),
        );

        let (rows, totals) = summarize(&t, None, Metric::Logical);
        assert_eq!(rows.len(), 5);
        let top = row(&rows, NodeId::ROOT);
        assert!(top.outside);
        // Everything but the bare repository, the Git directory and the
        // nested repository is the outer checkout's, the unresolved folder's
        // own files included. The bare repository and the Git directory are
        // listed themselves, so their metadata is in their own rows.
        assert_eq!((top.size, top.own, top.metadata), (97, 20, 0));
        assert_eq!(row(&rows, gd).metadata, 3);
        assert_eq!(row(&rows, bare).parent, Some(NodeId::ROOT));
        assert_eq!((row(&rows, bare).own, row(&rows, bare).metadata), (70, 70));
        let b = row(&rows, broken);
        assert_eq!((b.own, b.parent, b.outside), (9, Some(NodeId::ROOT), false));
        assert_eq!(row(&rows, nested).parent, Some(NodeId::ROOT));
        assert_eq!(
            (totals.checkout, totals.metadata, totals.union),
            (24, 73, 97)
        );
        assert_eq!((totals.roots, totals.unresolved), (4, 1));
        assert_eq!(t.git().listed_count(), 5);
    }

    #[test]
    fn summarize_reports_matching_bytes_per_root() {
        let mut t = Tree::new("/s");
        let r = dir(&mut t, NodeId::ROOT, "r");
        let dot = dir(&mut t, r, ".git");
        file(&mut t, dot, "x.rs", 7);
        file(&mut t, r, "a.rs", 10);
        file(&mut t, r, "a.txt", 100);
        let inner = dir(&mut t, r, "inner");
        file(&mut t, inner, "b.rs", 5);
        file(&mut t, NodeId::ROOT, "c.rs", 1000);
        t.set_git(GitRoot::new(r, RootKind::Repository));
        t.set_git(GitRoot::new(dot, RootKind::DotGit));
        t.set_git(GitRoot::new(inner, RootKind::Repository));
        let mut search = Search::new(Matcher::new("*.rs").unwrap());
        assert!(search.catch_up(&t, usize::MAX));

        let (rows, totals) = summarize(&t, Some(&search), Metric::Logical);
        assert_eq!(row(&rows, r).matching, Some(22));
        assert_eq!(row(&rows, inner).matching, Some(5));
        assert_eq!(totals.matching, Some(22), "nested matches counted once");
    }

    #[test]
    fn summarize_skips_removed_entries() {
        let mut t = Tree::new("/s");
        let r = dir(&mut t, NodeId::ROOT, "r");
        file(&mut t, r, "a", 10);
        t.set_git(GitRoot::new(r, RootKind::Repository));
        assert!(t.remove(r));
        let (rows, totals) = summarize(&t, None, Metric::Logical);
        assert!(rows.is_empty());
        assert_eq!(totals, GitTotals::default());
        assert_eq!(t.git().listed_count(), 0);
    }
}
