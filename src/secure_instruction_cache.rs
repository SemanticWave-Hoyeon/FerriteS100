//! App-private disposable portrayal cache. The payload hash detects corruption;
//! it is not a signature. Never consume cache sidecars in dataset folders.
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

pub(crate) const MAX_CACHE_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) const MAX_INSTRUCTIONS: u64 = 100_000;
static NONCE: AtomicU64 = AtomicU64::new(0);

pub(crate) fn root() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME").map(|p| PathBuf::from(p).join("Library/Caches"));
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".cache")));
    let path = base?.join("FerriteS100/portrayal-v1");
    prepare_root(&path).ok()?;
    Some(path)
}
fn prepare_root(path: &Path) -> std::io::Result<()> {
    if !path.is_absolute() {
        return Err(invalid("Cache root must be absolute"));
    }
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)?;
    // The application's immediate container is trusted configuration too.
    // Never adopt a linked or publicly writable app namespace.
    if let Some(parent) = path.parent() {
        let parent_meta = fs::symlink_metadata(parent)?;
        if !parent_meta.file_type().is_dir() {
            return Err(invalid("Cache parent is not a real directory"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            if parent_meta.uid() != unsafe { libc::geteuid() }
                || parent_meta.permissions().mode() & 0o022 != 0
            {
                return Err(invalid("Cache parent is not owned and protected"));
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if parent_meta.file_attributes() & 0x400 != 0 {
                return Err(invalid("Cache parent is a reparse point"));
            }
        }
    }
    let meta = fs::symlink_metadata(path)?;
    if !meta.file_type().is_dir() {
        return Err(invalid("Cache root is not a real directory"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if meta.uid() != unsafe { libc::geteuid() } || meta.permissions().mode() & 0o077 != 0 {
            return Err(invalid("Cache root is not private to the current user"));
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return Err(invalid("Cache root is a reparse point"));
        }
    }
    Ok(())
}
fn invalid(message: &'static str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message)
}
fn open_regular(path: &Path) -> std::io::Result<fs::File> {
    if !fs::symlink_metadata(path)?.file_type().is_file() {
        return Err(invalid("Cache is not a regular file"));
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_OPEN_REPARSE_POINT: inspect the link itself, never its target.
        options.custom_flags(0x0020_0000);
    }
    let file = options.open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(invalid("Opened cache is not a regular file"));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return Err(invalid("Opened cache is a reparse point"));
        }
    }
    Ok(file)
}
pub(crate) fn read(path: &Path) -> std::io::Result<Vec<u8>> {
    read_bounded(path, MAX_CACHE_BYTES)
}
fn read_bounded(path: &Path, limit: u64) -> std::io::Result<Vec<u8>> {
    let file = open_regular(path)?;
    if file.metadata()?.len() > limit {
        return Err(invalid("Cache exceeds byte budget"));
    }
    let mut bytes = Vec::new();
    file.take(
        limit
            .checked_add(1)
            .ok_or_else(|| invalid("Cache limit overflow"))?,
    )
    .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(invalid("Cache grew beyond byte budget"));
    }
    Ok(bytes)
}
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if bytes.len() as u64 > MAX_CACHE_BYTES {
        return Err(invalid("Cache exceeds byte budget"));
    }
    let parent = path
        .parent()
        .ok_or_else(|| invalid("Cache has no parent"))?;
    prepare_root(parent)?;
    // Reject existing links/special files. Atomic replacement never opens the destination.
    match fs::symlink_metadata(path) {
        Ok(meta) if !meta.file_type().is_file() => {
            return Err(invalid("Cache destination is not regular"))
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let tick = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| invalid("Cache clock unavailable"))?
        .as_nanos();
    let temporary = parent.join(format!(
        ".cache-{}-{tick}-{}.tmp",
        std::process::id(),
        NONCE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut owns_temporary = false;
    let result = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        owns_temporary = true;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        replace(&temporary, path)?;
        #[cfg(unix)]
        {
            fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
    })();
    if result.is_err() && owns_temporary {
        let _ = fs::remove_file(&temporary);
    }
    result
}
#[cfg(not(windows))]
fn replace(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::rename(from, to)
}
#[cfg(windows)]
fn replace(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    let ok = unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let tick = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let p = std::env::temp_dir().join(format!(
                "ferrite-private-cache-test-{}-{tick}-{}",
                std::process::id(),
                NONCE.fetch_add(1, Ordering::Relaxed)
            ));
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                fs::DirBuilder::new().mode(0o700).create(&p).unwrap();
            }
            #[cfg(not(unix))]
            fs::create_dir(&p).unwrap();
            let cache = p.join("cache");
            prepare_root(&cache).unwrap();
            Self(cache)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(self.0.parent().unwrap());
        }
    }
    #[test]
    fn bounded_read_and_atomic_replace() {
        let d = Fixture::new();
        let p = d.0.join("cache.bin");
        write_atomic(&p, b"old").unwrap();
        write_atomic(&p, b"newer").unwrap();
        assert_eq!(read(&p).unwrap(), b"newer");
        assert!(read_bounded(&p, 4).is_err());
        assert_eq!(fs::read_dir(&d.0).unwrap().count(), 1);
    }
    #[cfg(unix)]
    #[test]
    fn symlink_never_reads_or_overwrites_sentinel() {
        use std::os::unix::fs::symlink;
        let d = Fixture::new();
        let sentinel = d.0.join("sentinel");
        let p = d.0.join("cache.bin");
        fs::write(&sentinel, b"preserve").unwrap();
        symlink(&sentinel, &p).unwrap();
        assert!(read(&p).is_err());
        assert!(write_atomic(&p, b"changed").is_err());
        assert_eq!(fs::read(&sentinel).unwrap(), b"preserve");
    }
}
