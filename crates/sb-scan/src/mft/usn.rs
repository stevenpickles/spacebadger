//! USN journal records as `FSCTL_READ_USN_JOURNAL` returns them: the next
//! USN, then `USN_RECORD_V2` or `V3` records.

use super::record::{u16_at, u32_at, u64_at};

/// One journal record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsnRecord {
    /// File reference (low 64 bits of a V3 128-bit ID).
    pub file: u64,
    pub parent: u64,
    pub usn: u64,
    /// `FILETIME` ticks.
    pub timestamp: u64,
    pub reason: u32,
    pub name: Vec<u16>,
}

/// Splits one `FSCTL_READ_USN_JOURNAL` output buffer into the USN to
/// continue from and its records. Records of other versions are skipped;
/// a malformed length ends the buffer.
pub fn parse_journal(buf: &[u8]) -> (u64, Vec<UsnRecord>) {
    let next = u64_at(buf, 0).unwrap_or(0);
    let mut records = Vec::new();
    let mut at = 8;
    while let Some(len) = u32_at(buf, at) {
        let len = len as usize;
        if len < 8 || at + len > buf.len() {
            break;
        }
        let r = &buf[at..at + len];
        let parsed = match u16_at(r, 4) {
            Some(2) => (|| {
                Some(UsnRecord {
                    file: u64_at(r, 8)?,
                    parent: u64_at(r, 16)?,
                    usn: u64_at(r, 24)?,
                    timestamp: u64_at(r, 32)?,
                    reason: u32_at(r, 40)?,
                    name: name(r, 56)?,
                })
            })(),
            Some(3) => (|| {
                Some(UsnRecord {
                    file: u64_at(r, 8)?,
                    parent: u64_at(r, 24)?,
                    usn: u64_at(r, 40)?,
                    timestamp: u64_at(r, 48)?,
                    reason: u32_at(r, 56)?,
                    name: name(r, 72)?,
                })
            })(),
            _ => None,
        };
        records.extend(parsed);
        at += len;
    }
    (next, records)
}

/// The name whose length and offset are at `at` and `at + 2`.
fn name(r: &[u8], at: usize) -> Option<Vec<u16>> {
    let len = usize::from(u16_at(r, at)?);
    let offset = usize::from(u16_at(r, at + 2)?);
    let bytes = r.get(offset..offset + len)?;
    Some(
        bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v2(file: u64, usn: u64, name: &str) -> Vec<u8> {
        let name: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let len = (60 + name.len()).div_ceil(8) * 8;
        let mut r = vec![0u8; len];
        r[0..4].copy_from_slice(&(len as u32).to_le_bytes());
        r[4..6].copy_from_slice(&2u16.to_le_bytes());
        r[8..16].copy_from_slice(&file.to_le_bytes());
        r[16..24].copy_from_slice(&5u64.to_le_bytes());
        r[24..32].copy_from_slice(&usn.to_le_bytes());
        r[32..40].copy_from_slice(&7u64.to_le_bytes());
        r[40..44].copy_from_slice(&0x100u32.to_le_bytes());
        r[56..58].copy_from_slice(&(name.len() as u16).to_le_bytes());
        r[58..60].copy_from_slice(&60u16.to_le_bytes());
        r[60..60 + name.len()].copy_from_slice(&name);
        r
    }

    fn v3(file: u64, usn: u64) -> Vec<u8> {
        let mut r = vec![0u8; 80];
        r[0..4].copy_from_slice(&80u32.to_le_bytes());
        r[4..6].copy_from_slice(&3u16.to_le_bytes());
        r[8..16].copy_from_slice(&file.to_le_bytes());
        r[24..32].copy_from_slice(&5u64.to_le_bytes());
        r[40..48].copy_from_slice(&usn.to_le_bytes());
        r[56..60].copy_from_slice(&2u32.to_le_bytes());
        r[72..74].copy_from_slice(&2u16.to_le_bytes());
        r[74..76].copy_from_slice(&76u16.to_le_bytes());
        r[76..78].copy_from_slice(&u16::from(b'x').to_le_bytes());
        r
    }

    #[test]
    fn reads_v2_and_v3_records_and_the_next_usn() {
        let mut buf = 900u64.to_le_bytes().to_vec();
        buf.extend(v2(0x0001_0000_0000_0042, 100, "a.txt"));
        buf.extend(v3(0x43, 200));
        let mut odd = v2(1, 1, "");
        odd[4] = 9; // an unknown version is skipped
        buf.extend(odd);
        let (next, records) = parse_journal(&buf);
        assert_eq!(next, 900);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].file, 0x0001_0000_0000_0042);
        assert_eq!((records[0].usn, records[0].timestamp), (100, 7));
        assert_eq!(records[0].reason, 0x100);
        assert_eq!(String::from_utf16_lossy(&records[0].name), "a.txt");
        assert_eq!((records[1].file, records[1].parent), (0x43, 5));
        assert_eq!((records[1].usn, records[1].reason), (200, 2));
        assert_eq!(records[1].name, [u16::from(b'x')]);
    }

    #[test]
    fn a_bad_length_ends_the_buffer() {
        let mut buf = 1u64.to_le_bytes().to_vec();
        buf.extend(v2(5, 10, "ok"));
        buf.extend([0xFF, 0xFF, 0, 0]);
        let (_, records) = parse_journal(&buf);
        assert_eq!(records.len(), 1);
        assert_eq!(parse_journal(&[]).1, []);
    }
}
