//! Folder names and file extensions found in a scan, for the filter's
//! completion: `dir:no` suggests `node_modules`, `*.mk` suggests `mkv`.
//!
//! Names are grouped ignoring case and ranked by size. A folder name's size
//! counts each outermost folder of that name once, so `node_modules` inside
//! `node_modules` isn't counted twice; that is what `dir:` with the exact
//! name would match. Only the largest [`MAX_FOLDER_NAMES`] folder names are
//! kept, which bounds memory on trees with many distinct names.

use crate::tree::{NodeId, Tree};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::hash::{BuildHasherDefault, Hasher};

/// Folder names kept in an index, largest first.
pub const MAX_FOLDER_NAMES: usize = 100_000;

/// Longer "extensions" are usually part of the name (`notes.final draft`).
const MAX_EXT: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    lower: String,
    /// As first seen in the tree, when that differs from `lower`.
    display: Option<String>,
    /// Folders with this name, or files with this extension.
    pub count: u32,
    /// Logical bytes in them (see the module notes for folders).
    pub logical: u64,
}

impl Entry {
    /// As first seen in the tree, e.g. `Node_Modules` or `mkv`.
    pub fn name(&self) -> &str {
        self.display.as_deref().unwrap_or(&self.lower)
    }
}

#[derive(Debug, Default)]
pub struct NameIndex {
    folders: Vec<Entry>,
    extensions: Vec<Entry>,
}

impl NameIndex {
    /// Indexes every folder (except the scan root, whose name is a path)
    /// and file extension in `tree`.
    pub fn build(tree: &Tree) -> Self {
        let mut folders = Grouped::default();
        let mut extensions = Grouped::default();
        // Each folder's entry in `folders`, so a folder inside another of
        // the same name is found by comparing numbers.
        let mut folder_of = vec![NONE; tree.len()];
        let mut lower = String::new();
        for i in 1..tree.len() {
            let node = NodeId::from_index(i as u32);
            if tree.is_removed(node) {
                continue;
            }
            let name = tree.name(node);
            if tree.is_dir(node) {
                lowercase_into(name.as_encoded_bytes(), name, &mut lower);
                let id = folders.id(&lower, || name.to_string_lossy().into_owned());
                folder_of[i] = id;
                let nested = ancestors(tree, node).any(|a| folder_of[a.index()] == id);
                let e = &mut folders.entries[id as usize];
                e.count += 1;
                if !nested {
                    e.logical += tree.logical(node);
                }
            } else if let Some(ext) = extension(name.as_encoded_bytes()) {
                lowercase_into(ext, name, &mut lower);
                let id = extensions.id(&lower, String::new);
                let e = &mut extensions.entries[id as usize];
                e.count += 1;
                e.logical += tree.logical(node);
            }
        }
        let mut folders = folders.entries;
        let mut extensions = extensions.entries;
        let order = |a: &Entry, b: &Entry| b.logical.cmp(&a.logical).then(a.lower.cmp(&b.lower));
        // Only the kept names need sorting.
        if folders.len() > MAX_FOLDER_NAMES {
            folders.select_nth_unstable_by(MAX_FOLDER_NAMES - 1, order);
            folders.truncate(MAX_FOLDER_NAMES);
        }
        folders.sort_unstable_by(order);
        extensions.sort_unstable_by(order);
        Self {
            folders,
            extensions,
        }
    }

    /// Up to `limit` folder names for `typed`: those starting with it, then
    /// those containing it, each largest first.
    pub fn folders(&self, typed: &str, limit: usize) -> Vec<&Entry> {
        find(&self.folders, typed, limit)
    }

    /// Up to `limit` extensions for `typed`, as for [`NameIndex::folders`].
    pub fn extensions(&self, typed: &str, limit: usize) -> Vec<&Entry> {
        find(&self.extensions, typed, limit)
    }
}

const NONE: u32 = u32::MAX;

/// Entries grouped by lowercased name. Looking up a name already seen
/// doesn't allocate.
#[derive(Default)]
struct Grouped {
    /// By the lowercased name's hash, so a lookup needs no copy of it.
    ids: HashMap<u64, u32, BuildHasherDefault<WordHasher>>,
    /// Names whose hash another name already has.
    collided: HashMap<String, u32>,
    entries: Vec<Entry>,
}

/// A fast, non-cryptographic hasher for short names (the Fx hash used in
/// rustc). The default hasher resists collision attacks, which file names
/// on the user's own disk don't need, and is several times slower here.
#[derive(Default)]
struct WordHasher(u64);

impl Hasher for WordHasher {
    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for c in &mut chunks {
            self.add(u64::from_le_bytes(c.try_into().expect("8 bytes")));
        }
        for &b in chunks.remainder() {
            self.add(u64::from(b));
        }
    }

    fn write_u8(&mut self, b: u8) {
        self.add(u64::from(b));
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

impl WordHasher {
    fn add(&mut self, word: u64) {
        self.0 = (self.0.rotate_left(5) ^ word).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
}

impl Grouped {
    /// The entry for `lower`, added if new. `display` gives the name as
    /// seen, for a new entry; it's kept only when it differs from `lower`.
    fn id(&mut self, lower: &str, display: impl FnOnce() -> String) -> u32 {
        let mut hasher = WordHasher::default();
        hasher.write(lower.as_bytes());
        let hash = hasher.finish();
        let next = self.entries.len() as u32;
        match self.ids.get(&hash) {
            Some(&id) if self.entries[id as usize].lower == lower => return id,
            Some(_) => {
                if let Some(&id) = self.collided.get(lower) {
                    return id;
                }
                self.collided.insert(lower.to_owned(), next);
            }
            None => {
                self.ids.insert(hash, next);
            }
        }
        let display = display();
        self.entries.push(Entry {
            lower: lower.to_owned(),
            display: (!display.is_empty() && display != lower).then_some(display),
            count: 0,
            logical: 0,
        });
        next
    }
}

fn find<'a>(entries: &'a [Entry], typed: &str, limit: usize) -> Vec<&'a Entry> {
    let typed = typed.to_lowercase();
    let mut starts = Vec::new();
    let mut contains = Vec::new();
    for e in entries {
        if starts.len() >= limit {
            break;
        }
        if e.lower.starts_with(&typed) {
            starts.push(e);
        } else if contains.len() < limit && e.lower.contains(&typed) {
            contains.push(e);
        }
    }
    starts.extend(contains);
    starts.truncate(limit);
    starts
}

/// The folders above `node`, below the scan root.
fn ancestors(tree: &Tree, node: NodeId) -> impl Iterator<Item = NodeId> + '_ {
    std::iter::successors(tree.parent(node), |&p| tree.parent(p)).take_while(|&p| p != NodeId::ROOT)
}

/// `bytes` lowercased into `out`, reusing its allocation. Non-ASCII text
/// is lowercased from the whole `name` it's the end of.
fn lowercase_into(bytes: &[u8], name: &OsStr, out: &mut String) {
    out.clear();
    if bytes.is_ascii() {
        out.extend(bytes.iter().map(|&b| char::from(b.to_ascii_lowercase())));
    } else {
        let whole = name.to_string_lossy().to_lowercase();
        // `bytes` is a suffix of the name (the whole name, or its extension
        // after the last dot, which is ASCII-delimited).
        let tail = match whole.rfind('.') {
            Some(dot) if bytes.len() < name.as_encoded_bytes().len() => &whole[dot + 1..],
            _ => &whole[..],
        };
        out.push_str(tail);
    }
}

/// The bytes after a name's last dot, as `FileType::of` reads it: none for
/// hidden files (`.bashrc`), and none when it's long or has spaces.
fn extension(name: &[u8]) -> Option<&[u8]> {
    let dot = name.iter().rposition(|&b| b == b'.')?;
    let ext = &name[dot + 1..];
    (dot > 0 && !ext.is_empty() && ext.len() <= MAX_EXT && !ext.contains(&b' ')).then_some(ext)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::size::Allocated;
    use crate::tree::FileSizes;

    fn file(t: &mut Tree, dir: NodeId, name: &str, bytes: u64) {
        let sizes = FileSizes {
            logical: bytes,
            allocated: Allocated::known(bytes),
        };
        t.add_file(dir, name.as_ref(), sizes, None, 0).unwrap();
    }

    fn names(entries: Vec<&Entry>) -> Vec<(&str, u32, u64)> {
        entries
            .into_iter()
            .map(|e| (e.name(), e.count, e.logical))
            .collect()
    }

    #[test]
    fn indexes_folder_names_and_extensions_largest_first() {
        let mut t = Tree::new("/r/node");
        let nm = t.add_dir(NodeId::ROOT, "node_modules".as_ref()).unwrap();
        let inner = t.add_dir(nm, "Node_Modules".as_ref()).unwrap();
        let notes = t.add_dir(NodeId::ROOT, "notes".as_ref()).unwrap();
        let snode = t.add_dir(NodeId::ROOT, "src-node".as_ref()).unwrap();
        file(&mut t, nm, "a.js", 100);
        file(&mut t, inner, "b.JS", 50);
        file(&mut t, notes, "todo.md", 500);
        file(&mut t, snode, "film.MKV", 9_000);
        file(&mut t, NodeId::ROOT, ".bashrc", 1);
        file(&mut t, NodeId::ROOT, "draft.final version", 1);
        let index = NameIndex::build(&t);

        assert_eq!(
            names(index.folders("NO", 10)),
            [
                ("notes", 1, 500),
                ("node_modules", 2, 150),
                ("src-node", 1, 9_000)
            ],
            "starts first; same names grouped, nested ones counted once"
        );
        assert_eq!(names(index.folders("x", 10)), []);
        assert_eq!(names(index.folders("o", 1)).len(), 1, "the limit holds");
        assert_eq!(
            names(index.extensions("", 10)),
            [("mkv", 1, 9_000), ("md", 1, 500), ("js", 2, 150)],
            "hidden files and spaced endings have no extension"
        );
        assert_eq!(
            names(index.extensions("m", 10)),
            [("mkv", 1, 9_000), ("md", 1, 500)]
        );
    }
}
