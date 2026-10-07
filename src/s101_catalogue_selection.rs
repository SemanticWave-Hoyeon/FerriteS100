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
fn dataset_version(d: &DatasetIdentification) -> Result<SpecificationVersion> {
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
            ensure!((old.edition,old.revision)==(v.edition,v.revision),
                "Mixed S-101 Editions/Revisions require per-dataset catalogue owners; this build uses one pair. Clear existing charts and select one compatible product version.");
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
    // Conservative subset common to S98 same-Revision and S101 later-revision
    // compatibility. S98 20.2 permits either Clarification direction.
    Ok([fc, pc]
        .into_iter()
        .all(|v| v.edition == target.edition && v.revision == target.revision))
}
/// Local inventory hint only. Actual parsed FC/PC identity and product validation
/// decide compatibility; directory names are never authentication or schema proof.
/// No mutation, copying, fetching, signature bypass, or dataset rewriting.
pub(crate) fn resolve_pair_for_loading(
    incoming: &[DatasetIdentification],
    loaded: &[DatasetIdentification],
    current_fc: &FeatureCatalogue,
    current_pc: &PortrayalCatalogue,
    inventory: &Path,
) -> Result<Selection> {
    // Preserve an already compatible later Revision in the current Edition.
    // S-101 1.6.3 permits independent later FC/PC revisions; do not downgrade
    // a valid installed pair merely because the generic S98 inventory is stricter.
    if incoming.is_empty()
        || (ferrite_s101::validate_catalogue_pair(current_fc, current_pc).is_ok()
            && incoming.iter().chain(loaded).all(|d| {
                ferrite_s101::validate_dataset_catalogues(
                    d,
                    current_fc,
                    &current_pc.product_id,
                    &current_pc.version,
                )
                .is_ok()
            }))
    {
        return Ok(Selection::KeepCurrent);
    }
    let Some(target) = required_version(incoming, loaded)? else {
        return Ok(Selection::KeepCurrent);
    };
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
        if hint.edition == target.edition && hint.revision == target.revision {
            folders.push((hint, e.path()));
        }
    }
    folders.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
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
        return Ok(Selection::UseInstalled {
            fc: Box::new(fc),
            pc: Box::new(pc),
            notice,
        });
    }
    anyhow::bail!("SSE133: Dataset S-101 {}.{}.{} has no compatible installed FC/PC pair; current FC {} / PC {}. Install matching Edition/Revision catalogues; incompatible data is not portrayed.",target.edition,target.revision,target.clarification,current_fc.version,current_pc.version)
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
    fn mixed_loaded_edition_and_revision_fail_without_rewriting() {
        assert!(required_version(&[d("1.0")], &[d("2.0")]).is_err());
        assert!(required_version(&[d("2.1")], &[d("2.0")]).is_err());
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
        assert!(!pair_matches(t, "S-101", "1.1.0", "S-101", "1.0.2").unwrap());
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
}
