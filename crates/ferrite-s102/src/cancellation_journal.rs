//! Bounded, versioned S-102 historical cancellation receipts.
//! This codec grants no authority. Persistence, current policy, replay admission
//! before every import, replacement staging and atomic scene publication are
//! separate receiver duties; decoding a receipt cannot authorize removal.
use crate::discovery::{AuthenticatedS102OriginalMetadata, S102AuthorizedCancellation};
use anyhow::{ensure, Context, Result};
use chrono::NaiveDate;
use ferrite_security::OriginalDatasetAuthentication;
use std::{collections::BTreeMap, io::Read};

const MAGIC: &[u8; 8] = b"FS102CJ2";
const LEGACY_MAGIC: &[u8; 8] = b"FS102CJ1";
pub const MAX_RECORDS: usize = 4096;
pub const MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_PRODUCER: usize = 4;
const MAX_URI: usize = 4096;
const HEADER: usize = 12;
const LEGACY_FIXED_RECORD: usize = 4 + 4 + 48 * 4 + 10 + 8;
const FIXED_RECORD: usize = LEGACY_FIXED_RECORD + 11;

/// Strict S-102 3.0 original identity, independent of local filesystem paths,
/// packaging catalogue digests and notice identity. Repacking the same original
/// cannot bypass its tombstone. Profile is fixed by this versioned codec.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TargetKey {
    producer: String,
    uri: String,
    resource_sha384: [u8; 48],
}
impl TargetKey {
    pub fn from_authenticated_original(original: &OriginalDatasetAuthentication) -> Result<Self> {
        let metadata = AuthenticatedS102OriginalMetadata::from_authentication(original)?;
        Self::new(
            metadata.producer_code(),
            metadata.resource_uri(),
            metadata.resource_sha384(),
        )
    }
    fn new(producer: &str, uri: &str, digest: &str) -> Result<Self> {
        scalar(producer, MAX_PRODUCER)?;
        scalar(uri, MAX_URI)?;
        crate::discovery::validate_producer_code(producer)?;
        crate::discovery::validate_resource_uri(uri)?;
        Ok(Self {
            producer: producer.into(),
            uri: uri.into(),
            resource_sha384: digest_bytes(digest)?,
        })
    }
    pub fn producer(&self) -> &str {
        &self.producer
    }
    pub fn resource_uri(&self) -> &str {
        &self.uri
    }
}

/// Historical facts only: an intermediate authorized proof is not a current
/// commit capability. Caller must revalidate current owners/policy/time and
/// persist BEFORE an infallible complete scene publication.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Receipt {
    target: TargetKey,
    original_catalogue_sha384: [u8; 48],
    incoming_catalogue_sha384: [u8; 48],
    incoming_namespace_sha384: [u8; 48],
    issue_date: NaiveDate,
    original_issue_date: Option<NaiveDate>,
    recorded_unix_seconds: u64,
}
impl Receipt {
    pub fn from_authorized_notice(
        notice: &S102AuthorizedCancellation<'_, '_>,
        recorded_unix_seconds: u64,
    ) -> Result<Self> {
        let evidence = notice.evidence();
        // This first journal adapter cannot receipt a removal-with-replacement;
        // doing so requires a fully authenticated replacement publication capsule.
        ensure!(
            evidence.metadata().replacement_names().is_empty(),
            "Replacement must be staged in the same publication transaction"
        );
        let original = evidence.original().original();
        let metadata = AuthenticatedS102OriginalMetadata::from_authentication(original)?;
        Ok(Self {
            target: TargetKey::new(
                metadata.producer_code(),
                metadata.resource_uri(),
                metadata.resource_sha384(),
            )?,
            original_catalogue_sha384: digest_bytes(original.catalogue_sha384())?,
            incoming_catalogue_sha384: digest_bytes(
                evidence.original().incoming_catalogue().catalogue_sha384(),
            )?,
            incoming_namespace_sha384: digest_bytes(
                evidence.metadata().incoming_namespace_sha384(),
            )?,
            issue_date: evidence.metadata().issue_date(),
            original_issue_date: Some(metadata.issue_date()),
            recorded_unix_seconds,
        })
    }
    pub fn target(&self) -> &TargetKey {
        &self.target
    }
    pub fn issue_date(&self) -> NaiveDate {
        self.issue_date
    }
    /// None for legacy receipts; never substitute the notice date.
    pub fn original_issue_date(&self) -> Option<NaiveDate> {
        self.original_issue_date
    }
    fn encoded_len(&self) -> Result<usize> {
        FIXED_RECORD
            .checked_add(self.target.producer.len())
            .and_then(|n| n.checked_add(self.target.uri.len()))
            .context("Journal length overflow")
    }
}

/// No silent eviction. Staging leaves the old journal unchanged on conflict or
/// budget refusal. Bounds apply to logical serialized bytes, not allocator/RSS.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Journal {
    records: BTreeMap<TargetKey, Receipt>,
}
impl Journal {
    pub fn len(&self) -> usize {
        self.records.len()
    }
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
    pub fn contains(&self, target: &TargetKey) -> bool {
        self.records.contains_key(target)
    }
    /// Historical admission only; this never grants cancellation authority.
    /// S-100 17-4.4.1 compares name reuse against the cancelled ORIGINAL date.
    pub fn validate_reimport(&self, original: &OriginalDatasetAuthentication) -> Result<()> {
        let metadata = AuthenticatedS102OriginalMetadata::from_authentication(original)?;
        let target = TargetKey::new(
            metadata.producer_code(),
            metadata.resource_uri(),
            metadata.resource_sha384(),
        )?;
        self.validate_reimport_key(&target, metadata.issue_date())
    }
    fn validate_reimport_key(&self, target: &TargetKey, date: NaiveDate) -> Result<()> {
        ensure!(
            !self.contains(target),
            "Cancelled S102 original cannot be displayed or reused"
        );
        // Bounded by MAX_RECORDS; no temporary history clone or eviction.
        for receipt in self
            .records
            .values()
            .filter(|r| r.target.producer == target.producer && r.target.uri == target.uri)
        {
            let cancelled_date = receipt.original_issue_date.context("S102 legacy cancellation lacks the original issue date; name reuse needs verified history migration")?;
            ensure!(date > cancelled_date, "S102 name reuse must have an issue date later than the cancelled original (S-100 17-4.4.1)");
        }
        Ok(())
    }

    fn encoded_len(&self) -> Result<usize> {
        self.encoded_len_for_version(false)
    }
    fn encoded_len_for_version(&self, legacy: bool) -> Result<usize> {
        ensure!(self.len() <= MAX_RECORDS, "Journal record limit exceeded");
        let mut len = HEADER;
        for record in self.records.values() {
            len = len
                .checked_add(record.encoded_len()? - if legacy { 11 } else { 0 })
                .context("Journal length overflow")?;
            ensure!(len <= MAX_BYTES, "Journal byte limit exceeded");
        }
        Ok(len)
    }
    pub fn stage_receipt(&self, receipt: Receipt) -> Result<Self> {
        ensure!(
            !self.contains(receipt.target()),
            "Original already has a cancellation receipt"
        );
        ensure!(
            self.len() < MAX_RECORDS,
            "Journal record limit exceeded; history cannot be evicted"
        );
        let len = self
            .encoded_len()?
            .checked_add(receipt.encoded_len()?)
            .context("Journal length overflow")?;
        ensure!(
            len <= MAX_BYTES,
            "Journal byte limit exceeded; history cannot be evicted"
        );
        let mut next = self.clone();
        next.records.insert(receipt.target.clone(), receipt);
        Ok(next)
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        let len = self.encoded_len()?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(len)?;
        ensure!(
            bytes.capacity() <= MAX_BYTES,
            "Journal allocation limit exceeded"
        );
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&(self.len() as u32).to_be_bytes());
        for receipt in self.records.values() {
            for text in [&receipt.target.producer, &receipt.target.uri] {
                bytes.extend_from_slice(&(text.len() as u32).to_be_bytes());
                bytes.extend_from_slice(text.as_bytes());
            }
            for digest in [
                &receipt.target.resource_sha384,
                &receipt.original_catalogue_sha384,
                &receipt.incoming_catalogue_sha384,
                &receipt.incoming_namespace_sha384,
            ] {
                bytes.extend_from_slice(digest);
            }
            bytes.extend_from_slice(receipt.issue_date.format("%Y-%m-%d").to_string().as_bytes());
            match receipt.original_issue_date {
                Some(date) => {
                    bytes.push(1);
                    bytes.extend_from_slice(date.format("%Y-%m-%d").to_string().as_bytes());
                }
                None => bytes.extend_from_slice(&[0; 11]),
            }
            bytes.extend_from_slice(&receipt.recorded_unix_seconds.to_be_bytes());
        }
        ensure!(bytes.len() == len, "Journal encoded length mismatch");
        Ok(bytes)
    }
    /// Bounded reader even if metadata lies or a file grows while being read.
    /// NotFound/IO is not silently interpreted as empty history by this codec.
    pub fn read_from(mut reader: impl Read) -> Result<Self> {
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(MAX_BYTES)?;
        ensure!(
            bytes.capacity() <= MAX_BYTES,
            "Journal input allocation limit exceeded"
        );
        let mut chunk = [0u8; 8192];
        loop {
            let remaining = MAX_BYTES - bytes.len();
            if remaining == 0 {
                ensure!(
                    reader.read(&mut chunk[..1])? == 0,
                    "Journal byte limit exceeded"
                );
                break;
            }
            let count = reader.read(&mut chunk[..remaining.min(8192)])?;
            if count == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..count]);
        }
        Self::decode(&bytes)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        ensure!(bytes.len() <= MAX_BYTES, "Journal byte limit exceeded");
        let mut cursor = Cursor { bytes, position: 0 };
        let magic = cursor.take(8)?;
        let legacy = magic == LEGACY_MAGIC;
        ensure!(
            legacy || magic == MAGIC,
            "Unsupported journal format/version"
        );
        let fixed_record = if legacy {
            LEGACY_FIXED_RECORD
        } else {
            FIXED_RECORD
        };
        let count = cursor.u32()? as usize;
        ensure!(count <= MAX_RECORDS, "Journal record limit exceeded");
        ensure!(
            count <= bytes.len().saturating_sub(HEADER) / (fixed_record + 2),
            "Journal count exceeds available bytes"
        );
        let mut result = Self::default();
        for _ in 0..count {
            let producer = cursor.text(MAX_PRODUCER)?;
            let uri = cursor.text(MAX_URI)?;
            crate::discovery::validate_producer_code(&producer)?;
            crate::discovery::validate_resource_uri(&uri)?;
            let target = TargetKey {
                producer,
                uri,
                resource_sha384: cursor.array()?,
            };
            if let Some((previous, _)) = result.records.last_key_value() {
                ensure!(
                    previous < &target,
                    "Duplicate or noncanonical journal key order"
                );
            }
            let original_catalogue_sha384 = cursor.array()?;
            let incoming_catalogue_sha384 = cursor.array()?;
            let incoming_namespace_sha384 = cursor.array()?;
            let text = std::str::from_utf8(cursor.take(10)?)?;
            let issue_date = NaiveDate::parse_from_str(text, "%Y-%m-%d")?;
            ensure!(
                issue_date.format("%Y-%m-%d").to_string() == text,
                "Noncanonical journal date"
            );
            let original_issue_date = if legacy {
                None
            } else {
                let flag = cursor.take(1)?[0];
                let bytes = cursor.take(10)?;
                match flag {
                    0 => {
                        ensure!(bytes == [0; 10], "Noncanonical absent original issue date");
                        None
                    }
                    1 => {
                        let text = std::str::from_utf8(bytes)?;
                        let date = NaiveDate::parse_from_str(text, "%Y-%m-%d")?;
                        ensure!(
                            date.format("%Y-%m-%d").to_string() == text,
                            "Noncanonical original issue date"
                        );
                        Some(date)
                    }
                    _ => anyhow::bail!("Invalid original issue date presence flag"),
                }
            };
            let recorded_unix_seconds = u64::from_be_bytes(cursor.array()?);
            let receipt = Receipt {
                target: target.clone(),
                original_catalogue_sha384,
                incoming_catalogue_sha384,
                incoming_namespace_sha384,
                issue_date,
                original_issue_date,
                recorded_unix_seconds,
            };
            result.records.insert(target, receipt);
        }
        ensure!(cursor.position == bytes.len(), "Trailing journal bytes");
        ensure!(
            result.encoded_len_for_version(legacy)? == bytes.len(),
            "Journal length mismatch"
        );
        Ok(result)
    }
}
fn scalar(text: &str, max: usize) -> Result<()> {
    ensure!(
        !text.is_empty() && text.len() <= max && !text.chars().any(char::is_control),
        "Invalid journal scalar"
    );
    Ok(())
}
fn digest_bytes(text: &str) -> Result<[u8; 48]> {
    ensure!(
        text.len() == 96
            && text
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "Noncanonical SHA384 identity"
    );
    let mut digest = [0; 48];
    for (n, byte) in digest.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[n * 2..n * 2 + 2], 16)?;
    }
    Ok(digest)
}
struct Cursor<'a> {
    bytes: &'a [u8],
    position: usize,
}
impl<'a> Cursor<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .position
            .checked_add(len)
            .context("Journal cursor overflow")?;
        let value = self
            .bytes
            .get(self.position..end)
            .context("Truncated journal")?;
        self.position = end;
        Ok(value)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.take(N)?.try_into()?)
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.array()?))
    }
    fn text(&mut self, max: usize) -> Result<String> {
        let len = self.u32()? as usize;
        ensure!(len <= max, "Journal scalar length limit exceeded");
        let text = std::str::from_utf8(self.take(len)?)?;
        scalar(text, max)?;
        Ok(text.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn receipt(uri: &str, byte: u8) -> Receipt {
        Receipt {
            target: TargetKey::new("FR", uri, &format!("{byte:02x}").repeat(48)).unwrap(),
            original_catalogue_sha384: [1; 48],
            incoming_catalogue_sha384: [2; 48],
            incoming_namespace_sha384: [3; 48],
            issue_date: NaiveDate::from_ymd_opt(2026, 5, 26).unwrap(),
            original_issue_date: Some(NaiveDate::from_ymd_opt(2026, 5, 20).unwrap()),
            recorded_unix_seconds: 1,
        }
    }
    #[test]
    fn near_cap_legacy_history_reads_without_eviction_but_expansion_refuses() {
        let record_size = LEGACY_FIXED_RECORD + 2 + MAX_URI;
        let count = (MAX_BYTES - HEADER) / record_size;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(LEGACY_MAGIC);
        bytes.extend_from_slice(&(count as u32).to_be_bytes());
        for n in 0..count {
            let prefix = format!("file:/S-102/DATASET_FILES/{n:04}");
            let suffix = "/102AAAA.H5";
            let uri = format!(
                "{prefix}{}{suffix}",
                "X".repeat(MAX_URI - prefix.len() - suffix.len())
            );
            for text in ["FR", uri.as_str()] {
                bytes.extend_from_slice(&(text.len() as u32).to_be_bytes());
                bytes.extend_from_slice(text.as_bytes());
            }
            for digest in [[1; 48], [2; 48], [3; 48], [4; 48]] {
                bytes.extend_from_slice(&digest);
            }
            bytes.extend_from_slice(b"2026-05-26");
            bytes.extend_from_slice(&1u64.to_be_bytes());
        }
        assert!(bytes.len() <= MAX_BYTES && bytes.len() + count * 11 > MAX_BYTES);
        let original_bytes = bytes.clone();
        let legacy = Journal::decode(&bytes).unwrap();
        assert_eq!(legacy.len(), count);
        assert!(legacy.encode().is_err());
        assert_eq!(bytes, original_bytes);
        assert!(legacy
            .records
            .values()
            .all(|r| r.original_issue_date().is_none()));
    }
    #[test]
    fn name_reuse_compares_original_date_not_notice_date_and_never_same_digest() {
        let cancelled = receipt("file:/S-102/DATASET_FILES/102AAAA.H5", 1);
        let journal = Journal::default().stage_receipt(cancelled.clone()).unwrap();
        let later_key =
            TargetKey::new("FR", cancelled.target.resource_uri(), &"02".repeat(48)).unwrap();
        for day in [19, 20] {
            assert!(journal
                .validate_reimport_key(&later_key, NaiveDate::from_ymd_opt(2026, 5, day).unwrap())
                .is_err());
        }
        // New original is later than cancelled original, but BEFORE notice date.
        assert!(journal
            .validate_reimport_key(&later_key, NaiveDate::from_ymd_opt(2026, 5, 21).unwrap())
            .is_ok());
        assert!(journal
            .validate_reimport_key(
                cancelled.target(),
                NaiveDate::from_ymd_opt(2027, 1, 1).unwrap()
            )
            .is_err());
        let other_producer =
            TargetKey::new("GB", cancelled.target.resource_uri(), &"02".repeat(48)).unwrap();
        assert!(journal
            .validate_reimport_key(
                &other_producer,
                NaiveDate::from_ymd_opt(2025, 1, 1).unwrap()
            )
            .is_ok());
        let mut newer_cancelled = cancelled;
        newer_cancelled.target.resource_sha384 = [3; 48];
        newer_cancelled.original_issue_date = Some(NaiveDate::from_ymd_opt(2026, 5, 23).unwrap());
        let journal = journal.stage_receipt(newer_cancelled).unwrap();
        assert!(journal
            .validate_reimport_key(&later_key, NaiveDate::from_ymd_opt(2026, 5, 22).unwrap())
            .is_err());
        assert!(journal
            .validate_reimport_key(&later_key, NaiveDate::from_ymd_opt(2026, 5, 24).unwrap())
            .is_ok());
    }
    #[test]
    fn legacy_receipt_preserves_tombstone_without_manufacturing_original_date() {
        let r = receipt("file:/S-102/DATASET_FILES/102AAAA.H5", 1);
        let journal = Journal::default().stage_receipt(r.clone()).unwrap();
        let mut bytes = journal.encode().unwrap();
        bytes[..8].copy_from_slice(LEGACY_MAGIC);
        let start = bytes.len() - 19;
        bytes.drain(start..start + 11);
        let legacy = Journal::decode(&bytes).unwrap();
        assert!(legacy.contains(r.target()));
        assert_eq!(
            legacy
                .records
                .get(r.target())
                .unwrap()
                .original_issue_date(),
            None
        );
        let other = TargetKey::new("FR", r.target.resource_uri(), &"02".repeat(48)).unwrap();
        assert!(legacy
            .validate_reimport_key(&other, NaiveDate::from_ymd_opt(2099, 1, 1).unwrap())
            .unwrap_err()
            .to_string()
            .contains("migration"));
        let upgraded = legacy.encode().unwrap();
        assert_eq!(&upgraded[..8], MAGIC);
        assert_eq!(Journal::decode(&upgraded).unwrap(), legacy);
        let mut bad = upgraded.clone();
        let flag = bad.len() - 19;
        bad[flag] = 2;
        assert!(Journal::decode(&bad).is_err());
        bad = upgraded;
        bad[flag + 1] = 1;
        assert!(Journal::decode(&bad).is_err());
    }
    #[test]
    fn framed_keys_canonical_roundtrip_and_restart_replay() {
        let a = receipt("file:/S-102/DATASET_FILES/102AAAA.H5", 4);
        let b = receipt("file:/S-102/DATASET_FILES/102BBBB.H5", 5);
        let empty = Journal::default();
        let journal = empty
            .stage_receipt(b.clone())
            .unwrap()
            .stage_receipt(a.clone())
            .unwrap();
        assert!(empty.is_empty());
        let bytes = journal.encode().unwrap();
        let restarted = Journal::read_from(bytes.as_slice()).unwrap();
        assert_eq!(journal, restarted);
        assert!(restarted.contains(a.target()));
        assert!(restarted.stage_receipt(a).is_err());
        assert_eq!(bytes, restarted.encode().unwrap());
        // Catalogue/notice packaging changes are audit facts, never a new target.
        let mut repacked = b;
        repacked.original_catalogue_sha384 = [9; 48];
        repacked.incoming_namespace_sha384 = [8; 48];
        assert!(restarted.stage_receipt(repacked).is_err());
        assert_ne!(
            TargetKey::new(
                "F",
                "file:/S-102/DATASET_FILES/R/102AAAA.H5",
                &"00".repeat(48)
            )
            .unwrap(),
            TargetKey::new(
                "FR",
                "file:/S-102/DATASET_FILES/102AAAA.H5",
                &"00".repeat(48)
            )
            .unwrap()
        );
    }
    #[test]
    fn malformed_versions_counts_lengths_order_dates_and_tail_rejected() {
        let journal = Journal::default()
            .stage_receipt(receipt("file:/S-102/DATASET_FILES/102AAAA.H5", 1))
            .unwrap();
        let bytes = journal.encode().unwrap();
        for length in 0..bytes.len() {
            assert!(Journal::decode(&bytes[..length]).is_err(), "{length}");
        }
        let mut bad = bytes.clone();
        bad[7] = b'3';
        assert!(Journal::decode(&bad).is_err());
        let mut bad = bytes.clone();
        bad[8..12].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(Journal::decode(&bad).is_err());
        let mut bad = bytes.clone();
        bad[12..16].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(Journal::decode(&bad).is_err());
        let mut bad = bytes.clone();
        bad.push(0);
        assert!(Journal::decode(&bad).is_err());
        let mut bad = bytes.clone();
        let date = bad.len() - 29;
        bad[date..date + 10].copy_from_slice(b"2026-02-30");
        assert!(Journal::decode(&bad).is_err());
        let mut duplicate = bytes.clone();
        duplicate[8..12].copy_from_slice(&2u32.to_be_bytes());
        duplicate.extend_from_slice(&bytes[12..]);
        assert!(Journal::decode(&duplicate).is_err());
        assert!(TargetKey::new("FR", "file:/\0A.H5", &"00".repeat(48)).is_err());
        assert!(TargetKey::new(
            "FR",
            "file:/S-102/DATASET_FILES/102AAAA.H5",
            &"AB".repeat(48)
        )
        .is_err());
        assert!(Journal::read_from(std::io::repeat(0)).is_err());
    }
    #[test]
    fn record_and_byte_limits_refuse_without_mutating_or_evicting_history() {
        let mut journal = Journal::default();
        for n in 0..MAX_RECORDS {
            let r = receipt(&format!("file:/S-102/DATASET_FILES/102{n:04}.H5"), 1);
            journal.records.insert(r.target.clone(), r);
        }
        let bytes = journal.encode().unwrap();
        assert!(journal
            .stage_receipt(receipt("file:/S-102/DATASET_FILES/102EXTRA.H5", 1))
            .is_err());
        assert_eq!(bytes, journal.encode().unwrap());
        let mut large = Journal::default();
        for n in 0..1900 {
            let uri = format!(
                "file:/S-102/DATASET_FILES/{n:04}{}/102AAAA.H5",
                "X".repeat(4050)
            );
            let r = receipt(&uri, 1);
            large.records.insert(r.target.clone(), r);
        }
        // Byte budget limits before record-count budget. Existing encoded state fits.
        let before = large.encode().unwrap();
        let mut refused = false;
        for n in 1900..MAX_RECORDS {
            let r = receipt(
                &format!(
                    "file:/S-102/DATASET_FILES/{n:04}{}/102AAAA.H5",
                    "X".repeat(4050)
                ),
                1,
            );
            match large.stage_receipt(r) {
                Ok(next) => large = next,
                Err(_) => {
                    refused = true;
                    break;
                }
            }
        }
        assert!(refused);
        assert!(large.len() < MAX_RECORDS);
        assert!(large.encode().unwrap().len() <= MAX_BYTES);
        assert!(!before.is_empty());
    }
    #[test]
    fn malformed_utf8_lexical_keys_and_reader_errors_are_not_empty_history() {
        let bytes = Journal::default()
            .stage_receipt(receipt("file:/S-102/DATASET_FILES/102AAAA.H5", 1))
            .unwrap()
            .encode()
            .unwrap();
        let mut bad = bytes.clone();
        bad[16] = 255;
        assert!(Journal::decode(&bad).is_err());
        let mut bad = bytes.clone();
        bad[16] = b'f';
        assert!(Journal::decode(&bad).is_err());
        let mut bad = bytes.clone();
        let uri_start = 16 + 2 + 4;
        bad[uri_start] = b'x';
        assert!(Journal::decode(&bad).is_err());
        let a = receipt("file:/S-102/DATASET_FILES/102AAAA.H5", 1);
        let b = receipt("file:/S-102/DATASET_FILES/102BBBB.H5", 2);
        let first = Journal::default()
            .stage_receipt(a)
            .unwrap()
            .encode()
            .unwrap();
        let second = Journal::default()
            .stage_receipt(b)
            .unwrap()
            .encode()
            .unwrap();
        let mut reverse = second;
        reverse[8..12].copy_from_slice(&2u32.to_be_bytes());
        reverse.extend_from_slice(&first[12..]);
        assert!(Journal::decode(&reverse).is_err());
        struct FailedReader;
        impl Read for FailedReader {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("injected reader failure"))
            }
        }
        assert!(Journal::read_from(FailedReader).is_err());
    }
}

/// Exclusive append-only storage; no App removal or current authority supplied.
pub mod store;
