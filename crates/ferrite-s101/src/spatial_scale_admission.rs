//! S-101 Edition 2, section 4.8.2: spatial scale fields are Not Applicable.
//! Keep generic S-100 geometry support; enforce the product restriction at admission.
//! The core reader normalizes both 0 and u32::MAX to None as explicitly
//! permitted for older data by the section B-5.1.31 robust-reader note.
//! This is reader admission, not a certificate of strict original encoding.
use anyhow::{ensure, Context, Result};
use ferrite_kernel::SpecificationVersion;
use ferrite_s100_core::{DatasetIdentification, FeatureRecord};

pub fn validate_spatial_scale_properties<'a>(
    dataset: &DatasetIdentification,
    features: impl IntoIterator<Item = &'a FeatureRecord>,
) -> Result<()> {
    let version = dataset
        .product_edition
        .parse::<SpecificationVersion>()
        .context("Invalid dataset version for S-101 spatial scale validation")?;
    // This rule is taken from the published Edition 2 specification. The
    // caller validates product identity and catalogue compatibility separately.
    if version.edition != 2 {
        return Ok(());
    }
    for feature in features {
        for association in &feature.spatial_associations {
            ensure!(association.scale_minimum.is_none() && association.scale_maximum.is_none(),
                "S-101 2.0.0 section 4.8.2: feature {} spatial record {}:{} must encode scaleMinimum and scaleMaximum as Not Applicable; received {:?}/{:?}",
                feature.frid.rcid, association.spatial_id.rcnm, association.spatial_id.rcid,
                association.scale_minimum, association.scale_maximum);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_s100_core::{Attribute, RecordId, SpatialAssociation, SpatialPrimitiveType, FRID};

    fn feature(min: Option<u32>, max: Option<u32>) -> FeatureRecord {
        FeatureRecord {
            frid: FRID {
                rcid: 42,
                nftc: 1,
                rver: 1,
                ruin: 1,
            },
            foid: None,
            attributes: vec![Attribute {
                natc: 1,
                atix: 1,
                paix: 0,
                atvl: "10000".into(),
                value: None,
                code: Some("scaleMinimum".into()),
            }],
            spatial_associations: vec![SpatialAssociation {
                spatial_id: RecordId::new(120, 7),
                ornt: 1,
                usag: 1,
                mask: 0,
                scale_minimum: min,
                scale_maximum: max,
                update_instruction: 1,
            }],
            information_associations: vec![],
            feature_associations: vec![],
            masks: vec![],
            feature_code: None,
            primitive_type: SpatialPrimitiveType::Curve,
        }
    }
    fn dataset(version: &str) -> DatasetIdentification {
        DatasetIdentification {
            product_edition: version.into(),
            ..Default::default()
        }
    }
    #[test]
    fn not_applicable_spatial_scales_preserve_thematic_scale_and_masking() {
        let mut f = feature(None, None);
        f.spatial_associations[0].mask = 2;
        validate_spatial_scale_properties(&dataset("2.0.0"), [&f]).unwrap();
        assert_eq!(f.attributes[0].atvl, "10000");
        assert_eq!(f.spatial_associations[0].mask, 2);
    }
    #[test]
    fn numeric_spatial_scale_rejected_after_reader_null_normalization() {
        for (min, max) in [
            (Some(1), None),
            (Some(10000), None),
            (None, Some(1000)),
            (Some(10000), Some(1000)),
        ] {
            let err = validate_spatial_scale_properties(&dataset("2.0.0"), [&feature(min, max)])
                .unwrap_err()
                .to_string();
            assert!(err.contains("feature 42 spatial record 120:7"));
            assert!(err.contains("Not Applicable"));
        }
    }
    #[test]
    fn edition_two_rule_does_not_reinterpret_legacy_geometry() {
        validate_spatial_scale_properties(&dataset("1.0"), [&feature(Some(10000), Some(1000))])
            .unwrap();
    }
    #[test]
    fn invalid_version_cannot_skip_the_admission_check() {
        assert!(
            validate_spatial_scale_properties(&dataset("invalid"), [&feature(None, None)]).is_err()
        );
    }
}
