//! Moving an item to the Recycle Bin on Windows, never deleting it outright.
//!
//! The shell deletes an item permanently when it can't recycle it: on a
//! network share or removable drive, when it's larger than the Recycle Bin
//! allows, or when the user turned recycling off. Before each item is
//! deleted, the shell tells a progress sink whether it will be recycled; the
//! sink here aborts any delete that wouldn't be.

use crate::delete::RecycleError;
use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use windows::Win32::Foundation::{E_ABORT, RPC_E_CHANGED_MODE};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance, CoInitializeEx,
    CoTaskMemFree, CoUninitialize,
};
use windows::Win32::UI::Shell::{
    FOF_ALLOWUNDO, FOF_NO_UI, FOFX_EARLYFAILURE, FOFX_RECYCLEONDELETE, FileOperation,
    IFileOperation, IFileOperationProgressSink, IFileOperationProgressSink_Impl, IShellItem,
    SHCreateItemFromParsingName, SIGDN_FILESYSPATH, TSF_DELETE_RECYCLE_IF_POSSIBLE,
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

/// Why an item the shell would take for another is left alone.
pub const ELSEWHERE: &str = "Windows Explorer reads its path as leading to a different item (a folder in it may be a special shell folder), so it was left alone; nothing was moved.";

/// Moves `path` to the Recycle Bin. Fails, leaving the item alone, if
/// Windows would delete it permanently instead, or if the shell takes the
/// path to anything but the object at it.
pub fn recycle(path: &Path) -> std::result::Result<(), RecycleError> {
    let _com = Com::init().map_err(RecycleError::Failed)?;
    let seen = Arc::new(Seen::default());
    let item = shell_item(path)?;
    confirm(path, &item)?;
    // SAFETY: COM is initialized on this thread; every pointer passed is
    // valid for the duration of the call.
    let performed = unsafe {
        (|| -> Result<bool> {
            let op: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_ALL)?;
            op.SetOperationFlags(
                FOF_NO_UI | FOF_ALLOWUNDO | FOFX_RECYCLEONDELETE | FOFX_EARLYFAILURE,
            )?;
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
        performed.map_err(|e| describe(&e)),
    )
}

/// An error's message, or its code when Windows has no message for it.
fn describe(e: &windows::core::Error) -> String {
    let message = e.message();
    if message.trim().is_empty() {
        format!("error {}.", e.code())
    } else {
        message
    }
}

/// The shell's item for `path`. COM must be initialized on this thread.
fn shell_item(path: &Path) -> std::result::Result<IShellItem, RecycleError> {
    let name = HSTRING::from(shell_path(path).as_os_str());
    // SAFETY: `name` is NUL-terminated and outlives the call.
    unsafe { SHCreateItemFromParsingName(PCWSTR(name.as_ptr()), None) }.map_err(|e| {
        RecycleError::Failed(format!(
            "Couldn't move it to the Recycle Bin: {}",
            describe(&e)
        ))
    })
}

/// Refuses unless the shell's item is the object at `path` as the checks
/// read it. The shell parses with its own namespace on top of Win32: a
/// folder named `x.{CLSID}`, or a system folder whose `desktop.ini` gives a
/// CLSID, hands everything below it to another namespace, which can put a
/// user's real file, or the Recycle Bin, where the scan saw a different
/// one. So the file-system path the shell resolved must lead to the same
/// file ID on the same drive (not the same text: it may be in 8.3 form).
fn confirm(path: &Path, item: &IShellItem) -> std::result::Result<(), RecycleError> {
    let elsewhere = || RecycleError::Refused(ELSEWHERE.into());
    let resolved = filesystem_path(item).ok_or_else(elsewhere)?;
    match crate::identity::same_object(path, &resolved) {
        Ok(true) => Ok(()),
        _ => Err(elsewhere()),
    }
}

/// Where in the file system the shell's item is, if anywhere.
fn filesystem_path(item: &IShellItem) -> Option<PathBuf> {
    // SAFETY: a valid item; the string returned is ours to free, and is
    // copied before it is.
    unsafe {
        let name = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let path = PathBuf::from(OsString::from_wide(name.as_wide()));
        CoTaskMemFree(Some(name.0.cast_const().cast()));
        Some(path)
    }
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
/// `path`, if it might: a quick look at the names before the shell's own
/// reading is checked (`confirm`). The checks read `\\?\` paths exactly,
/// but the shell parses them the way Win32 does, so every name in the path,
/// a network server and share included, must mean the same to both: none
/// ending with a dot or a space (Win32 drops them), none that is a device
/// such as `NUL` or `COM1`, none ending in `.{CLSID}` (a shell folder), no
/// `.` or `..`, no `:` (a data stream) or wildcard, and Win32 must give back
/// the shell form of the path unchanged.
pub fn misread(path: &Path) -> Option<String> {
    let inexact = || {
        Some(format!(
            "Its path {} can't be given to the Recycle Bin exactly as it is, so it could reach another item; nothing was moved.",
            quoted(shell_path(path).as_os_str())
        ))
    };
    let mut components = path.components();
    let (verbatim, mut names) = match components.next() {
        Some(Component::Prefix(p)) if path.is_absolute() => match p.kind() {
            Prefix::Disk(_) => (false, vec![]),
            Prefix::VerbatimDisk(_) => (true, vec![]),
            Prefix::UNC(server, share) => (false, vec![server, share]),
            Prefix::VerbatimUNC(server, share) => (true, vec![server, share]),
            _ => return inexact(),
        },
        _ => return inexact(),
    };
    for c in components {
        match c {
            Component::RootDir => {}
            Component::Normal(part) => names.push(part),
            _ => return inexact(),
        }
    }
    for part in names {
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
        if shell_folder(&name) {
            return Some(format!(
                "The name {} in its path makes it a special shell folder, which the Recycle Bin would take for another place; nothing was moved.",
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

/// Whether a name is a device to Win32 and the shell, as `nul` and `COM1`
/// are. With an extension (`nul.txt`) it depends on the Windows version,
/// which the round trip through Win32 settles.
fn device(name: &str) -> bool {
    let upper = name.to_uppercase();
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

/// Whether a folder name ends in `.{CLSID}`, which makes the shell treat it
/// as that shell folder.
fn shell_folder(name: &str) -> bool {
    let Some((_, ext)) = name.rsplit_once('.') else {
        return false;
    };
    let Some(guid) = ext.strip_prefix('{').and_then(|g| g.strip_suffix('}')) else {
        return false;
    };
    let groups: Vec<&str> = guid.split('-').collect();
    groups.iter().map(|g| g.len()).eq([8, 4, 4, 4, 12])
        && groups
            .iter()
            .all(|g| g.chars().all(|c| c.is_ascii_hexdigit()))
}

fn quoted(text: &OsStr) -> String {
    format!("“{}”", text.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_SYSTEM,
        FILE_FLAGS_AND_ATTRIBUTES, SetFileAttributesW,
    };
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, IPersistFile};
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
    use windows::core::Interface;

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
            r"\\?\UNC\server.\share\f",
            r"\\server\share \f",
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
            r"\\?\C:\dir\Com1",
            r"\\?\C:\dir\COM¹",
            r"\\?\C:\dir\CONOUT$",
        ];
        for path in devices {
            assert!(
                misread(Path::new(path)).is_some_and(|m| m.contains("reserved by Windows")),
                "{path}"
            );
        }
        // Devices only on some versions of Windows, which Win32 itself
        // shows by rewriting the path.
        for path in [r"C:\dir\aux.js", r"C:\dir\lpt9.log", r"C:\nul .txt\f"] {
            assert!(
                misread(Path::new(path)).is_none_or(|m| m.contains("exactly as it is")),
                "{path}"
            );
        }
        let shell_folders = [
            r"\\?\C:\x.{59031a47-3f72-44a7-89c5-5595fe6b30ee}\f",
            r"C:\j.{645FF040-5081-101B-9F08-00AA002F954E}\a.txt",
        ];
        for path in shell_folders {
            assert!(
                misread(Path::new(path)).is_some_and(|m| m.contains("special shell folder")),
                "{path}"
            );
        }
        assert_eq!(misread(Path::new(r"C:\notes.{draft}\f")), None);
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
        // Named as the user would write it.
        assert!(
            misread(Path::new(r"\\?\C:\dir\a*"))
                .is_some_and(|m| m.starts_with(r"Its path “C:\dir\a*”")),
        );
    }

    /// A folder in the path that the shell treats as a shell folder, by its
    /// `.{CLSID}` name or its `desktop.ini`, leads the shell elsewhere: here
    /// to the user's profile folder, read but never touched. Nothing is
    /// moved; only the shell's reading is checked.
    #[test]
    fn a_path_the_shell_resolves_elsewhere_is_refused() {
        let tmp = Dir::new("sb-shellns");
        // The users-files folder: the profile, which always has AppData.
        let clsid = "{59031a47-3f72-44a7-89c5-5595fe6b30ee}";
        let named = tmp.0.join(format!("x.{clsid}")).join("AppData");
        let system = tmp.0.join("dj");
        std::fs::create_dir_all(&named).unwrap();
        std::fs::create_dir_all(system.join("AppData")).unwrap();
        std::fs::write(
            system.join("desktop.ini"),
            format!("[.ShellClassInfo]\r\nCLSID={clsid}\r\n"),
        )
        .unwrap();
        set_attributes(&system, FILE_ATTRIBUTE_SYSTEM);
        let plain = tmp.0.join("plain.txt");
        std::fs::write(&plain, "x").unwrap();
        let _com = Com::init().unwrap();
        let check = |path: &Path| shell_item(path).and_then(|item| confirm(path, &item));
        assert_eq!(check(&plain), Ok(()));
        for path in [named, system.join("AppData")] {
            assert_eq!(
                check(&path),
                Err(RecycleError::Refused(ELSEWHERE.into())),
                "{}",
                path.display()
            );
            assert!(path.exists());
        }
    }

    /// A folder shortcut (a read-only folder whose `desktop.ini` gives
    /// `CLSID2` and holds `target.lnk`) leads the shell to the target's
    /// items. With a hard link, the shell's item is the same file by ID
    /// under another name, which the move would take instead. Nothing is
    /// moved; only the shell's reading is checked.
    #[test]
    fn another_name_of_the_same_file_is_refused() {
        let tmp = Dir::new("sb-shellnsln");
        let real = tmp.0.join("real");
        let shortcut = tmp.0.join("fs");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::create_dir_all(&shortcut).unwrap();
        std::fs::write(real.join("a.txt"), "x").unwrap();
        std::fs::hard_link(real.join("a.txt"), shortcut.join("a.txt")).unwrap();
        std::fs::write(
            shortcut.join("desktop.ini"),
            "[.ShellClassInfo]\r\nCLSID2={0AFACED1-E828-11D1-9187-B532F1E9575D}\r\nFlags=2\r\n",
        )
        .unwrap();
        let _com = Com::init().unwrap();
        // SAFETY: COM is initialized; the strings outlive the calls.
        unsafe {
            let link: IShellLinkW =
                CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).unwrap();
            link.SetPath(&HSTRING::from(real.as_os_str())).unwrap();
            link.cast::<IPersistFile>()
                .unwrap()
                .Save(
                    &HSTRING::from(shortcut.join("target.lnk").as_os_str()),
                    true,
                )
                .unwrap();
        }
        set_attributes(&shortcut, FILE_ATTRIBUTE_READONLY);
        let check = |path: &Path| shell_item(path).and_then(|item| confirm(path, &item));
        let resolved = shell_item(&shortcut.join("a.txt"))
            .ok()
            .and_then(|item| filesystem_path(&item));
        let results = (check(&real.join("a.txt")), check(&shortcut.join("a.txt")));
        // Cleared so the folder can be removed, whatever the results.
        set_attributes(&shortcut, FILE_ATTRIBUTE_NORMAL);
        // The shell went to the other name.
        assert!(
            resolved.as_deref().is_some_and(|r| {
                crate::identity::same_object(r, &real.join("a.txt")).unwrap_or(false)
            }),
            "{resolved:?}"
        );
        assert_eq!(
            results,
            (Ok(()), Err(RecycleError::Refused(ELSEWHERE.into())))
        );
        assert!(real.join("a.txt").exists() && shortcut.join("a.txt").exists());
    }

    /// The shell names a long path's items in 8.3 form, and the network
    /// form of a local folder its own way; both are still the item.
    #[test]
    fn long_and_network_paths_are_confirmed() {
        let tmp = Dir::new("sb-shelllong");
        let mut long = tmp.0.clone();
        for i in 0..6 {
            long.push(format!("a-rather-long-folder-name-to-pass-max-path-{i}"));
        }
        std::fs::create_dir_all(&long).unwrap();
        let file = long.join("file.txt");
        std::fs::write(&file, "x").unwrap();
        assert!(file.as_os_str().len() > 260);
        let _com = Com::init().unwrap();
        let check = |path: &Path| shell_item(path).and_then(|item| confirm(path, &item));
        let verbatim = PathBuf::from(format!(r"\\?\{}", file.display()));
        assert_eq!(check(&verbatim), Ok(()));
        let short = tmp.0.join("short.txt");
        std::fs::write(&short, "x").unwrap();
        let local = short.to_string_lossy().to_string();
        let Some(rest) = local.strip_prefix("C:\\") else {
            return;
        };
        let unc = PathBuf::from(format!(r"\\?\UNC\localhost\C$\{rest}"));
        if std::fs::metadata(&unc).is_err() {
            eprintln!("skipped: {} isn't reachable", unc.display());
            return;
        }
        assert_eq!(check(&unc), Ok(()));
        assert_eq!(check(&shell_path(&unc)), Ok(()));
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

    /// A folder in a temporary folder, removed when dropped.
    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn set_attributes(folder: &Path, attributes: FILE_FLAGS_AND_ATTRIBUTES) {
        let wide: Vec<u16> = folder.as_os_str().encode_wide().chain([0]).collect();
        // SAFETY: a NUL-terminated path.
        unsafe { SetFileAttributesW(PCWSTR(wide.as_ptr()), attributes) }.unwrap();
    }

    /// A shell folder inside a folder being recycled is never followed:
    /// either the folder goes whole, the shell folder's own files with it,
    /// or (as Windows 11 does) the shell refuses and nothing moves. May put
    /// a small folder in the Recycle Bin, with a Control Panel junction
    /// inside (no files behind it). Run by hand:
    /// `cargo test -p spacebadger a_shell_folder_inside_is_never_followed -- --ignored`.
    #[test]
    #[ignore]
    fn a_shell_folder_inside_is_never_followed() {
        let outer = Dir::new("sb-recycle-junction");
        let junction = outer.0.join("cp");
        std::fs::create_dir_all(&junction).unwrap();
        std::fs::write(
            junction.join("desktop.ini"),
            "[.ShellClassInfo]\r\nCLSID={21EC2020-3AEA-1069-A2DD-08002B30309D}\r\n",
        )
        .unwrap();
        std::fs::write(junction.join("inner.txt"), "recycle me").unwrap();
        set_attributes(&junction, FILE_ATTRIBUTE_SYSTEM);
        let (_, inner) = crate::identity::hold(&junction.join("inner.txt"), false).unwrap();
        let result = recycle(&outer.0);
        let now = inner.whereabouts();
        if result.is_ok() {
            assert!(!outer.0.exists());
            assert!(matches!(
                now,
                crate::identity::Whereabouts::Moved(to) if crate::identity::in_recycle_bin(&to)
            ));
        } else {
            assert!(matches!(result, Err(RecycleError::Failed(_))), "{result:?}");
            assert_eq!(now, crate::identity::Whereabouts::Unmoved);
            assert!(junction.join("inner.txt").exists() && junction.join("desktop.ini").exists());
        }
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
