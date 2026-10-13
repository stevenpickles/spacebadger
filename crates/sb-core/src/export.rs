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
//! CSV follows RFC 4180: fields with commas, quotes, or line breaks are
//! quoted, with quotes doubled, and rows end in CRLF. The file starts with a
//! UTF-8 byte order mark, so spreadsheet apps read non-ASCII names
//! correctly. Sizes are given as whole bytes and again for display. Text
//! that starts with `=`, `+`, `-`, `@`, a tab, or a carriage return gets a
//! leading `'`, so a spreadsheet shows a name like `=1+2` as text instead of
//! running it as a formula.
//!
//! Names and paths that aren't valid Unicode are written with replacement
//! characters.

use crate::filetype::FileType;
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
    rows: u64,
}

/// The CSV header row.
pub const CSV_HEADER: &str =
    "Path,Name,Type,Size (bytes),Size,Files,File type,Share of folder (%)\r\n";

impl Csv {
    pub fn new(folder: NodeId, scope: Scope, metric: Metric) -> Self {
        Self {
            walk: Walk::new(folder, metric, scope == Scope::Everything),
            kilo: SYSTEM_KILO,
            rows: 0,
        }
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
        let (metric, kilo) = (self.walk.metric, self.kilo);
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
        let row = |n: NodeId, files: Option<u64>| {
            let size = metric.weight_in(tree, filter, n);
            let files = files.map_or(String::new(), format_count);
            format!(
                "{:>10}  {:>6}%  {:>10}  {}",
                format_bytes(size, kilo),
                share_short(size, whole),
                files,
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
                line(&row(n, Some(files_in(tree, filter, n))));
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
            line(&format!("{:>10}  {:>7}  {:>10}  Path", "Size", "Share", ""));
            for &n in &files {
                line(&row(n, None));
                listed += 1;
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

    fn file(tree: &mut Tree, parent: NodeId, name: &str, bytes: u64) -> NodeId {
        let sizes = FileSizes {
            logical: bytes,
            allocated: Allocated::known(bytes),
        };
        tree.add_file(parent, OsStr::new(name), sizes, 0).unwrap()
    }

    fn dir(tree: &mut Tree, parent: NodeId, name: &str) -> NodeId {
        let d = tree.add_dir(parent, OsStr::new(name)).unwrap();
        tree.set_dir_state(d, DirState::Listed);
        d
    }

    /// root/
    ///   docs/ (a.txt 300, deep/ (b.jpg 200))
    ///   big.jpg 1000
    ///   =cmd.txt 100
    fn sample() -> Tree {
        let mut t = Tree::new(std::path::Path::new("root"));
        let docs = dir(&mut t, NodeId::ROOT, "docs");
        file(&mut t, docs, "a.txt", 300);
        let deep = dir(&mut t, docs, "deep");
        file(&mut t, deep, "b.jpg", 200);
        file(&mut t, NodeId::ROOT, "big.jpg", 1000);
        file(&mut t, NodeId::ROOT, "=cmd.txt", 100);
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
                "62.50".into()
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
                "31.25".into()
            ]
        );
        assert_eq!(rows[2][1], "'=cmd.txt", "formula guard on the name");
        assert_eq!(rows[2][6], "document");
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
        assert!(text.contains(&format!("   83.3%              {}", path(&["big.jpg"]))));
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
