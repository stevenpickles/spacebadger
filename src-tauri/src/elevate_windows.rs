//! Starting SpaceBadger again with administrator rights, through the
//! Windows elevation (UAC) prompt.

use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use windows::Win32::Foundation::ERROR_CANCELLED;
use windows::Win32::UI::Shell::{SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, ShellExecuteExW};
use windows::core::{HRESULT, HSTRING, PCWSTR, w};

/// `SW_SHOWNORMAL`, without the feature that defines it.
const SHOW_NORMAL: i32 = 1;

/// Starts this program elevated, scanning `folder` if given. Returns
/// `Ok(false)` when the elevation prompt was declined.
pub fn relaunch_elevated(folder: Option<&Path>) -> Result<bool, String> {
    let exe =
        std::env::current_exe().map_err(|e| format!("Couldn't find SpaceBadger itself: {e}"))?;
    let file = HSTRING::from(exe.as_os_str());
    let params = folder.map(|f| HSTRING::from(quote(f.as_os_str()).as_os_str()));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        // Wait for the launch to finish: this process exits right after.
        fMask: SEE_MASK_NOASYNC,
        lpVerb: w!("runas"),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: params
            .as_ref()
            .map_or(PCWSTR::null(), |p| PCWSTR(p.as_ptr())),
        nShow: SHOW_NORMAL,
        ..Default::default()
    };
    // SAFETY: `info` is fully initialized and its strings outlive the call.
    match unsafe { ShellExecuteExW(&mut info) } {
        Ok(()) => Ok(true),
        Err(e) if e.code() == HRESULT::from_win32(ERROR_CANCELLED.0) => Ok(false),
        Err(e) => Err(format!(
            "Couldn't restart as administrator: {}",
            e.message()
        )),
    }
}

/// Quotes one command-line argument the way programs parse it back: inside
/// quotes, backslashes are literal except before a quote, so the trailing
/// ones (as in `C:\`) are doubled. Paths can't contain quotes.
fn quote(arg: &OsStr) -> OsString {
    let backslash = u16::from(b'\\');
    let trailing = arg
        .encode_wide()
        .collect::<Vec<_>>()
        .iter()
        .rev()
        .take_while(|&&c| c == backslash)
        .count();
    let mut quoted = OsString::from("\"");
    quoted.push(arg);
    quoted.push("\\".repeat(trailing));
    quoted.push("\"");
    quoted
}

#[cfg(test)]
mod tests {
    use super::quote;
    use std::ffi::OsStr;

    fn quoted(path: &str) -> String {
        quote(OsStr::new(path)).into_string().unwrap()
    }

    #[test]
    fn quotes_paths_so_they_parse_back_whole() {
        assert_eq!(quoted(r"C:\Users\Me\My Files"), r#""C:\Users\Me\My Files""#);
        assert_eq!(
            quoted(r"C:\"),
            r#""C:\\""#,
            "a drive root keeps its backslash"
        );
        assert_eq!(quoted(r"\\server\share\"), r#""\\server\share\\""#);
    }
}
