//! Binary treemap layout reply, decoded by `ui/src/lib/layoutWire.ts`.
//!
//! JSON for tens of thousands of rectangles is slow to build and parse, so
//! the layout command returns raw bytes. All values are little-endian.
//!
//! ```text
//! header (56 bytes)
//!   0  u32  magic "SBL1"
//!   4  u16  format version
//!   6  u16  header length (56)
//!   8  u64  scan generation
//!  16  u64  tree revision the layout was computed from
//!  24  u32  request number (echoed from LayoutRequest)
//!  28  u32  view node
//!  32  u64  view weight in the requested metric
//!  40  u32  rectangle count
//!  44  u32  label count
//!  48  u32  flags (LAYOUT_TRUNCATED, LAYOUT_FINAL)
//!  52  u32  reserved
//! rectangles (40 bytes each), parent before child
//!   0  f32 x   4 f32 y   8 f32 w   12 f32 h   (CSS pixels)
//!  16  u32  node (for "other small items": the folder)
//!  20  u32  count (merged items for "other small items", else 1)
//!  24  u64  weight
//!  32  u8   depth (1 = child of the view)
//!  33  u8   kind (0 file, 1 folder, 2 other small items)
//!  34  u8   flags (see sb_core::layout::rect_flags)
//!  35  u8   file type for files (see sb_core::filetype), else 0
//!  36  u32  parent: the folder whose contents the rectangle is part of
//! labels, for rectangles large enough to show text
//!   u32 rectangle index, u16 byte length, UTF-8 bytes
//! ```

pub const LAYOUT_MAGIC: [u8; 4] = *b"SBL1";
pub const LAYOUT_FORMAT_VERSION: u16 = 3;
pub const HEADER_LEN: usize = 56;
pub const RECT_LEN: usize = 40;

/// More rectangles would have been drawn with a larger budget.
pub const LAYOUT_TRUNCATED: u32 = 1 << 0;
/// The scan had finished when the layout was computed.
pub const LAYOUT_FINAL: u32 = 1 << 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub generation: u64,
    pub revision: u64,
    pub request: u32,
    pub view: u32,
    pub total: u64,
    pub flags: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub node: u32,
    pub count: u32,
    pub weight: u64,
    pub depth: u8,
    pub kind: u8,
    pub flags: u8,
    /// `sb_core::filetype::FileType` for files, else 0.
    pub file_type: u8,
    /// The folder whose contents this rectangle is part of; for "other small
    /// items", the same as `node`.
    pub parent: u32,
}

/// Encodes a layout. `labels` pairs a rectangle index with its text.
pub fn encode(header: &Header, rects: &[Rect], labels: &[(u32, &str)]) -> Vec<u8> {
    let label_bytes: usize = labels
        .iter()
        .map(|(_, s)| 6 + s.len().min(u16::MAX as usize))
        .sum();
    let mut out = Vec::with_capacity(HEADER_LEN + rects.len() * RECT_LEN + label_bytes);
    out.extend_from_slice(&LAYOUT_MAGIC);
    out.extend_from_slice(&LAYOUT_FORMAT_VERSION.to_le_bytes());
    out.extend_from_slice(&(HEADER_LEN as u16).to_le_bytes());
    out.extend_from_slice(&header.generation.to_le_bytes());
    out.extend_from_slice(&header.revision.to_le_bytes());
    out.extend_from_slice(&header.request.to_le_bytes());
    out.extend_from_slice(&header.view.to_le_bytes());
    out.extend_from_slice(&header.total.to_le_bytes());
    out.extend_from_slice(&(rects.len() as u32).to_le_bytes());
    out.extend_from_slice(&(labels.len() as u32).to_le_bytes());
    out.extend_from_slice(&header.flags.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    debug_assert_eq!(out.len(), HEADER_LEN);
    for r in rects {
        for v in [r.x, r.y, r.w, r.h] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&r.node.to_le_bytes());
        out.extend_from_slice(&r.count.to_le_bytes());
        out.extend_from_slice(&r.weight.to_le_bytes());
        out.extend_from_slice(&[r.depth, r.kind, r.flags, r.file_type]);
        out.extend_from_slice(&r.parent.to_le_bytes());
    }
    for &(index, text) in labels {
        let bytes = truncate_utf8(text, u16::MAX as usize);
        out.extend_from_slice(&index.to_le_bytes());
        out.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
        out.extend_from_slice(bytes);
    }
    out
}

fn truncate_utf8(text: &str, max: usize) -> &[u8] {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text.as_bytes()[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_is_little_endian_with_fixed_offsets() {
        let header = Header {
            generation: 7,
            revision: 0x0102_0304_0506_0708,
            request: 42,
            view: 5,
            total: 1 << 40,
            flags: LAYOUT_FINAL,
        };
        let rect = Rect {
            x: 1.5,
            y: 2.0,
            w: 3.0,
            h: 4.0,
            node: 9,
            count: 1,
            weight: 1000,
            depth: 1,
            kind: 1,
            flags: 3,
            file_type: 4,
            parent: 0x0A0B_0C0D,
        };
        let bytes = encode(&header, &[rect], &[(0, "naïve.txt")]);
        assert_eq!(&bytes[0..4], b"SBL1");
        assert_eq!(
            u16::from_le_bytes([bytes[6], bytes[7]]) as usize,
            HEADER_LEN
        );
        assert_eq!(bytes[8], 7);
        assert_eq!(bytes[16], 0x08);
        assert_eq!(bytes[24], 42);
        assert_eq!(bytes[40], 1, "rect count");
        assert_eq!(bytes[44], 1, "label count");
        assert_eq!(bytes[48], LAYOUT_FINAL as u8);
        let r = &bytes[HEADER_LEN..HEADER_LEN + RECT_LEN];
        assert_eq!(f32::from_le_bytes(r[0..4].try_into().unwrap()), 1.5);
        assert_eq!(r[16], 9);
        assert_eq!(u64::from_le_bytes(r[24..32].try_into().unwrap()), 1000);
        assert_eq!(&r[32..36], &[1, 1, 3, 4]);
        assert_eq!(&r[36..40], &[0x0D, 0x0C, 0x0B, 0x0A]);
        let l = &bytes[HEADER_LEN + RECT_LEN..];
        assert_eq!(l[0], 0);
        let len = u16::from_le_bytes([l[4], l[5]]) as usize;
        assert_eq!(std::str::from_utf8(&l[6..6 + len]).unwrap(), "naïve.txt");
        assert_eq!(bytes.len(), HEADER_LEN + RECT_LEN + 6 + len);
    }

    #[test]
    fn long_labels_are_cut_on_a_character_boundary() {
        let text = "é".repeat(40_000);
        let bytes = truncate_utf8(&text, u16::MAX as usize);
        assert!(std::str::from_utf8(bytes).is_ok());
        assert!(bytes.len() <= u16::MAX as usize);
    }
}
