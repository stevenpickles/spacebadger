//! Process memory, for reporting. `None` where not implemented (macOS).

/// Current working set (resident set) in bytes.
pub fn current() -> Option<u64> {
    imp::current()
}

/// Peak working set (resident set) of this process so far, in bytes.
pub fn peak() -> Option<u64> {
    imp::peak()
}

#[cfg(windows)]
mod imp {
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows::Win32::System::Threading::GetCurrentProcess;

    fn counters() -> Option<PROCESS_MEMORY_COUNTERS> {
        let mut c = PROCESS_MEMORY_COUNTERS {
            cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            ..Default::default()
        };
        unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) }.ok()?;
        Some(c)
    }

    pub fn current() -> Option<u64> {
        counters().map(|c| c.WorkingSetSize as u64)
    }

    pub fn peak() -> Option<u64> {
        counters().map(|c| c.PeakWorkingSetSize as u64)
    }
}

#[cfg(target_os = "linux")]
mod imp {
    /// A `kB` field from `/proc/self/status`.
    fn status(field: &str) -> Option<u64> {
        let text = std::fs::read_to_string("/proc/self/status").ok()?;
        let line = text.lines().find(|l| l.starts_with(field))?;
        let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
        Some(kb * 1024)
    }

    pub fn current() -> Option<u64> {
        status("VmRSS:")
    }

    pub fn peak() -> Option<u64> {
        status("VmHWM:")
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod imp {
    pub fn current() -> Option<u64> {
        None
    }

    pub fn peak() -> Option<u64> {
        None
    }
}
