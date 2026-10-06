//! S-101 2.0 Annex B-7 cancellation is a physical DSID-only dataset.
//! Authentication and atomic publication remain caller responsibilities.
use anyhow::{ensure, Context, Result};
use chrono::NaiveDate;
use ferrite_iso8211::{Leader, DDR, DR, FIELD_TERMINATOR};
use ferrite_s100_core::DatasetIdentification;

/// Receiver budget for the small cancellation file; not an IHO schema limit.
pub const MAX_CANCELLATION_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cancellation {
    dataset_name: String,
    dataset_title: String,
    reference_date: NaiveDate,
    counter: u16,
}

/// Catalogue edition is the positive edition being cancelled, while the
/// physical file's DSED must be exactly "0". This type makes no auth claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancellationMetadata {
    pub edition: u16,
    pub update: u16,
    pub issue_date: NaiveDate,
}
impl CancellationMetadata {
    pub fn new(edition: u32, update: Option<u32>, issue_date: &str) -> Result<Self> {
        let edition = u16::try_from(edition).context("Cancellation target edition overflow")?;
        ensure!(
            edition > 0,
            "Cancellation catalogue target edition must be positive"
        );
        let update =
            u16::try_from(update.context("S-101 cancellation requires catalogue updateNumber")?)?;
        ensure!(update <= 999, "Cancellation catalogue update exceeds 999");
        Ok(Self {
            edition,
            update,
            issue_date: date(issue_date, "%Y-%m-%d")?,
        })
    }
}

fn date(value: &str, format: &str) -> Result<NaiveDate> {
    ensure!(
        value.len() == if format == "%Y%m%d" { 8 } else { 10 },
        "Cancellation date has wrong fixed width"
    );
    let parsed = NaiveDate::parse_from_str(value, format).context("Invalid cancellation date")?;
    ensure!(
        parsed.format(format).to_string() == value,
        "Noncanonical cancellation date"
    );
    Ok(parsed)
}
fn name(value: &str) -> Result<(&str, u16)> {
    let (stem, extension) = value
        .rsplit_once('.')
        .context("Cancellation needs filename extension")?;
    ensure!(
        (8..=17).contains(&stem.len())
            && stem.starts_with("101")
            && stem
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
            && extension.len() == 3
            && extension.bytes().all(|b| b.is_ascii_digit()),
        "Invalid S-101 cancellation filename"
    );
    // Registration of the producer code is a separate registry validation.
    Ok((stem, extension.parse()?))
}
fn record(bytes: &[u8]) -> Result<(&[u8], &[u8], Leader)> {
    let leader = Leader::parse(bytes)?;
    let length = leader.record_length as usize;
    let base = leader.base_address as usize;
    ensure!(
        leader.interchange_level == b'3'
            && length >= 25
            && length <= bytes.len()
            && (25..=length).contains(&base),
        "Malformed cancellation record extent"
    );
    let data = &bytes[..length];
    ensure!(
        data[base - 1] == FIELD_TERMINATOR && leader.directory_entry_size() > 0,
        "Malformed cancellation directory"
    );
    let directory = &data[Leader::SIZE..base - 1];
    ensure!(
        leader.size_field_tag == 4
            && leader.size_field_length > 0
            && leader.size_field_position > 0
            && directory.len() % leader.directory_entry_size() == 0
            && !directory.contains(&FIELD_TERMINATOR),
        "Cancellation directory has invalid widths or early terminator"
    );
    for entry in directory.chunks_exact(leader.directory_entry_size()) {
        ensure!(
            entry[4..].iter().all(u8::is_ascii_digit),
            "Noncanonical cancellation directory number"
        );
    }
    let parsed = ferrite_iso8211::Directory::parse(&data[Leader::SIZE..base], &leader)?;
    let mut spans = parsed
        .entries
        .iter()
        .map(|entry| {
            let start = entry.position as usize;
            let end = start
                .checked_add(entry.length as usize)
                .context("Cancellation field extent overflow")?;
            ensure!(
                entry.length > 0 && end <= length - base,
                "Cancellation field outside record"
            );
            ensure!(
                data[base + end - 1] == FIELD_TERMINATOR,
                "Cancellation field lacks terminator"
            );
            Ok((start, end))
        })
        .collect::<Result<Vec<_>>>()?;
    spans.sort_unstable();
    let mut expected = 0;
    for (start, end) in spans {
        ensure!(
            start == expected,
            "Cancellation fields overlap or leave a gap"
        );
        expected = end;
    }
    ensure!(
        expected == length - base,
        "Cancellation record has unreferenced payload"
    );
    Ok((data, &bytes[length..], leader))
}
impl Cancellation {
    pub fn dataset_name(&self) -> &str {
        &self.dataset_name
    }
    pub fn dataset_title(&self) -> &str {
        &self.dataset_title
    }
    pub fn reference_date(&self) -> NaiveDate {
        self.reference_date
    }
    pub fn counter(&self) -> u16 {
        self.counter
    }
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() <= MAX_CANCELLATION_BYTES,
            "Cancellation receiver byte budget exceeded"
        );
        let (description, remaining, ddr) = record(bytes)?;
        ensure!(ddr.is_ddr(), "Cancellation requires physical ISO8211 DDR");
        ensure!(
            DDR::parse(description)?
                .field_definitions
                .iter()
                .filter(|f| f.tag == "DSID")
                .count()
                == 1,
            "Cancellation DDR must describe DSID"
        );
        let (body, remaining, leader) = record(remaining)?;
        ensure!(
            leader.is_dr() && remaining.is_empty(),
            "Cancellation requires exactly one DSID data record"
        );
        let dr = DR::parse(body)?;
        ensure!(
            dr.fields.len() == 1 && dr.fields[0].tag == "DSID",
            "Cancellation contains non-DSID fields"
        );
        let raw = &dr.fields[0].data;
        ensure!(
            raw.last() == Some(&FIELD_TERMINATOR),
            "Cancellation DSID lacks field terminator"
        );
        let data = &raw[..raw.len() - 1];
        ensure!(
            data.get(..5) == Some(&[10, 1, 0, 0, 0][..]),
            "Cancellation DSID must be RCNM10 RCID1"
        );
        let mut offset = 5;
        let mut string = || -> Result<&str> {
            let tail = data.get(offset..).context("Truncated cancellation DSID")?;
            let n = tail
                .iter()
                .position(|b| *b == 0x1f)
                .context("Unterminated cancellation DSID string")?;
            let value = std::str::from_utf8(&tail[..n])?;
            offset += n + 1;
            Ok(value)
        };
        let encoding = string()?;
        let encoding_edition = string()?;
        let product = string()?;
        let product_edition = string()?;
        let profile = string()?;
        let dataset_name = string()?.to_owned();
        let dataset_title = string()?.to_owned();
        drop(string);
        let reference = std::str::from_utf8(
            data.get(offset..offset + 8)
                .context("Truncated cancellation DSRD")?,
        )?;
        offset += 8;
        let tail = data
            .get(offset..)
            .context("Truncated cancellation trailer")?;
        let mut parts = tail.splitn(4, |b| *b == 0x1f);
        let language = parts.next().unwrap();
        let abstract_text = parts.next().context("Missing cancellation abstract")?;
        let edition = parts.next().context("Missing cancellation DSED")?;
        let topics = parts
            .next()
            .context("Missing cancellation topic categories")?;
        ensure!(
            encoding == "S-100 Part 10a"
                && encoding_edition == "5.2"
                && product == "INT.IHO.S-101.2.0"
                && product_edition == "2.0",
            "Unsupported cancellation encoding/product edition"
        );
        ensure!(
            profile == "2" && edition == b"0",
            "Cancellation requires PROF2 and exact DSED0"
        );
        ensure!(
            language == b"EN" && abstract_text.is_empty(),
            "Cancellation language/abstract violates Annex B-7"
        );
        ensure!(
            topics == [14, 18] || topics == [18, 14],
            "Cancellation requires topic categories 14 and 18"
        );
        let (_, counter) = name(&dataset_name)?;
        Ok(Self {
            dataset_name,
            dataset_title,
            reference_date: date(reference, "%Y%m%d")?,
            counter,
        })
    }
    /// Validate a received cancellation announcement when no chart content is
    /// stored. No predecessor sequence/date claim is made without a target.
    pub fn validate_announcement(&self, metadata: &CancellationMetadata) -> Result<()> {
        ensure!(
            metadata.edition > 0
                && (1..=999).contains(&metadata.update)
                && self.counter == metadata.update,
            "Cancellation announcement catalogue/file counter mismatch"
        );
        Ok(())
    }

    /// Check the actual edition/history being removed, never treat catalogue
    /// DSED0 as the target edition or silently skip a missing predecessor.
    pub fn validate_target(
        &self,
        loaded: &DatasetIdentification,
        metadata: &CancellationMetadata,
        previous_issue_date: NaiveDate,
    ) -> Result<()> {
        ensure!(
            metadata.edition > 0
                && loaded.edition_number > 0
                && loaded.application_profile == "1"
                && metadata.update <= 999
                && loaded.update_number <= 999,
            "Invalid cancellation target edition/update range"
        );
        ensure!(
            loaded.product_identifier == "INT.IHO.S-101.2.0" && loaded.product_edition == "2.0",
            "Cancellation product differs from loaded cell"
        );
        ensure!(
            name(&loaded.dataset_name)?.0 == name(&self.dataset_name)?.0,
            "Cancellation targets another logical dataset"
        );
        ensure!(
            loaded.edition_number == metadata.edition,
            "Cancellation targets another edition"
        );
        let expected = loaded
            .update_number
            .checked_add(1)
            .filter(|n| *n <= 999)
            .context("Cancellation needs a new edition after counter999")?;
        ensure!(
            self.counter == expected && metadata.update == expected,
            "Cancellation is not the next sequential update"
        );
        ensure!(
            metadata.issue_date > previous_issue_date,
            "Cancellation issueDate must be greater than previous issueDate"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(profile: &str, edition: &str, filename: &str) -> Vec<u8> {
        let definition=b"000000000\x1fDataset identification\x1fRCNM!RCID!ENSP!ENED!PRSP!PRED!PROF!DSNM!DSTL!DSRD!DSLG!DSAB!DSED!*DSTC\x1f(b11,b14,7A(),A(8),3A(),b11)\x1e";
        let mut header = b"000003LE1 0000038 ! 4504".to_vec();
        header[..5].copy_from_slice(format!("{:05}", 38 + definition.len()).as_bytes());
        let mut bytes = header;
        bytes.extend_from_slice(format!("DSID{:04}00000", definition.len()).as_bytes());
        bytes.push(FIELD_TERMINATOR);
        bytes.extend(definition);
        let mut field = vec![10, 1, 0, 0, 0];
        field.extend_from_slice(format!("S-100 Part 10a\x1f5.2\x1fINT.IHO.S-101.2.0\x1f2.0\x1f{profile}\x1f{filename}\x1fCancelled\x1f20241016EN\x1f\x1f{edition}\x1f").as_bytes());
        field.extend([14, 18, FIELD_TERMINATOR]);
        let mut leader = b"000003DE1 0000038 ! 4504".to_vec();
        leader[..5].copy_from_slice(format!("{:05}", 38 + field.len()).as_bytes());
        bytes.extend(leader);
        bytes.extend_from_slice(format!("DSID{:04}00000", field.len()).as_bytes());
        bytes.push(FIELD_TERMINATOR);
        bytes.extend(field);
        bytes
    }
    #[test]
    fn cancellation_is_physical_zero_edition_and_targets_positive_catalogue_edition() {
        let value = Cancellation::parse(&fixture("2", "0", "101AA00TEST.003")).unwrap();
        let target = DatasetIdentification {
            dataset_name: "101AA00TEST.000".into(),
            product_identifier: "INT.IHO.S-101.2.0".into(),
            product_edition: "2.0".into(),
            application_profile: "1".into(),
            edition_number: 4,
            update_number: 2,
            ..Default::default()
        };
        let metadata = CancellationMetadata::new(4, Some(3), "2024-10-17").unwrap();
        value
            .validate_target(&target, &metadata, date("2024-10-16", "%Y-%m-%d").unwrap())
            .unwrap();
        assert_eq!(value.reference_date, date("20241016", "%Y%m%d").unwrap());
        assert!(CancellationMetadata::new(0, Some(3), "2024-10-17").is_err());
        assert!(CancellationMetadata::new(4, None, "2024-10-17").is_err());
    }
    #[test]
    fn announcement_checks_physical_catalogue_identity_without_inventing_predecessor() {
        let value = Cancellation::parse(&fixture("2", "0", "101AA00TEST.003")).unwrap();
        let metadata = CancellationMetadata::new(4, Some(3), "2024-10-17").unwrap();
        assert!(value.validate_announcement(&metadata).is_ok());
        for (edition, update) in [(0, 3), (4, 0), (4, 2), (4, 4), (4, 1000)] {
            assert!(value
                .validate_announcement(&CancellationMetadata {
                    edition,
                    update,
                    issue_date: metadata.issue_date,
                })
                .is_err());
        }
        // Receiving a notice does not assert a loaded edition, previous counter,
        // or previous date; those remain required by validate_target.
        let older = CancellationMetadata::new(4, Some(3), "2020-01-01").unwrap();
        assert!(value.validate_announcement(&older).is_ok());
    }
    #[test]
    fn rejects_replay_wrong_target_edition_gap_and_nonincreasing_date() {
        let value = Cancellation::parse(&fixture("2", "0", "101AA00TEST.003")).unwrap();
        let mut target = DatasetIdentification {
            dataset_name: "101AA00TEST.000".into(),
            product_identifier: "INT.IHO.S-101.2.0".into(),
            product_edition: "2.0".into(),
            application_profile: "1".into(),
            edition_number: 4,
            update_number: 2,
            ..Default::default()
        };
        let previous = date("2024-10-16", "%Y-%m-%d").unwrap();
        for (edition, update, issued) in [
            (3, 3, "2024-10-17"),
            (4, 2, "2024-10-17"),
            (4, 4, "2024-10-17"),
            (4, 3, "2024-10-16"),
        ] {
            assert!(value
                .validate_target(
                    &target,
                    &CancellationMetadata::new(edition, Some(update), issued).unwrap(),
                    previous
                )
                .is_err());
        }
        let bypass = CancellationMetadata {
            edition: 0,
            update: 1000,
            issue_date: previous,
        };
        assert!(value.validate_target(&target, &bypass, previous).is_err());
        let mut profile_two = target.clone();
        profile_two.application_profile = "2".into();
        assert!(value
            .validate_target(
                &profile_two,
                &CancellationMetadata::new(4, Some(3), "2024-10-17").unwrap(),
                previous
            )
            .is_err());
        target.dataset_name = "101BB00OTHER.000".into();
        assert!(value
            .validate_target(
                &target,
                &CancellationMetadata::new(4, Some(3), "2024-10-17").unwrap(),
                previous
            )
            .is_err());
    }
    #[test]
    fn rejects_nonzero_or_noncanonical_edition_profile_trailing_body_truncation_and_oversize() {
        for (profile, edition) in [("1", "0"), ("2", "0.003"), ("2", "00"), ("2", "4.003")] {
            assert!(Cancellation::parse(&fixture(profile, edition, "101AA00TEST.003")).is_err());
        }
        let good = fixture("2", "0", "101AA00TEST.003");
        for n in 0..good.len() {
            assert!(Cancellation::parse(&good[..n]).is_err());
        }
        let mut trailing = good.clone();
        let start = Leader::parse(&good).unwrap().record_length as usize;
        trailing.extend_from_slice(&good[start..]);
        assert!(Cancellation::parse(&trailing).is_err());
        assert!(Cancellation::parse(&vec![0; MAX_CANCELLATION_BYTES + 1]).is_err());
        let mut invalid_date = good;
        let n = invalid_date
            .windows(8)
            .position(|s| s == b"20241016")
            .unwrap();
        invalid_date[n..n + 8].copy_from_slice(b"20240230");
        assert!(Cancellation::parse(&invalid_date).is_err());
        assert!(Cancellation::parse(&fixture("2", "0", "../101AA00TEST.003")).is_err());
    }
}
