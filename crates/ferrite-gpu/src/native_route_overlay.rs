//! Renderer preparation boundary for a host route-editing overlay.
//! This does NOT execute S421 XSLT, create S101 instructions, or claim product portrayal.
//!
//! The route data model (waypoints, declared legs, WGS84 world samples) is the
//! route overlay renderer extension of `ferrite_product_api::route_overlay`;
//! this module owns the drawing resources and the screen projection.
use crate::{SymbolCache, SymbolGeometry};
use ferrite_portrayal_catalog::{
    parse_standalone_line_style, CatalogueSources, ColorDefinition, ColorProfile, LineStyle,
    SimpleLineStyle, SrgbColor,
};
use ferrite_product_api::route_overlay::{
    payload, validate_paths_with_curves, EditingStyle, EditingStyleReader,
    NativeRouteLegDeclaration, NativeRoutePath, NativeRouteWorldSamples, MAX_LEG_VERTICES,
    MAX_POINTS,
};
use ferrite_product_api::RendererExtension;
use ferrite_render::{Scaler, ScreenPoint, WorldPoint};
use std::{path::Path, sync::Arc};

const MAX_SYMBOL_PAYLOAD: usize = 32 * 1024 * 1024;

/// Immutable, independent PC owner. No shared ENC palette/cache is accepted.
pub struct NativeRouteOverlayResources {
    sources: Arc<CatalogueSources>,
    contract: EditingStyle,
    profile: ColorProfile,
    waypoint: Arc<SymbolGeometry>,
    leg: SimpleLineStyle,
    leg_rgba: [u8; 4],
}
impl NativeRouteOverlayResources {
    pub fn load(
        pc_root: &Path,
        requested_palette: &str,
        reader: &EditingStyleReader,
    ) -> Result<Arc<Self>, String> {
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
        let contract = (reader.contract)(
            std::str::from_utf8(&pc).map_err(|e| e.to_string())?,
            std::str::from_utf8(&svg).map_err(|e| e.to_string())?,
        )?;
        // Only the declared COLOR01 file can supply this palette, not another PC file.
        let color_bytes = sources
            .read_relative(Path::new("ColorProfiles/colorProfile.xml"))
            .map_err(|e| e.to_string())?;
        let colors = (reader.palette)(
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
        for token in &contract.svg_color_tokens {
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
        &self.contract.product
    }
    pub fn actual_version(&self) -> &str {
        &self.contract.version
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

#[derive(Debug)]
pub struct PreparedNativeRoutePath {
    pub route_id: u32,
    pub waypoints: Vec<(u32, ScreenPoint)>,
    /// Includes interior curve vertices; each segment retains original endpoint identity.
    pub line_points: Vec<ScreenPoint>,
    pub segment_owners: Vec<(u32, u32)>,
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
    /// Project a route overlay renderer extension
    /// (`ferrite_product_api::route_overlay::ROUTE_OVERLAY_EXTENSION`) for this
    /// camera: the extension's world samples with their own routes,
    /// declarations and revision.
    pub fn from_extension(
        resources: Arc<NativeRouteOverlayResources>,
        extension: &RendererExtension,
        scaler: &Scaler,
        pixels_per_mm: f64,
    ) -> Result<Self, String> {
        let samples = payload(extension).ok_or("Not a route overlay renderer extension")?;
        Self::prepare_with_world_samples(
            resources,
            samples.revision(),
            samples.paths(),
            samples.declarations(),
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
