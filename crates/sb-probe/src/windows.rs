//! Windows probes: directory-enumeration metadata (no file open),
//! `GetCompressedFileSizeW`, attribute-only handles, and Cloud Files
//! placeholder information.

use crate::report::{Outcome, Samples, Tally};
use crate::{FileOpts, WalkOpts};
use serde::Serialize;
use std::ffi::{OsString, c_void};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{ERROR_NO_MORE_FILES, HANDLE};
use windows::Win32::Storage::CloudFilters::{
    CF_PLACEHOLDER_INFO_STANDARD, CF_PLACEHOLDER_STANDARD_INFO, CfGetPlaceholderInfo,
    CfGetPlaceholderStateFromAttributeTag,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_NO_RECALL,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAGS_AND_ATTRIBUTES, FILE_ID_EXTD_DIR_INFO, FILE_ID_INFO,
    FILE_INFO_BY_HANDLE_CLASS, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO, FileAttributeTagInfo,
    FileIdExtdDirectoryInfo, FileIdInfo, FileStandardInfo, GetCompressedFileSizeW,
    GetDiskFreeSpaceW, GetFileAttributesW, GetFileInformationByHandleEx,
    GetVolumeInformationByHandleW, GetVolumePathNameW, INVALID_FILE_ATTRIBUTES, INVALID_FILE_SIZE,
    OPEN_EXISTING,
};
use windows::core::PCWSTR;

const ATTRIBUTE_DIRECTORY: u32 = 0x10;
const ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

const ATTRIBUTE_NAMES: &[(u32, &str)] = &[
    (0x1, "READONLY"),
    (0x2, "HIDDEN"),
    (0x4, "SYSTEM"),
    (0x10, "DIRECTORY"),
    (0x20, "ARCHIVE"),
    (0x40, "DEVICE"),
    (0x80, "NORMAL"),
    (0x100, "TEMPORARY"),
    (0x200, "SPARSE_FILE"),
    (0x400, "REPARSE_POINT"),
    (0x800, "COMPRESSED"),
    (0x1000, "OFFLINE"),
    (0x2000, "NOT_CONTENT_INDEXED"),
    (0x4000, "ENCRYPTED"),
    (0x8000, "INTEGRITY_STREAM"),
    (0x10000, "VIRTUAL"),
    (0x20000, "NO_SCRUB_DATA"),
    (0x40000, "RECALL_ON_OPEN"),
    (0x80000, "PINNED"),
    (0x100000, "UNPINNED"),
    (0x400000, "RECALL_ON_DATA_ACCESS"),
    (0x20000000, "STRICTLY_SEQUENTIAL"),
];

/// Attributes worth tallying during a walk.
const NOTABLE_ATTRIBUTES: u32 =
    0x2 | 0x4 | 0x200 | 0x400 | 0x800 | 0x1000 | 0x4000 | 0x40000 | 0x80000 | 0x100000 | 0x400000;

const VOLUME_FLAG_NAMES: &[(u32, &str)] = &[
    (0x10, "FILE_COMPRESSION"),
    (0x40, "SUPPORTS_SPARSE_FILES"),
    (0x80, "SUPPORTS_REPARSE_POINTS"),
    (0x400000, "SUPPORTS_HARD_LINKS"),
    (0x1000000, "SUPPORTS_OPEN_BY_FILE_ID"),
    (0x8000000, "SUPPORTS_BLOCK_REFCOUNTING"),
    (0x10000000, "SUPPORTS_SPARSE_VDL"),
];

const PLACEHOLDER_STATE_NAMES: &[(u32, &str)] = &[
    (0x1, "PLACEHOLDER"),
    (0x2, "SYNC_ROOT"),
    (0x4, "ESSENTIAL_PROP_PRESENT"),
    (0x8, "IN_SYNC"),
    (0x10, "PARTIAL"),
    (0x20, "PARTIALLY_ON_DISK"),
];

fn reparse_tag_name(tag: u32) -> &'static str {
    if is_cloud_tag(tag) {
        return "CLOUD";
    }
    match tag {
        0xA000000C => "SYMLINK",
        0xA0000003 => "MOUNT_POINT",
        0x8000001B => "APPEXECLINK",
        0x80000013 => "DEDUP",
        0x80000017 => "WOF",
        0x80000018 | 0x90001018 => "WCI",
        0xA0000027 => "WCI_LINK",
        0x9000001C => "PROJFS",
        0x80000021 => "ONEDRIVE",
        0xA000001D => "LX_SYMLINK",
        0x80000023 => "AF_UNIX",
        0x80000024 => "LX_FIFO",
        0x80000025 => "LX_CHR",
        0x80000026 => "LX_BLK",
        0x80000014 => "NFS",
        0xC0000004 | 0x80000006 => "HSM",
        0x80000007 => "SIS",
        0x8000000A => "DFS",
        0x80000012 => "DFSR",
        0xA0000019 => "GLOBAL_REPARSE",
        0x8000001E => "STORAGE_SYNC",
        0xA0000028 => "DATALESS_CIM",
        _ => "UNKNOWN",
    }
}

/// `IO_REPARSE_TAG_CLOUD` and its sixteen `CLOUD_n` variants.
fn is_cloud_tag(tag: u32) -> bool {
    tag & 0xFFFF_0FFF == 0x9000_001A
}

fn flag_names(value: u32, table: &[(u32, &'static str)]) -> Vec<&'static str> {
    table
        .iter()
        .filter(|(bit, _)| value & bit != 0)
        .map(|(_, name)| *name)
        .collect()
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct Attributes {
    raw: String,
    names: Vec<&'static str>,
}

impl Attributes {
    fn new(raw: u32) -> Self {
        Self {
            raw: format!("{raw:#x}"),
            names: flag_names(raw, ATTRIBUTE_NAMES),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct ReparseTag {
    raw: String,
    name: &'static str,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct PlaceholderState {
    raw: String,
    names: Vec<&'static str>,
}

fn placeholder_state(attributes: u32, tag: u32) -> PlaceholderState {
    let state = unsafe { CfGetPlaceholderStateFromAttributeTag(attributes, tag) }.0;
    PlaceholderState {
        raw: format!("{state:#x}"),
        names: if state == u32::MAX {
            vec!["INVALID"]
        } else {
            flag_names(state, PLACEHOLDER_STATE_NAMES)
        },
    }
}

/// One entry as returned by directory enumeration, without opening the file.
struct RawEntry {
    name: OsString,
    end_of_file: u64,
    allocation_size: u64,
    attributes: u32,
    reparse_tag: u32,
    file_id: [u8; 16],
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct DirEntryReport {
    end_of_file: u64,
    allocation_size: u64,
    attributes: Attributes,
    reparse_tag: Option<ReparseTag>,
    placeholder_state: PlaceholderState,
    file_id: String,
}

impl From<&RawEntry> for DirEntryReport {
    fn from(entry: &RawEntry) -> Self {
        let has_tag = entry.attributes & ATTRIBUTE_REPARSE_POINT != 0;
        Self {
            end_of_file: entry.end_of_file,
            allocation_size: entry.allocation_size,
            attributes: Attributes::new(entry.attributes),
            reparse_tag: has_tag.then(|| ReparseTag {
                raw: format!("{:#010x}", entry.reparse_tag),
                name: reparse_tag_name(entry.reparse_tag),
            }),
            placeholder_state: placeholder_state(entry.attributes, entry.reparse_tag),
            file_id: hex(&entry.file_id),
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().rev().map(|b| format!("{b:02x}")).collect()
}

/// NUL-terminated wide path using the `\\?\` form so long paths work.
fn wide(path: &Path) -> Vec<u16> {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let text: Vec<u16> = absolute.as_os_str().encode_wide().collect();
    let starts = |prefix: &str| {
        let p: Vec<u16> = prefix.encode_utf16().collect();
        text.starts_with(&p)
    };
    let mut out: Vec<u16> = if starts(r"\\?\") || starts(r"\\.\") {
        text.clone()
    } else if starts(r"\\") {
        r"\\?\UNC\"
            .encode_utf16()
            .chain(text[2..].iter().copied())
            .collect()
    } else {
        r"\\?\".encode_utf16().chain(text.iter().copied()).collect()
    };
    out.push(0);
    out
}

fn open(
    path: &Path,
    access: u32,
    flags: FILE_FLAGS_AND_ATTRIBUTES,
) -> windows::core::Result<OwnedHandle> {
    let path = wide(path);
    let handle = unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            flags,
            None,
        )
    }?;
    // SAFETY: CreateFileW returned a valid handle that we now own.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle.0) })
}

fn raw(handle: &OwnedHandle) -> HANDLE {
    HANDLE(handle.as_raw_handle())
}

fn info<T: Default>(
    handle: &OwnedHandle,
    class: FILE_INFO_BY_HANDLE_CLASS,
) -> windows::core::Result<T> {
    let mut value = T::default();
    unsafe {
        GetFileInformationByHandleEx(
            raw(handle),
            class,
            (&raw mut value).cast::<c_void>(),
            size_of::<T>() as u32,
        )
    }?;
    Ok(value)
}

/// Enumerates a directory with `FileIdExtdDirectoryInfo`. Does not open the
/// entries themselves and does not follow a reparse point at `dir`.
fn read_dir(dir: &Path, mut each: impl FnMut(RawEntry)) -> windows::core::Result<()> {
    let handle = open(
        dir,
        FILE_LIST_DIRECTORY.0,
        FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
    )?;
    let mut buffer = vec![0u64; 8192];
    let name_offset = std::mem::offset_of!(FILE_ID_EXTD_DIR_INFO, FileName);
    loop {
        let result = unsafe {
            GetFileInformationByHandleEx(
                raw(&handle),
                FileIdExtdDirectoryInfo,
                buffer.as_mut_ptr().cast(),
                (buffer.len() * size_of::<u64>()) as u32,
            )
        };
        match result {
            Ok(()) => {}
            Err(err) if err.code() == ERROR_NO_MORE_FILES.to_hresult() => return Ok(()),
            Err(err) => return Err(err),
        }
        let base = buffer.as_ptr().cast::<u8>();
        let mut offset = 0usize;
        loop {
            // SAFETY: the API fills the buffer with 8-byte-aligned entries
            // chained by NextEntryOffset; names follow each fixed header.
            let entry = unsafe { base.add(offset).cast::<FILE_ID_EXTD_DIR_INFO>().read() };
            let name = unsafe {
                std::slice::from_raw_parts(
                    base.add(offset + name_offset).cast::<u16>(),
                    entry.FileNameLength as usize / 2,
                )
            };
            if name != [0x2e] && name != [0x2e, 0x2e] {
                each(RawEntry {
                    name: OsString::from_wide(name),
                    end_of_file: entry.EndOfFile as u64,
                    allocation_size: entry.AllocationSize as u64,
                    attributes: entry.FileAttributes,
                    reparse_tag: entry.ReparsePointTag,
                    file_id: entry.FileId.Identifier,
                });
            }
            if entry.NextEntryOffset == 0 {
                break;
            }
            offset += entry.NextEntryOffset as usize;
        }
    }
}

/// Finds `path` in its parent's directory listing.
fn dir_entry(path: &Path) -> Result<DirEntryReport, String> {
    let absolute = std::path::absolute(path).map_err(|e| e.to_string())?;
    let (Some(parent), Some(name)) = (absolute.parent(), absolute.file_name()) else {
        return Err("path has no parent directory".into());
    };
    let wanted = name.to_string_lossy().to_lowercase();
    let mut found = None;
    read_dir(parent, |entry| {
        if found.is_none() && entry.name.to_string_lossy().to_lowercase() == wanted {
            found = Some(DirEntryReport::from(&entry));
        }
    })
    .map_err(|e| e.to_string())?;
    found.ok_or_else(|| "not found in parent directory listing".into())
}

fn path_attributes(path: &Path) -> io::Result<u32> {
    let path = wide(path);
    let attributes = unsafe { GetFileAttributesW(PCWSTR(path.as_ptr())) };
    if attributes == INVALID_FILE_ATTRIBUTES {
        Err(io::Error::last_os_error())
    } else {
        Ok(attributes)
    }
}

fn compressed_size(path: &Path) -> io::Result<u64> {
    let path = wide(path);
    let mut high = 0u32;
    let low = unsafe { GetCompressedFileSizeW(PCWSTR(path.as_ptr()), Some(&mut high)) };
    if low == INVALID_FILE_SIZE {
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(0) {
            return Err(err);
        }
    }
    Ok(u64::from(high) << 32 | u64::from(low))
}

#[derive(Debug, Serialize)]
struct Snapshot {
    step: &'static str,
    dir_entry: Outcome<DirEntryReport>,
    path_attributes: Outcome<Attributes>,
}

fn snapshot(step: &'static str, path: &Path) -> Snapshot {
    Snapshot {
        step,
        dir_entry: dir_entry(path).into(),
        path_attributes: path_attributes(path).map(Attributes::new).into(),
    }
}

#[derive(Debug, Serialize)]
struct Standard {
    allocation_size: u64,
    end_of_file: u64,
    number_of_links: u32,
    directory: bool,
}

#[derive(Debug, Serialize)]
struct Identity {
    volume_serial: String,
    file_id: String,
}

#[derive(Debug, Serialize)]
struct AttributeTag {
    attributes: Attributes,
    reparse_tag: Option<ReparseTag>,
    placeholder_state: PlaceholderState,
}

#[derive(Debug, Serialize)]
struct CloudPlaceholder {
    on_disk_data_size: i64,
    validated_data_size: i64,
    modified_data_size: i64,
    properties_size: i64,
    pin_state: &'static str,
    in_sync_state: &'static str,
}

#[derive(Debug, Serialize)]
struct Volume {
    filesystem: String,
    serial: String,
    flags: Vec<&'static str>,
    cluster_size: Outcome<u32>,
}

#[derive(Debug, Serialize)]
struct HandleReport {
    flags: Vec<&'static str>,
    standard: Outcome<Standard>,
    identity: Outcome<Identity>,
    attribute_tag: Outcome<AttributeTag>,
    cloud_placeholder: Outcome<CloudPlaceholder>,
    volume: Outcome<Volume>,
}

fn handle_report(path: &Path, plain: bool) -> Outcome<HandleReport> {
    let mut flags = FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT;
    let mut flag_list = vec!["BACKUP_SEMANTICS", "OPEN_REPARSE_POINT"];
    if !plain {
        flags |= FILE_FLAG_OPEN_NO_RECALL;
        flag_list.push("OPEN_NO_RECALL");
    }
    let handle = match open(path, FILE_READ_ATTRIBUTES.0, flags) {
        Ok(handle) => handle,
        Err(err) => return Outcome::Error(err.to_string()),
    };
    let standard = info::<FILE_STANDARD_INFO>(&handle, FileStandardInfo).map(|s| Standard {
        allocation_size: s.AllocationSize as u64,
        end_of_file: s.EndOfFile as u64,
        number_of_links: s.NumberOfLinks,
        directory: s.Directory,
    });
    let identity = info::<FILE_ID_INFO>(&handle, FileIdInfo).map(|id| Identity {
        volume_serial: format!("{:#x}", id.VolumeSerialNumber),
        file_id: hex(&id.FileId.Identifier),
    });
    let attribute_tag =
        info::<FILE_ATTRIBUTE_TAG_INFO>(&handle, FileAttributeTagInfo).map(|t| AttributeTag {
            attributes: Attributes::new(t.FileAttributes),
            reparse_tag: (t.FileAttributes & ATTRIBUTE_REPARSE_POINT != 0).then(|| ReparseTag {
                raw: format!("{:#010x}", t.ReparseTag),
                name: reparse_tag_name(t.ReparseTag),
            }),
            placeholder_state: placeholder_state(t.FileAttributes, t.ReparseTag),
        });
    Outcome::Ok(HandleReport {
        flags: flag_list,
        standard: standard.into(),
        identity: identity.into(),
        attribute_tag: attribute_tag.into(),
        cloud_placeholder: cloud_placeholder(&handle).into(),
        volume: volume(path, &handle).into(),
    })
}

fn cloud_placeholder(handle: &OwnedHandle) -> windows::core::Result<CloudPlaceholder> {
    // Room for the fixed header plus the 4 KiB maximum file identity.
    let mut buffer = vec![0u64; 1024];
    unsafe {
        CfGetPlaceholderInfo(
            raw(handle),
            CF_PLACEHOLDER_INFO_STANDARD,
            buffer.as_mut_ptr().cast(),
            (buffer.len() * size_of::<u64>()) as u32,
            None,
        )
    }?;
    let info = unsafe {
        buffer
            .as_ptr()
            .cast::<CF_PLACEHOLDER_STANDARD_INFO>()
            .read()
    };
    Ok(CloudPlaceholder {
        on_disk_data_size: info.OnDiskDataSize,
        validated_data_size: info.ValidatedDataSize,
        modified_data_size: info.ModifiedDataSize,
        properties_size: info.PropertiesSize,
        pin_state: match info.PinState.0 {
            0 => "UNSPECIFIED",
            1 => "PINNED",
            2 => "UNPINNED",
            3 => "EXCLUDED",
            4 => "INHERIT",
            _ => "UNKNOWN",
        },
        in_sync_state: if info.InSyncState.0 == 1 {
            "IN_SYNC"
        } else {
            "NOT_IN_SYNC"
        },
    })
}

fn volume(path: &Path, handle: &OwnedHandle) -> windows::core::Result<Volume> {
    let mut serial = 0u32;
    let mut flags = 0u32;
    let mut filesystem = [0u16; 64];
    unsafe {
        GetVolumeInformationByHandleW(
            raw(handle),
            None,
            Some(&mut serial),
            None,
            Some(&mut flags),
            Some(&mut filesystem),
        )
    }?;
    let end = filesystem
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(filesystem.len());
    Ok(Volume {
        filesystem: String::from_utf16_lossy(&filesystem[..end]),
        serial: format!("{serial:#x}"),
        flags: flag_names(flags, VOLUME_FLAG_NAMES),
        cluster_size: cluster_size(path).into(),
    })
}

fn cluster_size(path: &Path) -> windows::core::Result<u32> {
    let path = wide(path);
    let mut root = [0u16; 1024];
    unsafe { GetVolumePathNameW(PCWSTR(path.as_ptr()), &mut root) }?;
    let (mut sectors_per_cluster, mut bytes_per_sector) = (0u32, 0u32);
    unsafe {
        GetDiskFreeSpaceW(
            PCWSTR(root.as_ptr()),
            Some(&mut sectors_per_cluster),
            Some(&mut bytes_per_sector),
            None,
            None,
        )
    }?;
    Ok(sectors_per_cluster * bytes_per_sector)
}

#[derive(Debug, Serialize)]
pub struct FileReport {
    path: PathBuf,
    snapshots: Vec<Snapshot>,
    compressed_file_size: Outcome<u64>,
    handle: Outcome<HandleReport>,
    /// Steps after which the directory entry or path attributes differed from
    /// the first snapshot (a sign of hydration or a concurrent change).
    changed_after: Vec<&'static str>,
}

pub fn probe_file(path: &Path, opts: &FileOpts) -> FileReport {
    let mut snapshots = vec![snapshot("initial", path)];
    let compressed_file_size = compressed_size(path).into();
    snapshots.push(snapshot("after_compressed_file_size", path));
    let handle = handle_report(path, opts.open_plain);
    snapshots.push(snapshot("after_handle", path));
    std::thread::sleep(Duration::from_millis(opts.settle_ms));
    snapshots.push(snapshot("after_settle", path));

    let key = |s: &Snapshot| format!("{:?}{:?}", s.dir_entry, s.path_attributes);
    let initial = key(&snapshots[0]);
    let changed_after = snapshots[1..]
        .iter()
        .filter(|s| key(s) != initial)
        .map(|s| s.step)
        .collect();
    FileReport {
        path: path.to_path_buf(),
        snapshots,
        compressed_file_size,
        handle,
        changed_after,
    }
}

#[derive(Debug, Serialize)]
struct SizeSample {
    path: String,
    end_of_file: u64,
    dir_allocation: u64,
    other: u64,
    attributes: Vec<&'static str>,
}

#[derive(Debug, Serialize)]
struct Comparison {
    /// Category → count/bytes, e.g. "equals_dir_allocation".
    outcomes: Tally,
    samples: Samples<SizeSample>,
}

impl Comparison {
    fn new(samples: usize) -> Self {
        Self {
            outcomes: Tally::default(),
            samples: Samples::new(samples),
        }
    }
}

#[derive(Debug, Serialize)]
struct ErrorSample {
    path: String,
    error: String,
}

#[derive(Debug, Serialize)]
pub struct WalkReport {
    root: PathBuf,
    elapsed_ms: u128,
    stopped_early: bool,
    directories: u64,
    files: u64,
    files_per_second: f64,
    end_of_file_total: u64,
    dir_allocation_total: u64,
    /// Files by notable attribute; bytes are directory-entry allocation.
    file_attributes: Tally,
    /// Files and directories by reparse tag; bytes are end-of-file.
    reparse_tags: Tally,
    /// Files by Cloud Files placeholder state; bytes are end-of-file.
    placeholder_states: Tally,
    /// Directories not descended into, by reparse tag.
    skipped_directories: Tally,
    errors: Samples<ErrorSample>,
    compressed_vs_dir_entry: Option<Comparison>,
    handle_vs_dir_entry: Option<Comparison>,
    multiply_linked_files: u64,
    /// Files whose attributes changed between enumeration and a later open.
    state_changes: Samples<SizeSample>,
}

pub fn walk(root: &Path, opts: &WalkOpts) -> WalkReport {
    let started = Instant::now();
    let mut report = WalkReport {
        root: root.to_path_buf(),
        elapsed_ms: 0,
        stopped_early: false,
        directories: 1,
        files: 0,
        files_per_second: 0.0,
        end_of_file_total: 0,
        dir_allocation_total: 0,
        file_attributes: Tally::default(),
        reparse_tags: Tally::default(),
        placeholder_states: Tally::default(),
        skipped_directories: Tally::default(),
        errors: Samples::new(opts.samples),
        compressed_vs_dir_entry: opts.compressed.then(|| Comparison::new(opts.samples)),
        handle_vs_dir_entry: opts.open.then(|| Comparison::new(opts.samples)),
        multiply_linked_files: 0,
        state_changes: Samples::new(opts.samples),
    };
    let mut pending = vec![root.to_path_buf()];
    'walk: while let Some(dir) = pending.pop() {
        let mut entries = Vec::new();
        if let Err(err) = read_dir(&dir, |entry| entries.push(entry)) {
            report.errors.add("read_dir", || ErrorSample {
                path: dir.display().to_string(),
                error: err.to_string(),
            });
            continue;
        }
        for entry in entries {
            let path = dir.join(&entry.name);
            let is_reparse = entry.attributes & ATTRIBUTE_REPARSE_POINT != 0;
            if is_reparse {
                report
                    .reparse_tags
                    .add(reparse_tag_name(entry.reparse_tag), entry.end_of_file);
            }
            if entry.attributes & ATTRIBUTE_DIRECTORY != 0 {
                if is_reparse && !is_cloud_tag(entry.reparse_tag) {
                    report
                        .skipped_directories
                        .add(reparse_tag_name(entry.reparse_tag), 0);
                } else {
                    report.directories += 1;
                    pending.push(path);
                }
                continue;
            }
            if opts.max_files.is_some_and(|max| report.files >= max) {
                report.stopped_early = true;
                break 'walk;
            }
            report.files += 1;
            report.end_of_file_total += entry.end_of_file;
            report.dir_allocation_total += entry.allocation_size;
            for name in flag_names(entry.attributes & NOTABLE_ATTRIBUTES, ATTRIBUTE_NAMES) {
                report.file_attributes.add(name, entry.allocation_size);
            }
            let state = placeholder_state(entry.attributes, entry.reparse_tag);
            report.placeholder_states.add(
                if state.names.is_empty() {
                    "NONE".to_owned()
                } else {
                    state.names.join("|")
                },
                entry.end_of_file,
            );
            let sample = |other: u64| SizeSample {
                path: path.display().to_string(),
                end_of_file: entry.end_of_file,
                dir_allocation: entry.allocation_size,
                other,
                attributes: flag_names(entry.attributes, ATTRIBUTE_NAMES),
            };
            if let Some(cmp) = report.compressed_vs_dir_entry.as_mut() {
                match compressed_size(&path) {
                    Ok(size) => {
                        let outcome = classify(size, &entry);
                        cmp.outcomes.add(outcome, size);
                        if outcome == "differs" {
                            cmp.samples.add(outcome, || sample(size));
                        }
                    }
                    Err(err) => report.errors.add("compressed_file_size", || ErrorSample {
                        path: path.display().to_string(),
                        error: err.to_string(),
                    }),
                }
            }
            if let Some(cmp) = report.handle_vs_dir_entry.as_mut() {
                let flags = FILE_FLAG_BACKUP_SEMANTICS
                    | FILE_FLAG_OPEN_REPARSE_POINT
                    | FILE_FLAG_OPEN_NO_RECALL;
                let standard = open(&path, FILE_READ_ATTRIBUTES.0, flags)
                    .and_then(|h| info::<FILE_STANDARD_INFO>(&h, FileStandardInfo));
                match standard {
                    Ok(s) => {
                        let allocation = s.AllocationSize as u64;
                        let outcome = if allocation == entry.allocation_size {
                            "allocation_matches"
                        } else {
                            "allocation_differs"
                        };
                        cmp.outcomes.add(outcome, allocation);
                        if outcome == "allocation_differs" {
                            cmp.samples.add(outcome, || sample(allocation));
                        }
                        if s.EndOfFile as u64 != entry.end_of_file {
                            cmp.outcomes.add("end_of_file_differs", s.EndOfFile as u64);
                            cmp.samples
                                .add("end_of_file_differs", || sample(s.EndOfFile as u64));
                        }
                        if s.NumberOfLinks > 1 {
                            report.multiply_linked_files += 1;
                        }
                    }
                    Err(err) => report.errors.add("open_for_attributes", || ErrorSample {
                        path: path.display().to_string(),
                        error: err.to_string(),
                    }),
                }
                if let Ok(after) = path_attributes(&path)
                    && after != entry.attributes
                {
                    report
                        .state_changes
                        .add("attributes_changed", || sample(u64::from(after)));
                }
            }
        }
    }
    report.elapsed_ms = started.elapsed().as_millis();
    report.files_per_second = report.files as f64 / started.elapsed().as_secs_f64().max(1e-9);
    report
}

fn classify(size: u64, entry: &RawEntry) -> &'static str {
    if size == entry.allocation_size {
        "equals_dir_allocation"
    } else if size == entry.end_of_file {
        "equals_end_of_file"
    } else {
        "differs"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_tags_cover_all_variants() {
        assert!(is_cloud_tag(0x9000_001A));
        assert!(is_cloud_tag(0x9000_F01A));
        assert!(!is_cloud_tag(0xA000_000C));
        assert_eq!(reparse_tag_name(0x9000_701A), "CLOUD");
        assert_eq!(reparse_tag_name(0xA000_0003), "MOUNT_POINT");
    }

    #[test]
    fn wide_paths_use_extended_prefix() {
        let text = |p: &str| {
            let w = wide(Path::new(p));
            String::from_utf16(&w[..w.len() - 1]).unwrap()
        };
        assert_eq!(text(r"C:\a\b"), r"\\?\C:\a\b");
        assert_eq!(text(r"\\server\share\x"), r"\\?\UNC\server\share\x");
        assert_eq!(text(r"\\?\C:\a"), r"\\?\C:\a");
    }

    #[test]
    fn finds_hard_link_aliases_by_directory_file_id() {
        let dir = std::env::temp_dir().join(format!("sb-probe-test-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("a"), b"data").unwrap();
        std::fs::hard_link(dir.join("a"), dir.join("b")).unwrap();
        let mut ids = Vec::new();
        read_dir(&dir, |entry| ids.push((entry.name, entry.file_id))).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0].1, ids[1].1);
    }
}
