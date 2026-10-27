//! Part 9 XSLT portrayal of the shipped S-124 and S-129 catalogues on real
//! test data, converted to Part 9a instruction strings.
use ferrite_feature_catalog::FeatureCatalogue;
use ferrite_s100_gml::portrayal::*;
use ferrite_s100_gml::{read_file, Dataset, GmlIdentity, ProductId, ReadLimits};
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

/// The S-124 and S-129 identities (as their product crates register them).
static PRODUCTS: [&GmlIdentity; 2] = [
    &GmlIdentity {
        id: ProductId("S-124"),
        identifiers: &["S-124", "S124"],
        code: "124",
        late_identification_check: "GML",
    },
    &GmlIdentity {
        id: ProductId("S-129"),
        identifiers: &["S-129", "S129"],
        code: "129",
        late_identification_check: "S-158-129/15",
    },
];

fn read(path: &Path, fc: &FeatureCatalogue) -> Dataset {
    read_file(path, &PRODUCTS, Some(fc), ReadLimits::default()).unwrap()
}

#[test]
fn synthetic_cell_matches_host_ids() {
    let fc = feature_catalogue("S-129");
    let pc = catalogue("S-129");
    let path = root().join("TestData/IHO-S129-Test-Datasets/12900MCTDS130TS.gml");
    let dataset = read(&path, &fc);
    let input = input_xml(&dataset, Some(&fc), &InputOptions::default());
    let out = pc.portray(&input, &[], &ConvertOptions::default()).unwrap();
    let key_of = |i: usize| 1_000 + i as i64;
    let cell = synthetic_cell(&input, key_of);
    assert_eq!(cell.features.len(), input.features.len());
    for f in &out.features {
        let host = f.host_feature_id(key_of(f.feature));
        let (code, key) = host.split_once('|').unwrap();
        assert_eq!(code, f.code);
        let record = &cell.features[&key.parse::<i64>().unwrap()];
        assert_eq!(record.feature_code.as_deref(), Some(code));
        let spatial = record.spatial_associations[0].spatial_id;
        match input.features[f.feature].primitive {
            "Point" => assert!(cell.points.contains_key(&spatial.key())),
            "Curve" => assert!(cell.curves.contains_key(&spatial.key())),
            "Surface" => {
                let surface = &cell.surfaces[&spatial.key()];
                let ring = &surface.exterior_ring[0];
                assert!(cell.curves[&ring.curve_id.key()].all_positions().len() >= 4);
            }
            other => panic!("{other}"),
        }
    }
}

#[test]
fn namespaced_and_deviating_display_lists() {
    let fc = feature_catalogue("S-129");
    let path = root().join("TestData/IHO-S129-Test-Datasets/12900MCTDS130TS.gml");
    let dataset = read(&path, &fc);
    let input = input_xml(&dataset, Some(&fc), &InputOptions::default());
    let list = r#"<p:displayList xmlns:p="http://www.iho.int/S100Presentation/5.2">
      <p:pointInstruction><p:featureReference>CP_01</p:featureReference><p:viewingGroup>1</p:viewingGroup>
        <p:displayPlane>OVERRADAR</p:displayPlane><p:drawingPriority>3</p:drawingPriority><p:scaleMinimum>50000</p:scaleMinimum>
        <p:symbol reference="S1" rotation="45" rotationCRS="GeographicCRS"><p:offset><p:x>1</p:x><p:y>2</p:y></p:offset>
          <p:overrideAll transparency="0.2">CHRED</p:overrideAll></p:symbol></p:pointInstruction>
      <p:textInstruction><p:featureReference>CP_01</p:featureReference><p:viewingGroup>1</p:viewingGroup>
        <p:displayPlane>UnderRadar</p:displayPlane><p:drawingPriority>3</p:drawingPriority>
        <p:textPoint horizontalAlignment="center"><p:element><p:text>a;b,c:d&amp;e</p:text><p:bodySize>12</p:bodySize>
          <p:flags><p:flag>UnderLine</p:flag></p:flags>
          <p:fontCharacteristics><p:serifs>true</p:serifs><p:weight>Bold</p:weight><p:slant>Italics</p:slant><p:proportion>MonoSpaced</p:proportion></p:fontCharacteristics>
          <p:foreground>CHBLK</p:foreground><p:bogus/></p:element></p:textPoint></p:textInstruction>
      <p:areaInstruction><p:featureReference>NON_NAVIGABLE_0</p:featureReference><p:viewingGroup>1</p:viewingGroup>
        <p:displayPlane>UnderRadar</p:displayPlane><p:drawingPriority>1</p:drawingPriority>
        <p:hatchFill><p:areaCRS>LocalGeometry</p:areaCRS><p:hatch><p:direction><p:x>1</p:x><p:y>1</p:y></p:direction><p:distance>2</p:distance>
          <p:lineStyle capStyle="Round"><p:pen width="0.3"><p:color>CHGRD</p:color></p:pen><p:symbol reference="S-129/LS"><p:position>1</p:position></p:symbol></p:lineStyle></p:hatch></p:hatchFill></p:areaInstruction>
      <p:augmentedRay><p:featureReference>CP_01</p:featureReference><p:viewingGroup>1</p:viewingGroup>
        <p:displayPlane>UnderRadar</p:displayPlane><p:drawingPriority>1</p:drawingPriority><p:crs>GeographicCRS</p:crs>
        <p:lineStyleReference reference="L1"/><p:direction>90</p:direction><p:length>1000</p:length></p:augmentedRay>
      <p:nullInstruction><p:featureReference>CP_02</p:featureReference><p:viewingGroup>1</p:viewingGroup>
        <p:displayPlane>UnderRadar</p:displayPlane><p:drawingPriority>0</p:drawingPriority></p:nullInstruction>
      <p:pointInstruction><p:featureReference>nobody</p:featureReference></p:pointInstruction>
    </p:displayList>"#;
    let doc = ferrite_xslt::parse(list, None, None).unwrap();
    let root = ferrite_xslt::NodeRef::root_of(doc);
    let options = ConvertOptions {
        apply_feature_scale_minimum: false,
        ..Default::default()
    };
    let out = display_list_instructions(&root, &input, None, &options);
    let cp = out
        .features
        .iter()
        .find(|f| f.feature_id == "CP_01")
        .unwrap();
    assert_eq!(
        cp.instructions,
        "ViewingGroup:1;DisplayPlane:OverRadar;DrawingPriority:3;ScaleMinimum:50000;Rotation:GeographicCRS,45;LocalOffset:1,2;\
OverrideAll:CHRED,0.2;PointInstruction:S-129/S1;\
DisplayPlane:UnderRadar;ScaleMinimum:4294967295;LocalOffset:0,0;Rotation:PortrayalCRS,0;TextAlignHorizontal:Center;\
FontSize:12;FontUnderline:true;FontWeight:Bold;FontSlant:Italics;FontSerifs:true;FontProportion:MonoSpaced;FontColor:CHBLK,0;\
TextInstruction:a&sb&mc&cd&ae;\
DrawingPriority:1;AugmentedRay:GeographicCRS,90,GeographicCRS,1000;LineInstruction:L1;ClearGeometry"
    );
    let area = out
        .features
        .iter()
        .find(|f| f.feature_id == "NON_NAVIGABLE_0")
        .unwrap();
    assert_eq!(
        area.instructions,
        "ViewingGroup:1;DisplayPlane:UnderRadar;DrawingPriority:1;AreaCRS:LocalGeometry;\
LineSymbol:S-129/LS,1,0,LocalCRS,1;LineStyle:_xslt1,,0.3,CHGRD,0,Round,Miter,0;HatchFill:1,1,2,_xslt1"
    );
    let null = out
        .features
        .iter()
        .find(|f| f.feature_id == "CP_02")
        .unwrap();
    assert_eq!(
        null.instructions,
        "ViewingGroup:1;DisplayPlane:UnderRadar;NullInstruction"
    );
    assert!(out
        .notes
        .iter()
        .any(|n| n.message.contains("unknown featureReference 'nobody'")));
    assert!(out
        .notes
        .iter()
        .any(|n| n.message.contains("Non-schema element bogus")));
}

#[test]
fn undeclared_viewing_groups_are_dropped() {
    let fc = feature_catalogue("S-124");
    let pc = catalogue("S-124");
    let path = root().join("TestData/UKHO-S100-2026/ExchangeSets/S-124_GB/S100_ROOT/S-124/DATASET_FILES/124GB00S1Y2026N36.GML");
    let dataset = read(&path, &fc);
    let input = input_xml(&dataset, Some(&fc), &InputOptions::default());
    let list = r#"<displayList><textInstruction><featureReference>P1</featureReference><viewingGroup>26</viewingGroup>
      <displayPlane>UNDERRADAR</displayPlane><drawingPriority>1</drawingPriority>
      <textPoint><element><text>x</text><bodySize>10</bodySize><foreground>CHBLK</foreground></element></textPoint></textInstruction></displayList>"#;
    let doc = ferrite_xslt::parse(list, None, None).unwrap();
    let out = display_list_instructions(
        &ferrite_xslt::NodeRef::root_of(doc.clone()),
        &input,
        Some(&pc.info),
        &ConvertOptions::default(),
    );
    assert!(out.features.is_empty());
    assert!(
        out.notes
            .iter()
            .any(|n| n.message.contains("viewing group 26 is not declared")),
        "{:#?}",
        out.notes
    );

    // Errata correction: the group is treated as declared, with a note.
    let options = ConvertOptions {
        implicit_viewing_groups: ["26".to_string()].into_iter().collect(),
        ..Default::default()
    };
    let out = display_list_instructions(
        &ferrite_xslt::NodeRef::root_of(doc),
        &input,
        Some(&pc.info),
        &options,
    );
    assert_eq!(out.features.len(), 1);
    assert!(
        out.features[0].instructions.starts_with("ViewingGroup:26;"),
        "{}",
        out.features[0].instructions
    );
    assert!(out.notes.iter().any(|n| n
        .message
        .contains("Undeclared viewing group 26 treated as declared")));
}

#[test]
fn symbol_namespacing_is_idempotent() {
    const S124: ProductId = ProductId("S-124");
    const S129: ProductId = ProductId("S-129");
    assert_eq!(namespaced_symbol(S129, "UKCCONPT"), "S-129/UKCCONPT");
    assert_eq!(namespaced_symbol(S129, "S-129/UKCCONPT"), "S-129/UKCCONPT");
    assert_eq!(namespaced_symbol(S124, "S-129X"), "S-124/S-129X");
    assert_eq!(namespaced_symbol(S124, "S-124/A"), "S-124/A");
}
