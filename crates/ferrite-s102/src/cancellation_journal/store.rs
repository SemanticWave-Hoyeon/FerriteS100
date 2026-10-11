//! Exclusive append-only historical store. No path replacement/rename is used.
//! SHA384 chaining detects corruption, not hostile same-user rewriting or authority.
//! A caller must reconcile the live scene with the journal before rendering after
//! any indeterminate result. This module does not publish or remove datasets.
use super::{
    Journal, Receipt, FIXED_RECORD, HEADER, LEGACY_FIXED_RECORD, MAX_BYTES, MAX_PRODUCER,
    MAX_RECORDS, MAX_URI,
};
use anyhow::{ensure, Context, Result};
use sha2::{Digest, Sha384};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};
const MAGIC: &[u8; 8] = b"FS102JL1";
const COMMIT: &[u8; 8] = b"COMMIT01";
pub const MAX_LOG_BYTES: u64 = 9 * 1024 * 1024;
const MAX_PAYLOAD: usize = HEADER + FIXED_RECORD + MAX_PRODUCER + MAX_URI;

/// OS-acknowledged synchronization scope, not a hardware power-loss certificate.
/// Windows file synchronization alone does not assert directory-entry durability;
/// that namespace contract must be separately qualified before App activation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncScope {
    FileAndUnixNamespace,
    FileOnly,
}
#[derive(Debug)]
#[must_use = "Indeterminate storage must block publication until reconciliation"]
pub enum CommitOutcome {
    Unchanged(String),
    Committed(SyncScope),
    Indeterminate(String),
}
/// The descriptor and OS lock remain owned across journal checks, persistence
/// and the caller's infallible publication. Never replace the locked inode/path.
pub struct Store {
    file: File,
    parent: PathBuf,
    #[cfg(unix)]
    parent_handle: File,
    path: PathBuf,
    journal: Journal,
    bytes: u64,
    chain: [u8; 48],
    recovery_required: bool,
}
impl Store {
    /// Parent must already be a trusted application-owned directory. Exclusive
    /// creation never overwrites existing history. Header-only bootstrap failure
    /// may leave a file; do not automatically delete or reset it on retry.
    pub fn initialize(path: &Path) -> Result<Self> {
        let (parent, path) = checked_location(path)?;
        #[cfg(unix)]
        let parent_handle = File::open(&parent)?;
        #[cfg(unix)]
        validate_parent_namespace(&parent_handle, &parent)?;
        let mut options = options();
        options.create_new(true);
        let mut file = options.open(&path)?;
        file.try_lock()?;
        validate_namespace(&file, &path)?;
        #[cfg(unix)]
        validate_parent_namespace(&parent_handle, &parent)?;
        file.write_all(MAGIC)?;
        file.sync_all()?;
        #[cfg(unix)]
        sync_namespace(&parent_handle, &parent)?;
        #[cfg(not(unix))]
        sync_namespace(&parent)?;
        validate_namespace(&file, &path)?;
        #[cfg(unix)]
        validate_parent_namespace(&parent_handle, &parent)?;
        Ok(Self {
            file,
            parent,
            #[cfg(unix)]
            parent_handle,
            path,
            journal: Journal::default(),
            bytes: 8,
            chain: seed(),
            recovery_required: false,
        })
    }
    /// Missing/corrupt/incomplete history is an error, never implicit empty state.
    pub fn open(path: &Path) -> Result<Self> {
        let (parent, path) = checked_location(path)?;
        #[cfg(unix)]
        let parent_handle = File::open(&parent)?;
        #[cfg(unix)]
        validate_parent_namespace(&parent_handle, &parent)?;
        let mut file = options().open(&path)?;
        file.try_lock()?;
        validate_namespace(&file, &path)?;
        #[cfg(unix)]
        validate_parent_namespace(&parent_handle, &parent)?;
        let (journal, bytes, chain) = read_log(&mut file)?;
        // Re-acknowledge complete historical bytes before permitting later use.
        file.sync_all()?;
        #[cfg(unix)]
        sync_namespace(&parent_handle, &parent)?;
        #[cfg(not(unix))]
        sync_namespace(&parent)?;
        validate_namespace(&file, &path)?;
        #[cfg(unix)]
        validate_parent_namespace(&parent_handle, &parent)?;
        Ok(Self {
            file,
            parent,
            #[cfg(unix)]
            parent_handle,
            path,
            journal,
            bytes,
            chain,
            recovery_required: false,
        })
    }
    fn sync_namespace(&self) -> Result<SyncScope> {
        #[cfg(unix)]
        {
            sync_namespace(&self.parent_handle, &self.parent)
        }
        #[cfg(not(unix))]
        {
            sync_namespace(&self.parent)
        }
    }
    fn validate_namespace(&self) -> Result<()> {
        #[cfg(unix)]
        validate_parent_namespace(&self.parent_handle, &self.parent)?;
        validate_namespace(&self.file, &self.path)
    }
    /// Optional App privacy policy, checked against retained descriptors as well
    /// as current pathname identities. The initial ancestor trust remains a
    /// caller contract; this does not certify hostile same-user anti-rollback.
    #[cfg(unix)]
    pub fn validate_private_namespace(&self, expected_uid: u32) -> Result<()> {
        use std::os::unix::fs::MetadataExt;
        self.validate_namespace()?;
        let parent = self.parent_handle.metadata()?;
        let leaf = self.file.metadata()?;
        ensure!(
            parent.uid() == expected_uid && parent.mode() & 0o077 == 0,
            "Opened S102 history parent is not private and user-owned"
        );
        ensure!(
            leaf.uid() == expected_uid && leaf.mode() & 0o077 == 0,
            "Opened S102 history file is not private and user-owned"
        );
        Ok(())
    }
    pub fn journal(&self) -> Result<&Journal> {
        ensure!(
            !self.recovery_required,
            "Journal requires recovery before scene use"
        );
        self.validate_namespace()?;
        Ok(&self.journal)
    }
    pub fn recovery_required(&self) -> bool {
        self.recovery_required
    }
    pub fn persist(&mut self, receipt: Receipt) -> CommitOutcome {
        self.persist_with(receipt, &mut |_| Ok(()))
    }
    /// Reconcile complete checked frames after an uncertain write/sync. A torn
    /// or corrupt tail remains blocked; no silent truncation/receipt eviction.
    /// Caller must reconcile its visible scene BEFORE clearing its own block.
    pub fn reconcile(&mut self) -> Result<SyncScope> {
        self.recovery_required = true;
        self.validate_namespace()?;
        let (journal, bytes, chain) = read_log(&mut self.file)?;
        self.file.sync_all()?;
        let scope = self.sync_namespace()?;
        self.validate_namespace()?;
        self.journal = journal;
        self.bytes = bytes;
        self.chain = chain;
        self.recovery_required = false;
        Ok(scope)
    }
    fn persist_with(
        &mut self,
        receipt: Receipt,
        hook: &mut impl FnMut(Step) -> std::io::Result<()>,
    ) -> CommitOutcome {
        if self.recovery_required {
            return CommitOutcome::Indeterminate("Journal requires recovery".into());
        }
        if let Err(e) = self.validate_namespace() {
            self.recovery_required = true;
            return CommitOutcome::Indeterminate(e.to_string());
        }
        // Cooperative writers are excluded by the stable descriptor lock. Fresh
        // full prefix check also detects externally changed bytes/length.
        match read_log(&mut self.file) {
            Ok((journal, bytes, chain))
                if journal == self.journal && bytes == self.bytes && chain == self.chain => {}
            other => {
                self.recovery_required = true;
                return CommitOutcome::Indeterminate(format!(
                    "Journal prefix changed: {}",
                    other
                        .err()
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| "different state".into())
                ));
            }
        }
        let prepared = (|| -> Result<_> {
            let next = self.journal.stage_receipt(receipt.clone())?;
            let singleton = Journal::default().stage_receipt(receipt)?;
            let payload = singleton.encode()?;
            ensure!(payload.len() <= MAX_PAYLOAD, "Journal frame payload limit");
            let seq = (self.journal.len() + 1) as u32;
            let digest = frame_digest(&self.chain, seq, &payload);
            let mut frame = Vec::new();
            frame.try_reserve_exact(8 + payload.len() + 56)?;
            frame.extend_from_slice(&seq.to_be_bytes());
            frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            frame.extend_from_slice(&payload);
            frame.extend_from_slice(&digest);
            frame.extend_from_slice(COMMIT);
            let bytes = self
                .bytes
                .checked_add(frame.len() as u64)
                .context("Log length overflow")?;
            ensure!(
                bytes <= MAX_LOG_BYTES,
                "Journal log byte limit; history cannot be evicted"
            );
            Ok((next, frame, bytes, digest))
        })();
        let (next, frame, bytes, digest) = match prepared {
            Ok(v) => v,
            Err(e) => return CommitOutcome::Unchanged(e.to_string()),
        };
        if let Err(e) =
            hook(Step::BeforeWrite).and_then(|_| self.file.seek(SeekFrom::End(0)).map(|_| ()))
        {
            return CommitOutcome::Unchanged(e.to_string());
        }
        // All failures after the first attempted write are conservatively
        // indeterminate, even when an OS error may have written no bytes.
        self.recovery_required = true;
        let written = (|| -> Result<SyncScope> {
            let half = frame.len() / 2;
            self.file.write_all(&frame[..half])?;
            hook(Step::AfterPrefix)?;
            self.file.write_all(&frame[half..])?;
            hook(Step::AfterWrite)?;
            self.file.sync_all()?;
            hook(Step::AfterFileSync)?;
            let scope = self.sync_namespace()?;
            hook(Step::AfterNamespaceSync)?;
            self.validate_namespace()?;
            Ok(scope)
        })();
        match written {
            Ok(scope) => {
                self.journal = next;
                self.bytes = bytes;
                self.chain = digest;
                self.recovery_required = false;
                CommitOutcome::Committed(scope)
            }
            Err(e) => CommitOutcome::Indeterminate(e.to_string()),
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    BeforeWrite,
    AfterPrefix,
    AfterWrite,
    AfterFileSync,
    AfterNamespaceSync,
}
fn options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x80000000);
        options.share_mode(3); // READ|WRITE, no FILE_SHARE_DELETE while descriptor is owned /* FILE_FLAG_WRITE_THROUGH */
    }
    options
}
fn checked_location(path: &Path) -> Result<(PathBuf, PathBuf)> {
    let parent = path
        .parent()
        .context("Journal has no parent")?
        .canonicalize()?;
    let name = path.file_name().context("Journal has no filename")?;
    Ok((parent.clone(), parent.join(name)))
}
#[cfg(unix)]
fn validate_parent_namespace(handle: &File, path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let named = std::fs::symlink_metadata(path)?;
    let opened = handle.metadata()?;
    ensure!(
        named.is_dir()
            && !named.file_type().is_symlink()
            && opened.is_dir()
            && named.dev() == opened.dev()
            && named.ino() == opened.ino(),
        "Journal parent pathname no longer owns the retained directory"
    );
    Ok(())
}
fn validate_namespace(file: &File, path: &Path) -> Result<()> {
    let named = std::fs::symlink_metadata(path)?;
    let opened = file.metadata()?;
    ensure!(
        named.is_file() && !named.file_type().is_symlink() && opened.is_file(),
        "Journal must be a regular nonsymlink file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            named.dev() == opened.dev() && named.ino() == opened.ino() && opened.nlink() == 1,
            "Journal pathname no longer owns the locked file"
        );
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ensure!(
            named.file_attributes() & 0x400 == 0,
            "Journal reparse point rejected"
        );
        // OpenOptions excludes FILE_SHARE_DELETE to prevent leaf path replacement.
        // Trusted parent namespace remains a caller contract, not a hostile-user proof.
    }
    Ok(())
}
#[cfg(unix)]
fn sync_namespace(handle: &File, parent: &Path) -> Result<SyncScope> {
    validate_parent_namespace(handle, parent)?;
    handle.sync_all()?;
    validate_parent_namespace(handle, parent)?;
    Ok(SyncScope::FileAndUnixNamespace)
}
#[cfg(not(unix))]
fn sync_namespace(parent: &Path) -> Result<SyncScope> {
    let _ = parent;
    Ok(SyncScope::FileOnly)
}

fn seed() -> [u8; 48] {
    Sha384::digest(MAGIC).into()
}
fn frame_digest(previous: &[u8; 48], seq: u32, payload: &[u8]) -> [u8; 48] {
    let mut hash = Sha384::new();
    hash.update(MAGIC);
    hash.update(previous);
    hash.update(seq.to_be_bytes());
    hash.update((payload.len() as u32).to_be_bytes());
    hash.update(payload);
    hash.finalize().into()
}
/// Startup is O(bytes + N log N), not repeated cloning of the full journal.
/// Payload allocation is bounded by one record; decoded total <=8MiB logical.
fn read_log(file: &mut File) -> Result<(Journal, u64, [u8; 48])> {
    let bytes = file.metadata()?.len();
    ensure!(
        (8..=MAX_LOG_BYTES).contains(&bytes),
        "Invalid journal log size"
    );
    file.seek(SeekFrom::Start(0))?;
    let mut magic = [0; 8];
    file.read_exact(&mut magic)?;
    ensure!(&magic == MAGIC, "Wrong journal log version");
    let mut cursor = 8u64;
    let mut chain = seed();
    let mut journal = Journal::default();
    let mut logical = HEADER;
    while cursor < bytes {
        ensure!(
            journal.len() < MAX_RECORDS && bytes - cursor >= 64,
            "Incomplete or oversized journal log"
        );
        let mut header = [0; 8];
        file.read_exact(&mut header)?;
        let seq = u32::from_be_bytes(header[..4].try_into()?);
        let len = u32::from_be_bytes(header[4..].try_into()?) as usize;
        ensure!(
            seq as usize == journal.len() + 1
                && (HEADER + LEGACY_FIXED_RECORD + 2..=MAX_PAYLOAD).contains(&len),
            "Invalid journal frame sequence/length"
        );
        let end = cursor
            .checked_add(64 + len as u64)
            .context("Log cursor overflow")?;
        ensure!(end <= bytes, "Incomplete journal frame");
        let mut payload = Vec::new();
        payload.try_reserve_exact(len)?;
        ensure!(
            payload.capacity() <= MAX_PAYLOAD,
            "Journal payload allocation limit"
        );
        payload.resize(len, 0);
        file.read_exact(&mut payload)?;
        let mut digest = [0; 48];
        file.read_exact(&mut digest)?;
        let mut commit = [0; 8];
        file.read_exact(&mut commit)?;
        ensure!(
            &commit == COMMIT && digest == frame_digest(&chain, seq, &payload),
            "Journal checksum/commit marker mismatch"
        );
        let singleton = Journal::decode(&payload)?;
        ensure!(
            singleton.len() == 1,
            "Journal frame must contain exactly one receipt"
        );
        let (key, receipt) = singleton
            .records
            .into_iter()
            .next()
            .context("Missing journal receipt")?;
        ensure!(!journal.contains(&key), "Duplicate journal target");
        logical = logical
            .checked_add(len - HEADER)
            .context("Journal logical size overflow")?;
        ensure!(logical <= MAX_BYTES, "Journal logical byte limit");
        journal.records.insert(key, receipt);
        chain = digest;
        cursor = end;
    }
    ensure!(
        file.metadata()?.len() == bytes,
        "Journal size changed during read"
    );
    Ok((journal, bytes, chain))
}
#[cfg(test)]
mod tests {
    use super::*;
    fn receipt(n: u32) -> Receipt {
        Receipt {
            target: super::super::TargetKey::new(
                "FR",
                &format!("file:/S-102/DATASET_FILES/102{n:04}.H5"),
                &"01".repeat(48),
            )
            .unwrap(),
            original_catalogue_sha384: [2; 48],
            incoming_catalogue_sha384: [3; 48],
            incoming_namespace_sha384: [4; 48],
            issue_date: chrono::NaiveDate::from_ymd_opt(2026, 5, 27).unwrap(),
            original_issue_date: Some(chrono::NaiveDate::from_ymd_opt(2026, 5, 20).unwrap()),
            recorded_unix_seconds: 1,
        }
    }
    fn independent_process(path: &Path, mode: &str) {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "cancellation_journal::store::tests::independent_process_child",
                "--nocapture",
            ])
            .env("FERRITE_JOURNAL_TEST_PATH", path)
            .env("FERRITE_JOURNAL_TEST_MODE", mode)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    #[test]
    fn independent_process_child() {
        let Some(path) = std::env::var_os("FERRITE_JOURNAL_TEST_PATH") else {
            return;
        };
        let path = PathBuf::from(path);
        match std::env::var("FERRITE_JOURNAL_TEST_MODE").unwrap().as_str() {
            "locked" => assert!(Store::open(&path).is_err()),
            "reopen" => {
                let mut store = Store::open(&path).unwrap();
                assert_eq!(store.journal().unwrap().len(), 3);
                for n in 0..3 {
                    assert!(store.journal().unwrap().contains(receipt(n).target()));
                }
                assert!(matches!(
                    store.persist(receipt(1)),
                    CommitOutcome::Unchanged(_)
                ));
            }
            _ => panic!("Unknown child test mode"),
        }
    }
    #[test]
    fn legacy_frame_reopens_and_new_receipt_appends_without_rewriting_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.log");
        let store = Store::initialize(&path).unwrap();
        drop(store);
        let old = receipt(1);
        let mut payload = Journal::default()
            .stage_receipt(old.clone())
            .unwrap()
            .encode()
            .unwrap();
        payload[..8].copy_from_slice(super::super::LEGACY_MAGIC);
        let start = payload.len() - 19;
        payload.drain(start..start + 11);
        let digest = frame_digest(&seed(), 1, &payload);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&1u32.to_be_bytes()).unwrap();
        file.write_all(&(payload.len() as u32).to_be_bytes())
            .unwrap();
        file.write_all(&payload).unwrap();
        file.write_all(&digest).unwrap();
        file.write_all(COMMIT).unwrap();
        file.sync_all().unwrap();
        drop(file);
        let prefix = std::fs::read(&path).unwrap();
        let mut store = Store::open(&path).unwrap();
        assert_eq!(
            store
                .journal()
                .unwrap()
                .records
                .get(old.target())
                .unwrap()
                .original_issue_date(),
            None
        );
        assert!(matches!(
            store.persist(receipt(2)),
            CommitOutcome::Committed(_)
        ));
        drop(store);
        assert!(std::fs::read(&path).unwrap().starts_with(&prefix));
        let store = Store::open(&path).unwrap();
        assert_eq!(store.journal().unwrap().len(), 2);
        assert_eq!(
            store
                .journal()
                .unwrap()
                .records
                .get(receipt(2).target())
                .unwrap()
                .original_issue_date(),
            receipt(2).original_issue_date()
        );
    }
    #[test]
    fn actual_disk_reopen_duplicate_denial_and_exclusive_writer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.log");
        let mut store = Store::initialize(&path).unwrap();
        assert!(Store::open(&path).is_err());
        assert!(Store::initialize(&path).is_err());
        for n in 0..3 {
            assert!(matches!(
                store.persist(receipt(n)),
                CommitOutcome::Committed(_)
            ));
        }
        assert!(matches!(
            store.persist(receipt(1)),
            CommitOutcome::Unchanged(_)
        ));
        independent_process(&path, "locked");
        drop(store);
        independent_process(&path, "reopen");
        let store = Store::open(&path).unwrap();
        assert_eq!(store.journal().unwrap().len(), 3);
        for n in 0..3 {
            assert!(store.journal().unwrap().contains(receipt(n).target()));
        }
        assert!(Store::open(&dir.path().join("missing.log")).is_err());
    }
    #[test]
    fn before_write_failure_is_unchanged_but_complete_unsynced_frame_reconciles() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.log");
        let mut store = Store::initialize(&path).unwrap();
        let before = std::fs::read(&path).unwrap();
        let mut fault = |s| {
            if s == Step::BeforeWrite {
                Err(std::io::Error::other("before"))
            } else {
                Ok(())
            }
        };
        assert!(matches!(
            store.persist_with(receipt(0), &mut fault),
            CommitOutcome::Unchanged(_)
        ));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(store.journal().unwrap().is_empty());
        let mut fault = |s| {
            if s == Step::AfterWrite {
                Err(std::io::Error::other("before sync"))
            } else {
                Ok(())
            }
        };
        assert!(matches!(
            store.persist_with(receipt(0), &mut fault),
            CommitOutcome::Indeterminate(_)
        ));
        assert!(store.journal().is_err());
        assert!(matches!(
            store.persist(receipt(1)),
            CommitOutcome::Indeterminate(_)
        ));
        store.reconcile().unwrap();
        assert!(store.journal().unwrap().contains(receipt(0).target()));
        assert!(matches!(
            store.persist(receipt(0)),
            CommitOutcome::Unchanged(_)
        ));
    }
    #[test]
    fn partial_write_stays_blocked_without_truncation_or_old_state_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.log");
        let mut store = Store::initialize(&path).unwrap();
        assert!(matches!(
            store.persist(receipt(0)),
            CommitOutcome::Committed(_)
        ));
        let before = std::fs::metadata(&path).unwrap().len();
        let mut fault = |s| {
            if s == Step::AfterPrefix {
                Err(std::io::Error::other("partial"))
            } else {
                Ok(())
            }
        };
        assert!(matches!(
            store.persist_with(receipt(1), &mut fault),
            CommitOutcome::Indeterminate(_)
        ));
        let after = std::fs::metadata(&path).unwrap().len();
        assert!(after > before);
        assert!(store.reconcile().is_err());
        assert!(store.recovery_required());
        drop(store);
        assert!(Store::open(&path).is_err());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), after);
    }
    #[test]
    fn post_sync_failure_and_corruption_cannot_be_reported_as_rollback() {
        for step in [Step::AfterFileSync, Step::AfterNamespaceSync] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("journal.log");
            let mut store = Store::initialize(&path).unwrap();
            let mut fault = |s| {
                if s == step {
                    Err(std::io::Error::other("post-sync"))
                } else {
                    Ok(())
                }
            };
            assert!(matches!(
                store.persist_with(receipt(0), &mut fault),
                CommitOutcome::Indeterminate(_)
            ));
            drop(store);
            let store = Store::open(&path).unwrap();
            assert!(store.journal().unwrap().contains(receipt(0).target()));
            drop(store);
            let mut bytes = std::fs::read(&path).unwrap();
            bytes[30] ^= 1;
            std::fs::write(&path, bytes).unwrap();
            assert!(Store::open(&path).is_err());
        }
    }
    #[cfg(unix)]
    #[test]
    fn parent_replacement_after_file_sync_is_indeterminate_not_committed() {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("history");
        std::fs::create_dir(&parent).unwrap();
        let path = parent.join("journal.log");
        let mut store = Store::initialize(&path).unwrap();
        let moved = root.path().join("moved-history");
        let mut swap = |step| {
            if step == Step::AfterFileSync {
                std::fs::rename(&parent, &moved)?;
                std::fs::create_dir(&parent)?;
                std::fs::rename(moved.join("journal.log"), &path)?;
            }
            Ok(())
        };
        assert!(matches!(
            store.persist_with(receipt(0), &mut swap),
            CommitOutcome::Indeterminate(_)
        ));
        assert!(store.recovery_required());
        assert!(store.journal().is_err());
        assert!(store.reconcile().is_err());
        // File sync did complete; no truncation/reset is allowed on the uncertain
        // namespace result. A fresh store can acknowledge the complete frame.
        let bytes = std::fs::read(&path).unwrap();
        drop(store);
        let reopened = Store::open(&path).unwrap();
        assert!(reopened.journal().unwrap().contains(receipt(0).target()));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
    #[test]
    fn size_and_sequence_corruption_are_refused_before_payload_allocation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.log");
        let mut store = Store::initialize(&path).unwrap();
        assert!(matches!(
            store.persist(receipt(0)),
            CommitOutcome::Committed(_)
        ));
        drop(store);
        let original = std::fs::read(&path).unwrap();
        let mut bad = original.clone();
        bad[12..16].copy_from_slice(&u32::MAX.to_be_bytes());
        std::fs::write(&path, bad).unwrap();
        assert!(Store::open(&path).is_err());
        let mut bad = original;
        bad[8..12].copy_from_slice(&2u32.to_be_bytes());
        std::fs::write(&path, bad).unwrap();
        assert!(Store::open(&path).is_err());
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(MAX_LOG_BYTES + 1).unwrap();
        drop(file);
        assert!(Store::open(&path).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn symlink_and_replaced_path_block_orphaned_descriptor_publication() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.log");
        let moved = dir.path().join("moved.log");
        let mut store = Store::initialize(&path).unwrap();
        let length = std::fs::metadata(&path).unwrap().len();
        std::fs::rename(&path, &moved).unwrap();
        let replacement = Store::initialize(&path).unwrap();
        drop(replacement);
        assert!(store.journal().is_err());
        assert!(matches!(
            store.persist(receipt(0)),
            CommitOutcome::Indeterminate(_)
        ));
        assert!(store.reconcile().is_err());
        assert_eq!(std::fs::metadata(&moved).unwrap().len(), length);
        drop(store);
        let alias = dir.path().join("alias.log");
        std::os::unix::fs::symlink(&path, &alias).unwrap();
        assert!(Store::open(&alias).is_err());
        assert!(Store::initialize(&alias).is_err());
    }
}
