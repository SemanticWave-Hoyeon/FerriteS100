//! Read-only S-102 2.1 trial-data inspection. Deliberately NOT a CoverageSource:
//! unknown vertical references must never enter depth composition/safety queries.
//! This bounded receiver profile is not a full S-102 2.1 conformity validator.
use anyhow::{ensure, Context, Result};
use hdf5::{
    types::{FixedAscii, FixedUnicode, FloatSize, TypeDescriptor, VarLenAscii, VarLenUnicode},
    Group,
};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrialVerticalDatum {
    Missing,
    Encoded(u32),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrialDiagnostic {
    MissingVerticalDatum,
    MissingDepthExtrema,
    MissingUncertaintyExtrema,
    /// Original trial metadata says H5T_NATIVE_FLOAT; the actual array is float32.
    LegacyFeatureDatatypeAlias,
}
#[derive(Debug, Clone, Copy)]
pub struct TrialGrid {
    pub width: usize,
    pub height: usize,
    pub origin_longitude: f64,
    pub origin_latitude: f64,
    pub spacing_longitude: f64,
    pub spacing_latitude: f64,
}
impl TrialGrid {
    /// Encoded sample coordinates, with no half-cell shift or interpolation.
    pub fn node(&self, column: usize, row: usize) -> Option<[f64; 2]> {
        (column < self.width && row < self.height).then_some([
            self.origin_longitude + column as f64 * self.spacing_longitude,
            self.origin_latitude + row as f64 * self.spacing_latitude,
        ])
    }
}
#[derive(Debug, Clone, Copy)]
pub struct TrialSample {
    pub depth: f32,
    pub uncertainty: f32,
}
/// Inspection data only: even an encoded datum does not promote this trial
/// reader to an operational BathymetryCoverage or certify product conformance.
#[derive(Debug)]
pub struct LegacyTrialGrid {
    values: hdf5::Dataset,
    geometry: TrialGrid,
    pub product_specification: String,
    pub issue_date_lexical: String,
    pub root_bounds: [f64; 4],
    pub vertical_datum: TrialVerticalDatum,
    pub common_point_rule: u8,
    pub feature_definitions: Vec<crate::FeatureDefinition>,
    pub declared_depth_extrema: Option<[f64; 2]>,
    pub declared_uncertainty_extrema: Option<[f64; 2]>,
    pub diagnostics: Vec<TrialDiagnostic>,
}
// UKHO trial attributes use a one-element rank1 dataspace rather than rank0.
// Accept exactly these two shapes in this isolated receiver profile; the 3.0
// scalar decoder remains unchanged and retains its stricter product contract.
fn one_attribute(g: &Group, name: &str) -> Result<hdf5::Attribute> {
    let a = g.attr(name)?;
    ensure!(
        a.is_scalar() || a.shape() == [1],
        "Trial {name} requires scalar or singleton rank1 encoding"
    );
    Ok(a)
}
fn one<T: hdf5::H5Type>(a: &hdf5::Attribute) -> Result<T> {
    // All callers first validate the dataspace and datatype. read_raw supports
    // rank0 and [1] uniformly, with exactly one element admitted beforehand.
    let mut values = a.read_raw::<T>()?;
    ensure!(values.len() == 1, "Trial attribute element count mismatch");
    Ok(values.pop().unwrap())
}
fn integer(g: &Group, name: &str) -> Result<u64> {
    let a = one_attribute(g, name)?;
    match a.dtype()?.to_descriptor()? {
        TypeDescriptor::Unsigned(_) => one::<u64>(&a),
        TypeDescriptor::Integer(_) => {
            let value = one::<i64>(&a)?;
            ensure!(value >= 0, "Trial {name} integer must be nonnegative");
            Ok(value as u64)
        }
        TypeDescriptor::Enum(e) if !e.signed => one::<u64>(&a),
        TypeDescriptor::Enum(_) => {
            let value = one::<i64>(&a)?;
            ensure!(value >= 0, "Trial {name} enumeration must be nonnegative");
            Ok(value as u64)
        }
        _ => {
            anyhow::bail!("Trial {name} requires integer or explicit enumeration; coercion refused")
        }
    }
}
fn unsigned32(g: &Group, name: &str) -> Result<u32> {
    u32::try_from(integer(g, name)?)
        .with_context(|| format!("Trial {name} exceeds unsigned32 range"))
}
fn unsigned8(g: &Group, name: &str) -> Result<u8> {
    u8::try_from(integer(g, name)?).with_context(|| format!("Trial {name} exceeds unsigned8 range"))
}
fn text(g: &Group, name: &str) -> Result<String> {
    let a = one_attribute(g, name)?;
    let checked = |bytes: &[u8], ascii: bool| -> Result<String> {
        ensure!(
            bytes.len() <= 4096 && (!ascii || bytes.is_ascii()),
            "Trial metadata exceeds4096 bytes or invalid ASCII"
        );
        Ok(std::str::from_utf8(bytes)?.to_owned())
    };
    match a.dtype()?.to_descriptor()? {
        TypeDescriptor::VarLenAscii => checked(one::<VarLenAscii>(&a)?.as_bytes(), true),
        TypeDescriptor::VarLenUnicode => checked(one::<VarLenUnicode>(&a)?.as_bytes(), false),
        TypeDescriptor::FixedAscii(n) => {
            ensure!(n <= 4096, "Trial metadata width exceeds4096");
            checked(one::<FixedAscii<4096>>(&a)?.as_bytes(), true)
        }
        TypeDescriptor::FixedUnicode(n) => {
            ensure!(n <= 4096, "Trial metadata width exceeds4096");
            checked(one::<FixedUnicode<4096>>(&a)?.as_bytes(), false)
        }
        _ => anyhow::bail!("Trial {name} must be an HDF string"),
    }
}
fn number(g: &Group, name: &str) -> Result<f64> {
    let a = one_attribute(g, name)?;
    ensure!(
        matches!(
            a.dtype()?.to_descriptor()?,
            TypeDescriptor::Float(FloatSize::U4 | FloatSize::U8)
        ),
        "Trial {name} requires floating point encoding"
    );
    let v = one::<f64>(&a)?;
    ensure!(v.is_finite(), "Trial {name} must be finite");
    Ok(v)
}
fn extrema(g: &Group, min: &str, max: &str) -> Result<Option<[f64; 2]>> {
    let names = g.attr_names()?;
    match (
        names.iter().any(|n| n == min),
        names.iter().any(|n| n == max),
    ) {
        (false, false) => Ok(None),
        (true, true) => {
            let v = [number(g, min)?, number(g, max)?];
            ensure!(v[0] <= v[1], "Trial extrema reversed");
            Ok(Some(v))
        }
        _ => anyhow::bail!("Trial extrema pair incomplete"),
    }
}
impl LegacyTrialGrid {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let file = hdf5::File::open(path)?;
        let product_specification = text(&file, "productSpecification")?;
        ensure!(
            matches!(
                product_specification.as_str(),
                "INT.IHO.S-102.2.1" | "INT.IHO.S-102.2.1.0"
            ),
            "Trial reader requires original S-102 2.1 edition"
        );
        ensure!(
            text(&file, "horizontalDatumReference")? == "EPSG"
                && unsigned32(&file, "horizontalDatumValue")? == 4326,
            "Trial reader currently supports encoded EPSG4326 only"
        );
        let issue_date_lexical = text(&file, "issueDate")?; // preserve old lexical form, never relabel
        ensure!(!issue_date_lexical.is_empty(), "Trial issueDate is empty");
        let root_bounds = [
            number(&file, "westBoundLongitude")?,
            number(&file, "eastBoundLongitude")?,
            number(&file, "southBoundLatitude")?,
            number(&file, "northBoundLatitude")?,
        ];
        ensure!(
            root_bounds[0] >= -180.
                && root_bounds[1] <= 180.
                && root_bounds[0] <= root_bounds[1]
                && root_bounds[2] >= -90.
                && root_bounds[3] <= 90.
                && root_bounds[2] <= root_bounds[3],
            "Trial bbox invalid"
        );
        let vertical_datum = if file.attr_names()?.iter().any(|n| n == "verticalDatum") {
            TrialVerticalDatum::Encoded(unsigned32(&file, "verticalDatum")?)
        } else {
            TrialVerticalDatum::Missing
        };
        let c = file.group("BathymetryCoverage")?;
        ensure!(
            unsigned32(&c, "numInstances")? == 1
                && unsigned32(&c, "dimension")? == 2
                && unsigned8(&c, "dataCodingFormat")? == 2
                && unsigned8(&c, "sequencingRule.type")? == 1,
            "Unsupported trial grid structure"
        );
        ensure!(
            text(&c, "sequencingRule.scanDirection")?
                .split(',')
                .map(str::trim)
                .eq(["Longitude", "Latitude"]),
            "Trial scan direction unsupported"
        );
        let axes = crate::AxisMetadata::read(&c, 4326)?;
        ensure!(
            axes.names == ["Longitude", "Latitude"],
            "Trial axes inconsistent with receiver profile"
        );
        let common_point_rule = unsigned8(&c, "commonPointRule")?;
        ensure!(
            (1..=4).contains(&common_point_rule),
            "Trial common point rule unsupported"
        );
        ensure!(
            unsigned8(&c, "interpolationType")? == 1,
            "Trial interpolation unsupported"
        );
        let instances: Vec<_> = c
            .member_names()?
            .into_iter()
            .filter(|n| n.starts_with("BathymetryCoverage."))
            .collect();
        ensure!(instances.len() == 1, "Trial instance count mismatch");
        let g = c.group(&instances[0])?;
        ensure!(
            unsigned32(&g, "numGRP")? == 1
                && text(&g, "startSequence")?
                    .split(',')
                    .map(str::trim)
                    .eq(["0", "0"]),
            "Trial group sequence unsupported"
        );
        ensure!(
            !g.link_exists("domainExtent.polygon"),
            "Trial polygons not supported by this receiver profile"
        );
        let geometry = TrialGrid {
            width: unsigned32(&g, "numPointsLongitudinal")? as usize,
            height: unsigned32(&g, "numPointsLatitudinal")? as usize,
            origin_longitude: number(&g, "gridOriginLongitude")?,
            origin_latitude: number(&g, "gridOriginLatitude")?,
            spacing_longitude: number(&g, "gridSpacingLongitudinal")?,
            spacing_latitude: number(&g, "gridSpacingLatitudinal")?,
        };
        ensure!(
            geometry.width > 0
                && geometry.height > 0
                && geometry
                    .width
                    .checked_mul(geometry.height)
                    .is_some_and(|n| n <= 100_000_000),
            "Trial grid exceeds receiver admission budget"
        );
        ensure!(
            geometry.spacing_longitude > 0. && geometry.spacing_latitude > 0.,
            "Trial spacing must be positive"
        );
        let last = geometry
            .node(geometry.width - 1, geometry.height - 1)
            .unwrap();
        ensure!(
            geometry.origin_longitude >= -180.
                && last[0] <= 180.
                && geometry.origin_latitude >= -90.
                && last[1] <= 90.,
            "Trial sample coordinates outside geographic axes"
        );
        let group = g.group("Group_001")?;
        let values = group.dataset("values")?;
        ensure!(
            values.shape() == [geometry.height, geometry.width],
            "Trial shape mismatch"
        );
        let TypeDescriptor::Compound(fields) = values.dtype()?.to_descriptor()? else {
            anyhow::bail!("Trial values must be compound")
        };
        ensure!(
            fields.fields.len() == 2
                && ["depth", "uncertainty"].into_iter().all(|name| fields
                    .fields
                    .iter()
                    .filter(|f| f.name == name && f.ty == TypeDescriptor::Float(FloatSize::U4))
                    .count()
                    == 1),
            "Trial values require float32 depth and uncertainty"
        );
        let gf = file.group("Group_F")?;
        let codes = gf.dataset("featureCode")?;
        ensure!(
            codes.shape() == [1],
            "Trial requires only BathymetryCoverage feature declaration"
        );
        let ty = codes.dtype()?;
        let space = codes.space()?;
        let mut bytes = 0;
        let status = hdf5::sync::sync(|| unsafe {
            // Dataset, datatype and dataspace handles stay live under the HDF5 lock.
            hdf5_sys::h5d::H5Dvlen_get_buf_size(codes.id(), ty.id(), space.id(), &mut bytes)
        });
        ensure!(
            status >= 0 && bytes <= 4097,
            "Trial featureCode payload exceeds receiver budget"
        );
        let code = match ty.to_descriptor()? {
            TypeDescriptor::VarLenAscii => codes
                .read_raw::<VarLenAscii>()?
                .into_iter()
                .next()
                .context("Missing trial feature code")?
                .as_str()
                .to_owned(),
            TypeDescriptor::VarLenUnicode => codes
                .read_raw::<VarLenUnicode>()?
                .into_iter()
                .next()
                .context("Missing trial feature code")?
                .as_str()
                .to_owned(),
            _ => anyhow::bail!("Trial featureCode requires variable length strings"),
        };
        ensure!(
            code == "BathymetryCoverage",
            "Trial featureCode unsupported"
        );
        let feature_definitions =
            crate::feature_metadata::definitions(&gf, "BathymetryCoverage", 2)?;
        ensure!(
            feature_definitions.len() == 2
                && ["depth", "uncertainty"]
                    .into_iter()
                    .all(|code| feature_definitions
                        .iter()
                        .filter(|r| r.code == code
                            && r.unit == "metres"
                            && matches!(r.datatype.as_str(), "H5T_FLOAT" | "H5T_NATIVE_FLOAT"))
                        .count()
                        == 1),
            "Trial feature definitions unsupported"
        );
        // Keep lexical fill values and original sample bits, no synthetic no-data or extrema.
        let declared_depth_extrema = extrema(&group, "minimumDepth", "maximumDepth")?;
        let declared_uncertainty_extrema =
            extrema(&group, "minimumUncertainty", "maximumUncertainty")?;
        let mut diagnostics = Vec::new();
        if feature_definitions
            .iter()
            .any(|r| r.datatype == "H5T_NATIVE_FLOAT")
        {
            diagnostics.push(TrialDiagnostic::LegacyFeatureDatatypeAlias);
        }
        if vertical_datum == TrialVerticalDatum::Missing {
            diagnostics.push(TrialDiagnostic::MissingVerticalDatum);
        }
        if declared_depth_extrema.is_none() {
            diagnostics.push(TrialDiagnostic::MissingDepthExtrema);
        }
        if declared_uncertainty_extrema.is_none() {
            diagnostics.push(TrialDiagnostic::MissingUncertaintyExtrema);
        }
        Ok(Self {
            values,
            geometry,
            product_specification,
            issue_date_lexical,
            root_bounds,
            vertical_datum,
            common_point_rule,
            feature_definitions,
            declared_depth_extrema,
            declared_uncertainty_extrema,
            diagnostics,
        })
    }
    pub fn geometry(&self) -> TrialGrid {
        self.geometry
    }
    /// Raw bounded sample inspection only. No portrayal, depth filtering, datum
    /// conversion, inferred safety contour, or permission to interpolate.
    pub fn read_window(
        &self,
        column: usize,
        row: usize,
        width: usize,
        height: usize,
    ) -> Result<Vec<TrialSample>> {
        ensure!(
            width > 0
                && height > 0
                && column
                    .checked_add(width)
                    .is_some_and(|n| n <= self.geometry.width)
                && row
                    .checked_add(height)
                    .is_some_and(|n| n <= self.geometry.height),
            "Trial window outside source"
        );
        let count = width.checked_mul(height).context("Trial window overflow")?;
        ensure!(
            count <= 65_536,
            "Trial window exceeds65536 sample receiver budget"
        );
        Ok(self
            .values
            .read_slice_2d::<crate::DepthValue, _>((row..row + height, column..column + width))?
            .into_iter()
            .map(|v| TrialSample {
                depth: v.depth,
                uncertainty: v.uncertainty,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hdf5::{types::VarLenAscii, H5Type};
    struct Fixture(std::path::PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    fn attr<T: H5Type>(g: &Group, n: &str, v: T) {
        g.new_attr::<T>()
            .create(n)
            .unwrap()
            .write_scalar(&v)
            .unwrap();
    }
    fn s(g: &Group, n: &str, v: &str) {
        attr(g, n, VarLenAscii::from_ascii(v).unwrap());
    }
    fn fixture() -> Fixture {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let p = std::env::temp_dir().join(format!(
            "legacy-trial-{}-{}.h5",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        {
            let f = hdf5::File::create(&p).unwrap();
            s(&f, "productSpecification", "INT.IHO.S-102.2.1");
            s(&f, "issueDate", "2021-03-24Z");
            s(&f, "horizontalDatumReference", "EPSG");
            attr(&f, "horizontalDatumValue", 4326i32);
            for (n, v) in [
                ("westBoundLongitude", -1.),
                ("eastBoundLongitude", -0.9),
                ("southBoundLatitude", 50.7),
                ("northBoundLatitude", 50.8),
            ] {
                attr(&f, n, v);
            }
            let c = f.create_group("BathymetryCoverage").unwrap();
            for (n, v) in [
                ("numInstances", 1u32),
                ("dimension", 2),
                ("dataCodingFormat", 2),
                ("sequencingRule.type", 1),
                ("commonPointRule", 1),
                ("interpolationType", 1),
            ] {
                attr(&c, n, v);
            }
            s(&c, "sequencingRule.scanDirection", "Longitude, Latitude");
            c.new_dataset::<VarLenAscii>()
                .shape(2)
                .create("axisNames")
                .unwrap()
                .write_raw(&["Longitude", "Latitude"].map(|v| VarLenAscii::from_ascii(v).unwrap()))
                .unwrap();
            let g = c.create_group("BathymetryCoverage.01").unwrap();
            attr(&g, "numGRP", 1u32);
            attr(&g, "numPointsLongitudinal", 2u32);
            attr(&g, "numPointsLatitudinal", 2u32);
            s(&g, "startSequence", "0,0");
            for (n, v) in [
                ("gridOriginLongitude", -1.),
                ("gridOriginLatitude", 50.7),
                ("gridSpacingLongitudinal", 0.0001),
                ("gridSpacingLatitudinal", 0.0001),
            ] {
                attr(&g, n, v);
            }
            let values = g.create_group("Group_001").unwrap();
            values
                .new_dataset::<crate::DepthValue>()
                .shape((2, 2))
                .create("values")
                .unwrap()
                .write_raw(&[
                    crate::DepthValue {
                        depth: -0.0,
                        uncertainty: 1.,
                    },
                    crate::DepthValue {
                        depth: 2.,
                        uncertainty: 1_000_000.,
                    },
                    crate::DepthValue {
                        depth: 1_000_000.,
                        uncertainty: 0.5,
                    },
                    crate::DepthValue {
                        depth: 7.,
                        uncertainty: 0.2,
                    },
                ])
                .unwrap();
            let gf = f.create_group("Group_F").unwrap();
            gf.new_dataset::<VarLenAscii>()
                .shape(1)
                .create("featureCode")
                .unwrap()
                .write_raw(&[VarLenAscii::from_ascii("BathymetryCoverage").unwrap()])
                .unwrap();
            gf.new_dataset::<crate::Definition>()
                .shape(2)
                .create("BathymetryCoverage")
                .unwrap()
                .write_raw(&[
                    crate::Definition::for_code("depth"),
                    crate::Definition::for_code("uncertainty"),
                ])
                .unwrap();
        }
        Fixture(p)
    }
    #[test]
    fn legacy_and_30_decoders_admit_the_same_scalar_or_singleton_shapes() {
        let f = fixture();
        let h = hdf5::File::open_rw(&f.0).unwrap();
        h.new_attr::<VarLenAscii>()
            .shape(1)
            .create("singletonText")
            .unwrap()
            .write_raw(&[VarLenAscii::from_ascii("2021-03-24Z").unwrap()])
            .unwrap();
        h.new_attr::<f64>()
            .shape(1)
            .create("singletonFloat")
            .unwrap()
            .write_raw(&[-0.0])
            .unwrap();
        h.new_attr::<u32>()
            .shape(1)
            .create("singletonInteger")
            .unwrap()
            .write_raw(&[4326])
            .unwrap();
        assert_eq!(text(&h, "singletonText").unwrap(), "2021-03-24Z");
        assert_eq!(
            number(&h, "singletonFloat").unwrap().to_bits(),
            (-0.0f64).to_bits()
        );
        assert_eq!(unsigned32(&h, "singletonInteger").unwrap(), 4326);
        // UKHO 2026 S-102 3.0 uses the same singleton encoding (crate::singleton).
        assert_eq!(crate::scalar::u32(&h, "singletonInteger").unwrap(), 4326);
        assert!(unsigned32(&h, "singletonFloat").is_err());
        assert!(number(&h, "singletonInteger").is_err());
        for (name, shape) in [
            ("vector", vec![2]),
            ("matrix", vec![1, 1]),
            ("empty", vec![0]),
        ] {
            h.new_attr::<u32>().shape(shape).create(name).unwrap();
            assert!(unsigned32(&h, name).is_err());
            assert!(crate::scalar::u32(&h, name).is_err());
        }
        attr(&h, "negative", -1i32);
        attr(&h, "tooWide", u64::from(u32::MAX) + 1);
        assert!(unsigned32(&h, "negative").is_err());
        assert!(unsigned32(&h, "tooWide").is_err());
        attr(&h, "tooWide8", 256u16);
        assert!(unsigned8(&h, "tooWide8").is_err());
    }
    #[test]
    fn original_trial_bits_unknown_reference_and_node_coordinates_are_preserved() {
        let f = fixture();
        let grid = LegacyTrialGrid::open(&f.0).unwrap();
        assert_eq!(grid.vertical_datum, TrialVerticalDatum::Missing);
        assert_eq!(grid.issue_date_lexical, "2021-03-24Z");
        assert_eq!(grid.common_point_rule, 1);
        assert_eq!(grid.geometry.node(0, 0), Some([-1., 50.7]));
        assert_eq!(
            grid.geometry.node(1, 1),
            Some([-1. + 0.0001, 50.7 + 0.0001])
        );
        let values = grid.read_window(0, 0, 2, 2).unwrap();
        assert_eq!(values[0].depth.to_bits(), (-0.0f32).to_bits());
        assert_eq!(values[1].uncertainty.to_bits(), 1_000_000f32.to_bits());
        assert_eq!(values[2].depth.to_bits(), 1_000_000f32.to_bits());
        assert_eq!(grid.declared_depth_extrema, None);
        assert!(grid
            .diagnostics
            .contains(&TrialDiagnostic::MissingVerticalDatum));
        assert!(grid.read_window(usize::MAX, 0, 1, 1).is_err());
        assert!(grid.read_window(0, 0, 0, 1).is_err());
    }
    #[test]
    fn legacy_feature_datatype_alias_is_preserved_and_diagnosed() {
        let f = fixture();
        {
            let h = hdf5::File::open_rw(&f.0).unwrap();
            let dataset = h.dataset("Group_F/BathymetryCoverage").unwrap();
            let mut rows = [
                crate::Definition::for_code("depth"),
                crate::Definition::for_code("uncertainty"),
            ];
            for row in &mut rows {
                row.datatype = VarLenAscii::from_ascii("H5T_NATIVE_FLOAT").unwrap();
            }
            dataset.write_raw(&rows).unwrap();
        }
        let grid = LegacyTrialGrid::open(&f.0).unwrap();
        assert!(grid
            .feature_definitions
            .iter()
            .all(|r| r.datatype == "H5T_NATIVE_FLOAT"));
        assert!(grid
            .diagnostics
            .contains(&TrialDiagnostic::LegacyFeatureDatatypeAlias));
        assert_eq!(
            grid.read_window(0, 0, 1, 1).unwrap()[0].depth.to_bits(),
            (-0.0f32).to_bits()
        );
        drop(grid); // Close the read-only HDF handle before rewriting the fixture.
                    // Unknown aliases remain unsupported, even though this is inspection only.
        {
            let h = hdf5::File::open_rw(&f.0).unwrap();
            let mut rows = [
                crate::Definition::for_code("depth"),
                crate::Definition::for_code("uncertainty"),
            ];
            rows[0].datatype = VarLenAscii::from_ascii("H5T_INTEGER").unwrap();
            h.dataset("Group_F/BathymetryCoverage")
                .unwrap()
                .write_raw(&rows)
                .unwrap();
        }
        assert!(LegacyTrialGrid::open(&f.0).is_err());
    }
    #[test]
    fn original_known_code_is_preserved_without_promoting_trial_to_operational() {
        let f = fixture();
        {
            let h = hdf5::File::open_rw(&f.0).unwrap();
            attr(&h, "verticalDatum", 10u32);
        }
        assert_eq!(
            LegacyTrialGrid::open(&f.0).unwrap().vertical_datum,
            TrialVerticalDatum::Encoded(10)
        );
    }
    #[test]
    fn original_30_encoding_is_not_relabelled_as_legacy() {
        let f = fixture();
        {
            let h = hdf5::File::open_rw(&f.0).unwrap();
            h.attr("productSpecification")
                .unwrap()
                .write_scalar(&VarLenAscii::from_ascii("INT.IHO.S-102.3.0.0").unwrap())
                .unwrap();
        }
        assert!(LegacyTrialGrid::open(&f.0).is_err());
    }
    #[test]
    #[ignore = "Root-controlled read-only original UKHO fixture validation; set FERRITE_UKHO_TRIAL_DIR"]
    fn actual_25_ukho_trials_metadata_and_bounded_raw_windows() {
        let dir = std::path::PathBuf::from(
            std::env::var_os("FERRITE_UKHO_TRIAL_DIR")
                .expect("explicit original fixture directory"),
        );
        // Original trials are nested as <cell>/1/<cell>.h5. Do not follow
        // symlinks while enumerating the root-controlled fixture tree.
        let mut pending = vec![dir];
        let mut files = Vec::new();
        let mut visited = 0usize;
        while let Some(directory) = pending.pop() {
            visited += 1;
            assert!(visited <= 1024, "Unexpected fixture tree size");
            for entry in std::fs::read_dir(directory).unwrap() {
                let entry = entry.unwrap();
                let kind = entry.file_type().unwrap();
                if kind.is_dir() {
                    pending.push(entry.path());
                } else if kind.is_file() && entry.path().extension().is_some_and(|e| e == "h5") {
                    files.push(entry.path());
                }
            }
        }
        files.sort();
        assert_eq!(files.len(), 25);
        for path in files {
            let grid = LegacyTrialGrid::open(&path)
                .unwrap_or_else(|e| panic!("{}: {e:#}", path.display()));
            assert_eq!(grid.product_specification, "INT.IHO.S-102.2.1");
            assert_eq!(grid.vertical_datum, TrialVerticalDatum::Missing);
            for (column, row) in [
                (0, 0),
                (grid.geometry.width - 1, 0),
                (0, grid.geometry.height - 1),
                (grid.geometry.width - 1, grid.geometry.height - 1),
            ] {
                assert_eq!(grid.read_window(column, row, 1, 1).unwrap().len(), 1);
            }
        }
    }
}
