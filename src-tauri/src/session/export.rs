//! Saving what's under a folder to a file, as CSV or a plain-text report.
//!
//! The file is written in steps: each takes the search and tree locks just
//! long enough to format up to [`EXPORT_STEP`] items into a buffer, which is
//! written to disk with the locks released. A running scan, the map, and
//! the lists carry on in between, and memory stays bounded however many
//! rows there are.

use super::{Session, current_search, display_path, metric, node};
use sb_core::export::{Csv, Report, ReportInfo, Scope};
use sb_core::search::Search;
use sb_core::tree::{NodeId, Tree};
use sb_protocol as proto;
use sb_scan::{OmissionReason, Progress, ScanState};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::Duration;

/// Items formatted per hold of the locks: a few megabytes of text, formatted
/// in milliseconds.
const EXPORT_STEP: usize = 50_000;

const FILTER_CHANGED: &str = "The filter changed while exporting. Export again.";

impl Session {
    /// Writes the export `req` asks for to `path`, calling `progress` with
    /// the rows written (items examined, for a report) after each step. An
    /// export that fails leaves no partial file behind.
    pub fn export(
        &self,
        req: &proto::ExportRequest,
        path: &Path,
        progress: impl Fn(u64),
    ) -> Result<proto::ExportReport, String> {
        let shown = display_path(path);
        let failed = |e: std::io::Error| format!("Couldn't write {shown}: {e}");
        let file = File::create(path).map_err(|e| format!("Couldn't create {shown}: {e}"))?;
        let mut out = BufWriter::new(file);
        let written = match req.format {
            proto::ExportFormat::Csv => self.export_csv(req, &mut out, &progress, &failed),
            proto::ExportFormat::Report => self.export_report(req, &mut out, &progress, &failed),
        }
        .and_then(|rows| out.flush().map(|()| rows).map_err(failed));
        match written {
            Ok(rows) => Ok(proto::ExportReport {
                generation: self.generation(),
                path: shown,
                format: req.format,
                rows,
            }),
            Err(e) => {
                drop(out);
                let _ = std::fs::remove_file(path);
                Err(e)
            }
        }
    }

    fn export_csv(
        &self,
        req: &proto::ExportRequest,
        out: &mut impl Write,
        progress: &impl Fn(u64),
        failed: &impl Fn(std::io::Error) -> String,
    ) -> Result<u64, String> {
        let scope = match req.scope {
            proto::ExportScope::Contents => Scope::Contents,
            proto::ExportScope::Everything => Scope::Everything,
        };
        let mut csv = Csv::new(NodeId::from_index(req.folder), scope, metric(req.metric));
        let mut text = String::new();
        loop {
            let done = self.export_step(req, |tree, filter| {
                csv.step(tree, filter, EXPORT_STEP, &mut text)
            })?;
            out.write_all(text.as_bytes()).map_err(failed)?;
            text.clear();
            progress(csv.rows());
            if done {
                return Ok(csv.rows());
            }
        }
    }

    fn export_report(
        &self,
        req: &proto::ExportRequest,
        out: &mut impl Write,
        progress: &impl Fn(u64),
        failed: &impl Fn(std::io::Error) -> String,
    ) -> Result<u64, String> {
        let mut report = Report::new(NodeId::from_index(req.folder), metric(req.metric));
        while !self.export_step(req, |tree, filter| report.step(tree, filter, EXPORT_STEP))? {
            progress(report.examined());
        }
        let scan = self.scan.progress();
        let mut info = ReportInfo {
            scan_root: display_path(&self.root),
            scan: scan_summary(&scan),
            exported_at: req.exported_at.clone(),
            filter: None,
            not_counted: scan
                .omissions
                .iter()
                .map(|(reason, count)| (omission_label(reason), count))
                .collect(),
        };
        let mut text = Vec::new();
        let listed = self.export_step(req, |tree, filter| {
            info.filter = filter.map(|s| s.matcher().query().to_owned());
            report.write(tree, filter, &info, &mut text)
        })?;
        let listed = listed.map_err(failed)?;
        out.write_all(&text).map_err(failed)?;
        Ok(listed)
    }

    /// Runs one step of an export with the tree and, when filtered, the
    /// search caught up with it. The search lock is taken only for a
    /// filtered export, so an unfiltered one never holds up the filtered map.
    fn export_step<R>(
        &self,
        req: &proto::ExportRequest,
        step: impl FnOnce(&Tree, Option<&Search>) -> R,
    ) -> Result<R, String> {
        let mut search = match req.search {
            Some(_) => Some(self.search.lock().map_err(|_| "search is unavailable")?),
            None => None,
        };
        let tree = self.scan.tree();
        let tree = tree.read().map_err(|_| "scan data is unavailable")?;
        node(&tree, req.folder)?;
        let filter = match (search.as_deref_mut(), req.search) {
            (Some(active), Some(id)) => Some(
                &current_search(active, id, &tree)
                    .map_err(|_| FILTER_CHANGED)?
                    .search,
            ),
            _ => None,
        };
        Ok(step(&tree, filter))
    }
}

/// How the scan went, for a report's header.
fn scan_summary(p: &Progress) -> String {
    let took = elapsed(p.elapsed);
    match &p.state {
        ScanState::Scanning => {
            format!("Still scanning ({took} so far): a partial result, so sizes are lower bounds")
        }
        ScanState::Complete => format!("Complete in {took}"),
        ScanState::CompleteWithOmissions => {
            format!("Complete in {took}; some items weren't counted (see below)")
        }
        ScanState::Cancelled => {
            format!("Cancelled after {took}: a partial result, so sizes are lower bounds")
        }
        ScanState::Failed(message) => format!("Failed after {took}: {message}"),
    }
}

/// "12.3 s", "4:05", or "1:02:03", as the status bar shows it.
fn elapsed(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        return format!("{:.1} s", d.as_secs_f64());
    }
    let (h, m, s) = (s / 3600, s / 60 % 60, s % 60);
    if h == 0 {
        format!("{m}:{s:02}")
    } else {
        format!("{h}:{m:02}:{s:02}")
    }
}

/// The reason's title under Not counted in the interface.
fn omission_label(reason: OmissionReason) -> String {
    let title = match reason {
        OmissionReason::PermissionDenied => "Permission denied",
        OmissionReason::Vanished => "Removed during the scan",
        OmissionReason::Disconnected => "Drive or share disconnected",
        OmissionReason::Unavailable => "Size unavailable",
        OmissionReason::Other => "Other errors",
        OmissionReason::Symlink => "Symbolic links",
        OmissionReason::MountPoint => "Junctions and mounted drives",
        OmissionReason::SpecialFile => "Special files",
        OmissionReason::UnknownReparse => "Unrecognized redirections",
    };
    if reason.is_policy() {
        format!("{title} (skipped on purpose)")
    } else {
        title.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sb_core::export::{SYSTEM_KILO, format_bytes};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    struct TempDir(PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A scanned folder holding `old/logs/a.log` (5000 bytes),
    /// `old/b, q.log` (3000), and `=keep.txt` (100); exports go to a
    /// sibling folder.
    fn scanned(name: &str) -> (TempDir, Session) {
        let base = std::env::temp_dir().join(format!("sb-export-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let dir = base.join("scan");
        std::fs::create_dir_all(dir.join("old").join("logs")).unwrap();
        std::fs::create_dir_all(base.join("out")).unwrap();
        std::fs::write(dir.join("old").join("logs").join("a.log"), vec![0; 5000]).unwrap();
        std::fs::write(dir.join("old").join("b, q.log"), vec![0; 3000]).unwrap();
        std::fs::write(dir.join("=keep.txt"), vec![0; 100]).unwrap();
        let done = Arc::new(Mutex::new(false));
        let sink = Arc::clone(&done);
        let session = Session::start(dir, move |s| {
            if s.phase != proto::ScanPhase::Scanning {
                *sink.lock().unwrap() = true;
            }
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while !*done.lock().unwrap() {
            assert!(Instant::now() < deadline, "scan never finished");
            std::thread::sleep(Duration::from_millis(5));
        }
        (TempDir(base), session)
    }

    fn request(session: &Session, format: proto::ExportFormat) -> proto::ExportRequest {
        proto::ExportRequest {
            generation: session.generation(),
            folder: 0,
            scope: proto::ExportScope::Everything,
            format,
            metric: proto::Metric::Logical,
            search: None,
            file_name: String::new(),
            exported_at: "2026-10-13 09:30".into(),
        }
    }

    #[test]
    fn writes_a_csv_file() {
        let (tmp, session) = scanned("csv");
        let path = tmp.0.join("out").join("all.csv");
        let calls = Mutex::new(Vec::new());
        let req = request(&session, proto::ExportFormat::Csv);
        let report = session
            .export(&req, &path, |rows| calls.lock().unwrap().push(rows))
            .unwrap();
        assert_eq!(report.rows, 5, "two folders and three files");
        assert_eq!(report.path, display_path(&path));
        assert_eq!(calls.into_inner().unwrap().last(), Some(&5));

        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.starts_with(b"\xEF\xBB\xBF"), "UTF-8 byte order mark");
        let text = String::from_utf8(bytes).unwrap();
        let lines: Vec<&str> = text.trim_start_matches('\u{feff}').split("\r\n").collect();
        assert_eq!(lines.len(), 7, "header, five rows, and the final CRLF");
        assert!(lines[0].starts_with("Path,Name,Type,Size (bytes)"));
        let old = session.root.join("old");
        let size = format_bytes(8000, SYSTEM_KILO);
        assert_eq!(
            lines[1],
            format!("{},old,folder,8000,{size},2,,98.77", display_path(&old))
        );
        assert!(
            text.contains(",\"b, q.log\",file,3000,"),
            "a name with a comma is one quoted field"
        );
        assert!(text.contains(",'=keep.txt,file,100,"), "formula guard");
    }

    #[test]
    fn exports_only_what_the_filter_matches() {
        let (tmp, session) = scanned("filtered");
        let search = session.search_set("a.log").unwrap().unwrap().search;
        let path = tmp.0.join("out").join("logs.csv");
        let mut req = request(&session, proto::ExportFormat::Csv);
        req.search = Some(search);
        let report = session.export(&req, &path, |_| {}).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let names: Vec<&str> = text
            .split("\r\n")
            .skip(1)
            .filter(|l| !l.is_empty())
            .map(|l| l.split(',').nth(1).unwrap())
            .collect();
        assert_eq!(names, ["old", "logs", "a.log"]);
        assert_eq!(report.rows, 3);
        assert!(text.contains(",old,folder,5000,"), "matching totals");

        // A filter replaced meanwhile: nothing is left behind.
        session.search_set("keep").unwrap();
        let err = session.export(&req, &path, |_| {}).unwrap_err();
        assert_eq!(err, FILTER_CHANGED);
        assert!(!path.exists());
    }

    #[test]
    fn writes_a_report() {
        let (tmp, session) = scanned("report");
        let search = session.search_set("*.log").unwrap().unwrap().search;
        let path = tmp.0.join("out").join("report.txt");
        let mut req = request(&session, proto::ExportFormat::Report);
        req.search = Some(search);
        let report = session.export(&req, &path, |_| {}).unwrap();
        assert_eq!(report.rows, 4, "two folders and two files");
        let text = std::fs::read_to_string(&path)
            .unwrap()
            .replace("\r\n", "\n");
        assert!(text.starts_with("SpaceBadger report\n"));
        for expected in [
            format!("Scan root:  {}\n", display_path(&session.root)),
            "Scan:       Complete in ".to_owned(),
            "Exported:   2026-10-13 09:30\n".to_owned(),
            "Filter:     *.log (only matching files are counted)\n".to_owned(),
            "Files:      2\n".to_owned(),
            "Nothing was left out.".to_owned(),
        ] {
            assert!(text.contains(&expected), "missing {expected:?} in\n{text}");
        }
        assert!(!text.contains("keep.txt"));
    }

    #[test]
    fn an_unwritable_path_is_reported() {
        let (tmp, session) = scanned("unwritable");
        let path = tmp.0.join("missing").join("x.csv");
        let req = request(&session, proto::ExportFormat::Csv);
        let err = session.export(&req, &path, |_| {}).unwrap_err();
        assert!(err.starts_with("Couldn't create "), "{err}");
    }

    #[test]
    fn durations_read_like_the_status_bar() {
        assert_eq!(elapsed(Duration::from_millis(12_340)), "12.3 s");
        assert_eq!(elapsed(Duration::from_secs(245)), "4:05");
        assert_eq!(elapsed(Duration::from_secs(3723)), "1:02:03");
    }
}
