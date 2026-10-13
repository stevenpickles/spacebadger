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

/// Folder names kept in an index, largest first.
pub const MAX_FOLDER_NAMES: usize = 100_000;

/// Longer "extensions" are usually part of the name (`notes.final draft`).
const MAX_EXT: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// As first seen in the tree, e.g. `Node_Modules` or `mkv`.
    pub name: String,
    lower: String,
    /// Folders with this name, or files with this extension.
    pub count: u32,
    /// Logical bytes in them (see the module notes for folders).
    pub logical: u64,
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
        let mut folders: HashMap<String, Entry> = HashMap::new();
        let mut extensions: HashMap<String, Entry> = HashMap::new();
        for i in 1..tree.len() {
            let node = NodeId::from_index(i as u32);
            if tree.is_removed(node) {
                continue;
            }
            let name = tree.name(node);
            if tree.is_dir(node) {
                let display = name.to_string_lossy();
                let entry = folders
                    .entry(display.to_lowercase())
                    .or_insert_with_key(|lower| Entry {
                        name: display.into_owned(),
                        lower: lower.clone(),
                        count: 0,
                        logical: 0,
                    });
                entry.count += 1;
                if !inside_same_name(tree, node, name) {
                    entry.logical += tree.logical(node);
                }
            } else if let Some(ext) = extension(name) {
                let entry = extensions
                    .entry(ext.to_lowercase())
                    .or_insert_with_key(|lower| Entry {
                        name: lower.clone(),
                        lower: lower.clone(),
                        count: 0,
                        logical: 0,
                    });
                entry.count += 1;
                entry.logical += tree.logical(node);
            }
        }
        let mut folders: Vec<Entry> = folders.into_values().collect();
        let mut extensions: Vec<Entry> = extensions.into_values().collect();
        let order = |a: &Entry, b: &Entry| b.logical.cmp(&a.logical).then(a.lower.cmp(&b.lower));
        folders.sort_unstable_by(order);
        folders.truncate(MAX_FOLDER_NAMES);
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

/// Whether a folder above `dir` (below the scan root) has the same name,
/// ignoring case.
fn inside_same_name(tree: &Tree, dir: NodeId, name: &OsStr) -> bool {
    let mut cur = tree.parent(dir);
    while let Some(p) = cur.filter(|&p| p != NodeId::ROOT) {
        if same_name(tree.name(p), name) {
            return true;
        }
        cur = tree.parent(p);
    }
    false
}

fn same_name(a: &OsStr, b: &OsStr) -> bool {
    let (x, y) = (a.as_encoded_bytes(), b.as_encoded_bytes());
    if x.is_ascii() && y.is_ascii() {
        return x.eq_ignore_ascii_case(y);
    }
    a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
}

/// The text after a name's last dot, as `FileType::of` reads it: none for
/// hidden files (`.bashrc`), and none when it's long or has spaces.
fn extension(name: &OsStr) -> Option<String> {
    let name = name.to_string_lossy();
    let dot = name.rfind('.')?;
    let ext = &name[dot + 1..];
    (dot > 0 && !ext.is_empty() && ext.len() <= MAX_EXT && !ext.contains(' '))
        .then(|| ext.to_owned())
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
        t.add_file(dir, name.as_ref(), sizes, 0).unwrap();
    }

    fn names(entries: Vec<&Entry>) -> Vec<(&str, u32, u64)> {
        entries
            .into_iter()
            .map(|e| (e.name.as_str(), e.count, e.logical))
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
