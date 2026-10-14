//! Layout level of detail: how much a layout costs, and how much sibling
//! order it remembers, when a folder holds far more children than can be
//! drawn. Two trees: one folder of `files` equal small files beside a few
//! large ones, and a synthetic mixed tree of `files` nodes.

use crate::{mib, ms, synth, time};
use sb_core::layout::{self, LayoutParams, Metric, OrderCache};
use sb_core::size::Allocated;
use sb_core::tree::{DirState, FileSizes, NodeId, Tree};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

/// The system allocator, counting live heap bytes while [`COUNTING`] is set
/// so the order cache's own memory can be measured exactly. Other cases
/// don't set it, so their threads don't contend on the counter.
pub struct Counting;

static COUNTING: AtomicBool = AtomicBool::new(false);
static LIVE: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() && COUNTING.load(Ordering::Relaxed) {
            LIVE.fetch_add(layout.size(), Ordering::Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        if COUNTING.load(Ordering::Relaxed) {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() && COUNTING.load(Ordering::Relaxed) {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
            LIVE.fetch_add(new_size, Ordering::Relaxed);
        }
        p
    }
}

/// Live bytes counted so far; wraps if memory allocated before counting
/// started is freed, so only differences are meaningful.
fn live() -> usize {
    LIVE.load(Ordering::Relaxed)
}

fn sizes(bytes: u64) -> FileSizes {
    FileSizes {
        logical: bytes,
        allocated: Allocated::known(bytes),
    }
}

pub fn run(files: usize, seed: u64) {
    println!("\n== layout level of detail, {files} ==");
    COUNTING.store(true, Ordering::Relaxed);
    let mut tree = Tree::new("/lod");
    let wide = tree
        .add_dir(NodeId::ROOT, "wide".as_ref())
        .expect("capacity");
    for i in 0..files {
        tree.add_file(wide, format!("f{i:08}").as_ref(), sizes(4096), None, 0)
            .expect("capacity");
    }
    tree.set_dir_state(wide, DirState::Listed);
    // Large files beside the wide folder, so it gets a share of the view.
    let total = files as u64 * 4096;
    for i in 0..8u64 {
        tree.add_file(
            NodeId::ROOT,
            format!("big{i}").as_ref(),
            sizes(total / (i + 2)),
            None,
            0,
        )
        .expect("capacity");
    }
    tree.set_dir_state(NodeId::ROOT, DirState::Listed);
    measure("equal files", &tree, &[NodeId::ROOT, wide]);

    let g = synth::generate(files, seed);
    // The 2,000 folders with the most children, as views a user opens.
    let mut folders: Vec<NodeId> = (0..g.tree.len() as u32)
        .map(NodeId::from_index)
        .filter(|&n| g.tree.is_dir(n))
        .collect();
    folders.sort_by_key(|&n| std::cmp::Reverse(g.tree.children(n).count()));
    folders.truncate(2_000);
    folders.insert(0, NodeId::ROOT);
    measure("mixed tree", &g.tree, &folders);
}

/// Times layouts of the first view, then lays out every view with one cache
/// and reports what the cache holds.
fn measure(name: &str, tree: &Tree, views: &[NodeId]) {
    let params = LayoutParams::new(1920.0, 1080.0);
    let view = views[0];
    let (fresh, l) = time(5, || {
        layout::layout(
            tree,
            view,
            Metric::Allocated,
            None,
            &params,
            &mut OrderCache::default(),
            false,
        )
    });
    let mut cache = OrderCache::default();
    let before = live();
    layout::layout(
        tree,
        view,
        Metric::Allocated,
        None,
        &params,
        &mut cache,
        true,
    );
    let first = live().wrapping_sub(before);
    let (stable, _) = time(5, || {
        layout::layout(
            tree,
            view,
            Metric::Allocated,
            None,
            &params,
            &mut cache,
            true,
        )
    });
    println!(
        "{name}: {} rects; fresh final layout {}, stable repeat {}, cache after one layout {}",
        l.rects.len(),
        ms(fresh),
        ms(stable),
        kib(first)
    );
    if views.len() > 1 {
        let t = Instant::now();
        for &v in views {
            layout::layout(tree, v, Metric::Allocated, None, &params, &mut cache, true);
        }
        println!(
            "{name}: {} views laid out in {}, cache holds {}",
            views.len(),
            ms(t.elapsed()),
            kib(live().wrapping_sub(before))
        );
    }
}

fn kib(bytes: usize) -> String {
    if bytes >= 1 << 20 {
        mib(bytes as u64)
    } else {
        format!("{} KiB", bytes / 1024)
    }
}
