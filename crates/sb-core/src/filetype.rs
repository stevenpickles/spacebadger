//! File-type categories for the "file type" color mode, by extension.
//!
//! Only the name is used; contents are never read. Extensions are compared
//! case-insensitively. A name with no extension, or one not listed here, is
//! [`FileType::Other`]. The numeric values are sent in the layout wire
//! format and mirrored in `ui/src/lib/layoutWire.ts`.

use std::ffi::OsStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum FileType {
    Other = 0,
    Video = 1,
    Audio = 2,
    Image = 3,
    Document = 4,
    Archive = 5,
    /// Disk images and virtual machine disks.
    DiskImage = 6,
    /// Executables, libraries, and installers.
    Program = 7,
    /// Source code and structured text.
    Code = 8,
}

const TABLE: &[(FileType, &[&str])] = &[
    (
        FileType::Video,
        &[
            "3gp", "avi", "flv", "m2ts", "m4v", "mkv", "mov", "mp4", "mpeg", "mpg", "mts", "vob",
            "webm", "wmv",
        ],
    ),
    (
        FileType::Audio,
        &[
            "aac", "aif", "aiff", "alac", "flac", "m4a", "mid", "midi", "mp3", "ogg", "opus",
            "wav", "wma",
        ],
    ),
    (
        FileType::Image,
        &[
            "arw", "avif", "bmp", "cr2", "cr3", "dng", "gif", "heic", "heif", "ico", "jpeg", "jpg",
            "nef", "png", "psd", "raw", "svg", "tif", "tiff", "webp",
        ],
    ),
    (
        FileType::Document,
        &[
            "csv", "doc", "docx", "epub", "key", "md", "numbers", "odp", "ods", "odt", "pages",
            "pdf", "ppt", "pptx", "rtf", "txt", "xls", "xlsx",
        ],
    ),
    (
        FileType::Archive,
        &[
            "7z", "bz2", "cab", "gz", "lz4", "lzma", "rar", "tar", "tbz2", "tgz", "txz", "xz",
            "zip", "zst",
        ],
    ),
    (
        FileType::DiskImage,
        &[
            "dmg", "esd", "img", "iso", "ova", "qcow2", "vdi", "vhd", "vhdx", "vmdk", "wim",
        ],
    ),
    (
        FileType::Program,
        &[
            "a", "apk", "app", "appimage", "deb", "dll", "dylib", "exe", "jar", "lib", "msi",
            "msix", "node", "o", "obj", "pdb", "pkg", "rlib", "rpm", "so", "sys",
        ],
    ),
    (
        FileType::Code,
        &[
            "c", "cc", "cpp", "cs", "css", "go", "h", "hpp", "html", "java", "js", "json", "kt",
            "lua", "php", "ps1", "py", "rb", "rs", "sh", "sql", "svelte", "swift", "toml", "ts",
            "tsx", "xml", "yaml", "yml",
        ],
    ),
];

/// Longest extension in [`TABLE`].
const MAX_EXT: usize = 8;

impl FileType {
    pub fn of(name: &OsStr) -> Self {
        let bytes = name.as_encoded_bytes();
        let Some(dot) = bytes.iter().rposition(|&b| b == b'.') else {
            return Self::Other;
        };
        let ext = &bytes[dot + 1..];
        // A leading dot (".bashrc") marks a hidden file, not an extension.
        if dot == 0 || ext.is_empty() || ext.len() > MAX_EXT || !ext.is_ascii() {
            return Self::Other;
        }
        let mut lower = [0u8; MAX_EXT];
        let lower = &mut lower[..ext.len()];
        lower.copy_from_slice(ext);
        lower.make_ascii_lowercase();
        TABLE
            .iter()
            .find(|(_, exts)| exts.iter().any(|e| e.as_bytes() == lower))
            .map_or(Self::Other, |&(t, _)| t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn of(name: &str) -> FileType {
        FileType::of(OsStr::new(name))
    }

    #[test]
    fn classifies_by_last_extension_ignoring_case() {
        assert_eq!(of("Holiday.MP4"), FileType::Video);
        assert_eq!(of("song.flac"), FileType::Audio);
        assert_eq!(of("scan.tar.gz"), FileType::Archive);
        assert_eq!(of("report.final.pdf"), FileType::Document);
        assert_eq!(of("ubuntu.iso"), FileType::DiskImage);
        assert_eq!(of("chrome.dll"), FileType::Program);
        assert_eq!(of("main.rs"), FileType::Code);
        assert_eq!(of("photo.HEIC"), FileType::Image);
    }

    #[test]
    fn names_without_a_known_extension_are_other() {
        assert_eq!(of("Makefile"), FileType::Other);
        assert_eq!(of(".bashrc"), FileType::Other);
        assert_eq!(of("trailing."), FileType::Other);
        assert_eq!(of("data.unknownext"), FileType::Other);
        assert_eq!(of("naïve.ñ"), FileType::Other);
    }

    #[test]
    fn table_has_no_duplicates_or_overlong_entries() {
        let mut seen = std::collections::HashSet::new();
        for (_, exts) in TABLE {
            for e in *exts {
                assert!(e.len() <= MAX_EXT && *e == e.to_ascii_lowercase(), "{e}");
                assert!(seen.insert(*e), "duplicate extension {e}");
            }
        }
    }
}
