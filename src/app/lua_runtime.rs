//! Lua portrayal coordination.
//!
//! `try_lua_portrayal` drives `ferrite-lua`'s `PortrayalEngine` over each
//! loaded S-101 cell, then `convert_lua_results_for_cell` translates the
//! resulting `DrawingCommand` stream into the renderer's
//! `DrawingInstruction` types. Both are large because S-100 portrayal
//! covers many command kinds (points, lines, areas with multiple fill
//! varieties, paths, text, masks); the size is intrinsic to the spec.
//!
//! Kept as free functions (rather than methods on `ChartApp`) because the
//! conversion only touches `RenderContext` plus catalogue references —
//! moving this into `ChartApp` would be a layering inversion.

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

use ferrite_feature_catalog::FeatureCatalogue;
use ferrite_lua::{
    ContextParameters as LuaContextParameters, PortrayalContext, PortrayalEngine, TypeCatalogue,
};
use ferrite_portrayal_catalog::PortrayalCatalogue;
use ferrite_render::{
    AreaInstruction, Color, HAlign, LineInstruction, LineStyle, PointInstruction, RenderContext,
    TextInstruction as RenderTextInstruction, VAlign, WorldPoint,
};
use ferrite_s100_core::S101Cell;
use ferrite_wgpu::SettingsState;

use crate::app::portrayal::lookup_pc_color;

/// Try to execute Lua portrayal rules and convert to drawing instructions.
/// Context parameters are loaded dynamically from PC XML (no hardcoding).
/// Processes each cell separately to avoid feature ID collisions across cells.
pub fn try_lua_portrayal(
    cells: &[S101Cell],
    fc: &FeatureCatalogue,
    pc: &PortrayalCatalogue,
    render_context: &mut RenderContext,
    profile_name: &str,
    settings: Option<&SettingsState>,
) -> Result<()> {
    let rules_path = pc.root_path.join("Rules");

    if !rules_path.exists() {
        return Err(anyhow::anyhow!(
            "Rules directory not found: {}",
            rules_path.display()
        ));
    }

    info!("Initializing Lua portrayal engine...");

    // Create portrayal engine
    let mut engine =
        PortrayalEngine::new(&rules_path).context("Failed to create portrayal engine")?;

    // Set type catalogue from FC
    let type_catalogue = TypeCatalogue::from_feature_catalogue(fc);
    engine.set_type_catalogue(type_catalogue);
    info!(
        "  Type catalogue loaded: {} feature types, {} attributes",
        fc.feature_types.len(),
        fc.simple_attributes.len()
    );

    // Initialize engine (load main.lua)
    engine
        .initialize()
        .context("Failed to initialize portrayal engine")?;
    info!("  Lua engine initialized");

    // Load context parameters from PC XML (dynamically, no hardcoding)
    let pc_context_params = pc.get_context_parameters();
    let mut context = LuaContextParameters::from_pc_context(pc_context_params);

    // Apply UI settings to context parameters if provided
    if let Some(s) = settings {
        context.safety_depth = s.safety_depth;
        context.safety_contour = s.safety_contour;
        context.shallow_contour = s.shallow_contour;
        context.deep_contour = s.deep_contour;
        context.two_shades = s.two_shades;
        context.simplified_symbols = s.simplified_symbols;
        context.isolated_dangers = s.isolated_dangers;
        context.full_sectors = s.full_light_sectors;
        context.ignore_scale_minimum = s.ignore_scale_minimum;
        context.ignore_scamin = s.ignore_scale_minimum;
        context.symbolized_boundaries = !s.plain_boundaries;
        info!(
            "  Applied UI settings: SafetyDepth={}, SafetyContour={}, TwoShades={}",
            s.safety_depth, s.safety_contour, s.two_shades
        );
    }

    info!(
        "  Context parameters loaded from PC XML: {} parameters",
        pc_context_params.len()
    );

    let mut total_results = 0;

    // Process each cell separately to avoid feature ID collisions
    for (cell_index, cell) in cells.iter().enumerate() {
        debug!(
            "Processing cell {}: {}",
            cell_index,
            cell.file_path.display()
        );

        // Create portrayal context for this cell
        let portrayal_context = PortrayalContext::from_cell(cell, context.clone());
        let cell_data_arc = portrayal_context.cell_data();
        let cell_data_guard = cell_data_arc.read().unwrap();

        // Process cell through Lua
        match engine.process_cell(&cell_data_guard, context.clone()) {
            Ok(results) => {
                debug!(
                    "  Cell {} produced {} portrayal results",
                    cell_index,
                    results.len()
                );
                total_results += results.len();

                // Convert THIS cell's Lua results using ONLY this cell's data
                // Pass cell_index so symbols can be looked up in the correct cell
                convert_lua_results_for_cell(
                    &results,
                    cell,
                    pc,
                    render_context,
                    cell_index,
                    profile_name,
                );
            }
            Err(e) => {
                warn!("  Cell {} portrayal failed: {}", cell_index, e);
            }
        }
    }

    info!("Lua portrayal complete: {} total results", total_results);
    Ok(())
}

/// Convert Lua portrayal results to drawing instructions for a single cell
/// Uses only this cell's data to avoid feature ID collisions across cells
fn convert_lua_results_for_cell(
    results: &[ferrite_lua::PortrayalResult],
    cell: &S101Cell,
    pc: &PortrayalCatalogue,
    context: &mut RenderContext,
    cell_index: usize,
    profile_name: &str,
) {
    use ferrite_lua::DrawingCommand;

    // Helper: convert Lua visibility scale fields to ScaleRange
    let make_scale_range = |vis: &ferrite_lua::VisibilityState| -> ferrite_render::ScaleRange {
        ferrite_render::ScaleRange {
            scale_minimum: vis.scale_minimum,
            scale_maximum: vis.scale_maximum,
        }
    };

    // Helper: convert Lua DisplayPlane to render DisplayPlane
    let make_display_plane = |vis: &ferrite_lua::VisibilityState| -> ferrite_render::DisplayPlane {
        match vis.display_plane {
            ferrite_lua::DisplayPlane::OverRadar => ferrite_render::DisplayPlane::OverRadar,
            _ => ferrite_render::DisplayPlane::UnderRadar,
        }
    };

    // Helper: extract primary viewing group from visibility state
    let make_viewing_group = |vis: &ferrite_lua::VisibilityState| -> u32 {
        vis.viewing_groups.first().copied().unwrap_or(21010)
    };

    // Helper to lookup color from token (from PC colorProfile.xml)
    // Uses the specified profile (Day/Dusk/Night) for color resolution
    let lookup_color = |token: &str| -> Color { lookup_pc_color(pc, token, profile_name) };

    // Helper: collect points from a ring (list of oriented curves)
    let collect_ring_points = |curves: &[ferrite_s100_core::OrientedCurve]| -> Vec<WorldPoint> {
        let mut points = Vec::new();
        for oriented_curve in curves {
            let curve_key = oriented_curve.curve_id.key();
            if let Some(curve) = cell.curves.get(&curve_key) {
                let positions = curve.all_positions();
                if oriented_curve.orientation {
                    for pos in positions {
                        points.push(WorldPoint::new(pos.x, pos.y));
                    }
                } else {
                    for pos in positions.into_iter().rev() {
                        points.push(WorldPoint::new(pos.x, pos.y));
                    }
                }
            } else if let Some(composite) = cell.composite_curves.get(&curve_key) {
                for sub_curve in &composite.curves {
                    let sub_key = sub_curve.curve_id.key();
                    if let Some(curve) = cell.curves.get(&sub_key) {
                        let positions = curve.all_positions();
                        let forward = oriented_curve.orientation == sub_curve.orientation;
                        if forward {
                            for pos in positions {
                                points.push(WorldPoint::new(pos.x, pos.y));
                            }
                        } else {
                            for pos in positions.into_iter().rev() {
                                points.push(WorldPoint::new(pos.x, pos.y));
                            }
                        }
                    }
                }
            }
        }
        // Remove duplicate consecutive points
        let mut cleaned = Vec::with_capacity(points.len());
        for point in points {
            if cleaned.is_empty() {
                cleaned.push(point);
            } else {
                let last = cleaned.last().unwrap();
                if (point.x - last.x).abs() > 1e-9 || (point.y - last.y).abs() > 1e-9 {
                    cleaned.push(point);
                }
            }
        }
        // Remove duplicate closing point
        if cleaned.len() > 3 {
            let first = cleaned.first().unwrap();
            let last = cleaned.last().unwrap();
            if (first.x - last.x).abs() < 1e-9 && (first.y - last.y).abs() < 1e-9 {
                cleaned.pop();
            }
        }
        cleaned
    };

    // Helper: collect exterior + validated interior rings from a surface.
    // Only includes interior rings whose bounding box is fully inside
    // the exterior ring's bounding box (prevents earcut failures).
    let collect_surface_points =
        |surface: &ferrite_s100_core::SurfaceRecord| -> (Vec<WorldPoint>, Vec<Vec<WorldPoint>>) {
            let exterior = collect_ring_points(&surface.exterior_ring);
            if exterior.len() < 3 || surface.interior_rings.is_empty() {
                return (exterior, Vec::new());
            }

            // Compute exterior bounding box
            let (mut ex0, mut ey0, mut ex1, mut ey1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
            for p in &exterior {
                if p.x < ex0 {
                    ex0 = p.x;
                }
                if p.y < ey0 {
                    ey0 = p.y;
                }
                if p.x > ex1 {
                    ex1 = p.x;
                }
                if p.y > ey1 {
                    ey1 = p.y;
                }
            }

            let mut valid_interiors = Vec::new();
            for ring_curves in &surface.interior_rings {
                if ring_curves.is_empty() {
                    continue;
                }
                // Verify ring closure from raw curve endpoints before collecting points.
                // collect_ring_points removes the closing duplicate so we can't check after.
                let is_closed = {
                    // Get first point of first curve
                    let first_oc = &ring_curves[0];
                    let last_oc = &ring_curves[ring_curves.len() - 1];
                    let first_key = first_oc.curve_id.key();
                    let last_key = last_oc.curve_id.key();

                    let get_positions = |key: i64,
                                         oc: &ferrite_s100_core::OrientedCurve|
                     -> Option<Vec<WorldPoint>> {
                        if let Some(curve) = cell.curves.get(&key) {
                            let pos = curve.all_positions();
                            if pos.is_empty() {
                                return None;
                            }
                            let pts: Vec<WorldPoint> = if oc.orientation {
                                pos.iter().map(|p| WorldPoint::new(p.x, p.y)).collect()
                            } else {
                                pos.iter()
                                    .rev()
                                    .map(|p| WorldPoint::new(p.x, p.y))
                                    .collect()
                            };
                            Some(pts)
                        } else if let Some(composite) = cell.composite_curves.get(&key) {
                            // Get first/last sub-curve points
                            let mut pts = Vec::new();
                            for sub in &composite.curves {
                                let sk = sub.curve_id.key();
                                if let Some(c) = cell.curves.get(&sk) {
                                    let p = c.all_positions();
                                    let forward = oc.orientation == sub.orientation;
                                    if forward {
                                        pts.extend(p.iter().map(|p| WorldPoint::new(p.x, p.y)));
                                    } else {
                                        pts.extend(
                                            p.iter().rev().map(|p| WorldPoint::new(p.x, p.y)),
                                        );
                                    }
                                }
                            }
                            if pts.is_empty() {
                                None
                            } else {
                                Some(pts)
                            }
                        } else {
                            None
                        }
                    };

                    match (
                        get_positions(first_key, first_oc),
                        get_positions(last_key, last_oc),
                    ) {
                        (Some(first_pts), Some(last_pts)) => {
                            let start = first_pts.first().unwrap();
                            let end = last_pts.last().unwrap();
                            (start.x - end.x).abs() < 1e-5 && (start.y - end.y).abs() < 1e-5
                        }
                        _ => false,
                    }
                };

                if !is_closed {
                    continue;
                }

                let ring = collect_ring_points(ring_curves);
                if ring.len() < 3 {
                    continue;
                }
                // Check that ring bbox is inside exterior bbox
                let mut inside = true;
                for p in &ring {
                    if p.x < ex0 || p.x > ex1 || p.y < ey0 || p.y > ey1 {
                        inside = false;
                        break;
                    }
                }
                if inside {
                    valid_interiors.push(ring);
                }
            }
            (exterior, valid_interiors)
        };

    // Helper: convert h_align string to render enum
    let parse_h_align = |s: &str| -> HAlign {
        match s {
            "Center" | "centre" => HAlign::Center,
            "End" | "right" => HAlign::Right,
            _ => HAlign::Left,
        }
    };

    // Helper: convert v_align string to render enum
    let parse_v_align = |s: &str| -> VAlign {
        match s {
            "Top" | "top" => VAlign::Top,
            "Bottom" | "bottom" => VAlign::Bottom,
            _ => VAlign::Middle,
        }
    };

    let mut area_count = 0;
    let mut area_rendered = 0;
    let mut line_count = 0;
    let mut point_count = 0;

    for result in results {
        // Parse feature ID from the result (format: "type|id")
        let feature_id: Option<i64> = result
            .feature_id
            .split('|')
            .next_back()
            .and_then(|s| s.parse().ok());

        // Get feature from THIS cell only (no collision with other cells)
        let feature = feature_id.and_then(|id| cell.features.get(&id));

        for instruction in &result.instructions {
            for cmd in &instruction.commands {
                match cmd {
                    DrawingCommand::PointInstruction {
                        symbol_ref,
                        rotation,
                        scale,
                        position,
                        line_placement,
                        visibility,
                        ..
                    } => {
                        point_count += 1;

                        // If explicit position is available (from AugmentedPoint), use it
                        // This is used for Sounding features where each point has specific coordinates
                        if let Some((x, y)) = position {
                            // For soundings: look up depth from multi_points spatial data
                            // The depth is used for decluttering (keep shallowest for safety)
                            let depth = feature.and_then(|f| {
                                // Find the MultiPoint spatial association
                                for spas in &f.spatial_associations {
                                    if let Some(mp) = cell.multi_points.get(&spas.spatial_id.key())
                                    {
                                        // Find the position with matching coordinates
                                        for coord in &mp.positions {
                                            // Use small epsilon for floating point comparison
                                            if (coord.x - x).abs() < 1e-9
                                                && (coord.y - y).abs() < 1e-9
                                            {
                                                return coord.depth();
                                            }
                                        }
                                    }
                                }
                                None
                            });

                            let mut point_inst =
                                PointInstruction::new(symbol_ref.clone(), WorldPoint::new(*x, *y))
                                    .with_rotation(*rotation)
                                    .with_scale(*scale)
                                    .with_priority(visibility.drawing_priority)
                                    .with_viewing_group(make_viewing_group(visibility))
                                    .with_scale_range(make_scale_range(visibility))
                                    .with_display_plane(make_display_plane(visibility))
                                    .with_feature_id(feature_id.unwrap_or(0))
                                    .with_cell_index(cell_index);

                            // Add depth for sounding decluttering (shallowest wins for safety)
                            if let Some(d) = depth {
                                point_inst = point_inst.with_depth(d);
                            }

                            context.add_instruction(ferrite_render::DrawingInstruction::Point(
                                point_inst,
                            ));
                        } else if let Some(feature) = feature {
                            // S-100 Part 9a LinePlacement: when the feature has curve geometry,
                            // place the symbol at a specific position along the curve.
                            let has_curve_geometry = feature.spatial_associations.iter().any(|s| {
                                let key = s.spatial_id.key();
                                cell.curves.contains_key(&key)
                                    || cell.composite_curves.contains_key(&key)
                            });

                            if has_curve_geometry {
                                if let Some((mode, offset)) = line_placement {
                                    // Collect all curve points from the feature's spatial associations
                                    let mut curve_points: Vec<(f64, f64)> = Vec::new();
                                    for spas in &feature.spatial_associations {
                                        let key = spas.spatial_id.key();
                                        let forward = spas.ornt != 2; // ornt=2 means reverse
                                        if let Some(curve) = cell.curves.get(&key) {
                                            let positions = curve.all_positions();
                                            if forward {
                                                for pos in &positions {
                                                    curve_points.push((pos.x, pos.y));
                                                }
                                            } else {
                                                for pos in positions.iter().rev() {
                                                    curve_points.push((pos.x, pos.y));
                                                }
                                            }
                                        } else if let Some(composite) =
                                            cell.composite_curves.get(&key)
                                        {
                                            for sub_curve in &composite.curves {
                                                let sub_key = sub_curve.curve_id.key();
                                                if let Some(curve) = cell.curves.get(&sub_key) {
                                                    let positions = curve.all_positions();
                                                    let sub_forward =
                                                        forward == sub_curve.orientation;
                                                    if sub_forward {
                                                        for pos in &positions {
                                                            curve_points.push((pos.x, pos.y));
                                                        }
                                                    } else {
                                                        for pos in positions.iter().rev() {
                                                            curve_points.push((pos.x, pos.y));
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }

                                    // Remove consecutive duplicates
                                    curve_points.dedup_by(|a, b| {
                                        (a.0 - b.0).abs() < 1e-12 && (a.1 - b.1).abs() < 1e-12
                                    });

                                    if curve_points.len() >= 2 {
                                        // Calculate cumulative segment lengths along the curve
                                        let mut seg_lengths =
                                            Vec::with_capacity(curve_points.len() - 1);
                                        let mut total_length = 0.0_f64;
                                        for i in 1..curve_points.len() {
                                            let dx = curve_points[i].0 - curve_points[i - 1].0;
                                            let dy = curve_points[i].1 - curve_points[i - 1].1;
                                            let len = (dx * dx + dy * dy).sqrt();
                                            seg_lengths.push(len);
                                            total_length += len;
                                        }

                                        if total_length > 0.0 {
                                            // Determine the target distance along the curve
                                            let target_dist = if mode == "Relative" {
                                                offset.clamp(0.0, 1.0) * total_length
                                            } else {
                                                // Absolute mode: offset is in mm.
                                                // Convert mm to approximate geographic degrees.
                                                // At the feature's latitude, 1 degree longitude ~
                                                // 111320 * cos(lat) meters.
                                                // Use midpoint latitude for the conversion.
                                                let mid_lat =
                                                    curve_points.iter().map(|p| p.1).sum::<f64>()
                                                        / curve_points.len() as f64;
                                                let meters_per_deg =
                                                    111_320.0 * mid_lat.to_radians().cos();
                                                let mm_to_deg = 1.0 / (meters_per_deg * 1000.0);
                                                let abs_dist = *offset * mm_to_deg;
                                                abs_dist.min(total_length)
                                            };

                                            // Walk along segments to find the interpolated point
                                            let mut accum = 0.0_f64;
                                            let mut placed = false;
                                            for (i, &seg_len) in seg_lengths.iter().enumerate() {
                                                if accum + seg_len >= target_dist {
                                                    // Interpolate within this segment
                                                    let t = if seg_len > 0.0 {
                                                        (target_dist - accum) / seg_len
                                                    } else {
                                                        0.0
                                                    };
                                                    let px = curve_points[i].0
                                                        + t * (curve_points[i + 1].0
                                                            - curve_points[i].0);
                                                    let py = curve_points[i].1
                                                        + t * (curve_points[i + 1].1
                                                            - curve_points[i].1);

                                                    let point_inst = PointInstruction::new(
                                                        symbol_ref.clone(),
                                                        WorldPoint::new(px, py),
                                                    )
                                                    .with_rotation(*rotation)
                                                    .with_scale(*scale)
                                                    .with_priority(visibility.drawing_priority)
                                                    .with_viewing_group(make_viewing_group(
                                                        visibility,
                                                    ))
                                                    .with_scale_range(make_scale_range(visibility))
                                                    .with_display_plane(make_display_plane(
                                                        visibility,
                                                    ))
                                                    .with_feature_id(feature_id.unwrap_or(0))
                                                    .with_cell_index(cell_index);

                                                    context.add_instruction(
                                                        ferrite_render::DrawingInstruction::Point(
                                                            point_inst,
                                                        ),
                                                    );
                                                    placed = true;
                                                    break;
                                                }
                                                accum += seg_len;
                                            }
                                            // If rounding prevented placement, use last point
                                            if !placed {
                                                let last = curve_points.last().unwrap();
                                                let point_inst = PointInstruction::new(
                                                    symbol_ref.clone(),
                                                    WorldPoint::new(last.0, last.1),
                                                )
                                                .with_rotation(*rotation)
                                                .with_scale(*scale)
                                                .with_priority(visibility.drawing_priority)
                                                .with_viewing_group(make_viewing_group(visibility))
                                                .with_scale_range(make_scale_range(visibility))
                                                .with_display_plane(make_display_plane(visibility))
                                                .with_feature_id(feature_id.unwrap_or(0))
                                                .with_cell_index(cell_index);

                                                context.add_instruction(
                                                    ferrite_render::DrawingInstruction::Point(
                                                        point_inst,
                                                    ),
                                                );
                                            }
                                        }
                                    }
                                }
                            } else {
                                // Point or Surface geometry: place symbol at point position
                                // or surface centroid (S-100 Part 9a: area features with
                                // PointInstruction place the symbol at the area centroid)
                                let mut placed = false;

                                // Try point geometry first
                                for spas in &feature.spatial_associations {
                                    if let Some(point) = cell.points.get(&spas.spatial_id.key()) {
                                        let point_inst = PointInstruction::new(
                                            symbol_ref.clone(),
                                            WorldPoint::new(point.position.x, point.position.y),
                                        )
                                        .with_rotation(*rotation)
                                        .with_scale(*scale)
                                        .with_priority(visibility.drawing_priority)
                                        .with_viewing_group(make_viewing_group(visibility))
                                        .with_scale_range(make_scale_range(visibility))
                                        .with_display_plane(make_display_plane(visibility))
                                        .with_feature_id(feature_id.unwrap_or(0))
                                        .with_cell_index(cell_index);

                                        context.add_instruction(
                                            ferrite_render::DrawingInstruction::Point(point_inst),
                                        );
                                        placed = true;
                                    }
                                }

                                // Surface geometry: compute centroid and place symbol there
                                if !placed {
                                    for spas in &feature.spatial_associations {
                                        if let Some(surface) =
                                            cell.surfaces.get(&spas.spatial_id.key())
                                        {
                                            let ext = collect_ring_points(&surface.exterior_ring);
                                            if ext.len() >= 3 {
                                                // Compute polygon centroid using the shoelace formula
                                                let mut cx = 0.0_f64;
                                                let mut cy = 0.0_f64;
                                                let mut area2 = 0.0_f64;
                                                let n = ext.len();
                                                for i in 0..n {
                                                    let j = (i + 1) % n;
                                                    let cross =
                                                        ext[i].x * ext[j].y - ext[j].x * ext[i].y;
                                                    cx += (ext[i].x + ext[j].x) * cross;
                                                    cy += (ext[i].y + ext[j].y) * cross;
                                                    area2 += cross;
                                                }
                                                if area2.abs() > 1e-15 {
                                                    cx /= 3.0 * area2;
                                                    cy /= 3.0 * area2;
                                                } else {
                                                    // Degenerate polygon: use average of points
                                                    cx = ext.iter().map(|p| p.x).sum::<f64>()
                                                        / n as f64;
                                                    cy = ext.iter().map(|p| p.y).sum::<f64>()
                                                        / n as f64;
                                                }

                                                let point_inst = PointInstruction::new(
                                                    symbol_ref.clone(),
                                                    WorldPoint::new(cx, cy),
                                                )
                                                .with_rotation(*rotation)
                                                .with_scale(*scale)
                                                .with_priority(visibility.drawing_priority)
                                                .with_viewing_group(make_viewing_group(visibility))
                                                .with_scale_range(make_scale_range(visibility))
                                                .with_display_plane(make_display_plane(visibility))
                                                .with_feature_id(feature_id.unwrap_or(0))
                                                .with_cell_index(cell_index);

                                                context.add_instruction(
                                                    ferrite_render::DrawingInstruction::Point(
                                                        point_inst,
                                                    ),
                                                );
                                                break; // One centroid symbol per feature
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    DrawingCommand::LineInstruction {
                        style_refs,
                        simple_style,
                        augmented_ray,
                        augmented_segments,
                        visibility,
                        ..
                    }
                    | DrawingCommand::LineInstructionUnsuppressed {
                        style_refs,
                        simple_style,
                        augmented_ray,
                        augmented_segments,
                        visibility,
                        ..
                    } => {
                        // S-100 Part 9-11.1.9: LineInstructionUnsuppressed cannot be
                        // suppressed by higher-priority lines on the same curve.
                        let unsuppressed =
                            matches!(cmd, DrawingCommand::LineInstructionUnsuppressed { .. });

                        line_count += 1;
                        // Determine line color and width from PC (no hardcoding)
                        let (color, width, line_color_token) =
                            if let Some((w, token)) = simple_style {
                                (lookup_color(token), *w, token.to_string())
                            } else if let Some(ref_name) = style_refs.first() {
                                // Look up from PC line styles
                                if let Some(style) = pc.line_styles.get(ref_name) {
                                    match style {
                                        ferrite_portrayal_catalog::LineStyle::Simple(s) => (
                                            lookup_color(&s.pen.color_token),
                                            s.pen.width as f32,
                                            s.pen.color_token.clone(),
                                        ),
                                        ferrite_portrayal_catalog::LineStyle::Complex(c) => {
                                            if let Some(s) = c.strokes.first() {
                                                (
                                                    lookup_color(&s.pen.color_token),
                                                    s.pen.width as f32,
                                                    s.pen.color_token.clone(),
                                                )
                                            } else {
                                                (lookup_color("CSTLN"), 1.0, "CSTLN".to_string())
                                            }
                                        }
                                        ferrite_portrayal_catalog::LineStyle::Composite(c) => {
                                            if let Some(s) = c.components.first() {
                                                (
                                                    lookup_color(&s.pen.color_token),
                                                    s.pen.width as f32,
                                                    s.pen.color_token.clone(),
                                                )
                                            } else {
                                                (lookup_color("CSTLN"), 1.0, "CSTLN".to_string())
                                            }
                                        }
                                    }
                                } else {
                                    (lookup_color("CSTLN"), 1.0, "CSTLN".to_string())
                                }
                            } else {
                                (lookup_color("CSTLN"), 1.0, "CSTLN".to_string())
                            };

                        // S-100 Part 9a-11.2.15: AugmentedRay — a line from the point
                        // feature's position in a given direction for a given length.
                        // Used for light sector lines, bearing lines, etc.
                        if let Some(ray) = augmented_ray {
                            // Find the feature's point position as the ray origin.
                            let origin = feature.and_then(|f| {
                                for spas in &f.spatial_associations {
                                    if let Some(pt) = cell.points.get(&spas.spatial_id.key()) {
                                        return Some((pt.position.x, pt.position.y));
                                    }
                                }
                                None
                            });

                            if let Some((ox, oy)) = origin {
                                // S-100: direction is degrees clockwise from north
                                // (GeographicCRS) or from positive y-axis (PortrayalCRS/LocalCRS).
                                // The trigonometric conversion is the same for both.
                                let dir_rad = ray.direction.to_radians();

                                // Compute endpoint based on length CRS.
                                let (ex, ey) = if ray.length_crs == "GeographicCRS" {
                                    // Length is in metres; convert to approximate degrees.
                                    // 1 degree latitude ~ 111320 m.
                                    // 1 degree longitude ~ 111320 * cos(lat) m.
                                    let lat_rad = oy.to_radians();
                                    let cos_lat = lat_rad.cos();
                                    let dy_deg = (ray.length * dir_rad.cos()) / 111_320.0;
                                    let dx_deg = if cos_lat.abs() > 1e-10 {
                                        (ray.length * dir_rad.sin()) / (111_320.0 * cos_lat)
                                    } else {
                                        0.0
                                    };
                                    (ox + dx_deg, oy + dy_deg)
                                } else {
                                    // PortrayalCRS or LocalCRS: length is in mm (screen space).
                                    // Convert mm to metres using the cell's compilation scale,
                                    // then to approximate degrees.
                                    let scale = cell.compilation_scale as f64;
                                    let length_m = ray.length * scale / 1000.0;
                                    let lat_rad = oy.to_radians();
                                    let cos_lat = lat_rad.cos();
                                    let dy_deg = (length_m * dir_rad.cos()) / 111_320.0;
                                    let dx_deg = if cos_lat.abs() > 1e-10 {
                                        (length_m * dir_rad.sin()) / (111_320.0 * cos_lat)
                                    } else {
                                        0.0
                                    };
                                    (ox + dx_deg, oy + dy_deg)
                                };

                                let points = vec![WorldPoint::new(ox, oy), WorldPoint::new(ex, ey)];

                                let mut line_inst = LineInstruction::new(points)
                                    .with_style(LineStyle::solid(color, width))
                                    .with_priority(visibility.drawing_priority)
                                    .with_viewing_group(make_viewing_group(visibility))
                                    .with_scale_range(make_scale_range(visibility))
                                    .with_display_plane(make_display_plane(visibility))
                                    .with_feature_id(feature_id.unwrap_or(0));
                                line_inst.color_token = Some(line_color_token.clone());

                                if unsuppressed {
                                    line_inst = line_inst.with_unsuppressed();
                                }

                                context.add_instruction(ferrite_render::DrawingInstruction::Line(
                                    line_inst,
                                ));
                            }
                        } else if !augmented_segments.is_empty() {
                            // S-100 Part 9a-11.2.16: AugmentedPath — line from path segments.
                            // Find the feature's point position as the local origin for
                            // LocalCRS/PortrayalCRS segments.
                            let origin = feature.and_then(|f| {
                                for spas in &f.spatial_associations {
                                    if let Some(pt) = cell.points.get(&spas.spatial_id.key()) {
                                        return Some((pt.position.x, pt.position.y));
                                    }
                                }
                                None
                            });

                            let scale = cell.compilation_scale as f64;

                            for seg in augmented_segments {
                                match seg {
                                    ferrite_lua::PathSegment::Polyline(pts) => {
                                        let points: Vec<WorldPoint> = pts
                                            .iter()
                                            .map(|(x, y)| WorldPoint::new(*x, *y))
                                            .collect();
                                        if points.len() >= 2 {
                                            let mut line_inst = LineInstruction::new(points)
                                                .with_style(LineStyle::solid(color, width))
                                                .with_priority(visibility.drawing_priority)
                                                .with_viewing_group(make_viewing_group(visibility))
                                                .with_scale_range(make_scale_range(visibility))
                                                .with_display_plane(make_display_plane(visibility))
                                                .with_feature_id(feature_id.unwrap_or(0));
                                            line_inst.color_token = Some(line_color_token.clone());
                                            if unsuppressed {
                                                line_inst = line_inst.with_unsuppressed();
                                            }
                                            context.add_instruction(
                                                ferrite_render::DrawingInstruction::Line(line_inst),
                                            );
                                        }
                                    }
                                    ferrite_lua::PathSegment::ArcByRadius {
                                        center,
                                        radius,
                                        start_angle,
                                        angular_distance,
                                    } => {
                                        // Arc center/radius are typically in LocalCRS (mm from
                                        // feature point).  Convert to geographic coordinates.
                                        if let Some((ox, oy)) = origin {
                                            let lat_rad = oy.to_radians();
                                            let cos_lat = lat_rad.cos();
                                            let r_m = radius * scale / 1000.0;
                                            let cx_deg = if cos_lat.abs() > 1e-10 {
                                                ox + (center.0 * scale / 1000.0)
                                                    / (111_320.0 * cos_lat)
                                            } else {
                                                ox
                                            };
                                            let cy_deg =
                                                oy + (center.1 * scale / 1000.0) / 111_320.0;
                                            let r_deg = r_m / 111_320.0;

                                            // Tessellate arc into polyline segments
                                            let step_count = ((angular_distance.abs() / 5.0).ceil()
                                                as usize)
                                                .max(8);
                                            let mut arc_pts = Vec::with_capacity(step_count + 1);
                                            for i in 0..=step_count {
                                                let frac = i as f64 / step_count as f64;
                                                let angle_deg =
                                                    start_angle + angular_distance * frac;
                                                let angle_rad = angle_deg.to_radians();
                                                // Angles are clockwise from north/+Y
                                                let px = if cos_lat.abs() > 1e-10 {
                                                    cx_deg + r_deg * angle_rad.sin() / cos_lat
                                                } else {
                                                    cx_deg
                                                };
                                                let py = cy_deg + r_deg * angle_rad.cos();
                                                arc_pts.push(WorldPoint::new(px, py));
                                            }

                                            if arc_pts.len() >= 2 {
                                                let mut line_inst = LineInstruction::new(arc_pts)
                                                    .with_style(LineStyle::solid(color, width))
                                                    .with_priority(visibility.drawing_priority)
                                                    .with_viewing_group(make_viewing_group(
                                                        visibility,
                                                    ))
                                                    .with_scale_range(make_scale_range(visibility))
                                                    .with_display_plane(make_display_plane(
                                                        visibility,
                                                    ))
                                                    .with_feature_id(feature_id.unwrap_or(0));
                                                line_inst.color_token =
                                                    Some(line_color_token.clone());
                                                if unsuppressed {
                                                    line_inst = line_inst.with_unsuppressed();
                                                }
                                                context.add_instruction(
                                                    ferrite_render::DrawingInstruction::Line(
                                                        line_inst,
                                                    ),
                                                );
                                            }
                                        }
                                    }
                                    ferrite_lua::PathSegment::Arc3Points { start, median, end } => {
                                        // Approximate 3-point arc: compute the circle through
                                        // the three points and tessellate.
                                        let points = vec![
                                            WorldPoint::new(start.0, start.1),
                                            WorldPoint::new(median.0, median.1),
                                            WorldPoint::new(end.0, end.1),
                                        ];
                                        let mut line_inst = LineInstruction::new(points)
                                            .with_style(LineStyle::solid(color, width))
                                            .with_priority(visibility.drawing_priority)
                                            .with_viewing_group(make_viewing_group(visibility))
                                            .with_scale_range(make_scale_range(visibility))
                                            .with_display_plane(make_display_plane(visibility))
                                            .with_feature_id(feature_id.unwrap_or(0));
                                        line_inst.color_token = Some(line_color_token.clone());
                                        if unsuppressed {
                                            line_inst = line_inst.with_unsuppressed();
                                        }
                                        context.add_instruction(
                                            ferrite_render::DrawingInstruction::Line(line_inst),
                                        );
                                    }
                                    ferrite_lua::PathSegment::Annulus { .. } => {
                                        // Annulus is primarily for area fills; not applicable
                                        // to line rendering.
                                    }
                                }
                            }
                        } else if let Some(feature) = feature {
                            // Default: get coordinates from feature's spatial associations
                            for spas in &feature.spatial_associations {
                                // S-101 4.8.3: Edge masking — check if this curve is
                                // suppressed for this feature.
                                // mask=2 in SPAS means "suppress portrayal" of this edge.
                                if spas.mask == 2 {
                                    continue;
                                }
                                // Also check the MASK field records (MIND=2 = suppress)
                                let masked_by_mask_field = feature.masks.iter().any(|m| {
                                    m.spatial_id.key() == spas.spatial_id.key() && m.mask_type == 2
                                });
                                if masked_by_mask_field {
                                    continue;
                                }

                                if let Some(curve) = cell.curves.get(&spas.spatial_id.key()) {
                                    let points: Vec<WorldPoint> = curve
                                        .all_positions()
                                        .iter()
                                        .map(|c| WorldPoint::new(c.x, c.y))
                                        .collect();

                                    if points.len() >= 2 {
                                        let mut line_inst = LineInstruction::new(points)
                                            .with_style(LineStyle::solid(color, width))
                                            .with_priority(visibility.drawing_priority)
                                            .with_viewing_group(make_viewing_group(visibility))
                                            .with_scale_range(make_scale_range(visibility))
                                            .with_display_plane(make_display_plane(visibility))
                                            .with_feature_id(feature_id.unwrap_or(0));
                                        line_inst.color_token = Some(line_color_token.clone());

                                        if unsuppressed {
                                            line_inst = line_inst.with_unsuppressed();
                                        }

                                        context.add_instruction(
                                            ferrite_render::DrawingInstruction::Line(line_inst),
                                        );
                                    }
                                }
                            }
                        }
                    }
                    DrawingCommand::ColorFill {
                        color_token,
                        visibility,
                        ..
                    } => {
                        area_count += 1;
                        let color = lookup_color(color_token);
                        let draw_priority = visibility.drawing_priority;
                        if let Some(feature) = feature {
                            for spas in &feature.spatial_associations {
                                if spas.spatial_id.rcnm != 130 {
                                    continue;
                                }
                                if let Some(surface) = cell.surfaces.get(&spas.spatial_id.key()) {
                                    let (exterior, interiors) = collect_surface_points(surface);
                                    if exterior.len() >= 3 {
                                        area_rendered += 1;
                                        let area_inst = AreaInstruction::new(exterior)
                                            .with_interiors(interiors)
                                            .with_solid_fill_token(color, color_token)
                                            .with_priority(draw_priority)
                                            .with_viewing_group(make_viewing_group(visibility))
                                            .with_scale_range(make_scale_range(visibility))
                                            .with_display_plane(make_display_plane(visibility))
                                            .with_feature_id(feature_id.unwrap_or(0));
                                        context.add_instruction(
                                            ferrite_render::DrawingInstruction::Area(area_inst),
                                        );
                                    }
                                }
                            }
                        }
                    }
                    DrawingCommand::AreaFillReference {
                        reference,
                        visibility,
                        ..
                    } => {
                        area_count += 1;
                        let draw_priority = visibility.drawing_priority;

                        // Look up fill type from PC
                        let fill = pc.area_fills.get(reference.as_str());

                        if let Some(feature) = feature {
                            for spas in &feature.spatial_associations {
                                if spas.spatial_id.rcnm != 130 {
                                    continue;
                                }
                                if let Some(surface) = cell.surfaces.get(&spas.spatial_id.key()) {
                                    let (exterior, interiors) = collect_surface_points(surface);
                                    if exterior.len() >= 3 {
                                        area_rendered += 1;
                                        let area_inst = AreaInstruction::new(exterior.clone())
                                            .with_interiors(interiors.clone())
                                            .with_priority(draw_priority)
                                            .with_viewing_group(make_viewing_group(visibility))
                                            .with_scale_range(make_scale_range(visibility))
                                            .with_display_plane(make_display_plane(visibility))
                                            .with_feature_id(feature_id.unwrap_or(0));

                                        let area_inst = if let Some(fill) = fill {
                                            match &fill.fill_type {
                                                ferrite_portrayal_catalog::AreaFillType::Color(c) => {
                                                    area_inst.with_solid_fill_token(lookup_color(&c.color_token), &c.color_token)
                                                }
                                                ferrite_portrayal_catalog::AreaFillType::Hatch(h) => {
                                                    area_inst.with_hatch_fill_token(
                                                        lookup_color(&h.line_color),
                                                        &h.line_color,
                                                        h.line_width as f32,
                                                        h.spacing as f32,
                                                        h.angle as f32,
                                                    )
                                                }
                                                ferrite_portrayal_catalog::AreaFillType::Symbol(s) => {
                                                    area_inst.with_pattern_fill(
                                                        s.symbol_ref.clone(),
                                                        (s.v1.x as f32, s.v1.y as f32),
                                                        (s.v2.x as f32, s.v2.y as f32),
                                                    )
                                                }
                                                ferrite_portrayal_catalog::AreaFillType::Pattern(p) => {
                                                    area_inst.with_pattern_fill(
                                                        p.symbol_ref.clone(),
                                                        (p.spacing_x as f32, 0.0),
                                                        (0.0, p.spacing_y as f32),
                                                    )
                                                }
                                                ferrite_portrayal_catalog::AreaFillType::Pixmap(px) => {
                                                    tracing::warn!("AreaFillReference: raster pixmap fill not yet renderable (image: {:?}), falling back to NODTA", px.image_ref);
                                                    area_inst.with_solid_fill_token(lookup_color("NODTA"), "NODTA")
                                                }
                                            }
                                        } else {
                                            area_inst.with_solid_fill_token(
                                                lookup_color("NODTA"),
                                                "NODTA",
                                            )
                                        };

                                        context.add_instruction(
                                            ferrite_render::DrawingInstruction::Area(area_inst),
                                        );
                                    }
                                }
                            }
                        }
                    }
                    DrawingCommand::PixmapFill {
                        reference,
                        visibility,
                        ..
                    } => {
                        area_count += 1;
                        let draw_priority = visibility.drawing_priority;

                        let fill = pc.area_fills.get(reference.as_str());

                        if let Some(feature) = feature {
                            for spas in &feature.spatial_associations {
                                if spas.spatial_id.rcnm != 130 {
                                    continue;
                                }
                                if let Some(surface) = cell.surfaces.get(&spas.spatial_id.key()) {
                                    let (exterior, interiors) = collect_surface_points(surface);
                                    if exterior.len() >= 3 {
                                        area_rendered += 1;
                                        let area_inst = AreaInstruction::new(exterior.clone())
                                            .with_interiors(interiors.clone())
                                            .with_priority(draw_priority)
                                            .with_viewing_group(make_viewing_group(visibility))
                                            .with_scale_range(make_scale_range(visibility))
                                            .with_display_plane(make_display_plane(visibility))
                                            .with_feature_id(feature_id.unwrap_or(0));

                                        let area_inst = if let Some(fill) = fill {
                                            match &fill.fill_type {
                                                ferrite_portrayal_catalog::AreaFillType::Color(c) => {
                                                    area_inst.with_solid_fill_token(lookup_color(&c.color_token), &c.color_token)
                                                }
                                                ferrite_portrayal_catalog::AreaFillType::Hatch(h) => {
                                                    area_inst.with_hatch_fill_token(
                                                        lookup_color(&h.line_color),
                                                        &h.line_color,
                                                        h.line_width as f32,
                                                        h.spacing as f32,
                                                        h.angle as f32,
                                                    )
                                                }
                                                ferrite_portrayal_catalog::AreaFillType::Symbol(s) => {
                                                    area_inst.with_pattern_fill(
                                                        s.symbol_ref.clone(),
                                                        (s.v1.x as f32, s.v1.y as f32),
                                                        (s.v2.x as f32, s.v2.y as f32),
                                                    )
                                                }
                                                ferrite_portrayal_catalog::AreaFillType::Pattern(p) => {
                                                    area_inst.with_pattern_fill(
                                                        p.symbol_ref.clone(),
                                                        (p.spacing_x as f32, 0.0),
                                                        (0.0, p.spacing_y as f32),
                                                    )
                                                }
                                                ferrite_portrayal_catalog::AreaFillType::Pixmap(px) => {
                                                    tracing::warn!("PixmapFill: raster pixmap fill not yet renderable (image: {:?}), falling back to NODTA", px.image_ref);
                                                    area_inst.with_solid_fill_token(lookup_color("NODTA"), "NODTA")
                                                }
                                            }
                                        } else {
                                            area_inst.with_solid_fill_token(
                                                lookup_color("NODTA"),
                                                "NODTA",
                                            )
                                        };

                                        context.add_instruction(
                                            ferrite_render::DrawingInstruction::Area(area_inst),
                                        );
                                    }
                                }
                            }
                        }
                    }
                    DrawingCommand::SymbolFill {
                        symbol,
                        v1,
                        v2,
                        visibility,
                        ..
                    } => {
                        area_count += 1;
                        // S-100 Part 9a: v1/v2 define parallelogram lattice for symbol tiling
                        let v1f = (v1.0 as f32, v1.1 as f32);
                        let v2f = (v2.0 as f32, v2.1 as f32);
                        if let Some(feature) = feature {
                            for spas in &feature.spatial_associations {
                                if spas.spatial_id.rcnm != 130 {
                                    continue;
                                }
                                if let Some(surface) = cell.surfaces.get(&spas.spatial_id.key()) {
                                    let (exterior, interiors) = collect_surface_points(surface);
                                    if exterior.len() >= 3 {
                                        area_rendered += 1;
                                        let area_inst = AreaInstruction::new(exterior)
                                            .with_interiors(interiors)
                                            .with_pattern_fill(symbol.clone(), v1f, v2f)
                                            .with_priority(visibility.drawing_priority)
                                            .with_viewing_group(make_viewing_group(visibility))
                                            .with_scale_range(make_scale_range(visibility))
                                            .with_display_plane(make_display_plane(visibility))
                                            .with_feature_id(feature_id.unwrap_or(0));
                                        context.add_instruction(
                                            ferrite_render::DrawingInstruction::Area(area_inst),
                                        );
                                    }
                                }
                            }
                        }
                    }
                    DrawingCommand::HatchFill {
                        direction,
                        distance,
                        line_styles,
                        visibility,
                        ..
                    } => {
                        area_count += 1;
                        // Look up first line style from PC for color and width
                        let (color, line_width_mm, hatch_token) = line_styles
                            .first()
                            .and_then(|name| pc.line_styles.get(name.as_str()))
                            .map(|style| match style {
                                ferrite_portrayal_catalog::LineStyle::Simple(s) => (
                                    lookup_color(&s.pen.color_token),
                                    s.pen.width as f32,
                                    s.pen.color_token.clone(),
                                ),
                                ferrite_portrayal_catalog::LineStyle::Complex(c) => {
                                    let (col, tok) = c
                                        .strokes
                                        .first()
                                        .map(|s| {
                                            (
                                                lookup_color(&s.pen.color_token),
                                                s.pen.color_token.clone(),
                                            )
                                        })
                                        .unwrap_or_else(|| {
                                            (lookup_color("CSTLN"), "CSTLN".to_string())
                                        });
                                    let w = c
                                        .strokes
                                        .first()
                                        .map(|s| s.pen.width as f32)
                                        .unwrap_or(0.32);
                                    (col, w, tok)
                                }
                                ferrite_portrayal_catalog::LineStyle::Composite(c) => {
                                    let (col, tok) = c
                                        .components
                                        .first()
                                        .map(|s| {
                                            (
                                                lookup_color(&s.pen.color_token),
                                                s.pen.color_token.clone(),
                                            )
                                        })
                                        .unwrap_or_else(|| {
                                            (lookup_color("CSTLN"), "CSTLN".to_string())
                                        });
                                    let w = c
                                        .components
                                        .first()
                                        .map(|s| s.pen.width as f32)
                                        .unwrap_or(0.32);
                                    (col, w, tok)
                                }
                            })
                            .unwrap_or_else(|| (lookup_color("CSTLN"), 0.32, "CSTLN".to_string()));
                        // Compute angle from direction vector (dirX, dirY) in degrees
                        let angle = (direction.1.atan2(direction.0).to_degrees()) as f32;
                        // distance is in mm per S-100 spec
                        let spacing_mm = (*distance as f32).max(0.5);
                        if let Some(feature) = feature {
                            for spas in &feature.spatial_associations {
                                if spas.spatial_id.rcnm != 130 {
                                    continue;
                                }
                                if let Some(surface) = cell.surfaces.get(&spas.spatial_id.key()) {
                                    let (exterior, interiors) = collect_surface_points(surface);
                                    if exterior.len() >= 3 {
                                        area_rendered += 1;
                                        let area_inst = AreaInstruction::new(exterior)
                                            .with_interiors(interiors)
                                            .with_hatch_fill_token(
                                                color,
                                                &hatch_token,
                                                line_width_mm,
                                                spacing_mm,
                                                angle,
                                            )
                                            .with_priority(visibility.drawing_priority)
                                            .with_viewing_group(make_viewing_group(visibility))
                                            .with_scale_range(make_scale_range(visibility))
                                            .with_display_plane(make_display_plane(visibility))
                                            .with_feature_id(feature_id.unwrap_or(0));
                                        context.add_instruction(
                                            ferrite_render::DrawingInstruction::Area(area_inst),
                                        );
                                    }
                                }
                            }
                        }
                    }
                    DrawingCommand::TextInstruction {
                        text,
                        font_size,
                        color_token,
                        bold,
                        italic,
                        h_align,
                        v_align,
                        rotation,
                        local_offset,
                        position,
                        visibility,
                        ..
                    } => {
                        let color = lookup_color(color_token);
                        // If explicit position from AugmentedPoint, use it
                        if let Some((x, y)) = position {
                            let mut text_inst =
                                RenderTextInstruction::new(text.clone(), WorldPoint::new(*x, *y))
                                    .with_font_size(*font_size)
                                    .with_color(color)
                                    .with_alignment(parse_h_align(h_align), parse_v_align(v_align))
                                    .with_rotation(*rotation)
                                    .with_offset(local_offset.0 as f32, local_offset.1 as f32)
                                    .with_priority(visibility.drawing_priority)
                                    .with_viewing_group(make_viewing_group(visibility))
                                    .with_scale_range(make_scale_range(visibility))
                                    .with_display_plane(make_display_plane(visibility))
                                    .with_feature_id(feature_id.unwrap_or(0));
                            text_inst.color_token = Some(color_token.clone());
                            context.add_instruction(ferrite_render::DrawingInstruction::Text(
                                text_inst,
                            ));
                        } else if let Some(feature) = feature {
                            // Place text at feature geometry:
                            // 1. Point geometry → at point position
                            // 2. Surface geometry → at area centroid
                            let mut placed = false;

                            // Try point geometry first
                            for spas in &feature.spatial_associations {
                                if let Some(point) = cell.points.get(&spas.spatial_id.key()) {
                                    let mut ti = RenderTextInstruction::new(
                                        text.clone(),
                                        WorldPoint::new(point.position.x, point.position.y),
                                    )
                                    .with_font_size(*font_size)
                                    .with_color(color)
                                    .with_alignment(parse_h_align(h_align), parse_v_align(v_align))
                                    .with_rotation(*rotation)
                                    .with_offset(local_offset.0 as f32, local_offset.1 as f32)
                                    .with_priority(visibility.drawing_priority)
                                    .with_viewing_group(make_viewing_group(visibility))
                                    .with_scale_range(make_scale_range(visibility))
                                    .with_display_plane(make_display_plane(visibility))
                                    .with_feature_id(feature_id.unwrap_or(0));
                                    ti.bold = *bold;
                                    ti.italic = *italic;
                                    ti.color_token = Some(color_token.clone());
                                    context.add_instruction(
                                        ferrite_render::DrawingInstruction::Text(ti),
                                    );
                                    placed = true;
                                }
                            }

                            // Surface geometry: place text at centroid
                            if !placed {
                                for spas in &feature.spatial_associations {
                                    if let Some(surface) = cell.surfaces.get(&spas.spatial_id.key())
                                    {
                                        let ext = collect_ring_points(&surface.exterior_ring);
                                        if ext.len() >= 3 {
                                            let mut cx = 0.0_f64;
                                            let mut cy = 0.0_f64;
                                            let mut area2 = 0.0_f64;
                                            let n = ext.len();
                                            for i in 0..n {
                                                let j = (i + 1) % n;
                                                let cross =
                                                    ext[i].x * ext[j].y - ext[j].x * ext[i].y;
                                                cx += (ext[i].x + ext[j].x) * cross;
                                                cy += (ext[i].y + ext[j].y) * cross;
                                                area2 += cross;
                                            }
                                            if area2.abs() > 1e-15 {
                                                cx /= 3.0 * area2;
                                                cy /= 3.0 * area2;
                                            } else {
                                                cx =
                                                    ext.iter().map(|p| p.x).sum::<f64>() / n as f64;
                                                cy =
                                                    ext.iter().map(|p| p.y).sum::<f64>() / n as f64;
                                            }

                                            let mut ti = RenderTextInstruction::new(
                                                text.clone(),
                                                WorldPoint::new(cx, cy),
                                            )
                                            .with_font_size(*font_size)
                                            .with_color(color)
                                            .with_alignment(
                                                parse_h_align(h_align),
                                                parse_v_align(v_align),
                                            )
                                            .with_rotation(*rotation)
                                            .with_offset(
                                                local_offset.0 as f32,
                                                local_offset.1 as f32,
                                            )
                                            .with_priority(visibility.drawing_priority)
                                            .with_viewing_group(make_viewing_group(visibility))
                                            .with_scale_range(make_scale_range(visibility))
                                            .with_display_plane(make_display_plane(visibility))
                                            .with_feature_id(feature_id.unwrap_or(0));
                                            ti.bold = *bold;
                                            ti.italic = *italic;
                                            ti.color_token = Some(color_token.clone());
                                            context.add_instruction(
                                                ferrite_render::DrawingInstruction::Text(ti),
                                            );
                                            break; // One text label per feature
                                        }
                                    }
                                }
                            }
                        }
                    }
                    DrawingCommand::CoverageFill { visibility, .. } => {
                        // Coverage fills require per-cell attribute grid rendering
                        // Rendered as transparent area placeholder for now
                        area_count += 1;
                        if let Some(feature) = feature {
                            for spas in &feature.spatial_associations {
                                if spas.spatial_id.rcnm != 130 {
                                    continue;
                                }
                                if let Some(surface) = cell.surfaces.get(&spas.spatial_id.key()) {
                                    let (exterior, interiors) = collect_surface_points(surface);
                                    if exterior.len() >= 3 {
                                        area_rendered += 1;
                                        let area_inst = AreaInstruction::new(exterior)
                                            .with_interiors(interiors)
                                            .with_solid_fill(lookup_color("NODTA"))
                                            .with_priority(visibility.drawing_priority)
                                            .with_viewing_group(make_viewing_group(visibility))
                                            .with_scale_range(make_scale_range(visibility))
                                            .with_display_plane(make_display_plane(visibility))
                                            .with_feature_id(feature_id.unwrap_or(0));
                                        context.add_instruction(
                                            ferrite_render::DrawingInstruction::Area(area_inst),
                                        );
                                    }
                                }
                            }
                        }
                    }
                    DrawingCommand::NullInstruction { .. } => {
                        // Feature purposefully not portrayed
                    }
                    DrawingCommand::AlertReference { .. } => {
                        // Alert handling is not part of visual rendering
                    }
                    DrawingCommand::AugmentedPoint { .. }
                    | DrawingCommand::SpatialReference { .. }
                    | DrawingCommand::Dash { .. } => {
                        // State-carrying commands, consumed during parse phase
                    }
                }
            }
        }
    }

    debug!(
        "Cell conversion: {} points, {} lines, {} areas ({} rendered)",
        point_count, line_count, area_count, area_rendered
    );
}
