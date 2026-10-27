//! The S-101 stage of a host open job ([`ferrite_product_api::open`]):
//! chain members are taken by name ([`super::S101`]), planned by the update
//! planner (authorized selections and folders, physical cancellations) or,
//! from bytes without signature verification, grouped by dataset
//! identification; then materialized in parallel, validated against their
//! catalogue owner and installed with the validated cancellations.
//! Moved from the engine facade (`ChartEngine`'s open job and install).
use super::catalogue_selection::{
    DatasetCatalogueOwner, DatasetCatalogueRegistry, DatasetCatalogueRequest,
};
use super::decoded_cache::SharedDecodedChartCache;
use super::publication::{CancellationHistory, CancellationRecord, HistoryStore, ValidatedRemoval};
use super::update_plan as s101_update_plan;
use super::{CellHydrationSource, ChartLoadResult, Inserted, S101Layer, PRODUCT, S101};
use anyhow::{bail, ensure, Context, Result};
use ferrite_feature_catalog::BoundFeatureCatalogue;
use ferrite_portrayal_catalog::BoundPortrayalCatalogue;
use ferrite_product_api::open::{InstallContext, Loaded, OpenContext, OpenNotes, OpenStage};
use ferrite_product_api::platform::parallel_map;
use ferrite_product_api::report::{
    DatasetId, DatasetSummary, InstallReport, IssueSeverity, ProductReport, ValidationIssue,
};
use ferrite_product_api::{ProductAdapter, Recognised};
use ferrite_s100_core::{DatasetIdentification, S101Cell};
use std::any::Any;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// What an S-101 stage loaded.
pub struct S101Opened {
    cells: Vec<(ChartLoadResult, String)>,
    removals: Vec<(PathBuf, ValidatedRemoval)>,
    /// Source digests of the bound catalogue pair the cells were admitted
    /// with.
    catalogues: ([u8; 32], [u8; 32]),
}

/// The S-101 stage: what the job needs of the layer, taken on the owner
/// thread.
pub struct S101Stage {
    fc: Arc<BoundFeatureCatalogue>,
    pc: Arc<BoundPortrayalCatalogue>,
    decoded: SharedDecodedChartCache,
    /// Installed S-101 catalogue inventory (empty: none).
    inventory: PathBuf,
    /// Loaded logical datasets when the job started (cancellation targets).
    loaded: s101_update_plan::LoadedDatasets,
    /// Inputs of the loaded chains (predecessors of selected updates).
    loaded_inputs: Vec<PathBuf>,
    charts: Vec<PathBuf>,
}

/// One S-101 chain read into memory.
struct ByteChain {
    base: (PathBuf, Vec<u8>),
    updates: Vec<Vec<u8>>,
}

impl S101Stage {
    pub fn new(layer: &S101Layer) -> Self {
        Self {
            fc: Arc::clone(&layer.fc),
            pc: Arc::clone(&layer.pc),
            decoded: Arc::clone(&layer.decoded_chart_cache),
            inventory: layer.catalogue_inventory.clone(),
            loaded: layer.loaded_datasets(),
            loaded_inputs: layer.loaded_chain_paths.iter().flatten().cloned().collect(),
            charts: Vec::new(),
        }
    }
}

impl OpenStage for S101Stage {
    /// Chain members by name, as the update planner selects them (the head
    /// is not read: a member with a damaged leader is reported by loading).
    fn sniff(&self, name: &Path, head: &[u8]) -> Option<Recognised> {
        S101.sniff(name, head)
    }
    fn accept(&mut self, path: &Path, _: &OpenContext<'_>, _: &mut OpenNotes) -> bool {
        self.charts.push(path.to_path_buf());
        true
    }
    fn counted_inputs(&self) -> usize {
        self.charts.len()
    }
    fn load(
        mut self: Box<Self>,
        context: &OpenContext<'_>,
        notes: &mut OpenNotes,
    ) -> Result<Loaded> {
        let charts = std::mem::take(&mut self.charts);
        let (cells, removals) = if charts.is_empty() {
            (Vec::new(), Vec::new())
        } else if context.data.local_paths() || context.required() {
            self.load_planned_charts(charts, context, notes)?
        } else {
            (self.load_byte_charts(charts, context, notes)?, Vec::new())
        };
        let counts = vec![("cells", cells.len()), ("cancellations", removals.len())];
        Ok(Loaded {
            payload: Box::new(S101Opened {
                cells,
                removals,
                catalogues: (*self.fc.source_digest(), *self.pc.source_digest()),
            }),
            counts,
        })
    }
}

fn authorized(
    context: &OpenContext<'_>,
    paths: &[PathBuf],
) -> Result<ferrite_security::AuthorizedDatasets> {
    (context.authorize)(paths)?
        .downcast::<ferrite_security::AuthorizedDatasets>()
        .map(|report| *report)
        .map_err(|_| anyhow::anyhow!("Host authorization is not a signature report"))
}

impl S101Stage {
    /// The viewer's authorized planner and loader, over any data source:
    /// local files (private snapshot files) or bytes (owned copies). An
    /// explicit selection also plans its physical cancellations.
    #[allow(clippy::type_complexity)]
    fn load_planned_charts(
        &self,
        charts: Vec<PathBuf>,
        context: &OpenContext<'_>,
        notes: &mut OpenNotes,
    ) -> Result<(
        Vec<(ChartLoadResult, String)>,
        Vec<(PathBuf, ValidatedRemoval)>,
    )> {
        let data = context.data;
        let explicit = context.explicit;
        let require = context.required();
        // An explicit selection also opens the base and predecessors of a
        // selected update (as the viewer does).
        let selected = charts.clone();
        // Planned inputs are canonical paths, the keys of their authorization
        // and lifecycle metadata.
        let charts = if explicit {
            s101_update_plan::expand_candidates_in(data, &charts, &self.loaded_inputs)?
        } else {
            let mut canonical = Vec::with_capacity(charts.len());
            for path in charts {
                match data.canonicalize(&path) {
                    Ok(path) => canonical.push(path),
                    Err(error) => notes
                        .notices
                        .push(format!("Skipped S-101 input {}: {error}", path.display())),
                }
            }
            canonical
        };
        let authorization = authorized(context, &charts)?;
        let provenance = if require {
            "signature verified"
        } else {
            "signature not verified"
        };
        let (plans, cancellations, plan_notices) = if explicit {
            let record = super::publication::history_record(context.user);
            let history = match &record {
                Some(record) => CancellationHistory::read_record(record.as_ref())?,
                None => CancellationHistory::default(),
            };
            let (plans, mut cancellations, mut plan_notices) =
                s101_update_plan::authorized_selection_plans_in(
                    data,
                    charts,
                    &selected,
                    &authorization,
                    require,
                    &self.loaded,
                    &history,
                )?;
            if record.is_none() {
                for plan in cancellations.drain(..) {
                    plan_notices.push(format!(
                        "Skipped S-101 cancellation {}: this host keeps no cancellation history",
                        plan.original().display()
                    ));
                }
            }
            (plans, cancellations, plan_notices)
        } else {
            let (plans, plan_notices) = s101_update_plan::authorized_folder_plans_in(
                data,
                charts,
                &authorization,
                require,
            )?;
            (plans, Vec::new(), plan_notices)
        };
        notes.notices.extend(plan_notices);
        let mut removals = Vec::new();
        for plan in cancellations {
            context.check_cancelled()?;
            let path = plan.original().to_path_buf();
            match plan.load() {
                Ok(removal) => removals.push((path, removal)),
                Err(error) => notes.issues.push(ValidationIssue {
                    severity: IssueSeverity::Error,
                    subject: path.display().to_string(),
                    check: "S-101 cancellation".into(),
                    message: format!("{error:#}"),
                }),
            }
        }
        let codes = Arc::new(self.fc.feature_type_codes());
        let owners = self.inventory_owners(&plans, context, &mut notes.notices);
        let plans: Vec<_> = plans.into_iter().map(Arc::new).collect();
        let results = parallel_map(context.executor, &plans, |plan| {
            if context.cancelled() {
                bail!("Open cancelled");
            }
            let (mut cell, source_identity) = plan.load_with_decoded_cache(&self.decoded)?;
            // Datasets of other catalogue editions: their installed owner.
            let owner = owners.as_ref().and_then(|(registry, codes)| {
                let key = S101Layer::catalogue_dataset_key(&plan.base.id).ok()?;
                let owner = registry.owner_for(&key)?;
                Some((owner, codes.get(owner.fc().source_digest())?))
            });
            let codes = match owner {
                Some((owner, owner_codes)) => {
                    S101Layer::admit_with(&mut cell, owner.fc(), owner.pc())?;
                    Arc::clone(owner_codes)
                }
                None => {
                    S101Layer::admit_with(&mut cell, &self.fc, &self.pc)?;
                    Arc::clone(&codes)
                }
            };
            cell.normalize_feature_codes(&codes);
            Ok(ChartLoadResult {
                base_metadata: plan.base.metadata.clone(),
                metadata: plan.ending_metadata().cloned(),
                input_paths: plan.input_paths(),
                cell,
                source_identity,
                hydration: Some(CellHydrationSource {
                    plan: Arc::clone(plan),
                    codes: vec![Arc::clone(&codes)],
                }),
            })
        });
        // Retain authenticated snapshots until every plan was parsed.
        drop(authorization);
        let mut cells = Vec::new();
        for (plan, result) in plans.iter().zip(results) {
            match result {
                Ok(result) => cells.push((result, provenance.to_string())),
                Err(error) => {
                    context.check_cancelled()?;
                    notes.issues.push(load_issue(&plan.base.original, error));
                }
            }
        }
        Ok((cells, removals))
    }

    /// Catalogue owners from the installed inventory for planned datasets
    /// the bound pair cannot process (as the viewer's loader stages them),
    /// with each owner's feature type codes. None without an inventory or
    /// when no dataset needs one; a failed selection is a notice (those
    /// datasets are then rejected by the bound pair).
    #[allow(clippy::type_complexity)]
    fn inventory_owners(
        &self,
        plans: &[s101_update_plan::Plan],
        context: &OpenContext<'_>,
        notices: &mut Vec<String>,
    ) -> Option<(
        DatasetCatalogueRegistry,
        BTreeMap<[u8; 32], Arc<Vec<String>>>,
    )> {
        if self.inventory.as_os_str().is_empty()
            || plans.iter().all(|plan| {
                crate::validate_dataset_catalogues(
                    &plan.base.id,
                    &self.fc,
                    &self.pc.product_id,
                    &self.pc.version,
                )
                .is_ok()
            })
        {
            return None;
        }
        let staged = (|| -> Result<DatasetCatalogueRegistry> {
            let requests = plans
                .iter()
                .map(|plan| {
                    Ok(DatasetCatalogueRequest {
                        dataset_key: S101Layer::catalogue_dataset_key(&plan.base.id)?,
                        source_identity: plan.captured_source_identity_sha(),
                        identification: plan.base.id.clone(),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let fallback = Arc::new(DatasetCatalogueOwner::new(
                Arc::clone(&self.fc),
                Arc::clone(&self.pc),
            )?);
            DatasetCatalogueRegistry::default().stage_in(
                context.data,
                &requests,
                fallback,
                &self.inventory,
            )
        })();
        let registry = match staged {
            Ok(registry) => registry,
            Err(error) => {
                notices.push(format!("Installed catalogue selection failed: {error:#}"));
                return None;
            }
        };
        let mut codes = BTreeMap::new();
        for plan in plans {
            let Some(owner) = S101Layer::catalogue_dataset_key(&plan.base.id)
                .ok()
                .and_then(|key| registry.owner_for(&key))
            else {
                continue;
            };
            codes
                .entry(*owner.fc().source_digest())
                .or_insert_with(|| Arc::new(owner.fc().feature_type_codes()));
        }
        Some((registry, codes))
    }

    /// Any source: chains planned from the dataset identifications and
    /// materialized from bytes (no signature verification).
    fn load_byte_charts(
        &self,
        charts: Vec<PathBuf>,
        context: &OpenContext<'_>,
        notes: &mut OpenNotes,
    ) -> Result<Vec<(ChartLoadResult, String)>> {
        let data = context.data;
        let max = ferrite_s100_core::updates::UpdateLimits::default().max_dataset_bytes as u64;
        let mut inputs = Vec::new();
        let mut total = 0u64;
        for path in charts {
            context.check_cancelled()?;
            let read = (|| -> Result<(DatasetIdentification, Vec<u8>)> {
                let bytes = data.read_limited(&path, max)?;
                ensure!(
                    bytes.len() as u64 <= max,
                    "S-101 input byte budget exceeded"
                );
                let id = ferrite_s100_core::inspect_dataset_identification_bytes(&bytes)?;
                Ok((id, bytes))
            })();
            match read {
                Ok((id, bytes)) => {
                    total += bytes.len() as u64;
                    ensure!(total <= max, "S-101 aggregate input budget exceeded");
                    inputs.push((path, id, bytes));
                }
                Err(error) => notes
                    .notices
                    .push(format!("Skipped S-101 input {}: {error:#}", path.display())),
            }
        }
        let chains = plan_byte_chains(inputs, &mut notes.notices);
        let codes = Arc::new(self.fc.feature_type_codes());
        let chains: Vec<Mutex<Option<ByteChain>>> =
            chains.into_iter().map(|c| Mutex::new(Some(c))).collect();
        let results = parallel_map(context.executor, &chains, |chain| {
            let chain = chain
                .lock()
                .ok()
                .and_then(|mut c| c.take())
                .context("Chain taken")?;
            let path = chain.base.0.clone();
            let run = || -> Result<ChartLoadResult> {
                if context.cancelled() {
                    bail!("Open cancelled");
                }
                let (mut cell, source_identity) = S101Cell::load_update_chain_bytes_with_identity(
                    &chain.base.0,
                    chain.base.1,
                    chain.updates,
                )?;
                S101Layer::admit_with(&mut cell, &self.fc, &self.pc)?;
                cell.normalize_feature_codes(&codes);
                Ok(ChartLoadResult {
                    base_metadata: None,
                    metadata: None,
                    input_paths: vec![path.clone()],
                    cell,
                    source_identity,
                    hydration: None,
                })
            };
            Ok::<_, anyhow::Error>((path.clone(), run()))
        });
        let mut cells = Vec::new();
        for result in results {
            let (path, result) = result?;
            match result {
                Ok(result) => cells.push((result, "signature not verified".to_string())),
                Err(error) => {
                    context.check_cancelled()?;
                    notes.issues.push(load_issue(&path, error));
                }
            }
        }
        Ok(cells)
    }
}

fn load_issue(path: &Path, error: anyhow::Error) -> ValidationIssue {
    ValidationIssue {
        severity: IssueSeverity::Error,
        subject: path.display().to_string(),
        check: "S-101 load".into(),
        message: format!("{error:#}"),
    }
}

/// An S-101 input read into memory with its identification.
type ByteInput = (PathBuf, DatasetIdentification, Vec<u8>);

/// Group inputs by logical dataset and select the latest edition's base
/// (or reissue) and its consecutive updates, as the S-101 update planner
/// does for authorized inputs. Rejected groups become notices.
fn plan_byte_chains(inputs: Vec<ByteInput>, notices: &mut Vec<String>) -> Vec<ByteChain> {
    let mut groups: BTreeMap<(String, String), Vec<ByteInput>> = BTreeMap::new();
    for input in inputs {
        match s101_update_plan::dataset_key(&input.1) {
            Ok(key) => groups.entry(key).or_default().push(input),
            Err(error) => notices.push(format!(
                "Skipped invalid S-101 dataset identity {}: {error:#}",
                input.0.display()
            )),
        }
    }
    let counter = |id: &DatasetIdentification| -> Option<u16> {
        Path::new(&id.dataset_name)
            .extension()?
            .to_str()?
            .parse()
            .ok()
    };
    let mut chains = Vec::new();
    for ((product, name), mut group) in groups {
        let planned = (|| -> Result<ByteChain> {
            ensure!(
                group.iter().all(|(_, id, _)| id.edition_number > 0),
                "Cancellation datasets need an explicit removal operation"
            );
            let edition = group
                .iter()
                .map(|(_, id, _)| id.edition_number)
                .max()
                .context("Empty group")?;
            group.retain(|(_, id, _)| id.edition_number == edition);
            let base_counter = group
                .iter()
                .filter(|(_, id, _)| id.application_profile == "1")
                .filter_map(|(_, id, _)| counter(id))
                .max()
                .context("Update chain has no profile-1 base in its latest edition")?;
            let mut bases = Vec::new();
            let mut updates = BTreeMap::new();
            for (path, id, bytes) in group {
                let Some(n) = counter(&id) else {
                    bail!("Missing dataset counter: {}", path.display());
                };
                if id.application_profile == "1" && n == base_counter {
                    bases.push((path, bytes));
                } else if id.application_profile == "2" && n > base_counter {
                    ensure!(updates.insert(n, bytes).is_none(), "Duplicate update {n}");
                }
            }
            ensure!(bases.len() == 1, "Ambiguous duplicate base/reissue");
            let mut expected = base_counter;
            for n in updates.keys() {
                expected = expected.checked_add(1).context("Update counter overflow")?;
                ensure!(*n == expected, "Missing update {expected} before {n}");
            }
            Ok(ByteChain {
                base: bases.pop().expect("one base"),
                updates: updates.into_values().collect(),
            })
        })();
        match planned {
            Ok(chain) => chains.push(chain),
            Err(error) => {
                notices.push(format!("Skipped S-101 dataset {product}/{name}: {error:#}"))
            }
        }
    }
    chains
}

fn opened_of(opened: &(dyn Any + Send)) -> Result<&S101Opened> {
    opened
        .downcast_ref::<S101Opened>()
        .context("Not an S-101 open result")
}

/// The cells were admitted with the bound catalogue pair still bound.
pub(crate) fn check_opened(layer: &S101Layer, opened: &(dyn Any + Send)) -> Result<()> {
    ensure!(
        opened_of(opened)?.catalogues == (*layer.fc.source_digest(), *layer.pc.source_digest()),
        "Bound catalogues changed while loading; reopen the datasets"
    );
    Ok(())
}

/// Validate the job's cancellations against the current cells and a
/// fresh, locked cancellation history, persist them, then remove the
/// cancelled cells. Nothing changes when any check or the write fails.
pub(crate) fn apply_cancellations(
    layer: &mut S101Layer,
    opened: &mut (dyn Any + Send),
    context: &InstallContext<'_>,
    report: &mut InstallReport,
) -> Result<()> {
    let opened = opened
        .downcast_mut::<S101Opened>()
        .context("Not an S-101 open result")?;
    let removals = std::mem::take(&mut opened.removals);
    let incoming = &opened.cells;
    let record = super::publication::history_record(context.user);
    if removals.is_empty() {
        // Reused cancelled names need a newer issueDate (fresh history).
        if !incoming.is_empty() {
            if let Some(record) = record {
                let history = CancellationHistory::read_record(record.as_ref())?;
                for (result, _) in incoming {
                    history.validate_reuse(
                        &result.cell.dsid,
                        result.base_metadata.as_ref().map(|m| m.issue_date),
                    )?;
                }
            }
        }
        return Ok(());
    }
    let record = record.context("This host keeps no S-101 cancellation history")?;
    let mut store = HistoryStore::begin_record(record)?;
    let incoming_keys = incoming
        .iter()
        .map(|(result, _)| s101_update_plan::dataset_key(&result.cell.dsid))
        .collect::<Result<HashSet<_>>>()?;
    let mut targets = Vec::new();
    for (path, removal) in &removals {
        store.history.validate_cancellation(removal.record())?;
        ensure!(
            !incoming_keys.contains(removal.key()),
            "Same dataset cannot be loaded and cancelled in one batch: {}",
            path.display()
        );
        let index = layer.cells.iter().position(|cell| {
            s101_update_plan::dataset_key(&cell.dsid).is_ok_and(|key| &key == removal.key())
        });
        if removal.removes_content() {
            let index = index.with_context(|| {
                format!("Cancellation target is not loaded: {}", path.display())
            })?;
            ensure!(
                removal.targets(
                    &layer.cells[index].dsid,
                    layer.loaded_source_identities.get(index)
                ),
                "Cancellation target changed after validation: {}",
                path.display()
            );
            ensure!(!targets.contains(&index), "Duplicate cancellation target");
            targets.push(index);
        } else {
            ensure!(
                index.is_none(),
                "Cancellation absence changed after announcement validation: {}",
                path.display()
            );
        }
    }
    for (result, _) in incoming {
        store.history.validate_reuse(
            &result.cell.dsid,
            result.base_metadata.as_ref().map(|m| m.issue_date),
        )?;
    }
    let records: Vec<CancellationRecord> =
        removals.iter().map(|(_, r)| r.record().clone()).collect();
    store.persist(records.clone())?;
    targets.sort_unstable();
    for index in targets.into_iter().rev() {
        let cell = layer.remove_dataset(index);
        if index < layer.provenance.len() {
            layer.provenance.remove(index);
        }
        report.notices.push(format!(
            "Cancelled S-101 dataset {} (edition {}, update {})",
            cell.dsid.dataset_name, cell.dsid.edition_number, cell.dsid.update_number
        ));
    }
    for (path, removal) in &removals {
        if !removal.removes_content() {
            report.notices.push(format!(
                "Recorded S-101 cancellation announcement {} (no loaded content)",
                path.display()
            ));
        }
    }
    report.products.extend(
        records
            .into_iter()
            .map(|record| ProductReport::new(PRODUCT, "cancellation", record)),
    );
    Ok(())
}

/// Install the loaded cells as one batch (the same identity is already
/// open; a newer edition replaces the loaded dataset) and report the new
/// ones.
pub(crate) fn install(
    layer: &mut S101Layer,
    opened: Box<dyn Any + Send>,
    _context: &InstallContext<'_>,
    report: &mut InstallReport,
) -> Result<()> {
    let opened = opened
        .downcast::<S101Opened>()
        .map_err(|_| anyhow::anyhow!("Not an S-101 open result"))?;
    let mut added = 0usize;
    let mut batch = layer.begin_batch();
    for (result, provenance) in opened.cells {
        match layer.insert_loaded(&mut batch, result)? {
            Inserted::AlreadyOpen(path) => {
                report
                    .notices
                    .push(format!("Already open: {}", path.display()));
            }
            Inserted::Added { replaced } => {
                if let Some(index) = replaced {
                    if index < layer.provenance.len() {
                        layer.provenance.remove(index);
                    }
                }
                layer.provenance.push(provenance);
                added += 1;
            }
        }
    }
    layer.finish_batch()?;
    let first_new = layer.cells.len() - added;
    for index in first_new..layer.cells.len() {
        report.datasets.push(layer.cell_summary(index));
    }
    Ok(())
}

impl S101Layer {
    /// The summary of loaded cell `index` in the common report model.
    pub fn cell_summary(&self, index: usize) -> DatasetSummary {
        let cell = &self.cells[index];
        DatasetSummary {
            id: DatasetId::Cell(index),
            product: PRODUCT.to_string(),
            name: cell.dsid.dataset_name.clone(),
            source: Some(cell.file_path.clone()),
            edition: format!("{}.{}", cell.dsid.edition_number, cell.dsid.update_number),
            features: self.cell_feature_count(index),
            provenance: self.provenance.get(index).cloned().unwrap_or_default(),
        }
    }
}

/// Exchange-set checks of the loaded datasets: the discovery metadata of
/// each dataset's exchange catalogue (`CATALOG.XML`) against the
/// materialized cell (XC profile, effective scales, regional coverage).
/// Disagreements are issues; outcomes and datasets without catalogue
/// metadata are notices.
pub(crate) fn exchange_set_checks(layer: &S101Layer) -> (Vec<ValidationIssue>, Vec<String>) {
    let mut issues = Vec::new();
    let mut notices = Vec::new();
    for cell in &layer.cells {
        let subject = cell.file_path.display().to_string();
        let metadata = s101_update_plan::dataset_key(&cell.dsid)
            .ok()
            .and_then(|key| layer.loaded_discovery.get(&key))
            .and_then(Option::as_ref);
        let Some(metadata) = metadata else {
            notices.push(format!(
                "{}: no exchange catalogue metadata; exchange-set checks not applied",
                cell.dsid.dataset_name
            ));
            continue;
        };
        let mut issue = |check: &str, error: anyhow::Error| {
            issues.push(ValidationIssue {
                severity: IssueSeverity::Error,
                subject: subject.clone(),
                check: check.into(),
                message: format!("{error:#}"),
            })
        };
        if let Err(error) = metadata.validate_xc_profile(&cell.dsid) {
            issue("XC profile", error);
        }
        match metadata.validate_effective_xc_scales(cell) {
            Ok(status) => notices.push(format!(
                "{}: exchange catalogue scales {status:?}",
                cell.dsid.dataset_name
            )),
            Err(error) => issue("XC scales", error),
        }
        match metadata.validate_xc_regions(cell) {
            Ok(status) => notices.push(format!(
                "{}: exchange catalogue coverage {status:?}",
                cell.dsid.dataset_name
            )),
            Err(error) => issue("XC coverage", error),
        }
    }
    (issues, notices)
}
