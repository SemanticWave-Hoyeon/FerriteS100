//! Route overlay: the renderer extension of a route product (S-421).
//!
//! The product portrays its routes as geographic data the renderer draws
//! itself (`docs/ENGINE_DESIGN.md`, "제품 모듈화", step 3): the ordered
//! waypoints of each route ([`NativeRoutePath`]), the declared leg geometry
//! ([`NativeRouteLegDeclaration`]) and their bounded WGS84 samples
//! ([`NativeRouteWorldSamples`]). [`extension`] wraps the samples as a
//! [`RendererExtension`] of kind [`ROUTE_OVERLAY_EXTENSION`]; renderers take
//! them back with [`payload`]. The drawing resources come from the
//! product's editing portrayal catalogue through an [`EditingStyleReader`].
//!
//! This is a host route-editing illustration: it does NOT execute S-421 XSLT
//! portrayal. (Moved from `ferrite_gpu::native_route_overlay`; the screen
//! projection and GPU drawing stay in the renderer.)
use crate::RendererExtension;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

pub mod leg;
use leg::MAX_STEP_M;
pub use leg::{sample_leg, NativeRouteLegDeclaration, NativeRouteLegGeometry, MAX_LEG_VERTICES};

/// Renderer extension kind of a route overlay; the payload is
/// `Arc<NativeRouteWorldSamples>`.
pub const ROUTE_OVERLAY_EXTENSION: &str = "S-421/route-overlay";

/// The samples as a renderer extension.
pub fn extension(samples: Arc<NativeRouteWorldSamples>) -> RendererExtension {
    RendererExtension {
        kind: ROUTE_OVERLAY_EXTENSION,
        payload: samples,
    }
}

/// The samples of a route overlay extension; None for other kinds.
pub fn payload(extension: &RendererExtension) -> Option<Arc<NativeRouteWorldSamples>> {
    if extension.kind != ROUTE_OVERLAY_EXTENSION {
        return None;
    }
    extension
        .payload
        .clone()
        .downcast::<NativeRouteWorldSamples>()
        .ok()
}

/// Declared editing resources, validated by the host's S-421 reader.
#[derive(Debug, Clone)]
pub struct EditingStyle {
    pub product: String,
    pub version: String,
    /// Colour tokens the waypoint SVG uses, in ascending order.
    pub svg_color_tokens: Vec<String>,
}
/// Requested editing palette: colour token to sRGB.
pub type EditingPalette = BTreeMap<String, [u8; 3]>;
/// Host S-421 readers: the editing PC/SVG contract and the requested palette.
pub struct EditingStyleReader {
    pub contract: fn(pc: &str, waypoint_svg: &str) -> Result<EditingStyle, String>,
    pub palette: fn(xml: &str, requested: &str) -> Result<EditingPalette, String>,
}
/// Route and waypoint budgets of a route overlay.
#[doc(hidden)]
pub const MAX_ROUTES: usize = 128;
#[doc(hidden)]
pub const MAX_POINTS: usize = 16_384;

/// Host controller supplies original geographic positions, never ABI drawing colors.
#[derive(Clone, Debug)]
pub struct NativeRouteWaypoint {
    pub waypoint_id: u32,
    pub longitude: f64,
    pub latitude: f64,
}
#[derive(Clone, Debug)]
pub struct NativeRoutePath {
    pub route_id: u32,
    pub waypoints: Vec<NativeRouteWaypoint>,
}
/// Immutable numerical source cache, scoped to one native host revision.
/// 4MiB charged Vec capacities/string lengths per cache; old+next at most 8MiB
/// at publication (excluding map nodes, Arc/allocator/GPU/inflight memory).
pub struct NativeRouteWorldSamples {
    revision: u64,
    paths: Vec<NativeRoutePath>,
    declarations: Vec<NativeRouteLegDeclaration>,
    step_bits: u64,
    vertex_limit: usize,
    #[doc(hidden)]
    pub samples: std::collections::BTreeMap<(u32, u32), Vec<[f64; 2]>>,
    payload_bytes: usize,
    solver_invocations: usize,
}
#[doc(hidden)]
pub const MAX_WORLD_SAMPLE_BYTES: usize = 4 * 1024 * 1024;
impl NativeRouteWorldSamples {
    pub fn prepare(
        paths: &[NativeRoutePath],
        declarations: &[NativeRouteLegDeclaration],
        revision: u64,
    ) -> Result<Arc<Self>, String> {
        validate_paths_with_curves(paths, !declarations.is_empty())?;
        if declarations.len() > MAX_POINTS {
            return Err("Native declared leg count exceeds waypoint budget".into());
        }
        let mut samples = std::collections::BTreeMap::new();
        let mut total = 0usize;
        let mut bytes = std::mem::size_of_val(paths)
            .checked_add(std::mem::size_of_val(declarations))
            .ok_or("World sample metadata budget overflow")?;
        for path in paths {
            bytes = bytes
                .checked_add(std::mem::size_of_val(path.waypoints.as_slice()))
                .ok_or("World sample budget overflow")?;
        }
        for leg in declarations {
            if !matches!(leg.source_profile, 0..=2) {
                return Err("Unknown declared leg source profile".into());
            }
            bytes = bytes
                .checked_add(leg.source_gml_id.len())
                .and_then(|n| n.checked_add(leg.original_geometry_text.len()))
                .ok_or("World sample budget overflow")?;
            if bytes > MAX_WORLD_SAMPLE_BYTES {
                return Err("World sample metadata payload budget exceeded".into());
            }
            if leg.source_profile == 0
                && (!leg.source_gml_id.is_empty() || !leg.original_geometry_text.is_empty())
            {
                return Err("Authored draft cannot claim original GML provenance".into());
            }
            let path = paths
                .iter()
                .find(|path| path.route_id == leg.route_id)
                .ok_or("Declared leg belongs to missing route")?;
            let pair = path
                .waypoints
                .windows(2)
                .find(|pair| pair[0].waypoint_id == leg.from && pair[1].waypoint_id == leg.to)
                .ok_or("Declared leg endpoints differ from ordered route")?;
            if samples.contains_key(&(leg.route_id, leg.to)) {
                return Err("Duplicate declared leg".into());
            }
            let available = MAX_LEG_VERTICES
                .checked_sub(total)
                .ok_or("World sample vertex budget exceeded")?;
            let points = sample_leg(
                [pair[0].longitude, pair[0].latitude],
                [pair[1].longitude, pair[1].latitude],
                leg.geometry,
                available,
            )?;
            total = total
                .checked_add(points.len())
                .ok_or("World sample count overflow")?;
            bytes = bytes
                .checked_add(points.capacity() * std::mem::size_of::<[f64; 2]>())
                .ok_or("World sample byte overflow")?;
            if bytes > MAX_WORLD_SAMPLE_BYTES {
                return Err("World sample payload budget exceeded".into());
            }
            samples.insert((leg.route_id, leg.to), points);
        }
        // Explicit owned inputs; exact equality below, no digest/pointer-only admission.
        let owned_paths = paths.to_vec();
        let owned_declarations = declarations.to_vec();
        // Charge actual retained capacities after cloning; construction may have a
        // transient allocator peak, so this is not an allocation-before-cap/RSS claim.
        let mut retained = owned_paths.capacity() * std::mem::size_of::<NativeRoutePath>()
            + owned_declarations.capacity() * std::mem::size_of::<NativeRouteLegDeclaration>();
        for path in &owned_paths {
            retained = retained
                .checked_add(path.waypoints.capacity() * std::mem::size_of::<NativeRouteWaypoint>())
                .ok_or("World identity capacity overflow")?;
        }
        for leg in &owned_declarations {
            retained = retained
                .checked_add(leg.source_gml_id.capacity())
                .and_then(|n| n.checked_add(leg.original_geometry_text.capacity()))
                .ok_or("World lexical capacity overflow")?;
        }
        for points in samples.values() {
            retained = retained
                .checked_add(points.capacity() * std::mem::size_of::<[f64; 2]>())
                .ok_or("World sample capacity overflow")?;
        }
        if retained > MAX_WORLD_SAMPLE_BYTES {
            return Err("World sample retained capacity budget exceeded".into());
        }
        bytes = retained;
        Ok(Arc::new(Self {
            revision,
            paths: owned_paths,
            declarations: owned_declarations,
            step_bits: MAX_STEP_M.to_bits(),
            vertex_limit: MAX_LEG_VERTICES,
            samples,
            payload_bytes: bytes,
            solver_invocations: declarations.len(),
        }))
    }
    pub fn matches(
        &self,
        paths: &[NativeRoutePath],
        declarations: &[NativeRouteLegDeclaration],
        revision: u64,
    ) -> bool {
        self.revision == revision
            && self.step_bits == MAX_STEP_M.to_bits()
            && self.vertex_limit == MAX_LEG_VERTICES
            && self.declarations == declarations
            && self.paths.len() == paths.len()
            && self.paths.iter().zip(paths).all(|(a, b)| {
                a.route_id == b.route_id
                    && a.waypoints.len() == b.waypoints.len()
                    && a.waypoints.iter().zip(&b.waypoints).all(|(a, b)| {
                        a.waypoint_id == b.waypoint_id
                            && a.longitude.to_bits() == b.longitude.to_bits()
                            && a.latitude.to_bits() == b.latitude.to_bits()
                    })
            })
    }
    /// Route revision the samples were prepared for.
    pub fn revision(&self) -> u64 {
        self.revision
    }
    /// The routes the samples were prepared from, in order.
    pub fn paths(&self) -> &[NativeRoutePath] {
        &self.paths
    }
    /// The declared legs the samples were prepared from.
    pub fn declarations(&self) -> &[NativeRouteLegDeclaration] {
        &self.declarations
    }
    pub fn payload_bytes(&self) -> usize {
        self.payload_bytes
    }
    pub fn solver_invocations(&self) -> usize {
        self.solver_invocations
    }
}
#[cfg(test)]
fn validate_paths(paths: &[NativeRoutePath]) -> Result<(), String> {
    validate_paths_with_curves(paths, false)
}
/// Route count, identity and coordinate checks shared by the samples and the
/// renderer's screen projection.
#[doc(hidden)]
pub fn validate_paths_with_curves(
    paths: &[NativeRoutePath],
    allow_curves: bool,
) -> Result<(), String> {
    if paths.len() > MAX_ROUTES {
        return Err("Native route count exceeds 128".into());
    }
    let mut routes = BTreeSet::new();
    let mut count = 0usize;
    for path in paths {
        if path.route_id == 0 || !routes.insert(path.route_id) {
            return Err("Invalid/duplicate native route identity".into());
        }
        count = count
            .checked_add(path.waypoints.len())
            .ok_or("Native waypoint count overflow")?;
        if count > MAX_POINTS {
            return Err("Native waypoint count exceeds 16384".into());
        }
        let mut ids = BTreeSet::new();
        for (i, point) in path.waypoints.iter().enumerate() {
            if point.waypoint_id == 0
                || !ids.insert(point.waypoint_id)
                || !point.longitude.is_finite()
                || !point.latitude.is_finite()
                || !(-180.0..=180.0).contains(&point.longitude)
                || !(-90.0..=90.0).contains(&point.latitude)
            {
                return Err("Invalid native waypoint identity/coordinate".into());
            }
            // No invented cross-seam geodesic: this first host illustration uses projected straight legs.
            if !allow_curves
                && i > 0
                && (point.longitude - path.waypoints[i - 1].longitude).abs() > 180.0
            {
                return Err("Cross-antimeridian host editing legs not implemented".into());
            }
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    fn point(id: u32, x: f64, y: f64) -> NativeRouteWaypoint {
        NativeRouteWaypoint {
            waypoint_id: id,
            longitude: x,
            latitude: y,
        }
    }
    #[test]
    fn authored_curve_uses_wgs84_and_rejects_fabricated_original_provenance() {
        let paths = [NativeRoutePath {
            route_id: 7,
            waypoints: vec![point(9, -30., 70.), point(3, 30., 70.)],
        }];
        let mut legs = [NativeRouteLegDeclaration {
            route_id: 7,
            from: 9,
            to: 3,
            geometry: super::NativeRouteLegGeometry::Orthodrome,
            source_profile: 0,
            source_gml_id: String::new(),
            original_geometry_text: String::new(),
        }];
        let geodesic = NativeRouteWorldSamples::prepare(&paths, &legs, 1).unwrap();
        assert!(geodesic.samples[&(7, 3)].iter().any(|p| p[1] > 72.));
        legs[0].geometry = super::NativeRouteLegGeometry::Loxodrome;
        let rhumb = NativeRouteWorldSamples::prepare(&paths, &legs, 2).unwrap();
        assert!(rhumb.samples[&(7, 3)]
            .iter()
            .all(|p| (p[1] - 70.).abs() < 1e-10));
        assert!(!geodesic.matches(&paths, &legs, 1));
        legs[0].source_gml_id = "invented-original".into();
        assert!(NativeRouteWorldSamples::prepare(&paths, &legs, 2).is_err());
        legs[0].source_gml_id.clear();
        legs[0].original_geometry_text = "1".into();
        assert!(NativeRouteWorldSamples::prepare(&paths, &legs, 2).is_err());
        legs[0].original_geometry_text.clear();
        legs[0].source_profile = 3;
        assert!(NativeRouteWorldSamples::prepare(&paths, &legs, 2).is_err());
    }
    #[test]
    fn world_sample_metadata_cap_and_vertex_exhaustion_decline_explicitly() {
        use super::NativeRouteLegGeometry;
        let paths = [NativeRoutePath {
            route_id: 7,
            waypoints: vec![
                point(9, -80., 0.),
                point(3, 80., 0.),
                point(4, -80., 1.),
                point(5, 80., 2.),
                point(6, -80., 3.),
            ],
        }];
        let mut legs = paths[0]
            .waypoints
            .windows(2)
            .enumerate()
            .map(|(i, p)| NativeRouteLegDeclaration {
                route_id: 7,
                from: p[0].waypoint_id,
                to: p[1].waypoint_id,
                geometry: NativeRouteLegGeometry::Orthodrome,
                source_profile: 1,
                source_gml_id: format!("LEG.{i}"),
                original_geometry_text: "2".into(),
            })
            .collect::<Vec<_>>();
        assert!(NativeRouteWorldSamples::prepare(&paths, &legs, 11).is_err());
        legs.truncate(1);
        legs[0].source_gml_id = "x".repeat(MAX_WORLD_SAMPLE_BYTES + 1);
        assert!(NativeRouteWorldSamples::prepare(&paths, &legs, 11).is_err());
    }
    #[test]
    fn original_order_and_coincident_points_valid() {
        let p = vec![NativeRoutePath {
            route_id: 7,
            waypoints: vec![point(9, 2., 50.), point(3, 2., 50.)],
        }];
        assert!(validate_paths(&p).is_ok());
        assert_eq!(p[0].waypoints[1].waypoint_id, 3);
    }
    #[test]
    fn coordinate_and_identity_rejections_precede_projection() {
        for p in [
            point(0, 2., 50.),
            point(1, f64::NAN, 50.),
            point(1, 2., 90.1),
        ] {
            assert!(validate_paths(&[NativeRoutePath {
                route_id: 1,
                waypoints: vec![p]
            }])
            .is_err());
        }
        assert!(validate_paths(&[NativeRoutePath {
            route_id: 1,
            waypoints: vec![point(1, 179., 50.), point(2, -179., 50.)]
        }])
        .is_err());
    }
    #[test]
    fn bounded_count_and_duplicate_ids_reject() {
        assert!(validate_paths(&vec![
            NativeRoutePath {
                route_id: 1,
                waypoints: vec![]
            };
            129
        ])
        .is_err());
        assert!(validate_paths(&[NativeRoutePath {
            route_id: 1,
            waypoints: vec![point(1, 0., 0.), point(1, 1., 1.)]
        }])
        .is_err());
    }
}
