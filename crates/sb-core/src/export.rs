//! Exporting what's under a folder, as CSV rows or a plain-text report.
//!
//! Trees with millions of nodes are written in steps. Each step examines up
//! to a budget of nodes and formats them into a buffer, so a caller can hold
//! the tree's read lock only while formatting, write the buffer out with the
//! lock released, and let a running scan continue in between. The walk
//! keeps a stack of node IDs across steps; IDs are never reused, and a node
//! removed between steps is skipped. Nodes the scan adds meanwhile are
//! included only if their folder hasn't been walked yet, so an export taken
//! while scanning is, like the map, a partial picture. Sizes then are as of
//! the step that wrote each row, a row's share is of the folder's size at
//! that moment (so shares stay within 100%), and each folder's contents are
//! ordered by size when that folder is reached.
//!
//! With a [`Search`], only matching files are listed, and folders only when
//! they contain matches, with matching totals, as in the filtered map.
//!
//! CSV rows end with the item's Git class (`checkout`, `worktree`,
//! `metadata`, or empty), and a report lists the Git repositories under the
//! folder, with what's in repositories counted once, nested ones included.
//!
//! CSV follows RFC 4180: fields with commas, quotes, or line breaks are
//! quoted, with quotes doubled, and rows end in CRLF. The file starts with a
//! UTF-8 byte order mark, so spreadsheet apps read non-ASCII names
//! correctly. Sizes are given as whole bytes and again for display. Text
//! that starts with `=`, `+`, `-`, `@`, a tab, or a carriage return gets a
//! leading `'`, so a spreadsheet shows a name like `=1+2` as text instead of
//! running it as a formula.
//!
//! Names and paths that aren't valid Unicode are written with replacement
//! characters. Modified times are left empty where unknown, and where the
//! tree only knows the time is before 1970 or after early 2106.

use crate::filetype::FileType;
use crate::git::{self, GitClass, Head, RootKind};
use crate::layout::Metric;
use crate::search::Search;
use crate::tree::{NodeId, Tree};
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fmt::Write as _;
use std::io::{self, Write};

/// Bytes per KB in display sizes, as the system's file manager counts them
/// (Explorer 1,024; Finder and Linux file managers 1,000), matching the
/// interface.
pub const SYSTEM_KILO: u64 = if cfg!(windows) { 1024 } else { 1000 };

/// Folders and files each listed in a report.
pub const REPORT_TOP: usize = 50;

const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];

/// Writes a modified time, in seconds since the Unix epoch, for a file. The
/// app passes local-time formats; these defaults use UTC, since this crate
/// knows no time zones.
pub type TimeFormat = fn(i64) -> String;

/// A file's modified time when it's known exactly: the tree stores times
/// before 1970 as 1 and after early 2106 as `u32::MAX`.
fn known_modified(tree: &Tree, node: NodeId) -> Option<i64> {
    tree.modified(node)
        .filter(|&secs| secs != 1 && secs != u32::MAX)
        .map(i64::from)
}

/// Year, month, day, hour, minute, and second of a time in UTC.
fn utc_parts(secs: i64) -> (i64, i64, i64, i64, i64, i64) {
    // Days to a civil date, after Howard Hinnant's `civil_from_days`.
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day, rem / 3600, rem / 60 % 60, rem % 60)
}

/// ISO 8601 in UTC, such as `2026-10-13T16:05:09Z`.
pub fn iso_utc(secs: i64) -> String {
    let (y, mo, d, h, mi, s) = utc_parts(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// A short date and time in UTC, such as `2026-10-13 16:05`.
pub fn short_utc(secs: i64) -> String {
    let (y, mo, d, h, mi, _) = utc_parts(secs);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}")
}

/// What a CSV export lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// The folder's own files and folders.
    Contents,
    /// Every file and folder at any depth under it.
    Everything,
}

/// A size for reading, such as "1.23 MB", like the interface shows it.
pub fn format_bytes(bytes: u64, kilo: u64) -> String {
    if bytes < kilo {
        return format!("{bytes} B");
    }
    let k = kilo as f64;
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= k && unit < UNITS.len() - 1 {
        value /= k;
        unit += 1;
    }
    // 999.6 KB would round to "1000 KB"; it reads "1.00 MB" instead.
    if value.round() >= k && unit < UNITS.len() - 1 {
        value /= k;
        unit += 1;
    }
    let digits = if value >= 100.0 {
        0
    } else if value >= 10.0 {
        1
    } else {
        2
    };
    format!("{value:.digits$} {}", UNITS[unit])
}

/// A count with thousands separators, such as "12,345".
pub fn format_count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `part` as a percentage of `whole`, with two decimals and no sign.
fn share(part: u64, whole: u64) -> String {
    if whole == 0 {
        return "0.00".to_owned();
    }
    format!("{:.2}", part as f64 * 100.0 / whole as f64)
}

/// Appends one CSV field: guarded against formula injection, then quoted
/// if it holds a comma, quote, or line break.
pub fn push_field(out: &mut String, text: &str) {
    let guard = text.starts_with(['=', '+', '-', '@', '\t', '\r']);
    if text.contains([',', '"', '\r', '\n']) {
        out.push('"');
        if guard {
            out.push('\'');
        }
        for c in text.chars() {
            if c == '"' {
                out.push('"');
            }
            out.push(c);
        }
        out.push('"');
    } else {
        if guard {
            out.push('\'');
        }
        out.push_str(text);
    }
}

/// Whether `node` is listed: with a filter, only where something matches.
fn included(filter: Option<&Search>, node: NodeId) -> bool {
    filter.is_none_or(|s| s.totals(node).files > 0)
}

/// Files counted at or under `node`, matching ones when filtered.
fn files_in(tree: &Tree, filter: Option<&Search>, node: NodeId) -> u64 {
    u64::from(match filter {
        Some(s) => s.totals(node).files,
        None => tree.file_count(node),
    })
}

/// A depth-first walk under a folder that can stop and resume: folders come
/// before their contents, and each folder's listed children largest first.
#[derive(Debug)]
struct Walk {
    folder: NodeId,
    metric: Metric,
    /// Descend into folders; otherwise only the folder's own children.
    deep: bool,
    started: bool,
    /// Nodes still to visit, the next on top.
    stack: Vec<NodeId>,
}

impl Walk {
    fn new(folder: NodeId, metric: Metric, deep: bool) -> Self {
        Self {
            folder,
            metric,
            deep,
            started: false,
            stack: Vec::new(),
        }
    }

    fn push_children(&mut self, tree: &Tree, filter: Option<&Search>, dir: NodeId) {
        let start = self.stack.len();
        self.stack.extend(
            tree.children(dir)
                .filter(|&c| !tree.is_removed(c) && included(filter, c)),
        );
        let metric = self.metric;
        // Smallest first, so the largest is popped first.
        self.stack[start..]
            .sort_unstable_by_key(|&c| (metric.weight_in(tree, filter, c), Reverse(c.index())));
    }

    /// Calls `visit` for up to `budget` nodes; returns `true` once every
    /// node has been visited.
    fn step(
        &mut self,
        tree: &Tree,
        filter: Option<&Search>,
        budget: usize,
        mut visit: impl FnMut(NodeId),
    ) -> bool {
        if !self.started {
            self.started = true;
            self.push_children(tree, filter, self.folder);
        }
        for _ in 0..budget {
            let Some(node) = self.stack.pop() else {
                return true;
            };
            // Removed between steps, along with everything under it.
            if tree.is_removed(node) {
                continue;
            }
            visit(node);
            if self.deep && tree.is_dir(node) {
                self.push_children(tree, filter, node);
            }
        }
        self.stack.is_empty()
    }
}

/// A CSV export of the files and folders under a folder, written in steps.
#[derive(Debug)]
pub struct Csv {
    walk: Walk,
    kilo: u64,
    time: TimeFormat,
    rows: u64,
}

/// The CSV header row.
pub const CSV_HEADER: &str =
    "Path,Name,Type,Size (bytes),Size,Files,File type,Share of folder (%),Modified,Git\r\n";

/// What the CSV's Git column says about content of a Git class.
fn git_keyword(class: GitClass) -> &'static str {
    match class {
        GitClass::Other => "",
        GitClass::Checkout => "checkout",
        GitClass::Worktree => "worktree",
        GitClass::Metadata => "metadata",
    }
}

impl Csv {
    pub fn new(folder: NodeId, scope: Scope, metric: Metric) -> Self {
        Self {
            walk: Walk::new(folder, metric, scope == Scope::Everything),
            kilo: SYSTEM_KILO,
            time: iso_utc,
            rows: 0,
        }
    }

    /// Writes modified times with `time` instead of ISO 8601 in UTC.
    pub fn with_time_format(mut self, time: TimeFormat) -> Self {
        self.time = time;
        self
    }

    /// Display sizes with this many bytes per KB instead of the system's.
    pub fn with_kilo(mut self, kilo: u64) -> Self {
        self.kilo = kilo;
        self
    }

    /// Rows written so far, not counting the header.
    pub fn rows(&self) -> u64 {
        self.rows
    }

    /// Appends up to `budget` rows to `out`; the first step starts with the
    /// byte order mark and header. `filter` must be caught up with `tree`.
    /// Returns `true` once done.
    pub fn step(
        &mut self,
        tree: &Tree,
        filter: Option<&Search>,
        budget: usize,
        out: &mut String,
    ) -> bool {
        if !self.walk.started {
            out.push('\u{feff}');
            out.push_str(CSV_HEADER);
        }
        // Taken under the same lock as the rows, since a scan may have grown
        // the folder since the last step.
        let whole = self.walk.metric.weight_in(tree, filter, self.walk.folder);
        let (metric, kilo, time) = (self.walk.metric, self.kilo, self.time);
        let rows = &mut self.rows;
        self.walk.step(tree, filter, budget, |node| {
            let folder = tree.is_dir(node);
            let size = metric.weight_in(tree, filter, node);
            push_field(out, &tree.path(node).to_string_lossy());
            out.push(',');
            push_field(out, &tree.name(node).to_string_lossy());
            out.push_str(if folder { ",folder," } else { ",file," });
            let _ = write!(out, "{size},");
            push_field(out, &format_bytes(size, kilo));
            out.push(',');
            if folder {
                let _ = write!(out, "{},", files_in(tree, filter, node));
            } else {
                out.push(',');
                out.push_str(FileType::of(tree.name(node), tree.logical(node)).keyword());
            }
            out.push(',');
            out.push_str(&share(size, whole));
            out.push(',');
            if let Some(secs) = known_modified(tree, node) {
                push_field(out, &time(secs));
            }
            out.push(',');
            // Looked up through the folders above, as the path is; a scan
            // without repositories skips that.
            if !tree.git().is_empty() {
                out.push_str(git_keyword(tree.git_class(node)));
            }
            out.push_str("\r\n");
            *rows += 1;
        })
    }
}

/// What a report says about the scan besides the tree itself.
#[derive(Debug, Clone, Default)]
pub struct ReportInfo {
    pub scan_root: String,
    /// How the scan went, e.g. "Complete in 12.3 s".
    pub scan: String,
    /// When the report was made, in the user's local time.
    pub exported_at: String,
    /// The filter query, if one is on.
    pub filter: Option<String>,
    /// Items the scan left out, by reason, for the whole scan.
    pub not_counted: Vec<(String, u64)>,
}

/// A readable summary of a folder: totals and the largest folders and files
/// at any depth under it. Gathered in steps, then written by
/// [`Report::write`].
#[derive(Debug)]
pub struct Report {
    walk: Walk,
    kilo: u64,
    time: TimeFormat,
    folders: u64,
    files: u64,
    /// The largest so far, smallest on top: (weight, earlier ID first).
    top_folders: BinaryHeap<Reverse<(u64, Reverse<NodeId>)>>,
    top_files: BinaryHeap<Reverse<(u64, Reverse<NodeId>)>>,
}

fn keep_top(heap: &mut BinaryHeap<Reverse<(u64, Reverse<NodeId>)>>, weight: u64, node: NodeId) {
    let entry = Reverse((weight, Reverse(node)));
    if heap.len() < REPORT_TOP {
        heap.push(entry);
    } else if heap.peek().is_some_and(|smallest| entry < *smallest) {
        heap.pop();
        heap.push(entry);
    }
}

impl Report {
    pub fn new(folder: NodeId, metric: Metric) -> Self {
        Self {
            walk: Walk::new(folder, metric, true),
            kilo: SYSTEM_KILO,
            time: short_utc,
            folders: 0,
            files: 0,
            top_folders: BinaryHeap::new(),
            top_files: BinaryHeap::new(),
        }
    }

    /// Display sizes with this many bytes per KB instead of the system's.
    pub fn with_kilo(mut self, kilo: u64) -> Self {
        self.kilo = kilo;
        self
    }

    /// Writes files' modified times with `time` instead of in UTC.
    pub fn with_time_format(mut self, time: TimeFormat) -> Self {
        self.time = time;
        self
    }

    /// Folders and files examined so far.
    pub fn examined(&self) -> u64 {
        self.folders + self.files
    }

    /// Examines up to `budget` nodes; returns `true` once done.
    pub fn step(&mut self, tree: &Tree, filter: Option<&Search>, budget: usize) -> bool {
        let metric = self.walk.metric;
        let (folders, files) = (&mut self.folders, &mut self.files);
        let (top_folders, top_files) = (&mut self.top_folders, &mut self.top_files);
        self.walk.step(tree, filter, budget, |node| {
            let weight = metric.weight_in(tree, filter, node);
            if tree.is_dir(node) {
                *folders += 1;
                if weight > 0 {
                    keep_top(top_folders, weight, node);
                }
            } else {
                *files += 1;
                if weight > 0 {
                    keep_top(top_files, weight, node);
                }
            }
        })
    }

    /// Writes the report once every step is done; sizes and paths are read
    /// from `tree` now. Lines end in CRLF on Windows. Returns the number of
    /// folders and files listed.
    pub fn write(
        &self,
        tree: &Tree,
        filter: Option<&Search>,
        info: &ReportInfo,
        out: &mut impl Write,
    ) -> io::Result<u64> {
        let nl = if cfg!(windows) { "\r\n" } else { "\n" };
        let kilo = self.kilo;
        let metric = self.walk.metric;
        let folder = self.walk.folder;
        let whole = metric.weight_in(tree, filter, folder);
        let mut text = String::new();
        let mut line = |s: &str| {
            text.push_str(s);
            text.push_str(nl);
        };
        line("SpaceBadger report");
        line("");
        line(&format!(
            "Folder:     {}",
            tree.path(folder).to_string_lossy()
        ));
        line(&format!("Scan root:  {}", info.scan_root));
        line(&format!("Scan:       {}", info.scan));
        line(&format!("Exported:   {}", info.exported_at));
        if let Some(query) = &info.filter {
            line(&format!(
                "Filter:     {query} (only matching files are counted)"
            ));
        }
        line("");
        line(&format!(
            "Size:       {} ({} bytes)",
            format_bytes(whole, kilo),
            format_count(whole)
        ));
        line(&format!("Files:      {}", format_count(self.files)));
        line(&format!("Folders:    {}", format_count(self.folders)));

        let mut listed = 0;
        let sorted = |heap: &BinaryHeap<Reverse<(u64, Reverse<NodeId>)>>| {
            let mut nodes: Vec<NodeId> = heap.iter().map(|e| e.0.1.0).collect();
            nodes.retain(|&n| !tree.is_removed(n));
            // Sizes may have changed since they were ranked.
            nodes.sort_unstable_by_key(|&n| (Reverse(metric.weight_in(tree, filter, n)), n));
            nodes
        };
        // `third` is a folder's file count or a file's modified time, padded.
        let row = |n: NodeId, third: String| {
            let size = metric.weight_in(tree, filter, n);
            format!(
                "{:>10}  {:>6}%  {third}  {}",
                format_bytes(size, kilo),
                share_short(size, whole),
                tree.path(n).to_string_lossy()
            )
        };

        let folders = sorted(&self.top_folders);
        line("");
        line(&format!(
            "Largest folders ({} of {})",
            format_count(folders.len() as u64),
            format_count(self.folders)
        ));
        if folders.is_empty() {
            line("  None.");
        } else {
            line(&format!(
                "{:>10}  {:>7}  {:>10}  Path",
                "Size", "Share", "Files"
            ));
            for &n in &folders {
                let files = format_count(files_in(tree, filter, n));
                line(&row(n, format!("{files:>10}")));
                listed += 1;
            }
        }

        let files = sorted(&self.top_files);
        line("");
        line(&format!(
            "Largest files ({} of {})",
            format_count(files.len() as u64),
            format_count(self.files)
        ));
        if files.is_empty() {
            line("  None.");
        } else {
            line(&format!(
                "{:>10}  {:>7}  {:<16}  Path",
                "Size", "Share", "Modified"
            ));
            for &n in &files {
                let modified = known_modified(tree, n).map_or(String::new(), self.time);
                line(&row(n, format!("{modified:<16}")));
                listed += 1;
            }
        }

        // Repositories at or under the folder (not one around it, nor the
        // one above the scan root), and their totals, from one summary.
        let under: Vec<git::RootSummary> = if tree.git().is_empty() {
            Vec::new()
        } else {
            git::summarize(tree, filter, metric)
                .0
                .into_iter()
                .filter(|r| !r.outside && within(tree, r.node, folder))
                .collect()
        };
        let size_of = |r: &git::RootSummary| r.matching.unwrap_or(r.size);
        let mut roots: Vec<(NodeId, u64)> = under
            .iter()
            .map(|r| (r.node, size_of(r)))
            .filter(|&(_, size)| filter.is_none() || size > 0)
            .collect();
        if !roots.is_empty() {
            // Matching sizes can order them differently.
            roots.sort_by_key(|&(n, size)| (Reverse(size), n));
            let all = roots.len();
            roots.truncate(REPORT_TOP);
            let heads: Vec<String> = roots
                .iter()
                .map(|&(n, _)| {
                    tree.git_entry(n)
                        .and_then(|r| r.head.as_ref())
                        .map_or(String::new(), |h| match h {
                            Head::Branch(name) | Head::Ref(name) => name.clone(),
                            Head::Detached(commit) => format!("detached at {commit}"),
                        })
                })
                .collect();
            let width = heads
                .iter()
                .map(|h| h.chars().count())
                .max()
                .unwrap_or(0)
                .max("Branch".len());
            line("");
            line(&format!(
                "Git repositories ({} of {})",
                format_count(roots.len() as u64),
                format_count(all as u64)
            ));
            line(&format!(
                "{:>10}  {:<16}  {:<width$}  Path",
                "Size", "Kind", "Branch"
            ));
            for (&(n, size), head) in roots.iter().zip(&heads) {
                let entry = tree.git_entry(n);
                let kind = entry.map_or("", |r| kind_label(r.kind));
                let mut text = format!(
                    "{:>10}  {kind:<16}  {head:<width$}  {}",
                    format_bytes(size, kilo),
                    tree.path(n).to_string_lossy()
                );
                if let Some(problem) = entry.and_then(|r| r.problem.as_ref()) {
                    let _ = write!(text, " ({problem})");
                }
                line(&text);
            }
            // Everything in a repository with a class is in the outermost
            // one under the folder; unresolved roots have no class, so the
            // repositories inside them count instead.
            let has_class = |n: NodeId| {
                tree.git_entry(n)
                    .is_some_and(|r| r.kind.listed() && r.kind.class().is_some())
            };
            let outermost = |r: &&git::RootSummary| {
                has_class(r.node)
                    && std::iter::successors(tree.parent(r.node), |&a| tree.parent(a))
                        .take_while(|&a| within(tree, a, folder))
                        .all(|a| !has_class(a))
            };
            let total: u64 = under.iter().filter(outermost).map(size_of).sum();
            if filter.is_some() {
                line(&format!(
                    "In Git repositories under this folder: {} matching, nested repositories counted once.",
                    format_bytes(total, kilo)
                ));
            } else {
                let metadata: u64 = under.iter().map(|r| r.metadata).sum();
                line(&format!(
                    "In Git repositories under this folder: {} (Git data {}), nested repositories counted once.",
                    format_bytes(total, kilo),
                    format_bytes(metadata, kilo)
                ));
            }
        }

        line("");
        line("Not counted (whole scan)");
        if info.not_counted.is_empty() {
            line("  Nothing was left out.");
        } else {
            for (reason, count) in &info.not_counted {
                line(&format!("  {reason}: {}", format_count(*count)));
            }
        }
        out.write_all(text.as_bytes())?;
        Ok(listed)
    }
}

/// Whether `node` is `folder` or inside it.
fn within(tree: &Tree, node: NodeId, folder: NodeId) -> bool {
    let mut cur = Some(node);
    while let Some(n) = cur {
        if n == folder {
            return true;
        }
        cur = tree.parent(n);
    }
    false
}

/// A listed Git root's kind, for reading.
fn kind_label(kind: RootKind) -> &'static str {
    match kind {
        RootKind::Repository => "repository",
        RootKind::LinkedWorktree => "linked worktree",
        RootKind::Submodule => "submodule",
        RootKind::SeparateGitDir => "separate Git dir",
        RootKind::Bare => "bare repository",
        RootKind::GitDir => "Git directory",
        RootKind::Unresolved => "unresolved",
        RootKind::Pending | RootKind::DotGit | RootKind::WorktreeAdmin => "",
    }
}

/// A share for reading, with one decimal.
fn share_short(part: u64, whole: u64) -> String {
    if whole == 0 {
        return "0.0".to_owned();
    }
    format!("{:.1}", part as f64 * 100.0 / whole as f64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::Matcher;
    use crate::size::Allocated;
    use crate::tree::{DirState, FileSizes};
    use std::ffi::OsStr;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    /// 2023-11-14 22:13:20 UTC.
    const WHEN: u64 = 1_700_000_000;

    fn file(tree: &mut Tree, parent: NodeId, name: &str, bytes: u64) -> NodeId {
        file_at(tree, parent, name, bytes, None)
    }

    fn file_at(
        tree: &mut Tree,
        parent: NodeId,
        name: &str,
        bytes: u64,
        modified: Option<SystemTime>,
    ) -> NodeId {
        let sizes = FileSizes {
            logical: bytes,
            allocated: Allocated::known(bytes),
        };
        tree.add_file(parent, OsStr::new(name), sizes, modified, 0)
            .unwrap()
    }

    fn dir(tree: &mut Tree, parent: NodeId, name: &str) -> NodeId {
        let d = tree.add_dir(parent, OsStr::new(name)).unwrap();
        tree.set_dir_state(d, DirState::Listed);
        d
    }

    /// root/
    ///   docs/ (a.txt 300, deep/ (b.jpg 200))
    ///   big.jpg 1000, modified at [`WHEN`]
    ///   =cmd.txt 100, modified before 1970
    fn sample() -> Tree {
        let mut t = Tree::new(std::path::Path::new("root"));
        let docs = dir(&mut t, NodeId::ROOT, "docs");
        file(&mut t, docs, "a.txt", 300);
        let deep = dir(&mut t, docs, "deep");
        file(&mut t, deep, "b.jpg", 200);
        let when = UNIX_EPOCH + Duration::from_secs(WHEN);
        file_at(&mut t, NodeId::ROOT, "big.jpg", 1000, Some(when));
        let early = UNIX_EPOCH - Duration::from_secs(60);
        file_at(&mut t, NodeId::ROOT, "=cmd.txt", 100, Some(early));
        t
    }

    fn csv(tree: &Tree, filter: Option<&Search>, folder: NodeId, scope: Scope) -> String {
        let mut out = String::new();
        let mut csv = Csv::new(folder, scope, Metric::Logical).with_kilo(1024);
        // Small steps, as the app takes them.
        while !csv.step(tree, filter, 2, &mut out) {}
        out
    }

    /// The rows without the BOM and header, split into fields naively
    /// (the sample has no quoted fields apart from those tested).
    fn rows(text: &str) -> Vec<Vec<String>> {
        let body = text
            .strip_prefix('\u{feff}')
            .unwrap()
            .strip_prefix(CSV_HEADER)
            .unwrap();
        assert!(body.is_empty() || body.ends_with("\r\n"));
        body.split_terminator("\r\n")
            .map(|r| r.split(',').map(str::to_owned).collect())
            .collect()
    }

    fn path(parts: &[&str]) -> String {
        let mut p = std::path::PathBuf::from("root");
        p.extend(parts);
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn fields_are_quoted_and_guarded() {
        let field = |s: &str| {
            let mut out = String::new();
            push_field(&mut out, s);
            out
        };
        assert_eq!(field("plain name.txt"), "plain name.txt");
        assert_eq!(field("a,b"), "\"a,b\"");
        assert_eq!(field("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(field("two\nlines"), "\"two\nlines\"");
        assert_eq!(field("cr\rhere"), "\"cr\rhere\"");
        assert_eq!(field("=SUM(A1)"), "'=SUM(A1)");
        assert_eq!(field("+1"), "'+1");
        assert_eq!(field("-rf"), "'-rf");
        assert_eq!(field("@home"), "'@home");
        assert_eq!(field("\tx"), "'\tx");
        assert_eq!(field("=a,b"), "\"'=a,b\"", "guarded inside the quotes");
        assert_eq!(field("\rx"), "\"'\rx\"");
        assert_eq!(field("a=b"), "a=b", "only a leading character counts");
        assert_eq!(field("grüße 日本.txt"), "grüße 日本.txt");
    }

    #[test]
    fn contents_lists_the_folders_own_items_largest_first() {
        let t = sample();
        let text = csv(&t, None, NodeId::ROOT, Scope::Contents);
        assert!(text.starts_with('\u{feff}'));
        let rows = rows(&text);
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[0],
            [
                path(&["big.jpg"]),
                "big.jpg".into(),
                "file".into(),
                "1000".into(),
                "1000 B".into(),
                "".into(),
                "image".into(),
                "62.50".into(),
                "2023-11-14T22:13:20Z".into(),
                "".into()
            ]
        );
        assert_eq!(
            rows[1],
            [
                path(&["docs"]),
                "docs".into(),
                "folder".into(),
                "500".into(),
                "500 B".into(),
                "2".into(),
                "".into(),
                "31.25".into(),
                "".into(),
                "".into()
            ]
        );
        assert_eq!(rows[2][1], "'=cmd.txt", "formula guard on the name");
        assert_eq!(rows[2][6], "document");
        assert_eq!(rows[2][8], "", "a time before 1970 is only known as early");
    }

    #[test]
    fn everything_lists_every_level_folders_before_their_contents() {
        let t = sample();
        let text = csv(&t, None, NodeId::ROOT, Scope::Everything);
        let names: Vec<String> = rows(&text).into_iter().map(|r| r[1].clone()).collect();
        assert_eq!(
            names,
            ["big.jpg", "docs", "a.txt", "deep", "b.jpg", "'=cmd.txt"]
        );
        let docs = t
            .children(NodeId::ROOT)
            .find(|&c| t.name(c) == "docs")
            .unwrap();
        let text = csv(&t, None, docs, Scope::Everything);
        let rows = rows(&text);
        let names: Vec<&str> = rows.iter().map(|r| r[1].as_str()).collect();
        assert_eq!(names, ["a.txt", "deep", "b.jpg"]);
        assert_eq!(rows[0][7], "60.00", "share of the exported folder");
        assert_eq!(rows[2][0], path(&["docs", "deep", "b.jpg"]));
    }

    #[test]
    fn filtered_export_lists_matches_with_matching_totals() {
        let t = sample();
        let mut s = Search::new(Matcher::new("*.jpg").unwrap());
        assert!(s.catch_up(&t, usize::MAX));
        let text = csv(&t, Some(&s), NodeId::ROOT, Scope::Everything);
        let all = rows(&text);
        let summary: Vec<(&str, &str, &str, &str)> = all
            .iter()
            .map(|r| (r[1].as_str(), r[3].as_str(), r[5].as_str(), r[7].as_str()))
            .collect();
        // a.txt and =cmd.txt don't match; docs and deep hold b.jpg only.
        assert_eq!(
            summary,
            [
                ("big.jpg", "1000", "", "83.33"),
                ("docs", "200", "1", "16.67"),
                ("deep", "200", "1", "16.67"),
                ("b.jpg", "200", "", "16.67"),
            ]
        );
        let text = csv(&t, Some(&s), NodeId::ROOT, Scope::Contents);
        assert_eq!(rows(&text).len(), 2, "docs and big.jpg");
    }

    #[test]
    fn names_with_separators_stay_one_field() {
        let mut t = Tree::new(std::path::Path::new("root"));
        file(&mut t, NodeId::ROOT, "a, \"b\".txt", 10);
        let text = csv(&t, None, NodeId::ROOT, Scope::Contents);
        let line = text.split("\r\n").nth(1).unwrap();
        assert!(line.contains(",\"a, \"\"b\"\".txt\",file,10,"), "{line}");
    }

    #[test]
    fn removed_items_are_skipped_between_steps() {
        let mut t = sample();
        let mut csv = Csv::new(NodeId::ROOT, Scope::Everything, Metric::Logical);
        let mut out = String::new();
        assert!(!csv.step(&t, None, 1, &mut out));
        let docs = t
            .children(NodeId::ROOT)
            .find(|&c| t.name(c) == "docs")
            .unwrap();
        t.remove(docs);
        while !csv.step(&t, None, 1, &mut out) {}
        let names: Vec<String> = rows(&out).into_iter().map(|r| r[1].clone()).collect();
        assert_eq!(names, ["big.jpg", "'=cmd.txt"]);
        assert_eq!(csv.rows(), 2);
    }

    #[test]
    fn shares_follow_a_growing_folder() {
        let mut t = Tree::new(std::path::Path::new("root"));
        file(&mut t, NodeId::ROOT, "a", 100);
        file(&mut t, NodeId::ROOT, "b", 100);
        let mut csv = Csv::new(NodeId::ROOT, Scope::Everything, Metric::Logical);
        let mut out = String::new();
        assert!(!csv.step(&t, None, 1, &mut out));
        // The scan finds more before the next step.
        file(&mut t, NodeId::ROOT, "c", 800);
        while !csv.step(&t, None, 1, &mut out) {}
        let shares: Vec<String> = rows(&out).into_iter().map(|r| r[7].clone()).collect();
        assert_eq!(
            shares,
            ["50.00", "10.00"],
            "c came after its folder was listed"
        );
    }

    #[test]
    fn report_has_totals_and_the_largest_items() {
        let t = sample();
        let mut s = Search::new(Matcher::new("*.jpg").unwrap());
        assert!(s.catch_up(&t, usize::MAX));
        let info = ReportInfo {
            scan_root: "root".into(),
            scan: "Complete in 1.0 s".into(),
            exported_at: "2026-10-13 12:00".into(),
            filter: Some("*.jpg".into()),
            not_counted: vec![("Permission denied".into(), 1234)],
        };
        let mut report = Report::new(NodeId::ROOT, Metric::Logical).with_kilo(1024);
        while !report.step(&t, Some(&s), 1) {}
        let mut out = Vec::new();
        let listed = report.write(&t, Some(&s), &info, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap().replace("\r\n", "\n");
        assert_eq!(listed, 4, "two folders and two files");
        for expected in [
            "Scan root:  root\n",
            "Scan:       Complete in 1.0 s\n",
            "Exported:   2026-10-13 12:00\n",
            "Filter:     *.jpg (only matching files are counted)\n",
            "Size:       1.17 KB (1,200 bytes)\n",
            "Files:      2\n",
            "Folders:    2\n",
            "Largest folders (2 of 2)\n",
            "Largest files (2 of 2)\n",
            "  Permission denied: 1,234\n",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in\n{text}");
        }
        let big = text.find(&path(&["big.jpg"])).unwrap();
        let b = text.find(&path(&["docs", "deep", "b.jpg"])).unwrap();
        assert!(big < b, "largest first");
        assert!(!text.contains("a.txt"), "non-matching files are left out");
        assert!(text.contains(&format!(
            "   83.3%  2023-11-14 22:13  {}",
            path(&["big.jpg"])
        )));
        assert!(text.contains("Size    Share  Modified          Path\n"));
    }

    #[test]
    fn report_keeps_only_the_largest() {
        let mut t = Tree::new(std::path::Path::new("root"));
        for i in 0..(REPORT_TOP as u64 + 20) {
            file(&mut t, NodeId::ROOT, &format!("f{i}"), i + 1);
        }
        let mut report = Report::new(NodeId::ROOT, Metric::Logical);
        assert!(report.step(&t, None, usize::MAX));
        let mut out = Vec::new();
        let listed = report
            .write(&t, None, &ReportInfo::default(), &mut out)
            .unwrap();
        let text = String::from_utf8(out).unwrap().replace("\r\n", "\n");
        assert_eq!(listed, REPORT_TOP as u64);
        assert!(text.contains("Largest files (50 of 70)"));
        assert!(text.contains(&path(&["f69"])));
        assert!(!text.contains(&format!("{}\n", path(&["f19"]))));
        assert!(text.contains("Nothing was left out."));
    }

    /// root/
    ///   app/ repository on main (src/main.rs 300, .git/pack 100,
    ///        lib/ nested repository (lib.rs 50, .git file 10))
    ///   wt/ linked worktree, detached (a.txt 70)
    ///   broken/ unresolved (x.txt 5)
    ///   loose.txt 1000
    fn repos() -> (Tree, [NodeId; 4]) {
        use crate::git::{GitRoot, Problem};
        let mut t = Tree::new(std::path::Path::new("root"));
        let app = dir(&mut t, NodeId::ROOT, "app");
        let src = dir(&mut t, app, "src");
        file(&mut t, src, "main.rs", 300);
        let dot = dir(&mut t, app, ".git");
        file(&mut t, dot, "pack", 100);
        let lib = dir(&mut t, app, "lib");
        file(&mut t, lib, "lib.rs", 50);
        let pointer = file(&mut t, lib, ".git", 10);
        let wt = dir(&mut t, NodeId::ROOT, "wt");
        file(&mut t, wt, "a.txt", 70);
        let broken = dir(&mut t, NodeId::ROOT, "broken");
        file(&mut t, broken, "x.txt", 5);
        file(&mut t, NodeId::ROOT, "loose.txt", 1000);
        t.set_git(GitRoot {
            head: Some(Head::Branch("main".into())),
            ..GitRoot::new(app, RootKind::Repository)
        });
        t.set_git(GitRoot::new(dot, RootKind::DotGit));
        t.set_git(GitRoot::new(lib, RootKind::Repository));
        t.set_git(GitRoot::new(pointer, RootKind::DotGit));
        t.set_git(GitRoot {
            head: Some(Head::Detached("0123456789ab".into())),
            ..GitRoot::new(wt, RootKind::LinkedWorktree)
        });
        t.set_git(GitRoot::unresolved(broken, Problem::NotGit));
        (t, [app, lib, wt, broken])
    }

    #[test]
    fn csv_rows_end_with_the_git_class() {
        let (t, _) = repos();
        let text = csv(&t, None, NodeId::ROOT, Scope::Everything);
        assert!(text.contains(",Modified,Git\r\n"));
        let git: Vec<(String, String)> = rows(&text)
            .into_iter()
            .map(|r| (r[1].clone(), r[9].clone()))
            .collect();
        let of = |name: &str| {
            let found: Vec<&str> = git
                .iter()
                .filter(|(n, _)| n == name)
                .map(|(_, g)| g.as_str())
                .collect();
            found.join("|")
        };
        assert_eq!(of("loose.txt"), "");
        assert_eq!(of("app"), "checkout");
        assert_eq!(of("main.rs"), "checkout");
        assert_eq!(of("pack"), "metadata");
        assert_eq!(of(".git"), "metadata|metadata", "the folder and the file");
        assert_eq!(of("lib.rs"), "checkout");
        assert_eq!(of("a.txt"), "worktree");
        assert_eq!(of("broken"), "", "unresolved has no class of its own");
        assert_eq!(of("x.txt"), "");
    }

    #[test]
    fn report_lists_git_repositories_counting_each_byte_once() {
        let (t, [app, ..]) = repos();
        let report = |folder, filter: Option<&Search>| {
            let mut report = Report::new(folder, Metric::Logical).with_kilo(1024);
            while !report.step(&t, filter, 3) {}
            let mut out = Vec::new();
            report
                .write(&t, filter, &ReportInfo::default(), &mut out)
                .unwrap();
            String::from_utf8(out).unwrap().replace("\r\n", "\n")
        };
        let text = report(NodeId::ROOT, None);
        assert!(text.contains("Git repositories (4 of 4)\n"), "{text}");
        // The branch column is as wide as its longest entry.
        let branch = |s: &str| format!("{s:<24}");
        assert!(text.contains(&format!(
            "      Size  Kind              {}  Path\n",
            branch("Branch")
        )));
        assert!(text.contains(&format!(
            "     460 B  repository        {}  {}\n",
            branch("main"),
            path(&["app"])
        )));
        assert!(text.contains(&format!(
            "      70 B  linked worktree   {}  {}\n",
            branch("detached at 0123456789ab"),
            path(&["wt"])
        )));
        assert!(text.contains(&format!(
            "       5 B  unresolved        {}  {} (folder named .git without Git data)\n",
            branch(""),
            path(&["broken"])
        )));
        let app_at = text.find(&path(&["app"])).unwrap();
        let lib_at = text
            .find(&format!("  {}\n", path(&["app", "lib"])))
            .unwrap();
        assert!(app_at < lib_at, "largest first");
        // app (460, its nested lib included) and wt (70), each byte once;
        // Git data is app's .git (100) and lib's .git file (10).
        assert!(text.contains(
            "In Git repositories under this folder: 530 B (Git data 110 B), nested repositories counted once.\n"
        ));

        // Only the repositories under the folder, the folder's own included.
        let text = report(app, None);
        assert!(text.contains("Git repositories (2 of 2)\n"), "{text}");
        assert!(!text.contains(&path(&["wt"])));
        assert!(text.contains("under this folder: 460 B (Git data 110 B)"));
        // Inside a repository but holding none: no section, as its
        // repository isn't under it.
        let src = t.children(app).find(|&c| t.name(c) == "src").unwrap();
        assert!(!report(src, None).contains("Git"));

        // With a filter, matching sizes, and only roots with matches.
        let mut s = Search::new(Matcher::new("*.rs").unwrap());
        assert!(s.catch_up(&t, usize::MAX));
        let text = report(NodeId::ROOT, Some(&s));
        assert!(text.contains("Git repositories (2 of 2)\n"), "{text}");
        assert!(text.contains(&format!(
            "     350 B  repository        main    {}\n",
            path(&["app"])
        )));
        assert!(text.contains(
            "In Git repositories under this folder: 350 B matching, nested repositories counted once.\n"
        ));

        // Nothing about Git without repositories.
        let plain = sample();
        let mut r = Report::new(NodeId::ROOT, Metric::Logical);
        assert!(r.step(&plain, None, usize::MAX));
        let mut out = Vec::new();
        r.write(&plain, None, &ReportInfo::default(), &mut out)
            .unwrap();
        assert!(!String::from_utf8(out).unwrap().contains("Git"));
    }

    #[test]
    fn times_convert_to_utc_dates() {
        assert_eq!(iso_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_utc(WHEN as i64), "2023-11-14T22:13:20Z");
        assert_eq!(iso_utc(951_782_400), "2000-02-29T00:00:00Z", "leap day");
        assert_eq!(iso_utc(i64::from(u32::MAX) - 1), "2106-02-07T06:28:14Z");
        assert_eq!(short_utc(WHEN as i64), "2023-11-14 22:13");
    }

    #[test]
    fn sizes_and_counts_read_like_the_interface() {
        assert_eq!(format_bytes(0, 1024), "0 B");
        assert_eq!(format_bytes(1023, 1024), "1023 B");
        assert_eq!(format_bytes(1536, 1024), "1.50 KB");
        assert_eq!(format_bytes(1_000_000, 1000), "1.00 MB");
        assert_eq!(format_bytes(1023 * 1024 + 1000, 1024), "1.00 MB");
        assert_eq!(format_bytes(150 * 1024 * 1024, 1024), "150 MB");
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1000), "1,000");
        assert_eq!(format_count(12_345_678), "12,345,678");
    }
}
