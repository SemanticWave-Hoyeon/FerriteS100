//! Application exchange-set planning. The core owns transactional materialization;
//! this module only selects a complete edition/reissue chain from authorized inputs.
use anyhow::{ensure, Context, Result};
use ferrite_s100_core::{DatasetIdentification, S101Cell};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

const MAX_INPUTS: usize = 4096;
const MAX_ENTRIES: usize = 100_000;

pub(crate) fn is_chart_file(path: &Path) -> bool {
    !path
        .file_name()
        .is_some_and(|n| n.as_encoded_bytes().starts_with(b"._"))
        && path.extension().is_some_and(|e| {
            let b = e.as_encoded_bytes();
            b.len() == 3 && b.iter().all(u8::is_ascii_digit)
        })
}

fn stem(path: &Path) -> Result<String> {
    ensure!(
        is_chart_file(path),
        "S-101 filename needs a three-digit extension: {}",
        path.display()
    );
    Ok(path
        .file_stem()
        .and_then(|s| s.to_str())
        .context("Non-UTF8 S-101 filename")?
        .to_owned())
}

/// A selected update also needs its base and predecessors. Loaded bases are
/// candidates, never substitutes for newly authenticated/parsed input bytes.
pub(crate) fn expand_candidates(selected: &[PathBuf], loaded: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let names: BTreeSet<_> = selected.iter().map(|p| stem(p)).collect::<Result<_>>()?;
    let mut roots = selected.to_vec();
    roots.extend(
        loaded
            .iter()
            .filter(|p| stem(p).is_ok_and(|s| names.contains(&s)))
            .cloned(),
    );
    let mut folders: BTreeMap<PathBuf, BTreeSet<String>> = BTreeMap::new();
    let mut inputs = BTreeSet::new();
    for path in roots {
        let name = stem(&path)?;
        // Canonicalize the selected input for authorization keys and deduplication.
        ensure!(path.is_file(), "Missing chart input: {}", path.display());
        inputs.insert(path.canonicalize()?);
        folders
            .entry(path.parent().unwrap_or(Path::new(".")).to_path_buf())
            .or_default()
            .insert(name);
    }
    let mut count = 0usize;
    for (folder, names) in folders {
        for entry in std::fs::read_dir(folder)? {
            let entry = entry?;
            count += 1;
            ensure!(
                count <= MAX_ENTRIES,
                "S-101 sibling discovery exceeds entry limit"
            );
            let path = entry.path();
            if entry.file_type()?.is_file() && is_chart_file(&path) && names.contains(&stem(&path)?)
            {
                inputs.insert(path.canonicalize()?);
                ensure!(inputs.len() <= MAX_INPUTS, "Too many S-101 chain inputs");
            }
        }
    }
    ensure!(inputs.len() <= MAX_INPUTS, "Too many S-101 chain inputs");
    Ok(inputs.into_iter().collect())
}

pub(crate) struct Input {
    pub original: PathBuf,
    pub data: PathBuf,
    pub id: DatasetIdentification,
    pub metadata: Option<crate::s101_lifecycle_metadata::MetadataEvidence>,
    _snapshot: Option<std::sync::Arc<ferrite_security::UnauthenticatedSnapshot>>,
    data_length: u64,
    data_sha256: [u8; 32],
}
pub(crate) struct Plan {
    pub base: Input,
    pub updates: Vec<Input>,
}

pub(crate) fn dataset_key(id: &DatasetIdentification) -> Result<(String, String)> {
    let path = Path::new(&id.dataset_name);
    ensure!(
        !id.dataset_name.contains(['/', '\\']),
        "Invalid dataset name"
    );
    let name = stem(path)?;
    ensure!(!name.is_empty(), "Empty S-101 dataset stem");
    Ok((id.product_identifier.clone(), name))
}

/// Only inspect private authenticated inputs when verification is required.
#[cfg(test)]
pub(crate) fn authorized_plans(
    candidates: Vec<PathBuf>,
    authorization: &ferrite_security::AuthorizedDatasets,
    require_signature: bool,
) -> Result<Vec<Plan>> {
    let (inputs, cancellations) = authorized_inputs(candidates, authorization, require_signature)?;
    ensure!(
        cancellations.is_empty(),
        "Cancellation datasets require the explicit App removal operation"
    );
    plans(inputs)
}

pub(crate) struct CancellationPlan {
    input: Input,
    target: Option<DatasetIdentification>,
    target_identity: Option<ferrite_s100_core::CellSourceIdentity>,
    previous_issue_date: Option<chrono::NaiveDate>,
}
impl CancellationPlan {
    pub(crate) fn load(self) -> Result<crate::chart_publication::ValidatedRemoval> {
        self.input.verify_captured()?;
        let bytes = read_cancellation_bytes(&self.input.data)?;
        let cancellation =
            parse_captured_cancellation(&bytes, self.input.data_length, self.input.data_sha256)?;
        let metadata = self
            .input
            .metadata
            .as_ref()
            .context("Cancellation has no captured catalogue metadata")?;
        let product_metadata = ferrite_s101::cancellation::CancellationMetadata::new(
            metadata.edition,
            metadata.update,
            &metadata.issue_date.format("%Y-%m-%d").to_string(),
        )?;
        self.input.verify_captured()?;
        if self.target.is_none() {
            return crate::chart_publication::ValidatedRemoval::announcement(
                &cancellation,
                &product_metadata,
                metadata.purpose,
            );
        }
        crate::chart_publication::ValidatedRemoval::validate(
            &cancellation,
            &product_metadata,
            metadata.purpose,
            self.target
                .as_ref()
                .expect("Target exists in loaded branch"),
            self.target_identity.expect("Loaded identity"),
            self.previous_issue_date.expect("Loaded date"),
        )
    }
}
fn read_cancellation_bytes(path: &Path) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(ferrite_s101::cancellation::MAX_CANCELLATION_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= ferrite_s101::cancellation::MAX_CANCELLATION_BYTES,
        "Cancellation input exceeds the receiver size limit"
    );
    Ok(bytes)
}
fn parse_captured_cancellation(
    bytes: &[u8],
    expected_length: u64,
    expected_sha256: [u8; 32],
) -> Result<ferrite_s101::cancellation::Cancellation> {
    use sha2::{Digest, Sha256};
    ensure!(
        bytes.len() <= ferrite_s101::cancellation::MAX_CANCELLATION_BYTES
            && bytes.len() as u64 == expected_length
            && <[u8; 32]>::from(Sha256::digest(bytes)) == expected_sha256,
        "Owned cancellation parse bytes differ from captured input"
    );
    ferrite_s101::cancellation::Cancellation::parse(bytes)
}
/// Cancellation is considered only when explicitly selected, never triggered
/// by discovering an old cancellation sibling while opening a different base.
pub(crate) fn authorized_batch(
    candidates: Vec<PathBuf>,
    selected: &[PathBuf],
    authorization: &ferrite_security::AuthorizedDatasets,
    require_signature: bool,
    loaded: &BTreeMap<
        (String, String),
        (
            DatasetIdentification,
            ferrite_s100_core::CellSourceIdentity,
            Option<crate::s101_lifecycle_metadata::MetadataEvidence>,
        ),
    >,
    history: &crate::chart_publication::CancellationHistory,
) -> Result<(Vec<Plan>, Vec<CancellationPlan>)> {
    let selected = selected
        .iter()
        .map(|p| p.canonicalize())
        .collect::<std::io::Result<BTreeSet<_>>>()?;
    let (mut ordinary, physical) = authorized_inputs(candidates, authorization, require_signature)?;
    let mut cancellations = Vec::new();
    let mut cancelled = BTreeSet::new();
    for input in physical
        .into_iter()
        .filter(|input| selected.contains(&input.original))
    {
        let key = dataset_key(&input.id)?;
        ensure!(
            cancelled.insert(key.clone()),
            "Duplicate cancellation of one logical dataset"
        );
        ensure!(
            !ordinary
                .iter()
                .any(|ordinary| selected.contains(&ordinary.original)
                    && dataset_key(&ordinary.id).is_ok_and(|k| k == key)),
            "Apply the complete base/update chain before selecting its cancellation"
        );
        if !loaded.contains_key(&key) {
            let metadata = input
                .metadata
                .as_ref()
                .context("Cancellation needs captured catalogue metadata")?;
            ensure!(
                !require_signature || metadata.is_authenticated(),
                "Cancellation metadata is not authenticated"
            );
            let physical = parse_captured_cancellation(
                &read_cancellation_bytes(&input.data)?,
                input.data_length,
                input.data_sha256,
            )?;
            let product = ferrite_s101::cancellation::CancellationMetadata::new(
                metadata.edition,
                metadata.update,
                &metadata.issue_date.format("%Y-%m-%d").to_string(),
            )?;
            let announcement = crate::chart_publication::ValidatedRemoval::announcement(
                &physical,
                &product,
                metadata.purpose,
            )?;
            history.validate_cancellation(announcement.record())?;
            cancellations.push(CancellationPlan {
                input,
                target: None,
                target_identity: None,
                previous_issue_date: None,
            });
            continue;
        }
        let (target, identity, previous) = loaded.get(&key).expect("Checked above");
        let previous = previous.as_ref().context(
            "Loaded target has no retained producer issueDate; reopen a valid exchange set",
        )?;
        let metadata = input
            .metadata
            .as_ref()
            .context("Cancellation needs catalogue edition, purpose, update and issueDate")?;
        ensure!(!require_signature || (previous.is_authenticated() && metadata.is_authenticated()),
            "Signature ON cannot use previously unchecked issueDate evidence; reopen the target with verification ON");
        ensure!(
            previous.edition == u32::from(target.edition_number)
                && previous.update == Some(u32::from(target.update_number)),
            "Retained ending metadata does not match cancellation target edition/update"
        );
        cancellations.push(CancellationPlan {
            input,
            target: Some(target.clone()),
            target_identity: Some(*identity),
            previous_issue_date: Some(previous.issue_date),
        });
    }
    ordinary.retain(|input| !dataset_key(&input.id).is_ok_and(|key| cancelled.contains(&key)));
    let plans = plans(ordinary)?;
    for plan in &plans {
        if let Some((loaded, _, _)) = loaded.get(&dataset_key(&plan.base.id)?) {
            plan.validate_against_loaded(loaded)?;
        }
        // The reused base's own date is required; a later update cannot make
        // an old cancelled base current. Check even if another instance's
        // cancellation arrived while this instance retained an old loaded key.
        history.validate_reuse(
            &plan.base.id,
            plan.base.metadata.as_ref().map(|m| m.issue_date),
        )?;
    }
    Ok((plans, cancellations))
}
fn input_fingerprint(path: &Path, limit: u64) -> Result<(u64, [u8; 32])> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut total = 0u64;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        total = total
            .checked_add(n as u64)
            .context("S-101 input size overflow")?;
        ensure!(total <= limit, "S-101 aggregate snapshot budget exceeded");
        hash.update(&buffer[..n]);
    }
    Ok((total, hash.finalize().into()))
}
impl Input {
    fn verify_captured(&self) -> Result<()> {
        ensure!(
            input_fingerprint(&self.data, self.data_length)?
                == (self.data_length, self.data_sha256),
            "Retained S-101 input changed after planning"
        );
        if let Some(metadata) = &self.metadata {
            metadata.verify_resource(&self.original, &self.data)?;
        }
        Ok(())
    }
}
fn authorized_inputs(
    candidates: Vec<PathBuf>,
    authorization: &ferrite_security::AuthorizedDatasets,
    require_signature: bool,
) -> Result<(Vec<Input>, Vec<Input>)> {
    ensure!(
        candidates.len() <= MAX_INPUTS,
        "Too many S-101 snapshot inputs"
    );
    let mut inputs = Vec::new();
    let mut cancellations = Vec::new();
    let mut remaining = 512u64 * 1024 * 1024;
    for path in candidates {
        let authenticated = crate::dataset_signature_policy::dataset_snapshot(
            authorization,
            &path,
            require_signature,
        )?;
        let unchecked = if authenticated.is_none() {
            Some(std::sync::Arc::new(
                ferrite_security::UnauthenticatedSnapshot::copy_bounded(&path, remaining)?,
            ))
        } else {
            None
        };
        let data = authenticated
            .as_ref()
            .map(|s| s.path().to_path_buf())
            .or_else(|| unchecked.as_ref().map(|s| s.path().to_path_buf()))
            .expect("Retained snapshot in every mode");
        let (data_length, data_sha256) = input_fingerprint(&data, remaining)?;
        remaining -= data_length;
        let cancellation =
            if data_length <= ferrite_s101::cancellation::MAX_CANCELLATION_BYTES as u64 {
                ferrite_s101::cancellation::Cancellation::parse(&std::fs::read(&data)?).ok()
            } else {
                None
            };
        let id = if let Some(cancellation) = &cancellation {
            ensure!(
                path.file_name().and_then(|p| p.to_str()) == Some(cancellation.dataset_name()),
                "Cancellation filename differs from DSID"
            );
            DatasetIdentification {
                dataset_name: cancellation.dataset_name().into(),
                product_identifier: "INT.IHO.S-101.2.0".into(),
                product_edition: "2.0".into(),
                application_profile: "2".into(),
                edition_number: 0,
                update_number: cancellation.counter(),
                ..Default::default()
            }
        } else {
            ferrite_s100_core::inspect_dataset_identification(&data)
                .with_context(|| format!("Cannot inspect S-101 input {}", path.display()))?
        };
        let metadata = match crate::s101_lifecycle_metadata::capture(&path, &data, authorization) {
            Ok(value) => value,
            Err(error) if !require_signature && cancellation.is_none() => {
                tracing::warn!("Unchecked catalogue metadata unavailable for {}: {error:#}; ordinary data still opens without a lifecycle date claim",path.display());
                None
            }
            Err(error) => return Err(error),
        };
        if cancellation.is_none() {
            if let Some(metadata) = &metadata {
                validate_catalogue_identity(
                    &id,
                    metadata.purpose,
                    metadata.edition,
                    metadata.update,
                )?;
            }
        }
        let input = Input {
            original: path,
            data,
            id,
            metadata,
            _snapshot: unchecked,
            data_length,
            data_sha256,
        };
        if cancellation.is_some() {
            cancellations.push(input);
        } else {
            inputs.push(input);
        }
    }
    Ok((inputs, cancellations))
}

fn validate_catalogue_identity(
    id: &DatasetIdentification,
    purpose: ferrite_security::DatasetPurpose,
    edition: u32,
    update: Option<u32>,
) -> Result<()> {
    use ferrite_security::DatasetPurpose::*;
    ensure!(
        edition == u32::from(id.edition_number) && edition > 0,
        "Catalogue/DSID edition mismatch"
    );
    let update = update.context("S-101 dataset requires catalogue updateNumber")?;
    ensure!(
        update <= 999
            && update == u32::from(id.update_number)
            && update == u32::from(filename_counter(id)?),
        "Catalogue/DSID/file update mismatch"
    );
    let valid = match purpose {
        NewDataset => id.application_profile == "1" && edition == 1 && update == 0,
        NewEdition => id.application_profile == "1" && edition > 1 && update == 0,
        Reissue => id.application_profile == "1",
        Update => id.application_profile == "2" && update > 0,
        Cancellation => false, // Requires its distinct physical B-7 operation.
    };
    ensure!(
        valid,
        "Catalogue purpose does not match S-101 physical profile/counter"
    );
    Ok(())
}

fn same_operation(a: &Input, b: &Input) -> bool {
    let (x, y) = (&a.id, &b.id);
    a.data_length == b.data_length
        && a.data_sha256 == b.data_sha256
        && x.dataset_name == y.dataset_name
        && x.dataset_title == y.dataset_title
        && x.product_identifier == y.product_identifier
        && x.product_edition == y.product_edition
        && x.edition_number == y.edition_number
        && x.update_number == y.update_number
        && x.application_profile == y.application_profile
        && x.update_application_date == y.update_application_date
        && x.issue_date == y.issue_date
        && crate::s101_lifecycle_metadata::MetadataEvidence::compatible_optional(
            a.metadata.as_ref(),
            b.metadata.as_ref(),
        )
}

fn insert_operation(
    operations: &mut BTreeMap<u16, Input>,
    counter: u16,
    input: Input,
) -> Result<()> {
    match operations.entry(counter) {
        std::collections::btree_map::Entry::Vacant(entry) => {
            entry.insert(input);
        }
        std::collections::btree_map::Entry::Occupied(mut entry) => {
            let previous = entry.get();
            ensure!(same_operation(previous, &input),
                "Conflicting duplicate S-101 operation: dataset {}, edition {}, counter {:03}: {} and {}",
                input.id.dataset_name, input.id.edition_number, counter,
                previous.original.display(), input.original.display());
            // Preserve the chosen snapshot and its complete metadata proof together.
            // Prefer optional OFF evidence with its original owner; never attach
            // that proof to the metadata-free copy. Then choose a stable path.
            let richer_unverified = input.metadata.is_some() && previous.metadata.is_none();
            let same_evidence_presence = input.metadata.is_some() == previous.metadata.is_some();
            if richer_unverified || (same_evidence_presence && input.original < previous.original) {
                entry.insert(input);
            }
        }
    }
    Ok(())
}

pub(crate) fn plans(inputs: Vec<Input>) -> Result<Vec<Plan>> {
    ensure!(inputs.len() <= MAX_INPUTS, "Too many S-101 chain inputs");
    let mut groups: BTreeMap<(String, String), Vec<Input>> = BTreeMap::new();
    for input in inputs {
        ensure!(
            matches!(input.id.application_profile.as_str(), "1" | "2"),
            "Unsupported S-101 application profile"
        );
        ensure!(
            input.id.edition_number > 0,
            "Cancellation datasets need an explicit removal operation"
        );
        let counter: u16 = Path::new(&input.id.dataset_name)
            .extension()
            .and_then(|s| s.to_str())
            .context("Missing dataset counter")?
            .parse()?;
        // A legacy .000 base DSED may contain just its edition; reissues and
        // updates must also identify their numeric counter.
        ensure!(
            input.id.update_number == counter,
            "DSID filename/update mismatch"
        );
        groups
            .entry(dataset_key(&input.id)?)
            .or_default()
            .push(input);
    }
    let mut result = Vec::new();
    for (_, mut group) in groups {
        let edition = group.iter().map(|i| i.id.edition_number).max().unwrap();
        group.retain(|i| i.id.edition_number == edition);
        let base_counter = group
            .iter()
            .filter(|i| i.id.application_profile == "1")
            .map(|i| filename_counter(&i.id))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .max()
            .context("Update chain has no profile-1 base in its latest edition")?;
        let mut bases = BTreeMap::new();
        let mut updates = BTreeMap::new();
        for input in group {
            let counter = filename_counter(&input.id)?;
            if input.id.application_profile == "1" && counter == base_counter {
                insert_operation(&mut bases, counter, input)?;
            } else if input.id.application_profile == "2" && counter > base_counter {
                insert_operation(&mut updates, counter, input)?;
            }
        }
        ensure!(bases.len() == 1, "Ambiguous duplicate base/reissue");
        let base = bases.pop_first().unwrap().1;
        let mut expected = base_counter;
        for (&counter, input) in &updates {
            expected = expected.checked_add(1).context("Update counter overflow")?;
            ensure!(
                counter == expected,
                "Missing update {} before {}",
                expected,
                counter
            );
            ensure!(
                input.id.product_edition == base.id.product_edition,
                "Update product specification edition mismatch"
            );
        }
        result.push(Plan {
            base,
            updates: updates.into_values().collect(),
        });
    }
    Ok(result)
}
fn filename_counter(id: &DatasetIdentification) -> Result<u16> {
    dataset_key(id)?;
    Ok(Path::new(&id.dataset_name)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap()
        .parse()?)
}
impl Plan {
    pub(crate) fn validate_against_loaded(&self, loaded: &DatasetIdentification) -> Result<()> {
        if dataset_key(loaded)? == dataset_key(&self.base.id)? {
            let ending = self
                .updates
                .last()
                .map(|i| i.id.update_number)
                .unwrap_or(self.base.id.update_number);
            ensure!((self.base.id.edition_number, ending) >= (loaded.edition_number, loaded.update_number),
                "Selected S-101 chain would downgrade a loaded edition/update; clear the chart before an explicit rollback");
        }
        Ok(())
    }
    pub(crate) fn ending_metadata(
        &self,
    ) -> Option<&crate::s101_lifecycle_metadata::MetadataEvidence> {
        self.updates.last().unwrap_or(&self.base).metadata.as_ref()
    }
    pub(crate) fn load(&self) -> Result<(S101Cell, ferrite_s100_core::CellSourceIdentity)> {
        use sha2::{Digest, Sha256};
        let chain: Vec<_> = std::iter::once(&self.base)
            .chain(self.updates.iter())
            .collect();
        for input in &chain {
            input.verify_captured()?;
        }
        let expected: [u8; 32] = if self.updates.is_empty() {
            self.base.data_sha256
        } else {
            let mut hash = Sha256::new();
            hash.update(b"FerriteS100/S101/ordered-raw-update-chain/v1\0");
            hash.update((chain.len() as u64).to_le_bytes());
            for (index, input) in chain.iter().enumerate() {
                hash.update((index as u64).to_le_bytes());
                hash.update(input.data_length.to_le_bytes());
                hash.update(input.data_sha256);
            }
            hash.finalize().into()
        };
        let updates: Vec<_> = self.updates.iter().map(|i| i.data.clone()).collect();
        let result = S101Cell::load_update_chain_from_with_identity(
            &self.base.original,
            &self.base.data,
            &updates,
        )?;
        ensure!(
            *result.1.sha256() == expected,
            "Materialized S-101 chain differs from captured raw inputs"
        );
        self.validate_materialized(&result.0)?;
        Ok(result)
    }
    pub(crate) fn input_paths(&self) -> Vec<PathBuf> {
        std::iter::once(&self.base)
            .chain(self.updates.iter())
            .map(|i| i.original.clone())
            .collect()
    }
    /// Recheck metadata from the owned parse, not just the earlier planning read.
    pub(crate) fn validate_materialized(&self, cell: &S101Cell) -> Result<()> {
        let expected = self
            .updates
            .last()
            .map(|i| i.id.update_number)
            .unwrap_or(self.base.id.update_number);
        ensure!(
            dataset_key(&cell.dsid)? == dataset_key(&self.base.id)?
                && cell.dsid.dataset_name == self.base.id.dataset_name
                && cell.dsid.application_profile == "1"
                && cell.dsid.product_edition == self.base.id.product_edition
                && cell.dsid.edition_number == self.base.id.edition_number
                && cell.dsid.update_number == expected,
            "Dataset identity changed after chain planning; reopen it"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn input(edition: u16, counter: u16, profile: &str) -> Input {
        let name = format!("101TEST.{counter:03}");
        Input {
            original: format!("ed{edition}/{name}").into(),
            data: format!("private/{edition}/{name}").into(),
            metadata: None,
            _snapshot: None,
            data_length: 0,
            data_sha256: [0; 32],
            id: DatasetIdentification {
                dataset_name: name,
                product_identifier: "INT.IHO.S-101.2.0".into(),
                product_edition: "2.0".into(),
                edition_number: edition,
                update_number: counter,
                application_profile: profile.into(),
                ..Default::default()
            },
        }
    }
    #[test]
    fn signed_catalogue_identity_rejects_wrong_edition_counter_and_purpose() {
        use ferrite_security::DatasetPurpose::*;
        let base = input(1, 0, "1");
        assert!(validate_catalogue_identity(&base.id, NewDataset, 1, Some(0)).is_ok());
        for (purpose, edition, update) in [
            (NewDataset, 2, Some(0)),
            (Update, 1, Some(0)),
            (Cancellation, 1, Some(0)),
            (NewDataset, 1, None),
            (NewDataset, 1, Some(1)),
        ] {
            assert!(validate_catalogue_identity(&base.id, purpose, edition, update).is_err());
        }
        let reissue = input(2, 2, "1");
        assert!(validate_catalogue_identity(&reissue.id, Reissue, 2, Some(2)).is_ok());
        assert!(validate_catalogue_identity(&reissue.id, Update, 2, Some(2)).is_err());
        let update = input(2, 3, "2");
        assert!(validate_catalogue_identity(&update.id, Update, 2, Some(3)).is_ok());
        assert!(validate_catalogue_identity(&update.id, Reissue, 2, Some(3)).is_err());
        assert!(validate_catalogue_identity(&input(2, 0, "1").id, NewEdition, 2, Some(0)).is_ok());
    }

    #[test]
    fn latest_edition_reissue_and_order_selected_without_old_updates() {
        let p = plans(vec![
            input(1, 0, "1"),
            input(1, 1, "2"),
            input(2, 0, "1"),
            input(2, 1, "2"),
            input(2, 2, "1"),
            input(2, 4, "2"),
            input(2, 3, "2"),
        ])
        .unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(
            (p[0].base.id.edition_number, p[0].base.id.update_number),
            (2, 2)
        );
        assert_eq!(
            p[0].updates
                .iter()
                .map(|i| i.id.update_number)
                .collect::<Vec<_>>(),
            [3, 4]
        );
        assert_eq!(p[0].input_paths().len(), 3);
    }
    #[test]
    fn rejects_gaps_latest_edition_without_base_and_conflicting_copies() {
        assert!(plans(vec![input(1, 0, "1"), input(1, 2, "2")]).is_err());
        assert!(plans(vec![input(1, 0, "1"), input(2, 1, "2")]).is_err());
        let mut different = input(1, 0, "1");
        different.data_sha256[0] = 1;
        assert!(plans(vec![input(1, 0, "1"), different]).is_err());
        let mut different = input(1, 1, "2");
        different.id.issue_date = "20261007".into();
        assert!(plans(vec![input(1, 0, "1"), input(1, 1, "2"), different]).is_err());
    }
    #[test]
    fn equivalent_base_reissue_updates_coalesce_in_stable_path_order() {
        for profile in ["1", "2"] {
            for reversed in [false, true] {
                let mut a = input(2, 1, profile);
                a.original = "a/101TEST.001".into();
                let mut b = input(2, 1, profile);
                b.original = "b/101TEST.001".into();
                let mut copies = if reversed { vec![b, a] } else { vec![a, b] };
                if profile == "2" {
                    copies.push(input(2, 0, "1"));
                }
                let p = plans(copies).unwrap();
                let chosen = if profile == "1" {
                    &p[0].base
                } else {
                    &p[0].updates[0]
                };
                assert_eq!(chosen.original, PathBuf::from("a/101TEST.001"));
            }
        }
    }
    #[test]
    fn conflicting_duplicate_reports_both_paths_and_counter() {
        let mut different = input(1, 1, "2");
        different.original = "other/101TEST.001".into();
        different.data_length = 1;
        let error = plans(vec![input(1, 0, "1"), input(1, 1, "2"), different])
            .err()
            .unwrap()
            .to_string();
        assert!(
            error.contains("counter 001")
                && error.contains("ed1/101TEST.001")
                && error.contains("other/101TEST.001")
        );
    }
    #[test]
    fn rejects_product_counter_and_filename_changes() {
        let mut mismatch = input(1, 1, "2");
        mismatch.id.product_edition = "2.1".into();
        assert!(plans(vec![input(1, 0, "1"), mismatch]).is_err());
        let mut bad = input(1, 1, "2");
        bad.id.update_number = 2;
        assert!(plans(vec![input(1, 0, "1"), bad]).is_err());
        let mut bad = input(1, 0, "1");
        bad.id.dataset_name = "../101TEST.000".into();
        assert!(plans(vec![bad]).is_err());
        let mut bad = input(1, 2, "1");
        bad.id.update_number = 0;
        assert!(plans(vec![bad]).is_err());
    }
    #[test]
    fn required_authentication_rejects_unchecked_inputs_before_inspection() {
        let dir = std::env::temp_dir().join(format!("ferrite-chain-auth-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("101TEST.000");
        std::fs::write(&path, b"not ISO8211").unwrap();
        let authorization =
            crate::dataset_signature_policy::unchecked_datasets(&[path.clone()]).unwrap();
        let error = authorized_plans(vec![path], &authorization, true)
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("Required authenticated dataset snapshot"));
        std::fs::remove_dir_all(dir).unwrap();
    }
    /// External official producer fixtures are opt-in; this is the exact
    #[test]
    #[ignore = "requires FERRITE_SHOM_EXCHANGE_FOLDER and FERRITE_SHOM_FOLDER_REPORT"]
    fn actual_shom_exchange_folder_all_chains() {
        let folder: PathBuf = std::env::var_os("FERRITE_SHOM_EXCHANGE_FOLDER")
            .unwrap()
            .into();
        let choices = crate::dataset_discovery::exchange_set_choices(&folder).unwrap();
        assert!(choices.len() > 1);
        let mut rows = Vec::new();
        for choice in &choices {
            let (charts, _) = crate::dataset_discovery::discover_exchange_folder(choice).unwrap();
            let candidates = expand_candidates(&charts, &[]).unwrap();
            let authorization =
                crate::dataset_signature_policy::unchecked_datasets(&candidates).unwrap();
            match authorized_plans(candidates, &authorization, false) {
                Ok(plans) => {
                    for plan in plans {
                        let (cell, identity) = plan.load().unwrap();
                        rows.push(serde_json::json!({"choice":choice,"dataset":cell.dsid.dataset_name,
                        "edition":cell.dsid.edition_number,"update":cell.dsid.update_number,
                        "features":cell.features.len(),"sha256":identity.sha256(),"paths":plan.input_paths()}));
                    }
                }
                Err(error) => {
                    assert!(error.to_string().contains("no profile-1 base"));
                    rows.push(serde_json::json!({"choice":choice,"requires_loaded_base":true,"error":error.to_string()}));
                }
            }
        }
        assert!(rows
            .iter()
            .any(|r| r["features"].as_u64().is_some_and(|n| n > 0)));
        let (all, _) = crate::dataset_discovery::discover_exchange_folder(&folder).unwrap();
        let all = expand_candidates(&all, &[]).unwrap();
        let authorization = crate::dataset_signature_policy::unchecked_datasets(&all).unwrap();
        let error = authorized_plans(all.clone(), &authorization, false)
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("Conflicting duplicate S-101 operation"));
        let (inputs, _) = authorized_inputs(all, &authorization, false).unwrap();
        let equivalent: Vec<_> = inputs
            .into_iter()
            .filter(|i| i.id.dataset_name.starts_with("101FR00571300.") && i.id.edition_number == 1)
            .collect();
        assert!(equivalent.len() > 3);
        let merged = plans(equivalent).unwrap();
        assert_eq!(merged.len(), 1);
        let (cell, _) = merged[0].load().unwrap();
        assert_eq!(cell.dsid.update_number, 2);
        let normalized = folder.parent().unwrap().join("NormalizedExchangeSets");
        let (normalized_charts, _) =
            crate::dataset_discovery::discover_exchange_folder(&normalized).unwrap();
        let selected: Vec<_> = merged[0]
            .input_paths()
            .into_iter()
            .chain(normalized_charts)
            .collect();
        let candidates = expand_candidates(&selected, &[]).unwrap();
        let authorization =
            crate::dataset_signature_policy::unchecked_datasets(&candidates).unwrap();
        let (inputs, _) = authorized_inputs(candidates, &authorization, false).unwrap();
        let mixed: Vec<_> = inputs
            .into_iter()
            .filter(|i| i.id.dataset_name.starts_with("101FR00571300.") && i.id.edition_number == 1)
            .collect();
        assert!(mixed.iter().any(|i| i.metadata.is_none()));
        assert!(mixed.iter().any(|i| i.metadata.is_some()));
        let mixed = plans(mixed).unwrap();
        assert!(std::iter::once(&mixed[0].base)
            .chain(mixed[0].updates.iter())
            .all(
                |i| i.metadata.as_ref().is_some_and(|m| !m.is_authenticated())
                    && i.original.starts_with(&normalized)
            ));
        let (mixed_cell, mixed_identity) = mixed[0].load().unwrap();
        let (_, merged_identity) = merged[0].load().unwrap();
        assert_eq!(mixed_cell.features.len(), cell.features.len());
        assert_eq!(mixed_identity.sha256(), merged_identity.sha256());
        rows.push(serde_json::json!({"actual_identical_copies_merged":true,
            "dataset":cell.dsid.dataset_name,"edition":cell.dsid.edition_number,
            "update":cell.dsid.update_number,"features":cell.features.len(),
            "actual_conflict_preserved":error}));
        rows.push(serde_json::json!({"raw_normalized_copies_merged":true,
            "retained_unverified_metadata_owner":mixed[0].input_paths(),
            "same_raw_chain_identity":true,"features":mixed_cell.features.len()}));
        let report: PathBuf = std::env::var_os("FERRITE_SHOM_FOLDER_REPORT")
            .unwrap()
            .into();
        std::fs::write(
            report,
            serde_json::to_vec_pretty(&serde_json::json!({
                "folder":folder,"choices":choices,
                "signature_verified":false,"chains":rows,"native_render_verified":false
            }))
            .unwrap(),
        )
        .unwrap();
    }
    /// External official producer fixtures are opt-in; this is the exact
    /// expansion/authorization/planning/materialization path used by the App.
    #[test]
    #[ignore = "requires FERRITE_SHOM_UPDATE_BASE and FERRITE_SHOM_UPDATE_REPORT"]
    fn actual_shom_application_chain() {
        let selected: PathBuf = std::env::var_os("FERRITE_SHOM_UPDATE_BASE").unwrap().into();
        let candidates = expand_candidates(&[selected], &[]).unwrap();
        let signed = std::env::var("FERRITE_SHOM_SIGNED").is_ok_and(|v| v == "1");
        let authorization = if signed {
            let mut anchors = ferrite_security::TrustAnchors::default();
            anchors
                .install_pem(
                    "IHO",
                    &std::fs::read(std::env::var_os("FERRITE_SHOM_TRUST").unwrap()).unwrap(),
                )
                .unwrap();
            let time = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64;
            ferrite_security::authorize_datasets(
                &candidates,
                &anchors,
                time,
                ferrite_security::UnsignedPolicy::Reject,
            )
            .unwrap()
        } else {
            crate::dataset_signature_policy::unchecked_datasets(&candidates).unwrap()
        };
        let plans = authorized_plans(candidates, &authorization, signed).unwrap();
        assert_eq!(plans.len(), 1);
        let (cell, identity) = plans[0].load().unwrap();
        let mut features: Vec<_> = cell.features.iter().collect();
        features.sort_by_key(|(id, _)| **id);
        fn attrs(attrs: &[ferrite_s100_core::Attribute]) -> Vec<serde_json::Value> {
            let mut paths = Vec::<Vec<(String, u16)>>::new();
            attrs
                .iter()
                .map(|a| {
                    let mut path = if a.paix == 0 {
                        Vec::new()
                    } else {
                        paths[usize::from(a.paix) - 1].clone()
                    };
                    path.push((a.code.clone().unwrap(), a.atix));
                    paths.push(path.clone());
                    serde_json::json!({"path":path,"value":a.atvl})
                })
                .collect()
        }
        let rows: Vec<_>=features.into_iter().map(|(_,f)| serde_json::json!({
            "id":f.frid.rcid,"feature_code":f.feature_code,"attributes":attrs(&f.attributes),
            "spatial_associations":f.spatial_associations.iter().map(|s|serde_json::json!({
                "name":s.spatial_id.rcnm,"id":s.spatial_id.rcid,"orientation":s.ornt,
                "scale_min":s.scale_minimum,"scale_max":s.scale_maximum,"instruction":s.update_instruction,
            })).collect::<Vec<_>>(),
            "information_associations":f.information_associations.iter().map(|a|serde_json::json!({
                "name":a.info_id.rcnm,"id":a.info_id.rcid,"association":a.niac,"role":a.narc,"attributes":attrs(&a.attributes),
            })).collect::<Vec<_>>(),
            "feature_associations":f.feature_associations.iter().map(|a|serde_json::json!({
                "name":a.feature_id.rcnm,"id":a.feature_id.rcid,"association":a.nfac,"role":a.narc,"attributes":attrs(&a.attributes),
            })).collect::<Vec<_>>(),
            "foid":f.foid.map(|id|[u64::from(id.agen),u64::from(id.fidn),u64::from(id.fids)]),
        })).collect();
        let output: PathBuf = std::env::var_os("FERRITE_SHOM_UPDATE_REPORT")
            .unwrap()
            .into();
        std::fs::write(output, serde_json::to_vec_pretty(&serde_json::json!({
            "source":cell.file_path,"edition":cell.dsid.edition_number,"update":cell.dsid.update_number,
            "feature_count":cell.features.len(),"input_paths":plans[0].input_paths(),
            "source_sha256":identity.sha256(),"features":rows,
            "association_mappings":{"FACS":cell.code_mappings.feature_associations.num_to_str,
                "IACS":cell.code_mappings.information_associations.num_to_str,
                "ARCS":cell.code_mappings.association_roles.num_to_str},
            "signature_verified":signed,"verified_count":authorization.signed_count,"dataset_discovery":authorization.dataset_discovery,"native_render_verified":false,
        })).unwrap()).unwrap();
    }
    #[test]
    fn preserves_loaded_newer_chain_and_rejects_silent_rollback() {
        let p = plans(vec![input(1, 0, "1"), input(1, 1, "2")]).unwrap();
        let loaded = input(1, 2, "2").id;
        assert!(p[0].validate_against_loaded(&loaded).is_err());
        assert!(p[0].validate_against_loaded(&input(1, 1, "2").id).is_ok());
        assert!(p[0].validate_against_loaded(&input(2, 0, "1").id).is_err());
        let p = plans(vec![input(2, 0, "1")]).unwrap();
        assert!(p[0].validate_against_loaded(&loaded).is_ok());
    }
    #[test]
    fn split_exchange_folders_keep_already_loaded_predecessors() {
        let root = std::env::temp_dir().join(format!("ferrite-chain-split-{}", std::process::id()));
        let mut paths = Vec::new();
        for counter in 0..=3 {
            let dir = root.join(format!("exchange{counter}"));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join(format!("101TEST.{counter:03}"));
            std::fs::write(&path, []).unwrap();
            paths.push(path);
        }
        let found = expand_candidates(&[paths[3].clone()], &paths[..3]).unwrap();
        assert_eq!(found.len(), 4);
        for path in &paths {
            assert!(found.contains(&path.canonicalize().unwrap()));
        }
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn discovery_collects_predecessors_deduplicates_and_ignores_metadata() {
        let dir =
            std::env::temp_dir().join(format!("ferrite-chain-discovery-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        for name in [
            "101TEST.000",
            "101TEST.001",
            "101TEST.002",
            "._101TEST.003",
            "OTHER.000",
            "101TEST.xml",
        ] {
            std::fs::write(dir.join(name), []).unwrap();
        }
        let found =
            expand_candidates(&[dir.join("101TEST.002"), dir.join("101TEST.002")], &[]).unwrap();
        assert_eq!(
            found,
            ["101TEST.000", "101TEST.001", "101TEST.002"]
                .map(|n| dir.join(n).canonicalize().unwrap())
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn cancellation_batch_requires_loaded_date_purpose_counter_and_explicit_selection() {
        use crate::s101_lifecycle_metadata::capture;
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let folder = std::env::temp_dir().join(format!(
            "ferrite-cancel-batch-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&folder).unwrap();
        let base = folder.join("101AA00TEST.002");
        let cancel = folder.join("101AA00TEST.003");
        let fixture = crate::chart_publication::tests::physical_cancellation_fixture;
        std::fs::write(&base, fixture("1", "4.002", "101AA00TEST.002")).unwrap();
        std::fs::write(&cancel, fixture("2", "0", "101AA00TEST.003")).unwrap();
        let xml = |purpose: &str, date: &str| {
            format!(
                r#"<S100_ExchangeCatalogue xmlns="http://www.iho.int/s100/xc/5.2"><datasetDiscoveryMetadata><S100_DatasetDiscoveryMetadata><fileName>101AA00TEST.002</fileName><purpose>4</purpose><editionNumber>4</editionNumber><updateNumber>2</updateNumber><issueDate>2024-10-16</issueDate></S100_DatasetDiscoveryMetadata></datasetDiscoveryMetadata><datasetDiscoveryMetadata><S100_DatasetDiscoveryMetadata><fileName>101AA00TEST.003</fileName><purpose>{purpose}</purpose><editionNumber>4</editionNumber><updateNumber>3</updateNumber><issueDate>{date}</issueDate></S100_DatasetDiscoveryMetadata></datasetDiscoveryMetadata></S100_ExchangeCatalogue>"#
            )
        };
        std::fs::write(folder.join("CATALOG.XML"), xml("5", "2024-10-17")).unwrap();
        let base = base.canonicalize().unwrap();
        let cancel = cancel.canonicalize().unwrap();
        let auth =
            crate::dataset_signature_policy::unchecked_datasets(&[base.clone(), cancel.clone()])
                .unwrap();
        let (cell, identity) = S101Cell::load_from_with_identity(&base, &base).unwrap();
        let previous = capture(&base, &base, &auth).unwrap().unwrap();
        let key = dataset_key(&cell.dsid).unwrap();
        let mut loaded = BTreeMap::new();
        loaded.insert(key, (cell.dsid.clone(), identity, Some(previous)));
        let history = crate::chart_publication::CancellationHistory::default();
        let (loads, cancels) = authorized_batch(
            vec![base.clone(), cancel.clone()],
            &[cancel.clone()],
            &auth,
            false,
            &loaded,
            &history,
        )
        .unwrap();
        assert!(loads.is_empty());
        assert_eq!(cancels.len(), 1);
        assert!(cancels.into_iter().next().unwrap().load().is_ok());
        let (loads, cancels) = authorized_batch(
            vec![base.clone(), cancel.clone()],
            &[base.clone()],
            &auth,
            false,
            &loaded,
            &history,
        )
        .unwrap();
        assert_eq!(loads.len(), 1);
        assert!(cancels.is_empty());
        let (loads, announcements) = authorized_batch(
            vec![cancel.clone()],
            &[cancel.clone()],
            &auth,
            false,
            &BTreeMap::new(),
            &history,
        )
        .unwrap();
        assert!(loads.is_empty());
        let announcement = announcements.into_iter().next().unwrap().load().unwrap();
        assert!(!announcement.removes_content());
        assert_eq!(announcement.record().cancelled_edition, 4);
        assert_eq!(announcement.record().cancellation_update, 3);
        // Unchecked metadata must never become authenticated by absence of content.
        assert!(authorized_batch(
            vec![cancel.clone()],
            &[cancel.clone()],
            &auth,
            true,
            &BTreeMap::new(),
            &history
        )
        .is_err());
        let mut stale_history = crate::chart_publication::CancellationHistory::default();
        let mut newer = announcement.record().clone();
        newer.issue_date = chrono::NaiveDate::from_ymd_opt(2024, 10, 18).unwrap();
        stale_history.record(vec![newer]);
        assert!(authorized_batch(
            vec![cancel.clone()],
            &[cancel.clone()],
            &auth,
            false,
            &BTreeMap::new(),
            &stale_history
        )
        .is_err());
        std::fs::write(folder.join("CATALOG.XML"), xml("3", "2024-10-17")).unwrap();
        assert!(authorized_batch(
            vec![cancel.clone()],
            &[cancel.clone()],
            &auth,
            false,
            &BTreeMap::new(),
            &history
        )
        .is_err());
        let (_, cancels) = authorized_batch(
            vec![cancel.clone()],
            &[cancel.clone()],
            &auth,
            false,
            &loaded,
            &history,
        )
        .unwrap();
        assert!(cancels.into_iter().next().unwrap().load().is_err());
        std::fs::write(folder.join("CATALOG.XML"), xml("5", "2024-10-16")).unwrap();
        let (_, cancels) = authorized_batch(
            vec![cancel.clone()],
            &[cancel.clone()],
            &auth,
            false,
            &loaded,
            &history,
        )
        .unwrap();
        assert!(cancels.into_iter().next().unwrap().load().is_err());
        std::fs::remove_file(folder.join("CATALOG.XML")).unwrap();
        assert!(authorized_batch(
            vec![cancel.clone()],
            &[cancel.clone()],
            &auth,
            false,
            &BTreeMap::new(),
            &history
        )
        .is_err());
        std::fs::remove_dir_all(folder).unwrap();
    }
    #[test]
    fn cancellation_reader_rejects_growth_beyond_physical_receiver_limit() {
        let path =
            std::env::temp_dir().join(format!("ferrite-cancel-limit-{}", std::process::id()));
        std::fs::write(
            &path,
            vec![0; ferrite_s101::cancellation::MAX_CANCELLATION_BYTES + 1],
        )
        .unwrap();
        assert!(read_cancellation_bytes(&path).is_err());
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn actual_owned_cancellation_bytes_must_match_capture_even_when_altered_bytes_are_valid_b7() {
        use sha2::{Digest, Sha256};
        let a = crate::chart_publication::tests::physical_cancellation_fixture(
            "2",
            "0",
            "101AA00TEST.003",
        );
        let mut b = a.clone();
        let title = b.windows(9).position(|part| part == b"Cancelled").unwrap();
        b[title] = b'c';
        assert!(ferrite_s101::cancellation::Cancellation::parse(&b).is_ok());
        let hash = Sha256::digest(&a).into();
        assert!(parse_captured_cancellation(&a, a.len() as u64, hash).is_ok());
        assert!(parse_captured_cancellation(&b, a.len() as u64, hash).is_err());
        assert!(parse_captured_cancellation(&a, a.len() as u64 + 1, hash).is_err());
    }
    #[test]
    fn later_update_cannot_reuse_cancelled_old_base_or_bypass_fresh_history_with_loaded_key() {
        let folder =
            std::env::temp_dir().join(format!("ferrite-reuse-dates-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let base = folder.join("101AA00TEST.002");
        let update = folder.join("101AA00TEST.003");
        let fixture = crate::chart_publication::tests::physical_cancellation_fixture;
        std::fs::write(&base, fixture("1", "4.002", "101AA00TEST.002")).unwrap();
        std::fs::write(&update, fixture("2", "4.003", "101AA00TEST.003")).unwrap();
        let xml = |date: &str| {
            format!(
                r#"<S100_ExchangeCatalogue xmlns="http://www.iho.int/s100/xc/5.2"><datasetDiscoveryMetadata><S100_DatasetDiscoveryMetadata><fileName>101AA00TEST.002</fileName><purpose>4</purpose><editionNumber>4</editionNumber><updateNumber>2</updateNumber><issueDate>{date}</issueDate></S100_DatasetDiscoveryMetadata></datasetDiscoveryMetadata><datasetDiscoveryMetadata><S100_DatasetDiscoveryMetadata><fileName>101AA00TEST.003</fileName><purpose>3</purpose><editionNumber>4</editionNumber><updateNumber>3</updateNumber><issueDate>2024-10-19</issueDate></S100_DatasetDiscoveryMetadata></datasetDiscoveryMetadata></S100_ExchangeCatalogue>"#
            )
        };
        std::fs::write(folder.join("CATALOG.XML"), xml("2024-10-16")).unwrap();
        let base = base.canonicalize().unwrap();
        let update = update.canonicalize().unwrap();
        let auth =
            crate::dataset_signature_policy::unchecked_datasets(&[base.clone(), update.clone()])
                .unwrap();
        let (cell, identity) = S101Cell::load_from_with_identity(&base, &base).unwrap();
        let key = dataset_key(&cell.dsid).unwrap();
        let mut history = crate::chart_publication::CancellationHistory::default();
        history.record(vec![crate::chart_publication::CancellationRecord {
            key: key.clone(),
            cancelled_edition: 4,
            cancellation_update: 3,
            issue_date: chrono::NaiveDate::from_ymd_opt(2024, 10, 17).unwrap(),
        }]);
        let candidates = vec![base.clone(), update.clone()];
        assert!(authorized_batch(
            candidates.clone(),
            &[update.clone()],
            &auth,
            false,
            &BTreeMap::new(),
            &history
        )
        .is_err());
        let previous = crate::s101_lifecycle_metadata::capture(&base, &base, &auth).unwrap();
        let mut loaded = BTreeMap::new();
        loaded.insert(key, (cell.dsid, identity, previous));
        assert!(authorized_batch(
            candidates.clone(),
            &[update.clone()],
            &auth,
            false,
            &loaded,
            &history
        )
        .is_err());
        std::fs::write(folder.join("CATALOG.XML"), xml("2024-10-18")).unwrap();
        assert!(authorized_batch(candidates, &[update], &auth, false, &loaded, &history).is_ok());
        std::fs::remove_dir_all(folder).unwrap();
    }
}
