//! In-memory filesystem for exercising the engine without touching disk.

use crate::adapter::{
    Entry, EntryKind, ErrorKind, FsAdapter, Identity, Measured, RootKind, ScanError, Sink, batched,
};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

#[derive(Debug, Clone)]
enum Node {
    Dir,
    File {
        logical: u64,
        allocated: Option<u64>,
        identity: Identity,
        cloud: bool,
        modified: Option<SystemTime>,
    },
    Other(EntryKind),
}

/// The listing's side of a [`Gate`]: says when it reaches the gate, then
/// waits there.
struct GateWaiter {
    entered: Sender<()>,
    release: Mutex<Receiver<()>>,
}

impl GateWaiter {
    fn wait(&self) {
        let _ = self.entered.send(());
        let _ = self.release.lock().unwrap().recv();
    }
}

type GateReceiver = Arc<GateWaiter>;

/// A fake filesystem rooted at [`FakeFs::ROOT`]. Paths given to the builder
/// are relative to the root and use `/` separators.
#[derive(Clone, Default)]
pub struct FakeFs {
    nodes: Arc<Mutex<BTreeMap<PathBuf, Node>>>,
    errors: Arc<Mutex<BTreeMap<PathBuf, ScanError>>>,
    gates: Arc<Mutex<BTreeMap<PathBuf, GateReceiver>>>,
    /// Listings that fail after this many entries.
    late_errors: Arc<Mutex<BTreeMap<PathBuf, (usize, ScanError)>>>,
    /// Listings that block after this many entries.
    late_gates: Arc<Mutex<BTreeMap<PathBuf, (usize, GateReceiver)>>>,
    measured: Arc<Mutex<BTreeMap<PathBuf, Measured>>>,
    root_kind: Arc<Mutex<Option<RootKind>>>,
}

/// Releases a gated directory listing when dropped or sent to.
pub struct Gate {
    release: Sender<()>,
    entered: Receiver<()>,
}

impl Gate {
    fn new() -> (Self, GateReceiver) {
        let (release, release_rx) = channel();
        let (entered_tx, entered) = channel();
        let receiver = GateWaiter {
            entered: entered_tx,
            release: Mutex::new(release_rx),
        };
        (Self { release, entered }, Arc::new(receiver))
    }

    pub fn release(self) {
        let _ = self.release.send(());
    }

    /// Waits up to `timeout` for a listing to reach the gate, where it then
    /// stays until released; whether one did.
    pub fn entered(&self, timeout: Duration) -> bool {
        self.entered.recv_timeout(timeout).is_ok()
    }
}

impl FakeFs {
    pub const ROOT: &'static str = "/fake";

    pub fn new() -> Self {
        let fs = Self::default();
        fs.nodes
            .lock()
            .unwrap()
            .insert(PathBuf::from(Self::ROOT), Node::Dir);
        fs
    }

    fn abs(path: &str) -> PathBuf {
        let mut p = PathBuf::from(Self::ROOT);
        for part in path.split('/').filter(|s| !s.is_empty()) {
            p.push(part);
        }
        p
    }

    fn insert(&self, path: &str, node: Node) -> &Self {
        let abs = Self::abs(path);
        let mut nodes = self.nodes.lock().unwrap();
        let mut ancestor = abs.parent();
        while let Some(a) = ancestor {
            if a == Path::new(Self::ROOT) || a.as_os_str().is_empty() {
                break;
            }
            nodes.entry(a.to_path_buf()).or_insert(Node::Dir);
            ancestor = a.parent();
        }
        nodes.insert(abs, node);
        self
    }

    pub fn dir(&self, path: &str) -> &Self {
        self.insert(path, Node::Dir)
    }

    pub fn file(&self, path: &str, logical: u64, allocated: u64) -> &Self {
        self.insert(
            path,
            Node::File {
                logical,
                allocated: Some(allocated),
                identity: Identity::Unique,
                cloud: false,
                modified: None,
            },
        )
    }

    pub fn file_unknown_allocation(&self, path: &str, logical: u64) -> &Self {
        self.insert(
            path,
            Node::File {
                logical,
                allocated: None,
                identity: Identity::Unique,
                cloud: false,
                modified: None,
            },
        )
    }

    pub fn cloud_file(&self, path: &str, logical: u64, allocated: u64) -> &Self {
        self.insert(
            path,
            Node::File {
                logical,
                allocated: Some(allocated),
                identity: Identity::Unique,
                cloud: true,
                modified: None,
            },
        )
    }

    /// A file sharing `key` with other names (a hard link).
    pub fn linked_file(&self, path: &str, logical: u64, allocated: u64, key: u128) -> &Self {
        self.insert(
            path,
            Node::File {
                logical,
                allocated: Some(allocated),
                identity: Identity::MaybeShared { key },
                cloud: false,
                modified: None,
            },
        )
    }

    /// Sets the modified time of the file at `path`, added earlier.
    pub fn set_modified(&self, path: &str, time: SystemTime) -> &Self {
        if let Some(Node::File { modified, .. }) =
            self.nodes.lock().unwrap().get_mut(&Self::abs(path))
        {
            *modified = Some(time);
        }
        self
    }

    pub fn other(&self, path: &str, kind: EntryKind) -> &Self {
        self.insert(path, Node::Other(kind))
    }

    /// Makes listing `path` fail.
    pub fn fail_dir(&self, path: &str, kind: ErrorKind) -> &Self {
        self.dir(path);
        self.errors
            .lock()
            .unwrap()
            .insert(Self::abs(path), ScanError::new(kind, "injected"));
        self
    }

    /// Makes listing `path` block until the returned gate is released or dropped.
    pub fn gate_dir(&self, path: &str) -> Gate {
        self.dir(path);
        let (gate, receiver) = Gate::new();
        self.gates.lock().unwrap().insert(Self::abs(path), receiver);
        gate
    }

    /// Makes listing `path` fail after its first `entries` entries (in name
    /// order) were handed over.
    pub fn fail_dir_after(&self, path: &str, entries: usize, kind: ErrorKind) -> &Self {
        self.dir(path);
        self.late_errors
            .lock()
            .unwrap()
            .insert(Self::abs(path), (entries, ScanError::new(kind, "injected")));
        self
    }

    /// Makes listing `path` block after its first `entries` entries (in name
    /// order) were handed over, until the returned gate is released or
    /// dropped. Entries are handed over in whole batches, so `entries` should
    /// be a multiple of [`BATCH_ENTRIES`](crate::adapter::BATCH_ENTRIES) for
    /// all of them to arrive first.
    pub fn gate_dir_after(&self, path: &str, entries: usize) -> Gate {
        self.dir(path);
        let (gate, receiver) = Gate::new();
        self.late_gates
            .lock()
            .unwrap()
            .insert(Self::abs(path), (entries, receiver));
        gate
    }

    /// Sets what [`FsAdapter::measure_file`] returns for `path`.
    pub fn set_measured(&self, path: &str, logical: u64, allocated: u64) -> &Self {
        self.measured.lock().unwrap().insert(
            Self::abs(path),
            Measured {
                logical,
                allocated: Some(allocated),
            },
        );
        self
    }

    pub fn set_root_kind(&self, kind: RootKind) -> &Self {
        *self.root_kind.lock().unwrap() = Some(kind);
        self
    }

    pub fn root(&self) -> PathBuf {
        PathBuf::from(Self::ROOT)
    }
}

impl FsAdapter for FakeFs {
    fn open_root(&self, root: &Path) -> Result<RootKind, ScanError> {
        if let Some(kind) = *self.root_kind.lock().unwrap() {
            return Ok(kind);
        }
        match self.nodes.lock().unwrap().get(root) {
            Some(Node::Dir) => Ok(RootKind::Directory),
            Some(Node::Other(EntryKind::Symlink | EntryKind::MountPoint)) => Ok(RootKind::Link),
            Some(_) => Ok(RootKind::NotDirectory),
            None => Err(ScanError::new(ErrorKind::Vanished, "no such root")),
        }
    }

    fn read_dir(&self, dir: &Path, sink: &mut Sink<'_>) -> Result<(), ScanError> {
        let gate = self.gates.lock().unwrap().get(dir).cloned();
        if let Some(gate) = gate {
            gate.wait();
        }
        if let Some(err) = self.errors.lock().unwrap().get(dir) {
            return Err(err.clone());
        }
        let late_error = self.late_errors.lock().unwrap().get(dir).cloned();
        let late_gate = self.late_gates.lock().unwrap().get(dir).cloned();
        let entries = self.entries(dir);
        batched(sink, |out| {
            for (i, entry) in entries.into_iter().enumerate() {
                if let Some((at, err)) = &late_error
                    && i == *at
                {
                    return Err(err.clone());
                }
                if let Some((at, gate)) = &late_gate
                    && i == *at
                {
                    gate.wait();
                }
                if out.push(entry).is_break() {
                    return Ok(());
                }
            }
            Ok(())
        })
    }

    fn measure_file(&self, file: &Path) -> Result<Measured, ScanError> {
        if let Some(m) = self.measured.lock().unwrap().get(file) {
            return Ok(*m);
        }
        match self.nodes.lock().unwrap().get(file) {
            Some(Node::File {
                logical, allocated, ..
            }) => Ok(Measured {
                logical: *logical,
                allocated: *allocated,
            }),
            _ => Err(ScanError::new(ErrorKind::Vanished, "not a file")),
        }
    }
}

impl FakeFs {
    /// The direct children of `dir` in name order.
    fn entries(&self, dir: &Path) -> Vec<Entry> {
        let nodes = self.nodes.lock().unwrap();
        nodes
            .iter()
            .filter(|(p, _)| p.parent() == Some(dir))
            .map(|(p, node)| {
                let name: OsString = p.file_name().unwrap().to_owned();
                match node {
                    Node::Dir => Entry {
                        name,
                        kind: EntryKind::Directory,
                        logical: 0,
                        allocated: Some(0),
                        identity: Identity::Unique,
                        id: 0,
                        cloud: false,
                        modified: None,
                    },
                    Node::File {
                        logical,
                        allocated,
                        identity,
                        cloud,
                        modified,
                    } => Entry {
                        name,
                        kind: EntryKind::File,
                        logical: *logical,
                        allocated: *allocated,
                        identity: *identity,
                        // Linked files stand in for objects with an ID.
                        id: match identity {
                            Identity::MaybeShared { key } => *key,
                            _ => 0,
                        },
                        cloud: *cloud,
                        modified: *modified,
                    },
                    Node::Other(kind) => Entry {
                        name,
                        kind: *kind,
                        logical: 0,
                        allocated: Some(0),
                        identity: Identity::Unique,
                        id: 0,
                        cloud: false,
                        modified: None,
                    },
                }
            })
            .collect()
    }
}
