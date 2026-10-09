//! One scan and the view state derived from it, plus conversions from the
//! scanner's types to the protocol's.

use sb_core::layout::{self, LayoutParams, OrderCache, RectKind, rect_flags};
use sb_core::tree::{DirState, NodeId, Tree, flags};
use sb_protocol as proto;
use sb_scan::{NativeFs, OmissionReason, Progress, Scan, ScanState, native};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Files narrower or shorter than this (CSS px) get no label.
const FILE_LABEL_MIN: (f32, f32) = (40.0, 14.0);

pub struct Session {
    pub root: PathBuf,
    pub scan: Scan,
    order: Mutex<OrderCache>,
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
        let tree = self.scan.tree();
        let tree = tree.read().map_err(|_| "scan data is unavailable")?;
        let view = node(&tree, req.view)?;
        let metric = match req.metric {
            proto::Metric::Allocated => layout::Metric::Allocated,
            proto::Metric::Logical => layout::Metric::Logical,
        };
        let params = LayoutParams::new(
            req.width.clamp(0.0, 16_384.0),
            req.height.clamp(0.0, 16_384.0),
        );
        // While scanning, keep sibling order stable; once finished, sort afresh.
        let result = {
            let mut order = self
                .order
                .lock()
                .map_err(|_| "layout state is unavailable")?;
            layout::layout(&tree, view, metric, None, &params, &mut order, !finished)
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
