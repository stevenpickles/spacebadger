//! Saving what's under a folder to a file, as CSV or a plain-text report.
//!
//! The file is written in steps: each takes the search and tree locks just
//! long enough to format up to [`EXPORT_STEP`] items into a buffer, which is
//! written to disk with the locks released. A running scan, the map, and
//! the lists carry on in between, and memory stays bounded however many
//! rows there are.
//!
//! The file is first written to a new temporary file beside the target
//! (see [`crate::atomic_file`]) and renamed over it only once complete, so
//! a failed export never destroys the file the user chose to replace, and
//! never touches any other. Between steps the export stops if a new
//! scan has replaced this one, or if the filter it started with is gone;
//! re-applying the same filter text doesn't count as a change.

use super::{ActiveSearch, Session, display_path, metric, node};
use crate::atomic_file::TempFile;
use chrono::{SecondsFormat, TimeZone};
use sb_core::export::{Csv, Report, ReportInfo, Scope, iso_utc, short_utc};
use sb_core::search::Search;
use sb_core::tree::{NodeId, Tree};
use sb_protocol as proto;
use sb_scan::{OmissionReason, Progress, ScanState};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::Duration;

/// Items formatted per hold of the locks: a few megabytes of text, formatted
/// in milliseconds.
const EXPORT_STEP: usize = 50_000;

const FILTER_CHANGED: &str = "The filter changed while exporting. Export again.";

const REPLACED: &str = "A new scan started, so the export was stopped.";

/// Characters no file name may hold on any of the supported systems.
const UNSAFE: &[char] = &['\\', '/', ':', '*', '?', '"', '<', '>', '|'];

/// What one export needs at every step.
struct Job<'a> {
    req: &'a proto::ExportRequest,
    /// The filter's text when the export started.
    query: Option<String>,
    /// Whether this session is still the app's current scan.
    current: &'a dyn Fn() -> bool,
}

impl Session {
    /// Writes the export `req` asks for to `path`, calling `progress` with
    /// 0 once the file is created and then with the rows written (items
    /// examined, for a report) after each step. Stops when `current` says
    /// this session was replaced. An export that fails leaves `path` as it
    /// was.
    pub fn export(
        &self,
        req: &proto::ExportRequest,
        path: &Path,
        current: impl Fn() -> bool,
        progress: impl Fn(u64),
    ) -> Result<proto::ExportReport, String> {
        let query = match req.search {
            Some(id) => Some(
                self.search
                    .lock()
                    .map_err(|_| "search is unavailable")?
                    .as_ref()
                    .filter(|a| a.id == id)
                    .map(|a| a.search.matcher().query().to_owned())
                    .ok_or(FILTER_CHANGED)?,
            ),
            None => None,
        };
        let job = Job {
            req,
            query,
            current: &current,
        };
        let shown = display_path(path);
        if path.file_name().is_none() {
            return Err(format!("{shown} isn't a file name."));
        }
        let failed = |e: std::io::Error| format!("Couldn't write {shown}: {e}");
        // Removed when dropped unless persisted, however the export ends.
        let temp = TempFile::beside(path).map_err(|e| format!("Couldn't create {shown}: {e}"))?;
        progress(0);
        let mut out = BufWriter::new(temp.file());
        let rows = match req.format {
            proto::ExportFormat::Csv => self.export_csv(&job, &mut out, &progress, &failed),
            proto::ExportFormat::Report => self.export_report(&job, &mut out, &progress, &failed),
        }?;
        out.into_inner().map_err(|e| failed(e.into_error()))?;
        temp.persist().map_err(|e| {
            // Typically Windows refusing to replace a file in use: access
            // denied, or a sharing violation (32) renaming through a handle.
            if e.kind() == std::io::ErrorKind::PermissionDenied
                || (cfg!(windows) && e.raw_os_error() == Some(32))
            {
                format!("Couldn't replace {shown}: it may be open in another program or read-only.")
            } else {
                failed(e)
            }
        })?;
        Ok(proto::ExportReport {
            generation: self.generation(),
            path: shown,
            format: req.format,
            rows,
        })
    }

    /// The name to suggest in the save dialog: only the last part of what
    /// the interface asked for, without characters file names can't hold,
    /// and with the format's extension.
    pub fn export_file_name(requested: &str, format: proto::ExportFormat) -> String {
        let ext = match format {
            proto::ExportFormat::Csv => "csv",
            proto::ExportFormat::Report => "txt",
        };
        let last = requested.rsplit(['/', '\\']).next().unwrap_or("");
        let clean: String = last
            .chars()
            .map(|c| {
                if UNSAFE.contains(&c) || c.is_control() {
                    '-'
                } else {
                    c
                }
            })
            .collect();
        let stem = match clean.rsplit_once('.') {
            Some((stem, old)) if old.eq_ignore_ascii_case(ext) => stem,
            _ => clean.as_str(),
        };
        let stem = stem.trim_matches(|c: char| c.is_whitespace() || c == '.');
        let stem = if stem.is_empty() { "SpaceBadger" } else { stem };
        format!("{stem}.{ext}")
    }

    fn export_csv(
        &self,
        job: &Job<'_>,
        out: &mut impl Write,
        progress: &impl Fn(u64),
        failed: &impl Fn(std::io::Error) -> String,
    ) -> Result<u64, String> {
        let req = job.req;
        let scope = match req.scope {
            proto::ExportScope::Contents => Scope::Contents,
            proto::ExportScope::Everything => Scope::Everything,
        };
        let mut csv = Csv::new(NodeId::from_index(req.folder), scope, metric(req.metric))
            .with_time_format(iso_local);
        let mut text = String::new();
        loop {
            let done = self.export_step(job, |tree, filter| {
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
        job: &Job<'_>,
        out: &mut impl Write,
        progress: &impl Fn(u64),
        failed: &impl Fn(std::io::Error) -> String,
    ) -> Result<u64, String> {
        let req = job.req;
        let mut report = Report::new(NodeId::from_index(req.folder), metric(req.metric))
            .with_time_format(short_local);
        while !self.export_step(job, |tree, filter| report.step(tree, filter, EXPORT_STEP))? {
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
        let listed = self.export_step(job, |tree, filter| {
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
        job: &Job<'_>,
        step: impl FnOnce(&Tree, Option<&Search>) -> R,
    ) -> Result<R, String> {
        if !(job.current)() {
            return Err(REPLACED.into());
        }
        let mut search = match job.query {
            Some(_) => Some(self.search.lock().map_err(|_| "search is unavailable")?),
            None => None,
        };
        let tree = self.scan.tree();
        let tree = tree.read().map_err(|_| "scan data is unavailable")?;
        node(&tree, job.req.folder)?;
        let filter = match (search.as_deref_mut(), &job.query) {
            (Some(slot), Some(query)) => Some(&same_filter(slot, query, &tree)?.search),
            _ => None,
        };
        Ok(step(&tree, filter))
    }
}

/// The active search if its query is `query`, caught up with `tree`. The
/// text is compared rather than the search number, since applying the same
/// filter again starts a new search.
fn same_filter<'a>(
    slot: &'a mut Option<ActiveSearch>,
    query: &str,
    tree: &Tree,
) -> Result<&'a mut ActiveSearch, String> {
    let active = slot
        .as_mut()
        .filter(|a| a.search.matcher().query() == query)
        .ok_or(FILTER_CHANGED)?;
    active.search.catch_up(tree, usize::MAX);
    Ok(active)
}

/// ISO 8601 in local time with its offset, such as
/// `2026-10-13T18:05:09+02:00`; the offset is the one in effect then.
fn iso_local(secs: i64) -> String {
    chrono::Local.timestamp_opt(secs, 0).single().map_or_else(
        || iso_utc(secs),
        |t| t.to_rfc3339_opts(SecondsFormat::Secs, false),
    )
}

/// A short local date and time, such as `2026-10-13 18:05`.
fn short_local(secs: i64) -> String {
    chrono::Local.timestamp_opt(secs, 0).single().map_or_else(
        || short_utc(secs),
        |t| t.format("%Y-%m-%d %H:%M").to_string(),
    )
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
    use crate::atomic_file::retry_while_held;
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
            .export(
                &req,
                &path,
                || true,
                |rows| calls.lock().unwrap().push(rows),
            )
            .unwrap();
        assert_eq!(report.rows, 5, "two folders and three files");
        assert_eq!(report.path, display_path(&path));
        assert_eq!(
            calls.into_inner().unwrap(),
            [0, 5],
            "0 once the file exists"
        );

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
            format!("{},old,folder,8000,{size},2,,98.77,", display_path(&old)),
            "folders have no modified time"
        );
        // A file's: local time with the offset in effect then.
        let a = lines.iter().find(|l| l.contains(",a.log,")).unwrap();
        let modified = a.rsplit(',').next().unwrap();
        let parsed = chrono::DateTime::parse_from_rfc3339(modified).unwrap();
        let written = std::fs::metadata(session.root.join("old").join("logs").join("a.log"))
            .unwrap()
            .modified()
            .unwrap();
        let written = chrono::DateTime::<chrono::Local>::from(written);
        assert_eq!(parsed.timestamp(), written.timestamp());
        assert_eq!(
            parsed.offset().local_minus_utc(),
            written.offset().local_minus_utc()
        );
        assert!(!modified.ends_with('Z'), "{modified}");
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
        let report = session.export(&req, &path, || true, |_| {}).unwrap();
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

        // The same filter applied again is a new search, but not a change.
        let again = session.search_set("a.log").unwrap().unwrap().search;
        assert_ne!(again, search);
        req.search = Some(again);
        assert_eq!(
            session.export(&req, &path, || true, |_| {}).unwrap().rows,
            3
        );

        // A different filter: the file already there is kept as it was,
        // and no partial file is left behind.
        session.search_set("keep").unwrap();
        let err = session.export(&req, &path, || true, |_| {}).unwrap_err();
        assert_eq!(err, FILTER_CHANGED);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        assert_eq!(std::fs::read_dir(tmp.0.join("out")).unwrap().count(), 1);
    }

    #[test]
    fn a_filter_changed_between_steps_stops_the_export() {
        let (tmp, session) = scanned("midway");
        session.search_set("*.log").unwrap();
        let path = tmp.0.join("out").join("report.txt");
        std::fs::write(&path, "keep me").unwrap();
        let mut req = request(&session, proto::ExportFormat::Report);
        let steps = Mutex::new(0);
        // `current` runs before every step: change the filter after the first.
        let mut run = |query: &str| {
            *steps.lock().unwrap() = 0;
            req.search = Some(session.search_set("*.log").unwrap().unwrap().search);
            session.export(
                &req,
                &path,
                || {
                    let mut n = steps.lock().unwrap();
                    *n += 1;
                    if *n == 2 {
                        session.search_set(query).unwrap();
                    }
                    true
                },
                |_| {},
            )
        };
        assert_eq!(run("keep").unwrap_err(), FILTER_CHANGED);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "keep me");
        assert!(run("*.log").is_ok(), "the same text applied again");
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .starts_with("SpaceBadger report")
        );
        assert_eq!(std::fs::read_dir(tmp.0.join("out")).unwrap().count(), 1);
    }

    #[test]
    fn a_replaced_scan_stops_the_export() {
        let (tmp, session) = scanned("replaced");
        let path = tmp.0.join("out").join("all.csv");
        let req = request(&session, proto::ExportFormat::Csv);
        let err = session.export(&req, &path, || false, |_| {}).unwrap_err();
        assert_eq!(err, REPLACED);
        assert!(!path.exists());
        assert_eq!(std::fs::read_dir(tmp.0.join("out")).unwrap().count(), 0);
    }

    #[test]
    fn files_beside_the_target_are_left_alone() {
        let (tmp, session) = scanned("beside");
        let out = tmp.0.join("out");
        let path = out.join("all.csv");
        // The name earlier versions wrote to first, as someone else's file.
        let old_temp = out.join(".all.csv.partial");
        std::fs::write(&old_temp, "not ours").unwrap();
        let req = request(&session, proto::ExportFormat::Csv);
        session.export(&req, &path, || true, |_| {}).unwrap();
        assert_eq!(std::fs::read_to_string(&old_temp).unwrap(), "not ours");
        assert!(std::fs::read_to_string(&path).unwrap().contains(",a.log,"));
        let err = session.export(&req, &path, || false, |_| {}).unwrap_err();
        assert_eq!(err, REPLACED);
        assert_eq!(std::fs::read_to_string(&old_temp).unwrap(), "not ours");
        assert_eq!(std::fs::read_dir(&out).unwrap().count(), 2);
    }

    #[test]
    fn concurrent_exports_to_one_file_both_finish() {
        let (tmp, session) = scanned("concurrent");
        let path = tmp.0.join("out").join("same.txt");
        let csv = request(&session, proto::ExportFormat::Csv);
        let report = request(&session, proto::ExportFormat::Report);
        std::thread::scope(|s| {
            for req in [&csv, &report, &csv, &report] {
                let (session, path) = (&session, &path);
                // As in `atomic_file`, another program may hold the file.
                let export = || session.export(req, path, || true, |_| {});
                s.spawn(move || {
                    retry_while_held(export, |e: &String| {
                        cfg!(windows) && e.starts_with("Couldn't replace ")
                    })
                    .unwrap()
                });
            }
        });
        let text = std::fs::read_to_string(&path).unwrap();
        let csv_whole = text.starts_with('\u{feff}') && text.matches("Path,Name,").count() == 1;
        let report_whole = text.starts_with("SpaceBadger report") && text.ends_with('\n');
        assert!(
            csv_whole || report_whole,
            "one export's whole file:\n{text}"
        );
        assert_eq!(std::fs::read_dir(tmp.0.join("out")).unwrap().count(), 1);
    }

    #[test]
    fn suggested_names_are_plain_file_names() {
        use proto::ExportFormat::{Csv, Report};
        let name = Session::export_file_name;
        assert_eq!(
            name("SpaceBadger-Downloads-2026-10-13.csv", Csv),
            "SpaceBadger-Downloads-2026-10-13.csv"
        );
        assert_eq!(name("a.CSV", Csv), "a.csv");
        assert_eq!(name("report.csv", Report), "report.csv.txt");
        assert_eq!(name("..\\..\\Windows\\evil.exe", Csv), "evil.exe.csv");
        assert_eq!(name("/etc/passwd", Report), "passwd.txt");
        assert_eq!(name("a:b*c?\"<>|.csv", Csv), "a-b-c-----.csv");
        assert_eq!(name("x\u{7}y", Csv), "x-y.csv");
        assert_eq!(name("", Csv), "SpaceBadger.csv");
        assert_eq!(name(" .. ", Report), "SpaceBadger.txt");
    }

    #[test]
    fn writes_a_report() {
        let (tmp, session) = scanned("report");
        let search = session.search_set("*.log").unwrap().unwrap().search;
        let path = tmp.0.join("out").join("report.txt");
        let mut req = request(&session, proto::ExportFormat::Report);
        req.search = Some(search);
        let report = session.export(&req, &path, || true, |_| {}).unwrap();
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

    #[cfg(windows)]
    #[test]
    fn a_file_in_use_is_reported_plainly() {
        use std::os::windows::fs::OpenOptionsExt;
        let (tmp, session) = scanned("in-use");
        let path = tmp.0.join("out").join("open.csv");
        std::fs::write(&path, "in use").unwrap();
        // As a spreadsheet holds a file open: no sharing.
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&path)
            .unwrap();
        let req = request(&session, proto::ExportFormat::Csv);
        let err = session.export(&req, &path, || true, |_| {}).unwrap_err();
        assert_eq!(
            err,
            format!(
                "Couldn't replace {}: it may be open in another program or read-only.",
                display_path(&path)
            )
        );
        drop(held);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "in use");
        assert_eq!(std::fs::read_dir(tmp.0.join("out")).unwrap().count(), 1);
    }

    #[test]
    fn an_unwritable_path_is_reported() {
        let (tmp, session) = scanned("unwritable");
        let path = tmp.0.join("missing").join("x.csv");
        let req = request(&session, proto::ExportFormat::Csv);
        let err = session.export(&req, &path, || true, |_| {}).unwrap_err();
        assert!(err.starts_with("Couldn't create "), "{err}");
    }

    #[test]
    fn local_times_carry_their_offset() {
        let secs = 1_700_000_000;
        let t = chrono::Local.timestamp_opt(secs, 0).unwrap();
        assert_eq!(
            iso_local(secs),
            t.to_rfc3339_opts(SecondsFormat::Secs, false)
        );
        assert_eq!(
            chrono::DateTime::parse_from_rfc3339(&iso_local(secs))
                .unwrap()
                .timestamp(),
            secs
        );
        assert_eq!(short_local(secs), t.format("%Y-%m-%d %H:%M").to_string());
    }

    #[test]
    fn durations_read_like_the_status_bar() {
        assert_eq!(elapsed(Duration::from_millis(12_340)), "12.3 s");
        assert_eq!(elapsed(Duration::from_secs(245)), "4:05");
        assert_eq!(elapsed(Duration::from_secs(3723)), "1:02:03");
    }
}
