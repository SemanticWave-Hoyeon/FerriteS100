use chrono::{TimeZone, Utc};
use ferrite_s100_gml::{Geometry, Value};
use ferrite_s124::import::*;
use ferrite_s124::navwarn::{evaluate, NavwarnInput, Status};
use std::path::{Path, PathBuf};

fn data(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../TestData/SHOM-PING-S124")
        .join(rel)
}

fn now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).unwrap()
}

fn fc() -> ferrite_feature_catalog::FeatureCatalogue {
    ferrite_feature_catalog::FeatureCatalogue::load(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Catalogues/FC/S-124/124_FC_2.0.0.xml"),
    )
    .unwrap()
}

#[test]
fn gpx_avinav_cherbourg() {
    let ds = import_path_with(&data("gpx/AVINAV CHERBOURG-72-26.gpx"), None, now()).unwrap();
    assert!(ds.imported);
    assert_eq!(ds.product, ferrite_s124::PRODUCT);
    let pre = ds.object("PRE").unwrap();
    assert_eq!(
        pre.value(&["messageSeriesIdentifier", "nameOfSeries"]),
        Some("AVINAV CHERBOURG")
    );
    assert_eq!(
        pre.value(&["messageSeriesIdentifier", "warningNumber"]),
        Some("72")
    );
    assert_eq!(
        pre.value(&["messageSeriesIdentifier", "year"]),
        Some("2026")
    );
    assert_eq!(
        pre.value(&["messageSeriesIdentifier", "agencyResponsibleForProduction"]),
        Some("COMNORD INFONAUT")
    );
    assert_eq!(pre.value(&["navwarnTypeGeneral"]), Some("1"));
    assert_eq!(
        pre.value(&["navwarnTitle", "text"]),
        Some("AVINAV CHERBOURG 72/26")
    );
    assert_eq!(
        pre.value(&["publicationTime"]),
        Some("2026-10-13T12:00:00Z")
    );
    let parts: Vec<_> = ds.objects_of("NavwarnPart").collect();
    assert_eq!(parts.len(), 1);
    let p = parts[0].1;
    assert_eq!(p.references[0].role, "header");
    assert_eq!(p.references[0].target, Some(0));
    assert_eq!(
        p.value(&["warningInformation", "information", "text"]),
        Some("Buoy Missing")
    );
    match &p.geometry[0] {
        Geometry::Point(c) => {
            assert!(
                (c[0] + 1.6338333333333022).abs() < 1e-12
                    && (c[1] - 49.14383333333335).abs() < 1e-12
            )
        }
        g => panic!("{g:?}"),
    }
    assert!(ds
        .issues
        .iter()
        .any(|i| i.check == "IMPORT" && i.message.contains("not authoritative")));
    assert!(ds.issues.iter().any(|i| i.message.contains("Disclaimer")));

    // With the feature catalogue, a navwarnTypeDetails label is coded.
    let fc = fc();
    let ds = import_path_with(&data("gpx/AVINAV CHERBOURG-72-26.gpx"), Some(&fc), now()).unwrap();
    let p = ds.object("P1").unwrap();
    let wi = p.attribute("warningInformation").unwrap();
    let details = wi
        .children()
        .iter()
        .find(|a| a.code == "navwarnTypeDetails")
        .unwrap();
    assert_eq!(
        details.value,
        Value::Simple {
            text: "Buoy Missing".into(),
            code: Some("49".into())
        }
    );

    // Lifecycle: imported, identity synthesised, active.
    let r = evaluate(
        &[NavwarnInput {
            dataset: &ds,
            received: now(),
        }],
        now(),
    );
    let m = &r.messages[0];
    assert!(m.imported);
    assert_eq!(
        m.identity.as_ref().unwrap().to_string(),
        "AVINAV CHERBOURG 72/26"
    );
    assert_eq!(m.status, Status::Active);
}

#[test]
fn kml_exports() {
    let mut kinds = Vec::new();
    for name in [
        "export.kml",
        "export (1).kml",
        "export (2).kml",
        "export (3).kml",
    ] {
        let ds = import_path_with(&data(&format!("kml/{name}")), None, now()).unwrap();
        assert!(ds.imported);
        let parts: Vec<_> = ds.objects_of("NavwarnPart").collect();
        assert_eq!(parts.len(), 1, "{name}");
        let g = &parts[0].1.geometry[0];
        kinds.push(g.primitive());
        for c in g.coords() {
            assert!(
                (-9.0..1.0).contains(&c[0]) && (47.0..51.0).contains(&c[1]),
                "{name} {c:?}"
            );
        }
        if let Geometry::Surface { exterior, .. } = g {
            assert_eq!(exterior.first(), exterior.last());
        }
        // No names in the export: no title, nil series number.
        let pre = ds.object("PRE").unwrap();
        assert_eq!(
            pre.value(&["messageSeriesIdentifier", "nameOfSeries"]),
            Some("Imported KML")
        );
        assert!(pre.attribute("navwarnTitle").is_none());
        assert!(matches!(p_warning(&ds), Value::Nil { .. }));
        assert!(ds
            .issues
            .iter()
            .any(|i| i.message.starts_with("Source folder identifier")));
    }
    assert_eq!(kinds, vec!["Point", "Surface", "Point", "Surface"]);
    let ds = import_path_with(&data("kml/export.kml"), None, now()).unwrap();
    assert_eq!(
        ds.objects[1].geometry[0],
        Geometry::Point([-8.430000000000033, 47.48000000000001])
    );
    let ds = import_path_with(&data("kml/export (1).kml"), None, now()).unwrap();
    match &ds.objects[1].geometry[0] {
        Geometry::Surface {
            exterior,
            interiors,
        } => {
            assert_eq!(exterior.len(), 65);
            assert!(interiors.is_empty());
        }
        g => panic!("{g:?}"),
    }
}

fn p_warning(ds: &ferrite_s100_gml::Dataset) -> &Value {
    &ds.object("P1")
        .unwrap()
        .attribute("warningInformation")
        .unwrap()
        .value
}

#[test]
fn shapefile_web_mercator_point() {
    let ds = import_path_with(&data("shapefile/extracted/point.shp"), None, now()).unwrap();
    assert!(ds.imported);
    let parts: Vec<_> = ds.objects_of("NavwarnPart").collect();
    assert_eq!(parts.len(), 1);
    match parts[0].1.geometry[0] {
        Geometry::Point([lon, lat]) => {
            assert!((lon + 1.6252333333333).abs() < 1e-9, "{lon}");
            assert!((lat - 49.6729166666666).abs() < 1e-9, "{lat}");
        }
        ref g => panic!("{g:?}"),
    }
    assert!(ds.issues.iter().any(|i| i.message.contains("EPSG:3857")));
}

#[test]
fn shapefile_prj_detection() {
    let prj = std::fs::read_to_string(data("shapefile/extracted/point.prj")).unwrap();
    assert_eq!(crs_from_prj(&prj).unwrap(), ShapeCrs::WebMercator);
    let wgs = r#"GEOGCS["GCS_WGS_1984",DATUM["D_WGS_1984",SPHEROID["WGS_1984",6378137.0,298.257223563]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]]"#;
    assert_eq!(crs_from_prj(wgs).unwrap(), ShapeCrs::Wgs84);
    let utm = r#"PROJCS["WGS 84 / UTM zone 30N",GEOGCS["WGS 84"],PROJECTION["Transverse_Mercator"],AUTHORITY["EPSG","32630"]]"#;
    assert!(crs_from_prj(utm).is_err());
}

/// Build a small polygon shapefile in EPSG:4326 with a dBASE text field.
#[test]
fn shapefile_polygon_with_hole_and_dbf() {
    let outer = [
        [0.0, 0.0],
        [0.0, 10.0],
        [10.0, 10.0],
        [10.0, 0.0],
        [0.0, 0.0],
    ]; // clockwise
    let hole: [[f64; 2]; 5] = [[2.0, 2.0], [4.0, 2.0], [4.0, 4.0], [2.0, 4.0], [2.0, 2.0]]; // counter-clockwise
    let mut content = Vec::new();
    content.extend(5i32.to_le_bytes());
    content.extend([0u8; 32]);
    content.extend(2i32.to_le_bytes());
    content.extend(10i32.to_le_bytes());
    content.extend(0i32.to_le_bytes());
    content.extend(5i32.to_le_bytes());
    for p in outer.iter().chain(hole.iter()) {
        content.extend(p[0].to_le_bytes());
        content.extend(p[1].to_le_bytes());
    }
    let mut shp = Vec::new();
    shp.extend(9994i32.to_be_bytes());
    shp.extend([0u8; 20]);
    let total = 100 + 8 + content.len();
    shp.extend(((total / 2) as i32).to_be_bytes());
    shp.extend(1000i32.to_le_bytes());
    shp.extend(5i32.to_le_bytes());
    shp.extend([0u8; 64]);
    shp.extend(1i32.to_be_bytes());
    shp.extend(((content.len() / 2) as i32).to_be_bytes());
    shp.extend(&content);

    let mut dbf = vec![3u8, 126, 10, 13];
    dbf.extend(1u32.to_le_bytes());
    dbf.extend(65u16.to_le_bytes());
    dbf.extend(21u16.to_le_bytes());
    dbf.extend([0u8; 20]);
    let mut field = [0u8; 32];
    field[..4].copy_from_slice(b"DESC");
    field[11] = b'C';
    field[16] = 20;
    dbf.extend(field);
    dbf.push(0x0d);
    dbf.push(b' ');
    dbf.extend(b"Firing practice     ");
    dbf.push(0x1a);

    let companions = ShapefileCompanions {
        dbf: Some(dbf),
        prj: Some(r#"GEOGCS["WGS 84",DATUM["WGS_1984"]]"#.into()),
        cpg: None,
    };
    let ds = import_shapefile(&shp, &companions, "AVURNAV BREST-12-2026.shp", None, now()).unwrap();
    let p = ds.object("P1").unwrap();
    match &p.geometry[0] {
        Geometry::Surface {
            exterior,
            interiors,
        } => {
            assert_eq!(exterior.len(), 5);
            assert_eq!(interiors.len(), 1);
        }
        g => panic!("{g:?}"),
    }
    assert_eq!(
        p.value(&["warningInformation", "information", "text"]),
        Some("Firing practice")
    );
    let pre = ds.object("PRE").unwrap();
    assert_eq!(
        pre.value(&["messageSeriesIdentifier", "nameOfSeries"]),
        Some("AVURNAV BREST")
    );
    assert_eq!(
        pre.value(&["messageSeriesIdentifier", "warningNumber"]),
        Some("12")
    );
    assert_eq!(
        pre.value(&["messageSeriesIdentifier", "year"]),
        Some("2026")
    );
}

#[test]
fn zip_imports_the_same_shapefile_as_the_extracted_files() {
    let zipped = import_path_with(&data("shapefile/shapefiles.zip"), None, now()).unwrap();
    let plain = import_path_with(&data("shapefile/extracted/point.shp"), None, now()).unwrap();
    let geometry = |d: &ferrite_s100_gml::Dataset| {
        d.objects
            .iter()
            .flat_map(|o| o.geometry.clone())
            .collect::<Vec<_>>()
    };
    assert!(!geometry(&zipped).is_empty());
    assert_eq!(geometry(&zipped), geometry(&plain));
}

#[test]
fn source_series_parsing() {
    let s = parse_source_series("AVINAV CHERBOURG 72/26").unwrap();
    assert_eq!(
        (s.series.as_str(), s.number, s.year),
        ("AVINAV CHERBOURG", 72, 2026)
    );
    let s = parse_source_series("AVINAV CHERBOURG 72/26 #1/1").unwrap();
    assert_eq!(s.number, 72);
    let s = parse_source_series("AVINAV CHERBOURG-72-26").unwrap();
    assert_eq!(
        (s.series.as_str(), s.number, s.year),
        ("AVINAV CHERBOURG", 72, 2026)
    );
    assert!(parse_source_series("export (1)").is_none());
    assert!(parse_source_series("7a7f01d0-249c-4615-94e0-441e4d3cf559").is_none());
}

#[test]
fn detect_by_content_and_reject_unknown() {
    assert_eq!(
        ImportFormat::detect(Path::new("x.dat"), b"<?xml version=\"1.0\"?><kml>"),
        Some(ImportFormat::Kml)
    );
    assert_eq!(
        ImportFormat::detect(Path::new("x.dat"), b"<gpx version=\"1.1\">"),
        Some(ImportFormat::Gpx)
    );
    assert_eq!(ImportFormat::detect(Path::new("x.dat"), b"hello"), None);
    assert!(import_bytes(
        b"<kml xmlns=\"http://www.opengis.net/kml/2.2\"/>",
        ImportFormat::Kml,
        "e.kml",
        None,
        now()
    )
    .is_err());
    assert!(import_bytes(
        b"<!DOCTYPE x [<!ENTITY a \"b\">]><kml/>",
        ImportFormat::Kml,
        "e.kml",
        None,
        now()
    )
    .is_err());
}
