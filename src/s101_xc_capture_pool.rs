//! OFF-only immutable catalogue bytes; this pool confers no authenticity.
use anyhow::{ensure, Context, Result};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, Weak},
};
const RETAINED_XML_BUDGET: usize = 32 * 1024 * 1024;
const MAX_OWNERS: usize = 256;
#[derive(Default)]
struct Pool {
    entries: Vec<Entry>,
}
struct Entry {
    path: PathBuf,
    digest: [u8; 32],
    bytes: Weak<[u8]>,
}
impl Pool {
    fn retain(&mut self, path: &Path, bytes: &[u8], budget: usize) -> Result<Arc<[u8]>> {
        ensure!(
            path.is_absolute(),
            "XC captured catalogue key must be absolute"
        );
        self.entries.retain(|entry| entry.bytes.strong_count() != 0);
        let digest: [u8; 32] = Sha256::digest(bytes).into();
        let mut retained = 0usize;
        for entry in &self.entries {
            if let Some(owner) = entry.bytes.upgrade() {
                // Full equality is the correctness criterion, not digest alone.
                if entry.path == path && entry.digest == digest && owner.as_ref() == bytes {
                    return Ok(owner);
                }
                retained = retained
                    .checked_add(owner.len())
                    .context("XC retained bytes overflow")?;
            }
        }
        ensure!(
            self.entries.len() < MAX_OWNERS,
            "XC retained catalogue owner budget exceeded"
        );
        ensure!(
            bytes.len() <= budget.saturating_sub(retained),
            "XC retained catalogue XML budget exceeded"
        );
        let owner: Arc<[u8]> = Arc::from(bytes);
        self.entries.push(Entry {
            path: path.to_owned(),
            digest,
            bytes: Arc::downgrade(&owner),
        });
        Ok(owner)
    }
}
pub(crate) fn retain_unverified(path: &Path, bytes: &[u8]) -> Result<Arc<[u8]>> {
    static POOL: OnceLock<Mutex<Pool>> = OnceLock::new();
    POOL.get_or_init(|| Mutex::new(Pool::default()))
        .lock()
        .map_err(|_| anyhow::anyhow!("XC catalogue pool poisoned"))?
        .retain(path, bytes, RETAINED_XML_BUDGET)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_same_path_bytes_share_but_changed_or_foreign_bytes_do_not() {
        let mut p = Pool::default();
        let key = Path::new("/xc/a.xml");
        let a = p.retain(key, b"abc", 32).unwrap();
        let b = p.retain(key, b"abc", 32).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        let changed = p.retain(key, b"abd", 32).unwrap();
        assert!(!Arc::ptr_eq(&a, &changed));
        let foreign = p.retain(Path::new("/xc/b.xml"), b"abc", 32).unwrap();
        assert!(!Arc::ptr_eq(&a, &foreign));
    }
    #[test]
    fn joint_live_budget_checks_before_allocation_and_drop_reclaims_admission() {
        let mut p = Pool::default();
        let a = p.retain(Path::new("/a"), b"abcd", 4).unwrap();
        assert!(p.retain(Path::new("/b"), b"x", 4).is_err());
        assert!(Arc::ptr_eq(
            &a,
            &p.retain(Path::new("/a"), b"abcd", 4).unwrap()
        ));
        drop(a);
        assert!(p.retain(Path::new("/b"), b"abcd", 4).is_ok());
    }
}
