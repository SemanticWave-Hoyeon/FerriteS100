//! App cell publication transaction. Product parsing stays in its adapter;
//! replacement moves owned cells and retains only replaced cells for rollback.
use crate::{cell_source_identity::LoadedSourceIdentities, s101_update_plan, ChartLoadResult};
use anyhow::{ensure, Result};
use ferrite_s100_core::{CellSourceIdentity, S101Cell};
use std::{collections::BTreeMap, path::PathBuf};

struct PreviousCell {
    index: usize,
    cell: S101Cell,
    identity: CellSourceIdentity,
    paths: Vec<PathBuf>,
}

/// A removal is pinned to the owned cell actually validated, not its live path.
/// Validation is product-specific; this type does not assert authentication.
pub(crate) struct ValidatedRemoval {
    key: (String, String),
    edition: u16,
    previous_update: Option<u16>,
    source_identity: Option<CellSourceIdentity>,
    record: CancellationRecord,
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CancellationRecord {
    pub key: (String, String),
    pub cancelled_edition: u16,
    pub cancellation_update: u16,
    pub issue_date: chrono::NaiveDate,
}
impl ValidatedRemoval {
    pub(crate) fn validate(
        cancellation: &ferrite_s101::cancellation::Cancellation,
        metadata: &ferrite_s101::cancellation::CancellationMetadata,
        purpose: ferrite_security::DatasetPurpose,
        loaded: &ferrite_s100_core::DatasetIdentification,
        source_identity: CellSourceIdentity,
        previous_issue_date: chrono::NaiveDate,
    ) -> Result<Self> {
        ensure!(
            purpose == ferrite_security::DatasetPurpose::Cancellation,
            "Catalogue purpose is not cancellation"
        );
        cancellation.validate_target(loaded, metadata, previous_issue_date)?;
        let key = s101_update_plan::dataset_key(loaded)?;
        Ok(Self {
            key: key.clone(),
            edition: loaded.edition_number,
            previous_update: Some(loaded.update_number),
            source_identity: Some(source_identity),
            record: CancellationRecord {
                key,
                cancelled_edition: metadata.edition,
                cancellation_update: metadata.update,
                issue_date: metadata.issue_date,
            },
        })
    }
}
impl ValidatedRemoval {
    pub(crate) fn announcement(
        cancellation: &ferrite_s101::cancellation::Cancellation,
        metadata: &ferrite_s101::cancellation::CancellationMetadata,
        purpose: ferrite_security::DatasetPurpose,
    ) -> Result<Self> {
        ensure!(
            purpose == ferrite_security::DatasetPurpose::Cancellation,
            "Catalogue purpose is not cancellation"
        );
        cancellation.validate_announcement(metadata)?;
        let key = s101_update_plan::dataset_key(&ferrite_s100_core::DatasetIdentification {
            product_identifier: "INT.IHO.S-101.2.0".into(),
            dataset_name: cancellation.dataset_name().into(),
            ..Default::default()
        })?;
        Ok(Self {
            key: key.clone(),
            edition: metadata.edition,
            previous_update: None,
            source_identity: None,
            record: CancellationRecord {
                key,
                cancelled_edition: metadata.edition,
                cancellation_update: metadata.update,
                issue_date: metadata.issue_date,
            },
        })
    }
    pub(crate) fn record(&self) -> &CancellationRecord {
        &self.record
    }
    pub(crate) fn removes_content(&self) -> bool {
        self.source_identity.is_some()
    }
    pub(crate) fn key(&self) -> &(String, String) {
        &self.key
    }
}

/// A small tombstone prevents name reuse with an older issue date. It retains
/// no chart geometry, attributes or cancelled file content.
#[derive(Default, Clone)]
pub(crate) struct CancellationHistory {
    by_dataset: BTreeMap<(String, String), CancellationRecord>,
}
impl CancellationHistory {
    pub(crate) fn record(&mut self, records: Vec<CancellationRecord>) {
        for record in records {
            match self.by_dataset.entry(record.key.clone()) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(record);
                }
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    if record.issue_date > entry.get().issue_date {
                        entry.insert(record);
                    }
                }
            }
        }
    }
    pub(crate) fn validate_cancellation(&self, record: &CancellationRecord) -> Result<()> {
        if let Some(previous) = self.by_dataset.get(&record.key) {
            ensure!(
                record.issue_date > previous.issue_date || record == previous,
                "Cancellation announcement is stale or conflicts with same-date history"
            );
        }
        Ok(())
    }
    pub(crate) fn validate_reuse(
        &self,
        id: &ferrite_s100_core::DatasetIdentification,
        issue_date: Option<chrono::NaiveDate>,
    ) -> Result<()> {
        if let Some(previous) = self.by_dataset.get(&s101_update_plan::dataset_key(id)?) {
            ensure!(
                id.edition_number > 0 && id.application_profile == "1",
                "Reused cancelled name requires a positive-edition base"
            );
            ensure!(
                issue_date.is_some_and(|date| date > previous.issue_date),
                "Reused cancelled name requires issueDate newer than cancellation history"
            );
        }
        Ok(())
    }
}

/// Serialized history is a local date/sequence guard, never an authentication
/// credential. The real current resources still require fresh security checks.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryDocument {
    version: u8,
    records: Vec<CancellationRecord>,
}
const MAX_HISTORY_BYTES: u64 = 4 * 1024 * 1024;
const MAX_HISTORY_RECORDS: usize = 10_000;
impl CancellationHistory {
    pub(crate) fn read(path: &std::path::Path) -> Result<Self> {
        use std::io::Read;
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default())
            }
            Err(error) => return Err(error.into()),
        };
        let mut bytes = Vec::new();
        file.take(MAX_HISTORY_BYTES + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= MAX_HISTORY_BYTES,
            "Cancellation history exceeds receiver budget"
        );
        let document: HistoryDocument = serde_json::from_slice(&bytes)?;
        ensure!(
            document.version == 1 && document.records.len() <= MAX_HISTORY_RECORDS,
            "Unsupported or excessive cancellation history"
        );
        let mut history = Self::default();
        for record in document.records {
            ensure!(
                record.key.0 == "INT.IHO.S-101.2.0"
                    && record.cancelled_edition > 0
                    && (1..=999).contains(&record.cancellation_update),
                "Invalid cancellation history identity"
            );
            let name = format!("{}.000", record.key.1);
            ensure!(
                record.key.1.starts_with("101")
                    && (8..=17).contains(&record.key.1.len())
                    && record
                        .key
                        .1
                        .bytes()
                        .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_'),
                "Invalid cancelled dataset stem"
            );
            ensure!(
                crate::s101_update_plan::is_chart_file(std::path::Path::new(&name)),
                "Invalid cancellation history filename"
            );
            ensure!(
                history
                    .by_dataset
                    .insert(record.key.clone(), record)
                    .is_none(),
                "Duplicate cancellation history key"
            );
        }
        Ok(history)
    }
    fn write_atomic(&self, path: &std::path::Path) -> Result<()> {
        use std::io::Write;
        ensure!(
            self.by_dataset.len() <= MAX_HISTORY_RECORDS,
            "Cancellation history record budget exceeded"
        );
        let bytes = serde_json::to_vec(&HistoryDocument {
            version: 1,
            records: self.by_dataset.values().cloned().collect(),
        })?;
        ensure!(
            bytes.len() as u64 <= MAX_HISTORY_BYTES,
            "Cancellation history byte budget exceeded"
        );
        let parent = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("History has no parent folder"))?;
        std::fs::create_dir_all(parent)?;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let temporary = parent.join(format!(".s101-history-{}-{nonce}.tmp", std::process::id()));
        let result = (|| -> Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&temporary, path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result
    }
}
/// Hold an OS file lock across fresh history validation, atomic file replacement
/// and the synchronous visible publication. No network or chart file is deleted.
pub(crate) struct HistoryStore {
    path: PathBuf,
    _lock: std::fs::File,
    pub history: CancellationHistory,
}
impl HistoryStore {
    pub(crate) fn begin(path: &std::path::Path) -> Result<Self> {
        let parent = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("History has no parent"))?;
        std::fs::create_dir_all(parent)?;
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path.with_extension("lock"))?;
        lock.lock()?;
        let history = CancellationHistory::read(path)?;
        Ok(Self {
            path: path.to_owned(),
            _lock: lock,
            history,
        })
    }
    pub(crate) fn persist(&mut self, records: Vec<CancellationRecord>) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        self.history.record(records);
        self.history.write_atomic(&self.path)
    }
}
pub(crate) fn default_history_path() -> PathBuf {
    if let Some(path) = std::env::var_os("FERRITE_S101_HISTORY_PATH") {
        return PathBuf::from(path);
    }
    if cfg!(test) || ferrite_wgpu::background_test::enabled() {
        return std::env::temp_dir()
            .join(format!("ferrite-s101-history-{}", std::process::id()))
            .join("history.json");
    }
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(target_os = "macos")]
    let base =
        std::env::var_os("HOME").map(|p| PathBuf::from(p).join("Library/Application Support"));
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/share")));
    base.unwrap_or_else(|| PathBuf::from("."))
        .join("FerriteS100/s101-cancellations.json")
}

#[must_use = "Commit only after portrayal preparation succeeds, or roll back"]
pub(crate) struct CellPublication {
    previous_len: usize,
    replaced: Vec<PreviousCell>,
    removed: Vec<(PreviousCell, CancellationRecord)>,
    announcements: Vec<CancellationRecord>,
}
impl CellPublication {
    /// Validate every key before mutating any cell. No unchanged cell is cloned.
    #[cfg(test)]
    pub(crate) fn stage(
        cells: &mut Vec<S101Cell>,
        identities: &mut LoadedSourceIdentities,
        paths: &mut Vec<Vec<PathBuf>>,
        incoming: Vec<ChartLoadResult>,
    ) -> Result<Self> {
        Self::stage_changes(cells, identities, paths, incoming, Vec::new())
    }
    /// Validate the complete batch, then compact all removals in one linear
    /// move pass. Retained cell geometry is never cloned.
    pub(crate) fn stage_changes(
        cells: &mut Vec<S101Cell>,
        identities: &mut LoadedSourceIdentities,
        paths: &mut Vec<Vec<PathBuf>>,
        incoming: Vec<ChartLoadResult>,
        removals: Vec<ValidatedRemoval>,
    ) -> Result<Self> {
        ensure!(
            cells.len() == identities.len() && cells.len() == paths.len(),
            "Cell publication state is misaligned"
        );
        let mut indices = BTreeMap::new();
        for (index, cell) in cells.iter().enumerate() {
            ensure!(
                indices
                    .insert(s101_update_plan::dataset_key(&cell.dsid)?, index)
                    .is_none(),
                "Duplicate loaded logical dataset"
            );
        }
        let keys = incoming
            .iter()
            .map(|r| s101_update_plan::dataset_key(&r.cell.dsid))
            .collect::<Result<Vec<_>>>()?;
        let mut unique = std::collections::BTreeSet::new();
        for key in &keys {
            ensure!(unique.insert(key), "Duplicate incoming logical dataset");
        }
        let mut remove_by_index = BTreeMap::new();
        let mut announcements = Vec::new();
        let mut cancellation_keys = std::collections::BTreeSet::new();
        for removal in removals {
            ensure!(
                cancellation_keys.insert(removal.key.clone()),
                "Duplicate cancellation target"
            );
            if !removal.removes_content() {
                ensure!(
                    !indices.contains_key(&removal.key) && !unique.contains(&removal.key),
                    "Cancellation absence changed after announcement validation"
                );
                announcements.push(removal.record);
                continue;
            }
            ensure!(
                !unique.contains(&removal.key),
                "Same dataset cannot be loaded and cancelled in one batch"
            );
            let index = *indices
                .get(&removal.key)
                .ok_or_else(|| anyhow::anyhow!("Cancellation target is not loaded"))?;
            let cell = &cells[index];
            ensure!(
                cell.dsid.edition_number == removal.edition
                    && Some(cell.dsid.update_number) == removal.previous_update
                    && identities.get(index) == removal.source_identity,
                "Cancellation target changed after validation"
            );
            ensure!(
                remove_by_index.insert(index, removal.record).is_none(),
                "Duplicate cancellation target"
            );
        }
        let mut transaction = Self {
            previous_len: cells.len() - remove_by_index.len(),
            replaced: Vec::new(),
            removed: Vec::new(),
            announcements,
        };
        if !remove_by_index.is_empty() {
            let old_cells = std::mem::take(cells);
            let old_ids = identities.take_all();
            let old_paths = std::mem::take(paths);
            cells.reserve(transaction.previous_len);
            paths.reserve(transaction.previous_len);
            indices.clear();
            for (index, ((cell, identity), chain_paths)) in old_cells
                .into_iter()
                .zip(old_ids)
                .zip(old_paths)
                .enumerate()
            {
                if let Some(record) = remove_by_index.remove(&index) {
                    transaction.removed.push((
                        PreviousCell {
                            index,
                            cell,
                            identity,
                            paths: chain_paths,
                        },
                        record,
                    ));
                } else {
                    // The key was already validated before any mutation.
                    let key = s101_update_plan::dataset_key(&cell.dsid).expect("Prevalidated key");
                    indices.insert(key, cells.len());
                    cells.push(cell);
                    identities.push(identity);
                    paths.push(chain_paths);
                }
            }
        }
        for (result, key) in incoming.into_iter().zip(keys) {
            if let Some(&index) = indices.get(&key) {
                transaction.replaced.push(PreviousCell {
                    index,
                    cell: std::mem::replace(&mut cells[index], result.cell),
                    identity: identities
                        .get(index)
                        .expect("Aligned identities checked before staging"),
                    paths: std::mem::replace(&mut paths[index], result.input_paths),
                });
                identities.replace(index, result.source_identity);
            } else {
                indices.insert(key, cells.len());
                cells.push(result.cell);
                identities.push(result.source_identity);
                paths.push(result.input_paths);
            }
        }
        Ok(transaction)
    }
    pub(crate) fn rollback(
        self,
        cells: &mut Vec<S101Cell>,
        identities: &mut LoadedSourceIdentities,
        paths: &mut Vec<Vec<PathBuf>>,
    ) {
        cells.truncate(self.previous_len);
        identities.truncate(self.previous_len);
        paths.truncate(self.previous_len);
        for previous in self.replaced {
            cells[previous.index] = previous.cell;
            identities.replace(previous.index, previous.identity);
            paths[previous.index] = previous.paths;
        }
        if !self.removed.is_empty() {
            let kept = std::mem::take(cells);
            let kept_ids = identities.take_all();
            let kept_paths = std::mem::take(paths);
            let total = kept.len() + self.removed.len();
            let mut kept = kept.into_iter().zip(kept_ids).zip(kept_paths);
            let mut removed = self.removed.into_iter().peekable();
            cells.reserve(total);
            paths.reserve(total);
            for index in 0..total {
                let (cell, identity, chain_paths) =
                    if removed.peek().is_some_and(|(old, _)| old.index == index) {
                        let (old, _) = removed.next().unwrap();
                        (old.cell, old.identity, old.paths)
                    } else {
                        let ((cell, identity), chain_paths) =
                            kept.next().expect("Aligned rollback storage");
                        (cell, identity, chain_paths)
                    };
                cells.push(cell);
                identities.push(identity);
                paths.push(chain_paths);
            }
        }
    }
    pub(crate) fn cancellations(&self) -> Vec<CancellationRecord> {
        self.removed
            .iter()
            .map(|(_, record)| record.clone())
            .chain(self.announcements.iter().cloned())
            .collect()
    }
    pub(crate) fn commit(self) -> Vec<CancellationRecord> {
        // Dropping old cells releases removed/replaced content. The caller
        // records tombstones only after visible portrayal preparation succeeds.
        self.removed
            .into_iter()
            .map(|(_, record)| record)
            .chain(self.announcements)
            .collect()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ferrite_s100_core::{Coordinate, PointRecord, RecordId};
    use std::{
        hash::Hasher,
        sync::atomic::{AtomicU64, Ordering},
    };
    fn cell(name: &str, marker: u8, update: u16) -> ChartLoadResult {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "ferrite-publish-{}-{}.000",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut bytes = b"000253LE1 0000025 ! 1104\x1e".to_vec();
        bytes[17] = marker;
        std::fs::write(&path, bytes).unwrap();
        let (mut cell, source_identity) = S101Cell::load_from_with_identity(&path, &path).unwrap();
        std::fs::remove_file(path).unwrap();
        cell.dsid.dataset_name = format!("{name}.000");
        cell.dsid.product_identifier = "INT.IHO.S-101.2.0".into();
        cell.dsid.application_profile = "1".into();
        cell.dsid.edition_number = 1;
        cell.dsid.update_number = update;
        let id = RecordId::new(110, 1);
        cell.points.insert(
            id.key(),
            PointRecord {
                id,
                position: Coordinate::new(f64::from(marker), 50.),
                update_instruction: 1,
            },
        );
        ChartLoadResult {
            base_metadata: None,
            metadata: None,
            cell,
            source_identity,
            input_paths: vec![format!("{name}.{update:03}").into()],
        }
    }
    fn state() -> (Vec<S101Cell>, LoadedSourceIdentities, Vec<Vec<PathBuf>>) {
        let mut cells = Vec::new();
        let mut ids = LoadedSourceIdentities::default();
        let mut paths = Vec::new();
        CellPublication::stage(
            &mut cells,
            &mut ids,
            &mut paths,
            vec![cell("A", b'A', 0), cell("B", b'B', 0)],
        )
        .unwrap()
        .commit();
        (cells, ids, paths)
    }
    fn digest(ids: &LoadedSourceIdentities) -> Vec<u64> {
        (0..ids.len())
            .map(|i| {
                let mut h = std::collections::hash_map::DefaultHasher::new();
                ids.hash_for_cell(i, &mut h).unwrap();
                h.finish()
            })
            .collect()
    }
    #[test]
    fn failed_portrayal_restores_replacement_append_source_identity_paths_and_unchanged_storage() {
        let (mut cells, mut ids, mut paths) = state();
        let before = digest(&ids);
        let old_paths = paths.clone();
        let unchanged = cells[1].points.values().next().unwrap() as *const PointRecord;
        let tx = CellPublication::stage(
            &mut cells,
            &mut ids,
            &mut paths,
            vec![cell("A", b'C', 1), cell("NEW", b'D', 0)],
        )
        .unwrap();
        assert_eq!(cells.len(), 3);
        assert_eq!(cells[0].dsid.update_number, 1);
        assert_ne!(digest(&ids)[0], before[0]);
        // An error from the separately staged portrayal must choose rollback.
        let prepared: Result<()> = Err(anyhow::anyhow!("Injected late coverage preparation error"));
        assert!(prepared.is_err());
        tx.rollback(&mut cells, &mut ids, &mut paths);
        assert_eq!(cells.len(), 2);
        assert_eq!(cells[0].dsid.update_number, 0);
        assert_eq!(digest(&ids), before);
        assert_eq!(paths, old_paths);
        assert_eq!(
            cells[1].points.values().next().unwrap() as *const PointRecord,
            unchanged
        );
    }
    #[test]
    fn commit_replaces_one_index_appends_once_and_keeps_unaffected_identity() {
        let (mut cells, mut ids, mut paths) = state();
        let before = digest(&ids);
        CellPublication::stage(
            &mut cells,
            &mut ids,
            &mut paths,
            vec![cell("A", b'C', 2), cell("NEW", b'D', 0)],
        )
        .unwrap()
        .commit();
        assert_eq!(cells.len(), 3);
        assert_eq!(cells[0].dsid.update_number, 2);
        assert_eq!(cells[1].dsid.dataset_name, "B.000");
        assert_eq!(digest(&ids)[1], before[1]);
        assert_ne!(digest(&ids)[0], before[0]);
        assert_eq!(paths[0], [PathBuf::from("A.002")]);
    }
    #[test]
    fn late_invalid_or_duplicate_key_never_partially_stages() {
        let (mut cells, mut ids, mut paths) = state();
        let before = digest(&ids);
        let old_paths = paths.clone();
        let mut bad = cell("INVALID", b'Z', 0);
        bad.cell.dsid.dataset_name = "../bad.000".into();
        assert!(CellPublication::stage(
            &mut cells,
            &mut ids,
            &mut paths,
            vec![cell("A", b'C', 1), bad]
        )
        .is_err());
        assert!(CellPublication::stage(
            &mut cells,
            &mut ids,
            &mut paths,
            vec![cell("A", b'C', 1), cell("A", b'D', 2)]
        )
        .is_err());
        assert_eq!(cells.len(), 2);
        assert_eq!(cells[0].dsid.update_number, 0);
        assert_eq!(digest(&ids), before);
        assert_eq!(paths, old_paths);
    }
    fn removal(cells: &[S101Cell], ids: &LoadedSourceIdentities, index: usize) -> ValidatedRemoval {
        let id = &cells[index].dsid;
        let key = s101_update_plan::dataset_key(id).unwrap();
        ValidatedRemoval {
            key: key.clone(),
            edition: id.edition_number,
            previous_update: Some(id.update_number),
            source_identity: Some(ids.get(index).unwrap()),
            record: CancellationRecord {
                key,
                cancelled_edition: id.edition_number,
                cancellation_update: id.update_number + 1,
                issue_date: chrono::NaiveDate::from_ymd_opt(2026, 10, 7).unwrap(),
            },
        }
    }
    fn announcement(name: &str) -> ValidatedRemoval {
        let cancellation = ferrite_s101::cancellation::Cancellation::parse(
            &physical_cancellation_fixture("2", "0", &format!("{name}.003")),
        )
        .unwrap();
        let metadata =
            ferrite_s101::cancellation::CancellationMetadata::new(4, Some(3), "2024-10-17")
                .unwrap();
        ValidatedRemoval::announcement(
            &cancellation,
            &metadata,
            ferrite_security::DatasetPurpose::Cancellation,
        )
        .unwrap()
    }
    #[test]
    fn absent_cancellation_preserves_unrelated_storage_and_commits_only_tombstone() {
        let (mut cells, mut ids, mut paths) = state();
        let before = digest(&ids);
        let old_paths = paths.clone();
        let address = cells[0].points.values().next().unwrap() as *const PointRecord;
        let removal = announcement("101AA00TEST");
        assert!(!removal.removes_content());
        let tx = CellPublication::stage_changes(
            &mut cells,
            &mut ids,
            &mut paths,
            Vec::new(),
            vec![removal],
        )
        .unwrap();
        assert_eq!(tx.cancellations().len(), 1);
        tx.rollback(&mut cells, &mut ids, &mut paths);
        assert_eq!(digest(&ids), before);
        assert_eq!(paths, old_paths);
        let tx = CellPublication::stage_changes(
            &mut cells,
            &mut ids,
            &mut paths,
            Vec::new(),
            vec![announcement("101AA00TEST")],
        )
        .unwrap();
        let records = tx.commit();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].key.1, "101AA00TEST");
        assert_eq!(cells.len(), 2);
        assert_eq!(
            cells[0].points.values().next().unwrap() as *const PointRecord,
            address
        );
        assert_eq!(digest(&ids), before);
        assert_eq!(paths, old_paths);
        let mut history = CancellationHistory::default();
        history.record(records);
        let mut reused = cells[0].dsid.clone();
        reused.dataset_name = "101AA00TEST.000".into();
        assert!(history
            .validate_reuse(
                &reused,
                Some(chrono::NaiveDate::from_ymd_opt(2024, 10, 17).unwrap())
            )
            .is_err());
        assert!(history
            .validate_reuse(
                &reused,
                Some(chrono::NaiveDate::from_ymd_opt(2024, 10, 18).unwrap())
            )
            .is_ok());
    }
    #[test]
    fn absent_cancellation_rejects_presence_race_batch_conflict_and_duplicate() {
        let (mut cells, mut ids, mut paths) = state();
        let before = digest(&ids);
        let old_paths = paths.clone();
        let mut present = announcement("101AA00TEST");
        present.key = s101_update_plan::dataset_key(&cells[0].dsid).unwrap();
        assert!(CellPublication::stage_changes(
            &mut cells,
            &mut ids,
            &mut paths,
            Vec::new(),
            vec![present]
        )
        .is_err());
        assert!(CellPublication::stage_changes(
            &mut cells,
            &mut ids,
            &mut paths,
            vec![cell("101AA00TEST", b'F', 0)],
            vec![announcement("101AA00TEST")]
        )
        .is_err());
        assert!(CellPublication::stage_changes(
            &mut cells,
            &mut ids,
            &mut paths,
            Vec::new(),
            vec![announcement("101AA00TEST"), announcement("101AA00TEST")]
        )
        .is_err());
        assert_eq!(digest(&ids), before);
        assert_eq!(paths, old_paths);
    }
    #[test]
    fn cancellation_history_allows_exact_replay_but_rejects_stale_or_conflicting_record() {
        let record = announcement("101AA00TEST").record;
        let mut history = CancellationHistory::default();
        history.record(vec![record.clone()]);
        assert!(history.validate_cancellation(&record).is_ok());
        let mut conflict = record.clone();
        conflict.cancellation_update += 1;
        assert!(history.validate_cancellation(&conflict).is_err());
        conflict.issue_date = chrono::NaiveDate::from_ymd_opt(2024, 10, 16).unwrap();
        assert!(history.validate_cancellation(&conflict).is_err());
        conflict.issue_date = chrono::NaiveDate::from_ymd_opt(2024, 10, 18).unwrap();
        assert!(history.validate_cancellation(&conflict).is_ok());
    }
    #[test]
    fn cancellation_failure_restores_original_order_paths_identities_and_unchanged_geometry() {
        let (mut cells, mut ids, mut paths) = state();
        CellPublication::stage(&mut cells, &mut ids, &mut paths, vec![cell("C", b'E', 0)])
            .unwrap()
            .commit();
        let before = digest(&ids);
        let old_paths = paths.clone();
        let unchanged = cells[1].points.values().next().unwrap() as *const PointRecord;
        let a = removal(&cells, &ids, 0);
        let c = removal(&cells, &ids, 2);
        let tx = CellPublication::stage_changes(
            &mut cells,
            &mut ids,
            &mut paths,
            vec![cell("NEW", b'F', 0)],
            vec![c, a],
        )
        .unwrap();
        assert_eq!(cells.len(), 2);
        assert_eq!(cells[0].dsid.dataset_name, "B.000");
        tx.rollback(&mut cells, &mut ids, &mut paths);
        assert_eq!(
            cells
                .iter()
                .map(|c| c.dsid.dataset_name.as_str())
                .collect::<Vec<_>>(),
            ["A.000", "B.000", "C.000"]
        );
        assert_eq!(paths, old_paths);
        assert_eq!(digest(&ids), before);
        assert_eq!(
            cells[1].points.values().next().unwrap() as *const PointRecord,
            unchanged
        );
    }
    #[test]
    fn cancellation_commit_drops_only_target_and_emits_history_after_success() {
        let (mut cells, mut ids, mut paths) = state();
        let before = digest(&ids);
        let a = removal(&cells, &ids, 0);
        let tx =
            CellPublication::stage_changes(&mut cells, &mut ids, &mut paths, Vec::new(), vec![a])
                .unwrap();
        let records = tx.commit();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].key.1, "A");
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].dsid.dataset_name, "B.000");
        assert_eq!(digest(&ids), [before[1]]);
        assert_eq!(paths[0], [PathBuf::from("B.000")]);
        let b = removal(&cells, &ids, 0);
        let records =
            CellPublication::stage_changes(&mut cells, &mut ids, &mut paths, Vec::new(), vec![b])
                .unwrap()
                .commit();
        assert_eq!(records.len(), 1);
        assert!(cells.is_empty());
        assert_eq!(ids.len(), 0);
        assert!(paths.is_empty());
    }
    #[test]
    fn cancellation_stale_identity_duplicate_and_load_conflict_reject_entire_batch() {
        let (mut cells, mut ids, mut paths) = state();
        let before = digest(&ids);
        let old_paths = paths.clone();
        let mut stale = removal(&cells, &ids, 0);
        stale.source_identity = Some(ids.get(1).unwrap());
        assert!(CellPublication::stage_changes(
            &mut cells,
            &mut ids,
            &mut paths,
            vec![cell("NEW", b'F', 0)],
            vec![stale]
        )
        .is_err());
        let a = removal(&cells, &ids, 0);
        let duplicate = removal(&cells, &ids, 0);
        assert!(CellPublication::stage_changes(
            &mut cells,
            &mut ids,
            &mut paths,
            Vec::new(),
            vec![a, duplicate]
        )
        .is_err());
        let a = removal(&cells, &ids, 0);
        assert!(CellPublication::stage_changes(
            &mut cells,
            &mut ids,
            &mut paths,
            vec![cell("A", b'F', 1)],
            vec![a]
        )
        .is_err());
        assert_eq!(digest(&ids), before);
        assert_eq!(paths, old_paths);
        assert_eq!(cells.len(), 2);
    }
    #[test]
    fn cancelled_name_reuse_requires_newer_date_and_positive_base_without_history_downgrade() {
        let (cells, ids, _) = state();
        let a = removal(&cells, &ids, 0);
        let mut history = CancellationHistory::default();
        let old = a.record.clone();
        history.record(vec![old.clone()]);
        let id = &cells[0].dsid;
        assert!(history.validate_reuse(id, None).is_err());
        assert!(history.validate_reuse(id, Some(old.issue_date)).is_err());
        let tomorrow = chrono::NaiveDate::from_ymd_opt(2026, 10, 8).unwrap();
        assert!(history.validate_reuse(id, Some(tomorrow)).is_ok());
        let mut older = old.clone();
        older.issue_date = chrono::NaiveDate::from_ymd_opt(2026, 10, 6).unwrap();
        history.record(vec![older]);
        assert!(history.validate_reuse(id, Some(old.issue_date)).is_err());
        let mut update = id.clone();
        update.application_profile = "2".into();
        assert!(history.validate_reuse(&update, Some(tomorrow)).is_err());
        let mut zero = id.clone();
        zero.edition_number = 0;
        assert!(history.validate_reuse(&zero, Some(tomorrow)).is_err());
        assert!(history.validate_reuse(&cells[1].dsid, None).is_ok());
    }
    pub(crate) fn physical_cancellation_fixture(
        profile: &str,
        edition: &str,
        filename: &str,
    ) -> Vec<u8> {
        let definition=b"000000000\x1fDataset identification\x1fRCNM!RCID!ENSP!ENED!PRSP!PRED!PROF!DSNM!DSTL!DSRD!DSLG!DSAB!DSED!*DSTC\x1f(b11,b14,7A(),A(8),3A(),b11)\x1e";
        let mut header = b"000003LE1 0000038 ! 4504".to_vec();
        header[..5].copy_from_slice(format!("{:05}", 38 + definition.len()).as_bytes());
        let mut bytes = header;
        bytes.extend_from_slice(format!("DSID{:04}00000", definition.len()).as_bytes());
        bytes.push(ferrite_iso8211::FIELD_TERMINATOR);
        bytes.extend(definition);
        let mut field = vec![10, 1, 0, 0, 0];
        field.extend_from_slice(format!("S-100 Part 10a\x1f5.2\x1fINT.IHO.S-101.2.0\x1f2.0\x1f{profile}\x1f{filename}\x1fCancelled\x1f20241016EN\x1f\x1f{edition}\x1f").as_bytes());
        field.extend([14, 18, ferrite_iso8211::FIELD_TERMINATOR]);
        let mut leader = b"000003DE1 0000038 ! 4504".to_vec();
        leader[..5].copy_from_slice(format!("{:05}", 38 + field.len()).as_bytes());
        bytes.extend(leader);
        bytes.extend_from_slice(format!("DSID{:04}00000", field.len()).as_bytes());
        bytes.push(ferrite_iso8211::FIELD_TERMINATOR);
        bytes.extend(field);
        bytes
    }

    #[test]
    fn physical_b7_factory_checks_purpose_base_profile_target_counter_date_and_pins_owned_source() {
        let (cells, ids, _) = state();
        let mut loaded = cells[0].dsid.clone();
        loaded.dataset_name = "101AA00TEST.000".into();
        loaded.product_edition = "2.0".into();
        loaded.edition_number = 4;
        loaded.update_number = 2;
        let cancellation = ferrite_s101::cancellation::Cancellation::parse(
            &physical_cancellation_fixture("2", "0", "101AA00TEST.003"),
        )
        .unwrap();
        let metadata =
            ferrite_s101::cancellation::CancellationMetadata::new(4, Some(3), "2024-10-17")
                .unwrap();
        let previous = chrono::NaiveDate::from_ymd_opt(2024, 10, 16).unwrap();
        let identity = ids.get(0).unwrap();
        let removal = ValidatedRemoval::validate(
            &cancellation,
            &metadata,
            ferrite_security::DatasetPurpose::Cancellation,
            &loaded,
            identity,
            previous,
        )
        .unwrap();
        assert_eq!(removal.source_identity, Some(identity));
        assert_eq!(removal.record.cancelled_edition, 4);
        assert_eq!(removal.record.cancellation_update, 3);
        assert!(ValidatedRemoval::validate(
            &cancellation,
            &metadata,
            ferrite_security::DatasetPurpose::Update,
            &loaded,
            identity,
            previous
        )
        .is_err());
        let mut update = loaded.clone();
        update.application_profile = "2".into();
        assert!(ValidatedRemoval::validate(
            &cancellation,
            &metadata,
            ferrite_security::DatasetPurpose::Cancellation,
            &update,
            identity,
            previous
        )
        .is_err());
        assert!(ValidatedRemoval::validate(
            &cancellation,
            &metadata,
            ferrite_security::DatasetPurpose::Cancellation,
            &loaded,
            identity,
            metadata.issue_date
        )
        .is_err());
    }
    #[test]
    fn mixed_remove_replace_append_rollback_restores_every_original_index() {
        let (mut cells, mut ids, mut paths) = state();
        CellPublication::stage(&mut cells, &mut ids, &mut paths, vec![cell("C", b'E', 0)])
            .unwrap()
            .commit();
        let before = digest(&ids);
        let old_paths = paths.clone();
        let a = removal(&cells, &ids, 0);
        let tx = CellPublication::stage_changes(
            &mut cells,
            &mut ids,
            &mut paths,
            vec![cell("C", b'F', 1), cell("NEW", b'G', 0)],
            vec![a],
        )
        .unwrap();
        assert_eq!(cells[1].dsid.update_number, 1);
        assert_eq!(cells[2].dsid.dataset_name, "NEW.000");
        tx.rollback(&mut cells, &mut ids, &mut paths);
        assert_eq!(
            cells
                .iter()
                .map(|c| c.dsid.dataset_name.as_str())
                .collect::<Vec<_>>(),
            ["A.000", "B.000", "C.000"]
        );
        assert_eq!(cells[2].dsid.update_number, 0);
        assert_eq!(paths, old_paths);
        assert_eq!(digest(&ids), before);
    }
    #[test]
    fn persistent_history_survives_reopen_and_refuses_corrupt_or_duplicate_records() {
        let folder =
            std::env::temp_dir().join(format!("ferrite-history-test-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let path = folder.join("history.json");
        let record = CancellationRecord {
            key: ("INT.IHO.S-101.2.0".into(), "101AA00TEST".into()),
            cancelled_edition: 4,
            cancellation_update: 3,
            issue_date: chrono::NaiveDate::from_ymd_opt(2024, 10, 17).unwrap(),
        };
        let mut store = HistoryStore::begin(&path).unwrap();
        store.persist(vec![record.clone()]).unwrap();
        drop(store);
        let reopened = CancellationHistory::read(&path).unwrap();
        assert_eq!(reopened.by_dataset.get(&record.key), Some(&record));
        let id = ferrite_s100_core::DatasetIdentification {
            dataset_name: "101AA00TEST.000".into(),
            product_identifier: "INT.IHO.S-101.2.0".into(),
            application_profile: "1".into(),
            edition_number: 1,
            ..Default::default()
        };
        assert!(reopened
            .validate_reuse(&id, Some(record.issue_date))
            .is_err());
        assert!(reopened
            .validate_reuse(&id, record.issue_date.succ_opt())
            .is_ok());
        std::fs::write(
            &path,
            serde_json::to_vec(&HistoryDocument {
                version: 1,
                records: vec![record.clone(), record],
            })
            .unwrap(),
        )
        .unwrap();
        assert!(CancellationHistory::read(&path).is_err());
        std::fs::write(&path, b"truncated").unwrap();
        assert!(CancellationHistory::read(&path).is_err());
        std::fs::remove_dir_all(folder).unwrap();
    }
    #[test]
    fn history_write_error_is_before_commit_and_can_restore_staged_chart() {
        let (mut cells, mut ids, mut paths) = state();
        let before = digest(&ids);
        let old_paths = paths.clone();
        let a = removal(&cells, &ids, 0);
        let tx =
            CellPublication::stage_changes(&mut cells, &mut ids, &mut paths, Vec::new(), vec![a])
                .unwrap();
        let folder =
            std::env::temp_dir().join(format!("ferrite-history-failure-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        // Destination is a directory, so rename of the staged regular file fails.
        let path = folder.join("destination-directory");
        std::fs::create_dir_all(&path).unwrap();
        let mut history = CancellationHistory::default();
        history.record(tx.cancellations());
        assert!(history.write_atomic(&path).is_err());
        tx.rollback(&mut cells, &mut ids, &mut paths);
        assert_eq!(digest(&ids), before);
        assert_eq!(paths, old_paths);
        assert_eq!(cells.len(), 2);
        assert!(std::fs::read_dir(&folder).unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp")));
        std::fs::remove_dir_all(folder).unwrap();
    }
}
