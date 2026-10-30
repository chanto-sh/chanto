//! The file system the tools see: the disk for the CLI, memory in the
//! browser. Paths are absolute; the tools resolve them first.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    /// A pipe, socket or device: not something to read as a file.
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Meta {
    pub kind: Kind,
    pub len: u64,
    pub modified: Option<SystemTime>,
}

impl Meta {
    pub fn is_file(&self) -> bool {
        self.kind == Kind::File
    }
    pub fn is_dir(&self) -> bool {
        self.kind == Kind::Dir
    }
}

/// One name in a directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
}

pub trait Fs: Send + Sync {
    /// What `p` is, following symlinks.
    fn metadata(&self, p: &Path) -> io::Result<Meta>;
    fn read(&self, p: &Path) -> io::Result<Vec<u8>>;
    /// A stream of the file, for reading a window of a large one.
    fn open(&self, p: &Path) -> io::Result<Box<dyn io::Read + Send>>;
    /// The names in a directory, in no particular order.
    fn read_dir(&self, p: &Path) -> io::Result<Vec<Entry>>;
}

/// The operating system's file system.
#[cfg(not(target_family = "wasm"))]
pub struct NativeFs;

#[cfg(not(target_family = "wasm"))]
impl Fs for NativeFs {
    fn metadata(&self, p: &Path) -> io::Result<Meta> {
        let m = std::fs::metadata(p)?;
        let kind = if m.is_file() {
            Kind::File
        } else if m.is_dir() {
            Kind::Dir
        } else {
            Kind::Other
        };
        Ok(Meta {
            kind,
            len: m.len(),
            modified: m.modified().ok(),
        })
    }
    fn read(&self, p: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(p)
    }
    fn open(&self, p: &Path) -> io::Result<Box<dyn io::Read + Send>> {
        Ok(Box::new(std::fs::File::open(p)?))
    }
    fn read_dir(&self, p: &Path) -> io::Result<Vec<Entry>> {
        Ok(std::fs::read_dir(p)?
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                Some(Entry {
                    is_dir: e.file_type().is_ok_and(|t| t.is_dir()),
                    name: e.file_name().to_str()?.to_string(),
                })
            })
            .collect())
    }
}

enum Node {
    File {
        bytes: Vec<u8>,
        modified: SystemTime,
    },
    Dir,
}

/// A file system held in memory, for the browser and for tests. The root
/// always exists; writing a file creates its parent directories.
///
/// Modification times count up from the Unix epoch, one tick per write: the
/// browser has no `SystemTime::now`, and the tools only compare them.
#[derive(Default)]
pub struct MemFs {
    nodes: Mutex<BTreeMap<PathBuf, Node>>,
    ticks: Mutex<u64>,
}

impl MemFs {
    pub fn new() -> MemFs {
        MemFs::default()
    }

    /// Create or replace a file, and its parent directories.
    pub fn insert(&self, p: impl AsRef<Path>, bytes: impl Into<Vec<u8>>) {
        let p = p.as_ref();
        if let Some(parent) = p.parent() {
            self.mkdir_p(parent);
        }
        let modified = self.tick();
        self.nodes.lock().unwrap().insert(
            p.to_path_buf(),
            Node::File {
                bytes: bytes.into(),
                modified,
            },
        );
    }

    /// Create a directory and its parents.
    pub fn mkdir_p(&self, p: impl AsRef<Path>) {
        let mut nodes = self.nodes.lock().unwrap();
        for a in p.as_ref().ancestors() {
            if a.parent().is_some() {
                nodes.entry(a.to_path_buf()).or_insert(Node::Dir);
            }
        }
    }

    fn tick(&self) -> SystemTime {
        let mut t = self.ticks.lock().unwrap();
        *t += 1;
        SystemTime::UNIX_EPOCH + Duration::from_nanos(*t)
    }
}

fn not_found(p: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("{}: not found", p.display()),
    )
}

fn is_root(p: &Path) -> bool {
    p.parent().is_none()
}

impl Fs for MemFs {
    fn metadata(&self, p: &Path) -> io::Result<Meta> {
        if is_root(p) {
            return Ok(Meta {
                kind: Kind::Dir,
                len: 0,
                modified: None,
            });
        }
        match self.nodes.lock().unwrap().get(p) {
            Some(Node::File { bytes, modified }) => Ok(Meta {
                kind: Kind::File,
                len: bytes.len() as u64,
                modified: Some(*modified),
            }),
            Some(Node::Dir) => Ok(Meta {
                kind: Kind::Dir,
                len: 0,
                modified: None,
            }),
            None => Err(not_found(p)),
        }
    }
    fn read(&self, p: &Path) -> io::Result<Vec<u8>> {
        match self.nodes.lock().unwrap().get(p) {
            Some(Node::File { bytes, .. }) => Ok(bytes.clone()),
            Some(Node::Dir) => Err(io::Error::new(
                io::ErrorKind::IsADirectory,
                format!("{} is a directory", p.display()),
            )),
            None => Err(not_found(p)),
        }
    }
    fn open(&self, p: &Path) -> io::Result<Box<dyn io::Read + Send>> {
        Ok(Box::new(io::Cursor::new(self.read(p)?)))
    }
    fn read_dir(&self, p: &Path) -> io::Result<Vec<Entry>> {
        if !self.metadata(p)?.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("{} is not a directory", p.display()),
            ));
        }
        let nodes = self.nodes.lock().unwrap();
        Ok(nodes
            .iter()
            .filter(|(k, _)| k.parent() == Some(p))
            .filter_map(|(k, n)| {
                Some(Entry {
                    name: k.file_name()?.to_str()?.to_string(),
                    is_dir: matches!(n, Node::Dir),
                })
            })
            .collect())
    }
}

#[cfg(test)]
mod fs_tests;
