//! Version-specific S-101 catalogue selection. Does not confer authentication.
//! S98 20.1/20.2 and Appendix B: absence/incompatibility is an error, not a
//! warning permission to process Edition 1 with Edition 2 catalogues.
use anyhow::{ensure, Context, Result};
use ferrite_feature_catalog::{BoundFeatureCatalogue, FeatureCatalogue};
use ferrite_kernel::SpecificationVersion;
use ferrite_portrayal_catalog::{BoundPortrayalCatalogue, PortrayalCatalogue};
use ferrite_s100_core::DatasetIdentification;
use std::path::{Path, PathBuf};

pub(crate) enum Selection {
    KeepCurrent,
    /// Owner must stage the complete pair/cache/UI change transactionally.
    /// Never attach either catalogue to a cell already portrayed with another pair.
    UseInstalled {
        fc: Box<BoundFeatureCatalogue>,
        pc: Box<BoundPortrayalCatalogue>,
        notice: String,
    },
}
pub(crate) fn dataset_version(d: &DatasetIdentification) -> Result<SpecificationVersion> {
    let pred = d.product_edition.parse::<SpecificationVersion>()?;
    let prsp = d
        .product_identifier
        .strip_prefix("INT.IHO.S-101.")
        .context("Unsupported dataset product identifier")?
        .parse::<SpecificationVersion>()?;
    ensure!(
        pred == prsp,
        "Dataset PRSP and PRED specification versions disagree"
    );
    Ok(pred)
}
fn required_version(
    incoming: &[DatasetIdentification],
    loaded: &[DatasetIdentification],
) -> Result<Option<SpecificationVersion>> {
    let mut target: Option<SpecificationVersion> = None;
    for d in incoming.iter().chain(loaded) {
        let v = dataset_version(d)?;
        if let Some(old) = target {
            ensure!(old.edition == v.edition,
                "Mixed S-101 Editions require per-dataset catalogue owners; this build uses one pair. Clear existing charts and select one compatible Edition.");
            target = Some(old.max(v));
        } else {
            target = Some(v);
        }
    }
    Ok(target)
}
fn pair_matches(
    target: SpecificationVersion,
    fc_product: &str,
    fc_version: &str,
    pc_product: &str,
    pc_version: &str,
) -> Result<bool> {
    if fc_product != "S-101" || pc_product != "S-101" {
        return Ok(false);
    }
    let fc = fc_version.parse::<SpecificationVersion>()?;
    let pc = pc_version.parse::<SpecificationVersion>()?;
    // S-101 section 1.6.3 permits each catalogue to process earlier Revisions
    // in its Edition. Clarifications do not affect this compatibility.
    Ok([fc, pc].into_iter().all(|v| {
        v.require_same_edition_backward_revision_compatibility(target)
            .is_ok()
    }))
}
/// Local inventory hint only. Actual parsed FC/PC identity and product validation
/// decide compatibility; directory names are never authentication or schema proof.
/// No mutation, copying, fetching, signature bypass, or dataset rewriting.
#[cfg(test)]
fn resolve_pair_for_loading(
    incoming: &[DatasetIdentification],
    loaded: &[DatasetIdentification],
    current_fc: &FeatureCatalogue,
    current_pc: &PortrayalCatalogue,
    inventory: &Path,
) -> Result<Selection> {
    resolve_pair_with_policy(incoming, loaded, current_fc, current_pc, inventory, false)
}
/// New owner admission only. Retained compatible owners are not automatically rebound.
/// Prefer exact Edition+Revision captured FC/PC; S-101 same-Edition later Revision
/// compatibility remains an explicit fallback, not a generic S-100 rule.
pub(crate) fn resolve_pair_exact_first(
    incoming: &[DatasetIdentification],
    loaded: &[DatasetIdentification],
    current_fc: &FeatureCatalogue,
    current_pc: &PortrayalCatalogue,
    inventory: &Path,
) -> Result<Selection> {
    resolve_pair_with_policy(incoming, loaded, current_fc, current_pc, inventory, true)
}
fn exact_revision_pair(
    target: SpecificationVersion,
    fc: &FeatureCatalogue,
    pc: &PortrayalCatalogue,
) -> Result<bool> {
    let exact = |version: &str| -> Result<bool> {
        let v = version.parse::<SpecificationVersion>()?;
        Ok((v.edition, v.revision) == (target.edition, target.revision))
    };
    Ok(fc.product_id == "S-101"
        && pc.product_id == "S-101"
        && exact(&fc.version)?
        && exact(&pc.version)?)
}
fn resolve_pair_with_policy(
    incoming: &[DatasetIdentification],
    loaded: &[DatasetIdentification],
    current_fc: &FeatureCatalogue,
    current_pc: &PortrayalCatalogue,
    inventory: &Path,
    exact_first: bool,
) -> Result<Selection> {
    // Preserve an already compatible later Revision in the current Edition.
    // S-101 1.6.3 permits independent later FC/PC revisions; do not downgrade
    // a valid installed pair merely because the generic S98 inventory is stricter.
    let current_exact = if exact_first && !incoming.is_empty() {
        required_version(incoming, loaded)?
            .map(|target| exact_revision_pair(target, current_fc, current_pc))
            .transpose()?
            .unwrap_or(true)
    } else {
        true
    };
    let current_compatible = ferrite_s101::validate_catalogue_pair(current_fc, current_pc).is_ok()
        && incoming.iter().chain(loaded).all(|d| {
            ferrite_s101::validate_dataset_catalogues(
                d,
                current_fc,
                &current_pc.product_id,
                &current_pc.version,
            )
            .is_ok()
        });
    if incoming.is_empty() || ((!exact_first || current_exact) && current_compatible) {
        return Ok(Selection::KeepCurrent);
    }
    let Some(target) = required_version(incoming, loaded)? else {
        return Ok(Selection::KeepCurrent);
    };
    if exact_first && current_compatible && !inventory.is_dir() {
        return Ok(Selection::KeepCurrent);
    }
    ensure!(
        inventory.is_dir(),
        "SSE131: No installed compatible S-101 FC/PC inventory at {}",
        inventory.display()
    );
    let mut folders = Vec::new();
    for (n, e) in std::fs::read_dir(inventory)?.enumerate() {
        ensure!(n < 256, "Installed catalogue inventory exceeds256 entries");
        let e = e?;
        if !e.file_type()?.is_dir() {
            continue;
        }
        let Some(hint) = e
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<SpecificationVersion>().ok())
        else {
            continue;
        };
        if hint.edition == target.edition {
            folders.push((hint, e.path()));
        }
    }
    folders.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let mut compatible_fallback = None;
    for (_, folder) in folders {
        let fc_dir = folder.join("FC");
        let pc_dir = folder.join("PC");
        if !fc_dir.is_dir() || !pc_dir.is_dir() {
            continue;
        }
        let mut files: Vec<PathBuf> = Vec::new();
        for (n, e) in std::fs::read_dir(&fc_dir)?.enumerate() {
            ensure!(n < 256, "Installed FC inventory exceeds256 entries");
            let e = e?;
            if e.file_type()?.is_file()
                && e.path()
                    .extension()
                    .is_some_and(|x| x.eq_ignore_ascii_case("xml"))
                && !e.file_name().as_encoded_bytes().starts_with(b"._")
            {
                files.push(e.path());
            }
        }
        ensure!(
            files.len() == 1,
            "SSE131: {} needs exactly one explicit FC XML",
            fc_dir.display()
        );
        let fc = FeatureCatalogue::load_bound(&files[0])
            .with_context(|| format!("Installed FC could not be read: {}", files[0].display()))?;
        let pc = PortrayalCatalogue::load_bound(&pc_dir)
            .with_context(|| format!("Installed PC could not be read: {}", pc_dir.display()))?;
        if !pair_matches(
            target,
            &fc.product_id,
            &fc.version,
            &pc.product_id,
            &pc.version,
        )? {
            continue;
        }
        ferrite_s101::validate_catalogue_pair(&fc, &pc)?;
        for d in incoming.iter().chain(loaded) {
            ferrite_s101::validate_dataset_catalogues(d, &fc, &pc.product_id, &pc.version)?;
        }
        let notice=format!("Using installed S-101 FC {} / PC {} for dataset version {}.{}.{}. Signature verification policy is unchanged.",fc.version,pc.version,target.edition,target.revision,target.clarification);
        let exact = exact_revision_pair(target, &fc, &pc)?;
        let selected = Selection::UseInstalled {
            fc: Box::new(fc),
            pc: Box::new(pc),
            notice,
        };
        if !exact_first || exact {
            return Ok(selected);
        }
        if compatible_fallback.is_none() {
            compatible_fallback = Some(selected);
        }
    }
    if exact_first && current_compatible {
        return Ok(Selection::KeepCurrent);
    }
    if let Some(selected) = compatible_fallback {
        return Ok(selected);
    }
    anyhow::bail!("SSE133: Dataset S-101 {}.{}.{} has no compatible installed FC/PC pair; current FC {} / PC {}. Install same-Edition catalogues with sufficient Revisions; incompatible data is not portrayed.",target.edition,target.revision,target.clarification,current_fc.version,current_pc.version)
}
/// Immutable captured pair; this binding confers no dataset authentication.
pub(crate) struct DatasetCatalogueOwner {
    fc: std::sync::Arc<BoundFeatureCatalogue>,
    pc: std::sync::Arc<BoundPortrayalCatalogue>,
}
impl DatasetCatalogueOwner {
    pub(crate) fn new(
        fc: std::sync::Arc<BoundFeatureCatalogue>,
        pc: std::sync::Arc<BoundPortrayalCatalogue>,
    ) -> Result<Self> {
        ferrite_s101::validate_catalogue_pair(&fc, &pc)?;
        Ok(Self { fc, pc })
    }
    pub(crate) fn fc(&self) -> &std::sync::Arc<BoundFeatureCatalogue> {
        &self.fc
    }
    pub(crate) fn pc(&self) -> &std::sync::Arc<BoundPortrayalCatalogue> {
        &self.pc
    }
    pub(crate) fn digests(&self) -> ([u8; 32], [u8; 32]) {
        (*self.fc.source_digest(), *self.pc.source_digest())
    }
    fn accepts(&self, d: &DatasetIdentification) -> bool {
        ferrite_s101::validate_dataset_catalogues(
            d,
            &self.fc,
            &self.pc.product_id,
            &self.pc.version,
        )
        .is_ok()
    }
}
/// Caller supplies the captured parsed input identity, never a later live-path hash.
pub(crate) struct DatasetCatalogueRequest {
    pub(crate) dataset_key: String,
    pub(crate) source_identity: [u8; 32],
    pub(crate) identification: DatasetIdentification,
}
#[derive(Clone)]
struct DatasetCatalogueBinding {
    source_identity: [u8; 32],
    owner: std::sync::Arc<DatasetCatalogueOwner>,
}
/// Private staging value. Failure cannot change a published registry.
#[derive(Clone, Default)]
pub(crate) struct DatasetCatalogueRegistry {
    bindings: std::collections::BTreeMap<String, DatasetCatalogueBinding>,
}
/// Opaque loading snapshot. No live-path lookup and no pointer-as-authority hash.
pub(crate) struct DatasetCatalogueCheckpoint {
    rows: Vec<CatalogueCheckpointRow>,
}
struct CatalogueCheckpointRow {
    key: String,
    source: [u8; 32],
    digests: ([u8; 32], [u8; 32]),
    owner: std::sync::Arc<DatasetCatalogueOwner>,
}
impl DatasetCatalogueRegistry {
    pub(crate) fn checkpoint(&self) -> DatasetCatalogueCheckpoint {
        DatasetCatalogueCheckpoint {
            rows: self
                .bindings
                .iter()
                .map(|(key, binding)| CatalogueCheckpointRow {
                    key: key.clone(),
                    source: binding.source_identity,
                    digests: binding.owner.digests(),
                    owner: std::sync::Arc::clone(&binding.owner),
                })
                .collect(),
        }
    }
    pub(crate) fn matches_checkpoint(&self, checkpoint: &DatasetCatalogueCheckpoint) -> bool {
        self.bindings.len() == checkpoint.rows.len()
            && self
                .bindings
                .iter()
                .zip(&checkpoint.rows)
                .all(|((key, binding), old)| {
                    key == &old.key
                        && binding.source_identity == old.source
                        && binding.owner.digests() == old.digests
                        && std::sync::Arc::ptr_eq(&binding.owner, &old.owner)
                })
    }
    pub(crate) fn owner_for(
        &self,
        dataset_key: &str,
    ) -> Option<&std::sync::Arc<DatasetCatalogueOwner>> {
        self.bindings.get(dataset_key).map(|b| &b.owner)
    }
    pub(crate) fn binding_identity(
        &self,
        dataset_key: &str,
    ) -> Option<([u8; 32], [u8; 32], [u8; 32])> {
        self.bindings.get(dataset_key).map(|b| {
            let (fc, pc) = b.owner.digests();
            (b.source_identity, fc, pc)
        })
    }
    /// Exact retained keys only; cell ordering must be changed atomically by App.
    pub(crate) fn retain_keys(&mut self, keys: &std::collections::BTreeSet<String>) {
        self.bindings.retain(|key, _| keys.contains(key));
    }
    pub(crate) fn stage(
        &self,
        requests: &[DatasetCatalogueRequest],
        fallback: std::sync::Arc<DatasetCatalogueOwner>,
        inventory: &Path,
    ) -> Result<Self> {
        ensure!(
            requests.len() <= 4096 && self.bindings.len() <= 4096,
            "Dataset catalogue registry limit exceeded"
        );
        let mut seen = std::collections::BTreeSet::new();
        let mut groups = std::collections::BTreeMap::<u32, Vec<&DatasetCatalogueRequest>>::new();
        // Validate the entire request list before inventory I/O or candidate changes.
        for request in requests {
            ensure!(
                !request.dataset_key.is_empty() && request.dataset_key.len() <= 1024,
                "Invalid dataset catalogue key"
            );
            ensure!(
                seen.insert(&request.dataset_key),
                "Duplicate dataset catalogue key"
            );
            let version = dataset_version(&request.identification)?;
            groups.entry(version.edition).or_default().push(request);
        }
        let additions = requests
            .iter()
            .filter(|r| !self.bindings.contains_key(&r.dataset_key))
            .count();
        ensure!(
            self.bindings
                .len()
                .checked_add(additions)
                .is_some_and(|n| n <= 4096),
            "Dataset catalogue registry limit exceeded"
        );
        let mut candidate = self.clone();
        for group in groups.values() {
            let mut unresolved = Vec::new();
            for request in group {
                if let Some(old) = self
                    .owner_for(&request.dataset_key)
                    .filter(|owner| owner.accepts(&request.identification))
                {
                    candidate.bindings.insert(
                        request.dataset_key.clone(),
                        DatasetCatalogueBinding {
                            source_identity: request.source_identity,
                            owner: std::sync::Arc::clone(old),
                        },
                    );
                } else {
                    unresolved.push(*request);
                }
            }
            if unresolved.is_empty() {
                continue;
            }
            let matches_all = |owner: &DatasetCatalogueOwner| {
                unresolved.iter().all(|r| owner.accepts(&r.identification))
            };
            let retained = self
                .bindings
                .values()
                .map(|b| &b.owner)
                .find(|owner| matches_all(owner));
            let owner = if let Some(owner) = retained {
                std::sync::Arc::clone(owner)
            } else if matches_all(&fallback) {
                std::sync::Arc::clone(&fallback)
            } else {
                let ids: Vec<_> = unresolved
                    .iter()
                    .map(|r| r.identification.clone())
                    .collect();
                match resolve_pair_exact_first(&ids, &[], &fallback.fc, &fallback.pc, inventory)? {
                    Selection::KeepCurrent => std::sync::Arc::clone(&fallback),
                    Selection::UseInstalled { fc, pc, .. } => {
                        std::sync::Arc::new(DatasetCatalogueOwner::new(
                            std::sync::Arc::new(*fc),
                            std::sync::Arc::new(*pc),
                        )?)
                    }
                }
            };
            for request in unresolved {
                ferrite_s101::validate_dataset_catalogues(
                    &request.identification,
                    &owner.fc,
                    &owner.pc.product_id,
                    &owner.pc.version,
                )?;
                candidate.bindings.insert(
                    request.dataset_key.clone(),
                    DatasetCatalogueBinding {
                        source_identity: request.source_identity,
                        owner: std::sync::Arc::clone(&owner),
                    },
                );
            }
        }
        ensure!(
            candidate.bindings.len() <= 4096,
            "Dataset catalogue registry limit exceeded"
        );
        let mut owners: Vec<&std::sync::Arc<DatasetCatalogueOwner>> = Vec::new();
        for binding in candidate.bindings.values() {
            if !owners
                .iter()
                .any(|owner| std::sync::Arc::ptr_eq(owner, &binding.owner))
            {
                owners.push(&binding.owner);
            }
        }
        ensure!(
            owners.len() <= 256,
            "Dataset catalogue owner limit exceeded"
        );
        Ok(candidate)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn d(v: &str) -> DatasetIdentification {
        DatasetIdentification {
            product_identifier: format!("INT.IHO.S-101.{v}"),
            product_edition: v.into(),
            ..Default::default()
        }
    }
    #[test]
    fn actual_ukho_version_selects_edition_one_not_two() {
        let t = required_version(&[d("1.0")], &[]).unwrap().unwrap();
        assert!(pair_matches(t, "S-101", "1.0.2", "S-101", "1.0.2").unwrap());
        assert!(!pair_matches(t, "S-101", "2.0.0", "S-101", "2.0.0").unwrap());
    }
    #[test]
    fn mixed_editions_fail_and_revision_requirement_preserves_actual_versions() {
        assert!(required_version(&[d("1.0")], &[d("2.0")]).is_err());
        assert_eq!(
            required_version(&[d("2.1")], &[d("2.0")]).unwrap(),
            Some("2.1".parse().unwrap())
        );
    }
    #[test]
    fn prsp_pred_disagreement_and_otherproduct_rejected() {
        let mut x = d("1.0");
        x.product_edition = "2.0".into();
        assert!(dataset_version(&x).is_err());
        x.product_identifier = "INT.IHO.S-102.1.0".into();
        assert!(dataset_version(&x).is_err());
    }
    #[test]
    fn clarification_equivalence_preserves_independent_pair_and_revision_gates() {
        let t = required_version(&[d("1.0.1"), d("1.0.2")], &[])
            .unwrap()
            .unwrap();
        assert_eq!(t.clarification, 2);
        assert!(pair_matches(t, "S-101", "1.0.3", "S-101", "1.0.2").unwrap());
        assert!(pair_matches(t, "S-101", "1.0.1", "S-101", "1.0.2").unwrap());
        assert!(pair_matches(t, "S-101", "1.1.0", "S-101", "1.0.2").unwrap());
        assert!(!pair_matches("1.1".parse().unwrap(), "S-101", "1.1.0", "S-101", "1.0.2").unwrap());
    }
    #[test]
    fn empty_input_keeps_current_owner() {
        assert_eq!(required_version(&[], &[]).unwrap(), None);
    }

    struct Fixture {
        root: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "s101-matching-pair-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&root).unwrap();
            Self { root }
        }
        fn pair(
            &self,
            path: &str,
            fc_version: &str,
            pc_version: &str,
        ) -> (BoundFeatureCatalogue, BoundPortrayalCatalogue) {
            let root = self.root.join(path);
            std::fs::create_dir_all(root.join("FC")).unwrap();
            std::fs::create_dir_all(root.join("PC/Rules")).unwrap();
            std::fs::write(root.join("FC/FC.xml"),format!("<S100_FC_FeatureCatalogue><name>Bound test</name><versionNumber>{fc_version}</versionNumber><productId>S-101</productId></S100_FC_FeatureCatalogue>")).unwrap();
            std::fs::write(root.join("PC/portrayal_catalogue.xml"),format!("<portrayalCatalog productId='S-101' version='{pc_version}'><foundationMode/><displayModes><displayMode id='Base'><description><name>Base</name></description></displayMode><displayMode id='Standard'><description><name>Standard</name></description></displayMode><displayMode id='Other'><description><name>Other</name></description></displayMode></displayModes><displayPlanes><displayPlane id='OverRadar' order='1'/></displayPlanes></portrayalCatalog>")).unwrap();
            std::fs::write(root.join("PC/Rules/main.lua"), "return 'owned-original-A'").unwrap();
            (
                FeatureCatalogue::load_bound(&root.join("FC/FC.xml")).unwrap(),
                PortrayalCatalogue::load_bound(&root.join("PC")).unwrap(),
            )
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
    fn owner(f: &Fixture, path: &str, fc: &str, pc: &str) -> std::sync::Arc<DatasetCatalogueOwner> {
        let (fc, pc) = f.pair(path, fc, pc);
        std::sync::Arc::new(
            DatasetCatalogueOwner::new(std::sync::Arc::new(fc), std::sync::Arc::new(pc)).unwrap(),
        )
    }
    fn request(key: &str, version: &str, identity: u8) -> DatasetCatalogueRequest {
        DatasetCatalogueRequest {
            dataset_key: key.into(),
            source_identity: [identity; 32],
            identification: d(version),
        }
    }
    #[test]
    fn mixed_owner_stage_preserves_retained_snapshot_and_source_identity() {
        let f = Fixture::new();
        let first = owner(&f, "first", "1.0.2", "1.0.2");
        f.pair("inventory/2.0.0", "2.0.0", "2.0.0");
        let old = DatasetCatalogueRegistry::default()
            .stage(
                &[request("UKHO", "1.0", 1)],
                first.clone(),
                &f.root.join("absent"),
            )
            .unwrap();
        let next = old
            .stage(
                &[request("SHOM", "2.0", 2)],
                first.clone(),
                &f.root.join("inventory"),
            )
            .unwrap();
        assert!(std::sync::Arc::ptr_eq(
            next.owner_for("UKHO").unwrap(),
            &first
        ));
        assert_eq!(next.owner_for("SHOM").unwrap().fc().version, "2.0.0");
        assert_eq!(next.owner_for("SHOM").unwrap().pc().version, "2.0.0");
        assert_eq!(next.binding_identity("UKHO").unwrap().0, [1; 32]);
        assert!(old.owner_for("SHOM").is_none());
        let rebound = next
            .stage(
                &[request("UKHO", "1.0", 3)],
                first.clone(),
                &f.root.join("absent"),
            )
            .unwrap();
        assert!(std::sync::Arc::ptr_eq(
            rebound.owner_for("UKHO").unwrap(),
            &first
        ));
        assert_eq!(rebound.binding_identity("UKHO").unwrap().0, [3; 32]);
    }
    #[test]
    fn missing_inventory_and_bad_request_leave_original_owner_untouched() {
        let f = Fixture::new();
        let first = owner(&f, "first", "1.0.2", "1.0.2");
        let old = DatasetCatalogueRegistry::default()
            .stage(
                &[request("UKHO", "1.0", 1)],
                first.clone(),
                &f.root.join("absent"),
            )
            .unwrap();
        assert!(old
            .stage(
                &[request("SHOM", "2.0", 2)],
                first.clone(),
                &f.root.join("absent")
            )
            .is_err());
        assert!(old
            .stage(
                &[request("UKHO", "1.0", 2), request("UKHO", "1.0", 3)],
                first.clone(),
                &f.root
            )
            .is_err());
        assert_eq!(old.binding_identity("UKHO").unwrap().0, [1; 32]);
        assert!(std::sync::Arc::ptr_eq(
            old.owner_for("UKHO").unwrap(),
            &first
        ));
    }
    #[test]
    fn revision_upgrade_changes_only_incompatible_dataset_and_removal_is_explicit() {
        let f = Fixture::new();
        let first = owner(&f, "first", "2.0.0", "2.0.0");
        f.pair("inventory/2.1.0", "2.1.0", "2.1.0");
        let old = DatasetCatalogueRegistry::default()
            .stage(&[request("A", "2.0", 1)], first.clone(), &f.root)
            .unwrap();
        let mut next = old
            .stage(
                &[request("B", "2.1", 2)],
                first.clone(),
                &f.root.join("inventory"),
            )
            .unwrap();
        assert!(std::sync::Arc::ptr_eq(next.owner_for("A").unwrap(), &first));
        assert_eq!(next.owner_for("B").unwrap().fc().version, "2.1.0");
        next.retain_keys(&std::collections::BTreeSet::from(["B".to_owned()]));
        assert!(next.owner_for("A").is_none());
        assert!(old.owner_for("A").is_some());
    }
    #[test]
    fn exact_first_does_not_choose_latest_compatible_inventory_hint() {
        let f = Fixture::new();
        let (fc, pc) = f.pair("current", "1.0.2", "1.0.2");
        f.pair("inventory/2.2.0", "2.2.0", "2.2.0");
        f.pair("inventory/2.0.1", "2.0.1", "2.0.1");
        let Selection::UseInstalled { fc, pc, .. } =
            resolve_pair_exact_first(&[d("2.0.9")], &[], &fc, &pc, &f.root.join("inventory"))
                .unwrap()
        else {
            panic!("expected new owner")
        };
        assert_eq!(fc.version, "2.0.1");
        assert_eq!(pc.version, "2.0.1");
    }
    #[test]
    fn checkpoint_rejects_source_change_removal_and_fresh_equal_digest_owner() {
        let f = Fixture::new();
        let first = owner(&f, "first", "1.0.2", "1.0.2");
        let old = DatasetCatalogueRegistry::default()
            .stage(&[request("A", "1.0", 1)], first.clone(), &f.root)
            .unwrap();
        let checkpoint = old.checkpoint();
        assert!(old.clone().matches_checkpoint(&checkpoint));
        let changed = old
            .stage(&[request("A", "1.0", 2)], first, &f.root)
            .unwrap();
        assert!(!changed.matches_checkpoint(&checkpoint));
        let replacement = owner(&f, "first", "1.0.2", "1.0.2");
        let equal_bytes = DatasetCatalogueRegistry::default()
            .stage(&[request("A", "1.0", 1)], replacement, &f.root)
            .unwrap();
        assert_eq!(old.binding_identity("A"), equal_bytes.binding_identity("A"));
        assert!(!equal_bytes.matches_checkpoint(&checkpoint));
        let mut removed = old.clone();
        removed.retain_keys(&Default::default());
        assert!(!removed.matches_checkpoint(&checkpoint));
    }
    #[test]
    fn valid_later_revisions_keep_current_pair_without_inventory() {
        let f = Fixture::new();
        let (fc, pc) = f.pair("current", "2.1.0", "2.2.0");
        assert!(matches!(
            resolve_pair_for_loading(&[d("2.0")], &[d("2.1.0")], &fc, &pc, &f.root.join("absent"))
                .unwrap(),
            Selection::KeepCurrent
        ));
    }
    #[test]
    fn installed_legacy_pair_is_bound_to_captured_bytes_not_later_live_edits() {
        let f = Fixture::new();
        let (fc, pc) = f.pair("current", "2.0.0", "2.0.0");
        f.pair("inventory/1.0.2", "1.0.2", "1.0.2");
        let Selection::UseInstalled {
            fc: next_fc,
            pc: next_pc,
            notice,
        } = resolve_pair_for_loading(&[d("1.0")], &[], &fc, &pc, &f.root.join("inventory"))
            .unwrap()
        else {
            panic!("Wrong Edition retained")
        };
        assert_eq!(next_fc.version, "1.0.2");
        assert_eq!(next_pc.version, "1.0.2");
        assert!(notice.contains("1.0.2"));
        let old_digest = *next_pc.source_digest();
        std::fs::write(
            f.root.join("inventory/1.0.2/PC/Rules/main.lua"),
            "return 'live-B'",
        )
        .unwrap();
        std::fs::remove_file(f.root.join("inventory/1.0.2/FC/FC.xml")).unwrap();
        assert_eq!(*next_pc.source_digest(), old_digest);
        assert_eq!(
            &*next_pc
                .sources()
                .read_relative(Path::new("Rules/main.lua"))
                .unwrap(),
            b"return 'owned-original-A'"
        );
        assert_eq!(next_fc.version, "1.0.2");
        assert_eq!(fc.version, "2.0.0");
        assert_eq!(pc.version, "2.0.0");
    }
    #[test]
    fn earlier_clarification_bound_inventory_is_compatible_with_newer_data() {
        let f = Fixture::new();
        let (fc, pc) = f.pair("current", "2.0.0", "2.0.0");
        f.pair("inventory/1.0.0", "1.0.0", "1.0.0");
        let Selection::UseInstalled {
            fc: next_fc,
            pc: next_pc,
            ..
        } = resolve_pair_for_loading(&[d("1.0.9")], &[], &fc, &pc, &f.root.join("inventory"))
            .unwrap()
        else {
            panic!("Earlier clarification was incorrectly rejected")
        };
        assert_eq!(next_fc.version, "1.0.0");
        assert_eq!(next_pc.version, "1.0.0");
    }
    #[test]
    fn misleading_inventory_folder_cannot_force_wrong_edition() {
        let f = Fixture::new();
        let (fc, pc) = f.pair("current", "2.0.0", "2.0.0");
        f.pair("inventory/1.0.2", "2.0.0", "2.0.0");
        let error = resolve_pair_for_loading(&[d("1.0")], &[], &fc, &pc, &f.root.join("inventory"))
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("SSE133"));
        assert_eq!(fc.version, "2.0.0");
    }
    #[test]
    fn inventory_uses_independent_later_revisions_in_same_edition() {
        let f = Fixture::new();
        let (fc, pc) = f.pair("current", "1.0.2", "1.0.2");
        f.pair("inventory/2.1.0", "2.1.0", "2.2.0");
        let Selection::UseInstalled { fc, pc, .. } =
            resolve_pair_for_loading(&[d("2.0.9")], &[], &fc, &pc, &f.root.join("inventory"))
                .unwrap()
        else {
            panic!("Compatible later revisions were not found")
        };
        assert_eq!(fc.version, "2.1.0");
        assert_eq!(pc.version, "2.2.0");
    }
    #[test]
    fn inventory_satisfies_maximum_incoming_and_loaded_revision() {
        let f = Fixture::new();
        let (fc, pc) = f.pair("current", "2.0.0", "2.0.0");
        f.pair("inventory/2.2.0", "2.2.0", "2.1.0");
        let Selection::UseInstalled { fc, pc, .. } = resolve_pair_for_loading(
            &[d("2.0.9")],
            &[d("2.1.3")],
            &fc,
            &pc,
            &f.root.join("inventory"),
        )
        .unwrap() else {
            panic!("Mixed compatible revisions could not share a pair")
        };
        assert_eq!(fc.version, "2.2.0");
        assert_eq!(pc.version, "2.1.0");
    }
    #[test]
    fn each_catalogue_must_cover_maximum_dataset_revision() {
        for (fv, pv) in [("2.0.9", "2.2.0"), ("2.2.0", "2.0.9")] {
            let f = Fixture::new();
            let (fc, pc) = f.pair("current", "2.0.0", "2.0.0");
            f.pair("inventory/2.2.0", fv, pv);
            let error =
                resolve_pair_for_loading(&[d("2.1")], &[], &fc, &pc, &f.root.join("inventory"))
                    .err()
                    .unwrap();
            assert!(format!("{error:#}").contains("SSE133"));
        }
    }
    #[test]
    fn folder_hint_does_not_override_actual_revision_compatibility() {
        let f = Fixture::new();
        let (fc, pc) = f.pair("current", "1.0.2", "1.0.2");
        f.pair("inventory/2.0.0", "2.1.0", "2.2.0");
        let Selection::UseInstalled { fc, pc, .. } =
            resolve_pair_for_loading(&[d("2.1")], &[], &fc, &pc, &f.root.join("inventory"))
                .unwrap()
        else {
            panic!("Actual compatible catalogue identities were ignored")
        };
        assert_eq!(fc.version, "2.1.0");
        assert_eq!(pc.version, "2.2.0");
    }
}
