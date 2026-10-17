//! CPU time, for telling kernel work from user work and the scan's
//! aggregator from its workers: the whole process (`GetProcessTimes`,
//! `getrusage`) and, where the platform allows, each thread by name.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

/// Kernel and user CPU time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cpu {
    pub kernel: Duration,
    pub user: Duration,
}

impl Cpu {
    /// This process's CPU time so far, if the platform reports it.
    pub fn process() -> Option<Self> {
        imp::process()
    }

    pub fn since(self, earlier: Self) -> Self {
        Self {
            kernel: self.kernel.saturating_sub(earlier.kernel),
            user: self.user.saturating_sub(earlier.user),
        }
    }

    pub fn total(self) -> Duration {
        self.kernel + self.user
    }
}

impl std::ops::AddAssign for Cpu {
    fn add_assign(&mut self, other: Self) {
        self.kernel += other.kernel;
        self.user += other.user;
    }
}

/// Which part of a measurement a thread belongs to, by the names `sb-scan`
/// and this benchmark give their threads.
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
pub fn role(name: &str) -> &'static str {
    // Linux truncates thread names to 15 bytes: "sb-scan-aggrega".
    if name.starts_with("sb-scan-aggrega") {
        "aggregator"
    } else if name.starts_with("sb-scan-worker") {
        "workers"
    } else if name.starts_with("sb-bench-floor") {
        "floor"
    } else if name.starts_with(WATCHER.get(..15).unwrap_or(WATCHER)) {
        WATCHER_ROLE
    } else {
        "other"
    }
}

/// The watcher thread's name, and its role, which comparisons leave out.
const WATCHER: &str = "sb-bench-threads";
pub const WATCHER_ROLE: &str = "watcher";

/// CPU time per role, with the number of threads seen in each.
pub type ByRole = BTreeMap<&'static str, (Cpu, usize)>;

/// Process CPU time without the watcher's own, so a measurement that
/// watches threads compares fairly with one that doesn't.
pub fn without_watcher(process: Cpu, roles: Option<&ByRole>) -> Cpu {
    match roles.and_then(|r| r.get(WATCHER_ROLE)) {
        Some((watcher, _)) => process.since(*watcher),
        None => process,
    }
}

/// Prints CPU per role, as continuation lines of a `cpu:` line.
pub fn print_roles(roles: &ByRole) {
    for (role, (cpu, threads)) in roles {
        println!(
            "         {role} ({threads} thread{}): kernel {}, user {}",
            if *threads == 1 { "" } else { "s" },
            crate::ms(cpu.kernel),
            crate::ms(cpu.user)
        );
    }
}

/// Watches this process's threads from a background thread until
/// [`Threads::finish`], keeping each one's CPU time and name.
///
/// On Windows each thread's handle is kept open once seen, so its final
/// time is read exactly even after it has exited; only threads that start
/// and finish between two 5 ms looks are missed. On Linux the last look
/// before a thread exits is kept, so up to 5 ms per thread is lost. Other
/// platforms report nothing.
pub struct Threads {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<Option<ByRole>>,
}

impl Threads {
    pub fn watch() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let handle = {
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name(WATCHER.into())
                .spawn(move || imp::watch(&stop))
                .expect("spawn thread watcher")
        };
        Self { stop, handle }
    }

    /// CPU per role over the threads' whole lives (threads that existed
    /// before [`Threads::watch`] count from their start).
    pub fn finish(self) -> Option<ByRole> {
        self.stop.store(true, Ordering::Relaxed);
        self.handle.join().expect("thread watcher")
    }
}

#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
const LOOK_EVERY: Duration = Duration::from_millis(5);

/// Adds `cpu` to `role` in `out`.
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
fn tally(out: &mut ByRole, name: &str, cpu: Cpu) {
    let slot = out.entry(role(name)).or_default();
    slot.0 += cpu;
    slot.1 += 1;
}

#[cfg(windows)]
mod imp {
    use super::{ByRole, Cpu, LOOK_EVERY, tally};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    use windows::Win32::Foundation::{
        CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, FILETIME, HANDLE, HLOCAL, LocalFree,
    };
    use windows::Win32::System::Threading::{
        GetCurrentProcess, GetProcessTimes, GetThreadDescription, GetThreadId, GetThreadTimes,
        THREAD_QUERY_LIMITED_INFORMATION,
    };

    struct Owned(HANDLE);

    impl Drop for Owned {
        fn drop(&mut self) {
            // SAFETY: the handle was opened by us and is closed once.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    fn duration(t: FILETIME) -> Duration {
        let ticks = (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
        Duration::from_nanos(ticks * 100)
    }

    pub fn process() -> Option<Cpu> {
        let [mut c, mut e, mut k, mut u] = [FILETIME::default(); 4];
        // SAFETY: the pseudo-handle is always valid; the out pointers live.
        unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u) }.ok()?;
        Some(Cpu {
            kernel: duration(k),
            user: duration(u),
        })
    }

    fn times(thread: HANDLE) -> Option<Cpu> {
        let [mut c, mut e, mut k, mut u] = [FILETIME::default(); 4];
        // SAFETY: `thread` is an open thread handle; the out pointers live.
        unsafe { GetThreadTimes(thread, &mut c, &mut e, &mut k, &mut u) }.ok()?;
        Some(Cpu {
            kernel: duration(k),
            user: duration(u),
        })
    }

    fn name(thread: HANDLE) -> String {
        // SAFETY: `thread` is open; the string the call allocates is freed
        // with LocalFree once copied, as its documentation asks.
        let Ok(text) = (unsafe { GetThreadDescription(thread) }) else {
            return String::new();
        };
        let name = unsafe { text.to_string() }.unwrap_or_default();
        unsafe {
            let _ = LocalFree(Some(HLOCAL(text.0.cast())));
        }
        name
    }

    /// How many looks ask an unnamed thread for its name: a thread is
    /// named just after it starts, so one still unnamed after a few looks
    /// stays unnamed (it's asked once more at the end).
    const NAME_ASKS: u8 = 3;

    /// A thread seen: its handle, kept open, its name, and how often it was
    /// asked for it.
    struct Seen {
        handle: Owned,
        name: String,
        asked: u8,
    }

    impl Seen {
        fn ask(&mut self) {
            if self.name.is_empty() && self.asked < NAME_ASKS {
                self.asked += 1;
                self.name = name(self.handle.0);
            }
        }
    }

    #[link(name = "ntdll")]
    unsafe extern "system" {
        /// The thread of `process` after `thread` (the first for null),
        /// opened with `access`; enumerates this process's threads only,
        /// unlike a Toolhelp snapshot, which copies every thread on the
        /// system and took hundreds of milliseconds on a busy machine.
        fn NtGetNextThread(
            process: HANDLE,
            thread: HANDLE,
            access: u32,
            attributes: u32,
            flags: u32,
            next: *mut HANDLE,
        ) -> i32;
    }

    /// Keeps a handle to every thread of this process not seen yet, and
    /// asks the unnamed ones their names.
    fn look(seen: &mut HashMap<u32, Seen>) {
        let mut previous: Option<Owned> = None;
        loop {
            let mut next = HANDLE::default();
            let after = previous.as_ref().map_or(HANDLE::default(), |h| h.0);
            // SAFETY: `after` is null or a thread handle we hold open.
            let status = unsafe {
                NtGetNextThread(
                    GetCurrentProcess(),
                    after,
                    THREAD_QUERY_LIMITED_INFORMATION.0,
                    0,
                    0,
                    &mut next,
                )
            };
            if status < 0 {
                return;
            }
            let next = Owned(next);
            // SAFETY: `next` is an open thread handle.
            let tid = unsafe { GetThreadId(next.0) };
            if let Some(thread) = seen.get_mut(&tid) {
                thread.ask();
                previous = Some(next);
            } else {
                // Keep this handle; continue from a duplicate of it.
                let mut dup = HANDLE::default();
                // SAFETY: duplicating our own open handle within our process.
                let ok = unsafe {
                    DuplicateHandle(
                        GetCurrentProcess(),
                        next.0,
                        GetCurrentProcess(),
                        &mut dup,
                        0,
                        false,
                        DUPLICATE_SAME_ACCESS,
                    )
                }
                .is_ok();
                let mut thread = Seen {
                    handle: next,
                    name: String::new(),
                    asked: 0,
                };
                thread.ask();
                seen.insert(tid, thread);
                if !ok {
                    return;
                }
                previous = Some(Owned(dup));
            }
        }
    }

    pub fn watch(stop: &AtomicBool) -> Option<ByRole> {
        let mut seen = HashMap::new();
        while !stop.load(Ordering::Relaxed) {
            look(&mut seen);
            std::thread::sleep(LOOK_EVERY);
        }
        look(&mut seen);
        let mut out = ByRole::new();
        for thread in seen.values_mut() {
            if thread.name.is_empty() {
                thread.name = name(thread.handle.0);
            }
            if let Some(cpu) = times(thread.handle.0) {
                tally(&mut out, &thread.name, cpu);
            }
        }
        Some(out)
    }
}

#[cfg(unix)]
mod imp {
    use super::Cpu;
    use std::time::Duration;

    pub fn process() -> Option<Cpu> {
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
        // SAFETY: getrusage fills the struct it's given.
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
            return None;
        }
        // SAFETY: initialised by the successful call (and zeroed before).
        let usage = unsafe { usage.assume_init() };
        let time = |t: libc::timeval| {
            Duration::from_secs(t.tv_sec as u64) + Duration::from_micros(t.tv_usec as u64)
        };
        Some(Cpu {
            kernel: time(usage.ru_stime),
            user: time(usage.ru_utime),
        })
    }

    #[cfg(target_os = "linux")]
    pub use linux::watch;

    #[cfg(target_os = "linux")]
    mod linux {
        use super::super::{ByRole, Cpu, LOOK_EVERY, tally};
        use std::collections::HashMap;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;

        /// Name, user and system time of one thread from its `stat` file.
        fn read(tid: &str, tick: Duration) -> Option<(String, Cpu)> {
            let text = std::fs::read_to_string(format!("/proc/self/task/{tid}/stat")).ok()?;
            let open = text.find('(')?;
            let close = text.rfind(')')?;
            let name = text[open + 1..close].to_string();
            // Fields after the name start at field 3 (state); utime and
            // stime are fields 14 and 15.
            let mut rest = text[close + 1..].split_whitespace().skip(11);
            let user: u32 = rest.next()?.parse().ok()?;
            let kernel: u32 = rest.next()?.parse().ok()?;
            Some((
                name,
                Cpu {
                    kernel: tick * kernel,
                    user: tick * user,
                },
            ))
        }

        pub fn watch(stop: &AtomicBool) -> Option<ByRole> {
            // SAFETY: sysconf has no preconditions.
            let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
            let tick = Duration::from_secs(1) / u32::try_from(hz).ok().filter(|&h| h > 0)?;
            let mut seen: HashMap<String, (String, Cpu)> = HashMap::new();
            loop {
                let done = stop.load(Ordering::Relaxed);
                if let Ok(dir) = std::fs::read_dir("/proc/self/task") {
                    for task in dir.flatten() {
                        let tid = task.file_name().to_string_lossy().into_owned();
                        if let Some(now) = read(&tid, tick) {
                            seen.insert(tid, now);
                        }
                    }
                }
                if done {
                    break;
                }
                std::thread::sleep(LOOK_EVERY);
            }
            let mut out = ByRole::new();
            for (name, cpu) in seen.values() {
                tally(&mut out, name, *cpu);
            }
            Some(out)
        }
    }

    #[cfg(not(target_os = "linux"))]
    pub fn watch(_stop: &std::sync::atomic::AtomicBool) -> Option<super::ByRole> {
        None
    }
}

#[cfg(not(any(windows, unix)))]
mod imp {
    use super::{ByRole, Cpu};

    pub fn process() -> Option<Cpu> {
        None
    }

    pub fn watch(_stop: &std::sync::atomic::AtomicBool) -> Option<ByRole> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_follow_scan_thread_names() {
        assert_eq!(role("sb-scan-aggregator"), "aggregator");
        assert_eq!(role("sb-scan-aggrega"), "aggregator");
        assert_eq!(role("sb-scan-worker-3"), "workers");
        assert_eq!(role("main"), "other");
        assert_eq!(role(""), "other");
        assert_eq!(role("sb-bench-floor-2"), "floor");
        assert_eq!(role("sb-bench-threads"), WATCHER_ROLE);
        assert_eq!(role("sb-bench-thread"), WATCHER_ROLE);
    }

    #[test]
    fn the_watchers_cpu_is_left_out() {
        let ms = Duration::from_millis;
        let process = Cpu {
            kernel: ms(900),
            user: ms(300),
        };
        let mut roles = ByRole::new();
        roles.insert(
            WATCHER_ROLE,
            (
                Cpu {
                    kernel: ms(40),
                    user: ms(10),
                },
                1,
            ),
        );
        let without = without_watcher(process, Some(&roles));
        assert_eq!((without.kernel, without.user), (ms(860), ms(290)));
        assert_eq!(without_watcher(process, None), process);
    }

    #[test]
    fn busy_threads_are_attributed_by_name() {
        let threads = Threads::watch();
        let spin = |name: &str| {
            std::thread::Builder::new()
                .name(name.into())
                .spawn(|| {
                    let t = std::time::Instant::now();
                    let mut x = 0u64;
                    while t.elapsed() < Duration::from_millis(300) {
                        x = std::hint::black_box(x.wrapping_add(1));
                    }
                })
                .unwrap()
        };
        let a = spin("sb-scan-aggregator");
        let w = spin("sb-scan-worker-0");
        a.join().unwrap();
        w.join().unwrap();
        let Some(roles) = threads.finish() else {
            return; // Not reported on this platform.
        };
        for role in ["aggregator", "workers"] {
            let (cpu, n) = roles.get(role).copied().unwrap_or_default();
            assert_eq!(n, 1, "{role}: {roles:?}");
            assert!(cpu.total() > Duration::ZERO, "{role}: {roles:?}");
        }
    }

    #[test]
    fn process_cpu_moves_forward() {
        let Some(before) = Cpu::process() else {
            return;
        };
        let t = std::time::Instant::now();
        let mut x = 0u64;
        while t.elapsed() < Duration::from_millis(100) {
            x = std::hint::black_box(x.wrapping_add(1));
        }
        let after = Cpu::process().unwrap();
        assert!(after.since(before).total() >= Duration::from_millis(30));
    }
}
