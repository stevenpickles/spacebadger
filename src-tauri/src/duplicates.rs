//! A search for duplicate files in one scan, run in the background on
//! request. See `sb_scan::duplicates` for how files are compared; this keeps
//! what was found and reports it, leaving out files deleted since.

use crate::session::display_path;
use sb_core::selection;
use sb_core::tree::{NodeId, Tree, flags};
use sb_protocol as proto;
use sb_scan::duplicates::{self, Contents, Counters, Event, Group, Plan, Skip};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// While reading, status updates go out at most this often.
pub const REPORT_EVERY: Duration = Duration::from_millis(250);

/// What to look through, fixed when the search starts.
pub struct Scope {
    pub generation: u64,
    pub folder: u32,
    pub path: String,
    pub query: Option<String>,
    pub min_size: u64,
}

pub struct Job {
    pub id: u32,
    scope: Scope,
    plan: Plan,
    started: Instant,
    cancel: AtomicBool,
    counters: Counters,
    found: Mutex<Found>,
}

struct Found {
    groups: Vec<Group>,
    skipped: proto::DuplicateSkips,
    /// Bumped per group found.
    version: u32,
    phase: proto::DuplicatesPhase,
    elapsed: Option<Duration>,
}

impl Job {
    pub fn new(id: u32, scope: Scope, plan: Plan) -> Self {
        let skipped = proto::DuplicateSkips {
            cloud: plan.cloud.len() as u32,
            ..Default::default()
        };
        Self {
            id,
            scope,
            plan,
            started: Instant::now(),
            cancel: AtomicBool::new(false),
            counters: Counters::default(),
            found: Mutex::new(Found {
                groups: Vec::new(),
                skipped,
                version: 0,
                phase: proto::DuplicatesPhase::Running,
                elapsed: None,
            }),
        }
    }

    /// Stops reading; what was found stays.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn is_running(&self) -> bool {
        self.found.lock().unwrap().phase == proto::DuplicatesPhase::Running
    }

    /// Reads the planned files, recording groups as they're confirmed.
    /// Blocks until done or cancelled.
    /// A reader that panics ends the search as if stopped, keeping what
    /// was found, rather than leaving it running.
    pub fn run(&self, contents: &impl Contents) {
        let complete = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            duplicates::run(
                &self.plan,
                contents,
                duplicates::READERS,
                &self.cancel,
                &self.counters,
                |event| self.record(event),
            )
        }))
        .unwrap_or(false);
        let mut found = self.found.lock().unwrap_or_else(|e| e.into_inner());
        found.phase = if complete {
            proto::DuplicatesPhase::Complete
        } else {
            proto::DuplicatesPhase::Cancelled
        };
        found.elapsed = Some(self.started.elapsed());
    }

    fn record(&self, event: Event) {
        let mut found = self.found.lock().unwrap();
        match event {
            Event::Found(group) => {
                found.groups.push(group);
                found.version += 1;
            }
            Event::Skipped(_, why) => {
                let s = &mut found.skipped;
                let count = match why {
                    Skip::Cloud => &mut s.cloud,
                    Skip::Vanished => &mut s.vanished,
                    Skip::Changed => &mut s.changed,
                    Skip::Special => &mut s.special,
                    Skip::InUse => &mut s.in_use,
                    Skip::Denied => &mut s.denied,
                    Skip::Unreadable => &mut s.unreadable,
                };
                *count += 1;
            }
        }
    }

    /// The groups with two or more files still in the scan, most
    /// reclaimable space first.
    fn current(&self, tree: &Tree) -> (Vec<Group>, Summary) {
        let found = self.found.lock().unwrap();
        let mut groups: Vec<Group> = found
            .groups
            .iter()
            .filter_map(|g| {
                let files: Vec<NodeId> = g
                    .files
                    .iter()
                    .copied()
                    .filter(|&n| tree.contains(n) && !tree.is_removed(n))
                    .collect();
                (files.len() >= 2).then_some(Group {
                    size: g.size,
                    files,
                })
            })
            .collect();
        groups.sort_by(|a, b| {
            reclaimable(b)
                .cmp(&reclaimable(a))
                .then(b.size.cmp(&a.size))
        });
        let summary = Summary {
            skipped: found.skipped.clone(),
            version: found.version,
            phase: found.phase,
            elapsed: found.elapsed.unwrap_or_else(|| self.started.elapsed()),
        };
        (groups, summary)
    }

    /// How the search is going; `revision` is the scan's, which changes
    /// when items are deleted.
    pub fn status(&self, tree: &Tree, revision: u64) -> proto::DuplicatesStatus {
        let (groups, found) = self.current(tree);
        proto::DuplicatesStatus {
            generation: self.scope.generation,
            job: self.id,
            folder: self.scope.folder,
            path: self.scope.path.clone(),
            query: self.scope.query.clone(),
            min_size: self.scope.min_size,
            phase: found.phase,
            elapsed_ms: found.elapsed.as_millis() as u64,
            candidates: self.plan.files() as u32,
            total_bytes: self.plan.bytes,
            done_bytes: self.counters.done.load(Ordering::Relaxed),
            read_bytes: self.counters.read.load(Ordering::Relaxed),
            groups: groups.len() as u32,
            files: groups.iter().map(|g| g.files.len() as u32).sum(),
            reclaimable: groups.iter().map(reclaimable).sum(),
            skipped: found.skipped,
            version: found.version.wrapping_add(revision as u32),
        }
    }

    /// The groups found so far, each with its shortest path first.
    pub fn list(&self, tree: &Tree, revision: u64) -> proto::DuplicatesList {
        let (groups, found) = self.current(tree);
        let total = groups.len() as u32;
        let groups = groups
            .into_iter()
            .take(proto::DUPLICATE_GROUPS_LISTED as usize)
            .map(|g| {
                let mut files: Vec<proto::DuplicateFile> = g
                    .files
                    .iter()
                    .map(|&n| proto::DuplicateFile {
                        node: n.index() as u32,
                        parent: tree.parent(n).unwrap_or(NodeId::ROOT).index() as u32,
                        path: display_path(&tree.path(n)),
                        hardlinked: tree.flags(n) & flags::HARDLINKED != 0,
                    })
                    .collect();
                files.sort_by(|a, b| {
                    let len = |f: &proto::DuplicateFile| f.path.chars().count();
                    len(a).cmp(&len(b)).then_with(|| a.path.cmp(&b.path))
                });
                proto::DuplicateGroup {
                    size: g.size,
                    files,
                }
            })
            .collect();
        proto::DuplicatesList {
            generation: self.scope.generation,
            job: self.id,
            version: found.version.wrapping_add(revision as u32),
            total,
            groups,
        }
    }
}

/// Most groups whose staying copies one check looks for on disk, and
/// most copies it looks for per group, so a slow share can't hold up the
/// delete dialog for long.
const CHECK_GROUPS: usize = 200;
const CHECK_COPIES: usize = 8;

/// A delete check whose remaining part looks at the disk, done without
/// holding any lock.
pub struct PendingCheck {
    check: proto::DuplicatesCheck,
    /// Per group: a file that would go, the size, and copies that would stay.
    on_disk: Vec<(String, u64, Vec<PathBuf>)>,
}

impl Job {
    /// Starts checking which groups deleting `selected` (with everything in
    /// selected folders) would leave with no copy: every remaining file
    /// selected, or the ones that would stay no longer on disk at the size
    /// compared. Groups already down to one file count too.
    pub fn check(&self, tree: &Tree, selected: &[u32]) -> PendingCheck {
        let items: HashSet<NodeId> = selection::resolve(tree, selected)
            .items
            .into_iter()
            .collect();
        let picked = |n: NodeId| {
            let mut cur = Some(n);
            while let Some(c) = cur {
                if items.contains(&c) {
                    return true;
                }
                cur = tree.parent(c);
            }
            false
        };
        let mut pending = PendingCheck {
            check: proto::DuplicatesCheck {
                all_selected: 0,
                kept_gone: 0,
                unchecked: 0,
                examples: Vec::new(),
            },
            on_disk: Vec::new(),
        };
        let found = self.found.lock().unwrap();
        for g in &found.groups {
            let (going, staying): (Vec<NodeId>, Vec<NodeId>) = g
                .files
                .iter()
                .copied()
                .filter(|&n| tree.contains(n) && !tree.is_removed(n))
                .partition(|&n| picked(n));
            let Some(&example) = going.first() else {
                continue;
            };
            let example = display_path(&tree.path(example));
            if staying.is_empty() {
                pending.check.all_selected += 1;
                pending.example(example);
            } else if pending.on_disk.len() < CHECK_GROUPS {
                let copies = staying
                    .iter()
                    .take(CHECK_COPIES)
                    .map(|&n| tree.path(n))
                    .collect();
                pending.on_disk.push((example, g.size, copies));
            } else {
                pending.check.unchecked += 1;
            }
        }
        pending
    }
}

impl PendingCheck {
    fn example(&mut self, path: String) {
        if self.check.examples.len() < proto::DUPLICATE_CHECK_EXAMPLES {
            self.check.examples.push(path);
        }
    }

    /// Looks on disk for the copies that would stay.
    pub fn finish(mut self) -> proto::DuplicatesCheck {
        for (example, size, copies) in std::mem::take(&mut self.on_disk) {
            if !copies.iter().any(|p| still_there(p, size)) {
                self.check.kept_gone += 1;
                self.example(example);
            }
        }
        self.check
    }
}

/// Whether a plain file of `size` bytes is still at `path`.
fn still_there(path: &Path, size: u64) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file() && m.len() == size)
}

/// What [`Job::current`] copies out besides the groups.
struct Summary {
    skipped: proto::DuplicateSkips,
    version: u32,
    phase: proto::DuplicatesPhase,
    elapsed: Duration,
}

/// Space the copies beyond the first take.
fn reclaimable(g: &Group) -> u64 {
    g.size * (g.files.len() as u64).saturating_sub(1)
}
