//! Pin cache identity to bytes actually parsed, independently of later live-path edits.
use ferrite_s100_core::CellSourceIdentity;
use std::hash::{Hash, Hasher};

#[derive(Default)]
pub(crate) struct LoadedSourceIdentities {
    by_cell: Vec<CellSourceIdentity>,
}
impl LoadedSourceIdentities {
    pub(crate) fn push(&mut self, identity: CellSourceIdentity) {
        self.by_cell.push(identity);
    }
    pub(crate) fn replace(&mut self, index: usize, identity: CellSourceIdentity) {
        *self
            .by_cell
            .get_mut(index)
            .expect("Cell/source identity indices stay aligned") = identity;
    }
    pub(crate) fn len(&self) -> usize {
        self.by_cell.len()
    }
    pub(crate) fn get(&self, index: usize) -> Option<CellSourceIdentity> {
        self.by_cell.get(index).copied()
    }
    pub(crate) fn truncate(&mut self, len: usize) {
        self.by_cell.truncate(len);
    }
    /// Move the aligned identity vector during cancellation compaction.
    pub(crate) fn take_all(&mut self) -> Vec<CellSourceIdentity> {
        std::mem::take(&mut self.by_cell)
    }
    pub(crate) fn clear(&mut self) {
        self.by_cell.clear();
    }
    pub(crate) fn hash_for_cell<H: Hasher>(&self, index: usize, hash: &mut H) -> Option<()> {
        self.by_cell.get(index)?.hash(hash);
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_s100_core::S101Cell;
    use sha2::{Digest, Sha256};
    use std::{
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };
    fn empty_ddr(marker: u8) -> Vec<u8> {
        let mut bytes = b"000253LE1 0000025 ! 1104".to_vec();
        assert_eq!(bytes.len(), 24);
        bytes[17] = marker;
        bytes.push(0x1e);
        bytes
    }
    struct Temp {
        dir: PathBuf,
    }
    impl Temp {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "ferrite-loaded-source-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&dir).unwrap();
            Self { dir }
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
    fn cache_component(ids: &LoadedSourceIdentities, index: usize) -> Option<u64> {
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        ids.hash_for_cell(index, &mut hash)?;
        Some(hash.finish())
    }
    #[test]
    fn retained_snapshot_digest_survives_live_path_change_and_new_load_gets_new_key() {
        let temp = Temp::new();
        let live = temp.dir.join("live.000");
        let snapshot = temp.dir.join("snapshot.000");
        let a = empty_ddr(b'A');
        let b = empty_ddr(b'B');
        std::fs::write(&live, &a).unwrap();
        std::fs::write(&snapshot, &a).unwrap();
        let (cell, identity) = S101Cell::load_from_with_identity(&live, &snapshot).unwrap();
        assert_eq!(cell.file_path, live);
        assert_eq!(identity.sha256().as_slice(), Sha256::digest(&a).as_slice());
        let mut old = LoadedSourceIdentities::default();
        old.push(identity);
        let old_key = cache_component(&old, 0).unwrap();
        // A later original-path edit must never label results from retained input A as B.
        std::fs::write(&live, &b).unwrap();
        assert_ne!(
            Sha256::digest(std::fs::read(&live).unwrap()).as_slice(),
            identity.sha256().as_slice()
        );
        assert_eq!(cache_component(&old, 0), Some(old_key));
        let (_, new_identity) = S101Cell::load_from_with_identity(&live, &live).unwrap();
        assert_eq!(
            new_identity.sha256().as_slice(),
            Sha256::digest(&b).as_slice()
        );
        let mut new = LoadedSourceIdentities::default();
        new.push(new_identity);
        assert_ne!(cache_component(&new, 0), Some(old_key));
        // Duplicate original paths must keep each separately parsed snapshot identity.
        old.push(new_identity);
        assert_eq!(cache_component(&old, 0), Some(old_key));
        assert_eq!(cache_component(&old, 1), cache_component(&new, 0));
        let mut replacement = LoadedSourceIdentities::default();
        replacement.push(identity);
        replacement.replace(0, new_identity);
        assert_eq!(cache_component(&replacement, 0), cache_component(&new, 0));
        assert_eq!(cache_component(&old, 0), Some(old_key));
        std::fs::remove_file(&live).unwrap();
        assert_eq!(
            cache_component(&old, 0),
            Some(old_key),
            "No live reopen during cache identity lookup"
        );
    }
    #[test]
    fn missing_or_cleared_loaded_identity_disables_cache_component() {
        let temp = Temp::new();
        let path = temp.dir.join("cell.000");
        std::fs::write(&path, empty_ddr(b'A')).unwrap();
        let (_, identity) = S101Cell::load_from_with_identity(&path, &path).unwrap();
        let mut ids = LoadedSourceIdentities::default();
        assert!(cache_component(&ids, 0).is_none());
        ids.push(identity);
        assert!(cache_component(&ids, 0).is_some());
        assert!(cache_component(&ids, 1).is_none());
        ids.clear();
        assert!(cache_component(&ids, 0).is_none());
    }
}
