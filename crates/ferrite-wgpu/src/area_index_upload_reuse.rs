//! Exact ordered index bytes only. Never caches authority, geometry or coverage.
const CAP: usize = 8 * 1024 * 1024;
const CONTROL: usize = 1024;
#[derive(Default, Debug, serde::Serialize)]
pub struct Work {
    pub attempts: u64,
    pub hits: u64,
    pub misses: u64,
    pub upload_calls: u64,
    pub upload_bytes: u64,
    pub saved_upload_bytes: u64,
    pub cap_declines: u64,
    pub allocation_declines: u64,
    pub invalidations: u64,
    pub metadata_capacity_bytes: u64,
}
pub(crate) struct Cache<K> {
    enabled: bool,
    metadata: Option<(K, Vec<u32>)>,
    work: Work,
}
enum Decline {
    Cap,
    Allocation,
}
fn snapshot_with(
    indices: &[u32],
    cap: usize,
    allocate: impl FnOnce(usize) -> Option<Vec<u32>>,
) -> Result<Vec<u32>, Decline> {
    if indices.is_empty()
        || indices
            .len()
            .checked_mul(4)
            .and_then(|n| n.checked_add(CONTROL))
            .is_none_or(|n| n > cap)
    {
        return Err(Decline::Cap);
    }
    let mut owned = allocate(indices.len()).ok_or(Decline::Allocation)?;
    if !owned.is_empty()
        || owned.capacity() < indices.len()
        || owned
            .capacity()
            .checked_mul(4)
            .and_then(|n| n.checked_add(CONTROL))
            .is_none_or(|n| n > cap)
    {
        return Err(Decline::Cap);
    }
    owned.extend_from_slice(indices); // Reserved above; cannot allocate here.
    Ok(owned)
}
impl<K: PartialEq> Cache<K> {
    pub(crate) fn new(value: Option<&std::ffi::OsStr>) -> Self {
        Self {
            enabled: value == Some(std::ffi::OsStr::new("1")),
            metadata: None,
            work: Work::default(),
        }
    }
    pub(crate) fn work(&self) -> &Work {
        &self.work
    }
    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }
    pub(crate) fn max_metadata_bytes(&self) -> usize {
        CAP
    }
    pub(crate) fn invalidate(&mut self) {
        if self.metadata.take().is_some() {
            self.work.invalidations = self.work.invalidations.saturating_add(1);
        }
        self.work.metadata_capacity_bytes = 0;
    }
    pub(crate) fn reuse(
        &mut self,
        key: &K,
        indices: &[u32],
        buffer_present: bool,
        count: u32,
    ) -> bool {
        if !self.enabled {
            return false;
        }
        self.work.attempts = self.work.attempts.saturating_add(1);
        let hit = !indices.is_empty()
            && buffer_present
            && indices.len() == count as usize
            && self
                .metadata
                .as_ref()
                .is_some_and(|(old, words)| old == key && words.as_slice() == indices);
        if hit {
            self.work.hits = self.work.hits.saturating_add(1);
            self.work.saved_upload_bytes = self
                .work
                .saved_upload_bytes
                .saturating_add((indices.len() as u64).saturating_mul(4));
        } else {
            self.work.misses = self.work.misses.saturating_add(1);
        }
        hit
    }
    /// Called only AFTER the original GPU upload is issued. No extra GPU object.
    pub(crate) fn uploaded(&mut self, key: K, indices: &[u32]) {
        self.invalidate(); // Retire previous metadata BEFORE reserving another snapshot.
        if !self.enabled || indices.is_empty() {
            return;
        }
        self.work.upload_calls = self.work.upload_calls.saturating_add(1);
        self.work.upload_bytes = self
            .work
            .upload_bytes
            .saturating_add((indices.len() as u64).saturating_mul(4));
        match snapshot_with(indices, CAP, |n| {
            let mut words = Vec::new();
            words.try_reserve_exact(n).ok()?;
            Some(words)
        }) {
            Ok(words) => {
                self.work.metadata_capacity_bytes = (words.capacity() * 4) as u64;
                self.metadata = Some((key, words));
            }
            Err(Decline::Cap) => self.work.cap_declines = self.work.cap_declines.saturating_add(1),
            Err(Decline::Allocation) => {
                self.work.allocation_declines = self.work.allocation_declines.saturating_add(1)
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn cache() -> Cache<u64> {
        Cache::new(Some(std::ffi::OsStr::new("1")))
    }
    #[test]
    fn exact_order_count_and_every_u32_bit() {
        let mut c = cache();
        let a = [0, 1, u32::MAX, 0x80000000];
        assert!(!c.reuse(&1, &a, true, 4));
        c.uploaded(1, &a);
        assert!(c.reuse(&1, &a, true, 4));
        for b in [[0, 1, u32::MAX, 0x80000001], [1, 0, u32::MAX, 0x80000000]] {
            assert!(!c.reuse(&1, &b, true, 4));
        }
        assert!(!c.reuse(&1, &a[..3], true, 3));
        assert!(!c.reuse(&1, &a, true, 3));
        assert!(!c.reuse(&1, &a, false, 4));
        assert!(!c.reuse(&2, &a, true, 4));
        assert_eq!(c.work().saved_upload_bytes, 16);
    }
    #[test]
    fn replacement_clear_and_empty_never_reuse() {
        let mut c = cache();
        c.uploaded(1, &[2, 3]);
        c.invalidate();
        assert!(!c.reuse(&1, &[2, 3], true, 2));
        c.uploaded(1, &[2, 3]);
        c.uploaded(1, &[2, 4]);
        assert!(!c.reuse(&1, &[2, 3], true, 2));
        assert!(c.reuse(&1, &[2, 4], true, 2));
        c.uploaded(1, &[]);
        assert!(!c.reuse(&1, &[], true, 0));
        assert!(!c.reuse(&1, &[2, 4], true, 2));
    }
    #[test]
    fn actual_capacity_overflow_and_reservation_failure() {
        assert!(matches!(
            snapshot_with(&[1, 2], CONTROL + 8, |_| None),
            Err(Decline::Allocation)
        ));
        assert!(matches!(
            snapshot_with(&[1, 2], CONTROL + 8, |_| Some(Vec::with_capacity(8))),
            Err(Decline::Cap)
        ));
        let mut called = false;
        assert!(matches!(
            snapshot_with(&[1, 2], CONTROL + 4, |_| {
                called = true;
                Some(Vec::new())
            }),
            Err(Decline::Cap)
        ));
        assert!(!called);
        assert!(snapshot_with(&[1, 2], CONTROL + 8, |n| Some(Vec::with_capacity(n))).is_ok());
    }
    #[test]
    fn off_is_not_a_permission_cache() {
        for value in [
            None,
            Some(std::ffi::OsStr::new("true")),
            Some(std::ffi::OsStr::new("0")),
        ] {
            let mut c: Cache<u64> = Cache::new(value);
            c.uploaded(1, &[5]);
            assert!(!c.reuse(&1, &[5], true, 1));
            assert!(c.metadata.is_none());
            assert_eq!(c.work().attempts, 0);
            assert_eq!(c.work().upload_bytes, 0);
        }
    }
}
