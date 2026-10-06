//! Exact immutable payload reuse, never overwrite a retained/live GPU resource.
//! Logical retained payload/handle budget, not a total RSS/device-memory claim.
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

struct Entry<T> { bytes: Box<[u8]>, resource: T }
pub(crate) struct ImmutablePayloadCache<T> {
    buckets: HashMap<u64, Vec<Entry<T>>>,
    retained_bytes: usize,
    entries: usize,
    pub requests: u64,
    pub hits: u64,
    pub creations: u64,
    pub rejected: u64,
    pub resets: u64,
}
impl<T: Clone> ImmutablePayloadCache<T> {
    pub const BUDGET: usize=16*1024*1024;
    pub const MAX_ENTRIES: usize=128;
    pub const MAX_PAYLOAD: usize=8*1024*1024;
    // Conservatively charge a hash bucket/Vec/key/entry logical metadata allowance.
    // Actual allocator/GPU residency and externally cloned handles are not bounded here.
    const METADATA: usize=128+std::mem::size_of::<Entry<T>>();
    pub fn new()->Self {Self {buckets:HashMap::new(),retained_bytes:0,entries:0,requests:0,hits:0,creations:0,rejected:0,resets:0}}
    pub fn retained_bytes(&self)->usize {self.retained_bytes}
    pub fn entries(&self)->usize {self.entries}
    pub fn record_uncached_creation(&mut self) {
        self.requests=self.requests.saturating_add(1);self.creations=self.creations.saturating_add(1);self.rejected=self.rejected.saturating_add(1);
    }
    pub fn reuse_or_create(&mut self,bytes:&[u8],create:impl FnOnce()->T)->T {
        let mut hash=std::collections::hash_map::DefaultHasher::new();bytes.hash(&mut hash);
        self.with_hash(hash.finish(),bytes,create)
    }
    fn with_hash(&mut self,hash:u64,bytes:&[u8],create:impl FnOnce()->T)->T {
        self.requests=self.requests.saturating_add(1);
        if let Some(bucket)=self.buckets.get(&hash) {
            if let Some(entry)=bucket.iter().find(|e|e.bytes.as_ref()==bytes) {
                self.hits=self.hits.saturating_add(1);return entry.resource.clone();
            }
        }
        self.creations=self.creations.saturating_add(1);
        let resource=create();
        let charge=bytes.len().checked_mul(2).and_then(|n|n.checked_add(Self::METADATA));
        if bytes.is_empty() || bytes.len()>Self::MAX_PAYLOAD || charge.is_none_or(|n|n>Self::BUDGET) {
            self.rejected=self.rejected.saturating_add(1);return resource;
        }
        let charge=charge.unwrap();
        if self.entries==Self::MAX_ENTRIES || self.retained_bytes>Self::BUDGET-charge {
            // Eviction drops only cache-owned handles. Any live batch/publication
            // retains its own immutable handle, and no recycled buffer is written.
            self.buckets.clear();self.entries=0;self.retained_bytes=0;
            self.resets=self.resets.saturating_add(1);
        }
        self.buckets.entry(hash).or_default().push(Entry {bytes:bytes.into(),resource:resource.clone()});
        self.entries+=1;self.retained_bytes+=charge;resource
    }
}
#[cfg(test)]
mod tests {
    use super::*;use std::sync::Arc;
    #[test]
    fn exact_hit_returns_same_immutable_owner_without_creation() {
        let mut c=ImmutablePayloadCache::new();let old=c.reuse_or_create(b"original",||Arc::new(4));
        let new=c.reuse_or_create(b"original",||panic!("hit allocated"));
        assert!(Arc::ptr_eq(&old,&new));assert_eq!((c.requests,c.hits,c.creations),(2,1,1));
    }
    #[test]
    fn hash_collision_never_returns_different_payload_or_mutates_old_owner() {
        let mut c=ImmutablePayloadCache::new();let old=c.with_hash(0,b"original",||Arc::new(4));
        let new=c.with_hash(0,b"modified",||Arc::new(9));assert!(!Arc::ptr_eq(&old,&new));
        assert_eq!(*old,4);assert_eq!(*new,9);assert_eq!(c.entries(),2);
    }
    #[test]
    fn eviction_keeps_live_alias_and_logical_bounds() {
        let mut c=ImmutablePayloadCache::new();let old=c.reuse_or_create(b"original",||Arc::new(4));
        for i in 0..ImmutablePayloadCache::<Arc<i32>>::MAX_ENTRIES {c.reuse_or_create(&i.to_le_bytes(),||Arc::new(9));}
        assert_eq!(*old,4);assert_eq!(c.resets,1);assert!(c.entries()<=ImmutablePayloadCache::<Arc<i32>>::MAX_ENTRIES);
        assert!(c.retained_bytes()<=ImmutablePayloadCache::<Arc<i32>>::BUDGET);
    }
    #[test]
    fn empty_and_oversized_payloads_do_not_enter_retained_cache() {
        let mut c=ImmutablePayloadCache::new();c.reuse_or_create(b"",||Arc::new(1));
        let big=vec![0;ImmutablePayloadCache::<Arc<i32>>::MAX_PAYLOAD+1];c.reuse_or_create(&big,||Arc::new(2));
        assert_eq!((c.entries(),c.retained_bytes(),c.rejected),(0,0,2));
    }
}
