//! Bounded, inspection-only worker wire format for the legacy S-102 trial reader.
//! No HDF handles, paths, datum conversions or operational coverage traits cross
//! this boundary. Source identity is checked against the parent's captured input;
//! it is not a signature or proof that a malicious worker actually read that file.
use crate::legacy_trial::{LegacyTrialGrid, TrialGrid, TrialVerticalDatum};
use anyhow::{ensure, Context, Result};
use std::io::{Read, Write};

const MAGIC: &[u8; 8] = b"FSTI0001";
const MAX_TEXT: usize = 4096;
pub const MAX_SAMPLES: usize = 65_536;
pub const MAX_WIRE_BYTES: u64 = 768 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrialSourceIdentity {
    pub byte_length: u64,
    pub sha256: [u8; 32],
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrialWindow {
    pub column: u32,
    pub row: u32,
    pub width: u32,
    pub height: u32,
}
impl TrialWindow {
    fn count(self) -> Result<usize> {
        ensure!(self.width > 0 && self.height > 0, "Empty trial wire window");
        let count = u64::from(self.width) * u64::from(self.height);
        ensure!(
            count <= MAX_SAMPLES as u64,
            "Trial wire sample budget exceeded"
        );
        Ok(count as usize)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrialInspectionRequest {
    pub source: TrialSourceIdentity,
    pub window: Option<TrialWindow>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrialSampleBits {
    pub depth: u32,
    pub uncertainty: u32,
}
#[derive(Debug, Clone, PartialEq)]
pub struct TrialInspectionMetadata {
    pub product_specification: String,
    pub issue_date_lexical: String,
    pub root_bounds: [f64; 4],
    pub width: u32,
    pub height: u32,
    pub origin: [f64; 2],
    pub spacing: [f64; 2],
    pub vertical_datum: TrialVerticalDatum,
    pub common_point_rule: u8,
    /// All eight lexical Group_F columns in code/name/uom.name/fillValue/
    /// datatype/lower/upper/closure order. No inferred source metadata.
    pub feature_definitions: Vec<[String; 8]>,
    pub declared_depth_extrema: Option<[f64; 2]>,
    pub declared_uncertainty_extrema: Option<[f64; 2]>,
}
impl TrialInspectionMetadata {
    fn validate(&self) -> Result<()> {
        ensure!(
            matches!(
                self.product_specification.as_str(),
                "INT.IHO.S-102.2.1" | "INT.IHO.S-102.2.1.0"
            ),
            "Trial wire edition mismatch"
        );
        ensure!(
            !self.issue_date_lexical.is_empty(),
            "Missing original trial date"
        );
        for text in std::iter::once(&self.product_specification)
            .chain(std::iter::once(&self.issue_date_lexical))
            .chain(self.feature_definitions.iter().flat_map(|r| r.iter()))
        {
            ensure!(text.len() <= MAX_TEXT, "Trial wire text limit exceeded");
        }
        ensure!(
            self.width > 0
                && self.height > 0
                && u64::from(self.width) * u64::from(self.height) <= 100_000_000,
            "Trial wire grid budget exceeded"
        );
        ensure!(
            self.root_bounds
                .iter()
                .chain(self.origin.iter())
                .chain(self.spacing.iter())
                .all(|v| v.is_finite()),
            "Trial wire nonfinite geometry"
        );
        ensure!(
            self.root_bounds[0] >= -180.
                && self.root_bounds[1] <= 180.
                && self.root_bounds[0] <= self.root_bounds[1]
                && self.root_bounds[2] >= -90.
                && self.root_bounds[3] <= 90.
                && self.root_bounds[2] <= self.root_bounds[3],
            "Trial wire bbox invalid"
        );
        ensure!(
            self.spacing.iter().all(|v| *v > 0.),
            "Trial wire spacing invalid"
        );
        let last = [
            self.origin[0] + f64::from(self.width - 1) * self.spacing[0],
            self.origin[1] + f64::from(self.height - 1) * self.spacing[1],
        ];
        ensure!(
            self.origin[0] >= -180. && last[0] <= 180. && self.origin[1] >= -90. && last[1] <= 90.,
            "Trial wire nodes outside geographic axes"
        );
        ensure!(
            (1..=4).contains(&self.common_point_rule),
            "Trial wire common point rule unsupported"
        );
        ensure!(
            self.feature_definitions.len() == 2
                && ["depth", "uncertainty"].into_iter().all(|code| self
                    .feature_definitions
                    .iter()
                    .filter(|row| row[0] == code
                        && row[2] == "metres"
                        && matches!(row[4].as_str(), "H5T_FLOAT" | "H5T_NATIVE_FLOAT"))
                    .count()
                    == 1),
            "Trial wire feature definitions unsupported"
        );
        for values in [
            self.declared_depth_extrema,
            self.declared_uncertainty_extrema,
        ]
        .into_iter()
        .flatten()
        {
            ensure!(
                values.iter().all(|v| v.is_finite()) && values[0] <= values[1],
                "Trial wire extrema invalid"
            );
        }
        Ok(())
    }
    fn window_count(&self, window: Option<TrialWindow>) -> Result<usize> {
        if let Some(w) = window {
            let count = w.count()?;
            ensure!(
                u64::from(w.column) + u64::from(w.width) <= u64::from(self.width)
                    && u64::from(w.row) + u64::from(w.height) <= u64::from(self.height),
                "Trial wire window outside source"
            );
            Ok(count)
        } else {
            Ok(0)
        }
    }
    /// Original trial producer alias stays lexical and explicitly diagnostic.
    /// Its actual HDF values must already have been checked as float32 by the reader.
    pub fn has_legacy_datatype_alias(&self) -> bool {
        self.feature_definitions
            .iter()
            .any(|row| row[4] == "H5T_NATIVE_FLOAT")
    }
    pub fn missing_vertical_datum(&self) -> bool {
        self.vertical_datum == TrialVerticalDatum::Missing
    }
}
/// Only this validated representation leaves decode(). It deliberately provides
/// no operational coverage adapter or public mutable metadata/sample fields.
#[derive(Debug, Clone, PartialEq)]
pub struct TrialInspection {
    request: TrialInspectionRequest,
    metadata: TrialInspectionMetadata,
    samples: Vec<TrialSampleBits>,
}
impl TrialInspection {
    pub fn metadata(&self) -> &TrialInspectionMetadata {
        &self.metadata
    }
    pub fn samples(&self) -> &[TrialSampleBits] {
        &self.samples
    }
    pub fn request(&self) -> TrialInspectionRequest {
        self.request
    }
    fn validate(&self, expected: TrialInspectionRequest) -> Result<()> {
        ensure!(
            self.request == expected,
            "Trial worker source/window identity mismatch"
        );
        ensure!(
            expected.source.byte_length > 0,
            "Trial source length is empty"
        );
        self.metadata.validate()?;
        let count = self.metadata.window_count(expected.window)?;
        ensure!(
            self.samples.len() == count,
            "Trial wire sample count mismatch"
        );
        Ok(())
    }
    /// Worker-side extraction only. Caller must retain/verify CapturedInput and
    /// run HDF decoding in the existing isolated, time/output-bounded worker.
    /// Passing an identity here records it; it does not authenticate its contents.
    pub fn from_grid(grid: &LegacyTrialGrid, request: TrialInspectionRequest) -> Result<Self> {
        ensure!(
            request.source.byte_length > 0,
            "Trial source length is empty"
        );
        // LegacyTrialGrid's raw source metadata fields are public. Recheck their
        // sizes before cloning, so a caller-modified instance cannot duplicate
        // arbitrarily large text/row buffers in this bounded worker boundary.
        ensure!(
            grid.feature_definitions.len() == 2,
            "Trial feature row count invalid"
        );
        for text in std::iter::once(&grid.product_specification)
            .chain(std::iter::once(&grid.issue_date_lexical))
            .chain(grid.feature_definitions.iter().flat_map(|r| {
                [
                    &r.code,
                    &r.name,
                    &r.unit,
                    &r.fill_value,
                    &r.datatype,
                    &r.lower,
                    &r.upper,
                    &r.closure,
                ]
            }))
        {
            ensure!(text.len() <= MAX_TEXT, "Trial wire text limit exceeded");
        }
        let TrialGrid {
            width,
            height,
            origin_longitude,
            origin_latitude,
            spacing_longitude,
            spacing_latitude,
        } = grid.geometry();
        let metadata = TrialInspectionMetadata {
            product_specification: grid.product_specification.clone(),
            issue_date_lexical: grid.issue_date_lexical.clone(),
            root_bounds: grid.root_bounds,
            width: u32::try_from(width)?,
            height: u32::try_from(height)?,
            origin: [origin_longitude, origin_latitude],
            spacing: [spacing_longitude, spacing_latitude],
            vertical_datum: grid.vertical_datum,
            common_point_rule: grid.common_point_rule,
            feature_definitions: grid
                .feature_definitions
                .iter()
                .map(|r| {
                    [
                        r.code.clone(),
                        r.name.clone(),
                        r.unit.clone(),
                        r.fill_value.clone(),
                        r.datatype.clone(),
                        r.lower.clone(),
                        r.upper.clone(),
                        r.closure.clone(),
                    ]
                })
                .collect(),
            declared_depth_extrema: grid.declared_depth_extrema,
            declared_uncertainty_extrema: grid.declared_uncertainty_extrema,
        };
        metadata.validate()?;
        metadata.window_count(request.window)?;
        let samples = if let Some(w) = request.window {
            w.count()?;
            grid.read_window(
                w.column as usize,
                w.row as usize,
                w.width as usize,
                w.height as usize,
            )?
            .into_iter()
            .map(|s| TrialSampleBits {
                depth: s.depth.to_bits(),
                uncertainty: s.uncertainty.to_bits(),
            })
            .collect()
        } else {
            Vec::new()
        };
        let result = Self {
            request,
            metadata,
            samples,
        };
        result.validate(request)?;
        Ok(result)
    }
    pub fn encode(&self, mut writer: impl Write) -> Result<()> {
        self.validate(self.request)?;
        // Validation fixes the maximum encoded size below768KiB. No unbounded
        // serialization allocation and no path/unknown semantic fields in v1.
        writer.write_all(MAGIC)?;
        writer.write_all(&self.request.source.byte_length.to_le_bytes())?;
        writer.write_all(&self.request.source.sha256)?;
        write_u8(&mut writer, u8::from(self.request.window.is_some()))?;
        if let Some(w) = self.request.window {
            for v in [w.column, w.row, w.width, w.height] {
                write_u32(&mut writer, v)?;
            }
        }
        let m = &self.metadata;
        for s in [&m.product_specification, &m.issue_date_lexical] {
            write_text(&mut writer, s)?;
        }
        for v in m.root_bounds {
            write_f64(&mut writer, v)?;
        }
        for v in [m.width, m.height] {
            write_u32(&mut writer, v)?;
        }
        for v in m.origin.into_iter().chain(m.spacing) {
            write_f64(&mut writer, v)?;
        }
        match m.vertical_datum {
            TrialVerticalDatum::Missing => write_u8(&mut writer, 0)?,
            TrialVerticalDatum::Encoded(code) => {
                write_u8(&mut writer, 1)?;
                write_u32(&mut writer, code)?;
            }
        }
        write_u8(&mut writer, m.common_point_rule)?;
        write_u8(&mut writer, m.feature_definitions.len() as u8)?;
        for row in &m.feature_definitions {
            for s in row {
                write_text(&mut writer, s)?;
            }
        }
        for values in [m.declared_depth_extrema, m.declared_uncertainty_extrema] {
            write_u8(&mut writer, u8::from(values.is_some()))?;
            if let Some(values) = values {
                for v in values {
                    write_f64(&mut writer, v)?;
                }
            }
        }
        write_u32(&mut writer, self.samples.len() as u32)?;
        for s in &self.samples {
            write_u32(&mut writer, s.depth)?;
            write_u32(&mut writer, s.uncertainty)?;
        }
        Ok(())
    }
    pub fn decode(reader: impl Read, expected: TrialInspectionRequest) -> Result<Self> {
        // read_exact is bounded by each admitted field/count; Take is an extra
        // aggregate limit, not a substitute for a worker timeout or memory cap.
        ensure!(
            expected.source.byte_length > 0,
            "Trial source length is empty"
        );
        let mut r = reader.take(MAX_WIRE_BYTES + 1);
        ensure!(
            &read_array::<8>(&mut r)? == MAGIC,
            "Unknown trial worker protocol"
        );
        let source = TrialSourceIdentity {
            byte_length: u64::from_le_bytes(read_array(&mut r)?),
            sha256: read_array(&mut r)?,
        };
        let window = match read_u8(&mut r)? {
            0 => None,
            1 => Some(TrialWindow {
                column: read_u32(&mut r)?,
                row: read_u32(&mut r)?,
                width: read_u32(&mut r)?,
                height: read_u32(&mut r)?,
            }),
            _ => anyhow::bail!("Invalid trial window tag"),
        };
        let request = TrialInspectionRequest { source, window };
        ensure!(
            request == expected,
            "Trial worker source/window identity mismatch"
        );
        if let Some(w) = window {
            w.count()?;
        }
        let product_specification = read_text(&mut r)?;
        let issue_date_lexical = read_text(&mut r)?;
        let mut root_bounds = [0.; 4];
        for v in &mut root_bounds {
            *v = read_f64(&mut r)?;
        }
        let width = read_u32(&mut r)?;
        let height = read_u32(&mut r)?;
        let origin = [read_f64(&mut r)?, read_f64(&mut r)?];
        let spacing = [read_f64(&mut r)?, read_f64(&mut r)?];
        let vertical_datum = match read_u8(&mut r)? {
            0 => TrialVerticalDatum::Missing,
            1 => TrialVerticalDatum::Encoded(read_u32(&mut r)?),
            _ => anyhow::bail!("Invalid trial datum tag"),
        };
        let common_point_rule = read_u8(&mut r)?;
        let row_count = read_u8(&mut r)?;
        ensure!(row_count == 2, "Trial feature row count invalid");
        let mut feature_definitions = Vec::with_capacity(2);
        for _ in 0..2 {
            feature_definitions.push([
                read_text(&mut r)?,
                read_text(&mut r)?,
                read_text(&mut r)?,
                read_text(&mut r)?,
                read_text(&mut r)?,
                read_text(&mut r)?,
                read_text(&mut r)?,
                read_text(&mut r)?,
            ]);
        }
        let mut extrema = [None; 2];
        for v in &mut extrema {
            *v = match read_u8(&mut r)? {
                0 => None,
                1 => Some([read_f64(&mut r)?, read_f64(&mut r)?]),
                _ => anyhow::bail!("Invalid trial extrema tag"),
            };
        }
        let metadata = TrialInspectionMetadata {
            product_specification,
            issue_date_lexical,
            root_bounds,
            width,
            height,
            origin,
            spacing,
            vertical_datum,
            common_point_rule,
            feature_definitions,
            declared_depth_extrema: extrema[0],
            declared_uncertainty_extrema: extrema[1],
        };
        metadata.validate()?;
        let expected_count = metadata.window_count(window)?;
        let count = read_u32(&mut r)? as usize;
        ensure!(
            count == expected_count && count <= MAX_SAMPLES,
            "Trial sample count invalid"
        );
        let mut samples = Vec::with_capacity(count);
        for _ in 0..count {
            samples.push(TrialSampleBits {
                depth: read_u32(&mut r)?,
                uncertainty: read_u32(&mut r)?,
            });
        }
        let mut trailing = [0u8; 1];
        ensure!(r.read(&mut trailing)? == 0, "Trailing trial worker data");
        let result = Self {
            request,
            metadata,
            samples,
        };
        result.validate(expected)?;
        Ok(result)
    }
}
fn write_u8(w: &mut impl Write, v: u8) -> Result<()> {
    w.write_all(&[v])?;
    Ok(())
}
fn write_u32(w: &mut impl Write, v: u32) -> Result<()> {
    w.write_all(&v.to_le_bytes())?;
    Ok(())
}
fn write_f64(w: &mut impl Write, v: f64) -> Result<()> {
    w.write_all(&v.to_bits().to_le_bytes())?;
    Ok(())
}
fn write_text(w: &mut impl Write, v: &str) -> Result<()> {
    ensure!(v.len() <= MAX_TEXT, "Trial text exceeds limit");
    write_u32(w, v.len() as u32)?;
    w.write_all(v.as_bytes())?;
    Ok(())
}
fn read_array<const N: usize>(r: &mut impl Read) -> Result<[u8; N]> {
    let mut v = [0; N];
    r.read_exact(&mut v)
        .context("Truncated trial worker response")?;
    Ok(v)
}
fn read_u8(r: &mut impl Read) -> Result<u8> {
    Ok(read_array::<1>(r)?[0])
}
fn read_u32(r: &mut impl Read) -> Result<u32> {
    Ok(u32::from_le_bytes(read_array(r)?))
}
fn read_f64(r: &mut impl Read) -> Result<f64> {
    Ok(f64::from_bits(u64::from_le_bytes(read_array(r)?)))
}
fn read_text(r: &mut impl Read) -> Result<String> {
    let n = read_u32(r)? as usize;
    ensure!(n <= MAX_TEXT, "Trial text exceeds limit");
    let mut bytes = vec![0; n];
    r.read_exact(&mut bytes)?;
    Ok(String::from_utf8(bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn value(window: Option<TrialWindow>) -> TrialInspection {
        let request = TrialInspectionRequest {
            source: TrialSourceIdentity {
                byte_length: 123,
                sha256: [17; 32],
            },
            window,
        };
        let row = |code: &str| {
            [
                code.to_owned(),
                code.to_owned(),
                "metres".into(),
                "1000000".into(),
                "H5T_FLOAT".into(),
                "0".into(),
                "12000".into(),
                "closedInterval".into(),
            ]
        };
        TrialInspection {
            request,
            metadata: TrialInspectionMetadata {
                product_specification: "INT.IHO.S-102.2.1".into(),
                issue_date_lexical: "2021-03-24Z".into(),
                root_bounds: [-1., -0.9, 50.7, 50.8],
                width: 2,
                height: 2,
                origin: [-1., 50.7],
                spacing: [0.0001, 0.0001],
                vertical_datum: TrialVerticalDatum::Missing,
                common_point_rule: 1,
                feature_definitions: vec![row("depth"), row("uncertainty")],
                declared_depth_extrema: None,
                declared_uncertainty_extrema: None,
            },
            samples: window
                .map(|w| {
                    vec![
                        TrialSampleBits {
                            depth: (-0.0f32).to_bits(),
                            uncertainty: f32::from_bits(0x7fc00011).to_bits()
                        };
                        w.count().unwrap()
                    ]
                })
                .unwrap_or_default(),
        }
    }
    fn encoded(v: &TrialInspection) -> Vec<u8> {
        let mut b = Vec::new();
        v.encode(&mut b).unwrap();
        b
    }
    #[test]
    fn metadata_and_sample_bits_roundtrip_without_inventing_datum() {
        for window in [
            None,
            Some(TrialWindow {
                column: 0,
                row: 0,
                width: 2,
                height: 2,
            }),
        ] {
            let v = value(window);
            let bytes = encoded(&v);
            let reply = TrialInspection::decode(bytes.as_slice(), v.request).unwrap();
            assert_eq!(reply, v);
            assert!(reply.metadata().missing_vertical_datum());
        }
    }
    #[test]
    fn wrong_identity_window_protocol_truncation_and_trailing_are_rejected() {
        let v = value(None);
        let bytes = encoded(&v);
        let mut wrong = v.request;
        wrong.source.sha256[0] ^= 1;
        assert!(TrialInspection::decode(bytes.as_slice(), wrong).is_err());
        wrong = v.request;
        wrong.window = Some(TrialWindow {
            column: 0,
            row: 0,
            width: 1,
            height: 1,
        });
        assert!(TrialInspection::decode(bytes.as_slice(), wrong).is_err());
        for n in 0..bytes.len() {
            assert!(TrialInspection::decode(&bytes[..n], v.request).is_err());
        }
        let mut unknown = bytes.clone();
        unknown[7] = b'2';
        assert!(TrialInspection::decode(unknown.as_slice(), v.request).is_err());
        let mut trailing = bytes;
        trailing.push(0);
        assert!(TrialInspection::decode(trailing.as_slice(), v.request).is_err());
    }
    #[test]
    fn count_text_and_geometry_budgets_are_checked_before_publication() {
        let v = value(None);
        let mut bytes = encoded(&v);
        // First text prefix follows magic8 + sourceLength8 + digest32 + windowTag1.
        bytes[49..53].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(TrialInspection::decode(bytes.as_slice(), v.request).is_err());
        let mut v = value(None);
        v.metadata.width = u32::MAX;
        assert!(v.encode(Vec::new()).is_err());
        let mut v = value(None);
        v.metadata.origin[0] = f64::NAN;
        assert!(v.encode(Vec::new()).is_err());
        let mut v = value(None);
        v.samples.push(TrialSampleBits {
            depth: 0,
            uncertainty: 0,
        });
        assert!(v.encode(Vec::new()).is_err());
        let v = value(Some(TrialWindow {
            column: 2,
            row: 0,
            width: 1,
            height: 1,
        }));
        assert!(v.encode(Vec::new()).is_err());
        assert!(TrialWindow {
            column: 0,
            row: 0,
            width: u32::MAX,
            height: u32::MAX
        }
        .count()
        .is_err());
    }
    #[test]
    fn out_of_grid_worker_window_is_rejected_before_sample_payload() {
        let v = value(Some(TrialWindow {
            column: 0,
            row: 0,
            width: 1,
            height: 1,
        }));
        let mut bytes = encoded(&v);
        // Same requested one-sample window, but source metadata says only2x2.
        // Change column in both trusted request and forged header; equality alone
        // is insufficient. Remove sample payload to assert semantic early failure.
        bytes[49..53].copy_from_slice(&2u32.to_le_bytes());
        let mut expected = v.request;
        expected.window.as_mut().unwrap().column = 2;
        bytes.truncate(bytes.len() - 12); // sample count4 + one sample8
        let error = TrialInspection::decode(bytes.as_slice(), expected).unwrap_err();
        assert!(error.to_string().contains("window outside source"));
    }
    #[test]
    fn empty_trusted_source_is_rejected_without_reading_worker_data() {
        struct MustNotRead;
        impl Read for MustNotRead {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                panic!("invalid request read worker data")
            }
        }
        let mut expected = value(None).request;
        expected.source.byte_length = 0;
        assert!(TrialInspection::decode(MustNotRead, expected).is_err());
    }
    #[test]
    fn forged_sample_count_does_not_allocate_unbounded_memory() {
        let v = value(None);
        let mut bytes = encoded(&v);
        let end = bytes.len();
        bytes[end - 4..].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(TrialInspection::decode(bytes.as_slice(), v.request).is_err());
    }
}
