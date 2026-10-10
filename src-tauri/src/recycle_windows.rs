//! Moving an item to the Recycle Bin on Windows, never deleting it outright.
//!
//! The shell deletes an item permanently when it can't recycle it: on a
//! network share or removable drive, when it's larger than the Recycle Bin
//! allows, or when the user turned recycling off. Before each item is
//! deleted, the shell tells a progress sink whether it will be recycled; the
//! sink here aborts any delete that wouldn't be.

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
pub fn recycle(path: &Path) -> std::result::Result<(), String> {
    let _com = Com::init()?;
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
    if seen.refused.load(Ordering::SeqCst) {
        return Err(NOT_RECYCLABLE.into());
    }
    if seen.unrecycled.load(Ordering::SeqCst) {
        return Err("Windows deleted it without keeping a copy in the Recycle Bin.".into());
    }
    match performed {
        Ok(true) => Ok(()),
        Ok(false) => Err("The move was cancelled or blocked. It may be in use.".into()),
        Err(e) => Err(format!(
            "Couldn't move it to the Recycle Bin: {}",
            e.message()
        )),
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(result, Err(NOT_RECYCLABLE.to_owned()));
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
