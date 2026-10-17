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
//!   into a 64 KiB buffer, falling back once for the whole run to
//!   `FileIdBothDirectoryInfo` where the volume doesn't support it, as the
//!   adapter does. On Linux, `openat` + `getdents` (rustix's `Dir`) and a
//!   `statx` per entry relative to the folder, with the adapter's flags,
//!   mask and mount checks; elsewhere `read_dir` and `symlink_metadata` per
//!   entry, as the portable adapter.
//! - `ntdll` (Windows): the same path opens, `NtQueryDirectoryFile` direct.
//! - `relative` (Windows): `NtOpenFile` relative to the parent's handle
//!   with the child's name from the listing, then `NtQueryDirectoryFile`.
//!   Folders are taken newest first, so parents' handles close soon after
//!   their children are opened, and at most [`PARENT_HANDLES`] folder
//!   handles are open at once: past that, children are opened by path.
//!
//! `current` and `ntdll` take folders oldest first (breadth first), as the
//! engine's queue does. Every strategy descends into the same folders the
//! engine does: not links, mount points or other name-surrogate reparse
//! points (cloud-tagged folders are descended), and on Unix not other
//! filesystems. Each thread keeps one buffer, so allocation isn't part of
//! the floor.

use crate::cpu::{Cpu, Threads, print_roles, without_watcher};
use crate::ms;
use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Folder handles `relative` keeps open at most (design §2.2 item 4).
pub const PARENT_HANDLES: usize = 4096;

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
            Self::Current => "openat + getdents + statx per entry",
            #[cfg(all(unix, not(target_os = "linux")))]
            Self::Current => "read_dir + lstat per entry",
            Self::Ntdll => "CreateFileW + NtQueryDirectoryFile(FileIdExtdDirectoryInformation)",
            Self::Relative => "NtOpenFile relative to parent + NtQueryDirectoryFile, newest first",
        }
    }

    /// Whether this platform has the strategy.
    pub fn available(self) -> bool {
        cfg!(windows) || self == Self::Current
    }
}

/// Settings of one walk; [`Options::new`] gives the measured defaults.
#[derive(Clone, Copy, Debug)]
// The handle cap and the older class exist only on Windows.
#[cfg_attr(not(windows), allow(dead_code))]
pub struct Options {
    pub threads: usize,
    pub strategy: Strategy,
    /// Most folder handles `relative` keeps open at once.
    pub parent_handles: usize,
    /// Start with the older directory-information class (Windows), as the
    /// adapter does once the newer one is found unsupported.
    pub older_class: bool,
}

impl Options {
    pub fn new(threads: usize, strategy: Strategy) -> Self {
        Self {
            threads,
            strategy,
            parent_handles: PARENT_HANDLES,
            older_class: false,
        }
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
    /// `relative`: folders opened by path because the handle cap was
    /// reached.
    pub by_path: u64,
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
        self.by_path += o.by_path;
    }
}

/// What a walk found, and what it learnt about the listing calls.
#[derive(Debug, Default, Clone)]
pub struct Walked {
    pub wall: Duration,
    pub stats: Stats,
    /// Whether the older directory-information class was used (Windows).
    pub older_class: bool,
    /// Most folder handles open at once (`relative`).
    pub peak_handles: usize,
}

/// Folders still to list, and how many are being listed.
struct Queue<T> {
    state: Mutex<(VecDeque<T>, usize)>,
    ready: Condvar,
    /// Newest first rather than oldest first.
    lifo: bool,
}

impl<T> Queue<T> {
    fn new(first: T, lifo: bool) -> Self {
        Self {
            state: Mutex::new((VecDeque::from([first]), 0)),
            ready: Condvar::new(),
            lifo,
        }
    }

    fn push(&self, items: impl IntoIterator<Item = T>) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let before = s.0.len();
        s.0.extend(items);
        match s.0.len() - before {
            0 => {}
            1 => self.ready.notify_one(),
            _ => self.ready.notify_all(),
        }
    }

    /// The next folder, or `None` once nothing is queued or being listed.
    /// Each folder returned must be marked [`Taken`]-done, which the
    /// returned guard does when dropped, even if listing it panics.
    fn pop(&self) -> Option<(T, Taken<'_, T>)> {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            let next = if self.lifo {
                s.0.pop_back()
            } else {
                s.0.pop_front()
            };
            if let Some(item) = next {
                s.1 += 1;
                return Some((item, Taken(self)));
            }
            if s.1 == 0 {
                return None;
            }
            s = self.ready.wait(s).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Marks a popped folder done (after its subfolders were pushed).
    fn done(&self) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.1 -= 1;
        if s.1 == 0 && s.0.is_empty() {
            self.ready.notify_all();
        }
    }
}

/// Marks a popped folder done when dropped, so a thread that panics while
/// listing doesn't leave the others waiting for it forever.
struct Taken<'a, T>(&'a Queue<T>);

impl<T> Drop for Taken<'_, T> {
    fn drop(&mut self) {
        self.0.done();
    }
}

fn nanos(since: Instant) -> u64 {
    since.elapsed().as_nanos() as u64
}

/// Lists `root` with `threads` threads and the default settings.
#[cfg(test)]
pub fn walk(root: &Path, threads: usize, strategy: Strategy) -> Walked {
    walk_with(root, Options::new(threads, strategy))
}

/// Lists `root` as `options` say, returning the wall time and totals.
/// Panics if a listing thread panicked (after the others finished).
pub fn walk_with(root: &Path, options: Options) -> Walked {
    let strategy = options.strategy;
    assert!(
        strategy.available(),
        "{} isn't available here",
        strategy.name()
    );
    let lister = Arc::new(imp::Lister::new(root, options));
    let lifo = strategy == Strategy::Relative;
    let queue = Arc::new(Queue::new(imp::Item::root(root), lifo));
    let started = Instant::now();
    let workers: Vec<_> = (0..options.threads.max(1))
        .map(|i| {
            let (queue, lister) = (Arc::clone(&queue), Arc::clone(&lister));
            std::thread::Builder::new()
                .name(format!("sb-bench-floor-{i}"))
                .spawn(move || {
                    let mut stats = Stats::default();
                    let mut scratch = imp::Scratch::default();
                    let mut subdirs = Vec::new();
                    while let Some((item, _taken)) = queue.pop() {
                        lister.list(item, &mut scratch, &mut stats, &mut subdirs);
                        queue.push(subdirs.drain(..));
                    }
                    stats
                })
                .expect("spawn floor thread")
        })
        .collect();
    let mut total = Stats::default();
    let mut panicked = 0;
    for w in workers {
        match w.join() {
            Ok(stats) => total.add(&stats),
            Err(_) => panicked += 1,
        }
    }
    assert_eq!(panicked, 0, "{panicked} floor threads panicked");
    Walked {
        wall: started.elapsed(),
        stats: total,
        older_class: lister.older_class(),
        peak_handles: lister.peak_handles(),
    }
}

/// Runs and prints one floor measurement.
pub fn run(root: &Path, threads: usize, strategy: Strategy) {
    println!(
        "\n== floor {}: {} ({}), {threads} threads ==",
        root.display(),
        strategy.name(),
        strategy.describe()
    );
    // The engine's adapter enables backup access when the process is
    // elevated, which lists protected folders; the floor must too.
    let privileged = sb_scan::native::privileged_access();
    println!(
        "access:  {}",
        if privileged {
            "privileged (backup privilege enabled / root)"
        } else {
            "ordinary (not elevated)"
        }
    );
    // Watched like the engine, so both carry the watcher; its own CPU is
    // left out of both.
    let threads_seen = Threads::watch();
    let before = Cpu::process();
    let w = walk_with(root, Options::new(threads, strategy));
    let process = Cpu::process().zip(before).map(|(a, b)| a.since(b));
    let roles = threads_seen.finish();
    let cpu = process.map(|p| without_watcher(p, roles.as_ref()));
    let s = &w.stats;
    let secs = w.wall.as_secs_f64();
    let folders = s.dirs + 1;
    println!(
        "listed:  {} entries ({} folders, {} not followed) in {}; {} errors",
        s.entries,
        s.dirs,
        s.skipped,
        ms(w.wall),
        s.errors
    );
    if w.older_class {
        println!("class:   FileIdExtdDirectoryInfo unsupported; used FileIdBothDirectoryInfo");
    }
    if strategy == Strategy::Relative {
        println!(
            "handles: at most {} folder handles open (cap {PARENT_HANDLES}); {} folders opened by path past the cap",
            w.peak_handles, s.by_path
        );
    }
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
            "cpu:     kernel {}, user {} ({:.1} cores busy; without the thread watcher)",
            ms(cpu.kernel),
            ms(cpu.user),
            cpu.total().as_secs_f64() / secs
        );
    }
    if let Some(roles) = &roles {
        print_roles(roles);
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
        "result:  floor wall_ms={:.1} entries={} dirs={} folders={folders} errors={} kernel_ms={k:.0} user_ms={u:.0} \
         privileged={}",
        secs * 1e3,
        s.entries,
        s.dirs,
        s.errors,
        u8::from(privileged)
    );
}

#[cfg(windows)]
mod imp {
    use super::{Options, Stats, Strategy, nanos};
    use std::ffi::{OsString, c_void};
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Instant;
    use windows::Win32::Foundation::{CloseHandle, ERROR_NO_MORE_FILES, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_NO_RECALL,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_BOTH_DIR_INFO, FILE_ID_EXTD_DIR_INFO,
        FILE_INFO_BY_HANDLE_CLASS, FILE_LIST_DIRECTORY, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FileIdBothDirectoryInfo, FileIdExtdDirectoryInfo,
        GetFileInformationByHandleEx, OPEN_EXISTING,
    };
    use windows::core::PCWSTR;

    const DIRECTORY: u32 = 0x10;
    const REPARSE_POINT: u32 = 0x400;

    /// `IO_REPARSE_TAG_CLOUD` and its variants: folders the engine lists.
    fn is_cloud(tag: u32) -> bool {
        tag & 0xFFFF_0FFF == 0x9000_001A
    }

    /// A handle closed on drop.
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

    /// An open folder handle counted against the cap until dropped; shared
    /// by the children opened relative to it.
    pub struct Open {
        handle: Handle,
        live: Arc<AtomicUsize>,
    }

    impl Drop for Open {
        fn drop(&mut self) {
            self.live.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// Where a folder sits, for opening it by path when its parent's
    /// handle can't be used: its name and its parent's place, or (at the
    /// top) a path. Built only from names the listings already hold.
    pub struct Place {
        parent: Option<Arc<Place>>,
        name: Vec<u16>,
        path: Option<PathBuf>,
    }

    impl Place {
        fn path(&self) -> PathBuf {
            match (&self.path, &self.parent) {
                (Some(path), _) => path.clone(),
                (None, Some(parent)) => parent.path().join(OsString::from_wide(&self.name)),
                (None, None) => PathBuf::new(),
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
    const FILE_ID_BOTH_DIRECTORY_INFORMATION: u32 = 37;
    const FILE_ID_EXTD_DIRECTORY_INFORMATION: u32 = 60;

    /// NT statuses meaning the information class isn't supported here:
    /// invalid info class, invalid parameter, invalid device request, not
    /// supported (what `ERROR_INVALID_FUNCTION`, `ERROR_INVALID_PARAMETER`
    /// and `ERROR_NOT_SUPPORTED` come from).
    fn unsupported_status(status: i32) -> bool {
        matches!(
            status as u32,
            0xC000_0003 | 0xC000_000D | 0xC000_0010 | 0xC000_00BB
        )
    }

    /// Win32 errors the adapter treats as "class unsupported": its
    /// `ErrorKind::Unavailable` codes.
    fn unsupported_error(code: u32) -> bool {
        matches!(code, 1 | 32 | 33 | 50 | 87)
    }

    /// One folder to list.
    pub enum Item {
        Path(PathBuf),
        /// By name relative to its open parent (`relative`).
        Relative {
            place: Arc<Place>,
            parent: Arc<Open>,
        },
    }

    impl Item {
        pub fn root(root: &Path) -> Self {
            Self::Path(root.to_path_buf())
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
        cap: usize,
        /// Set once the newer class is found unsupported, as the adapter's
        /// `use_both_dir_info`.
        older: AtomicBool,
        live: Arc<AtomicUsize>,
        peak: AtomicUsize,
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
        Unsupported,
        Failed,
    }

    /// The two directory-information records, decoded alike.
    trait Record: Copy {
        const NAME_OFFSET: usize;
        fn next(&self) -> u32;
        fn name_len(&self) -> usize;
        fn attributes(&self) -> u32;
        fn tag(&self) -> u32;
    }

    impl Record for FILE_ID_EXTD_DIR_INFO {
        const NAME_OFFSET: usize = std::mem::offset_of!(FILE_ID_EXTD_DIR_INFO, FileName);
        fn next(&self) -> u32 {
            self.NextEntryOffset
        }
        fn name_len(&self) -> usize {
            self.FileNameLength as usize / 2
        }
        fn attributes(&self) -> u32 {
            self.FileAttributes
        }
        fn tag(&self) -> u32 {
            self.ReparsePointTag
        }
    }

    impl Record for FILE_ID_BOTH_DIR_INFO {
        const NAME_OFFSET: usize = std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
        fn next(&self) -> u32 {
            self.NextEntryOffset
        }
        fn name_len(&self) -> usize {
            self.FileNameLength as usize / 2
        }
        fn attributes(&self) -> u32 {
            self.FileAttributes
        }
        /// This class reports a reparse point's tag in `EaSize`.
        fn tag(&self) -> u32 {
            self.EaSize
        }
    }

    impl Lister {
        pub fn new(_root: &Path, options: Options) -> Self {
            Self {
                strategy: options.strategy,
                cap: options.parent_handles.max(1),
                older: AtomicBool::new(options.older_class),
                live: Arc::new(AtomicUsize::new(0)),
                peak: AtomicUsize::new(0),
            }
        }

        pub fn older_class(&self) -> bool {
            self.older.load(Ordering::Relaxed)
        }

        pub fn peak_handles(&self) -> usize {
            self.peak.load(Ordering::Relaxed)
        }

        fn query(&self, handle: &Handle, buffer: &mut [u64], first: bool, older: bool) -> Query {
            let len = (buffer.len() * 8) as u32;
            if self.strategy == Strategy::Current {
                let class: FILE_INFO_BY_HANDLE_CLASS = if older {
                    FileIdBothDirectoryInfo
                } else {
                    FileIdExtdDirectoryInfo
                };
                // SAFETY: the buffer is writable for `len` bytes.
                let result = unsafe {
                    GetFileInformationByHandleEx(handle.0, class, buffer.as_mut_ptr().cast(), len)
                };
                return match result {
                    Ok(()) => Query::More,
                    Err(e) if e.code() == ERROR_NO_MORE_FILES.to_hresult() => Query::End,
                    Err(e)
                        if (e.code().0 as u32) & 0xFFFF_0000 == 0x8007_0000
                            && unsupported_error(e.code().0 as u32 & 0xFFFF) =>
                    {
                        Query::Unsupported
                    }
                    Err(_) => Query::Failed,
                };
            }
            let class = if older {
                FILE_ID_BOTH_DIRECTORY_INFORMATION
            } else {
                FILE_ID_EXTD_DIRECTORY_INFORMATION
            };
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
                    class,
                    0,
                    std::ptr::null_mut(),
                    u8::from(first),
                )
            };
            match status {
                0 => Query::More,
                STATUS_NO_MORE_FILES => Query::End,
                s if unsupported_status(s) => Query::Unsupported,
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
            let relative = self.strategy == Strategy::Relative;
            let t = Instant::now();
            let (handle, place, path, parent) = match item {
                Item::Relative { place, parent } => (
                    open_relative(&parent.handle, &place.name),
                    Some(place),
                    None,
                    Some(parent),
                ),
                Item::Path(path) => (open_path(&path), None, Some(path), None),
            };
            stats.open_ns += nanos(t);
            // The parent's handle may close as soon as this one is open.
            drop(parent);
            let Some(handle) = handle else {
                stats.errors += 1;
                return;
            };
            let live = self.live.fetch_add(1, Ordering::Relaxed) + 1;
            self.peak.fetch_max(live, Ordering::Relaxed);
            let open = Arc::new(Open {
                handle,
                live: Arc::clone(&self.live),
            });
            // Where this folder is, for its children (relative only).
            let here: Option<Arc<Place>> = relative.then(|| {
                place.unwrap_or_else(|| {
                    Arc::new(Place {
                        parent: None,
                        name: Vec::new(),
                        path: path.clone(),
                    })
                })
            });
            let mut first = true;
            let mut any = false;
            loop {
                let older = self.older.load(Ordering::Relaxed);
                let t = Instant::now();
                let result = self.query(&open.handle, &mut scratch.0, first, older);
                let took = nanos(t);
                stats.queries += 1;
                stats.query_ns += took;
                match result {
                    Query::More => {}
                    Query::End => {
                        stats.empty_query_ns += took;
                        return;
                    }
                    // Only before any entry was read, so none is counted
                    // twice; the query restarts with the older class.
                    Query::Unsupported if !any && !older => {
                        self.older.store(true, Ordering::Relaxed);
                        continue;
                    }
                    Query::Unsupported | Query::Failed => {
                        stats.errors += 1;
                        return;
                    }
                }
                first = false;
                any = true;
                let t = Instant::now();
                let mut by_path = 0;
                let mut subdir = |name: &[u16]| {
                    subdirs.push(match &here {
                        Some(here) if self.live.load(Ordering::Relaxed) < self.cap => {
                            Item::Relative {
                                place: Arc::new(Place {
                                    parent: Some(Arc::clone(here)),
                                    name: name.to_vec(),
                                    path: None,
                                }),
                                parent: Arc::clone(&open),
                            }
                        }
                        Some(here) => {
                            by_path += 1;
                            Item::Path(here.path().join(OsString::from_wide(name)))
                        }
                        None => Item::Path(
                            path.as_deref()
                                .unwrap_or(Path::new(""))
                                .join(OsString::from_wide(name)),
                        ),
                    });
                };
                if older {
                    decode::<FILE_ID_BOTH_DIR_INFO>(&scratch.0, stats, &mut subdir);
                } else {
                    decode::<FILE_ID_EXTD_DIR_INFO>(&scratch.0, stats, &mut subdir);
                }
                stats.by_path += by_path;
                stats.decode_ns += nanos(t);
            }
        }
    }

    /// Counts one buffer of directory-information records, calling
    /// `subdir` with the name of each folder the engine would descend into.
    fn decode<T: Record>(buffer: &[u64], stats: &mut Stats, subdir: &mut impl FnMut(&[u16])) {
        let base = buffer.as_ptr().cast::<u8>();
        let mut offset = 0usize;
        loop {
            // SAFETY: the call filled the buffer with 8-byte-aligned records
            // chained by NextEntryOffset, each followed by its name.
            let record = unsafe { base.add(offset).cast::<T>().read() };
            let name = unsafe {
                std::slice::from_raw_parts(
                    base.add(offset + T::NAME_OFFSET).cast::<u16>(),
                    record.name_len(),
                )
            };
            if name != [0x2e] && name != [0x2e, 0x2e] {
                stats.entries += 1;
                let attributes = record.attributes();
                let reparse = attributes & REPARSE_POINT != 0;
                if reparse && !is_cloud(record.tag()) {
                    stats.skipped += 1;
                } else if attributes & DIRECTORY != 0 {
                    stats.dirs += 1;
                    subdir(name);
                }
            }
            if record.next() == 0 {
                break;
            }
            offset += record.next() as usize;
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    //! The Linux adapter's calls: `openat` with `O_NOFOLLOW`, rustix's
    //! `Dir` (`getdents64`), and `statx` relative to the folder with
    //! `AT_SYMLINK_NOFOLLOW | AT_NO_AUTOMOUNT`; a folder is another mount
    //! when its device differs, `STATX_ATTR_MOUNT_ROOT` is set or its mount
    //! ID differs, or (when statx reports neither) it's in mountinfo.
    use super::{Options, Stats, nanos};
    use rustix::fs::{
        AtFlags, CWD, Dir, FileType, Mode, OFlags, Statx, StatxAttributes, StatxFlags, openat,
        statx,
    };
    use std::collections::HashSet;
    use std::ffi::{OsStr, OsString};
    use std::os::fd::AsFd;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    const STAT_FLAGS: AtFlags = AtFlags::SYMLINK_NOFOLLOW.union(AtFlags::NO_AUTOMOUNT);
    const STAT_MASK: StatxFlags = StatxFlags::BASIC_STATS
        .union(StatxFlags::MNT_ID)
        .union(StatxFlags::BTIME);

    pub struct Item(PathBuf);

    impl Item {
        pub fn root(root: &Path) -> Self {
            Self(root.to_path_buf())
        }
    }

    /// Nothing to keep per thread: `Dir` brings its own buffer.
    #[derive(Default)]
    pub struct Scratch {
        _none: (),
    }

    pub struct Lister {
        dev: Option<u64>,
        mount_id: Option<u64>,
        mount_points: Option<HashSet<PathBuf>>,
    }

    fn dev(stx: &Statx) -> u64 {
        (u64::from(stx.stx_dev_major) << 32) | u64::from(stx.stx_dev_minor)
    }

    fn mount_id(stx: &Statx) -> Option<u64> {
        (stx.stx_mask & StatxFlags::MNT_ID.bits() != 0).then_some(stx.stx_mnt_id)
    }

    fn kind_of(stx: &Statx) -> FileType {
        FileType::from_raw_mode(rustix::fs::RawMode::from(stx.stx_mode))
    }

    /// Mount points listed in `/proc/self/mountinfo` (field 5, octal-escaped).
    fn read_mount_points() -> Option<HashSet<PathBuf>> {
        let text = std::fs::read("/proc/self/mountinfo").ok()?;
        let mut points = HashSet::new();
        for line in text.split(|&b| b == b'\n') {
            if let Some(field) = line.split(|&b| b == b' ').nth(4) {
                points.insert(PathBuf::from(OsString::from_vec(unescape_octal(field))));
            }
        }
        Some(points)
    }

    fn unescape_octal(field: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(field.len());
        let mut i = 0;
        while i < field.len() {
            if field[i] == b'\\'
                && i + 3 < field.len()
                && field[i + 1..i + 4]
                    .iter()
                    .all(|b| (b'0'..=b'7').contains(b))
            {
                let value = field[i + 1..i + 4]
                    .iter()
                    .fold(0u32, |acc, b| acc * 8 + u32::from(b - b'0'));
                out.push(value as u8);
                i += 4;
            } else {
                out.push(field[i]);
                i += 1;
            }
        }
        out
    }

    impl Lister {
        pub fn new(root: &Path, _options: Options) -> Self {
            let stx = statx(CWD, root, STAT_FLAGS, STAT_MASK).ok();
            let has_mount_attr = stx
                .as_ref()
                .is_some_and(|s| s.stx_attributes_mask.contains(StatxAttributes::MOUNT_ROOT));
            let id = stx.as_ref().and_then(mount_id);
            Self {
                dev: stx.as_ref().map(dev),
                mount_id: id,
                mount_points: (stx.is_some() && !has_mount_attr && id.is_none())
                    .then(read_mount_points)
                    .flatten(),
            }
        }

        pub fn older_class(&self) -> bool {
            false
        }

        pub fn peak_handles(&self) -> usize {
            0
        }

        fn crosses(&self, dir: &Path, name: &OsStr, stx: &Statx) -> bool {
            Some(dev(stx)) != self.dev
                || (stx
                    .stx_attributes_mask
                    .contains(StatxAttributes::MOUNT_ROOT)
                    && stx.stx_attributes.contains(StatxAttributes::MOUNT_ROOT))
                || matches!((mount_id(stx), self.mount_id), (Some(a), Some(b)) if a != b)
                || self
                    .mount_points
                    .as_ref()
                    .is_some_and(|points| points.contains(&dir.join(name)))
        }

        pub fn list(
            &self,
            item: Item,
            _scratch: &mut Scratch,
            stats: &mut Stats,
            subdirs: &mut Vec<Item>,
        ) {
            let t = Instant::now();
            let fd = openat(
                CWD,
                &item.0,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            );
            let dir = fd
                .as_ref()
                .ok()
                .and_then(|fd| Dir::read_from(fd.as_fd()).ok());
            stats.open_ns += nanos(t);
            let (Ok(fd), Some(mut dir)) = (&fd, dir) else {
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
                let name = entry.file_name();
                if matches!(name.to_bytes(), b"." | b"..") {
                    continue;
                }
                stats.entries += 1;
                let t = Instant::now();
                let stx = statx(fd.as_fd(), name, STAT_FLAGS, STAT_MASK);
                stats.stat_ns += nanos(t);
                let Ok(stx) = stx else {
                    stats.errors += 1;
                    continue;
                };
                match kind_of(&stx) {
                    FileType::Symlink => stats.skipped += 1,
                    FileType::Directory => {
                        let name = OsStr::from_bytes(name.to_bytes());
                        if self.crosses(&item.0, name, &stx) {
                            stats.skipped += 1;
                        } else {
                            stats.dirs += 1;
                            subdirs.push(Item(item.0.join(name)));
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
mod imp {
    use super::{Options, Stats, nanos};
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
        pub fn new(root: &Path, _options: Options) -> Self {
            Self {
                dev: std::fs::symlink_metadata(root).ok().map(|m| m.dev()),
            }
        }

        pub fn older_class(&self) -> bool {
            false
        }

        pub fn peak_handles(&self) -> usize {
            0
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
                // By path, as the portable adapter does.
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

    fn check(name: &str, options: Options) -> Walked {
        let root = scratch_tree(name);
        let w = walk_with(&root, options);
        std::fs::remove_dir_all(&root).unwrap();
        let s = &w.stats;
        assert_eq!(s.dirs, 39, "{options:?}: {s:?}");
        assert_eq!(s.entries, 39 + 160, "{options:?}: {s:?}");
        assert_eq!(s.errors, 0, "{options:?}: {s:?}");
        assert_eq!(s.skipped, 0, "{options:?}: {s:?}");
        w
    }

    const ALL: [Strategy; 3] = [Strategy::Current, Strategy::Ntdll, Strategy::Relative];

    #[test]
    fn every_strategy_counts_the_same_tree() {
        for strategy in ALL.into_iter().filter(|s| s.available()) {
            for threads in [1, 4] {
                let name = format!("{}-{threads}", strategy.name());
                check(&name, Options::new(threads, strategy));
            }
        }
    }

    #[test]
    fn the_older_listing_class_counts_the_same_tree() {
        for strategy in ALL.into_iter().filter(|s| s.available()) {
            let options = Options {
                older_class: true,
                ..Options::new(2, strategy)
            };
            let w = check(&format!("older-{}", strategy.name()), options);
            assert_eq!(w.older_class, cfg!(windows));
        }
    }

    #[cfg(windows)]
    #[test]
    fn relative_opens_by_path_past_the_handle_cap() {
        let options = Options {
            parent_handles: 2,
            ..Options::new(1, Strategy::Relative)
        };
        let w = check("cap", options);
        assert!(w.stats.by_path > 0, "{:?}", w.stats);
        // One thread, newest first: the cap holds, give or take the folder
        // being opened.
        assert!(w.peak_handles <= 3, "{}", w.peak_handles);
        let uncapped = check("uncapped", Options::new(1, Strategy::Relative));
        assert_eq!(uncapped.stats.by_path, 0);
    }

    #[test]
    fn a_missing_root_is_an_error_not_a_hang() {
        for strategy in ALL.into_iter().filter(|s| s.available()) {
            let w = walk(Path::new("/no/such/sb-bench/root"), 2, strategy);
            assert_eq!((w.stats.entries, w.stats.errors), (0, 1), "{strategy:?}");
        }
    }

    #[test]
    fn a_panicking_listing_still_marks_its_folder_done() {
        let queue = Arc::new(Queue::new(0u32, false));
        let panicker = {
            let queue = Arc::clone(&queue);
            std::thread::spawn(move || {
                let (_item, _taken) = queue.pop().unwrap();
                panic!("listing failed");
            })
        };
        assert!(panicker.join().is_err());
        // Without the guard this would wait forever for the folder.
        assert!(queue.pop().is_none());
    }

    #[test]
    fn lifo_takes_the_newest_folder_first() {
        let queue = Queue::new(0u32, true);
        let (first, taken) = queue.pop().unwrap();
        assert_eq!(first, 0);
        queue.push([1, 2, 3]);
        drop(taken);
        let order: Vec<u32> = std::iter::from_fn(|| queue.pop().map(|(i, _)| i)).collect();
        assert_eq!(order, [3, 2, 1]);
    }

    #[cfg(unix)]
    #[test]
    fn links_are_counted_not_followed() {
        let root = scratch_tree("links");
        std::os::unix::fs::symlink(root.join("d0"), root.join("d0-link")).unwrap();
        let w = walk(&root, 2, Strategy::Current);
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!((w.stats.dirs, w.stats.skipped), (39, 1), "{:?}", w.stats);
        assert_eq!(w.stats.entries, 39 + 160 + 1, "{:?}", w.stats);
    }

    #[test]
    fn strategies_parse_by_name() {
        for strategy in ALL {
            assert_eq!(Strategy::parse(strategy.name()), Some(strategy));
        }
        assert_eq!(Strategy::parse("fast"), None);
    }
}
