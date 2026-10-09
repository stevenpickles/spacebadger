//! One scan and the view state derived from it, plus conversions from the
//! scanner's types to the protocol's.

use sb_core::filetype::FileType;
use sb_core::layout::{self, LayoutParams, OrderCache, RectKind, rect_flags};
use sb_core::search::{Matcher, Search};
use sb_core::tree::{DirState, NodeId, Tree, flags};
use sb_protocol as proto;
use sb_scan::{NativeFs, OmissionReason, Progress, Scan, ScanState, native};
use std::cmp::Reverse;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

/// Files narrower or shorter than this (CSS px) get no label.
const FILE_LABEL_MIN: (f32, f32) = (40.0, 14.0);

/// Nodes a new search examines per hold of the tree's read lock, so the
/// scanner can keep writing while a large tree is searched.
const SEARCH_STEP: usize = 200_000;

/// Largest page of search results returned at once.
const MAX_PAGE: u32 = 500;

// Lock order: `search` before the tree, and `order` last.
pub struct Session {
    pub root: PathBuf,
    pub scan: Scan,
    order: Mutex<OrderCache>,
    search: Mutex<Option<ActiveSearch>>,
    /// Number of the most recently requested search.
    latest_search: AtomicU32,
}

struct ActiveSearch {
    id: u32,
    search: Search,
    /// Sibling order for the filtered map.
    order: OrderCache,
    /// Matches sorted by size, for the result list.
    sorted: Option<Sorted>,
}

struct Sorted {
    metric: layout::Metric,
    version: u64,
    files: Vec<NodeId>,
}

impl Session {
    pub fn start(root: PathBuf, on_status: impl Fn(proto::ScanStatus) + Send + 'static) -> Self {
        let display = display_path(&root);
        let scan = Scan::start(
            root.clone(),
            NativeFs::new(),
            native::scan_config(&root),
            move |p| {
                on_status(status(&display, p));
            },
        );
        Self {
            root,
            scan,
            order: Mutex::new(OrderCache::default()),
            search: Mutex::new(None),
            latest_search: AtomicU32::new(0),
        }
    }

    pub fn generation(&self) -> u64 {
        self.scan.generation()
    }

    pub fn started(&self) -> proto::ScanStarted {
        proto::ScanStarted {
            generation: self.generation(),
            root: display_path(&self.root),
        }
    }

    pub fn status(&self) -> proto::ScanStatus {
        status(&display_path(&self.root), &self.scan.progress())
    }

    /// Computes and encodes the treemap for one request.
    pub fn layout(&self, req: &proto::LayoutRequest) -> Result<Vec<u8>, String> {
        let progress = self.scan.progress();
        let finished = progress.state.is_finished();
        let metric = metric(req.metric);
        let params = LayoutParams::new(
            req.width.clamp(0.0, 16_384.0),
            req.height.clamp(0.0, 16_384.0),
        );
        let mut search = self.search.lock().map_err(|_| "search is unavailable")?;
        let tree = self.scan.tree();
        let tree = tree.read().map_err(|_| "scan data is unavailable")?;
        let view = node(&tree, req.view)?;
        // While scanning, keep sibling order stable; once finished, sort afresh.
        let result = match req.search {
            Some(id) => {
                let active = current_search(&mut search, id, &tree)?;
                layout::layout(
                    &tree,
                    view,
                    metric,
                    Some(&active.search),
                    &params,
                    &mut active.order,
                    !finished,
                )
            }
            None => {
                drop(search);
                let mut order = self
                    .order
                    .lock()
                    .map_err(|_| "layout state is unavailable")?;
                layout::layout(&tree, view, metric, None, &params, &mut order, !finished)
            }
        };

        let mut rects = Vec::with_capacity(result.rects.len());
        let mut names = Vec::new();
        for (i, r) in result.rects.iter().enumerate() {
            let labelled = match r.kind {
                RectKind::Folder => r.flags & rect_flags::HEADER != 0,
                RectKind::File => r.w >= FILE_LABEL_MIN.0 && r.h >= FILE_LABEL_MIN.1,
                RectKind::OtherSmall => false,
            };
            if labelled {
                names.push((i as u32, tree.name(r.node).to_string_lossy()));
            }
            rects.push(proto::wire::Rect {
                x: r.x,
                y: r.y,
                w: r.w,
                h: r.h,
                node: r.node.index() as u32,
                count: r.count,
                weight: r.weight,
                depth: r.depth,
                kind: r.kind as u8,
                flags: r.flags,
                file_type: if r.kind == RectKind::File {
                    FileType::of(tree.name(r.node), tree.logical(r.node)) as u8
                } else {
                    0
                },
            });
        }
        let labels: Vec<(u32, &str)> = names.iter().map(|(i, s)| (*i, s.as_ref())).collect();
        let mut header_flags = 0;
        if result.truncated {
            header_flags |= proto::wire::LAYOUT_TRUNCATED;
        }
        if finished {
            header_flags |= proto::wire::LAYOUT_FINAL;
        }
        let header = proto::wire::Header {
            generation: self.generation(),
            revision: progress.revision,
            request: req.request,
            view: req.view,
            total: result.total,
            flags: header_flags,
        };
        Ok(proto::wire::encode(&header, &rects, &labels))
    }

    /// Starts filtering by `query`, replacing any earlier search; an empty
    /// query clears the filter. Examines the tree in steps and gives up early
    /// if a newer search is requested meanwhile.
    pub fn search_set(&self, query: &str) -> Result<Option<proto::SearchSummary>, String> {
        let id = self.latest_search.fetch_add(1, Ordering::SeqCst) + 1;
        let superseded = || format!("search {id} was replaced by a newer one");
        let Some(matcher) = Matcher::new(query) else {
            let mut current = self.search.lock().map_err(|_| "search is unavailable")?;
            if self.latest_search.load(Ordering::SeqCst) == id {
                *current = None;
            }
            return Ok(None);
        };
        let tree_lock = self.scan.tree();
        let mut search = Search::new(matcher);
        loop {
            if self.latest_search.load(Ordering::SeqCst) != id {
                return Err(superseded());
            }
            let tree = tree_lock.read().map_err(|_| "scan data is unavailable")?;
            if search.catch_up(&tree, SEARCH_STEP) {
                break;
            }
        }
        let mut current = self.search.lock().map_err(|_| "search is unavailable")?;
        if self.latest_search.load(Ordering::SeqCst) != id {
            return Err(superseded());
        }
        let tree = tree_lock.read().map_err(|_| "scan data is unavailable")?;
        search.catch_up(&tree, usize::MAX);
        let active = current.insert(ActiveSearch {
            id,
            search,
            order: OrderCache::default(),
            sorted: None,
        });
        Ok(Some(self.summary(active)))
    }

    /// Matching totals for search `id`, brought up to date with the scan.
    pub fn search_summary(&self, id: u32) -> Result<proto::SearchSummary, String> {
        let mut search = self.search.lock().map_err(|_| "search is unavailable")?;
        let tree = self.scan.tree();
        let tree = tree.read().map_err(|_| "scan data is unavailable")?;
        let active = current_search(&mut search, id, &tree)?;
        Ok(self.summary(active))
    }

    fn summary(&self, active: &ActiveSearch) -> proto::SearchSummary {
        let totals = active.search.totals(NodeId::ROOT);
        proto::SearchSummary {
            generation: self.generation(),
            search: active.id,
            query: active.search.matcher().query().to_owned(),
            files: u64::from(totals.files),
            logical: totals.logical,
            allocated: totals.allocated,
            unknown_allocation_files: u64::from(totals.unknown),
        }
    }

    /// One page of matching files, largest first.
    pub fn search_results(
        &self,
        req: &proto::SearchResultsRequest,
    ) -> Result<proto::SearchPage, String> {
        let mut search = self.search.lock().map_err(|_| "search is unavailable")?;
        let tree = self.scan.tree();
        let tree = tree.read().map_err(|_| "scan data is unavailable")?;
        let active = current_search(&mut search, req.search, &tree)?;
        let metric = metric(req.metric);
        let version = active.search.version();
        let stale = active
            .sorted
            .as_ref()
            .is_none_or(|s| s.metric != metric || s.version != version);
        if stale {
            let mut files = active.search.matches().to_vec();
            files.sort_unstable_by_key(|&f| (Reverse(metric.weight(&tree, f)), f.index()));
            active.sorted = Some(Sorted {
                metric,
                version,
                files,
            });
        }
        let files = &active.sorted.as_ref().expect("sorted above").files;
        let start = (req.offset as usize).min(files.len());
        let end = start
            .saturating_add(req.limit.min(MAX_PAGE) as usize)
            .min(files.len());
        let rows = files[start..end]
            .iter()
            .map(|&f| {
                let parent = tree.parent(f).unwrap_or(NodeId::ROOT);
                let path = tree.path(parent);
                let folder = path.strip_prefix(tree.root_path()).unwrap_or(&path);
                proto::SearchRow {
                    node: f.index() as u32,
                    name: tree.name(f).to_string_lossy().into_owned(),
                    parent: parent.index() as u32,
                    folder: display_path(folder),
                    logical: tree.logical(f),
                    allocated: tree.file_allocated(f).get(),
                    alias: tree.flags(f) & flags::HARDLINK_ALIAS != 0,
                }
            })
            .collect();
        Ok(proto::SearchPage {
            generation: self.generation(),
            search: active.id,
            offset: start as u32,
            total: files.len() as u32,
            rows,
        })
    }

    /// A page of the items merged into a small-items region.
    pub fn small_items(
        &self,
        req: &proto::SmallItemsRequest,
    ) -> Result<proto::SmallItemsPage, String> {
        let mut search = self.search.lock().map_err(|_| "search is unavailable")?;
        let tree = self.scan.tree();
        let tree = tree.read().map_err(|_| "scan data is unavailable")?;
        let folder = node(&tree, req.folder)?;
        let filter = match req.search {
            Some(id) => Some(&current_search(&mut search, id, &tree)?.search),
            None => None,
        };
        let (items, empty) = layout::small_children(
            &tree,
            folder,
            metric(req.metric),
            filter,
            req.count as usize,
        );
        let start = (req.offset as usize).min(items.len());
        let end = start
            .saturating_add(req.limit.min(MAX_PAGE) as usize)
            .min(items.len());
        Ok(proto::SmallItemsPage {
            generation: self.generation(),
            folder: req.folder,
            offset: start as u32,
            total: items.len() as u32,
            items: items[start..end]
                .iter()
                .map(|&(n, weight)| proto::SmallItem {
                    node: n.index() as u32,
                    name: tree.name(n).to_string_lossy().into_owned(),
                    folder: tree.is_dir(n),
                    weight,
                })
                .collect(),
            empty: empty as u32,
        })
    }

    /// Capacity of the volume holding the root, read now (free space changes).
    pub fn volume(&self) -> Option<proto::VolumeInfo> {
        native::volume_info(&self.root).map(|v| proto::VolumeInfo {
            generation: self.generation(),
            is_root: v.is_root,
            capacity: v.capacity,
            free: v.free,
            filesystem: v.filesystem,
            metadata: v.metadata,
        })
    }

    pub fn details(&self, id: u32) -> Result<proto::NodeDetails, String> {
        let tree = self.scan.tree();
        let tree = tree.read().map_err(|_| "scan data is unavailable")?;
        let n = node(&tree, id)?;
        let node_flags = tree.flags(n);
        let is_dir = tree.is_dir(n);
        let allocated = if is_dir {
            Some(tree.allocated(n))
        } else {
            tree.file_allocated(n).get()
        };
        let mut ancestors = Vec::new();
        let mut cur = Some(n);
        while let Some(c) = cur {
            ancestors.push(proto::Crumb {
                node: c.index() as u32,
                name: node_name(&tree, c),
            });
            cur = tree.parent(c);
        }
        ancestors.reverse();
        Ok(proto::NodeDetails {
            generation: self.generation(),
            node: id,
            name: node_name(&tree, n),
            path: display_path(&tree.path(n)),
            folder: is_dir.then(|| match tree.dir_state(n) {
                DirState::Pending => proto::FolderState::Pending,
                DirState::Listed => proto::FolderState::Listed,
                DirState::Failed => proto::FolderState::Failed,
                DirState::NotScanned => proto::FolderState::NotScanned,
            }),
            logical: tree.logical(n),
            allocated,
            unknown_allocation_files: u64::from(tree.unknown_allocation_files(n)),
            files: u64::from(tree.file_count(n)),
            cloud: node_flags & flags::CLOUD != 0,
            hardlinked: node_flags & flags::HARDLINKED != 0,
            alias_of: tree.alias_owner(n).map(|o| display_path(&tree.path(o))),
            ancestors,
        })
    }

    /// The native path of a node, for file manager actions.
    pub fn path(&self, id: u32) -> Result<PathBuf, String> {
        let tree = self.scan.tree();
        let tree = tree.read().map_err(|_| "scan data is unavailable")?;
        Ok(tree.path(node(&tree, id)?))
    }
}

/// The active search if it is `id`, caught up with `tree`.
fn current_search<'a>(
    search: &'a mut Option<ActiveSearch>,
    id: u32,
    tree: &Tree,
) -> Result<&'a mut ActiveSearch, String> {
    let active = search
        .as_mut()
        .filter(|a| a.id == id)
        .ok_or_else(|| format!("search {id} is no longer current"))?;
    active.search.catch_up(tree, usize::MAX);
    Ok(active)
}

fn metric(m: proto::Metric) -> layout::Metric {
    match m {
        proto::Metric::Allocated => layout::Metric::Allocated,
        proto::Metric::Logical => layout::Metric::Logical,
    }
}

fn node(tree: &Tree, id: u32) -> Result<NodeId, String> {
    let n = NodeId::from_index(id);
    if tree.contains(n) {
        Ok(n)
    } else {
        Err(format!("unknown node {id}"))
    }
}

/// A node's display name; the root shows its full path.
fn node_name(tree: &Tree, n: NodeId) -> String {
    if n == NodeId::ROOT {
        display_path(tree.root_path())
    } else {
        tree.name(n).to_string_lossy().into_owned()
    }
}

/// Paths are shown with replacement characters where the native name isn't
/// valid Unicode; the native form stays in the tree for actions.
pub fn display_path(path: &Path) -> String {
    path.display().to_string()
}

pub fn status(root: &str, p: &Progress) -> proto::ScanStatus {
    let phase = match &p.state {
        ScanState::Scanning => proto::ScanPhase::Scanning,
        ScanState::Complete => proto::ScanPhase::Complete,
        ScanState::CompleteWithOmissions => proto::ScanPhase::CompleteWithOmissions,
        ScanState::Cancelled => proto::ScanPhase::Cancelled,
        ScanState::Failed(message) => proto::ScanPhase::Failed {
            message: message.clone(),
        },
    };
    let omissions = p
        .omissions
        .iter()
        .map(|(reason, count)| proto::OmissionGroup {
            reason: omission_reason(reason),
            policy: reason.is_policy(),
            count,
            samples: p
                .omissions
                .samples(reason)
                .iter()
                .map(|s| proto::OmissionSample {
                    path: display_path(&s.path),
                    detail: s.detail.clone(),
                })
                .collect(),
        })
        .collect();
    proto::ScanStatus {
        generation: p.generation,
        root: root.to_owned(),
        phase,
        elapsed_ms: p.elapsed.as_millis() as u64,
        files: p.files,
        dirs: p.dirs,
        logical: p.logical,
        allocated: p.allocated,
        unknown_allocation_files: p.unknown_allocation_files,
        hardlink_aliases: p.hardlink_aliases,
        pending_dirs: p.pending_dirs,
        revision: p.revision,
        omissions,
    }
}

fn omission_reason(reason: OmissionReason) -> proto::OmissionReason {
    match reason {
        OmissionReason::Symlink => proto::OmissionReason::Symlink,
        OmissionReason::MountPoint => proto::OmissionReason::MountPoint,
        OmissionReason::SpecialFile => proto::OmissionReason::SpecialFile,
        OmissionReason::UnknownReparse => proto::OmissionReason::UnknownReparse,
        OmissionReason::PermissionDenied => proto::OmissionReason::PermissionDenied,
        OmissionReason::Vanished => proto::OmissionReason::Vanished,
        OmissionReason::Disconnected => proto::OmissionReason::Disconnected,
        OmissionReason::Unavailable => proto::OmissionReason::Unavailable,
        OmissionReason::Other => proto::OmissionReason::Other,
    }
}
