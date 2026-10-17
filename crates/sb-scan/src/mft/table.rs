//! The flat record table built from parsed records: extension records
//! merged into their base, names grouped per record, and a walk from any
//! directory that yields its nodes and bottom-up subtree totals.

use super::record::{AttributeList, Parsed, RECORD_MASK};
use super::{attr, classify, is_cloud};
use crate::adapter::EntryKind;
use std::collections::HashMap;

/// The volume's root directory.
pub const ROOT: u64 = 5;
/// Records below this are NTFS metafiles (`$MFT` .. `$Extend`, reserved),
/// never shown by a listing.
pub const FIRST_USER_RECORD: u64 = 16;

/// Whether a record has an attribute list, and where.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ListKind {
    #[default]
    None,
    Resident,
    NonResident,
}

/// One base record, with what its extension records added.
#[derive(Debug, Clone, Default)]
pub struct Record {
    pub sequence: u16,
    pub in_use: bool,
    pub directory: bool,
    /// `$STANDARD_INFORMATION` attributes, plus `FILE_ATTRIBUTE_DIRECTORY`
    /// for a directory (which NTFS keeps in the record header instead).
    pub attributes: u32,
    /// Reparse tag; 0 when none.
    pub reparse_tag: u32,
    pub reparse_nonresident: bool,
    /// The unnamed data stream, as a listing reports it.
    pub logical: u64,
    pub allocated: u64,
    pub has_data: bool,
    /// Allocation of WOF's compressed stream, for a WOF-compressed file.
    pub wof_allocated: Option<u64>,
    /// Allocation of alternate data streams (not counted, as listings don't).
    pub streams_allocated: u64,
    pub usn: u64,
    pub modified: u64,
    pub link_count: u16,
    pub attribute_list: ListKind,
    pub extensions: u16,
    names_start: u32,
    names_len: u32,
}

impl Record {
    /// The 64-bit file ID listings report: sequence and record number.
    pub fn reference(&self, number: u64) -> u64 {
        (u64::from(self.sequence) << 48) | number
    }
}

/// One name of a record.
#[derive(Debug, Clone, Copy)]
pub struct Name {
    /// Parent directory's file reference.
    pub parent: u64,
    start: u32,
    len: u16,
}

/// Counts from building the table, for the probe's report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    pub base_records: u64,
    pub extension_records: u64,
    pub merged_extensions: u64,
    /// Extension records whose base isn't in use or has another sequence.
    pub orphan_extensions: u64,
    pub attribute_list_records: u64,
    /// File references of records whose attribute list isn't resident: an
    /// engine re-reads these by file ID rather than trusting the merge.
    pub nonresident_attribute_lists: Vec<u64>,
    /// Attribute-list entries naming a record that wasn't merged into
    /// their base (stale or torn extension).
    pub missing_list_segments: u64,
    pub dos_names: u64,
    pub hard_linked_records: u64,
    /// File references of records whose reparse data isn't resident.
    pub nonresident_reparse: Vec<u64>,
    /// Names whose parent isn't an in-use directory with that sequence.
    pub stale_parents: u64,
}

/// The table: one slot per record number.
pub struct Table {
    pub records: Vec<Record>,
    names: Vec<Name>,
    pool: Vec<u16>,
    /// Child edges grouped by parent record: `edges[offsets[p]..offsets[p+1]]`.
    offsets: Vec<u32>,
    edges: Vec<(u64, u32)>,
    pub stats: Stats,
}

/// Bottom-up totals for a subtree, hard links counted once.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Totals {
    pub files: u64,
    pub directories: u64,
    /// Further names of files already counted.
    pub aliases: u64,
    pub logical: u64,
    pub allocated: u64,
    pub symlinks: u64,
    pub mount_points: u64,
    pub special: u64,
    pub unknown_reparse: u64,
}

impl Totals {
    fn add(&mut self, other: &Totals) {
        self.files += other.files;
        self.directories += other.directories;
        self.aliases += other.aliases;
        self.logical += other.logical;
        self.allocated += other.allocated;
        self.symlinks += other.symlinks;
        self.mount_points += other.mount_points;
        self.special += other.special;
        self.unknown_reparse += other.unknown_reparse;
    }
}

/// One name under the walked directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Node {
    pub record: u64,
    /// The parent directory's record number.
    pub parent: u64,
    /// Index of this name, for [`Table::name`].
    pub name: u32,
}

/// What a walk found under a directory.
pub struct Walk {
    pub nodes: Vec<Node>,
    pub totals: Totals,
    /// Every directory reached (the root first) with its subtree totals.
    pub directories: Vec<(u64, Totals)>,
}

impl Table {
    /// Builds the table for `count` records from parsed records in any
    /// order (base and extension records alike).
    pub fn build(chunks: impl IntoIterator<Item = Vec<Parsed>>, count: u64) -> Table {
        let count = usize::try_from(count).expect("record count fits in memory");
        let mut records = vec![Record::default(); count];
        let mut stats = Stats::default();
        // (record number, parent, name) for grouping.
        let mut pending: Vec<(u64, u64, Vec<u16>)> = Vec::new();
        let mut extensions = Vec::new();
        let mut lists: Vec<(u64, Vec<u64>)> = Vec::new();
        for chunk in chunks {
            for parsed in chunk {
                let Some(slot) = records.get_mut(parsed.number as usize) else {
                    continue;
                };
                stats.dos_names += u64::from(parsed.dos_names);
                if parsed.base != 0 {
                    stats.extension_records += 1;
                    extensions.push(parsed);
                    continue;
                }
                stats.base_records += 1;
                let standard = parsed.standard.unwrap_or_default();
                *slot = Record {
                    sequence: parsed.sequence,
                    in_use: true,
                    directory: parsed.directory,
                    attributes: standard.attributes
                        | if parsed.directory { attr::DIRECTORY } else { 0 },
                    usn: standard.usn,
                    modified: standard.modified,
                    link_count: parsed.link_count,
                    ..Record::default()
                };
                apply_streams(slot, &parsed, &mut stats);
                match &parsed.attribute_list {
                    Some(AttributeList::Resident(entries)) => {
                        slot.attribute_list = ListKind::Resident;
                        let segments = entries
                            .iter()
                            .map(|e| e.segment & RECORD_MASK)
                            .filter(|&s| s != parsed.number)
                            .collect();
                        lists.push((parsed.number, segments));
                    }
                    Some(AttributeList::NonResident) => {
                        slot.attribute_list = ListKind::NonResident;
                        stats.nonresident_attribute_lists.push(parsed.reference());
                    }
                    None => {}
                }
                pending.extend(
                    parsed
                        .names
                        .into_iter()
                        .map(|n| (parsed.number, n.parent, n.name)),
                );
            }
        }
        // Extension records, now that every base is in place.
        let mut owner: HashMap<u64, u64> = HashMap::new();
        for ext in extensions {
            let number = ext.base & RECORD_MASK;
            let sequence = (ext.base >> 48) as u16;
            let Some(base) = records
                .get_mut(number as usize)
                .filter(|r| r.in_use && r.sequence == sequence)
            else {
                stats.orphan_extensions += 1;
                continue;
            };
            stats.merged_extensions += 1;
            base.extensions += 1;
            owner.insert(ext.number, number);
            if let Some(standard) = ext.standard {
                base.attributes |= standard.attributes;
                base.usn = base.usn.max(standard.usn);
            }
            apply_streams(base, &ext, &mut stats);
            pending.extend(ext.names.into_iter().map(|n| (number, n.parent, n.name)));
        }
        for (base, segments) in &lists {
            stats.missing_list_segments += segments
                .iter()
                .filter(|s| owner.get(s) != Some(base))
                .count() as u64;
        }
        stats.attribute_list_records =
            lists.len() as u64 + stats.nonresident_attribute_lists.len() as u64;

        // Names grouped per record; a record's own names first.
        pending.sort_by_key(|(record, _, _)| *record);
        let mut names = Vec::with_capacity(pending.len());
        let mut pool = Vec::new();
        for (record, parent, name) in pending {
            let slot = &mut records[record as usize];
            if slot.names_len == 0 {
                slot.names_start = names.len() as u32;
            }
            slot.names_len += 1;
            names.push(Name {
                parent,
                start: pool.len() as u32,
                len: name.len() as u16,
            });
            pool.extend_from_slice(&name);
        }
        stats.hard_linked_records = records.iter().filter(|r| r.names_len > 1).count() as u64;

        // Child edges: names of user records under in-use directories.
        let mut edges: Vec<(u64, u64, u32)> = Vec::with_capacity(names.len());
        for (number, record) in records.iter().enumerate() {
            let number = number as u64;
            if number < FIRST_USER_RECORD || !record.in_use {
                continue;
            }
            for i in record.names_start..record.names_start + record.names_len {
                let parent = names[i as usize].parent;
                let p = (parent & RECORD_MASK) as usize;
                match records.get(p) {
                    Some(r) if r.in_use && r.directory && r.sequence == (parent >> 48) as u16 => {
                        edges.push((p as u64, number, i));
                    }
                    _ => stats.stale_parents += 1,
                }
            }
        }
        edges.sort_by_key(|&(parent, _, _)| parent);
        let mut offsets = vec![0u32; count + 1];
        for &(parent, _, _) in &edges {
            offsets[parent as usize + 1] += 1;
        }
        for i in 1..offsets.len() {
            offsets[i] += offsets[i - 1];
        }
        let edges = edges.into_iter().map(|(_, c, n)| (c, n)).collect();
        Table {
            records,
            names,
            pool,
            offsets,
            edges,
            stats,
        }
    }

    /// The record at `number`, if it's an in-use base record.
    pub fn get(&self, number: u64) -> Option<&Record> {
        self.records.get(number as usize).filter(|r| r.in_use)
    }

    /// The file ID listings report for record `number`.
    pub fn reference_of(&self, number: u64) -> u64 {
        self.records[number as usize].reference(number)
    }

    /// How a scan treats the record, by the same rules as a listing.
    pub fn kind(&self, number: u64) -> EntryKind {
        let r = &self.records[number as usize];
        classify(r.attributes, r.reparse_tag)
    }

    /// Whether the record belongs to a cloud provider, as a listing says.
    pub fn cloud(&self, number: u64) -> bool {
        let r = &self.records[number as usize];
        is_cloud(r.attributes, r.reparse_tag)
    }

    /// A record's names (indices for [`Table::name`]).
    pub fn names_of(&self, number: u64) -> std::ops::Range<u32> {
        let r = &self.records[number as usize];
        r.names_start..r.names_start + r.names_len
    }

    pub fn name(&self, index: u32) -> (&Name, &[u16]) {
        let n = &self.names[index as usize];
        (
            n,
            &self.pool[n.start as usize..n.start as usize + usize::from(n.len)],
        )
    }

    /// A readable path for a record from its first name, for reports.
    pub fn path(&self, number: u64) -> String {
        let mut parts = Vec::new();
        let mut at = number;
        for _ in 0..4096 {
            if at == ROOT {
                break;
            }
            let Some(first) = self.names_of(at).next() else {
                parts.push(format!("<record {at}>"));
                break;
            };
            let (name, text) = self.name(first);
            parts.push(String::from_utf16_lossy(text));
            at = name.parent & RECORD_MASK;
        }
        parts.reverse();
        format!("\\{}", parts.join("\\"))
    }

    /// Walks the directory `root` down, through directories only (never
    /// into links, mount points or other redirections, which are leaves),
    /// listing each name once and totalling each subtree bottom-up. A file
    /// with several names under the root counts once, at the first name
    /// reached. Metafiles aren't reached: they sit below record 16 or
    /// under `$Extend`.
    pub fn walk(&self, root: u64) -> Walk {
        let mut walk = Walk {
            nodes: Vec::new(),
            totals: Totals::default(),
            directories: Vec::new(),
        };
        if self.get(root).is_none_or(|r| !r.directory) {
            return walk;
        }
        let mut visited = vec![false; self.records.len()];
        let mut counted = vec![false; self.records.len()];
        visited[root as usize] = true;
        let mut order = vec![(root, Totals::default())];
        let mut stack = vec![(root, self.offsets[root as usize], 0usize)];
        while let Some(top) = stack.last_mut() {
            let (dir, pos, slot) = *top;
            if pos == self.offsets[dir as usize + 1] {
                stack.pop();
                let totals = order[slot].1;
                if let Some(&(_, _, parent)) = stack.last() {
                    order[parent].1.add(&totals);
                }
                continue;
            }
            top.1 += 1;
            let (child, name) = self.edges[pos as usize];
            walk.nodes.push(Node {
                record: child,
                parent: dir,
                name,
            });
            let t = &mut order[slot].1;
            match self.kind(child) {
                EntryKind::Directory => {
                    t.directories += 1;
                    if !std::mem::replace(&mut visited[child as usize], true) {
                        order.push((child, Totals::default()));
                        stack.push((child, self.offsets[child as usize], order.len() - 1));
                    }
                }
                EntryKind::File => {
                    if std::mem::replace(&mut counted[child as usize], true) {
                        t.aliases += 1;
                    } else {
                        let r = &self.records[child as usize];
                        t.files += 1;
                        t.logical += r.logical;
                        t.allocated += r.allocated;
                    }
                }
                EntryKind::Symlink => t.symlinks += 1,
                EntryKind::MountPoint => t.mount_points += 1,
                EntryKind::Special => t.special += 1,
                EntryKind::UnknownReparse | EntryKind::Failed(_) => t.unknown_reparse += 1,
            }
        }
        walk.totals = order[0].1;
        walk.directories = order;
        walk
    }
}

/// Takes the data streams and reparse tag a (base or extension) record holds.
fn apply_streams(record: &mut Record, parsed: &Parsed, stats: &mut Stats) {
    if let Some(data) = parsed.data {
        record.logical = data.logical;
        record.allocated = data.allocated;
        record.has_data = true;
    }
    if let Some(wof) = parsed.wof {
        record.wof_allocated = Some(wof.allocated);
    }
    record.streams_allocated += parsed.streams_allocated;
    if let Some(tag) = parsed.reparse_tag {
        record.reparse_tag = tag;
        if parsed.reparse_nonresident {
            record.reparse_nonresident = true;
            stats.nonresident_reparse.push(parsed.reference());
        }
    }
}
