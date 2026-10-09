//! Native filesystem adapters. [`NativeFs`] is the adapter for the current OS.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(all(unix, not(target_os = "linux")))]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "linux")]
pub use linux::LinuxFs as NativeFs;
#[cfg(all(unix, not(target_os = "linux")))]
pub use unix::UnixFs as NativeFs;
#[cfg(windows)]
pub use windows::WindowsFs as NativeFs;
