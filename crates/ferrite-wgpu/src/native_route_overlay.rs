//! Renderer preparation boundary for a host route-editing overlay.
//! This does NOT execute S421 XSLT, create S101 instructions, or claim product portrayal.
use crate::native_route_leg::{sample_leg, MAX_LEG_VERTICES, MAX_STEP_M};
use crate::{NativeRouteLegDeclaration, SymbolCache, SymbolGeometry};
use ferrite_portrayal_catalog::{
    parse_standalone_line_style, CatalogueSources, ColorDefinition, ColorProfile, LineStyle,
    SimpleLineStyle, SrgbColor,
};
use ferrite_render::{Scaler, ScreenPoint, WorldPoint};
use ferrite_s421::editing_style_contract::{read_editing_palette, EditingStyleContract};
use std::{collections::BTreeSet, path::Path, sync::Arc};

pub const NATIVE_S421_PROVIDER: &str = "native:s421-route";
const MAX_ROUTES: usize = 128;
const MAX_POINTS: usize = 16_384;
const MAX_SYMBOL_PAYLOAD: usize = 32 * 1024 * 1024;

/// Immutable, independent PC owner. No shared ENC palette/cache is accepted.
pub struct NativeRouteOverlayResources {
    sources: Arc<CatalogueSources>,
    contract: EditingStyleContract,
    profile: ColorProfile,
    waypoint: Arc<SymbolGeometry>,
    leg: SimpleLineStyle,
    leg_rgba: [u8; 4],
}
impl NativeRouteOverlayResources {
    pub fn load(pc_root: &Path, requested_palette: &str) -> Result<Arc<Self>, String> {
        // Legacy public editing adjuncts do not declare modern display-plane
        // metadata. Capture original resources without fabricating that metadata
        // or treating this host illustration as a full product portrayal.
        let sources = CatalogueSources::capture(pc_root).map_err(|e| e.to_string())?;
        let pc = sources
            .read_relative(Path::new("portrayal_catalogue.xml"))
            .map_err(|e| e.to_string())?;
        let svg = sources
            .read_relative(Path::new("Symbols/RTEWPT01.svg"))
            .map_err(|e| e.to_string())?;
        let contract = EditingStyleContract::from_declared_pc(
            std::str::from_utf8(&pc).map_err(|e| e.to_string())?,
            std::str::from_utf8(&svg).map_err(|e| e.to_string())?,
        )?;
        // Only the declared COLOR01 file can supply this palette, not another PC file.
        let color_bytes = sources
            .read_relative(Path::new("ColorProfiles/colorProfile.xml"))
            .map_err(|e| e.to_string())?;
        let colors = read_editing_palette(
            std::str::from_utf8(&color_bytes).map_err(|e| e.to_string())?,
            requested_palette,
        )?;
        let mut profile =
            ColorProfile::new(requested_palette.to_owned(), requested_palette.to_owned());
        for (token, [r, g, b]) in colors {
            profile.colors.insert(
                token.clone(),
                ColorDefinition {
                    token,
                    srgb: Some(SrgbColor::new(r, g, b)),
                    cie: None,
                },
            );
        }
        for token in contract.svg_color_tokens() {
            if profile.get_srgb(token).is_none() {
                return Err(format!("S421 palette lacks SVG token {token}"));
            }
        }
        let leg_bytes = sources
            .read_relative(Path::new("LineStyles/RTEACTLEGLINE.xml"))
            .map_err(|e| e.to_string())?;
        let leg = match parse_standalone_line_style(&leg_bytes, "RTEACTLEGLINE")
            .map_err(|e| e.to_string())?
        {
            LineStyle::Simple(style) => style,
            _ => return Err("S421 editing requires declared simple RTEACTLEGLINE".into()),
        };
        validate_leg(&leg)?;
        let color = profile
            .get_srgb(&leg.pen.color_token)
            .ok_or("S421 palette lacks leg pen token")?;
        let mut cache = SymbolCache::new_with_sources(pc_root.join("Symbols"), sources.clone());
        let waypoint = cache
            .get_symbol("RTEWPT01", &profile)
            .ok_or("S421 declared waypoint SVG rasterization failed")?
            .clone();
        let expected = (waypoint.width as usize)
            .checked_mul(waypoint.height as usize)
            .and_then(|n| n.checked_mul(4))
            .ok_or("S421 symbol payload overflow")?;
        if expected == 0 || expected > MAX_SYMBOL_PAYLOAD || expected != waypoint.pixels.len() {
            return Err(
                "S421 symbol RGBA payload exceeds receiver budget or is inconsistent".into(),
            );
        }
        Ok(Arc::new(Self {
            sources,
            contract,
            profile,
            waypoint: Arc::new(waypoint),
            leg,
            leg_rgba: [color.r, color.g, color.b, 255],
        }))
    }
    pub fn source_digest(&self) -> &[u8; 32] {
        self.sources.digest()
    }
    pub fn actual_product(&self) -> &str {
        self.contract.product()
    }
    pub fn actual_version(&self) -> &str {
        self.contract.version()
    }
    pub fn palette(&self) -> &str {
        &self.profile.name
    }
    pub fn waypoint_symbol(&self) -> &SymbolGeometry {
        &self.waypoint
    }
    pub fn leg_style(&self) -> &SimpleLineStyle {
        &self.leg
    }
    pub fn leg_rgba(&self) -> [u8; 4] {
        self.leg_rgba
    }
}
fn validate_leg(style: &SimpleLineStyle) -> Result<(), String> {
    if !style.pen.width.is_finite()
        || style.pen.width <= 0.0
        || !style.offset_mm.is_finite()
        || style.offset_mm != 0.0
        || !style.symbols.is_empty()
    {
        return Err("Unsupported editing line width/offset/line-symbol capability".into());
    }
    style.dash_cycle().map_err(str::to_owned)?;
    Ok(())
}

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
#[derive(Debug)]
pub struct PreparedNativeRoutePath {
    pub route_id: u32,
    pub waypoints: Vec<(u32, ScreenPoint)>,
    /// Includes interior curve vertices; each segment retains original endpoint identity.
    pub line_points: Vec<ScreenPoint>,
    pub segment_owners: Vec<(u32, u32)>,
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
    samples: std::collections::BTreeMap<(u32, u32), Vec<[f64; 2]>>,
    payload_bytes: usize,
    solver_invocations: usize,
}
const MAX_WORLD_SAMPLE_BYTES: usize = 4 * 1024 * 1024;
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
    pub fn payload_bytes(&self) -> usize {
        self.payload_bytes
    }
    pub fn solver_invocations(&self) -> usize {
        self.solver_invocations
    }
}
/// Owned candidate; no writes to renderer resources or the current chart context.
/// A GPU uploader must retain this owner and keep route picking in its own namespace.
pub struct PreparedNativeRouteOverlay {
    resources: Arc<NativeRouteOverlayResources>,
    route_revision: u64,
    view: [u64; 16],
    pixels_per_mm: u64,
    paths: Vec<PreparedNativeRoutePath>,
}
impl PreparedNativeRouteOverlay {
    pub fn prepare(
        resources: Arc<NativeRouteOverlayResources>,
        route_revision: u64,
        paths: &[NativeRoutePath],
        scaler: &Scaler,
        pixels_per_mm: f64,
    ) -> Result<Self, String> {
        Self::prepare_with_leg_semantics(
            resources,
            route_revision,
            paths,
            &[],
            scaler,
            pixels_per_mm,
        )
    }
    pub fn prepare_with_leg_semantics(
        resources: Arc<NativeRouteOverlayResources>,
        route_revision: u64,
        paths: &[NativeRoutePath],
        declarations: &[NativeRouteLegDeclaration],
        scaler: &Scaler,
        pixels_per_mm: f64,
    ) -> Result<Self, String> {
        let samples = NativeRouteWorldSamples::prepare(paths, declarations, route_revision)?;
        Self::prepare_with_world_samples(
            resources,
            route_revision,
            paths,
            declarations,
            &samples,
            scaler,
            pixels_per_mm,
        )
    }
    pub fn prepare_with_world_samples(
        resources: Arc<NativeRouteOverlayResources>,
        route_revision: u64,
        paths: &[NativeRoutePath],
        declarations: &[NativeRouteLegDeclaration],
        samples: &Arc<NativeRouteWorldSamples>,
        scaler: &Scaler,
        pixels_per_mm: f64,
    ) -> Result<Self, String> {
        if !samples.matches(paths, declarations, route_revision) {
            return Err("Stale native world sample source/revision/policy".into());
        }
        validate_paths_with_curves(paths, !declarations.is_empty())?;
        if declarations.len() > MAX_POINTS {
            return Err("Native declared leg count exceeds waypoint budget".into());
        }
        let mut declared = std::collections::BTreeMap::new();
        for leg in declarations {
            let path = paths
                .iter()
                .find(|p| p.route_id == leg.route_id)
                .ok_or("Declared leg belongs to missing route")?;
            if !path
                .waypoints
                .windows(2)
                .any(|p| p[0].waypoint_id == leg.from && p[1].waypoint_id == leg.to)
                || declared.insert((leg.route_id, leg.to), leg).is_some()
            {
                return Err("Declared leg endpoints differ from ordered route or duplicate".into());
            }
        }
        if !pixels_per_mm.is_finite() || pixels_per_mm <= 0.0 {
            return Err("Invalid physical display calibration".into());
        }
        if !scaler.viewport.width.is_finite()
            || !scaler.viewport.height.is_finite()
            || scaler.viewport.width <= 0.0
            || scaler.viewport.height <= 0.0
        {
            return Err("Invalid native editing viewport".into());
        }
        let view = scaler
            .flat_encoded_identity()
            .ok_or("Unsupported native editing camera")?;
        let mut prepared = Vec::with_capacity(paths.len());
        let mut total_line_vertices = 0usize;
        for path in paths {
            let mut points = Vec::with_capacity(path.waypoints.len());
            for point in &path.waypoints {
                // Original Scaler projection and original f32 screen conversion, without quantization.
                let screen =
                    scaler.world_to_screen(WorldPoint::new(point.longitude, point.latitude));
                if !screen.x.is_finite() || !screen.y.is_finite() {
                    return Err("S421 waypoint has no finite screen projection".into());
                }
                points.push((point.waypoint_id, screen));
            }
            let mut line_points = Vec::new();
            let mut segment_owners = Vec::new();
            let mut drawing_longitude = path.waypoints.first().map_or(0., |p| p.longitude);
            if !declared.keys().any(|(route, _)| *route == path.route_id) {
                total_line_vertices = total_line_vertices
                    .checked_add(points.len())
                    .ok_or("Curve count overflow")?;
                if total_line_vertices > MAX_LEG_VERTICES {
                    return Err("Native curve total vertex budget exceeded".into());
                }
            } else {
                // The camera can display either longitude sheet at the seam.
                // Lift only the transient drawing copy; world samples and source
                // coordinates stay camera-independent and retain their identity.
                let camera_longitude = scaler.geo_bounds.center().x;
                let route_lift = ((camera_longitude - drawing_longitude) / 360.).round() * 360.;
                drawing_longitude += route_lift;
                if let (Some(first), Some(original)) = (points.first_mut(), path.waypoints.first())
                {
                    first.1 = scaler
                        .world_to_screen(WorldPoint::new(drawing_longitude, original.latitude));
                }
                for (index, pair) in path.waypoints.windows(2).enumerate() {
                    let from = &pair[0];
                    let to = &pair[1];
                    let geographic: std::borrow::Cow<'_, [[f64; 2]]> = if declared
                        .contains_key(&(path.route_id, to.waypoint_id))
                    {
                        std::borrow::Cow::Borrowed(
                            samples
                                .samples
                                .get(&(path.route_id, to.waypoint_id))
                                .ok_or("Missing declared world samples")?
                                .as_slice(),
                        )
                    } else {
                        if (to.longitude - from.longitude).abs() > 180. {
                            return Err("Undeclared cross-antimeridian host leg unsupported".into());
                        }
                        std::borrow::Cow::Owned(vec![
                            [from.longitude, from.latitude],
                            [to.longitude, to.latitude],
                        ])
                    };
                    // Keep adjacent legs on one drawing sheet, without changing source coordinates.
                    let mut lift = route_lift;
                    if index > 0 {
                        let prior = path.waypoints[index].longitude;
                        let desired = drawing_longitude;
                        lift = desired - prior;
                    }
                    for (i, position) in geographic.iter().enumerate() {
                        let longitude = position[0] + lift;
                        let screen =
                            scaler.world_to_screen(WorldPoint::new(longitude, position[1]));
                        if !screen.x.is_finite() || !screen.y.is_finite() {
                            return Err("S421 leg has no finite screen projection".into());
                        }
                        if i == 0 && index == 0 {
                            line_points.push(screen);
                            total_line_vertices = total_line_vertices
                                .checked_add(1)
                                .ok_or("Curve count overflow")?;
                        } else if i > 0 {
                            line_points.push(screen);
                            segment_owners.push((from.waypoint_id, to.waypoint_id));
                            total_line_vertices = total_line_vertices
                                .checked_add(1)
                                .ok_or("Curve count overflow")?;
                        }
                        if total_line_vertices > MAX_LEG_VERTICES {
                            return Err("Native curve total vertex budget exceeded".into());
                        }
                        if i + 1 == geographic.len() {
                            drawing_longitude = longitude;
                        }
                    }
                    // Waypoint sprites use the corresponding drawing copy, never a fabricated interior point.
                    points[index + 1].1 =
                        *line_points.last().ok_or("Missing prepared leg endpoint")?;
                }
            }
            prepared.push(PreparedNativeRoutePath {
                route_id: path.route_id,
                waypoints: points,
                line_points,
                segment_owners,
            });
        }
        Ok(Self {
            resources,
            route_revision,
            view,
            pixels_per_mm: pixels_per_mm.to_bits(),
            paths: prepared,
        })
    }
    /// Validate immediately before an infallible renderer swap; source owner uses Arc identity.
    pub fn validate(
        &self,
        resources: &Arc<NativeRouteOverlayResources>,
        revision: u64,
        scaler: &Scaler,
        pixels_per_mm: f64,
    ) -> Result<(), String> {
        if !Arc::ptr_eq(resources, &self.resources)
            || revision != self.route_revision
            || scaler.flat_encoded_identity() != Some(self.view)
            || pixels_per_mm.to_bits() != self.pixels_per_mm
        {
            return Err("Stale S421 native overlay owner/route/camera/calibration".into());
        }
        Ok(())
    }
    pub fn resource_owner(&self) -> &Arc<NativeRouteOverlayResources> {
        &self.resources
    }
    pub fn camera_identity(&self) -> [u64; 16] {
        self.view
    }
    pub fn matches_camera(&self, scaler: &Scaler) -> bool {
        scaler.flat_encoded_identity() == Some(self.view)
            && scaler.pixels_per_mm().to_bits() == self.pixels_per_mm
    }
    pub fn paths(&self) -> &[PreparedNativeRoutePath] {
        &self.paths
    }
    pub fn resources(&self) -> &NativeRouteOverlayResources {
        &self.resources
    }
    pub fn route_revision(&self) -> u64 {
        self.route_revision
    }
    pub fn pixels_per_mm(&self) -> f64 {
        f64::from_bits(self.pixels_per_mm)
    }
}
#[cfg(test)]
fn validate_paths(paths: &[NativeRoutePath]) -> Result<(), String> {
    validate_paths_with_curves(paths, false)
}
fn validate_paths_with_curves(paths: &[NativeRoutePath], allow_curves: bool) -> Result<(), String> {
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
    struct Fixture(std::path::PathBuf);
    impl Fixture {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "ferrite-native-own-pc-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&root).unwrap();
            for directory in ["Symbols", "LineStyles", "ColorProfiles"] {
                std::fs::create_dir(root.join(directory)).unwrap();
            }
            // Original public catalogue and resource bytes, no invented plane
            // metadata. Only the three declared resources consumed here exist.
            std::fs::write(
                root.join("portrayal_catalogue.xml"),
                include_bytes!("../../ferrite-s421/tests/fixtures/editing/portrayal_catalogue.xml"),
            )
            .unwrap();
            for (file, bytes) in [
                (
                    "Symbols/RTEWPT01.svg",
                    include_bytes!("../../ferrite-s421/tests/fixtures/editing/RTEWPT01.svg")
                        .as_slice(),
                ),
                (
                    "LineStyles/RTEACTLEGLINE.xml",
                    include_bytes!("../../ferrite-s421/tests/fixtures/editing/RTEACTLEGLINE.xml")
                        .as_slice(),
                ),
                (
                    "ColorProfiles/colorProfile.xml",
                    include_bytes!("../../ferrite-s421/tests/fixtures/editing/colorProfile.xml")
                        .as_slice(),
                ),
            ] {
                std::fs::write(root.join(file), bytes).unwrap();
            }
            Self(root)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
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
            geometry: crate::NativeRouteLegGeometry::Orthodrome,
            source_profile: 0,
            source_gml_id: String::new(),
            original_geometry_text: String::new(),
        }];
        let geodesic = NativeRouteWorldSamples::prepare(&paths, &legs, 1).unwrap();
        assert!(geodesic.samples[&(7, 3)].iter().any(|p| p[1] > 72.));
        legs[0].geometry = crate::NativeRouteLegGeometry::Loxodrome;
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
    fn immutable_world_samples_match_original_solver_and_do_not_recompute_for_cameras() {
        use crate::NativeRouteLegGeometry;
        use ferrite_render::{GeoBounds, Viewport};
        let fixture = Fixture::new();
        let resources = NativeRouteOverlayResources::load(&fixture.0, "Day").unwrap();
        let paths = [NativeRoutePath {
            route_id: 7,
            waypoints: vec![point(9, 1., 70.), point(3, 3., 70.)],
        }];
        let legs = [NativeRouteLegDeclaration {
            route_id: 7,
            from: 9,
            to: 3,
            geometry: NativeRouteLegGeometry::Orthodrome,
            source_profile: 1,
            source_gml_id: "LEG.1".into(),
            original_geometry_text: "2".into(),
        }];
        let world = NativeRouteWorldSamples::prepare(&paths, &legs, 11).unwrap();
        let legacy = sample_leg([1., 70.], [3., 70.], legs[0].geometry, MAX_LEG_VERTICES).unwrap();
        let actual = world.samples.get(&(7, 3)).unwrap();
        assert_eq!(actual.len(), legacy.len());
        assert!(actual
            .iter()
            .zip(&legacy)
            .all(|(a, b)| a[0].to_bits() == b[0].to_bits() && a[1].to_bits() == b[1].to_bits()));
        assert_eq!(world.solver_invocations(), 1);
        let calls = crate::native_route_leg::solver_call_count();
        for (width, x) in [(800., 0.), (1000., 0.1)] {
            let scaler = Scaler::new(
                GeoBounds::new(x, 68., 4. + x, 72.),
                Viewport::new(width, 600.),
            );
            let packet = PreparedNativeRouteOverlay::prepare_with_world_samples(
                resources.clone(),
                11,
                &paths,
                &legs,
                &world,
                &scaler,
                scaler.pixels_per_mm(),
            )
            .unwrap();
            packet
                .validate(&resources, 11, &scaler, scaler.pixels_per_mm())
                .unwrap();
        }
        assert_eq!(crate::native_route_leg::solver_call_count(), calls);
        assert!(world.payload_bytes() <= MAX_WORLD_SAMPLE_BYTES);
        let mut altered = paths.clone();
        altered[0].waypoints[1].latitude = 70.00000000000001;
        assert!(!world.matches(&altered, &legs, 11));
        assert!(!world.matches(&paths, &legs, 12));
        let mut profile = legs.clone();
        profile[0].source_profile = 2;
        assert!(!world.matches(&paths, &profile, 11));
        let mut lexical = legs.clone();
        lexical[0].original_geometry_text = "02".into();
        assert!(!world.matches(&paths, &lexical, 11));
    }
    #[test]
    fn world_sample_metadata_cap_and_vertex_exhaustion_decline_explicitly() {
        use crate::NativeRouteLegGeometry;
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
    fn semantic_packet_has_curve_vertices_original_pick_owners_and_stale_camera_guard() {
        use crate::NativeRouteLegGeometry;
        use ferrite_render::{GeoBounds, Viewport};
        let fixture = Fixture::new();
        let resources = NativeRouteOverlayResources::load(&fixture.0, "Day").unwrap();
        let scaler = Scaler::new(
            GeoBounds::new(-35., 65., 35., 77.),
            Viewport::new(800., 600.),
        );
        let paths = [NativeRoutePath {
            route_id: 7,
            waypoints: vec![point(9, -30., 70.), point(3, 30., 70.)],
        }];
        let leg = NativeRouteLegDeclaration {
            route_id: 7,
            from: 9,
            to: 3,
            geometry: NativeRouteLegGeometry::Orthodrome,
            source_profile: 1,
            source_gml_id: "LEG.1".into(),
            original_geometry_text: "2".into(),
        };
        let packet = PreparedNativeRouteOverlay::prepare_with_leg_semantics(
            resources.clone(),
            11,
            &paths,
            &[leg.clone()],
            &scaler,
            scaler.pixels_per_mm(),
        )
        .unwrap();
        let path = &packet.paths()[0];
        assert!(path.line_points.len() > 2);
        assert_eq!(path.segment_owners.len() + 1, path.line_points.len());
        assert!(path.segment_owners.iter().all(|owner| *owner == (9, 3)));
        assert_eq!(path.waypoints.len(), 2);
        assert!(path.line_points[path.line_points.len() / 2].y < path.waypoints[0].1.y);
        let other = Scaler::new(
            GeoBounds::new(-34., 65., 36., 77.),
            Viewport::new(800., 600.),
        );
        assert!(packet
            .validate(&resources, 11, &other, other.pixels_per_mm())
            .is_err());
        let reversed = NativeRouteLegDeclaration {
            from: 3,
            to: 9,
            ..leg
        };
        assert!(PreparedNativeRouteOverlay::prepare_with_leg_semantics(
            resources,
            11,
            &paths,
            &[reversed],
            &scaler,
            scaler.pixels_per_mm()
        )
        .is_err());
    }
    #[test]
    fn declared_seam_packet_uses_continuous_copy_and_legacy_still_declines() {
        use ferrite_render::{GeoBounds, Viewport};
        let fixture = Fixture::new();
        let resources = NativeRouteOverlayResources::load(&fixture.0, "Day").unwrap();
        let scaler = Scaler::new(
            GeoBounds::new(178., 58., 182., 62.),
            Viewport::new(800., 600.),
        );
        let paths = [NativeRoutePath {
            route_id: 7,
            waypoints: vec![point(9, 179., 60.), point(3, -179., 60.)],
        }];
        assert!(PreparedNativeRouteOverlay::prepare(
            resources.clone(),
            11,
            &paths,
            &scaler,
            scaler.pixels_per_mm()
        )
        .is_err());
        let legs = [NativeRouteLegDeclaration {
            route_id: 7,
            from: 9,
            to: 3,
            geometry: crate::NativeRouteLegGeometry::Loxodrome,
            source_profile: 1,
            source_gml_id: "LEG.1".into(),
            original_geometry_text: "1".into(),
        }];
        let packet = PreparedNativeRouteOverlay::prepare_with_leg_semantics(
            resources.clone(),
            11,
            &paths,
            &legs,
            &scaler,
            scaler.pixels_per_mm(),
        )
        .unwrap();
        let expected = scaler.world_to_screen(WorldPoint::new(181., 60.));
        assert_eq!(
            packet.paths()[0].waypoints[1].1.x.to_bits(),
            expected.x.to_bits()
        );
        assert_eq!(
            paths[0].waypoints[1].longitude.to_bits(),
            (-179_f64).to_bits()
        );
        let opposite = Scaler::new(
            GeoBounds::new(-182., 58., -178., 62.),
            Viewport::new(800., 600.),
        );
        let world = NativeRouteWorldSamples::prepare(&paths, &legs, 11).unwrap();
        let opposite_packet = PreparedNativeRouteOverlay::prepare_with_world_samples(
            resources,
            11,
            &paths,
            &legs,
            &world,
            &opposite,
            opposite.pixels_per_mm(),
        )
        .unwrap();
        let opposite_path = &opposite_packet.paths()[0];
        for (i, longitude) in [-181., -179.].into_iter().enumerate() {
            assert_eq!(
                opposite_path.waypoints[i].1,
                opposite.world_to_screen(WorldPoint::new(longitude, 60.))
            );
        }
        assert!(opposite_path
            .line_points
            .iter()
            .all(|p| p.x >= 0. && p.x <= 800.));
        assert!(world.matches(&paths, &legs, 11));
    }
    #[test]
    fn own_public_assets_prepare_exact_camera_and_refuse_stale_candidates() {
        use ferrite_render::{GeoBounds, Viewport};
        let fixture = Fixture::new();
        let resources = NativeRouteOverlayResources::load(&fixture.0, "Day").unwrap();
        assert_eq!(resources.actual_version(), "");
        assert_eq!(resources.palette(), "Day");
        assert_eq!(resources.leg_style().pen.width, 0.64);
        assert_eq!(resources.leg_style().dashes[0].start, 2.2);
        assert!(resources.waypoint_symbol().pixels.iter().any(|v| *v != 0));
        assert!(NativeRouteOverlayResources::load(&fixture.0, "Night").is_err());
        let scaler = Scaler::new(GeoBounds::new(-1., 49., 3., 52.), Viewport::new(800., 600.));
        let paths = [NativeRoutePath {
            route_id: 7,
            waypoints: vec![point(9, 2., 50.), point(3, 1., 51.)],
        }];
        let packet = PreparedNativeRouteOverlay::prepare(
            resources.clone(),
            11,
            &paths,
            &scaler,
            scaler.pixels_per_mm(),
        )
        .unwrap();
        for ((id, screen), original) in packet.paths()[0].waypoints.iter().zip(&paths[0].waypoints)
        {
            let expected =
                scaler.world_to_screen(WorldPoint::new(original.longitude, original.latitude));
            assert_eq!(*id, original.waypoint_id);
            assert_eq!(
                [screen.x.to_bits(), screen.y.to_bits()],
                [expected.x.to_bits(), expected.y.to_bits()]
            );
        }
        packet
            .validate(&resources, 11, &scaler, scaler.pixels_per_mm())
            .unwrap();
        assert!(packet
            .validate(&resources, 12, &scaler, scaler.pixels_per_mm())
            .is_err());
        assert!(packet
            .validate(&resources, 11, &scaler, scaler.pixels_per_mm() * 2.)
            .is_err());
        let other_owner = NativeRouteOverlayResources::load(&fixture.0, "Day").unwrap();
        assert!(packet
            .validate(&other_owner, 11, &scaler, scaler.pixels_per_mm())
            .is_err());
        let invalid_view = Scaler::new(GeoBounds::new(-1., 49., 3., 52.), Viewport::new(0., 600.));
        assert!(PreparedNativeRouteOverlay::prepare(
            resources.clone(),
            11,
            &paths,
            &invalid_view,
            scaler.pixels_per_mm()
        )
        .is_err());
        let mut changed = scaler.clone();
        changed.pan(1., 0.);
        assert!(packet
            .validate(&resources, 11, &changed, changed.pixels_per_mm())
            .is_err());
        // Filesystem mutations cannot change resources already captured by owner.
        let digest = *resources.source_digest();
        std::fs::write(fixture.0.join("Symbols/RTEWPT01.svg"), b"broken").unwrap();
        assert_eq!(*resources.source_digest(), digest);
        assert!(NativeRouteOverlayResources::load(&fixture.0, "Day").is_err());
        packet
            .validate(&resources, 11, &scaler, scaler.pixels_per_mm())
            .unwrap();
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
