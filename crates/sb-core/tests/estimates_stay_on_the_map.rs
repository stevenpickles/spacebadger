//! Estimates from an earlier scan shape the map's areas and nothing else.
//!
//! The same scan, stopped part way, is built twice: once with a prior whose
//! sizes are far from today's, once without. Everything that reports a size
//! or a count must agree between the two: the tree's own numbers, snapshots
//! (so comparisons later), comparisons now, exports, selections, searches,
//! Git summaries, and the layout's weights. Only the logical, unfiltered
//! map's areas may differ.
//!
//! As an integration test this sees only the crate's public interface, in
//! which the estimates can't be read at all (`Tree::pending_estimate` is
//! private to the crate); `only_the_layout_reads_estimates` in the crate
//! checks that inside it only the layout does.

use sb_core::compare::Comparison;
use sb_core::export::{Csv, Report, ReportInfo, Scope};
use sb_core::git::{self, GitRoot, RootKind};
use sb_core::layout::{self, LayoutParams, Metric, OrderCache};
use sb_core::prior::Prior;
use sb_core::search::{Matcher, Search};
use sb_core::selection;
use sb_core::size::Allocated;
use sb_core::snapshot::Snapshot;
use sb_core::tree::{DirState, FileSizes, NodeId, Tree};
use std::ffi::OsStr;
use std::sync::Arc;

fn sizes(logical: u64) -> FileSizes {
    FileSizes {
        logical,
        allocated: Allocated::known(logical.div_ceil(4096) * 4096),
    }
}

fn dir(t: &mut Tree, parent: NodeId, name: &str) -> NodeId {
    t.add_dir(parent, OsStr::new(name)).unwrap()
}

fn file(t: &mut Tree, parent: NodeId, name: &str, bytes: u64) {
    t.add_file(parent, OsStr::new(name), sizes(bytes), None, 0)
        .unwrap();
}

/// An earlier scan with every folder much bigger than today.
fn prior() -> Arc<Prior> {
    let mut t = Tree::new("/data");
    for (name, bytes) in [
        ("repo", 900_000_000),
        ("media", 50_000_000_000),
        ("docs", 7_000_000),
    ] {
        let d = dir(&mut t, NodeId::ROOT, name);
        let sub = dir(&mut t, d, "sub");
        file(&mut t, sub, "big", bytes);
        let inner = dir(&mut t, sub, "inner");
        file(&mut t, inner, "deep", bytes / 3);
        t.set_dir_state(d, DirState::Listed);
        t.set_dir_state(sub, DirState::Listed);
        t.set_dir_state(inner, DirState::Listed);
    }
    t.set_dir_state(NodeId::ROOT, DirState::Listed);
    Arc::new(Prior::new(Snapshot::capture(&t, 1_000, true)))
}

/// Today's scan, part way through: the root and some folders listed, the
/// rest still waiting.
fn scan(prior: Option<Arc<Prior>>) -> Tree {
    let mut t = Tree::new("/data");
    if let Some(p) = prior {
        t.set_prior(p);
    }
    let repo = dir(&mut t, NodeId::ROOT, "repo");
    let media = dir(&mut t, NodeId::ROOT, "Media");
    let docs = dir(&mut t, NodeId::ROOT, "docs");
    file(&mut t, NodeId::ROOT, "notes.txt", 2_000);
    t.set_dir_state(NodeId::ROOT, DirState::Listed);
    // repo is listed: a Git checkout with its metadata still waiting.
    let dotgit = dir(&mut t, repo, ".git");
    let sub = dir(&mut t, repo, "sub");
    file(&mut t, repo, "main.rs", 30_000);
    file(&mut t, repo, "big.bin", 5_000_000);
    t.set_dir_state(repo, DirState::Listed);
    t.set_git(GitRoot::new(repo, RootKind::Repository));
    t.set_git(GitRoot::new(dotgit, RootKind::DotGit));
    // media listed in part (one batch in); docs not at all.
    file(&mut t, media, "clip.mp4", 3_000_000_000);
    let _ = (sub, docs);
    t
}

fn every_node(t: &Tree) -> impl Iterator<Item = NodeId> {
    (0..t.len() as u32).map(NodeId::from_index)
}

#[test]
fn the_prior_is_in_effect() {
    let with = scan(Some(prior()));
    assert!(with.is_provisional(NodeId::ROOT));
    assert_eq!(with.prior_created_ms(), Some(1_000));
    // The logical map gives the waiting folders room, so it differs.
    let params = LayoutParams::new(800.0, 600.0);
    let lay = |t: &Tree| {
        layout::layout(
            t,
            NodeId::ROOT,
            Metric::Logical,
            None,
            &params,
            &mut OrderCache::default(),
            true,
        )
    };
    assert_ne!(lay(&with).rects, lay(&scan(None)).rects);
}

#[test]
fn tree_numbers_are_measured_only() {
    let (with, without) = (scan(Some(prior())), scan(None));
    assert_eq!(with.len(), without.len());
    assert_eq!(with.dir_count(), without.dir_count());
    assert_eq!(with.alias_count(), without.alias_count());
    for n in every_node(&with) {
        assert_eq!(with.logical(n), without.logical(n), "{n:?}");
        assert_eq!(with.allocated(n), without.allocated(n));
        assert_eq!(with.file_count(n), without.file_count(n));
        assert_eq!(
            with.unknown_allocation_files(n),
            without.unknown_allocation_files(n)
        );
        assert_eq!(with.flags(n), without.flags(n));
        assert_eq!(with.dir_state(n), without.dir_state(n));
        assert_eq!(with.git_class(n), without.git_class(n));
    }
}

#[test]
fn snapshots_and_comparisons_ignore_estimates() {
    let (with, without) = (scan(Some(prior())), scan(None));
    assert_eq!(
        Snapshot::capture(&with, 5, true).encode(),
        Snapshot::capture(&without, 5, true).encode()
    );
    let mut earlier = Tree::new("/data");
    let repo = dir(&mut earlier, NodeId::ROOT, "repo");
    file(&mut earlier, repo, "big.bin", 4_000_000);
    file(&mut earlier, NodeId::ROOT, "notes.txt", 1_000);
    earlier.set_dir_state(repo, DirState::Listed);
    earlier.set_dir_state(NodeId::ROOT, DirState::Listed);
    let snap = Snapshot::capture(&earlier, 1, true);
    let (a, b) = (
        Comparison::new(&with, &snap),
        Comparison::new(&without, &snap),
    );
    for n in every_node(&with) {
        assert_eq!(a.status(n), b.status(n));
        assert_eq!(a.change(&with, &snap, n), b.change(&without, &snap, n));
    }
    assert_eq!(
        a.changes_under(&with, &snap, NodeId::ROOT, false),
        b.changes_under(&without, &snap, NodeId::ROOT, false)
    );
}

#[test]
fn exports_ignore_estimates() {
    let (with, without) = (scan(Some(prior())), scan(None));
    for metric in [Metric::Logical, Metric::Allocated] {
        for scope in [Scope::Contents, Scope::Everything] {
            let csv = |t: &Tree| {
                let mut export =
                    Csv::new(NodeId::ROOT, scope, metric).with_time_format(|_| "t".into());
                let mut out = String::new();
                while !export.step(t, None, 2, &mut out) {}
                out
            };
            assert_eq!(csv(&with), csv(&without));
        }
        let report = |t: &Tree| {
            let mut report = Report::new(NodeId::ROOT, metric).with_time_format(|_| "t".into());
            while !report.step(t, None, 2) {}
            let mut out = Vec::new();
            report
                .write(t, None, &ReportInfo::default(), &mut out)
                .unwrap();
            out
        };
        assert_eq!(report(&with), report(&without));
    }
}

#[test]
fn selections_searches_and_git_summaries_ignore_estimates() {
    let (with, without) = (scan(Some(prior())), scan(None));
    let all: Vec<u32> = (0..with.len() as u32).collect();
    assert_eq!(
        selection::resolve(&with, &all),
        selection::resolve(&without, &all)
    );
    for query in ["*", "size:>1MB", "git:repo"] {
        let search = |t: &Tree| {
            let mut s = Search::new(Matcher::new(query).unwrap());
            s.catch_up(t, usize::MAX);
            s
        };
        let (a, b) = (search(&with), search(&without));
        for n in every_node(&with) {
            assert_eq!(a.totals(n), b.totals(n), "{query}");
        }
        for metric in [Metric::Logical, Metric::Allocated] {
            assert_eq!(
                git::summarize(&with, Some(&a), metric),
                git::summarize(&without, Some(&b), metric)
            );
        }
    }
    for metric in [Metric::Logical, Metric::Allocated] {
        assert_eq!(
            git::summarize(&with, None, metric),
            git::summarize(&without, None, metric)
        );
    }
}

#[test]
fn layout_weights_and_totals_stay_measured() {
    let (with, without) = (scan(Some(prior())), scan(None));
    let params = LayoutParams::new(1200.0, 800.0);
    let mut s = Search::new(Matcher::new("*").unwrap());
    s.catch_up(&with, usize::MAX);
    let mut s2 = Search::new(Matcher::new("*").unwrap());
    s2.catch_up(&without, usize::MAX);
    for (metric, filtered) in [
        (Metric::Logical, false),
        (Metric::Allocated, false),
        (Metric::Logical, true),
    ] {
        let lay = |t: &Tree, f: Option<&Search>| {
            layout::layout(
                t,
                NodeId::ROOT,
                metric,
                f,
                &params,
                &mut OrderCache::default(),
                true,
            )
        };
        let a = lay(&with, filtered.then_some(&s));
        let b = lay(&without, filtered.then_some(&s2));
        assert_eq!(a.total, b.total);
        for r in &a.rects {
            let weight = metric.weight(&with, r.node);
            if r.kind != layout::RectKind::OtherSmall {
                assert_eq!(
                    r.weight,
                    if filtered {
                        s.weight(metric, r.node)
                    } else {
                        weight
                    }
                );
            }
        }
        if metric == Metric::Allocated || filtered {
            // Nothing estimated there: the same map.
            assert_eq!(a.rects, b.rects);
        }
    }
}
