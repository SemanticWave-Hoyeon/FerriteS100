//! S-102 HDF5 adapter with bounded window reads; no application or GPU dependency.
#![allow(non_local_definitions)]
mod scalar;
use anyhow::{ensure, Context, Result};
use ferrite_kernel::{CoverageSample, CoverageSource, CoverageTile, GridGeometry, GridWindow};
pub use hdf5;
use hdf5::{
    types::{VarLenAscii, VarLenUnicode},
    H5Type,
};
use std::path::Path;
#[derive(H5Type, Clone, Copy)]
#[repr(C)]
struct DepthValue {
    depth: f32,
    uncertainty: f32,
}
#[derive(H5Type, Clone, Copy)]
#[repr(C)]
struct DepthOnly {
    depth: f32,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UncertaintyEncoding {
    PerCell,
    GroupConstant,
}
#[cfg(test)]
#[allow(non_snake_case)]
#[derive(H5Type, Clone)]
#[repr(C)]
struct Definition {
    code: VarLenAscii,
    name: VarLenAscii,
    #[hdf5(rename = "uom.name")]
    unit: VarLenAscii,
    fillValue: VarLenAscii,
    datatype: VarLenAscii,
    lower: VarLenAscii,
    upper: VarLenAscii,
    closure: VarLenAscii,
}
#[cfg(test)]
impl Definition {
    fn for_code(code: &str) -> Self {
        let text = |v: &str| VarLenAscii::from_ascii(v).unwrap();
        let (name, unit, fill, datatype, lower, upper, closure) = match code {
            "uncertainty" => (
                code,
                "metres",
                "1000000",
                "H5T_FLOAT",
                "0",
                "",
                "geSemiInterval",
            ),
            "iD" => ("ID", "", "0", "H5T_INTEGER", "1", "", "geSemiInterval"),
            _ => (
                code,
                "metres",
                "1000000",
                "H5T_FLOAT",
                "-14",
                "11050",
                "closedInterval",
            ),
        };
        Self {
            code: text(code),
            name: text(name),
            unit: text(unit),
            fillValue: text(fill),
            datatype: text(datatype),
            lower: text(lower),
            upper: text(upper),
            closure: text(closure),
        }
    }
}
#[cfg(test)]
fn write_test_feature_declarations(g: &hdf5::Group) {
    g.new_dataset::<VarLenAscii>()
        .shape(2)
        .create("featureCode")
        .unwrap()
        .write_raw(
            &["BathymetryCoverage", "QualityOfBathymetryCoverage"]
                .map(|s| VarLenAscii::from_ascii(s).unwrap()),
        )
        .unwrap();
    g.new_dataset::<Definition>()
        .shape(1)
        .create("QualityOfBathymetryCoverage")
        .unwrap()
        .write_raw(&[Definition::for_code("iD")])
        .unwrap();
}
#[derive(Debug)]
pub struct BathymetryCoverage {
    pub feature_metadata: std::sync::Arc<FeatureMetadata>,
    pub issue: IssueMetadata,
    pub axes: AxisMetadata,
    pub root_bounds: RootBounds,
    pub root_enclosure: RootEnclosure,
    pub quality: Option<std::sync::Arc<QualityCoverage>>,
    geometry: GridGeometry,
    pub domain: InstanceDomain,
    values: hdf5::Dataset,
    range_violations: std::sync::atomic::AtomicU8,
    /// Container coordinate uncertainty (metres); -1 means unknown/inapplicable.
    /// Kept separately from the depth values group's cell/constant uncertainty.
    pub horizontal_position_uncertainty: f32,
    pub vertical_position_uncertainty: f32,
    pub uncertainty_encoding: UncertaintyEncoding,
    pub declared_min_uncertainty: f32,
    pub declared_max_uncertainty: f32,
    pub time_point: String,
    pub instance_name: String,
    pub product_specification: String,
    pub vertical_crs: u32,
    pub vertical_datum: u32,
    pub vertical_datum_reference: u8,
    pub depth_fill: f32,
    pub uncertainty_fill: f32,
    pub declared_min_depth: f32,
    pub declared_max_depth: f32,
}
fn checked_metadata_text(bytes: &[u8], ascii: bool, name: &str) -> Result<String> {
    let limit = match name {
        "issueDate" => 8,
        "issueTime" => 64,
        _ => 4096,
    };
    // Inspect borrowed HDF5 bytes before allocating an owned Rust payload.
    // This does not bound HDF5's own variable-string allocation.
    ensure!(
        bytes.len() <= limit,
        "S102 {name} string exceeds supported{limit} bytes"
    );
    ensure!(
        !ascii || bytes.is_ascii(),
        "Invalid S102 {name} declaredASCII bytes"
    );
    Ok(std::str::from_utf8(bytes)
        .with_context(|| format!("Invalid S102 {name} UTF8"))?
        .to_owned())
}
fn string_attr(g: &hdf5::Group, name: &str) -> Result<String> {
    use hdf5::types::{FixedAscii, FixedUnicode, TypeDescriptor};
    let a = g.attr(name)?;
    ensure!(a.is_scalar(), "S102 {name} must be scalar");
    match a.dtype()?.to_descriptor()? {
        TypeDescriptor::VarLenUnicode => {
            let v = a.read_scalar::<VarLenUnicode>()?;
            checked_metadata_text(v.as_bytes(), false, name)
        }
        TypeDescriptor::VarLenAscii => {
            let v = a.read_scalar::<VarLenAscii>()?;
            checked_metadata_text(v.as_bytes(), true, name)
        }
        TypeDescriptor::FixedUnicode(n) => {
            ensure!(
                n <= 4096,
                "Fixed metadata string exceeds supported4096 bytes"
            );
            let v = a.read_scalar::<FixedUnicode<4096>>()?;
            checked_metadata_text(v.as_bytes(), false, name)
        }
        TypeDescriptor::FixedAscii(n) => {
            ensure!(
                n <= 4096,
                "Fixed metadata string exceeds supported4096 bytes"
            );
            let v = a.read_scalar::<FixedAscii<4096>>()?;
            checked_metadata_text(v.as_bytes(), true, name)
        }
        _ => anyhow::bail!("S102 {name} must be a string attribute"),
    }
}
// S-1023.0 Table10-4 and Table10-6: these are encoded metadata contracts,
// independent of GPU sampling or any application portrayal settings.
// Product storage contract; kernel GridGeometry remains orientation-neutral.
// S1023.0 §4.2/4.4 and Table5-1 override generic signed/permuted examples.
fn canonical_axes(crs: u32) -> Result<[&'static str; 2]> {
    match crs {
        4326 => Ok(["Longitude", "Latitude"]),
        32601..=32660 | 32701..=32760 | 5041 | 5042 => Ok(["Easting", "Northing"]),
        _ => anyhow::bail!("Unsupported S-102 horizontalCRS {crs}"),
    }
}
fn validate_storage_geometry(g: &GridGeometry) -> Result<()> {
    g.validate()?;
    ensure!(
        g.spacing_x > 0. && g.spacing_y > 0.,
        "S-102 canonical storage requires southwest origin and positive east/north spacing"
    );
    Ok(())
}
fn validate_container(container: &hdf5::Group) -> Result<u8> {
    for (name, expected) in [
        ("dimension", 2u8),
        ("commonPointRule", 2),
        ("interpolationType", 1),
    ] {
        ensure!(
            (if name == "dimension" {
                crate::scalar::unsigned_u8(container, name)?
            } else {
                crate::scalar::u8(container, name)?
            }) == expected,
            "S-102 requires {name}={expected}"
        );
    }
    let count = crate::scalar::unsigned_u8(container, "numInstances")?;
    ensure!(count > 0, "S-102 numInstances must be positive");
    Ok(count)
}
// S1023.0 Table10-4 rows4/5: mandatory scalar float32 container metadata.
// S100 Part10c describes -1 as unknown/inapplicable, or a measured uncertainty
// in metres. Zero is retained; the example wording is not a strict-positive gate.
fn position_uncertainties(container: &hdf5::Group) -> Result<[f32; 2]> {
    use hdf5::types::{FloatSize, TypeDescriptor};
    let mut values = [0.; 2];
    for (i, name) in ["horizontalPositionUncertainty", "verticalUncertainty"]
        .into_iter()
        .enumerate()
    {
        let a = container
            .attr(name)
            .with_context(|| format!("Missing S-102 {name}"))?;
        ensure!(
            a.is_scalar() && a.dtype()?.to_descriptor()? == TypeDescriptor::Float(FloatSize::U4),
            "S-102 {name} must be scalar float32"
        );
        let v = a.read_scalar::<f32>()?;
        ensure!(
            v.is_finite() && (v == -1. || v >= 0.),
            "Invalid S-102 {name}: expected -1 unknown or nonnegative metres"
        );
        values[i] = v;
    }
    Ok(values)
}
fn validate_datum(datum: u32) -> Result<()> {
    ensure!(
        (1..=30).contains(&datum) || datum == 44,
        "Unsupported S-102 verticalDatum code {datum}"
    );
    Ok(())
}
fn validate_vertical_root(file: &hdf5::Group) -> Result<()> {
    ensure!(
        crate::scalar::u32(file, "verticalCS")? == 6498,
        "S-102 requires verticalCS=6498 (depth metres down)"
    );
    ensure!(
        crate::scalar::u8(file, "verticalCoordinateBase")? == 2,
        "S-102 requires verticalCoordinateBase=2 (verticalDatum)"
    );
    Ok(())
}
// Exact decoded-dyadic cell-boundary comparison, with explicit admission for
// coordinates too coarse to represent distinct grid cells. No tolerance snaps
// different grids together. Full grid rectangle only, not domainExtent masks.
#[derive(Clone, Copy)]
struct GridBoundary {
    terms: [f64; 3],
    rounded: f64,
}
fn boundary(origin: f64, spacing: f64, coefficient: f64) -> Result<GridBoundary> {
    let product = spacing * coefficient;
    let residual = spacing.mul_add(coefficient, -product);
    let rounded = (origin + product) + residual;
    ensure!(
        [product, residual, rounded].iter().all(|v| v.is_finite()),
        "S-102 grid extent overflow"
    );
    Ok(GridBoundary {
        terms: [origin, product, residual],
        rounded,
    })
}
fn compare_boundary(a: GridBoundary, b: GridBoundary) -> Result<std::cmp::Ordering> {
    // Six finite inputs produce at most six expansion terms. Retain the original
    // grow-expansion operation order and overflow rejection, with no heap storage.
    let mut expansion = [0.; 6];
    let mut length = 0;
    for mut q in a.terms.into_iter().chain(b.terms.map(|v| -v)) {
        let mut next = [0.; 6];
        let mut next_length = 0;
        for &e in &expansion[..length] {
            let sum = q + e;
            ensure!(
                sum.is_finite(),
                "S-102 extent difference precision overflow"
            );
            let virtual_e = sum - q;
            let error = (q - (sum - virtual_e)) + (e - virtual_e);
            ensure!(
                virtual_e.is_finite() && error.is_finite(),
                "S-102 extent expansion overflow"
            );
            if error != 0. {
                next[next_length] = error;
                next_length += 1;
            }
            q = sum;
        }
        if q != 0. {
            next[next_length] = q;
            next_length += 1;
        }
        expansion = next;
        length = next_length;
    }
    Ok(if length == 0 {
        std::cmp::Ordering::Equal
    } else {
        expansion[length - 1].partial_cmp(&0.).unwrap()
    })
}
fn same_boundary(a: GridBoundary, b: GridBoundary) -> Result<bool> {
    Ok(compare_boundary(a, b)? == std::cmp::Ordering::Equal)
}
fn grid_extent(g: &GridGeometry) -> Result<[GridBoundary; 4]> {
    g.validate()?;
    ensure!(
        g.width <= u32::MAX as usize && g.height <= u32::MAX as usize,
        "S-102 grid dimensions exceed encoded32bit limits"
    );
    let last = g
        .position(g.width - 1, g.height - 1)
        .context("Missing grid endpoint")?;
    for (origin, end, spacing) in [
        (g.origin_x, last.0, g.spacing_x),
        (g.origin_y, last.1, g.spacing_y),
    ] {
        ensure!(
            end.is_finite() && origin + spacing != origin && end + spacing != end,
            "S-102 cell spacing is below coordinate precision"
        );
        let half = spacing * 0.5;
        ensure!(
            half != 0. && half * 2. == spacing,
            "S-102 cell boundary below floating precision"
        );
    }
    let xs = [
        boundary(g.origin_x, g.spacing_x, -0.5)?,
        boundary(g.origin_x, g.spacing_x, g.width as f64 - 0.5)?,
    ];
    let ys = [
        boundary(g.origin_y, g.spacing_y, -0.5)?,
        boundary(g.origin_y, g.spacing_y, g.height as f64 - 0.5)?,
    ];
    let x = if g.spacing_x > 0. { xs } else { [xs[1], xs[0]] };
    let y = if g.spacing_y > 0. { ys } else { [ys[1], ys[0]] };
    ensure!(
        x[0].rounded < x[1].rounded && y[0].rounded < y[1].rounded,
        "S-102 cell-grid extent collapsed at coordinate precision"
    );
    Ok([x[0], x[1], y[0], y[1]])
}
fn validate_instance_domains(items: &[(u32, GridGeometry)]) -> Result<()> {
    let mut datums = std::collections::BTreeSet::new();
    let mut original: Option<[GridBoundary; 4]> = None;
    for (datum, geometry) in items {
        ensure!(
            datums.insert(*datum),
            "S-102 feature instances must use unique verticalDatum values"
        );
        let extent = grid_extent(geometry)?;
        if let Some(first) = original {
            for k in 0..4 {
                ensure!(same_boundary(extent[k],first[k])?,"S-102 feature instances must share exact encoded cell-grid location and extent");
            }
        } else {
            original = Some(extent);
        }
    }
    Ok(())
}
fn instance_vertical_datum(group: &hdf5::Group, root_datum: u32) -> Result<u32> {
    let attributes = group.attr_names()?;
    let datum = if attributes.iter().any(|n| n == "verticalDatum") {
        let datum = crate::scalar::unsigned_u32(group, "verticalDatum")?;
        validate_datum(datum)?;
        ensure!(
            datum != root_datum,
            "S-102 default-datum instance must not override verticalDatum"
        );
        datum
    } else {
        root_datum
    };
    if attributes.iter().any(|n| n == "verticalDatumReference") {
        ensure!(
            datum != root_datum,
            "S-102 default-datum instance must not override verticalDatumReference"
        );
        ensure!(
            crate::scalar::u8(group, "verticalDatumReference")? == 1,
            "S-102 instance verticalDatumReference must be1"
        );
    }
    Ok(datum)
}
// S1023.0 clauses10.2.6/10.2.7: all five group attributes are required,
// and a missing uncertainty field is legal only for a group constant.
fn validate_range(min: f32, max: f32, fill: f32, uncertainty: bool) -> Result<()> {
    ensure!(
        min.is_finite() && max.is_finite() && min <= max,
        "Invalid declared values extrema"
    );
    ensure!(
        (min == fill) == (max == fill),
        "Mixed fill and real declared extrema"
    );
    ensure!(
        !uncertainty || min == fill || min >= 0.,
        "Negative uncertainty extrema"
    );
    Ok(())
}
fn validate_time_point(value: &str) -> Result<()> {
    ensure!(value.is_ascii(), "Invalid HDF5 timePoint encoding");
    let (date, time) = value
        .split_once('T')
        .context("timePoint requires DateTime")?;
    ensure!(
        date.len() == 8
            && date.bytes().all(|c| c.is_ascii_digit())
            && time.len() >= 6
            && time.as_bytes()[..6].iter().all(|c| c.is_ascii_digit())
            && !time.contains(':'),
        "HDF5 timePoint must use ISO8601 basic encoding"
    );
    // The general DateTime type permits unzoned local time. Adding Z only checks
    // its calendar/clock validity; the original value and unknown offset are retained.
    ferrite_kernel::parse_viewing_instant(value)
        .or_else(|_| ferrite_kernel::parse_viewing_instant(&format!("{value}Z")))?;
    Ok(())
}
/// Geographic location is derived separately from the unchanged source node.
#[derive(Debug, Clone, Copy)]
pub struct GeographicNodePosition {
    pub column: usize,
    pub row: usize,
    pub horizontal_crs: u32,
    pub native_x: f64,
    pub native_y: f64,
    pub geographic: ferrite_kernel::geodesy::GeographicPosition,
    pub longitude_defined: bool,
    pub within_crs_area: bool,
}
#[derive(Debug, Clone, Copy)]
pub struct NativeGeographicQueryPosition {
    pub geographic: ferrite_kernel::geodesy::GeographicPosition,
    pub horizontal_crs: u32,
    pub native_x: f64,
    pub native_y: f64,
    pub within_crs_area: bool,
}
impl BathymetryCoverage {
    /// S102 axes are validated as Longitude/Latitude or Easting/Northing.
    /// No original node index, value, domain or root metadata is rewritten.
    pub fn geographic_node_position(
        &self,
        column: usize,
        row: usize,
    ) -> Result<GeographicNodePosition> {
        use ferrite_kernel::{
            geodesy::GeographicPosition,
            projection::{NativeProjectedPosition, Wgs84ProjectedCrs},
        };
        let (x, y) = self
            .geometry
            .position(column, row)
            .context("S102 node outside source grid")?;
        let (geographic, longitude_defined, within_crs_area) =
            if self.geometry.horizontal_crs == 4326 {
                let p = GeographicPosition::new(y, x)?;
                (p, p.latitude().abs() != 90., true)
            } else {
                let p = NativeProjectedPosition::new(
                    Wgs84ProjectedCrs::from_epsg(self.geometry.horizontal_crs)?,
                    x,
                    y,
                )?
                .to_geographic()?;
                (p.position, p.longitude_defined, p.within_crs_area)
            };
        Ok(GeographicNodePosition {
            column,
            row,
            horizontal_crs: self.geometry.horizontal_crs,
            native_x: x,
            native_y: y,
            geographic,
            longitude_defined,
            within_crs_area,
        })
    }
    /// Transform a query once before the existing native-domain/closed-cell
    /// composition selection. This method performs no sample lookup or tie rule.
    pub fn native_position_for_geographic(
        &self,
        geographic: ferrite_kernel::geodesy::GeographicPosition,
    ) -> Result<NativeGeographicQueryPosition> {
        let (native_x, native_y, within_crs_area) = if self.geometry.horizontal_crs == 4326 {
            (geographic.longitude(), geographic.latitude(), true)
        } else {
            let p = ferrite_kernel::projection::Wgs84ProjectedCrs::from_epsg(
                self.geometry.horizontal_crs,
            )?
            .project(geographic)?;
            (
                p.position.easting(),
                p.position.northing(),
                p.within_crs_area,
            )
        };
        Ok(NativeGeographicQueryPosition {
            geographic,
            horizontal_crs: self.geometry.horizontal_crs,
            native_x,
            native_y,
            within_crs_area,
        })
    }

    /// Issues observed in requested windows: depth/uncertainty outside declared
    /// extrema. This is a producer metadata diagnostic, not a changed sample.
    pub fn observed_range_violations(&self) -> [bool; 2] {
        let flags = self
            .range_violations
            .load(std::sync::atomic::Ordering::Relaxed);
        [flags & 1 != 0, flags & 2 != 0]
    }
    /// Whether a successfully decoded requested window contained a populated
    /// depth whose ORIGINAL sample position lies outside its validity domain.
    /// A centroid diagnostic, not a claim about the entire intersecting cell.
    /// Samples are retained; repeated reads do not increase a misleading count.
    pub fn observed_depth_centroids_outside_domain(&self) -> bool {
        self.range_violations
            .load(std::sync::atomic::Ordering::Relaxed)
            & 4
            != 0
    }
    fn validated_sample(
        &self,
        depth: f32,
        uncertainty: f32,
        issues: &mut u8,
    ) -> Result<CoverageSample> {
        let mut sample = |v: f32, fill: f32, min: f32, max: f32, bit: u8| -> Result<Option<f32>> {
            if v == fill {
                return Ok(None);
            }
            ensure!(
                v.is_finite() && (bit != 2 || v >= 0.),
                "S102 invalid nonfinite value or negative uncertainty"
            );
            // Encoded extrema are metadata, not a license to clamp or discard
            // populated values. Preserve raw samples and diagnose inconsistency.
            if min == fill || v < min || v > max {
                *issues |= bit;
            }
            Ok(Some(v))
        };
        Ok(CoverageSample {
            value: sample(
                depth,
                self.depth_fill,
                self.declared_min_depth,
                self.declared_max_depth,
                1,
            )?,
            uncertainty: sample(
                uncertainty,
                self.uncertainty_fill,
                self.declared_min_uncertainty,
                self.declared_max_uncertainty,
                2,
            )?,
        })
    }
    /// Read only structure and metadata. Values stay in HDF5 until a window is requested.
    pub fn open(path: impl AsRef<Path>) -> Result<Vec<Self>> {
        let file = hdf5::File::open(path)?;
        domain::preflight_work(&file)?;
        let root_bounds = RootBounds::read(&file)?;
        let issue = IssueMetadata::read(&file)?;
        let spec = string_attr(&file, "productSpecification")?;
        ensure!(
            spec == "INT.IHO.S-102.3.0.0",
            "Unsupported S-102 product edition: {spec}"
        );
        let crs = crate::scalar::u32(&file, "horizontalCRS")?;
        validate_vertical_root(&file)?;
        let vertical_crs = crate::scalar::u32(&file, "verticalCS")?;
        let vertical_datum = crate::scalar::unsigned_u32(&file, "verticalDatum")?;
        validate_datum(vertical_datum)?;
        let vertical_datum_reference = crate::scalar::u8(&file, "verticalDatumReference")?;
        ensure!(
            vertical_datum_reference == 1,
            "S-102 verticalDatumReference must be1"
        );
        let container = file.group("BathymetryCoverage")?;
        let declared_instances = validate_container(&container)?;
        let [horizontal_position_uncertainty, vertical_position_uncertainty] =
            position_uncertainties(&container)?;
        ensure!(
            crate::scalar::u8(&container, "dataCodingFormat")? == 2,
            "S-102 coverage is not a regular grid"
        );
        ensure!(
            crate::scalar::u8(&container, "sequencingRule.type")? == 1,
            "Unsupported grid sequencing rule"
        );
        let axes = string_attr(&container, "sequencingRule.scanDirection")?;
        ensure!(
            axes.split(',').map(str::trim).eq(canonical_axes(crs)?),
            "Unsupported scan direction: {axes}"
        );
        ensure!(
            crate::scalar::u8(&container, "dataOffsetCode")? == 5,
            "S-102 requires cell-centre dataOffsetCode=5"
        );
        let axes = AxisMetadata::read(&container, crs)?;
        let feature_metadata = std::sync::Arc::new(FeatureMetadata::read(&file)?);
        let (depth_fill, uncertainty_fill) = feature_metadata.fills();
        let mut coverages = Vec::new();
        let mut names = container.member_names()?;
        names.sort();
        for name in names
            .into_iter()
            .filter(|n| n.starts_with("BathymetryCoverage."))
        {
            let g = container.group(&name)?;
            let instance_datum = instance_vertical_datum(&g, vertical_datum)?;
            ensure!(
                crate::scalar::unsigned_u32(&g, "numGRP")? == 1,
                "Multiple bathymetry value groups are not allowed in S-102"
            );
            let start = string_attr(&g, "startSequence")?;
            ensure!(
                start.split(',').all(|s| s.trim() == "0") && start.split(',').count() == 2,
                "Unsupported startSequence: {start}"
            );
            let geometry = GridGeometry {
                width: crate::scalar::unsigned_u32(&g, "numPointsLongitudinal")? as usize,
                height: crate::scalar::unsigned_u32(&g, "numPointsLatitudinal")? as usize,
                origin_x: crate::scalar::f64(&g, "gridOriginLongitude")?,
                origin_y: crate::scalar::f64(&g, "gridOriginLatitude")?,
                spacing_x: crate::scalar::f64(&g, "gridSpacingLongitudinal")?,
                spacing_y: crate::scalar::f64(&g, "gridSpacingLatitudinal")?,
                horizontal_crs: crs,
            };
            validate_storage_geometry(&geometry)?;
            let group = g.group("Group_001")?;
            let declared_min_depth = crate::scalar::f32(&group, "minimumDepth")?;
            let declared_max_depth = crate::scalar::f32(&group, "maximumDepth")?;
            validate_range(declared_min_depth, declared_max_depth, depth_fill, false)?;
            let declared_min_uncertainty = crate::scalar::f32(&group, "minimumUncertainty")?;
            let declared_max_uncertainty = crate::scalar::f32(&group, "maximumUncertainty")?;
            validate_range(
                declared_min_uncertainty,
                declared_max_uncertainty,
                1e6,
                true,
            )?;
            let time_point = string_attr(&group, "timePoint")?;
            validate_time_point(&time_point)?;
            let values = g.dataset("Group_001/values")?;
            let desc = values.dtype()?.to_descriptor()?;
            let hdf5::types::TypeDescriptor::Compound(fields) = desc else {
                anyhow::bail!("S102 values must be compound");
            };
            ensure!(
                fields
                    .fields
                    .iter()
                    .all(|p| matches!(p.name.as_str(), "depth" | "uncertainty")),
                "S102 values require only depth and optional uncertainty members"
            );
            let is_f32 = |name: &str| {
                fields.fields.iter().any(|p| {
                    p.name == name
                        && p.ty == hdf5::types::TypeDescriptor::Float(hdf5::types::FloatSize::U4)
                })
            };
            ensure!(is_f32("depth"), "S102 depth compound field must be float32");
            let has_uncertainty = fields.fields.iter().any(|p| p.name == "uncertainty");
            ensure!(
                has_uncertainty == uncertainty_fill.is_some(),
                "Group_F and values disagree about uncertainty presence"
            );
            let uncertainty_encoding = if has_uncertainty {
                ensure!(
                    is_f32("uncertainty"),
                    "S102 uncertainty compound field must be float32"
                );
                UncertaintyEncoding::PerCell
            } else {
                ensure!(
                    declared_min_uncertainty == declared_max_uncertainty,
                    "Omitted cell uncertainty requires equal group extrema"
                );
                UncertaintyEncoding::GroupConstant
            };

            ensure!(
                values.shape() == [geometry.height, geometry.width],
                "S-102 values shape differs from grid dimensions"
            );
            let domain = InstanceDomain::read(&g, &geometry)?;
            let root_enclosure = root_bounds.assess(&geometry, &domain, &g)?;
            coverages.push(Self {
                feature_metadata: feature_metadata.clone(),
                issue: issue.clone(),
                axes: axes.clone(),
                root_bounds,
                root_enclosure,
                quality: None,
                domain,
                geometry,
                values,
                range_violations: std::sync::atomic::AtomicU8::new(0),
                instance_name: name,
                product_specification: spec.clone(),
                vertical_crs,
                vertical_datum: instance_datum,
                vertical_datum_reference,
                depth_fill,
                uncertainty_fill: uncertainty_fill.unwrap_or(1e6),
                horizontal_position_uncertainty,
                vertical_position_uncertainty,
                uncertainty_encoding,
                declared_min_uncertainty,
                declared_max_uncertainty,
                time_point,
                declared_min_depth,
                declared_max_depth,
            });
        }
        ensure!(
            !coverages.is_empty(),
            "No BathymetryCoverage instances found"
        );
        ensure!(
            coverages.len() == usize::from(declared_instances),
            "S-102 numInstances differs from actual feature instance groups"
        );
        validate_instance_domains(
            &coverages
                .iter()
                .map(|c| (c.vertical_datum, c.geometry))
                .collect::<Vec<_>>(),
        )?;
        let quality = QualityCoverage::open_optional(
            &file,
            &coverages.iter().map(|c| c.geometry).collect::<Vec<_>>(),
        )?;
        for coverage in &mut coverages {
            coverage.quality = quality.clone();
        }
        Ok(coverages)
    }
}
impl CoverageSource for BathymetryCoverage {
    fn geometry(&self) -> &GridGeometry {
        &self.geometry
    }
    fn is_valid_position(&self, x: f64, y: f64) -> bool {
        self.domain.contains(x, y)
    }
    fn requires_geometric_mask(&self) -> bool {
        self.domain.requires_mask()
    }
    fn read_window(&self, window: GridWindow) -> Result<CoverageTile> {
        window.validate(&self.geometry)?;
        let bounds = (
            window.row..window.row + window.height,
            window.column..window.column + window.width,
        );
        let mut issues = 0u8;
        let samples: Vec<CoverageSample> = match self.uncertainty_encoding {
            UncertaintyEncoding::PerCell => self
                .values
                .read_slice_2d::<DepthValue, _>(bounds)?
                .into_iter()
                .map(|v| self.validated_sample(v.depth, v.uncertainty, &mut issues))
                .collect::<Result<Vec<_>>>()?,
            UncertaintyEncoding::GroupConstant => self
                .values
                .read_slice_2d::<DepthOnly, _>(bounds)?
                .into_iter()
                .map(|v| self.validated_sample(v.depth, self.declared_min_uncertainty, &mut issues))
                .collect::<Result<Vec<_>>>()?,
        };
        if self.domain.requires_mask()
            && samples.iter().enumerate().any(|(index, s)| {
                if s.value.is_none() {
                    return false;
                }
                let (x, y) = self
                    .geometry
                    .position(
                        window.column + index % window.width,
                        window.row + index / window.width,
                    )
                    .expect("Validated window position");
                !self.domain.contains(x, y)
            })
        {
            issues |= 4;
        }
        if issues != 0 {
            self.range_violations
                .fetch_or(issues, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(CoverageTile { window, samples })
    }
}

mod domain;
pub use domain::InstanceDomain;
mod root_bounds;
pub use root_bounds::{Enclosure, RootBounds, RootEnclosure};
mod composition;
pub use composition::*;

/// Original-candidate packet for qualified continuous domain rendering.
pub mod continuous;

mod portrayal;
pub use portrayal::*;

#[cfg(test)]
mod tests {
    use super::*;
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
    #[test]
    fn window_orientation_and_missing_uncertainty() {
        let path =
            std::env::temp_dir().join(format!("ferrite-s102-window-{}.h5", std::process::id()));
        {
            let f = hdf5::File::create(&path).unwrap();
            text(&f, "productSpecification", "INT.IHO.S-102.3.0.0");
            text(&f, "issueDate", "20261006");
            for (n, v) in [
                ("westBoundLongitude", 0.5f32),
                ("eastBoundLongitude", 3.5),
                ("southBoundLatitude", 1.5),
                ("northBoundLatitude", 3.5),
            ] {
                attr(&f, n, v);
            }
            attr(&f, "horizontalCRS", 4326u32);
            attr(&f, "verticalCS", 6498u32);
            attr(&f, "verticalCoordinateBase", 2u8);
            attr(&f, "verticalDatum", 10u32);
            attr(&f, "verticalDatumReference", 1u8);
            let b = f.create_group("BathymetryCoverage").unwrap();
            write_test_axes(&b, 4326);
            attr(&b, "horizontalPositionUncertainty", -1f32);
            attr(&b, "verticalUncertainty", -1f32);
            attr(&b, "dataCodingFormat", 2u8);
            attr(&b, "dimension", 2u8);
            attr(&b, "commonPointRule", 2u8);
            attr(&b, "interpolationType", 1u8);
            attr(&b, "numInstances", 1u8);
            attr(&b, "dataOffsetCode", 5u8);
            attr(&b, "sequencingRule.type", 1u8);
            text(&b, "sequencingRule.scanDirection", "Longitude, Latitude");
            let g = b.create_group("BathymetryCoverage.01").unwrap();
            attr(&g, "numGRP", 1u32);
            text(&g, "startSequence", "0,0");
            attr(&g, "numPointsLongitudinal", 3u32);
            attr(&g, "numPointsLatitudinal", 2u32);
            attr(&g, "gridOriginLongitude", 1f64);
            attr(&g, "gridOriginLatitude", 2f64);
            attr(&g, "gridSpacingLongitudinal", 1f64);
            attr(&g, "gridSpacingLatitudinal", 1f64);
            for (n, v) in [
                ("westBoundLongitude", 0.5f32),
                ("eastBoundLongitude", 3.5),
                ("southBoundLatitude", 1.5),
                ("northBoundLatitude", 3.5),
            ] {
                attr(&g, n, v);
            }
            let data = g.create_group("Group_001").unwrap();
            attr(&data, "minimumDepth", -3f32);
            attr(&data, "maximumDepth", 6f32);
            attr(&data, "minimumUncertainty", 0.2f32);
            attr(&data, "maximumUncertainty", 0.5f32);
            text(&data, "timePoint", "00010101T000000Z");
            data.new_dataset::<DepthValue>()
                .shape([2, 3])
                .create("values")
                .unwrap()
                .write_raw(&[
                    DepthValue {
                        depth: -3.,
                        uncertainty: 0.2,
                    },
                    DepthValue {
                        depth: 1e6,
                        uncertainty: 1e6,
                    },
                    DepthValue {
                        depth: 1.,
                        uncertainty: 0.3,
                    },
                    DepthValue {
                        depth: 3.,
                        uncertainty: 0.4,
                    },
                    DepthValue {
                        depth: 5.,
                        uncertainty: 1e6,
                    },
                    DepthValue {
                        depth: 6.,
                        uncertainty: 0.5,
                    },
                ])
                .unwrap();
            let info = f.create_group("Group_F").unwrap();
            write_test_feature_declarations(&info);
            let definitions = [
                Definition::for_code("depth"),
                Definition::for_code("uncertainty"),
            ];
            info.new_dataset::<Definition>()
                .shape(2)
                .create("BathymetryCoverage")
                .unwrap()
                .write_raw(&definitions)
                .unwrap();
        }
        {
            let coverages = BathymetryCoverage::open(&path).unwrap();
            let c = &coverages[0];
            assert_eq!(c.sample_nearest(1., 2.).unwrap().unwrap().value, Some(-3.));
            let tile = c
                .read_window(GridWindow {
                    column: 1,
                    row: 1,
                    width: 2,
                    height: 1,
                })
                .unwrap();
            assert_eq!(tile.samples[0].value, Some(5.));
            assert_eq!(tile.samples[0].uncertainty, None);
            assert_eq!(tile.samples[1].value, Some(6.));
            assert_eq!(c.sample_nearest(2., 2.).unwrap().unwrap().value, None);
            assert!(c.sample_nearest(100., 100.).unwrap().is_none());
            assert!(c
                .read_window(GridWindow {
                    column: 2,
                    row: 1,
                    width: 2,
                    height: 1
                })
                .is_err());
        }
        // Actual adapter-level inheritance/override, beyond the helper contract.
        {
            let f = hdf5::File::open_rw(&path).unwrap();
            let b = f.group("BathymetryCoverage").unwrap();
            b.attr("numInstances").unwrap().write_scalar(&2u8).unwrap();
            let a = b.group("BathymetryCoverage.01").unwrap();
            let g = b.create_group("BathymetryCoverage.02").unwrap();
            attr(&g, "verticalDatum", 23u16);
            attr(&g, "numGRP", 1u32);
            text(&g, "startSequence", "0,0");
            for n in ["numPointsLongitudinal", "numPointsLatitudinal"] {
                attr(&g, n, a.attr(n).unwrap().read_scalar::<u32>().unwrap());
            }
            for n in [
                "gridOriginLongitude",
                "gridOriginLatitude",
                "gridSpacingLongitudinal",
                "gridSpacingLatitudinal",
            ] {
                attr(&g, n, a.attr(n).unwrap().read_scalar::<f64>().unwrap());
            }
            for n in [
                "westBoundLongitude",
                "eastBoundLongitude",
                "southBoundLatitude",
                "northBoundLatitude",
            ] {
                attr(&g, n, a.attr(n).unwrap().read_scalar::<f32>().unwrap());
            }
            let data = g.create_group("Group_001").unwrap();
            attr(&data, "minimumDepth", -3f32);
            attr(&data, "maximumDepth", 6f32);
            attr(&data, "minimumUncertainty", 0.2f32);
            attr(&data, "maximumUncertainty", 0.5f32);
            text(&data, "timePoint", "00010101T000000Z");
            let values = a
                .dataset("Group_001/values")
                .unwrap()
                .read_raw::<DepthValue>()
                .unwrap();
            data.new_dataset::<DepthValue>()
                .shape([2, 3])
                .create("values")
                .unwrap()
                .write_raw(&values)
                .unwrap();
        }
        {
            let groups = BathymetryCoverage::open(&path).unwrap();
            assert_eq!(groups.len(), 2);
            assert_eq!(
                groups.iter().map(|c| c.vertical_datum).collect::<Vec<_>>(),
                [10, 23]
            );
            assert!(groups.iter().all(|c| c.vertical_datum_reference == 1));
            assert_eq!(
                groups[0].sample_nearest(1., 2.).unwrap().unwrap().value,
                groups[1].sample_nearest(1., 2.).unwrap().unwrap().value
            );
            use ferrite_kernel::depth_selection::{
                DepthAdjustment, DepthAdjustmentProvider, DepthReference,
            };
            struct Offset;
            impl DepthAdjustmentProvider for Offset {
                fn adjustment(
                    &self,
                    from: DepthReference,
                    to: DepthReference,
                    _: f64,
                    _: f64,
                ) -> Result<Option<DepthAdjustment>> {
                    assert_eq!(to, DepthReference(10));
                    Ok(Some(DepthAdjustment {
                        correction_metres: if from == to { 0. } else { -4. },
                        provenance: 23,
                    }))
                }
            }
            let provider = Offset;
            let composed = ConservativeCoverage::new(
                groups
                    .iter()
                    .map(|c| DatumCoverage {
                        coverage: c,
                        reference: DepthReference(c.vertical_datum as u64),
                    })
                    .collect(),
                DepthReference(10),
                &provider,
            )
            .unwrap();
            let winner = composed.query_nearest(1., 2.).unwrap().unwrap();
            assert_eq!(winner.candidate.source.instance, 1);
            assert_eq!(winner.adjusted_depth, -7.);
            assert_eq!(winner.candidate.raw_depth, Some(-3.));
            assert_eq!((winner.candidate.x, winner.candidate.y), (1., 2.));
        }
        {
            let f = hdf5::File::open_rw(&path).unwrap();
            f.group("BathymetryCoverage")
                .unwrap()
                .attr("numInstances")
                .unwrap()
                .write_scalar(&1u8)
                .unwrap();
        }
        assert!(BathymetryCoverage::open(&path)
            .unwrap_err()
            .to_string()
            .contains("numInstances"));
        std::fs::remove_file(path).unwrap();
    }
}

mod quality;
pub use quality::{QualityCoverage, QualityRecord};

impl BathymetryCoverage {
    /// Whole-feature plane assignment. Grid-valued attributes are not scalar feature selectors.
    /// The caller must apply the returned viewing group, not only its plane/priority.
    pub fn interoperability_assignment(
        &self,
        catalogue: &ferrite_interoperability::Catalogue,
    ) -> Result<Option<ferrite_interoperability::Assignment>> {
        catalogue.resolve("S-102", "BathymetryCoverage", "coverage", |path| {
            anyhow::bail!(
                "S-102 grid/feature attribute selector is unsupported: {}",
                path.join("/")
            )
        })
    }
}

#[cfg(test)]
mod metadata_contract_tests {
    use super::*;
    fn attr<T: H5Type>(g: &hdf5::Group, n: &str, v: T) {
        g.new_attr::<T>()
            .create(n)
            .unwrap()
            .write_scalar(&v)
            .unwrap();
    }
    #[test]
    fn file_metadata_selects_nearest_and_low_and_requires_declared_dimensions() {
        let path =
            std::env::temp_dir().join(format!("ferrite-s102-contract-{}.h5", std::process::id()));
        {
            let file = hdf5::File::create(&path).unwrap();
            let g = file.create_group("BathymetryCoverage").unwrap();
            attr(&g, "dimension", 2u8);
            attr(&g, "commonPointRule", 2u8);
            attr(&g, "numInstances", 2u8);
            assert!(validate_container(&g)
                .unwrap_err()
                .to_string()
                .contains("interpolationType"));
            attr(&g, "interpolationType", 1u8);
            assert_eq!(validate_container(&g).unwrap(), 2);
            for (name, bad) in [
                ("interpolationType", 2u8),
                ("commonPointRule", 1),
                ("dimension", 3),
                ("numInstances", 0),
            ] {
                let a = g.attr(name).unwrap();
                let old = a.read_scalar::<u8>().unwrap();
                a.write_scalar(&bad).unwrap();
                assert!(validate_container(&g).is_err());
                a.write_scalar(&old).unwrap();
            }
        }
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn instance_datum_inherits_or_overrides_without_mislabeling_depths() {
        let path = std::env::temp_dir().join(format!(
            "ferrite-s102-datum-contract-{}.h5",
            std::process::id()
        ));
        {
            let file = hdf5::File::create(&path).unwrap();
            let g = file.create_group("BathymetryCoverage.01").unwrap();
            assert_eq!(instance_vertical_datum(&g, 10).unwrap(), 10);
            attr(&g, "verticalDatum", 23u16);
            assert_eq!(instance_vertical_datum(&g, 10).unwrap(), 23);
            attr(&g, "verticalDatumReference", 1u8);
            assert_eq!(instance_vertical_datum(&g, 10).unwrap(), 23);
            g.attr("verticalDatumReference")
                .unwrap()
                .write_scalar(&2u8)
                .unwrap();
            assert!(instance_vertical_datum(&g, 10).is_err());
            g.attr("verticalDatumReference")
                .unwrap()
                .write_scalar(&1u8)
                .unwrap();
            g.attr("verticalDatum")
                .unwrap()
                .write_scalar(&10u16)
                .unwrap();
            assert!(instance_vertical_datum(&g, 10).is_err());
            g.attr("verticalDatum")
                .unwrap()
                .write_scalar(&31u16)
                .unwrap();
            assert!(instance_vertical_datum(&g, 10).is_err());
            let h = file.create_group("BathymetryCoverage.02").unwrap();
            attr(&h, "verticalDatumReference", 1u8);
            assert!(instance_vertical_datum(&h, 10).is_err());
        }
        std::fs::remove_file(path).unwrap();
    }
}

#[cfg(test)]
mod vertical_contract_tests {
    use super::*;
    #[test]
    fn required_vertical_axis_is_depth_metres_down_and_datum_based() {
        let path =
            std::env::temp_dir().join(format!("ferrite-s102-axis-{}.h5", std::process::id()));
        {
            let file = hdf5::File::create(&path).unwrap();
            file.new_attr::<u32>()
                .create("verticalCS")
                .unwrap()
                .write_scalar(&6498u32)
                .unwrap();
            assert!(validate_vertical_root(&file).is_err());
            file.new_attr::<u8>()
                .create("verticalCoordinateBase")
                .unwrap()
                .write_scalar(&2u8)
                .unwrap();
            assert!(validate_vertical_root(&file).is_ok());
            file.attr("verticalCS")
                .unwrap()
                .write_scalar(&6499u32)
                .unwrap();
            assert!(validate_vertical_root(&file).is_err());
            file.attr("verticalCS")
                .unwrap()
                .write_scalar(&6498u32)
                .unwrap();
            file.attr("verticalCoordinateBase")
                .unwrap()
                .write_scalar(&1u8)
                .unwrap();
            assert!(validate_vertical_root(&file).is_err());
        }
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn distinct_datums_may_change_resolution_or_reverse_axes_but_not_extent() {
        let a = GridGeometry {
            width: 3,
            height: 2,
            origin_x: 1.,
            origin_y: 2.,
            spacing_x: 1.,
            spacing_y: 1.,
            horizontal_crs: 4326,
        };
        let mut b = a;
        b.origin_x = 3.;
        b.origin_y = 3.;
        b.spacing_x = -1.;
        b.spacing_y = -1.;
        assert!(validate_instance_domains(&[(10, a), (23, b)]).is_ok());
        b = a;
        b.width = 6;
        b.origin_x = 0.75;
        b.spacing_x = 0.5;
        assert!(validate_instance_domains(&[(10, a), (23, b)]).is_ok());
        assert!(validate_instance_domains(&[(10, a), (10, b)]).is_err());
        b.origin_x += 0.000001;
        assert!(validate_instance_domains(&[(10, a), (23, b)]).is_err());
        b = a;
        b.origin_x = f64::MAX;
        b.spacing_x = f64::MAX;
        assert!(grid_extent(&b).is_err());
    }
    #[test]
    fn large_origin_tiny_spacing_and_collapsed_extent_are_not_snapped() {
        let mut g = GridGeometry {
            width: 1,
            height: 1,
            origin_x: 1e16,
            origin_y: 2.,
            spacing_x: 1.,
            spacing_y: 1.,
            horizontal_crs: 4326,
        };
        assert!(grid_extent(&g).is_err());
        g.origin_x += 2.;
        assert!(grid_extent(&g).is_err());
        g.origin_x = 0.;
        g.spacing_x = f64::from_bits(1);
        assert!(grid_extent(&g).is_err());
        let first = boundary(1e16, 1., -0.5).unwrap();
        let second = boundary(1e16 + 2., 1., -0.5).unwrap();
        assert!(!same_boundary(first, second).unwrap());
        // Same rounded f64 endpoint but distinct exact encoded boundary.
        let same_rounded = boundary(1e16, 1., 0.5).unwrap();
        assert_eq!(first.rounded, same_rounded.rounded);
        assert!(!same_boundary(first, same_rounded).unwrap());
        assert!(same_boundary(
            boundary(0.75, 0.5, -0.5).unwrap(),
            boundary(1., 1., -0.5).unwrap()
        )
        .unwrap());
        assert!(same_boundary(
            boundary(3., -1., -0.5).unwrap(),
            boundary(1., 1., 2.5).unwrap()
        )
        .unwrap());
    }
}

#[cfg(test)]
mod values_contract_tests {
    use super::*;
    use ferrite_kernel::NumericCoverageSource;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static SERIAL: AtomicUsize = AtomicUsize::new(0);
    struct Fixture(std::path::PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    fn attr<T: H5Type>(g: &hdf5::Group, n: &str, v: T) {
        g.new_attr::<T>()
            .create(n)
            .unwrap()
            .write_scalar(&v)
            .unwrap();
    }
    fn text(g: &hdf5::Group, n: &str, v: &str) {
        attr(g, n, VarLenAscii::from_ascii(v).unwrap());
    }
    // Independent encoded fixture: no production reader or composition used to create values.
    fn fixture(
        cell_uncertainty: bool,
        definition_uncertainty: bool,
        min: f32,
        max: f32,
    ) -> Fixture {
        let path = std::env::temp_dir().join(format!(
            "s102-values-{}-{}.h5",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        let f = hdf5::File::create(&path).unwrap();
        text(&f, "productSpecification", "INT.IHO.S-102.3.0.0");
        text(&f, "issueDate", "20261006");
        for (n, v) in [
            ("westBoundLongitude", 0.5f32),
            ("eastBoundLongitude", 3.5),
            ("southBoundLatitude", 1.5),
            ("northBoundLatitude", 3.5),
        ] {
            attr(&f, n, v);
        }
        attr(&f, "horizontalCRS", 4326u32);
        attr(&f, "verticalCS", 6498u32);
        attr(&f, "verticalCoordinateBase", 2u8);
        attr(&f, "verticalDatum", 10u32);
        attr(&f, "verticalDatumReference", 1u8);
        let b = f.create_group("BathymetryCoverage").unwrap();
        write_test_axes(&b, 4326);
        attr(&b, "horizontalPositionUncertainty", -1f32);
        attr(&b, "verticalUncertainty", -1f32);
        for (n, v) in [
            ("dataCodingFormat", 2u8),
            ("dimension", 2),
            ("commonPointRule", 2),
            ("interpolationType", 1),
            ("numInstances", 1),
            ("dataOffsetCode", 5),
            ("sequencingRule.type", 1),
        ] {
            attr(&b, n, v);
        }
        text(&b, "sequencingRule.scanDirection", "Longitude, Latitude");
        let g = b.create_group("BathymetryCoverage.01").unwrap();
        attr(&g, "numGRP", 1u32);
        text(&g, "startSequence", "0,0");
        attr(&g, "numPointsLongitudinal", 3u32);
        attr(&g, "numPointsLatitudinal", 2u32);
        for (n, v) in [
            ("gridOriginLongitude", 1f64),
            ("gridOriginLatitude", 2.),
            ("gridSpacingLongitudinal", 1.),
            ("gridSpacingLatitudinal", 1.),
        ] {
            attr(&g, n, v);
        }
        for (n, v) in [
            ("westBoundLongitude", 0.5f32),
            ("eastBoundLongitude", 3.5),
            ("southBoundLatitude", 1.5),
            ("northBoundLatitude", 3.5),
        ] {
            attr(&g, n, v);
        }
        let data = g.create_group("Group_001").unwrap();
        attr(&data, "minimumDepth", -3f32);
        attr(&data, "maximumDepth", 6f32);
        attr(&data, "minimumUncertainty", min);
        attr(&data, "maximumUncertainty", max);
        text(&data, "timePoint", "00010101T000000Z");
        let depths = [-3., 1e6, 1., 3., 5., 6.];
        if cell_uncertainty {
            let rows: Vec<_> = depths
                .into_iter()
                .map(|depth| DepthValue {
                    depth,
                    uncertainty: min,
                })
                .collect();
            data.new_dataset::<DepthValue>()
                .shape([2, 3])
                .create("values")
                .unwrap()
                .write_raw(&rows)
                .unwrap();
        } else {
            let rows: Vec<_> = depths
                .into_iter()
                .map(|depth| DepthOnly { depth })
                .collect();
            data.new_dataset::<DepthOnly>()
                .shape([2, 3])
                .create("values")
                .unwrap()
                .write_raw(&rows)
                .unwrap();
        }
        let info = f.create_group("Group_F").unwrap();
        write_test_feature_declarations(&info);
        let codes = if definition_uncertainty {
            vec!["depth", "uncertainty"]
        } else {
            vec!["depth"]
        };
        let rows: Vec<_> = codes
            .into_iter()
            .map(|code| Definition::for_code(code))
            .collect();
        info.new_dataset::<Definition>()
            .shape(rows.len())
            .create("BathymetryCoverage")
            .unwrap()
            .write_raw(&rows)
            .unwrap();
        Fixture(path)
    }
    #[derive(H5Type, Clone, Copy)]
    #[repr(C)]
    struct Vertex {
        longitude: f64,
        latitude: f64,
    }

    #[test]
    fn container_position_uncertainties_are_mandatory_and_do_not_replace_cell_values() {
        let f = fixture(true, true, 0.2, 0.5);
        {
            let file = hdf5::File::open_rw(&f.0).unwrap();
            let b = file.group("BathymetryCoverage").unwrap();
            b.attr("horizontalPositionUncertainty")
                .unwrap()
                .write_scalar(&2.25f32)
                .unwrap();
            b.attr("verticalUncertainty")
                .unwrap()
                .write_scalar(&0f32)
                .unwrap();
        }
        let c = BathymetryCoverage::open(&f.0).unwrap();
        assert_eq!(
            c[0].horizontal_position_uncertainty.to_bits(),
            2.25f32.to_bits()
        );
        assert_eq!(c[0].vertical_position_uncertainty.to_bits(), 0f32.to_bits());
        let t = c[0]
            .read_window(GridWindow {
                column: 0,
                row: 0,
                width: 1,
                height: 1,
            })
            .unwrap();
        assert_eq!(t.samples[0].value, Some(-3.));
        assert_eq!(t.samples[0].uncertainty, Some(0.2));
        drop(c);
        for name in ["horizontalPositionUncertainty", "verticalUncertainty"] {
            let f = fixture(true, true, 0.2, 0.5);
            {
                let file = hdf5::File::open_rw(&f.0).unwrap();
                file.group("BathymetryCoverage")
                    .unwrap()
                    .delete_attr(name)
                    .unwrap();
            }
            assert!(BathymetryCoverage::open(&f.0)
                .unwrap_err()
                .to_string()
                .contains(name));
        }
    }
    #[test]
    fn container_position_uncertainties_reject_wrong_shape_precision_and_invalid_values() {
        for name in ["horizontalPositionUncertainty", "verticalUncertainty"] {
            for kind in 0..3 {
                let f = fixture(true, true, 0.2, 0.5);
                {
                    let file = hdf5::File::open_rw(&f.0).unwrap();
                    let b = file.group("BathymetryCoverage").unwrap();
                    b.delete_attr(name).unwrap();
                    match kind {
                        0 => attr(&b, name, 1f64),
                        1 => attr(&b, name, 1u32),
                        _ => {
                            b.new_attr::<f32>()
                                .shape(1)
                                .create(name)
                                .unwrap()
                                .write_raw(&[1.])
                                .unwrap();
                        }
                    }
                }
                assert!(BathymetryCoverage::open(&f.0)
                    .unwrap_err()
                    .to_string()
                    .contains("scalar float32"));
            }
            for v in [
                f32::NAN,
                f32::INFINITY,
                f32::NEG_INFINITY,
                -2.,
                -f32::from_bits(1),
            ] {
                let f = fixture(true, true, 0.2, 0.5);
                {
                    let file = hdf5::File::open_rw(&f.0).unwrap();
                    file.group("BathymetryCoverage")
                        .unwrap()
                        .attr(name)
                        .unwrap()
                        .write_scalar(&v)
                        .unwrap();
                }
                assert!(BathymetryCoverage::open(&f.0).is_err());
            }
        }
        let f = fixture(false, false, 0.5, 0.5);
        let c = BathymetryCoverage::open(&f.0).unwrap();
        assert_eq!(c[0].horizontal_position_uncertainty, -1.);
        assert_eq!(c[0].vertical_position_uncertainty, -1.);
        assert_eq!(
            c[0].uncertainty_encoding,
            UncertaintyEncoding::GroupConstant
        );
    }
    #[test]
    fn axis_order_diagnostic_does_not_transpose_original_nodes_or_quality() {
        let f = fixture(true, true, 0.2, 0.5);
        {
            let file = hdf5::File::open_rw(&f.0).unwrap();
            file.group("BathymetryCoverage")
                .unwrap()
                .dataset("axisNames")
                .unwrap()
                .write_raw(&[
                    VarLenAscii::from_ascii("Longitude").unwrap(),
                    VarLenAscii::from_ascii("Latitude").unwrap(),
                ])
                .unwrap();
        }
        let c = BathymetryCoverage::open(&f.0).unwrap();
        assert_eq!(c[0].axes.order, AxisNamesOrder::ScanFastFirst);
        assert_eq!(c[0].issue.date, "20261006");
        let t = c[0]
            .read_window(GridWindow {
                column: 0,
                row: 0,
                width: 3,
                height: 2,
            })
            .unwrap();
        assert_eq!(
            t.samples.iter().map(|s| s.value).collect::<Vec<_>>(),
            [Some(-3.), None, Some(1.), Some(3.), Some(5.), Some(6.)]
        );
        assert_eq!(c[0].geometry.position(2, 1), Some((3., 3.)));
        drop(c);
        {
            let file = hdf5::File::open_rw(&f.0).unwrap();
            file.group("BathymetryCoverage")
                .unwrap()
                .unlink("axisNames")
                .unwrap();
        }
        assert!(BathymetryCoverage::open(&f.0)
            .unwrap_err()
            .to_string()
            .contains("axisNames"));
    }
    fn polygon_instance(path: &Path, points: Option<&[[f64; 2]]>) {
        let file = hdf5::File::open_rw(path).unwrap();
        let b = file.group("BathymetryCoverage").unwrap();
        b.relink("BathymetryCoverage.01", "Saved").unwrap();
        let old = b.group("Saved").unwrap();
        let g = b.create_group("BathymetryCoverage.01").unwrap();
        for n in ["numPointsLongitudinal", "numPointsLatitudinal", "numGRP"] {
            attr(&g, n, old.attr(n).unwrap().read_scalar::<u32>().unwrap());
        }
        for n in [
            "gridOriginLongitude",
            "gridOriginLatitude",
            "gridSpacingLongitudinal",
            "gridSpacingLatitudinal",
        ] {
            attr(&g, n, old.attr(n).unwrap().read_scalar::<f64>().unwrap());
        }
        text(&g, "startSequence", "0,0");
        old.relink(
            "Group_001",
            "/BathymetryCoverage/BathymetryCoverage.01/Group_001",
        )
        .unwrap();
        if let Some(points) = points {
            let vertices: Vec<_> = points
                .iter()
                .map(|p| Vertex {
                    longitude: p[0],
                    latitude: p[1],
                })
                .collect();
            g.new_dataset::<Vertex>()
                .shape(vertices.len())
                .create("domainExtent.polygon")
                .unwrap()
                .write_raw(&vertices)
                .unwrap();
        }
        b.unlink("Saved").unwrap();
    }
    #[test]
    fn polygon_queries_use_actual_position_preserve_raw_and_refuse_centroid_rendering() {
        let f = fixture(false, false, 0.5, 0.5);
        polygon_instance(
            &f.0,
            Some(&[[0.5, 1.5], [3.5, 1.5], [0.5, 3.5], [0.5, 1.5]]),
        );
        let items = BathymetryCoverage::open(&f.0).unwrap();
        let c = &items[0];
        assert!(c.requires_geometric_mask());
        assert!(!c.observed_depth_centroids_outside_domain());
        c.read_window(GridWindow {
            column: 0,
            row: 0,
            width: 1,
            height: 1,
        })
        .unwrap();
        assert!(!c.observed_depth_centroids_outside_domain());
        assert_eq!(
            c.read_window(GridWindow {
                column: 2,
                row: 0,
                width: 1,
                height: 1
            })
            .unwrap()
            .samples[0]
                .value,
            Some(1.)
        );
        assert!(c.observed_depth_centroids_outside_domain());
        assert_eq!(c.observed_range_violations(), [false, false]);
        assert!(c.sample_nearest(3., 2.).unwrap().is_none());
        assert_eq!(
            c.sample_nearest(2.75, 1.75).unwrap().unwrap().value,
            Some(1.)
        );
        assert_eq!(c.sample_nearest(2.75, 2.).unwrap().unwrap().value, Some(1.));
        assert!(c
            .sample_nearest(2.75, f64::from_bits(2f64.to_bits() + 1))
            .unwrap()
            .is_none());
        assert_eq!(
            c.sample_nearest(2.75, f64::from_bits(2f64.to_bits() - 1))
                .unwrap()
                .unwrap()
                .value,
            Some(1.)
        );
        use ferrite_kernel::depth_selection::{DepthReference, IdentityDepthAdjustment};
        let provider = IdentityDepthAdjustment;
        let mosaic = ConservativeCoverage::new(
            vec![DatumCoverage {
                coverage: c,
                reference: DepthReference(10),
            }],
            DepthReference(10),
            &provider,
        )
        .unwrap();
        assert!(mosaic.requires_spatial_mask());
        assert!(mosaic.query_nearest(3., 2.).unwrap().is_none());
        assert!(!mosaic.covers_position(3., 2.).unwrap());
        assert!(mosaic.covers_position(2.75, 2.).unwrap());
        assert!(!mosaic.covers_position(0.25, 1.75).unwrap());
        let selected = mosaic.query_nearest(2.75, 1.75).unwrap().unwrap();
        assert_eq!(selected.candidate.raw_depth, Some(1.));
        assert_eq!(
            (
                selected.candidate.source.column,
                selected.candidate.source.row
            ),
            (2, 0)
        );
    }
    #[test]
    fn root_footprint_cannot_shrink_to_a_tiny_domain_on_all_fill_grid() {
        let f = fixture(false, false, 0.5, 0.5);
        polygon_instance(
            &f.0,
            Some(&[
                [1.25, 1.75],
                [1.75, 1.75],
                [1.75, 2.25],
                [1.25, 2.25],
                [1.25, 1.75],
            ]),
        );
        {
            let file = hdf5::File::open_rw(&f.0).unwrap();
            for (name, v) in [
                ("westBoundLongitude", 1.25f32),
                ("eastBoundLongitude", 1.75),
                ("southBoundLatitude", 1.75),
                ("northBoundLatitude", 2.25),
            ] {
                file.attr(name).unwrap().write_scalar(&v).unwrap();
            }
            let g = file
                .group("BathymetryCoverage/BathymetryCoverage.01/Group_001")
                .unwrap();
            g.dataset("values")
                .unwrap()
                .write_raw(&[DepthOnly { depth: 1e6 }; 6])
                .unwrap();
            g.attr("minimumDepth")
                .unwrap()
                .write_scalar(&1e6f32)
                .unwrap();
            g.attr("maximumDepth")
                .unwrap()
                .write_scalar(&1e6f32)
                .unwrap();
        }
        let c = BathymetryCoverage::open(&f.0).unwrap();
        assert_eq!(
            c[0].root_enclosure,
            RootEnclosure {
                full_grid: Enclosure::TooSmall,
                declared_domain: Enclosure::Literal
            }
        );
        assert!(!c[0].root_enclosure.encoding_compatible());
        assert!(c[0]
            .read_window(GridWindow {
                column: 0,
                row: 0,
                width: 3,
                height: 2
            })
            .unwrap()
            .samples
            .iter()
            .all(|s| s.value.is_none()));
    }
    #[test]
    fn projected_and_wrapped_root_bounds_never_receive_unearned_enclosure() {
        let f = fixture(false, false, 0.5, 0.5);
        {
            let file = hdf5::File::open_rw(&f.0).unwrap();
            file.attr("horizontalCRS")
                .unwrap()
                .write_scalar(&32631u32)
                .unwrap();
            file.group("BathymetryCoverage")
                .unwrap()
                .attr("sequencingRule.scanDirection")
                .unwrap()
                .write_scalar(&VarLenAscii::from_ascii("Easting, Northing").unwrap())
                .unwrap();
            write_test_axes(
                &file.group("BathymetryCoverage").unwrap(),
                file.attr("horizontalCRS")
                    .unwrap()
                    .read_scalar::<u32>()
                    .unwrap(),
            );
        }
        let c = BathymetryCoverage::open(&f.0).unwrap();
        assert_eq!(
            c[0].root_enclosure.full_grid,
            Enclosure::UnverifiedProjected
        );
        let f = fixture(false, false, 0.5, 0.5);
        {
            let file = hdf5::File::open_rw(&f.0).unwrap();
            file.attr("westBoundLongitude")
                .unwrap()
                .write_scalar(&170f32)
                .unwrap();
            file.attr("eastBoundLongitude")
                .unwrap()
                .write_scalar(&-170f32)
                .unwrap();
        }
        let c = BathymetryCoverage::open(&f.0).unwrap();
        assert_eq!(&c[0].root_bounds.encoded[..2], &[170., -170.]);
        assert_eq!(c[0].root_enclosure.full_grid, Enclosure::PeriodicTooSmall);
        assert!(!c[0].root_enclosure.encoding_compatible());
    }
    #[test]
    fn periodic_metadata_retains_canonical_source_nodes_and_unwrapped_qualification() {
        for (origin, bounds, expected) in [
            (176., [175.5f32, 178.5], Enclosure::PeriodicLiteral),
            (-179., [-179.5, -176.5], Enclosure::PeriodicLiteral),
            (180., [179.5, 182.5], Enclosure::UnverifiedLongitudeSheet),
        ] {
            let f = fixture(false, false, 0.5, 0.5);
            {
                let file = hdf5::File::open_rw(&f.0).unwrap();
                file.attr("westBoundLongitude")
                    .unwrap()
                    .write_scalar(&170f32)
                    .unwrap();
                file.attr("eastBoundLongitude")
                    .unwrap()
                    .write_scalar(&-170f32)
                    .unwrap();
                let g = file
                    .group("BathymetryCoverage/BathymetryCoverage.01")
                    .unwrap();
                g.attr("gridOriginLongitude")
                    .unwrap()
                    .write_scalar(&origin)
                    .unwrap();
                g.attr("westBoundLongitude")
                    .unwrap()
                    .write_scalar(&bounds[0])
                    .unwrap();
                g.attr("eastBoundLongitude")
                    .unwrap()
                    .write_scalar(&bounds[1])
                    .unwrap();
            }
            let c = BathymetryCoverage::open(&f.0).unwrap();
            assert_eq!(c[0].root_enclosure.full_grid, expected);
            assert_eq!(c[0].root_enclosure.declared_domain, expected);
            assert_eq!(c[0].geometry.origin_x, origin);
            assert_eq!(c[0].geometry.position(2, 0).unwrap().0, origin + 2.);
            assert!(!c[0].root_enclosure.encoding_compatible());
            let samples = c[0]
                .read_window(GridWindow {
                    column: 0,
                    row: 0,
                    width: 3,
                    height: 2,
                })
                .unwrap()
                .samples;
            assert_eq!(
                samples.iter().map(|s| s.value).collect::<Vec<_>>(),
                vec![Some(-3.), None, Some(1.), Some(3.), Some(5.), Some(6.)]
            );
            let q = c[0]
                .query_nearest_wrapped(origin + 360., 2., true)
                .unwrap()
                .unwrap();
            assert_eq!(q.longitude_shift, 360.);
            assert_eq!(
                (q.query.column, q.query.row, q.query.x, q.query.y),
                (0, 0, origin, 2.)
            );
            assert_eq!(q.query.sample.value, Some(-3.));
        }
    }
    #[test]
    fn projected_node_and_query_coordinates_keep_original_hdf_source_and_root_status() {
        for (epsg, x, y, pole) in [
            (32631u32, 500_000., 0., false),
            (32731, 500_000., 10_000_000., false),
            (5041, 2_000_000., 2_000_000., true),
            (5042, 2_000_000., 2_000_000., true),
        ] {
            let f = fixture(false, false, 0.5, 0.5);
            {
                let file = hdf5::File::open_rw(&f.0).unwrap();
                file.attr("horizontalCRS")
                    .unwrap()
                    .write_scalar(&epsg)
                    .unwrap();
                // Global geographic root conservatively includes even a pole; native bbox stays metres.
                for (n, v) in [
                    ("westBoundLongitude", -180f32),
                    ("eastBoundLongitude", 180.),
                    ("southBoundLatitude", -90.),
                    ("northBoundLatitude", 90.),
                ] {
                    file.attr(n).unwrap().write_scalar(&v).unwrap();
                }
                file.group("BathymetryCoverage")
                    .unwrap()
                    .attr("sequencingRule.scanDirection")
                    .unwrap()
                    .write_scalar(&VarLenAscii::from_ascii("Easting, Northing").unwrap())
                    .unwrap();
                write_test_axes(
                    &file.group("BathymetryCoverage").unwrap(),
                    file.attr("horizontalCRS")
                        .unwrap()
                        .read_scalar::<u32>()
                        .unwrap(),
                );
                let g = file
                    .group("BathymetryCoverage/BathymetryCoverage.01")
                    .unwrap();
                g.attr("gridOriginLongitude")
                    .unwrap()
                    .write_scalar(&x)
                    .unwrap();
                g.attr("gridOriginLatitude")
                    .unwrap()
                    .write_scalar(&y)
                    .unwrap();
                for (n, v) in [
                    ("westBoundLongitude", (x - 0.5) as f32),
                    ("eastBoundLongitude", (x + 2.5) as f32),
                    ("southBoundLatitude", (y - 0.5) as f32),
                    ("northBoundLatitude", (y + 1.5) as f32),
                ] {
                    g.attr(n).unwrap().write_scalar(&v).unwrap();
                }
            }
            let c = BathymetryCoverage::open(&f.0).unwrap();
            let c = &c[0];
            let n = c.geographic_node_position(0, 0).unwrap();
            assert_eq!(
                (n.column, n.row, n.horizontal_crs, n.native_x, n.native_y),
                (0, 0, epsg, x, y)
            );
            assert_eq!(n.longitude_defined, !pole);
            if pole {
                assert_eq!(
                    n.geographic.latitude(),
                    if epsg == 5041 { 90. } else { -90. }
                );
            } else {
                assert_eq!(n.geographic.latitude(), 0.);
                assert_eq!(n.geographic.longitude(), 3.);
            }
            let n = c.geographic_node_position(2, 1).unwrap();
            let q = c.native_position_for_geographic(n.geographic).unwrap();
            assert!(
                (q.native_x - n.native_x).abs() < 1e-5 && (q.native_y - n.native_y).abs() < 1e-5
            );
            assert_eq!(c.root_enclosure.full_grid, Enclosure::UnverifiedProjected);
            assert!(c.geographic_node_position(3, 0).is_err());
            assert_eq!(
                c.read_window(GridWindow {
                    column: 0,
                    row: 0,
                    width: 3,
                    height: 2
                })
                .unwrap()
                .samples
                .iter()
                .map(|s| s.value)
                .collect::<Vec<_>>(),
                vec![Some(-3.), None, Some(1.), Some(3.), Some(5.), Some(6.)]
            );
        }
    }
    #[test]
    fn domain_diagnostics_ignore_fill_and_commit_only_successful_windows() {
        let f = fixture(true, true, 0.5, 0.5);
        polygon_instance(
            &f.0,
            Some(&[[0.5, 1.5], [3.5, 1.5], [0.5, 3.5], [0.5, 1.5]]),
        );
        {
            let file = hdf5::File::open_rw(&f.0).unwrap();
            let values = file
                .dataset("BathymetryCoverage/BathymetryCoverage.01/Group_001/values")
                .unwrap();
            let mut rows = values.read_raw::<DepthValue>().unwrap();
            rows[3].depth = f32::NAN;
            values.write_raw(&rows).unwrap();
        }
        let items = BathymetryCoverage::open(&f.0).unwrap();
        let c = &items[0];
        assert!(c
            .read_window(GridWindow {
                column: 0,
                row: 0,
                width: 3,
                height: 2
            })
            .is_err());
        assert!(!c.observed_depth_centroids_outside_domain());
        assert_eq!(
            c.read_window(GridWindow {
                column: 2,
                row: 0,
                width: 1,
                height: 1
            })
            .unwrap()
            .samples[0]
                .value,
            Some(1.)
        );
        assert!(c.observed_depth_centroids_outside_domain());
        drop(items);
        {
            let file = hdf5::File::open_rw(&f.0).unwrap();
            let values = file
                .dataset("BathymetryCoverage/BathymetryCoverage.01/Group_001/values")
                .unwrap();
            let mut rows = values.read_raw::<DepthValue>().unwrap();
            rows[2].depth = 1e6;
            values.write_raw(&rows).unwrap();
        }
        let items = BathymetryCoverage::open(&f.0).unwrap();
        let c = &items[0];
        assert_eq!(
            c.read_window(GridWindow {
                column: 2,
                row: 0,
                width: 1,
                height: 1
            })
            .unwrap()
            .samples[0]
                .value,
            None
        );
        assert!(!c.observed_depth_centroids_outside_domain());
    }
    #[test]
    fn domains_require_one_representation_and_a_simple_closed_ring() {
        let f = fixture(false, false, 0.5, 0.5);
        polygon_instance(&f.0, None);
        assert!(BathymetryCoverage::open(&f.0).is_err());
        for vertices in [
            vec![[0., 0.], [1., 0.], [0., 1.], [1., 1.]],
            vec![[0., 0.], [1., 1.], [0., 1.], [1., 0.], [0., 0.]],
        ] {
            let f = fixture(false, false, 0.5, 0.5);
            polygon_instance(&f.0, Some(&vertices));
            assert!(BathymetryCoverage::open(&f.0).is_err());
        }
        let f = fixture(false, false, 0.5, 0.5);
        {
            let file = hdf5::File::open_rw(&f.0).unwrap();
            let g = file
                .group("BathymetryCoverage/BathymetryCoverage.01")
                .unwrap();
            let v = [Vertex {
                longitude: 0.,
                latitude: 0.,
            }; 4];
            g.new_dataset::<Vertex>()
                .shape(4)
                .create("domainExtent.polygon")
                .unwrap()
                .write_raw(&v)
                .unwrap();
        }
        assert!(BathymetryCoverage::open(&f.0).is_err());
    }
    #[test]
    fn product_storage_rejects_reverse_axes_and_accepts_supported_projected_names() {
        for (sx, sy) in [(-1f64, 1f64), (1., -1.), (-1., -1.)] {
            let f = fixture(false, false, 0.5, 0.5);
            {
                let file = hdf5::File::open_rw(&f.0).unwrap();
                let g = file
                    .group("BathymetryCoverage/BathymetryCoverage.01")
                    .unwrap();
                g.attr("gridSpacingLongitudinal")
                    .unwrap()
                    .write_scalar(&sx)
                    .unwrap();
                g.attr("gridSpacingLatitudinal")
                    .unwrap()
                    .write_scalar(&sy)
                    .unwrap();
            }
            assert!(BathymetryCoverage::open(&f.0)
                .unwrap_err()
                .to_string()
                .contains("canonical storage"));
        }
        for crs in [32601u32, 32630, 32660, 32701, 32760, 5041, 5042] {
            let f = fixture(false, false, 0.5, 0.5);
            {
                let file = hdf5::File::open_rw(&f.0).unwrap();
                file.attr("horizontalCRS")
                    .unwrap()
                    .write_scalar(&crs)
                    .unwrap();
                file.group("BathymetryCoverage")
                    .unwrap()
                    .attr("sequencingRule.scanDirection")
                    .unwrap()
                    .write_scalar(&VarLenAscii::from_ascii("Easting, Northing").unwrap())
                    .unwrap();
                write_test_axes(
                    &file.group("BathymetryCoverage").unwrap(),
                    file.attr("horizontalCRS")
                        .unwrap()
                        .read_scalar::<u32>()
                        .unwrap(),
                );
            }
            let c = BathymetryCoverage::open(&f.0).unwrap();
            assert_eq!(c[0].geometry().horizontal_crs, crs);
            assert_eq!(
                c[0].sample_nearest(1., 2.).unwrap().unwrap().value,
                Some(-3.)
            );
        }
        for (crs, axes) in [
            (4326u32, "Latitude, Longitude"),
            (4326, "-Longitude, Latitude"),
            (32630, "Longitude, Latitude"),
            (32600, "Easting, Northing"),
            (32761, "Easting, Northing"),
        ] {
            let f = fixture(false, false, 0.5, 0.5);
            {
                let file = hdf5::File::open_rw(&f.0).unwrap();
                file.attr("horizontalCRS")
                    .unwrap()
                    .write_scalar(&crs)
                    .unwrap();
                file.group("BathymetryCoverage")
                    .unwrap()
                    .attr("sequencingRule.scanDirection")
                    .unwrap()
                    .write_scalar(&VarLenAscii::from_ascii(axes).unwrap())
                    .unwrap();
            }
            assert!(BathymetryCoverage::open(&f.0).is_err());
        }
    }
    #[test]
    fn omitted_uncertainty_supplies_group_constant_for_windows_query_and_selection() {
        for constant in [0f32, 0.75, 1e6] {
            let f = fixture(false, false, constant, constant);
            let c = BathymetryCoverage::open(&f.0).unwrap();
            let c = &c[0];
            assert_eq!(c.uncertainty_encoding, UncertaintyEncoding::GroupConstant);
            assert_eq!(c.time_point, "00010101T000000Z");
            let expected = if constant == 1e6 {
                None
            } else {
                Some(constant)
            };
            let tile = c
                .read_window(GridWindow {
                    column: 1,
                    row: 0,
                    width: 2,
                    height: 2,
                })
                .unwrap();
            assert_eq!(
                tile.samples.iter().map(|s| s.value).collect::<Vec<_>>(),
                [None, Some(1.), Some(5.), Some(6.)]
            );
            assert!(tile.samples.iter().all(|s| s.uncertainty == expected));
            assert_eq!(
                c.sample_nearest(1., 2.).unwrap().unwrap().uncertainty,
                expected
            );
            use ferrite_kernel::depth_selection::{DepthReference, IdentityDepthAdjustment};
            let provider = IdentityDepthAdjustment;
            let composed = ConservativeCoverage::new(
                vec![DatumCoverage {
                    coverage: c,
                    reference: DepthReference(10),
                }],
                DepthReference(10),
                &provider,
            )
            .unwrap();
            let selected = composed.query_nearest(1., 2.).unwrap().unwrap();
            assert_eq!(selected.candidate.raw_depth, Some(-3.));
            assert_eq!(selected.candidate.uncertainty, expected.map(f64::from));
        }
    }
    #[test]
    fn uncertainty_field_and_feature_definition_must_agree() {
        for (cell, definition) in [(true, false), (false, true)] {
            let f = fixture(cell, definition, 0.5, 0.5);
            assert!(BathymetryCoverage::open(&f.0)
                .unwrap_err()
                .to_string()
                .contains("disagree"));
        }
        let f = fixture(false, false, 0.5, 0.75);
        assert!(BathymetryCoverage::open(&f.0)
            .unwrap_err()
            .to_string()
            .contains("equal group extrema"));
        let f = fixture(true, true, 0.5, 0.5);
        assert_eq!(
            BathymetryCoverage::open(&f.0).unwrap()[0].uncertainty_encoding,
            UncertaintyEncoding::PerCell
        );
    }
    #[test]
    fn required_group_attributes_are_not_silently_defaulted() {
        for missing in [
            "minimumDepth",
            "maximumDepth",
            "minimumUncertainty",
            "maximumUncertainty",
            "timePoint",
        ] {
            let f = fixture(false, false, 0.5, 0.5);
            // Recreate a values group with one required attribute omitted.
            {
                let file = hdf5::File::open_rw(&f.0).unwrap();
                let instance = file
                    .group("BathymetryCoverage/BathymetryCoverage.01")
                    .unwrap();
                instance.relink("Group_001", "Saved").unwrap();
                let saved = instance.group("Saved").unwrap();
                let group = instance.create_group("Group_001").unwrap();
                for name in [
                    "minimumDepth",
                    "maximumDepth",
                    "minimumUncertainty",
                    "maximumUncertainty",
                ] {
                    if name != missing {
                        attr(
                            &group,
                            name,
                            saved.attr(name).unwrap().read_scalar::<f32>().unwrap(),
                        );
                    }
                }
                if missing != "timePoint" {
                    text(&group, "timePoint", "00010101T000000Z");
                }
                let rows = saved
                    .dataset("values")
                    .unwrap()
                    .read_raw::<DepthOnly>()
                    .unwrap();
                group
                    .new_dataset::<DepthOnly>()
                    .shape([2, 3])
                    .create("values")
                    .unwrap()
                    .write_raw(&rows)
                    .unwrap();
                instance.unlink("Saved").unwrap();
            }
            assert!(BathymetryCoverage::open(&f.0).is_err(), "{missing}");
        }
    }
    #[test]
    fn extrema_and_decoded_values_reject_invalid_or_unbounded_data() {
        for (min, max) in [
            (f32::NAN, 0.5),
            (0.5, f32::INFINITY),
            (-0.5, -0.5),
            (1e6, 0.5),
            (0.75, 0.5),
        ] {
            let f = fixture(false, false, min, max);
            assert!(BathymetryCoverage::open(&f.0).is_err());
        }
        for (depth, uncertainty) in [(f32::NAN, 0.5), (0., f32::INFINITY), (0., -0.5)] {
            let f = fixture(true, true, 0.5, 0.5);
            {
                let file = hdf5::File::open_rw(&f.0).unwrap();
                let values = file
                    .dataset("BathymetryCoverage/BathymetryCoverage.01/Group_001/values")
                    .unwrap();
                let mut rows = values.read_raw::<DepthValue>().unwrap();
                rows[0] = DepthValue { depth, uncertainty };
                values.write_raw(&rows).unwrap();
            }
            let c = BathymetryCoverage::open(&f.0).unwrap();
            assert!(c[0]
                .read_window(GridWindow {
                    column: 0,
                    row: 0,
                    width: 1,
                    height: 1
                })
                .is_err());
            // Invalid unrelated cells are not read by a bounded window.
            assert_eq!(
                c[0].read_window(GridWindow {
                    column: 2,
                    row: 1,
                    width: 1,
                    height: 1
                })
                .unwrap()
                .samples[0]
                    .value,
                Some(6.)
            );
        }
    }

    #[test]
    fn inconsistent_extrema_preserve_raw_values_and_record_separate_issues() {
        let f = fixture(true, true, 0.5, 0.5);
        {
            let file = hdf5::File::open_rw(&f.0).unwrap();
            let values = file
                .dataset("BathymetryCoverage/BathymetryCoverage.01/Group_001/values")
                .unwrap();
            let mut rows = values.read_raw::<DepthValue>().unwrap();
            rows[0] = DepthValue {
                depth: 7.,
                uncertainty: 0.75,
            };
            values.write_raw(&rows).unwrap();
        }
        let c = BathymetryCoverage::open(&f.0).unwrap();
        let c = &c[0];
        assert_eq!(c.observed_range_violations(), [false, false]);
        let sample = c
            .read_window(GridWindow {
                column: 0,
                row: 0,
                width: 1,
                height: 1,
            })
            .unwrap()
            .samples[0];
        assert_eq!((sample.value, sample.uncertainty), (Some(7.), Some(0.75)));
        assert_eq!(c.observed_range_violations(), [true, true]);
    }
    #[derive(H5Type, Clone, Copy)]
    #[repr(C)]
    struct Extra {
        depth: f32,
        other: f32,
    }
    #[derive(H5Type, Clone, Copy)]
    #[repr(C)]
    struct Wide {
        depth: f64,
    }
    #[test]
    fn unknown_members_definition_codes_and_wrong_precision_are_rejected() {
        for wide in [false, true] {
            let f = fixture(false, false, 0.5, 0.5);
            {
                let file = hdf5::File::open_rw(&f.0).unwrap();
                let g = file
                    .group("BathymetryCoverage/BathymetryCoverage.01/Group_001")
                    .unwrap();
                g.unlink("values").unwrap();
                if wide {
                    g.new_dataset::<Wide>()
                        .shape([2, 3])
                        .create("values")
                        .unwrap()
                        .write_raw(&[Wide { depth: 1. }; 6])
                        .unwrap();
                } else {
                    g.new_dataset::<Extra>()
                        .shape([2, 3])
                        .create("values")
                        .unwrap()
                        .write_raw(
                            &[Extra {
                                depth: 1.,
                                other: 2.,
                            }; 6],
                        )
                        .unwrap();
                }
            }
            assert!(BathymetryCoverage::open(&f.0).is_err());
        }
        for code in ["other", "depth"] {
            let f = fixture(false, false, 0.5, 0.5);
            {
                let file = hdf5::File::open_rw(&f.0).unwrap();
                let g = file.group("Group_F").unwrap();
                g.unlink("BathymetryCoverage").unwrap();
                let rows: Vec<_> = ["depth", code]
                    .into_iter()
                    .map(|code| Definition::for_code(code))
                    .collect();
                g.new_dataset::<Definition>()
                    .shape(2)
                    .create("BathymetryCoverage")
                    .unwrap()
                    .write_raw(&rows)
                    .unwrap();
            }
            assert!(BathymetryCoverage::open(&f.0).is_err());
        }
    }
    #[test]
    fn fixed_unicode_timepoint_is_decoded_without_losing_the_original_clock() {
        let f = fixture(false, false, 0.5, 0.5);
        {
            let file = hdf5::File::open_rw(&f.0).unwrap();
            let instance = file
                .group("BathymetryCoverage/BathymetryCoverage.01")
                .unwrap();
            instance.relink("Group_001", "Saved").unwrap();
            let saved = instance.group("Saved").unwrap();
            let g = instance.create_group("Group_001").unwrap();
            for n in [
                "minimumDepth",
                "maximumDepth",
                "minimumUncertainty",
                "maximumUncertainty",
            ] {
                attr(&g, n, saved.attr(n).unwrap().read_scalar::<f32>().unwrap());
            }
            let stamp: hdf5::types::FixedUnicode<32> = "20260228T120000+0900".parse().unwrap();
            attr(&g, "timePoint", stamp);
            let rows = saved
                .dataset("values")
                .unwrap()
                .read_raw::<DepthOnly>()
                .unwrap();
            g.new_dataset::<DepthOnly>()
                .shape([2, 3])
                .create("values")
                .unwrap()
                .write_raw(&rows)
                .unwrap();
            instance.unlink("Saved").unwrap();
        }
        assert_eq!(
            BathymetryCoverage::open(&f.0).unwrap()[0].time_point,
            "20260228T120000+0900"
        );
    }
    #[test]
    fn hdf_datetime_is_complete_basic_and_calendar_valid() {
        for value in [
            "00010101T000000Z",
            "20260228T120000+0900",
            "20260228T120000",
            "20260228T120000.125Z",
        ] {
            assert!(validate_time_point(value).is_ok(), "{value}");
        }
        for value in [
            "2026-02-28T12:00:00Z",
            "20260230T120000Z",
            "20260228T1200Z",
            "20260228T120000+09:00",
            "20260228T250000Z",
            "20260228",
            "garbage",
        ] {
            assert!(validate_time_point(value).is_err(), "{value}");
        }
    }
}

#[cfg(test)]
mod boundary_stack_tests {
    use super::*;
    fn legacy_compare_boundary(a: GridBoundary, b: GridBoundary) -> Result<std::cmp::Ordering> {
        // Grow an error-free expansion for a-b. Each term is an exact decoded
        // f64 or FMA product residual; at most six terms, independent of cell count.
        let mut expansion: Vec<f64> = Vec::with_capacity(6);
        for mut q in a.terms.into_iter().chain(b.terms.map(|v| -v)) {
            let mut next: Vec<f64> = Vec::with_capacity(6);
            for e in expansion {
                let sum = q + e;
                ensure!(
                    sum.is_finite(),
                    "S-102 extent difference precision overflow"
                );
                let virtual_e = sum - q;
                let error = (q - (sum - virtual_e)) + (e - virtual_e);
                ensure!(
                    virtual_e.is_finite() && error.is_finite(),
                    "S-102 extent expansion overflow"
                );
                if error != 0. {
                    next.push(error);
                }
                q = sum;
            }
            if q != 0. {
                next.push(q);
            }
            expansion = next;
        }
        Ok(expansion
            .last()
            .map(|v| v.partial_cmp(&0.).unwrap())
            .unwrap_or(std::cmp::Ordering::Equal))
    }
    #[test]
    fn fixed_expansion_matches_independent_scaled_integer_oracle() {
        let mut state = 0x67a83297bff10da3u64;
        let mut next = || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            state
        };
        for case in 0..12000 {
            let base = -1000 + (next() % 1901) as i32;
            let mut terms = [0.; 6];
            let mut integers = [0i128; 6];
            for i in 0..6 {
                let shift = (next() % 61) as u32;
                let mantissa = ((next() % 2097153) as i64) - 1048576;
                integers[i] = (mantissa as i128) << shift;
                terms[i] = (mantissa as f64) * 2f64.powi(base + shift as i32);
            }
            if case % 3 == 0 {
                terms[3] = terms[0];
                integers[3] = integers[0];
            }
            if case % 7 == 0 {
                terms[4] = terms[1];
                terms[5] = terms[2];
                integers[4] = integers[1];
                integers[5] = integers[2];
            }
            let a = GridBoundary {
                terms: [terms[0], terms[1], terms[2]],
                rounded: 0.,
            };
            let b = GridBoundary {
                terms: [terms[3], terms[4], terms[5]],
                rounded: 0.,
            };
            let exact = (integers[0] + integers[1] + integers[2])
                - (integers[3] + integers[4] + integers[5]);
            let actual = compare_boundary(a, b).unwrap();
            assert_eq!(actual, exact.cmp(&0), "case {case}");
            assert_eq!(actual, legacy_compare_boundary(a, b).unwrap());
            assert_eq!(compare_boundary(b, a).unwrap(), actual.reverse());
        }
    }
    #[test]
    fn exact_cell_boundaries_and_overflow_behavior_unchanged() {
        for origin in [-1e16, -500000., -180., 0., 90., 500000., 1e16] {
            for spacing in [2f64.powi(-40), 0.25, 1., 2., 5000.] {
                for coefficient in [-0.5, 0.5, 1.5, 2047.5, 1048575.5] {
                    let a = boundary(origin, spacing, coefficient).unwrap();
                    for point in [
                        a.rounded,
                        f64::from_bits(a.rounded.to_bits().wrapping_add(1)),
                        f64::from_bits(a.rounded.to_bits().wrapping_sub(1)),
                    ] {
                        if !point.is_finite() {
                            continue;
                        }
                        let b = boundary(point, 1., 0.).unwrap();
                        assert_eq!(
                            compare_boundary(a, b).unwrap(),
                            legacy_compare_boundary(a, b).unwrap()
                        );
                    }
                }
            }
        }
        let a = GridBoundary {
            terms: [f64::MAX, f64::MAX, 0.],
            rounded: 0.,
        };
        let b = GridBoundary {
            terms: [0.; 3],
            rounded: 0.,
        };
        assert_eq!(
            compare_boundary(a, b).unwrap_err().to_string(),
            legacy_compare_boundary(a, b).unwrap_err().to_string()
        );
    }
}

mod metadata;
pub use metadata::{AxisMetadata, AxisNamesOrder, IssueMetadata};
#[cfg(test)]
fn write_test_axes(g: &hdf5::Group, crs: u32) {
    let names = canonical_axes(crs).unwrap();
    let a = [
        VarLenAscii::from_ascii(names[1]).unwrap(),
        VarLenAscii::from_ascii(names[0]).unwrap(),
    ];
    if g.link_exists("axisNames") {
        g.dataset("axisNames").unwrap().write_raw(&a).unwrap();
    } else {
        g.new_dataset::<VarLenAscii>()
            .shape(2)
            .create("axisNames")
            .unwrap()
            .write_raw(&a)
            .unwrap();
    }
}

mod feature_metadata;
pub use feature_metadata::{
    DefinitionInterval, FeatureDefinition, FeatureMetadata, FeatureMetadataDiagnostic,
    IntervalClosure,
};
