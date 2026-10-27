//! Routes as the route overlay renderer extension
//! (`ferrite_product_api::route_overlay`): ordered waypoints and the declared
//! legs whose WGS84 samples the renderer draws. Moved from the viewer's
//! native route host (`src/native_s421.rs`, `prepare_overlay`), shared by
//! the product layer and the route editor.
use crate::route::Route;
use crate::s421::{ImportedDataset, ImportedRoute, LegGeometry, Profile};
use ferrite_product_api::route_overlay::{
    NativeRouteLegDeclaration, NativeRouteLegGeometry, NativeRoutePath, NativeRouteWaypoint,
};

/// Declared leg geometry as the renderer samples it.
pub fn leg_geometry(geometry: LegGeometry) -> NativeRouteLegGeometry {
    match geometry {
        LegGeometry::Loxodrome => NativeRouteLegGeometry::Loxodrome,
        LegGeometry::Orthodrome => NativeRouteLegGeometry::Orthodrome,
    }
}

/// Declared provenance of an imported leg: 1 published 1.0, 2 CDV 2.0
/// (0 is a locally authored draft, never a producer/FC profile).
pub fn source_profile(profile: Profile) -> u8 {
    match profile {
        Profile::Published1 => 1,
        Profile::Candidate2 => 2,
    }
}

/// A route's waypoints with their original positions.
pub fn path(route: &Route) -> NativeRoutePath {
    NativeRoutePath {
        route_id: route.id,
        waypoints: route
            .waypoints
            .iter()
            .map(|point| NativeRouteWaypoint {
                waypoint_id: point.id,
                longitude: point.lon,
                latitude: point.lat,
            })
            .collect(),
    }
}

/// The declared legs of an imported route drawn as `route`. Imported CRS is
/// explicitly checked EPSG:4326 by the core parser. Native edits cannot
/// borrow a declaration for changed original coordinates.
pub fn imported_route_legs(
    route: &Route,
    profile: Profile,
    imported: &ImportedRoute,
    declarations: &mut Vec<NativeRouteLegDeclaration>,
) -> Result<(), String> {
    if !imported.legs.is_empty()
        && (route.waypoints.len() != imported.route.waypoints.len()
            || route
                .waypoints
                .iter()
                .zip(&imported.route.waypoints)
                .any(|(a, b)| {
                    a.id != b.id
                        || a.lon.to_bits() != b.lon.to_bits()
                        || a.lat.to_bits() != b.lat.to_bits()
                }))
    {
        return Err("Edited route cannot reuse original declared leg endpoints".into());
    }
    for leg in &imported.legs {
        declarations.push(NativeRouteLegDeclaration {
            route_id: route.id,
            from: leg.from_waypoint_id,
            to: leg.to_waypoint_id,
            source_profile: source_profile(profile),
            source_gml_id: leg.gml_id.clone(),
            original_geometry_text: leg.original_geometry_text.clone(),
            geometry: leg_geometry(leg.geometry),
        });
    }
    Ok(())
}

/// The legs a locally authored route declares (profile 0, no original
/// identity or lexical value).
pub fn authored_route_legs(route: &Route, declarations: &mut Vec<NativeRouteLegDeclaration>) {
    for pair in route.waypoints.windows(2) {
        if let Some(geometry) = pair[1].incoming_geometry {
            declarations.push(NativeRouteLegDeclaration {
                route_id: route.id,
                from: pair[0].id,
                to: pair[1].id,
                geometry: leg_geometry(geometry),
                source_profile: 0,
                source_gml_id: String::new(),
                original_geometry_text: String::new(),
            });
        }
    }
}

/// The overlay inputs of the routes: their paths, then the declared legs of
/// the imported routes (`source` gives a route's retained dataset; every
/// imported route of it is checked against the drawn route), then those of
/// the locally authored ones.
pub fn routes<'a>(
    routes: &[Route],
    source: impl Fn(u32) -> Option<&'a ImportedDataset>,
) -> Result<(Vec<NativeRoutePath>, Vec<NativeRouteLegDeclaration>), String> {
    let paths = routes.iter().map(path).collect::<Vec<_>>();
    let mut declarations = Vec::new();
    for route in routes {
        if let Some(dataset) = source(route.id) {
            for imported in &dataset.routes {
                imported_route_legs(route, dataset.profile, imported, &mut declarations)?;
            }
        }
    }
    for route in routes.iter().filter(|route| source(route.id).is_none()) {
        authored_route_legs(route, &mut declarations);
    }
    Ok((paths, declarations))
}
