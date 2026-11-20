//! Node-by-node agreement between the MFT record table and a listing walk
//! of the same root with the production adapter. Platform-independent so
//! the rules are tested everywhere; the Windows side supplies the walk and
//! the two per-file queries ([`Probe`]).

use crate::report::Samples;
use sb_scan::mft::record::RECORD_MASK;
use sb_scan::mft::table::{ListKind, Table, Totals};
use sb_scan::mft::{attr, tag};
use sb_scan::{Entry, EntryKind, Measured};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};

/// One entry the listing walk returned.
pub struct ListedNode {
    /// Record number of the directory it was listed in.
    pub parent: u64,
    pub name: Vec<u16>,
    pub entry: Entry,
    /// Full path, for per-file queries and reports.
    pub path: String,
}

/// The listing walk of the compared root.
#[derive(Default)]
pub struct Listing {
    pub nodes: Vec<ListedNode>,
    /// Record numbers of every directory listed in full (the root too).
    pub listed_dirs: HashSet<u64>,
    /// Directories that couldn't be listed: (path, error).
    pub failures: Vec<(String, String)>,
}

/// Per-file queries the comparison may need, made the production way.
pub trait Probe {
    /// The adapter's authoritative re-measure (what the engine does for
    /// hard-link owners whose listing may be stale).
    fn remeasure(&self, path: &str) -> Option<Measured>;
    /// Attributes and reparse tag from an attributes-only open that
    /// neither follows links nor recalls cloud files.
    fn attribute_tag(&self, path: &str) -> Option<(u32, u32)>;
}

/// One difference, for the report.
#[derive(Debug, Serialize)]
pub struct Diff {
    pub path: String,
    pub record: u64,
    pub mft: String,
    pub listing: String,
}

/// Totals as the report shows them.
#[derive(Debug, Default, Serialize, PartialEq, Eq)]
pub struct Counted {
    pub files: u64,
    pub aliases: u64,
    pub directories: u64,
    pub logical: u64,
    pub allocated: u64,
    pub symlinks: u64,
    pub mount_points: u64,
    pub special: u64,
    pub unknown_reparse: u64,
}

impl From<Totals> for Counted {
    fn from(t: Totals) -> Self {
        Self {
            files: t.files,
            aliases: t.aliases,
            directories: t.directories,
            logical: t.logical,
            allocated: t.allocated,
            symlinks: t.symlinks,
            mount_points: t.mount_points,
            special: t.special,
            unknown_reparse: t.unknown_reparse,
        }
    }
}

/// Differences that follow from a documented listing behaviour rather than
/// from the MFT reading, counted so they stay visible.
#[derive(Debug, Default, Serialize)]
pub struct Explained {
    /// A hard-linked file whose listed sizes were stale (the directory
    /// entry of a name it wasn't written through) and whose re-measure
    /// agreed with the record.
    pub hard_link_remeasured: u64,
    /// A single-name file whose listed sizes were stale (directory entries
    /// of files open and growing, such as logs and databases, are updated
    /// lazily) and whose re-measure agreed with the record.
    pub listing_stale_remeasured: u64,
    /// A WOF-compressed file whose listed allocation is its compressed
    /// stream's (which the record also holds).
    pub wof_compressed_stream: u64,
    /// A cloud placeholder whose reparse tag the Cloud Files filter hides
    /// from ordinary processes (the record has the real cloud tag).
    pub cloud_tag_disguised: u64,
}

#[derive(Debug, Serialize)]
pub struct Comparison {
    pub mft_nodes: u64,
    pub listed_nodes: u64,
    pub matched: u64,
    /// Matched nodes identical on every compared field.
    pub agreed: u64,
    /// Differences on records whose USN didn't move: each one fails the
    /// gate. By field (`kind`, `logical`, `allocated`, `reparse_tag`,
    /// `cloud`, `file_id`, `only_in_mft`, `only_in_listing`, ...).
    pub disagreements: BTreeMap<String, u64>,
    /// Differences on records the journal says changed during the run.
    pub changed_during_run: BTreeMap<String, u64>,
    pub explained: Explained,
    /// MFT nodes in folders the walk couldn't list (see `listing_failures`).
    pub unverified: u64,
    pub listing_failures: u64,
    pub totals_mft: Counted,
    pub totals_listing: Counted,
    pub samples: Samples<Diff>,
    /// The first matched nodes with the record's values, when asked for
    /// (`--show`), so small trees such as the fixtures can be read off.
    pub nodes: Vec<NodeView>,
    /// Differences excused as changed during the run, against matched nodes.
    pub changed_ratio: f64,
    /// `changed_ratio` is under 1 %, so few nodes went unverified. Not part
    /// of `pass` (tiny trees such as the fixtures can't meet it); the gate
    /// on a whole volume needs both.
    pub changed_within_limit: bool,
    pub pass: bool,
}

/// A matched node as the record describes it.
#[derive(Debug, Serialize)]
pub struct NodeView {
    pub path: String,
    pub record: u64,
    pub kind: String,
    pub logical: u64,
    pub allocated: u64,
    pub reparse_tag: String,
    pub cloud: bool,
    pub names: usize,
    pub attribute_list: bool,
    pub agreed: bool,
}

fn kind_name(kind: EntryKind) -> String {
    format!("{kind:?}")
}

/// Compares the table's walk from `root` with the listing.
pub fn compare(
    table: &Table,
    root: u64,
    listing: &Listing,
    changed: &HashSet<u64>,
    probe: &impl Probe,
    samples: usize,
    show: usize,
) -> Comparison {
    let walk = table.walk(root);
    let mut index: HashMap<(u64, &[u16]), usize> = HashMap::with_capacity(walk.nodes.len());
    for (i, node) in walk.nodes.iter().enumerate() {
        index.insert((node.parent, table.name(node.name).1), i);
    }
    let mut seen = vec![false; walk.nodes.len()];
    let mut out = Comparison {
        mft_nodes: walk.nodes.len() as u64,
        listed_nodes: listing.nodes.len() as u64,
        matched: 0,
        agreed: 0,
        disagreements: BTreeMap::new(),
        changed_during_run: BTreeMap::new(),
        explained: Explained::default(),
        unverified: 0,
        listing_failures: listing.failures.len() as u64,
        totals_mft: walk.totals.into(),
        totals_listing: Counted::default(),
        samples: Samples::new(samples),
        nodes: Vec::new(),
        changed_ratio: 0.0,
        changed_within_limit: true,
        pass: false,
    };
    let mut counted_ids = HashSet::new();
    for listed in &listing.nodes {
        count_listed(&mut out.totals_listing, &mut counted_ids, &listed.entry);
        let listed_record = (listed.entry.id as u64) & RECORD_MASK;
        let Some(&i) = index.get(&(listed.parent, listed.name.as_slice())) else {
            let allowed = changed.contains(&listed_record);
            let diff = Diff {
                path: listed.path.clone(),
                record: listed_record,
                mft: "absent".into(),
                listing: kind_name(listed.entry.kind),
            };
            note(&mut out, allowed, "only_in_listing", diff);
            continue;
        };
        seen[i] = true;
        out.matched += 1;
        let record = walk.nodes[i].record;
        let allowed = changed.contains(&record) || changed.contains(&listed_record);
        let mut diffs: Vec<(&str, String, String)> = Vec::new();
        let r = &table.records[record as usize];
        let reference = table.reference_of(record);
        if listed.entry.id != u128::from(reference) {
            diffs.push((
                "file_id",
                format!("{reference:#x}"),
                format!("{:#x}", listed.entry.id),
            ));
        }
        let kind = table.kind(record);
        if kind != listed.entry.kind {
            let field = if matches!(listed.entry.kind, EntryKind::Failed(_)) {
                "listing_failed_entry"
            } else {
                "kind"
            };
            diffs.push((field, kind_name(kind), kind_name(listed.entry.kind)));
        }
        let cloud = table.cloud(record);
        if cloud != listed.entry.cloud {
            diffs.push(("cloud", cloud.to_string(), listed.entry.cloud.to_string()));
        }
        if kind == EntryKind::File && listed.entry.kind == EntryKind::File {
            let sizes = (r.logical, Some(r.allocated));
            let listed_sizes = (listed.entry.logical, listed.entry.allocated);
            if sizes != listed_sizes {
                // Any listing may be stale; the adapter's re-measure is
                // what counts, and only when it equals the record.
                let remeasured = probe
                    .remeasure(&listed.path)
                    .is_some_and(|m| (m.logical, m.allocated) == sizes);
                let wof = listed_sizes.0 == r.logical
                    && r.wof_allocated.is_some()
                    && listed_sizes.1 == r.wof_allocated;
                if remeasured && table.names_of(record).len() > 1 {
                    out.explained.hard_link_remeasured += 1;
                } else if remeasured {
                    out.explained.listing_stale_remeasured += 1;
                } else if wof {
                    out.explained.wof_compressed_stream += 1;
                } else {
                    if listed_sizes.0 != r.logical {
                        diffs.push(("logical", r.logical.to_string(), listed_sizes.0.to_string()));
                    }
                    if listed_sizes.1 != Some(r.allocated) {
                        diffs.push((
                            "allocated",
                            r.allocated.to_string(),
                            format!("{:?}", listed_sizes.1),
                        ));
                    }
                }
            }
        }
        let plain = matches!(listed.entry.kind, EntryKind::File | EntryKind::Directory);
        if r.reparse_tag != 0 || !plain || listed.entry.cloud {
            match probe.attribute_tag(&listed.path) {
                Some((attributes, raw)) => {
                    let listed_tag = if attributes & attr::REPARSE_POINT != 0 {
                        raw
                    } else {
                        0
                    };
                    // Cloud tags count as one: the filter may report
                    // another CLOUD_n variant than the one stored.
                    let same = listed_tag == r.reparse_tag
                        || (tag::is_cloud(listed_tag) && tag::is_cloud(r.reparse_tag));
                    if !same {
                        if tag::is_cloud(r.reparse_tag) && listed_tag == 0 {
                            out.explained.cloud_tag_disguised += 1;
                        } else {
                            diffs.push((
                                "reparse_tag",
                                format!("{:#x}", r.reparse_tag),
                                format!("{listed_tag:#x}"),
                            ));
                        }
                    }
                }
                None => diffs.push((
                    "reparse_tag_unreadable",
                    format!("{:#x}", r.reparse_tag),
                    "open failed".into(),
                )),
            }
        }
        if diffs.is_empty() {
            out.agreed += 1;
        }
        if out.nodes.len() < show {
            out.nodes.push(NodeView {
                path: listed.path.clone(),
                record,
                kind: kind_name(kind),
                logical: r.logical,
                allocated: r.allocated,
                reparse_tag: format!("{:#x}", r.reparse_tag),
                cloud,
                names: table.names_of(record).len(),
                attribute_list: r.attribute_list != ListKind::None,
                agreed: diffs.is_empty(),
            });
        }
        for (field, mft, listing_value) in diffs {
            let diff = Diff {
                path: listed.path.clone(),
                record,
                mft,
                listing: listing_value,
            };
            note(&mut out, allowed, field, diff);
        }
    }

    for (i, node) in walk.nodes.iter().enumerate() {
        if seen[i] {
            continue;
        }
        let diff = Diff {
            path: table.path(node.record),
            record: node.record,
            mft: kind_name(table.kind(node.record)),
            listing: "absent".into(),
        };
        if !listing.listed_dirs.contains(&node.parent) {
            out.unverified += 1;
            out.samples.add("unverified", || diff);
        } else {
            note(
                &mut out,
                changed.contains(&node.record),
                "only_in_mft",
                diff,
            );
        }
    }
    let changed: u64 = out.changed_during_run.values().sum();
    out.changed_ratio = changed as f64 / out.matched.max(1) as f64;
    out.changed_within_limit = out.changed_ratio < 0.01;
    out.pass = out.disagreements.is_empty() && out.unverified == 0 && out.listing_failures == 0;
    out
}

/// Counts one difference: allowed when its record changed during the run.
fn note(out: &mut Comparison, allowed: bool, field: &str, diff: Diff) {
    let bucket = if allowed {
        &mut out.changed_during_run
    } else {
        &mut out.disagreements
    };
    *bucket.entry(field.to_owned()).or_default() += 1;
    if !allowed {
        out.samples.add(field, || diff);
    }
}

/// Adds a listed entry to the listing's totals, a file ID once.
fn count_listed(t: &mut Counted, ids: &mut HashSet<u128>, entry: &Entry) {
    match entry.kind {
        EntryKind::File => {
            if entry.id != 0 && !ids.insert(entry.id) {
                t.aliases += 1;
            } else {
                t.files += 1;
                t.logical += entry.logical;
                t.allocated += entry.allocated.unwrap_or(0);
            }
        }
        EntryKind::Directory => t.directories += 1,
        EntryKind::Symlink => t.symlinks += 1,
        EntryKind::MountPoint => t.mount_points += 1,
        EntryKind::Special => t.special += 1,
        EntryKind::UnknownReparse | EntryKind::Failed(_) => t.unknown_reparse += 1,
    }
}
