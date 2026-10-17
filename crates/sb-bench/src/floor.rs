//! The scan floor: a bare multi-threaded listing of a real folder, with no
//! tree, no aggregation and no hard-link work. Each thread takes a folder,
//! opens it, lists it, counts its entries and queues its subfolders; that
//! is all. Engine time divided by floor time is how far the scanner is from
//! what the filesystem allows.
//!
//! Strategies (see `docs/validation/scan-floor.md`):
//!
//! - `current` (all platforms): the calls the native adapter makes today.
//!   On Windows, `CreateFileW` by `\\?\` path with the adapter's access and
//!   flags and `GetFileInformationByHandleEx(FileIdExtdDirectoryInfo)`
//!   into a 64 KiB buffer. On Linux, `read_dir` and a `statx` per entry
//!   relative to the folder (what `DirEntry::metadata` does); elsewhere
//!   `read_dir` and `symlink_metadata` per entry, as the portable adapter.
//! - `ntdll` (Windows): the same path opens, `NtQueryDirectoryFile` direct.
//! - `relative` (Windows): `NtOpenFile` relative to the parent's handle
//!   with the child's name from the listing, then `NtQueryDirectoryFile`.
//!
//! Every strategy descends into the same folders the engine does: not
//! links, mount points or other name-surrogate reparse points (cloud-tagged
//! folders are descended), and on Unix not other filesystems. Each thread
//! keeps one buffer, so allocation isn't part of the floor.

use crate::cpu::Cpu;
use crate::ms;
use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strategy {
    Current,
    Ntdll,
    Relative,
}

impl Strategy {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "current" => Some(Self::Current),
            "ntdll" => Some(Self::Ntdll),
            "relative" => Some(Self::Relative),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Ntdll => "ntdll",
            Self::Relative => "relative",
        }
    }

    /// What the strategy calls on this platform.
    fn describe(self) -> &'static str {
        match self {
            #[cfg(windows)]
            Self::Current => "CreateFileW + GetFileInformationByHandleEx(FileIdExtdDirectoryInfo)",
            #[cfg(target_os = "linux")]
            Self::Current => "read_dir + statx per entry",
            #[cfg(all(unix, not(target_os = "linux")))]
            Self::Current => "read_dir + lstat per entry",
            Self::Ntdll => "CreateFileW + NtQueryDirectoryFile(FileIdExtdDirectoryInformation)",
            Self::Relative => "NtOpenFile relative to parent + NtQueryDirectoryFile",
        }
    }

    /// Whether this platform has the strategy.
    pub fn available(self) -> bool {
        cfg!(windows) || self == Self::Current
    }
}

/// Totals of one listing, summed over threads.
#[derive(Debug, Default, Clone)]
pub struct Stats {
    /// Every name listed below the root (not `.` or `..`).
    pub entries: u64,
    /// Folders descended into (below the root).
    pub dirs: u64,
    /// Links, mount points and other reparse points not followed.
    pub skipped: u64,
    /// Opens or listings that failed.
    pub errors: u64,
    pub queries: u64,
    pub open_ns: u64,
    pub query_ns: u64,
    /// Queries that found nothing more (on Windows, one per folder).
    pub empty_query_ns: u64,
    pub decode_ns: u64,
    /// Per-entry metadata calls (Unix).
    pub stat_ns: u64,
}

impl Stats {
    fn add(&mut self, o: &Self) {
        self.entries += o.entries;
        self.dirs += o.dirs;
        self.skipped += o.skipped;
        self.errors += o.errors;
        self.queries += o.queries;
        self.open_ns += o.open_ns;
        self.query_ns += o.query_ns;
        self.empty_query_ns += o.empty_query_ns;
        self.decode_ns += o.decode_ns;
        self.stat_ns += o.stat_ns;
    }
}

/// Folders still to list, and how many are being listed.
struct Queue<T> {
    state: Mutex<(VecDeque<T>, usize)>,
    ready: Condvar,
}

impl<T> Queue<T> {
    fn new(first: T) -> Self {
        Self {
            state: Mutex::new((VecDeque::from([first]), 0)),
            ready: Condvar::new(),
        }
    }

    fn push(&self, items: impl IntoIterator<Item = T>) {
        let mut s = self.state.lock().unwrap();
        let before = s.0.len();
        s.0.extend(items);
        match s.0.len() - before {
            0 => {}
            1 => self.ready.notify_one(),
            _ => self.ready.notify_all(),
        }
    }

    /// The next folder, or `None` once nothing is queued or being listed.
    fn pop(&self) -> Option<T> {
        let mut s = self.state.lock().unwrap();
        loop {
            if let Some(item) = s.0.pop_front() {
                s.1 += 1;
                return Some(item);
            }
            if s.1 == 0 {
                return None;
            }
            s = self.ready.wait(s).unwrap();
        }
    }

    /// Marks a popped folder done (after its subfolders were pushed).
    fn done(&self) {
        let mut s = self.state.lock().unwrap();
        s.1 -= 1;
        if s.1 == 0 && s.0.is_empty() {
            self.ready.notify_all();
        }
    }
}

fn nanos(since: Instant) -> u64 {
    since.elapsed().as_nanos() as u64
}

/// Lists `root` with `threads` threads, returning the wall time and totals.
pub fn walk(root: &Path, threads: usize, strategy: Strategy) -> (Duration, Stats) {
    assert!(
        strategy.available(),
        "{} isn't available here",
        strategy.name()
    );
    let lister = Arc::new(imp::Lister::new(root, strategy));
    let queue = Arc::new(Queue::new(imp::Item::root(root)));
    let started = Instant::now();
    let workers: Vec<_> = (0..threads.max(1))
        .map(|i| {
            let (queue, lister) = (Arc::clone(&queue), Arc::clone(&lister));
            std::thread::Builder::new()
                .name(format!("sb-bench-floor-{i}"))
                .spawn(move || {
                    let mut stats = Stats::default();
                    let mut scratch = imp::Scratch::default();
                    let mut subdirs = Vec::new();
                    while let Some(item) = queue.pop() {
                        lister.list(item, &mut scratch, &mut stats, &mut subdirs);
                        queue.push(subdirs.drain(..));
                        queue.done();
                    }
                    stats
                })
                .expect("spawn floor thread")
        })
        .collect();
    let mut total = Stats::default();
    for w in workers {
        total.add(&w.join().expect("floor thread"));
    }
    (started.elapsed(), total)
}

/// Runs and prints one floor measurement.
pub fn run(root: &Path, threads: usize, strategy: Strategy) {
    println!(
        "\n== floor {}: {} ({}), {threads} threads ==",
        root.display(),
        strategy.name(),
        strategy.describe()
    );
    let before = Cpu::process();
    let (wall, s) = walk(root, threads, strategy);
    let cpu = Cpu::process().zip(before).map(|(a, b)| a.since(b));
    let secs = wall.as_secs_f64();
    let folders = s.dirs + 1;
    println!(
        "listed:  {} entries ({} folders, {} not followed) in {}; {} errors",
        s.entries,
        s.dirs,
        s.skipped,
        ms(wall),
        s.errors
    );
    println!(
        "rate:    {:.0} entries/s, {:.0} folders/s; {} queries ({:.2} per folder)",
        s.entries as f64 / secs,
        folders as f64 / secs,
        s.queries,
        s.queries as f64 / folders as f64
    );
    let us = |ns: u64, n: u64| ns as f64 / 1e3 / n.max(1) as f64;
    let mut per = format!(
        "per:     folder open {:.1} us, query {:.1} us, end-of-listing query {:.1} us",
        us(s.open_ns, folders),
        us(s.query_ns, s.queries),
        us(s.empty_query_ns, folders)
    );
    if s.stat_ns > 0 {
        per += &format!(", entry metadata {:.2} us", us(s.stat_ns, s.entries));
    }
    println!("{per}");
    println!(
        "threads: open {:.0} ms, query {:.0} ms, decode {:.0} ms, metadata {:.0} ms (summed)",
        s.open_ns as f64 / 1e6,
        s.query_ns as f64 / 1e6,
        s.decode_ns as f64 / 1e6,
        s.stat_ns as f64 / 1e6
    );
    if let Some(cpu) = cpu {
        println!(
            "cpu:     kernel {}, user {} ({:.1} cores busy)",
            ms(cpu.kernel),
            ms(cpu.user),
            cpu.total().as_secs_f64() / secs
        );
    }
    if s.errors > 0 && s.entries == 0 {
        println!("warning: nothing could be listed; is the root a readable folder?");
    } else if s.errors * 100 > folders {
        println!(
            "warning: {} of {folders} folders couldn't be listed; times aren't comparable",
            s.errors
        );
    }
    let (k, u) = cpu.map_or((-1.0, -1.0), |c| {
        (c.kernel.as_secs_f64() * 1e3, c.user.as_secs_f64() * 1e3)
    });
    println!(
        "result:  floor wall_ms={:.1} entries={} dirs={} errors={} kernel_ms={k:.0} user_ms={u:.0}",
        secs * 1e3,
        s.entries,
        s.dirs,
        s.errors
    );
}

#[cfg(windows)]
mod imp {
    use super::{Stats, Strategy, nanos};
    use std::ffi::{OsString, c_void};
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Instant;
    use windows::Win32::Foundation::{CloseHandle, ERROR_NO_MORE_FILES, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_NO_RECALL,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_EXTD_DIR_INFO, FILE_LIST_DIRECTORY,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileIdExtdDirectoryInfo,
        GetFileInformationByHandleEx, OPEN_EXISTING,
    };
    use windows::core::PCWSTR;

    const DIRECTORY: u32 = 0x10;
    const REPARSE_POINT: u32 = 0x400;

    /// `IO_REPARSE_TAG_CLOUD` and its variants: folders the engine lists.
    fn is_cloud(tag: u32) -> bool {
        tag & 0xFFFF_0FFF == 0x9000_001A
    }

    /// A handle closed on drop, shared by the children opened relative to it.
    struct Handle(HANDLE);

    // SAFETY: a kernel handle may be used and closed from any thread.
    unsafe impl Send for Handle {}
    unsafe impl Sync for Handle {}

    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: opened by us, closed once.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    #[repr(C)]
    struct UnicodeString {
        length: u16,
        maximum: u16,
        buffer: *mut u16,
    }

    #[repr(C)]
    struct ObjectAttributes {
        length: u32,
        root: HANDLE,
        name: *mut UnicodeString,
        attributes: u32,
        security_descriptor: *mut c_void,
        security_qos: *mut c_void,
    }

    #[repr(C)]
    #[derive(Default)]
    struct IoStatusBlock {
        status: usize,
        information: usize,
    }

    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtOpenFile(
            handle: *mut HANDLE,
            access: u32,
            attributes: *mut ObjectAttributes,
            iosb: *mut IoStatusBlock,
            share: u32,
            options: u32,
        ) -> i32;
        fn NtQueryDirectoryFile(
            handle: HANDLE,
            event: HANDLE,
            apc: *mut c_void,
            context: *mut c_void,
            iosb: *mut IoStatusBlock,
            info: *mut c_void,
            length: u32,
            class: u32,
            single: u8,
            name: *mut UnicodeString,
            restart: u8,
        ) -> i32;
    }

    const SYNCHRONIZE: u32 = 0x0010_0000;
    const FILE_DIRECTORY_FILE: u32 = 0x1;
    const FILE_SYNCHRONOUS_IO_NONALERT: u32 = 0x20;
    const FILE_OPEN_FOR_BACKUP_INTENT: u32 = 0x4000;
    const FILE_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const OBJ_CASE_INSENSITIVE: u32 = 0x40;
    const STATUS_NO_MORE_FILES: i32 = 0x8000_0006_u32 as i32;
    const FILE_ID_EXTD_DIRECTORY_INFORMATION: u32 = 60;

    /// One folder to list: by path, or by name relative to its parent.
    pub struct Item {
        path: PathBuf,
        relative: Option<(Arc<Handle>, Vec<u16>)>,
    }

    impl Item {
        pub fn root(root: &Path) -> Self {
            Self {
                path: root.to_path_buf(),
                relative: None,
            }
        }
    }

    /// One listing buffer per thread, 64 KiB as the adapter uses.
    pub struct Scratch(Vec<u64>);

    impl Default for Scratch {
        fn default() -> Self {
            Self(vec![0; 8192])
        }
    }

    pub struct Lister {
        strategy: Strategy,
    }

    /// NUL-terminated `\\?\` path, as the adapter builds it.
    fn wide(path: &Path) -> Vec<u16> {
        let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        let text: Vec<u16> = absolute.as_os_str().encode_wide().collect();
        let starts = |prefix: &str| text.starts_with(&prefix.encode_utf16().collect::<Vec<_>>());
        let mut out: Vec<u16> = if starts(r"\\?\") || starts(r"\\.\") {
            text
        } else if starts(r"\\") {
            r"\\?\UNC\"
                .encode_utf16()
                .chain(text[2..].iter().copied())
                .collect()
        } else {
            r"\\?\".encode_utf16().chain(text).collect()
        };
        out.push(0);
        out
    }

    /// Opens a folder by path with the adapter's access, sharing and flags.
    fn open_path(path: &Path) -> Option<Handle> {
        let path = wide(path);
        // SAFETY: `path` is NUL-terminated and outlives the call.
        let handle = unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                FILE_LIST_DIRECTORY.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS
                    | FILE_FLAG_OPEN_REPARSE_POINT
                    | FILE_FLAG_OPEN_NO_RECALL,
                None,
            )
        };
        handle.ok().map(Handle)
    }

    /// Opens `name` in the folder `parent` with the options the path open
    /// amounts to: backup intent and reparse point. (`FILE_OPEN_NO_RECALL`
    /// with `FILE_DIRECTORY_FILE` is refused as an invalid parameter; a
    /// folder has no data to recall.)
    fn open_relative(parent: &Handle, name: &[u16]) -> Option<Handle> {
        let bytes = u16::try_from(name.len() * 2).ok()?;
        let mut unicode = UnicodeString {
            length: bytes,
            maximum: bytes,
            buffer: name.as_ptr().cast_mut(),
        };
        let mut attributes = ObjectAttributes {
            length: size_of::<ObjectAttributes>() as u32,
            root: parent.0,
            name: &mut unicode,
            attributes: OBJ_CASE_INSENSITIVE,
            security_descriptor: std::ptr::null_mut(),
            security_qos: std::ptr::null_mut(),
        };
        let mut iosb = IoStatusBlock::default();
        let mut handle = HANDLE::default();
        // SAFETY: every pointer refers to a live local; `name` outlives it.
        let status = unsafe {
            NtOpenFile(
                &mut handle,
                FILE_LIST_DIRECTORY.0 | SYNCHRONIZE,
                &mut attributes,
                &mut iosb,
                (FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0,
                FILE_DIRECTORY_FILE
                    | FILE_SYNCHRONOUS_IO_NONALERT
                    | FILE_OPEN_FOR_BACKUP_INTENT
                    | FILE_OPEN_REPARSE_POINT,
            )
        };
        (status >= 0).then_some(Handle(handle))
    }

    enum Query {
        More,
        End,
        Failed,
    }

    impl Lister {
        pub fn new(_root: &Path, strategy: Strategy) -> Self {
            Self { strategy }
        }

        fn query(&self, handle: &Handle, buffer: &mut [u64], first: bool) -> Query {
            let len = (buffer.len() * 8) as u32;
            if self.strategy == Strategy::Current {
                // SAFETY: the buffer is writable for `len` bytes.
                let result = unsafe {
                    GetFileInformationByHandleEx(
                        handle.0,
                        FileIdExtdDirectoryInfo,
                        buffer.as_mut_ptr().cast(),
                        len,
                    )
                };
                return match result {
                    Ok(()) => Query::More,
                    Err(e) if e.code() == ERROR_NO_MORE_FILES.to_hresult() => Query::End,
                    Err(_) => Query::Failed,
                };
            }
            let mut iosb = IoStatusBlock::default();
            // SAFETY: synchronous handle; the buffer is writable for `len`.
            let status = unsafe {
                NtQueryDirectoryFile(
                    handle.0,
                    HANDLE::default(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut iosb,
                    buffer.as_mut_ptr().cast(),
                    len,
                    FILE_ID_EXTD_DIRECTORY_INFORMATION,
                    0,
                    std::ptr::null_mut(),
                    u8::from(first),
                )
            };
            match status {
                0 => Query::More,
                STATUS_NO_MORE_FILES => Query::End,
                _ => Query::Failed,
            }
        }

        pub fn list(
            &self,
            item: Item,
            scratch: &mut Scratch,
            stats: &mut Stats,
            subdirs: &mut Vec<Item>,
        ) {
            let t = Instant::now();
            let handle = match &item.relative {
                Some((parent, name)) => open_relative(parent, name),
                None => open_path(&item.path),
            };
            stats.open_ns += nanos(t);
            let Some(handle) = handle else {
                stats.errors += 1;
                return;
            };
            let handle = Arc::new(handle);
            let relative = self.strategy == Strategy::Relative;
            let mut first = true;
            loop {
                let t = Instant::now();
                let result = self.query(&handle, &mut scratch.0, first);
                let took = nanos(t);
                first = false;
                stats.queries += 1;
                stats.query_ns += took;
                match result {
                    Query::More => {}
                    Query::End => {
                        stats.empty_query_ns += took;
                        return;
                    }
                    Query::Failed => {
                        stats.errors += 1;
                        return;
                    }
                }
                let t = Instant::now();
                decode(&scratch.0, stats, |name| {
                    subdirs.push(if relative {
                        Item {
                            path: PathBuf::new(),
                            relative: Some((Arc::clone(&handle), name.to_vec())),
                        }
                    } else {
                        Item {
                            path: item.path.join(OsString::from_wide(name)),
                            relative: None,
                        }
                    });
                });
                stats.decode_ns += nanos(t);
            }
        }
    }

    /// Counts one buffer of `FILE_ID_EXTD_DIR_INFO` records, calling
    /// `subdir` with the name of each folder the engine would descend into.
    fn decode(buffer: &[u64], stats: &mut Stats, mut subdir: impl FnMut(&[u16])) {
        const NAME_OFFSET: usize = std::mem::offset_of!(FILE_ID_EXTD_DIR_INFO, FileName);
        let base = buffer.as_ptr().cast::<u8>();
        let mut offset = 0usize;
        loop {
            // SAFETY: the call filled the buffer with 8-byte-aligned records
            // chained by NextEntryOffset, each followed by its name.
            let record = unsafe { base.add(offset).cast::<FILE_ID_EXTD_DIR_INFO>().read() };
            let name = unsafe {
                std::slice::from_raw_parts(
                    base.add(offset + NAME_OFFSET).cast::<u16>(),
                    record.FileNameLength as usize / 2,
                )
            };
            if name != [0x2e] && name != [0x2e, 0x2e] {
                stats.entries += 1;
                let attributes = record.FileAttributes;
                let reparse = attributes & REPARSE_POINT != 0;
                if reparse && !is_cloud(record.ReparsePointTag) {
                    stats.skipped += 1;
                } else if attributes & DIRECTORY != 0 {
                    stats.dirs += 1;
                    subdir(name);
                }
            }
            if record.NextEntryOffset == 0 {
                break;
            }
            offset += record.NextEntryOffset as usize;
        }
    }
}

#[cfg(unix)]
mod imp {
    use super::{Stats, Strategy, nanos};
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    pub struct Item(PathBuf);

    impl Item {
        pub fn root(root: &Path) -> Self {
            Self(root.to_path_buf())
        }
    }

    /// Nothing to keep per thread: `read_dir` brings its own buffer.
    #[derive(Default)]
    pub struct Scratch {
        _none: (),
    }

    pub struct Lister {
        /// The root's device: other filesystems aren't descended.
        dev: Option<u64>,
    }

    impl Lister {
        pub fn new(root: &Path, _strategy: Strategy) -> Self {
            Self {
                dev: std::fs::symlink_metadata(root).ok().map(|m| m.dev()),
            }
        }

        pub fn list(
            &self,
            item: Item,
            _scratch: &mut Scratch,
            stats: &mut Stats,
            subdirs: &mut Vec<Item>,
        ) {
            let t = Instant::now();
            let dir = std::fs::read_dir(&item.0);
            stats.open_ns += nanos(t);
            let Ok(mut dir) = dir else {
                stats.errors += 1;
                return;
            };
            loop {
                let t = Instant::now();
                let next = dir.next();
                let took = nanos(t);
                stats.queries += 1;
                stats.query_ns += took;
                let entry = match next {
                    None => {
                        stats.empty_query_ns += took;
                        return;
                    }
                    Some(Err(_)) => {
                        stats.errors += 1;
                        return;
                    }
                    Some(Ok(entry)) => entry,
                };
                stats.entries += 1;
                let t = Instant::now();
                // Relative to the open folder (fstatat/statx), as the Linux
                // adapter; the portable adapter stats by path.
                #[cfg(target_os = "linux")]
                let meta = entry.metadata();
                #[cfg(not(target_os = "linux"))]
                let meta = std::fs::symlink_metadata(entry.path());
                stats.stat_ns += nanos(t);
                let Ok(meta) = meta else {
                    stats.errors += 1;
                    continue;
                };
                let kind = meta.file_type();
                if kind.is_symlink() {
                    stats.skipped += 1;
                } else if kind.is_dir() {
                    if Some(meta.dev()) == self.dev {
                        stats.dirs += 1;
                        subdirs.push(Item(entry.path()));
                    } else {
                        stats.skipped += 1;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch_tree(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("sb-bench-floor-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        // 3 levels of 3 folders, 4 files in each folder: 1 + 3 + 9 + 27
        // folders listed, 39 below the root, 160 files.
        fn fill(dir: &Path, depth: u32) {
            std::fs::create_dir_all(dir).unwrap();
            for f in 0..4 {
                std::fs::write(dir.join(format!("f{f}.txt")), b"x").unwrap();
            }
            if depth < 3 {
                for d in 0..3 {
                    fill(&dir.join(format!("d{d}")), depth + 1);
                }
            }
        }
        fill(&root, 0);
        root
    }

    fn check(strategy: Strategy, threads: usize) {
        if !strategy.available() {
            return;
        }
        let root = scratch_tree(&format!("{}-{threads}", strategy.name()));
        let (_, s) = walk(&root, threads, strategy);
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(s.dirs, 39, "{strategy:?}: {s:?}");
        assert_eq!(s.entries, 39 + 160, "{strategy:?}: {s:?}");
        assert_eq!(s.errors, 0, "{strategy:?}: {s:?}");
        assert_eq!(s.skipped, 0, "{strategy:?}: {s:?}");
    }

    #[test]
    fn every_strategy_counts_the_same_tree() {
        for strategy in [Strategy::Current, Strategy::Ntdll, Strategy::Relative] {
            check(strategy, 1);
            check(strategy, 4);
        }
    }

    #[test]
    fn a_missing_root_is_an_error_not_a_hang() {
        for strategy in [Strategy::Current, Strategy::Ntdll, Strategy::Relative] {
            if !strategy.available() {
                continue;
            }
            let (_, s) = walk(Path::new("/no/such/sb-bench/root"), 2, strategy);
            assert_eq!((s.entries, s.errors), (0, 1), "{strategy:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn links_are_counted_not_followed() {
        let root = scratch_tree("links");
        std::os::unix::fs::symlink(root.join("d0"), root.join("d0-link")).unwrap();
        let (_, s) = walk(&root, 2, Strategy::Current);
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!((s.dirs, s.skipped), (39, 1), "{s:?}");
        assert_eq!(s.entries, 39 + 160 + 1, "{s:?}");
    }

    #[test]
    fn strategies_parse_by_name() {
        for strategy in [Strategy::Current, Strategy::Ntdll, Strategy::Relative] {
            assert_eq!(Strategy::parse(strategy.name()), Some(strategy));
        }
        assert_eq!(Strategy::parse("fast"), None);
    }
}
