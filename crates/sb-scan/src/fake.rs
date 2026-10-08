//! In-memory filesystem for exercising the engine without touching disk.

use crate::adapter::{
    Entry, EntryKind, ErrorKind, FsAdapter, Identity, Measured, RootKind, ScanError,
};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
enum Node {
    Dir,
    File {
        logical: u64,
        allocated: Option<u64>,
        identity: Identity,
        cloud: bool,
    },
    Other(EntryKind),
}

type GateReceiver = Arc<Mutex<Receiver<()>>>;

/// A fake filesystem rooted at [`FakeFs::ROOT`]. Paths given to the builder
/// are relative to the root and use `/` separators.
#[derive(Clone, Default)]
pub struct FakeFs {
    nodes: Arc<Mutex<BTreeMap<PathBuf, Node>>>,
    errors: Arc<Mutex<BTreeMap<PathBuf, ScanError>>>,
    gates: Arc<Mutex<BTreeMap<PathBuf, GateReceiver>>>,
    measured: Arc<Mutex<BTreeMap<PathBuf, Measured>>>,
    root_kind: Arc<Mutex<Option<RootKind>>>,
}

/// Releases a gated directory listing when dropped or sent to.
pub struct Gate(Sender<()>);

impl Gate {
    pub fn release(self) {
        let _ = self.0.send(());
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
            },
        )
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
        let (tx, rx) = channel();
        self.gates
            .lock()
            .unwrap()
            .insert(Self::abs(path), Arc::new(Mutex::new(rx)));
        Gate(tx)
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

    fn read_dir(&self, dir: &Path) -> Result<Vec<Entry>, ScanError> {
        let gate = self.gates.lock().unwrap().get(dir).cloned();
        if let Some(gate) = gate {
            let _ = gate.lock().unwrap().recv();
        }
        if let Some(err) = self.errors.lock().unwrap().get(dir) {
            return Err(err.clone());
        }
        let nodes = self.nodes.lock().unwrap();
        let entries = nodes
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
                        cloud: false,
                    },
                    Node::File {
                        logical,
                        allocated,
                        identity,
                        cloud,
                    } => Entry {
                        name,
                        kind: EntryKind::File,
                        logical: *logical,
                        allocated: *allocated,
                        identity: *identity,
                        cloud: *cloud,
                    },
                    Node::Other(kind) => Entry {
                        name,
                        kind: *kind,
                        logical: 0,
                        allocated: Some(0),
                        identity: Identity::Unique,
                        cloud: false,
                    },
                }
            })
            .collect();
        Ok(entries)
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
