//! Filename search as an overlay on the scan tree.
//!
//! A query is a list of patterns separated by `;`, matched against a file's
//! basename (including the extension), ignoring case. A pattern with `*` or
//! `?` wildcards must match the whole name; one without matches any name
//! containing it. A leading `!` excludes. A pattern starting with `dir:`
//! matches folder names instead: a file then matches only inside a folder
//! (at any depth below the scan root) that matches one, and `!dir:` leaves
//! out files inside matching folders. `is:video` (or any [`FileType`]
//! keyword) matches files of that type, like an included pattern, and
//! `!is:` leaves a type out. Full paths are never matched, and file
//! contents are never read.
//!
//! [`Search`] keeps its own totals for matching files and every folder above
//! them, so the treemap can be drawn from matching bytes only. It catches up
//! incrementally: new nodes are examined in ID order (IDs only grow), and
//! files re-measured after being added are replayed from
//! [`Tree::changed_files`], which also lists removed files; those leave the
//! matches. Hard-link ownership is kept: an alias that matches contributes
//! its logical length but no allocation, exactly as in the tree.

use crate::filetype::FileType;
use crate::layout::Metric;
use crate::tree::{NodeId, Tree};
use std::ffi::OsStr;

const NONE: u32 = u32::MAX;

/// Case-insensitive matcher for a query such as `*.jpg; *.png; !thumb*` or
/// `*.js; !dir:node_modules`.
///
/// A file matches when its name matches any included file pattern (or there
/// are none) and no excluded one, it's inside a folder matching an included
/// `dir:` pattern (or there are none), and inside no folder matching an
/// excluded one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Matcher {
    query: String,
    include: Vec<Pattern>,
    exclude: Vec<Pattern>,
    /// `dir:` patterns, matched against folder names.
    dir_include: Vec<Pattern>,
    dir_exclude: Vec<Pattern>,
    /// `is:` file types; an included type counts like an included pattern.
    types: TypeSet,
    /// Every pattern is ASCII, so ASCII names can be matched bytewise.
    ascii: bool,
    /// Some pattern has `?`, so non-ASCII names are needed as characters.
    globs: bool,
}

/// One pattern of a query, lowercased.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Pattern {
    /// No wildcards: the name contains it.
    Contains(String),
    /// Only `*` wildcards, kept as the pieces between them: the name starts
    /// with the first, ends with the last, and holds the rest in order.
    Stars { text: String, pieces: Vec<String> },
    /// `*` matches any run of characters and `?` one character; the whole
    /// name must match.
    Glob { text: String, chars: Vec<char> },
}

impl Pattern {
    fn new(term: &str) -> Self {
        let text = term.to_lowercase();
        if text.contains('?') {
            let chars = text.chars().collect();
            Self::Glob { text, chars }
        } else if text.contains('*') {
            let pieces = text.split('*').map(str::to_owned).collect();
            Self::Stars { text, pieces }
        } else {
            Self::Contains(text)
        }
    }

    fn text(&self) -> &str {
        match self {
            Self::Contains(text) | Self::Stars { text, .. } | Self::Glob { text, .. } => text,
        }
    }

    /// Matches an ASCII name against an ASCII pattern without allocating.
    fn matches_ascii(&self, name: &[u8]) -> bool {
        match self {
            Self::Contains(needle) => find_ascii(name, needle.as_bytes()).is_some(),
            Self::Stars { pieces, .. } => stars(
                pieces,
                name,
                |p| p.as_bytes(),
                |a, b| a.eq_ignore_ascii_case(b),
                find_ascii,
            ),
            Self::Glob { text, .. } => wildcard(text.as_bytes(), name, b'*', b'?', |p, n| {
                p == n.to_ascii_lowercase()
            }),
        }
    }

    /// Matches a lowercased name, given also as characters when there are
    /// `?` globs to match.
    fn matches_lower(&self, name: &str, chars: &[char]) -> bool {
        match self {
            Self::Contains(needle) => name.contains(needle.as_str()),
            Self::Stars { pieces, .. } => stars(
                pieces,
                name.as_bytes(),
                |p| p.as_bytes(),
                |a, b| a == b,
                // Both sides are lowercased UTF-8, so equal bytes are
                // equal characters.
                find_exact,
            ),
            Self::Glob { chars: pattern, .. } => wildcard(pattern, chars, '*', '?', |p, n| p == n),
        }
    }
}

/// Where `needle` first occurs in `hay`.
fn find_exact(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Where `needle` first occurs in `hay`, ignoring ASCII case.
fn find_ascii(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    hay.windows(needle.len())
        .position(|w| w.eq_ignore_ascii_case(needle))
}

/// Whether `name` matches star-separated `pieces` (at least two): it starts
/// with the first, ends with the last, and holds the others in order,
/// without overlaps. Taking each middle piece at its earliest place is
/// always safe.
fn stars(
    pieces: &[String],
    name: &[u8],
    bytes: impl Fn(&String) -> &[u8],
    eq: impl Fn(&[u8], &[u8]) -> bool,
    find: impl Fn(&[u8], &[u8]) -> Option<usize>,
) -> bool {
    let (Some(first), Some(last)) = (pieces.first(), pieces.last()) else {
        return false;
    };
    let (first, last) = (bytes(first), bytes(last));
    if first.len() + last.len() > name.len()
        || !eq(&name[..first.len()], first)
        || !eq(&name[name.len() - last.len()..], last)
    {
        return false;
    }
    let mut rest = &name[first.len()..name.len() - last.len()];
    for piece in &pieces[1..pieces.len() - 1] {
        let piece = bytes(piece);
        match find(rest, piece) {
            Some(at) => rest = &rest[at + piece.len()..],
            None => return false,
        }
    }
    true
}

/// Whether `name` matches `pattern`, where `star` matches any run of items
/// and `one` any single item. Greedy, backtracking only to the last star,
/// which is enough for this pattern language.
fn wildcard<T: Copy + PartialEq>(
    pattern: &[T],
    name: &[T],
    star: T,
    one: T,
    eq: impl Fn(T, T) -> bool,
) -> bool {
    let (mut p, mut n) = (0, 0);
    // Pattern position after the last star, and the name position it
    // currently stands for.
    let mut back: Option<(usize, usize)> = None;
    while n < name.len() {
        match pattern.get(p) {
            Some(&c) if c == star => {
                p += 1;
                back = Some((p, n));
            }
            Some(&c) if c == one || eq(c, name[n]) => {
                p += 1;
                n += 1;
            }
            _ => match back {
                Some((bp, bn)) => {
                    p = bp;
                    n = bn + 1;
                    back = Some((bp, n));
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|&c| c == star)
}

impl Matcher {
    /// `None` when the query has no patterns; an unknown `is:` type counts
    /// as none. Use [`Matcher::parse`] to report it.
    pub fn new(query: &str) -> Option<Self> {
        Self::parse(query).ok().flatten()
    }

    /// `Ok(None)` when the query has no patterns. Patterns are separated by
    /// `;` and trimmed; a leading `!` excludes, a `dir:` prefix (any case)
    /// makes a folder pattern, and `is:` names a file type. Fails on an
    /// unknown or ambiguous type.
    pub fn parse(query: &str) -> Result<Option<Self>, String> {
        let mut include = Vec::new();
        let mut exclude = Vec::new();
        let mut dir_include = Vec::new();
        let mut dir_exclude = Vec::new();
        let mut types = TypeSet::default();
        for term in query.split(';') {
            let term = term.trim();
            let (excluded, term) = match term.strip_prefix('!') {
                Some(rest) => (true, rest.trim_start()),
                None => (false, term),
            };
            if let Some(word) = strip_prefix_ignore_case(term, "is:") {
                let word = word.trim();
                // "is:" alone, still being typed, is no pattern yet.
                if !word.is_empty() {
                    let t = FileType::from_keyword(word)?;
                    if excluded {
                        types.exclude |= TypeSet::bit(t);
                    } else {
                        types.include |= TypeSet::bit(t);
                    }
                }
                continue;
            }
            let (folder, term) = match strip_prefix_ignore_case(term, "dir:") {
                Some(rest) => (true, rest.trim_start()),
                None => (false, term),
            };
            let list = match (folder, excluded) {
                (false, false) => &mut include,
                (false, true) => &mut exclude,
                (true, false) => &mut dir_include,
                (true, true) => &mut dir_exclude,
            };
            if !term.is_empty() {
                list.push(Pattern::new(term));
            }
        }
        let all = || {
            include
                .iter()
                .chain(&exclude)
                .chain(&dir_include)
                .chain(&dir_exclude)
        };
        if all().next().is_none() && types.is_empty() {
            return Ok(None);
        }
        let ascii = all().all(|p| p.text().is_ascii());
        let globs = all().any(|p| matches!(p, Pattern::Glob { .. }));
        Ok(Some(Self {
            query: query.to_owned(),
            include,
            exclude,
            dir_include,
            dir_exclude,
            types,
            ascii,
            globs,
        }))
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    /// Whether a file name passes the file patterns, taking its type from
    /// the name alone. See [`Matcher::matches_file`].
    pub fn matches(&self, name: &OsStr) -> bool {
        self.matches_file(name, 0)
    }

    /// Whether a file passes the file patterns and `is:` types; `len` only
    /// settles `.ts` (see [`FileType::of`]). The folder patterns are applied
    /// by [`Search`], which knows where the file is.
    pub fn matches_file(&self, name: &OsStr, len: u64) -> bool {
        let kind = || TypeSet::bit(FileType::of(name, len));
        let any_include = !self.include.is_empty() || self.types.include != 0;
        self.with_name(name, |test| {
            let included = !any_include
                || self.include.iter().any(test)
                || (self.types.include != 0 && self.types.include & kind() != 0);
            let excluded = self.exclude.iter().any(test)
                || (self.types.exclude != 0 && self.types.exclude & kind() != 0);
            included && !excluded
        })
    }

    /// The query has `dir:` patterns.
    pub fn has_folder_patterns(&self) -> bool {
        !self.dir_include.is_empty() || !self.dir_exclude.is_empty()
    }

    /// How a folder's own name meets the `dir:` patterns.
    pub fn folder(&self, name: &OsStr) -> FolderMatch {
        if !self.has_folder_patterns() {
            return FolderMatch::default();
        }
        self.with_name(name, |test| FolderMatch {
            included: self.dir_include.iter().any(test),
            excluded: self.dir_exclude.iter().any(test),
        })
    }

    /// Calls `decide` with a test of `name` against one pattern.
    fn with_name<R>(&self, name: &OsStr, decide: impl FnOnce(&dyn Fn(&Pattern) -> bool) -> R) -> R {
        let bytes = name.as_encoded_bytes();
        // Fast path without allocating: ASCII names against ASCII patterns.
        if self.ascii && bytes.is_ascii() {
            return decide(&|p| p.matches_ascii(bytes));
        }
        let lower = name.to_string_lossy().to_lowercase();
        let chars: Vec<char> = if self.globs {
            lower.chars().collect()
        } else {
            Vec::new()
        };
        decide(&|p| p.matches_lower(&lower, &chars))
    }

    /// Whether a file inside folders described by `scope` can match.
    fn in_scope(&self, scope: FolderMatch) -> bool {
        (self.dir_include.is_empty() || scope.included) && !scope.excluded
    }
}

/// Included and excluded file types, one bit per [`FileType`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct TypeSet {
    include: u16,
    exclude: u16,
}

impl TypeSet {
    fn bit(t: FileType) -> u16 {
        1 << (t as u8)
    }

    fn is_empty(self) -> bool {
        self.include == 0 && self.exclude == 0
    }
}

/// Whether a folder, or one above it, matches the included or excluded
/// `dir:` patterns.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FolderMatch {
    pub included: bool,
    pub excluded: bool,
}

impl FolderMatch {
    fn within(self, outer: Self) -> Self {
        Self {
            included: self.included || outer.included,
            excluded: self.excluded || outer.excluded,
        }
    }

    fn bits(self) -> u8 {
        u8::from(self.included) | (u8::from(self.excluded) << 1)
    }

    fn from_bits(bits: u8) -> Self {
        Self {
            included: bits & 1 != 0,
            excluded: bits & 2 != 0,
        }
    }
}

/// `text` without `prefix`, compared ignoring ASCII case.
fn strip_prefix_ignore_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &text[prefix.len()..])
}

/// Sizes of the matching files at or under a node.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Totals {
    pub logical: u64,
    /// Known, owned allocation (aliases and unknown allocations count 0).
    pub allocated: u64,
    /// Matching files whose allocation is unknown.
    pub unknown: u32,
    pub files: u32,
}

impl Totals {
    fn of_file(tree: &Tree, file: NodeId) -> Self {
        Self {
            logical: tree.logical(file),
            allocated: tree.allocated(file),
            unknown: tree.unknown_allocation_files(file),
            files: u32::from(!tree.is_removed(file)),
        }
    }

    pub fn weight(&self, metric: Metric) -> u64 {
        match metric {
            Metric::Allocated => self.allocated,
            Metric::Logical => self.logical,
        }
    }
}

#[derive(Debug)]
pub struct Search {
    matcher: Matcher,
    /// Next node index to examine.
    next: usize,
    /// Position reached in the tree's change log.
    changes_seen: usize,
    /// Node → index into `totals`, or `NONE` when nothing under it matches.
    slot: Vec<u32>,
    totals: Vec<Totals>,
    matches: Vec<NodeId>,
    /// Per examined folder, [`FolderMatch`] bits for it and the folders
    /// above it; empty without `dir:` patterns.
    scope: Vec<u8>,
    /// Bumped whenever any total changes.
    version: u64,
}

impl Search {
    /// An empty overlay; call [`Search::catch_up`] to examine the tree.
    pub fn new(matcher: Matcher) -> Self {
        Self {
            matcher,
            next: 0,
            changes_seen: 0,
            slot: Vec::new(),
            totals: Vec::new(),
            matches: Vec::new(),
            scope: Vec::new(),
            version: 0,
        }
    }

    pub fn matcher(&self) -> &Matcher {
        &self.matcher
    }

    /// Examines up to `budget` new nodes and replays re-measured files.
    /// Returns `true` once every node in `tree` has been examined.
    pub fn catch_up(&mut self, tree: &Tree, budget: usize) -> bool {
        // Changes to files not yet examined are picked up when they are.
        let changes = tree.changed_files();
        let mut removed = false;
        for &raw in &changes[self.changes_seen..] {
            let file = NodeId::from_index(raw);
            if file.index() < self.next && self.slot[file.index()] != NONE {
                self.refresh(tree, file);
                removed |= tree.is_removed(file);
            }
        }
        self.changes_seen = changes.len();
        if removed {
            self.matches.retain(|&f| !tree.is_removed(f));
        }

        let end = tree.len().min(self.next.saturating_add(budget));
        self.slot.resize(end, NONE);
        let folders = self.matcher.has_folder_patterns();
        if folders {
            self.scope.resize(end, 0);
        }
        for i in self.next..end {
            let node = NodeId::from_index(i as u32);
            // A folder's ID is lower than its contents', so the folders
            // above a node were examined before it.
            let outer = match tree.parent(node) {
                Some(p) if folders => FolderMatch::from_bits(self.scope[p.index()]),
                _ => FolderMatch::default(),
            };
            if tree.is_dir(node) {
                // The scan root's name is its whole path, so it never counts.
                if folders && node != NodeId::ROOT {
                    let own = self.matcher.folder(tree.name(node));
                    self.scope[i] = own.within(outer).bits();
                }
            } else if !tree.is_removed(node)
                && self.matcher.in_scope(outer)
                && self
                    .matcher
                    .matches_file(tree.name(node), tree.logical(node))
            {
                self.add(tree, node);
            }
        }
        self.next = end;
        self.next == tree.len()
    }

    fn add(&mut self, tree: &Tree, file: NodeId) {
        let own = Totals::of_file(tree, file);
        self.matches.push(file);
        self.version += 1;
        let mut cur = Some(file);
        while let Some(n) = cur {
            let t = self.totals_mut(n);
            if n == file {
                *t = own;
            } else {
                t.logical += own.logical;
                t.allocated += own.allocated;
                t.unknown += own.unknown;
                t.files += 1;
            }
            cur = tree.parent(n);
        }
    }

    /// Re-reads a matching file's sizes and adjusts its ancestors.
    fn refresh(&mut self, tree: &Tree, file: NodeId) {
        let new = Totals::of_file(tree, file);
        let old = std::mem::replace(self.totals_mut(file), new);
        if old == new {
            return;
        }
        self.version += 1;
        let mut cur = tree.parent(file);
        while let Some(n) = cur {
            let t = self.totals_mut(n);
            t.logical = t
                .logical
                .wrapping_add(new.logical.wrapping_sub(old.logical));
            t.allocated = t
                .allocated
                .wrapping_add(new.allocated.wrapping_sub(old.allocated));
            t.unknown = t
                .unknown
                .wrapping_add(new.unknown.wrapping_sub(old.unknown));
            t.files = t.files.wrapping_add(new.files.wrapping_sub(old.files));
            cur = tree.parent(n);
        }
    }

    fn totals_mut(&mut self, node: NodeId) -> &mut Totals {
        let i = node.index();
        if self.slot[i] == NONE {
            self.slot[i] = self.totals.len() as u32;
            self.totals.push(Totals::default());
        }
        &mut self.totals[self.slot[i] as usize]
    }

    /// Matching totals at or under `node`; zero when nothing matches there.
    pub fn totals(&self, node: NodeId) -> Totals {
        match self.slot.get(node.index()) {
            Some(&s) if s != NONE => self.totals[s as usize],
            _ => Totals::default(),
        }
    }

    pub fn weight(&self, metric: Metric, node: NodeId) -> u64 {
        self.totals(node).weight(metric)
    }

    /// Matching files, in discovery order.
    pub fn matches(&self) -> &[NodeId] {
        &self.matches
    }

    /// Changes whenever any matching total changes.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Nodes examined so far.
    pub fn examined(&self) -> usize {
        self.next
    }
}

/// Matching files largest first, sorted only as far as has been asked for.
///
/// A result list shows a page at a time from the top, so sorting every
/// match on each change is wasted work: selecting the largest `n` is linear,
/// and only that prefix is sorted. Deeper pages extend the sorted prefix.
#[derive(Debug)]
pub struct Ranked {
    metric: Metric,
    version: u64,
    files: Vec<NodeId>,
    /// `files[..sorted]` is in final order; the rest are all smaller.
    sorted: usize,
}

/// Smallest prefix sorted at once, so scrolling doesn't re-select often.
const MIN_RANKED: usize = 1_000;

impl Ranked {
    pub fn new(search: &Search, metric: Metric) -> Self {
        Self {
            metric,
            version: search.version(),
            files: search.matches().to_vec(),
            sorted: 0,
        }
    }

    /// Ranks any set of files, such as those under one folder. Its
    /// currency is the caller's to track.
    pub fn from_files(files: Vec<NodeId>, metric: Metric) -> Self {
        Self {
            metric,
            version: 0,
            files,
            sorted: 0,
        }
    }

    /// Whether this ranking still reflects `search` under `metric`.
    pub fn is_current(&self, search: &Search, metric: Metric) -> bool {
        self.metric == metric && self.version == search.version()
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// The `n` largest matches in order (ties by node ID).
    pub fn top(&mut self, tree: &Tree, n: usize) -> &[NodeId] {
        let n = n.min(self.files.len());
        if n > self.sorted {
            let metric = self.metric;
            let key = |f: &NodeId| (std::cmp::Reverse(metric.weight(tree, *f)), f.index());
            let want = n
                .max(self.sorted.saturating_mul(2))
                .max(MIN_RANKED)
                .min(self.files.len());
            let rest = &mut self.files[self.sorted..];
            let k = want - self.sorted;
            if k < rest.len() {
                rest.select_nth_unstable_by_key(k - 1, key);
            }
            rest[..k].sort_unstable_by_key(key);
            self.sorted = want;
        }
        &self.files[..n]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::size::Allocated;
    use crate::tree::FileSizes;

    fn sizes(logical: u64, allocated: u64) -> FileSizes {
        FileSizes {
            logical,
            allocated: Allocated::known(allocated),
        }
    }

    fn search(query: &str, tree: &Tree) -> Search {
        let mut s = Search::new(Matcher::new(query).unwrap());
        assert!(s.catch_up(tree, usize::MAX));
        s
    }

    #[test]
    fn matches_basenames_case_insensitively() {
        let m = Matcher::new("RePort").unwrap();
        assert!(m.matches("annual report.PDF".as_ref()));
        assert!(m.matches("REPORT".as_ref()));
        assert!(!m.matches("rep ort".as_ref()));
        let m = Matcher::new(".pdf").unwrap();
        assert!(m.matches("a.PDF".as_ref()), "extension is part of the name");
        let m = Matcher::new("ÄPFEL").unwrap();
        assert!(m.matches("äpfel und birnen".as_ref()));
        let m = Matcher::new("annual report").unwrap();
        assert!(
            m.matches("2024 Annual Report.pdf".as_ref()),
            "spaces inside are kept"
        );
        assert!(Matcher::new("").is_none());
    }

    #[test]
    fn wildcards_match_the_whole_name() {
        let m = Matcher::new("*.txt").unwrap();
        assert!(m.matches("a.TXT".as_ref()));
        assert!(m.matches(".txt".as_ref()));
        assert!(!m.matches("a.txt.bak".as_ref()));
        let m = Matcher::new("IMG_????.jpg").unwrap();
        assert!(m.matches("img_0042.JPG".as_ref()));
        assert!(!m.matches("img_042.jpg".as_ref()));
        assert!(!m.matches("img_00042.jpg".as_ref()));
        let m = Matcher::new("*report*2024*").unwrap();
        assert!(m.matches("Q3 report - final 2024.xlsx".as_ref()));
        assert!(m.matches("report2024".as_ref()));
        assert!(!m.matches("2024 report".as_ref()));
        let m = Matcher::new("a*b*c").unwrap();
        assert!(m.matches("aXbYbZc".as_ref()), "backtracks past an early b");
        assert!(!m.matches("aXbYc_".as_ref()));
        assert!(Matcher::new("*").unwrap().matches("".as_ref()));
        assert!(Matcher::new("**").unwrap().matches("anything".as_ref()));
        let m = Matcher::new("Ä?fel*").unwrap();
        assert!(m.matches("äpfel.txt".as_ref()));
        assert!(!m.matches("apfel.txt".as_ref()));
        let m = Matcher::new("*.txt").unwrap();
        assert!(
            m.matches("grüße.txt".as_ref()),
            "ASCII pattern, other names"
        );
    }

    #[test]
    fn star_only_patterns_agree_with_the_general_matcher() {
        let patterns = [
            "*", "**", "a*", "*a", "a*a", "ab*ba", "*b*", "a**b", "*a*b*a*", "ä*", "*ß*", "x*ä*y",
        ];
        let alphabet = ['a', 'b', 'A', 'ä', 'ß', 'x', 'y'];
        let mut names = vec![String::new()];
        for _ in 0..4 {
            let longer: Vec<String> = names
                .iter()
                .flat_map(|n| alphabet.iter().map(move |c| format!("{n}{c}")))
                .collect();
            names.extend(longer);
        }
        for pattern in patterns {
            let fast = Matcher::new(pattern).unwrap();
            assert!(matches!(fast.include[0], Pattern::Stars { .. }));
            let lower = pattern.to_lowercase();
            let chars: Vec<char> = lower.chars().collect();
            for name in &names {
                let lowered: Vec<char> = name.to_lowercase().chars().collect();
                let expected = wildcard(&chars, &lowered, '*', '?', |p, n| p == n);
                assert_eq!(
                    fast.matches(name.as_ref()),
                    expected,
                    "{pattern:?} on {name:?}"
                );
            }
        }
    }

    #[test]
    fn several_patterns_and_exclusions() {
        let m = Matcher::new(" *.jpg ; *.png;;heic ").unwrap();
        assert!(m.matches("a.jpg".as_ref()));
        assert!(m.matches("b.PNG".as_ref()));
        assert!(
            m.matches("c.heic".as_ref()),
            "plain patterns still mean contains"
        );
        assert!(!m.matches("d.gif".as_ref()));
        assert!(!m.matches("a.jpg.part".as_ref()));

        let m = Matcher::new("*.log; !debug*").unwrap();
        assert!(m.matches("app.log".as_ref()));
        assert!(!m.matches("Debug-1.log".as_ref()));
        assert!(!m.matches("debug.txt".as_ref()));

        let m = Matcher::new("! *.tmp; !cache").unwrap();
        assert!(
            m.matches("notes.txt".as_ref()),
            "only exclusions keep the rest"
        );
        assert!(!m.matches("x.TMP".as_ref()));
        assert!(!m.matches("webcache.db".as_ref()));

        for empty in ["", " ", " ; ;", "!", "! ;"] {
            assert!(Matcher::new(empty).is_none(), "{empty:?}");
        }
        assert_eq!(Matcher::new("*.a; *.b").unwrap().query(), "*.a; *.b");
    }

    #[test]
    fn totals_cover_matching_files_and_their_ancestors_only() {
        let mut t = Tree::new("/r");
        let docs = t.add_dir(NodeId::ROOT, "docs".as_ref()).unwrap();
        let report_dir = t.add_dir(NodeId::ROOT, "report".as_ref()).unwrap();
        let a = t
            .add_file(docs, "report.pdf".as_ref(), sizes(100, 4096), None, 0)
            .unwrap();
        t.add_file(docs, "notes.txt".as_ref(), sizes(50, 4096), None, 0)
            .unwrap();
        t.add_file(report_dir, "data.csv".as_ref(), sizes(10, 4096), None, 0)
            .unwrap();
        t.add_file(NodeId::ROOT, "Report-2.pdf".as_ref(), sizes(0, 0), None, 0)
            .unwrap();

        let s = search("report", &t);
        assert_eq!(s.matches().len(), 2, "plain patterns never match folders");
        assert_eq!(
            s.totals(a),
            Totals {
                logical: 100,
                allocated: 4096,
                unknown: 0,
                files: 1
            }
        );
        assert_eq!(s.totals(docs).files, 1);
        assert_eq!(s.totals(report_dir), Totals::default());
        assert_eq!(s.totals(NodeId::ROOT).files, 2, "zero-byte matches count");
        assert_eq!(s.weight(Metric::Allocated, NodeId::ROOT), 4096);
        assert_eq!(s.weight(Metric::Logical, NodeId::ROOT), 100);
    }

    #[test]
    fn catches_up_with_new_files_in_budgeted_steps() {
        let mut t = Tree::new("/r");
        let d = t.add_dir(NodeId::ROOT, "d".as_ref()).unwrap();
        t.add_file(d, "x1".as_ref(), sizes(1, 1), None, 0).unwrap();
        let mut s = Search::new(Matcher::new("x").unwrap());
        assert!(!s.catch_up(&t, 2));
        assert!(s.catch_up(&t, 2));
        assert_eq!(s.totals(NodeId::ROOT).files, 1);

        let v = s.version();
        t.add_file(d, "x2".as_ref(), sizes(2, 2), None, 0).unwrap();
        t.add_file(d, "y".as_ref(), sizes(4, 4), None, 0).unwrap();
        assert!(s.catch_up(&t, usize::MAX));
        assert_eq!(s.totals(d).logical, 3);
        assert!(s.version() > v);
    }

    #[test]
    fn follows_remeasures_and_keeps_hard_link_ownership() {
        let mut t = Tree::new("/r");
        let d = t.add_dir(NodeId::ROOT, "d".as_ref()).unwrap();
        let owner = t
            .add_file(
                NodeId::ROOT,
                "movie.mkv".as_ref(),
                sizes(100, 4096),
                None,
                0,
            )
            .unwrap();
        let alias = t
            .add_file(d, "movie link.mkv".as_ref(), sizes(100, 4096), None, 0)
            .unwrap();
        let mut s = search("link", &t);
        assert_eq!(s.totals(d).allocated, 4096);

        // The scanner finds the alias and re-measures it.
        t.mark_alias(alias, owner);
        t.update_file(
            alias,
            FileSizes {
                logical: 300,
                allocated: Allocated::UNKNOWN,
            },
        );
        s.catch_up(&t, usize::MAX);
        let root = s.totals(NodeId::ROOT);
        assert_eq!(root.allocated, 0, "the alias's bytes stay with its owner");
        assert_eq!(root.logical, 300);
        assert_eq!(root.unknown, 1);
        assert_eq!(s.totals(d), root);
    }

    #[test]
    fn ranking_matches_a_full_sort_at_every_depth() {
        let mut t = Tree::new("/r");
        let mut x = 12345u64;
        for i in 0..5_000 {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let size = (x >> 40) % 10_000;
            t.add_file(
                NodeId::ROOT,
                format!("f{i}").as_ref(),
                sizes(size, size),
                None,
                0,
            )
            .unwrap();
        }
        let s = search("f", &t);
        let mut expected = s.matches().to_vec();
        expected.sort_unstable_by_key(|&f| (std::cmp::Reverse(t.allocated(f)), f.index()));
        let mut ranked = Ranked::new(&s, Metric::Allocated);
        assert_eq!(ranked.top(&t, 10), &expected[..10]);
        assert_eq!(ranked.top(&t, 2_500), &expected[..2_500]);
        assert_eq!(ranked.top(&t, usize::MAX), &expected[..]);
        assert!(ranked.is_current(&s, Metric::Allocated));
        assert!(!ranked.is_current(&s, Metric::Logical));
    }

    #[test]
    fn removed_files_leave_the_matches_and_totals() {
        let mut t = Tree::new("/r");
        let d = t.add_dir(NodeId::ROOT, "d".as_ref()).unwrap();
        let e = t.add_dir(d, "e".as_ref()).unwrap();
        let keep = t
            .add_file(NodeId::ROOT, "a.log".as_ref(), sizes(1, 4096), None, 0)
            .unwrap();
        let gone = t
            .add_file(d, "b.log".as_ref(), sizes(2, 8192), None, 0)
            .unwrap();
        let deeper = t
            .add_file(e, "c.log".as_ref(), sizes(4, 4096), None, 0)
            .unwrap();
        let mut s = search("log", &t);
        assert_eq!(s.totals(NodeId::ROOT).files, 3);
        let v = s.version();

        t.remove(gone);
        s.catch_up(&t, usize::MAX);
        assert_eq!(s.totals(d).files, 1);
        assert_eq!(s.totals(d).allocated, 4096);
        assert!(s.version() > v);
        assert_eq!(s.matches().len(), 2);

        t.remove(d);
        s.catch_up(&t, usize::MAX);
        assert_eq!(s.matches(), [keep]);
        assert_eq!(
            s.totals(NodeId::ROOT),
            Totals {
                logical: 1,
                allocated: 4096,
                unknown: 0,
                files: 1
            }
        );
        assert_eq!(s.totals(deeper).files, 0);

        // A search started after the removal never sees the removed files.
        let fresh = search("log", &t);
        assert_eq!(fresh.matches(), [keep]);
        assert_eq!(fresh.totals(NodeId::ROOT), s.totals(NodeId::ROOT));
    }

    #[test]
    fn changes_to_unexamined_files_are_read_when_examined() {
        let mut t = Tree::new("/r");
        let f = t
            .add_file(NodeId::ROOT, "a".as_ref(), sizes(1, 1), None, 0)
            .unwrap();
        let mut s = Search::new(Matcher::new("a").unwrap());
        s.catch_up(&t, 1); // only the root
        t.update_file(f, sizes(7, 8));
        s.catch_up(&t, usize::MAX);
        assert_eq!(
            s.totals(NodeId::ROOT),
            Totals {
                logical: 7,
                allocated: 8,
                unknown: 0,
                files: 1
            }
        );
    }

    /// Names of the matching files, sorted.
    fn names(s: &Search, t: &Tree) -> Vec<String> {
        let mut n: Vec<_> = s
            .matches()
            .iter()
            .map(|&f| t.name(f).to_string_lossy().into_owned())
            .collect();
        n.sort();
        n
    }

    #[test]
    fn dir_patterns_match_folder_names_at_any_depth() {
        let mut t = Tree::new("/r/cache");
        let src = t.add_dir(NodeId::ROOT, "src".as_ref()).unwrap();
        let nm = t.add_dir(NodeId::ROOT, "node_modules".as_ref()).unwrap();
        let deep = t.add_dir(nm, "lib".as_ref()).unwrap();
        let cache = t.add_dir(src, "Cache".as_ref()).unwrap();
        for (dir, name) in [
            (src, "app.js"),
            (src, "readme.md"),
            (nm, "index.js"),
            (deep, "util.js"),
            (deep, "notes.md"),
            (cache, "blob.bin"),
            (NodeId::ROOT, "top.js"),
        ] {
            t.add_file(dir, name.as_ref(), sizes(10, 4096), None, 0)
                .unwrap();
        }

        let s = search("dir:node_modules", &t);
        assert_eq!(names(&s, &t), ["index.js", "notes.md", "util.js"]);
        assert_eq!(s.totals(nm).files, 3);
        assert_eq!(s.totals(src), Totals::default());

        let s = search("DIR: cache", &t);
        assert_eq!(
            names(&s, &t),
            ["blob.bin"],
            "case and spaces don't matter; the scan root's own name doesn't count"
        );

        let s = search("dir:node_modules; *.js", &t);
        assert_eq!(
            names(&s, &t),
            ["index.js", "util.js"],
            "a folder pattern narrows the file patterns"
        );

        let s = search("*.js; !dir:node_modules", &t);
        assert_eq!(names(&s, &t), ["app.js", "top.js"]);

        let s = search("!dir:node*", &t);
        assert_eq!(s.matches().len(), 4, "only exclusions: everything else");

        let s = search("dir:no_such_folder", &t);
        assert!(s.matches().is_empty());
        assert!(
            Matcher::new("dir:").is_none(),
            "an empty folder pattern is no pattern"
        );
    }

    #[test]
    fn dir_patterns_apply_to_folders_added_later() {
        let mut t = Tree::new("/r");
        let nm = t.add_dir(NodeId::ROOT, "node_modules".as_ref()).unwrap();
        let mut s = search("dir:node_modules", &t);
        assert!(s.matches().is_empty());
        let inner = t.add_dir(nm, "pkg".as_ref()).unwrap();
        t.add_file(inner, "a.js".as_ref(), sizes(5, 4096), None, 0)
            .unwrap();
        t.add_file(NodeId::ROOT, "b.js".as_ref(), sizes(5, 4096), None, 0)
            .unwrap();
        assert!(s.catch_up(&t, usize::MAX));
        assert_eq!(names(&s, &t), ["a.js"]);
    }

    #[test]
    fn is_keywords_match_file_types() {
        let mut t = Tree::new("/r");
        let dl = t.add_dir(NodeId::ROOT, "Downloads".as_ref()).unwrap();
        for (dir, name, len) in [
            (dl, "film.mkv", 900),
            (dl, "photo.JPG", 50),
            (dl, "notes.pdf", 10),
            (NodeId::ROOT, "clip.mp4", 700),
            (NodeId::ROOT, "app.ts", 2),
            (NodeId::ROOT, "stream.ts", 2 << 20),
        ] {
            t.add_file(dir, name.as_ref(), sizes(len, 4096), None, 0)
                .unwrap();
        }

        let s = search("is:video", &t);
        assert_eq!(
            names(&s, &t),
            ["clip.mp4", "film.mkv", "stream.ts"],
            "a large .ts is video"
        );
        let s = search("IS:Image; is:document", &t);
        assert_eq!(names(&s, &t), ["notes.pdf", "photo.JPG"]);
        let s = search("is:video; dir:downloads", &t);
        assert_eq!(names(&s, &t), ["film.mkv"]);
        let s = search("*.pdf; is:image", &t);
        assert_eq!(
            names(&s, &t),
            ["notes.pdf", "photo.JPG"],
            "a type adds to name patterns"
        );
        let s = search("!is:video", &t);
        assert_eq!(names(&s, &t), ["app.ts", "notes.pdf", "photo.JPG"]);
        let s = search("is:vid", &t);
        assert_eq!(s.matches().len(), 3, "an unambiguous start of a type works");

        assert!(
            Matcher::parse("is:").unwrap().is_none(),
            "is: alone is no pattern yet"
        );
        let err = Matcher::parse("*.jpg; is:vidoe").unwrap_err();
        assert!(
            err.contains("isn't a file type") && err.contains("video"),
            "{err}"
        );
        let err = Matcher::parse("is:d").unwrap_err();
        assert!(err.contains("document or diskimage"), "{err}");
        assert!(Matcher::new("is:vidoe").is_none());
    }
}
