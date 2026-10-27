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
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
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

/// The local file system and files in memory, defined with the product
/// interface (product modules read through them).
pub use ferrite_product_api::platform::{memory_path, MemoryDataSource, NativeDataSource};

fn not_found(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("{} is not in the memory source", path.display()),
    )
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
/// shipped with an application ([`ferrite_s101::layer::portrayal::PrecomputedCells`]):
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

/// Per-user persistent state of the products (defined with the product
/// interface): records by product and key, kept as the registered
/// [`RecordKind`]s say ([`crate::products::user_record_kinds`]).
pub use ferrite_product_api::platform::{user_file_name, FileRecord, RecordKind, UserStore};
use ferrite_product_api::ProductId;

/// Location of a product's per-user file for a record key (None: not
/// persisted).
pub type UserPathFn = Arc<dyn Fn(&str) -> Option<PathBuf> + Send + Sync>;

/// Native user data locations by product, chosen by the host (the viewer
/// passes its application data paths, which differ under its hidden test
/// modes). Records of products without a location are not persisted.
#[derive(Clone, Default)]
pub struct NativeUserStore {
    pub paths: std::collections::BTreeMap<ProductId, UserPathFn>,
}
impl NativeUserStore {
    /// No product persists anything.
    pub fn new() -> Self {
        Self::default()
    }
    /// Keep `product`'s records at `path(key)`.
    pub fn with(mut self, product: ProductId, path: UserPathFn) -> Self {
        self.paths.insert(product, path);
        self
    }
    /// The standard per-user application data folder of every registered
    /// product (`FerriteS100/s101-cancellations.json`,
    /// `FerriteS100/navwarn/<user>.json`).
    pub fn standard() -> Self {
        let mut store = Self::new();
        for kind in crate::products::user_record_kinds() {
            let standard = kind.standard_path;
            store = store.with(
                kind.product,
                Arc::new(move |key| standard(standard_data_dir().as_deref(), key)),
            );
        }
        store
    }
}
impl UserStore for NativeUserStore {
    fn product_state(&self, product: ProductId, key: &str) -> Option<Box<dyn UserRecord>> {
        let kind = crate::products::user_record_kind(product)?;
        let path = self.product_state_path(product, key)?;
        Some((kind.native_record)(path))
    }
    fn product_state_path(&self, product: ProductId, key: &str) -> Option<PathBuf> {
        crate::products::user_record_kind(product)?;
        (self.paths.get(&product)?)(key)
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

/// User records kept in memory (lost when the process ends), under the
/// registered products' memory keys (`navwarn/<user>`,
/// `s101-cancellations`).
#[derive(Clone, Default)]
pub struct MemoryUserStore {
    records: Arc<Mutex<HashMap<String, Vec<u8>>>>,
}
/// Key of the S-101 cancellation history in a [`MemoryUserStore`].
#[cfg(feature = "s101")]
pub use ferrite_s101::layer::publication::MEMORY_HISTORY_KEY;

impl MemoryUserStore {
    /// The stored bytes of a record (`navwarn/<user>`,
    /// `s101-cancellations`), for inspection.
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
    fn product_state(&self, product: ProductId, key: &str) -> Option<Box<dyn UserRecord>> {
        let kind = crate::products::user_record_kind(product)?;
        Some(Box::new(MemoryRecord {
            key: (kind.memory_key)(key),
            records: Arc::clone(&self.records),
        }))
    }
    fn product_state_path(&self, _product: ProductId, _key: &str) -> Option<PathBuf> {
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

/// Executor interface and the sequential executor, defined with the product
/// interface (product modules run parallel work on it).
pub use ferrite_product_api::platform::{parallel_map, Executor, SequentialExecutor};

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

impl Platform {
    /// The handles a product layer keeps ([`ferrite_product_api::platform::Services`]).
    pub fn services(&self) -> ferrite_product_api::platform::Services {
        ferrite_product_api::platform::Services {
            data: Arc::clone(&self.data),
            cache: Arc::clone(&self.cache),
            executor: Arc::clone(&self.executor),
        }
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

        #[cfg(feature = "s124")]
        {
            let user = MemoryUserStore::default();
            let record = ferrite_s124::user_state::user_record(&user, "a b").unwrap();
            assert_eq!(record.read(10).unwrap(), None);
            record.write(b"state").unwrap();
            assert_eq!(record.read(10).unwrap().unwrap(), b"state");
            assert_eq!(user.get("navwarn/a_b").unwrap(), b"state");
            assert!(user
                .product_state_path(ferrite_s124::PRODUCT, "a b")
                .is_none());
        }
    }

    #[test]
    fn fixed_clock_reports_its_instant() {
        let at = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let clock = FixedClock(at);
        assert_eq!(clock.unix_seconds(), 1_700_000_000);
        assert_eq!(clock.utc().timestamp(), 1_700_000_000);
    }

    // The S-101 portrayal cache and cancellation history through the
    // engine's memory stores.
    /// An in-memory store remembers a passed SHA-256 check per write: the
    /// next read of the same bytes skips it, a rewrite checks again, and a
    /// corrupted entry is still rejected. Host-checked entries
    /// (`write_verified`) start checked; stores without memory (the default
    /// `read_entry`) check every read.
    #[cfg(feature = "s101")]
    #[test]
    fn memory_store_remembers_integrity_checks_per_write() {
        use ferrite_render::PointInstruction;
        use ferrite_render::{DrawingInstruction, WorldPoint};
        use ferrite_s101::layer::portrayal_cache::{
            encode_cache_payload, read_and_deserialize_cache, stored_digest, wrap_cache,
        };
        use sha2::{Digest, Sha256};
        use std::path::Path;
        let instructions = vec![DrawingInstruction::Point(PointInstruction::new(
            "S".into(),
            WorldPoint::new(1., 2.),
        ))];
        let file = wrap_cache(&encode_cache_payload(&instructions).unwrap());
        assert_eq!(stored_digest(&file), Some(&Sha256::digest(&file[40..])[..]));
        assert_eq!(stored_digest(b"FRC"), None);
        let store = MemoryCacheStore::default();
        let path = Path::new("/memory-cache/portrayal-v1/.ferrite_cell_x.bin");
        store.write(path, &file).unwrap();
        assert!(!store.read_entry(path).unwrap().verified);
        assert_eq!(
            read_and_deserialize_cache(&store, path)
                .unwrap()
                .unwrap()
                .len(),
            1
        );
        assert!(store.read_entry(path).unwrap().verified);
        assert_eq!(
            read_and_deserialize_cache(&store, path)
                .unwrap()
                .unwrap()
                .len(),
            1
        );
        // A rewrite (here corrupted) is checked again and rejected.
        let mut corrupted = file.clone();
        *corrupted.last_mut().unwrap() ^= 1;
        store.write(path, &corrupted).unwrap();
        assert!(!store.read_entry(path).unwrap().verified);
        assert!(read_and_deserialize_cache(&store, path)
            .unwrap()
            .unwrap_err()
            .contains("SHA-256"));
        assert!(!store.read_entry(path).unwrap().verified);
        // A check recorded for an earlier write does not carry over.
        let stale = store.read_entry(path).unwrap().version.unwrap();
        store.write(path, &file).unwrap();
        store.mark_verified(path, stale);
        assert!(!store.read_entry(path).unwrap().verified);
        // Entries the host checked start verified; magic and schema
        // version are still checked.
        store.write_verified(path, &file).unwrap();
        assert!(store.read_entry(path).unwrap().verified);
        let mut old_schema = file.clone();
        old_schema[4] ^= 1;
        store.write_verified(path, &old_schema).unwrap();
        assert!(read_and_deserialize_cache(&store, path)
            .unwrap()
            .unwrap_err()
            .contains("schema"));
        assert!(read_and_deserialize_cache(&NoCacheStore, path).is_err());
    }

    #[cfg(feature = "s101")]
    #[test]
    fn history_store_in_a_memory_user_store_round_trips_and_refuses_corruption() {
        use ferrite_s101::layer::publication::{
            CancellationHistory, CancellationRecord, HistoryStore,
        };
        let user = MemoryUserStore::default();
        let record = CancellationRecord {
            key: ("INT.IHO.S-101.2.0".into(), "101AA00TEST".into()),
            cancelled_edition: 4,
            cancellation_update: 3,
            issue_date: chrono::NaiveDate::from_ymd_opt(2024, 10, 17).unwrap(),
        };
        let mut store = HistoryStore::begin_record(
            ferrite_s101::layer::publication::history_record(&user).unwrap(),
        )
        .unwrap();
        assert!(store.history.is_empty());
        store.persist(vec![record.clone()]).unwrap();
        drop(store);
        let reopened = CancellationHistory::read_record(
            ferrite_s101::layer::publication::history_record(&user)
                .unwrap()
                .as_ref(),
        )
        .unwrap();
        assert_eq!(reopened.get(&record.key), Some(&record));
        // Same document bytes as the native file.
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("history.json");
        HistoryStore::begin(&path)
            .unwrap()
            .persist(vec![record.clone()])
            .unwrap();
        assert_eq!(
            user.get(MEMORY_HISTORY_KEY).unwrap(),
            std::fs::read(&path).unwrap()
        );
        ferrite_s101::layer::publication::history_record(&user)
            .unwrap()
            .write(b"truncated")
            .unwrap();
        assert!(CancellationHistory::read_record(
            ferrite_s101::layer::publication::history_record(&user)
                .unwrap()
                .as_ref()
        )
        .is_err());
        assert!(HistoryStore::begin_record(
            ferrite_s101::layer::publication::history_record(&user).unwrap()
        )
        .is_err());
    }
}
