//! Admission accounting only; no authentication or cancellation decision.
use super::original_authentication::{OriginalDatasetAuthentication, ResourceAuthentication};
use anyhow::{ensure, Context, Result};
use std::{
    collections::{HashMap, HashSet},
    mem::size_of,
    sync::Arc,
};

// Local admission limits, not product-specification limits. Bound dedup scratch
// and traversal even when a caller supplies usize::MAX as its payload budget.
const MAX_ARC_OWNERS: usize = 16_384;
const MAX_SIGNATURE_ENTRIES: usize = 65_536;
const MAX_TRUST_ENTRIES: usize = 65_536;
const ARC_CONTROL_CHARGE: usize = 2 * size_of::<usize>();

/// Checked aggregate logical retained storage for referenced original proofs.
/// Each iterator record charges its inline Original and URI String capacity,
/// including clones; callers charge their registry containers/keys separately.
/// Only shared Arc allocations deduplicate, scoped to these LIVE borrowed owners.
/// Independently allocated identical bytes charge separately. No pointers escape.
///
/// Charges include Vec/String/PathBuf capacities, Arc payload/control words,
/// and a conservative capacity-based HashMap bucket/control model. This is a
/// logical admission charge, NOT allocator layout, peak RSS or total memory.
/// Arc slice payload has exactly its exposed length (no Vec spare capacity).
/// Scratch HashSets and allocation headers are outside retained payload charge;
/// their entry counts are bounded. Metadata caps can decline admission without
/// changing verification or allowing proof eviction from a displayed dataset.
/// Snapshot payload and caller old/new registry storage are separate budgets.
/// This function grants no new authority and never reopens a resource.
pub fn retained_original_authentication_storage<'a>(
    proofs: impl IntoIterator<Item = &'a OriginalDatasetAuthentication>,
    budget_bytes: usize,
    max_records: usize,
) -> Result<usize> {
    let mut charge = Charge::new(budget_bytes);
    let mut records = 0usize;
    for proof in proofs {
        records = records
            .checked_add(1)
            .context("Original record count overflow")?;
        ensure!(
            records <= max_records,
            "Original proof record admission cap exceeded"
        );
        charge.add(size_of::<OriginalDatasetAuthentication>())?;
        charge.add(proof.resource_uri.capacity())?;
        charge.resource(&proof.resource)?;
        charge.resource(&proof.catalogue)?;
        charge.bytes(&proof.catalogue_bytes)?;
        charge.trust(&proof.trust_anchor_sha256)?;
    }
    Ok(charge.total)
}

struct Charge {
    total: usize,
    budget: usize,
    signatures: usize,
    trust_entries: usize,
    resources: HashSet<*const ResourceAuthentication>,
    bytes: HashSet<*const [u8]>,
    trusts: HashSet<*const HashMap<String, String>>,
}
impl Charge {
    fn new(budget: usize) -> Self {
        Self {
            total: 0,
            budget,
            signatures: 0,
            trust_entries: 0,
            resources: HashSet::new(),
            bytes: HashSet::new(),
            trusts: HashSet::new(),
        }
    }
    fn add(&mut self, bytes: usize) -> Result<()> {
        let total = self
            .total
            .checked_add(bytes)
            .context("Original proof retained charge overflow")?;
        ensure!(
            total <= self.budget,
            "Original proof retained payload admission budget exceeded"
        );
        self.total = total;
        Ok(())
    }
    fn product(&mut self, count: usize, item: usize) -> Result<()> {
        self.add(
            count
                .checked_mul(item)
                .context("Original proof capacity charge overflow")?,
        )
    }
    fn owner(&self) -> Result<()> {
        let count = self
            .resources
            .len()
            .checked_add(self.bytes.len())
            .and_then(|v| v.checked_add(self.trusts.len()))
            .context("Original proof owner count overflow")?;
        ensure!(
            count < MAX_ARC_OWNERS,
            "Original proof unique Arc admission cap exceeded"
        );
        Ok(())
    }
    fn bytes(&mut self, bytes: &Arc<[u8]>) -> Result<()> {
        let key = Arc::as_ptr(bytes);
        if self.bytes.contains(&key) {
            return Ok(());
        }
        self.owner()?;
        self.bytes
            .try_reserve(1)
            .context("Original proof dedup scratch allocation failed")?;
        self.add(ARC_CONTROL_CHARGE)?;
        self.add(bytes.len())?;
        self.bytes.insert(key);
        Ok(())
    }
    fn resource(&mut self, resource: &Arc<ResourceAuthentication>) -> Result<()> {
        let key = Arc::as_ptr(resource);
        if self.resources.contains(&key) {
            return Ok(());
        }
        self.owner()?;
        self.resources
            .try_reserve(1)
            .context("Original proof dedup scratch allocation failed")?;
        self.add(ARC_CONTROL_CHARGE)?;
        self.add(size_of::<ResourceAuthentication>())?;
        self.add(resource.path.capacity())?;
        self.add(resource.sha384.capacity())?;
        self.signatures = self
            .signatures
            .checked_add(resource.signatures.len())
            .context("Original signature count overflow")?;
        ensure!(
            self.signatures <= MAX_SIGNATURE_ENTRIES,
            "Original signature metadata admission cap exceeded"
        );
        self.product(
            resource.signatures.capacity(),
            size_of::<super::VerifiedSignatureDescriptor>(),
        )?;
        // Reserve/register this live resource before traversing nested Arc owners,
        // so owner admission also includes its in-progress allocation. Errors
        // discard the complete accumulator; no partial charge is published.
        self.resources.insert(key);
        for s in &resource.signatures {
            self.add(s.id.capacity())?;
            self.add(s.certificate_id.capacity())?;
            self.add(s.der.capacity())?;
            self.add(s.signer_certificate_sha256.capacity())?;
            if let Some(target) = &s.signature_target {
                self.add(target.capacity())?;
            }
            self.bytes(&s.signer_certificate_der)?;
        }
        Ok(())
    }
    fn trust(&mut self, map: &Arc<HashMap<String, String>>) -> Result<()> {
        let key = Arc::as_ptr(map);
        if self.trusts.contains(&key) {
            return Ok(());
        }
        self.owner()?;
        self.trusts
            .try_reserve(1)
            .context("Original proof dedup scratch allocation failed")?;
        self.add(ARC_CONTROL_CHARGE)?;
        self.add(size_of::<HashMap<String, String>>())?;
        self.trust_entries = self
            .trust_entries
            .checked_add(map.len())
            .context("Original trust entry count overflow")?;
        ensure!(
            self.trust_entries <= MAX_TRUST_ENTRIES,
            "Original trust metadata admission cap exceeded"
        );
        // Round up a conservative power-of-two bucket model; do not describe
        // this model as Rust HashMap's guaranteed physical allocator layout.
        if map.capacity() > 0 {
            let buckets = map
                .capacity()
                .checked_add(1)
                .and_then(usize::checked_next_power_of_two)
                .context("Original trust capacity overflow")?;
            self.product(buckets, size_of::<(String, String)>())?;
            self.add(buckets)?;
            self.add(16)?;
        }
        for (name, digest) in map.iter() {
            self.add(name.capacity())?;
            self.add(digest.capacity())?;
        }
        self.trusts.insert(key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::VerifiedSignatureDescriptor;
    use super::*;
    use std::path::PathBuf;
    // Synthetic private structs test STORAGE only, never verified signatures.
    fn synthetic() -> OriginalDatasetAuthentication {
        let cert: Arc<[u8]> = Arc::from(&b"synthetic not a certificate"[..]);
        let descriptor = VerifiedSignatureDescriptor {
            id: "s".into(),
            certificate_id: "c".into(),
            der: vec![1, 2, 3],
            signer_certificate_sha256: "fixture only".into(),
            signer_certificate_der: cert,
            signature_target: Some("parent".into()),
        };
        let resource = Arc::new(ResourceAuthentication {
            path: PathBuf::from("fixture-resource"),
            sha384: "fixture digest".into(),
            size: 0,
            signatures: vec![descriptor],
        });
        OriginalDatasetAuthentication {
            resource: Arc::clone(&resource),
            catalogue: resource,
            catalogue_bytes: Arc::from(&b"fixture XML"[..]),
            verified_unix_seconds: 0,
            trust_anchor_sha256: Arc::new(HashMap::from([("fixture".into(), "digest".into())])),
            resource_uri: "fixture-resource".into(),
            discovery_range: 0..0,
        }
    }
    fn count(v: &[&OriginalDatasetAuthentication]) -> usize {
        retained_original_authentication_storage(v.iter().copied(), usize::MAX, 1024).unwrap()
    }
    #[test]
    fn clone_uri_is_charged_and_shared_live_arcs_deduplicate() {
        let a = synthetic();
        let b = a.clone();
        let one = count(&[&a]);
        let two = count(&[&a, &b]);
        assert_eq!(
            two - one,
            size_of::<OriginalDatasetAuthentication>() + b.resource_uri.capacity()
        );
        // Repeated references conservatively charge each declared record.
        assert_eq!(
            count(&[&a, &a]) - one,
            size_of::<OriginalDatasetAuthentication>() + a.resource_uri.capacity()
        );
    }
    #[test]
    fn independently_owned_identical_payloads_are_not_content_deduplicated() {
        let a = synthetic();
        let b = synthetic();
        assert_eq!(count(&[&a, &b]), count(&[&a]) + count(&[&b]));
        let mut c = Charge::new(usize::MAX);
        let x: Arc<[u8]> = Arc::from(&b"same"[..]);
        c.bytes(&x).unwrap();
        let first = c.total;
        c.bytes(&Arc::clone(&x)).unwrap();
        assert_eq!(c.total, first);
        c.bytes(&Arc::from(&b"same"[..])).unwrap();
        assert_eq!(c.total, 2 * first);
    }
    #[test]
    fn spare_capacities_and_exact_payload_record_limits_are_charged() {
        let a = synthetic();
        let mut b = synthetic();
        b.resource_uri.reserve(512);
        assert_eq!(
            count(&[&b]) - count(&[&a]),
            b.resource_uri.capacity() - a.resource_uri.capacity()
        );
        let total = count(&[&a]);
        assert_eq!(
            retained_original_authentication_storage([&a], total, 1).unwrap(),
            total
        );
        assert!(retained_original_authentication_storage([&a], total - 1, 1).is_err());
        assert!(retained_original_authentication_storage([&a], total, 0).is_err());
        assert_eq!(
            retained_original_authentication_storage(std::iter::empty(), 0, 0).unwrap(),
            0
        );
        let refs = vec![&a; 1024];
        assert!(retained_original_authentication_storage(
            refs.iter().copied(),
            128 * 1024 * 1024,
            1024
        )
        .is_ok());
        assert!(
            retained_original_authentication_storage(refs.iter().copied(), usize::MAX, 1023)
                .is_err()
        );
    }
    #[test]
    fn checked_arithmetic_and_bounded_metadata_decline_without_authority() {
        let mut c = Charge::new(usize::MAX);
        c.add(usize::MAX).unwrap();
        assert!(c.add(1).is_err());
        let mut c = Charge::new(usize::MAX);
        assert!(c.product(usize::MAX, 2).is_err());
        let a = synthetic();
        let mut c = Charge::new(usize::MAX);
        c.signatures = MAX_SIGNATURE_ENTRIES;
        assert!(c.resource(&a.resource).is_err());
        let mut c = Charge::new(usize::MAX);
        c.trust_entries = MAX_TRUST_ENTRIES;
        assert!(c.trust(&a.trust_anchor_sha256).is_err());
    }
}
