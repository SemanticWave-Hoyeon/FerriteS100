//! Immutable procedural legacy-line INDEX data only; never visibility or geometry caching.
use crate::state::GpuState;
const CAP_BYTES: usize = 8 * 1024 * 1024;
fn counts(cap: usize) -> Option<(usize, usize)> {
    let quads = cap / (6 * std::mem::size_of::<u32>());
    if quads == 0 || quads.checked_mul(4)? > u32::MAX as usize {
        return None;
    }
    Some((quads, quads.checked_mul(6)?))
}
fn build(cap: usize) -> Option<Vec<u32>> {
    let (quads, count) = counts(cap)?;
    let mut indices = Vec::new();
    indices.try_reserve_exact(count).ok()?;
    if indices.capacity().checked_mul(std::mem::size_of::<u32>())? > cap {
        return None;
    }
    for q in 0..quads {
        let b = u32::try_from(q.checked_mul(4)?).ok()?;
        indices.extend([b, b + 1, b + 2, b, b + 2, b + 3]);
    }
    Some(indices)
}
fn requested(count: usize, certified: bool, capacity: usize) -> bool {
    certified && count != 0 && count.is_multiple_of(6) && count <= capacity
}
pub(crate) struct Cache {
    enabled: bool,
    buffer: Option<wgpu::Buffer>,
    capacity: usize,
    builds: u64,
    hits: u64,
    declines: u64,
    saved_upload_bytes: u64,
    initial_prepare_ns: u64,
}
impl Cache {
    pub(crate) fn new(flag: Option<&std::ffi::OsStr>) -> Self {
        Self {
            enabled: flag == Some(std::ffi::OsStr::new("1")),
            buffer: None,
            capacity: 0,
            builds: 0,
            hits: 0,
            declines: 0,
            saved_upload_bytes: 0,
            initial_prepare_ns: 0,
        }
    }
    /// Optional immutable resource creation occurs before Renderer publication, not mid-frame.
    pub(crate) async fn prepare(flag: Option<&std::ffi::OsStr>, state: &GpuState) -> Self {
        let mut cache = Self::new(flag);
        if !cache.enabled {
            return cache;
        }
        let started = std::time::Instant::now();
        let cap =
            CAP_BYTES.min(usize::try_from(state.device.limits().max_buffer_size).unwrap_or(0));
        if let Some(indices) = build(cap) {
            state
                .device
                .push_error_scope(wgpu::ErrorFilter::OutOfMemory);
            state.device.push_error_scope(wgpu::ErrorFilter::Validation);
            let buffer = state.create_index_buffer(&indices, "immutable-line-topology");
            let validation = state.device.pop_error_scope().await;
            let oom = state.device.pop_error_scope().await;
            if validation.is_none() && oom.is_none() {
                cache.capacity = indices.len();
                cache.buffer = Some(buffer);
                cache.builds = 1;
            } else {
                cache.declines = 1;
            }
        } else {
            cache.declines = 1;
        }
        cache.initial_prepare_ns = started.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        cache
    }
    pub(crate) fn get(&mut self, count: usize, certified: bool) -> Option<wgpu::Buffer> {
        if !self.enabled {
            return None;
        }
        if !requested(count, certified, self.capacity) || self.buffer.is_none() {
            self.declines = self.declines.saturating_add(1);
            return None;
        }
        self.hits = self.hits.saturating_add(1);
        self.saved_upload_bytes = self
            .saved_upload_bytes
            .saturating_add((count as u64).saturating_mul(4));
        self.buffer.clone()
    }
    pub(crate) fn statistics(&self) -> serde_json::Value {
        serde_json::json!({"enabled":self.enabled,"cap_bytes":CAP_BYTES,
            "retained_gpu_bytes":self.capacity*4,"builds":self.builds,"hits":self.hits,"initial_prepare_ns":self.initial_prepare_ns,"initial_upload_bytes":self.capacity*4,
            "declines":self.declines,"saved_repeated_upload_bytes":self.saved_upload_bytes,
            "scope":"normal-legacy-index-only; no CPU projection or private publication bypass"})
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn original_append_oracle_all_prefixes_ranges_and_shrink() {
        let pool = build(257 * 24).unwrap();
        let mut original = Vec::new();
        for q in 0..257u32 {
            let base = q * 4;
            // Independent original Geometry::append_emitted body.
            original.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
            assert_eq!(&pool[..original.len()], original);
            for start in [0, q as usize * 6] {
                assert_eq!(&pool[start..original.len()], &original[start..]);
            }
        }
        for count in [6, 12, 120, 6] {
            assert!(requested(count, true, pool.len()));
        }
    }
    #[test]
    fn checked_capacity_empty_foreign_and_nonquad_decline() {
        assert!(build(23).is_none());
        assert_eq!(build(24).unwrap(), [0, 1, 2, 0, 2, 3]);
        assert!(!requested(0, true, 60));
        assert!(!requested(6, false, 60));
        assert!(!requested(7, true, 60));
        assert!(!requested(66, true, 60));
        assert_eq!(counts(usize::MAX).is_none(), usize::BITS > 32);
        for cap in [24, 25, 255, 4096] {
            let v = build(cap).unwrap();
            assert!(v.capacity() * 4 <= cap);
            assert_eq!(v.len() % 6, 0);
        }
    }
    #[test]
    fn exact_policy_default_off_invalid_and_no_heap_state() {
        for flag in [None, Some("0".as_ref()), Some("invalid".as_ref())] {
            let c = Cache::new(flag);
            assert!(!c.enabled);
            assert!(c.buffer.is_none());
            assert_eq!(c.capacity, 0);
        }
        assert!(Cache::new(Some("1".as_ref())).enabled);
    }
}
