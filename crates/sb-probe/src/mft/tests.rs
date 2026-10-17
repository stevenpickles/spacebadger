use super::compare::{ListedNode, Listing, Probe, compare};
use super::drive_letter;
use sb_scan::mft::record::{FileName, Parsed, Standard, Stream};
use sb_scan::mft::table::{ROOT, Table};
use sb_scan::mft::{attr, tag};
use sb_scan::{Entry, EntryKind, ErrorKind, Identity, Measured};
use std::collections::{HashMap, HashSet};

fn frn(number: u64, sequence: u16) -> u64 {
    (u64::from(sequence) << 48) | number
}

const ROOT_REF: u64 = (5 << 48) | 5;

fn record(number: u64, names: &[(u64, &str)], attributes: u32) -> Parsed {
    Parsed {
        number,
        sequence: 1,
        directory: attributes & attr::DIRECTORY != 0,
        link_count: names.len() as u16,
        standard: Some(Standard {
            attributes: attributes & !attr::DIRECTORY,
            ..Standard::default()
        }),
        names: names
            .iter()
            .map(|&(parent, name)| FileName {
                parent,
                name: name.encode_utf16().collect(),
                namespace: 1,
            })
            .collect(),
        ..Parsed::default()
    }
}

fn file(number: u64, names: &[(u64, &str)], logical: u64, allocated: u64) -> Parsed {
    Parsed {
        data: Some(Stream {
            logical,
            allocated,
            resident: false,
        }),
        ..record(number, names, 0x20)
    }
}

fn root() -> Parsed {
    Parsed {
        sequence: 5,
        ..record(ROOT, &[(ROOT_REF, ".")], attr::DIRECTORY)
    }
}

fn listed(parent: u64, dir: &str, name: &str, kind: EntryKind, id: u64) -> ListedNode {
    ListedNode {
        parent,
        name: name.encode_utf16().collect(),
        path: format!("{dir}\\{name}"),
        entry: Entry {
            name: name.into(),
            kind,
            logical: 0,
            allocated: Some(0),
            identity: Identity::MaybeShared { key: id.into() },
            id: id.into(),
            cloud: false,
            modified: None,
        },
    }
}

fn sized(mut node: ListedNode, logical: u64, allocated: u64) -> ListedNode {
    node.entry.logical = logical;
    node.entry.allocated = Some(allocated);
    node
}

#[derive(Default)]
struct FakeProbe {
    sizes: HashMap<String, Measured>,
    tags: HashMap<String, (u32, u32)>,
}

impl Probe for FakeProbe {
    fn remeasure(&self, path: &str) -> Option<Measured> {
        self.sizes.get(path).copied()
    }
    fn attribute_tag(&self, path: &str) -> Option<(u32, u32)> {
        self.tags.get(path).copied()
    }
}

/// Root with `docs\a.txt` and a file symlink `link`.
fn simple() -> (Table, Listing, FakeProbe) {
    let docs = frn(40, 1);
    let mut link = record(42, &[(ROOT_REF, "link")], attr::REPARSE_POINT);
    link.reparse_tag = Some(tag::SYMLINK);
    let table = Table::build(
        [vec![
            root(),
            record(40, &[(ROOT_REF, "docs")], attr::DIRECTORY),
            file(41, &[(docs, "a.txt")], 100, 104),
            link,
        ]],
        64,
    );
    let listing = Listing {
        nodes: vec![
            listed(5, "C:", "docs", EntryKind::Directory, frn(40, 1)),
            listed(5, "C:", "link", EntryKind::Symlink, frn(42, 1)),
            sized(
                listed(40, "C:\\docs", "a.txt", EntryKind::File, frn(41, 1)),
                100,
                104,
            ),
        ],
        listed_dirs: [5, 40].into(),
        failures: Vec::new(),
    };
    let mut probe = FakeProbe::default();
    probe
        .tags
        .insert("C:\\link".into(), (attr::REPARSE_POINT, tag::SYMLINK));
    (table, listing, probe)
}

#[test]
fn identical_trees_agree() {
    let (table, listing, probe) = simple();
    let c = compare(&table, ROOT, &listing, &HashSet::new(), &probe, 5, 0);
    assert!(c.pass, "{:?}", c.disagreements);
    assert_eq!(
        (c.mft_nodes, c.listed_nodes, c.matched, c.agreed),
        (3, 3, 3, 3)
    );
    assert_eq!(c.totals_mft, c.totals_listing);
    assert_eq!((c.totals_mft.files, c.totals_mft.symlinks), (1, 1));
}

#[test]
fn a_size_difference_fails_unless_the_record_changed() {
    let (table, mut listing, probe) = simple();
    listing.nodes[2].entry.logical = 99;
    let c = compare(&table, ROOT, &listing, &HashSet::new(), &probe, 5, 0);
    assert!(!c.pass);
    assert_eq!(c.disagreements.get("logical"), Some(&1));
    assert_eq!(c.agreed, 2);

    let c = compare(&table, ROOT, &listing, &[41].into(), &probe, 5, 0);
    assert!(c.pass);
    assert_eq!(c.changed_during_run.get("logical"), Some(&1));
}

#[test]
fn ids_kinds_tags_and_cloud_flags_are_compared() {
    let (table, mut listing, mut probe) = simple();
    listing.nodes[0].entry.id = frn(40, 2).into();
    listing.nodes[1].entry.kind = EntryKind::MountPoint;
    listing.nodes[2].entry.cloud = true;
    probe
        .tags
        .insert("C:\\link".into(), (attr::REPARSE_POINT, tag::MOUNT_POINT));
    // A cloud entry has its tag checked too.
    probe.tags.insert("C:\\docs\\a.txt".into(), (0x20, 0));
    let c = compare(&table, ROOT, &listing, &HashSet::new(), &probe, 5, 0);
    let fields: Vec<_> = c.disagreements.keys().map(String::as_str).collect();
    assert_eq!(fields, ["cloud", "file_id", "kind", "reparse_tag"]);
    assert_eq!(c.agreed, 0);

    let (table, mut listing, probe) = simple();
    listing.nodes[2].entry.kind = EntryKind::Failed(ErrorKind::Vanished);
    let c = compare(&table, ROOT, &listing, &HashSet::new(), &probe, 5, 0);
    assert_eq!(c.disagreements.get("listing_failed_entry"), Some(&1));
}

#[test]
fn missing_nodes_on_either_side() {
    let (table, mut listing, probe) = simple();
    let gone = listing.nodes.remove(2);
    listing.nodes.push(sized(
        listed(40, "C:\\docs", "new.txt", EntryKind::File, frn(50, 1)),
        1,
        8,
    ));
    let c = compare(&table, ROOT, &listing, &HashSet::new(), &probe, 5, 0);
    assert_eq!(c.disagreements.get("only_in_mft"), Some(&1));
    assert_eq!(c.disagreements.get("only_in_listing"), Some(&1));
    // Both records changed during the run: allowed.
    let c = compare(&table, ROOT, &listing, &[41, 50].into(), &probe, 5, 0);
    assert!(c.pass);
    assert_eq!(c.changed_during_run.len(), 2);

    // A folder the walk couldn't list: its records are unverified.
    listing.nodes.pop();
    listing.listed_dirs.remove(&40);
    listing
        .failures
        .push(("C:\\docs".into(), "access denied".into()));
    let c = compare(&table, ROOT, &listing, &HashSet::new(), &probe, 5, 0);
    assert_eq!((c.unverified, c.listing_failures), (1, 1));
    assert!(c.disagreements.is_empty());
    assert!(!c.pass);
    drop(gone);
}

#[test]
fn stale_hard_link_listings_are_settled_by_the_re_measure() {
    let docs = frn(40, 1);
    let table = Table::build(
        [vec![
            root(),
            record(40, &[(ROOT_REF, "docs")], attr::DIRECTORY),
            file(
                41,
                &[(docs, "a.bin"), (ROOT_REF, "b.bin")],
                1 << 20,
                1 << 20,
            ),
        ]],
        64,
    );
    let listing = Listing {
        nodes: vec![
            listed(5, "C:", "docs", EntryKind::Directory, frn(40, 1)),
            sized(
                listed(40, "C:\\docs", "a.bin", EntryKind::File, frn(41, 1)),
                1 << 20,
                1 << 20,
            ),
            // Not written through this name: its directory entry is stale.
            sized(
                listed(5, "C:", "b.bin", EntryKind::File, frn(41, 1)),
                262_144,
                262_144,
            ),
        ],
        listed_dirs: [5, 40].into(),
        failures: Vec::new(),
    };
    let mut probe = FakeProbe::default();
    let c = compare(&table, ROOT, &listing, &HashSet::new(), &probe, 5, 0);
    assert_eq!(
        c.disagreements.get("logical"),
        Some(&1),
        "no re-measure yet"
    );
    probe.sizes.insert(
        "C:\\b.bin".into(),
        Measured {
            logical: 1 << 20,
            allocated: Some(1 << 20),
        },
    );
    let c = compare(&table, ROOT, &listing, &HashSet::new(), &probe, 5, 0);
    assert!(c.pass);
    assert_eq!(c.explained.hard_link_remeasured, 1);
    assert_eq!((c.totals_mft.files, c.totals_mft.aliases), (1, 1));
}

#[test]
fn cloud_placeholders_and_wof_files() {
    let mut cloud = file(41, &[(ROOT_REF, "doc.docx")], 5000, 0);
    cloud.standard.as_mut().unwrap().attributes = attr::REPARSE_POINT | attr::RECALL_ON_DATA_ACCESS;
    cloud.reparse_tag = Some(0x9000_701A);
    let mut wof = file(42, &[(ROOT_REF, "app.dll")], 9000, 0);
    wof.standard.as_mut().unwrap().attributes = attr::REPARSE_POINT;
    wof.reparse_tag = Some(tag::WOF);
    wof.wof = Some(Stream {
        logical: 3000,
        allocated: 4096,
        resident: false,
    });
    let table = Table::build([vec![root(), cloud, wof]], 64);
    let mut placeholder = sized(
        listed(5, "C:", "doc.docx", EntryKind::File, frn(41, 1)),
        5000,
        0,
    );
    placeholder.entry.cloud = true;
    let listing = Listing {
        nodes: vec![
            placeholder,
            sized(
                listed(5, "C:", "app.dll", EntryKind::File, frn(42, 1)),
                9000,
                4096,
            ),
        ],
        listed_dirs: [5].into(),
        failures: Vec::new(),
    };
    let mut probe = FakeProbe::default();
    // The Cloud Files filter hides the placeholder's reparse point.
    probe
        .tags
        .insert("C:\\doc.docx".into(), (attr::RECALL_ON_DATA_ACCESS, 0));
    probe
        .tags
        .insert("C:\\app.dll".into(), (attr::REPARSE_POINT, tag::WOF));
    let c = compare(&table, ROOT, &listing, &HashSet::new(), &probe, 5, 0);
    assert!(c.pass, "{:?}", c.disagreements);
    assert_eq!(c.explained.cloud_tag_disguised, 1);
    assert_eq!(c.explained.wof_compressed_stream, 1);

    // An unreadable tag is a difference.
    probe.tags.clear();
    let c = compare(&table, ROOT, &listing, &HashSet::new(), &probe, 5, 0);
    assert_eq!(c.disagreements.get("reparse_tag_unreadable"), Some(&2));
}

#[test]
fn a_stale_single_name_entry_settles_only_when_the_re_measure_matches() {
    // A log open and growing: its directory entry trails the record.
    let (table, mut listing, mut probe) = simple();
    listing.nodes[2].entry.logical = 64;
    listing.nodes[2].entry.allocated = Some(64);
    let c = compare(&table, ROOT, &listing, &HashSet::new(), &probe, 5, 0);
    assert!(!c.pass, "no re-measure: a disagreement");

    probe.sizes.insert(
        "C:\\docs\\a.txt".into(),
        Measured {
            logical: 100,
            allocated: Some(104),
        },
    );
    let c = compare(&table, ROOT, &listing, &HashSet::new(), &probe, 5, 0);
    assert!(c.pass, "{:?}", c.disagreements);
    assert_eq!(c.explained.listing_stale_remeasured, 1);
    assert_eq!(c.explained.hard_link_remeasured, 0);

    // A re-measure that disagrees with the record still fails.
    probe.sizes.insert(
        "C:\\docs\\a.txt".into(),
        Measured {
            logical: 200,
            allocated: Some(208),
        },
    );
    let c = compare(&table, ROOT, &listing, &HashSet::new(), &probe, 5, 0);
    assert!(!c.pass);
    assert_eq!(c.disagreements.get("logical"), Some(&1));
    assert_eq!(c.explained.listing_stale_remeasured, 0);
}

#[test]
fn cloud_tag_variants_count_as_one() {
    let mut cloud = file(41, &[(ROOT_REF, "doc.docx")], 5000, 0);
    cloud.standard.as_mut().unwrap().attributes = attr::REPARSE_POINT | attr::PINNED;
    cloud.reparse_tag = Some(0x9000_601A);
    let table = Table::build([vec![root(), cloud]], 64);
    let mut node = sized(
        listed(5, "C:", "doc.docx", EntryKind::File, frn(41, 1)),
        5000,
        0,
    );
    node.entry.cloud = true;
    let listing = Listing {
        nodes: vec![node],
        listed_dirs: [5].into(),
        failures: Vec::new(),
    };
    let mut probe = FakeProbe::default();
    probe
        .tags
        .insert("C:\\doc.docx".into(), (attr::REPARSE_POINT, 0x9000_001A));
    let c = compare(&table, ROOT, &listing, &HashSet::new(), &probe, 5, 0);
    assert!(c.pass, "{:?}", c.disagreements);
    assert_eq!(c.explained.cloud_tag_disguised, 0);
    probe
        .tags
        .insert("C:\\doc.docx".into(), (attr::REPARSE_POINT, tag::SYMLINK));
    let c = compare(&table, ROOT, &listing, &HashSet::new(), &probe, 5, 0);
    assert_eq!(c.disagreements.get("reparse_tag"), Some(&1));
}

#[test]
fn reads_drive_letters() {
    assert_eq!(drive_letter("c"), Some('C'));
    assert_eq!(drive_letter("C:"), Some('C'));
    assert_eq!(drive_letter("d:\\"), Some('D'));
    assert_eq!(drive_letter(r"\\.\E:"), Some('E'));
    assert_eq!(drive_letter("CD:"), None);
    assert_eq!(drive_letter("1:"), None);
    assert_eq!(drive_letter(""), None);
}
