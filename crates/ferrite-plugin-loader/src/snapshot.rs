//! Owned verification input and private library lifetime. Same-user/administrator
//! filesystem tampering and dependent-library search are not sandboxed here.
use crate::{verifier::compute_sha256, PluginError};
use libloading::Library;
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};
pub(crate) const MAX_PLUGIN_BYTES: usize = 128 * 1024 * 1024;
fn error(message: &str) -> PluginError {
    PluginError::LoadError(message.into())
}
fn filename(name: &str) -> Result<(), PluginError> {
    if name.is_empty()
        || name.len() > 255
        || matches!(name, "." | "..")
        || name.ends_with('.')
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(error(
            "DLL/resource must be one portable filename, not a path/ADS",
        ));
    }
    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && matches!(stem.as_bytes()[3], b'1'..=b'9'))
    {
        return Err(error("Reserved Windows device filename"));
    }
    Ok(())
}
pub(crate) fn canonical_root(root: &Path) -> Result<PathBuf, PluginError> {
    let metadata = fs::symlink_metadata(root)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(error("Plugin root must be a real directory"));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(error("Plugin root reparse point forbidden"));
        }
    }
    Ok(root.canonicalize()?)
}
fn regular(metadata: &fs::Metadata, max: usize) -> Result<(), PluginError> {
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > max as u64 {
        return Err(error("Plugin resource must be a bounded regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(error("Plugin resource hard links forbidden"));
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(error("Plugin resource reparse point forbidden"));
        }
    }
    Ok(())
}
pub(crate) fn capture_regular(root: &Path, name: &str, max: usize) -> Result<Vec<u8>, PluginError> {
    filename(name)?;
    let path = root.join(name);
    let before = fs::symlink_metadata(&path)?;
    regular(&before, max)?;
    if path.canonicalize()?.parent() != Some(root) {
        return Err(error("Plugin resource escapes canonical root"));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x00200000).share_mode(1);
    }
    let file = options.open(&path)?;
    let opened = file.metadata()?;
    regular(&opened, max)?;
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        };
        let mut information: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) } == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if information.nNumberOfLinks != 1 {
            return Err(error("Plugin resource hard links forbidden"));
        }
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if (opened.dev(), opened.ino()) != (before.dev(), before.ino()) {
            return Err(error("Plugin resource replaced before capture"));
        }
    }
    let mut bytes = Vec::new();
    file.take(max as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > max {
        return Err(error("Plugin resource grew beyond receiver cap"));
    }
    let after = fs::symlink_metadata(&path)?;
    regular(&after, max)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if (opened.dev(), opened.ino()) != (after.dev(), after.ino()) {
            return Err(error("Plugin resource replaced during capture"));
        }
    }
    Ok(bytes)
}
struct PrivateDirectory {
    path: PathBuf,
}
impl PrivateDirectory {
    fn new() -> Result<Self, PluginError> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let parent = std::env::temp_dir().canonicalize()?;
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| error("Invalid host clock"))?
            .as_nanos();
        for _ in 0..64 {
            let path = parent.join(format!(
                "ferrite-plugin-{}-{time:x}-{:x}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match create_private_directory(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err(error("Private plugin directory creation collision limit"))
    }
}
impl Drop for PrivateDirectory {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.path) {
            tracing::warn!("Private plugin snapshot cleanup failed: {error}");
        }
    }
}
#[cfg(unix)]
fn create_private_directory(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700);
    builder.create(path)?;
    if fs::symlink_metadata(path)?.permissions().mode() & 0o077 != 0 {
        let _ = fs::remove_dir(path);
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "Private directory mode unavailable",
        ));
    }
    Ok(())
}
#[cfg(windows)]
fn create_private_directory(path: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::{
            Authorization::{
                ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
            },
            SECURITY_ATTRIBUTES,
        },
        Storage::FileSystem::{CreateDirectoryW, GetVolumeInformationW, GetVolumePathNameW},
    };
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut volume = vec![0u16; 32768];
    let mut flags = 0u32;
    // Filesystems that cannot persist private ACLs are an explicit capability rejection.
    unsafe {
        if GetVolumePathNameW(wide.as_ptr(), volume.as_mut_ptr(), volume.len() as u32) == 0
            || GetVolumeInformationW(
                volume.as_ptr(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut flags,
                std::ptr::null_mut(),
                0,
            ) == 0
        {
            return Err(std::io::Error::last_os_error());
        }
    }
    if flags & 8 == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Persistent private ACLs required for plugin snapshots",
        ));
    }
    // Protected DACL, inheritable owner-rights and SYSTEM only. Applied at creation,
    // rather than exposing an empty shared directory before a later ACL update.
    let sddl: Vec<u16> = "D:P(A;OICI;FA;;;OW)(A;OICI;FA;;;SY)"
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let mut descriptor = std::ptr::null_mut();
    unsafe {
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            std::ptr::null_mut(),
        ) == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        let success = CreateDirectoryW(wide.as_ptr(), &attributes);
        let error = if success == 0 {
            Some(std::io::Error::last_os_error())
        } else {
            None
        };
        LocalFree(descriptor);
        if let Some(error) = error {
            return Err(error);
        }
    }
    Ok(())
}
#[cfg(not(any(unix, windows)))]
fn create_private_directory(_: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "Private plugin snapshot platform unsupported",
    ))
}
pub(crate) struct PrivateSnapshot {
    _directory: PrivateDirectory,
    path: PathBuf,
}
impl PrivateSnapshot {
    pub(crate) fn new(name: &str, bytes: &[u8]) -> Result<Self, PluginError> {
        filename(name)?;
        if bytes.is_empty() || bytes.len() > MAX_PLUGIN_BYTES {
            return Err(error("Plugin snapshot payload outside receiver cap"));
        }
        let directory = PrivateDirectory::new()?;
        let path = directory.path.join(name);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options.open(&path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        // Only compare with the authorized byte capture, never re-read the source.
        let copied = capture_regular(&directory.path, name, MAX_PLUGIN_BYTES)?;
        if copied.len() != bytes.len() || compute_sha256(&copied) != compute_sha256(bytes) {
            return Err(error("Private plugin snapshot copy mismatch"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o400))?;
        }
        #[cfg(windows)]
        {
            let mut permissions = fs::metadata(&path)?.permissions();
            permissions.set_readonly(true);
            fs::set_permissions(&path, permissions)?;
        }
        Ok(Self {
            _directory: directory,
            path,
        })
    }
}
impl Drop for PrivateSnapshot {
    fn drop(&mut self) {
        #[cfg(windows)]
        {
            if let Ok(metadata) = fs::metadata(&self.path) {
                let mut permissions = metadata.permissions();
                permissions.set_readonly(false);
                let _ = fs::set_permissions(&self.path, permissions);
            }
        }
        // directory's field drop occurs afterwards, only after library unloading.
    }
}
/// Library is dropped before its private snapshot (Rust declaration-order field drop).
/// Manager keeps the plugin instance ahead of this wrapper for the same reason.
pub struct PluginLibrary {
    library: Library,
    _snapshot: PrivateSnapshot,
}
impl PluginLibrary {
    pub(crate) unsafe fn load(snapshot: PrivateSnapshot) -> Result<Self, PluginError> {
        let library = unsafe { Library::new(&snapshot.path) }
            .map_err(|error| PluginError::LoadError(error.to_string()))?;
        Ok(Self {
            library,
            _snapshot: snapshot,
        })
    }
}
impl std::ops::Deref for PluginLibrary {
    type Target = Library;
    fn deref(&self) -> &Library {
        &self.library
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    #[test]
    fn portable_filename_rejects_traversal_drives_ads_and_devices() {
        for name in [
            "../evil.dll",
            "/evil.dll",
            "C:\\evil.dll",
            "lib.dll:stream",
            "a/b.so",
            "a\\b.so",
            "..",
            "NUL.dll",
            "COM1.dll",
            "lib.",
        ] {
            assert!(filename(name).is_err(), "{name}");
        }
        for name in [
            "route-plugin.dll",
            "libroute_plugin.dylib",
            "libroute_plugin.so",
        ] {
            assert!(filename(name).is_ok());
        }
    }
    #[test]
    fn snapshot_is_owned_retained_and_source_mutation_does_not_change_it() {
        let source = PrivateDirectory::new().unwrap();
        let path = source.path.join("plugin.dll");
        fs::write(&path, b"authorized original").unwrap();
        let root = canonical_root(&source.path).unwrap();
        let captured = capture_regular(&root, "plugin.dll", 128).unwrap();
        let snapshot = PrivateSnapshot::new("plugin.dll", &captured).unwrap();
        fs::write(&path, b"changed source").unwrap();
        assert_eq!(fs::read(&snapshot.path).unwrap(), b"authorized original");
        let owned = snapshot._directory.path.clone();
        assert!(owned.exists());
        drop(snapshot);
        assert!(!owned.exists());
    }
    // Exercise the loader's capture -> authorize -> private-copy boundary while
    // another thread atomically replaces the original path. This is not a
    // dynamic-library execution or dependency-search isolation test.
    #[cfg(unix)]
    #[test]
    fn signed_capture_survives_concurrent_original_path_replacement() {
        use crate::verifier::{compute_sha256_hex, PluginVerifier};
        use ed25519_dalek::{Signer, SigningKey};
        use std::sync::mpsc;

        let source = PrivateDirectory::new().unwrap();
        let path = source.path.join("plugin.dylib");
        let original = b"authorized non-executable regression fixture";
        fs::write(&path, original).unwrap();
        let captured = capture_regular(&source.path, "plugin.dylib", 128).unwrap();
        let key = SigningKey::from_bytes(&[37; 32]);
        let signature = key.sign(&compute_sha256(&captured)).to_bytes();
        let verifier = PluginVerifier::new(Some(&key.verifying_key().to_bytes())).unwrap();
        let digest = compute_sha256_hex(&captured);
        verifier.verify_hash_bytes(&captured, &digest).unwrap();
        verifier
            .verify_signature_bytes(&captured, Some(&signature))
            .unwrap();

        let (request, requests) = mpsc::channel();
        let (completed, completion) = mpsc::channel();
        let replacement = source.path.join("replacement.dylib");
        let writer_path = path.clone();
        let writer = std::thread::spawn(move || {
            for generation in requests {
                fs::write(
                    &replacement,
                    format!("unauthorized replacement {generation}"),
                )
                .unwrap();
                fs::rename(&replacement, &writer_path).unwrap();
                completed.send(()).unwrap();
            }
        });
        // Deterministically replace after verification, before private copying.
        request.send(0).unwrap();
        completion.recv().unwrap();
        let snapshot = PrivateSnapshot::new("plugin.dylib", &captured).unwrap();
        for generation in 1..=32 {
            request.send(generation).unwrap();
            completion.recv().unwrap();
            let copied = fs::read(&snapshot.path).unwrap();
            assert_eq!(copied, original);
            verifier.verify_hash_bytes(&copied, &digest).unwrap();
            verifier
                .verify_signature_bytes(&copied, Some(&signature))
                .unwrap();
            let changed = capture_regular(&source.path, "plugin.dylib", 128).unwrap();
            assert!(verifier.verify_hash_bytes(&changed, &digest).is_err());
            assert!(verifier
                .verify_signature_bytes(&changed, Some(&signature))
                .is_err());
        }
        drop(request);
        writer.join().unwrap();
    }

    #[test]
    fn capture_caps_before_read_and_rejects_directory() {
        let root = PrivateDirectory::new().unwrap();
        let path = root.path.join("plugin.dll");
        let file = File::create(&path).unwrap();
        file.set_len(129).unwrap();
        assert!(capture_regular(&root.path, "plugin.dll", 128).is_err());
        assert!(capture_regular(&root.path, ".", 128).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn links_reject_and_snapshot_modes_private() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let root = PrivateDirectory::new().unwrap();
        let outside = PrivateDirectory::new().unwrap();
        let target = outside.path.join("plugin.dll");
        fs::write(&target, b"test").unwrap();
        symlink(&target, root.path.join("link.dll")).unwrap();
        assert!(capture_regular(&root.path, "link.dll", 128).is_err());
        fs::hard_link(&target, root.path.join("hard.dll")).unwrap();
        assert!(capture_regular(&root.path, "hard.dll", 128).is_err());
        symlink(&outside.path, root.path.join("directory")).unwrap();
        assert!(canonical_root(&root.path.join("directory")).is_err());
        let snapshot = PrivateSnapshot::new("plugin.dll", b"test").unwrap();
        assert_eq!(
            fs::metadata(&snapshot._directory.path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&snapshot.path).unwrap().permissions().mode() & 0o777,
            0o400
        );
    }
}
