//! Part 9 XSLT portrayal of the shipped S-124 and S-129 catalogues on real
//! test data, converted to Part 9a instruction strings.
use ferrite_feature_catalog::FeatureCatalogue;
use ferrite_s100_gml::portrayal::*;
use ferrite_s100_gml::{read_file, Dataset, ReadLimits};
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn catalogue(product: &str) -> XsltPortrayal {
    XsltPortrayal::load(&root().join("Catalogues/PC").join(product)).unwrap()
}

fn feature_catalogue(product: &str) -> FeatureCatalogue {
    let dir = root().join("Catalogues/FC").join(product);
    let file = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("xml")))
        .unwrap();
    FeatureCatalogue::load(file).unwrap()
}

fn datasets(dir: &str, extension: &str) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(root().join(dir))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.extension()
                .is_some_and(|e| e.eq_ignore_ascii_case(extension))
        })
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no test data in {dir}");
    files
}

fn read(path: &Path, fc: &FeatureCatalogue) -> Dataset {
    read_file(
        path,
        &[&ferrite_s124::rules::IDENTITY],
        Some(fc),
        ReadLimits::default(),
    )
    .unwrap()
}

const COMMANDS: &[&str] = &[
    "ViewingGroup",
    "DisplayPlane",
    "DrawingPriority",
    "ScaleMinimum",
    "ScaleMaximum",
    "Id",
    "Parent",
    "Hover",
    "LocalOffset",
    "LinePlacement",
    "AreaPlacement",
    "AreaCRS",
    "Rotation",
    "ScaleFactor",
    "Dash",
    "LineSymbol",
    "LineStyle",
    "FontColor",
    "FontBackgroundColor",
    "FontSize",
    "FontProportion",
    "FontWeight",
    "FontSlant",
    "FontSerifs",
    "FontUnderline",
    "FontStrikethrough",
    "FontUpperline",
    "FontReference",
    "TextAlignHorizontal",
    "TextAlignVertical",
    "TextVerticalOffset",
    "OverrideColor",
    "OverrideAll",
    "ClearOverride",
    "SpatialReference",
    "AugmentedPoint",
    "AugmentedRay",
    "AugmentedPath",
    "Polyline",
    "Arc3Points",
    "ArcByRadius",
    "Annulus",
    "ClearGeometry",
    "AlertReference",
    "PointInstruction",
    "LineInstruction",
    "LineInstructionUnsuppressed",
    "ColorFill",
    "AreaFillReference",
    "PixmapFill",
    "SymbolFill",
    "HatchFill",
    "TextInstruction",
    "NullInstruction",
];
const DRAWING: &[&str] = &[
    "PointInstruction",
    "LineInstruction",
    "LineInstructionUnsuppressed",
    "ColorFill",
    "AreaFillReference",
    "PixmapFill",
    "SymbolFill",
    "HatchFill",
    "TextInstruction",
    "NullInstruction",
];

/// The host's Part 9a parser accepts the string, plus a shape check.
fn check_shape(s: &str) {
    assert!(!s.is_empty());
    if let Err(e) = ferrite_lua::PortrayalResult::parse("Feature|1", s, "") {
        panic!("Part 9a parser rejects {s:?}: {e}");
    }
    let mut drawing = false;
    for command in s.split(';') {
        let (name, params) = command.split_once(':').unwrap_or((command, ""));
        assert!(COMMANDS.contains(&name), "unknown command {name:?} in {s}");
        drawing |= DRAWING.contains(&name);
        let numbers = |n: usize| {
            let p: Vec<&str> = params.split(',').collect();
            assert!(p.len() >= n, "{command}");
            for v in &p[..n] {
                assert!(v.parse::<f64>().is_ok(), "non-numeric {v:?} in {command}");
            }
        };
        match name {
            "DrawingPriority" => assert!(params.parse::<i32>().is_ok(), "{command}"),
            "ScaleMinimum" | "ScaleMaximum" => assert!(params.parse::<u32>().is_ok(), "{command}"),
            "LocalOffset" | "Dash" => numbers(2),
            "ViewingGroup" => assert!(params.split(',').all(|g| !g.is_empty())),
            "DisplayPlane" | "PointInstruction" | "AreaFillReference" => {
                assert!(!params.is_empty(), "{command}")
            }
            "Rotation" => {
                let (crs, angle) = params.split_once(',').unwrap();
                assert!(
                    ["GeographicCRS", "PortrayalCRS", "LocalCRS", "LineCRS"].contains(&crs),
                    "{command}"
                );
                assert!(angle.parse::<f64>().is_ok(), "{command}");
            }
            "LineStyle" => {
                let p: Vec<&str> = params.split(',').collect();
                assert_eq!(p.len(), 8, "{command}");
                assert!(p[1].is_empty() || p[1].parse::<f64>().is_ok(), "{command}");
                assert!(p[2].parse::<f64>().is_ok() && !p[3].is_empty(), "{command}");
                let t: f64 = p[4].parse().unwrap();
                assert!((0.0..=1.0).contains(&t), "{command}");
                assert!(["Butt", "Square", "Round"].contains(&p[5]), "{command}");
                assert!(["Miter", "Bevel", "Round"].contains(&p[6]), "{command}");
                assert!(p[7].parse::<f64>().is_ok(), "{command}");
            }
            "ColorFill" | "FontColor" | "FontBackgroundColor" => {
                let (token, t) = params.split_once(',').unwrap();
                assert!(!token.is_empty());
                let t: f64 = t.parse().unwrap();
                assert!((0.0..=1.0).contains(&t), "{command}");
            }
            "SymbolFill" => {
                let p: Vec<&str> = params.split(',').collect();
                assert_eq!(p.len(), 6, "{command}");
                for v in &p[1..5] {
                    assert!(v.parse::<f64>().is_ok(), "{command}");
                }
                assert!(["true", "false"].contains(&p[5]), "{command}");
            }
            "HatchFill" => numbers(3),
            "AreaCRS" => assert!(["Global", "LocalGeometry", "GlobalGeometry"].contains(&params)),
            "TextInstruction" => assert!(!params.is_empty() && !params.contains([';', ','])),
            _ => {}
        }
    }
    assert!(drawing, "no drawing command in {s}");
}

#[test]
fn s124_datasets_portray() {
    let fc = feature_catalogue("S-124");
    let pc = catalogue("S-124");
    let mut total = 0;
    for path in datasets(
        "TestData/UKHO-S100-2026/ExchangeSets/S-124_GB/S100_ROOT/S-124/DATASET_FILES",
        "GML",
    ) {
        let dataset = read(&path, &fc);
        let input = input_xml(&dataset, Some(&fc), &InputOptions::default());
        let xml = input.to_xml();
        assert!(xml.starts_with("<Dataset><InformationTypes>"), "{xml}");
        assert!(xml.contains("<Features>"), "{xml}");
        let out = pc.portray(&input, &[], &ConvertOptions::default()).unwrap();
        assert!(
            !out.features.is_empty(),
            "{}: nothing portrayed",
            path.display()
        );
        for f in &out.features {
            check_shape(&f.instructions);
            // Display planes take the catalogue's declared spelling.
            assert!(
                f.instructions.contains("DisplayPlane:UnderRADAR")
                    || f.instructions.contains("DisplayPlane:OverRADAR"),
                "{}",
                f.instructions
            );
            total += 1;
        }
        let parts = out
            .features
            .iter()
            .filter(|f| f.code == "NavwarnPart")
            .count();
        assert!(parts > 0, "{}", path.display());
        // The preamble (no geometry) is matched by no rule.
        assert!(out.features.iter().all(|f| f.code != "NavwarnPreamble"));
        for f in &out.features {
            assert_eq!(input.features[f.feature].id, f.feature_id);
        }
    }
    assert!(total >= 5);
}

#[test]
fn s124_surface_navwarn_part_point() {
    let fc = feature_catalogue("S-124");
    let pc = catalogue("S-124");
    let path = root().join("TestData/UKHO-S100-2026/ExchangeSets/S-124_GB/S100_ROOT/S-124/DATASET_FILES/124GB00S1Y2026N36.GML");
    let dataset = read(&path, &fc);
    let input = input_xml(&dataset, Some(&fc), &InputOptions::default());
    let out = pc.portray(&input, &[], &ConvertOptions::default()).unwrap();
    let part = out.features.iter().find(|f| f.feature_id == "P1").unwrap();
    assert!(part.instructions.starts_with(
        "ViewingGroup:31020;DisplayPlane:OverRADAR;DrawingPriority:15;PointInstruction:S-124/NavigationalWarningFeaturePartPoint"
    ), "{}", part.instructions);
}

/// The S-124 simpleLineStyle template has XML comments between its
/// xsl:param declarations (regression: they must not end the params).
#[test]
fn s124_surface_navwarn_part_strings() {
    let fc = feature_catalogue("S-124");
    let pc = catalogue("S-124");
    let path = root().join("TestData/UKHO-S100-2026/ExchangeSets/S-124_GB/S100_ROOT/S-124/DATASET_FILES/124GB00S1Y2026N36.GML");
    let dataset = read(&path, &fc);
    let input = input_xml(&dataset, Some(&fc), &InputOptions::default());
    // The header association is named by the catalogue association code.
    let xml = input.to_xml();
    assert!(xml.contains("role=\"header\""), "{xml}");
    let out = pc.portray(&input, &[], &ConvertOptions::default()).unwrap();
    let part = out.features.iter().find(|f| f.feature_id == "P1").unwrap();
    assert_eq!(
        part.instructions,
        "ViewingGroup:31020;DisplayPlane:OverRADAR;DrawingPriority:15;PointInstruction:S-124/NavigationalWarningFeaturePartPoint;\
Dash:0,3.6;LineStyle:_xslt1,5.4,0.64,CHMGD,0,Butt,Miter,0;LineInstruction:_xslt1"
    );
    assert!(
        out.notes.iter().any(|n| n
            .message
            .contains("OVERRADAR matched to declared OverRADAR")),
        "{:#?}",
        out.notes
    );
}

/// Literal S-124 portrayal (default options): the TextPlacement rule's
/// viewing group 26 is undeclared, so its text instructions are dropped.
#[test]
fn s124_text_placement_dropped_by_default() {
    let fc = feature_catalogue("S-124");
    let pc = catalogue("S-124");
    let path = root().join("TestData/UKHO-S100-2026/ExchangeSets/S-124_GB/S100_ROOT/S-124/DATASET_FILES/124GB00S2Y2026N43.GML");
    let text = std::fs::read_to_string(&path).unwrap().replace(
        "</S124:members>",
        "<S124:TextPlacement gml:id=\"TP1\"><S124:text>TEST LABEL</S124:text>\
         <S124:geometry><S100:pointProperty><S100:Point gml:id=\"TPG\"><gml:pos>50.70 -1.10</gml:pos>\
         </S100:Point></S100:pointProperty></S124:geometry></S124:TextPlacement></S124:members>",
    );
    let dataset = ferrite_s100_gml::read_bytes(
        text.as_bytes(),
        &[&ferrite_s124::rules::IDENTITY],
        Some(&fc),
        Default::default(),
    )
    .unwrap();
    let input = input_xml(&dataset, Some(&fc), &InputOptions::default());
    let literal = pc.portray(&input, &[], &ConvertOptions::default()).unwrap();
    assert!(literal
        .features
        .iter()
        .all(|f| !f.instructions.contains("TEST LABEL")));
    assert!(literal
        .notes
        .iter()
        .any(|n| n.message.contains("viewing group 26 is not declared")));
    let options = ConvertOptions {
        implicit_viewing_groups: ["26", "12210"].into_iter().map(String::from).collect(),
        ..Default::default()
    };
    let corrected = pc.portray(&input, &[], &options).unwrap();
    assert!(corrected
        .features
        .iter()
        .any(|f| f.instructions.contains("TEST LABEL")));
}
