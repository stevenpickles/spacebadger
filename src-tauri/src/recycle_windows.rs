//! Moving an item to the Recycle Bin on Windows, never deleting it outright.
//!
//! The shell deletes an item permanently when it can't recycle it: on a
//! network share or removable drive, when it's larger than the Recycle Bin
//! allows, or when the user turned recycling off. Before each item is
//! deleted, the shell tells a progress sink whether it will be recycled; the
//! sink here aborts any delete that wouldn't be.

use crate::delete::RecycleError;
use std::os::windows::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use windows::Win32::Foundation::{E_ABORT, RPC_E_CHANGED_MODE};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance, CoInitializeEx,
    CoUninitialize,
};
use windows::Win32::UI::Shell::{
    FOF_ALLOWUNDO, FOF_NO_UI, FOFX_EARLYFAILURE, FOFX_RECYCLEONDELETE, FileOperation,
    IFileOperation, IFileOperationProgressSink, IFileOperationProgressSink_Impl, IShellItem,
    SHCreateItemFromParsingName, TSF_DELETE_RECYCLE_IF_POSSIBLE,
};
use windows::core::{HRESULT, HSTRING, PCWSTR, Ref, Result, implement};

#[derive(Default)]
struct Seen {
    /// The shell was about to delete without recycling, and was stopped.
    refused: AtomicBool,
    /// An item was deleted without a copy in the Recycle Bin.
    unrecycled: AtomicBool,
}

#[implement(IFileOperationProgressSink)]
struct Guard(Arc<Seen>);

impl IFileOperationProgressSink_Impl for Guard_Impl {
    fn PreDeleteItem(&self, flags: u32, _item: Ref<IShellItem>) -> Result<()> {
        if flags & TSF_DELETE_RECYCLE_IF_POSSIBLE.0 as u32 == 0 {
            self.0.refused.store(true, Ordering::SeqCst);
            return Err(E_ABORT.into());
        }
        Ok(())
    }

    fn PostDeleteItem(
        &self,
        _flags: u32,
        _item: Ref<IShellItem>,
        result: HRESULT,
        recycled: Ref<IShellItem>,
    ) -> Result<()> {
        if result.is_ok() && recycled.is_null() {
            self.0.unrecycled.store(true, Ordering::SeqCst);
        }
        Ok(())
    }

    fn StartOperations(&self) -> Result<()> {
        Ok(())
    }
    fn FinishOperations(&self, _: HRESULT) -> Result<()> {
        Ok(())
    }
    fn PreRenameItem(&self, _: u32, _: Ref<IShellItem>, _: &PCWSTR) -> Result<()> {
        Ok(())
    }
    fn PostRenameItem(
        &self,
        _: u32,
        _: Ref<IShellItem>,
        _: &PCWSTR,
        _: HRESULT,
        _: Ref<IShellItem>,
    ) -> Result<()> {
        Ok(())
    }
    fn PreMoveItem(
        &self,
        _: u32,
        _: Ref<IShellItem>,
        _: Ref<IShellItem>,
        _: &PCWSTR,
    ) -> Result<()> {
        Ok(())
    }
    fn PostMoveItem(
        &self,
        _: u32,
        _: Ref<IShellItem>,
        _: Ref<IShellItem>,
        _: &PCWSTR,
        _: HRESULT,
        _: Ref<IShellItem>,
    ) -> Result<()> {
        Ok(())
    }
    fn PreCopyItem(
        &self,
        _: u32,
        _: Ref<IShellItem>,
        _: Ref<IShellItem>,
        _: &PCWSTR,
    ) -> Result<()> {
        Ok(())
    }
    fn PostCopyItem(
        &self,
        _: u32,
        _: Ref<IShellItem>,
        _: Ref<IShellItem>,
        _: &PCWSTR,
        _: HRESULT,
        _: Ref<IShellItem>,
    ) -> Result<()> {
        Ok(())
    }
    fn PreNewItem(&self, _: u32, _: Ref<IShellItem>, _: &PCWSTR) -> Result<()> {
        Ok(())
    }
    fn PostNewItem(
        &self,
        _: u32,
        _: Ref<IShellItem>,
        _: &PCWSTR,
        _: &PCWSTR,
        _: u32,
        _: HRESULT,
        _: Ref<IShellItem>,
    ) -> Result<()> {
        Ok(())
    }
    fn UpdateProgress(&self, _: u32, _: u32) -> Result<()> {
        Ok(())
    }
    fn ResetTimer(&self) -> Result<()> {
        Ok(())
    }
    fn PauseTimer(&self) -> Result<()> {
        Ok(())
    }
    fn ResumeTimer(&self) -> Result<()> {
        Ok(())
    }
}

/// COM for this thread, released when dropped if this call started it.
struct Com(bool);

impl Com {
    fn init() -> std::result::Result<Self, String> {
        // SAFETY: plain COM initialization for the calling thread.
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
        if hr == RPC_E_CHANGED_MODE {
            // Already initialized differently on this thread; usable as is.
            Ok(Self(false))
        } else if hr.is_ok() {
            Ok(Self(true))
        } else {
            Err(format!("the Windows shell is unavailable ({hr})"))
        }
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: balances the successful CoInitializeEx in `init`.
            unsafe { CoUninitialize() };
        }
    }
}

/// Why an item stayed where it was.
pub const NOT_RECYCLABLE: &str = "It can't go to the Recycle Bin here (it may be on a network or removable drive, be too large for the Recycle Bin, or recycling may be turned off), and SpaceBadger never deletes permanently.";

/// Moves `path` to the Recycle Bin. Fails, leaving the item alone, if
/// Windows would delete it permanently instead.
pub fn recycle(path: &Path) -> std::result::Result<(), RecycleError> {
    let _com = Com::init().map_err(RecycleError::Failed)?;
    let seen = Arc::new(Seen::default());
    let name = HSTRING::from(shell_path(path).as_os_str());
    // SAFETY: COM is initialized on this thread; every pointer passed is
    // valid for the duration of the call.
    let performed = unsafe {
        (|| -> Result<bool> {
            let op: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_ALL)?;
            op.SetOperationFlags(
                FOF_NO_UI | FOF_ALLOWUNDO | FOFX_RECYCLEONDELETE | FOFX_EARLYFAILURE,
            )?;
            let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(name.as_ptr()), None)?;
            let guard: IFileOperationProgressSink = Guard(Arc::clone(&seen)).into();
            op.DeleteItem(&item, &guard)?;
            let done = op.PerformOperations();
            let aborted = op.GetAnyOperationsAborted()?.as_bool();
            done.map(|()| !aborted)
        })()
    };
    classify(
        seen.refused.load(Ordering::SeqCst),
        seen.unrecycled.load(Ordering::SeqCst),
        performed.map_err(|e| e.message()),
    )
}

/// The result of a move from what the progress sink saw (a delete it
/// stopped, a delete without a copy) and what the operation returned
/// (whether it ran unaborted, or its error). A delete without a copy is
/// reported above everything else, since nothing can bring it back.
fn classify(
    refused: bool,
    unrecycled: bool,
    performed: std::result::Result<bool, String>,
) -> std::result::Result<(), RecycleError> {
    if unrecycled {
        return Err(RecycleError::Unrecycled);
    }
    if refused {
        return Err(RecycleError::Refused(NOT_RECYCLABLE.into()));
    }
    match performed {
        Ok(true) => Ok(()),
        Ok(false) => Err(RecycleError::Failed(
            "The move was cancelled or blocked. It may be in use.".into(),
        )),
        Err(message) => Err(RecycleError::Failed(format!(
            "Couldn't move it to the Recycle Bin: {message}"
        ))),
    }
}

/// An absolute path in the form the shell parses: no `\\?\` prefix.
fn shell_path(path: &Path) -> PathBuf {
    let path = std::path::absolute(path).unwrap_or_else(|_| path.to_owned());
    let mut components = path.components();
    let mut out = match components.next() {
        Some(Component::Prefix(p)) => match p.kind() {
            Prefix::VerbatimDisk(letter) => PathBuf::from(format!("{}:\\", letter as char)),
            Prefix::VerbatimUNC(host, share) => {
                let mut s = std::ffi::OsString::from(r"\\");
                s.push(host);
                s.push(r"\");
                s.push(share);
                s.push(r"\");
                PathBuf::from(s)
            }
            _ => return path,
        },
        _ => return path,
    };
    for c in components {
        if let Component::Normal(part) = c {
            out.push(part);
        }
    }
    out
}

/// Why the shell might reach something other than the exact item at
/// `path`, if it might. The checks read `\\?\` paths exactly, but the shell
/// parses them the way Win32 does, so every name in the path, from the
/// drive down, must mean the same to both: none ending with a dot or a
/// space (Win32 drops them), none that is a device such as `NUL` or `COM1`,
/// no `.` or `..`, no `:` (a data stream) or wildcard, and Win32 must give
/// back the shell form of the path unchanged.
pub fn misread(path: &Path) -> Option<String> {
    let inexact = || {
        Some(format!(
            "Its path {} can't be given to the Recycle Bin exactly as it is, so it could reach another item; nothing was moved.",
            quoted(path.as_os_str())
        ))
    };
    let mut components = path.components();
    let verbatim = match components.next() {
        Some(Component::Prefix(p)) if path.is_absolute() => match p.kind() {
            Prefix::Disk(_) | Prefix::UNC(..) => false,
            Prefix::VerbatimDisk(_) | Prefix::VerbatimUNC(..) => true,
            _ => return inexact(),
        },
        _ => return inexact(),
    };
    for c in components {
        let part = match c {
            Component::RootDir => continue,
            Component::Normal(part) => part,
            _ => return inexact(),
        };
        let name = part.to_string_lossy();
        if name.ends_with(['.', ' ']) {
            return Some(format!(
                "The name {} in its path ends with a dot or a space, which the Recycle Bin would mistake for another item; nothing was moved.",
                quoted(part)
            ));
        }
        if device(&name) {
            return Some(format!(
                "The name {} in its path is reserved by Windows for a device, so the Recycle Bin can't reach it safely; nothing was moved.",
                quoted(part)
            ));
        }
        if name.chars().any(|c| c < ' ' || r#"<>:"/\|?*"#.contains(c)) {
            return inexact();
        }
    }
    // Anything else Win32 would rewrite: `absolute` calls `GetFullPathNameW`
    // for a path without `\\?\`, as `shell_path` already did for one that
    // never had it. A `/` is a separator to both (the checks read it as
    // one too).
    let shell = shell_path(path);
    let rewritten = |p: &Path| {
        std::path::absolute(p).ok().as_deref().map(Path::as_os_str) != Some(p.as_os_str())
    };
    let differs = || {
        let wide = path.as_os_str().encode_wide().map(|c| {
            if c == u16::from(b'/') {
                u16::from(b'\\')
            } else {
                c
            }
        });
        !wide.eq(shell.as_os_str().encode_wide())
    };
    if rewritten(&shell) || (!verbatim && differs()) {
        return inexact();
    }
    None
}

/// Whether Win32 takes a name for a device, as it does `nul` and `COM1`,
/// with any extension or trailing spaces before it.
fn device(name: &str) -> bool {
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches(' ');
    let upper = stem.to_uppercase();
    match upper.as_str() {
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" => true,
        _ => {
            let mut chars = upper.chars();
            let start: String = chars.by_ref().take(3).collect();
            let digit = chars.next();
            (start == "COM" || start == "LPT")
                && chars.next().is_none()
                && digit.is_some_and(|d| d.is_ascii_digit() || "¹²³".contains(d))
        }
    }
}

fn quoted(text: &std::ffi::OsStr) -> String {
    format!("“{}”", text.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_delete_without_a_copy_outranks_every_other_result() {
        assert_eq!(classify(false, false, Ok(true)), Ok(()));
        assert_eq!(
            classify(false, true, Ok(true)),
            Err(RecycleError::Unrecycled)
        );
        assert_eq!(
            classify(true, true, Err("aborted".into())),
            Err(RecycleError::Unrecycled)
        );
        assert_eq!(
            classify(true, false, Ok(false)),
            Err(RecycleError::Refused(NOT_RECYCLABLE.into()))
        );
        assert!(matches!(
            classify(false, false, Ok(false)),
            Err(RecycleError::Failed(m)) if m.contains("cancelled or blocked")
        ));
        assert_eq!(
            classify(false, false, Err("Access is denied.".into())),
            Err(RecycleError::Failed(
                "Couldn't move it to the Recycle Bin: Access is denied.".into()
            ))
        );
    }

    #[test]
    fn verbatim_paths_become_shell_paths() {
        assert_eq!(
            shell_path(Path::new(r"\\?\C:\dir\file.txt")),
            Path::new(r"C:\dir\file.txt")
        );
        assert_eq!(
            shell_path(Path::new(r"\\?\UNC\host\share\dir\f")),
            Path::new(r"\\host\share\dir\f")
        );
        assert_eq!(
            shell_path(Path::new(r"C:\dir\file.txt")),
            Path::new(r"C:\dir\file.txt")
        );
        assert!(shell_path(Path::new("relative.txt")).is_absolute());
    }

    /// Every name in the path counts, the drive's folders included, in
    /// each form a path can take.
    #[test]
    fn paths_the_shell_would_read_differently_are_refused() {
        let fine = [
            r"C:\dir\file.txt",
            r"\\?\C:\dir\file.txt",
            r"\\host\share\dir\f",
            r"\\?\UNC\host\share\dir\f",
            r"C:\",
            r"\\?\C:\",
            r"C:\.git\a b\file.tar.gz",
            r"C:\dir\console\COM10\LPT\nul-ish\auxiliary.txt",
            r"C:\PROGRA~1\file",
            "C:/dir/file.txt",
        ];
        for path in fine {
            assert_eq!(misread(Path::new(path)), None, "{path}");
        }
        let trailing = [
            r"C:\root.\file.txt",
            r"\\?\C:\root.\file.txt",
            r"\\?\C:\up \root\file.txt",
            r"\\?\C:\dir\file.",
            r"\\?\C:\dir\file ",
            r"\\?\UNC\host\share\root.\f",
            r"\\?\C:\...\f",
        ];
        for path in trailing {
            assert!(
                misread(Path::new(path)).is_some_and(|m| m.contains("ends with a dot or a space")),
                "{path}"
            );
        }
        let devices = [
            r"\\?\C:\dir\NUL",
            r"\\?\C:\con\file",
            r"\\?\C:\dir\aux.txt",
            r"\\?\C:\dir\Com1",
            r"\\?\C:\dir\lpt9.log",
            r"\\?\C:\dir\COM¹",
            r"\\?\C:\dir\nul .txt",
            r"\\?\C:\dir\CONOUT$",
        ];
        for path in devices {
            assert!(
                misread(Path::new(path)).is_some_and(|m| m.contains("reserved by Windows")),
                "{path}"
            );
        }
        let inexact = [
            r"\\?\C:\dir\..\file",
            r"\\?\C:\dir\.\file",
            // A `.` here is skipped by `components`; Win32 rewriting the
            // path gives it away.
            r"C:\dir\.\file",
            r"C:\dir\..\file",
            r"\\?\C:\dir\file:stream",
            r"\\?\C:\dir\a/b",
            r"\\?\C:\dir\a*",
            r"\\?\C:\dir\a?b",
            "\\\\?\\C:\\dir\\a\tb",
            r"\\?\Volume{0b1c2d3e-0000-0000-0000-000000000000}\dir\f",
            r"\\.\C:\dir\f",
            r"C:dir\f",
            r"dir\f",
        ];
        for path in inexact {
            assert!(
                misread(Path::new(path)).is_some_and(|m| m.contains("exactly as it is")),
                "{path}"
            );
        }
    }

    /// A network path has no Recycle Bin, so the shell would delete the
    /// file outright; it must be left alone. Skipped when the local
    /// administrative share isn't reachable.
    #[test]
    fn never_deletes_what_it_cannot_recycle() {
        let dir = std::env::temp_dir().join(format!("sb-norecycle-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("keep.bin");
        std::fs::write(&file, b"must survive").unwrap();
        let local = dir.to_string_lossy().to_string();
        let Some(rest) = local.strip_prefix("C:\\") else {
            return;
        };
        let unc = PathBuf::from(format!(r"\\localhost\C$\{rest}")).join("keep.bin");
        if std::fs::metadata(&unc).is_err() {
            eprintln!("skipped: {} isn't reachable", unc.display());
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let result = recycle(&unc);
        let survived = file.exists();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            result,
            Err(RecycleError::Refused(NOT_RECYCLABLE.to_owned()))
        );
        assert!(survived, "the file was deleted");
    }

    /// Puts a small file in the Recycle Bin. Run by hand:
    /// `cargo test -p spacebadger recycles_a_local_file -- --ignored`.
    #[test]
    #[ignore]
    fn recycles_a_local_file() {
        let file = std::env::temp_dir().join(format!("sb-recycle-test-{}.txt", std::process::id()));
        std::fs::write(&file, b"recycle me").unwrap();
        assert_eq!(recycle(&file), Ok(()));
        assert!(!file.exists());
    }
}
