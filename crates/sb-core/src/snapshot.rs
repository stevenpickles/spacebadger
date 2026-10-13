//! A compact record of a finished scan, kept so a later scan of the same
//! folder can be compared with it ("what grew?").
//!
//! A snapshot holds every folder (name, logical size, file count) and every
//! file of at least [`FILE_THRESHOLD`] bytes (name, logical size). Smaller
//! files leave only a 32-bit hash of their name in their folder's entry: not
//! enough to say how big they were, but enough to tell a file that is new
//! since the snapshot from one that was merely small then. That keeps
//! snapshots to a few bytes per small file.
//!
//! Names are the native bytes from the tree (`OsStr::as_encoded_bytes`).
//! They are only compared and shown (lossily), never turned back into
//! paths, so a damaged file can't produce an invalid `OsStr`.
//!
//! A folder the scan couldn't read (permission denied) records that its
//! contents aren't known, so a comparison doesn't take its missing children
//! as removed or a later scan's children as new.
//!
//! # Format (version 1)
//!
//! All integers are little-endian; `varint` is unsigned LEB128.
//!
//! ```text
//! magic     8 bytes  "SBSNAP\0\0"
//! version   u32      1
//! head_len  u32      bytes in the header that follows
//! header    created_ms u64, threshold u64, logical u64, files u64,
//!           entries u32, flags u32 (bit 0: names compared ignoring case),
//!           root_len u32, root (UTF-8, as displayed)
//! entries   one per entry, parents before their children:
//!           varint parent distance (0 for the root, else index − parent),
//!           u8 kind (bit 0: folder; bit 1: the folder's contents were
//!           read, so its children are all here), varint name length, name bytes,
//!           varint logical size; folders add varint file count,
//!           varint small-file count, and that many u32 name hashes
//! checksum  u64      FNV-1a over every byte before it
//! ```
//!
//! The header can be read on its own to list snapshots without decoding
//! them. Readers ignore header bytes past the fields they know.

use crate::tree::{DirState, NodeId, Tree};
use std::fmt;
use std::io::Read;

/// Files at least this large are recorded with their size; smaller ones
/// only by a hash of their name.
pub const FILE_THRESHOLD: u64 = 1 << 20;

pub const MAGIC: [u8; 8] = *b"SBSNAP\0\0";
pub const VERSION: u32 = 1;

/// Snapshots compare names ignoring case where the platform's file names
/// usually do (Windows, and macOS by default), and exactly elsewhere.
pub const CASE_INSENSITIVE: bool = cfg!(any(windows, target_os = "macos"));

const KIND_DIR: u8 = 1;
const KIND_READ: u8 = 2;

const FLAG_CASE_INSENSITIVE: u32 = 1;
const MAX_HEADER: u32 = 64 * 1024;
const PREFIX: usize = 16;
const NONE: u32 = u32::MAX;

/// Why a snapshot couldn't be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotError {
    /// Not a snapshot file at all.
    NotASnapshot,
    /// Written by a newer version of the app.
    UnsupportedVersion(u32),
    /// Ends early, as after an interrupted write.
    Truncated,
    /// The contents don't add up.
    Corrupt(&'static str),
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotASnapshot => f.write_str("not a scan snapshot"),
            Self::UnsupportedVersion(v) => {
                write!(f, "snapshot version {v} is newer than this app reads")
            }
            Self::Truncated => f.write_str("snapshot is incomplete"),
            Self::Corrupt(what) => write!(f, "snapshot is damaged ({what})"),
        }
    }
}

impl std::error::Error for SnapshotError {}

/// What a snapshot is of, readable without decoding the entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotHeader {
    /// When the scan finished, in milliseconds since the Unix epoch.
    pub created_ms: u64,
    /// The [`FILE_THRESHOLD`] the snapshot was taken with.
    pub threshold: u64,
    /// Logical bytes under the root.
    pub logical: u64,
    /// Files under the root.
    pub files: u64,
    pub entries: u32,
    pub case_insensitive: bool,
    /// The scan root, as displayed.
    pub root: String,
}

/// A decoded snapshot. Entry 0 is the root; parents precede children.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    header: SnapshotHeader,
    parent: Vec<u32>,
    /// [`KIND_DIR`] and [`KIND_READ`] bits.
    kind: Vec<u8>,
    name_start: Vec<u32>,
    name_len: Vec<u16>,
    names: Vec<u8>,
    logical: Vec<u64>,
    files: Vec<u32>,
    /// Folders: their small files' name hashes, sorted, as a range of `small`.
    small_start: Vec<u32>,
    small_len: Vec<u32>,
    small: Vec<u32>,
}

impl Snapshot {
    /// Records `tree` (removed nodes left out). `created_ms` is the time to
    /// list it under; names are hashed for `case_insensitive` comparison.
    pub fn capture(tree: &Tree, created_ms: u64, case_insensitive: bool) -> Self {
        let root = NodeId::ROOT;
        let mut snap = Self {
            header: SnapshotHeader {
                created_ms,
                threshold: FILE_THRESHOLD,
                logical: tree.logical(root),
                files: u64::from(tree.file_count(root)),
                entries: 0,
                case_insensitive,
                root: tree.root_path().display().to_string(),
            },
            parent: Vec::new(),
            kind: Vec::new(),
            name_start: Vec::new(),
            name_len: Vec::new(),
            names: Vec::new(),
            logical: Vec::new(),
            files: Vec::new(),
            small_start: Vec::new(),
            small_len: Vec::new(),
            small: Vec::new(),
        };
        snap.push(
            NONE,
            b"",
            dir_kind(tree, root),
            tree.logical(root),
            tree.file_count(root),
        );
        let mut stack = vec![(root, 0u32)];
        let mut hashes = Vec::new();
        let mut key = Vec::new();
        while let Some((node, entry)) = stack.pop() {
            hashes.clear();
            for c in tree.children(node) {
                let name = tree.name(c).as_encoded_bytes();
                if tree.is_dir(c) {
                    let id = snap.push(
                        entry,
                        name,
                        dir_kind(tree, c),
                        tree.logical(c),
                        tree.file_count(c),
                    );
                    stack.push((c, id));
                } else if tree.logical(c) >= FILE_THRESHOLD {
                    snap.push(entry, name, 0, tree.logical(c), 1);
                } else {
                    hashes.push(name_hash(fold(name, case_insensitive, &mut key)));
                }
            }
            hashes.sort_unstable();
            hashes.dedup();
            let e = entry as usize;
            snap.small_start[e] = snap.small.len() as u32;
            snap.small_len[e] = hashes.len() as u32;
            snap.small.extend_from_slice(&hashes);
        }
        snap.header.entries = snap.parent.len() as u32;
        snap
    }

    fn push(&mut self, parent: u32, name: &[u8], kind: u8, logical: u64, files: u32) -> u32 {
        let id = self.parent.len() as u32;
        // Tree names are at most u16::MAX bytes, so this never truncates.
        let len = name.len().min(usize::from(u16::MAX));
        self.parent.push(parent);
        self.kind.push(kind);
        self.name_start.push(self.names.len() as u32);
        self.name_len.push(len as u16);
        self.names.extend_from_slice(&name[..len]);
        self.logical.push(logical);
        self.files.push(files);
        self.small_start.push(0);
        self.small_len.push(0);
        id
    }

    pub fn header(&self) -> &SnapshotHeader {
        &self.header
    }

    /// Number of entries, including the root.
    pub fn len(&self) -> usize {
        self.parent.len()
    }

    pub fn is_empty(&self) -> bool {
        self.parent.is_empty()
    }

    /// The entry's parent; `None` for the root.
    pub fn parent(&self, entry: u32) -> Option<u32> {
        let p = self.parent[entry as usize];
        (p != NONE).then_some(p)
    }

    pub fn is_dir(&self, entry: u32) -> bool {
        self.kind[entry as usize] & KIND_DIR != 0
    }

    /// Whether folder `entry`'s contents were read, so every child of it
    /// (above the threshold) is in the snapshot.
    pub fn was_read(&self, entry: u32) -> bool {
        self.kind[entry as usize] & KIND_READ != 0
    }

    /// The entry's native name bytes; empty for the root.
    pub fn name(&self, entry: u32) -> &[u8] {
        let i = entry as usize;
        let start = self.name_start[i] as usize;
        &self.names[start..start + self.name_len[i] as usize]
    }

    /// The name for display, with replacement characters where the native
    /// bytes aren't valid Unicode.
    pub fn display_name(&self, entry: u32) -> String {
        String::from_utf8_lossy(self.name(entry)).into_owned()
    }

    pub fn logical(&self, entry: u32) -> u64 {
        self.logical[entry as usize]
    }

    /// Files at or under the entry.
    pub fn file_count(&self, entry: u32) -> u32 {
        self.files[entry as usize]
    }

    /// Whether folder `entry` held a file below the threshold whose
    /// (folded) name hashes to `hash`.
    pub fn had_small_file(&self, entry: u32, hash: u32) -> bool {
        let i = entry as usize;
        let start = self.small_start[i] as usize;
        self.small[start..start + self.small_len[i] as usize]
            .binary_search(&hash)
            .is_ok()
    }

    /// Encodes the snapshot in the format described in the module notes.
    pub fn encode(&self) -> Vec<u8> {
        let h = &self.header;
        let mut out = Vec::with_capacity(PREFIX + 64 + self.names.len() + self.len() * 8);
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        let root = h.root.as_bytes();
        let head_len = 8 * 4 + 4 * 3 + root.len();
        out.extend_from_slice(&(head_len as u32).to_le_bytes());
        out.extend_from_slice(&h.created_ms.to_le_bytes());
        out.extend_from_slice(&h.threshold.to_le_bytes());
        out.extend_from_slice(&h.logical.to_le_bytes());
        out.extend_from_slice(&h.files.to_le_bytes());
        out.extend_from_slice(&(self.len() as u32).to_le_bytes());
        let flags = if h.case_insensitive {
            FLAG_CASE_INSENSITIVE
        } else {
            0
        };
        out.extend_from_slice(&flags.to_le_bytes());
        out.extend_from_slice(&(root.len() as u32).to_le_bytes());
        out.extend_from_slice(root);
        for i in 0..self.len() {
            let parent = self.parent[i];
            let distance = if parent == NONE {
                0
            } else {
                i as u64 - u64::from(parent)
            };
            put_varint(&mut out, distance);
            out.push(self.kind[i]);
            let name = self.name(i as u32);
            put_varint(&mut out, name.len() as u64);
            out.extend_from_slice(name);
            put_varint(&mut out, self.logical[i]);
            if self.kind[i] & KIND_DIR != 0 {
                put_varint(&mut out, u64::from(self.files[i]));
                let start = self.small_start[i] as usize;
                let small = &self.small[start..start + self.small_len[i] as usize];
                put_varint(&mut out, small.len() as u64);
                for hash in small {
                    out.extend_from_slice(&hash.to_le_bytes());
                }
            }
        }
        let sum = fnv64(&out);
        out.extend_from_slice(&sum.to_le_bytes());
        out
    }

    /// Decodes a whole snapshot file. Damaged or incomplete data gives an
    /// error, never a panic.
    pub fn decode(bytes: &[u8]) -> Result<Self, SnapshotError> {
        let (header, body_start) = parse_header(bytes)?;
        if bytes.len() < body_start + 8 {
            return Err(SnapshotError::Truncated);
        }
        let (content, sum) = bytes.split_at(bytes.len() - 8);
        let sum = u64::from_le_bytes(sum.try_into().expect("8 bytes"));
        if fnv64(content) != sum {
            // An interrupted write is the likelier story, but either way
            // the data can't be trusted.
            return Err(SnapshotError::Corrupt("checksum mismatch"));
        }
        let mut r = Cursor {
            bytes: &content[body_start..],
        };
        // Each entry takes at least four bytes, which bounds what a bogus
        // count can make us allocate.
        let cap = (header.entries as usize).min(r.bytes.len() / 4);
        let mut snap = Self {
            header,
            parent: Vec::with_capacity(cap),
            kind: Vec::with_capacity(cap),
            name_start: Vec::with_capacity(cap),
            name_len: Vec::with_capacity(cap),
            names: Vec::new(),
            logical: Vec::with_capacity(cap),
            files: Vec::with_capacity(cap),
            small_start: Vec::with_capacity(cap),
            small_len: Vec::with_capacity(cap),
            small: Vec::new(),
        };
        for i in 0..snap.header.entries {
            let distance = r.varint()?;
            let parent = if i == 0 {
                if distance != 0 {
                    return Err(SnapshotError::Corrupt("root has a parent"));
                }
                NONE
            } else {
                if distance == 0 || distance > u64::from(i) {
                    return Err(SnapshotError::Corrupt("parent out of order"));
                }
                let p = i - distance as u32;
                if snap.kind[p as usize] & KIND_DIR == 0 {
                    return Err(SnapshotError::Corrupt("parent is a file"));
                }
                p
            };
            let kind = r.byte()?;
            if kind & !(KIND_DIR | KIND_READ) != 0 || kind == KIND_READ {
                return Err(SnapshotError::Corrupt("unknown entry kind"));
            }
            let dir = kind & KIND_DIR != 0;
            if i == 0 && !dir {
                return Err(SnapshotError::Corrupt("root is a file"));
            }
            let name_len = r.varint()?;
            if name_len > u64::from(u16::MAX) {
                return Err(SnapshotError::Corrupt("name too long"));
            }
            let name = r.take(name_len as usize)?;
            let logical = r.varint()?;
            snap.parent.push(parent);
            snap.kind.push(kind);
            snap.name_start.push(
                u32::try_from(snap.names.len())
                    .map_err(|_| SnapshotError::Corrupt("names too long"))?,
            );
            snap.name_len.push(name_len as u16);
            snap.names.extend_from_slice(name);
            snap.logical.push(logical);
            if dir {
                let files = u32::try_from(r.varint()?)
                    .map_err(|_| SnapshotError::Corrupt("file count too large"))?;
                let count = r.varint()?;
                if count > (r.bytes.len() / 4) as u64 {
                    return Err(SnapshotError::Truncated);
                }
                snap.files.push(files);
                snap.small_start.push(snap.small.len() as u32);
                snap.small_len.push(count as u32);
                for chunk in r.take(count as usize * 4)?.chunks_exact(4) {
                    snap.small
                        .push(u32::from_le_bytes(chunk.try_into().expect("4 bytes")));
                }
            } else {
                snap.files.push(1);
                snap.small_start.push(0);
                snap.small_len.push(0);
            }
        }
        if !r.bytes.is_empty() {
            return Err(SnapshotError::Corrupt("unexpected data after the entries"));
        }
        if snap.is_empty() {
            return Err(SnapshotError::Corrupt("no root"));
        }
        Ok(snap)
    }
}

/// Reads just the header from the start of a snapshot file.
pub fn read_header(reader: &mut impl Read) -> Result<SnapshotHeader, SnapshotError> {
    let mut prefix = [0u8; PREFIX];
    read_exact(reader, &mut prefix)?;
    let head_len = check_prefix(&prefix)?;
    let mut bytes = vec![0u8; PREFIX + head_len as usize];
    bytes[..PREFIX].copy_from_slice(&prefix);
    read_exact(reader, &mut bytes[PREFIX..])?;
    parse_header(&bytes).map(|(h, _)| h)
}

fn read_exact(reader: &mut impl Read, buf: &mut [u8]) -> Result<(), SnapshotError> {
    reader.read_exact(buf).map_err(|e| match e.kind() {
        std::io::ErrorKind::UnexpectedEof => SnapshotError::Truncated,
        _ => SnapshotError::Corrupt("unreadable"),
    })
}

/// Checks the magic and version; returns the header length.
fn check_prefix(prefix: &[u8; PREFIX]) -> Result<u32, SnapshotError> {
    if prefix[..8] != MAGIC {
        return Err(SnapshotError::NotASnapshot);
    }
    let version = u32::from_le_bytes(prefix[8..12].try_into().expect("4 bytes"));
    if version != VERSION {
        return Err(SnapshotError::UnsupportedVersion(version));
    }
    let head_len = u32::from_le_bytes(prefix[12..16].try_into().expect("4 bytes"));
    if head_len > MAX_HEADER {
        return Err(SnapshotError::Corrupt("header too long"));
    }
    Ok(head_len)
}

/// The header, and where the entries start.
fn parse_header(bytes: &[u8]) -> Result<(SnapshotHeader, usize), SnapshotError> {
    if bytes.len() < 8 {
        // Too short to tell; an empty file is most likely a failed write.
        return Err(if MAGIC.starts_with(bytes) {
            SnapshotError::Truncated
        } else {
            SnapshotError::NotASnapshot
        });
    }
    let prefix: &[u8; PREFIX] = bytes
        .get(..PREFIX)
        .ok_or_else(|| check_magic_only(bytes))?
        .try_into()
        .expect("16 bytes");
    let head_len = check_prefix(prefix)? as usize;
    let end = PREFIX + head_len;
    let mut r = Cursor {
        bytes: bytes.get(PREFIX..end).ok_or(SnapshotError::Truncated)?,
    };
    let created_ms = r.u64()?;
    let threshold = r.u64()?;
    let logical = r.u64()?;
    let files = r.u64()?;
    let entries = r.u32()?;
    let flags = r.u32()?;
    let root_len = r.u32()? as usize;
    let root = String::from_utf8_lossy(r.take(root_len)?).into_owned();
    Ok((
        SnapshotHeader {
            created_ms,
            threshold,
            logical,
            files,
            entries,
            case_insensitive: flags & FLAG_CASE_INSENSITIVE != 0,
            root,
        },
        end,
    ))
}

fn check_magic_only(bytes: &[u8]) -> SnapshotError {
    if bytes[..8] == MAGIC {
        SnapshotError::Truncated
    } else {
        SnapshotError::NotASnapshot
    }
}

struct Cursor<'a> {
    bytes: &'a [u8],
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], SnapshotError> {
        if n > self.bytes.len() {
            return Err(SnapshotError::Truncated);
        }
        let (head, rest) = self.bytes.split_at(n);
        self.bytes = rest;
        Ok(head)
    }

    fn byte(&mut self) -> Result<u8, SnapshotError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, SnapshotError> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("4 bytes"),
        ))
    }

    fn u64(&mut self) -> Result<u64, SnapshotError> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("8 bytes"),
        ))
    }

    fn varint(&mut self) -> Result<u64, SnapshotError> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let b = self.byte()?;
            let bits = u64::from(b & 0x7f);
            if shift == 63 && bits > 1 {
                return Err(SnapshotError::Corrupt("number too large"));
            }
            value |= bits << shift;
            if b & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(SnapshotError::Corrupt("number too long"))
    }
}

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn fnv64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// FNV-1a over a (folded) name, for the small-file sets.
pub fn name_hash(name: &[u8]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for &b in name {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// `name` as compared: unchanged when case matters, else upper-cased char
/// by char the way NTFS compares names: only characters in the Basic
/// Multilingual Plane, and only one-to-one mappings (so `ß` stays `ß`).
/// Bytes that aren't valid UTF-8, such as Windows names' unpaired
/// surrogates, are kept as they are. Uses `buf` only when the name changes.
pub fn fold<'a>(name: &'a [u8], case_insensitive: bool, buf: &'a mut Vec<u8>) -> &'a [u8] {
    if !case_insensitive || !name.iter().any(|b| b.is_ascii_lowercase() || *b >= 0x80) {
        return name;
    }
    buf.clear();
    if name.is_ascii() {
        buf.extend(name.iter().map(u8::to_ascii_uppercase));
        return buf;
    }
    let mut utf8 = [0u8; 4];
    for chunk in name.utf8_chunks() {
        for c in chunk.valid().chars() {
            buf.extend_from_slice(upcase(c).encode_utf8(&mut utf8).as_bytes());
        }
        buf.extend_from_slice(chunk.invalid());
    }
    buf
}

/// `c` upper-cased when that maps one BMP character to another.
fn upcase(c: char) -> char {
    if u32::from(c) > 0xffff {
        return c;
    }
    let mut upper = c.to_uppercase();
    match (upper.next(), upper.next()) {
        (Some(u), None) if u32::from(u) <= 0xffff => u,
        _ => c,
    }
}

/// A folder's kind bits: read once its listing succeeded.
fn dir_kind(tree: &Tree, dir: NodeId) -> u8 {
    if tree.dir_state(dir) == DirState::Listed {
        KIND_DIR | KIND_READ
    } else {
        KIND_DIR
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::size::Allocated;
    use crate::tree::FileSizes;

    fn sizes(logical: u64) -> FileSizes {
        FileSizes {
            logical,
            allocated: Allocated::known(logical),
        }
    }

    fn sample() -> Tree {
        let mut t = Tree::new("/data");
        let a = t.add_dir(NodeId::ROOT, "a".as_ref()).unwrap();
        let b = t.add_dir(a, "b".as_ref()).unwrap();
        t.add_file(a, "big.iso".as_ref(), sizes(5 << 20), 0)
            .unwrap();
        t.add_file(a, "note.txt".as_ref(), sizes(10), 0).unwrap();
        t.add_file(b, "ü.bin".as_ref(), sizes(2 << 20), 0).unwrap();
        t.add_file(NodeId::ROOT, "x".as_ref(), sizes(1), 0).unwrap();
        // `b` couldn't be read.
        t.set_dir_state(NodeId::ROOT, DirState::Listed);
        t.set_dir_state(a, DirState::Listed);
        t.set_dir_state(b, DirState::Failed);
        t
    }

    #[test]
    fn round_trips_through_bytes() {
        let tree = sample();
        let snap = Snapshot::capture(&tree, 1_700_000_000_000, false);
        // Root, a, b, big.iso, ü.bin: small files leave only a hash.
        assert_eq!(snap.len(), 5);
        assert_eq!(snap.header().logical, (7 << 20) + 11);
        assert_eq!(snap.header().files, 4);
        let bytes = snap.encode();
        let back = Snapshot::decode(&bytes).unwrap();
        assert_eq!(back, snap);
        assert_eq!(read_header(&mut &bytes[..]).unwrap(), *snap.header());

        let a = (0..back.len() as u32)
            .find(|&e| back.name(e) == b"a")
            .unwrap();
        assert!(back.is_dir(a));
        assert_eq!(back.file_count(a), 3);
        assert!(back.had_small_file(a, name_hash(b"note.txt")));
        assert!(!back.had_small_file(a, name_hash(b"big.iso")));
        assert!(back.had_small_file(0, name_hash(b"x")));
        assert!(back.was_read(0) && back.was_read(a));
        let b = (0..back.len() as u32)
            .find(|&e| back.name(e) == b"b")
            .unwrap();
        assert!(back.is_dir(b) && !back.was_read(b));
    }

    #[test]
    fn leaves_removed_nodes_out() {
        let mut tree = sample();
        let a = tree
            .children(NodeId::ROOT)
            .find(|&c| tree.is_dir(c))
            .unwrap();
        tree.remove(a);
        let snap = Snapshot::capture(&tree, 0, false);
        assert_eq!(snap.len(), 1);
        assert_eq!(snap.header().files, 1);
    }

    #[test]
    fn damaged_or_incomplete_files_are_errors() {
        let bytes = Snapshot::capture(&sample(), 0, false).encode();
        // Every truncation is an error, not a panic.
        for n in 0..bytes.len() {
            assert!(Snapshot::decode(&bytes[..n]).is_err(), "length {n}");
        }
        assert_eq!(Snapshot::decode(&[]), Err(SnapshotError::Truncated));
        assert_eq!(
            Snapshot::decode(&bytes[..40]),
            Err(SnapshotError::Truncated)
        );
        assert_eq!(
            Snapshot::decode(b"hello, world, not a snapshot"),
            Err(SnapshotError::NotASnapshot)
        );
        // Any flipped byte is caught (by the checksum at the latest).
        for i in 0..bytes.len() {
            let mut bad = bytes.clone();
            bad[i] ^= 0x5a;
            assert!(Snapshot::decode(&bad).is_err(), "byte {i}");
        }
        let mut newer = bytes.clone();
        newer[8] = 2;
        assert_eq!(
            Snapshot::decode(&newer),
            Err(SnapshotError::UnsupportedVersion(2))
        );
        assert_eq!(
            read_header(&mut &bytes[..20]),
            Err(SnapshotError::Truncated)
        );
    }

    #[test]
    fn rejects_well_formed_nonsense() {
        // A valid checksum over entries that don't make sense.
        let mut out = Vec::new();
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&(44u32).to_le_bytes());
        out.extend_from_slice(&[0; 32]);
        out.extend_from_slice(&u32::MAX.to_le_bytes()); // entries
        out.extend_from_slice(&[0; 8]); // flags, root length
        out.extend_from_slice(&[0, 1, 0, 0, 0, 0]); // root
        out.extend_from_slice(&[5, 0, 0, 0]); // parent 5 entries back
        let sum = fnv64(&out);
        out.extend_from_slice(&sum.to_le_bytes());
        assert_eq!(
            Snapshot::decode(&out),
            Err(SnapshotError::Corrupt("parent out of order"))
        );
    }

    #[test]
    fn folds_names_like_ntfs() {
        let mut buf = Vec::new();
        assert_eq!(fold(b"Readme.TXT", false, &mut buf), b"Readme.TXT");
        assert_eq!(fold(b"Readme.TXT", true, &mut buf), b"README.TXT");
        assert_eq!(fold("ärger".as_bytes(), true, &mut buf), "ÄRGER".as_bytes());
        assert_eq!(
            fold("straße".as_bytes(), true, &mut buf),
            "STRAßE".as_bytes()
        );
        assert_eq!(fold(b"ALREADY", true, &mut buf), b"ALREADY");
        // Outside the BMP, NTFS doesn't fold: Deseret small long I.
        assert_eq!(
            fold("\u{10428}".as_bytes(), true, &mut buf),
            "\u{10428}".as_bytes()
        );
        // Unpaired surrogates (WTF-8) stay distinct rather than becoming
        // one replacement character.
        let a = [b'x', 0xed, 0xa0, 0x80];
        let b = [b'x', 0xed, 0xa0, 0x81];
        let fa = fold(&a, true, &mut buf).to_vec();
        let fb = fold(&b, true, &mut buf).to_vec();
        assert_eq!(fa, [b'X', 0xed, 0xa0, 0x80]);
        assert_ne!(fa, fb);
    }

    #[test]
    fn varints_round_trip() {
        for v in [0, 1, 127, 128, 300, u64::from(u32::MAX), u64::MAX] {
            let mut out = Vec::new();
            put_varint(&mut out, v);
            assert_eq!(Cursor { bytes: &out }.varint(), Ok(v));
        }
        let eleven = [0xff; 11];
        assert!(Cursor { bytes: &eleven }.varint().is_err());
    }
}
