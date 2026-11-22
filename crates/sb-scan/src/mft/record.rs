//! One MFT file record: update-sequence fixups, the attribute walk, and the
//! attributes a scan needs. Byte layouts follow the on-disk NTFS 3.1 format.

/// `$STANDARD_INFORMATION`.
pub const ATTR_STANDARD_INFORMATION: u32 = 0x10;
/// `$ATTRIBUTE_LIST`.
pub const ATTR_ATTRIBUTE_LIST: u32 = 0x20;
/// `$FILE_NAME`.
pub const ATTR_FILE_NAME: u32 = 0x30;
/// `$DATA`.
pub const ATTR_DATA: u32 = 0x80;
/// `$REPARSE_POINT`.
pub const ATTR_REPARSE_POINT: u32 = 0xC0;
const ATTR_END: u32 = 0xFFFF_FFFF;

/// Record header flags.
const RECORD_IN_USE: u16 = 0x1;
const RECORD_IS_DIRECTORY: u16 = 0x2;

/// Attribute header flags.
const ATTRIBUTE_COMPRESSED: u16 = 0x0001;
const ATTRIBUTE_SPARSE: u16 = 0x8000;

/// `$FILE_NAME` namespace that holds only the 8.3 alias of another name.
pub const NAMESPACE_DOS: u8 = 2;

/// The update sequence protects the last two bytes of every 512-byte
/// stride of a record, whatever the device's sector size.
const STRIDE: usize = 512;

/// Low 48 bits of a file reference: the record number.
pub const RECORD_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

/// The named `$DATA` stream WOF keeps a compressed file's contents in.
const WOF_STREAM: &str = "WofCompressedData";

/// Why a record couldn't be parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordError {
    /// Not a `FILE` record (`BAAD` or garbage).
    BadSignature,
    /// A sector's update-sequence check value didn't match: the record was
    /// being written while it was read (torn), or is damaged.
    Torn,
    /// The header or an attribute points outside the record.
    Malformed,
    /// The record's own number disagrees with its position.
    WrongNumber,
    /// A run list didn't decode.
    BadRunList,
}

impl std::fmt::Display for RecordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::BadSignature => "not a FILE record",
            Self::Torn => "update sequence mismatch (torn record)",
            Self::Malformed => "malformed record",
            Self::WrongNumber => "record number mismatch",
            Self::BadRunList => "bad run list",
        })
    }
}

impl std::error::Error for RecordError {}

pub(crate) fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        b.get(at..at.checked_add(2)?)?.try_into().ok()?,
    ))
}

pub(crate) fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        b.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}

pub(crate) fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        b.get(at..at.checked_add(8)?)?.try_into().ok()?,
    ))
}

fn utf16(bytes: &[u8]) -> Vec<u16> {
    bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect()
}

/// Checks and undoes the update-sequence protection in place: every
/// stride's last two bytes must equal the sequence number and are replaced
/// by the saved originals. The array must cover exactly the record's
/// strides (`len / 512 + 1` entries, the check value first).
///
/// Applying it to a record that is already fixed up changes nothing: a
/// stride tail that happens to equal the check value is replaced by the
/// saved original, which is what it already holds.
pub fn apply_fixups(record: &mut [u8]) -> Result<(), RecordError> {
    let offset = usize::from(u16_at(record, 4).ok_or(RecordError::Malformed)?);
    let count = usize::from(u16_at(record, 6).ok_or(RecordError::Malformed)?);
    if record.is_empty()
        || !record.len().is_multiple_of(STRIDE)
        || count != record.len() / STRIDE + 1
        || offset + 2 * count > record.len()
    {
        return Err(RecordError::Malformed);
    }
    let check = [record[offset], record[offset + 1]];
    for i in 1..count {
        let end = i * STRIDE - 2;
        if record[end..end + 2] != check {
            return Err(RecordError::Torn);
        }
    }
    for i in 1..count {
        let end = i * STRIDE - 2;
        let saved = offset + 2 * i;
        record.copy_within(saved..saved + 2, end);
    }
    Ok(())
}

/// Fixups for a record that may already have had them applied, as records
/// returned by `FSCTL_GET_NTFS_FILE_RECORD` may (they come from the cache):
/// applied when every stride carries the check value, else taken as is.
pub fn apply_fixups_if_protected(record: &mut [u8]) -> Result<(), RecordError> {
    match apply_fixups(record) {
        Err(RecordError::Torn) => Ok(()),
        other => other,
    }
}

/// One attribute inside a record.
#[derive(Debug, Clone, Copy)]
pub struct Attribute<'a> {
    pub type_code: u32,
    /// The whole attribute, header included.
    pub bytes: &'a [u8],
    pub non_resident: bool,
    pub flags: u16,
    /// The attribute's name as little-endian UTF-16 bytes; empty if unnamed.
    pub name: &'a [u8],
}

impl<'a> Attribute<'a> {
    /// A resident attribute's value.
    pub fn value(&self) -> Result<&'a [u8], RecordError> {
        if self.non_resident {
            return Err(RecordError::Malformed);
        }
        let len = u32_at(self.bytes, 16).ok_or(RecordError::Malformed)? as usize;
        let at = usize::from(u16_at(self.bytes, 20).ok_or(RecordError::Malformed)?);
        self.bytes.get(at..at + len).ok_or(RecordError::Malformed)
    }

    /// A non-resident attribute's first VCN; 0 for resident attributes.
    pub fn lowest_vcn(&self) -> u64 {
        if self.non_resident {
            u64_at(self.bytes, 16).unwrap_or(0)
        } else {
            0
        }
    }

    /// A non-resident attribute's mapping pairs (run list).
    pub fn runs(&self) -> Result<Vec<Run>, RecordError> {
        if !self.non_resident {
            return Err(RecordError::Malformed);
        }
        let at = usize::from(u16_at(self.bytes, 32).ok_or(RecordError::Malformed)?);
        decode_runs(self.bytes.get(at..).ok_or(RecordError::Malformed)?)
    }

    fn is_named(&self, name: &str) -> bool {
        self.name.len() == 2 * name.len()
            && utf16(self.name).iter().copied().eq(name.encode_utf16())
    }

    /// Length and allocation as a directory listing reports them: a
    /// resident value's length rounded up to 8 bytes; for a non-resident
    /// one the allocated size, or the total allocated (clusters actually
    /// held) when it's compressed or sparse.
    ///
    /// Only meaningful for the instance with the lowest VCN 0: later pieces
    /// of a split attribute carry no sizes.
    pub fn stream(&self) -> Result<Stream, RecordError> {
        if !self.non_resident {
            let len = self.value()?.len() as u64;
            return Ok(Stream {
                logical: len,
                allocated: len.div_ceil(8) * 8,
                resident: true,
            });
        }
        let field = |at| u64_at(self.bytes, at).ok_or(RecordError::Malformed);
        let allocated = field(40)?;
        let logical = field(48)?;
        let allocated = if self.flags & (ATTRIBUTE_COMPRESSED | ATTRIBUTE_SPARSE) != 0 {
            field(64)?
        } else {
            allocated
        };
        Ok(Stream {
            logical,
            allocated,
            resident: false,
        })
    }
}

/// The attributes of a record whose fixups have been applied, in order,
/// up to the end marker.
pub fn attributes(record: &[u8]) -> impl Iterator<Item = Result<Attribute<'_>, RecordError>> {
    let used = u32_at(record, 24).map_or(0, |u| (u as usize).min(record.len()));
    let mut at = u16_at(record, 20).map_or(usize::MAX, usize::from);
    let mut done = false;
    std::iter::from_fn(move || {
        if done {
            return None;
        }
        let item = (|| {
            let type_code = u32_at(record, at).filter(|_| at + 4 <= used);
            let type_code = type_code.ok_or(RecordError::Malformed)?;
            if type_code == ATTR_END {
                return Ok(None);
            }
            let len = u32_at(record, at + 4).ok_or(RecordError::Malformed)? as usize;
            if len < 24 || !len.is_multiple_of(8) || at + len > used {
                return Err(RecordError::Malformed);
            }
            let bytes = &record[at..at + len];
            let name_len = usize::from(bytes[9]) * 2;
            let name_at = usize::from(u16_at(bytes, 10).ok_or(RecordError::Malformed)?);
            let name = if name_len == 0 {
                &[][..]
            } else {
                bytes
                    .get(name_at..name_at + name_len)
                    .ok_or(RecordError::Malformed)?
            };
            let attribute = Attribute {
                type_code,
                bytes,
                non_resident: bytes[8] != 0,
                flags: u16_at(bytes, 12).ok_or(RecordError::Malformed)?,
                name,
            };
            if attribute.non_resident && len < 64 {
                return Err(RecordError::Malformed);
            }
            at += len;
            Ok(Some(attribute))
        })();
        match item {
            Ok(Some(a)) => Some(Ok(a)),
            Ok(None) => {
                done = true;
                None
            }
            Err(e) => {
                done = true;
                Some(Err(e))
            }
        }
    })
}

/// One run of a non-resident attribute: `clusters` clusters starting at
/// `lcn`, or a sparse hole when `lcn` is `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    pub lcn: Option<u64>,
    pub clusters: u64,
}

/// Decodes a mapping-pairs array. Each pair is a header byte (low nibble:
/// length size, high nibble: offset size), the run length, and the run's
/// start as a signed delta from the previous run's; an offset size of 0
/// marks a sparse run. A 0 header ends the list.
pub fn decode_runs(bytes: &[u8]) -> Result<Vec<Run>, RecordError> {
    let mut runs = Vec::new();
    let mut at = 0;
    let mut lcn: i64 = 0;
    loop {
        let header = *bytes.get(at).ok_or(RecordError::BadRunList)?;
        if header == 0 {
            return Ok(runs);
        }
        let len_size = usize::from(header & 0xF);
        let off_size = usize::from(header >> 4);
        if len_size == 0 || len_size > 8 || off_size > 8 {
            return Err(RecordError::BadRunList);
        }
        let field = bytes
            .get(at + 1..at + 1 + len_size + off_size)
            .ok_or(RecordError::BadRunList)?;
        let mut clusters = [0u8; 8];
        clusters[..len_size].copy_from_slice(&field[..len_size]);
        let clusters = u64::from_le_bytes(clusters);
        if clusters == 0 || clusters > i64::MAX as u64 {
            return Err(RecordError::BadRunList);
        }
        let run_lcn = if off_size == 0 {
            None
        } else {
            let raw = &field[len_size..];
            let fill = if raw[off_size - 1] & 0x80 != 0 {
                0xFF
            } else {
                0
            };
            let mut delta = [fill; 8];
            delta[..off_size].copy_from_slice(raw);
            lcn = lcn
                .checked_add(i64::from_le_bytes(delta))
                .filter(|&l| l >= 0)
                .ok_or(RecordError::BadRunList)?;
            Some(lcn as u64)
        };
        runs.push(Run {
            lcn: run_lcn,
            clusters,
        });
        at += 1 + len_size + off_size;
    }
}

/// Length and allocation of a data stream; see [`Attribute::stream`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stream {
    pub logical: u64,
    pub allocated: u64,
    pub resident: bool,
}

/// Fields of `$STANDARD_INFORMATION` a scan uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Standard {
    /// `FILE_ATTRIBUTE_*` flags as stored (no `DIRECTORY` bit: that comes
    /// from the record header).
    pub attributes: u32,
    /// Last write time (`FILETIME` ticks).
    pub modified: u64,
    /// The record's last USN; 0 when the journal never touched it.
    pub usn: u64,
}

/// One `$FILE_NAME` (never a DOS-only alias).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileName {
    /// File reference (record number and sequence) of the parent directory.
    pub parent: u64,
    pub name: Vec<u16>,
    pub namespace: u8,
}

/// One `$ATTRIBUTE_LIST` entry: where an attribute (or a piece of one,
/// from `lowest_vcn`) lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListEntry {
    pub type_code: u32,
    pub lowest_vcn: u64,
    /// File reference of the record holding it.
    pub segment: u64,
    pub name: Vec<u16>,
}

/// A record's attribute list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttributeList {
    Resident(Vec<ListEntry>),
    /// Stored outside the record. Its extension records are still merged
    /// by their base reference, but the list itself isn't checked; such
    /// records are reported so an engine can re-read them by file ID.
    NonResident,
}

/// Parses an attribute list's value.
pub fn parse_attribute_list(value: &[u8]) -> Result<Vec<ListEntry>, RecordError> {
    let mut entries = Vec::new();
    let mut at = 0;
    while at + 26 <= value.len() {
        let e = &value[at..];
        let len = usize::from(u16_at(e, 4).ok_or(RecordError::Malformed)?);
        if len < 26 || len > e.len() {
            return Err(RecordError::Malformed);
        }
        let name_len = usize::from(e[6]) * 2;
        let name_at = usize::from(e[7]);
        let name = e
            .get(name_at..name_at + name_len)
            .filter(|_| name_at + name_len <= len)
            .ok_or(RecordError::Malformed)?;
        entries.push(ListEntry {
            type_code: u32_at(e, 0).ok_or(RecordError::Malformed)?,
            lowest_vcn: u64_at(e, 8).ok_or(RecordError::Malformed)?,
            segment: u64_at(e, 16).ok_or(RecordError::Malformed)?,
            name: utf16(name),
        });
        at += len;
    }
    Ok(entries)
}

/// What a scan needs from one in-use record (base or extension).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Parsed {
    pub number: u64,
    pub sequence: u16,
    /// File reference of the base record; 0 for a base record.
    pub base: u64,
    pub directory: bool,
    pub link_count: u16,
    pub standard: Option<Standard>,
    /// The unnamed `$DATA` stream (from its lowest-VCN-0 instance).
    pub data: Option<Stream>,
    /// WOF's `WofCompressedData` stream.
    pub wof: Option<Stream>,
    /// Allocation of other named `$DATA` streams (alternate data streams),
    /// which listings don't count.
    pub streams_allocated: u64,
    pub reparse_tag: Option<u32>,
    /// The reparse data is stored outside the record, so its tag isn't
    /// known here (`reparse_tag` is then 0).
    pub reparse_nonresident: bool,
    pub attribute_list: Option<AttributeList>,
    pub names: Vec<FileName>,
    /// `$FILE_NAME`s in the DOS namespace, skipped.
    pub dos_names: u16,
}

impl Parsed {
    /// This record's file reference.
    pub fn reference(&self) -> u64 {
        (u64::from(self.sequence) << 48) | self.number
    }
}

/// Parses a record whose fixups have been applied. `Ok(None)` for a record
/// that isn't in use (or was never initialised: all zeros).
pub fn parse_record(record: &[u8], number: u64) -> Result<Option<Parsed>, RecordError> {
    match record.get(..4) {
        Some(b"FILE") => {}
        Some([0, 0, 0, 0]) => return Ok(None),
        Some(_) => return Err(RecordError::BadSignature),
        None => return Err(RecordError::Malformed),
    }
    let flags = u16_at(record, 22).ok_or(RecordError::Malformed)?;
    if flags & RECORD_IN_USE == 0 {
        return Ok(None);
    }
    // NTFS 3.1 (XP on) keeps the record's own number after the header.
    let usa_offset = u16_at(record, 4).ok_or(RecordError::Malformed)?;
    if usa_offset >= 0x30 {
        let own = u32_at(record, 44).ok_or(RecordError::Malformed)?;
        if u64::from(own) != number & 0xFFFF_FFFF {
            return Err(RecordError::WrongNumber);
        }
    }
    let mut parsed = Parsed {
        number,
        sequence: u16_at(record, 16).ok_or(RecordError::Malformed)?,
        base: u64_at(record, 32).ok_or(RecordError::Malformed)?,
        directory: flags & RECORD_IS_DIRECTORY != 0,
        link_count: u16_at(record, 18).ok_or(RecordError::Malformed)?,
        ..Parsed::default()
    };
    for attribute in attributes(record) {
        let attribute = attribute?;
        match attribute.type_code {
            ATTR_STANDARD_INFORMATION => {
                let v = attribute.value()?;
                parsed.standard = Some(Standard {
                    modified: u64_at(v, 8).ok_or(RecordError::Malformed)?,
                    attributes: u32_at(v, 32).ok_or(RecordError::Malformed)?,
                    // NTFS 1.2 records stop before the USN.
                    usn: u64_at(v, 64).unwrap_or(0),
                });
            }
            ATTR_FILE_NAME => {
                let v = attribute.value()?;
                let len = usize::from(*v.get(64).ok_or(RecordError::Malformed)?);
                let namespace = *v.get(65).ok_or(RecordError::Malformed)?;
                if namespace == NAMESPACE_DOS {
                    parsed.dos_names += 1;
                    continue;
                }
                let name = v.get(66..66 + 2 * len).ok_or(RecordError::Malformed)?;
                parsed.names.push(FileName {
                    parent: u64_at(v, 0).ok_or(RecordError::Malformed)?,
                    name: utf16(name),
                    namespace,
                });
            }
            ATTR_DATA => {
                // Later pieces of a split attribute carry no sizes.
                if attribute.lowest_vcn() != 0 {
                    continue;
                }
                let stream = attribute.stream()?;
                if attribute.name.is_empty() {
                    parsed.data = Some(stream);
                } else if attribute.is_named(WOF_STREAM) {
                    parsed.wof = Some(stream);
                } else {
                    parsed.streams_allocated =
                        parsed.streams_allocated.saturating_add(stream.allocated);
                }
            }
            ATTR_REPARSE_POINT => {
                // Reparse data is small and normally resident; the tag of
                // a non-resident one isn't read here.
                parsed.reparse_nonresident = attribute.non_resident;
                parsed.reparse_tag = Some(if attribute.non_resident {
                    0
                } else {
                    u32_at(attribute.value()?, 0).ok_or(RecordError::Malformed)?
                });
            }
            ATTR_ATTRIBUTE_LIST => {
                parsed.attribute_list = Some(if attribute.non_resident {
                    AttributeList::NonResident
                } else {
                    AttributeList::Resident(parse_attribute_list(attribute.value()?)?)
                });
            }
            _ => {}
        }
    }
    Ok(Some(parsed))
}

/// The unnamed `$DATA` pieces of a record, as (lowest VCN, runs), for
/// following `$MFT`'s own extents.
pub fn data_runs(record: &[u8]) -> Result<Vec<(u64, Vec<Run>)>, RecordError> {
    let mut pieces = Vec::new();
    for attribute in attributes(record) {
        let attribute = attribute?;
        if attribute.type_code == ATTR_DATA && attribute.name.is_empty() && attribute.non_resident {
            pieces.push((attribute.lowest_vcn(), attribute.runs()?));
        }
    }
    Ok(pieces)
}

/// The unnamed `$DATA` stream's sizes from a record (for `$MFT`'s length).
pub fn data_stream(record: &[u8]) -> Result<Option<Stream>, RecordError> {
    for attribute in attributes(record) {
        let attribute = attribute?;
        if attribute.type_code == ATTR_DATA
            && attribute.name.is_empty()
            && attribute.lowest_vcn() == 0
        {
            return attribute.stream().map(Some);
        }
    }
    Ok(None)
}

/// A record's attribute list attribute: the resident value, or the runs
/// and length of a non-resident one.
pub enum ListLocation {
    Resident(Vec<ListEntry>),
    NonResident { runs: Vec<Run>, length: u64 },
}

/// Where a record's attribute list is, if it has one.
pub fn attribute_list_location(record: &[u8]) -> Result<Option<ListLocation>, RecordError> {
    for attribute in attributes(record) {
        let attribute = attribute?;
        if attribute.type_code == ATTR_ATTRIBUTE_LIST {
            return Ok(Some(if attribute.non_resident {
                ListLocation::NonResident {
                    runs: attribute.runs()?,
                    length: attribute.stream()?.logical,
                }
            } else {
                ListLocation::Resident(parse_attribute_list(attribute.value()?)?)
            }));
        }
    }
    Ok(None)
}

/// A non-resident stream's runs laid out by VCN.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    pub vcn: u64,
    /// `None` for a sparse hole.
    pub lcn: Option<u64>,
    pub clusters: u64,
}

/// Joins the pieces of a split attribute (each piece's runs start at its
/// lowest VCN) into extents, checking the pieces cover the stream without
/// gaps or overlaps.
pub fn extents(mut pieces: Vec<(u64, Vec<Run>)>) -> Result<Vec<Extent>, RecordError> {
    pieces.sort_by_key(|(vcn, _)| *vcn);
    let mut out = Vec::new();
    let mut next = 0u64;
    for (vcn, runs) in pieces {
        if vcn != next {
            return Err(RecordError::BadRunList);
        }
        for run in runs {
            out.push(Extent {
                vcn: next,
                lcn: run.lcn,
                clusters: run.clusters,
            });
            next = next
                .checked_add(run.clusters)
                .ok_or(RecordError::BadRunList)?;
        }
    }
    Ok(out)
}

/// Checks every mapped extent lies on a volume of `total_clusters`.
pub fn check_bounds(extents: &[Extent], total_clusters: u64) -> Result<(), RecordError> {
    for e in extents {
        if let Some(lcn) = e.lcn {
            let end = lcn.checked_add(e.clusters).ok_or(RecordError::BadRunList)?;
            if end > total_clusters {
                return Err(RecordError::BadRunList);
            }
        }
    }
    Ok(())
}

/// The byte offset on the volume of byte `offset` of a stream laid out in
/// `extents`; `None` when it's in a hole, past the end, or overflows.
pub fn volume_offset(extents: &[Extent], cluster: u64, offset: u64) -> Option<u64> {
    let vcn = offset / cluster;
    let e = extents
        .iter()
        .find(|e| vcn >= e.vcn && vcn - e.vcn < e.clusters)?;
    e.lcn?
        .checked_add(vcn - e.vcn)?
        .checked_mul(cluster)?
        .checked_add(offset % cluster)
}
