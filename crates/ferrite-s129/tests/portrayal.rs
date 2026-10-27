//! Part 9 XSLT portrayal of the shipped S-124 and S-129 catalogues on real
//! test data, converted to Part 9a instruction strings.
use ferrite_feature_catalog::FeatureCatalogue;
use ferrite_s100_gml::portrayal::*;
use ferrite_s100_gml::{read_file, Dataset, ReadLimits};
use ferrite_s129::virtual_features::{input_options, INFORMATION_BOX, PLANNED_ROUTE};
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
        &[&ferrite_s129::rules::IDENTITY],
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
fn s129_datasets_portray() {
    let fc = feature_catalogue("S-129");
    let pc = catalogue("S-129");
    assert_eq!(
        pc.info.context,
        vec![("PlainBoundaries".to_string(), "true".to_string())]
    );
    for path in datasets("TestData/IHO-S129-Test-Datasets", "gml") {
        let dataset = read(&path, &fc);
        let input = input_xml(&dataset, Some(&fc), &input_options());
        let out = pc.portray(&input, &[], &ConvertOptions::default()).unwrap();
        for f in &out.features {
            check_shape(&f.instructions);
        }
        let by_code = |code: &'static str| out.features.iter().filter(move |f| f.code == code);

        // Planned route through the control points in passing order.
        let route = by_code(PLANNED_ROUTE).next().expect("planned route");
        assert!(
            route.instructions.contains("ViewingGroup:29060"),
            "{}",
            route.instructions
        );
        assert!(
            route
                .instructions
                .contains("LineStyle:_xslt1,,1,PLRTE,0,Butt,Miter,1;LineInstruction:_xslt1"),
            "{}",
            route.instructions
        );
        let rf = &input.features[route.feature];
        let curve = &input.spatials[rf.spatials[0]];
        let cps: Vec<_> = dataset
            .objects_of("UnderKeelClearanceControlPoint")
            .collect();
        assert_eq!(curve.coords.len(), cps.len());
        let mut times: Vec<(String, [f64; 2])> = cps
            .iter()
            .map(|(_, o)| {
                let ferrite_s100_gml::Geometry::Point(p) = o.geometry[0] else {
                    panic!()
                };
                (o.value(&["expectedPassingTime"]).unwrap().to_string(), p)
            })
            .collect();
        times.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(curve.coords, times.iter().map(|t| t.1).collect::<Vec<_>>());

        // One information box per control point, text escaped for Part 9a.
        let boxes: Vec<_> = by_code(INFORMATION_BOX).collect();
        assert_eq!(boxes.len(), cps.len());
        let first = boxes
            .iter()
            .find(|b| b.feature_id.ends_with(".CP_01"))
            .unwrap();
        // Part 9a escapes only ; : , & (as &s &c &m &a); the rule's line feeds
        // stay literal (parse_instruction_string splits on ';' and the first
        // ':' and trims only the ends, so inner newlines survive).
        let text = first
            .instructions
            .rsplit_once("TextInstruction:")
            .unwrap()
            .1;
        assert!(!text.contains([';', ':', ',']), "{text}");
        let decoded = text
            .replace("&s", ";")
            .replace("&c", ":")
            .replace("&m", ",")
            .replace("&a", "&");
        let lines: Vec<&str> = decoded.split('\n').collect();
        assert_eq!(lines.len(), 4, "{decoded}");
        assert_eq!(lines[0], "CP01");
        assert_eq!(lines[1], "ETA:   17/2200");
        assert!(
            lines[2].starts_with("Open:  ") && lines[3].starts_with("Close: "),
            "{decoded}"
        );
        let parsed = ferrite_lua::PortrayalResult::parse("Box|1", &first.instructions, "").unwrap();
        let host_text = parsed
            .instructions
            .iter()
            .flat_map(|i| &i.commands)
            .find_map(|c| match c {
                ferrite_lua::DrawingCommand::TextInstruction { text, .. } => Some(text.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(host_text, decoded);
        assert!(
            first.instructions.contains("FontColor:BLK,1"),
            "{}",
            first.instructions
        );
        assert!(
            first.instructions.contains("TextAlignVertical:Top")
                && !first.instructions.contains("TextAlignHorizontal"),
            "{}",
            first.instructions
        );
        assert!(
            first.instructions.contains("LocalOffset:30,20"),
            "{}",
            first.instructions
        );
        assert!(out
            .notes
            .iter()
            .any(|n| n.message.contains("text is invisible")));

        // Control point symbols.
        let cp = by_code("UnderKeelClearanceControlPoint").next().unwrap();
        assert!(
            cp.instructions.ends_with("PointInstruction:S-129/UKCCONPT"),
            "{}",
            cp.instructions
        );
        assert!(
            cp.instructions.contains("DisplayPlane:UnderRadar"),
            "{}",
            cp.instructions
        );

        // Non-navigable areas: colour fill and two symbol fill layers.
        let nn = by_code("UnderKeelClearanceNonNavigableArea")
            .next()
            .unwrap();
        assert!(
            nn.instructions.contains("ColorFill:RED,0.5"),
            "{}",
            nn.instructions
        );
        assert_eq!(
            nn.instructions
                .matches("SymbolFill:S-129/DIAMOND1P,22.5,0,0,43.13,true")
                .count(),
            2,
            "{}",
            nn.instructions
        );
        assert!(
            nn.instructions.contains("ViewingGroup:29030,29040"),
            "{}",
            nn.instructions
        );
        assert!(
            nn.instructions.contains("LocalOffset:1,0"),
            "{}",
            nn.instructions
        );
        // The datasets' scaleMinimum 1 is applied (and reported).
        assert!(
            nn.instructions.contains("ScaleMinimum:1;"),
            "{}",
            nn.instructions
        );
        assert!(out
            .notes
            .iter()
            .any(|n| n.message.contains("scaleMinimum 1 hides")));

        // Plan area: plain boundaries by default, symbolized on request.
        let pa = by_code("UnderKeelClearancePlanArea").next().unwrap();
        assert!(
            pa.instructions
                .contains("Dash:0,1.76;LineStyle:_xslt1,3.18,0.32,CHMGD,0,Butt,Miter,0"),
            "{}",
            pa.instructions
        );
        let symbolized = pc
            .portray(
                &input,
                &[("PlainBoundaries", "false")],
                &ConvertOptions::default(),
            )
            .unwrap();
        let pa = symbolized
            .features
            .iter()
            .find(|f| f.code == "UnderKeelClearancePlanArea")
            .unwrap();
        assert!(
            pa.instructions.ends_with("LineInstruction:UKCARE01"),
            "{}",
            pa.instructions
        );

        // Options: opaque text and no feature scale minimum.
        let options = ConvertOptions {
            apply_feature_scale_minimum: false,
            opaque_invisible_text: true,
            ..Default::default()
        };
        let alt = pc.portray(&input, &[], &options).unwrap();
        assert!(alt
            .features
            .iter()
            .all(|f| !f.instructions.contains("ScaleMinimum")));
        let ib = alt
            .features
            .iter()
            .find(|f| f.code == INFORMATION_BOX)
            .unwrap();
        assert!(
            ib.instructions
                .contains("FontColor:BLK,0;FontBackgroundColor:WHITE,0"),
            "{}",
            ib.instructions
        );
    }
}
