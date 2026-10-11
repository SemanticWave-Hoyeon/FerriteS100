//! S-101 product adapter: official Lua output to product-neutral display instructions.
//! No window, application settings, file dialogs, or GPU dependency belongs here.
pub mod cancellation;

use ferrite_portrayal_catalog::PortrayalCatalogue;
use ferrite_render::{
    AreaInstruction, Color, HAlign, LineInstruction, LineStyle, PointInstruction, RenderContext,
    TextInstruction as RenderTextInstruction, VAlign, WorldPoint,
};
use ferrite_s100_core::S101Cell;
use tracing::debug;

/// Validate supported geometry before any result in the batch mutates the context.
/// A rejected command must not become a successful, partially drawn feature.
fn preflight_augmented_line_geometry(
    ray: Option<&ferrite_lua::AugmentedRayDef>,
    crs: Option<&ferrite_lua::AugmentedPathCrs>,
    segments: &[ferrite_lua::PathSegment],
    cell: &S101Cell,
    feature_id: &str,
) -> anyhow::Result<()> {
    use ferrite_lua::PathSegment;
    let point_origin = || {
        feature_id
            .split('|')
            .next_back()
            .and_then(|id| id.parse::<i64>().ok())
            .and_then(|id| cell.features.get(&id))
            .and_then(|feature| {
                feature
                    .spatial_associations
                    .iter()
                    .find_map(|association| cell.points.get(&association.spatial_id.key()))
            })
            .map(|point| (point.position.x, point.position.y))
    };
    if let Some(ray) = ray {
        let geographic = ray.direction_crs == "GeographicCRS" && ray.length_crs == "GeographicCRS";
        let screen_units = matches!(ray.length_crs.as_str(), "LocalCRS" | "PortrayalCRS")
            && matches!(
                ray.direction_crs.as_str(),
                "LocalCRS" | "PortrayalCRS" | "GeographicCRS"
            );
        anyhow::ensure!(
            geographic || screen_units,
            "Unsupported AugmentedRay CRS combination: {}/{}",
            ray.direction_crs,
            ray.length_crs
        );
        let (x, y) = point_origin()
            .ok_or_else(|| anyhow::anyhow!("AugmentedRay requires a feature point origin"))?;
        anyhow::ensure!(
            x.is_finite()
                && y.is_finite()
                && ray.direction.is_finite()
                && ray.length.is_finite()
                && ray.length >= 0.,
            "Invalid AugmentedRay origin, direction or length"
        );
        if geographic {
            // Validate the same geodesic operation that conversion will use.
            geographic_ray_points(x, y, ray.direction, ray.length)?;
        }
        return Ok(());
    }
    let Some(crs) = crs else {
        return Ok(());
    };
    // An explicitly active empty path stays empty; it never uses feature geometry.
    if segments.is_empty() {
        return Ok(());
    }
    if crs.crs_position == "LocalCRS"
        && matches!(crs.crs_distance.as_str(), "LocalCRS" | "PortrayalCRS")
        && matches!(
            crs.crs_angle.as_str(),
            "LocalCRS" | "PortrayalCRS" | "GeographicCRS"
        )
    {
        let (x, y) = point_origin().ok_or_else(|| {
            anyhow::anyhow!("AugmentedPath requires a local feature point origin")
        })?;
        anyhow::ensure!(
            x.is_finite() && y.is_finite(),
            "Invalid local feature point origin"
        );
        return Ok(());
    }
    anyhow::ensure!(
        crs.crs_position == "GeographicCRS",
        "Unsupported AugmentedPath CRS combination: {}/{}/{}",
        crs.crs_position,
        crs.crs_angle,
        crs.crs_distance
    );
    for segment in segments {
        match segment {
            PathSegment::Polyline(_) => {}
            PathSegment::Arc3Points { .. } => {
                // Part 9 defines an arbitrary median, but the geographic
                // interpolation model is not implemented. Do not omit this arc.
                anyhow::bail!("Unsupported GeographicCRS Arc3Points interpolation");
            }
            PathSegment::ArcByRadius { .. } | PathSegment::Annulus { .. } => {
                anyhow::ensure!(
                    crs.crs_angle == "GeographicCRS" && crs.crs_distance == "GeographicCRS",
                    "Unsupported mixed CRS geographic arc or annulus: {}/{}",
                    crs.crs_angle,
                    crs.crs_distance
                );
            }
        }
    }
    Ok(())
}

/// Convert Lua portrayal results to drawing instructions for a single cell
/// Uses only this cell's data to avoid feature ID collisions across cells
pub fn convert_lua_results_for_cell(
    results: &[ferrite_lua::PortrayalResult],
    cell: &S101Cell,
    pc: &PortrayalCatalogue,
    context: &mut RenderContext,
    cell_index: usize,
    profile_name: &str,
) -> anyhow::Result<()> {
    use ferrite_lua::DrawingCommand;
    // Reject unresolved identities before mutating the candidate render context.
    for result in results {
        for instruction in &result.instructions {
            for cmd in &instruction.commands {
                if let DrawingCommand::PointInstruction { rotation_crs, .. }
                | DrawingCommand::TextInstruction { rotation_crs, .. } = cmd
                {
                    ferrite_render::RotationCrs::from_lua(rotation_crs)
                        .map_err(anyhow::Error::msg)?;
                }
                match cmd {
                    DrawingCommand::AreaFillReference { area_crs, .. }
                    | DrawingCommand::PixmapFill { area_crs, .. }
                    | DrawingCommand::SymbolFill { area_crs, .. } => {
                        area_crs
                            .parse::<ferrite_render::PatternCrs>()
                            .map_err(anyhow::Error::msg)?;
                    }
                    DrawingCommand::LineInstruction {
                        style_refs,
                        simple_style,
                        augmented_ray,
                        augmented_segments,
                        augmented_crs,
                        ..
                    }
                    | DrawingCommand::LineInstructionUnsuppressed {
                        style_refs,
                        simple_style,
                        augmented_ray,
                        augmented_segments,
                        augmented_crs,
                        ..
                    } => {
                        resolve_line_strokes(style_refs, simple_style.as_ref(), pc, profile_name)?;
                        preflight_augmented_line_geometry(
                            augmented_ray.as_ref(),
                            augmented_crs.as_ref(),
                            augmented_segments,
                            cell,
                            &result.feature_id,
                        )
                        .map_err(|error| {
                            error.context(format!(
                                "Augmented line geometry for feature {}",
                                result.feature_id
                            ))
                        })?;
                    }
                    DrawingCommand::HatchFill {
                        area_crs,
                        direction,
                        distance,
                        line_styles,
                        inline_styles,
                        ..
                    } => {
                        area_crs
                            .parse::<ferrite_render::PatternCrs>()
                            .map_err(anyhow::Error::msg)?;
                        resolve_hatch_strokes(line_styles, inline_styles, pc, profile_name)?;
                        checked_hatch_dimensions(*direction, *distance)?;
                        anyhow::ensure!(
                            (1..=2).contains(&line_styles.len()),
                            "HatchFill requires one or two line styles"
                        );
                    }
                    _ => {}
                }
                if let Some(vis) = cmd.visibility() {
                    if !matches!(cmd, DrawingCommand::NullInstruction { .. }) {
                        if let Some(name) = vis.display_plane.reference() {
                            pc.display_planes.resolve(name)?;
                        }
                    }
                    pc.viewing_groups
                        .resolve_drawing_groups(&vis.viewing_groups, &vis.named_viewing_groups)?;
                }
            }
        }
    }

    // Helper: convert Lua visibility scale fields to ScaleRange
    let make_scale_range = |vis: &ferrite_lua::VisibilityState| -> ferrite_render::ScaleRange {
        ferrite_render::ScaleRange {
            scale_minimum: vis.scale_minimum,
            scale_maximum: vis.scale_maximum,
        }
    };

    // Helper: convert Lua DisplayPlane to render DisplayPlane
    let make_display_plane = |vis: &ferrite_lua::VisibilityState| -> ferrite_render::DisplayPlane {
        match vis.display_plane.reference() {
            Some(name) => ferrite_render::DisplayPlane::from_catalogue_order(
                pc.display_planes
                    .resolve(name)
                    .expect("display plane preflight validated"),
            ),
            // Preserve the application's existing compatibility plane when the
            // Lua initial state has no declared catalogue reference.
            None => ferrite_render::DisplayPlane::default(),
        }
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
        let feature_origin = if let Some(feature) = feature {
            if feature.primitive_type == ferrite_s100_core::SpatialPrimitiveType::Point {
                let point = feature
                    .spatial_associations
                    .iter()
                    .find_map(|association| cell.points.get(&association.spatial_id.key()))
                    .ok_or_else(|| {
                        anyhow::anyhow!("Missing point origin for feature {:?}", feature_id)
                    })?;
                ferrite_render::PortrayalOrigin::feature_point(WorldPoint::new(
                    point.position.x,
                    point.position.y,
                ))?
            } else {
                ferrite_render::PortrayalOrigin::NonPoint
            }
        } else {
            ferrite_render::PortrayalOrigin::Unspecified
        };

        for instruction in &result.instructions {
            for cmd in &instruction.commands {
                let generated_start = context.instruction_count();
                let origin = match cmd {
                    DrawingCommand::PointInstruction {
                        position: Some((x, y)),
                        position_crs,
                        ..
                    }
                    | DrawingCommand::TextInstruction {
                        position: Some((x, y)),
                        position_crs,
                        ..
                    } => {
                        let crs = position_crs.as_deref().ok_or_else(|| {
                            anyhow::anyhow!(
                                "Missing augmented point CRS for feature {:?}",
                                feature_id
                            )
                        })?;
                        let crs = ferrite_render::PointOriginCrs::from_lua(crs)?;
                        if crs == ferrite_render::PointOriginCrs::Local {
                            let anchor = match &feature_origin {
                                ferrite_render::PortrayalOrigin::Point(point) => {
                                    match point.as_ref() {
                                        ferrite_render::PointOriginGeometry::FeaturePoint(
                                            point,
                                        ) => *point,
                                        _ => anyhow::bail!(
                                        "Augmented Local point requires point feature origin: {:?}",
                                        feature_id
                                    ),
                                    }
                                }
                                _ => anyhow::bail!(
                                    "Augmented Local point requires point feature origin: {:?}",
                                    feature_id
                                ),
                            };
                            ferrite_render::PortrayalOrigin::augmented_local_point(
                                anchor,
                                [*x, *y],
                            )?
                        } else {
                            ferrite_render::PortrayalOrigin::augmented_point(crs, [*x, *y])?
                        }
                    }
                    _ => feature_origin.clone(),
                };

                match cmd {
                    DrawingCommand::PointInstruction {
                        symbol_ref,
                        rotation,
                        rotation_crs,
                        scale,
                        position,
                        line_placement,
                        line_placement_visible_parts,
                        spatial_refs,
                        visibility,
                        local_offset,
                        ..
                    } => {
                        point_count += 1;

                        // If explicit position is available (from AugmentedPoint), use it
                        // This is used for Sounding features where each point has specific coordinates
                        if let Some((x, y)) = position {
                            // For soundings: look up depth from multi_points spatial data
                            // The depth is used for decluttering (keep shallowest for safety)
                            let is_geographic = matches!(&origin,
                                ferrite_render::PortrayalOrigin::Point(point) if matches!(point.as_ref(),
                                    ferrite_render::PointOriginGeometry::AugmentedPoint { crs: ferrite_render::PointOriginCrs::Geographic, .. }));
                            let depth = feature.filter(|_| is_geographic).and_then(|f| {
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

                            let (anchor, mm) =
                                local_augmented_placement(&origin, WorldPoint::new(*x, *y));
                            let offset = checked_physical_offset(
                                local_offset.0 + mm[0],
                                local_offset.1 + mm[1],
                            )?;
                            let mut point_inst = PointInstruction::new(symbol_ref.clone(), anchor)
                                .with_rotation(*rotation)
                                .with_rotation_crs(
                                    ferrite_render::RotationCrs::from_lua(rotation_crs)
                                        .map_err(anyhow::Error::msg)?,
                                )
                                .with_scale(*scale)
                                .with_offset(offset[0], offset[1])
                                .with_priority(visibility.drawing_priority)
                                .with_viewing_groups(&pc.viewing_groups.resolve_drawing_groups(
                                    &visibility.viewing_groups,
                                    &visibility.named_viewing_groups,
                                )?)
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

                            if has_curve_geometry || !spatial_refs.is_empty() {
                                if let Some((mode, offset)) = line_placement {
                                    let mode = ferrite_render::LinePlacementMode::from_lua(mode)
                                        .map_err(anyhow::Error::msg)?;
                                    let mut paths: Vec<SpatialLine> = Vec::new();
                                    for mut path in
                                        resolve_feature_line_geometry(cell, feature, spatial_refs)
                                            .map_err(anyhow::Error::msg)?
                                    {
                                        path.points.dedup_by(|a, b| a.x == b.x && a.y == b.y);
                                        if path.points.len() < 2 {
                                            continue;
                                        }
                                        if let Some(last) = paths.last_mut() {
                                            if last.points.last() == path.points.first()
                                                && last.scale_range.scale_minimum
                                                    == path.scale_range.scale_minimum
                                                && last.scale_range.scale_maximum
                                                    == path.scale_range.scale_maximum
                                            {
                                                last.points.extend(path.points.into_iter().skip(1));
                                                continue;
                                            }
                                        }
                                        paths.push(path);
                                    }
                                    for path in paths {
                                        let placement = ferrite_render::LineSymbolPlacement {
                                            points: path.points.into_boxed_slice(),
                                            mode,
                                            offset: *offset,
                                            visible_parts: *line_placement_visible_parts,
                                        };
                                        placement.validate().map_err(anyhow::Error::msg)?;
                                        let point = PointInstruction::new(
                                            symbol_ref.clone(),
                                            placement.points[0],
                                        )
                                        .with_rotation(*rotation)
                                        .with_rotation_crs(
                                            ferrite_render::RotationCrs::from_lua(rotation_crs)
                                                .map_err(anyhow::Error::msg)?,
                                        )
                                        .with_scale(*scale)
                                        .with_offset(local_offset.0 as f32, local_offset.1 as f32)
                                        .with_priority(visibility.drawing_priority)
                                        .with_viewing_groups(
                                            &pc.viewing_groups.resolve_drawing_groups(
                                                &visibility.viewing_groups,
                                                &visibility.named_viewing_groups,
                                            )?,
                                        )
                                        .with_scale_range(intersect_scale_ranges(
                                            make_scale_range(visibility),
                                            path.scale_range,
                                        ))
                                        .with_display_plane(make_display_plane(visibility))
                                        .with_feature_id(feature_id.unwrap_or(0))
                                        .with_cell_index(cell_index)
                                        .with_line_placement(placement);
                                        context.add_instruction(
                                            ferrite_render::DrawingInstruction::Point(point),
                                        );
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
                                        .with_rotation_crs(
                                            ferrite_render::RotationCrs::from_lua(rotation_crs)
                                                .map_err(anyhow::Error::msg)?,
                                        )
                                        .with_scale(*scale)
                                        .with_offset(local_offset.0 as f32, local_offset.1 as f32)
                                        .with_priority(visibility.drawing_priority)
                                        .with_viewing_groups(
                                            &pc.viewing_groups.resolve_drawing_groups(
                                                &visibility.viewing_groups,
                                                &visibility.named_viewing_groups,
                                            )?,
                                        )
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
                                                .with_rotation_crs(
                                                    ferrite_render::RotationCrs::from_lua(
                                                        rotation_crs,
                                                    )
                                                    .map_err(anyhow::Error::msg)?,
                                                )
                                                .with_scale(*scale)
                                                .with_offset(
                                                    local_offset.0 as f32,
                                                    local_offset.1 as f32,
                                                )
                                                .with_priority(visibility.drawing_priority)
                                                .with_viewing_groups(
                                                    &pc.viewing_groups.resolve_drawing_groups(
                                                        &visibility.viewing_groups,
                                                        &visibility.named_viewing_groups,
                                                    )?,
                                                )
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
                        spatial_refs,
                        augmented_ray,
                        augmented_segments,
                        augmented_crs,
                        visibility,
                        ..
                    }
                    | DrawingCommand::LineInstructionUnsuppressed {
                        style_refs,
                        simple_style,
                        spatial_refs,
                        augmented_ray,
                        augmented_segments,
                        augmented_crs,
                        visibility,
                        ..
                    } => {
                        // S-100 Part 9-11.1.9: LineInstructionUnsuppressed cannot be
                        // suppressed by higher-priority lines on the same curve.
                        let unsuppressed =
                            matches!(cmd, DrawingCommand::LineInstructionUnsuppressed { .. });

                        line_count += 1;
                        let strokes = resolve_line_strokes(
                            style_refs,
                            simple_style.as_ref(),
                            pc,
                            profile_name,
                        )?;
                        let mut geometry_instructions = Vec::new();
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
                                // Screen-unit rays are resolved in the active viewport,
                                // never with the cell compilation scale. Geographic rays
                                // use WGS84 ellipsoidal direct geodesics, not a metres/degree
                                // approximation. Preserve the nearby longitude copy.
                                let points = if ray.length_crs == "GeographicCRS"
                                    && ray.direction_crs == "GeographicCRS"
                                {
                                    geographic_ray_points(ox, oy, ray.direction, ray.length)?
                                } else if ray.length_crs == "LocalCRS"
                                    || ray.length_crs == "PortrayalCRS"
                                {
                                    vec![WorldPoint::new(ox, oy), WorldPoint::new(ox, oy)]
                                } else {
                                    anyhow::bail!(
                                        "Unsupported AugmentedRay CRS combination: {}/{}",
                                        ray.direction_crs,
                                        ray.length_crs
                                    );
                                };

                                let mut line_inst = LineInstruction::new(points)
                                    .with_style(strokes[0].clone())
                                    .with_priority(visibility.drawing_priority)
                                    .with_viewing_groups(
                                        &pc.viewing_groups.resolve_drawing_groups(
                                            &visibility.viewing_groups,
                                            &visibility.named_viewing_groups,
                                        )?,
                                    )
                                    .with_scale_range(make_scale_range(visibility))
                                    .with_display_plane(make_display_plane(visibility))
                                    .with_feature_id(feature_id.unwrap_or(0))
                                    .with_cell_index(cell_index);
                                if ray.length_crs == "LocalCRS" || ray.length_crs == "PortrayalCRS"
                                {
                                    line_inst.screen_ray = Some(ferrite_render::ScreenRay {
                                        direction: ray.direction,
                                        length_mm: ray.length,
                                        geographic_direction: ray.direction_crs == "GeographicCRS",
                                    });
                                    line_inst.points =
                                        vec![WorldPoint::new(ox, oy), WorldPoint::new(ox, oy)];
                                }

                                if unsuppressed {
                                    line_inst = line_inst.with_unsuppressed();
                                }

                                geometry_instructions.push(line_inst);
                            }
                        } else if let Some(crs) = augmented_crs {
                            if augmented_segments.is_empty() {
                                continue;
                            }
                            // An active empty path is empty geometry, never feature geometry.
                            let origin = feature.and_then(|f| {
                                f.spatial_associations.iter().find_map(|a| {
                                    cell.points
                                        .get(&a.spatial_id.key())
                                        .map(|p| WorldPoint::new(p.position.x, p.position.y))
                                })
                            });
                            if crs.crs_position == "LocalCRS"
                                && matches!(crs.crs_distance.as_str(), "LocalCRS" | "PortrayalCRS")
                                && matches!(
                                    crs.crs_angle.as_str(),
                                    "LocalCRS" | "PortrayalCRS" | "GeographicCRS"
                                )
                            {
                                if let Some(origin) = origin {
                                    let mut group = Vec::new();
                                    for seg in augmented_segments {
                                        use ferrite_lua::PathSegment as P;
                                        use ferrite_render::PortrayalPath as R;
                                        let geographic = crs.crs_angle == "GeographicCRS";
                                        let paths: Vec<R> = match seg {
                                            P::Polyline(points) => {
                                                vec![R::Polyline(points.clone())]
                                            }
                                            P::Arc3Points { start, median, end } => vec![R::Arc3 {
                                                start: *start,
                                                median: *median,
                                                end: *end,
                                            }],
                                            P::ArcByRadius {
                                                center,
                                                radius,
                                                start_angle,
                                                angular_distance,
                                            } => vec![R::Arc {
                                                center: *center,
                                                radius: *radius,
                                                start: *start_angle,
                                                sweep: *angular_distance,
                                                geographic_angle: geographic,
                                            }],
                                            P::Annulus {
                                                center,
                                                outer_radius,
                                                inner_radius,
                                                start_angle,
                                                angular_distance,
                                            } => {
                                                if !outer_radius.is_finite()
                                                    || !inner_radius.is_finite()
                                                    || *outer_radius < 0.
                                                    || *inner_radius < 0.
                                                    || inner_radius > outer_radius
                                                {
                                                    tracing::error!(
                                                        "Invalid annulus radii: feature {:?}",
                                                        feature_id
                                                    );
                                                    Vec::new()
                                                } else if angular_distance.abs() == 360. {
                                                    let mut p = vec![R::Arc {
                                                        center: *center,
                                                        radius: *outer_radius,
                                                        start: *start_angle,
                                                        sweep: *angular_distance,
                                                        geographic_angle: geographic,
                                                    }];
                                                    if *inner_radius > 0. {
                                                        p.push(R::Arc {
                                                            center: *center,
                                                            radius: *inner_radius,
                                                            start: *start_angle,
                                                            sweep: -*angular_distance,
                                                            geographic_angle: geographic,
                                                        });
                                                    }
                                                    p
                                                } else {
                                                    vec![R::Annulus {
                                                        center: *center,
                                                        outer: *outer_radius,
                                                        inner: *inner_radius,
                                                        start: *start_angle,
                                                        sweep: *angular_distance,
                                                        geographic_angle: geographic,
                                                    }]
                                                }
                                            }
                                        };
                                        group.extend(paths);
                                    }
                                    if !group.is_empty() {
                                        let mut line = LineInstruction::new(vec![origin, origin])
                                            .with_style(strokes[0].clone())
                                            .with_priority(visibility.drawing_priority)
                                            .with_viewing_groups(
                                                &pc.viewing_groups.resolve_drawing_groups(
                                                    &visibility.viewing_groups,
                                                    &visibility.named_viewing_groups,
                                                )?,
                                            )
                                            .with_scale_range(make_scale_range(visibility))
                                            .with_display_plane(make_display_plane(visibility))
                                            .with_feature_id(feature_id.unwrap_or(0))
                                            .with_cell_index(cell_index);
                                        line.portrayal_path =
                                            Some(ferrite_render::PortrayalPath::Group(group));
                                        if unsuppressed {
                                            line = line.with_unsuppressed();
                                        }
                                        geometry_instructions.push(line);
                                    }
                                } else {
                                    anyhow::bail!(
                                        "AugmentedPath requires local origin: feature {:?}",
                                        feature_id
                                    );
                                }
                            } else if crs.crs_position == "GeographicCRS" {
                                for seg in augmented_segments {
                                    if let ferrite_lua::PathSegment::Polyline(points) = seg {
                                        let mut line = LineInstruction::new(
                                            points
                                                .iter()
                                                .map(|p| WorldPoint::new(p.0, p.1))
                                                .collect(),
                                        )
                                        .with_style(strokes[0].clone())
                                        .with_priority(visibility.drawing_priority)
                                        .with_viewing_groups(
                                            &pc.viewing_groups.resolve_drawing_groups(
                                                &visibility.viewing_groups,
                                                &visibility.named_viewing_groups,
                                            )?,
                                        )
                                        .with_scale_range(make_scale_range(visibility))
                                        .with_display_plane(make_display_plane(visibility))
                                        .with_feature_id(feature_id.unwrap_or(0))
                                        .with_cell_index(cell_index);
                                        if unsuppressed {
                                            line = line.with_unsuppressed();
                                        }
                                        geometry_instructions.push(line);
                                    } else if let ferrite_lua::PathSegment::ArcByRadius {
                                        center,
                                        radius,
                                        start_angle,
                                        angular_distance,
                                    } = seg
                                    {
                                        if crs.crs_angle != "GeographicCRS"
                                            || crs.crs_distance != "GeographicCRS"
                                        {
                                            anyhow::bail!("Unsupported mixed CRS geographic arc: feature {:?}", feature_id);
                                        }
                                        // Geographic X/Y follows the same chart contract as
                                        // the adjacent existing Polyline adapter. Retain the
                                        // metric arc for active-viewport tessellation/picking.
                                        let center_point = WorldPoint::new(center.0, center.1);
                                        let mut line =
                                            LineInstruction::new(vec![center_point, center_point])
                                                .with_style(strokes[0].clone())
                                                .with_priority(visibility.drawing_priority)
                                                .with_viewing_groups(
                                                    &pc.viewing_groups.resolve_drawing_groups(
                                                        &visibility.viewing_groups,
                                                        &visibility.named_viewing_groups,
                                                    )?,
                                                )
                                                .with_scale_range(make_scale_range(visibility))
                                                .with_display_plane(make_display_plane(visibility))
                                                .with_feature_id(feature_id.unwrap_or(0))
                                                .with_cell_index(cell_index);
                                        line.portrayal_path =
                                            Some(ferrite_render::PortrayalPath::GeographicArc {
                                                center: *center,
                                                radius_m: *radius,
                                                start: *start_angle,
                                                sweep: *angular_distance,
                                            });
                                        if unsuppressed {
                                            line = line.with_unsuppressed();
                                        }
                                        geometry_instructions.push(line);
                                    } else if let ferrite_lua::PathSegment::Annulus {
                                        center,
                                        outer_radius,
                                        inner_radius,
                                        start_angle,
                                        angular_distance,
                                    } = seg
                                    {
                                        anyhow::ensure!(
                                            crs.crs_angle == "GeographicCRS"
                                                && crs.crs_distance == "GeographicCRS",
                                            "Unsupported mixed CRS geographic annulus"
                                        );
                                        let point = WorldPoint::new(center.0, center.1);
                                        let mut line = LineInstruction::new(vec![point, point])
                                            .with_style(strokes[0].clone())
                                            .with_priority(visibility.drawing_priority)
                                            .with_viewing_groups(
                                                &pc.viewing_groups.resolve_drawing_groups(
                                                    &visibility.viewing_groups,
                                                    &visibility.named_viewing_groups,
                                                )?,
                                            )
                                            .with_scale_range(make_scale_range(visibility))
                                            .with_display_plane(make_display_plane(visibility))
                                            .with_feature_id(feature_id.unwrap_or(0))
                                            .with_cell_index(cell_index);
                                        line.portrayal_path = Some(
                                            ferrite_render::PortrayalPath::GeographicAnnulus {
                                                center: *center,
                                                outer: *outer_radius,
                                                inner: *inner_radius,
                                                start: *start_angle,
                                                sweep: *angular_distance,
                                            },
                                        );
                                        if unsuppressed {
                                            line = line.with_unsuppressed();
                                        }
                                        geometry_instructions.push(line);
                                    } else {
                                        anyhow::bail!("Unsupported GeographicCRS Arc3Points interpolation for feature {:?}", feature_id);
                                    }
                                }
                            } else {
                                anyhow::bail!(
                                    "Unsupported AugmentedPath CRS {:?}: feature {:?}",
                                    crs,
                                    feature_id
                                );
                            }
                        } else if let Some(feature) = feature {
                            match resolve_feature_line_geometry(cell, feature, spatial_refs) {
                                Ok(lines) => {
                                    for geometry in lines {
                                        let mut line_inst = LineInstruction::new(geometry.points)
                                            .with_style(strokes[0].clone())
                                            .with_priority(visibility.drawing_priority)
                                            .with_viewing_groups(
                                                &pc.viewing_groups.resolve_drawing_groups(
                                                    &visibility.viewing_groups,
                                                    &visibility.named_viewing_groups,
                                                )?,
                                            )
                                            .with_scale_range(intersect_scale_ranges(
                                                make_scale_range(visibility),
                                                geometry.scale_range,
                                            ))
                                            .with_display_plane(make_display_plane(visibility))
                                            .with_feature_id(feature_id.unwrap_or(0))
                                            .with_cell_index(cell_index);
                                        if unsuppressed {
                                            line_inst = line_inst.with_unsuppressed();
                                        }
                                        geometry_instructions.push(line_inst);
                                    }
                                }
                                Err(error) => tracing::error!(
                                    "Feature {:?} line geometry: {}",
                                    feature_id,
                                    error
                                ),
                            }
                        }
                        add_line_strokes(context, geometry_instructions, &strokes);
                    }
                    DrawingCommand::ColorFill {
                        color_token,
                        visibility,
                        transparency,
                        ..
                    } => {
                        area_count += 1;
                        let color = lookup_color(color_token);
                        let opacity = 1.0 - *transparency as f32;
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
                                            .with_solid_fill_token_opacity(
                                                color,
                                                color_token,
                                                opacity,
                                            )
                                            .with_priority(draw_priority)
                                            .with_viewing_groups(
                                                &pc.viewing_groups.resolve_drawing_groups(
                                                    &visibility.viewing_groups,
                                                    &visibility.named_viewing_groups,
                                                )?,
                                            )
                                            .with_scale_range(make_scale_range(visibility))
                                            .with_display_plane(make_display_plane(visibility))
                                            .with_feature_id(feature_id.unwrap_or(0))
                                            .with_cell_index(cell_index);
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
                        area_crs,
                        visibility,
                        ..
                    } => {
                        let pattern_crs = area_crs
                            .parse::<ferrite_render::PatternCrs>()
                            .map_err(anyhow::Error::msg)?;
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
                                            .with_pattern_crs(pattern_crs)
                                            .with_interiors(interiors.clone())
                                            .with_priority(draw_priority)
                                            .with_viewing_groups(
                                                &pc.viewing_groups.resolve_drawing_groups(
                                                    &visibility.viewing_groups,
                                                    &visibility.named_viewing_groups,
                                                )?,
                                            )
                                            .with_scale_range(make_scale_range(visibility))
                                            .with_display_plane(make_display_plane(visibility))
                                            .with_feature_id(feature_id.unwrap_or(0))
                                            .with_cell_index(cell_index);

                                        let mut area_inst = area_inst;
                                        area_inst.fill_ref = Some(reference.clone());
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
                        area_crs,
                        visibility,
                        ..
                    } => {
                        let pattern_crs = area_crs
                            .parse::<ferrite_render::PatternCrs>()
                            .map_err(anyhow::Error::msg)?;
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
                                            .with_pattern_crs(pattern_crs)
                                            .with_interiors(interiors.clone())
                                            .with_priority(draw_priority)
                                            .with_viewing_groups(
                                                &pc.viewing_groups.resolve_drawing_groups(
                                                    &visibility.viewing_groups,
                                                    &visibility.named_viewing_groups,
                                                )?,
                                            )
                                            .with_scale_range(make_scale_range(visibility))
                                            .with_display_plane(make_display_plane(visibility))
                                            .with_feature_id(feature_id.unwrap_or(0))
                                            .with_cell_index(cell_index);

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
                        area_crs,
                        clip_symbols,
                        visibility,
                        ..
                    } => {
                        let pattern_crs = area_crs
                            .parse::<ferrite_render::PatternCrs>()
                            .map_err(anyhow::Error::msg)?;
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
                                            .with_pattern_crs(pattern_crs)
                                            .with_pattern_clip_symbols(*clip_symbols)
                                            .with_interiors(interiors)
                                            .with_pattern_fill(symbol.clone(), v1f, v2f)
                                            .with_priority(visibility.drawing_priority)
                                            .with_viewing_groups(
                                                &pc.viewing_groups.resolve_drawing_groups(
                                                    &visibility.viewing_groups,
                                                    &visibility.named_viewing_groups,
                                                )?,
                                            )
                                            .with_scale_range(make_scale_range(visibility))
                                            .with_display_plane(make_display_plane(visibility))
                                            .with_feature_id(feature_id.unwrap_or(0))
                                            .with_cell_index(cell_index);
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
                        inline_styles,
                        area_crs,
                        visibility,
                        ..
                    } => {
                        let pattern_crs = area_crs
                            .parse::<ferrite_render::PatternCrs>()
                            .map_err(anyhow::Error::msg)?;
                        area_count += 1;
                        let strokes =
                            resolve_hatch_strokes(line_styles, inline_styles, pc, profile_name)?;
                        let first = strokes
                            .first()
                            .ok_or_else(|| anyhow::anyhow!("HatchFill has no strokes"))?;
                        let color = first.style.color;
                        let line_width_mm = first.style.width;
                        let hatch_token = first.style.color_token.clone().unwrap_or_default();
                        let (angle, spacing_mm) = checked_hatch_dimensions(*direction, *distance)?;
                        anyhow::ensure!(
                            (1..=2).contains(&line_styles.len()),
                            "HatchFill requires one or two line styles"
                        );
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
                                            .with_pattern_crs(pattern_crs)
                                            .with_hatch_line_style_refs(line_styles)
                                            .with_hatch_strokes(strokes.clone())
                                            .with_interiors(interiors)
                                            .with_hatch_fill_token(
                                                color,
                                                &hatch_token,
                                                line_width_mm,
                                                spacing_mm,
                                                angle,
                                            )
                                            .with_priority(visibility.drawing_priority)
                                            .with_viewing_groups(
                                                &pc.viewing_groups.resolve_drawing_groups(
                                                    &visibility.viewing_groups,
                                                    &visibility.named_viewing_groups,
                                                )?,
                                            )
                                            .with_scale_range(make_scale_range(visibility))
                                            .with_display_plane(make_display_plane(visibility))
                                            .with_feature_id(feature_id.unwrap_or(0))
                                            .with_cell_index(cell_index);
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
                        font_weight,
                        font_proportion,
                        serifs,
                        underline,
                        strikethrough,
                        upperline,
                        font_reference,
                        bold,
                        italic,
                        h_align,
                        v_align,
                        rotation,
                        rotation_crs,
                        local_offset,
                        position,
                        visibility,
                        vertical_offset,
                        scale_factor,
                        color_transparency,
                        bg_color_token,
                        bg_transparency,
                        ..
                    } => {
                        let font_style = ferrite_render::TextFontStyle {
                            weight: ferrite_render::TextFontWeight::from_lua(font_weight)
                                .map_err(anyhow::Error::msg)?,
                            proportion: ferrite_render::TextFontProportion::from_lua(
                                font_proportion,
                            )
                            .map_err(anyhow::Error::msg)?,
                            serifs: *serifs,
                            underline: *underline,
                            strikethrough: *strikethrough,
                            upperline: *upperline,
                            reference: (!font_reference.is_empty()).then(|| font_reference.clone()),
                        };
                        let color = lookup_color(color_token);
                        let opacity = 1.0 - *color_transparency as f32;
                        let background = if bg_color_token.is_empty() {
                            None
                        } else {
                            Some(lookup_color(bg_color_token))
                        };
                        let background_opacity = 1.0 - *bg_transparency as f32;
                        // If explicit position from AugmentedPoint, use it
                        if let Some((x, y)) = position {
                            let (anchor, mm) =
                                local_augmented_placement(&origin, WorldPoint::new(*x, *y));
                            let offset = checked_physical_offset(
                                local_offset.0 + mm[0],
                                local_offset.1 + vertical_offset + mm[1],
                            )?;
                            let mut text_inst = RenderTextInstruction::new(text.clone(), anchor)
                                .with_font_size(*font_size * *scale_factor as f32)
                                .with_color_token_opacity(color, color_token, opacity)
                                .with_alignment(parse_h_align(h_align), parse_v_align(v_align))
                                .with_rotation(*rotation)
                                .with_rotation_crs(
                                    ferrite_render::RotationCrs::from_lua(rotation_crs)
                                        .map_err(anyhow::Error::msg)?,
                                )
                                .with_offset(offset[0], offset[1])
                                .with_priority(visibility.drawing_priority)
                                .with_viewing_groups(&pc.viewing_groups.resolve_drawing_groups(
                                    &visibility.viewing_groups,
                                    &visibility.named_viewing_groups,
                                )?)
                                .with_scale_range(make_scale_range(visibility))
                                .with_display_plane(make_display_plane(visibility))
                                .with_feature_id(feature_id.unwrap_or(0))
                                .with_cell_index(cell_index);
                            text_inst.font_style = font_style.clone();
                            text_inst.bold = *bold;
                            text_inst.italic = *italic;
                            if let Some(background) = background {
                                text_inst = text_inst.with_background_token_opacity(
                                    background,
                                    bg_color_token,
                                    background_opacity,
                                );
                            }
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
                                    .with_font_size(*font_size * *scale_factor as f32)
                                    .with_color_token_opacity(color, color_token, opacity)
                                    .with_alignment(parse_h_align(h_align), parse_v_align(v_align))
                                    .with_rotation(*rotation)
                                    .with_rotation_crs(
                                        ferrite_render::RotationCrs::from_lua(rotation_crs)
                                            .map_err(anyhow::Error::msg)?,
                                    )
                                    .with_offset(
                                        local_offset.0 as f32,
                                        (local_offset.1 + vertical_offset) as f32,
                                    )
                                    .with_priority(visibility.drawing_priority)
                                    .with_viewing_groups(
                                        &pc.viewing_groups.resolve_drawing_groups(
                                            &visibility.viewing_groups,
                                            &visibility.named_viewing_groups,
                                        )?,
                                    )
                                    .with_scale_range(make_scale_range(visibility))
                                    .with_display_plane(make_display_plane(visibility))
                                    .with_feature_id(feature_id.unwrap_or(0))
                                    .with_cell_index(cell_index);
                                    ti.font_style = font_style.clone();
                                    ti.bold = *bold;
                                    ti.italic = *italic;
                                    if let Some(background) = background {
                                        ti = ti.with_background_token_opacity(
                                            background,
                                            bg_color_token,
                                            background_opacity,
                                        );
                                    }
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
                                            .with_font_size(*font_size * *scale_factor as f32)
                                            .with_color_token_opacity(color, color_token, opacity)
                                            .with_alignment(
                                                parse_h_align(h_align),
                                                parse_v_align(v_align),
                                            )
                                            .with_rotation(*rotation)
                                            .with_rotation_crs(
                                                ferrite_render::RotationCrs::from_lua(rotation_crs)
                                                    .map_err(anyhow::Error::msg)?,
                                            )
                                            .with_offset(
                                                local_offset.0 as f32,
                                                (local_offset.1 + vertical_offset) as f32,
                                            )
                                            .with_priority(visibility.drawing_priority)
                                            .with_viewing_groups(
                                                &pc.viewing_groups.resolve_drawing_groups(
                                                    &visibility.viewing_groups,
                                                    &visibility.named_viewing_groups,
                                                )?,
                                            )
                                            .with_scale_range(make_scale_range(visibility))
                                            .with_display_plane(make_display_plane(visibility))
                                            .with_feature_id(feature_id.unwrap_or(0))
                                            .with_cell_index(cell_index);
                                            ti.font_style = font_style.clone();
                                            ti.bold = *bold;
                                            ti.italic = *italic;
                                            if let Some(background) = background {
                                                ti = ti.with_background_token_opacity(
                                                    background,
                                                    bg_color_token,
                                                    background_opacity,
                                                );
                                            }
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
                                            .with_viewing_groups(
                                                &pc.viewing_groups.resolve_drawing_groups(
                                                    &visibility.viewing_groups,
                                                    &visibility.named_viewing_groups,
                                                )?,
                                            )
                                            .with_scale_range(make_scale_range(visibility))
                                            .with_display_plane(make_display_plane(visibility))
                                            .with_feature_id(feature_id.unwrap_or(0))
                                            .with_cell_index(cell_index);
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
                context.set_portrayal_origin_from(generated_start, origin);
                if let Some(visibility) = cmd.visibility() {
                    context.set_time_intervals_from(generated_start, &visibility.time_intervals);
                    context.set_dependency_from(
                        generated_start,
                        ferrite_render::DrawingDependency::new(
                            cell_index as u64,
                            visibility.id.as_deref(),
                            visibility.parent.as_deref(),
                            visibility.hover,
                        ),
                    );
                }
            }
        }
    }

    debug!(
        "Cell conversion: {} points, {} lines, {} areas ({} rendered)",
        point_count, line_count, area_count, area_rendered
    );
    Ok(())
}

/// Resolve a catalogue colour token using the selected colour profile.
pub fn lookup_pc_color(pc: &PortrayalCatalogue, token: &str, profile_name: &str) -> Color {
    // Try to get the specified profile, fallback to default or first available
    let profile = pc
        .color_profiles
        .profiles
        .get(profile_name)
        .or_else(|| {
            pc.color_profiles
                .default_profile
                .as_ref()
                .and_then(|name| pc.color_profiles.profiles.get(name))
        })
        .or_else(|| pc.color_profiles.profiles.values().next());

    if let Some(profile) = profile {
        if let Some(srgb) = profile.get_srgb(token) {
            return Color::rgb(
                srgb.r as f32 / 255.0,
                srgb.g as f32 / 255.0,
                srgb.b as f32 / 255.0,
            );
        }
    }

    // Color not found in PC - log warning and return gray
    tracing::warn!("Color token '{}' not found in PC color profile", token);
    Color::rgb(0.5, 0.5, 0.5)
}

mod pick_report;
pub use pick_report::pick_report_attributes;

mod line_geometry;
pub use line_geometry::{intersect_scale_ranges, resolve_feature_line_geometry, SpatialLine};

mod interoperability;
pub use interoperability::{
    plan_interoperability, plan_interoperability_for_cells, InteroperabilityPlan,
};

mod catalogue_compatibility;
pub use catalogue_compatibility::{validate_catalogue_pair, validate_dataset_catalogues};

mod spatial_scale_admission;
pub use spatial_scale_admission::validate_spatial_scale_properties;

mod context_validation;
pub use context_validation::{
    context_validation_parameters, synchronize_legacy_context, validate_portrayal_context,
};

mod display_selection;
pub use display_selection::{
    optional_viewing_layers, resolve_display_mode, viewing_groups_for_layers,
    viewing_groups_for_preset, DisplayPreset,
};

/// Geographic ray positions for S-100 Part 9a AugmentedRay. The returned endpoint
/// is on the WGS84 ellipsoid; rendering projections stay outside this adapter.
fn geographic_ray_points(
    lon: f64,
    lat: f64,
    bearing: f64,
    metres: f64,
) -> anyhow::Result<Vec<WorldPoint>> {
    anyhow::ensure!(
        metres.is_finite() && metres >= 0.,
        "Invalid geographic ray length"
    );
    use ferrite_kernel::geodesy::{direct, GeographicPosition};
    let start = GeographicPosition::new(lat, lon)?;
    let end = direct(start, bearing, metres)?;
    Ok(vec![
        WorldPoint::new(lon, lat),
        WorldPoint::new(end.longitude_near(lon)?, end.latitude()),
    ])
}

#[cfg(test)]
mod geographic_ray_tests {
    use super::*;
    #[test]
    fn geographic_ray_uses_reference_geodesic_and_preserves_dateline_copy() {
        let points = geographic_ray_points(-73.78, 40.64, 45., 10_000_000.).unwrap();
        assert!((points[1].x - 49.052_487_092_959_836).abs() < 1e-10);
        assert!((points[1].y - 32.621_100_463_725_796).abs() < 1e-10);
        let points = geographic_ray_points(179.9, 0., 90., 30_000.).unwrap();
        assert!(points[1].x > 180. && points[1].x < 181.);
        let points = geographic_ray_points(0., 90., 180., 100_000.).unwrap();
        assert!(points[1].y < 90. && points[1].y > 89.);
        assert!(geographic_ray_points(0., 0., 0., -1.).is_err());
        assert!(geographic_ray_points(0., 91., 0., 1.).is_err());
    }
}

pub mod coverage_scale;

pub mod coverage_geometry;
pub mod coverage_loading;
pub mod coverage_projection;

// Adapter-only conversion: authored augmented position participates in coverage
// origin; LocalOffset and VerticalOffset affect glyph layout, never that origin.
fn local_augmented_placement(
    origin: &ferrite_render::PortrayalOrigin,
    authored: ferrite_render::WorldPoint,
) -> (ferrite_render::WorldPoint, [f64; 2]) {
    if let ferrite_render::PortrayalOrigin::Point(point) = origin {
        if let ferrite_render::PointOriginGeometry::AugmentedLocalPoint {
            reference_point,
            millimetres,
        } = point.as_ref()
        {
            return (*reference_point, *millimetres);
        }
    }
    (authored, [0., 0.])
}
fn checked_physical_offset(x: f64, y: f64) -> anyhow::Result<[f32; 2]> {
    anyhow::ensure!(
        [x, y].iter().all(|v| v.is_finite()),
        "Non-finite physical displacement"
    );
    let offset = [x as f32, y as f32];
    anyhow::ensure!(
        offset.iter().all(|v| v.is_finite()),
        "Physical displacement exceeds renderer range"
    );
    Ok(offset)
}
#[cfg(test)]
mod local_position_adapter_tests {
    use super::*;
    #[test]
    fn reference_point_and_physical_position_are_not_interchanged() {
        let anchor = ferrite_render::WorldPoint::new(127., 35.);
        let origin =
            ferrite_render::PortrayalOrigin::augmented_local_point(anchor, [3.2, -1.5]).unwrap();
        assert_eq!(
            local_augmented_placement(&origin, ferrite_render::WorldPoint::new(3.2, -1.5)),
            (anchor, [3.2, -1.5])
        );
        assert_eq!(
            checked_physical_offset(3.2 + 1., -1.5 + 2.).unwrap(),
            [4.2, 0.5]
        );
        assert!(checked_physical_offset(f64::MAX, 0.).is_err());
        assert!(checked_physical_offset(0., f64::NAN).is_err());
        // Adding glyph displacement must not mutate source geometry.
        let ferrite_render::PortrayalOrigin::Point(p) = origin else {
            panic!()
        };
        assert!(
            matches!(*p, ferrite_render::PointOriginGeometry::AugmentedLocalPoint { reference_point, millimetres } if reference_point == anchor && millimetres == [3.2, -1.5])
        );
    }
}

/// Keep valid authored millimetres exactly (subject only to the renderer's f32
/// representation); never substitute a minimum 0.5 mm hatch distance.
fn checked_hatch_dimensions(direction: (f64, f64), distance: f64) -> anyhow::Result<(f32, f32)> {
    anyhow::ensure!(
        direction.0.is_finite()
            && direction.1.is_finite()
            && (direction.0 != 0. || direction.1 != 0.),
        "Invalid HatchFill direction"
    );
    anyhow::ensure!(
        distance.is_finite() && distance > 0.,
        "Invalid HatchFill distance"
    );
    let spacing = distance as f32;
    anyhow::ensure!(
        spacing.is_finite() && spacing > 0.,
        "HatchFill distance exceeds renderer range"
    );
    Ok((direction.1.atan2(direction.0).to_degrees() as f32, spacing))
}
#[cfg(test)]
mod hatch_adapter_tests {
    use super::*;
    #[test]
    fn sub_half_mm_spacing_is_preserved_and_invalid_dimensions_rejected() {
        assert_eq!(
            checked_hatch_dimensions((1., 1.), 0.125).unwrap(),
            (45., 0.125)
        );
        assert_eq!(checked_hatch_dimensions((0., 1.), 2.).unwrap(), (90., 2.));
        for distance in [
            0.,
            -1.,
            f64::NAN,
            f64::INFINITY,
            f64::MAX,
            f64::MIN_POSITIVE,
        ] {
            assert!(checked_hatch_dimensions((1., 0.), distance).is_err());
        }
        for direction in [(0., 0.), (f64::NAN, 1.), (1., f64::INFINITY)] {
            assert!(checked_hatch_dimensions(direction, 1.).is_err());
        }
    }
    #[test]
    fn pattern_origin_and_both_line_references_survive_instruction_creation() {
        let refs = vec!["FIRST".to_string(), "SECOND".to_string()];
        let a = ferrite_render::AreaInstruction::new(vec![])
            .with_pattern_crs(ferrite_render::PatternCrs::LocalGeometry)
            .with_hatch_line_style_refs(&refs);
        let b = a.clone();
        assert_eq!(b.pattern_crs, ferrite_render::PatternCrs::LocalGeometry);
        assert_eq!(&*b.hatch_line_style_refs, &refs);
    }
}

fn apply_inline_stroke_metadata(
    style: &mut LineStyle,
    definition: Option<&ferrite_kernel::StrokeDefinition>,
) {
    if let Some(definition) = definition {
        style.cap = match definition.cap {
            ferrite_kernel::StrokeCap::Butt => ferrite_render::CapStyle::Butt,
            ferrite_kernel::StrokeCap::Round => ferrite_render::CapStyle::Round,
            ferrite_kernel::StrokeCap::Square => ferrite_render::CapStyle::Square,
        };
        style.join = match definition.join {
            ferrite_kernel::StrokeJoin::Miter => ferrite_render::JoinStyle::Miter,
            ferrite_kernel::StrokeJoin::Round => ferrite_render::JoinStyle::Round,
            ferrite_kernel::StrokeJoin::Bevel => ferrite_render::JoinStyle::Bevel,
        };
        style.offset_mm = definition.offset_mm;
        style.interval_length_mm = definition.interval_length_mm;
        style.authored_symbols = definition.symbols.clone();
    }
}

// Resolve geometry once and paint every piece of one stroke before the next.
// This avoids a later piece's background stroke overwriting an earlier overlay.
fn add_line_strokes(
    context: &mut RenderContext,
    lines: Vec<LineInstruction>,
    strokes: &[LineStyle],
) {
    if lines.is_empty() {
        return;
    }
    for style in &strokes[..strokes.len() - 1] {
        for line in &lines {
            let mut part = line.clone();
            part.style = style.clone();
            part.color_token = style.color_token.clone();
            context.add_instruction(ferrite_render::DrawingInstruction::Line(part));
        }
    }
    let style = strokes.last().unwrap();
    for mut line in lines {
        line.style = style.clone();
        line.color_token = style.color_token.clone();
        context.add_instruction(ferrite_render::DrawingInstruction::Line(line));
    }
}

fn resolve_line_strokes(
    references: &[String],
    inline: Option<&ferrite_kernel::StrokeDefinition>,
    pc: &PortrayalCatalogue,
    profile: &str,
) -> anyhow::Result<Vec<LineStyle>> {
    anyhow::ensure!(
        (1..=64).contains(&references.len()),
        "Invalid LineInstruction style references"
    );
    let mut snapshots = vec![None; references.len()];
    snapshots[0] = inline.cloned();
    let strokes = resolve_strokes(references, &snapshots, pc, profile)?;
    Ok(strokes
        .into_iter()
        .map(|mut s| {
            s.style.authored_symbols = s.symbols.into_vec();
            s.style
        })
        .collect())
}

fn resolve_hatch_strokes(
    references: &[String],
    inline_styles: &[Option<ferrite_kernel::StrokeDefinition>],
    pc: &PortrayalCatalogue,
    profile: &str,
) -> anyhow::Result<Vec<ferrite_render::HatchStroke>> {
    anyhow::ensure!(
        (1..=2).contains(&references.len()),
        "Invalid HatchFill style references"
    );
    resolve_strokes(references, inline_styles, pc, profile)
}

fn resolve_strokes(
    references: &[String],
    inline_styles: &[Option<ferrite_kernel::StrokeDefinition>],
    pc: &PortrayalCatalogue,
    profile: &str,
) -> anyhow::Result<Vec<ferrite_render::HatchStroke>> {
    use ferrite_portrayal_catalog::LineStyle as PcStyle;
    anyhow::ensure!(
        (1..=64).contains(&references.len()) && references.len() == inline_styles.len(),
        "Invalid HatchFill style snapshots"
    );
    let mut strokes = Vec::new();
    for (reference, inline) in references.iter().zip(inline_styles) {
        if let Some(definition) = inline {
            anyhow::ensure!(
                definition.width_mm.is_finite()
                    && definition.width_mm >= 0.
                    && definition.transparency.is_finite()
                    && (0. ..=1.).contains(&definition.transparency),
                "Invalid inline HatchFill stroke dimensions/opacity"
            );
            anyhow::ensure!(
                definition.offset_mm.is_finite()
                    && definition.interval_length_mm.is_finite()
                    && definition.interval_length_mm >= 0.
                    && definition.symbols.len() <= 4096,
                "Invalid inline HatchFill metadata"
            );
            for symbol in &definition.symbols {
                anyhow::ensure!(
                    !symbol.reference.is_empty()
                        && symbol.position_mm.is_finite()
                        && symbol.rotation_degrees.is_finite()
                        && symbol.scale_factor.is_finite(),
                    "Invalid inline HatchFill symbol"
                );
            }
            if let Some(dash) = &definition.dash_cycle {
                let canonical = ferrite_kernel::DashCycle::new(
                    dash.period,
                    dash.intervals
                        .iter()
                        .map(|&(start, end)| (start, end - start)),
                )
                .map_err(anyhow::Error::msg)?;
                anyhow::ensure!(
                    canonical == *dash,
                    "Noncanonical inline HatchFill dash cycle"
                );
            }
            anyhow::ensure!(strokes.len() < 64, "HatchFill stroke budget exceeded");
            let mut style = LineStyle::solid_mm(
                lookup_pc_color(pc, &definition.color_token, profile)
                    .with_alpha(1. - definition.transparency),
                definition.width_mm,
            );
            style.color_token = Some(definition.color_token.clone());
            style.opacity = 1. - definition.transparency;
            style.dash_cycle = definition.dash_cycle.clone();
            apply_inline_stroke_metadata(&mut style, Some(definition));
            strokes.push(ferrite_render::HatchStroke {
                interval_length_mm: definition.interval_length_mm,
                style,
                symbols: definition.symbols.clone().into_boxed_slice(),
            });
            continue;
        }
        let resource = pc
            .line_styles
            .get(reference)
            .ok_or_else(|| anyhow::anyhow!("Missing HatchFill line style: {reference}"))?;
        let parts: &[ferrite_portrayal_catalog::SimpleLineStyle] = match resource {
            PcStyle::Simple(s) => std::slice::from_ref(s),
            PcStyle::Complex(c) => &c.strokes,
            PcStyle::Composite(c) => &c.components,
        };
        anyhow::ensure!(
            !parts.is_empty() && strokes.len() + parts.len() <= 64,
            "HatchFill stroke budget/empty resource"
        );
        for part in parts {
            anyhow::ensure!(
                part.pen.width.is_finite()
                    && part.pen.width >= 0.
                    && part.pen.width <= f32::MAX as f64,
                "Invalid HatchFill pen width"
            );
            anyhow::ensure!(
                part.interval_length.is_finite() && part.interval_length >= 0.,
                "Invalid HatchFill interval length"
            );
            let mut style = LineStyle::solid_mm(
                lookup_pc_color(pc, &part.pen.color_token, profile),
                part.pen.width as f32,
            );
            style.color_token = Some(part.pen.color_token.clone());
            anyhow::ensure!(part.offset_mm.is_finite(), "Invalid HatchFill offset");
            style.offset_mm = part.offset_mm;
            style.interval_length_mm = part.interval_length;
            style.dash_cycle = part.dash_cycle().map_err(anyhow::Error::msg)?;
            style.cap = match part.pen.cap_style {
                ferrite_portrayal_catalog::CapStyle::Butt => ferrite_render::CapStyle::Butt,
                ferrite_portrayal_catalog::CapStyle::Round => ferrite_render::CapStyle::Round,
                ferrite_portrayal_catalog::CapStyle::Square => ferrite_render::CapStyle::Square,
            };
            style.join = match part.pen.join_style {
                ferrite_portrayal_catalog::JoinStyle::Miter => ferrite_render::JoinStyle::Miter,
                ferrite_portrayal_catalog::JoinStyle::Round => ferrite_render::JoinStyle::Round,
                ferrite_portrayal_catalog::JoinStyle::Bevel => ferrite_render::JoinStyle::Bevel,
            };
            anyhow::ensure!(
                part.symbols.len() <= 4096,
                "HatchFill symbol budget exceeded"
            );
            let symbols = part
                .symbols
                .iter()
                .map(|s| {
                    anyhow::ensure!(
                        !s.reference.is_empty()
                            && s.position.is_finite()
                            && s.rotation.is_finite()
                            && s.scale_factor.is_finite(),
                        "Invalid HatchFill line symbol"
                    );
                    Ok(ferrite_render::HatchLineSymbol {
                        reference: s.reference.clone(),
                        position_mm: s.position,
                        rotation_degrees: s.rotation,
                        crs: s.crs_type,
                        scale_factor: s.scale_factor,
                    })
                })
                .collect::<anyhow::Result<Vec<_>>>()?
                .into_boxed_slice();
            strokes.push(ferrite_render::HatchStroke {
                style,
                interval_length_mm: part.interval_length,
                symbols,
            });
        }
    }
    Ok(strokes)
}

#[cfg(test)]
mod hatch_resource_tests {
    use super::*;
    fn catalogue() -> PortrayalCatalogue {
        use ferrite_portrayal_catalog::*;
        PortrayalCatalogue {
            root_path: Default::default(),
            product_id: String::new(),
            version: String::new(),
            color_profiles: ColorProfiles::new(),
            symbols: Symbols::new(std::path::PathBuf::new()),
            line_styles: Default::default(),
            area_fills: Default::default(),
            viewing_groups: ViewingGroups::new(),
            viewing_group_layers: ViewingGroupLayers::new(),
            display_modes: DisplayModes::new(),
            display_planes: DisplayPlanes::default(),
            foundation_mode: Vec::new(),
            rules: PortrayalRules::new(std::path::PathBuf::new()),
        }
    }
    fn empty_plane_cell() -> S101Cell {
        use ferrite_s100_core::*;
        S101Cell {
            file_path: Default::default(),
            dsid: Default::default(),
            code_mappings: DatasetCodeMappings::new(),
            coord_factor: 1.,
            coord_factor_y: 1.,
            coord_factor_z: 1.,
            coord_origin_x: 0.,
            coord_origin_y: 0.,
            coord_origin_z: 0.,
            minimum_display_scale: None,
            maximum_display_scale: None,
            points: Default::default(),
            multi_points: Default::default(),
            curves: Default::default(),
            composite_curves: Default::default(),
            surfaces: Default::default(),
            features: Default::default(),
            information: Default::default(),
            spatial_information_associations: Default::default(),
        }
    }
    fn surface_text_fixture() -> S101Cell {
        use ferrite_s100_core::*;
        let mut cell = empty_plane_cell();
        let curve_id = RecordId::new(120, 1);
        let surface_id = RecordId::new(130, 1);
        cell.curves.insert(
            curve_id.key(),
            CurveRecord {
                id: curve_id,
                segments: vec![CurveSegment {
                    segment_type: SegmentType::Line,
                    positions: [(0., 0.), (1., 0.), (1., 1.), (0., 1.), (0., 0.)]
                        .into_iter()
                        .map(|(x, y)| Coordinate::new(x, y))
                        .collect(),
                }],
                start_point: None,
                end_point: None,
                update_instruction: 0,
            },
        );
        cell.surfaces.insert(
            surface_id.key(),
            SurfaceRecord {
                id: surface_id,
                exterior_ring: vec![OrientedCurve {
                    curve_id,
                    orientation: true,
                }],
                interior_rings: vec![],
                update_instruction: 0,
            },
        );
        cell.features.insert(
            42,
            FeatureRecord {
                frid: FRID {
                    rcid: 42,
                    nftc: 0,
                    rver: 1,
                    ruin: 0,
                },
                foid: None,
                attributes: vec![],
                spatial_associations: vec![SpatialAssociation {
                    spatial_id: surface_id,
                    ornt: 1,
                    usag: 1,
                    mask: 0,
                    scale_minimum: None,
                    scale_maximum: None,
                    update_instruction: 0,
                }],
                information_associations: vec![],
                feature_associations: vec![],
                masks: vec![],
                feature_code: None,
                primitive_type: SpatialPrimitiveType::Surface,
            },
        );
        cell
    }
    #[test]
    fn all_three_text_placement_branches_keep_full_font_metadata() {
        use ferrite_s100_core::*;
        for placement in [0, 1, 2] {
            let mut cell = surface_text_fixture();
            if placement == 1 {
                let id = RecordId::new(110, 9);
                cell.points.insert(
                    id.key(),
                    PointRecord {
                        id,
                        position: Coordinate::new(2., 3.),
                        update_instruction: 0,
                    },
                );
                let feature = cell.features.get_mut(&42).unwrap();
                feature.primitive_type = SpatialPrimitiveType::Point;
                feature.spatial_associations[0].spatial_id = id;
            }
            let position = if placement == 0 {
                "AugmentedPoint:GeographicCRS,2,3;"
            } else {
                ""
            };
            let result = ferrite_lua::PortrayalResult::parse("42", &format!("{position}FontWeight:Light;FontProportion:MonoSpaced;FontSerifs:true;FontUnderline:true;FontStrikethrough:true;FontUpperline:true;FontReference:Font-A;TextInstruction:depth"), "").unwrap();
            let mut context = RenderContext::new(ferrite_render::Viewport::new(800., 600.));
            convert_lua_results_for_cell(&[result], &cell, &catalogue(), &mut context, 3, "Day")
                .unwrap();
            assert_eq!(context.raw_instructions().len(), 1);
            let ferrite_render::DrawingInstruction::Text(text) = &context.raw_instructions()[0]
            else {
                panic!()
            };
            assert_eq!(
                text.font_style.weight,
                ferrite_render::TextFontWeight::Light
            );
            assert_eq!(
                text.font_style.proportion,
                ferrite_render::TextFontProportion::MonoSpaced
            );
            assert!(
                text.font_style.serifs
                    && text.font_style.underline
                    && text.font_style.strikethrough
                    && text.font_style.upperline
            );
            assert_eq!(text.font_style.reference.as_deref(), Some("Font-A"));
            assert_eq!(text.cell_index, Some(3));
            assert_eq!(text.feature_id, Some(42));
        }
    }
    #[test]
    fn augmented_geometry_preflight_rejects_entire_batch_before_mutation() {
        let cell = surface_text_fixture();
        let pc = catalogue();
        let parse = |script: &str| ferrite_lua::PortrayalResult::parse("42", script, "").unwrap();
        let valid = parse("Polyline:0,0,1,1;AugmentedPath:GeographicCRS,GeographicCRS,GeographicCRS;LineStyle:L,,0.32,CHBLK;LineInstruction:L");
        for (geometry, crs, expected) in [
            (
                "Arc3Points:0,0,1,1,2,0",
                "GeographicCRS,GeographicCRS,GeographicCRS",
                "Arc3Points",
            ),
            (
                "ArcByRadius:0,0,10",
                "GeographicCRS,LocalCRS,GeographicCRS",
                "mixed CRS",
            ),
            (
                "Annulus:0,0,10,5",
                "GeographicCRS,GeographicCRS,LocalCRS",
                "mixed CRS",
            ),
            (
                "Polyline:0,0,1,1",
                "PortrayalCRS,LocalCRS,LocalCRS",
                "CRS combination",
            ),
            (
                "Polyline:0,0,1,1",
                "LocalCRS,LocalCRS,LocalCRS",
                "point origin",
            ),
        ] {
            for command in ["LineInstruction", "LineInstructionUnsuppressed"] {
                let rejected = parse(&format!(
                    "{geometry};AugmentedPath:{crs};LineStyle:L,,0.32,CHBLK;{command}:L"
                ));
                let mut context = RenderContext::new(ferrite_render::Viewport::new(800., 600.));
                convert_lua_results_for_cell(
                    std::slice::from_ref(&valid),
                    &cell,
                    &pc,
                    &mut context,
                    3,
                    "Day",
                )
                .unwrap();
                let before = serde_json::to_value(context.raw_instructions()).unwrap();
                let owner = context.static_instruction_order_identity();
                let error = convert_lua_results_for_cell(
                    &[valid.clone(), rejected],
                    &cell,
                    &pc,
                    &mut context,
                    3,
                    "Day",
                )
                .unwrap_err();
                let message = format!("{error:#}");
                assert!(
                    message.contains("feature 42") && message.contains(expected),
                    "{message}"
                );
                assert_eq!(
                    before,
                    serde_json::to_value(context.raw_instructions()).unwrap()
                );
                assert!(std::sync::Arc::ptr_eq(
                    &owner,
                    &context.static_instruction_order_identity()
                ));
            }
        }
    }

    #[test]
    fn ray_preflight_catches_missing_origin_and_unsupported_metre_direction() {
        let cell = surface_text_fixture();
        let pc = catalogue();
        for (ray, expected) in [
            ("GeographicCRS,90,LocalCRS,10", "point origin"),
            ("LocalCRS,90,GeographicCRS,10", "CRS combination"),
        ] {
            let valid = ferrite_lua::PortrayalResult::parse("42", "Polyline:0,0,1,1;AugmentedPath:GeographicCRS,GeographicCRS,GeographicCRS;LineStyle:L,,0.32,CHBLK;LineInstruction:L", "").unwrap();
            let rejected = ferrite_lua::PortrayalResult::parse(
                "42",
                &format!("AugmentedRay:{ray};LineStyle:L,,0.32,CHBLK;LineInstruction:L"),
                "",
            )
            .unwrap();
            let mut context = RenderContext::new(ferrite_render::Viewport::new(800., 600.));
            let error = convert_lua_results_for_cell(
                &[valid, rejected],
                &cell,
                &pc,
                &mut context,
                3,
                "Day",
            )
            .unwrap_err();
            assert!(format!("{error:#}").contains(expected));
            assert!(context.raw_instructions().is_empty());
        }
    }

    #[test]
    fn explicit_empty_local_path_needs_no_origin_and_never_draws_feature_geometry() {
        let cell = surface_text_fixture();
        let pc = catalogue();
        let result = ferrite_lua::PortrayalResult::parse(
            "42",
            "AugmentedPath:LocalCRS,LocalCRS,LocalCRS;LineStyle:L,,0.32,CHBLK;LineInstruction:L",
            "",
        )
        .unwrap();
        let mut context = RenderContext::new(ferrite_render::Viewport::new(800., 600.));
        convert_lua_results_for_cell(&[result], &cell, &pc, &mut context, 3, "Day").unwrap();
        assert!(context.raw_instructions().is_empty());
    }

    #[test]
    fn lua_symbol_fill_retains_clip_policy_geometry_and_source() {
        let cell = surface_text_fixture();
        let pc = catalogue();
        for (suffix, expected) in [(",false", false), (",true", true), ("", true)] {
            let mut context = RenderContext::new(ferrite_render::Viewport::new(800., 600.));
            let result = ferrite_lua::PortrayalResult::parse(
                "42",
                &format!("AreaCRS:LocalGeometry;SymbolFill:ASYM,-4,0,2,4{suffix}"),
                "",
            )
            .unwrap();
            convert_lua_results_for_cell(&[result], &cell, &pc, &mut context, 3, "Day").unwrap();
            assert_eq!(context.raw_instructions().len(), 1);
            let ferrite_render::DrawingInstruction::Area(area) = &context.raw_instructions()[0]
            else {
                panic!("expected surface symbol fill")
            };
            assert_eq!(area.pattern_clip_symbols, expected);
            assert_eq!(area.cell_index, Some(3));
            assert_eq!(area.feature_id, Some(42));
            assert_eq!(area.pattern_crs, ferrite_render::PatternCrs::LocalGeometry);
            // collect_surface_points removes the repeated closing endpoint;
            // preserve all four authored corners in the same order.
            assert_eq!(
                area.exterior.iter().map(|p| (p.x, p.y)).collect::<Vec<_>>(),
                [(0., 0.), (1., 0.), (1., 1.), (0., 1.)]
            );
            let ferrite_render::AreaFillType::Pattern { symbol_ref, v1, v2 } = &area.fill else {
                panic!("expected retained lattice")
            };
            assert_eq!(symbol_ref, "ASYM");
            assert_eq!(*v1, (-4., 0.));
            assert_eq!(*v2, (2., 4.));
            // An older JSON instruction lacking the new option must retain the standard default.
            let mut json = serde_json::to_value(area).unwrap();
            json.as_object_mut().unwrap().remove("pattern_clip_symbols");
            let restored: ferrite_render::AreaInstruction = serde_json::from_value(json).unwrap();
            assert!(restored.pattern_clip_symbols);
        }
    }
    #[test]
    fn geographic_annulus_coverage_origin_follows_actual_feature_not_rendered_center() {
        use ferrite_s100_core::*;
        let pc = catalogue();
        let mut cell = surface_text_fixture();
        let script = "Annulus:179.9,50,50000,25000,35,360;AugmentedPath:GeographicCRS,GeographicCRS,GeographicCRS;LineStyle:L,,0.32,CHBLK;LineInstructionUnsuppressed:L";
        let convert = |cell: &S101Cell, id: &str| {
            let result = ferrite_lua::PortrayalResult::parse(id, script, "").unwrap();
            let mut context = RenderContext::new(ferrite_render::Viewport::new(800., 600.));
            convert_lua_results_for_cell(&[result], cell, &pc, &mut context, 3, "Day").unwrap();
            assert_eq!(context.raw_instructions().len(), 1);
            context.raw_instructions()[0].portrayal_origin().clone()
        };
        assert_eq!(
            convert(&cell, "42"),
            ferrite_render::PortrayalOrigin::NonPoint
        );
        assert_eq!(
            convert(&cell, "43"),
            ferrite_render::PortrayalOrigin::Unspecified
        );
        let point_id = RecordId::new(110, 9);
        cell.points.insert(
            point_id.key(),
            PointRecord {
                id: point_id,
                position: Coordinate::new(2., 3.),
                update_instruction: 0,
            },
        );
        let feature = cell.features.get_mut(&42).unwrap();
        feature.primitive_type = SpatialPrimitiveType::Point;
        feature.spatial_associations[0].spatial_id = point_id;
        assert_eq!(
            convert(&cell, "42"),
            ferrite_render::PortrayalOrigin::feature_point(WorldPoint::new(2., 3.)).unwrap()
        );
    }
    #[test]
    fn geographic_annulus_actual_commands_retain_source_order_scale_and_separate_rings() {
        let pc = catalogue();
        let cell = surface_text_fixture();
        let scaler = ferrite_render::Scaler::new(
            ferrite_render::GeoBounds::new(178.9, 49., 180.9, 51.),
            ferrite_render::Viewport::new(800., 600.),
        );
        for (inner, sweep, expected_runs) in [
            (25_000., 360_f64, 2),
            (0., 360., 1),
            (25_000., -270., 1),
            (0., 270., 1),
        ] {
            let instructions=format!("DrawingPriority:7;ScaleMinimum:200000;ScaleMaximum:10000;Annulus:179.9,50,50000,{inner},35,{sweep};AugmentedPath:GeographicCRS,GeographicCRS,GeographicCRS;LineStyle:L,,0.32,CHBLK;LineInstructionUnsuppressed:L");
            let result = ferrite_lua::PortrayalResult::parse("42", &instructions, "").unwrap();
            let mut context = RenderContext::new(ferrite_render::Viewport::new(800., 600.));
            convert_lua_results_for_cell(&[result], &cell, &pc, &mut context, 3, "Day").unwrap();
            assert_eq!(context.raw_instructions().len(), 1);
            assert_eq!(
                *context.raw_instructions()[0].portrayal_origin(),
                ferrite_render::PortrayalOrigin::NonPoint
            );
            let ferrite_render::DrawingInstruction::Line(line) = &context.raw_instructions()[0]
            else {
                panic!("annulus remains authored line")
            };
            assert_eq!(line.cell_index, Some(3));
            assert_eq!(line.feature_id, Some(42));
            assert_eq!(line.priority.0, 7);
            assert!(matches!(
                line.portrayal_path,
                Some(ferrite_render::PortrayalPath::GeographicAnnulus {
                    center: (179.9, 50.),
                    outer: 50_000.,
                    ..
                })
            ));
            assert!(!line.suppressible);
            assert_eq!(line.scale_range.scale_minimum, Some(200000));
            assert_eq!(line.scale_range.scale_maximum, Some(10000));
            for scale in [10000, 50000, 200000] {
                assert!(line.scale_range.is_visible_at(scale));
            }
            for scale in [9999, 200001] {
                assert!(!line.scale_range.is_visible_at(scale));
            }
            let paths = line.render_paths(&scaler).collect::<Vec<_>>();
            assert_eq!(paths.len(), expected_runs);
            assert!(paths.iter().all(|p| p.first() == p.last() && p.len() > 2));
            use ferrite_kernel::geodesy::{direct, GeographicPosition};
            let center = GeographicPosition::new(50., 179.9).unwrap();
            let query = |radius| {
                let p = direct(center, 35., radius).unwrap();
                scaler.world_to_screen(WorldPoint::new(
                    p.longitude_near(179.9).unwrap(),
                    p.latitude(),
                ))
            };
            let command = &context.raw_instructions()[0];
            assert!(ferrite_render::hit_geometry(command, &scaler, query(50000.), 3.).is_some());
            if inner > 0. {
                assert!(ferrite_render::hit_geometry(command, &scaler, query(inner), 3.).is_some());
            }
            let ray_mid = (inner + 50000.) * 0.5;
            assert_eq!(
                ferrite_render::hit_geometry(command, &scaler, query(ray_mid), 3.).is_some(),
                sweep.abs() != 360.,
                "full ring must not invent a radial selection edge"
            );
        }
    }

    #[test]
    fn geographic_radius_arc_reaches_retained_geometry_with_order_and_source() {
        let pc = catalogue();
        let cell = empty_plane_cell();
        let mut context = RenderContext::new(ferrite_render::Viewport::new(800., 600.));
        let result=ferrite_lua::PortrayalResult::parse("42","DrawingPriority:7;ArcByRadius:179.9,50,50000,35,-270;AugmentedPath:GeographicCRS,GeographicCRS,GeographicCRS;LineStyle:L,,0.32,CHBLK;LineInstruction:L","").unwrap();
        convert_lua_results_for_cell(&[result], &cell, &pc, &mut context, 3, "Day").unwrap();
        assert_eq!(context.raw_instructions().len(), 1);
        let ferrite_render::DrawingInstruction::Line(line) = &context.raw_instructions()[0] else {
            panic!("arc must remain a line")
        };
        assert_eq!(line.cell_index, Some(3));
        assert_eq!(line.feature_id, Some(42));
        assert!(matches!(
            line.portrayal_path,
            Some(ferrite_render::PortrayalPath::GeographicArc {
                center: (179.9, 50.),
                radius_m: 50_000.,
                start: 35.,
                sweep: -270.
            })
        ));
        let scaler = ferrite_render::Scaler::new(
            ferrite_render::GeoBounds::new(178., 49., 181., 51.),
            ferrite_render::Viewport::new(800., 600.),
        );
        assert!(line.render_points(&scaler).len() > 2);
    }
    #[test]
    fn named_pc_orders_reach_common_instructions_and_unknown_name_is_atomic() {
        let mut pc = catalogue();
        for (id, order) in [("OverRadar", -701), ("underRadar", 90000)] {
            pc.display_planes
                .planes
                .insert(id.into(), std::num::NonZeroI32::new(order).unwrap());
        }
        let cell = empty_plane_cell();
        let mut ctx = RenderContext::new(ferrite_render::Viewport::new(200., 100.));
        let good=ferrite_lua::PortrayalResult::parse("f", "AugmentedPoint:GeographicCRS,1,2;DisplayPlane:OverRadar;PointInstruction:A;DisplayPlane:underRadar;PointInstruction:B", "").unwrap();
        convert_lua_results_for_cell(&[good], &cell, &pc, &mut ctx, 0, "Day").unwrap();
        assert_eq!(ctx.raw_instructions().len(), 2);
        let orders: Vec<_> = ctx
            .raw_instructions()
            .iter()
            .map(|i| i.display_plane().order().get())
            .collect();
        assert_eq!(orders, [-701, 90000]);
        let before = serde_json::to_vec(ctx.raw_instructions()).unwrap();
        let bad=ferrite_lua::PortrayalResult::parse("f", "AugmentedPoint:GeographicCRS,1,2;DisplayPlane:OverRadar;PointInstruction:A;DisplayPlane:UnderRadar;PointInstruction:B", "").unwrap();
        assert!(convert_lua_results_for_cell(&[bad], &cell, &pc, &mut ctx, 0, "Day").is_err());
        assert_eq!(before, serde_json::to_vec(ctx.raw_instructions()).unwrap());
        let null = ferrite_lua::PortrayalResult::parse(
            "f",
            "DisplayPlane:Unregistered;NullInstruction",
            "",
        )
        .unwrap();
        convert_lua_results_for_cell(&[null], &cell, &pc, &mut ctx, 0, "Day").unwrap();
        assert_eq!(before, serde_json::to_vec(ctx.raw_instructions()).unwrap());
    }
    #[test]
    fn split_curve_components_are_painted_as_complete_layers() {
        let mut background = LineStyle::solid_mm(Color::default(), 1.28);
        background.color_token = Some("BG".into());
        let mut overlay = LineStyle::solid_mm(Color::default(), 0.64);
        overlay.color_token = Some("FG".into());
        let lines = (0..3)
            .map(|i| {
                LineInstruction::new(vec![
                    WorldPoint::new(i as f64, 0.),
                    WorldPoint::new(i as f64 + 1., 0.),
                ])
                .with_feature_id(i)
            })
            .collect();
        let mut context = RenderContext::new(ferrite_render::Viewport::new(200., 100.));
        add_line_strokes(&mut context, lines, &[background, overlay]);
        let sequence: Vec<_> = context
            .raw_instructions()
            .iter()
            .map(|i| {
                let ferrite_render::DrawingInstruction::Line(line) = i else {
                    panic!()
                };
                (
                    line.color_token.as_deref().unwrap(),
                    line.feature_id.unwrap(),
                )
            })
            .collect();
        assert_eq!(
            sequence,
            vec![
                ("BG", 0),
                ("BG", 1),
                ("BG", 2),
                ("FG", 0),
                ("FG", 1),
                ("FG", 2)
            ]
        );
    }
    #[test]
    fn generic_strokes_keep_order_geometry_and_source_identity() {
        let mut styles = Vec::new();
        for (token, width, offset) in [("A", 1.28, -1.), ("B", 0.64, 2.)] {
            let mut style = LineStyle::solid_mm(Color::default(), width);
            style.color_token = Some(token.into());
            style.offset_mm = offset;
            styles.push(style);
        }
        let points = vec![WorldPoint::new(-1., 50.), WorldPoint::new(-1.01, 50.01)];
        let line = LineInstruction::new(points.clone())
            .with_feature_id(123)
            .with_cell_index(7)
            .with_unsuppressed();
        let mut context = RenderContext::new(ferrite_render::Viewport::new(200., 100.));
        add_line_strokes(&mut context, vec![line], &styles);
        assert_eq!(context.raw_instructions().len(), 2);
        for (index, instruction) in context.raw_instructions().iter().enumerate() {
            let ferrite_render::DrawingInstruction::Line(line) = instruction else {
                panic!()
            };
            assert_eq!(line.points, points);
            assert_eq!(line.feature_id, Some(123));
            assert_eq!(line.cell_index, Some(7));
            assert!(!line.suppressible);
            assert_eq!(line.color_token, styles[index].color_token);
            assert_eq!(line.style.offset_mm, styles[index].offset_mm);
            assert_eq!(line.style.width, styles[index].width);
        }
    }
    #[test]
    fn both_catalogue_styles_expand_all_strokes_with_dashes_caps_and_symbols() {
        use ferrite_portrayal_catalog as pc;
        let mut catalogue = catalogue();
        let stroke = |token: &str, width: f64| pc::SimpleLineStyle {
            id: token.into(),
            interval_length: 5.,
            offset_mm: 0.,
            pen: pc::Pen {
                width,
                color_token: token.into(),
                cap_style: pc::CapStyle::Square,
                join_style: pc::JoinStyle::Bevel,
            },
            dashes: vec![pc::Dash {
                start: 1.,
                length: 2.,
            }],
            symbols: vec![pc::LineSymbol {
                reference: "SYMBOL".into(),
                position: -4.,
                rotation: 30.,
                crs_type: ferrite_kernel::LineSymbolCrs::LineCRS,
                scale_factor: -1.5,
            }],
        };
        catalogue.line_styles.insert(
            "complex".into(),
            pc::LineStyle::Complex(pc::ComplexLineStyle {
                id: "complex".into(),
                strokes: vec![stroke("A", 0.64), stroke("B", 0.32)],
            }),
        );
        catalogue.line_styles.insert(
            "composite".into(),
            pc::LineStyle::Composite(pc::CompositeLineStyle {
                id: "composite".into(),
                components: vec![stroke("C", 0.2), stroke("D", 0.1)],
            }),
        );
        let result = resolve_hatch_strokes(
            &["complex".into(), "composite".into()],
            &[None, None],
            &catalogue,
            "Day",
        )
        .unwrap();
        let generic = resolve_line_strokes(
            &["complex".into(), "composite".into(), "complex".into()],
            None,
            &catalogue,
            "Day",
        )
        .unwrap();
        assert_eq!(generic.len(), 6);
        assert_eq!(
            generic
                .iter()
                .map(|s| s.color_token.as_deref().unwrap())
                .collect::<Vec<_>>(),
            vec!["A", "B", "C", "D", "A", "B"]
        );
        assert!(generic.iter().all(|s| s.authored_symbols.len() == 1
            && s.authored_symbols[0].rotation_degrees == 30.
            && s.dash_cycle.as_ref().unwrap().intervals == vec![(1., 3.)]));
        assert_eq!(result.len(), 4);
        for (i, token) in ["A", "B", "C", "D"].into_iter().enumerate() {
            assert_eq!(result[i].style.color_token.as_deref(), Some(token));
            assert_eq!(
                result[i].style.dash_cycle.as_ref().unwrap().intervals,
                vec![(1., 3.)]
            );
            assert_eq!(result[i].style.cap, ferrite_render::CapStyle::Square);
            assert_eq!(result[i].style.join, ferrite_render::JoinStyle::Bevel);
            assert_eq!(result[i].symbols[0].reference, "SYMBOL");
            assert_eq!(result[i].symbols[0].position_mm, -4.);
            assert_eq!(result[i].symbols[0].rotation_degrees, 30.);
            assert_eq!(
                result[i].symbols[0].crs,
                ferrite_kernel::LineSymbolCrs::LineCRS
            );
            assert_eq!(result[i].symbols[0].scale_factor, -1.5);
        }
        assert_eq!(
            result.iter().map(|s| s.style.width).collect::<Vec<_>>(),
            vec![0.64, 0.32, 0.2, 0.1]
        );
    }
    #[test]
    fn lua_named_hatch_style_snapshots_keep_both_opacity_and_independent_dashes() {
        let command=ferrite_lua::parse_instruction_string("1","Dash:1,2;LineStyle:base,5,0.64,A,0.25;Dash:2,1;LineStyle:overlay,7,0.32,B,0.5;HatchFill:1,0,1,base,overlay;LineStyle:base,,9,C").unwrap();
        let ferrite_lua::DrawingCommand::HatchFill {
            line_styles,
            inline_styles,
            ..
        } = command
            .commands
            .iter()
            .find(|c| matches!(c, ferrite_lua::DrawingCommand::HatchFill { .. }))
            .unwrap()
        else {
            panic!()
        };
        let result =
            resolve_hatch_strokes(line_styles, inline_styles, &catalogue(), "Day").unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].style.width, 0.64);
        assert_eq!(result[1].style.width, 0.32);
        assert_eq!(result[0].style.opacity, 0.75);
        assert_eq!(result[1].style.opacity, 0.5);
        assert_eq!(result[0].style.dash_cycle.as_ref().unwrap().period, 5.);
        assert_eq!(result[1].style.dash_cycle.as_ref().unwrap().period, 7.);
        assert_eq!(result[0].style.color_token.as_deref(), Some("A"));
        let mut area = ferrite_render::AreaInstruction::new(vec![]).with_hatch_strokes(result);
        area.fill_opacity = 0.4;
        let mut instruction = ferrite_render::DrawingInstruction::Area(area);
        instruction.remap_colors(&|token| {
            if token == "A" {
                Color::rgba(1., 0., 0., 0.8)
            } else {
                Color::rgba(0., 1., 0., 0.6)
            }
        });
        let ferrite_render::DrawingInstruction::Area(area) = instruction else {
            panic!()
        };
        assert!((area.hatch_strokes[0].style.color.a - 0.8 * 0.75).abs() < 1e-6);
        assert!((area.hatch_strokes[1].style.color.a - 0.6 * 0.5).abs() < 1e-6);
    }
    #[test]
    fn inline_hatch_metadata_reaches_common_render_model_without_loss() {
        let p=ferrite_lua::parse_instruction_string("1", "LineSymbol:R,1,30,PortrayalCRS,1.5;LineStyle:L,5,0.32,A,0.25,Square,Bevel,-0.4;HatchFill:1,0,3,L").unwrap();
        let ferrite_lua::DrawingCommand::HatchFill {
            line_styles,
            inline_styles,
            ..
        } = &p.commands[0]
        else {
            panic!()
        };
        let result =
            resolve_hatch_strokes(line_styles, inline_styles, &catalogue(), "Day").unwrap();
        let stroke = &result[0];
        assert_eq!(stroke.style.cap, ferrite_render::CapStyle::Square);
        assert_eq!(stroke.style.join, ferrite_render::JoinStyle::Bevel);
        assert_eq!(stroke.style.offset_mm, -0.4);
        assert_eq!(stroke.interval_length_mm, 5.);
        assert_eq!(
            stroke.symbols.as_ref(),
            inline_styles[0].as_ref().unwrap().symbols
        );
        assert_eq!(
            stroke.style.authored_symbols,
            inline_styles[0].as_ref().unwrap().symbols
        );
        let mut bad = inline_styles.clone();
        bad[0].as_mut().unwrap().symbols[0].scale_factor = f64::NAN;
        assert!(resolve_hatch_strokes(line_styles, &bad, &catalogue(), "Day").is_err());
    }
    #[test]
    fn missing_second_style_and_empty_complex_style_fail_instead_of_fallback() {
        let mut catalogue = catalogue();
        catalogue.line_styles.insert(
            "empty".into(),
            ferrite_portrayal_catalog::LineStyle::Complex(Default::default()),
        );
        let inline = ferrite_kernel::StrokeDefinition {
            width_mm: 1.,
            color_token: "A".into(),
            transparency: 0.,
            dash_cycle: None,
            ..Default::default()
        };
        assert!(resolve_hatch_strokes(
            &["inline".into(), "absent".into()],
            &[Some(inline), None],
            &catalogue,
            "Day"
        )
        .is_err());
        assert!(resolve_hatch_strokes(&["empty".into()], &[None], &catalogue, "Day").is_err());
    }
}

// Optionality is a product-profile property, not implied by a rule hash or
// fill style. A custom catalogue can keep the same script while making this
// group foundational or adding it to a display-mode layer. Unknown semantics
// must leave the original commands visible.
fn shallow_selector_is_independent(pc: &ferrite_portrayal_catalog::PortrayalCatalogue) -> bool {
    if pc.product_id != "S-101" {
        return false;
    }
    let Some(group) = pc.viewing_groups.runtime_id("90000") else {
        return false;
    };
    if pc.foundation_mode.contains(&group) {
        return false;
    }
    let Some(layer) = pc.viewing_group_layers.layers.get("900") else {
        return false;
    };
    if layer.viewing_group_ids.as_slice() != [group] || pc.display_modes.modes.is_empty() {
        return false;
    }
    // Reject alternate membership as unknown even if it is currently omitted
    // by every mode: this is no longer the audited independent selector shape.
    if pc
        .viewing_group_layers
        .layers
        .iter()
        .any(|(id, layer)| id != "900" && layer.viewing_group_ids.contains(&group))
    {
        return false;
    }
    !pc.display_modes
        .modes
        .values()
        .any(|mode| mode.viewing_group_layers.iter().any(|id| id == "900"))
}

/// Current audited S-101 independent shallow-water selector profile. Unknown PC
/// rules retain every pattern; the host must report that this toggle is unavailable.
pub fn shallow_pattern_contract(
    pc: &ferrite_portrayal_catalog::BoundPortrayalCatalogue,
) -> Option<ferrite_render::ShallowPatternContract> {
    if !shallow_selector_is_independent(pc) {
        return None;
    }
    let known = [
        0x0f, 0xe2, 0x04, 0xc6, 0x15, 0x7b, 0xeb, 0xd6, 0x1a, 0xf1, 0xd5, 0x1f, 0x98, 0x16, 0xb1,
        0x36, 0x68, 0x3a, 0x1f, 0xad, 0x7c, 0xe7, 0x20, 0x03, 0x2f, 0xd9, 0x94, 0x29, 0x3a, 0x53,
        0xae, 0x9b,
    ];
    let fill = pc.get_area_fill("DIAMOND1")?;
    let ferrite_portrayal_catalog::AreaFillType::Symbol(s) = &fill.fill_type else {
        return None;
    };
    if s.symbol_ref != "DIAMOND1P"
        || s.area_crs != "GlobalGeometry"
        || (s.v1.x, s.v1.y) != (22.5, 0.)
        || (s.v2.x, s.v2.y) != (0., 43.13)
    {
        return None;
    }
    ferrite_render::ShallowPatternContract::from_bound_selector(
        pc,
        std::path::Path::new("Rules/SEABED01.lua"),
        known,
        "900",
        "90000",
        "DIAMOND1",
        9,
        ferrite_render::DisplayPlane::UnderRadar,
    )
}

#[cfg(test)]
mod shallow_pattern_catalogue_tests {
    #[test]
    fn parsed_snapshot_requires_independent_nonfoundation_selector() {
        let root = std::env::temp_dir().join(format!(
            "ferrite-shallow-independence-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let xml = |foundation: &str, mode: &str, extra: &str| {
            format!(
            "<portrayalCatalog productId='S-101' version='2.0.0'><foundationMode>{foundation}</foundationMode><displayPlanes><displayPlane id='UnderRadar' order='-1'/></displayPlanes><viewingGroups><viewingGroup id='90000'><description><name>Shallow pattern</name></description></viewingGroup></viewingGroups><viewingGroupLayers><viewingGroupLayer id='900'><viewingGroup>90000</viewingGroup></viewingGroupLayer>{extra}</viewingGroupLayers><displayModes><displayMode id='StandardDisplay'><name>Standard</name>{mode}</displayMode></displayModes></portrayalCatalog>")
        };
        let original = xml("", "", "");
        std::fs::write(root.join("portrayal_catalogue.xml"), &original).unwrap();
        let retained = ferrite_portrayal_catalog::PortrayalCatalogue::load_bound(&root).unwrap();
        assert!(super::shallow_selector_is_independent(&retained));
        for mutated in [xml("<viewingGroup>90000</viewingGroup>","",""),
            xml("","<viewingGroupLayer>900</viewingGroupLayer>",""),
            xml("","","<viewingGroupLayer id='custom'><viewingGroup>90000</viewingGroup></viewingGroupLayer>")] {
            std::fs::write(root.join("portrayal_catalogue.xml"),mutated).unwrap();
            let pc=ferrite_portrayal_catalog::PortrayalCatalogue::load_bound(&root).unwrap();
            assert_ne!(retained.source_digest(),pc.source_digest());
            assert!(!super::shallow_selector_is_independent(&pc));
            assert!(super::shallow_pattern_contract(&pc).is_none());
        }
        assert!(super::shallow_selector_is_independent(&retained));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "requires immutable official PC input via FERRITE_SHALLOW_PC; mutates owned copy only"]
    fn official_same_rule_fill_and_layer_cannot_override_foundation_or_display_modes() {
        let source = std::path::PathBuf::from(std::env::var("FERRITE_SHALLOW_PC").unwrap());
        let original = ferrite_portrayal_catalog::PortrayalCatalogue::load_bound(&source).unwrap();
        assert!(super::shallow_pattern_contract(&original).is_some());
        let root = std::env::temp_dir().join(format!(
            "ferrite-shallow-official-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        // All bytes come from the bounded retained map, not a second live read.
        let mut pending = vec![source.clone()];
        while let Some(directory) = pending.pop() {
            for entry in std::fs::read_dir(directory).unwrap() {
                let entry = entry.unwrap();
                let kind = entry.file_type().unwrap();
                assert!(!kind.is_symlink());
                let path = entry.path();
                let relative = path.strip_prefix(&source).unwrap();
                let target = root.join(relative);
                if kind.is_dir() {
                    std::fs::create_dir_all(target).unwrap();
                    pending.push(path);
                } else {
                    assert!(kind.is_file());
                    let bytes = original.sources().read_relative(relative).unwrap();
                    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
                    std::fs::write(target, bytes.as_ref()).unwrap();
                }
            }
        }
        let file = root.join("portrayal_catalogue.xml");
        let xml = std::fs::read_to_string(&file).unwrap();
        let copied = ferrite_portrayal_catalog::PortrayalCatalogue::load_bound(&root).unwrap();
        assert!(super::shallow_pattern_contract(&copied).is_some());
        for (closing, insertion) in [
            ("</foundationMode>", "<viewingGroup>90000</viewingGroup>"),
            (
                "</displayMode>",
                "<viewingGroupLayer>900</viewingGroupLayer>",
            ),
        ] {
            assert!(xml.contains(closing));
            let mutant = xml.replacen(closing, &format!("{insertion}{closing}"), 1);
            std::fs::write(&file, mutant).unwrap();
            let parsed = ferrite_portrayal_catalog::PortrayalCatalogue::load_bound(&root).unwrap();
            assert_eq!(
                copied
                    .sources()
                    .read_relative(std::path::Path::new("Rules/SEABED01.lua"))
                    .unwrap(),
                parsed
                    .sources()
                    .read_relative(std::path::Path::new("Rules/SEABED01.lua"))
                    .unwrap()
            );
            assert_eq!(
                parsed.viewing_group_layers.layers["900"].viewing_group_ids,
                copied.viewing_group_layers.layers["900"].viewing_group_ids
            );
            assert!(super::shallow_pattern_contract(&parsed).is_none());
            assert!(super::shallow_pattern_contract(&copied).is_some());
        }
        let after = ferrite_portrayal_catalog::PortrayalCatalogue::load_bound(&source).unwrap();
        assert_eq!(original.source_digest(), after.source_digest());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "requires separately source-SHA-guarded official PC directory via FERRITE_SHALLOW_PC"]
    fn official_pc_selector_is_bound_and_nonshallow_patterns_are_retained() {
        let pc = ferrite_portrayal_catalog::PortrayalCatalogue::load_bound(
            std::env::var("FERRITE_SHALLOW_PC").unwrap(),
        )
        .unwrap();
        let contract = super::shallow_pattern_contract(&pc).expect("audited selector contract");
        assert_eq!(contract.source_digest(), *pc.source_digest());
        let raw = super::shallow_pattern_contract(&pc).unwrap();
        for (fill, symbol, group, optional) in [
            ("DIAMOND1", "DIAMOND1P", 90000, true),
            ("DRGARE01", "DRGARE01P", 13030, false),
            ("NODATA03", "NODATA03P", 11050, false),
            ("TSSJCT02", "TSSJCT02P", 25010, false),
        ] {
            let mut a = ferrite_render::AreaInstruction::new(vec![
                ferrite_render::WorldPoint::new(0., 0.),
                ferrite_render::WorldPoint::new(1., 0.),
                ferrite_render::WorldPoint::new(0., 1.),
            ])
            .with_pattern_fill(symbol.into(), (22.5, 0.), (0., 43.13))
            .with_priority(9)
            .with_feature_id(1)
            .with_cell_index(0)
            .with_pattern_crs(ferrite_render::PatternCrs::GlobalGeometry);
            a.fill_ref = Some(fill.into());
            a.viewing_group.0 = group;
            a.portrayal_origin = ferrite_render::PortrayalOrigin::NonPoint;
            let item = ferrite_render::DrawingInstruction::Area(a);
            assert_eq!(raw.is_optional(&item), optional);
            assert_eq!(
                ferrite_render::pattern_display_allows(&item, false, Some(&raw)),
                !optional
            );
        }
    }
}
