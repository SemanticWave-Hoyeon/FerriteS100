//! S-101 2.0.0 section 1.6.3: later catalogue revisions read earlier datasets
//! within one Edition. FC and PC can be revised independently (section 8.5).
use anyhow::{ensure, Context, Result};
use ferrite_feature_catalog::FeatureCatalogue;
use ferrite_kernel::SpecificationVersion;
use ferrite_portrayal_catalog::PortrayalCatalogue;
use ferrite_s100_core::DatasetIdentification;

pub fn validate_catalogue_pair(fc: &FeatureCatalogue, pc: &PortrayalCatalogue) -> Result<()> {
    validate_catalogue_identity(&fc.product_id, &fc.version, &pc.product_id, &pc.version)?;
    for preset in [
        super::DisplayPreset::Base,
        super::DisplayPreset::Standard,
        super::DisplayPreset::Other,
    ] {
        super::viewing_groups_for_preset(pc, preset)?;
    }
    Ok(())
}
fn validate_catalogue_identity(
    fc_product: &str,
    fc_version: &str,
    pc_product: &str,
    pc_version: &str,
) -> Result<(SpecificationVersion, SpecificationVersion)> {
    ensure!(
        fc_product == "S-101" && pc_product == "S-101",
        "S-101 requires S-101 FC and PC; received FC {fc_product}, PC {pc_product}"
    );
    let fc = fc_version
        .parse::<SpecificationVersion>()
        .context("Invalid FC version")?;
    let pc = pc_version
        .parse::<SpecificationVersion>()
        .context("Invalid PC version")?;
    ensure!(
        fc.edition == pc.edition,
        "FC {fc_version} and PC {pc_version} have different Editions"
    );
    Ok((fc, pc))
}
pub fn validate_dataset_catalogues(
    dataset: &DatasetIdentification,
    fc: &FeatureCatalogue,
    pc_product: &str,
    pc_version: &str,
) -> Result<()> {
    let identifier_version = dataset
        .product_identifier
        .strip_prefix("INT.IHO.S-101.")
        .context(
            "Dataset product identifier must identify INT.IHO.S-101 and its specification version",
        )?
        .parse::<SpecificationVersion>()
        .context("Invalid dataset product identifier version")?;
    let (fcv, pcv) =
        validate_catalogue_identity(&fc.product_id, &fc.version, pc_product, pc_version)?;
    let data = dataset
        .product_edition
        .parse::<SpecificationVersion>()
        .context("Invalid dataset product edition")?;
    ensure!(
        identifier_version == data,
        "Dataset PRSP and PRED specification versions disagree"
    );
    fcv.require_same_edition_backward_revision_compatibility(data)
        .context("Feature Catalogue cannot process dataset specification")?;
    pcv.require_same_edition_backward_revision_compatibility(data)
        .context("Portrayal Catalogue cannot process dataset specification")?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn product_identity_and_independent_catalogue_revisions() {
        assert!(validate_catalogue_identity("S-101", "2.1.0", "S-101", "2.0.1").is_ok());
        assert!(validate_catalogue_identity("S-102", "2.0.0", "S-101", "2.0.0").is_err());
        assert!(validate_catalogue_identity("S-101", "1.1.0", "S-101", "2.0.0").is_err());
    }
    #[test]
    fn actual_dataset_edition_is_checked_against_both_catalogues() {
        let mut fc = FeatureCatalogue {
            source_path: Default::default(),
            name: String::new(),
            scope: String::new(),
            version: String::new(),
            version_date: String::new(),
            product_id: String::new(),
            simple_attributes: Default::default(),
            complex_attributes: Default::default(),
            feature_types: Default::default(),
            information_types: Default::default(),
        };
        fc.product_id = "S-101".into();
        fc.version = "2.1.0".into();
        let mut dataset = DatasetIdentification {
            product_identifier: "INT.IHO.S-101.2.0".into(),
            product_edition: "2.0".into(),
            ..Default::default()
        };
        assert!(validate_dataset_catalogues(&dataset, &fc, "S-101", "2.0.0").is_ok());
        dataset.product_identifier = "INT.IHO.S-101.2.1".into();
        assert!(validate_dataset_catalogues(&dataset, &fc, "S-101", "2.1.0").is_err());
        dataset.product_identifier = "S-101".into();
        assert!(validate_dataset_catalogues(&dataset, &fc, "S-101", "2.1.0").is_err());
        dataset.product_edition = "2.1".into();
        dataset.product_identifier = "INT.IHO.S-101.2.1".into();
        assert!(validate_dataset_catalogues(&dataset, &fc, "S-101", "2.0.0").is_err());
        dataset.product_edition = "1.1".into();
        dataset.product_identifier = "INT.IHO.S-101.1.1".into();
        assert!(validate_dataset_catalogues(&dataset, &fc, "S-101", "2.0.0").is_err());
        dataset.product_identifier = "INT.IHO.S-102.1.1".into();
        assert!(validate_dataset_catalogues(&dataset, &fc, "S-101", "2.0.0").is_err());
    }
}

#[cfg(test)]
mod clarification_policy_tests {
    use super::*;
    fn fc(version: &str) -> FeatureCatalogue {
        FeatureCatalogue {
            source_path: Default::default(),
            name: String::new(),
            scope: String::new(),
            version: version.into(),
            version_date: String::new(),
            product_id: "S-101".into(),
            simple_attributes: Default::default(),
            complex_attributes: Default::default(),
            feature_types: Default::default(),
            information_types: Default::default(),
        }
    }
    fn data(version: &str) -> DatasetIdentification {
        DatasetIdentification {
            product_identifier: format!("INT.IHO.S-101.{version}"),
            product_edition: version.into(),
            ..Default::default()
        }
    }
    #[test]
    fn both_catalogues_accept_earlier_and_later_clarifications_independently() {
        for (fv, pv, dv) in [
            ("2.0.0", "2.0.1", "2.0.9"),
            ("2.0.9", "2.0.0", "2.0.1"),
            ("2.1.0", "2.0.0", "2.0.99"),
        ] {
            assert!(validate_dataset_catalogues(&data(dv), &fc(fv), "S-101", pv).is_ok());
        }
    }
    #[test]
    fn compatibility_does_not_bypass_identity_revision_or_edition_checks() {
        assert!(
            validate_dataset_catalogues(&data("2.1.0"), &fc("2.0.99"), "S-101", "2.1.0").is_err()
        );
        assert!(
            validate_dataset_catalogues(&data("2.1.0"), &fc("2.1.0"), "S-101", "2.0.99").is_err()
        );
        assert!(
            validate_dataset_catalogues(&data("1.0.9"), &fc("2.0.0"), "S-101", "2.0.0").is_err()
        );
        assert!(
            validate_dataset_catalogues(&data("2.0.9"), &fc("2.0.0"), "S-102", "2.0.0").is_err()
        );
        let mut mismatch = data("2.0.9");
        mismatch.product_edition = "2.0.0".into();
        assert!(validate_dataset_catalogues(&mismatch, &fc("2.0.0"), "S-101", "2.0.0").is_err());
    }
}
