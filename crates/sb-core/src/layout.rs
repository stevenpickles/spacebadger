//! Viewport-scoped nested treemap layout.
//!
//! - Squarified rows (Bruls, Huizing, van Wijk) inside each folder.
//! - Level of detail: the largest folders are expanded first, and expansion
//!   stops at a rectangle budget or when a folder is too small to show its
//!   contents.
//! - Children too small to see are merged into one "other small items"
//!   rectangle with their true combined area; nothing is inflated.
//! - Sibling order is kept stable between layouts while a scan runs and is
//!   re-sorted only when sizes drift past [`REORDER_HYSTERESIS`].
//! - With a filename [`Search`], areas come from matching files only, and
//!   folders without matches are left out.
//!
//! Coordinates are in the caller's units (CSS pixels for the interface).
//! Rectangles are emitted parent before child, so drawing in order paints
//! nested content on top and hit testing in reverse finds the deepest one.

use crate::search::Search;
use crate::tree::{DirState, NodeId, Tree, flags};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};

/// Which size gives a node its area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    /// Known allocated bytes. Unknown allocations and hard-link aliases have
    /// no area.
    Allocated,
    /// Reported length, counting every name of a hard-linked file.
    Logical,
}

impl Metric {
    pub fn weight(self, tree: &Tree, node: NodeId) -> u64 {
        match self {
            Self::Allocated => tree.allocated(node),
            Self::Logical => tree.logical(node),
        }
    }

    /// The weight of `node`, counting only matching files when filtered.
    pub fn weight_in(self, tree: &Tree, filter: Option<&Search>, node: NodeId) -> u64 {
        match filter {
            Some(search) => search.weight(self, node),
            None => self.weight(tree, node),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayoutParams {
    pub width: f32,
    pub height: f32,
    /// Children smaller than this area are merged into "other small items".
    pub min_area: f32,
    /// Height of a folder's label strip, when the folder is big enough.
    pub header: f32,
    /// Inset between a folder's border and its contents.
    pub padding: f32,
    /// Stop expanding folders once this many rectangles exist.
    pub max_rects: usize,
}

impl LayoutParams {
    pub fn new(width: f32, height: f32) -> Self {
        Self {
            width,
            height,
            min_area: 12.0,
            header: 16.0,
            padding: 2.0,
            max_rects: 30_000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RectKind {
    File = 0,
    Folder = 1,
    /// Small children of `node` merged together.
    OtherSmall = 2,
}

/// Bits in [`LayoutRect::flags`].
pub mod rect_flags {
    /// The folder has a label strip at the top.
    pub const HEADER: u8 = 1 << 0;
    /// The folder's contents are drawn inside it.
    pub const EXPANDED: u8 = 1 << 1;
    /// A folder whose contents are still being listed.
    pub const PENDING: u8 = 1 << 2;
    /// A folder whose own listing failed or was cancelled.
    pub const INCOMPLETE: u8 = 1 << 3;
    pub const CLOUD: u8 = 1 << 4;
    pub const HARDLINKED: u8 = 1 << 5;
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayoutRect {
    /// The file or folder; for [`RectKind::OtherSmall`], the folder whose
    /// small children are merged.
    pub node: NodeId,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// 1 for children of the view root.
    pub depth: u8,
    pub kind: RectKind,
    pub flags: u8,
    pub weight: u64,
    /// Number of merged items for [`RectKind::OtherSmall`], else 1.
    pub count: u32,
}

#[derive(Debug, Clone, Default)]
pub struct Layout {
    pub rects: Vec<LayoutRect>,
    /// Weight of the view root.
    pub total: u64,
    /// The rectangle budget stopped further expansion.
    pub truncated: bool,
}

/// A sibling may move only when it is this many times larger than the one
/// before it.
pub const REORDER_HYSTERESIS: f64 = 1.5;

/// Remembers sibling order between layouts of one scan.
#[derive(Debug, Default)]
pub struct OrderCache {
    /// Folder → rank of each child in the last order used.
    rank: HashMap<NodeId, HashMap<NodeId, u32>>,
}

/// Upper bound on cached folders; the cache is cleared when exceeded.
const ORDER_CACHE_LIMIT: usize = 100_000;

impl OrderCache {
    pub fn clear(&mut self) {
        self.rank.clear();
    }

    /// Orders `children` (with weights) largest first, keeping the previous
    /// order when `stable` and no sibling has outgrown its predecessor by
    /// more than [`REORDER_HYSTERESIS`].
    fn arrange(
        &mut self,
        dir: NodeId,
        mut children: Vec<(NodeId, u64)>,
        stable: bool,
    ) -> Vec<(NodeId, u64)> {
        let sort = |c: &mut Vec<(NodeId, u64)>| {
            c.sort_unstable_by_key(|&(id, w)| (Reverse(w), id.index()))
        };
        if stable && let Some(rank) = self.rank.get(&dir) {
            let mut known = Vec::with_capacity(children.len());
            let mut new = Vec::new();
            for &(id, w) in &children {
                match rank.get(&id) {
                    Some(&r) => known.push((r, id, w)),
                    None => new.push((id, w)),
                }
            }
            known.sort_unstable_by_key(|&(r, _, _)| r);
            let mut known: Vec<_> = known.into_iter().map(|(_, id, w)| (id, w)).collect();
            let unchanged = new.is_empty();
            sort(&mut new);
            known.extend(new);
            let drifted = known
                .windows(2)
                .any(|p| p[1].1 as f64 > p[0].1 as f64 * REORDER_HYSTERESIS);
            if !drifted {
                if !unchanged {
                    self.remember(dir, &known);
                }
                return known;
            }
        }
        sort(&mut children);
        self.remember(dir, &children);
        children
    }

    fn remember(&mut self, dir: NodeId, children: &[(NodeId, u64)]) {
        if self.rank.len() >= ORDER_CACHE_LIMIT && !self.rank.contains_key(&dir) {
            self.rank.clear();
        }
        let rank = children
            .iter()
            .enumerate()
            .map(|(i, &(id, _))| (id, i as u32))
            .collect();
        self.rank.insert(dir, rank);
    }
}

/// A folder rectangle waiting to be expanded, ordered by area.
struct Pending {
    area: f32,
    rect: usize,
}

impl PartialEq for Pending {
    fn eq(&self, other: &Self) -> bool {
        self.area == other.area
    }
}
impl Eq for Pending {}
impl PartialOrd for Pending {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Pending {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.area
            .total_cmp(&other.area)
            .then(other.rect.cmp(&self.rect))
    }
}

/// Lays out the contents of `view` to fill `params.width` × `params.height`.
///
/// `filter` limits the map to files matching a search; it must be caught up
/// with `tree`. `stable` keeps sibling order from earlier layouts (use while
/// scanning); pass `false` for a final relayout that sorts every folder
/// afresh. Use a separate `cache` per filter.
pub fn layout(
    tree: &Tree,
    view: NodeId,
    metric: Metric,
    filter: Option<&Search>,
    params: &LayoutParams,
    cache: &mut OrderCache,
    stable: bool,
) -> Layout {
    let mut out = Layout {
        total: metric.weight_in(tree, filter, view),
        ..Layout::default()
    };
    if !tree.contains(view) || !tree.is_dir(view) || params.width <= 0.0 || params.height <= 0.0 {
        return out;
    }
    let mut ctx = Ctx {
        tree,
        metric,
        filter,
        params,
        cache,
        stable,
        rects: Vec::new(),
        queue: BinaryHeap::new(),
    };
    let full = Area {
        x: 0.0,
        y: 0.0,
        w: params.width,
        h: params.height,
    };
    ctx.fill(view, full, 1);
    while let Some(next) = ctx.queue.pop() {
        if ctx.rects.len() >= params.max_rects {
            out.truncated = true;
            break;
        }
        ctx.expand(next.rect);
    }
    out.rects = ctx.rects;
    out
}

#[derive(Debug, Clone, Copy)]
struct Area {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

struct Ctx<'a> {
    tree: &'a Tree,
    metric: Metric,
    filter: Option<&'a Search>,
    params: &'a LayoutParams,
    cache: &'a mut OrderCache,
    stable: bool,
    rects: Vec<LayoutRect>,
    queue: BinaryHeap<Pending>,
}

impl Ctx<'_> {
    /// Places a folder's contents inside its rectangle (or its interior).
    fn expand(&mut self, index: usize) {
        let r = self.rects[index];
        let p = self.params;
        let has_header = r.w >= 40.0 && r.h >= p.header + 2.0 * p.padding + 8.0;
        let top = if has_header { p.header } else { p.padding };
        let inner = Area {
            x: r.x + p.padding,
            y: r.y + top,
            w: r.w - 2.0 * p.padding,
            h: r.h - top - p.padding,
        };
        if inner.w < 4.0 || inner.h < 4.0 {
            return;
        }
        let rect = &mut self.rects[index];
        rect.flags |= rect_flags::EXPANDED;
        if has_header {
            rect.flags |= rect_flags::HEADER;
        }
        self.fill(r.node, inner, r.depth.saturating_add(1));
    }

    /// Squarifies `dir`'s children into `area`.
    fn fill(&mut self, dir: NodeId, area: Area, depth: u8) {
        let tree = self.tree;
        let total = self.metric.weight_in(tree, self.filter, dir);
        if total == 0 {
            return;
        }
        let children: Vec<(NodeId, u64)> = tree
            .children(dir)
            .map(|c| (c, self.metric.weight_in(tree, self.filter, c)))
            .filter(|&(_, w)| w > 0)
            .collect();
        let children = self.cache.arrange(dir, children, self.stable);
        let scale = f64::from(area.w) * f64::from(area.h) / total as f64;

        let mut items: Vec<Item> = Vec::with_capacity(children.len().min(1024));
        let mut small = (0u64, 0u32);
        for (node, weight) in children {
            if (weight as f64 * scale) < f64::from(self.params.min_area) {
                small.0 += weight;
                small.1 += 1;
            } else {
                items.push(Item::Node(node, weight));
            }
        }
        if small.1 > 0 {
            items.push(Item::Other(small.0, small.1));
        }

        let areas: Vec<f64> = items.iter().map(|i| i.weight() as f64 * scale).collect();
        let mut placed = Vec::with_capacity(items.len());
        squarify(&areas, area, &mut placed);
        for (item, a) in items.into_iter().zip(placed) {
            let rect = match item {
                Item::Node(node, weight) => self.node_rect(node, weight, a, depth),
                Item::Other(weight, count) => LayoutRect {
                    node: dir,
                    x: a.x,
                    y: a.y,
                    w: a.w,
                    h: a.h,
                    depth,
                    kind: RectKind::OtherSmall,
                    flags: 0,
                    weight,
                    count,
                },
            };
            let index = self.rects.len();
            if rect.kind == RectKind::Folder {
                self.queue.push(Pending {
                    area: rect.w * rect.h,
                    rect: index,
                });
            }
            self.rects.push(rect);
        }
    }

    fn node_rect(&self, node: NodeId, weight: u64, a: Area, depth: u8) -> LayoutRect {
        let tree = self.tree;
        let node_flags = tree.flags(node);
        let mut f = 0;
        if node_flags & flags::CLOUD != 0 {
            f |= rect_flags::CLOUD;
        }
        if node_flags & flags::HARDLINKED != 0 {
            f |= rect_flags::HARDLINKED;
        }
        let kind = if tree.is_dir(node) {
            match tree.dir_state(node) {
                DirState::Pending => f |= rect_flags::PENDING,
                DirState::Failed | DirState::NotScanned => f |= rect_flags::INCOMPLETE,
                DirState::Listed => {}
            }
            RectKind::Folder
        } else {
            RectKind::File
        };
        LayoutRect {
            node,
            x: a.x,
            y: a.y,
            w: a.w,
            h: a.h,
            depth,
            kind,
            flags: f,
            weight,
            count: 1,
        }
    }
}

enum Item {
    Node(NodeId, u64),
    Other(u64, u32),
}

impl Item {
    fn weight(&self) -> u64 {
        match *self {
            Self::Node(_, w) | Self::Other(w, _) => w,
        }
    }
}

/// Worst aspect ratio of a row with the given sum/min/max along `side`.
fn worst(sum: f64, min: f64, max: f64, side: f64) -> f64 {
    let s2 = side * side;
    let sum2 = sum * sum;
    (s2 * max / sum2).max(sum2 / (s2 * min))
}

/// Squarified treemap: `areas` (roughly largest first, summing to the area
/// of `rect`) are placed in rows along the shorter side.
fn squarify(areas: &[f64], rect: Area, out: &mut Vec<Area>) {
    let (mut x, mut y) = (f64::from(rect.x), f64::from(rect.y));
    let (mut w, mut h) = (f64::from(rect.w), f64::from(rect.h));
    let mut i = 0;
    while i < areas.len() {
        let side = w.min(h).max(f64::MIN_POSITIVE);
        let (mut sum, mut min, mut max) = (areas[i], areas[i], areas[i]);
        let mut end = i + 1;
        let mut current = worst(sum, min, max, side);
        while end < areas.len() {
            let a = areas[end];
            let next = worst(sum + a, min.min(a), max.max(a), side);
            if next > current {
                break;
            }
            (sum, min, max, current) = (sum + a, min.min(a), max.max(a), next);
            end += 1;
        }
        // The last row takes whatever space remains, absorbing rounding.
        let last = end == areas.len();
        if w >= h {
            let col = if last { w } else { (sum / h).min(w) };
            let mut cy = y;
            for (k, &a) in areas[i..end].iter().enumerate() {
                let ch = if k + 1 == end - i {
                    y + h - cy
                } else {
                    a / col
                };
                out.push(area(x, cy, col, ch));
                cy += ch;
            }
            x += col;
            w -= col;
        } else {
            let row = if last { h } else { (sum / w).min(h) };
            let mut cx = x;
            for (k, &a) in areas[i..end].iter().enumerate() {
                let cw = if k + 1 == end - i {
                    x + w - cx
                } else {
                    a / row
                };
                out.push(area(cx, y, cw, row));
                cx += cw;
            }
            y += row;
            h -= row;
        }
        i = end;
    }
}

fn area(x: f64, y: f64, w: f64, h: f64) -> Area {
    Area {
        x: x as f32,
        y: y as f32,
        w: w.max(0.0) as f32,
        h: h.max(0.0) as f32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::size::Allocated;
    use crate::tree::FileSizes;
    use std::ffi::OsStr;

    fn sizes(bytes: u64) -> FileSizes {
        FileSizes {
            logical: bytes,
            allocated: Allocated::known(bytes),
        }
    }

    fn file(tree: &mut Tree, parent: NodeId, name: &str, bytes: u64) -> NodeId {
        tree.add_file(parent, OsStr::new(name), sizes(bytes), 0)
            .unwrap()
    }

    fn dir(tree: &mut Tree, parent: NodeId, name: &str) -> NodeId {
        let d = tree.add_dir(parent, OsStr::new(name)).unwrap();
        tree.set_dir_state(d, DirState::Listed);
        d
    }

    fn run(tree: &Tree, view: NodeId, params: &LayoutParams) -> Layout {
        layout(
            tree,
            view,
            Metric::Allocated,
            None,
            params,
            &mut OrderCache::default(),
            false,
        )
    }

    fn top_level(l: &Layout) -> Vec<&LayoutRect> {
        l.rects.iter().filter(|r| r.depth == 1).collect()
    }

    fn overlaps(a: &LayoutRect, b: &LayoutRect) -> bool {
        let e = 1e-3;
        a.x + e < b.x + b.w && b.x + e < a.x + a.w && a.y + e < b.y + b.h && b.y + e < a.y + a.h
    }

    #[test]
    fn areas_are_proportional_and_fill_the_view() {
        let mut t = Tree::new("/r");
        for (i, size) in [600u64, 300, 100].into_iter().enumerate() {
            file(&mut t, NodeId::ROOT, &format!("f{i}"), size);
        }
        let l = run(&t, NodeId::ROOT, &LayoutParams::new(200.0, 100.0));
        let rects = top_level(&l);
        assert_eq!(rects.len(), 3);
        let total: f32 = rects.iter().map(|r| r.w * r.h).sum();
        assert!((total - 20_000.0).abs() < 1.0, "{total}");
        for r in &rects {
            let expected = r.weight as f32 / 1000.0 * 20_000.0;
            assert!((r.w * r.h - expected).abs() < 1.0, "{r:?}");
            assert!(r.x >= 0.0 && r.y >= 0.0 && r.x + r.w <= 200.01 && r.y + r.h <= 100.01);
        }
        for (i, a) in rects.iter().enumerate() {
            for b in &rects[i + 1..] {
                assert!(!overlaps(a, b), "{a:?} overlaps {b:?}");
            }
        }
        assert_eq!(rects[0].weight, 600, "largest first");
    }

    #[test]
    fn squarified_rows_keep_aspect_ratios_reasonable() {
        let mut t = Tree::new("/r");
        for i in 0..50 {
            file(&mut t, NodeId::ROOT, &format!("f{i}"), 1000);
        }
        let l = run(&t, NodeId::ROOT, &LayoutParams::new(500.0, 500.0));
        for r in top_level(&l) {
            let ratio = r.w.max(r.h) / r.w.min(r.h);
            assert!(ratio < 3.0, "{r:?}");
        }
    }

    #[test]
    fn small_items_are_merged_with_their_true_area() {
        let mut t = Tree::new("/r");
        file(&mut t, NodeId::ROOT, "big", 1_000_000);
        for i in 0..1000 {
            file(&mut t, NodeId::ROOT, &format!("tiny{i}"), 1);
        }
        let l = run(&t, NodeId::ROOT, &LayoutParams::new(100.0, 100.0));
        let rects = top_level(&l);
        assert_eq!(rects.len(), 2);
        let other = rects
            .iter()
            .find(|r| r.kind == RectKind::OtherSmall)
            .unwrap();
        assert_eq!(
            (other.weight, other.count, other.node),
            (1000, 1000, NodeId::ROOT)
        );
        let expected = 1000.0 / 1_001_000.0 * 10_000.0;
        assert!(
            (other.w * other.h - expected).abs() < 0.5,
            "not inflated: {other:?}"
        );
    }

    #[test]
    fn folders_nest_with_header_and_padding() {
        let mut t = Tree::new("/r");
        let a = dir(&mut t, NodeId::ROOT, "a");
        file(&mut t, a, "x", 500);
        file(&mut t, a, "y", 500);
        let l = run(&t, NodeId::ROOT, &LayoutParams::new(300.0, 200.0));
        let folder = l.rects[0];
        assert_eq!((folder.kind, folder.depth), (RectKind::Folder, 1));
        assert_ne!(folder.flags & rect_flags::HEADER, 0);
        assert_ne!(folder.flags & rect_flags::EXPANDED, 0);
        let kids: Vec<_> = l.rects.iter().filter(|r| r.depth == 2).collect();
        assert_eq!(kids.len(), 2);
        for k in kids {
            assert!(k.y >= folder.y + 16.0 - 1e-3);
            assert!(k.x >= folder.x + 2.0 - 1e-3 && k.x + k.w <= folder.x + folder.w - 2.0 + 1e-3);
        }
    }

    #[test]
    fn rect_budget_limits_expansion_largest_first() {
        let mut t = Tree::new("/r");
        let big = dir(&mut t, NodeId::ROOT, "big");
        let small = dir(&mut t, NodeId::ROOT, "small");
        for i in 0..20 {
            file(&mut t, big, &format!("b{i}"), 100);
            file(&mut t, small, &format!("s{i}"), 10);
        }
        let params = LayoutParams {
            max_rects: 10,
            ..LayoutParams::new(400.0, 400.0)
        };
        let l = run(&t, NodeId::ROOT, &params);
        assert!(l.truncated);
        let parent_of = |r: &LayoutRect| t.parent(r.node);
        assert!(
            l.rects
                .iter()
                .any(|r| r.depth == 2 && parent_of(r) == Some(big))
        );
        assert!(
            !l.rects
                .iter()
                .any(|r| r.depth == 2 && parent_of(r) == Some(small))
        );
    }

    #[test]
    fn zero_weight_entries_and_pending_folders_get_no_area() {
        let mut t = Tree::new("/r");
        file(&mut t, NodeId::ROOT, "data", 100);
        file(&mut t, NodeId::ROOT, "empty", 0);
        t.add_dir(NodeId::ROOT, OsStr::new("pending")).unwrap();
        let unknown = FileSizes {
            logical: 50,
            allocated: Allocated::UNKNOWN,
        };
        t.add_file(NodeId::ROOT, OsStr::new("unknown"), unknown, 0)
            .unwrap();
        let l = run(&t, NodeId::ROOT, &LayoutParams::new(100.0, 100.0));
        assert_eq!(l.rects.len(), 1);
        assert_eq!(t.name(l.rects[0].node), "data");

        let logical = layout(
            &t,
            NodeId::ROOT,
            Metric::Logical,
            None,
            &LayoutParams::new(100.0, 100.0),
            &mut OrderCache::default(),
            false,
        );
        assert_eq!(
            logical.rects.len(),
            2,
            "unknown allocation still has a length"
        );
    }

    #[test]
    fn stable_order_resists_small_changes_and_yields_to_large_ones() {
        let mut t = Tree::new("/r");
        let a = file(&mut t, NodeId::ROOT, "a", 100);
        let b = file(&mut t, NodeId::ROOT, "b", 90);
        let params = LayoutParams::new(100.0, 100.0);
        let mut cache = OrderCache::default();
        let order = |t: &Tree, cache: &mut OrderCache, stable| {
            layout(
                t,
                NodeId::ROOT,
                Metric::Allocated,
                None,
                &params,
                cache,
                stable,
            )
            .rects
            .iter()
            .map(|r| r.node)
            .collect::<Vec<_>>()
        };
        assert_eq!(order(&t, &mut cache, true), [a, b]);

        // b grows a little past a: order is kept while scanning...
        t.update_file(b, sizes(120));
        assert_eq!(order(&t, &mut cache, true), [a, b]);
        // ...and corrected by the final relayout.
        assert_eq!(order(&t, &mut cache, false), [b, a]);

        // a large change reorders even while scanning.
        t.update_file(a, sizes(1000));
        assert_eq!(order(&t, &mut cache, true), [a, b]);
    }

    #[test]
    fn filtered_layout_uses_matching_bytes_only() {
        use crate::search::{Matcher, Search};
        let mut t = Tree::new("/r");
        let photos = dir(&mut t, NodeId::ROOT, "photos");
        let music = dir(&mut t, NodeId::ROOT, "music");
        let jpg = file(&mut t, photos, "a.jpg", 300);
        file(&mut t, photos, "b.png", 5_000);
        file(&mut t, music, "song.mp3", 10_000);
        let mut s = Search::new(Matcher::new(".JPG").unwrap());
        s.catch_up(&t, usize::MAX);

        let l = layout(
            &t,
            NodeId::ROOT,
            Metric::Allocated,
            Some(&s),
            &LayoutParams::new(100.0, 100.0),
            &mut OrderCache::default(),
            false,
        );
        assert_eq!(l.total, 300);
        let nodes: Vec<_> = l.rects.iter().map(|r| r.node).collect();
        assert_eq!(nodes, [photos, jpg], "music has no matches");
        assert_eq!(l.rects[0].weight, 300, "folder weight counts matches only");
    }
}
