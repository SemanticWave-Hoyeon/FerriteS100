//! Session RAM memo of raw parser output. No authentication or FC/PC authority.
use ferrite_s100_core::{CellSourceIdentity, S101Cell};
use std::{
    collections::VecDeque,
    mem::size_of,
    sync::{Arc, Mutex},
};
const BYTES: usize = 96 * 1024 * 1024;
const ENTRIES: usize = 32;
pub(crate) type SharedDecodedChartCache = Arc<Mutex<DecodedChartCache>>;
pub(crate) type DecodedChartCache = Cache<(S101Cell, CellSourceIdentity)>;
struct Entry<T> {
    key: [u8; 32],
    value: T,
    charge: usize,
}
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub(crate) struct Statistics {
    pub requests: u64,
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub retention_declines: u64,
    pub resets: u64,
    pub retained_entries: usize,
    pub charged_payload_bytes: usize,
    pub allocation_payload_limit_bytes: usize,
}
pub(crate) struct Cache<T> {
    entries: VecDeque<Entry<T>>,
    bytes: usize,
    generation: u64,
    limit: usize,
    exhausted: bool,
    statistics: Statistics,
}
impl<T: Clone> Cache<T> {
    pub(crate) fn new() -> Self {
        Self::with_limit(BYTES)
    }
    fn with_limit(limit: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(ENTRIES),
            bytes: 0,
            generation: 0,
            limit,
            exhausted: false,
            statistics: Statistics::default(),
        }
    }
    pub(crate) fn statistics(&self) -> Statistics {
        Statistics {
            retained_entries: self.entries.len(),
            charged_payload_bytes: self.bytes.saturating_add(
                self.entries
                    .capacity()
                    .saturating_mul(size_of::<Entry<T>>()),
            ),
            allocation_payload_limit_bytes: self.limit,
            ..self.statistics
        }
    }
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }
    pub(crate) fn clear(&mut self) {
        self.statistics.resets = self.statistics.resets.saturating_add(1);
        self.entries.clear();
        self.bytes = 0;
        if let Some(next) = self.generation.checked_add(1) {
            self.generation = next;
        } else {
            self.exhausted = true;
        }
    }
    pub(crate) fn get(&mut self, key: &[u8; 32]) -> Option<T> {
        self.statistics.requests = self.statistics.requests.saturating_add(1);
        let index = if self.exhausted {
            None
        } else {
            self.entries.iter().position(|e| &e.key == key)
        };
        let Some(index) = index else {
            self.statistics.misses = self.statistics.misses.saturating_add(1);
            return None;
        };
        self.statistics.hits = self.statistics.hits.saturating_add(1);
        let entry = self.entries.remove(index)?;
        let value = entry.value.clone();
        self.entries.push_back(entry);
        Some(value)
    }
    // The caller owns its live worker copy; only this independent immutable snapshot is retained.
    pub(crate) fn insert(
        &mut self,
        key: [u8; 32],
        value: T,
        charge: usize,
        generation: u64,
    ) -> bool {
        let overhead = self.entries.capacity().checked_mul(size_of::<Entry<T>>());
        let Some(available) = overhead.and_then(|n| self.limit.checked_sub(n)) else {
            return false;
        };
        if self.exhausted || generation != self.generation || charge > available {
            return false;
        }
        if let Some(i) = self.entries.iter().position(|e| e.key == key) {
            let e = self.entries.remove(i).expect("located entry");
            self.bytes -= e.charge;
        }
        while self.entries.len() >= ENTRIES || self.bytes > available - charge {
            let Some(e) = self.entries.pop_front() else {
                return false;
            };
            self.bytes -= e.charge;
            self.statistics.evictions = self.statistics.evictions.saturating_add(1);
        }
        self.bytes += charge;
        self.entries.push_back(Entry { key, value, charge });
        true
    }
}
impl Cache<(S101Cell, CellSourceIdentity)> {
    fn decline(&mut self) -> bool {
        self.statistics.retention_declines = self.statistics.retention_declines.saturating_add(1);
        false
    }

    pub(crate) fn remember(
        &mut self,
        key: [u8; 32],
        raw: &(S101Cell, CellSourceIdentity),
        generation: u64,
    ) -> bool {
        // Decline before cloning if the source itself already exceeds retention admission.
        let Some(before) = raw.0.retained_payload_upper_bound() else {
            return self.decline();
        };
        let Some(available) = self
            .entries
            .capacity()
            .checked_mul(size_of::<Entry<(S101Cell, CellSourceIdentity)>>())
            .and_then(|n| self.limit.checked_sub(n))
        else {
            return self.decline();
        };
        if self.exhausted || generation != self.generation || before > available {
            return self.decline();
        }
        // Release old cache snapshots before allocating the prospective retained copy.
        while self.bytes > available - before || self.entries.len() >= ENTRIES {
            let Some(entry) = self.entries.pop_front() else {
                return self.decline();
            };
            self.bytes -= entry.charge;
            self.statistics.evictions = self.statistics.evictions.saturating_add(1);
        }
        let snapshot = raw.clone();
        let Some(charge) = snapshot.0.retained_payload_upper_bound() else {
            return self.decline();
        };
        // Re-account actual clone capacities, not source file bytes or length-only estimates.
        if self.insert(key, snapshot, charge, generation) {
            true
        } else {
            self.decline()
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hit_is_owned_and_source_key_exact() {
        let mut c = Cache::with_limit(4096);
        let g = c.generation();
        assert!(c.insert([1; 32], vec![1, 2], 2, g));
        let mut hit = c.get(&[1; 32]).unwrap();
        hit[0] = 9;
        assert_eq!(c.get(&[1; 32]), Some(vec![1, 2]));
        assert!(c.get(&[2; 32]).is_none());
    }
    #[test]
    fn lru_budget_and_oversize_decline() {
        let overhead = ENTRIES * size_of::<Entry<Vec<u8>>>();
        let mut c = Cache::with_limit(overhead + 4);
        let g = c.generation();
        assert!(c.insert([1; 32], vec![1], 2, g));
        assert!(c.insert([2; 32], vec![2], 2, g));
        c.get(&[1; 32]);
        assert!(c.insert([3; 32], vec![3], 2, g));
        assert!(c.get(&[2; 32]).is_none());
        assert!(!c.insert([4; 32], vec![4], 5, g));
        assert_eq!(c.bytes, 4);
    }
    #[test]
    fn reset_blocks_inflight_publish() {
        let mut c = Cache::with_limit(4096);
        let g = c.generation();
        c.clear();
        assert!(!c.insert([1; 32], vec![1], 1, g));
        assert!(c.get(&[1; 32]).is_none());
    }
    #[test]
    fn entry_count_is_bounded() {
        let mut c = Cache::with_limit(65536);
        let g = c.generation();
        for i in 0..40 {
            assert!(c.insert([i; 32], vec![i], 1, g));
        }
        assert_eq!(c.entries.len(), 32);
        assert!(c.get(&[0; 32]).is_none());
    }
    #[test]
    fn shared_reset_is_serialized() {
        let c = Arc::new(Mutex::new(Cache::with_limit(4096)));
        let g = c.lock().unwrap().generation();
        let other = c.clone();
        std::thread::spawn(move || other.lock().unwrap().clear())
            .join()
            .unwrap();
        assert!(!c.lock().unwrap().insert([1; 32], vec![1], 1, g));
    }
    #[test]
    fn statistics_distinguish_lookup_and_retention() {
        let mut c = Cache::with_limit(4096);
        assert!(c.get(&[1; 32]).is_none());
        let g = c.generation();
        assert!(c.insert([1; 32], vec![1], 1, g));
        assert!(c.get(&[1; 32]).is_some());
        let s = c.statistics();
        assert_eq!(
            (s.requests, s.hits, s.misses, s.retained_entries),
            (2, 1, 1, 1)
        );
        assert!(s.charged_payload_bytes <= s.allocation_payload_limit_bytes);
        c.clear();
        assert_eq!(c.statistics().resets, 1);
        assert_eq!(c.statistics().retained_entries, 0);
    }
}
