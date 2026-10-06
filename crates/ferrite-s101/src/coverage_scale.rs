//! S-101 DataCoverage scale attributes, preserving each coverage feature.
//! Dataset polygon selection and obscuring masks are a separate operation.
use anyhow::{ensure, Context, Result};
use ferrite_kernel::scale_policy::CoverageScaleRange;
use ferrite_s100_core::{Attribute, FeatureRecord, S101Cell};

fn denominator(attributes: &[Attribute], code: &str) -> Result<u32> {
    let mut found = attributes
        .iter()
        .filter(|a| a.code.as_deref() == Some(code));
    let attribute = found
        .next()
        .with_context(|| format!("Missing DataCoverage {code}"))?;
    ensure!(found.next().is_none(), "Repeated DataCoverage {code}");
    ensure!(attribute.paix == 0, "Nested DataCoverage {code}");
    let value: u32 = attribute
        .atvl
        .parse()
        .with_context(|| format!("Invalid DataCoverage {code}: {:?}", attribute.atvl))?;
    ensure!(value > 0, "Zero DataCoverage {code}");
    Ok(value)
}

/// None means a different feature type; malformed coverage is an error.
pub fn feature_scale(feature: &FeatureRecord) -> Result<Option<CoverageScaleRange>> {
    if feature.feature_code.as_deref() != Some("DataCoverage") {
        return Ok(None);
    }
    let scales = CoverageScaleRange {
        minimum_denominator: Some(denominator(&feature.attributes, "minimumDisplayScale")?),
        optimum_denominator: denominator(&feature.attributes, "optimumDisplayScale")?,
        maximum_denominator: denominator(&feature.attributes, "maximumDisplayScale")?,
    };
    scales.validate()?;
    Ok(Some(scales))
}

/// Stable record keys keep regional ranges separate, never merged into one
/// dataset-wide min/max. Keys identify geometry in the original cell.
pub fn cell_scales(cell: &S101Cell) -> Result<Vec<(i64, CoverageScaleRange)>> {
    let mut result = Vec::new();
    for (&key, feature) in &cell.features {
        if let Some(scales) =
            feature_scale(feature).with_context(|| format!("DataCoverage record {key}"))?
        {
            result.push((key, scales));
        }
    }
    result.sort_unstable_by_key(|(key, _)| *key);
    Ok(result)
}

/// Current S-101 dataset reference scale, derived from the largest optimum
/// display scale among its DataCoverage features (smallest denominator).
/// Legacy product editions lack this mandatory attribute and return None.
/// No source filename or display zoom is interpreted as scale metadata.
pub fn dataset_reference_scale(cell: &S101Cell) -> Result<Option<u32>> {
    let version: ferrite_kernel::SpecificationVersion = cell.dsid.product_edition.parse()?;
    if version.edition == 1 {
        return Ok(None);
    }
    ensure!(
        version.edition == 2,
        "Unsupported S-101 scale model Edition {}",
        version.edition
    );
    reference_scale(&cell_scales(cell)?)
}

fn reference_scale(coverages: &[(i64, CoverageScaleRange)]) -> Result<Option<u32>> {
    ensure!(
        !coverages.is_empty(),
        "Current S-101 dataset has no DataCoverage"
    );
    let minimum = coverages[0].1.minimum_denominator;
    ensure!(
        coverages
            .iter()
            .all(|(_, c)| c.minimum_denominator == minimum),
        "DataCoverage minimum display scales differ within dataset"
    );
    Ok(coverages.iter().map(|(_, c)| c.optimum_denominator).min())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_s100_core::{SpatialPrimitiveType, FRID};
    fn feature() -> FeatureRecord {
        FeatureRecord {
            frid: FRID {
                rcid: 1,
                nftc: 0,
                rver: 1,
                ruin: 1,
            },
            foid: None,
            attributes: [
                ("minimumDisplayScale", "90000"),
                ("optimumDisplayScale", "45000"),
                ("maximumDisplayScale", "12000"),
            ]
            .into_iter()
            .enumerate()
            .map(|(i, (code, value))| Attribute {
                natc: 0,
                atix: i as u16 + 1,
                paix: 0,
                atvl: value.into(),
                value: None,
                code: Some(code.into()),
            })
            .collect(),
            spatial_associations: vec![],
            information_associations: vec![],
            feature_associations: vec![],
            masks: vec![],
            feature_code: Some("DataCoverage".into()),
            primitive_type: SpatialPrimitiveType::Surface,
        }
    }
    #[test]
    fn dataset_reference_uses_largest_regional_optimum_and_checks_consistency() {
        let a = CoverageScaleRange {
            minimum_denominator: Some(90000),
            optimum_denominator: 45000,
            maximum_denominator: 12000,
        };
        let b = CoverageScaleRange {
            minimum_denominator: Some(90000),
            optimum_denominator: 22000,
            maximum_denominator: 6000,
        };
        assert_eq!(reference_scale(&[(1, a), (2, b)]).unwrap(), Some(22000));
        assert!(reference_scale(&[]).is_err());
        let wrong = CoverageScaleRange {
            minimum_denominator: Some(180000),
            ..b
        };
        assert!(reference_scale(&[(1, a), (2, wrong)]).is_err());
    }
    #[test]
    fn coverage_ranges_do_not_become_instruction_visibility_limits() {
        let range = feature_scale(&feature()).unwrap().unwrap();
        assert!(range.within_minimum(90000.).unwrap());
        assert!(!range.within_minimum(90001.).unwrap());
        let overscale = range.overscale(11999., true).unwrap();
        assert!(overscale.indicator_required && overscale.pattern_required);
        assert!(!range.overscale(12000., true).unwrap().pattern_required);
    }
    #[test]
    fn missing_nested_duplicate_unknown_and_inverted_ranges_are_rejected() {
        let base = feature();
        for variant in 0..6 {
            let mut f = base.clone();
            match variant {
                0 => {
                    f.attributes.pop();
                }
                1 => {
                    f.attributes[0].paix = 1;
                }
                2 => {
                    f.attributes.push(f.attributes[0].clone());
                }
                3 => {
                    f.attributes[0].atvl.clear();
                }
                4 => {
                    f.attributes[2].atvl = "50000".into();
                }
                _ => {
                    f.attributes[0].atvl = "0".into();
                }
            }
            assert!(feature_scale(&f).is_err(), "variant {variant}");
        }
        let mut other = base;
        other.feature_code = Some("Wreck".into());
        assert!(feature_scale(&other).unwrap().is_none());
    }
}
