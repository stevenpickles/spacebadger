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
//! `size:`, `older:`, and `newer:` terms restrict instead: every one must
//! hold, on top of the patterns. `size:>500MB`, `size:<=1GB`, and
//! `size:10MB..1GB` compare the logical size; `older:1y` and `newer:30d`
//! (or a date, `older:2024-01-01`) compare the modified time. A leading `!`
//! turns a restriction around. Files whose modified time is unknown never
//! pass an age term, turned around or not.
//!
//! `git:repo`, `git:worktree`, and `git:metadata` restrict by where a file
//! is as far as Git goes (see [`GitClass`]): `repo` is anything in a
//! detected repository or linked worktree, its Git metadata included;
//! `worktree` a linked worktree's checkout; `metadata` Git's own data. The
//! positive ones form a union, and `!git:` leaves a kind out; together they
//! restrict on top of every other term, like `dir:`. Size and age terms stay
//! about each file, never about a repository as a whole.
//!
//! [`Search`] keeps its own totals for matching files and every folder above
//! them, so the treemap can be drawn from matching bytes only. It catches up
//! incrementally: new nodes are examined in ID order (IDs only grow), and
//! files re-measured after being added are replayed from
//! [`Tree::changed_files`], which also lists removed files; those leave the
//! matches. When the query depends on size, a re-measured file is matched
//! again and can join or leave the matches. Hard-link ownership is kept: an
//! alias that matches contributes its logical length but no allocation,
//! exactly as in the tree.
//!
//! Git repositories can be found after the files in them were examined.
//! With `git:` terms, the nodes logged in [`GitIndex::changes`] are walked
//! again: a folder whose class changed passes the change on to its examined
//! contents, a step at a time within the same budget, and files join or
//! leave the matches as they go, so totals stay whole after every step.
//! Queries without `git:` terms never look at Git entries.
//!
//! [`GitIndex::changes`]: crate::git::GitIndex::changes

use crate::filetype::FileType;
use crate::git::GitClass;
use crate::layout::Metric;
use crate::tree::{NodeId, Tree};
use std::collections::HashSet;
use std::ffi::OsStr;
use std::time::{SystemTime, UNIX_EPOCH};

const NONE: u32 = u32::MAX;

/// Bytes in a KB, as the interface shows sizes: Windows Explorer counts
/// 1,024, macOS Finder and Linux file managers 1,000. A filter typed from
/// what's on screen then means what it says.
const KILO: u64 = if cfg!(windows) { 1024 } else { 1000 };

/// Case-insensitive matcher for a query such as `*.jpg; *.png; !thumb*` or
/// `*.js; !dir:node_modules`.
///
/// A file matches when its name matches any included file pattern (or there
/// are none) and no excluded one, it's inside a folder matching an included
/// `dir:` pattern (or there are none), inside no folder matching an
/// excluded one, its size and modified time pass every `size:`, `older:`,
/// and `newer:` term, and where it is in Git passes the `git:` terms.
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
    /// `size:` terms, in bytes of logical size.
    sizes: Vec<Range>,
    /// `older:` and `newer:` terms, in seconds since the Unix epoch.
    ages: Vec<Range>,
    /// `git:` terms.
    git: GitSet,
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
    /// `None` when the query has no patterns; an unknown `is:` type or a
    /// malformed term counts as none. Use [`Matcher::parse`] to report it.
    pub fn new(query: &str) -> Option<Self> {
        Self::parse(query).ok().flatten()
    }

    /// `Ok(None)` when the query has no patterns. Patterns are separated by
    /// `;` and trimmed; a leading `!` excludes, a `dir:` prefix (any case)
    /// makes a folder pattern, `is:` names a file type, and `size:`,
    /// `older:`, and `newer:` restrict by size and age, counted from now,
    /// and `git:` by Git class. Fails on an unknown or ambiguous type, an
    /// unknown `git:` kind, or a malformed size or age.
    ///
    /// Dates such as `older:2024-01-01` start at midnight UTC here; the app
    /// uses [`Matcher::parse_at`] to start them at local midnight instead.
    pub fn parse(query: &str) -> Result<Option<Self>, String> {
        Self::parse_at(query, SystemTime::now(), &utc_midnight)
    }

    /// [`Matcher::parse`] with ages counted back from `now`, and dates
    /// starting at the moment `midnight(year, month, day)` gives, in seconds
    /// since the Unix epoch. It's only asked about real dates; `None` makes
    /// the date an error.
    pub fn parse_at(
        query: &str,
        now: SystemTime,
        midnight: &dyn Fn(i32, u32, u32) -> Option<i64>,
    ) -> Result<Option<Self>, String> {
        let now = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let mut include = Vec::new();
        let mut exclude = Vec::new();
        let mut dir_include = Vec::new();
        let mut dir_exclude = Vec::new();
        let mut types = TypeSet::default();
        let mut sizes = Vec::new();
        let mut ages = Vec::new();
        let mut git = GitSet::default();
        for term in query.split(';') {
            let term = term.trim();
            let (excluded, term) = match term.strip_prefix('!') {
                Some(rest) => (true, rest.trim_start()),
                None => (false, term),
            };
            if let Some(value) = strip_prefix_ignore_case(term, "size:") {
                // A term still being typed, such as "size:>", is no term yet.
                if let Some(range) = size_range(value.trim())? {
                    sizes.push(range.negated(excluded));
                }
                continue;
            }
            let age =
                [("older:", true), ("newer:", false)]
                    .into_iter()
                    .find_map(|(prefix, older)| {
                        strip_prefix_ignore_case(term, prefix).map(|value| (value, older))
                    });
            if let Some((value, older)) = age {
                if let Some(range) = age_range(value.trim(), older, now, midnight)? {
                    ages.push(range.negated(excluded));
                }
                continue;
            }
            if let Some(word) = strip_prefix_ignore_case(term, "git:") {
                let word = word.trim();
                // "git:" alone, still being typed, is no term yet.
                if !word.is_empty() {
                    let bit = GitSet::from_keyword(word)?;
                    if excluded {
                        git.exclude |= bit;
                    } else {
                        git.include |= bit;
                    }
                }
                continue;
            }
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
        if all().next().is_none()
            && types.is_empty()
            && sizes.is_empty()
            && ages.is_empty()
            && git == GitSet::default()
        {
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
            sizes,
            ages,
            git,
            ascii,
            globs,
        }))
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    /// Whether a file name passes the file patterns, taking its type from
    /// the name alone, as an empty file of unknown age. See
    /// [`Matcher::matches_file`].
    pub fn matches(&self, name: &OsStr) -> bool {
        self.matches_file(name, 0, None)
    }

    /// Whether a file passes the file patterns, `is:` types, and the size
    /// and age terms. `len` is its logical size, which also settles `.ts`
    /// (see [`FileType::of`]), and `modified` its last-modified time in
    /// seconds since the Unix epoch. The folder patterns are applied by
    /// [`Search`], which knows where the file is.
    pub fn matches_file(&self, name: &OsStr, len: u64, modified: Option<u32>) -> bool {
        if !self.sizes.iter().all(|r| r.holds(len)) {
            return false;
        }
        if !self.ages.is_empty() {
            let Some(modified) = modified else {
                return false;
            };
            if !self.ages.iter().all(|r| r.holds(u64::from(modified))) {
                return false;
            }
        }
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

    /// Whether a file can start or stop matching when its size changes:
    /// the query has `size:` terms, or `is:` types, since a `.ts` file's
    /// type depends on its size.
    pub fn depends_on_size(&self) -> bool {
        !self.sizes.is_empty() || !self.types.is_empty()
    }

    /// The query has `dir:` patterns.
    pub fn has_folder_patterns(&self) -> bool {
        !self.dir_include.is_empty() || !self.dir_exclude.is_empty()
    }

    /// The query has `git:` terms, so what matches depends on the Git
    /// entries found.
    pub fn depends_on_git(&self) -> bool {
        self.git != GitSet::default()
    }

    /// Whether a file of Git class `class` passes the `git:` terms: an
    /// included kind holds (or none is included) and no excluded one does.
    pub fn git_allows(&self, class: GitClass) -> bool {
        let holds = GitSet::holding(class);
        (self.git.include == 0 || self.git.include & holds != 0) && self.git.exclude & holds == 0
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

/// Included and excluded `git:` kinds, one bit each.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct GitSet {
    include: u8,
    exclude: u8,
}

impl GitSet {
    /// In a repository or linked worktree, Git metadata included.
    const REPO: u8 = 1;
    /// In a linked worktree's checkout.
    const WORKTREE: u8 = 2;
    /// Git's own data.
    const METADATA: u8 = 4;

    /// The kind a `git:` value names, ignoring case. Any start of a keyword
    /// works (`r`, `work`, `meta`).
    fn from_keyword(word: &str) -> Result<u8, String> {
        const KEYWORDS: [(&str, u8); 5] = [
            ("repository", GitSet::REPO),
            ("repositories", GitSet::REPO),
            ("worktree", GitSet::WORKTREE),
            ("worktrees", GitSet::WORKTREE),
            ("metadata", GitSet::METADATA),
        ];
        let lower = word.to_ascii_lowercase();
        KEYWORDS
            .iter()
            .find(|(keyword, _)| keyword.starts_with(&lower))
            .map(|&(_, bit)| bit)
            .ok_or_else(|| {
                format!(
                    "“git:{word}” isn't a Git filter. Use git:repo, git:worktree, or git:metadata."
                )
            })
    }

    /// The kinds content of `class` counts as.
    fn holding(class: GitClass) -> u8 {
        match class {
            GitClass::Other => 0,
            GitClass::Checkout => Self::REPO,
            GitClass::Worktree => Self::REPO | Self::WORKTREE,
            GitClass::Metadata => Self::REPO | Self::METADATA,
        }
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

/// An inclusive range a file's size or modified time must fall in, or
/// outside of when negated. `lo > hi` holds for nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Range {
    lo: u64,
    hi: u64,
    negated: bool,
}

impl Range {
    fn new(lo: u64, hi: u64) -> Self {
        Self {
            lo,
            hi,
            negated: false,
        }
    }

    /// Values below `limit`; nothing when it's 0.
    fn below(limit: u64) -> Self {
        match limit.checked_sub(1) {
            Some(hi) => Self::new(0, hi),
            None => Self::new(1, 0),
        }
    }

    fn negated(self, negated: bool) -> Self {
        Self { negated, ..self }
    }

    fn holds(&self, value: u64) -> bool {
        (self.lo <= value && value <= self.hi) != self.negated
    }
}

/// The range a `size:` value describes: `>N`, `>=N`, `<N`, `<=N`, or
/// `N..M` (inclusive). `None` while it's still being typed.
fn size_range(value: &str) -> Result<Option<Range>, String> {
    if value.is_empty() {
        return Ok(None);
    }
    if let Some((from, to)) = value.split_once("..") {
        let (from, to) = (from.trim(), to.trim());
        let lo = if from.is_empty() {
            0.0
        } else {
            bytes(from, value)?
        };
        if to.is_empty() {
            return Ok(None);
        }
        let hi = bytes(to, value)?;
        if lo > hi {
            return Err(format!(
                "“size:{value}” goes from larger to smaller. Put the smaller size first."
            ));
        }
        return Ok(Some(Range::new(whole(lo.ceil()), whole(hi.floor()))));
    }
    let (op, number) = [">=", "<=", ">", "<"]
        .into_iter()
        .find_map(|op| value.strip_prefix(op).map(|rest| (op, rest.trim())))
        .ok_or_else(|| {
            // "size:=1GB" reads as "size:>1GB", not "size:>=1GB", in the hint.
            let size = value.trim_start_matches('=').trim_start();
            format!(
                "“size:{value}” needs a comparison: size:>{size}, size:<{size}, \
                 or a range like size:10MB..1GB."
            )
        })?;
    if number.is_empty() {
        return Ok(None);
    }
    // Fractional bounds round toward what the comparison means: on Windows
    // 1.3KB is 1,331.2 bytes, so <1.3KB keeps 1,331 bytes and >1.3KB starts
    // at 1,332.
    let n = bytes(number, value)?;
    Ok(Some(match op {
        ">" => Range::new(whole(n.floor()).saturating_add(1), u64::MAX),
        ">=" => Range::new(whole(n.ceil()), u64::MAX),
        "<" => Range::below(whole(n.ceil())),
        _ => Range::new(0, whole(n.floor())),
    }))
}

/// A whole, non-negative number of bytes as `u64`; the conversion
/// saturates, so an absurd size simply matches nothing.
fn whole(n: f64) -> u64 {
    n as u64
}

/// A size such as `500MB`, `1.5 GB`, or `4096` (bytes), possibly
/// fractional. `value` is the whole term, for the error message.
fn bytes(text: &str, value: &str) -> Result<f64, String> {
    let (number, unit) = split_number(text);
    let multiplier = match unit.to_ascii_lowercase().as_str() {
        "" | "b" => Some(1),
        "k" | "kb" => Some(KILO),
        "m" | "mb" => Some(KILO.pow(2)),
        "g" | "gb" => Some(KILO.pow(3)),
        "t" | "tb" => Some(KILO.pow(4)),
        "kib" => Some(1 << 10),
        "mib" => Some(1 << 20),
        "gib" => Some(1 << 30),
        "tib" => Some(1 << 40),
        _ => None,
    };
    match (number.parse::<f64>(), multiplier) {
        (Ok(n), Some(m)) if n.is_finite() && n >= 0.0 => Ok(n * m as f64),
        _ => Err(format!(
            "“size:{value}” isn't a size: try size:>500MB or size:>1.5GB (B, KB, MB, GB, TB)."
        )),
    }
}

/// Splits `1.5GB` into `1.5` and `GB`, trimming between them.
fn split_number(text: &str) -> (&str, &str) {
    let end = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    (&text[..end], text[end..].trim())
}

const DAY: u64 = 86_400;

/// The range of modified times an `older:` or `newer:` value describes, in
/// seconds since the Unix epoch, counted back from `now`: a duration such
/// as `30d`, `6mo`, or `1y` (a month is 30 days, a year 365), or a date
/// such as `2024-01-01`, starting when `midnight` says. Older is strictly
/// before that moment; newer is at or after it. `None` while it's still
/// being typed, including a date such as `2024-06-`.
///
/// The tree keeps times before 1970 as 1 (see
/// [`crate::tree::epoch_seconds`]), so moments before 1970 aren't told
/// apart: `older:1960-01-01` matches every file from before 1970.
fn age_range(
    value: &str,
    older: bool,
    now: u64,
    midnight: &dyn Fn(i32, u32, u32) -> Option<i64>,
) -> Result<Option<Range>, String> {
    if value.is_empty() {
        return Ok(None);
    }
    let keyword = if older { "older" } else { "newer" };
    let moment = if let Some(day) = date(value) {
        let secs = day
            .and_then(|(y, m, d)| midnight(y, m, d))
            .ok_or_else(|| format!("“{keyword}:{value}” isn't a real date."))?;
        u64::try_from(secs).unwrap_or(0)
    } else if date_prefix(value) {
        return Ok(None);
    } else {
        let (number, unit) = split_number(value);
        let unit_secs = match unit.to_ascii_lowercase().as_str() {
            "h" => Some(3_600),
            "d" => Some(DAY),
            "w" => Some(7 * DAY),
            "mo" => Some(30 * DAY),
            "y" => Some(365 * DAY),
            "m" => {
                return Err(format!(
                    "“{keyword}:{value}”: use mo for months, like {keyword}:{number}mo."
                ));
            }
            _ => None,
        };
        match (number.parse::<f64>(), unit_secs) {
            (Ok(n), Some(unit)) if n.is_finite() && n >= 0.0 => {
                now.saturating_sub((n * unit as f64) as u64)
            }
            _ => {
                return Err(format!(
                    "“{keyword}:{value}” isn't an age: try {keyword}:30d, {keyword}:6mo, \
                     {keyword}:1y, or {keyword}:2024-01-01."
                ));
            }
        }
    };
    Ok(Some(if older {
        // Stored pre-1970 times (1) are older than any moment from 1970 on.
        Range::below(moment.max(2))
    } else {
        Range::new(moment, u64::MAX)
    }))
}

/// The year, month, and day of a `YYYY-MM-DD` date. `None` when `text`
/// isn't shaped like a date; `Some(None)` when it is but the date doesn't
/// exist.
fn date(text: &str) -> Option<Option<(i32, u32, u32)>> {
    let mut parts = text.split('-');
    let (y, m, d) = (parts.next()?, parts.next()?, parts.next()?);
    let digits = |s: &str, len: std::ops::RangeInclusive<usize>| {
        len.contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit())
    };
    if parts.next().is_some() || !digits(y, 4..=4) || !digits(m, 1..=2) || !digits(d, 1..=2) {
        return None;
    }
    let (y, m, d): (i32, u32, u32) = (y.parse().ok()?, m.parse().ok()?, d.parse().ok()?);
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let days_in_month = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return Some(None),
    };
    Some((1..=days_in_month).contains(&d).then_some((y, m, d)))
}

/// Whether `text` is the start of a `YYYY-MM-DD` date still being typed:
/// `2024`, `2024-`, `2024-0`, `2024-06`, or `2024-06-`.
fn date_prefix(text: &str) -> bool {
    let digits = |s: &str, len: std::ops::RangeInclusive<usize>| {
        len.contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit())
    };
    let parts: Vec<&str> = text.split('-').collect();
    match parts[..] {
        [y] => digits(y, 4..=4),
        [y, m] => digits(y, 4..=4) && digits(m, 0..=2),
        [y, m, ""] => digits(y, 4..=4) && digits(m, 1..=2),
        _ => false,
    }
}

/// Midnight UTC of a date, in seconds since the Unix epoch (negative
/// before 1970). The resolver [`Matcher::parse`] uses.
pub fn utc_midnight(year: i32, month: u32, day: u32) -> Option<i64> {
    // Days since 1970-01-01, after Howard Hinnant's days_from_civil: years
    // start in March, so the leap day falls at the end of one.
    let y = i64::from(year) - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let year_of_era = y - era * 400;
    let day_of_year = (153 * i64::from((month + 9) % 12) + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    Some(days * DAY as i64)
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
    /// Per examined folder, its [`GitClass`] as last worked out; empty
    /// without `git:` terms.
    git: Vec<u8>,
    /// Position reached in the tree's Git change log.
    git_seen: usize,
    /// Examined nodes whose Git class may have changed, to look at again
    /// before examining new ones; the next on top.
    rewalk: Vec<Rewalk>,
    /// Bumped whenever any total changes.
    version: u64,
}

/// What [`Search::catch_up`] has left to look at again after a Git entry
/// changed.
#[derive(Debug, Clone, Copy)]
enum Rewalk {
    /// One node.
    Node(NodeId),
    /// A node and the siblings after it, so a folder of a million files
    /// costs one entry rather than a million.
    From(NodeId),
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
            git: Vec::new(),
            git_seen: 0,
            rewalk: Vec::new(),
            version: 0,
        }
    }

    pub fn matcher(&self) -> &Matcher {
        &self.matcher
    }

    /// Examines up to `budget` new nodes and replays re-measured files.
    /// With `git:` terms, nodes a changed Git entry may have reclassified
    /// are looked at again first, out of the same budget, and new nodes
    /// wait until that's done. Returns `true` once every node in `tree` has
    /// been examined and none is left to look at again.
    pub fn catch_up(&mut self, tree: &Tree, budget: usize) -> bool {
        let changes = tree.changed_files();
        let recheck = self.matcher.depends_on_size();
        // Files that stopped matching during this call: still listed in
        // `matches` until it ends, so one matching again is restored there
        // rather than listed twice.
        let mut left = HashSet::new();
        for &raw in &changes[self.changes_seen..] {
            let file = NodeId::from_index(raw);
            // Changes to files not yet examined are picked up when they are.
            if file.index() >= self.next {
                continue;
            }
            // Removed files keep their slot but count no files.
            let matched = self.totals(file).files != 0;
            // Every entry for a file is judged on the tree as it is now, so
            // repeats within one batch agree and never add a file twice.
            let matches = if recheck {
                self.file_matches(tree, file)
            } else {
                matched && !tree.is_removed(file)
            };
            if matched {
                let new = if matches {
                    Totals::of_file(tree, file)
                } else {
                    Totals::default()
                };
                self.replace(tree, file, new);
                if !matches {
                    left.insert(file);
                }
            } else if matches {
                self.join(tree, file, &mut left);
            }
        }
        self.changes_seen = changes.len();

        let git = self.matcher.depends_on_git();
        let mut spent = 0;
        if git {
            let log = tree.git().changes();
            // Entries of nodes not yet examined are read when they are.
            for &raw in &log[self.git_seen..] {
                if (raw as usize) < self.next {
                    self.rewalk.push(Rewalk::Node(NodeId::from_index(raw)));
                }
            }
            self.git_seen = log.len();
            spent = self.rewalk(tree, budget, &mut left);
        }
        if !left.is_empty() {
            let (slot, totals) = (&self.slot, &self.totals);
            self.matches.retain(|f| match slot[f.index()] {
                NONE => false,
                s => totals[s as usize].files != 0,
            });
        }
        // New nodes take their folder's class, so it must be current first.
        if !self.rewalk.is_empty() {
            return false;
        }

        let end = tree
            .len()
            .min(self.next.saturating_add(budget.saturating_sub(spent)));
        self.slot.resize(end, NONE);
        let folders = self.matcher.has_folder_patterns();
        if folders {
            self.scope.resize(end, 0);
        }
        if git {
            self.git.resize(end, 0);
        }
        for i in self.next..end {
            let node = NodeId::from_index(i as u32);
            if tree.is_dir(node) {
                // The scan root's name is its whole path, so it never counts.
                if folders && node != NodeId::ROOT {
                    let own = self.matcher.folder(tree.name(node));
                    self.scope[i] = own.within(self.outer(tree, node)).bits();
                }
                if git {
                    self.git[i] = self.class(tree, node) as u8;
                }
            } else if self.file_matches(tree, node) {
                self.add(tree, node);
            }
        }
        self.next = end;
        self.next == tree.len()
    }

    /// Looks again at up to `budget` nodes whose Git class may have
    /// changed, and returns how many it looked at. A folder whose class
    /// changed has its contents looked at in turn; one whose class didn't
    /// leaves them be, since nothing below it changed but entries of their
    /// own, which the change log brings separately. A file joins or leaves
    /// the matches, adjusting the totals above it, so every step leaves
    /// them whole. A node can be looked at before a folder above it whose
    /// class changed too, and then again once that folder's change reaches
    /// it; files that leave go in `left`, as in [`Search::catch_up`].
    fn rewalk(&mut self, tree: &Tree, budget: usize, left: &mut HashSet<NodeId>) -> usize {
        let mut spent = 0;
        while spent < budget {
            let Some(item) = self.rewalk.pop() else {
                break;
            };
            spent += 1;
            let node = match item {
                Rewalk::Node(n) => n,
                Rewalk::From(n) => {
                    // A removed node keeps its link to the next sibling.
                    if let Some(next) = tree.next_sibling(n) {
                        self.rewalk.push(Rewalk::From(next));
                    }
                    n
                }
            };
            // Added after the last examined node, it's examined later with
            // its class as it is then.
            if node.index() >= self.next || tree.is_removed(node) {
                continue;
            }
            if tree.is_dir(node) {
                let class = self.class(tree, node) as u8;
                if std::mem::replace(&mut self.git[node.index()], class) != class
                    && let Some(first) = tree.first_child(node)
                {
                    self.rewalk.push(Rewalk::From(first));
                }
            } else {
                let matched = self.totals(node).files != 0;
                let matches = self.file_matches(tree, node);
                if matched && !matches {
                    self.replace(tree, node, Totals::default());
                    left.insert(node);
                } else if matches && !matched {
                    self.join(tree, node, left);
                }
            }
        }
        spent
    }

    /// Makes a file that doesn't match now match: restored in place if it
    /// left during this call (it's still in `matches`), else added.
    fn join(&mut self, tree: &Tree, file: NodeId, left: &mut HashSet<NodeId>) {
        if left.remove(&file) {
            self.replace(tree, file, Totals::of_file(tree, file));
        } else {
            self.add(tree, file);
        }
    }

    /// The Git class an examined node has: its own entry's, or else that of
    /// the folder it's in, which has a lower ID and so was examined first;
    /// for the scan root, that of the repository above the scan, if any.
    fn class(&self, tree: &Tree, node: NodeId) -> GitClass {
        tree.git_own(node)
            .unwrap_or_else(|| match tree.parent(node) {
                Some(p) => GitClass::from_u8(self.git[p.index()]),
                None => tree
                    .git()
                    .above()
                    .and_then(|(_, r)| r.kind.class())
                    .unwrap_or_default(),
            })
    }

    /// How the folders above an examined node meet the `dir:` patterns. A
    /// folder's ID is lower than its contents', so they were examined first.
    fn outer(&self, tree: &Tree, node: NodeId) -> FolderMatch {
        match tree.parent(node) {
            Some(p) if !self.scope.is_empty() => FolderMatch::from_bits(self.scope[p.index()]),
            _ => FolderMatch::default(),
        }
    }

    /// Whether a file in the examined part of the tree matches as it is now.
    fn file_matches(&self, tree: &Tree, file: NodeId) -> bool {
        !tree.is_removed(file)
            && self.matcher.in_scope(self.outer(tree, file))
            && (!self.matcher.depends_on_git() || self.matcher.git_allows(self.class(tree, file)))
            && self
                .matcher
                .matches_file(tree.name(file), tree.logical(file), tree.modified(file))
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

    /// Replaces a matching file's totals, re-read or zero when it no longer
    /// matches, and adjusts its ancestors by the difference.
    fn replace(&mut self, tree: &Tree, file: NodeId, new: Totals) {
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

    /// About how much [`Search::catch_up`] has left to do on `tree`, zero
    /// once it's done: nodes not yet examined plus change-log entries not
    /// yet replayed, some of which it skips (files not yet examined,
    /// repeats). With `git:` terms, also Git entries changed since and
    /// nodes waiting to be looked at again, where a folder's contents
    /// count as one until they're reached.
    pub fn backlog(&self, tree: &Tree) -> usize {
        let git = if self.matcher.depends_on_git() {
            tree.git().changes().len().saturating_sub(self.git_seen) + self.rewalk.len()
        } else {
            0
        };
        tree.len().saturating_sub(self.next)
            + tree.changed_files().len().saturating_sub(self.changes_seen)
            + git
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
        assert_eq!(s.backlog(&t), 2);
        s.catch_up(&t, 1); // only the root
        t.update_file(f, sizes(7, 8));
        assert_eq!(s.backlog(&t), 2, "the file, and its change");
        s.catch_up(&t, usize::MAX);
        assert_eq!(s.backlog(&t), 0);
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

    #[test]
    fn size_terms_compare_logical_sizes() {
        let k = KILO;
        let m = |q: &str, len: u64| {
            Matcher::new(q)
                .unwrap()
                .matches_file("a".as_ref(), len, None)
        };
        assert!(m("size:>500MB", 500 * k * k + 1));
        assert!(!m("size:>500MB", 500 * k * k));
        assert!(m("size:>=500mb", 500 * k * k));
        assert!(m("SIZE: < 10KB", 10 * k - 1));
        assert!(!m("size:<10KB", 10 * k));
        assert!(m("size:<=10KB", 10 * k));
        assert!(m("size:>1.5GB", 3 * k * k * k / 2 + 1));
        assert!(!m("size:>1.5GB", 3 * k * k * k / 2));
        assert!(m("size:>100", 101), "a bare number is bytes");
        assert!(m("size:>1 KiB", 1025));
        assert!(!m("size:<0", 0), "nothing is smaller than nothing");
        for (len, expected) in [
            (10 * k * k - 1, false),
            (10 * k * k, true),
            (k * k * k, true),
            (k * k * k + 1, false),
        ] {
            assert_eq!(m("size:10MB..1GB", len), expected, "{len}");
        }
        assert!(m("size:..1KB", 0), "an open start is 0");

        // Fractional bounds round the way each comparison means.
        let x = 1.3 * k as f64;
        let (floor, ceil) = (x.floor() as u64, x.ceil() as u64);
        assert!(m("size:<1.3KB", ceil - 1) && !m("size:<1.3KB", ceil));
        assert!(m("size:<=1.3KB", floor) && !m("size:<=1.3KB", floor + 1));
        assert!(!m("size:>1.3KB", floor) && m("size:>1.3KB", floor + 1));
        assert!(m("size:>=1.3KB", ceil) && !m("size:>=1.3KB", ceil - 1));
        let y = 1.7 * k as f64;
        assert!(m("size:1.3KB..1.7KB", ceil) && !m("size:1.3KB..1.7KB", ceil - 1));
        assert!(m("size:1.3KB..1.7KB", y.floor() as u64));
        assert!(!m("size:1.3KB..1.7KB", y.floor() as u64 + 1));
        assert!(
            !m("size:0.2..0.8", 0) && !m("size:0.2..0.8", 1),
            "no whole byte"
        );

        assert!(!m("!size:>1GB", 2 * k * k * k), "! turns a term around");
        assert!(m("!size:>1GB", k));
        assert!(
            !m("size:>1KB; size:<1MB", 2 * k * k),
            "every size term must hold"
        );
        assert!(m("size:>1KB; size:<1MB", 2 * k));
    }

    #[test]
    fn malformed_sizes_and_ages_are_reported() {
        for (query, words) in [
            ("size:500MB", "needs a comparison"),
            ("size:>abc", "isn't a size"),
            ("size:>5PB", "isn't a size"),
            ("size:>-1", "isn't a size"),
            ("size:1GB..10MB", "larger to smaller"),
            ("size:abc..", "isn't a size"),
            ("older:30", "isn't an age"),
            ("newer:soon", "isn't an age"),
            ("older:6m", "use mo for months"),
            ("older:2023-02-29", "isn't a real date"),
            ("newer:2024-13-01", "isn't a real date"),
        ] {
            let err = Matcher::parse(query).unwrap_err();
            assert!(err.contains(words), "{query}: {err}");
            assert!(err.contains(query.split_once(':').unwrap().1), "{err}");
        }
        for (query, hint) in [("size:==1GB", "size:>1GB,"), ("size:=1GB", "size:<1GB,")] {
            let err = Matcher::parse(query).unwrap_err();
            assert!(err.contains(hint) && !err.contains(">="), "{err}");
        }
        for typing in [
            "size:",
            "size:>",
            "size:>=",
            "size:10MB..",
            "older:",
            "!newer: ",
            "older:2024",
            "older:2024-",
            "older:2024-0",
            "newer:2024-06",
            "newer:2024-06-",
        ] {
            assert!(Matcher::parse(typing).unwrap().is_none(), "{typing:?}");
        }
    }

    #[test]
    fn age_terms_compare_modified_times() {
        let now = UNIX_EPOCH + std::time::Duration::from_secs(1_800_000_000);
        let m = |q: &str, age_days: Option<u32>| {
            let modified = age_days.map(|d| 1_800_000_000 - d * 86_400);
            Matcher::parse_at(q, now, &utc_midnight)
                .unwrap()
                .unwrap()
                .matches_file("a".as_ref(), 0, modified)
        };
        assert!(m("older:1y", Some(366)));
        assert!(!m("older:1y", Some(364)));
        assert!(m("newer:30d", Some(29)));
        assert!(m("newer:30d", Some(30)), "newer includes the moment itself");
        assert!(!m("newer:30d", Some(31)));
        assert!(m("older:6MO", Some(181)));
        assert!(!m("older:6mo", Some(179)));
        assert!(m("newer:2w", Some(13)));
        assert!(m("newer:48h", Some(1)));
        assert!(!m("newer:48h", Some(3)));
        assert!(m("older:1y; newer:2y", Some(400)), "every age term holds");
        assert!(!m("older:1y; newer:2y", Some(800)));
        assert!(m("!older:1y", Some(10)));
        assert!(!m("!older:1y", Some(400)));
        for q in ["older:1y", "newer:1y", "!older:1y", "!newer:30d"] {
            assert!(!m(q, None), "{q}: an unknown time never passes");
        }

        // 2024-01-01T00:00:00Z.
        let m = Matcher::new("older:2024-01-01").unwrap();
        assert!(m.matches_file("a".as_ref(), 0, Some(1_704_067_199)));
        assert!(!m.matches_file("a".as_ref(), 0, Some(1_704_067_200)));
        let m = Matcher::new("newer:2024-06-30").unwrap();
        assert!(m.matches_file("a".as_ref(), 0, Some(1_719_705_600)));
        assert!(!m.matches_file("a".as_ref(), 0, Some(1_719_705_599)));
        assert_eq!(date("2024-2-29"), Some(Some((2024, 2, 29))));
        assert_eq!(date("2023-02-29"), Some(None));
        assert_eq!(date("30d"), None);
        assert_eq!(utc_midnight(1970, 1, 1), Some(0));
        assert_eq!(utc_midnight(2000, 3, 1), Some(951_868_800));
        assert_eq!(utc_midnight(2024, 2, 29), Some(1_709_164_800));
        assert_eq!(utc_midnight(1969, 12, 31), Some(-86_400));
    }

    #[test]
    fn dates_start_when_the_resolver_says() {
        // A resolver for a zone eight hours behind UTC.
        let pacific = |y, m, d| utc_midnight(y, m, d).map(|s| s + 8 * 3_600);
        let now = SystemTime::now();
        let m = Matcher::parse_at("newer:2024-06-30", now, &pacific)
            .unwrap()
            .unwrap();
        let utc_midnight_30th = 1_719_705_600;
        assert!(!m.matches_file("a".as_ref(), 0, Some(utc_midnight_30th)));
        assert!(m.matches_file("a".as_ref(), 0, Some(utc_midnight_30th + 8 * 3_600)));
        let none = |_, _, _| None;
        let err = Matcher::parse_at("older:2024-03-10", now, &none).unwrap_err();
        assert!(err.contains("isn't a real date"), "{err}");
    }

    #[test]
    fn times_before_1970_are_not_told_apart() {
        // The tree stores any time before 1970 as 1.
        let m = |q: &str, modified| {
            Matcher::new(q)
                .unwrap()
                .matches_file("a".as_ref(), 0, Some(modified))
        };
        assert!(m("older:1970-01-02", 1));
        assert!(
            m("older:1960-01-01", 1),
            "a date before 1970 still finds them"
        );
        assert!(!m("older:1960-01-01", 86_400));
        assert!(m("older:200y", 1));
        assert!(m("newer:1960-01-01", 1));
        assert!(m("older:2200-01-01", u32::MAX), "nor after 2106");
    }

    #[test]
    fn size_and_age_terms_combine_with_patterns() {
        let now = SystemTime::now();
        let ago = |days: u64| Some(now - std::time::Duration::from_secs(days * 86_400));
        let mut t = Tree::new("/r");
        let dl = t.add_dir(NodeId::ROOT, "Downloads".as_ref()).unwrap();
        for (dir, name, len, modified) in [
            (dl, "old.iso", 4 << 30, ago(800)),
            (dl, "new.iso", 4 << 30, ago(2)),
            (dl, "small.iso", 1 << 20, ago(800)),
            (dl, "undated.iso", 4 << 30, None),
            (NodeId::ROOT, "old.mkv", 2 << 30, ago(900)),
            (NodeId::ROOT, "notes.txt", 10, ago(900)),
        ] {
            t.add_file(dir, name.as_ref(), sizes(len, len), modified, 0)
                .unwrap();
        }

        let s = search("size:>1GB", &t);
        assert_eq!(
            names(&s, &t),
            ["new.iso", "old.iso", "old.mkv", "undated.iso"],
            "a restriction alone matches every file that passes it"
        );
        let s = search("size:>1GB; older:1y", &t);
        assert_eq!(names(&s, &t), ["old.iso", "old.mkv"]);
        let s = search("*.iso; older:1y", &t);
        assert_eq!(names(&s, &t), ["old.iso", "small.iso"]);
        let s = search("is:video; *.iso; size:>1GB; !older:1y", &t);
        assert_eq!(names(&s, &t), ["new.iso"]);
        let s = search("dir:downloads; newer:1w", &t);
        assert_eq!(names(&s, &t), ["new.iso"]);
        assert_eq!(s.totals(dl).logical, 4 << 30);
    }

    #[test]
    fn remeasured_files_join_and_leave_size_matches() {
        let mut t = Tree::new("/r");
        let d = t.add_dir(NodeId::ROOT, "d".as_ref()).unwrap();
        let grows = t
            .add_file(d, "grows.bin".as_ref(), sizes(100, 4096), None, 0)
            .unwrap();
        let shrinks = t
            .add_file(d, "shrinks.bin".as_ref(), sizes(5000, 8192), None, 0)
            .unwrap();
        let mut s = search("size:>1000", &t);
        assert_eq!(s.matches(), [shrinks]);
        let v = s.version();

        t.update_file(grows, sizes(2000, 4096));
        t.update_file(shrinks, sizes(10, 4096));
        t.update_file(grows, sizes(3000, 4096));
        s.catch_up(&t, usize::MAX);
        assert_eq!(s.matches(), [grows], "each file counted once");
        assert!(s.version() > v);
        assert_eq!(
            s.totals(NodeId::ROOT),
            Totals {
                logical: 3000,
                allocated: 4096,
                unknown: 0,
                files: 1
            }
        );
        assert_eq!(s.totals(d), s.totals(NodeId::ROOT));
        assert_eq!(s.totals(shrinks), Totals::default());

        // Back in again, and then removed.
        t.update_file(shrinks, sizes(7000, 8192));
        s.catch_up(&t, usize::MAX);
        assert_eq!(s.matches(), [grows, shrinks]);
        assert_eq!(s.totals(d).logical, 10_000);
        t.remove(shrinks);
        s.catch_up(&t, usize::MAX);
        assert_eq!(s.matches(), [grows]);
        assert_eq!(s.totals(d).logical, 3000);

        // The same as a search started now.
        let fresh = search("size:>1000", &t);
        assert_eq!(fresh.matches(), s.matches());
        assert_eq!(fresh.totals(NodeId::ROOT), s.totals(NodeId::ROOT));
    }

    #[test]
    fn remeasured_ts_files_change_type() {
        let mut t = Tree::new("/r");
        let f = t
            .add_file(NodeId::ROOT, "clip.ts".as_ref(), sizes(2, 4096), None, 0)
            .unwrap();
        let mut s = search("is:video", &t);
        assert!(s.matches().is_empty(), "a small .ts is code");
        t.update_file(f, sizes(2 << 20, 2 << 20));
        s.catch_up(&t, usize::MAX);
        assert_eq!(s.matches(), [f], "a large one is video");
    }

    use crate::git::{GitRoot, Problem, RootKind};

    /// A scan holding Git checkouts, worktrees and metadata:
    ///
    /// ```text
    /// /r
    ///   app/                 repository
    ///     .git/              (HEAD, objects/pack.pack 300 MB)
    ///     src/main.rs, src/notes.txt
    ///     big.iso 200 MB
    ///     vendor/lib/        nested repository: .git file, lib.rs
    ///   wt/                  linked worktree: .git file, src/app.rs, huge.iso 600 MB
    ///   bare.git/            bare repository: HEAD, objects/old.pack 700 MB
    ///   photos/a.jpg, film.iso 900 MB, notes.txt
    /// ```
    struct Repos {
        t: Tree,
        app: NodeId,
        app_git: NodeId,
        lib: NodeId,
        lib_git: NodeId,
        wt: NodeId,
        wt_git: NodeId,
        bare: NodeId,
    }

    const MB: u64 = KILO * KILO;

    impl Repos {
        /// The tree without any Git entries yet.
        fn new() -> Self {
            let mut t = Tree::new("/r");
            let root = NodeId::ROOT;
            let dir = |t: &mut Tree, p, name: &str| t.add_dir(p, name.as_ref()).unwrap();
            let file = |t: &mut Tree, p, name: &str, len: u64| {
                t.add_file(p, name.as_ref(), sizes(len, len), None, 0)
                    .unwrap()
            };
            let app = dir(&mut t, root, "app");
            let app_git = dir(&mut t, app, ".git");
            file(&mut t, app_git, "HEAD", 23);
            let objects = dir(&mut t, app_git, "objects");
            file(&mut t, objects, "pack.pack", 300 * MB);
            let src = dir(&mut t, app, "src");
            file(&mut t, src, "main.rs", 100);
            file(&mut t, src, "notes.txt", 40);
            file(&mut t, app, "big.iso", 200 * MB);
            let vendor = dir(&mut t, app, "vendor");
            let lib = dir(&mut t, vendor, "lib");
            let lib_git = file(&mut t, lib, ".git", 30);
            file(&mut t, lib, "lib.rs", 50);
            let wt = dir(&mut t, root, "wt");
            let wt_git = file(&mut t, wt, ".git", 60);
            let wt_src = dir(&mut t, wt, "src");
            file(&mut t, wt_src, "app.rs", 70);
            file(&mut t, wt, "huge.iso", 600 * MB);
            let bare = dir(&mut t, root, "bare.git");
            file(&mut t, bare, "HEAD", 21);
            let bare_objects = dir(&mut t, bare, "objects");
            file(&mut t, bare_objects, "old.pack", 700 * MB);
            let photos = dir(&mut t, root, "photos");
            file(&mut t, photos, "a.jpg", 5);
            file(&mut t, root, "film.iso", 900 * MB);
            file(&mut t, root, "notes.txt", 8);
            Self {
                t,
                app,
                app_git,
                lib,
                lib_git,
                wt,
                wt_git,
                bare,
            }
        }

        /// Every Git entry, as detection would record them.
        fn entries(&self) -> Vec<GitRoot> {
            vec![
                GitRoot::new(self.app, RootKind::Repository),
                GitRoot::new(self.app_git, RootKind::DotGit),
                GitRoot::new(self.lib, RootKind::Repository),
                GitRoot::new(self.lib_git, RootKind::DotGit),
                GitRoot::new(self.wt, RootKind::LinkedWorktree),
                GitRoot::new(self.wt_git, RootKind::DotGit),
                GitRoot::new(self.bare, RootKind::Bare),
            ]
        }

        fn found() -> Self {
            let mut r = Self::new();
            for e in r.entries() {
                r.t.set_git(e);
            }
            r
        }
    }

    /// Checks that `s` has caught up to exactly what a fresh search of
    /// `query` finds: the same matches and the same totals everywhere.
    fn assert_fresh(s: &Search, t: &Tree, query: &str) {
        let fresh = search(query, t);
        assert_eq!(names(s, t), names(&fresh, t), "{query}");
        let mut matches = s.matches().to_vec();
        matches.sort_unstable();
        let listed = matches.len();
        matches.dedup();
        assert_eq!(matches.len(), listed, "{query}: a file listed twice");
        let mut expected = fresh.matches().to_vec();
        expected.sort_unstable();
        assert_eq!(matches, expected, "{query}");
        for i in 0..t.len() {
            let n = NodeId::from_index(i as u32);
            assert_eq!(s.totals(n), fresh.totals(n), "{query}: totals of {n:?}");
        }
        assert_eq!(s.backlog(t), 0, "{query}");
    }

    /// Checks that every examined folder's totals are the sum of its
    /// examined contents': the totals are never torn between steps.
    fn assert_whole(s: &Search, t: &Tree) {
        for i in 0..s.examined() {
            let d = NodeId::from_index(i as u32);
            if !t.is_dir(d) || t.is_removed(d) {
                continue;
            }
            let (files, logical) = t
                .children(d)
                .filter(|c| c.index() < s.examined())
                .map(|c| s.totals(c))
                .fold((0, 0), |(f, l), c| (f + c.files, l + c.logical));
            let own = s.totals(d);
            assert_eq!((own.files, own.logical), (files, logical), "{d:?}");
        }
    }

    #[test]
    fn git_terms_parse_with_prefixes_and_report_unknown_kinds() {
        for (query, class, allowed) in [
            ("git:repo", GitClass::Metadata, true),
            ("GIT: Repositories", GitClass::Worktree, true),
            ("git:r", GitClass::Other, false),
            ("git:repos", GitClass::Checkout, true),
            ("git:w", GitClass::Worktree, true),
            ("git:work", GitClass::Checkout, false),
            ("git:worktrees", GitClass::Metadata, false),
            ("git:m", GitClass::Metadata, true),
            ("git:meta", GitClass::Checkout, false),
            ("!git:metadata", GitClass::Other, true),
            ("! git:metadata", GitClass::Metadata, false),
            ("!git:repo", GitClass::Worktree, false),
        ] {
            let m = Matcher::new(query).unwrap();
            assert!(m.depends_on_git(), "{query}");
            assert_eq!(m.git_allows(class), allowed, "{query} on {class:?}");
        }
        for typing in ["git:", "!git: ", "git:  ; "] {
            assert!(Matcher::parse(typing).unwrap().is_none(), "{typing:?}");
        }
        for unknown in ["git:x", "git:repox", "git:worktreeses", "git:wt"] {
            let err = Matcher::parse(unknown).unwrap_err();
            assert!(
                err.contains(&format!("“{unknown}” isn't a Git filter"))
                    && err.contains("git:repo, git:worktree, or git:metadata"),
                "{err}"
            );
        }
        assert!(!Matcher::new("*.rs; dir:src").unwrap().depends_on_git());
        assert!(
            Matcher::parse("git:repo | git:worktree").is_err(),
            "terms are separated by ;"
        );
    }

    #[test]
    fn positive_git_kinds_form_a_union() {
        let r = Repos::found();
        let t = &r.t;
        let repo = [
            "HEAD",
            "HEAD",
            "app.rs",
            "big.iso",
            "huge.iso",
            "lib.rs",
            "main.rs",
            "notes.txt",
            "old.pack",
            "pack.pack",
            ".git",
            ".git",
        ];
        let mut sorted = repo.map(str::to_owned).to_vec();
        sorted.sort();
        assert_eq!(names(&search("git:repo", t), t), sorted);
        assert_eq!(
            names(&search("git:repo; git:worktree", t), t),
            sorted,
            "worktrees are repositories too"
        );
        assert_eq!(
            names(&search("git:worktree", t), t),
            ["app.rs", "huge.iso"],
            "the worktree's .git file is metadata: the nearest entry wins"
        );
        assert_eq!(
            names(&search("git:metadata", t), t),
            [".git", ".git", "HEAD", "HEAD", "old.pack", "pack.pack"],
            ".git folders and files, and the bare repository"
        );
        let s = search("git:worktree; git:metadata", t);
        assert_eq!(s.matches().len(), 2 + 6);
        let s = search("!git:repo", t);
        assert_eq!(names(&s, t), ["a.jpg", "film.iso", "notes.txt"]);
        assert_eq!(s.totals(r.app), Totals::default());
    }

    #[test]
    fn git_terms_negate_and_intersect_with_every_other_term() {
        let r = Repos::found();
        let t = &r.t;
        let q = |query: &str| names(&search(query, t), t);
        assert_eq!(
            q("git:repo; !git:metadata"),
            [
                "app.rs",
                "big.iso",
                "huge.iso",
                "lib.rs",
                "main.rs",
                "notes.txt"
            ]
        );
        assert_eq!(
            q("!git:metadata; !git:worktree"),
            [
                "a.jpg",
                "big.iso",
                "film.iso",
                "lib.rs",
                "main.rs",
                "notes.txt",
                "notes.txt"
            ]
        );
        assert_eq!(q("*.rs; git:repo"), ["app.rs", "lib.rs", "main.rs"]);
        assert_eq!(q("*.rs; !git:worktree"), ["lib.rs", "main.rs"]);
        assert_eq!(q("notes; git:repo"), ["notes.txt"]);
        assert_eq!(q("is:code; git:repo"), ["app.rs", "lib.rs", "main.rs"]);
        assert_eq!(q("!is:code; git:worktree"), ["huge.iso"]);
        assert_eq!(q("dir:src; git:repo"), ["app.rs", "main.rs", "notes.txt"]);
        assert_eq!(q("dir:objects; !git:repo"), Vec::<String>::new());
        assert_eq!(
            q("!dir:objects; git:metadata"),
            [".git", ".git", "HEAD", "HEAD"]
        );
        assert_eq!(
            q("size:>100MB; git:repo"),
            ["big.iso", "huge.iso", "old.pack", "pack.pack"]
        );
        assert_eq!(
            q("size:<100; git:metadata"),
            [".git", ".git", "HEAD", "HEAD"]
        );
        assert_eq!(
            q("older:1d; git:repo"),
            Vec::<String>::new(),
            "unknown times never pass an age term"
        );
        assert_eq!(q("!newer:1d; !git:repo"), Vec::<String>::new());
    }

    #[test]
    fn every_example_query_does_what_it_says() {
        let r = Repos::found();
        let t = &r.t;
        let q = |query: &str| names(&search(query, t), t);
        let all_repo = q("git:repo");
        // git:repo | git:worktree: a union, so the same as git:repo.
        assert_eq!(q("git:repo; git:worktree"), all_repo);
        assert_eq!(q("git:worktree; git:repo"), all_repo);
        // !git:metadata | git:repo: repository content without Git's data.
        assert_eq!(
            q("!git:metadata; git:repo"),
            [
                "app.rs",
                "big.iso",
                "huge.iso",
                "lib.rs",
                "main.rs",
                "notes.txt"
            ]
        );
        // is:code | git:worktree: code in linked worktrees.
        assert_eq!(q("is:code; git:worktree"), ["app.rs"]);
        // size:>100MB | git:metadata: large files of Git's own; size is the
        // file's, not the repository's.
        let s = search("size:>100MB; git:metadata", t);
        assert_eq!(names(&s, t), ["old.pack", "pack.pack"]);
        assert_eq!(s.totals(r.app).logical, 300 * MB);
        assert_eq!(s.totals(r.bare).logical, 700 * MB);
        assert_eq!(q("size:>500MB; git:metadata"), ["old.pack"]);
        // size:>500MB alone: files anywhere, as before.
        assert_eq!(q("size:>500MB"), ["film.iso", "huge.iso", "old.pack"]);
        let s = search("git:repo", t);
        assert_eq!(
            s.totals(NodeId::ROOT).logical,
            t.logical(r.app) + t.logical(r.wt) + t.logical(r.bare),
            "matching bytes count each file once"
        );
    }

    #[test]
    fn entries_found_after_their_files_reclassify_them() {
        for query in [
            "git:repo",
            "git:worktree",
            "git:metadata",
            "!git:metadata",
            "*.rs; !git:repo",
            "size:>100MB; git:repo",
        ] {
            let mut r = Repos::new();
            let mut s = Search::new(Matcher::new(query).unwrap());
            assert!(s.catch_up(&r.t, usize::MAX));
            for e in r.entries() {
                r.t.set_git(e);
            }
            assert!(s.backlog(&r.t) > 0, "{query}");
            assert!(s.catch_up(&r.t, usize::MAX));
            assert_fresh(&s, &r.t, query);
        }
    }

    #[test]
    fn stepping_one_node_at_a_time_converges_with_whole_totals_each_step() {
        for query in [
            "git:repo",
            "git:worktree; !git:metadata",
            "!git:repo",
            "*.iso; git:repo",
        ] {
            let mut r = Repos::new();
            let mut s = Search::new(Matcher::new(query).unwrap());
            // Part of the tree examined, then entries arrive one at a time
            // between single steps, as a scan's probes would.
            for _ in 0..12 {
                s.catch_up(&r.t, 1);
                assert_whole(&s, &r.t);
            }
            for e in r.entries() {
                r.t.set_git(e);
                s.catch_up(&r.t, 1);
                assert_whole(&s, &r.t);
            }
            let mut steps = 0;
            while !s.catch_up(&r.t, 1) {
                assert_whole(&s, &r.t);
                steps += 1;
                assert!(steps < 10 * r.t.len(), "{query}: never finishes");
            }
            assert_fresh(&s, &r.t, query);
        }
    }

    #[test]
    fn a_nested_repository_found_before_or_after_its_outer_one() {
        for nested_first in [true, false] {
            let mut r = Repos::new();
            let mut s = Search::new(Matcher::new("git:repo; !git:metadata").unwrap());
            assert!(s.catch_up(&r.t, usize::MAX));
            let outer = [
                GitRoot::new(r.app, RootKind::Repository),
                GitRoot::new(r.app_git, RootKind::DotGit),
            ];
            let nested = [
                GitRoot::new(r.lib, RootKind::Repository),
                GitRoot::new(r.lib_git, RootKind::DotGit),
            ];
            let (first, second) = if nested_first {
                (nested, outer)
            } else {
                (outer, nested)
            };
            for e in first {
                r.t.set_git(e);
            }
            while !s.catch_up(&r.t, 2) {
                assert_whole(&s, &r.t);
            }
            assert_fresh(&s, &r.t, "git:repo; !git:metadata");
            for e in second {
                r.t.set_git(e);
            }
            while !s.catch_up(&r.t, 2) {
                assert_whole(&s, &r.t);
            }
            assert_fresh(&s, &r.t, "git:repo; !git:metadata");
            assert_eq!(
                names(&s, &r.t),
                ["big.iso", "lib.rs", "main.rs", "notes.txt"],
                "nested_first: {nested_first}"
            );
        }
    }

    #[test]
    fn a_repository_above_the_scan_root_found_late() {
        let mut r = Repos::found();
        let mut s = search("!git:repo", &r.t);
        assert_eq!(s.matches().len(), 3);
        r.t.set_git_above("/".into(), GitRoot::new(NodeId::ROOT, RootKind::Repository));
        assert!(s.catch_up(&r.t, usize::MAX));
        assert!(s.matches().is_empty(), "everything is in a repository now");
        assert_fresh(&s, &r.t, "!git:repo");
        // An entry of the root itself wins over the one above.
        r.t.set_git(GitRoot::new(NodeId::ROOT, RootKind::LinkedWorktree));
        let mut w = search("git:worktree", &r.t);
        assert_eq!(
            w.matches().len(),
            5,
            "a.jpg, film.iso, notes.txt, and the worktree's checkout"
        );
        r.t.set_git(GitRoot::unresolved(NodeId::ROOT, Problem::NotGit));
        assert!(w.catch_up(&r.t, usize::MAX));
        assert_fresh(&w, &r.t, "git:worktree");
        assert_eq!(
            names(&w, &r.t),
            ["app.rs", "huge.iso"],
            "back to the repository above"
        );
    }

    #[test]
    fn detection_sequences_converge_step_by_step() {
        // As the scanner records them: candidates first, each finding later,
        // some never confirmed; a scan of a `.git` folder itself is
        // metadata, inside the repository found above it.
        for query in ["git:repo", "!git:repo", "git:metadata"] {
            let mut r = Repos::new();
            let mut s = Search::new(Matcher::new(query).unwrap());
            let steps = |s: &mut Search, t: &Tree| {
                while !s.catch_up(t, 3) {
                    assert_whole(s, t);
                }
                assert_fresh(s, t, query);
            };
            steps(&mut s, &r.t);
            for n in [r.app, r.lib, r.wt] {
                r.t.set_git(GitRoot::pending(n));
                steps(&mut s, &r.t);
            }
            r.t.set_git_above("/".into(), GitRoot::new(NodeId::ROOT, RootKind::Repository));
            steps(&mut s, &r.t);
            r.t.set_git(GitRoot::new(NodeId::ROOT, RootKind::DotGit));
            steps(&mut s, &r.t);
            r.t.set_git(GitRoot::new(r.app, RootKind::Repository));
            r.t.set_git(GitRoot::new(r.app_git, RootKind::DotGit));
            r.t.set_git(GitRoot::unresolved(r.lib, Problem::NotChecked));
            r.t.set_git(GitRoot::unresolved(r.wt, Problem::NotChecked));
            steps(&mut s, &r.t);
            r.t.set_git(GitRoot::new(NodeId::ROOT, RootKind::WorktreeAdmin));
            steps(&mut s, &r.t);
        }
    }

    #[test]
    fn a_git_file_classified_late_becomes_metadata() {
        let mut r = Repos::new();
        let mut s = search("git:metadata", &r.t);
        assert!(s.matches().is_empty());
        // The worktree checks out before its `.git` file is marked.
        r.t.set_git(GitRoot::new(r.wt, RootKind::LinkedWorktree));
        assert!(s.catch_up(&r.t, usize::MAX));
        assert!(s.matches().is_empty());
        r.t.set_git(GitRoot::new(r.wt_git, RootKind::DotGit));
        assert_eq!(s.backlog(&r.t), 1, "one change to look at");
        assert!(s.catch_up(&r.t, usize::MAX));
        assert_eq!(s.matches(), [r.wt_git]);
        assert_eq!(s.totals(r.wt).logical, 60);
        assert_fresh(&s, &r.t, "git:metadata");
    }

    #[test]
    fn deleting_a_repositorys_git_folder_leaves_it_unconfirmed() {
        let mut r = Repos::found();
        let mut s = search("git:repo", &r.t);
        let before = s.matches().len();
        r.t.remove(r.app_git);
        assert!(s.catch_up(&r.t, usize::MAX));
        assert_fresh(&s, &r.t, "git:repo");
        // app's checkout is no longer known to be one; its nested library
        // still is.
        assert_eq!(s.matches().len(), before - 2 - 3);
        assert_eq!(s.totals(r.app).files, 2, "lib.rs and lib's .git");
    }

    #[test]
    fn queries_without_git_terms_ignore_git_entries() {
        let mut r = Repos::new();
        let mut s = search("*.rs", &r.t);
        let version = s.version();
        for e in r.entries() {
            r.t.set_git(e);
        }
        assert_eq!(s.backlog(&r.t), 0, "nothing to do");
        assert!(s.catch_up(&r.t, 1));
        assert_eq!(s.version(), version);
        assert!(s.git.is_empty() && s.rewalk.is_empty(), "no memory used");
    }

    #[test]
    fn the_backlog_counts_what_is_left_to_look_at_again() {
        let mut r = Repos::new();
        let mut s = search("git:worktree", &r.t);
        assert_eq!(s.backlog(&r.t), 0);
        r.t.set_git(GitRoot::new(r.wt, RootKind::LinkedWorktree));
        assert_eq!(s.backlog(&r.t), 1, "one entry changed");
        assert!(
            !s.catch_up(&r.t, 1),
            "the worktree's folder, not its contents"
        );
        assert!(s.backlog(&r.t) > 0, "its contents are left");
        assert!(s.matches().is_empty());
        while !s.catch_up(&r.t, 1) {}
        assert_eq!(s.backlog(&r.t), 0);
        assert_eq!(
            names(&s, &r.t),
            [".git", "app.rs", "huge.iso"],
            "its .git file isn't marked yet"
        );
    }

    #[test]
    fn a_file_dropped_and_restored_in_one_catch_up_is_listed_once() {
        // The file's own entry goes class-less, so it's looked at with its
        // folder's stale class and drops out, before its folder's new class
        // brings it back.
        let mut t = Tree::new("/r");
        let a = t.add_dir(NodeId::ROOT, "a".as_ref()).unwrap();
        let f = t
            .add_file(a, "f.txt".as_ref(), sizes(10, 10), None, 0)
            .unwrap();
        t.set_git(GitRoot::new(f, RootKind::DotGit));
        let mut s = search("git:repo", &t);
        assert_eq!(s.matches(), [f]);
        t.set_git(GitRoot::new(a, RootKind::Repository));
        t.set_git(GitRoot::unresolved(f, Problem::NotGit));
        assert!(s.catch_up(&t, usize::MAX));
        assert_eq!(s.matches(), [f]);
        assert_fresh(&s, &t, "git:repo");
    }

    #[test]
    fn a_nested_repository_left_unchecked_inside_a_new_one_is_listed_once() {
        let mut r = Repos::new();
        r.t.set_git(GitRoot::new(r.lib, RootKind::Repository));
        let mut s = search("git:repo", &r.t);
        assert_eq!(s.matches().len(), 2, "lib.rs and lib's .git");
        r.t.set_git(GitRoot::new(r.app, RootKind::Repository));
        r.t.set_git(GitRoot::unresolved(r.lib, Problem::NotChecked));
        assert!(s.catch_up(&r.t, usize::MAX));
        assert_fresh(&s, &r.t, "git:repo");
        // And with a size term, re-measured files judged against classes
        // not yet walked again.
        let mut r = Repos::new();
        r.t.set_git(GitRoot::new(r.lib, RootKind::Repository));
        let mut s = search("git:repo; size:<1000", &r.t);
        r.t.set_git(GitRoot::new(r.app, RootKind::Repository));
        r.t.set_git(GitRoot::unresolved(r.lib, Problem::NotChecked));
        let lib_rs =
            r.t.children(r.lib)
                .find(|&c| r.t.name(c) == "lib.rs")
                .unwrap();
        r.t.update_file(lib_rs, sizes(60, 60));
        assert!(s.catch_up(&r.t, usize::MAX));
        assert_fresh(&s, &r.t, "git:repo; size:<1000");
    }

    #[test]
    fn interleaved_growth_and_discovery_converge_to_a_fresh_search() {
        // xorshift, so failures repeat.
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move |n: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % n
        };
        let kinds = [
            RootKind::Repository,
            RootKind::LinkedWorktree,
            RootKind::DotGit,
            RootKind::Bare,
            RootKind::Pending,
            RootKind::Unresolved,
        ];
        let queries = [
            "git:repo",
            "git:worktree; !git:metadata",
            "!git:repo; *1*",
            "size:>500; git:repo",
            "!git:metadata; size:<300",
        ];
        for round in 0..200 {
            let query = queries[round % queries.len()];
            let mut t = Tree::new("/r");
            let mut dirs = vec![NodeId::ROOT];
            let mut files = Vec::new();
            let mut entries: Vec<NodeId> = Vec::new();
            let mut s = Search::new(Matcher::new(query).unwrap());
            for _ in 0..300 {
                match next(14) {
                    0..=2 => {
                        let p = dirs[next(dirs.len() as u64) as usize];
                        let name = format!("d{}", t.len());
                        dirs.push(t.add_dir(p, name.as_ref()).unwrap());
                    }
                    3..=6 => {
                        let p = dirs[next(dirs.len() as u64) as usize];
                        let name = format!("f{}", t.len());
                        let len = next(1000);
                        files.push(
                            t.add_file(p, name.as_ref(), sizes(len, len), None, 0)
                                .unwrap(),
                        );
                    }
                    7 | 8 => {
                        let n = NodeId::from_index(next(t.len() as u64) as u32);
                        let kind = kinds[next(kinds.len() as u64) as usize];
                        t.set_git(GitRoot::new(n, kind));
                        entries.push(n);
                    }
                    // An entry changing kind, often from a class to none
                    // (confirmed, then unresolved or unchecked).
                    9 if !entries.is_empty() => {
                        let n = entries[next(entries.len() as u64) as usize];
                        let kind = kinds[next(kinds.len() as u64) as usize];
                        t.set_git(GitRoot::new(n, kind));
                    }
                    // A file re-measured, so size terms judge it again.
                    10 if !files.is_empty() => {
                        let f = files[next(files.len() as u64) as usize];
                        let len = next(1000);
                        t.update_file(f, sizes(len, len));
                    }
                    11 => {
                        assert!(s.catch_up(&t, usize::MAX));
                        assert_fresh(&s, &t, query);
                    }
                    _ => {
                        s.catch_up(&t, 1 + next(5) as usize);
                        assert_whole(&s, &t);
                    }
                }
            }
            while !s.catch_up(&t, 3) {
                assert_whole(&s, &t);
            }
            assert_fresh(&s, &t, query);
        }
    }
}
