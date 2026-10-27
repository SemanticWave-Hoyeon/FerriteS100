//! Platform services injected into the engine (`docs/ENGINE_DESIGN.md` 4a).
//!
//! The kernel reads datasets and catalogues through a [`DataSource`], keeps
//! its portrayal cell cache in a [`CacheStore`], persists per-user state in a
//! [`UserStore`], reads the time from a [`Clock`] and runs parallel work on an
//! [`Executor`]. [`Platform::native`] is the behaviour of the desktop viewer
//! (files, the private cache directory, application data folders, the system
//! clock and threads); [`Platform::in_memory`] needs no file system at all.
//!
//! Paths stay the identity of inputs everywhere (cell source paths, cache
//! keys, catalogue roots): a memory source simply uses them as keys.
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

// ─────────────────────────────────────────────────────────────────────────
// Data source
// ─────────────────────────────────────────────────────────────────────────

/// Data-source interface, defined with the product interface (product
/// modules read their inputs through it).
pub use ferrite_product_api::platform::{DataSource, EntryKind, SourceEntry};

/// Stylesheet modules (S-124/S-129 XSLT rules) read through a [`DataSource`].
pub struct SourceModules<'a>(pub &'a dyn DataSource);
impl ferrite_xslt::ModuleSource for SourceModules<'_> {
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        self.0.canonicalize(path)
    }
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.0.read(path)
    }
    fn is_file(&self, path: &Path) -> bool {
        self.0.is_file(path)
    }
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

/// Cache-store interface, defined with the product interface (product
/// modules keep derived data in it).
pub use ferrite_product_api::platform::{CacheEntry, CacheStore};

/// The application's private cache directory
/// ([`crate::secure_instruction_cache`]): owner and permission checks, no
/// links, atomic replacement and least-recently-used pruning.
#[derive(Clone, Copy, Debug, Default)]
pub struct NativeCacheStore;
impl CacheStore for NativeCacheStore {
    fn root(&self) -> Option<PathBuf> {
        crate::secure_instruction_cache::root()
    }
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        crate::secure_instruction_cache::read(path)
    }
    fn modified(&self, path: &Path) -> Option<SystemTime> {
        std::fs::metadata(path).ok()?.modified().ok()
    }
    fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        crate::secure_instruction_cache::write_atomic(path, bytes)
    }
    fn mark_used(&self, path: &Path) {
        crate::secure_instruction_cache::mark_used(path)
    }
    fn prune(&self) {
        crate::secure_instruction_cache::prune_root()
    }
}

/// Entries in a host-chosen directory, e.g. precomputed portrayal payloads
/// shipped with an application ([`crate::portrayal::PrecomputedCells`]):
/// regular files only (links are rejected), atomic replacement, the native
/// per-entry byte budget, no pruning. Payload hashes detect corruption; the
/// directory is not access-checked like the private native cache.
#[derive(Clone, Debug)]
pub struct DirectoryCacheStore {
    root: PathBuf,
}
impl DirectoryCacheStore {
    /// Use (and create) `root`.
    pub fn new(root: impl Into<PathBuf>) -> io::Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        Ok(Self {
            root: std::fs::canonicalize(&root)?,
        })
    }
}
impl CacheStore for DirectoryCacheStore {
    fn root(&self) -> Option<PathBuf> {
        Some(self.root.clone())
    }
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        use std::io::Read;
        if !std::fs::symlink_metadata(path)?.file_type().is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Cache entry is not a regular file",
            ));
        }
        let limit = crate::secure_instruction_cache::MAX_CACHE_BYTES;
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take(limit + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Cache exceeds byte budget",
            ));
        }
        Ok(bytes)
    }
    fn modified(&self, path: &Path) -> Option<SystemTime> {
        std::fs::symlink_metadata(path).ok()?.modified().ok()
    }
    fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        if bytes.len() as u64 > crate::secure_instruction_cache::MAX_CACHE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Cache exceeds byte budget",
            ));
        }
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Entry has no parent"))?;
        std::fs::create_dir_all(parent)?;
        let name = path
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Entry has no name"))?;
        let mut temporary = name.to_os_string();
        temporary.push(format!(".{}.tmp", std::process::id()));
        let temporary = parent.join(temporary);
        let result =
            std::fs::write(&temporary, bytes).and_then(|()| std::fs::rename(&temporary, path));
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result
    }
    fn mark_used(&self, _path: &Path) {}
    fn prune(&self) {}
}

/// No cache: every portrayal runs the rules.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoCacheStore;
impl CacheStore for NoCacheStore {
    fn root(&self) -> Option<PathBuf> {
        None
    }
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        Err(not_found(path))
    }
    fn modified(&self, _path: &Path) -> Option<SystemTime> {
        None
    }
    fn write(&self, _path: &Path, _bytes: &[u8]) -> io::Result<()> {
        Ok(())
    }
    fn mark_used(&self, _path: &Path) {}
    fn prune(&self) {}
}

/// A cache held in memory (same byte budget per entry as the native one;
/// the whole store is bounded by [`MemoryCacheStore::with_budget`]).
/// Memory cache entries: use order and bytes.
#[derive(Debug)]
struct MemoryCacheEntry {
    /// Write or last use order (`modified`, pruning).
    tick: u64,
    bytes: Arc<[u8]>,
    /// Unique per write; a check is remembered only for the write it read.
    version: u64,
    /// These bytes passed the portrayal cache integrity check.
    verified: bool,
}
type MemoryCacheEntries = HashMap<PathBuf, MemoryCacheEntry>;

#[derive(Debug)]
pub struct MemoryCacheStore {
    entries: Mutex<MemoryCacheEntries>,
    tick: std::sync::atomic::AtomicU64,
    budget: u64,
}
impl Default for MemoryCacheStore {
    fn default() -> Self {
        Self::with_budget(256 * 1024 * 1024)
    }
}
impl MemoryCacheStore {
    pub fn with_budget(budget: u64) -> Self {
        Self {
            entries: Mutex::default(),
            tick: Default::default(),
            budget,
        }
    }
    pub fn len(&self) -> usize {
        self.entries.lock().map_or(0, |e| e.len())
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Store an entry whose integrity the caller already checked
    /// ([`CacheStore::read_entry`] then reports it verified), e.g. bytes a
    /// host verified with its own SHA-256 against the entry's stored hash.
    pub fn write_verified(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.insert(path, bytes, true)
    }
    fn insert(&self, path: &Path, bytes: &[u8], verified: bool) -> io::Result<()> {
        if bytes.len() as u64 > crate::secure_instruction_cache::MAX_CACHE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Cache exceeds byte budget",
            ));
        }
        let tick = self.next();
        self.entries
            .lock()
            .map_err(|_| io::Error::other("Memory cache poisoned"))?
            .insert(
                path.to_path_buf(),
                MemoryCacheEntry {
                    tick,
                    bytes: Arc::from(bytes),
                    version: tick,
                    verified,
                },
            );
        Ok(())
    }
    fn next(&self) -> u64 {
        self.tick
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .wrapping_add(1)
    }
}
impl CacheStore for MemoryCacheStore {
    fn root(&self) -> Option<PathBuf> {
        Some(PathBuf::from("/memory-cache/portrayal-v1"))
    }
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        let entries = self
            .entries
            .lock()
            .map_err(|_| io::Error::other("Memory cache poisoned"))?;
        entries
            .get(path)
            .map(|entry| entry.bytes.to_vec())
            .ok_or_else(|| not_found(path))
    }
    /// Entries carry their write/use order as a time after the epoch.
    fn modified(&self, path: &Path) -> Option<SystemTime> {
        let entries = self.entries.lock().ok()?;
        let tick = entries.get(path)?.tick;
        Some(std::time::UNIX_EPOCH + std::time::Duration::from_nanos(tick))
    }
    fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.insert(path, bytes, false)
    }
    fn mark_used(&self, path: &Path) {
        let tick = self.next();
        if let Ok(mut entries) = self.entries.lock() {
            if let Some(entry) = entries.get_mut(path) {
                entry.tick = tick;
            }
        }
    }
    fn read_entry(&self, path: &Path) -> io::Result<CacheEntry> {
        let entries = self
            .entries
            .lock()
            .map_err(|_| io::Error::other("Memory cache poisoned"))?;
        let entry = entries.get(path).ok_or_else(|| not_found(path))?;
        Ok(CacheEntry {
            bytes: entry.bytes.to_vec(),
            verified: entry.verified,
            version: Some(entry.version),
        })
    }
    fn mark_verified(&self, path: &Path, version: u64) {
        if let Ok(mut entries) = self.entries.lock() {
            if let Some(entry) = entries.get_mut(path).filter(|e| e.version == version) {
                entry.verified = true;
            }
        }
    }
    fn prune(&self) {
        let Ok(mut entries) = self.entries.lock() else {
            return;
        };
        let mut total: u64 = entries.values().map(|e| e.bytes.len() as u64).sum();
        let mut order: Vec<(u64, PathBuf)> =
            entries.iter().map(|(p, e)| (e.tick, p.clone())).collect();
        order.sort();
        for (_, path) in order {
            if total <= self.budget {
                break;
            }
            if let Some(entry) = entries.remove(&path) {
                total = total.saturating_sub(entry.bytes.len() as u64);
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// User store
// ─────────────────────────────────────────────────────────────────────────

/// One persisted per-user record and its lock guard (defined with the
/// product interface, which product modules persist through).
pub use ferrite_product_api::platform::{RecordLock, UserRecord};

/// Per-user persistent state of the engine.
pub trait UserStore: Send + Sync {
    /// The S-124 user state record of an OS user (None: not persisted).
    fn navwarn_state(&self, user: &str) -> Option<Box<dyn UserRecord>>;
    /// The transactional S-101 cancellation history record
    /// ([`crate::chart_publication::HistoryStore`]). None: cancellations are
    /// not supported by this host.
    fn cancellation_history(&self) -> Option<Box<dyn UserRecord>>;
    /// Native location of the cancellation history file, for hosts that
    /// address it directly (the viewer); None for stores without files.
    fn cancellation_history_path(&self) -> Option<PathBuf>;
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

/// Location of a per-user file for an OS user (None: not persisted).
pub type UserPathFn = Arc<dyn Fn(&str) -> Option<PathBuf> + Send + Sync>;

/// Native user data locations, chosen by the host (the viewer passes its
/// application data paths, which differ under its hidden test modes).
#[derive(Clone)]
pub struct NativeUserStore {
    pub history_path: PathBuf,
    pub navwarn_path: UserPathFn,
}
impl NativeUserStore {
    /// The standard per-user application data folder
    /// (`FerriteS100/s101-cancellations.json`, `FerriteS100/navwarn/<user>.json`).
    pub fn standard() -> Self {
        Self {
            history_path: standard_data_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("FerriteS100/s101-cancellations.json"),
            navwarn_path: Arc::new(|user| {
                Some(
                    standard_data_dir()?
                        .join("FerriteS100/navwarn")
                        .join(format!("{}.json", user_file_name(user))),
                )
            }),
        }
    }
}
impl UserStore for NativeUserStore {
    fn navwarn_state(&self, user: &str) -> Option<Box<dyn UserRecord>> {
        (self.navwarn_path)(user).map(|path| {
            Box::new(FileRecord {
                path,
                temporary_prefix: ".navwarn-state",
            }) as Box<dyn UserRecord>
        })
    }
    fn cancellation_history(&self) -> Option<Box<dyn UserRecord>> {
        Some(Box::new(crate::chart_publication::HistoryFile::new(
            self.history_path.clone(),
        )))
    }
    fn cancellation_history_path(&self) -> Option<PathBuf> {
        Some(self.history_path.clone())
    }
}

/// The OS per-user data directory.
pub fn standard_data_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(target_os = "macos")]
    let base =
        std::env::var_os("HOME").map(|p| PathBuf::from(p).join("Library/Application Support"));
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/share")));
    base
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

/// User records kept in memory (lost when the process ends).
#[derive(Clone, Default)]
pub struct MemoryUserStore {
    records: Arc<Mutex<HashMap<String, Vec<u8>>>>,
}
/// Key of the S-101 cancellation history in a [`MemoryUserStore`].
pub const MEMORY_HISTORY_KEY: &str = "s101-cancellations";

impl MemoryUserStore {
    /// The stored bytes of a record (`navwarn/<user>`,
    /// [`MEMORY_HISTORY_KEY`]), for inspection.
    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        self.records.lock().ok()?.get(key).cloned()
    }
}
struct MemoryRecord {
    key: String,
    records: Arc<Mutex<HashMap<String, Vec<u8>>>>,
}
impl UserRecord for MemoryRecord {
    fn read(&self, limit: u64) -> io::Result<Option<Vec<u8>>> {
        let records = self
            .records
            .lock()
            .map_err(|_| io::Error::other("Memory user store poisoned"))?;
        Ok(records.get(&self.key).map(|bytes| {
            let end = usize::try_from(limit.saturating_add(1))
                .unwrap_or(usize::MAX)
                .min(bytes.len());
            bytes[..end].to_vec()
        }))
    }
    fn write(&self, bytes: &[u8]) -> io::Result<()> {
        self.records
            .lock()
            .map_err(|_| io::Error::other("Memory user store poisoned"))?
            .insert(self.key.clone(), bytes.to_vec());
        Ok(())
    }
    fn set_aside(&self, suffix: &str) {
        if let Ok(mut records) = self.records.lock() {
            if let Some(bytes) = records.remove(&self.key) {
                records.insert(format!("{}{suffix}", self.key), bytes);
            }
        }
    }
    fn describe(&self) -> String {
        format!("memory:{}", self.key)
    }
}
impl UserStore for MemoryUserStore {
    fn navwarn_state(&self, user: &str) -> Option<Box<dyn UserRecord>> {
        Some(Box::new(MemoryRecord {
            key: format!("navwarn/{}", user_file_name(user)),
            records: Arc::clone(&self.records),
        }))
    }
    fn cancellation_history(&self) -> Option<Box<dyn UserRecord>> {
        Some(Box::new(MemoryRecord {
            key: MEMORY_HISTORY_KEY.into(),
            records: Arc::clone(&self.records),
        }))
    }
    fn cancellation_history_path(&self) -> Option<PathBuf> {
        None
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Clock
// ─────────────────────────────────────────────────────────────────────────

/// Wall-clock time (dataset lifecycles, receipts, signature validity),
/// defined with the product interface.
pub use ferrite_product_api::platform::Clock;

/// The system clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;
impl Clock for SystemClock {
    #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
    /// wasm32-unknown-unknown has no std wall clock (`SystemTime::now`
    /// panics); chrono reads the JavaScript `Date` there.
    #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
    fn now(&self) -> SystemTime {
        let millis = chrono::Utc::now().timestamp_millis().max(0) as u64;
        std::time::UNIX_EPOCH + std::time::Duration::from_millis(millis)
    }
    fn utc(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc::now()
    }
}

/// A clock standing still at a given instant (tests, replays).
#[derive(Clone, Copy, Debug)]
pub struct FixedClock(pub SystemTime);
impl Clock for FixedClock {
    fn now(&self) -> SystemTime {
        self.0
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

/// Native threads: up to the available parallelism (or one thread per job
/// when fewer), background jobs on their own thread (`parallel` feature).
#[cfg(feature = "parallel")]
#[derive(Clone, Copy, Debug, Default)]
pub struct ThreadExecutor;
#[cfg(feature = "parallel")]
impl Executor for ThreadExecutor {
    fn for_each_index(&self, count: usize, job: &(dyn Fn(usize) + Sync)) {
        if count == 0 {
            return;
        }
        let workers = self.parallelism().min(count).max(1);
        let next = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..workers {
                let next = &next;
                scope.spawn(move || loop {
                    let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if index >= count {
                        break;
                    }
                    job(index);
                });
            }
        });
    }
    fn spawn(&self, job: Box<dyn FnOnce() + Send + 'static>) {
        std::thread::spawn(job);
    }
    fn parallelism(&self) -> usize {
        std::thread::available_parallelism().map_or(1, |n| n.get())
    }
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

/// The executor of [`Platform::native`]: threads with the `parallel`
/// feature, otherwise sequential.
pub fn default_executor() -> Arc<dyn Executor> {
    #[cfg(feature = "parallel")]
    {
        Arc::new(ThreadExecutor)
    }
    #[cfg(not(feature = "parallel"))]
    {
        Arc::new(SequentialExecutor)
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
// Platform
// ─────────────────────────────────────────────────────────────────────────

/// The services an engine runs on.
#[derive(Clone)]
pub struct Platform {
    pub data: Arc<dyn DataSource>,
    pub cache: Arc<dyn CacheStore>,
    pub user: Arc<dyn UserStore>,
    pub clock: Arc<dyn Clock>,
    pub executor: Arc<dyn Executor>,
}

impl Platform {
    /// The desktop viewer's behaviour: files, the private portrayal cache
    /// directory, the standard application data folders, the system clock
    /// and threads.
    pub fn native() -> Self {
        Self::native_with_user_store(NativeUserStore::standard())
    }
    /// [`Platform::native`] with host-chosen user data locations.
    pub fn native_with_user_store(user: NativeUserStore) -> Self {
        Self {
            data: Arc::new(NativeDataSource),
            cache: Arc::new(NativeCacheStore),
            user: Arc::new(user),
            clock: Arc::new(SystemClock),
            executor: default_executor(),
        }
    }
    /// No file system: inputs from `data`, a memory cache and user store, the
    /// system clock and sequential execution.
    pub fn in_memory(data: MemoryDataSource) -> Self {
        Self {
            data: Arc::new(data),
            cache: Arc::new(MemoryCacheStore::default()),
            user: Arc::new(MemoryUserStore::default()),
            clock: Arc::new(SystemClock),
            executor: Arc::new(SequentialExecutor),
        }
    }
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }
    pub fn with_executor(mut self, executor: Arc<dyn Executor>) -> Self {
        self.executor = executor;
        self
    }
    pub fn with_data(mut self, data: Arc<dyn DataSource>) -> Self {
        self.data = data;
        self
    }
    pub fn with_cache(mut self, cache: Arc<dyn CacheStore>) -> Self {
        self.cache = cache;
        self
    }
}

impl Default for Platform {
    fn default() -> Self {
        Self::native()
    }
}

impl std::fmt::Debug for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Platform")
            .field("local_paths", &self.data.local_paths())
            .field("parallelism", &self.executor.parallelism())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_source_lists_reads_and_canonicalizes() {
        let data = MemoryDataSource::new()
            .with("/set/a/ENC.000", b"one".to_vec())
            .with("/set/b/x.gml", b"two".to_vec());
        assert_eq!(data.kind(Path::new("/set")).unwrap(), EntryKind::Dir);
        assert!(data.is_file(Path::new("/set/a/../a/ENC.000")));
        assert!(data.kind(Path::new("/missing")).is_err());
        let walked: Vec<_> = data
            .walk(Path::new("/set"))
            .map(|e| e.unwrap().path)
            .collect();
        assert_eq!(
            walked,
            ["/set", "/set/a", "/set/a/ENC.000", "/set/b", "/set/b/x.gml"]
                .map(PathBuf::from)
                .to_vec()
        );
        let children: Vec<_> = data
            .read_dir(Path::new("/set"))
            .unwrap()
            .into_iter()
            .map(|e| (e.path, e.kind))
            .collect();
        assert_eq!(
            children,
            vec![
                (PathBuf::from("/set/a"), EntryKind::Dir),
                (PathBuf::from("/set/b"), EntryKind::Dir)
            ]
        );
        assert_eq!(
            data.read_limited(Path::new("/set/b/x.gml"), 1).unwrap(),
            b"tw"
        );
        assert_eq!(
            data.canonicalize(Path::new("/set/b/./x.gml")).unwrap(),
            PathBuf::from("/set/b/x.gml")
        );
    }

    #[test]
    fn memory_paths_are_platform_absolute_keys() {
        let key = memory_path("/xs/S100_ROOT/CATALOG.XML");
        assert!(key.is_absolute(), "{}", key.display());
        assert!(key.ends_with("xs/S100_ROOT/CATALOG.XML"));
        assert_eq!(memory_path(&key), key);
        assert_eq!(memory_path("relative/x"), PathBuf::from("relative/x"));
        if cfg!(windows) {
            assert_eq!(key, PathBuf::from(r"C:\xs\S100_ROOT\CATALOG.XML"));
        } else {
            assert_eq!(key, PathBuf::from("/xs/S100_ROOT/CATALOG.XML"));
        }
        // Memory keys normalize the same either way.
        let data = MemoryDataSource::new().with(key.clone(), b"x".to_vec());
        assert_eq!(data.canonicalize(&key).unwrap(), key);
        assert!(data.is_dir(&memory_path("/xs")));
    }

    #[test]
    fn executors_map_in_order() {
        let items: Vec<usize> = (0..37).collect();
        for executor in [
            default_executor(),
            Arc::new(SequentialExecutor) as Arc<dyn Executor>,
        ] {
            assert_eq!(
                parallel_map(executor.as_ref(), &items, |i| i * 2),
                items.iter().map(|i| i * 2).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn memory_stores_round_trip_and_prune() {
        let cache = MemoryCacheStore::with_budget(4);
        let root = cache.root().unwrap();
        cache.write(&root.join("a"), b"123").unwrap();
        cache.write(&root.join("b"), b"456").unwrap();
        cache.mark_used(&root.join("a"));
        cache.prune();
        assert!(cache.read(&root.join("b")).is_err());
        assert_eq!(cache.read(&root.join("a")).unwrap(), b"123");

        let user = MemoryUserStore::default();
        let record = user.navwarn_state("a b").unwrap();
        assert_eq!(record.read(10).unwrap(), None);
        record.write(b"state").unwrap();
        assert_eq!(record.read(10).unwrap().unwrap(), b"state");
        assert_eq!(user.get("navwarn/a_b").unwrap(), b"state");
        assert!(user.cancellation_history_path().is_none());
    }

    #[test]
    fn fixed_clock_reports_its_instant() {
        let at = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let clock = FixedClock(at);
        assert_eq!(clock.unix_seconds(), 1_700_000_000);
        assert_eq!(clock.utc().timestamp(), 1_700_000_000);
    }
}
