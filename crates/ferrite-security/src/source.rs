//! Where exchange sets are read from. The verifier reads catalogues and
//! resources through an [`ExchangeSource`]: [`LocalFiles`] is the file
//! system exactly as before (the path-based functions use it), and any host
//! can provide bytes instead (for example the engine's data source, or
//! [`MemoryExchange`]). Paths stay the identity of resources.
use std::{
    collections::BTreeMap,
    io::{self, Read},
    path::{Component, Path, PathBuf},
    sync::Arc,
};

/// Read-only storage of exchange sets.
pub trait ExchangeSource: Send + Sync {
    /// Canonical identity of `path` (links and `.`/`..` resolved); fails
    /// when it does not exist.
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf>;
    /// Whether `path` exists (file or directory, following links).
    fn exists(&self, path: &Path) -> bool;
    /// Whether `path` is a regular file, following links.
    fn is_file(&self, path: &Path) -> bool;
    /// Byte length of the file at `path`.
    fn len(&self, path: &Path) -> io::Result<u64>;
    /// Stream the bytes of the file at `path`.
    fn open(&self, path: &Path) -> io::Result<Box<dyn Read + '_>>;
    /// Whether authenticated snapshots are private temporary files (native
    /// readers open them by path) rather than owned bytes.
    fn file_snapshots(&self) -> bool {
        false
    }
}

/// The local file system (`std::fs`), the behaviour of the path-based API.
#[derive(Clone, Copy, Debug, Default)]
pub struct LocalFiles;

impl ExchangeSource for LocalFiles {
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        path.canonicalize()
    }
    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }
    fn is_file(&self, path: &Path) -> bool {
        path.is_file()
    }
    fn len(&self, path: &Path) -> io::Result<u64> {
        Ok(std::fs::metadata(path)?.len())
    }
    fn open(&self, path: &Path) -> io::Result<Box<dyn Read + '_>> {
        Ok(Box::new(std::fs::File::open(path)?))
    }
    fn file_snapshots(&self) -> bool {
        true
    }
}

pub(crate) fn local_files() -> Arc<dyn ExchangeSource> {
    Arc::new(LocalFiles)
}

/// Retained handle of the source a resource was authenticated from.
#[derive(Clone)]
pub(crate) struct SourceHandle(pub(crate) Arc<dyn ExchangeSource>);
impl std::fmt::Debug for SourceHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ExchangeSource")
    }
}

/// Files held in memory under absolute or relative paths; directories are
/// implied by the files. There are no links: canonical paths are lexical.
#[derive(Clone, Debug, Default)]
pub struct MemoryExchange {
    files: BTreeMap<PathBuf, Arc<[u8]>>,
}

fn normalize(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    Some(out)
}

fn not_found(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("{} is not in the memory exchange", path.display()),
    )
}

impl MemoryExchange {
    pub fn new() -> Self {
        Self::default()
    }
    /// Add or replace a file.
    pub fn insert(&mut self, path: impl Into<PathBuf>, bytes: impl Into<Arc<[u8]>>) {
        let path = path.into();
        let path = normalize(&path).unwrap_or(path);
        self.files.insert(path, bytes.into());
    }
    fn file(&self, path: &Path) -> Option<&Arc<[u8]>> {
        self.files.get(&normalize(path)?)
    }
    fn is_dir(&self, path: &Path) -> bool {
        let Some(path) = normalize(path) else {
            return false;
        };
        self.files
            .range(path.clone()..)
            .next()
            .is_some_and(|(p, _)| p != &path && p.starts_with(&path))
    }
}

impl ExchangeSource for MemoryExchange {
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        let normal = normalize(path).ok_or_else(|| not_found(path))?;
        if self.files.contains_key(&normal) || self.is_dir(&normal) {
            Ok(normal)
        } else {
            Err(not_found(path))
        }
    }
    fn exists(&self, path: &Path) -> bool {
        self.file(path).is_some() || self.is_dir(path)
    }
    fn is_file(&self, path: &Path) -> bool {
        self.file(path).is_some()
    }
    fn len(&self, path: &Path) -> io::Result<u64> {
        self.file(path)
            .map(|b| b.len() as u64)
            .ok_or_else(|| not_found(path))
    }
    fn open(&self, path: &Path) -> io::Result<Box<dyn Read + '_>> {
        let bytes = self.file(path).ok_or_else(|| not_found(path))?;
        Ok(Box::new(&bytes[..]))
    }
}
