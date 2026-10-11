//! Import/restart replay gate only. Never creates removal permission or deletes user files.
//! Root must connect explicit trusted existing-path configuration and every render consumer.
use crate::s102_original_inputs::OwnedOriginalInput;
use anyhow::{ensure, Context, Result};
use ferrite_s102::cancellation_journal::store::Store;
use ferrite_security::AuthenticatedSnapshot;
use sha2::{Digest, Sha384};
use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};

/// An explicit option cannot silently disappear on malformed/missing values.
pub(crate) fn history_path(args: impl IntoIterator<Item = String>) -> Result<Option<PathBuf>> {
    let mut args = args.into_iter();
    let mut chosen = None;
    while let Some(arg) = args.next() {
        let value = if arg == "--s102-cancellation-history" {
            Some(
                args.next()
                    .context("S102 cancellation history needs an existing absolute path")?,
            )
        } else {
            arg.strip_prefix("--s102-cancellation-history=")
                .map(str::to_owned)
        };
        if let Some(value) = value {
            ensure!(
                chosen.is_none(),
                "Duplicate S102 cancellation history option"
            );
            let path = PathBuf::from(value);
            ensure!(
                path.is_absolute(),
                "S102 cancellation history needs an existing absolute path"
            );
            chosen = Some(path);
        }
    }
    Ok(chosen)
}

pub(crate) struct Gate {
    store: Store,
    blocked: bool,
    successful_checks: u64,
}
impl Gate {
    /// Caller explicitly chose an existing journal in a trusted application-owned
    /// parent. Store opens read/write, acquires an exclusive OS lock and syncs the
    /// existing file/namespace; this is not a read-only file operation.
    /// Missing/corrupt history is an error: never implicit initialization here.
    pub(crate) fn open_existing(trusted_path: &Path) -> Result<Self> {
        private_location(trusted_path)?;
        let store = Store::open(trusted_path)?;
        validate_opened_private_location(&store)?;
        Ok(Self {
            store,
            blocked: false,
            successful_checks: 0,
        })
    }
    pub(crate) fn successful_checks(&self) -> u64 {
        self.successful_checks
    }
    pub(crate) fn require_ready(&self) -> Result<()> {
        ensure!(
            !self.blocked && !self.store.recovery_required(),
            "S102 journal recovery must precede scene use"
        );
        validate_opened_private_location(&self.store)?;
        self.store.journal()?;
        Ok(())
    }
    /// Future cancellation transaction calls this for any indeterminate durable
    /// write. UI unload/clear must not reset it. No automatic unblock API is offered.
    pub(crate) fn block_for_recovery(&mut self) {
        self.blocked = true;
    }

    /// Call after authenticated capture, BEFORE decoding, and again before scene
    /// publication with the SAME retained parser snapshot owner. A UI signature
    /// toggle never replaces these private authentication inputs.
    pub(crate) fn check_input(
        &mut self,
        record: Option<&Arc<OwnedOriginalInput>>,
        parser_snapshot: Option<&Arc<AuthenticatedSnapshot>>,
        canonical_source: &Path,
    ) -> Result<()> {
        if let Err(error) = self.require_ready() {
            self.blocked = true;
            return Err(error);
        }
        if self.store.journal().is_err() {
            self.blocked = true;
            anyhow::bail!("S102 journal namespace/state became unavailable");
        }
        let record = record.context("Enforced S102 journal requires authenticated original; OFF/evaluation cannot bypass replay")?;
        let parser_snapshot =
            parser_snapshot.context("Enforced S102 journal needs authenticated parser snapshot")?;
        ensure!(
            Arc::ptr_eq(&record.snapshot, parser_snapshot),
            "Replay gate received a foreign parser snapshot owner"
        );
        ensure!(
            record.snapshot.source == canonical_source
                && record.original.resource_path() == canonical_source,
            "Replay original/source locator ownership mismatch"
        );
        // Hash the retained private file, never reopen canonical_source. Existing
        // capture.verify still runs before/after decoding and before publication.
        verify_snapshot(record)?;
        let journal = match self.store.journal() {
            Ok(journal) => journal,
            Err(error) => {
                self.blocked = true;
                return Err(error);
            }
        };
        journal.validate_reimport(&record.original)?;
        self.successful_checks = self.successful_checks.saturating_add(1);
        Ok(())
    }
}
/// Explicit existing state only: never create folders, follow a leaf/parent
/// symlink, or accept a namespace writable by other users. Windows ACL policy
/// must be qualified before this optional operational history mode is enabled.
fn validate_opened_private_location(store: &Store) -> Result<()> {
    #[cfg(unix)]
    {
        store.validate_private_namespace(unsafe { libc::geteuid() })
    }
    #[cfg(not(unix))]
    {
        let _ = store;
        anyhow::bail!(
            "S102 operational history private-directory policy is not qualified on this platform"
        )
    }
}

fn private_location(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let parent = path
            .parent()
            .context("Journal needs an explicit private parent")?;
        let dir = std::fs::symlink_metadata(parent)?;
        let leaf = std::fs::symlink_metadata(path)?;
        let uid = unsafe { libc::geteuid() };
        ensure!(
            dir.is_dir()
                && !dir.file_type().is_symlink()
                && dir.uid() == uid
                && dir.mode() & 0o077 == 0,
            "S102 history requires an existing private user-owned directory"
        );
        ensure!(
            leaf.is_file()
                && !leaf.file_type().is_symlink()
                && leaf.uid() == uid
                && leaf.mode() & 0o077 == 0,
            "S102 history requires an existing private user-owned regular file"
        );
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        anyhow::bail!(
            "S102 operational history private-directory policy is not qualified on this platform"
        )
    }
}

fn verify_snapshot(record: &OwnedOriginalInput) -> Result<()> {
    const CAP: u64 = 512 * 1024 * 1024; // Existing App S102 aggregate capture budget.
    let expected = record.original.resource_size();
    ensure!(
        expected <= CAP,
        "Original snapshot exceeds existing App capture budget"
    );
    let mut file = File::open(record.snapshot.path())?;
    ensure!(
        file.metadata()?.is_file(),
        "Captured original is not a regular file"
    );
    let mut hash = Sha384::new();
    let mut bytes = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        // An extra byte detects growth without trusting path metadata alone.
        let remaining = expected.checked_add(1).context("Snapshot bound overflow")? - bytes;
        let limit = usize::try_from(remaining.min(buffer.len() as u64))?;
        let n = file.read(&mut buffer[..limit])?;
        if n == 0 {
            break;
        }
        bytes = bytes
            .checked_add(n as u64)
            .context("Snapshot length overflow")?;
        ensure!(
            bytes <= expected,
            "Captured original grew or changed length"
        );
        hash.update(&buffer[..n]);
    }
    ensure!(bytes == expected, "Captured original shortened");
    let digest: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
    ensure!(
        digest.eq_ignore_ascii_case(record.original.resource_sha384()),
        "Captured original differs from retained cryptographic authentication"
    );
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    fn private_dir() -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }
    #[test]
    fn explicit_history_option_never_silently_falls_back() {
        let parse = |args: &[&str]| history_path(args.iter().map(|v| (*v).to_owned()));
        assert!(parse(&["--debug"]).unwrap().is_none());
        assert_eq!(
            parse(&["--s102-cancellation-history", "/private/state/history.log"]).unwrap(),
            Some(PathBuf::from("/private/state/history.log"))
        );
        assert_eq!(
            parse(&["--s102-cancellation-history=/private/state/history.log"]).unwrap(),
            Some(PathBuf::from("/private/state/history.log"))
        );
        for args in [
            vec!["--s102-cancellation-history="],
            vec!["--s102-cancellation-history=relative.log"],
            vec![
                "--s102-cancellation-history=/one.log",
                "--s102-cancellation-history",
                "/two.log",
            ],
            vec![
                "--s102-cancellation-history",
                "/one.log",
                "--s102-cancellation-history=/two.log",
            ],
            vec![
                "--s102-cancellation-history=/one.log",
                "--s102-cancellation-history=/two.log",
            ],
            vec!["--s102-cancellation-history"],
            vec!["--s102-cancellation-history", "--debug"],
            vec!["--s102-cancellation-history", "relative.log"],
            vec![
                "--s102-cancellation-history",
                "/one.log",
                "--s102-cancellation-history",
                "/two.log",
            ],
        ] {
            assert!(parse(&args).is_err());
        }
    }
    #[test]
    fn shared_namespace_or_history_permissions_are_rejected_without_reset() {
        use std::os::unix::fs::PermissionsExt;
        let dir = private_dir();
        let path = dir.path().join("s102.log");
        drop(Store::initialize(&path).unwrap());
        let bytes = std::fs::read(&path).unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(Gate::open_existing(&path).is_err());
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Gate::open_existing(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
    #[test]
    fn opened_history_permissions_are_rechecked_and_block_remains_sticky() {
        use std::os::unix::fs::PermissionsExt;
        for change_parent in [false, true] {
            let dir = private_dir();
            let path = dir.path().join("s102.log");
            drop(Store::initialize(&path).unwrap());
            let original = std::fs::read(&path).unwrap();
            let mut gate = Gate::open_existing(&path).unwrap();
            let changed = if change_parent {
                dir.path()
            } else {
                path.as_path()
            };
            let restored = if change_parent { 0o700 } else { 0o600 };
            std::fs::set_permissions(changed, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert!(gate.require_ready().is_err());
            assert!(gate.check_input(None, None, Path::new("any.h5")).is_err());
            std::fs::set_permissions(changed, std::fs::Permissions::from_mode(restored)).unwrap();
            assert!(gate.require_ready().is_err());
            assert_eq!(std::fs::read(&path).unwrap(), original);
        }
    }
    #[test]
    fn parent_replacement_is_rejected_even_with_the_same_leaf_inode() {
        let root = private_dir();
        let parent = root.path().join("history");
        std::fs::create_dir(&parent).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = parent.join("s102.log");
        drop(Store::initialize(&path).unwrap());
        let gate = Gate::open_existing(&path).unwrap();
        let old = root.path().join("old-history");
        std::fs::rename(&parent, &old).unwrap();
        std::fs::create_dir(&parent).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();
        // Move, rather than hard-link, the leaf: the original descriptor still
        // matches this pathname and has nlink=1. Parent identity must catch it.
        std::fs::rename(old.join("s102.log"), &path).unwrap();
        assert!(gate.require_ready().is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"FS102JL1");
    }
    #[test]
    fn missing_history_never_creates_parent_or_empty_history() {
        let dir = private_dir();
        let path = dir.path().join("uncreated").join("s102.log");
        assert!(Gate::open_existing(&path).is_err());
        assert!(!path.parent().unwrap().exists());
        let path = dir.path().join("absent.log");
        assert!(Gate::open_existing(&path).is_err());
        assert!(!path.exists());
    }
    #[test]
    fn explicit_library_initialization_is_not_an_import_side_effect() {
        let dir = private_dir();
        let path = dir.path().join("s102.log");
        let bootstrap = Store::initialize(&path).unwrap();
        assert!(Gate::open_existing(&path).is_err()); // Other descriptor/process exclusion.
        drop(bootstrap);
        let gate = Gate::open_existing(&path).unwrap();
        gate.require_ready().unwrap();
        assert!(gate.store.journal().unwrap().is_empty());
    }
    #[test]
    fn signature_off_and_empty_snapshot_do_not_admit_replay_or_write_history() {
        let dir = private_dir();
        let path = dir.path().join("s102.log");
        drop(Store::initialize(&path).unwrap());
        let original = std::fs::read(&path).unwrap();
        let mut gate = Gate::open_existing(&path).unwrap();
        let error = gate
            .check_input(None, None, Path::new("unchecked.h5"))
            .unwrap_err();
        assert!(error.to_string().contains("authenticated original"));
        assert_eq!(original, std::fs::read(&path).unwrap());
        assert!(gate.store.journal().unwrap().is_empty());
        gate.require_ready().unwrap(); // A refused untrusted import is not a corrupt log.
    }
    #[test]
    fn corrupt_restart_and_explicit_recovery_block_never_silently_reset() {
        let dir = private_dir();
        let path = dir.path().join("s102.log");
        std::fs::write(&path, b"invalid").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert!(Gate::open_existing(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"invalid");
        std::fs::remove_file(&path).unwrap(); // Test-only explicit fixture replacement.
        drop(Store::initialize(&path).unwrap());
        let mut gate = Gate::open_existing(&path).unwrap();
        gate.block_for_recovery();
        for _ in 0..500 {
            assert!(gate.require_ready().is_err());
        }
        assert!(gate.check_input(None, None, Path::new("any.h5")).is_err());
        assert!(gate.store.journal().unwrap().is_empty());
    }
}
