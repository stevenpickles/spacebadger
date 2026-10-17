//! Viewport-scoped nested treemap layout.
//!
//! - Squarified rows (Bruls, Huizing, van Wijk) inside each folder.
//! - Level of detail: the largest folders are expanded first, and expansion
//!   stops at a rectangle budget or when a folder is too small to show its
//!   contents.
//! - Children too small to see are merged into one "other small items"
//!   rectangle with their true combined area; nothing is inflated.
//! - Sibling order is kept stable between layouts while a scan runs and is
//!   re-sorted only when sizes drift past [`REORDER_HYSTERESIS`]. Only
//!   children drawn on their own are ordered; the rest are just summed.
//! - With a filename [`Search`], areas come from matching files only, and
//!   folders without matches are left out.
//! - Every rectangle carries its [`GitClass`] in its flags, passed down from
//!   folder to contents, so drawing needs no lookups.
//! - While a rescan runs, folders not yet listed take the area they had in
//!   the previous scan (see [`Tree::set_prior`]) on the unfiltered logical
//!   map, and their rectangles are marked [`LayoutRect::provisional`]. The
//!   rectangles' weights and the view's total stay what was measured. Once
//!   every folder is listed nothing is estimated, so the final layout is
//!   the exact one.
//!
//! Coordinates are in the caller's units (CSS pixels for the interface).
//! Rectangles are emitted parent before child, so drawing in order paints
//! nested content on top and hit testing in reverse finds the deepest one.

use crate::git::GitClass;
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

    /// The area `node` is given beyond its weight while a rescan runs: the
    /// previous scan's size of the folders at or under it not yet listed
    /// (see [`Tree::set_prior`]). Snapshots keep logical sizes only, so
    /// only the logical map uses them, and never a filtered one.
    pub(crate) fn estimate(self, tree: &Tree, filter: Option<&Search>, node: NodeId) -> u64 {
        match (self, filter) {
            (Self::Logical, None) => tree.pending_estimate(node),
            _ => 0,
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
    /// A folder whose own listing failed, was cancelled, or was read only
    /// in part (see [`Tree::fully_listed`]).
    pub const INCOMPLETE: u8 = 1 << 3;
    pub const CLOUD: u8 = 1 << 4;
    pub const HARDLINKED: u8 = 1 << 5;
    /// Bits 6–7 hold the rectangle's [`GitClass`](crate::git::GitClass):
    /// `(flags & GIT_MASK) >> GIT_SHIFT`.
    pub const GIT_SHIFT: u8 = 6;
    pub const GIT_MASK: u8 = 0b11 << GIT_SHIFT;
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
    /// The measured weight. The area is in proportion to it, except while
    /// `provisional`.
    pub weight: u64,
    /// Number of merged items for [`RectKind::OtherSmall`], else 1. 0 for
    /// the estimated filler of a folder still being listed: the part of
    /// its earlier size nothing found so far stands for (always
    /// `provisional`, weight 0).
    pub count: u32,
    /// Part of the area stands for folders not yet listed, sized from the
    /// previous scan (see [`Tree::set_prior`]); `weight` doesn't count it.
    pub provisional: bool,
}

impl LayoutRect {
    /// The folder whose contents this rectangle is part of. Siblings are
    /// placed consecutively, largest first.
    pub fn parent(&self, tree: &Tree) -> NodeId {
        match self.kind {
            RectKind::OtherSmall => self.node,
            _ => tree.parent(self.node).unwrap_or(NodeId::ROOT),
        }
    }
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
///
/// Only children drawn on their own are remembered, so a folder of millions
/// of tiny files costs as little as one of a few files. The cache holds at
/// most [`ORDER_CACHE_ENTRIES`] children in all and forgets the folders used
/// least recently to stay within that.
#[derive(Debug)]
pub struct OrderCache {
    /// Folder → its children's ranks in the last order used.
    folders: HashMap<NodeId, Ranks>,
    /// Children remembered across all folders.
    entries: usize,
    limit: usize,
    /// Advances with every folder ordered, to find the least recently used.
    clock: u64,
}

#[derive(Debug)]
struct Ranks {
    /// (child, rank), sorted by child.
    rank: Box<[(NodeId, u32)]>,
    used: u64,
}

impl Ranks {
    fn get(&self, id: NodeId) -> Option<u32> {
        self.rank
            .binary_search_by_key(&id, |&(c, _)| c)
            .ok()
            .map(|i| self.rank[i].1)
    }
}

/// Upper bound on remembered children across all folders, 8 bytes each, a
/// folder counting as [`FOLDER_COST`] more. One folder may use at most half
/// of it, about 520,000 children, so it never pushes out every other folder;
/// a folder drawing more is sorted afresh each time.
pub const ORDER_CACHE_ENTRIES: usize = 1 << 20;

/// A folder's own entry in the cache, in children: its map slot with the
/// map's spare capacity, and its allocation's overhead, about 96 bytes.
const FOLDER_COST: usize = 12;

impl Default for OrderCache {
    fn default() -> Self {
        Self::with_limit(ORDER_CACHE_ENTRIES)
    }
}

impl OrderCache {
    /// A cache holding at most `limit` children.
    pub fn with_limit(limit: usize) -> Self {
        Self {
            folders: HashMap::new(),
            entries: 0,
            limit,
            clock: 0,
        }
    }

    pub fn clear(&mut self) {
        self.folders.clear();
        self.entries = 0;
    }

    /// Number of children remembered.
    pub fn entries(&self) -> usize {
        self.entries
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
        self.clock += 1;
        if stable && let Some(ranks) = self.folders.get_mut(&dir) {
            ranks.used = self.clock;
            let mut known = Vec::with_capacity(children.len());
            let mut new = Vec::new();
            for &(id, w) in &children {
                match ranks.get(id) {
                    Some(r) => known.push((r, id, w)),
                    None => new.push((id, w)),
                }
            }
            let unchanged = new.is_empty();
            known.sort_unstable_by_key(|&(r, _, _)| r);
            let mut known: Vec<_> = known.into_iter().map(|(_, id, w)| (id, w)).collect();
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
        if let Some(old) = self.folders.remove(&dir) {
            self.entries -= old.rank.len();
        }
        // An empty order needs no remembering, and one larger than half the
        // cache isn't kept (the folder is sorted afresh each time instead),
        // so that remembering it leaves room for at least a quarter of the
        // cache's worth of other folders.
        let cost = children.len() + FOLDER_COST;
        if children.is_empty() || cost > self.limit / 2 {
            return;
        }
        if self.cost() + cost > self.limit {
            self.evict(self.limit - cost);
        }
        let mut rank: Box<[(NodeId, u32)]> = children
            .iter()
            .enumerate()
            .map(|(i, &(id, _))| (id, i as u32))
            .collect();
        rank.sort_unstable_by_key(|&(id, _)| id);
        self.entries += rank.len();
        self.folders.insert(
            dir,
            Ranks {
                rank,
                used: self.clock,
            },
        );
    }

    /// Children remembered, plus each folder's own entry.
    fn cost(&self) -> usize {
        self.entries + FOLDER_COST * self.folders.len()
    }

    /// Forgets the least recently used folders until at most `keep` of
    /// [`OrderCache::cost`] remains, less a quarter of the limit, so
    /// evictions are infrequent.
    fn evict(&mut self, keep: usize) {
        let keep = keep.saturating_sub(self.limit / 4);
        let mut by_use: Vec<(u64, NodeId)> =
            self.folders.iter().map(|(&d, r)| (r.used, d)).collect();
        by_use.sort_unstable();
        for (_, dir) in by_use {
            if self.cost() <= keep {
                break;
            }
            if let Some(old) = self.folders.remove(&dir) {
                self.entries -= old.rank.len();
            }
        }
        self.folders.shrink_to_fit();
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
    ctx.fill(view, full, 1, tree.git_class(view));
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

/// Children of `dir` in an "other small items" region of `count` items,
/// largest first: the `count` smallest children with a weight, since the
/// layout merges every child below its area threshold. Also returns how many
/// children have no weight (they're never drawn).
pub fn small_children(
    tree: &Tree,
    dir: NodeId,
    metric: Metric,
    filter: Option<&Search>,
    count: usize,
) -> (Vec<(NodeId, u64)>, usize) {
    // Chosen by area weight, as the layout chose them (so while a rescan
    // runs, folders not listed yet are among them by their estimates), but
    // reported with their measured weight.
    let mut weighted = Vec::new();
    let mut empty = 0;
    for c in tree.children(dir) {
        // With a filter, only children that contain matches belong here.
        if filter.is_some_and(|s| s.totals(c).files == 0) {
            continue;
        }
        let weight = metric.weight_in(tree, filter, c);
        match weight + metric.estimate(tree, filter, c) {
            0 => empty += 1,
            area => weighted.push((c, area, weight)),
        }
    }
    weighted.sort_unstable_by_key(|&(id, area, _)| (Reverse(area), id.index()));
    let start = weighted.len().saturating_sub(count);
    let small = weighted[start..].iter().map(|&(c, _, w)| (c, w)).collect();
    (small, empty)
}

/// Every file anywhere under `dir` with a weight, in tree order (matching
/// files only when filtered), and how many other files have no weight.
/// Rank them with [`crate::search::Ranked::from_files`].
pub fn files_under(
    tree: &Tree,
    dir: NodeId,
    metric: Metric,
    filter: Option<&Search>,
) -> (Vec<NodeId>, usize) {
    let mut files = Vec::new();
    let mut empty = 0;
    let mut stack = vec![dir];
    while let Some(d) = stack.pop() {
        for c in tree.children(d) {
            // Folders without matches can be skipped whole.
            if filter.is_some_and(|s| s.totals(c).files == 0) {
                continue;
            }
            if tree.is_dir(c) {
                stack.push(c);
            } else if metric.weight_in(tree, filter, c) == 0 {
                empty += 1;
            } else {
                files.push(c);
            }
        }
    }
    (files, empty)
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
        self.fill(
            r.node,
            inner,
            r.depth.saturating_add(1),
            git_class_of(r.flags),
        );
    }

    /// Squarifies `dir`'s children into `area`; `class` is `dir`'s Git
    /// class, which its contents have unless they have entries of their own.
    /// The area `node` is given: its weight, plus while a rescan runs the
    /// estimate for what it holds that isn't listed yet.
    fn area_weight(&self, node: NodeId) -> (u64, u64) {
        let weight = self.metric.weight_in(self.tree, self.filter, node);
        let estimate = self.metric.estimate(self.tree, self.filter, node);
        (weight + estimate, estimate)
    }

    fn fill(&mut self, dir: NodeId, area: Area, depth: u8, class: GitClass) {
        let tree = self.tree;
        let (total, _) = self.area_weight(dir);
        if total == 0 {
            return;
        }
        let scale = f64::from(area.w) * f64::from(area.h) / total as f64;
        let min_area = f64::from(self.params.min_area);

        // Whether a child is drawn depends only on its own weight, so the
        // ones too small to see are counted up front and only the rest are
        // ordered: with a positive `min_area`, never more than the area over
        // `min_area`. Children are ordered and placed by their area weight,
        // which is their weight once the scan has finished.
        let mut visible = Vec::new();
        let mut small = Small::default();
        let mut children = 0u64;
        for c in tree.children(dir) {
            match self.area_weight(c) {
                (0, _) => {}
                (w, estimate) if (w as f64 * scale) < min_area => {
                    children += w;
                    small.area += w;
                    small.weight += w - estimate;
                    small.count += 1;
                    small.provisional |= estimate > 0;
                }
                (w, _) => {
                    children += w;
                    visible.push((c, w));
                }
            }
        }
        let visible = self.cache.arrange(dir, visible, self.stable);
        let mut items: Vec<Item> = Vec::with_capacity(visible.len() + 2);
        items.extend(visible.into_iter().map(|(node, w)| Item::Node(node, w)));
        if small.count > 0 {
            items.push(Item::Other(small));
        }
        // A folder still being listed keeps the part of its estimate its
        // children haven't taken over yet: an estimated filler holds it, so
        // no child is stretched over it.
        let rest = total.saturating_sub(children);
        if rest > 0 && self.metric.estimate(tree, self.filter, dir) > 0 {
            items.push(Item::Other(Small {
                area: rest,
                weight: 0,
                count: 0,
                provisional: true,
            }));
        }

        let areas: Vec<f64> = items.iter().map(|i| i.area() as f64 * scale).collect();
        let mut placed = Vec::with_capacity(items.len());
        squarify(&areas, area, &mut placed);
        for (item, a) in items.into_iter().zip(placed) {
            let rect = match item {
                Item::Node(node, area) => {
                    let class = tree.git_own(node).unwrap_or(class);
                    let weight = self.metric.weight_in(tree, self.filter, node);
                    let mut rect = node_rect(tree, node, weight, a, depth, class);
                    rect.provisional = area != weight;
                    rect
                }
                Item::Other(small) => LayoutRect {
                    node: dir,
                    x: a.x,
                    y: a.y,
                    w: a.w,
                    h: a.h,
                    depth,
                    kind: RectKind::OtherSmall,
                    flags: git_flags(class),
                    weight: small.weight,
                    count: small.count,
                    provisional: small.provisional,
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
}

/// [`LayoutRect::flags`] bits for a Git class.
fn git_flags(class: GitClass) -> u8 {
    (class as u8) << rect_flags::GIT_SHIFT
}

/// The Git class [`LayoutRect::flags`] carry.
pub fn git_class_of(flags: u8) -> GitClass {
    GitClass::from_u8((flags & rect_flags::GIT_MASK) >> rect_flags::GIT_SHIFT)
}

/// `class` is the node's Git class.
fn node_rect(
    tree: &Tree,
    node: NodeId,
    weight: u64,
    a: Area,
    depth: u8,
    class: GitClass,
) -> LayoutRect {
    let node_flags = tree.flags(node);
    let mut f = git_flags(class);
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
            // Listed, but some entries couldn't be read.
            DirState::Listed if !tree.fully_listed(node) => f |= rect_flags::INCOMPLETE,
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
        provisional: false,
    }
}

/// Children merged into one "other small items" rectangle.
#[derive(Default)]
struct Small {
    /// Their area weight (see [`Ctx::area_weight`]).
    area: u64,
    /// Their measured weight.
    weight: u64,
    count: u32,
    /// Some of the area is estimated.
    provisional: bool,
}

enum Item {
    /// A child and its area weight.
    Node(NodeId, u64),
    Other(Small),
}

impl Item {
    fn area(&self) -> u64 {
        match self {
            Self::Node(_, w) => *w,
            Self::Other(small) => small.area,
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
    use crate::git::{GitRoot, RootKind};
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
        tree.add_file(parent, OsStr::new(name), sizes(bytes), None, 0)
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
    fn folders_read_in_part_are_marked_incomplete() {
        let mut t = Tree::new("/r");
        let whole = dir(&mut t, NodeId::ROOT, "whole");
        let partial = dir(&mut t, NodeId::ROOT, "partial");
        let failed = dir(&mut t, NodeId::ROOT, "failed");
        file(&mut t, whole, "x", 500);
        file(&mut t, partial, "y", 500);
        file(&mut t, failed, "z", 500);
        t.mark_unreadable_entries(partial);
        t.set_dir_state(failed, DirState::Failed);
        let l = run(&t, NodeId::ROOT, &LayoutParams::new(300.0, 200.0));
        let incomplete = |d: NodeId| {
            let r = l.rects.iter().find(|r| r.node == d).unwrap();
            r.flags & rect_flags::INCOMPLETE != 0
        };
        assert!(!incomplete(whole));
        assert!(incomplete(partial));
        assert!(incomplete(failed));
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
        t.add_file(NodeId::ROOT, OsStr::new("unknown"), unknown, None, 0)
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
    fn small_children_are_the_merged_ones_largest_first() {
        let mut t = Tree::new("/r");
        file(&mut t, NodeId::ROOT, "big", 1_000_000);
        let a = file(&mut t, NodeId::ROOT, "a", 3);
        let b = file(&mut t, NodeId::ROOT, "b", 7);
        file(&mut t, NodeId::ROOT, "empty", 0);
        let l = run(&t, NodeId::ROOT, &LayoutParams::new(100.0, 100.0));
        let other = l
            .rects
            .iter()
            .find(|r| r.kind == RectKind::OtherSmall)
            .expect("small items merged");
        let (items, empty) = small_children(
            &t,
            NodeId::ROOT,
            Metric::Allocated,
            None,
            other.count as usize,
        );
        assert_eq!(items, [(b, 7), (a, 3)]);
        assert_eq!(items.iter().map(|i| i.1).sum::<u64>(), other.weight);
        assert_eq!(empty, 1);
    }

    #[test]
    fn files_under_reaches_every_level_and_honors_the_filter() {
        use crate::search::{Matcher, Ranked, Search};
        let mut t = Tree::new("/r");
        let a = dir(&mut t, NodeId::ROOT, "a");
        let deep = dir(&mut t, a, "deep");
        let top = file(&mut t, NodeId::ROOT, "top.txt", 50);
        let mid = file(&mut t, a, "mid.jpg", 200);
        let low = file(&mut t, deep, "low.jpg", 900);
        file(&mut t, deep, "zero.jpg", 0);

        let (files, empty) = files_under(&t, NodeId::ROOT, Metric::Logical, None);
        let mut ranked = Ranked::from_files(files, Metric::Logical);
        assert_eq!(ranked.top(&t, 10), [low, mid, top]);
        assert_eq!(empty, 1);

        let (files, _) = files_under(&t, a, Metric::Logical, None);
        assert_eq!(files.len(), 2, "only files under the folder");

        let mut s = Search::new(Matcher::new("*.jpg").unwrap());
        s.catch_up(&t, usize::MAX);
        let (files, empty) = files_under(&t, NodeId::ROOT, Metric::Logical, Some(&s));
        let mut ranked = Ranked::from_files(files, Metric::Logical);
        assert_eq!(ranked.top(&t, 10), [low, mid]);
        assert_eq!(empty, 1, "the empty match");
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

    #[test]
    fn rects_carry_the_git_class_from_folder_to_contents() {
        let mut t = Tree::new("/r");
        let repo = dir(&mut t, NodeId::ROOT, "repo");
        let dot = dir(&mut t, repo, ".git");
        let pack = file(&mut t, dot, "pack", 4_000);
        let src = dir(&mut t, repo, "src");
        let main = file(&mut t, src, "main.rs", 3_000);
        // Many tiny files, merged into one small-items rectangle.
        for i in 0..50 {
            file(&mut t, src, &format!("tiny{i}"), 1);
        }
        let wt = dir(&mut t, repo, "wt");
        let pointer = file(&mut t, wt, ".git", 1_000);
        let wt_file = file(&mut t, wt, "a.txt", 2_000);
        let loose = file(&mut t, NodeId::ROOT, "loose", 5_000);
        t.set_git(GitRoot::new(repo, RootKind::Repository));
        t.set_git(GitRoot::new(dot, RootKind::DotGit));
        t.set_git(GitRoot::new(wt, RootKind::LinkedWorktree));
        t.set_git(GitRoot::new(pointer, RootKind::DotGit));

        let class = |l: &Layout, n: NodeId, kind: RectKind| {
            let r = l.rects.iter().find(|r| r.node == n && r.kind == kind);
            git_class_of(r.unwrap_or_else(|| panic!("no rect for {n:?}")).flags)
        };
        // Small enough that the tiny files are merged.
        let l = run(&t, NodeId::ROOT, &LayoutParams::new(200.0, 150.0));
        assert_eq!(class(&l, loose, RectKind::File), GitClass::Other);
        assert_eq!(class(&l, repo, RectKind::Folder), GitClass::Checkout);
        assert_eq!(class(&l, src, RectKind::Folder), GitClass::Checkout);
        assert_eq!(class(&l, main, RectKind::File), GitClass::Checkout);
        assert_eq!(
            class(&l, src, RectKind::OtherSmall),
            GitClass::Checkout,
            "small items take their folder's class"
        );
        assert_eq!(class(&l, dot, RectKind::Folder), GitClass::Metadata);
        assert_eq!(class(&l, pack, RectKind::File), GitClass::Metadata);
        assert_eq!(class(&l, wt, RectKind::Folder), GitClass::Worktree);
        assert_eq!(class(&l, wt_file, RectKind::File), GitClass::Worktree);
        assert_eq!(class(&l, pointer, RectKind::File), GitClass::Metadata);
        // The other bits are untouched.
        let r = l.rects.iter().find(|r| r.node == repo).unwrap();
        assert_eq!(
            r.flags & !rect_flags::GIT_MASK,
            rect_flags::HEADER | rect_flags::EXPANDED
        );

        // A view inside the repository starts from its class.
        let l = run(&t, src, &LayoutParams::new(800.0, 600.0));
        assert_eq!(class(&l, main, RectKind::File), GitClass::Checkout);
        let l = run(&t, wt, &LayoutParams::new(800.0, 600.0));
        assert_eq!(class(&l, wt_file, RectKind::File), GitClass::Worktree);
        assert_eq!(class(&l, pointer, RectKind::File), GitClass::Metadata);

        // Above the scan root, everything is in that repository.
        t.set_git_above("/".into(), GitRoot::new(NodeId::ROOT, RootKind::Repository));
        let l = run(&t, NodeId::ROOT, &LayoutParams::new(800.0, 600.0));
        assert_eq!(class(&l, loose, RectKind::File), GitClass::Checkout);
    }

    /// A snapshot of /r: big/(9,000: deep/(8,000)), mid/(900), tiny/(1),
    /// and 99 bytes of files.
    fn prior() -> std::sync::Arc<crate::prior::Prior> {
        let mut t = Tree::new("/r");
        let big = dir(&mut t, NodeId::ROOT, "big");
        let deep = dir(&mut t, big, "deep");
        let mid = dir(&mut t, NodeId::ROOT, "mid");
        let tiny = dir(&mut t, NodeId::ROOT, "tiny");
        file(&mut t, deep, "d", 8_000);
        file(&mut t, big, "b", 1_000);
        file(&mut t, mid, "m", 900);
        file(&mut t, tiny, "t", 1);
        file(&mut t, NodeId::ROOT, "f", 99);
        t.set_dir_state(NodeId::ROOT, DirState::Listed);
        std::sync::Arc::new(crate::prior::Prior::new(
            crate::snapshot::Snapshot::capture(&t, 0, true),
        ))
    }

    fn logical(t: &Tree, params: &LayoutParams) -> Layout {
        layout(
            t,
            NodeId::ROOT,
            Metric::Logical,
            None,
            params,
            &mut OrderCache::default(),
            false,
        )
    }

    #[test]
    fn unlisted_folders_take_their_prior_area_flagged_but_weigh_nothing() {
        let mut t = Tree::new("/r");
        t.set_prior(prior());
        // The root is listed; its folders are still waiting.
        let big = t.add_dir(NodeId::ROOT, OsStr::new("big")).unwrap();
        let mid = t.add_dir(NodeId::ROOT, OsStr::new("mid")).unwrap();
        let tiny = t.add_dir(NodeId::ROOT, OsStr::new("tiny")).unwrap();
        let f = file(&mut t, NodeId::ROOT, "f", 100);
        t.set_dir_state(NodeId::ROOT, DirState::Listed);

        let params = LayoutParams::new(100.0, 100.0);
        let l = logical(&t, &params);
        assert_eq!(l.total, 100, "the view's weight is what was measured");
        let rect = |n: NodeId| *l.rects.iter().find(|r| r.node == n).unwrap();
        let area = |r: LayoutRect| r.w * r.h;
        // 10,000 px² for 9,000 + 900 + 1 estimated and 100 measured bytes.
        assert!((area(rect(big)) - 9_000.0).abs() < 1.0, "{:?}", rect(big));
        assert!((area(rect(mid)) - 900.0).abs() < 1.0);
        assert!((area(rect(f)) - 100.0).abs() < 1.0);
        for n in [big, mid] {
            let r = rect(n);
            assert!(r.provisional && r.weight == 0 && r.flags & rect_flags::PENDING != 0);
        }
        assert!(!rect(f).provisional);
        // tiny is too small to draw: merged, the merge flagged, its weight
        // still measured only.
        let other = l
            .rects
            .iter()
            .find(|r| r.kind == RectKind::OtherSmall)
            .unwrap();
        assert_eq!((other.count, other.weight, other.provisional), (1, 0, true));
        let _ = tiny;

        // Not on the allocated map, which the snapshot can't speak for, nor
        // a filtered one.
        let allocated = run(&t, NodeId::ROOT, &params);
        assert!(allocated.rects.iter().all(|r| !r.provisional));
        assert_eq!(allocated.rects.len(), 1);
        let mut s = crate::search::Search::new(crate::search::Matcher::new("*").unwrap());
        s.catch_up(&t, usize::MAX);
        let filtered = layout(
            &t,
            NodeId::ROOT,
            Metric::Logical,
            Some(&s),
            &params,
            &mut OrderCache::default(),
            false,
        );
        assert!(filtered.rects.iter().all(|r| !r.provisional));

        // big listed: its contents take over, deep still estimated inside.
        let deep = t.add_dir(big, OsStr::new("deep")).unwrap();
        file(&mut t, big, "b", 1_000);
        t.set_dir_state(big, DirState::Listed);
        let l = logical(&t, &params);
        let r = l.rects.iter().find(|r| r.node == big).unwrap();
        assert!(r.provisional && r.weight == 1_000);
        assert!(l.rects.iter().any(|r| r.node == deep && r.provisional));
    }

    /// Review of #62: a folder still being listed counted its own estimate
    /// beside its children's, and its last child took over the difference
    /// (a 99-byte file covered 92% of the map, unflagged).
    #[test]
    fn a_folder_being_listed_stretches_no_child_over_its_estimate() {
        let mut t = Tree::new("/r");
        t.set_prior(prior());
        // The root's listing is under way: mid (estimated 900) and f (99).
        let mid = t.add_dir(NodeId::ROOT, OsStr::new("mid")).unwrap();
        let f = file(&mut t, NodeId::ROOT, "f", 99);
        let params = LayoutParams::new(100.0, 100.0);
        let l = logical(&t, &params);
        let area = |r: &LayoutRect| r.w * r.h;
        let rf = l.rects.iter().find(|r| r.node == f).unwrap();
        let rm = l.rects.iter().find(|r| r.node == mid).unwrap();
        // Of the root's 10,000 bytes then: f 99, mid 900, and 9,001 not
        // found yet, held by an estimated filler.
        assert!((area(rf) - 99.0).abs() < 1.0, "{rf:?}");
        assert!(!rf.provisional);
        assert!((area(rm) - 900.0).abs() < 1.0 && rm.provisional);
        let filler = l
            .rects
            .iter()
            .find(|r| r.kind == RectKind::OtherSmall && r.count == 0)
            .expect("a filler for the rest");
        assert_eq!((filler.node, filler.weight), (NodeId::ROOT, 0));
        assert!(filler.provisional && (area(filler) - 9_001.0).abs() < 1.0);
        let total: f32 = l.rects.iter().filter(|r| r.depth == 1).map(area).sum();
        assert!((total - 10_000.0).abs() < 1.0);

        // Once the root's listing ends it has nothing left to fill; mid,
        // still waiting, is all filler inside.
        t.set_dir_state(NodeId::ROOT, DirState::Listed);
        let l = logical(&t, &params);
        let fillers: Vec<_> = l
            .rects
            .iter()
            .filter(|r| r.count == 0)
            .map(|r| r.node)
            .collect();
        assert_eq!(fillers, [mid]);
    }

    #[test]
    fn small_items_are_chosen_by_area_but_reported_measured() {
        let mut t = Tree::new("/r");
        t.set_prior(prior());
        let tiny = t.add_dir(NodeId::ROOT, OsStr::new("tiny")).unwrap();
        file(&mut t, NodeId::ROOT, "f", 1_000_000);
        t.set_dir_state(NodeId::ROOT, DirState::Listed);
        let l = logical(&t, &LayoutParams::new(100.0, 100.0));
        let other = l
            .rects
            .iter()
            .find(|r| r.kind == RectKind::OtherSmall)
            .unwrap();
        assert_eq!((other.count, other.weight, other.provisional), (1, 0, true));
        // tiny weighs nothing yet but has an estimated area: it's the merged
        // item, at its measured size.
        let (items, empty) = small_children(
            &t,
            NodeId::ROOT,
            Metric::Logical,
            None,
            other.count as usize,
        );
        assert_eq!((items, empty), (vec![(tiny, 0)], 0));
    }

    #[test]
    fn once_every_folder_is_listed_the_layout_is_exactly_the_unestimated_one() {
        let build = |with_prior: bool| {
            let mut t = Tree::new("/r");
            if with_prior {
                t.set_prior(prior());
            }
            let big = t.add_dir(NodeId::ROOT, OsStr::new("big")).unwrap();
            let mid = t.add_dir(NodeId::ROOT, OsStr::new("mid")).unwrap();
            let new = t.add_dir(NodeId::ROOT, OsStr::new("new")).unwrap();
            file(&mut t, NodeId::ROOT, "f", 100);
            t.set_dir_state(NodeId::ROOT, DirState::Listed);
            let deep = t.add_dir(big, OsStr::new("deep")).unwrap();
            // Things changed since the snapshot.
            file(&mut t, big, "b", 3_000);
            file(&mut t, deep, "d", 50);
            file(&mut t, mid, "m", 7_000);
            file(&mut t, new, "n", 500);
            for d in [big, deep, mid, new] {
                t.set_dir_state(d, DirState::Listed);
            }
            t
        };
        let params = LayoutParams::new(640.0, 480.0);
        let with = logical(&build(true), &params);
        let without = logical(&build(false), &params);
        assert!(same(&with, &without));
        assert!(with.rects.iter().all(|r| !r.provisional));
    }

    /// The layout as it was before small children were set aside up front:
    /// every child with a weight is ordered (and its rank remembered) before
    /// the small ones are merged. Kept to show the new one draws the same.
    mod reference {
        use super::super::*;

        #[derive(Default)]
        pub struct Cache {
            rank: HashMap<NodeId, HashMap<NodeId, u32>>,
        }

        impl Cache {
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
                let rank = children
                    .iter()
                    .enumerate()
                    .map(|(i, &(id, _))| (id, i as u32))
                    .collect();
                self.rank.insert(dir, rank);
            }
        }

        struct Ctx<'a> {
            tree: &'a Tree,
            metric: Metric,
            filter: Option<&'a Search>,
            params: &'a LayoutParams,
            cache: &'a mut Cache,
            stable: bool,
            rects: Vec<LayoutRect>,
            queue: BinaryHeap<Pending>,
        }

        pub fn layout(
            tree: &Tree,
            view: NodeId,
            metric: Metric,
            filter: Option<&Search>,
            params: &LayoutParams,
            cache: &mut Cache,
            stable: bool,
        ) -> Layout {
            let mut out = Layout {
                total: metric.weight_in(tree, filter, view),
                ..Layout::default()
            };
            if !tree.contains(view)
                || !tree.is_dir(view)
                || params.width <= 0.0
                || params.height <= 0.0
            {
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

        impl Ctx<'_> {
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
                    items.push(Item::Other(Small {
                        area: small.0,
                        weight: small.0,
                        count: small.1,
                        provisional: false,
                    }));
                }

                let areas: Vec<f64> = items.iter().map(|i| i.area() as f64 * scale).collect();
                let mut placed = Vec::with_capacity(items.len());
                squarify(&areas, area, &mut placed);
                for (item, a) in items.into_iter().zip(placed) {
                    let rect = match item {
                        // Classes looked up afresh, not passed down.
                        Item::Node(node, weight) => {
                            node_rect(tree, node, weight, a, depth, tree.git_class(node))
                        }
                        Item::Other(Small { weight, count, .. }) => LayoutRect {
                            node: dir,
                            x: a.x,
                            y: a.y,
                            w: a.w,
                            h: a.h,
                            depth,
                            kind: RectKind::OtherSmall,
                            flags: git_flags(tree.git_class(dir)),
                            weight,
                            count,
                            provisional: false,
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
        }
    }

    /// xorshift64*, so random trees are the same on every run and platform.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n.max(1)
        }
    }

    const EXTS: [&str; 4] = ["a", "b", "jpg", "txt"];

    /// A size with plenty of zeros, ties, and a long tail.
    fn random_size(rng: &mut Rng) -> u64 {
        match rng.below(10) {
            0 => 0,
            1..=3 => [1, 7, 100, 4096][rng.below(4) as usize],
            4..=7 => rng.below(10_000),
            _ => rng.below(1 << 30),
        }
    }

    fn random_file(rng: &mut Rng, t: &mut Tree, parent: NodeId, i: usize) -> NodeId {
        let name = format!("n{i}.{}", EXTS[rng.below(4) as usize]);
        let extra = match rng.below(20) {
            0 => flags::CLOUD,
            1 => flags::HARDLINKED,
            _ => 0,
        };
        let s = random_size(rng);
        let allocated = if rng.below(30) == 0 {
            Allocated::UNKNOWN
        } else {
            Allocated::known(s)
        };
        let sizes = FileSizes {
            logical: s,
            allocated,
        };
        t.add_file(parent, OsStr::new(&name), sizes, None, extra)
            .unwrap()
    }

    /// A random tree: nested folders in every state, some of them wide with
    /// equal files, the rest with random sizes.
    fn random_tree(rng: &mut Rng) -> Tree {
        let mut t = Tree::new("/r");
        let mut dirs = vec![(NodeId::ROOT, 0)];
        let mut i = 0;
        while let Some((d, level)) = dirs.pop() {
            let wide = t.len() < 10_000 && rng.below(8) == 0;
            let n = if wide {
                2_000 + rng.below(3_000)
            } else {
                rng.below(40)
            };
            let equal = 1 + rng.below(50);
            for _ in 0..n {
                i += 1;
                if !wide && level < 4 && rng.below(6) == 0 {
                    let sub = t
                        .add_dir(
                            d,
                            OsStr::new(&format!("d{i}.{}", EXTS[rng.below(4) as usize])),
                        )
                        .unwrap();
                    dirs.push((sub, level + 1));
                } else if wide {
                    let name = format!("e{i}.{}", EXTS[rng.below(4) as usize]);
                    t.add_file(d, OsStr::new(&name), sizes(equal), None, 0)
                        .unwrap();
                } else {
                    random_file(rng, &mut t, d, i);
                }
            }
            let state = match rng.below(10) {
                0 => DirState::Pending,
                1 => DirState::Failed,
                2 => DirState::NotScanned,
                _ => DirState::Listed,
            };
            t.set_dir_state(d, state);
            if state == DirState::Listed && rng.below(8) == 0 {
                t.mark_unreadable_entries(d);
            }
        }
        // Git entries here and there, nested ones included.
        for i in 0..t.len() {
            if rng.below(40) == 0 {
                let kind = [
                    RootKind::Repository,
                    RootKind::LinkedWorktree,
                    RootKind::DotGit,
                    RootKind::Unresolved,
                ][rng.below(4) as usize];
                t.set_git(GitRoot::new(NodeId::from_index(i as u32), kind));
            }
        }
        t
    }

    fn random_params(rng: &mut Rng) -> LayoutParams {
        let mut p = LayoutParams::new(
            [0.0, 3.0, 50.0, 400.0, 1920.0][rng.below(5) as usize],
            [0.0, 5.0, 37.5, 300.0, 1080.0][rng.below(5) as usize],
        );
        p.min_area = [0.0, 1.0, 12.0, 300.0][rng.below(4) as usize];
        p.max_rects = [5, 200, 30_000][rng.below(3) as usize];
        p
    }

    fn filters(t: &Tree) -> Vec<Option<crate::search::Search>> {
        use crate::search::{Matcher, Search};
        let mut out = vec![None];
        for q in [
            "*.jpg",
            "b",
            "*.a; !dir:d1*",
            "size:>1KB",
            "git:repo; !git:metadata",
            "nothing matches",
        ] {
            let mut s = Search::new(Matcher::new(q).unwrap());
            s.catch_up(t, usize::MAX);
            out.push(Some(s));
        }
        out
    }

    fn same(a: &Layout, b: &Layout) -> bool {
        a.rects == b.rects && a.total == b.total && a.truncated == b.truncated
    }

    #[test]
    fn final_and_fresh_layouts_match_the_reference() {
        let mut rng = Rng(0x5eed_1a70);
        for round in 0..40 {
            let t = random_tree(&mut rng);
            let dirs: Vec<NodeId> = (0..t.len() as u32)
                .map(NodeId::from_index)
                .filter(|&n| t.is_dir(n))
                .collect();
            for filter in filters(&t) {
                for _ in 0..4 {
                    let view = dirs[rng.below(dirs.len() as u64) as usize];
                    let params = random_params(&mut rng);
                    let metric = if rng.below(2) == 0 {
                        Metric::Allocated
                    } else {
                        Metric::Logical
                    };
                    let f = filter.as_ref();
                    // A fresh layout, stable or not, and a final one with a
                    // cache already holding an order: all sort afresh.
                    let mut cache = OrderCache::default();
                    let mut ref_cache = reference::Cache::default();
                    for stable in [true, false, false] {
                        let new = layout(&t, view, metric, f, &params, &mut cache, stable);
                        let old =
                            reference::layout(&t, view, metric, f, &params, &mut ref_cache, stable);
                        assert!(
                            same(&new, &old),
                            "round {round}, {params:?}, stable {stable}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn stable_layouts_match_the_reference_when_every_child_is_drawn() {
        // With no small children the two caches see the same siblings, so
        // keeping order while a scan grows the tree must agree step by step.
        let mut rng = Rng(0xab1e);
        for round in 0..30 {
            let mut t = random_tree(&mut rng);
            let mut params = random_params(&mut rng);
            params.min_area = 0.0;
            let mut cache = OrderCache::default();
            let mut ref_cache = reference::Cache::default();
            let mut i = t.len();
            for step in 0..12 {
                let stable = step < 11;
                let new = layout(
                    &t,
                    NodeId::ROOT,
                    Metric::Allocated,
                    None,
                    &params,
                    &mut cache,
                    stable,
                );
                let old = reference::layout(
                    &t,
                    NodeId::ROOT,
                    Metric::Allocated,
                    None,
                    &params,
                    &mut ref_cache,
                    stable,
                );
                assert!(same(&new, &old), "round {round}, step {step}");
                // Grow some files and add others, as a running scan does.
                let dirs: Vec<NodeId> = (0..t.len() as u32)
                    .map(NodeId::from_index)
                    .filter(|&n| t.is_dir(n))
                    .collect();
                for _ in 0..20 {
                    let n = NodeId::from_index(rng.below(t.len() as u64) as u32);
                    if t.is_dir(n) {
                        i += 1;
                        random_file(&mut rng, &mut t, n, i);
                    } else {
                        let grown = t.logical(n) + random_size(&mut rng);
                        t.update_file(n, sizes(grown));
                    }
                }
                let d = dirs[rng.below(dirs.len() as u64) as usize];
                i += 1;
                random_file(&mut rng, &mut t, d, i);
            }
        }
    }

    #[test]
    fn a_wide_folder_of_equal_files_orders_only_what_is_drawn() {
        let mut t = Tree::new("/r");
        let wide = dir(&mut t, NodeId::ROOT, "wide");
        for i in 0..200_000 {
            file(&mut t, wide, &format!("f{i}"), 4096);
        }
        file(&mut t, NodeId::ROOT, "big", 200_000 * 4096);
        let params = LayoutParams::new(400.0, 300.0);
        let mut cache = OrderCache::default();
        let new = layout(
            &t,
            NodeId::ROOT,
            Metric::Allocated,
            None,
            &params,
            &mut cache,
            true,
        );
        let old = reference::layout(
            &t,
            NodeId::ROOT,
            Metric::Allocated,
            None,
            &params,
            &mut reference::Cache::default(),
            true,
        );
        assert!(same(&new, &old));
        let other = new
            .rects
            .iter()
            .find(|r| r.kind == RectKind::OtherSmall)
            .unwrap();
        assert_eq!((other.node, other.count), (wide, 200_000));
        assert_eq!(cache.entries(), 2, "the root's two children only");
        assert_eq!(cache.folders.len(), 1, "nothing drawn in the wide folder");
    }

    #[test]
    fn a_child_of_exactly_the_minimum_area_is_drawn_as_before() {
        let mut t = Tree::new("/r");
        let edge = file(&mut t, NodeId::ROOT, "edge", 12);
        file(&mut t, NodeId::ROOT, "below", 11);
        file(&mut t, NodeId::ROOT, "rest", 10_000 - 23);
        let params = LayoutParams::new(100.0, 100.0);
        let new = run(&t, NodeId::ROOT, &params);
        let old = reference::layout(
            &t,
            NodeId::ROOT,
            Metric::Allocated,
            None,
            &params,
            &mut reference::Cache::default(),
            false,
        );
        assert!(same(&new, &old));
        assert!(new.rects.iter().any(|r| r.node == edge));
        let other = new.rects.last().unwrap();
        assert_eq!((other.kind, other.weight), (RectKind::OtherSmall, 11));
    }

    #[test]
    fn small_children_changing_order_does_not_reorder_the_drawn_ones() {
        let mut t = Tree::new("/r");
        let a = file(&mut t, NodeId::ROOT, "a", 10_000);
        let b = file(&mut t, NodeId::ROOT, "b", 9_000);
        let tiny = file(&mut t, NodeId::ROOT, "tiny", 1);
        let params = LayoutParams::new(100.0, 100.0);
        let mut cache = OrderCache::default();
        let order = |t: &Tree, cache: &mut OrderCache| {
            layout(
                t,
                NodeId::ROOT,
                Metric::Allocated,
                None,
                &params,
                cache,
                true,
            )
            .rects
            .iter()
            .filter(|r| r.kind != RectKind::OtherSmall)
            .map(|r| r.node)
            .collect::<Vec<_>>()
        };
        assert_eq!(order(&t, &mut cache), [a, b]);
        // b passes a, but not by enough to move while scanning; a tiny file
        // arriving larger than the one before used to force a full re-sort.
        t.update_file(b, sizes(11_000));
        file(&mut t, NodeId::ROOT, "tiny2", 3);
        t.update_file(tiny, sizes(2));
        assert_eq!(order(&t, &mut cache), [a, b]);
    }

    #[test]
    fn the_order_cache_is_bounded_by_children_not_folders() {
        let mut t = Tree::new("/r");
        let mut dirs = Vec::new();
        for d in 0..50 {
            let sub = dir(&mut t, NodeId::ROOT, &format!("d{d}"));
            for i in 0..10 {
                file(&mut t, sub, &format!("f{i}"), 1000 + i);
            }
            dirs.push(sub);
        }
        let params = LayoutParams::new(400.0, 400.0);
        let mut cache = OrderCache::with_limit(100);
        for &d in &dirs {
            layout(&t, d, Metric::Allocated, None, &params, &mut cache, true);
            assert!(cache.cost() <= 100, "{}", cache.cost());
        }
        // The folder used last is still remembered; the first was forgotten.
        let last = *dirs.last().unwrap();
        assert!(cache.folders.contains_key(&last));
        assert!(!cache.folders.contains_key(&dirs[0]));
        // A folder costing more than half the cache isn't kept.
        let mut tiny = OrderCache::with_limit(40);
        layout(&t, last, Metric::Allocated, None, &params, &mut tiny, true);
        assert_eq!(tiny.entries(), 0);
        cache.clear();
        assert_eq!(cache.entries(), 0);
    }

    #[test]
    fn a_folder_filling_most_of_the_cache_does_not_push_out_the_others() {
        let mut t = Tree::new("/r");
        let big = dir(&mut t, NodeId::ROOT, "big");
        // 80 drawn children: with the folder's own cost, 92 of the 100.
        for i in 0..80 {
            file(&mut t, big, &format!("f{i}"), 1000);
        }
        let a = file(&mut t, NodeId::ROOT, "a", 100_000);
        let b = file(&mut t, NodeId::ROOT, "b", 90_000);
        let params = LayoutParams::new(1000.0, 1000.0);
        let mut cache = OrderCache::with_limit(100);
        let order = |t: &Tree, cache: &mut OrderCache| {
            layout(
                t,
                NodeId::ROOT,
                Metric::Allocated,
                None,
                &params,
                cache,
                true,
            )
            .rects
            .iter()
            .filter(|r| r.depth == 1)
            .map(|r| r.node)
            .collect::<Vec<_>>()
        };
        assert_eq!(order(&t, &mut cache), [a, b, big]);
        assert!(!cache.folders.contains_key(&big), "sorted afresh instead");
        // b passes a while scanning, but not by enough to move: the root's
        // order must still be remembered on every later layout.
        t.update_file(b, sizes(120_000));
        for _ in 0..3 {
            assert_eq!(order(&t, &mut cache), [a, b, big]);
        }
    }
}
