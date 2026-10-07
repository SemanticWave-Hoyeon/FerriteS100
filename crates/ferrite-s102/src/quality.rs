//! S-102 3.0 feature-oriented quality grid and source survey attributes.
use crate::string_attr;
use anyhow::{ensure, Context, Result};
use ferrite_kernel::{validate_s100_date, GridGeometry, GridWindow};
use hdf5::{
    types::{
        CompoundField, CompoundType, FloatSize, IntSize, TypeDescriptor, VarLenAscii, VarLenUnicode,
    },
    H5Type,
};
use std::{collections::HashMap, sync::Arc};
const FIELDS: [&str; 15] = [
    "id",
    "dataAssessment",
    "featuresDetected.leastDepthOfDetectedFeaturesMeasured",
    "featuresDetected.significantFeaturesDetected",
    "featuresDetected.sizeOfFeaturesDetected",
    "featureSizeVar",
    "fullSeafloorCoverageAchieved",
    "bathyCoverage",
    "zoneOfConfidence.horizontalPositionUncertainty.uncertaintyFixed",
    "zoneOfConfidence.horizontalPositionUncertainty.uncertaintyVariableFactor",
    "surveyDateRange.dateStart",
    "surveyDateRange.dateEnd",
    "sourceSurveyID",
    "surveyAuthority",
    "typeOfBathymetricEstimationUncertainty",
];
/// Named one-field projection. repr(C) places the only field at byte zero.
#[repr(C)]
struct Column<T, const N: usize> {
    value: T,
}
// SAFETY: descriptor names the sole field at offset zero with its exact H5Type,
// and reports the actual repr(C) object size, including alignment padding.
unsafe impl<T: H5Type, const N: usize> H5Type for Column<T, N> {
    fn type_descriptor() -> TypeDescriptor {
        TypeDescriptor::Compound(CompoundType {
            fields: vec![CompoundField {
                name: FIELDS[N].into(),
                ty: T::type_descriptor(),
                offset: 0,
                index: 0,
            }],
            size: std::mem::size_of::<Self>(),
        })
    }
}
fn column<T: H5Type, const N: usize>(d: &hdf5::Dataset) -> Result<Vec<T>> {
    Ok(d.read_raw::<Column<T, N>>()?
        .into_iter()
        .map(|v| v.value)
        .collect())
}
fn decode_string_bytes(bytes: &[u8], declared_ascii: bool) -> (String, Option<String>) {
    match std::str::from_utf8(bytes) {
        Ok(value) if !declared_ascii || bytes.is_ascii() => (value.into(),None),
        Ok(value) => (value.into(),Some("Declared ASCII contains non-ASCII UTF-8; original bytes retained".into())),
        Err(_) => (bytes.iter().map(|b|char::from(*b)).collect(),Some("Invalid UTF-8; display uses ISO-8859-1 interpretation (source encoding unverified), original bytes retained".into())),
    }
}

#[derive(Debug, Clone, Default)]
pub struct QualityRecord {
    pub id: u32,
    /// Original bytes for fields whose declared character set was inconsistent.
    pub raw_string_bytes: std::collections::BTreeMap<String, Vec<u8>>,
    pub encoding_warnings: Vec<String>,
    pub data_assessment: Option<u8>,
    pub least_depth_measurement_capability: Option<bool>,
    pub significant_feature_detection_capability: Option<bool>,
    pub size_of_features_detected: Option<f32>,
    pub feature_size_variation: Option<f32>,
    pub full_seafloor_coverage: Option<bool>,
    pub bathymetry_observed: Option<bool>,
    pub horizontal_uncertainty_fixed: Option<f32>,
    pub horizontal_uncertainty_variable_factor: Option<f32>,
    pub survey_date_start: Option<String>,
    pub survey_date_end: Option<String>,
    pub source_survey_id: Option<String>,
    pub survey_authority: Option<String>,
    pub uncertainty_type: Option<u8>,
}
impl QualityRecord {
    fn validate_semantics(&self) -> Result<()> {
        ensure!(
            !(self.full_seafloor_coverage == Some(false)
                && self.bathymetry_observed == Some(true)),
            "Quality record {}: bathyCoverage must be false when fullSeafloorCoverageAchieved is false",
            self.id
        );
        for (name, date) in [
            ("surveyDateRange.dateStart", &self.survey_date_start),
            ("surveyDateRange.dateEnd", &self.survey_date_end),
        ] {
            if let Some(date) = date {
                ensure!(
                    date.len() == 8 && date.is_ascii(),
                    "Quality record {}: {} must use the 8-character HDF5 date encoding",
                    self.id,
                    name
                );
                validate_s100_date(date)
                    .with_context(|| format!("Quality record {}: invalid {}", self.id, name))?;
            }
        }
        Ok(())
    }

    pub fn description(&self) -> String {
        let mut lines = vec![format!("Quality record: {}", self.id)];
        if let Some(v) = self.data_assessment {
            lines.push(format!(
                "Data assessment: {} ({v})",
                match v {
                    1 => "assessed",
                    2 => "unassessed",
                    3 => "oceanic",
                    _ => "unknown",
                }
            ));
        }
        for (label, v) in [
            (
                "Least-depth measurement capability",
                self.least_depth_measurement_capability,
            ),
            (
                "Significant-feature detection capability",
                self.significant_feature_detection_capability,
            ),
            ("Full seafloor coverage", self.full_seafloor_coverage),
            ("Bathymetry directly observed", self.bathymetry_observed),
        ] {
            if let Some(v) = v {
                lines.push(format!("{label}: {}", if v { "yes" } else { "no" }));
            }
        }
        for (label, v) in [
            (
                "Detectable feature size (m)",
                self.size_of_features_detected,
            ),
            (
                "Feature size variation (% of depth)",
                self.feature_size_variation,
            ),
            (
                "Horizontal uncertainty fixed (m)",
                self.horizontal_uncertainty_fixed,
            ),
            (
                "Horizontal uncertainty variable factor",
                self.horizontal_uncertainty_variable_factor,
            ),
        ] {
            if let Some(v) = v {
                lines.push(format!("{label}: {v}"));
            }
        }
        for (label, v) in [
            ("Survey start", &self.survey_date_start),
            ("Survey end", &self.survey_date_end),
            ("Source survey", &self.source_survey_id),
            ("Survey authority", &self.survey_authority),
        ] {
            if let Some(v) = v {
                lines.push(format!("{label}: {v}"));
            }
        }
        if let Some(v) = self.uncertainty_type {
            lines.push(format!(
                "Depth uncertainty type: {} ({v})",
                match v {
                    1 => "raw standard deviation",
                    2 => "CUBE standard deviation",
                    3 => "product uncertainty",
                    4 => "historical standard deviation",
                    _ => "unknown",
                }
            ));
        }
        if self.significant_feature_detection_capability == Some(false) {
            lines.push(
                "Feature size values are not applicable when detection capability is no.".into(),
            );
        }
        lines.extend(
            self.encoding_warnings
                .iter()
                .map(|w| format!("Source encoding warning: {w}")),
        );
        lines.join("\n")
    }
}
#[derive(Debug)]
pub struct QualityCoverage {
    pub horizontal_position_uncertainty: f32,
    pub vertical_position_uncertainty: f32,
    pub axes: crate::AxisMetadata,
    pub root_enclosure: Option<crate::RootEnclosure>,
    pub domain: crate::InstanceDomain,
    outside_domain: std::sync::atomic::AtomicBool,
    geometry: GridGeometry,
    values: hdf5::Dataset,
    records: HashMap<u32, QualityRecord>,
}
impl QualityCoverage {
    pub(crate) fn open_optional(
        file: &hdf5::File,
        grids: &[GridGeometry],
    ) -> Result<Option<Arc<Self>>> {
        if !file.link_exists("QualityOfBathymetryCoverage") {
            return Ok(None);
        }
        let q = file.group("QualityOfBathymetryCoverage")?;
        // S-1023.0.0 section10.2.8 inherits Table10-4 attributes. Quality remains
        // one shared instance even when bathymetry has several vertical datums.
        // No missing metadata is substituted from the bathymetry container.
        ensure!(
            crate::validate_container(&q)? == 1,
            "Quality must have one shared instance"
        );
        let [horizontal_position_uncertainty, vertical_position_uncertainty] =
            crate::position_uncertainties(&q)?;
        let bathymetry = file
            .group("BathymetryCoverage")
            .context("Quality without BathymetryCoverage container")?;
        let inherited = crate::position_uncertainties(&bathymetry)?;
        ensure!(
            [
                horizontal_position_uncertainty,
                vertical_position_uncertainty
            ] == inherited,
            "Quality container position uncertainties must match BathymetryCoverage"
        );
        ensure!(
            crate::scalar::u8(&q, "dataCodingFormat")? == 9,
            "Quality grid requires dataCodingFormat=9"
        );
        ensure!(
            crate::scalar::u8(&q, "dataOffsetCode")? == 5,
            "Quality grid requires cell-centre dataOffsetCode=5"
        );
        ensure!(
            crate::scalar::u8(&q, "sequencingRule.type")? == 1,
            "Unsupported quality sequencing rule"
        );
        ensure!(
            string_attr(&q, "sequencingRule.scanDirection")?
                .split(',')
                .map(str::trim)
                .eq(super::canonical_axes(
                    grids
                        .first()
                        .context("Quality without bathymetry")?
                        .horizontal_crs
                )?),
            "Unsupported quality scan direction"
        );
        let axes = crate::AxisMetadata::read(
            &q,
            grids
                .first()
                .context("Quality without bathymetry")?
                .horizontal_crs,
        )?;
        let names: Vec<_> = q
            .member_names()?
            .into_iter()
            .filter(|s| s.starts_with("QualityOfBathymetryCoverage."))
            .collect();
        ensure!(
            names.len() == 1,
            "S-102 requires exactly one shared quality instance when present"
        );
        ensure!(
            crate::scalar::unsigned_u8(&q, "numInstances")? == 1,
            "Quality numInstances must equal its one shared instance"
        );
        let g = q.group(&names[0])?;
        ensure!(
            crate::scalar::unsigned_u32(&g, "numGRP")? == 1,
            "Multiple quality groups are not allowed"
        );
        ensure!(
            string_attr(&g, "startSequence")?
                .split(',')
                .map(str::trim)
                .eq(["0", "0"]),
            "Unsupported quality startSequence"
        );
        let first = grids.first().context("Quality grid without bathymetry")?;
        let geometry = GridGeometry {
            width: crate::scalar::unsigned_u32(&g, "numPointsLongitudinal")? as usize,
            height: crate::scalar::unsigned_u32(&g, "numPointsLatitudinal")? as usize,
            origin_x: crate::scalar::f64(&g, "gridOriginLongitude")?,
            origin_y: crate::scalar::f64(&g, "gridOriginLatitude")?,
            spacing_x: crate::scalar::f64(&g, "gridSpacingLongitudinal")?,
            spacing_y: crate::scalar::f64(&g, "gridSpacingLatitudinal")?,
            horizontal_crs: first.horizontal_crs,
        };
        super::validate_storage_geometry(&geometry)?;
        for grid in grids {
            ensure!(
                geometry.width == grid.width
                    && geometry.height == grid.height
                    && geometry.origin_x == grid.origin_x
                    && geometry.origin_y == grid.origin_y
                    && geometry.spacing_x == grid.spacing_x
                    && geometry.spacing_y == grid.spacing_y
                    && geometry.horizontal_crs == grid.horizontal_crs,
                "Quality and bathymetry grid geometry differ"
            );
        }
        let values = g.dataset("Group_001/values")?;
        ensure!(
            values.shape() == [geometry.height, geometry.width]
                && values.dtype()?.to_descriptor()? == TypeDescriptor::Unsigned(IntSize::U4),
            "Invalid quality ID grid type or dimensions"
        );
        let table = q.dataset("featureAttributeTable")?;
        ensure!(
            table.ndim() == 1 && table.size() <= 100_000,
            "Quality table must be one-dimensional, at most 100000 records"
        );
        let TypeDescriptor::Compound(desc) = table.dtype()?.to_descriptor()? else {
            anyhow::bail!("Quality table is not a compound dataset")
        };
        let mut fields = HashMap::new();
        for f in desc.fields {
            ensure!(
                fields.insert(f.name.clone(), f.ty).is_none(),
                "Duplicate quality table column {}",
                f.name
            );
        }
        ensure!(
            fields.get("id") == Some(&TypeDescriptor::Unsigned(IntSize::U4)),
            "Quality id must be unsigned 32-bit"
        );
        let mut rows: Vec<_> = column::<u32, 0>(&table)?
            .into_iter()
            .map(|id| QualityRecord {
                id,
                ..Default::default()
            })
            .collect();
        macro_rules! num {
            ($n:literal,$ty:ty,$field:ident,$desc:expr) => {
                if let Some(t) = fields.get(FIELDS[$n]) {
                    ensure!(*t == $desc, "Invalid quality column {} type", FIELDS[$n]);
                    for (row, value) in rows.iter_mut().zip(column::<$ty, $n>(&table)?) {
                        row.$field = Some(value);
                    }
                }
            };
        }
        macro_rules! boolean {
            ($n:literal,$field:ident) => {
                if let Some(t) = fields.get(FIELDS[$n]) {
                    ensure!(
                        *t == TypeDescriptor::Unsigned(IntSize::U1),
                        "Invalid quality boolean type"
                    );
                    for (row, value) in rows.iter_mut().zip(column::<u8, $n>(&table)?) {
                        ensure!(value <= 1, "Invalid quality boolean {}", FIELDS[$n]);
                        row.$field = Some(value == 1);
                    }
                }
            };
        }
        num!(
            1,
            u8,
            data_assessment,
            TypeDescriptor::Unsigned(IntSize::U1)
        );
        boolean!(2, least_depth_measurement_capability);
        boolean!(3, significant_feature_detection_capability);
        num!(
            4,
            f32,
            size_of_features_detected,
            TypeDescriptor::Float(FloatSize::U4)
        );
        num!(
            5,
            f32,
            feature_size_variation,
            TypeDescriptor::Float(FloatSize::U4)
        );
        boolean!(6, full_seafloor_coverage);
        boolean!(7, bathymetry_observed);
        num!(
            8,
            f32,
            horizontal_uncertainty_fixed,
            TypeDescriptor::Float(FloatSize::U4)
        );
        num!(
            9,
            f32,
            horizontal_uncertainty_variable_factor,
            TypeDescriptor::Float(FloatSize::U4)
        );
        macro_rules! string {
            ($n:literal,$field:ident) => {
                if let Some(t) = fields.get(FIELDS[$n]) {
                    // Validate bytes before invoking any HDF5 string as_str/Display.
                    // Delivered files can contradict their UTF-8/ASCII declarations.
                    let values: Vec<Vec<u8>> = match t {
                        TypeDescriptor::VarLenAscii => column::<VarLenAscii, $n>(&table)?
                            .into_iter()
                            .map(|v| v.as_bytes().to_vec())
                            .collect(),
                        TypeDescriptor::VarLenUnicode => column::<VarLenUnicode, $n>(&table)?
                            .into_iter()
                            .map(|v| v.as_bytes().to_vec())
                            .collect(),
                        _ => anyhow::bail!("Unsupported quality string type {}", FIELDS[$n]),
                    };
                    for (row, bytes) in rows.iter_mut().zip(values) {
                        ensure!(bytes.len() <= 16_384, "Quality string exceeds 16KiB");
                        let (value, warning) =
                            decode_string_bytes(&bytes, *t == TypeDescriptor::VarLenAscii);
                        if let Some(warning) = warning {
                            row.encoding_warnings
                                .push(format!("{}: {warning}", FIELDS[$n]));
                            row.raw_string_bytes.insert(FIELDS[$n].into(), bytes);
                        }
                        row.$field = (!value.is_empty()).then_some(value);
                    }
                }
            };
        }
        string!(10, survey_date_start);
        string!(11, survey_date_end);
        string!(12, source_survey_id);
        string!(13, survey_authority);
        if let Some(t) = fields.get(FIELDS[14]) {
            ensure!(
                matches!(t,TypeDescriptor::Enum(e) if e.size==IntSize::U1)
                    || *t == TypeDescriptor::Unsigned(IntSize::U1),
                "Invalid uncertainty type column"
            );
            for (row, value) in rows.iter_mut().zip(column::<u8, 14>(&table)?) {
                ensure!(value <= 4, "Unknown bathymetric uncertainty code {value}");
                row.uncertainty_type = Some(value);
            }
        }
        let mut records = HashMap::with_capacity(rows.len());
        for row in rows {
            row.validate_semantics()?;
            ensure!(
                row.data_assessment.is_none_or(|v| v <= 3),
                "Unknown data assessment code"
            );
            for v in [
                row.size_of_features_detected,
                row.feature_size_variation,
                row.horizontal_uncertainty_fixed,
                row.horizontal_uncertainty_variable_factor,
            ]
            .into_iter()
            .flatten()
            {
                ensure!(v.is_finite(), "Non-finite quality attribute");
            }
            let id = row.id;
            ensure!(
                records.insert(id, row).is_none(),
                "Duplicate quality record id {id}"
            );
        }
        let domain = crate::InstanceDomain::read(&g, &geometry)?;
        // Standalone quality-adapter fixtures need not be whole products. The
        // BathymetryCoverage product entrypoint requires the root attributes.
        let root_names = [
            "westBoundLongitude",
            "eastBoundLongitude",
            "southBoundLatitude",
            "northBoundLatitude",
        ];
        let attrs = file.attr_names()?;
        let root_enclosure = if root_names
            .iter()
            .any(|n| attrs.iter().any(|a| a.as_str() == *n))
        {
            Some(crate::RootBounds::read(file)?.assess(&geometry, &domain, &g)?)
        } else {
            None
        };
        Ok(Some(Arc::new(Self {
            axes,
            root_enclosure,
            domain,
            horizontal_position_uncertainty,
            vertical_position_uncertainty,
            outside_domain: std::sync::atomic::AtomicBool::new(false),
            geometry,
            values,
            records,
        })))
    }
    pub fn geometry(&self) -> &GridGeometry {
        &self.geometry
    }
    pub fn record_count(&self) -> usize {
        self.records.len()
    }
    pub fn records(&self) -> impl Iterator<Item = &QualityRecord> {
        self.records.values()
    }
    /// Nonzero IDs at original sample positions outside this quality instance's
    /// own domain, observed only in successfully decoded requested windows.
    /// This diagnostic does not overwrite IDs or borrow any bathymetry VD mask.
    pub fn observed_id_centroids_outside_domain(&self) -> bool {
        self.outside_domain
            .load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn read_window_ids(&self, window: GridWindow) -> Result<Vec<u32>> {
        window.validate(&self.geometry)?;
        let ids: Vec<_> = self
            .values
            .read_slice_2d::<u32, _>((
                window.row..window.row + window.height,
                window.column..window.column + window.width,
            ))?
            .into_iter()
            .collect();
        ensure!(
            ids.iter()
                .all(|id| *id == 0 || self.records.contains_key(id)),
            "Quality grid references a missing attribute record"
        );
        if self.domain.requires_mask()
            && ids.iter().enumerate().any(|(index, id)| {
                if *id == 0 {
                    return false;
                }
                let (x, y) = self
                    .geometry
                    .position(
                        window.column + index % window.width,
                        window.row + index / window.width,
                    )
                    .expect("Validated quality window");
                !self.domain.contains(x, y)
            })
        {
            self.outside_domain
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(ids)
    }
    pub fn sample_nearest(&self, x: f64, y: f64) -> Result<Option<&QualityRecord>> {
        if !self.domain.contains(x, y) {
            return Ok(None);
        }
        let Some((column, row)) = self.geometry.nearest(x, y) else {
            return Ok(None);
        };
        let id = self.read_window_ids(GridWindow {
            column,
            row,
            width: 1,
            height: 1,
        })?[0];
        Ok((id != 0).then(|| self.records.get(&id)).flatten())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[allow(non_snake_case)]
    #[derive(H5Type, Clone)]
    #[repr(C)]
    struct Row {
        id: u32,
        dataAssessment: u8,
        fullSeafloorCoverageAchieved: u8,
        bathyCoverage: u8,
        sourceSurveyID: VarLenAscii,
        surveyAuthority: VarLenUnicode,
        typeOfBathymetricEstimationUncertainty: Uncertainty,
    }
    #[derive(H5Type, Clone, Copy)]
    #[repr(u8)]
    enum Uncertainty {
        Unknown = 0,
        Product = 3,
    }
    fn attr<T: H5Type>(g: &hdf5::Group, name: &str, value: T) {
        g.new_attr::<T>()
            .create(name)
            .unwrap()
            .write_scalar(&value)
            .unwrap();
    }
    fn text(g: &hdf5::Group, name: &str, value: &str) {
        attr(g, name, VarLenAscii::from_ascii(value).unwrap());
    }
    fn fixture(
        ids: Vec<u32>,
        duplicate: bool,
        bad_bool: bool,
    ) -> (std::path::PathBuf, GridGeometry) {
        static NEXT_FILE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "s102-quality-{}-{}.h5",
            std::process::id(),
            NEXT_FILE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let f = hdf5::File::create(&path).unwrap();
        let q = f.create_group("QualityOfBathymetryCoverage").unwrap();
        let b = f.create_group("BathymetryCoverage").unwrap();
        for group in [&q, &b] {
            for (n, v) in [
                ("dimension", 2u8),
                ("commonPointRule", 2),
                ("interpolationType", 1),
                ("numInstances", 1),
            ] {
                attr(group, n, v);
            }
            attr(group, "horizontalPositionUncertainty", -1f32);
            attr(group, "verticalUncertainty", -1f32);
        }
        crate::write_test_axes(&q, 4326);
        attr(&q, "dataCodingFormat", 9u8);
        attr(&q, "dataOffsetCode", 5u8);
        attr(&q, "sequencingRule.type", 1u8);
        text(&q, "sequencingRule.scanDirection", "Longitude, Latitude");
        let g = q.create_group("QualityOfBathymetryCoverage.01").unwrap();
        attr(&g, "numGRP", 1u32);
        text(&g, "startSequence", "0,0");
        attr(&g, "numPointsLongitudinal", 3u32);
        attr(&g, "numPointsLatitudinal", 2u32);
        attr(&g, "gridOriginLongitude", 1f64);
        attr(&g, "gridOriginLatitude", 20f64);
        attr(&g, "gridSpacingLongitudinal", 1f64);
        attr(&g, "gridSpacingLatitudinal", 1f64);
        for (n, v) in [
            ("westBoundLongitude", 0.5f32),
            ("eastBoundLongitude", 3.5),
            ("southBoundLatitude", 19.5),
            ("northBoundLatitude", 21.5),
        ] {
            attr(&g, n, v);
        }
        let values = g
            .create_group("Group_001")
            .unwrap()
            .new_dataset::<u32>()
            .shape([2, 3])
            .create("values")
            .unwrap();
        values.write_raw(&ids).unwrap();
        let row = Row {
            id: 1,
            dataAssessment: 1,
            fullSeafloorCoverageAchieved: if bad_bool { 2 } else { 0 },
            bathyCoverage: 0,
            sourceSurveyID: VarLenAscii::from_ascii("FR-SURVEY-1").unwrap(),
            surveyAuthority: VarLenUnicode::from_str("Shom – 조사").unwrap(),
            typeOfBathymetricEstimationUncertainty: Uncertainty::Product,
        };
        let records = if duplicate {
            vec![row.clone(), row]
        } else {
            vec![row]
        };
        q.new_dataset_builder()
            .with_data(&records)
            .create("featureAttributeTable")
            .unwrap();
        (
            path,
            GridGeometry {
                width: 3,
                height: 2,
                origin_x: 1.,
                origin_y: 20.,
                spacing_x: 1.,
                spacing_y: 1.,
                horizontal_crs: 4326,
            },
        )
    }
    use std::str::FromStr;
    #[test]
    fn missing_quality_metadata_and_wrong_precision_fail_before_values_loading() {
        for missing in [
            "dimension",
            "commonPointRule",
            "interpolationType",
            "horizontalPositionUncertainty",
            "verticalUncertainty",
            "wrongPrecision",
        ] {
            let (template, geometry) = fixture(vec![1; 6], false, false);
            let p = std::env::temp_dir().join(format!(
                "s102-quality-missing-{}-{}.h5",
                std::process::id(),
                missing
            ));
            {
                let f = hdf5::File::create(&p).unwrap();
                let q = f.create_group("QualityOfBathymetryCoverage").unwrap();
                for (n, v) in [
                    ("dimension", 2u8),
                    ("commonPointRule", 2),
                    ("interpolationType", 1),
                    ("numInstances", 1),
                ] {
                    if n != missing {
                        attr(&q, n, v);
                    }
                }
                for n in ["horizontalPositionUncertainty", "verticalUncertainty"] {
                    if n != missing {
                        if missing == "wrongPrecision" && n == "horizontalPositionUncertainty" {
                            attr(&q, n, -1f64);
                        } else {
                            attr(&q, n, -1f32);
                        }
                    }
                }
            }
            {
                let f = hdf5::File::open(&p).unwrap();
                let error = QualityCoverage::open_optional(&f, &[geometry])
                    .unwrap_err()
                    .to_string();
                assert!(
                    error.contains(if missing == "wrongPrecision" {
                        "float32"
                    } else {
                        missing
                    }),
                    "{missing}: {error}"
                );
            }
            std::fs::remove_file(p).unwrap();
            std::fs::remove_file(template).unwrap();
        }
        let (p, g) = fixture(vec![1; 6], false, false);
        {
            let f = hdf5::File::open_rw(&p).unwrap();
            f.group("QualityOfBathymetryCoverage")
                .unwrap()
                .attr("verticalUncertainty")
                .unwrap()
                .write_scalar(&0.25f32)
                .unwrap();
        }
        {
            let f = hdf5::File::open(&p).unwrap();
            assert!(QualityCoverage::open_optional(&f, &[g])
                .unwrap_err()
                .to_string()
                .contains("must match"));
        }
        std::fs::remove_file(p).unwrap();
    }
    #[test]
    fn shared_quality_requires_complete_inherited_metadata_without_silent_substitution() {
        for (field, value) in [
            ("dimension", 3u8),
            ("commonPointRule", 1),
            ("interpolationType", 2),
        ] {
            let (p, g) = fixture(vec![1; 6], false, false);
            {
                let f = hdf5::File::open_rw(&p).unwrap();
                f.group("QualityOfBathymetryCoverage")
                    .unwrap()
                    .attr(field)
                    .unwrap()
                    .write_scalar(&value)
                    .unwrap();
            }
            {
                let f = hdf5::File::open(&p).unwrap();
                assert!(QualityCoverage::open_optional(&f, &[g]).is_err(), "{field}");
            }
            std::fs::remove_file(p).unwrap();
        }
        let (p, g) = fixture(vec![1; 6], false, false);
        {
            let f = hdf5::File::open_rw(&p).unwrap();
            f.group("QualityOfBathymetryCoverage")
                .unwrap()
                .attr("horizontalPositionUncertainty")
                .unwrap()
                .write_scalar(&0.5f32)
                .unwrap();
        }
        {
            let f = hdf5::File::open(&p).unwrap();
            assert!(QualityCoverage::open_optional(&f, &[g]).is_err());
        }
        {
            let f = hdf5::File::open_rw(&p).unwrap();
            f.group("BathymetryCoverage")
                .unwrap()
                .attr("horizontalPositionUncertainty")
                .unwrap()
                .write_scalar(&0.5f32)
                .unwrap();
            f.group("BathymetryCoverage")
                .unwrap()
                .attr("numInstances")
                .unwrap()
                .write_scalar(&2u8)
                .unwrap();
        }
        {
            let f = hdf5::File::open(&p).unwrap();
            let q = QualityCoverage::open_optional(&f, &[g, g])
                .unwrap()
                .unwrap();
            assert_eq!(q.horizontal_position_uncertainty, 0.5);
            assert_eq!(q.vertical_position_uncertainty, -1.);
        }
        std::fs::remove_file(p).unwrap();
    }
    #[test]
    fn quality_projection_enum_unicode_false_and_missing_are_distinct() {
        let (path, g) = fixture(vec![0, 1, 1, 1, 0, 1], false, false);
        let f = hdf5::File::open(&path).unwrap();
        let q = QualityCoverage::open_optional(&f, &[g]).unwrap().unwrap();
        assert_eq!(
            q.read_window_ids(GridWindow {
                column: 1,
                row: 0,
                width: 2,
                height: 2
            })
            .unwrap(),
            [1, 1, 0, 1]
        );
        assert!(q.sample_nearest(1., 20.).unwrap().is_none());
        let record = q.sample_nearest(3., 21.).unwrap().unwrap();
        assert_eq!(record.id, 1);
        assert_eq!(record.full_seafloor_coverage, Some(false));
        assert_eq!(record.significant_feature_detection_capability, None);
        assert_eq!(record.uncertainty_type, Some(3));
        assert_eq!(record.survey_authority.as_deref(), Some("Shom – 조사"));
        assert!(q.sample_nearest(20., 20.).unwrap().is_none());
        drop(q);
        drop(f);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn quality_uses_own_continuous_polygon_and_retains_original_ids() {
        #[derive(H5Type, Clone, Copy)]
        #[repr(C)]
        struct Vertex {
            longitude: f64,
            latitude: f64,
        }
        let (path, geometry) = fixture(vec![1; 6], false, false);
        {
            let f = hdf5::File::open_rw(&path).unwrap();
            let container = f.group("QualityOfBathymetryCoverage").unwrap();
            container
                .relink("QualityOfBathymetryCoverage.01", "Saved")
                .unwrap();
            let old = container.group("Saved").unwrap();
            let g = container
                .create_group("QualityOfBathymetryCoverage.01")
                .unwrap();
            for name in ["numGRP", "numPointsLongitudinal", "numPointsLatitudinal"] {
                attr(
                    &g,
                    name,
                    old.attr(name).unwrap().read_scalar::<u32>().unwrap(),
                );
            }
            for name in [
                "gridOriginLongitude",
                "gridOriginLatitude",
                "gridSpacingLongitudinal",
                "gridSpacingLatitudinal",
            ] {
                attr(
                    &g,
                    name,
                    old.attr(name).unwrap().read_scalar::<f64>().unwrap(),
                );
            }
            text(&g, "startSequence", "0,0");
            old.relink(
                "Group_001",
                "/QualityOfBathymetryCoverage/QualityOfBathymetryCoverage.01/Group_001",
            )
            .unwrap();
            let vertices = [[0.5, 19.5], [3.5, 19.5], [0.5, 21.5], [0.5, 19.5]].map(|p| Vertex {
                longitude: p[0],
                latitude: p[1],
            });
            g.new_dataset::<Vertex>()
                .shape(4)
                .create("domainExtent.polygon")
                .unwrap()
                .write_raw(&vertices)
                .unwrap();
            container.unlink("Saved").unwrap();
        }
        let f = hdf5::File::open(&path).unwrap();
        let q = QualityCoverage::open_optional(&f, &[geometry])
            .unwrap()
            .unwrap();
        assert!(!q.observed_id_centroids_outside_domain());
        assert_eq!(q.sample_nearest(1., 20.).unwrap().unwrap().id, 1);
        assert!(!q.observed_id_centroids_outside_domain());
        assert!(q.sample_nearest(3., 20.).unwrap().is_none());
        assert!(!q.observed_id_centroids_outside_domain());
        assert_eq!(q.sample_nearest(2.75, 19.75).unwrap().unwrap().id, 1);
        assert!(q.observed_id_centroids_outside_domain());
        assert_eq!(q.sample_nearest(2.75, 20.).unwrap().unwrap().id, 1);
        assert!(q
            .sample_nearest(2.75, f64::from_bits(20f64.to_bits() + 1))
            .unwrap()
            .is_none());
        assert_eq!(
            q.sample_nearest(2.75, f64::from_bits(20f64.to_bits() - 1))
                .unwrap()
                .unwrap()
                .id,
            1
        );
        assert_eq!(
            q.read_window_ids(GridWindow {
                column: 0,
                row: 0,
                width: 3,
                height: 2
            })
            .unwrap(),
            [1; 6]
        );
        drop(q);
        drop(f);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn duplicate_records_and_invalid_boolean_are_rejected() {
        for (duplicate, bad_bool) in [(true, false), (false, true)] {
            let (path, g) = fixture(vec![1; 6], duplicate, bad_bool);
            let f = hdf5::File::open(&path).unwrap();
            assert!(QualityCoverage::open_optional(&f, &[g]).is_err());
            drop(f);
            std::fs::remove_file(path).unwrap();
        }
    }
    #[test]
    fn declared_instance_count_must_match_shared_grid() {
        let (path, g) = fixture(vec![1; 6], false, false);
        let f = hdf5::File::open_rw(&path).unwrap();
        let group = f.group("QualityOfBathymetryCoverage").unwrap();
        for count in [0u8, 2u8] {
            group
                .attr("numInstances")
                .unwrap()
                .write_scalar(&count)
                .unwrap();
            assert!(QualityCoverage::open_optional(&f, &[g]).is_err());
        }
        group
            .attr("numInstances")
            .unwrap()
            .write_scalar(&1u8)
            .unwrap();
        assert!(QualityCoverage::open_optional(&f, &[g]).is_ok());
        drop(group);
        drop(f);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn orphan_ids_and_misaligned_geometry_are_rejected() {
        let (path, g) = fixture(vec![0, 1, 9, 1, 0, 1], false, false);
        let f = hdf5::File::open(&path).unwrap();
        let q = QualityCoverage::open_optional(&f, &[g]).unwrap().unwrap();
        assert!(q
            .read_window_ids(GridWindow {
                column: 2,
                row: 0,
                width: 1,
                height: 1
            })
            .is_err());
        let shifted = GridGeometry {
            origin_x: 1.0001,
            ..g
        };
        assert!(QualityCoverage::open_optional(&f, &[shifted]).is_err());
        drop(q);
        drop(f);
        std::fs::remove_file(path).unwrap();
    }
}

#[cfg(test)]
mod encoding_tests {
    use super::*;
    #[test]
    fn invalid_utf8_retains_each_original_byte_with_explicit_display_interpretation() {
        let bytes = b"D\xe9partementale";
        let (display, warning) = decode_string_bytes(bytes, false);
        assert_eq!(display, "Départementale");
        assert!(warning.unwrap().contains("unverified"));
        assert_eq!(display.chars().map(|c| c as u8).collect::<Vec<_>>(), bytes);
        let (_, warning) = decode_string_bytes("조사".as_bytes(), true);
        assert!(warning.is_some());
        assert!(decode_string_bytes("조사".as_bytes(), false).1.is_none());
    }
}

#[cfg(test)]
mod semantic_tests {
    use super::*;
    #[test]
    fn rejects_contradictory_coverage_but_preserves_absent_flags() {
        for (full, observed, valid) in [
            (Some(false), Some(true), false),
            (Some(false), Some(false), true),
            (Some(true), Some(false), true),
            (Some(true), Some(true), true),
            (None, Some(true), true),
            (Some(false), None, true),
        ] {
            let record = QualityRecord {
                id: 9,
                full_seafloor_coverage: full,
                bathymetry_observed: observed,
                ..Default::default()
            };
            assert_eq!(record.validate_semantics().is_ok(), valid);
        }
    }
    #[test]
    fn accepts_truncated_dates_and_rejects_invalid_hdf5_date_encodings() {
        for date in ["20240229", "2024----", "----0229", "------31"] {
            let record = QualityRecord {
                survey_date_start: Some(date.into()),
                ..Default::default()
            };
            assert!(record.validate_semantics().is_ok(), "{date}");
        }
        for date in ["20230229", "--------", "2024-02-29", "2024-02", "20241301"] {
            for start in [true, false] {
                let mut record = QualityRecord::default();
                if start {
                    record.survey_date_start = Some(date.into());
                } else {
                    record.survey_date_end = Some(date.into());
                }
                assert!(record.validate_semantics().is_err(), "{date}");
            }
        }
    }
}
