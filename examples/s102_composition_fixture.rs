//! Synthetic adapter fixture, NOT navigational transformation evidence.
//! Writes only a new directory's files. No Window/Surface/Device/EventLoop.
#![allow(non_local_definitions)]
use anyhow::{ensure, Context, Result};
use ferrite_s102::hdf5;
use hdf5::{types::VarLenAscii, H5Type};
use sha2::{Digest, Sha256};
use std::path::Path;
#[derive(H5Type, Clone, Copy)]
#[repr(C)]
struct Vertex {
    longitude: f64,
    latitude: f64,
}
#[derive(H5Type, Clone, Copy)]
#[repr(C)]
struct DepthOnly {
    depth: f32,
}
#[derive(H5Type, Clone, Copy)]
#[repr(C)]
struct Value {
    depth: f32,
    uncertainty: f32,
}
#[derive(H5Type, Clone)]
#[repr(C)]
#[allow(non_snake_case)]
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
impl Definition {
    fn for_code(code: &str) -> Self {
        let text = |s: &str| VarLenAscii::from_ascii(s).unwrap();
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
fn declarations(g: &hdf5::Group) {
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

fn attr<T: H5Type>(g: &hdf5::Group, name: &str, value: T) -> Result<()> {
    g.new_attr::<T>().create(name)?.write_scalar(&value)?;
    Ok(())
}
fn text(g: &hdf5::Group, name: &str, value: &str) -> Result<()> {
    attr(g, name, VarLenAscii::from_ascii(value)?)
}
const FILL: f32 = 1_000_000.;
const STEP: f64 = 1. / 1024.;
const WEST: f64 = -2.;
const SOUTH: f64 = 48.;
// Independent reference matrix below is north-to-south, east-to-west.
// Encoded HDF rows are canonical SW/east-then-north and use reversed indices.
const SECOND: [[f32; 4]; 2] = [[40., 55., FILL, 40.], [40., FILL, 55., 40.]];
fn write_instance(b: &hdf5::Group, index: u32, domain_population: bool) -> Result<()> {
    let g = b.create_group(&format!("BathymetryCoverage.{index:02}"))?;
    if index == 2 {
        attr(&g, "verticalDatum", 23u16)?;
    }
    attr(&g, "numGRP", 1u8)?;
    text(&g, "startSequence", "0,0")?;
    let (w, ox, oy, sx, sy) = if index == 1 {
        (3, WEST + STEP / 2., SOUTH + STEP / 2., STEP, STEP)
    } else {
        (
            4,
            WEST + 3. * STEP / 8.,
            SOUTH + STEP / 2.,
            3. * STEP / 4.,
            STEP,
        )
    };
    attr(&g, "numPointsLongitudinal", w as u32)?;
    attr(&g, "numPointsLatitudinal", 2u32)?;
    attr(&g, "gridOriginLongitude", ox)?;
    attr(&g, "gridOriginLatitude", oy)?;
    attr(&g, "gridSpacingLongitudinal", sx)?;
    attr(&g, "gridSpacingLatitudinal", sy)?;
    if domain_population {
        let vertices = [
            [WEST, SOUTH],
            [WEST + 3. * STEP, SOUTH],
            [WEST, SOUTH + 2. * STEP],
            [WEST, SOUTH],
        ]
        .map(|p| Vertex {
            longitude: p[0],
            latitude: p[1],
        });
        g.new_dataset::<Vertex>()
            .shape(4)
            .create("domainExtent.polygon")?
            .write_raw(&vertices)?;
    } else {
        attr(&g, "westBoundLongitude", WEST as f32)?;
        attr(&g, "eastBoundLongitude", (WEST + 3. * STEP) as f32)?;
        attr(&g, "southBoundLatitude", SOUTH as f32)?;
        attr(&g, "northBoundLatitude", (SOUTH + 2. * STEP) as f32)?;
    }
    let values: Vec<_> = (0..2)
        .flat_map(|row| {
            (0..w).map(move |col| {
                let d = if index == 1 {
                    35.
                } else {
                    SECOND[1 - row][3 - col]
                };
                Value {
                    depth: d,
                    uncertainty: if d == FILL {
                        FILL
                    } else if index == 1 {
                        0.25
                    } else {
                        0.5
                    },
                }
            })
        })
        .collect();
    let data = g.create_group("Group_001")?;
    attr(
        &data,
        "minimumDepth",
        if index == 1 { 35f32 } else { 40f32 },
    )?;
    attr(
        &data,
        "maximumDepth",
        if index == 1 { 35f32 } else { 55f32 },
    )?;
    attr(
        &data,
        "minimumUncertainty",
        if index == 1 { 0.25f32 } else { 0.5f32 },
    )?;
    attr(
        &data,
        "maximumUncertainty",
        if index == 1 { 0.25f32 } else { 0.5f32 },
    )?;
    text(&data, "timePoint", "00010101T000000Z")?;
    data.new_dataset::<Value>()
        .shape([2, w])
        .create("values")?
        .write_raw(&values)?;
    Ok(())
}
fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let out = args
        .next()
        .context("usage: s102_composition_fixture OUTPUT_DIRECTORY")?;
    let option = args.next();
    ensure!(
        option.as_deref().is_none_or(|v| v == "--omit-uncertainty"
            || v == "--all-fill"
            || v == "--domain-population"
            || v == "--root-too-small"),
        "Unknown fixture option"
    );
    let omit = option.as_deref().is_some_and(|v| v == "--omit-uncertainty");
    let domain_population = option
        .as_deref()
        .is_some_and(|v| v == "--domain-population" || v == "--root-too-small");
    let root_small = option.as_deref().is_some_and(|v| v == "--root-too-small");
    let all_fill = root_small || option.as_deref().is_some_and(|v| v == "--all-fill");
    ensure!(args.next().is_none(), "Extra fixture arguments");
    let out = Path::new(&out);
    std::fs::create_dir_all(out)?;
    let h5 = out.join("synthetic-two-datums.h5");
    let config = out.join("synthetic-depth-policy.json");
    let expected = out.join("independent-expected.json");
    ensure!(
        !h5.exists() && !config.exists() && !expected.exists(),
        "Refusing to overwrite fixture artifacts"
    );
    {
        let file = hdf5::File::create(&h5)?;
        text(&file, "productSpecification", "INT.IHO.S-102.3.0.0")?;
        text(&file, "issueDate", "20261006")?;
        attr(&file, "horizontalCRS", 4326u32)?;
        attr(&file, "verticalCS", 6498u32)?;
        attr(&file, "verticalCoordinateBase", 2u8)?;
        attr(&file, "verticalDatum", 10u16)?;
        attr(&file, "verticalDatumReference", 1u8)?;
        attr(&file, "westBoundLongitude", WEST as f32)?;
        attr(&file, "eastBoundLongitude", (WEST + 3. * STEP) as f32)?;
        attr(&file, "southBoundLatitude", SOUTH as f32)?;
        attr(&file, "northBoundLatitude", (SOUTH + 2. * STEP) as f32)?;
        let b = file.create_group("BathymetryCoverage")?;
        b.new_dataset::<VarLenAscii>()
            .shape(2)
            .create("axisNames")?
            .write_raw(&[
                VarLenAscii::from_ascii("Latitude")?,
                VarLenAscii::from_ascii("Longitude")?,
            ])?;
        for (name, value) in [
            ("dataCodingFormat", 2),
            ("dimension", 2),
            ("commonPointRule", 2),
            ("interpolationType", 1),
            ("numInstances", 2),
            ("dataOffsetCode", 5),
            ("sequencingRule.type", 1),
        ] {
            attr(&b, name, value as u8)?;
        }
        text(&b, "sequencingRule.scanDirection", "Longitude, Latitude")?;
        attr(&b, "horizontalPositionUncertainty", -1f32)?;
        attr(&b, "verticalUncertainty", -1f32)?;
        write_instance(&b, 1, domain_population)?;
        write_instance(&b, 2, domain_population)?;
        let f = file.create_group("Group_F")?;
        declarations(&f);
        let definitions = [
            Definition::for_code("depth"),
            Definition::for_code("uncertainty"),
        ];
        f.new_dataset::<Definition>()
            .shape(2)
            .create("BathymetryCoverage")?
            .write_raw(&definitions)?;
        file.flush()?;
    }
    // Independent single-datum raster control, with oracle-adjusted original values.
    let control = out.join("independent-adjusted-single-datum.h5");
    ensure!(
        !control.exists(),
        "Refusing overwrite of independent control"
    );
    std::fs::copy(&h5, &control)?;
    {
        let f = hdf5::File::open_rw(&control)?;
        let b = f.group("BathymetryCoverage")?;
        b.attr("numInstances")?.write_scalar(&1u8)?;
        b.unlink("BathymetryCoverage.02")?;
        let g = b.group("BathymetryCoverage.01")?;
        g.attr("numPointsLongitudinal")?.write_scalar(&12u32)?;
        g.attr("gridOriginLongitude")?
            .write_scalar(&(WEST + STEP / 8.))?;
        g.attr("gridSpacingLongitudinal")?
            .write_scalar(&(STEP / 4.))?;
        let d = g.group("Group_001")?;
        d.attr("minimumDepth")?.write_scalar(&20f32)?;
        d.attr("maximumUncertainty")?.write_scalar(&0.5f32)?;
        d.unlink("values")?;
        let values: Vec<_> = (0..2usize)
            .flat_map(|row| {
                (0..12usize).map(move |col| {
                    let raw = SECOND[1 - row][3 - col / 3];
                    let wins = raw != FILL && raw - 20. < 35.;
                    Value {
                        depth: if wins { raw - 20. } else { 35. },
                        uncertainty: if wins { 0.5 } else { 0.25 },
                    }
                })
            })
            .collect();
        d.new_dataset::<Value>()
            .shape([2, 12])
            .create("values")?
            .write_raw(&values)?;
        f.flush()?;
    }
    if all_fill {
        for path in [&h5, &control] {
            let file = hdf5::File::open_rw(path)?;
            let b = file.group("BathymetryCoverage")?;
            for name in b
                .member_names()?
                .into_iter()
                .filter(|n| n.starts_with("BathymetryCoverage."))
            {
                let g = b.group(&name)?.group("Group_001")?;
                let values = g.dataset("values")?;
                let rows = vec![
                    Value {
                        depth: FILL,
                        uncertainty: FILL
                    };
                    values.size()
                ];
                values.write_raw(&rows)?;
                for name in [
                    "minimumDepth",
                    "maximumDepth",
                    "minimumUncertainty",
                    "maximumUncertainty",
                ] {
                    g.attr(name)?.write_scalar(&FILL)?;
                }
            }
            file.flush()?;
        }
    }
    if omit {
        let file = hdf5::File::open_rw(&h5)?;
        for index in 1..=2 {
            let g = file.group(&format!(
                "BathymetryCoverage/BathymetryCoverage.{index:02}/Group_001"
            ))?;
            let values = g.dataset("values")?;
            let shape = values.shape();
            let depths: Vec<_> = values
                .read_raw::<Value>()?
                .into_iter()
                .map(|v| DepthOnly { depth: v.depth })
                .collect();
            drop(values);
            g.unlink("values")?;
            g.new_dataset::<DepthOnly>()
                .shape(shape)
                .create("values")?
                .write_raw(&depths)?;
        }
        let g = file.group("Group_F")?;
        g.unlink("BathymetryCoverage")?;
        let definitions = [Definition::for_code("depth")];
        g.new_dataset::<Definition>()
            .shape(1)
            .create("BathymetryCoverage")?
            .write_raw(&definitions)?;
        file.flush()?;
    }
    if root_small {
        for path in [&h5, &control] {
            let file = hdf5::File::open_rw(path)?;
            for (name, value) in [
                ("westBoundLongitude", WEST + STEP),
                ("eastBoundLongitude", WEST + 2. * STEP),
                ("southBoundLatitude", SOUTH + STEP / 2.),
                ("northBoundLatitude", SOUTH + 3. * STEP / 2.),
            ] {
                file.attr(name)?.write_scalar(&(value as f32))?;
            }
            file.flush()?;
        }
    }
    let digest = format!("{:x}", Sha256::digest(std::fs::read(&h5)?));
    let policy = serde_json::json!({"datasets":[{"sha256":digest,"target_datum":10,"corrections":[{"datum":23,"correction_metres":-20.0,"source":"SYNTHETIC constant offset for test only; not navigational evidence"}]}]});
    std::fs::write(&config, serde_json::to_vec_pretty(&policy)?)?;
    // INDEPENDENT oracle: integer partition arithmetic, never calls production
    // ConservativeCoverage/selector/GridGeometry/raster helpers or reads generated H5.
    let mut cells = Vec::new();
    for row in 0..2usize {
        for col in 0..12usize {
            let first = [col / 4, row];
            let second = [col / 3, row];
            let raw = SECOND[1 - second[1]][3 - second[0]];
            let other = (raw != FILL).then_some(f64::from(raw) - 20.);
            let wins = other.is_some_and(|d| d < 35.);
            let (instance, node, depth, datum, raw_depth, correction, uncertainty) = if wins {
                (1, second, other.unwrap(), 23, f64::from(raw), -20., 0.5)
            } else {
                (0, first, 35., 10, 35., 0., 0.25)
            };
            let source_position = if instance == 0 {
                [
                    WEST + (node[0] as f64 + 0.5) * STEP,
                    SOUTH + (node[1] as f64 + 0.5) * STEP,
                ]
            } else {
                [
                    WEST + (node[0] as f64 + 0.5) * 3. * STEP / 4.,
                    SOUTH + (node[1] as f64 + 0.5) * STEP,
                ]
            };
            cells.push(serde_json::json!({"common_column":col,"common_row":row,"common_position":[WEST+(col as f64+0.5)*STEP/4.,SOUTH+(row as f64+0.5)*STEP],"raw_first":35.,"raw_second":if raw==FILL {None}else{Some(raw)},"adjusted_first":35.,"adjusted_second":other,"tie":other==Some(35.),"winner_instance_zero_based":instance,"winner_instance_name":format!("BathymetryCoverage.{:02}",instance+1),"winner_original_node":node,"winner_original_position":source_position,"raw_depth":raw_depth,"source_datum":datum,"correction_metres":correction,"target_datum":10,"adjusted_depth":depth,"uncertainty":uncertainty}));
        }
    }
    // This diagnostic option deliberately populates outside-domain sample positions
    // and overlaps datum polygons. It supplies no masked-composition render oracle.
    if all_fill || domain_population {
        cells.clear();
    }
    let table = serde_json::json!({"synthetic":true,"navigation_evidence":false,"dataset_sha256":digest,"target_datum":10,"geometry":{"bounds":[WEST,SOUTH,WEST+3.*STEP,SOUTH+2.*STEP],"width":12,"height":2,"origin":[WEST+STEP/8.,SOUTH+STEP/2.],"spacing":[STEP/4.,STEP]},"source_shapes":[[3,2],[4,2]],"opposite_xy_axes":false,"canonical_positive_storage":true,"omitted_uncertainty":omit,"all_fill":all_fill,"root_bbox_deliberately_too_small":root_small,"domain_population_diagnostic_fixture":domain_population,"original_sample_position_outside_counts":if domain_population {Some(serde_json::json!({"source":[3,3],"source_populated":[6,6],"control":12,"control_populated":24}))}else{None},"masked_composition_oracle_supplied":false,"cells":cells,"quality_supplied":false,"independent_control":control,"no_full_schema_conformance_claim":true,"gpu_boundary_low_certified":false});
    std::fs::write(&expected, serde_json::to_vec_pretty(&table)?)?;
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"h5":h5,"control":control,"policy":config,"oracle":expected,"dataset_sha256":digest})
        )?
    );
    Ok(())
}
