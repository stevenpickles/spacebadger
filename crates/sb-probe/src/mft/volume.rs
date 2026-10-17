//! Windows I/O for `sb-probe mft`. Strictly read-only: the volume is
//! opened with `GENERIC_READ` sharing reading and writing, and the only
//! control codes sent are queries (`FSCTL_GET_NTFS_VOLUME_DATA`,
//! `FSCTL_QUERY_USN_JOURNAL`, `FSCTL_READ_USN_JOURNAL`,
//! `FSCTL_GET_NTFS_FILE_RECORD`). Nothing is written, flushed or locked,
//! and no file is opened for its data.

use super::compare::{self, Comparison, Counted, ListedNode, Listing, Probe};
use super::{Failure, MftOpts};
use sb_scan::mft::record::{
    self, ATTR_DATA, Extent, ListLocation, Parsed, RECORD_MASK, RecordError,
};
use sb_scan::mft::table::{ROOT, Table};
use sb_scan::mft::usn::{UsnRecord, parse_journal};
use sb_scan::{EntryKind, FsAdapter, Measured, NativeFs, RootKind};
use serde::Serialize;
use std::alloc::{Layout, alloc_zeroed, dealloc, handle_alloc_error};
use std::collections::{BTreeMap, HashSet};
use std::ffi::c_void;
use std::ops::ControlFlow;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::FileExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::sync::{Condvar, Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use windows::Win32::Foundation::{GENERIC_READ, HANDLE};
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_NO_BUFFERING,
    FILE_FLAG_OPEN_NO_RECALL, FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_SEQUENTIAL_SCAN,
    FILE_ID_INFO, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FileAttributeTagInfo, FileIdInfo, GetDriveTypeW, GetFileInformationByHandleEx,
    GetVolumeInformationW, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::{
    FSCTL_GET_NTFS_FILE_RECORD, FSCTL_GET_NTFS_VOLUME_DATA, FSCTL_QUERY_USN_JOURNAL,
    FSCTL_READ_USN_JOURNAL, NTFS_VOLUME_DATA_BUFFER,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::PCWSTR;

const MIB: u64 = 1 << 20;
/// Largest non-resident attribute list read for `$MFT`; real ones are a
/// few KiB.
const MAX_ATTRIBUTE_LIST: u64 = 256 * 1024;
/// Seconds from 1601-01-01 (`FILETIME`) to the Unix epoch.
const FILETIME_EPOCH_OFFSET: u64 = 11_644_473_600;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// A `\\?\` path so long paths open.
fn long_path(path: &Path) -> Vec<u16> {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let text: Vec<u16> = absolute.as_os_str().encode_wide().collect();
    let prefix: Vec<u16> = r"\\".encode_utf16().collect();
    let mut out: Vec<u16> = if text.starts_with(&prefix) {
        text
    } else {
        r"\\?\".encode_utf16().chain(text).collect()
    };
    out.push(0);
    out
}

fn raw(handle: &impl AsRawHandle) -> HANDLE {
    HANDLE(handle.as_raw_handle())
}

/// `GetDriveTypeW` for the drive's root.
fn drive_type(letter: char) -> u32 {
    let root = wide(&format!("{letter}:\\"));
    unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) }
}

fn filesystem(letter: char) -> Option<String> {
    let root = wide(&format!("{letter}:\\"));
    let mut name = [0u16; 64];
    unsafe {
        GetVolumeInformationW(
            PCWSTR(root.as_ptr()),
            None,
            None,
            None,
            None,
            Some(&mut name),
        )
    }
    .ok()?;
    let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    Some(String::from_utf16_lossy(&name[..len]))
}

/// Whether this process runs elevated (it never asks to be).
fn elevated() -> bool {
    let mut token = HANDLE::default();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.is_err() {
        return false;
    }
    // SAFETY: OpenProcessToken returned a valid handle that we now own.
    let token = unsafe { OwnedHandle::from_raw_handle(token.0) };
    let mut elevation = TOKEN_ELEVATION::default();
    let mut len = 0u32;
    unsafe {
        GetTokenInformation(
            raw(&token),
            TokenElevation,
            Some((&raw mut elevation).cast::<c_void>()),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
    }
    .is_ok()
        && elevation.TokenIsElevated != 0
}

fn ioctl(
    handle: &impl AsRawHandle,
    code: u32,
    input: &[u8],
    out: &mut [u8],
) -> windows::core::Result<usize> {
    let mut returned = 0u32;
    unsafe {
        DeviceIoControl(
            raw(handle),
            code,
            (!input.is_empty()).then_some(input.as_ptr().cast::<c_void>()),
            input.len() as u32,
            Some(out.as_mut_ptr().cast::<c_void>()),
            out.len() as u32,
            Some(&mut returned),
            None,
        )
    }?;
    Ok(returned as usize)
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    b.get(at..at + 8)
        .map_or(0, |s| u64::from_le_bytes(s.try_into().unwrap()))
}

/// A zeroed buffer aligned for unbuffered reads.
struct Aligned {
    ptr: std::ptr::NonNull<u8>,
    len: usize,
}

// SAFETY: the buffer is uniquely owned.
unsafe impl Send for Aligned {}

impl Aligned {
    const ALIGN: usize = 4096;

    fn new(len: usize) -> Self {
        let layout = Layout::from_size_align(len.max(1), Self::ALIGN).unwrap();
        // SAFETY: the layout has a non-zero size.
        let ptr = unsafe { alloc_zeroed(layout) };
        let ptr = std::ptr::NonNull::new(ptr).unwrap_or_else(|| handle_alloc_error(layout));
        Self { ptr, len }
    }
}

impl std::ops::Deref for Aligned {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        // SAFETY: `len` initialised bytes owned by this buffer.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl std::ops::DerefMut for Aligned {
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: as for Deref, and uniquely borrowed.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for Aligned {
    fn drop(&mut self) {
        let layout = Layout::from_size_align(self.len.max(1), Self::ALIGN).unwrap();
        // SAFETY: allocated in `new` with this layout.
        unsafe { dealloc(self.ptr.as_ptr(), layout) }
    }
}

/// The open volume and its geometry.
struct Volume {
    file: std::fs::File,
    serial: u64,
    sector: u64,
    cluster: u64,
    record_size: u64,
    mft_valid: u64,
    mft_start_lcn: u64,
    total_clusters: u64,
}

impl Volume {
    fn open(letter: char) -> Result<Volume, Failure> {
        let device = wide(&format!(r"\\.\{letter}:"));
        let handle = unsafe {
            CreateFileW(
                PCWSTR(device.as_ptr()),
                GENERIC_READ.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_NO_BUFFERING | FILE_FLAG_SEQUENTIAL_SCAN,
                None,
            )
        }
        .map_err(|e| {
            let message = format!(
                "opening \\\\.\\{letter}: for reading failed: {}",
                e.message()
            );
            if e.code() == windows::Win32::Foundation::E_ACCESSDENIED {
                Failure::Refused(message)
            } else {
                Failure::Failed(message)
            }
        })?;
        // SAFETY: CreateFileW returned a valid handle that we now own.
        let file = std::fs::File::from(unsafe { OwnedHandle::from_raw_handle(handle.0) });
        let mut data = NTFS_VOLUME_DATA_BUFFER::default();
        // SAFETY: a plain-data struct viewed as its own bytes.
        let bytes = unsafe {
            std::slice::from_raw_parts_mut(
                (&raw mut data).cast::<u8>(),
                size_of::<NTFS_VOLUME_DATA_BUFFER>(),
            )
        };
        ioctl(&file, FSCTL_GET_NTFS_VOLUME_DATA, &[], bytes)
            .map_err(|e| Failure::Failed(format!("FSCTL_GET_NTFS_VOLUME_DATA: {}", e.message())))?;
        let volume = Volume {
            file,
            serial: data.VolumeSerialNumber as u64,
            sector: u64::from(data.BytesPerSector),
            cluster: u64::from(data.BytesPerCluster),
            record_size: u64::from(data.BytesPerFileRecordSegment),
            mft_valid: data.MftValidDataLength as u64,
            mft_start_lcn: data.MftStartLcn as u64,
            total_clusters: data.TotalClusters as u64,
        };
        let sane = |v: u64| v.is_power_of_two() && (512..=2 * MIB).contains(&v);
        if !sane(volume.sector) || !sane(volume.cluster) || !sane(volume.record_size) {
            return Err(Failure::Failed(format!(
                "unexpected geometry: {} bytes/sector, {} bytes/cluster, {} bytes/record",
                volume.sector, volume.cluster, volume.record_size
            )));
        }
        Ok(volume)
    }

    /// Reads `buf.len()` bytes at `offset` (both sector-aligned, `buf`
    /// from an [`Aligned`]).
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> Result<(), String> {
        let mut done = 0;
        while done < buf.len() {
            let n = self
                .file
                .seek_read(&mut buf[done..], offset + done as u64)
                .map_err(|e| format!("reading the volume at {:#x}: {e}", offset + done as u64))?;
            if n == 0 {
                return Err(format!("short read at {:#x}", offset + done as u64));
            }
            done += n;
        }
        Ok(())
    }

    fn round_up(&self, n: u64) -> u64 {
        n.div_ceil(self.sector) * self.sector
    }

    /// Reads `len` bytes of a stream laid out in `extents` from `offset`.
    fn read_stream(&self, extents: &[Extent], offset: u64, len: u64) -> Result<Vec<u8>, String> {
        let mut out = Vec::with_capacity(len as usize);
        let mut pos = offset;
        while pos < offset + len {
            let vcn = pos / self.cluster;
            let e = extents
                .iter()
                .find(|e| vcn >= e.vcn && vcn < e.vcn + e.clusters)
                .ok_or_else(|| format!("stream offset {pos:#x} isn't mapped"))?;
            e.lcn.ok_or("sparse run in a metadata stream")?;
            let extent_end = (e.vcn + e.clusters) * self.cluster;
            let take = (offset + len).min(extent_end) - pos;
            let at = record::volume_offset(extents, self.cluster, pos)
                .ok_or_else(|| format!("stream offset {pos:#x} maps past the volume"))?;
            let start = at - at % self.sector;
            let mut buf = Aligned::new(self.round_up(at + take - start) as usize);
            self.read_exact_at(&mut buf, start)?;
            let skip = (at - start) as usize;
            out.extend_from_slice(&buf[skip..skip + take as usize]);
            pos += take;
        }
        Ok(out)
    }

    /// Record 0 (`$MFT`), fixed up.
    fn record_zero(&self) -> Result<Vec<u8>, String> {
        let extent = [Extent {
            vcn: 0,
            lcn: Some(self.mft_start_lcn),
            clusters: self.record_size.div_ceil(self.cluster),
        }];
        let mut rec = self.read_stream(&extent, 0, self.record_size)?;
        record::apply_fixups(&mut rec).map_err(|e| format!("record 0: {e}"))?;
        Ok(rec)
    }

    /// `$MFT`'s extents, following its attribute list into the extension
    /// records that hold later pieces of its run list.
    fn mft_extents(&self) -> Result<Vec<Extent>, String> {
        let rec0 = self.record_zero()?;
        let bad = |e: RecordError| format!("record 0: {e}");
        let sequence = record::parse_record(&rec0, 0)
            .map_err(bad)?
            .ok_or("record 0 isn't in use")?
            .sequence;
        let mut pieces = record::data_runs(&rec0).map_err(bad)?;
        let entries = match record::attribute_list_location(&rec0).map_err(bad)? {
            None => Vec::new(),
            Some(ListLocation::Resident(entries)) => entries,
            Some(ListLocation::NonResident { runs, length }) => {
                if length > MAX_ATTRIBUTE_LIST {
                    return Err(format!("record 0: a {length}-byte attribute list"));
                }
                let extents = record::extents(vec![(0, runs)]).map_err(bad)?;
                record::check_bounds(&extents, self.total_clusters).map_err(bad)?;
                let bytes = self.read_stream(&extents, 0, length)?;
                record::parse_attribute_list(&bytes).map_err(bad)?
            }
        };
        let mut segments: Vec<u64> = Vec::new();
        for e in &entries {
            let segment = e.segment & RECORD_MASK;
            if e.type_code == ATTR_DATA
                && e.name.is_empty()
                && segment != 0
                && !segments.contains(&segment)
            {
                segments.push(segment);
            }
        }
        for segment in segments {
            let known = record::extents(pieces.clone()).map_err(bad)?;
            record::check_bounds(&known, self.total_clusters).map_err(bad)?;
            let at = segment
                .checked_mul(self.record_size)
                .ok_or("$MFT segment number overflows")?;
            let mut rec = self.read_stream(&known, at, self.record_size)?;
            let seg_err = |e: String| format!("$MFT segment {segment}: {e}");
            record::apply_fixups(&mut rec).map_err(|e| seg_err(e.to_string()))?;
            // Must be an in-use extension of record 0 at its own number.
            let parsed = record::parse_record(&rec, segment)
                .map_err(|e| seg_err(e.to_string()))?
                .ok_or_else(|| seg_err("not in use".into()))?;
            if parsed.base != u64::from(sequence) << 48 {
                return Err(seg_err(format!(
                    "base reference {:#x} isn't record 0",
                    parsed.base
                )));
            }
            pieces.extend(record::data_runs(&rec).map_err(bad)?);
        }
        let extents = record::extents(pieces).map_err(bad)?;
        record::check_bounds(&extents, self.total_clusters).map_err(bad)?;
        Ok(extents)
    }

    /// Re-reads one record through NTFS (from its cache), for records torn
    /// by a concurrent write during the raw read. `Ok(None)` when the
    /// record is no longer in use.
    fn reread(&self, number: u64) -> Result<Option<Parsed>, String> {
        let mut out = vec![0u8; 12 + self.record_size as usize];
        ioctl(
            &self.file,
            FSCTL_GET_NTFS_FILE_RECORD,
            &number.to_le_bytes(),
            &mut out,
        )
        .map_err(|e| e.message())?;
        // NTFS returns the nearest in-use record at or below the one asked.
        if u64_at(&out, 0) & RECORD_MASK != number {
            return Ok(None);
        }
        let len = u32::from_le_bytes(out[8..12].try_into().unwrap()) as usize;
        let mut rec = out.get(12..12 + len).ok_or("short record")?.to_vec();
        record::apply_fixups_if_protected(&mut rec).map_err(|e| e.to_string())?;
        record::parse_record(&rec, number).map_err(|e| e.to_string())
    }

    fn query_journal(&self) -> Result<Journal, String> {
        let mut out = [0u8; 80];
        ioctl(&self.file, FSCTL_QUERY_USN_JOURNAL, &[], &mut out).map_err(|e| e.message())?;
        Ok(Journal {
            id: u64_at(&out, 0),
            first: u64_at(&out, 8),
            next: u64_at(&out, 16),
        })
    }

    /// Hands each journal record from `from` up to `until` to `each` as
    /// it's read, so the journal is never held whole.
    fn read_journal(
        &self,
        mut from: u64,
        id: u64,
        until: u64,
        mut each: impl FnMut(UsnRecord),
    ) -> Result<(), String> {
        let mut out = vec![0u8; MIB as usize];
        loop {
            let mut input = [0u8; 40];
            input[0..8].copy_from_slice(&from.to_le_bytes());
            input[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
            input[32..40].copy_from_slice(&id.to_le_bytes());
            let n = ioctl(&self.file, FSCTL_READ_USN_JOURNAL, &input, &mut out)
                .map_err(|e| e.message())?;
            let (next, batch) = parse_journal(&out[..n]);
            batch.into_iter().for_each(&mut each);
            if n <= 8 || next >= until || next <= from {
                return Ok(());
            }
            from = next;
        }
    }
}

#[derive(Clone, Copy)]
struct Journal {
    id: u64,
    first: u64,
    next: u64,
}

/// One read of the table: consecutive records from `first`.
struct Chunk {
    first: u64,
    buf: Aligned,
    len: usize,
}

#[derive(Default)]
struct ParseOut {
    parsed: Vec<Vec<Parsed>>,
    failed: Vec<(u64, RecordError)>,
    cpu: Duration,
}

/// Fixes up and parses every record of a chunk.
fn parse_chunk(chunk: &mut Chunk, record_size: usize, out: &mut ParseOut) {
    let started = Instant::now();
    let mut parsed = Vec::new();
    let len = chunk.len;
    for (i, rec) in chunk.buf[..len].chunks_exact_mut(record_size).enumerate() {
        let number = chunk.first + i as u64;
        // Never-used and free records need no fixups.
        if rec[..4] == [0, 0, 0, 0] || (&rec[..4] == b"FILE" && rec[22] & 1 == 0) {
            continue;
        }
        let result = record::apply_fixups(rec).and_then(|()| record::parse_record(rec, number));
        match result {
            Ok(Some(p)) => parsed.push(p),
            Ok(None) => {}
            Err(e) => out.failed.push((number, e)),
        }
    }
    out.parsed.push(parsed);
    out.cpu += started.elapsed();
}

/// What reading the table found.
struct Read {
    parsed: Vec<Vec<Parsed>>,
    failed: Vec<(u64, RecordError)>,
    read: Duration,
    parse_cpu: Duration,
    wall: Duration,
    records: u64,
}

/// Reads the table's extents in order in `chunk` bytes, handing each
/// chunk to `threads` parsers.
fn read_table(
    volume: &Volume,
    extents: &[Extent],
    chunk: u64,
    threads: usize,
) -> Result<Read, String> {
    let started = Instant::now();
    let rs = volume.record_size;
    let total = volume.mft_valid / rs * rs;
    let chunk = chunk / rs * rs;
    let capacity = volume.round_up(chunk) as usize;
    let (tx, rx) = mpsc::sync_channel::<Chunk>(threads * 2);
    let (pool_tx, pool_rx) = mpsc::channel::<Aligned>();
    let rx = Mutex::new(rx);
    std::thread::scope(|scope| {
        let parsers: Vec<_> = (0..threads)
            .map(|_| {
                let (rx, pool_tx) = (&rx, pool_tx.clone());
                scope.spawn(move || {
                    let mut out = ParseOut::default();
                    loop {
                        let next = rx.lock().unwrap().recv();
                        let Ok(mut c) = next else { break };
                        parse_chunk(&mut c, rs as usize, &mut out);
                        let _ = pool_tx.send(c.buf);
                    }
                    out
                })
            })
            .collect();
        drop(pool_tx);
        let mut read = Duration::ZERO;
        let mut result = Ok(());
        let mut pos = 0;
        while pos < total {
            let len = chunk.min(total - pos);
            let mut buf = pool_rx
                .try_recv()
                .unwrap_or_else(|_| Aligned::new(capacity));
            let t = Instant::now();
            if let Err(e) = fill(volume, extents, &mut buf, pos, len) {
                result = Err(e);
                break;
            }
            read += t.elapsed();
            let c = Chunk {
                first: pos / rs,
                buf,
                len: len as usize,
            };
            if tx.send(c).is_err() {
                break;
            }
            pos += len;
        }
        drop(tx);
        let mut all = ParseOut::default();
        for p in parsers {
            let out = p.join().expect("parser thread");
            all.parsed.extend(out.parsed);
            all.failed.extend(out.failed);
            all.cpu += out.cpu;
        }
        result.map(|()| Read {
            parsed: all.parsed,
            failed: all.failed,
            read,
            parse_cpu: all.cpu,
            wall: started.elapsed(),
            records: total / rs,
        })
    })
}

/// Fills `buf` with `len` bytes of the table from stream offset `pos`,
/// one read per extent piece.
fn fill(
    volume: &Volume,
    extents: &[Extent],
    buf: &mut [u8],
    pos: u64,
    len: u64,
) -> Result<(), String> {
    let c = volume.cluster;
    let mut at = pos;
    while at < pos + len {
        let vcn = at / c;
        let e = extents
            .iter()
            .find(|e| vcn >= e.vcn && vcn < e.vcn + e.clusters)
            .ok_or_else(|| format!("$MFT offset {at:#x} isn't mapped"))?;
        e.lcn.ok_or("sparse run in $MFT")?;
        let take = (pos + len).min((e.vcn + e.clusters) * c) - at;
        let from = (at - pos) as usize;
        let size = volume.round_up(take) as usize;
        let to = (from + size).min(buf.len());
        let offset = record::volume_offset(extents, c, at)
            .ok_or_else(|| format!("$MFT offset {at:#x} maps past the volume"))?;
        volume.read_exact_at(&mut buf[from..to], offset)?;
        at += take;
    }
    Ok(())
}

/// The file reference and volume serial of `path`.
fn file_reference(path: &Path) -> Result<(u64, u64), String> {
    let handle =
        open_attributes(path).map_err(|e| format!("{}: {}", path.display(), e.message()))?;
    let mut id = FILE_ID_INFO::default();
    unsafe {
        GetFileInformationByHandleEx(
            raw(&handle),
            FileIdInfo,
            (&raw mut id).cast::<c_void>(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    }
    .map_err(|e| e.message())?;
    let frn = u64::from_le_bytes(id.FileId.Identifier[..8].try_into().unwrap());
    Ok((id.VolumeSerialNumber, frn))
}

/// An attributes-only open that never follows a link or recalls a file.
fn open_attributes(path: &Path) -> windows::core::Result<OwnedHandle> {
    let path = long_path(path);
    let handle = unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            FILE_READ_ATTRIBUTES.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_OPEN_NO_RECALL,
            None,
        )
    }?;
    // SAFETY: CreateFileW returned a valid handle that we now own.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle.0) })
}

struct WinProbe<'a> {
    fs: &'a NativeFs,
}

impl Probe for WinProbe<'_> {
    fn remeasure(&self, path: &str) -> Option<Measured> {
        self.fs.measure_file(Path::new(path)).ok()
    }

    fn attribute_tag(&self, path: &str) -> Option<(u32, u32)> {
        let handle = open_attributes(Path::new(path)).ok()?;
        let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
        unsafe {
            GetFileInformationByHandleEx(
                raw(&handle),
                FileAttributeTagInfo,
                (&raw mut info).cast::<c_void>(),
                size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
            )
        }
        .ok()?;
        Some((info.FileAttributes, info.ReparseTag))
    }
}

/// Lists `root` down with the production adapter on `threads` threads,
/// descending only what it reports as directories.
fn list_tree(fs: &NativeFs, root: &Path, root_record: u64, threads: usize) -> Listing {
    let queue = Mutex::new((vec![(root.to_path_buf(), root_record)], 0usize));
    let wake = Condvar::new();
    let listing = Mutex::new(Listing::default());
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let job = {
                        let mut q = queue.lock().unwrap();
                        loop {
                            if let Some(job) = q.0.pop() {
                                q.1 += 1;
                                break Some(job);
                            }
                            if q.1 == 0 {
                                break None;
                            }
                            q = wake.wait(q).unwrap();
                        }
                    };
                    let Some((dir, record)) = job else {
                        wake.notify_all();
                        return;
                    };
                    let mut nodes = Vec::new();
                    let mut subdirs = Vec::new();
                    let result = fs.read_dir(&dir, &mut |batch| {
                        for entry in batch {
                            let path = dir.join(&entry.name);
                            if entry.kind == EntryKind::Directory {
                                subdirs.push((path.clone(), (entry.id as u64) & RECORD_MASK));
                            }
                            nodes.push(ListedNode {
                                parent: record,
                                name: entry.name.encode_wide().collect(),
                                path: path.to_string_lossy().into_owned(),
                                entry,
                            });
                        }
                        ControlFlow::Continue(())
                    });
                    {
                        let mut l = listing.lock().unwrap();
                        match result {
                            Ok(()) => {
                                l.listed_dirs.insert(record);
                            }
                            Err(e) => l.failures.push((dir.display().to_string(), e.to_string())),
                        }
                        l.nodes.extend(nodes);
                    }
                    let mut q = queue.lock().unwrap();
                    q.0.extend(subdirs);
                    q.1 -= 1;
                    drop(q);
                    wake.notify_all();
                }
            });
        }
    });
    listing.into_inner().unwrap()
}

fn now_filetime() -> u64 {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    (since.as_secs() + FILETIME_EPOCH_OFFSET) * 10_000_000 + u64::from(since.subsec_nanos() / 100)
}

#[derive(Serialize)]
struct Geometry {
    bytes_per_sector: u64,
    bytes_per_cluster: u64,
    bytes_per_record: u64,
    mft_valid_bytes: u64,
    mft_extents: usize,
    records: u64,
}

#[derive(Serialize)]
struct Records {
    in_use_base: u64,
    extension: u64,
    merged_extensions: u64,
    orphan_extensions: u64,
    /// First-pass failures by reason, before re-reading.
    failed_first_pass: BTreeMap<String, u64>,
    reread_ok: u64,
    reread_not_in_use: u64,
    reread_failed: Vec<String>,
    attribute_list_records: u64,
    nonresident_attribute_lists: usize,
    nonresident_attribute_list_samples: Vec<String>,
    missing_list_segments: u64,
    nonresident_reparse: usize,
    hard_linked_records: u64,
    dos_names_skipped: u64,
    stale_parents: u64,
    /// In-use base records the walk from the root doesn't reach:
    /// metafiles (records 0-15, `$Extend` and below) and orphans.
    not_under_root: u64,
    alternate_streams_allocated: u64,
}

#[derive(Serialize)]
struct Timing {
    /// Time spent in volume reads (on the reader thread).
    read_s: f64,
    /// Parser CPU summed over threads.
    parse_cpu_s: f64,
    /// Wall time from the first read to the last record parsed: the gate.
    read_parse_wall_s: f64,
    reread_s: f64,
    build_s: f64,
    /// Read and parse, torn-record re-reads, and the table build: the
    /// whole cost before a tree could be shown.
    read_parse_build_s: f64,
    walk_s: f64,
    listing_walk_s: Option<f64>,
    compare_s: Option<f64>,
    journal_s: f64,
    total_s: f64,
}

#[derive(Serialize)]
struct Changes {
    journal_id: Option<String>,
    start_next_usn: Option<u64>,
    end_next_usn: Option<u64>,
    /// The journal was reset or wrapped past the start USN: changes during
    /// the run can't be known, so a comparison can't pass.
    lost: bool,
    lag_secs: u64,
    /// Journal records from the start USN on.
    records_during: u64,
    /// Journal records in the lag window before the start.
    records_in_lag_window: u64,
    /// Distinct file records either touched.
    changed_records: usize,
    samples: Vec<String>,
    error: Option<String>,
}

#[derive(Serialize)]
struct Report {
    volume: String,
    filesystem: String,
    threads: usize,
    chunk_mib: u64,
    geometry: Geometry,
    records: Records,
    /// Everything under the volume root, hard links counted once.
    volume_totals: Counted,
    timing: Timing,
    /// The ≤ 4 s read-and-parse gate (meaningful on a cold cache only).
    read_parse_within_4s: bool,
    /// The same budget held to read, parse and build.
    read_parse_build_within_4s: bool,
    changes: Changes,
    comparison: Option<Comparison>,
}

pub fn run(letter: char, opts: &MftOpts) -> Result<(String, bool), Failure> {
    let started = Instant::now();
    const DRIVE_NO_ROOT_DIR: u32 = 1;
    const DRIVE_REMOTE: u32 = 4;
    match drive_type(letter) {
        DRIVE_REMOTE => {
            return Err(Failure::Refused(format!(
                "{letter}: is a network drive: its file table isn't on this computer"
            )));
        }
        0 | DRIVE_NO_ROOT_DIR => {
            return Err(Failure::Refused(format!("{letter}: isn't a volume")));
        }
        _ => {}
    }
    let fs_name = filesystem(letter)
        .ok_or_else(|| Failure::Refused(format!("{letter}: isn't a readable volume")))?;
    if fs_name != "NTFS" {
        return Err(Failure::Refused(format!(
            "{letter}: is {fs_name}, not NTFS: there is no master file table to read"
        )));
    }
    if !elevated() {
        return Err(Failure::Refused(format!(
            "reading {letter}:'s master file table needs an elevated (administrator) prompt; \
             nothing was read"
        )));
    }
    let compare_root = match &opts.compare {
        Some(root) => {
            let absolute =
                std::path::absolute(root).map_err(|e| Failure::Refused(e.to_string()))?;
            let text = absolute.to_string_lossy().to_ascii_uppercase();
            if !text.starts_with(&format!("{letter}:")) {
                return Err(Failure::Refused(format!(
                    "{} isn't on {letter}:",
                    absolute.display()
                )));
            }
            Some(absolute)
        }
        None => None,
    };
    let failed = |e: String| Failure::Failed(e);
    let volume = Volume::open(letter)?;
    let journal_start = volume.query_journal();
    let start_time = now_filetime();

    let extents = volume.mft_extents().map_err(failed)?;
    let read = read_table(&volume, &extents, opts.chunk_mib * MIB, opts.threads).map_err(failed)?;

    let reread_started = Instant::now();
    let mut failed_first_pass: BTreeMap<String, u64> = BTreeMap::new();
    let (mut reread_ok, mut reread_not_in_use, mut reread_failed) = (0, 0, Vec::new());
    let mut recovered = Vec::new();
    for &(number, error) in &read.failed {
        *failed_first_pass.entry(error.to_string()).or_default() += 1;
        match volume.reread(number) {
            Ok(Some(p)) => {
                reread_ok += 1;
                recovered.push(p);
            }
            Ok(None) => reread_not_in_use += 1,
            Err(e) => reread_failed.push(format!("record {number}: {error}; re-read: {e}")),
        }
    }
    let reread_s = reread_started.elapsed().as_secs_f64();

    let build_started = Instant::now();
    let mut chunks = read.parsed;
    chunks.push(recovered);
    let table = Table::build(chunks, read.records);
    let build_s = build_started.elapsed().as_secs_f64();
    let read_parse_build_s = read.wall.as_secs_f64() + reread_s + build_s;
    let walk_started = Instant::now();
    let walk = table.walk(ROOT);
    let walk_s = walk_started.elapsed().as_secs_f64();
    let mut reached = vec![false; table.records.len()];
    for node in &walk.nodes {
        reached[node.record as usize] = true;
    }
    reached[ROOT as usize] = true;
    let not_under_root = table
        .records
        .iter()
        .zip(&reached)
        .filter(|(r, seen)| r.in_use && !**seen)
        .count() as u64;
    let alternate_streams_allocated = table.records.iter().map(|r| r.streams_allocated).sum();
    let volume_totals: Counted = walk.totals.into();
    drop(walk);

    // The listing walk, before the journal is read so its changes count.
    let mut listing = None;
    let mut listing_walk_s = None;
    if let Some(root) = &compare_root {
        let fs = NativeFs::new();
        match fs.open_root(root) {
            Ok(RootKind::Directory) => {}
            other => {
                return Err(Failure::Failed(format!(
                    "{} isn't a plain folder: {other:?}",
                    root.display()
                )));
            }
        }
        let (serial, frn) = file_reference(root).map_err(failed)?;
        if serial != volume.serial {
            return Err(Failure::Refused(format!(
                "{} is on another volume (a mount point)",
                root.display()
            )));
        }
        let t = Instant::now();
        let walk_threads = std::thread::available_parallelism().map_or(8, |n| n.get().min(16));
        let l = list_tree(&fs, root, frn & RECORD_MASK, walk_threads);
        listing_walk_s = Some(t.elapsed().as_secs_f64());
        listing = Some((fs, frn & RECORD_MASK, l));
    }

    let journal_started = Instant::now();
    let mut changes = Changes {
        journal_id: None,
        start_next_usn: None,
        end_next_usn: None,
        lost: true,
        lag_secs: opts.lag_secs,
        records_during: 0,
        records_in_lag_window: 0,
        changed_records: 0,
        samples: Vec::new(),
        error: None,
    };
    let mut changed: HashSet<u64> = HashSet::new();
    match (journal_start, volume.query_journal()) {
        (Ok(start), Ok(end)) => {
            changes.journal_id = Some(format!("{:#x}", start.id));
            changes.start_next_usn = Some(start.next);
            changes.end_next_usn = Some(end.next);
            changes.lost = start.id != end.id || end.first > start.next;
            if !changes.lost {
                let from = if opts.lag_secs == 0 {
                    start.next
                } else {
                    end.first
                };
                let window = start_time.saturating_sub(opts.lag_secs * 10_000_000);
                let (samples, mut during, mut before) = (opts.samples, 0, 0);
                let mut names = Vec::new();
                let result = volume.read_journal(from, end.id, end.next, |r| {
                    let is_during = r.usn >= start.next;
                    if !is_during && r.timestamp < window {
                        return;
                    }
                    if is_during {
                        during += 1;
                    } else {
                        before += 1;
                    }
                    if changed.insert(r.file & RECORD_MASK) && names.len() < samples {
                        names.push(String::from_utf16_lossy(&r.name));
                    }
                });
                changes.records_during = during;
                changes.records_in_lag_window = before;
                changes.samples = names;
                match result {
                    Ok(()) => {}
                    Err(e) => {
                        changes.lost = true;
                        changes.error = Some(format!("FSCTL_READ_USN_JOURNAL: {e}"));
                    }
                }
            }
        }
        (Err(e), _) | (_, Err(e)) => {
            changes.error = Some(format!("FSCTL_QUERY_USN_JOURNAL: {e}"));
        }
    }
    changes.changed_records = changed.len();
    let journal_s = journal_started.elapsed().as_secs_f64();

    let mut compare_s = None;
    let comparison = listing.map(|(fs, root, l)| {
        let t = Instant::now();
        let probe = WinProbe { fs: &fs };
        let mut c = compare::compare(&table, root, &l, &changed, &probe, opts.samples, opts.show);
        c.pass &= !changes.lost;
        compare_s = Some(t.elapsed().as_secs_f64());
        c
    });
    let failed_compare = comparison.as_ref().is_some_and(|c| !c.pass);

    let s = &table.stats;
    let report = Report {
        volume: format!("{letter}:"),
        filesystem: fs_name,
        threads: opts.threads,
        chunk_mib: opts.chunk_mib,
        geometry: Geometry {
            bytes_per_sector: volume.sector,
            bytes_per_cluster: volume.cluster,
            bytes_per_record: volume.record_size,
            mft_valid_bytes: volume.mft_valid,
            mft_extents: extents.len(),
            records: read.records,
        },
        records: Records {
            in_use_base: s.base_records,
            extension: s.extension_records,
            merged_extensions: s.merged_extensions,
            orphan_extensions: s.orphan_extensions,
            failed_first_pass,
            reread_ok,
            reread_not_in_use,
            reread_failed,
            attribute_list_records: s.attribute_list_records,
            nonresident_attribute_lists: s.nonresident_attribute_lists.len(),
            nonresident_attribute_list_samples: s
                .nonresident_attribute_lists
                .iter()
                .take(opts.samples)
                .map(|&r| table.path(r & RECORD_MASK))
                .collect(),
            missing_list_segments: s.missing_list_segments,
            nonresident_reparse: s.nonresident_reparse.len(),
            hard_linked_records: s.hard_linked_records,
            dos_names_skipped: s.dos_names,
            stale_parents: s.stale_parents,
            not_under_root,
            alternate_streams_allocated,
        },
        volume_totals,
        timing: Timing {
            read_s: read.read.as_secs_f64(),
            parse_cpu_s: read.parse_cpu.as_secs_f64(),
            read_parse_wall_s: read.wall.as_secs_f64(),
            reread_s,
            build_s,
            read_parse_build_s,
            walk_s,
            listing_walk_s,
            compare_s,
            journal_s,
            total_s: started.elapsed().as_secs_f64(),
        },
        read_parse_within_4s: read.wall <= Duration::from_secs(4),
        read_parse_build_within_4s: read_parse_build_s <= 4.0,
        changes,
        comparison,
    };
    let json = serde_json::to_string_pretty(&report).map_err(|e| Failure::Failed(e.to_string()))?;
    Ok((json, failed_compare))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const CLUSTER: usize = 4096;
    const RS: usize = 1024;

    fn resident(type_code: u32, value: &[u8]) -> Vec<u8> {
        let len = (24 + value.len()).div_ceil(8) * 8;
        let mut a = vec![0u8; len];
        a[0..4].copy_from_slice(&type_code.to_le_bytes());
        a[4..8].copy_from_slice(&(len as u32).to_le_bytes());
        a[16..20].copy_from_slice(&(value.len() as u32).to_le_bytes());
        a[20..22].copy_from_slice(&24u16.to_le_bytes());
        a[24..24 + value.len()].copy_from_slice(value);
        a
    }

    fn mft_data(runs: &[u8], clusters: u64) -> Vec<u8> {
        let len = (64 + runs.len() + 1).div_ceil(8) * 8;
        let mut a = vec![0u8; len];
        a[0..4].copy_from_slice(&ATTR_DATA.to_le_bytes());
        a[4..8].copy_from_slice(&(len as u32).to_le_bytes());
        a[8] = 1;
        a[24..32].copy_from_slice(&(clusters - 1).to_le_bytes());
        a[32..34].copy_from_slice(&64u16.to_le_bytes());
        let bytes = clusters * CLUSTER as u64;
        for at in [40, 48, 56] {
            a[at..at + 8].copy_from_slice(&bytes.to_le_bytes());
        }
        a[64..64 + runs.len()].copy_from_slice(runs);
        a
    }

    fn name(parent: u64, text: &str) -> Vec<u8> {
        let text: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let mut v = vec![0u8; 66 + text.len()];
        v[0..8].copy_from_slice(&parent.to_le_bytes());
        v[64] = (text.len() / 2) as u8;
        v[65] = 1;
        v[66..].copy_from_slice(&text);
        resident(0x30, &v)
    }

    /// A protected (on-disk) in-use record.
    fn make(number: u32, directory: bool, attrs: &[Vec<u8>]) -> Vec<u8> {
        let mut r = vec![0u8; RS];
        r[0..4].copy_from_slice(b"FILE");
        r[4..6].copy_from_slice(&0x30u16.to_le_bytes());
        r[6..8].copy_from_slice(&3u16.to_le_bytes());
        r[16..18].copy_from_slice(&1u16.to_le_bytes());
        r[20..22].copy_from_slice(&0x38u16.to_le_bytes());
        r[22..24].copy_from_slice(&(1u16 | if directory { 2 } else { 0 }).to_le_bytes());
        r[44..48].copy_from_slice(&number.to_le_bytes());
        let mut at = 0x38;
        for a in attrs {
            r[at..at + a.len()].copy_from_slice(a);
            at += a.len();
        }
        r[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        r[24..28].copy_from_slice(&((at + 8) as u32).to_le_bytes());
        r[0x30..0x32].copy_from_slice(&7u16.to_le_bytes());
        for i in 1..3 {
            let tail = i * 512 - 2;
            let saved = [r[tail], r[tail + 1]];
            r[0x30 + 2 * i..0x32 + 2 * i].copy_from_slice(&saved);
            r[tail..tail + 2].copy_from_slice(&7u16.to_le_bytes());
        }
        r
    }

    #[test]
    fn reads_a_fragmented_table_from_a_volume_image() {
        // $MFT in three extents, the last one before the second (a
        // negative run offset): records 0-15 at LCN 10, 16-23 at LCN 40,
        // 24-31 at LCN 20.
        let runs = [0x11, 0x04, 0x0A, 0x11, 0x02, 0x1E, 0x11, 0x02, 0xEC, 0x00];
        let at = |record: usize| match record {
            0..16 => 10 * CLUSTER + record * RS,
            16..24 => 40 * CLUSTER + (record - 16) * RS,
            _ => 20 * CLUSTER + (record - 24) * RS,
        };
        let mut image = vec![0u8; 48 * CLUSTER];
        let mut put = |record: usize, bytes: Vec<u8>| {
            image[at(record)..at(record) + RS].copy_from_slice(&bytes);
        };
        let si = resident(0x10, &[0u8; 72]);
        let root_ref = (5u64 << 48) | 5;
        put(0, make(0, false, &[si.clone(), mft_data(&runs, 8)]));
        let mut root = make(5, true, &[si.clone(), name(root_ref, ".")]);
        root[16..18].copy_from_slice(&5u16.to_le_bytes());
        put(5, root);
        for n in 16..32 {
            let data = resident(0x80, &vec![b'x'; n]);
            let file = name(root_ref, &format!("f{n}"));
            put(n, make(n as u32, false, &[si.clone(), file, data]));
        }
        // Record 27 was being written when read: its second stride's tail
        // no longer carries the check value.
        image[at(27) + 1022] = 0;

        let dir = std::env::temp_dir().join(format!("sb-mft-image-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("volume.img");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(&image)
            .unwrap();
        let volume = Volume {
            file: std::fs::File::open(&path).unwrap(),
            serial: 0,
            sector: 512,
            cluster: CLUSTER as u64,
            record_size: RS as u64,
            mft_valid: 32 * RS as u64,
            mft_start_lcn: 10,
            total_clusters: 48,
        };
        let extents = volume.mft_extents().unwrap();
        assert_eq!(extents.len(), 3);
        assert_eq!((extents[2].vcn, extents[2].lcn), (6, Some(20)));
        // Chunks smaller than an extent and straddling extent boundaries.
        for chunk in [3 * RS as u64, 5 * RS as u64, 64 * RS as u64] {
            let read = read_table(&volume, &extents, chunk, 3).unwrap();
            assert_eq!(read.records, 32);
            assert_eq!(read.failed, [(27, RecordError::Torn)]);
            let table = Table::build(read.parsed, read.records);
            let walk = table.walk(ROOT);
            assert_eq!(walk.totals.files, 15, "chunk {chunk}");
            let sizes: u64 = (16..32).filter(|&n| n != 27).sum();
            assert_eq!(walk.totals.logical, sizes);
            assert_eq!(table.path(31), "\\f31");
        }
        drop(volume);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
