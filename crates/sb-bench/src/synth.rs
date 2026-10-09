//! Deterministic synthetic scan trees.
//!
//! Trees are built folder by folder, as the scanner does: a folder's whole
//! listing is added at once, and its subfolders are listed later in
//! breadth-first order. Shapes follow what real disks look like in the
//! milestone 2 measurements: about six files per folder on average with a
//! long tail of very large folders, sizes spanning bytes to tens of GiB with
//! a heavy tail, a mix of common extensions, and some non-ASCII names.

use sb_core::size::Allocated;
use sb_core::tree::{DirState, FileSizes, NodeId, Tree};
use std::collections::VecDeque;
use std::ffi::OsString;

/// xorshift64* — small, fast, and reproducible across platforms.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in (0, 1].
    pub fn unit(&mut self) -> f64 {
        ((self.next_u64() >> 11) + 1) as f64 / (1u64 << 53) as f64
    }

    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n.max(1)
    }

    /// Pareto-distributed value with minimum `min` and tail index `alpha`.
    pub fn pareto(&mut self, min: f64, alpha: f64) -> f64 {
        min / self.unit().powf(1.0 / alpha)
    }
}

const SYLLABLES: &[&str] = &[
    "ba", "ko", "ri", "net", "ser", "data", "img", "log", "app", "lib", "core", "min", "max",
    "doc", "test", "cache", "temp", "user", "proj", "src", "build", "re", "lo", "ta", "vi", "mo",
];

const EXTENSIONS: &[(&str, u32)] = &[
    ("", 6),
    ("dll", 6),
    ("exe", 2),
    ("txt", 6),
    ("json", 5),
    ("js", 8),
    ("ts", 3),
    ("py", 4),
    ("rs", 2),
    ("h", 4),
    ("c", 3),
    ("png", 8),
    ("jpg", 6),
    ("svg", 3),
    ("pdf", 2),
    ("docx", 1),
    ("mp3", 1),
    ("mp4", 1),
    ("zip", 1),
    ("log", 3),
    ("dat", 4),
    ("bin", 3),
    ("xml", 3),
    ("html", 2),
    ("pak", 1),
    ("tmp", 2),
];

const NON_ASCII: &[&str] = &["ü", "é", "文件", "データ", "ñ", "Ω"];

fn name(rng: &mut Rng, folder: bool) -> OsString {
    let mut s = String::new();
    // About 20 bytes on average, with some long descriptive names.
    let parts = if rng.below(10) == 0 {
        8 + rng.below(8)
    } else {
        2 + rng.below(4)
    };
    for i in 0..parts {
        if i > 0 && rng.below(4) == 0 {
            s.push(if rng.below(2) == 0 { '-' } else { ' ' });
        }
        s.push_str(SYLLABLES[rng.below(SYLLABLES.len() as u64) as usize]);
    }
    if rng.below(100) < 3 {
        s.push_str(NON_ASCII[rng.below(NON_ASCII.len() as u64) as usize]);
    }
    if rng.below(2) == 0 {
        s.push('_');
        s.push_str(&rng.below(100_000).to_string());
    }
    if !folder {
        let total: u32 = EXTENSIONS.iter().map(|e| e.1).sum();
        let mut pick = rng.below(u64::from(total)) as u32;
        for &(ext, weight) in EXTENSIONS {
            if pick < weight {
                if !ext.is_empty() {
                    s.push('.');
                    s.push_str(ext);
                }
                break;
            }
            pick -= weight;
        }
    }
    s.into()
}

fn file_sizes(rng: &mut Rng) -> FileSizes {
    let roll = rng.below(1000);
    let logical = if roll < 5 {
        0
    } else {
        (rng.pareto(600.0, 0.55).min(64.0 * (1u64 << 30) as f64)) as u64
    };
    // A few files have unknown allocation (cloud placeholders that failed),
    // and a few are sparse or compressed and take less than their length.
    let allocated = match roll {
        5..25 => Allocated::UNKNOWN,
        25..40 => Allocated::known(logical / 4 / 4096 * 4096),
        _ => Allocated::known(logical.div_ceil(4096) * 4096),
    };
    FileSizes { logical, allocated }
}

/// Summary of a generated tree.
pub struct Generated {
    pub tree: Tree,
    pub files: u64,
    pub folders: u64,
    pub name_bytes: u64,
}

/// Builds a tree with about `nodes` nodes.
pub fn generate(nodes: usize, seed: u64) -> Generated {
    let mut rng = Rng::new(seed);
    let mut tree = Tree::new("/synthetic");
    let mut queue = VecDeque::from([NodeId::ROOT]);
    let (mut files, mut folders, mut name_bytes) = (0u64, 1u64, 0u64);
    while let Some(dir) = queue.pop_front() {
        let remaining = nodes.saturating_sub(tree.len());
        if remaining == 0 {
            tree.set_dir_state(dir, DirState::Listed);
            continue;
        }
        // Most folders are small; a few hold thousands of files.
        let n_files = (rng.pareto(1.6, 1.1) as usize).min(20_000).min(remaining);
        let mut n_dirs = match rng.below(10) {
            0..=3 => 0,
            4..=6 => 1,
            7 | 8 => 2 + rng.below(3) as usize,
            _ => 5 + rng.below(20) as usize,
        };
        // Keep the tree growing until it reaches its size.
        if queue.is_empty() && n_dirs == 0 {
            n_dirs = 2;
        }
        let n_dirs = n_dirs.min(remaining - n_files);
        for _ in 0..n_files {
            let n = name(&mut rng, false);
            name_bytes += n.len() as u64;
            tree.add_file(dir, &n, file_sizes(&mut rng), 0)
                .expect("synthetic tree within capacity");
            files += 1;
        }
        for _ in 0..n_dirs {
            let n = name(&mut rng, true);
            name_bytes += n.len() as u64;
            let child = tree
                .add_dir(dir, &n)
                .expect("synthetic tree within capacity");
            queue.push_back(child);
            folders += 1;
        }
        tree.set_dir_state(dir, DirState::Listed);
    }
    Generated {
        tree,
        files,
        folders,
        name_bytes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_is_deterministic_and_sized() {
        let a = generate(20_000, 7);
        let b = generate(20_000, 7);
        assert_eq!(a.tree.len(), b.tree.len());
        assert_eq!(
            a.tree.allocated(NodeId::ROOT),
            b.tree.allocated(NodeId::ROOT)
        );
        assert!(a.tree.len() <= 20_000 && a.tree.len() > 19_000);
        assert_eq!(a.files + a.folders, a.tree.len() as u64);
        assert!(a.files > a.folders * 2, "files outnumber folders");
    }
}
