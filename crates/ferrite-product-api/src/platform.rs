//! Platform services product modules use directly. The engine's `Platform`
//! (`ferrite_engine::platform`) provides the implementations: the system
//! clock, files in the application data folder or records in memory, the
//! data source datasets and catalogues are read through (local files or
//! memory) and the disposable cache store. The local file system
//! ([`NativeDataSource`]), files in memory ([`MemoryDataSource`]) and the
//! sequential [`Executor`] are defined here so product layers and their
//! tests can use them without the engine; [`Services`] bundles the shared
//! handles a product layer keeps.
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
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

/// Whether `path` names a dataset file with `extension` (any case), not an
/// AppleDouble metadata companion (`._name`).
pub fn is_dataset_file(path: &Path, extension: &str) -> bool {
    !path
        .file_name()
        .is_some_and(|name| name.as_encoded_bytes().starts_with(b"._"))
        && path
            .extension()
            .is_some_and(|value| value.eq_ignore_ascii_case(extension))
}

/// The local file system: `std::fs` and `walkdir`, exactly as the viewer
/// always read its inputs.
#[derive(Clone, Copy, Debug, Default)]
pub struct NativeDataSource;

fn entry_kind(kind: std::fs::FileType) -> EntryKind {
    if kind.is_symlink() {
        EntryKind::Symlink
    } else if kind.is_dir() {
        EntryKind::Dir
    } else if kind.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    }
}

impl DataSource for NativeDataSource {
    fn kind(&self, path: &Path) -> io::Result<EntryKind> {
        Ok(entry_kind(std::fs::symlink_metadata(path)?.file_type()))
    }
    fn is_file(&self, path: &Path) -> bool {
        path.is_file()
    }
    fn is_dir(&self, path: &Path) -> bool {
        path.is_dir()
    }
    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }
    fn len(&self, path: &Path) -> io::Result<u64> {
        Ok(std::fs::metadata(path)?.len())
    }
    fn modified(&self, path: &Path) -> Option<SystemTime> {
        std::fs::metadata(path).ok()?.modified().ok()
    }
    fn read_dir(&self, path: &Path) -> io::Result<Vec<SourceEntry>> {
        std::fs::read_dir(path)?
            .map(|entry| {
                let entry = entry?;
                Ok(SourceEntry {
                    path: entry.path(),
                    kind: entry_kind(entry.file_type()?),
                })
            })
            .collect()
    }
    fn walk<'a>(&'a self, root: &Path) -> Box<dyn Iterator<Item = io::Result<SourceEntry>> + 'a> {
        Box::new(
            walkdir::WalkDir::new(root)
                .follow_links(false)
                .into_iter()
                .map(|entry| {
                    // Keep walkdir's message (it names the path).
                    let entry = entry.map_err(|error| {
                        let kind = error
                            .io_error()
                            .map_or(io::ErrorKind::Other, io::Error::kind);
                        io::Error::new(kind, error)
                    })?;
                    let kind = entry_kind(entry.file_type());
                    Ok(SourceEntry {
                        path: entry.into_path(),
                        kind,
                    })
                }),
        )
    }
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }
    fn read_limited(&self, path: &Path, limit: u64) -> io::Result<Vec<u8>> {
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take(limit.saturating_add(1))
            .read_to_end(&mut bytes)?;
        Ok(bytes)
    }
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        path.canonicalize()
    }
    fn local_paths(&self) -> bool {
        true
    }
    /// The native snapshot: symlink root rejected, bounded parallel reads.
    fn portrayal_sources(
        &self,
        root: &Path,
    ) -> ferrite_portrayal_catalog::Result<Arc<ferrite_portrayal_catalog::CatalogueSources>> {
        ferrite_portrayal_catalog::CatalogueSources::capture(root)
    }
}

/// Files held in memory, keyed by their (absolute or relative) paths.
/// Directories are implied by the files they hold.
#[derive(Clone, Debug, Default)]
pub struct MemoryDataSource {
    files: BTreeMap<PathBuf, Arc<[u8]>>,
}

/// Lexically normalized path (`.` removed, `..` applied); None when `..`
/// would leave the root.
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

/// A platform-absolute in-memory key for a root-relative virtual path such
/// as `/xs/CATALOG.XML`. In-memory paths that act as dataset, catalogue or
/// authorization keys must be absolute ([`Path::is_absolute`]); `/x` is
/// absolute on Unix-style platforms (WebAssembly included) and is returned
/// unchanged there, while on Windows it has no drive, so it is placed on the
/// virtual drive `C:` (`C:/xs/CATALOG.XML`; forward slashes are separators
/// there). Other paths (already absolute, or relative) are unchanged.
pub fn memory_path(path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    if cfg!(windows) && path.has_root() && !path.is_absolute() {
        let mut keyed = std::ffi::OsString::from("C:");
        keyed.push(path.as_os_str());
        return PathBuf::from(keyed);
    }
    path.to_path_buf()
}

fn not_found(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("{} is not in the memory source", path.display()),
    )
}

impl MemoryDataSource {
    pub fn new() -> Self {
        Self::default()
    }
    /// Add (or replace) a file.
    pub fn insert(&mut self, path: impl Into<PathBuf>, bytes: impl Into<Arc<[u8]>>) {
        let path = path.into();
        let path = normalize(&path).unwrap_or(path);
        self.files.insert(path, bytes.into());
    }
    /// Builder form of [`MemoryDataSource::insert`].
    pub fn with(mut self, path: impl Into<PathBuf>, bytes: impl Into<Arc<[u8]>>) -> Self {
        self.insert(path, bytes);
        self
    }
    /// Copy every regular file under `from` of another source (e.g. a test
    /// fixture folder read once from disk) into memory under `to`.
    pub fn copy_tree(
        &mut self,
        source: &dyn DataSource,
        from: &Path,
        to: &Path,
    ) -> io::Result<usize> {
        let mut count = 0;
        for entry in source.walk(from) {
            let entry = entry?;
            if entry.kind == EntryKind::File {
                let bytes = source.read(&entry.path)?;
                let relative = entry.path.strip_prefix(from).map_err(io::Error::other)?;
                self.insert(to.join(relative), bytes);
                count += 1;
            }
        }
        Ok(count)
    }
    pub fn len(&self) -> usize {
        self.files.len()
    }
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
    fn file(&self, path: &Path) -> Option<&Arc<[u8]>> {
        self.files.get(&normalize(path)?)
    }
    fn has_dir(&self, path: &Path) -> bool {
        let Some(path) = normalize(path) else {
            return false;
        };
        self.files
            .range(path.clone()..)
            .next()
            .is_some_and(|(p, _)| p != &path && p.starts_with(&path))
            || (path.as_os_str().is_empty() && !self.files.is_empty())
    }
    fn entries_under(&self, root: &Path) -> Vec<SourceEntry> {
        let mut dirs = BTreeSet::new();
        let mut files = Vec::new();
        for path in self
            .files
            .keys()
            .filter(|p| p.starts_with(root) && *p != root)
        {
            files.push(path.clone());
            let mut parent = path.parent();
            while let Some(dir) = parent.filter(|d| d.starts_with(root) && *d != root) {
                dirs.insert(dir.to_path_buf());
                parent = dir.parent();
            }
        }
        let mut entries: Vec<SourceEntry> = dirs
            .into_iter()
            .map(|path| SourceEntry {
                path,
                kind: EntryKind::Dir,
            })
            .chain(files.into_iter().map(|path| SourceEntry {
                path,
                kind: EntryKind::File,
            }))
            .collect();
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        entries
    }
}

impl DataSource for MemoryDataSource {
    fn kind(&self, path: &Path) -> io::Result<EntryKind> {
        if self.file(path).is_some() {
            Ok(EntryKind::File)
        } else if self.has_dir(path) {
            Ok(EntryKind::Dir)
        } else {
            Err(not_found(path))
        }
    }
    fn is_file(&self, path: &Path) -> bool {
        self.file(path).is_some()
    }
    fn is_dir(&self, path: &Path) -> bool {
        self.has_dir(path)
    }
    fn len(&self, path: &Path) -> io::Result<u64> {
        self.file(path)
            .map(|b| b.len() as u64)
            .ok_or_else(|| not_found(path))
    }
    fn modified(&self, _path: &Path) -> Option<SystemTime> {
        None
    }
    fn read_dir(&self, path: &Path) -> io::Result<Vec<SourceEntry>> {
        if !self.has_dir(path) {
            return Err(not_found(path));
        }
        let root = normalize(path).ok_or_else(|| not_found(path))?;
        Ok(self
            .entries_under(&root)
            .into_iter()
            .filter(|e| e.path.parent() == Some(root.as_path()))
            .collect())
    }
    fn walk<'a>(&'a self, root: &Path) -> Box<dyn Iterator<Item = io::Result<SourceEntry>> + 'a> {
        let Some(normal) = normalize(root) else {
            return Box::new(std::iter::once(Err(not_found(root))));
        };
        let first = match self.kind(&normal) {
            Ok(kind) => SourceEntry {
                path: normal.clone(),
                kind,
            },
            Err(error) => return Box::new(std::iter::once(Err(error))),
        };
        let rest = if first.kind == EntryKind::Dir {
            self.entries_under(&normal)
        } else {
            Vec::new()
        };
        Box::new(std::iter::once(Ok(first)).chain(rest.into_iter().map(Ok)))
    }
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.file(path)
            .map(|b| b.to_vec())
            .ok_or_else(|| not_found(path))
    }
    fn read_limited(&self, path: &Path, limit: u64) -> io::Result<Vec<u8>> {
        let bytes = self.file(path).ok_or_else(|| not_found(path))?;
        let end = usize::try_from(limit.saturating_add(1))
            .unwrap_or(usize::MAX)
            .min(bytes.len());
        Ok(bytes[..end].to_vec())
    }
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        let normal = normalize(path).ok_or_else(|| not_found(path))?;
        self.kind(&normal)?;
        Ok(normal)
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Cache store
// ─────────────────────────────────────────────────────────────────────────

/// Largest cache entry (bytes) a store reads or a portrayal cache encodes.
pub const MAX_CACHE_BYTES: u64 = 64 * 1024 * 1024;
/// Most drawing instructions one portrayal cache entry holds.
pub const MAX_INSTRUCTIONS: u64 = 100_000;

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
    /// `ferrite_s101::layer::portrayal_cache::verify_and_deserialize_cache`). Only a
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

/// Per-user persistent state of the products, keyed by product: S-124 keeps
/// one record per OS user (key: the user name), S-101 its cancellation
/// history (one key). How each product's records are kept is registered as
/// a [`RecordKind`].
pub trait UserStore: Send + Sync {
    /// Record `key` of `product` (None: not persisted by this host).
    fn product_state(&self, product: crate::ProductId, key: &str) -> Option<Box<dyn UserRecord>>;
    /// Native file of the record, for hosts that address it directly; None
    /// for stores without files.
    fn product_state_path(&self, product: crate::ProductId, key: &str) -> Option<PathBuf>;
}

/// How a product keeps its per-user records.
#[derive(Clone, Copy)]
pub struct RecordKind {
    pub product: crate::ProductId,
    /// The record in a native file at `path`.
    pub native_record: fn(path: PathBuf) -> Box<dyn UserRecord>,
    /// Standard native file of `key` under the per-user application data
    /// folder (None when the OS has none); None: not persisted.
    pub standard_path: fn(data_dir: Option<&Path>, key: &str) -> Option<PathBuf>,
    /// Key of `key` in an in-memory store.
    pub memory_key: fn(key: &str) -> String,
}

impl std::fmt::Debug for RecordKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordKind")
            .field("product", &self.product)
            .finish_non_exhaustive()
    }
}

/// A [`UserRecord`] in a file: private directory, temporary file and rename.
#[derive(Clone, Debug)]
pub struct FileRecord {
    pub path: PathBuf,
    /// File name prefix of the temporary file written beside the record.
    pub temporary_prefix: &'static str,
}
impl UserRecord for FileRecord {
    fn read(&self, limit: u64) -> io::Result<Option<Vec<u8>>> {
        use std::io::Read;
        let file = match std::fs::File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let mut bytes = Vec::new();
        file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
        Ok(Some(bytes))
    }
    fn write(&self, bytes: &[u8]) -> io::Result<()> {
        use std::io::Write;
        let path = &self.path;
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(parent)?;
        let nonce = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos();
        let temporary = parent.join(format!(
            "{}-{}-{nonce}.tmp",
            self.temporary_prefix,
            std::process::id()
        ));
        let result = (|| -> io::Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            // wasm32: the std::fs stub File has no Drop (native-only path).
            #[allow(clippy::drop_non_drop)]
            drop(file);
            std::fs::rename(&temporary, path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result
    }
    fn set_aside(&self, suffix: &str) {
        let mut name = self.path.as_os_str().to_owned();
        name.push(suffix);
        if let Err(error) = std::fs::rename(&self.path, &name) {
            tracing::warn!("Could not set aside {}: {error}", self.path.display());
        }
    }
    fn describe(&self) -> String {
        self.path.display().to_string()
    }
}

/// A user name as a safe file name.
pub fn user_file_name(user: &str) -> String {
    let file: String = user
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let file = file.trim_start_matches('.');
    if file.is_empty() {
        "unknown".into()
    } else {
        file.into()
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Executor
// ─────────────────────────────────────────────────────────────────────────

/// Where parallel and background work runs.
pub trait Executor: Send + Sync {
    /// Run `job(i)` for every `i < count`, possibly concurrently; returns
    /// once all finished. A panicking job propagates its panic.
    fn for_each_index(&self, count: usize, job: &(dyn Fn(usize) + Sync));
    /// Start a background job. A sequential executor runs it before returning.
    fn spawn(&self, job: Box<dyn FnOnce() + Send + 'static>);
    /// How many jobs may run at once.
    fn parallelism(&self) -> usize;
}

/// Everything on the calling thread, in order (hosts without threads).
#[derive(Clone, Copy, Debug, Default)]
pub struct SequentialExecutor;
impl Executor for SequentialExecutor {
    fn for_each_index(&self, count: usize, job: &(dyn Fn(usize) + Sync)) {
        for index in 0..count {
            job(index);
        }
    }
    fn spawn(&self, job: Box<dyn FnOnce() + Send + 'static>) {
        job()
    }
    fn parallelism(&self) -> usize {
        1
    }
}

/// `f` over `items` on the executor; results in item order.
pub fn parallel_map<T: Sync, R: Send>(
    executor: &dyn Executor,
    items: &[T],
    f: impl Fn(&T) -> R + Sync,
) -> Vec<R> {
    let slots: Vec<Mutex<Option<R>>> = items.iter().map(|_| Mutex::new(None)).collect();
    executor.for_each_index(items.len(), &|index| {
        let result = f(&items[index]);
        if let Ok(mut slot) = slots[index].lock() {
            *slot = Some(result);
        }
    });
    slots
        .into_iter()
        .map(|slot| {
            slot.into_inner()
                .ok()
                .flatten()
                .expect("executor ran every job")
        })
        .collect()
}

// ─────────────────────────────────────────────────────────────────────────
// Layer services
// ─────────────────────────────────────────────────────────────────────────

/// The platform services a product layer reads inputs, keeps derived data
/// and runs parallel work through: handles shared with the host's platform,
/// taken when the layer is created.
#[derive(Clone)]
pub struct Services {
    pub data: Arc<dyn DataSource>,
    pub cache: Arc<dyn CacheStore>,
    pub executor: Arc<dyn Executor>,
}

impl std::fmt::Debug for Services {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Services")
            .field("local_paths", &self.data.local_paths())
            .field("parallelism", &self.executor.parallelism())
            .finish_non_exhaustive()
    }
}
