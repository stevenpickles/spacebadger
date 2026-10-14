//! One scan and the view state derived from it, plus conversions from the
//! scanner's types to the protocol's.

mod export;
mod filter;

use crate::delete::{self, Outcome, Scanned, Target};
use crate::duplicates::{self, Job};
use crate::identity::{self, Found};
use crate::metrics::{self, locked};
use crate::snapshots::Store;
use filter::{ActiveSearch, Filter, current};
use sb_core::filetype::FileType;
use sb_core::layout::{self, LayoutParams, OrderCache, RectKind, rect_flags};
use sb_core::ranking::{self, Basis, Key, Ranking, Rankings, Scope, Signature};
use sb_core::search::{Matcher, Ranked, Search};
use sb_core::selection;
use sb_core::suggest::{self, NameIndex};
use sb_core::tree::{DirState, NodeId, Tree, flags};
use sb_protocol as proto;
use sb_scan::duplicates::plan;
use sb_scan::{FsAdapter, NativeFs, OmissionReason, Progress, Scan, ScanConfig, ScanState, native};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, RwLock, Weak};
use std::time::{Duration, Instant, SystemTime};

mod compare;
#[cfg(test)]
mod load;

/// Files narrower or shorter than this (CSS px) get no label.
const FILE_LABEL_MIN: (f32, f32) = (40.0, 14.0);

/// Nodes a new search examines per hold of the tree's read lock, so the
/// scanner can keep writing while a large tree is searched.
const SEARCH_STEP: usize = 200_000;

/// Largest page of search results returned at once.
const MAX_PAGE: u32 = 500;

/// Nodes a ranking for the Contents list looks at per hold of the tree's
/// read lock (and the search's), about a millisecond, so the scanner and
/// other requests get in between.
const RANK_STEP: usize = 100_000;

/// While scanning, nodes one request walks to bring a ranking up to date
/// before it serves the one kept instead; the next request goes on.
const RANK_BUDGET: usize = 4 * RANK_STEP;

/// Longest a request waits at once for another to finish a ranking it
/// needs, before looking again.
const RANK_WAIT: Duration = Duration::from_millis(250);

type StatusSink = Arc<dyn Fn(proto::ScanStatus) + Send + Sync>;

// Lock order: `deleting` first, the search (`filter.active`) and `compare`
// before the tree, then `rankings`, and the order caches and
// `search_ranked` last; waiting on `ranked` holds only `rankings`, and the
// scanner takes the tree and then its progress. `rematching` is taken with
// the tree read-locked, and `compare` briefly inside it. Requests only
// read-lock the search; it's write-locked briefly to set a search or bring
// it up to date once the scan has finished. The job keeping it caught up
// while scanning read-locks the tree first and only tries the search's
// write lock, so it never waits holding either; when requests keep it off,
// it waits for the search's write lock, then the tree, in request order
// (see `filter`).
pub struct Session {
    pub root: PathBuf,
    pub scan: Scan,
    on_status: StatusSink,
    order: Mutex<OrderCache>,
    /// The active search, kept caught up with the scan in the background.
    filter: Arc<Filter>,
    /// Sibling order for the filtered map, for the search numbered.
    search_order: Mutex<Option<(u32, OrderCache)>>,
    /// The active search's matches ranked by size, for the result list.
    search_ranked: Mutex<Option<(u32, Ranked)>>,
    /// Number of the most recently requested search.
    latest_search: AtomicU32,
    /// Held while deleting, so deletes run one at a time.
    deleting: Mutex<()>,
    /// The Contents list's rankings: folders' items, or the files under
    /// them, by size.
    rankings: Mutex<Rankings>,
    /// Signalled when a walk taken to be finished is kept or given up.
    ranked: Condvar,
    /// Folder names and extensions for the filter's completion.
    names: Arc<Mutex<Option<Names>>>,
    /// Set while the completion index is rebuilt in the background.
    names_building: Arc<AtomicBool>,
    /// The latest search for duplicate files.
    duplicates: Mutex<Option<Arc<Job>>>,
    /// Number of the latest search for duplicates.
    latest_duplicates: AtomicU32,
    /// Set to stop a delete reading copies of duplicates that would stay.
    recheck_cancel: AtomicBool,
    /// Where snapshots of completed scans are kept; `None` in tests.
    snapshots: Option<Arc<Store>>,
    /// The snapshot saved for this scan once it completed.
    saved_snapshot: Arc<Mutex<Option<String>>>,
    /// When the scan started (ms since the Unix epoch): only snapshots from
    /// before then are earlier scans.
    started_ms: u64,
    /// The comparison with an earlier scan, when on.
    compare: Mutex<Option<compare::Active>>,
    /// Held while the comparison is matched again after the scan changed,
    /// so only one request does it.
    rematching: Mutex<()>,
    /// The earlier scan carried over from the previous scan of this root.
    carried: Option<compare::Carried>,
    /// The scan root as found when the scan started, so a delete can tell
    /// it hasn't been replaced since; `None` if it couldn't be read.
    root_found: Option<Found>,
}

struct Names {
    revision: u64,
    built: Instant,
    index: Arc<NameIndex>,
}

/// While scanning, the completion index is rebuilt at most this often.
const NAMES_REFRESH: Duration = Duration::from_secs(5);

/// Nodes the completion index takes in per hold of the tree's lock.
const NAMES_STEP: usize = 50_000;

/// A walk taken to be finished by [`Session::ranked`]. Dropping it ends
/// the walk's finishing if its ranking wasn't kept (an edit came first, or
/// an error), and wakes the requests waiting for it.
struct Finishing<'a> {
    session: &'a Session,
    key: Key,
    walk: u64,
}

impl Drop for Finishing<'_> {
    fn drop(&mut self) {
        if let Ok(mut rankings) = self.session.rankings.lock() {
            rankings.give_up(self.key, self.walk);
        }
        self.session.ranked.notify_all();
    }
}

impl Session {
    /// Starts scanning without saving a snapshot at the end.
    #[cfg(test)]
    pub fn start(
        root: PathBuf,
        on_status: impl Fn(proto::ScanStatus) + Send + Sync + 'static,
    ) -> Self {
        Self::start_with(root, None, None, on_status)
    }

    /// Starts scanning; once the scan completes, a snapshot of it is saved
    /// in `snapshots`, so later scans can be compared with it. `carried` is
    /// the earlier scan a previous scan of this root was compared with, kept
    /// so comparing can go on after the rescan (see [`Session::carry`]).
    pub fn start_with(
        root: PathBuf,
        snapshots: Option<Arc<Store>>,
        carried: Option<compare::Carried>,
        on_status: impl Fn(proto::ScanStatus) + Send + Sync + 'static,
    ) -> Self {
        let config = native::scan_config(&root);
        Self::start_on(root, NativeFs::new(), config, snapshots, carried, on_status)
    }

    /// [`Session::start_with`] scanning through `adapter`, for tests and
    /// benchmarks with a synthetic filesystem.
    fn start_on(
        root: PathBuf,
        adapter: impl FsAdapter,
        config: ScanConfig,
        snapshots: Option<Arc<Store>>,
        carried: Option<compare::Carried>,
        on_status: impl Fn(proto::ScanStatus) + Send + Sync + 'static,
    ) -> Self {
        let started_ms = compare::now_ms();
        let root_found = identity::hold(&root, false).ok().map(|(found, _)| found);
        let on_status: StatusSink = Arc::new(on_status);
        let display = display_path(&root);
        let sink = Arc::clone(&on_status);
        let tree_cell = Arc::new(OnceLock::new());
        let filter = Filter::start(Arc::clone(&tree_cell));
        let nudge = Arc::clone(&filter);
        let saved_snapshot = Arc::new(Mutex::new(None));
        let saver = snapshots.as_ref().map(|store| compare::Saver {
            store: Arc::clone(store),
            root: root.clone(),
            tree: Arc::clone(&tree_cell),
            saved: Arc::clone(&saved_snapshot),
            protect: carried.as_ref().map(|c| c.id().to_owned()),
        });
        let scan = Scan::start(root.clone(), adapter, config, move |p| {
            nudge.nudge(p.revision);
            sink(status(&display, p));
            if let Some(saver) = &saver {
                saver.progress(&p.state);
            }
        });
        let _ = tree_cell.set(Arc::downgrade(&scan.tree()));
        Self {
            root,
            scan,
            on_status,
            order: Mutex::new(OrderCache::default()),
            filter,
            search_order: Mutex::new(None),
            search_ranked: Mutex::new(None),
            latest_search: AtomicU32::new(0),
            deleting: Mutex::new(()),
            rankings: Mutex::new(Rankings::default()),
            ranked: Condvar::new(),
            names: Arc::new(Mutex::new(None)),
            names_building: Arc::new(AtomicBool::new(false)),
            duplicates: Mutex::new(None),
            latest_duplicates: AtomicU32::new(0),
            recheck_cancel: AtomicBool::new(false),
            snapshots,
            saved_snapshot,
            started_ms,
            compare: Mutex::new(None),
            rematching: Mutex::new(()),
            carried,
            root_found,
        }
    }

    /// A delete is moving items right now.
    pub fn is_deleting(&self) -> bool {
        self.deleting.try_lock().is_err()
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
        let metric = metric(req.metric);
        let params = LayoutParams::new(
            req.width.clamp(0.0, 16_384.0),
            req.height.clamp(0.0, 16_384.0),
        );
        let tree_lock = self.scan.tree();
        // A newer layout for the same map may have come while this one
        // waited for the locks; then it's left to that one.
        let (search, tree, progress) = self.read_locked(&tree_lock, req.search.is_some())?;
        crate::runner::check()?;
        // The progress is read with the tree's lock held, so the revision
        // sent describes the tree laid out (see `with_duplicates`).
        let finished = progress.state.is_finished();
        let view = node(&tree, req.view)?;
        // While scanning, keep sibling order stable; once finished, sort afresh.
        let result = match req.search {
            Some(id) => {
                let active = current(&search, id)?;
                let mut order = locked("order", || self.search_order.lock())
                    .map_err(|_| "layout state is unavailable")?;
                let order = match &mut *order {
                    Some((of, order)) if *of == id => order,
                    slot => &mut slot.insert((id, OrderCache::default())).1,
                };
                let _compute = metrics::span("compute");
                layout::layout(
                    &tree,
                    view,
                    metric,
                    Some(&active.search),
                    &params,
                    order,
                    !finished,
                )
            }
            None => {
                let mut order = locked("order", || self.order.lock())
                    .map_err(|_| "layout state is unavailable")?;
                let _compute = metrics::span("compute");
                layout::layout(&tree, view, metric, None, &params, &mut order, !finished)
            }
        };
        // Encoding needs only the tree, and its last step not even that.
        drop(search);
        crate::runner::check()?;

        let _encode = metrics::span("encode");
        let mut rects = Vec::with_capacity(result.rects.len());
        let mut names = Vec::new();
        for (i, r) in result.rects.iter().enumerate() {
            let labelled = match r.kind {
                RectKind::Folder => r.flags & rect_flags::HEADER != 0,
                RectKind::File => r.w >= FILE_LABEL_MIN.0 && r.h >= FILE_LABEL_MIN.1,
                RectKind::OtherSmall => false,
            };
            if labelled {
                names.push((i as u32, tree.name(r.node).to_string_lossy().into_owned()));
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
                parent: r.parent(&tree).index() as u32,
            });
        }
        drop(tree);
        let labels: Vec<(u32, &str)> = names.iter().map(|(i, s)| (*i, s.as_str())).collect();
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
        let Some(matcher) = Matcher::parse_at(query, SystemTime::now(), &local_midnight)? else {
            let mut current = locked("search", || self.filter.active.write())
                .map_err(|_| "search is unavailable")?;
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
            let tree =
                locked("tree", || tree_lock.read()).map_err(|_| "scan data is unavailable")?;
            let _examine = metrics::span("examine");
            if search.catch_up(&tree, SEARCH_STEP) {
                break;
            }
        }
        let mut current =
            locked("search", || self.filter.active.write()).map_err(|_| "search is unavailable")?;
        if self.latest_search.load(Ordering::SeqCst) != id {
            return Err(superseded());
        }
        let tree = locked("tree", || tree_lock.read()).map_err(|_| "scan data is unavailable")?;
        let mut active = ActiveSearch {
            id,
            search,
            revision: 0,
        };
        // What was scanned since the last step; read with the tree's lock
        // held, so the revision describes the tree.
        active.catch_up(&tree, self.scan.progress().revision);
        Ok(Some(self.summary(current.insert(active))))
    }

    /// Matching totals for search `id`. While scanning, they can trail the
    /// scan by a status (see [`proto::SearchSummary::revision`]).
    pub fn search_summary(&self, id: u32) -> Result<proto::SearchSummary, String> {
        let tree = self.scan.tree();
        let (search, _tree, _) = self.read_locked(&tree, true)?;
        let active = current(&search, id)?;
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
            revision: active.revision,
        }
    }

    /// One page of matching files, largest first.
    pub fn search_results(
        &self,
        req: &proto::SearchResultsRequest,
    ) -> Result<proto::SearchPage, String> {
        let tree = self.scan.tree();
        let (search, tree, _) = self.read_locked(&tree, true)?;
        let active = current(&search, req.search)?;
        let metric = metric(req.metric);
        let mut ranked = locked("search_ranked", || self.search_ranked.lock())
            .map_err(|_| "search results are unavailable")?;
        let rank = metrics::span("rank");
        let ranked = match &mut *ranked {
            Some((id, r)) if *id == active.id && r.is_current(&active.search, metric) => r,
            slot => {
                &mut slot
                    .insert((active.id, Ranked::new(&active.search, metric)))
                    .1
            }
        };
        let total = ranked.len();
        let start = (req.offset as usize).min(total);
        let end = start
            .saturating_add(req.limit.min(MAX_PAGE) as usize)
            .min(total);
        let top = ranked.top(&tree, end);
        drop(rank);
        let rows = top[start..]
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
            total: total as u32,
            rows,
        })
    }

    /// One page of the files anywhere under a folder, largest first.
    pub fn folder_files(
        &self,
        req: &proto::FolderFilesRequest,
    ) -> Result<proto::FolderFilesPage, String> {
        let key = Key {
            scope: Scope::Files,
            folder: NodeId::from_index(req.folder),
            metric: metric(req.metric),
            search: req.search,
        };
        let limit = req.limit.min(MAX_PAGE) as usize;
        let end = (req.offset as usize).saturating_add(limit);
        self.ranked(key, end, |tree, ranking| {
            let total = ranking.total();
            let start = (req.offset as usize).min(total);
            let end = start.saturating_add(limit).min(total);
            let base = tree.path(key.folder);
            let files = ranking.items()[start..end]
                .iter()
                .map(|&(f, weight)| {
                    let parent = tree.parent(f).unwrap_or(NodeId::ROOT);
                    let path = tree.path(parent);
                    let sub = path.strip_prefix(&base).unwrap_or(&path);
                    proto::FolderFile {
                        node: f.index() as u32,
                        name: tree.name(f).to_string_lossy().into_owned(),
                        parent: parent.index() as u32,
                        subfolder: display_path(sub),
                        weight,
                    }
                })
                .collect();
            proto::FolderFilesPage {
                generation: self.generation(),
                folder: req.folder,
                offset: start as u32,
                total: total as u32,
                files,
                empty: ranking.empty() as u32,
                revision: ranking.basis().revision,
            }
        })
    }

    /// Serves a page with `serve` from a ranking of `key`'s items that
    /// covers the first `end`, with the tree read-locked.
    ///
    /// A ranking kept for the key is served while [`Ranking::current`]
    /// says so. Otherwise a walk ranks the items again, a step at a time,
    /// letting go of the locks between steps. While scanning, a request
    /// walks at most [`RANK_BUDGET`] nodes and then serves the ranking kept,
    /// if there is one, so a large folder is ranked again across several
    /// requests; after the scan, a request walks until it's done, so it
    /// never serves a ranking from before an edit such as a delete.
    ///
    /// Requests for the same key share a walk: they step the same one, and
    /// while one of them finishes it the others wait for its ranking (the
    /// Contents tab asks for several pages at once).
    fn ranked<R>(
        &self,
        key: Key,
        end: usize,
        serve: impl Fn(&Tree, &Ranking) -> R,
    ) -> Result<R, String> {
        let tree_lock = self.scan.tree();
        let mut walked = 0;
        // Waited for another request's walk, whose ranking then does.
        let mut waited = false;
        loop {
            // Superseded between steps: the walk stays for the next request.
            crate::runner::check()?;
            // The progress is read with the tree's lock held, so the revision
            // describes the tree walked (see `with_duplicates`).
            let (search, tree, progress) = self.read_locked(&tree_lock, key.search.is_some())?;
            let folder = node(&tree, key.folder.index() as u32)?;
            let filter = match key.search {
                Some(id) => Some(&current(&search, id)?.search),
                None => None,
            };
            let now = Basis {
                revision: progress.revision,
                finished: progress.state.is_finished(),
                signature: Signature::of(&tree, folder, filter),
                at: Instant::now(),
            };
            let mut rankings = locked("rankings", || self.rankings.lock())
                .map_err(|_| "rankings are unavailable")?;
            let entry = rankings.entry(key);
            let serve_kept = entry.ranking().is_some_and(|r| {
                r.covers(end)
                    && (r.current(&now, ranking::REFRESH)
                        || (!now.finished && (waited || walked >= RANK_BUDGET)))
            });
            if serve_kept {
                drop(search);
                return Ok(serve(&tree, entry.ranking().expect("checked above")));
            }
            if entry.finishing() {
                drop((rankings, tree, search));
                self.wait_for_ranking(key)?;
                waited = true;
                continue;
            }
            let done = {
                let _rank = metrics::span("rank");
                entry.walk(&now, end).step(&tree, filter, RANK_STEP)
            };
            walked += RANK_STEP;
            if !done {
                continue;
            }
            let walk = entry.take_walk().expect("a walk was just stepped");
            // Declared before the locks below, so it's dropped after them.
            let _finishing = Finishing {
                session: self,
                key,
                walk: walk.id(),
            };
            drop((rankings, tree, search));
            let ranking = {
                let _rank = metrics::span("rank");
                walk.finish()
            };
            let tree =
                locked("tree", || tree_lock.read()).map_err(|_| "scan data is unavailable")?;
            let progress = self.scan.progress();
            let made = ranking.basis();
            if progress.state.is_finished()
                && !(made.finished && made.revision == progress.revision)
            {
                // Edited since the walk started, or walked while scanning:
                // either may list what's gone since.
                continue;
            }
            let page = serve(&tree, &ranking);
            drop(tree);
            let mut rankings = locked("rankings", || self.rankings.lock())
                .map_err(|_| "rankings are unavailable")?;
            rankings.entry(key).keep(ranking);
            rankings.trim(key);
            return Ok(page);
        }
    }

    /// Waits while another request finishes a walk for `key`, for at most
    /// [`RANK_WAIT`]. Takes only the rankings' lock.
    fn wait_for_ranking(&self, key: Key) -> Result<(), String> {
        let _wait = metrics::span("rank wait");
        let rankings = self
            .rankings
            .lock()
            .map_err(|_| "rankings are unavailable")?;
        let _ = self
            .ranked
            .wait_timeout_while(rankings, RANK_WAIT, |r| r.finishing(key))
            .map_err(|_| "rankings are unavailable")?;
        Ok(())
    }

    /// Folder names or extensions for the filter's completion, largest
    /// first. The index is rebuilt when the scan has changed, but during a
    /// scan at most every [`NAMES_REFRESH`], and then in the background
    /// while the previous index answers. It's built in steps, so the
    /// scanner isn't held up meanwhile.
    pub fn suggest(&self, req: &proto::SuggestRequest) -> Result<Vec<proto::Suggestion>, String> {
        let progress = self.scan.progress();
        let scanning = matches!(progress.state, ScanState::Scanning);
        let index = {
            let mut names =
                locked("names", || self.names.lock()).map_err(|_| "suggestions are unavailable")?;
            let fresh = names.as_ref().is_some_and(|n| {
                n.revision == progress.revision || (scanning && n.built.elapsed() < NAMES_REFRESH)
            });
            if !fresh && scanning && names.is_some() {
                self.refresh_names(progress.revision);
            } else if !fresh {
                let tree = self.scan.tree();
                // Read with the tree's lock held, so the index is kept under
                // the revision of the tree it's built from.
                *names = Some(build_names(&tree, || self.scan.progress().revision)?);
            }
            Arc::clone(&names.as_ref().expect("built").index)
        };
        let limit = req.limit.min(50) as usize;
        let found = match req.kind {
            proto::SuggestKind::Folder => index.folders(&req.typed, limit),
            proto::SuggestKind::Extension => index.extensions(&req.typed, limit),
        };
        Ok(found
            .into_iter()
            .map(|e| proto::Suggestion {
                name: e.name().to_owned(),
                count: e.count,
                logical: e.logical,
            })
            .collect())
    }

    /// Rebuilds the completion index on another thread, unless that's
    /// already happening; `revision` is the scan's when asked, which the
    /// index reflects at least.
    fn refresh_names(&self, revision: u64) {
        if self.names_building.swap(true, Ordering::SeqCst) {
            return;
        }
        let tree = Arc::downgrade(&self.scan.tree());
        let names = Arc::clone(&self.names);
        let building = Arc::clone(&self.names_building);
        let spawned = std::thread::Builder::new()
            .name("sb-names".into())
            .spawn(move || {
                let _timing = metrics::command("suggest_index");
                let built = tree.upgrade().map(|t| build_names(&t, || revision));
                if let (Some(Ok(built)), Ok(mut names)) = (built, names.lock()) {
                    // A request may have built a newer one meanwhile.
                    if names.as_ref().is_none_or(|n| n.revision <= revision) {
                        *names = Some(built);
                    }
                }
                building.store(false, Ordering::SeqCst);
            });
        if spawned.is_err() {
            self.names_building.store(false, Ordering::SeqCst);
        }
    }

    /// A page of the items merged into a small-items region.
    pub fn small_items(
        &self,
        req: &proto::SmallItemsRequest,
    ) -> Result<proto::SmallItemsPage, String> {
        let key = Key {
            scope: Scope::Children,
            folder: NodeId::from_index(req.folder),
            metric: metric(req.metric),
            search: req.search,
        };
        self.ranked(key, 0, |tree, ranking| {
            // The region merges the smallest children with a weight.
            let all = ranking.items();
            let items = &all[all.len().saturating_sub(req.count as usize)..];
            let start = (req.offset as usize).min(items.len());
            let end = start
                .saturating_add(req.limit.min(MAX_PAGE) as usize)
                .min(items.len());
            proto::SmallItemsPage {
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
                empty: ranking.empty() as u32,
                revision: ranking.basis().revision,
            }
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

    /// Details of a node; with `search`, also what that filter matches
    /// under it. A search that's no longer current gives no matching totals
    /// rather than an error, since a newer one is on its way. Without one,
    /// such as for a tooltip, the search's lock isn't taken.
    pub fn details(&self, id: u32, search: Option<u32>) -> Result<proto::NodeDetails, String> {
        let tree = self.scan.tree();
        let (active, tree, _) = self.read_locked(&tree, search.is_some())?;
        let n = node(&tree, id)?;
        let matching = search.and_then(|s| current(&active, s).ok()).map(|a| {
            let t = a.search.totals(n);
            proto::Matching {
                files: u64::from(t.files),
                logical: t.logical,
            }
        });
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
                DirState::Listed if !tree.fully_listed(n) => proto::FolderState::Partial,
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
            // 1 and u32::MAX stand for any time before 1970 or after 2106,
            // which would show as a misleading exact date.
            modified: tree
                .modified(n)
                .filter(|&s| s != 1 && s != u32::MAX)
                .map(u64::from),
            alias_of: tree.alias_owner(n).map(|o| display_path(&tree.path(o))),
            ancestors,
            matching,
        })
    }

    /// What a set of selected items amounts to.
    pub fn selection(&self, nodes: &[u32]) -> Result<proto::SelectionSummary, String> {
        let tree = self.scan.tree();
        let tree = locked("tree", || tree.read()).map_err(|_| "scan data is unavailable")?;
        let r = selection::resolve(&tree, nodes);
        Ok(proto::SelectionSummary {
            generation: self.generation(),
            items: r.items.len() as u32,
            folders: r.folders as u32,
            files: r.files,
            logical: r.logical,
            allocated: r.allocated,
            unknown_allocation_files: r.unknown,
            hardlinked: r.hardlinked,
            missing: r.missing as u32,
            samples: r
                .items
                .iter()
                .take(proto::SELECTION_SAMPLES)
                .map(|&n| proto::SelectedItem {
                    node: n.index() as u32,
                    name: node_name(&tree, n),
                    path: display_path(&tree.path(n)),
                    folder: tree.is_dir(n),
                    logical: tree.logical(n),
                    allocated: if tree.is_dir(n) {
                        Some(tree.allocated(n))
                    } else {
                        tree.file_allocated(n).get()
                    },
                    files: u64::from(tree.file_count(n)),
                })
                .collect(),
        })
    }

    /// Moves the selected items to the Recycle Bin or Trash, each only if
    /// it is still the item the scan found, and removes those no longer
    /// where they were from the scan, which is published as a new revision.
    /// Refused while the scan runs, since the scanner may still be adding
    /// to them.
    ///
    /// Duplicates selected themselves are checked first, and the copies
    /// that would stay read again, with progress through `on_recheck`; see
    /// [`duplicates::Retained`].
    pub fn delete(
        &self,
        nodes: &[u32],
        on_recheck: &(dyn Fn(proto::DuplicatesRecheck) + Sync),
    ) -> Result<proto::DeleteReport, String> {
        self.delete_using(nodes, on_recheck, delete::delete)
    }

    /// [`Session::delete`] with the per-item delete passed in, for tests.
    fn delete_using(
        &self,
        nodes: &[u32],
        on_recheck: &(dyn Fn(proto::DuplicatesRecheck) + Sync),
        delete_one: impl Fn(&Target) -> Outcome,
    ) -> Result<proto::DeleteReport, String> {
        let _one_at_a_time = self
            .deleting
            .lock()
            .map_err(|_| "deleting is unavailable")?;
        if !self.scan.progress().state.is_finished() {
            return Err("Items can be deleted once the scan finishes or is cancelled.".into());
        }
        let job = self
            .duplicates
            .lock()
            .map_err(|_| "duplicates are unavailable")?
            .clone();
        let (targets, retained): (Vec<(NodeId, Target)>, _) = {
            let tree = self.scan.tree();
            let tree = locked("tree", || tree.read()).map_err(|_| "scan data is unavailable")?;
            let targets = selection::resolve(&tree, nodes)
                .items
                .into_iter()
                .map(|n| (n, self.target(&tree, n)))
                .collect();
            (targets, job.map(|j| j.retained(&tree, nodes)))
        };
        // Duplicates selected themselves are kept unless they still match
        // and a copy that stays is confirmed, read with no lock held; the
        // confirmed copies are held until the moves are done. Everything
        // else then goes through the per-item check against the scan.
        let verified = match retained {
            Some(r) if !r.is_empty() => self.recheck(r, on_recheck),
            _ => duplicates::Verified::keeping(duplicates::Verdicts::new()),
        };
        let mut report = proto::DeleteReport {
            generation: self.generation(),
            recycled: Vec::new(),
            removed: Vec::new(),
            others: Vec::new(),
            files: 0,
            logical: 0,
            allocated: 0,
        };
        let mut recycled = Vec::new();
        let mut removed = Vec::new();
        for (n, target) in targets {
            if let Some(why) = verified.verdicts.get(&n) {
                let path = target.path();
                let name = path.file_name().unwrap_or(path.as_os_str());
                report.others.push(proto::DeleteItem {
                    node: n.index() as u32,
                    path: display_path(path),
                    outcome: proto::DeleteOutcome::Kept,
                    message: why.message(&name.to_string_lossy()),
                });
                continue;
            }
            let (outcome, message) = match delete_one(&target) {
                Outcome::Recycled => {
                    recycled.push(n);
                    removed.push(n);
                    continue;
                }
                Outcome::AlreadyMissing => {
                    removed.push(n);
                    (proto::DeleteOutcome::AlreadyMissing, String::new())
                }
                Outcome::Uncertain { reason, gone } => {
                    // What's left at its path stays in the scan.
                    if gone {
                        removed.push(n);
                    }
                    (proto::DeleteOutcome::Uncertain, reason)
                }
                Outcome::Changed(message) => (proto::DeleteOutcome::Changed, message),
                Outcome::Failed(message) => (proto::DeleteOutcome::Failed, message),
            };
            report.others.push(proto::DeleteItem {
                node: n.index() as u32,
                path: display_path(target.path()),
                outcome,
                message,
            });
        }
        // The moves are done: release the copies held.
        drop(verified);
        if removed.is_empty() {
            return Ok(report);
        }
        let edited = self.scan.edit_finished(|tree| {
            for &n in &recycled {
                report.files += u64::from(tree.file_count(n));
                report.logical += tree.logical(n);
                report.allocated += tree.allocated(n);
            }
            for &n in &removed {
                tree.remove(n);
            }
        });
        let ((), progress) = edited.ok_or("scan data is unavailable")?;
        // Derived views are brought up to date before the change is
        // announced, so the requests it prompts find them ready.
        self.filter.nudge(progress.revision);
        self.rematch();
        report.recycled = recycled.iter().map(|n| n.index() as u32).collect();
        report.removed = removed.iter().map(|n| n.index() as u32).collect();
        (self.on_status)(status(&display_path(&self.root), &progress));
        Ok(report)
    }

    /// What the scan recorded for `node` and every folder above it.
    fn target(&self, tree: &Tree, node: NodeId) -> Target {
        let mut chain = Vec::new();
        let mut at = Some(node);
        while let Some(n) = at {
            let ids = match (&self.root_found, n == NodeId::ROOT) {
                (Some(root), true) => root.ids,
                (None, true) => [0; 2],
                _ => [tree.id(n).unwrap_or(0), 0],
            };
            chain.push(Scanned {
                path: tree.path(n),
                folder: tree.is_dir(n),
                ids,
                len: tree.logical(n),
                modified: tree.modified(n),
                hardlinked: tree.flags(n) & (flags::HARDLINKED | flags::HARDLINK_ALIAS) != 0,
            });
            at = tree.parent(n);
        }
        chain.reverse();
        Target {
            chain,
            volume: self.root_found.as_ref().map(|r| r.volume),
        }
    }

    /// Starts looking for duplicate files, replacing any search already
    /// running, and reports progress through `emit` until it ends. Refused
    /// while the scan runs, since files are still being added.
    pub fn duplicates_start(
        self: &Arc<Self>,
        req: &proto::DuplicatesRequest,
        emit: impl Fn(proto::DuplicatesStatus) + Send + Sync + 'static,
    ) -> Result<proto::DuplicatesStatus, String> {
        if !self.scan.progress().state.is_finished() {
            return Err("Duplicates can be found once the scan finishes or is cancelled.".into());
        }
        let (scope, plan) = {
            let tree = self.scan.tree();
            let (search, tree, _) = self.read_locked(&tree, req.search.is_some())?;
            let folder = node(&tree, req.folder)?;
            let filter = match req.search {
                Some(id) => Some(&current(&search, id)?.search),
                None => None,
            };
            let scope = duplicates::Scope {
                generation: self.generation(),
                folder: req.folder,
                path: display_path(&tree.path(folder)),
                query: filter.map(|s| s.matcher().query().to_owned()),
                min_size: req.min_size,
            };
            (scope, plan(&tree, folder, filter, req.min_size))
        };
        let id = self.latest_duplicates.fetch_add(1, Ordering::SeqCst) + 1;
        let job = Arc::new(Job::new(id, scope, plan));
        {
            let mut slot = self
                .duplicates
                .lock()
                .map_err(|_| "duplicates are unavailable")?;
            if let Some(old) = slot.replace(Arc::clone(&job)) {
                old.cancel();
            }
        }
        let session = Arc::downgrade(self);
        let reading = Arc::clone(&job);
        std::thread::Builder::new()
            .name("sb-duplicates".into())
            .spawn(move || {
                std::thread::scope(|s| {
                    s.spawn(|| report_duplicates(&session, &reading, &emit));
                    reading.run(&native::NativeContents);
                });
                if let Some(status) = session.upgrade().and_then(|s| s.duplicates_status(id).ok()) {
                    emit(status);
                }
            })
            .map_err(|e| format!("Couldn't start looking for duplicates: {e}"))?;
        self.duplicates_status(id)
    }

    /// The search for duplicates numbered `job`, if it's the latest.
    fn duplicates_job(&self, job: u32) -> Result<Arc<Job>, String> {
        self.duplicates
            .lock()
            .map_err(|_| "duplicates are unavailable")?
            .clone()
            .filter(|j| j.id == job)
            .ok_or_else(|| format!("search for duplicates {job} is no longer current"))
    }

    /// Calls `f` with the search for duplicates numbered `job`, the tree,
    /// and the tree's revision. The revision is read with the tree's lock
    /// held, since an edit bumps it under the write lock (tree, then
    /// progress, the aggregator's order), so it always describes the tree
    /// passed. Read before, a delete in between would pair the edited tree
    /// with the old revision, and the job would keep its figures for files
    /// no longer there.
    fn with_duplicates<R>(
        &self,
        job: u32,
        f: impl FnOnce(&Job, &Tree, u64) -> R,
    ) -> Result<R, String> {
        let job = self.duplicates_job(job)?;
        let tree = self.scan.tree();
        let tree = tree.read().map_err(|_| "scan data is unavailable")?;
        let revision = self.scan.progress().revision;
        Ok(f(&job, &tree, revision))
    }

    pub fn duplicates_status(&self, job: u32) -> Result<proto::DuplicatesStatus, String> {
        self.with_duplicates(job, |job, tree, revision| job.status(tree, revision))
    }

    /// The duplicates found so far, leaving out files deleted since.
    pub fn duplicates_list(&self, job: u32) -> Result<proto::DuplicatesList, String> {
        self.with_duplicates(job, |job, tree, revision| job.list(tree, revision))
    }

    /// A page of a group's files, leaving out files deleted since.
    pub fn duplicate_files(
        &self,
        req: &proto::DuplicateFilesRequest,
    ) -> Result<proto::DuplicateFilesPage, String> {
        self.with_duplicates(req.job, |job, tree, revision| {
            job.files(tree, revision, req.group, req.offset, req.limit)
        })?
    }

    /// Every file but the first of each listed group, by node.
    pub fn duplicates_extras(&self, job: u32) -> Result<proto::DuplicateExtras, String> {
        self.with_duplicates(job, |job, tree, revision| job.extras(tree, revision))
    }

    /// Whether deleting `nodes` would leave a group of duplicates with no copy.
    pub fn duplicates_check(
        &self,
        job: u32,
        nodes: &[u32],
    ) -> Result<proto::DuplicatesCheck, String> {
        let job = self.duplicates_job(job)?;
        let pending = {
            let tree = self.scan.tree();
            let tree = locked("tree", || tree.read()).map_err(|_| "scan data is unavailable")?;
            job.check(&tree, nodes)
        };
        // The disk is looked at with no lock held.
        Ok(pending.finish(&native::NativeContents))
    }

    /// Stops reading; the groups found so far stay.
    pub fn duplicates_cancel(&self, job: u32) -> Result<(), String> {
        self.duplicates_job(job)?.cancel();
        Ok(())
    }

    /// Checks the selected duplicates and reads the copies a delete would
    /// keep, reporting progress every so often; returns the selected files
    /// to keep, and holds the confirmed copies until it's dropped.
    fn recheck(
        &self,
        retained: duplicates::Retained,
        on_recheck: &(dyn Fn(proto::DuplicatesRecheck) + Sync),
    ) -> duplicates::Verified {
        self.recheck_cancel.store(false, Ordering::SeqCst);
        let progress = sb_scan::duplicates::ConfirmProgress::default();
        progress.total.store(retained.bytes(), Ordering::Relaxed);
        let report = |finished| {
            let total = progress.total.load(Ordering::Relaxed);
            on_recheck(proto::DuplicatesRecheck {
                generation: self.generation(),
                done_bytes: if finished {
                    total
                } else {
                    progress.done().min(total)
                },
                total_bytes: total,
                finished,
            })
        };
        report(false);
        // A panic while reading confirms nothing: every file is kept.
        let unread =
            duplicates::Verified::keeping(retained.keep_all(duplicates::Unconfirmed::Unknown));
        let done = AtomicBool::new(false);
        let verdicts = std::thread::scope(|s| {
            s.spawn(|| {
                while !done.load(Ordering::Relaxed) {
                    std::thread::park_timeout(duplicates::REPORT_EVERY);
                    if !done.load(Ordering::Relaxed) {
                        report(false);
                    }
                }
            });
            let verdicts = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                retained.verify(&native::NativeContents, &self.recheck_cancel, &progress)
            }));
            done.store(true, Ordering::Relaxed);
            verdicts
        });
        report(true);
        verdicts.unwrap_or(unread)
    }

    /// Stops a delete reading copies of duplicates; the items whose copy
    /// wasn't confirmed yet are kept.
    pub fn duplicates_recheck_cancel(&self) {
        self.recheck_cancel.store(true, Ordering::SeqCst);
    }

    /// The native path of a node, for file manager actions.
    pub fn path(&self, id: u32) -> Result<PathBuf, String> {
        let tree = self.scan.tree();
        let tree = locked("tree", || tree.read()).map_err(|_| "scan data is unavailable")?;
        Ok(tree.path(node(&tree, id)?))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.filter.stop();
        if let Ok(Some(job)) = self.duplicates.get_mut().map(Option::take) {
            job.cancel();
        }
    }
}

/// Builds the completion index from `tree` a step at a time, releasing the
/// lock in between; `revision` is read with it held at the last step and
/// kept with the index.
fn build_names(tree: &RwLock<Tree>, revision: impl Fn() -> u64) -> Result<Names, String> {
    let mut builder = suggest::Builder::default();
    loop {
        let tree = locked("tree", || tree.read()).map_err(|_| "scan data is unavailable")?;
        let _index = metrics::span("index");
        if builder.step(&tree, NAMES_STEP) {
            let revision = revision();
            drop(tree);
            return Ok(Names {
                revision,
                built: Instant::now(),
                index: Arc::new(builder.finish()),
            });
        }
    }
}

/// Sends the status of a search for duplicates every so often while it reads.
fn report_duplicates(session: &Weak<Session>, job: &Job, emit: &impl Fn(proto::DuplicatesStatus)) {
    loop {
        std::thread::sleep(duplicates::REPORT_EVERY);
        if !job.is_running() {
            return;
        }
        let Some(status) = session
            .upgrade()
            .and_then(|s| s.duplicates_status(job.id).ok())
        else {
            return;
        };
        emit(status);
    }
}

fn metric(m: proto::Metric) -> layout::Metric {
    match m {
        proto::Metric::Allocated => layout::Metric::Allocated,
        proto::Metric::Logical => layout::Metric::Logical,
    }
}

fn node(tree: &Tree, id: u32) -> Result<NodeId, String> {
    let n = NodeId::from_index(id);
    if !tree.contains(n) {
        Err(format!("unknown node {id}"))
    } else if tree.is_removed(n) {
        Err("It was deleted.".into())
    } else {
        Ok(n)
    }
}

/// Local midnight of a date, so a filter such as `newer:2024-06-30` starts
/// the day when the details, which show local times, would. Where a
/// daylight-saving change skips midnight, the day starts at 1:00.
fn local_midnight(year: i32, month: u32, day: u32) -> Option<i64> {
    use chrono::TimeZone;
    [0, 1].into_iter().find_map(|hour| {
        chrono::Local
            .with_ymd_and_hms(year, month, day, hour, 0, 0)
            .earliest()
            .map(|t| t.timestamp())
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    struct TempDir(PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fixture() -> TempDir {
        fixture_named("delete")
    }

    /// Each test gets its own folder, since tests run in parallel.
    fn fixture_named(name: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!("sb-session-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("old").join("logs")).unwrap();
        std::fs::write(dir.join("old").join("logs").join("a.log"), vec![0; 5000]).unwrap();
        std::fs::write(dir.join("old").join("b.log"), vec![0; 3000]).unwrap();
        std::fs::write(dir.join("keep.txt"), vec![0; 100]).unwrap();
        TempDir(dir)
    }

    fn finished(statuses: &Mutex<Vec<proto::ScanStatus>>) -> bool {
        statuses
            .lock()
            .unwrap()
            .last()
            .is_some_and(|s| s.phase != proto::ScanPhase::Scanning)
    }

    /// Stands in for the trash.
    fn remove(target: &Target) -> Outcome {
        delete::delete_with(target, &delete::tests::TestBin::new())
    }

    fn child(tree: &Tree, parent: NodeId, name: &str) -> NodeId {
        tree.children(parent)
            .find(|&c| tree.name(c) == name)
            .unwrap()
    }

    #[test]
    fn dates_start_at_local_midnight() {
        use chrono::{Datelike, TimeZone, Timelike};
        let secs = local_midnight(2024, 6, 30).unwrap();
        let t = chrono::Local.timestamp_opt(secs, 0).unwrap();
        assert_eq!((t.year(), t.month(), t.day()), (2024, 6, 30));
        assert_eq!((t.hour(), t.minute()), (0, 0));
        assert_eq!(local_midnight(2024, 2, 30), None);
    }

    #[test]
    fn details_give_modified_times_except_clamped_ones() {
        let tmp = fixture_named("modified");
        let set = |name: &str, time| {
            std::fs::File::options()
                .write(true)
                .open(tmp.0.join(name))
                .unwrap()
                .set_modified(time)
                .unwrap();
        };
        set(
            "keep.txt",
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        );
        // Before 1970; filesystems without such times make the test moot.
        let ancient = SystemTime::UNIX_EPOCH.checked_sub(Duration::from_secs(86_400 * 3_650));
        if let Some(ancient) = ancient {
            set(&format!("old{}b.log", std::path::MAIN_SEPARATOR), ancient);
        }
        let statuses = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&statuses);
        let session = Session::start(tmp.0.clone(), move |s| sink.lock().unwrap().push(s));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !finished(&statuses) {
            assert!(Instant::now() < deadline, "scan never finished");
            std::thread::sleep(Duration::from_millis(5));
        }
        let (keep, b) = {
            let tree = session.scan.tree();
            let tree = tree.read().unwrap();
            let old = child(&tree, NodeId::ROOT, "old");
            (
                child(&tree, NodeId::ROOT, "keep.txt").index() as u32,
                child(&tree, old, "b.log").index() as u32,
            )
        };
        assert_eq!(
            session.details(keep, None).unwrap().modified,
            Some(1_700_000_000)
        );
        assert_eq!(
            session.details(0, None).unwrap().modified,
            None,
            "folders have none"
        );
        let stored = std::fs::metadata(tmp.0.join("old").join("b.log"))
            .unwrap()
            .modified()
            .unwrap();
        if stored < SystemTime::UNIX_EPOCH {
            assert_eq!(session.details(b, None).unwrap().modified, None);
        }
    }

    #[test]
    fn suggests_folder_names_and_extensions_from_the_scan() {
        let tmp = fixture_named("suggest");
        let statuses = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&statuses);
        let session = Session::start(tmp.0.clone(), move |s| sink.lock().unwrap().push(s));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !finished(&statuses) {
            assert!(Instant::now() < deadline, "scan never finished");
            std::thread::sleep(Duration::from_millis(5));
        }
        let ask = |kind, typed: &str| {
            session
                .suggest(&proto::SuggestRequest {
                    generation: session.generation(),
                    kind,
                    typed: typed.to_owned(),
                    limit: 10,
                })
                .unwrap()
        };
        let folders = ask(proto::SuggestKind::Folder, "LO");
        assert_eq!(folders.len(), 1);
        assert_eq!(
            (
                folders[0].name.as_str(),
                folders[0].count,
                folders[0].logical
            ),
            ("logs", 1, 5000)
        );
        let exts = ask(proto::SuggestKind::Extension, "");
        let exts: Vec<_> = exts.iter().map(|e| (e.name.as_str(), e.count)).collect();
        assert_eq!(exts, [("log", 2), ("txt", 1)]);
    }

    #[test]
    fn details_report_what_the_filter_matches() {
        let tmp = fixture_named("matching");
        let statuses = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&statuses);
        let session = Session::start(tmp.0.clone(), move |s| sink.lock().unwrap().push(s));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !finished(&statuses) {
            assert!(Instant::now() < deadline, "scan never finished");
            std::thread::sleep(Duration::from_millis(5));
        }
        let old = {
            let tree = session.scan.tree();
            let tree = tree.read().unwrap();
            child(&tree, NodeId::ROOT, "old").index() as u32
        };
        let search = session.search_set("*.log").unwrap().unwrap().search;

        let root = session.details(0, Some(search)).unwrap();
        assert_eq!(
            root.matching,
            Some(proto::Matching {
                files: 2,
                logical: 8000
            })
        );
        assert_eq!(
            (root.files, root.logical),
            (3, 8100),
            "the whole folder too"
        );
        let old = session.details(old, Some(search)).unwrap();
        assert_eq!(
            old.matching,
            Some(proto::Matching {
                files: 2,
                logical: 8000
            })
        );

        assert_eq!(session.details(0, None).unwrap().matching, None);
        assert_eq!(
            session.details(0, Some(search + 7)).unwrap().matching,
            None,
            "a search that isn't current gives no totals, not an error"
        );
    }

    #[test]
    fn lists_the_largest_files_under_a_folder() {
        let tmp = fixture_named("files");
        let statuses = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&statuses);
        let session = Session::start(tmp.0.clone(), move |s| sink.lock().unwrap().push(s));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !finished(&statuses) {
            assert!(Instant::now() < deadline, "scan never finished");
            std::thread::sleep(Duration::from_millis(5));
        }
        let old = {
            let tree = session.scan.tree();
            let tree = tree.read().unwrap();
            child(&tree, NodeId::ROOT, "old").index() as u32
        };
        let request = |folder: u32, offset: u32| proto::FolderFilesRequest {
            generation: session.generation(),
            folder,
            metric: proto::Metric::Logical,
            search: None,
            offset,
            limit: 2,
        };
        let page = session.folder_files(&request(0, 0)).unwrap();
        assert_eq!(page.total, 3);
        let names: Vec<_> = page.files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["a.log", "b.log"], "largest first, from every level");
        let sub = Path::new("old").join("logs");
        assert_eq!(page.files[0].subfolder, display_path(&sub));
        let rest = session.folder_files(&request(0, 2)).unwrap();
        assert_eq!(rest.files[0].name, "keep.txt");
        assert_eq!(rest.files[0].subfolder, "");

        let page = session.folder_files(&request(old, 0)).unwrap();
        assert_eq!(page.total, 2, "only files under the folder");
        assert_eq!(page.files[0].subfolder, "logs");
    }

    /// Runs `ask` on another thread while `remove` is deleted from the
    /// scan, so it waits for the tree's lock while the edit holds it, and
    /// returns what it got.
    fn during_delete<R: Send>(
        session: &Session,
        remove: NodeId,
        ask: impl FnOnce() -> R + Send,
    ) -> R {
        std::thread::scope(|s| {
            let mut asked = None;
            session
                .scan
                .edit_finished(|tree| {
                    asked = Some(s.spawn(ask));
                    // Time for the request to get as far as the tree's lock.
                    std::thread::sleep(Duration::from_millis(100));
                    tree.remove(remove);
                })
                .unwrap();
            asked.unwrap().join().unwrap()
        })
    }

    #[test]
    fn folder_files_asked_during_a_delete_are_ranked_from_the_edited_tree() {
        let tmp = fixture_named("files-delete");
        let statuses = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&statuses);
        let session = Session::start(tmp.0.clone(), move |s| sink.lock().unwrap().push(s));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !finished(&statuses) {
            assert!(Instant::now() < deadline, "scan never finished");
            std::thread::sleep(Duration::from_millis(5));
        }
        let a = {
            let tree = session.scan.tree();
            let tree = tree.read().unwrap();
            let logs = child(&tree, child(&tree, NodeId::ROOT, "old"), "logs");
            child(&tree, logs, "a.log")
        };
        let request = proto::FolderFilesRequest {
            generation: session.generation(),
            folder: 0,
            metric: proto::Metric::Logical,
            search: None,
            offset: 0,
            limit: 10,
        };
        assert_eq!(session.folder_files(&request).unwrap().total, 3);

        // The ranking cached for the old revision isn't served for the
        // edited tree.
        let page = during_delete(&session, a, || session.folder_files(&request)).unwrap();
        let names: Vec<_> = page.files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["b.log", "keep.txt"]);
        assert_eq!(session.folder_files(&request).unwrap().total, 2);
    }

    #[test]
    fn contents_pages_asked_for_together_share_one_walk() {
        let session = load::synthetic(8, 2_000);
        let deadline = Instant::now() + Duration::from_secs(120);
        while !session.scan.progress().state.is_finished() {
            assert!(Instant::now() < deadline, "scan never finished");
            std::thread::sleep(Duration::from_millis(10));
        }
        let nodes = session.scan.tree().read().unwrap().len();
        assert!(nodes > 2 * RANK_STEP, "a walk takes several steps: {nodes}");
        let generation = session.generation();
        let request = move |offset| proto::FolderFilesRequest {
            generation,
            folder: 0,
            metric: proto::Metric::Logical,
            search: None,
            offset,
            limit: 200,
        };

        // Four pages at once, as the Contents tab asks after the scan.
        let start = std::sync::Barrier::new(4);
        let pages: Vec<_> = std::thread::scope(|s| {
            let asked: Vec<_> = (0..4)
                .map(|p| {
                    let (start, session) = (&start, &session);
                    s.spawn(move || {
                        start.wait();
                        session.folder_files(&request(p * 200)).unwrap()
                    })
                })
                .collect();
            asked.into_iter().map(|a| a.join().unwrap()).collect()
        });
        let key = Key {
            scope: Scope::Files,
            folder: NodeId::ROOT,
            metric: layout::Metric::Logical,
            search: None,
        };
        {
            let mut rankings = session.rankings.lock().unwrap();
            let entry = rankings.entry(key);
            assert_eq!(entry.walks(), 1, "one walk, shared");
            assert!(!entry.finishing());
        }

        // The pages fit together into a full sort, from one revision.
        let tree = session.scan.tree();
        let tree = tree.read().unwrap();
        let (files, _) = layout::files_under(&tree, NodeId::ROOT, key.metric, None);
        let mut all: Vec<_> = files
            .into_iter()
            .map(|f| (f.index() as u32, tree.logical(f)))
            .collect();
        all.sort_unstable_by_key(|&(n, w)| (std::cmp::Reverse(w), n));
        for (p, page) in pages.iter().enumerate() {
            assert_eq!(page.revision, session.scan.progress().revision);
            assert_eq!(page.total as usize, all.len());
            let got: Vec<_> = page.files.iter().map(|f| (f.node, f.weight)).collect();
            assert_eq!(got, all[p * 200..(p + 1) * 200], "page {p}");
        }
    }

    #[test]
    fn superseded_layouts_and_rankings_stop_and_leave_the_walk_for_later() {
        let session = load::synthetic(6, 2_000);
        let deadline = Instant::now() + Duration::from_secs(120);
        while !session.scan.progress().state.is_finished() {
            assert!(Instant::now() < deadline, "scan never finished");
            std::thread::sleep(Duration::from_millis(10));
        }
        let generation = session.generation();
        let layout = proto::LayoutRequest {
            generation,
            request: 1,
            view: 0,
            metric: proto::Metric::Logical,
            width: 800.0,
            height: 600.0,
            search: None,
        };
        let files = proto::FolderFilesRequest {
            generation,
            folder: 0,
            metric: proto::Metric::Logical,
            search: None,
            offset: 0,
            limit: 50,
        };
        let superseded = Some(proto::SUPERSEDED.to_owned());
        let stopped = crate::runner::as_superseded(|| session.layout(&layout));
        assert_eq!(stopped.err(), superseded);
        let stopped = crate::runner::as_superseded(|| session.folder_files(&files));
        assert_eq!(stopped.err(), superseded);
        // Outside a superseded job both answer, the ranking from scratch.
        assert!(session.layout(&layout).is_ok());
        assert!(session.folder_files(&files).unwrap().total > 0);
    }

    #[test]
    fn contents_pages_after_a_delete_are_ranked_again_under_the_new_revision() {
        let tmp = fixture_named("contents-delete");
        let statuses = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&statuses);
        let session = Session::start(tmp.0.clone(), move |s| sink.lock().unwrap().push(s));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !finished(&statuses) {
            assert!(Instant::now() < deadline, "scan never finished");
            std::thread::sleep(Duration::from_millis(5));
        }
        let (old, b) = {
            let tree = session.scan.tree();
            let tree = tree.read().unwrap();
            let old = child(&tree, NodeId::ROOT, "old");
            (old, child(&tree, old, "b.log"))
        };
        let search = session.search_set("*.log").unwrap().unwrap().search;
        let files = |offset| proto::FolderFilesRequest {
            generation: session.generation(),
            folder: 0,
            metric: proto::Metric::Logical,
            search: Some(search),
            offset,
            limit: 1,
        };
        let items = proto::SmallItemsRequest {
            generation: session.generation(),
            folder: old.index() as u32,
            metric: proto::Metric::Logical,
            search: None,
            count: u32::MAX,
            offset: 0,
            limit: 10,
        };
        let first = session.folder_files(&files(0)).unwrap();
        assert_eq!((first.files[0].name.as_str(), first.total), ("a.log", 2));
        let before = session.small_items(&items).unwrap();
        assert_eq!(before.total, 2);
        assert_eq!(
            before.revision, first.revision,
            "both ranked after the scan"
        );

        // The second page, asked for after b.log is deleted, comes from a
        // new ranking, which the revision tells apart.
        let ((), progress) = session
            .scan
            .edit_finished(|t| assert!(t.remove(b)))
            .unwrap();
        let second = session.folder_files(&files(1)).unwrap();
        assert_eq!((second.files.len(), second.total), (0, 1));
        assert_eq!(second.revision, progress.revision);
        assert!(second.revision > first.revision);
        let after = session.small_items(&items).unwrap();
        let names: Vec<_> = after.items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["logs"]);
        assert_eq!(after.revision, progress.revision);
    }

    #[test]
    fn filter_totals_asked_during_a_delete_count_the_edited_tree() {
        let tmp = fixture_named("filter-delete");
        let statuses = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&statuses);
        let session = Session::start(tmp.0.clone(), move |s| sink.lock().unwrap().push(s));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !finished(&statuses) {
            assert!(Instant::now() < deadline, "scan never finished");
            std::thread::sleep(Duration::from_millis(5));
        }
        let a = {
            let tree = session.scan.tree();
            let tree = tree.read().unwrap();
            let logs = child(&tree, child(&tree, NodeId::ROOT, "old"), "logs");
            child(&tree, logs, "a.log")
        };
        let search = session.search_set("*.log").unwrap().unwrap().search;
        // Read with the search caught up before the edit; it's brought up
        // to date again rather than counting the deleted file.
        let details = during_delete(&session, a, || session.details(0, Some(search))).unwrap();
        assert_eq!(
            details.matching,
            Some(proto::Matching {
                files: 1,
                logical: 3000
            })
        );
        let summary = session.search_summary(search).unwrap();
        assert_eq!((summary.files, summary.logical), (1, 3000));
        assert_eq!(summary.revision, session.scan.progress().revision);
    }

    #[test]
    fn suggestions_asked_during_a_delete_are_kept_under_the_edited_revision() {
        let tmp = fixture_named("suggest-delete");
        let statuses = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&statuses);
        let session = Session::start(tmp.0.clone(), move |s| sink.lock().unwrap().push(s));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !finished(&statuses) {
            assert!(Instant::now() < deadline, "scan never finished");
            std::thread::sleep(Duration::from_millis(5));
        }
        let old = {
            let tree = session.scan.tree();
            let tree = tree.read().unwrap();
            child(&tree, NodeId::ROOT, "old")
        };
        let request = proto::SuggestRequest {
            generation: session.generation(),
            kind: proto::SuggestKind::Folder,
            typed: "lo".to_owned(),
            limit: 10,
        };
        let found = during_delete(&session, old, || session.suggest(&request)).unwrap();
        assert!(found.is_empty(), "built from the edited tree");
        let names = session.names.lock().unwrap();
        assert_eq!(
            names.as_ref().unwrap().revision,
            session.scan.progress().revision,
            "kept under the revision of the tree it was built from"
        );
    }

    #[test]
    fn deletes_selected_items_and_publishes_the_change() {
        let tmp = fixture();
        let statuses = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&statuses);
        let session = Session::start(tmp.0.clone(), move |s| sink.lock().unwrap().push(s));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !finished(&statuses) {
            assert!(Instant::now() < deadline, "scan never finished");
            std::thread::sleep(Duration::from_millis(5));
        }
        let (old, a, keep) = {
            let tree = session.scan.tree();
            let tree = tree.read().unwrap();
            let old = child(&tree, NodeId::ROOT, "old");
            let logs = child(&tree, old, "logs");
            (
                old,
                child(&tree, logs, "a.log"),
                child(&tree, NodeId::ROOT, "keep.txt"),
            )
        };
        let id = |n: NodeId| n.index() as u32;
        let search = session.search_set("log").unwrap().unwrap();
        assert_eq!(search.files, 2);

        let summary = session.selection(&[id(a), id(old), 0]).unwrap();
        assert_eq!((summary.items, summary.folders, summary.files), (1, 1, 2));
        assert_eq!(summary.logical, 8000);
        assert_eq!(summary.missing, 1, "the scan root is never an item");

        let before = session.scan.progress().revision;
        let report = session
            .delete_using(&[id(a), id(old)], &|_| {}, remove)
            .unwrap();
        assert_eq!(report.recycled, [id(old)]);
        assert_eq!(report.removed, [id(old)]);
        assert!(report.others.is_empty());
        assert_eq!((report.files, report.logical), (2, 8000));
        assert!(!tmp.0.join("old").exists());
        assert!(tmp.0.join("keep.txt").exists());

        let last = statuses.lock().unwrap().last().cloned().unwrap();
        assert_eq!(last.revision, before + 1);
        assert_eq!((last.files, last.dirs, last.logical), (1, 0, 100));
        assert!(session.details(id(old), None).is_err());
        assert!(session.details(id(keep), None).is_ok());
        let summary = session.search_summary(search.search).unwrap();
        assert_eq!(summary.files, 0);

        // Already gone: removed from the scan, never counted as recycled.
        std::fs::remove_file(tmp.0.join("keep.txt")).unwrap();
        let report = session.delete_using(&[id(keep)], &|_| {}, remove).unwrap();
        assert!(report.recycled.is_empty());
        assert_eq!(report.removed, [id(keep)]);
        assert_eq!(report.others.len(), 1);
        assert_eq!(
            report.others[0].outcome,
            proto::DeleteOutcome::AlreadyMissing
        );
        assert_eq!((report.files, report.logical), (0, 0));
        assert_eq!(session.status().files, 0);
    }

    #[test]
    fn leaves_an_item_replaced_since_the_scan_in_place_and_in_the_scan() {
        let tmp = fixture_named("replaced");
        let statuses = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&statuses);
        let session = Session::start(tmp.0.clone(), move |s| sink.lock().unwrap().push(s));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !finished(&statuses) {
            assert!(Instant::now() < deadline, "scan never finished");
            std::thread::sleep(Duration::from_millis(5));
        }
        let keep = {
            let tree = session.scan.tree();
            let tree = tree.read().unwrap();
            child(&tree, NodeId::ROOT, "keep.txt").index() as u32
        };
        let file = tmp.0.join("keep.txt");
        let text = std::fs::read(&file).unwrap();
        std::fs::rename(&file, tmp.0.join("scanned.txt")).unwrap();
        std::fs::write(&file, text).unwrap();
        let report = session.delete_using(&[keep], &|_| {}, remove).unwrap();
        assert!(report.recycled.is_empty() && report.removed.is_empty());
        assert_eq!(report.others[0].outcome, proto::DeleteOutcome::Changed);
        assert_eq!(report.others[0].message, "It was replaced since the scan.");
        assert!(file.exists());
        assert!(session.details(keep, None).is_ok(), "still in the scan");
    }

    #[test]
    fn finds_duplicates_and_drops_deleted_ones() {
        let tmp = fixture_named("duplicates");
        let copy: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(tmp.0.join("copy.bin"), &copy).unwrap();
        std::fs::write(tmp.0.join("old").join("copy-2.bin"), &copy).unwrap();
        std::fs::write(tmp.0.join("old").join("logs").join("copy-3.dat"), &copy).unwrap();
        let mut other = copy.clone();
        other[100_000] ^= 1;
        std::fs::write(tmp.0.join("other.bin"), &other).unwrap();

        let statuses = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&statuses);
        let session = Arc::new(Session::start(tmp.0.clone(), move |s| {
            sink.lock().unwrap().push(s)
        }));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !finished(&statuses) {
            assert!(Instant::now() < deadline, "scan never finished");
            std::thread::sleep(Duration::from_millis(5));
        }
        let find = |search: Option<u32>, candidates: u32| {
            let (tx, rx) = std::sync::mpsc::channel();
            let request = proto::DuplicatesRequest {
                generation: session.generation(),
                folder: 0,
                search,
                min_size: 1000,
            };
            let started = session
                .duplicates_start(&request, move |s| {
                    let _ = tx.send(s);
                })
                .unwrap();
            assert_eq!(started.candidates, candidates);
            loop {
                let s = rx.recv_timeout(Duration::from_secs(10)).unwrap();
                if s.phase != proto::DuplicatesPhase::Running {
                    return s;
                }
            }
        };

        // Same size and at least 1000 bytes.
        let done = find(None, 4);
        assert_eq!(done.phase, proto::DuplicatesPhase::Complete);
        assert_eq!((done.groups, done.files), (1, 3));
        assert_eq!(done.reclaimable, 400_000);
        assert_eq!(done.done_bytes, done.total_bytes);
        let list = session.duplicates_list(done.job).unwrap();
        let paths: Vec<_> = list.groups[0]
            .files
            .iter()
            .map(|f| f.path.as_str())
            .collect();
        assert_eq!(
            paths,
            [
                display_path(&tmp.0.join("copy.bin")),
                display_path(&tmp.0.join("old").join("copy-2.bin")),
                display_path(&tmp.0.join("old").join("logs").join("copy-3.dat")),
            ],
            "shortest path first"
        );

        // Deleting every copy, or all but one that has gone meanwhile, is
        // flagged; keeping one that's still there isn't.
        let nodes: Vec<u32> = list.groups[0].files.iter().map(|f| f.node).collect();
        let check = |picked: &[u32]| session.duplicates_check(done.job, picked).unwrap();
        assert_eq!(
            check(&nodes[1..]).all_selected + check(&nodes[1..]).kept_gone,
            0
        );
        assert_eq!(check(&nodes).all_selected, 1);
        let old = {
            let tree = session.scan.tree();
            let tree = tree.read().unwrap();
            child(&tree, NodeId::ROOT, "old").index() as u32
        };
        assert_eq!(
            check(&[old, nodes[0]]).all_selected,
            1,
            "a selected folder takes the copies in it"
        );
        let kept = tmp.0.join("copy.bin");
        let scanned_time = std::fs::metadata(&kept).unwrap().modified().unwrap();
        std::fs::write(&kept, b"changed").unwrap();
        let risky = check(&nodes[1..]);
        assert_eq!((risky.all_selected, risky.kept_gone), (0, 1));
        assert_eq!(
            risky.examples,
            [display_path(&tmp.0.join("old").join("copy-2.bin"))]
        );
        std::fs::write(&kept, &copy).unwrap();
        // As scanned again, so it can be deleted below.
        std::fs::File::options()
            .write(true)
            .open(&kept)
            .unwrap()
            .set_modified(scanned_time)
            .unwrap();

        // With a filter, only matching files count.
        let search = session.search_set("*.bin").unwrap().unwrap().search;
        let filtered = find(Some(search), 3);
        assert_eq!((filtered.groups, filtered.files), (1, 2));
        assert_eq!(filtered.query.as_deref(), Some("*.bin"));

        // Deleting one copy leaves a group of one, which is no group.
        let list = session.duplicates_list(filtered.job).unwrap();
        let first = list.groups[0].files[0].node;
        session.delete_using(&[first], &|_| {}, remove).unwrap();
        let after = session.duplicates_status(filtered.job).unwrap();
        assert_eq!((after.groups, after.reclaimable), (0, 0));
        assert_ne!(after.version, filtered.version);
        // Deleting the copy left would still lose the last of them.
        let last = list.groups[0].files[1].node;
        let check = session.duplicates_check(filtered.job, &[last]).unwrap();
        assert_eq!((check.all_selected, check.kept_gone), (1, 0));
        assert!(
            session.duplicates_list(done.job).is_err(),
            "replaced by the newer search"
        );
    }

    #[test]
    fn deletes_only_files_whose_other_copy_is_confirmed() {
        let tmp = fixture_named("recheck");
        let copy: Vec<u8> = (0..300_000u32).map(|i| (i % 253) as u8).collect();
        let (kept, extra) = (tmp.0.join("kept.bin"), tmp.0.join("old").join("extra.bin"));
        let (kept2, extra2) = (
            tmp.0.join("kept2.dat"),
            tmp.0.join("old").join("extra2.dat"),
        );
        let other: Vec<u8> = copy.iter().map(|b| b ^ 0x5A).collect();
        std::fs::write(&kept, &copy).unwrap();
        std::fs::write(&extra, &copy).unwrap();
        std::fs::write(&kept2, &other).unwrap();
        std::fs::write(&extra2, &other).unwrap();
        let (kept3, extra3) = (
            tmp.0.join("kept3.txt"),
            tmp.0.join("old").join("extra3.txt"),
        );
        let third: Vec<u8> = copy.iter().map(|b| b ^ 0x33).collect();
        std::fs::write(&kept3, &third).unwrap();
        std::fs::write(&extra3, &third).unwrap();

        let statuses = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&statuses);
        let session = Arc::new(Session::start(tmp.0.clone(), move |s| {
            sink.lock().unwrap().push(s)
        }));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !finished(&statuses) {
            assert!(Instant::now() < deadline, "scan never finished");
            std::thread::sleep(Duration::from_millis(5));
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let request = proto::DuplicatesRequest {
            generation: session.generation(),
            folder: 0,
            search: None,
            min_size: 1000,
        };
        session
            .duplicates_start(&request, move |s| {
                let _ = tx.send(s);
            })
            .unwrap();
        let done = loop {
            let s = rx.recv_timeout(Duration::from_secs(10)).unwrap();
            if s.phase != proto::DuplicatesPhase::Running {
                break s;
            }
        };
        assert_eq!((done.groups, done.skipped.changing), (3, 0));
        let id = |p: &Path| {
            let tree = session.scan.tree();
            let tree = tree.read().unwrap();
            let mut n = NodeId::ROOT;
            for part in p.strip_prefix(&tmp.0).unwrap() {
                n = child(&tree, n, part.to_str().unwrap());
            }
            n.index() as u32
        };
        let (e1, e2, e3) = (id(&extra), id(&extra2), id(&extra3));

        // The kept copy is overwritten with other bytes of the same length.
        let mut edited = copy.clone();
        edited[150_000] ^= 0xFF;
        std::fs::write(&kept, &edited).unwrap();
        let f = std::fs::File::options().write(true).open(&kept).unwrap();
        f.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000))
            .unwrap();
        drop(f);
        let check = session.duplicates_check(done.job, &[e1, e2]).unwrap();
        assert_eq!(
            (check.kept_gone, check.to_recheck),
            (1, 2),
            "the same size is no longer proof"
        );

        let progress = Mutex::new(Vec::new());
        let report = session
            .delete_using(&[e1, e2], &|p| progress.lock().unwrap().push(p), remove)
            .unwrap();
        assert_eq!(report.recycled, [e2], "its copy is unchanged");
        assert_eq!(report.others.len(), 1);
        assert_eq!(report.others[0].node, e1);
        assert_eq!(report.others[0].outcome, proto::DeleteOutcome::Kept);
        assert_eq!(
            report.others[0].message,
            duplicates::Unconfirmed::Changed.message("extra.bin")
        );
        assert!(extra.exists() && !extra2.exists() && kept.exists());
        let progress = progress.into_inner().unwrap();
        let last = progress.last().unwrap();
        assert!(last.finished);
        assert_eq!(last.total_bytes, 2 * copy.len() as u64);
        assert_eq!(last.done_bytes, last.total_bytes);
        assert!(progress.iter().all(|p| p.done_bytes <= p.total_bytes));

        // An unchanged copy that can't be read leaves it unknown, which is
        // never taken as proof.
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            let locked = std::fs::File::options()
                .read(true)
                .share_mode(0)
                .open(&kept3)
                .unwrap();
            let report = session.delete_using(&[e3], &|_| {}, remove).unwrap();
            assert!(report.recycled.is_empty());
            assert_eq!(
                report.others[0].message,
                duplicates::Unconfirmed::Unknown.message("extra3.txt")
            );
            drop(locked);
            assert!(extra3.exists());
        }
        // Confirmed, the copy that stays is held until the moves are done
        // (on Windows), without keeping the selected file from going.
        let report = session
            .delete_using(&[e3], &|_| {}, |target| {
                let write = std::fs::File::options().append(true).open(&kept3);
                assert_eq!(write.is_err(), cfg!(windows), "{write:?}");
                remove(target)
            })
            .unwrap();
        assert_eq!(report.recycled, [e3]);
        assert!(!extra3.exists() && kept3.exists());
        std::fs::File::options()
            .append(true)
            .open(&kept3)
            .expect("released after the moves");

        // A selected file that changed itself no longer matches its copies.
        std::fs::write(&kept, &copy).unwrap();
        std::fs::write(&extra, &edited).unwrap();
        let f = std::fs::File::options().write(true).open(&extra).unwrap();
        f.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000))
            .unwrap();
        drop(f);
        let report = session.delete_using(&[e1], &|_| {}, remove).unwrap();
        assert_eq!(
            report.others[0].message,
            duplicates::Unconfirmed::NoLongerMatches.message("extra.bin")
        );

        // Duplicates in a selected folder go with it: only files selected
        // themselves are kept.
        let old = id(&tmp.0.join("old"));
        let report = session.delete_using(&[old], &|_| {}, remove).unwrap();
        assert_eq!(report.recycled, [old]);
        assert!(!extra.exists() && kept.exists());
    }
}
