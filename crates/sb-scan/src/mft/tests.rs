//! Tests with hand-built MFT records.

use super::record::*;
use super::*;

const SIZE: usize = 1024;
const USN: u16 = 0x0B0B;

/// Builds a 1 KiB NTFS 3.1 file record attribute by attribute.
struct Builder {
    bytes: Vec<u8>,
    at: usize,
    next_id: u16,
}

fn utf16le(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

fn pad8(n: usize) -> usize {
    n.div_ceil(8) * 8
}

impl Builder {
    fn new(number: u32, sequence: u16) -> Self {
        let mut bytes = vec![0u8; SIZE];
        bytes[0..4].copy_from_slice(b"FILE");
        bytes[4..6].copy_from_slice(&0x30u16.to_le_bytes());
        bytes[6..8].copy_from_slice(&3u16.to_le_bytes());
        bytes[16..18].copy_from_slice(&sequence.to_le_bytes());
        bytes[18..20].copy_from_slice(&1u16.to_le_bytes());
        bytes[20..22].copy_from_slice(&0x38u16.to_le_bytes());
        bytes[22..24].copy_from_slice(&1u16.to_le_bytes());
        bytes[28..32].copy_from_slice(&(SIZE as u32).to_le_bytes());
        bytes[44..48].copy_from_slice(&number.to_le_bytes());
        Self {
            bytes,
            at: 0x38,
            next_id: 0,
        }
    }

    fn not_in_use(mut self) -> Self {
        self.bytes[22] &= !1;
        self
    }

    fn push(&mut self, attr: Vec<u8>) {
        self.bytes[self.at..self.at + attr.len()].copy_from_slice(&attr);
        self.at += attr.len();
    }

    fn resident(&mut self, type_code: u32, name: &str, value: &[u8]) {
        let name = utf16le(name);
        let value_at = pad8(24 + name.len());
        let len = pad8(value_at + value.len());
        let mut a = vec![0u8; len];
        a[0..4].copy_from_slice(&type_code.to_le_bytes());
        a[4..8].copy_from_slice(&(len as u32).to_le_bytes());
        a[9] = (name.len() / 2) as u8;
        a[10..12].copy_from_slice(&24u16.to_le_bytes());
        a[14..16].copy_from_slice(&self.next_id.to_le_bytes());
        a[16..20].copy_from_slice(&(value.len() as u32).to_le_bytes());
        a[20..22].copy_from_slice(&(value_at as u16).to_le_bytes());
        a[24..24 + name.len()].copy_from_slice(&name);
        a[value_at..value_at + value.len()].copy_from_slice(value);
        self.next_id += 1;
        self.push(a);
    }

    #[allow(clippy::too_many_arguments)]
    fn non_resident(
        &mut self,
        type_code: u32,
        name: &str,
        flags: u16,
        lowest_vcn: u64,
        runs: &[u8],
        allocated: u64,
        logical: u64,
        total: u64,
    ) {
        let name = utf16le(name);
        let header = if flags & 0x8001 != 0 { 72 } else { 64 };
        let runs_at = pad8(header + name.len());
        let len = pad8(runs_at + runs.len() + 1);
        let mut a = vec![0u8; len];
        a[0..4].copy_from_slice(&type_code.to_le_bytes());
        a[4..8].copy_from_slice(&(len as u32).to_le_bytes());
        a[8] = 1;
        a[9] = (name.len() / 2) as u8;
        a[10..12].copy_from_slice(&(header as u16).to_le_bytes());
        a[12..14].copy_from_slice(&flags.to_le_bytes());
        a[14..16].copy_from_slice(&self.next_id.to_le_bytes());
        a[16..24].copy_from_slice(&lowest_vcn.to_le_bytes());
        a[32..34].copy_from_slice(&(runs_at as u16).to_le_bytes());
        a[40..48].copy_from_slice(&allocated.to_le_bytes());
        a[48..56].copy_from_slice(&logical.to_le_bytes());
        a[56..64].copy_from_slice(&logical.to_le_bytes());
        if header == 72 {
            a[64..72].copy_from_slice(&total.to_le_bytes());
        }
        a[header..header + name.len()].copy_from_slice(&name);
        a[runs_at..runs_at + runs.len()].copy_from_slice(runs);
        self.next_id += 1;
        self.push(a);
    }

    fn standard(mut self, attributes: u32, usn: u64) -> Self {
        let mut v = vec![0u8; 72];
        v[8..16].copy_from_slice(&123u64.to_le_bytes());
        v[32..36].copy_from_slice(&attributes.to_le_bytes());
        v[64..72].copy_from_slice(&usn.to_le_bytes());
        self.resident(ATTR_STANDARD_INFORMATION, "", &v);
        self
    }

    fn name(mut self, parent: u64, name: &str, namespace: u8) -> Self {
        let text = utf16le(name);
        let mut v = vec![0u8; 66 + text.len()];
        v[0..8].copy_from_slice(&parent.to_le_bytes());
        v[64] = (text.len() / 2) as u8;
        v[65] = namespace;
        v[66..].copy_from_slice(&text);
        self.resident(ATTR_FILE_NAME, "", &v);
        self
    }

    fn data(mut self, len: usize) -> Self {
        self.resident(ATTR_DATA, "", &vec![b'x'; len]);
        self
    }

    fn named_data(mut self, name: &str, len: usize) -> Self {
        self.resident(ATTR_DATA, name, &vec![b'y'; len]);
        self
    }

    fn big_data(mut self, flags: u16, allocated: u64, logical: u64, total: u64) -> Self {
        self.non_resident(
            ATTR_DATA,
            "",
            flags,
            0,
            &[0x11, 0x01, 0x20],
            allocated,
            logical,
            total,
        );
        self
    }

    fn reparse(mut self, tag: u32) -> Self {
        let mut v = vec![0u8; 8];
        v[0..4].copy_from_slice(&tag.to_le_bytes());
        self.resident(ATTR_REPARSE_POINT, "", &v);
        self
    }

    fn attribute_list(mut self, entries: &[(u32, u64)]) -> Self {
        let mut v = Vec::new();
        for &(type_code, segment) in entries {
            let mut e = vec![0u8; 32];
            e[0..4].copy_from_slice(&type_code.to_le_bytes());
            e[4..6].copy_from_slice(&32u16.to_le_bytes());
            e[7] = 26;
            e[16..24].copy_from_slice(&segment.to_le_bytes());
            v.extend(e);
        }
        self.resident(ATTR_ATTRIBUTE_LIST, "", &v);
        self
    }

    /// The finished record as stored on disk: end marker written and the
    /// last two bytes of every 512-byte stride replaced by the check value.
    fn protected(mut self) -> Vec<u8> {
        let end = self.at;
        self.bytes[end..end + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        self.bytes[24..28].copy_from_slice(&((end + 8) as u32).to_le_bytes());
        let usa = 0x30;
        self.bytes[usa..usa + 2].copy_from_slice(&USN.to_le_bytes());
        for i in 1..3 {
            let tail = i * 512 - 2;
            let saved = [self.bytes[tail], self.bytes[tail + 1]];
            self.bytes[usa + 2 * i..usa + 2 * i + 2].copy_from_slice(&saved);
            self.bytes[tail..tail + 2].copy_from_slice(&USN.to_le_bytes());
        }
        self.bytes
    }

    /// Protected, then fixed up and parsed as record `number`.
    fn parse(self, number: u64) -> Parsed {
        let mut bytes = self.protected();
        apply_fixups(&mut bytes).unwrap();
        parse_record(&bytes, number).unwrap().unwrap()
    }
}

fn frn(number: u64, sequence: u16) -> u64 {
    (u64::from(sequence) << 48) | number
}

const ROOT_REF: u64 = (5 << 48) | 5;

#[test]
fn fixups_restore_the_stride_tails_and_catch_torn_records() {
    // A value that spans the first stride's tail survives the round trip.
    let b = Builder::new(40, 1).standard(0x20, 0).name(ROOT_REF, "a", 1);
    let mut b = b;
    b.resident(ATTR_DATA, "", &vec![0xAB; 600]);
    let mut bytes = b.protected();
    assert_eq!(bytes[510..512], USN.to_le_bytes());
    let mut torn = bytes.clone();
    apply_fixups(&mut bytes).unwrap();
    assert_eq!(bytes[510..512], [0xAB, 0xAB]);
    let parsed = parse_record(&bytes, 40).unwrap().unwrap();
    assert_eq!(parsed.data.unwrap().logical, 600);

    // The second stride was rewritten mid-read: its tail no longer matches.
    torn[1022] ^= 0xFF;
    assert_eq!(apply_fixups(&mut torn), Err(RecordError::Torn));

    // Already applied (as FSCTL_GET_NTFS_FILE_RECORD may return it): kept.
    let before = bytes.clone();
    apply_fixups_if_protected(&mut bytes).unwrap();
    assert_eq!(bytes, before);

    // An update sequence array that doesn't fit is malformed.
    let mut bad = Builder::new(40, 1).protected();
    bad[6..8].copy_from_slice(&9u16.to_le_bytes());
    assert_eq!(apply_fixups(&mut bad), Err(RecordError::Malformed));
}

#[test]
fn unused_bad_and_misplaced_records() {
    let mut zero = vec![0u8; SIZE];
    assert_eq!(parse_record(&zero, 7), Ok(None));
    zero[0..4].copy_from_slice(b"BAAD");
    assert_eq!(parse_record(&zero, 7), Err(RecordError::BadSignature));
    let mut free = Builder::new(7, 1).not_in_use().protected();
    apply_fixups(&mut free).unwrap();
    assert_eq!(parse_record(&free, 7), Ok(None));
    let mut moved = Builder::new(8, 1).protected();
    apply_fixups(&mut moved).unwrap();
    assert_eq!(parse_record(&moved, 7), Err(RecordError::WrongNumber));
}

#[test]
fn resident_data_is_rounded_to_eight_as_listings_report() {
    let p = Builder::new(30, 2)
        .standard(0x20, 99)
        .name(ROOT_REF, "tiny.txt", 1)
        .data(100)
        .parse(30);
    assert_eq!(p.reference(), frn(30, 2));
    assert_eq!(p.standard.unwrap().usn, 99);
    assert_eq!(p.standard.unwrap().attributes, 0x20);
    let data = p.data.unwrap();
    assert_eq!(
        (data.logical, data.allocated, data.resident),
        (100, 104, true)
    );
    let empty = Builder::new(31, 1).data(0).parse(31).data.unwrap();
    assert_eq!((empty.logical, empty.allocated), (0, 0));
}

#[test]
fn non_resident_data_uses_total_allocated_when_sparse_or_compressed() {
    let plain = Builder::new(32, 1).big_data(0, 1_052_672, 1_048_577, 0);
    let plain = plain.parse(32).data.unwrap();
    assert_eq!((plain.logical, plain.allocated), (1_048_577, 1_052_672));
    assert!(!plain.resident);
    // The fixture values: a 1 GiB sparse file holding 64 KiB, and a
    // 16 MiB compressed text file.
    let sparse = Builder::new(33, 1).big_data(0x8000, 1 << 30, 1 << 30, 65_536);
    let sparse = sparse.parse(33).data.unwrap();
    assert_eq!((sparse.logical, sparse.allocated), (1 << 30, 65_536));
    let compressed = Builder::new(34, 1).big_data(0x0001, 16_777_216, 16_777_216, 2_101_248);
    let compressed = compressed.parse(34).data.unwrap();
    assert_eq!(compressed.allocated, 2_101_248);
}

#[test]
fn later_pieces_of_a_split_data_attribute_carry_no_sizes() {
    let mut b = Builder::new(35, 1);
    b.non_resident(ATTR_DATA, "", 0, 0x100, &[0x11, 0x01, 0x20], 0, 0, 0);
    assert_eq!(b.parse(35).data, None);
}

#[test]
fn decodes_run_lists_with_negative_offsets_and_sparse_runs() {
    let bytes = [
        0x21, 0x18, 0x34, 0x56, // 0x18 clusters at 0x5634
        0x11, 0x08, 0xF0, // 8 clusters at -0x10 relative
        0x01, 0x10, // 16 sparse clusters
        0x31, 0x02, 0x00, 0x00, 0x01, // 2 clusters at +0x10000
        0x00,
    ];
    let runs = decode_runs(&bytes).unwrap();
    assert_eq!(
        runs,
        [
            Run {
                lcn: Some(0x5634),
                clusters: 0x18
            },
            Run {
                lcn: Some(0x5624),
                clusters: 8
            },
            Run {
                lcn: None,
                clusters: 16
            },
            Run {
                lcn: Some(0x15624),
                clusters: 2
            },
        ]
    );
    assert_eq!(decode_runs(&[0x00]).unwrap(), []);
    assert_eq!(decode_runs(&[0x11, 0x01]), Err(RecordError::BadRunList));
    assert_eq!(
        decode_runs(&[0x10, 0x01, 0x00]),
        Err(RecordError::BadRunList)
    );
    assert_eq!(
        decode_runs(&[0x11, 0x01, 0xFF, 0x00]),
        Err(RecordError::BadRunList),
        "an offset before cluster 0"
    );
    assert_eq!(
        decode_runs(&[0x11, 0x01, 0x05]),
        Err(RecordError::BadRunList)
    );

    let ext = extents(vec![
        (0x30, vec![runs[3]]),
        (0, vec![runs[0], runs[1], runs[2]]),
    ])
    .unwrap();
    assert_eq!(ext.len(), 4);
    assert_eq!((ext[3].vcn, ext[3].lcn), (0x30, Some(0x15624)));
    assert_eq!(volume_offset(&ext, 4096, 0), Some(0x5634 * 4096));
    assert_eq!(
        volume_offset(&ext, 4096, 0x19 * 4096 + 5),
        Some(0x5625 * 4096 + 5)
    );
    assert_eq!(volume_offset(&ext, 4096, 0x20 * 4096), None, "sparse");
    assert_eq!(volume_offset(&ext, 4096, 0x32 * 4096), None, "past the end");
    assert_eq!(
        extents(vec![(0, vec![runs[0]]), (0x40, vec![runs[1]])]),
        Err(RecordError::BadRunList),
        "a gap between pieces"
    );
}

#[test]
fn reads_the_mft_runs_and_attribute_list_from_record_zero() {
    let mut b = Builder::new(0, 1).standard(0x6, 0);
    b.non_resident(
        ATTR_DATA,
        "",
        0,
        0,
        &[0x21, 0x40, 0x00, 0x0C, 0x00],
        0x40 * 4096,
        0x40 * 4096,
        0,
    );
    let mut bytes = b
        .attribute_list(&[(ATTR_DATA, frn(0, 1)), (ATTR_DATA, frn(17, 1))])
        .protected();
    apply_fixups(&mut bytes).unwrap();
    let pieces = data_runs(&bytes).unwrap();
    assert_eq!(
        pieces,
        [(
            0,
            vec![Run {
                lcn: Some(0xC00),
                clusters: 0x40
            }]
        )]
    );
    assert_eq!(data_stream(&bytes).unwrap().unwrap().logical, 0x40 * 4096);
    let Some(ListLocation::Resident(list)) = attribute_list_location(&bytes).unwrap() else {
        panic!("resident list expected");
    };
    assert_eq!(list[1].segment, frn(17, 1));
    assert_eq!(list[1].type_code, ATTR_DATA);
}

#[test]
fn keeps_every_name_but_dos_aliases() {
    let p = Builder::new(36, 1)
        .standard(0x20, 0)
        .name(ROOT_REF, "LONGNA~1.TXT", NAMESPACE_DOS)
        .name(ROOT_REF, "long name.txt", 1)
        .name(frn(40, 3), "Other", 0)
        .name(frn(41, 1), "Both", 3)
        .parse(36);
    let names: Vec<_> = p
        .names
        .iter()
        .map(|n| (n.parent, String::from_utf16_lossy(&n.name)))
        .collect();
    assert_eq!(
        names,
        [
            (ROOT_REF, "long name.txt".to_owned()),
            (frn(40, 3), "Other".to_owned()),
            (frn(41, 1), "Both".to_owned()),
        ]
    );
    assert_eq!(p.dos_names, 1);
}

#[test]
fn reads_reparse_tags_wof_and_alternate_streams() {
    let link = Builder::new(37, 1)
        .standard(0x400, 0)
        .reparse(tag::SYMLINK)
        .parse(37);
    assert_eq!(link.reparse_tag, Some(tag::SYMLINK));
    let wof = Builder::new(38, 1)
        .standard(0x400 | 0x200, 0)
        .reparse(tag::WOF)
        .big_data(0x8000, 8192, 8000, 0)
        .named_data("WofCompressedData", 300)
        .named_data("extra", 20)
        .parse(38);
    assert_eq!(wof.data.unwrap().allocated, 0);
    assert_eq!(wof.wof.unwrap().allocated, 304);
    assert_eq!(wof.streams_allocated, 24);
}

#[test]
fn classifies_like_the_listing_adapter() {
    // The same table as the Windows adapter's own test.
    let rp = attr::REPARSE_POINT;
    let dir = attr::DIRECTORY;
    assert_eq!(classify(0x20, 0), EntryKind::File);
    assert_eq!(classify(dir, 0), EntryKind::Directory);
    assert_eq!(classify(rp, tag::SYMLINK), EntryKind::Symlink);
    assert_eq!(classify(dir | rp, tag::SYMLINK), EntryKind::Symlink);
    assert_eq!(classify(dir | rp, tag::MOUNT_POINT), EntryKind::MountPoint);
    assert_eq!(classify(rp, 0x9000_301A), EntryKind::File, "cloud file");
    assert_eq!(classify(dir | rp, 0x9000_001A), EntryKind::Directory);
    assert_eq!(classify(rp, 0x8000_0013), EntryKind::File, "dedup file");
    assert_eq!(classify(dir | rp, 0x8000_0013), EntryKind::UnknownReparse);
    assert_eq!(classify(rp, 0xA000_0019), EntryKind::UnknownReparse);
    assert_eq!(classify(rp, tag::AF_UNIX), EntryKind::Special);
    assert_eq!(classify(attr::DEVICE, 0), EntryKind::Special);
    assert!(is_cloud(attr::RECALL_ON_DATA_ACCESS, 0));
    assert!(is_cloud(rp, 0x9000_101A));
    assert!(!is_cloud(0, 0x9000_101A), "a tag without the attribute");
    assert!(!is_cloud(0x20, 0));
}
