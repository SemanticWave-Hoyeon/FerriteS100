//! Product2 XC numeric checks; never pair regional polygons by ordinal or bbox.
use crate::s101_xc_coverage::{XcDataCoverage, XcScale};
use anyhow::{ensure, Context, Result};
use ferrite_kernel::{scale_policy::SCALE_BAND_OPTIMUM_DENOMINATORS, SpecificationVersion};
use ferrite_security::DatasetPurpose;
use roxmltree::Node;
use std::collections::BTreeMap;
const XC: &str = "http://www.iho.int/s100/xc/5.2";
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct XcProduct {
    pub identifier: String,
    pub version: String,
    pub registry_number: String,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScaleConsistency {
    LegacyNotApplied,
    CancellationRequiresRetainedOriginal,
    NumericAgreementPolygonUnverified,
    RegionalPolygonPairingRequired,
    NullInterpretationRequired,
}
fn children<'a, 'i>(node: Node<'a, 'i>, name: &'a str) -> impl Iterator<Item = Node<'a, 'i>> {
    node.children().filter(move |n| {
        n.is_element() && n.tag_name().namespace() == Some(XC) && n.tag_name().name() == name
    })
}
fn optional_text(node: Node<'_, '_>, name: &str) -> Result<String> {
    let mut found = children(node, name);
    let Some(n) = found.next() else {
        return Ok(String::new());
    };
    ensure!(found.next().is_none(), "Duplicate XC product {name}");
    ensure!(
        n.children().all(|n| n.is_text() || n.is_comment()),
        "Nested XC product scalar"
    );
    let mut raw = String::new();
    for text in n
        .children()
        .filter(|n| n.is_text())
        .filter_map(|n| n.text())
    {
        ensure!(
            text.len() <= 128usize.saturating_sub(raw.len()),
            "XC product lexical receiver budget exceeded"
        );
        raw.push_str(text);
    }
    Ok(raw)
}
pub(crate) fn capture_product(entry: Node<'_, '_>) -> Result<Option<XcProduct>> {
    let mut found = children(entry, "productSpecification");
    let Some(n) = found.next() else {
        return Ok(None);
    };
    ensure!(found.next().is_none(), "Duplicate XC productSpecification");
    Ok(Some(XcProduct {
        identifier: optional_text(n, "productIdentifier")?,
        version: optional_text(n, "version")?,
        registry_number: optional_text(n, "number")?,
    }))
}
fn positive(value: &XcScale, name: &str) -> Result<u32> {
    match value {
        XcScale::Positive { denominator, .. } => Ok(*denominator),
        _ => anyhow::bail!("S1012 XC mandatory numeric {name} missing or unresolved"),
    }
}
fn product2(
    product: Option<&XcProduct>,
    id: &ferrite_s100_core::DatasetIdentification,
) -> Result<bool> {
    let dataset: SpecificationVersion = id.product_edition.parse()?;
    if dataset.edition == 1 {
        if let Some(product) = product {
            ensure!(
                product.identifier.trim() == "S-101",
                "Legacy XC productIdentifier mismatch"
            );
            ensure!(
                product.version.trim().parse::<SpecificationVersion>()? == dataset,
                "Legacy XC/DSID product version mismatch"
            );
        }
        return Ok(false);
    }
    ensure!(
        dataset == "2.0.0".parse()?,
        "Unsupported S101 XC product scale profile"
    );
    let product = product.context("S1012 XC mandatory productSpecification missing")?;
    ensure!(
        product.identifier.trim() == "S-101",
        "XC productIdentifier is not S-101"
    );
    ensure!(
        product.version.trim().parse::<SpecificationVersion>()? == dataset,
        "XC/DSID product specification version mismatch"
    );
    let number = product
        .registry_number
        .trim_matches([' ', '\t', '\n', '\r']);
    let digits = number.strip_prefix('+').unwrap_or(number);
    ensure!(
        !digits.is_empty()
            && digits.bytes().all(|b| b.is_ascii_digit())
            && digits.parse::<u32>()? > 0,
        "XC mandatory positive GI registry index invalid"
    );
    // Registry index is not product number101 (public SHOM uses214).
    Ok(true)
}
pub(crate) fn validate_profile(
    product: Option<&XcProduct>,
    purpose: DatasetPurpose,
    rows: &[XcDataCoverage],
    id: &ferrite_s100_core::DatasetIdentification,
) -> Result<ScaleConsistency> {
    if !product2(product, id)? {
        return Ok(ScaleConsistency::LegacyNotApplied);
    }
    if purpose == DatasetPurpose::Cancellation {
        return Ok(ScaleConsistency::CancellationRequiresRetainedOriginal);
    }
    ensure!(
        !rows.is_empty(),
        "S1012 XC requires one or more dataCoverage rows"
    );
    let mut unresolved = false;
    for row in rows {
        let opt = positive(&row.optimum, "optimumDisplayScale")?;
        let max = positive(&row.maximum, "maximumDisplayScale")?;
        ensure!(
            SCALE_BAND_OPTIMUM_DENOMINATORS.contains(&opt),
            "S1012 XC non-prescribed optimumDisplayScale"
        );
        ensure!(
            max > 0 && max <= opt,
            "S1012 XC maximum/optimum scale order mismatch"
        );
        match &row.minimum {
            XcScale::Absent => anyhow::bail!("S1012 XC mandatory minimumDisplayScale absent"),
            XcScale::UnresolvedNull { .. } => unresolved = true,
            XcScale::Positive {
                denominator: min, ..
            } => {
                ensure!(
                    SCALE_BAND_OPTIMUM_DENOMINATORS[..14].contains(min),
                    "S1012 XC non-prescribed minimumDisplayScale"
                );
                ensure!(
                    *min > opt,
                    "S1012 XC minimum must be smaller scale than optimum"
                );
            }
        }
    }
    Ok(if unresolved {
        ScaleConsistency::NullInterpretationRequired
    } else {
        ScaleConsistency::NumericAgreementPolygonUnverified
    })
}
pub(crate) fn compare_effective(
    product: Option<&XcProduct>,
    purpose: DatasetPurpose,
    rows: &[XcDataCoverage],
    cell: &ferrite_s100_core::S101Cell,
) -> Result<ScaleConsistency> {
    let profile = validate_profile(product, purpose, rows, &cell.dsid)?;
    if matches!(
        profile,
        ScaleConsistency::LegacyNotApplied | ScaleConsistency::CancellationRequiresRetainedOriginal
    ) {
        return Ok(profile);
    }
    let features = ferrite_s101::coverage_scale::cell_scales(cell)?;
    ensure!(
        !features.is_empty(),
        "Effective S1012 cell has no DataCoverage"
    );
    ensure!(
        rows.len() == features.len(),
        "XC must list every effective DataCoverage feature (S1014.5.2)"
    );
    let mut feature_set = BTreeMap::new();
    for (_, s) in features {
        *feature_set
            .entry((
                s.minimum_denominator,
                s.optimum_denominator,
                s.maximum_denominator,
            ))
            .or_insert(0usize) += 1;
    }
    let mut xc_set = BTreeMap::new();
    for row in rows {
        if let XcScale::Positive {
            denominator: min, ..
        } = &row.minimum
        {
            *xc_set
                .entry((
                    Some(*min),
                    positive(&row.optimum, "optimum")?,
                    positive(&row.maximum, "maximum")?,
                ))
                .or_insert(0usize) += 1;
        }
    }
    if profile == ScaleConsistency::NullInterpretationRequired {
        // Compare definite numeric rows only; never label unresolved XML nil as
        // equal to an ISO empty minimum, and never synthesize a denominator.
        let mut feature_pairs = BTreeMap::new();
        for ((_, opt, max), count) in &feature_set {
            *feature_pairs.entry((*opt, *max)).or_insert(0usize) += *count;
        }
        let mut xc_pairs = BTreeMap::new();
        for row in rows {
            *xc_pairs
                .entry((
                    positive(&row.optimum, "optimum")?,
                    positive(&row.maximum, "maximum")?,
                ))
                .or_insert(0usize) += 1;
        }
        ensure!(
            xc_pairs == feature_pairs,
            "Unresolved NULL XC rows optimum/maximum multiplicity mismatch"
        );
        ensure!(
            xc_set
                .iter()
                .all(|(tuple, count)| feature_set.get(tuple).is_some_and(|actual| count <= actual)),
            "Numeric XC row has no matching effective feature scale tuple"
        );
        return Ok(profile);
    }
    ensure!(
        xc_set == feature_set,
        "XC/effective DataCoverage numeric scale tuple set mismatch"
    );
    Ok(if xc_set.len() == 1 {
        ScaleConsistency::NumericAgreementPolygonUnverified
    } else {
        ScaleConsistency::RegionalPolygonPairingRequired
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_s100_core::{
        Attribute, DatasetCodeMappings, DatasetIdentification, FeatureRecord, S101Cell,
        SpatialPrimitiveType, FRID,
    };
    use std::{collections::HashMap, path::PathBuf};
    fn product() -> XcProduct {
        XcProduct {
            identifier: "S-101".into(),
            version: "2.0".into(),
            registry_number: "214".into(),
        }
    }
    fn id() -> DatasetIdentification {
        DatasetIdentification {
            product_identifier: "INT.IHO.S-101.2.0".into(),
            product_edition: "2.0".into(),
            application_profile: "1".into(),
            edition_number: 1,
            ..Default::default()
        }
    }
    fn scalar(d: u32) -> XcScale {
        XcScale::Positive {
            lexical: d.to_string(),
            denominator: d,
        }
    }
    fn row(min: u32, opt: u32, max: u32) -> XcDataCoverage {
        XcDataCoverage {
            entry_range: 0..0,
            bounding_polygon_range: 0..0,
            minimum: scalar(min),
            optimum: scalar(opt),
            maximum: scalar(max),
        }
    }
    fn feature(key: u32, min: &str, opt: u32, max: u32) -> FeatureRecord {
        FeatureRecord {
            frid: FRID {
                rcid: key,
                nftc: 0,
                rver: 1,
                ruin: 1,
            },
            foid: None,
            attributes: [
                ("minimumDisplayScale", min.to_owned()),
                ("optimumDisplayScale", opt.to_string()),
                ("maximumDisplayScale", max.to_string()),
            ]
            .into_iter()
            .enumerate()
            .map(|(i, (code, atvl))| Attribute {
                natc: 0,
                atix: i as u16 + 1,
                paix: 0,
                atvl,
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
    fn cell(features: Vec<FeatureRecord>) -> S101Cell {
        S101Cell {
            file_path: PathBuf::from("/private-test/101.000"),
            dsid: id(),
            code_mappings: DatasetCodeMappings::new(),
            coord_factor: 1.,
            coord_factor_y: 1.,
            coord_factor_z: 0.01,
            coord_origin_x: 0.,
            coord_origin_y: 0.,
            coord_origin_z: 0.,
            minimum_display_scale: None,
            maximum_display_scale: None,
            points: HashMap::new(),
            multi_points: HashMap::new(),
            curves: HashMap::new(),
            composite_curves: HashMap::new(),
            surfaces: HashMap::new(),
            features: features
                .into_iter()
                .map(|f| (i64::from(f.frid.rcid), f))
                .collect(),
            information: HashMap::new(),
            spatial_information_associations: HashMap::new(),
        }
    }
    #[test]
    fn registry_index_is_not_101_and_product_version_is_bound() {
        let p = product();
        assert!(validate_profile(
            Some(&p),
            DatasetPurpose::NewDataset,
            &[row(45000, 12000, 6000)],
            &id()
        )
        .is_ok());
        let mut wrong = p.clone();
        wrong.identifier = "S-102".into();
        assert!(validate_profile(
            Some(&wrong),
            DatasetPurpose::NewDataset,
            &[row(45000, 12000, 6000)],
            &id()
        )
        .is_err());
        wrong = p.clone();
        wrong.version = "1.0.2".into();
        assert!(validate_profile(
            Some(&wrong),
            DatasetPurpose::NewDataset,
            &[row(45000, 12000, 6000)],
            &id()
        )
        .is_err());
        wrong = p;
        wrong.registry_number = "0".into();
        assert!(validate_profile(
            Some(&wrong),
            DatasetPurpose::NewDataset,
            &[row(45000, 12000, 6000)],
            &id()
        )
        .is_err());
    }
    #[test]
    fn current_non_cancellation_requires_rows_and_all_three_scales() {
        for purpose in [
            DatasetPurpose::NewDataset,
            DatasetPurpose::NewEdition,
            DatasetPurpose::Update,
            DatasetPurpose::Reissue,
        ] {
            assert!(validate_profile(Some(&product()), purpose, &[], &id()).is_err());
            for field in [0, 1, 2] {
                let mut r = row(45000, 12000, 6000);
                match field {
                    0 => r.minimum = XcScale::Absent,
                    1 => r.optimum = XcScale::Absent,
                    _ => r.maximum = XcScale::Absent,
                };
                assert!(validate_profile(Some(&product()), purpose, &[r], &id()).is_err());
            }
        }
    }
    #[test]
    fn scale_value_and_order_checks_do_not_change_original_lexical() {
        for r in [
            row(1000, 12000, 6000),
            row(12000, 12000, 6000),
            row(45000, 11000, 6000),
            row(45000, 12000, 12001),
        ] {
            assert!(
                validate_profile(Some(&product()), DatasetPurpose::Update, &[r], &id()).is_err()
            );
        }
        let mut r = row(45000, 12000, 6000);
        r.minimum = XcScale::Positive {
            lexical: " +045000 ".into(),
            denominator: 45000,
        };
        assert!(validate_profile(
            Some(&product()),
            DatasetPurpose::NewEdition,
            std::slice::from_ref(&r),
            &id()
        )
        .is_ok());
        assert!(matches!(r.minimum,XcScale::Positive{lexical,..} if lexical==" +045000 "));
    }
    #[test]
    fn uniform_numeric_agreement_does_not_claim_polygon_agreement() {
        let c = cell(vec![
            feature(1, "45000", 12000, 6000),
            feature(2, "45000", 12000, 6000),
        ]);
        assert_eq!(
            compare_effective(
                Some(&product()),
                DatasetPurpose::NewDataset,
                &[row(45000, 12000, 6000), row(45000, 12000, 6000)],
                &c
            )
            .unwrap(),
            ScaleConsistency::NumericAgreementPolygonUnverified
        );
        assert!(compare_effective(
            Some(&product()),
            DatasetPurpose::NewDataset,
            &[row(45000, 22000, 6000), row(45000, 22000, 6000)],
            &c
        )
        .is_err());
    }
    #[test]
    fn distinct_regions_never_get_zipped_and_permutation_does_not_assign_rights() {
        let c = cell(vec![
            feature(1, "45000", 12000, 6000),
            feature(2, "45000", 22000, 8000),
        ]);
        let rows = [row(45000, 22000, 8000), row(45000, 12000, 6000)];
        assert_eq!(
            compare_effective(Some(&product()), DatasetPurpose::Update, &rows, &c).unwrap(),
            ScaleConsistency::RegionalPolygonPairingRequired
        );
        assert!(
            compare_effective(Some(&product()), DatasetPurpose::Update, &rows[..1], &c).is_err()
        );
    }
    #[test]
    fn unresolved_xml_null_is_not_assigned_to_iso_empty_minimum() {
        let c = cell(vec![feature(1, "", 12000, 6000)]);
        let mut r = row(45000, 12000, 6000);
        r.minimum = XcScale::UnresolvedNull {
            lexical: "".into(),
            xsi_nil: Some("true".into()),
            nil_reason: None,
        };
        assert_eq!(
            compare_effective(
                Some(&product()),
                DatasetPurpose::NewDataset,
                std::slice::from_ref(&r),
                &c
            )
            .unwrap(),
            ScaleConsistency::NullInterpretationRequired
        );
        r.optimum = scalar(22000);
        assert!(compare_effective(Some(&product()), DatasetPurpose::NewDataset, &[r], &c).is_err());
    }
    #[test]
    fn legacy_and_cancel_are_not_relabelled_as_current_numeric_match() {
        let mut legacy = id();
        legacy.product_edition = "1.0.2".into();
        assert_eq!(
            validate_profile(None, DatasetPurpose::NewDataset, &[], &legacy).unwrap(),
            ScaleConsistency::LegacyNotApplied
        );
        assert_eq!(
            validate_profile(Some(&product()), DatasetPurpose::Cancellation, &[], &id()).unwrap(),
            ScaleConsistency::CancellationRequiresRetainedOriginal
        );
    }
}

#[cfg(test)]
mod product_capture_tests {
    use super::*;
    #[test]
    fn real_shom_product_shape_preserves_raw_registry_index_and_abbreviated_version() {
        let xml=format!("<xc:S100_DatasetDiscoveryMetadata xmlns:xc='{XC}'><xc:productSpecification><xc:version>2.0</xc:version><xc:productIdentifier>S-101</xc:productIdentifier><xc:number>214</xc:number></xc:productSpecification></xc:S100_DatasetDiscoveryMetadata>");
        let doc = roxmltree::Document::parse(&xml).unwrap();
        assert_eq!(
            capture_product(doc.root_element()).unwrap().unwrap(),
            XcProduct {
                identifier: "S-101".into(),
                version: "2.0".into(),
                registry_number: "214".into()
            }
        );
        let duplicate = xml.replace(
            "</xc:productSpecification>",
            "<xc:version>2.0.0</xc:version></xc:productSpecification>",
        );
        let doc = roxmltree::Document::parse(&duplicate).unwrap();
        assert!(capture_product(doc.root_element()).is_err());
    }
}
