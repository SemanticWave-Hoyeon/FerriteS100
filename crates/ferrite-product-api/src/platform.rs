//! Platform services product modules use directly. The engine's `Platform`
//! (`ferrite_engine::platform`) provides the implementations: the system
//! clock, files in the application data folder or records in memory, the
//! data source datasets and catalogues are read through (local files or
//! memory) and the disposable cache store.
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

// ─────────────────────────────────────────────────────────────────────────
// Data source
// ─────────────────────────────────────────────────────────────────────────

/// Kind of a directory entry, never following a final symbolic link.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    Other,
}

/// An entry produced by [`DataSource::walk`] or [`DataSource::read_dir`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceEntry {
    pub path: PathBuf,
    pub kind: EntryKind,
}

/// Where datasets, exchange sets and catalogues are read from.
pub trait DataSource: Send + Sync {
    /// Kind of `path` without following a final link (`symlink_metadata`).
    fn kind(&self, path: &Path) -> io::Result<EntryKind>;
    /// Whether `path` is a regular file, following links (`Path::is_file`).
    fn is_file(&self, path: &Path) -> bool;
    /// Whether `path` is a directory, following links (`Path::is_dir`).
    fn is_dir(&self, path: &Path) -> bool;
    /// Whether `path` exists, following links (`Path::exists`).
    fn exists(&self, path: &Path) -> bool {
        self.is_file(path) || self.is_dir(path)
    }
    /// Byte length of the file at `path`, following links.
    fn len(&self, path: &Path) -> io::Result<u64>;
    /// Last modification time, when the source records one.
    fn modified(&self, path: &Path) -> Option<SystemTime>;
    /// Children of a directory, in source order (callers sort).
    fn read_dir(&self, path: &Path) -> io::Result<Vec<SourceEntry>>;
    /// Every entry under `root` (itself first), not following links.
    fn walk<'a>(&'a self, root: &Path) -> Box<dyn Iterator<Item = io::Result<SourceEntry>> + 'a>;
    /// Whole contents of a file.
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;
    /// At most `limit + 1` bytes of a file (callers reject the extra byte).
    fn read_limited(&self, path: &Path, limit: u64) -> io::Result<Vec<u8>>;
    /// Absolute path with `.`/`..` and links resolved; fails when missing.
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf>;
    /// Whether paths of this source name real local files that native-only,
    /// path-based readers (HDF5, signature verification, the S-101 update
    /// planner, cancellation history) can open themselves.
    fn local_paths(&self) -> bool {
        false
    }
    /// Capture a portrayal catalogue folder as an immutable snapshot.
    fn portrayal_sources(
        &self,
        root: &Path,
    ) -> ferrite_portrayal_catalog::Result<Arc<ferrite_portrayal_catalog::CatalogueSources>> {
        let io = ferrite_portrayal_catalog::PCError::Io;
        let mut files = Vec::new();
        for entry in self.walk(root) {
            let entry = entry.map_err(io)?;
            match entry.kind {
                EntryKind::File => {
                    let relative = entry
                        .path
                        .strip_prefix(root)
                        .map_err(|_| {
                            ferrite_portrayal_catalog::PCError::InvalidValue(
                                "PC snapshot entry outside root".into(),
                            )
                        })?
                        .to_path_buf();
                    let bytes = self
                        .read_limited(&entry.path, 50 * 1024 * 1024)
                        .map_err(io)?;
                    files.push((relative, Arc::from(bytes)));
                }
                EntryKind::Dir => {}
                EntryKind::Symlink | EntryKind::Other => {
                    return Err(ferrite_portrayal_catalog::PCError::InvalidValue(format!(
                        "PC snapshot rejects link or special file: {}",
                        entry.path.display()
                    )))
                }
            }
        }
        ferrite_portrayal_catalog::CatalogueSources::from_files(root, files)
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Cache store
// ─────────────────────────────────────────────────────────────────────────

/// The disposable portrayal cell cache. Entries are named by paths under
/// [`CacheStore::root`]; the payload hash (not the store) detects corruption.
pub trait CacheStore: Send + Sync {
    /// The cache location, None when no cache is available now.
    fn root(&self) -> Option<PathBuf>;
    /// A cache entry, within the cache byte budget.
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;
    /// When the entry was written or last used; None when it is missing.
    fn modified(&self, path: &Path) -> Option<SystemTime>;
    /// Replace an entry atomically.
    fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()>;
    /// Record a hit for least-recently-used pruning.
    fn mark_used(&self, path: &Path);
    /// Keep the store within its budget; failures only log.
    fn prune(&self);
    /// [`Self::read`], also telling whether these exact bytes already passed
    /// the portrayal cache integrity check (SHA-256,
    /// `ferrite_engine::portrayal_cache::verify_and_deserialize_cache`). Only a
    /// store whose entries cannot change except through [`Self::write`] may
    /// remember that (in-memory stores); the default never does, so every
    /// read is checked again.
    fn read_entry(&self, path: &Path) -> io::Result<CacheEntry> {
        Ok(CacheEntry {
            bytes: self.read(path)?,
            verified: false,
            version: None,
        })
    }
    /// Record that the entry read as `version` ([`CacheEntry::version`])
    /// passed the integrity check; ignored when it was replaced meanwhile.
    fn mark_verified(&self, _path: &Path, _version: u64) {}
}

/// An entry read with [`CacheStore::read_entry`].
#[derive(Debug)]
pub struct CacheEntry {
    pub bytes: Vec<u8>,
    /// These bytes passed the integrity check before.
    pub verified: bool,
    /// Identity of this write of the entry, when the store can remember
    /// checks ([`CacheStore::mark_verified`]).
    pub version: Option<u64>,
}

// ─────────────────────────────────────────────────────────────────────────
// Clock and user records
// ─────────────────────────────────────────────────────────────────────────

/// The time source.
pub trait Clock: Send + Sync {
    fn now(&self) -> SystemTime;
    /// [`Clock::now`] as UTC.
    fn utc(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::<chrono::Utc>::from(self.now())
    }
    /// Seconds since the Unix epoch (negative before it).
    fn unix_seconds(&self) -> i64 {
        match self.now().duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => d.as_secs() as i64,
            Err(e) => -(e.duration().as_secs() as i64),
        }
    }
}

/// One persisted per-user record (e.g. the S-124 NAVWARN user state).
pub trait UserRecord: Send + Sync {
    /// The record, None when it was never written; at most `limit + 1` bytes.
    fn read(&self, limit: u64) -> io::Result<Option<Vec<u8>>>;
    /// Replace the record atomically.
    fn write(&self, bytes: &[u8]) -> io::Result<()>;
    /// Keep an unreadable record for inspection (renamed with `suffix`).
    fn set_aside(&self, suffix: &str);
    /// Where the record lives, for messages.
    fn describe(&self) -> String;
    /// Hold the record exclusively across a read, validate and write
    /// transaction; dropping the guard releases it. None: the record needs no
    /// lock (its single owner serializes access, e.g. memory records).
    fn lock(&self) -> io::Result<Option<RecordLock>> {
        Ok(None)
    }
}

/// Guard of [`UserRecord::lock`] (an OS file lock for native records).
pub type RecordLock = Box<dyn std::any::Any + Send>;
