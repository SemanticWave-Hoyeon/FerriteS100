//! Root-only bounded source qualification. No DLL or GUI, no network/schema fetching.
use ferrite_s421::{
    route::{Route, Waypoint},
    s421::{self, PublishedExport},
};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output = std::path::PathBuf::from(
        std::env::args_os()
            .nth(1)
            .ok_or("Expected new output directory")?,
    );
    std::fs::create_dir(&output)?;
    let fixtures = [
        (
            "v1-GMIN",
            include_str!("../tests/fixtures/v1-GMIN.gml"),
            None,
        ),
        (
            "v1-GBASIC",
            include_str!("../tests/fixtures/v1-GBASIC.gml"),
            Some("Unresolved reference"),
        ),
        (
            "v1-GFULL",
            include_str!("../tests/fixtures/v1-GFULL.gml"),
            Some("Unresolved reference"),
        ),
        (
            "v2-GMIN",
            include_str!("../tests/fixtures/v2-GMIN.gml"),
            None,
        ),
        (
            "v2-GBASIC",
            include_str!("../tests/fixtures/v2-GBASIC.gml"),
            None,
        ),
        (
            "v2-GFULL",
            include_str!("../tests/fixtures/v2-GFULL.gml"),
            Some("Missing point CRS"),
        ),
    ];
    let mut rows = Vec::new();
    for (name, xml, expected_error) in fixtures {
        match (s421::import_dataset(xml),expected_error) {
            (Ok(parsed),None) => rows.push(serde_json::json!({"fixture":name,"outcome":"supported_subset","profile":format!("{:?}",parsed.profile),"routes":parsed.routes.len(),"waypoints":parsed.routes[0].route.waypoints.len(),"warnings":parsed.compatibility_warnings})),
            (Err(error),Some(expected)) if error.contains(expected) => rows.push(serde_json::json!({"fixture":name,"outcome":"expected_reject","error":error})),
            (result,expected) => return Err(format!("Unexpected fixture outcome {name}: {result:?}, expected {expected:?}").into()),
        }
    }
    let mut route = Route::with_name(1, "Route Alpha");
    route.add_waypoint(
        Waypoint::new(1, 25.123456789012345, 59.23456789012345).with_turn_radius(0.25),
    );
    route.add_waypoint(
        Waypoint::new(2, 25.223456789012345, 59.33456789012345).with_turn_radius(0.50),
    );
    let xml = s421::export_published(
        &route,
        PublishedExport {
            route_id: "RTE.Alpha",
            edition: 1,
            status: 1,
        },
    )?;
    let back = s421::import_dataset(&xml)?;
    for (actual, expected) in back.routes[0].route.waypoints.iter().zip(&route.waypoints) {
        assert_eq!(actual.lon.to_bits(), expected.lon.to_bits());
        assert_eq!(actual.lat.to_bits(), expected.lat.to_bits());
    }
    std::fs::write(output.join("published-export.gml"), xml)?;
    std::fs::write(
        output.join("parser-outcomes.json"),
        serde_json::to_vec_pretty(
            &serde_json::json!({"passed":true,"scope":"subset parser/roundtrip only; offline XSD is independent, CDV not published","fixtures":rows}),
        )?,
    )?;
    Ok(())
}
