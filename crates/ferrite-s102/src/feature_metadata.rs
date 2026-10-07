//! Complete encoded Group_F metadata, independent of application and portrayal.
//! Receiver diagnostics do not rewrite the producer's feature list or sample values.
use anyhow::{ensure, Context, Result};
use ferrite_kernel::ExactDecimal;
use hdf5::{
    types::{CompoundField, CompoundType, TypeDescriptor, VarLenAscii, VarLenUnicode},
    H5Type,
};
use std::cmp::Ordering;
const FIELDS: [&str; 8] = [
    "code",
    "name",
    "uom.name",
    "fillValue",
    "datatype",
    "lower",
    "upper",
    "closure",
];
const FEATURES: [&str; 2] = ["BathymetryCoverage", "QualityOfBathymetryCoverage"];
const STRING_LIMIT: usize = 4096;
#[repr(C)]
struct Column<T, const N: usize> {
    value: T,
}
// SAFETY: sole repr(C) field has offset0, actual T descriptor and object size.
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
fn text(bytes: &[u8], ascii: bool) -> Result<String> {
    ensure!(
        bytes.len() <= STRING_LIMIT,
        "Group_F string exceeds supported4096 bytes"
    );
    ensure!(
        !ascii || bytes.is_ascii(),
        "Invalid Group_F declared ASCII bytes"
    );
    Ok(std::str::from_utf8(bytes)
        .context("Invalid Group_F UTF8")?
        .to_owned())
}
fn forecast(d: &hdf5::Dataset, cap: u64) -> Result<()> {
    let ty = d.dtype()?;
    let space = d.space()?;
    let mut size = 0;
    let status = hdf5::sync::sync(|| unsafe {
        hdf5_sys::h5d::H5Dvlen_get_buf_size(d.id(), ty.id(), space.id(), &mut size)
    });
    ensure!(status >= 0, "Cannot forecast Group_F variable payload");
    ensure!(
        size <= cap,
        "Group_F variable payload exceeds supported budget"
    );
    Ok(())
}
fn column<const N: usize>(d: &hdf5::Dataset, ascii: bool) -> Result<Vec<String>> {
    if ascii {
        d.read_raw::<Column<VarLenAscii, N>>()?
            .into_iter()
            .map(|v| text(v.value.as_bytes(), true))
            .collect()
    } else {
        d.read_raw::<Column<VarLenUnicode, N>>()?
            .into_iter()
            .map(|v| text(v.value.as_bytes(), false))
            .collect()
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntervalClosure {
    Open,
    GeLt,
    GtLe,
    Closed,
    GtSemi,
    GeSemi,
    LtSemi,
    LeSemi,
}
#[derive(Debug, Clone, PartialEq)]
pub struct DefinitionInterval {
    pub lower: Option<ExactDecimal>,
    pub upper: Option<ExactDecimal>,
    pub closure: IntervalClosure,
}
impl DefinitionInterval {
    fn parse(lower: &str, upper: &str, closure: &str) -> Result<Self> {
        let c = match closure {
            "openInterval" => IntervalClosure::Open,
            "geLtInterval" => IntervalClosure::GeLt,
            "gtLeInterval" => IntervalClosure::GtLe,
            "closedInterval" => IntervalClosure::Closed,
            "gtSemiInterval" => IntervalClosure::GtSemi,
            "geSemiInterval" => IntervalClosure::GeSemi,
            "ltSemiInterval" => IntervalClosure::LtSemi,
            "leSemiInterval" => IntervalClosure::LeSemi,
            _ => anyhow::bail!("Unknown Group_F interval closure"),
        };
        let number = |s: &str| -> Result<Option<ExactDecimal>> {
            if s.is_empty() {
                Ok(None)
            } else {
                Ok(Some(
                    ExactDecimal::parse(s).context("Invalid/unsupported Group_F exact decimal")?,
                ))
            }
        };
        let lo = number(lower)?;
        let hi = number(upper)?;
        ensure!(
            matches!(c, IntervalClosure::LtSemi | IntervalClosure::LeSemi) || lo.is_some(),
            "Group_F interval requires lower endpoint"
        );
        ensure!(
            matches!(c, IntervalClosure::GtSemi | IntervalClosure::GeSemi) || hi.is_some(),
            "Group_F interval requires upper endpoint"
        );
        // Preserve a redundant endpoint on a semi-infinite interval; its closure decides membership.
        if !matches!(
            c,
            IntervalClosure::GtSemi
                | IntervalClosure::GeSemi
                | IntervalClosure::LtSemi
                | IntervalClosure::LeSemi
        ) {
            if let (Some(l), Some(u)) = (&lo, &hi) {
                let order = l.compare(u);
                ensure!(
                    order == Ordering::Less
                        || order == Ordering::Equal && c == IntervalClosure::Closed,
                    "Empty/reversed Group_F interval"
                );
            }
        }
        Ok(Self {
            lower: lo,
            upper: hi,
            closure: c,
        })
    }
    pub fn contains(&self, v: f64) -> bool {
        use IntervalClosure::*;
        if !v.is_finite() {
            return false;
        }
        let lower = match self.closure {
            LtSemi | LeSemi => true,
            Open | GtLe | GtSemi => self
                .lower
                .as_ref()
                .is_some_and(|l| l.compare_binary64(v).unwrap() == Ordering::Less),
            _ => self
                .lower
                .as_ref()
                .is_some_and(|l| l.compare_binary64(v).unwrap() != Ordering::Greater),
        };
        let upper = match self.closure {
            GtSemi | GeSemi => true,
            Open | GeLt | LtSemi => self
                .upper
                .as_ref()
                .is_some_and(|u| u.compare_binary64(v).unwrap() == Ordering::Greater),
            _ => self
                .upper
                .as_ref()
                .is_some_and(|u| u.compare_binary64(v).unwrap() != Ordering::Less),
        };
        lower && upper
    }
}
#[derive(Debug, Clone, PartialEq)]
pub struct FeatureDefinition {
    /// All eight original lexical strings, addressed by their canonical member names.
    /// Dataset row order is preserved; physical compound member order is not asserted.
    pub code: String,
    pub name: String,
    pub unit: String,
    pub fill_value: String,
    pub datatype: String,
    pub lower: String,
    pub upper: String,
    pub closure: String,
    pub interval: DefinitionInterval,
}
impl FeatureDefinition {
    fn from_fields(v: [String; 8]) -> Result<Self> {
        let [code, name, unit, fill_value, datatype, lower, upper, closure] = v;
        let interval = DefinitionInterval::parse(&lower, &upper, &closure)?;
        Ok(Self {
            code,
            name,
            unit,
            fill_value,
            datatype,
            lower,
            upper,
            closure,
            interval,
        })
    }
}
fn definitions(g: &hdf5::Group, name: &str, max_rows: usize) -> Result<Vec<FeatureDefinition>> {
    let d = g
        .dataset(name)
        .with_context(|| format!("Missing Group_F/{name}"))?;
    let shape = d.shape();
    ensure!(
        shape.len() == 1 && shape[0] > 0 && shape[0] <= max_rows,
        "Unsupported Group_F/{name} definition shape"
    );
    let TypeDescriptor::Compound(c) = d.dtype()?.to_descriptor()? else {
        anyhow::bail!("Group_F definition must be compound")
    };
    ensure!(
        c.fields.len() == 8,
        "Group_F definitions require all eight named members"
    );
    let mut ascii = [false; 8];
    for (i, n) in FIELDS.iter().enumerate() {
        let matches: Vec<_> = c.fields.iter().filter(|f| f.name == *n).collect();
        ensure!(matches.len() == 1, "Missing/duplicate Group_F member {n}");
        ascii[i] = match matches[0].ty {
            TypeDescriptor::VarLenAscii => true,
            TypeDescriptor::VarLenUnicode => false,
            _ => anyhow::bail!("Group_F {n} must use variable-length strings"),
        };
    }
    forecast(&d, (max_rows * 8 * (STRING_LIMIT + 1)) as u64)?;
    let cols = [
        column::<0>(&d, ascii[0])?,
        column::<1>(&d, ascii[1])?,
        column::<2>(&d, ascii[2])?,
        column::<3>(&d, ascii[3])?,
        column::<4>(&d, ascii[4])?,
        column::<5>(&d, ascii[5])?,
        column::<6>(&d, ascii[6])?,
        column::<7>(&d, ascii[7])?,
    ];
    ensure!(
        cols.iter().all(|c| c.len() == shape[0]),
        "Incomplete Group_F fields"
    );
    let mut rows = Vec::with_capacity(shape[0]);
    let mut columns = cols.map(Vec::into_iter);
    for _ in 0..shape[0] {
        rows.push(FeatureDefinition::from_fields(std::array::from_fn(|i| {
            // Every column length was validated above before consuming any row.
            columns[i].next().unwrap()
        }))?)
    }
    Ok(rows)
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeatureMetadataDiagnostic {
    MissingDeclaredFeature(String),
}
#[derive(Debug, Clone)]
pub struct FeatureMetadata {
    pub declared_features: Vec<String>,
    pub bathymetry: Vec<FeatureDefinition>,
    pub quality: Vec<FeatureDefinition>,
    pub diagnostics: Vec<FeatureMetadataDiagnostic>,
}
impl FeatureMetadata {
    pub(crate) fn read(file: &hdf5::File) -> Result<Self> {
        let g = file.group("Group_F").context("Missing Group_F")?;
        ensure!(
            g.attr_names()?.is_empty(),
            "S102 Group_F must not have attributes"
        );
        let d = g
            .dataset("featureCode")
            .context("Missing Group_F/featureCode")?;
        let shape = d.shape();
        ensure!(
            shape.len() == 1 && (1..=2).contains(&shape[0]),
            "Unsupported Group_F featureCode shape"
        );
        let desc = d.dtype()?.to_descriptor()?;
        let ascii = match desc {
            TypeDescriptor::VarLenAscii => true,
            TypeDescriptor::VarLenUnicode => false,
            _ => anyhow::bail!("Group_F featureCode requires variable-length strings"),
        };
        forecast(&d, 2 * (STRING_LIMIT + 1) as u64)?;
        let declared_features: Vec<String> = if ascii {
            d.read_raw::<VarLenAscii>()?
                .into_iter()
                .map(|v| text(v.as_bytes(), true))
                .collect::<Result<_>>()?
        } else {
            d.read_raw::<VarLenUnicode>()?
                .into_iter()
                .map(|v| text(v.as_bytes(), false))
                .collect::<Result<_>>()?
        };
        let mut seen = std::collections::BTreeSet::new();
        for code in &declared_features {
            ensure!(
                FEATURES.contains(&code.as_str()),
                "Unknown Group_F featureCode {code}"
            );
            ensure!(seen.insert(code.as_str()), "Duplicate Group_F featureCode");
        }
        let diagnostics = FEATURES
            .iter()
            .filter(|f| !seen.contains(**f))
            .map(|f| FeatureMetadataDiagnostic::MissingDeclaredFeature((*f).into()))
            .collect();
        // Actual named definition tables are required even when a producer omits a list entry.
        let bathymetry = definitions(&g, FEATURES[0], 2)?;
        let quality = definitions(&g, FEATURES[1], 1)?;
        let mut codes = std::collections::BTreeSet::new();
        for row in &bathymetry {
            ensure!(
                matches!(row.code.as_str(), "depth" | "uncertainty"),
                "Unknown BathymetryCoverage feature definition"
            );
            ensure!(
                codes.insert(row.code.as_str()),
                "Duplicate Group_F feature definition"
            );
            ensure!(
                row.unit == "metres" && row.datatype == "H5T_FLOAT",
                "Unsupported S102 depth/uncertainty units or datatype"
            );
            ensure!(
                ExactDecimal::parse(&row.fill_value)?.compare_binary64(1_000_000.)?
                    == Ordering::Equal,
                "S102 depth/uncertainty fill must be1000000"
            );
        }
        ensure!(codes.contains("depth"), "Missing depth feature definition");
        let q = &quality[0];
        ensure!(
            q.code == "iD"
                && q.unit.is_empty()
                && q.datatype == "H5T_INTEGER"
                && q.fill_value.parse::<u32>()? == 0,
            "Unsupported S102 quality feature definition"
        );
        Ok(Self {
            declared_features,
            bathymetry,
            quality,
            diagnostics,
        })
    }
    /// Metadata-only producer gate. Does not assert full-file or full-standard conformance.
    pub fn validate_declaration(&self) -> Result<()> {
        ensure!(
            self.diagnostics.is_empty(),
            "Incomplete Group_F featureCode declaration: {:?}",
            self.diagnostics
        );
        Ok(())
    }
    pub(crate) fn fills(&self) -> (f32, Option<f32>) {
        (
            1_000_000.,
            self.bathymetry
                .iter()
                .any(|v| v.code == "uncertainty")
                .then_some(1_000_000.),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Definition;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    struct File(std::path::PathBuf, hdf5::File);
    impl Drop for File {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    fn file() -> File {
        let p = std::env::temp_dir().join(format!(
            "s102-definition-{}-{}.h5",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let f = hdf5::File::create(&p).unwrap();
        File(p, f)
    }
    fn fixture(codes: &[&str]) -> File {
        let f = file();
        let g = f.1.create_group("Group_F").unwrap();
        g.new_dataset::<VarLenAscii>()
            .shape(codes.len())
            .create("featureCode")
            .unwrap()
            .write_raw(
                &codes
                    .iter()
                    .map(|s| VarLenAscii::from_ascii(s).unwrap())
                    .collect::<Vec<_>>(),
            )
            .unwrap();
        g.new_dataset::<Definition>()
            .shape(2)
            .create(FEATURES[0])
            .unwrap()
            .write_raw(&[
                Definition::for_code("uncertainty"),
                Definition::for_code("depth"),
            ])
            .unwrap();
        g.new_dataset::<Definition>()
            .shape(1)
            .create(FEATURES[1])
            .unwrap()
            .write_raw(&[Definition::for_code("iD")])
            .unwrap();
        f
    }
    fn replace(f: &File, rows: &[Definition]) {
        let g = f.1.group("Group_F").unwrap();
        g.unlink(FEATURES[0]).unwrap();
        g.new_dataset::<Definition>()
            .shape(rows.len())
            .create(FEATURES[0])
            .unwrap()
            .write_raw(rows)
            .unwrap();
    }
    #[test]
    fn full_definition_preserves_order_lexical_fields_and_infinite_endpoint() {
        let f = fixture(&[FEATURES[1], FEATURES[0]]);
        let m = FeatureMetadata::read(&f.1).unwrap();
        assert_eq!(m.declared_features, [FEATURES[1], FEATURES[0]]);
        m.validate_declaration().unwrap();
        assert_eq!(m.bathymetry[0].code, "uncertainty");
        assert_eq!(m.bathymetry[0].upper, "");
        assert_eq!(m.bathymetry[0].closure, "geSemiInterval");
        assert!(m.bathymetry[0].interval.contains(1e100));
        assert!(!m.bathymetry[0].interval.contains(-1.));
        assert_eq!(m.bathymetry[1].lower, "-14");
        assert_eq!(m.bathymetry[1].unit, "metres");
        assert_eq!(m.quality[0].code, "iD");
        assert_eq!(m.quality[0].name, "ID");
        assert_eq!(m.fills(), (1e6, Some(1e6)));
        replace(&f, &[Definition::for_code("depth")]);
        assert_eq!(FeatureMetadata::read(&f.1).unwrap().fills(), (1e6, None));
    }
    #[test]
    fn incomplete_producer_list_diagnosed_without_inventing_or_discarding_source() {
        let f = fixture(&[FEATURES[0]]);
        let m = FeatureMetadata::read(&f.1).unwrap();
        assert_eq!(m.declared_features, [FEATURES[0]]);
        assert_eq!(
            m.diagnostics,
            [FeatureMetadataDiagnostic::MissingDeclaredFeature(
                FEATURES[1].into()
            )]
        );
        assert!(m.validate_declaration().is_err());
        assert_eq!(m.quality.len(), 1);
    }
    #[test]
    fn feature_codes_admit_only_known_unique_variable_1d_strings() {
        for codes in [
            vec![FEATURES[0], FEATURES[0]],
            vec!["bathymetryCoverage"],
            vec!["Unknown"],
        ] {
            let f = fixture(&codes);
            assert!(FeatureMetadata::read(&f.1).is_err());
        }
        let f = fixture(&FEATURES);
        let g = f.1.group("Group_F").unwrap();
        g.unlink("featureCode").unwrap();
        assert!(FeatureMetadata::read(&f.1).is_err());
        g.new_dataset::<VarLenAscii>()
            .shape([1, 2])
            .create("featureCode")
            .unwrap();
        assert!(FeatureMetadata::read(&f.1).is_err());
        g.unlink("featureCode").unwrap();
        g.new_dataset::<VarLenAscii>()
            .create("featureCode")
            .unwrap();
        assert!(FeatureMetadata::read(&f.1).is_err());
        g.unlink("featureCode").unwrap();
        g.new_dataset::<hdf5::types::FixedAscii<32>>()
            .shape(2)
            .create("featureCode")
            .unwrap();
        assert!(FeatureMetadata::read(&f.1).is_err());
    }
    #[test]
    fn missing_compound_members_wrong_units_type_fill_and_code_rejected() {
        let f = fixture(&FEATURES);
        let g = f.1.group("Group_F").unwrap();
        g.unlink(FEATURES[0]).unwrap();
        g.new_dataset::<Column<VarLenAscii, 0>>()
            .shape(1)
            .create(FEATURES[0])
            .unwrap();
        assert!(FeatureMetadata::read(&f.1)
            .unwrap_err()
            .to_string()
            .contains("eight"));
        for mutation in 0..7 {
            let f = fixture(&FEATURES);
            let mut r = Definition::for_code("depth");
            let v = |s: &str| VarLenAscii::from_ascii(s).unwrap();
            match mutation {
                0 => r.unit = v("feet"),
                1 => r.datatype = v("H5T_INTEGER"),
                2 => r.fillValue = v("-9999"),
                3 => r.code = v("Depth"),
                4 => r.closure = v("unknown"),
                5 => r.lower = v("NaN"),
                _ => r.upper = v("-15"),
            };
            replace(&f, &[r]);
            assert!(FeatureMetadata::read(&f.1).is_err());
        }
        let f = fixture(&FEATURES);
        replace(
            &f,
            &[Definition::for_code("depth"), Definition::for_code("depth")],
        );
        assert!(FeatureMetadata::read(&f.1).is_err());
        let f = fixture(&FEATURES);
        let g = f.1.group("Group_F").unwrap();
        g.unlink(FEATURES[1]).unwrap();
        assert!(FeatureMetadata::read(&f.1).is_err());
    }
    #[test]
    fn all_eight_interval_types_use_exact_endpoint_ownership_and_missing_bound_rules() {
        for (c, lower, upper, at_lo, at_hi) in [
            ("openInterval", "0", "1", false, false),
            ("geLtInterval", "0", "1", true, false),
            ("gtLeInterval", "0", "1", false, true),
            ("closedInterval", "0", "1", true, true),
            ("gtSemiInterval", "0", "", false, true),
            ("geSemiInterval", "0", "", true, true),
            ("ltSemiInterval", "", "1", true, false),
            ("leSemiInterval", "", "1", true, true),
        ] {
            let i = DefinitionInterval::parse(lower, upper, c).unwrap();
            assert_eq!(i.contains(0.), at_lo, "{c}");
            assert_eq!(i.contains(1.), at_hi, "{c}");
            assert!(i.contains(0.5));
            assert!(!i.contains(f64::NAN));
        }
        assert!(DefinitionInterval::parse("0", "0", "closedInterval")
            .unwrap()
            .contains(0.));
        for (a, b, c) in [
            ("", "1", "closedInterval"),
            ("0", "", "closedInterval"),
            ("0", "0", "openInterval"),
            ("1", "0", "geLtInterval"),
            ("NaN", "", "geSemiInterval"),
            ("0", "inf", "closedInterval"),
        ] {
            assert!(DefinitionInterval::parse(a, b, c).is_err());
        }
    }
    #[test]
    fn decimal_metadata_is_not_rounded_and_ray_unused_endpoint_is_preserved() {
        let i = DefinitionInterval::parse("0", "1.0000000000000000001", "openInterval").unwrap();
        assert!(i.contains(1.));
        let narrow = DefinitionInterval::parse(
            "1.0000000000000000001",
            "1.0000000000000000002",
            "openInterval",
        )
        .unwrap();
        assert!(!narrow.contains(1.));
        for upper in ["0", "-100", "5"] {
            let ray = DefinitionInterval::parse("5", upper, "geSemiInterval").unwrap();
            assert!(ray.contains(5.));
            assert!(!ray.contains(4.));
        }
        let f = fixture(&FEATURES);
        let mut row = Definition::for_code("depth");
        row.fillValue = VarLenAscii::from_ascii("1000000.0000000000001").unwrap();
        replace(&f, &[row]);
        assert!(FeatureMetadata::read(&f.1).is_err());
    }
    #[test]
    fn informative_long_names_are_preserved_without_unverified_catalogue_guess() {
        let f = fixture(&FEATURES);
        let mut row = Definition::for_code("depth");
        row.name = VarLenAscii::from_ascii("Sounding depth").unwrap();
        replace(&f, &[row]);
        let g = f.1.group("Group_F").unwrap();
        g.unlink(FEATURES[1]).unwrap();
        let mut q = Definition::for_code("iD");
        q.name = VarLenAscii::from_ascii("Survey quality identifier").unwrap();
        g.new_dataset::<Definition>()
            .shape(1)
            .create(FEATURES[1])
            .unwrap()
            .write_raw(&[q])
            .unwrap();
        let m = FeatureMetadata::read(&f.1).unwrap();
        assert_eq!(m.bathymetry[0].name, "Sounding depth");
        assert_eq!(m.quality[0].name, "Survey quality identifier");
    }
    #[test]
    fn variable_payload_size_forecast_precedes_owned_strings() {
        let f = fixture(&FEATURES);
        let mut r = Definition::for_code("depth");
        r.name = VarLenAscii::from_ascii(&vec![b'a'; 1024 * 1024]).unwrap();
        replace(&f, &[r]);
        assert!(FeatureMetadata::read(&f.1)
            .unwrap_err()
            .to_string()
            .contains("payload"));
        assert!(text(&vec![b'a'; 4097], true).is_err());
        assert!(text(&[0xff], false).is_err());
        assert!(text("é".as_bytes(), true).is_err());
    }
    #[derive(H5Type, Clone)]
    #[repr(C)]
    #[allow(non_snake_case)]
    struct UnicodeDefinition {
        code: VarLenUnicode,
        name: VarLenUnicode,
        #[hdf5(rename = "uom.name")]
        unit: VarLenUnicode,
        fillValue: VarLenUnicode,
        datatype: VarLenUnicode,
        lower: VarLenUnicode,
        upper: VarLenUnicode,
        closure: VarLenUnicode,
    }
    #[test]
    fn unicode_compound_and_invalid_source_utf8_are_checked_without_str_ub() {
        let f = fixture(&FEATURES);
        let g = f.1.group("Group_F").unwrap();
        g.unlink(FEATURES[0]).unwrap();
        let t = |s: &str| s.parse::<VarLenUnicode>().unwrap();
        let d = g
            .new_dataset::<UnicodeDefinition>()
            .shape(1)
            .create(FEATURES[0])
            .unwrap();
        d.write_raw(&[UnicodeDefinition {
            code: t("depth"),
            name: t("depth"),
            unit: t("metres"),
            fillValue: t("1000000"),
            datatype: t("H5T_FLOAT"),
            lower: t("-14"),
            upper: t("11050"),
            closure: t("closedInterval"),
        }])
        .unwrap();
        assert!(FeatureMetadata::read(&f.1).is_ok());
        // Write invalid bytes through a full eight-pointer C record; never build invalid Rust str.
        let ty = d.dtype().unwrap();
        let bad = [0xffu8, 0];
        let raw = [
            b"depth\0".as_ptr(),
            bad.as_ptr(),
            b"metres\0".as_ptr(),
            b"1000000\0".as_ptr(),
            b"H5T_FLOAT\0".as_ptr(),
            b"-14\0".as_ptr(),
            b"11050\0".as_ptr(),
            b"closedInterval\0".as_ptr(),
        ];
        let status = hdf5::sync::sync(|| unsafe {
            hdf5_sys::h5d::H5Dwrite(
                d.id(),
                ty.id(),
                hdf5_sys::h5s::H5S_ALL,
                hdf5_sys::h5s::H5S_ALL,
                hdf5_sys::h5p::H5P_DEFAULT,
                raw.as_ptr().cast(),
            )
        });
        assert!(status >= 0);
        assert!(FeatureMetadata::read(&f.1)
            .unwrap_err()
            .to_string()
            .contains("UTF8"));
    }
}
