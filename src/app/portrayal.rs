//! Fallback portrayal when the Lua engine isn't available.
//!
//! `try_lua_portrayal()` (still in `main.rs` because it threads through
//! `RenderContext` + a wide set of `ferrite_lua` types) is the production
//! path. This module supplies the simpler "draw something reasonable from
//! the spatial primitives alone" fallback used when the Lua run errors out.
//!
//! Per CLAUDE.md, color tokens are resolved against the loaded Portrayal
//! Catalogue — never hardcoded. `lookup_pc_color` is the single entry point
//! and falls back to the default profile, then to any profile, then to a
//! gray with a logged warning if even that fails.

use ferrite_portrayal_catalog::PortrayalCatalogue;
use ferrite_render::{
    AreaInstruction, Color, LineInstruction, LineStyle, PointInstruction, RenderContext, WorldPoint,
};
use ferrite_s100_core::{S101Cell, SpatialPrimitiveType};

/// Generate default drawing instructions for a cell from its spatial primitives.
/// Uses PC color lookup for every fill/stroke — no hardcoded colors.
pub fn generate_default_instructions(
    cell: &S101Cell,
    context: &mut RenderContext,
    pc: &PortrayalCatalogue,
    profile_name: &str,
) {
    for (key, feature) in &cell.features {
        let feature_code = feature.feature_code.as_deref().unwrap_or("UNKNOWN");

        let (color_token, priority) = get_feature_color_token(feature_code);
        let color = lookup_pc_color(pc, color_token, profile_name);

        match feature.primitive_type {
            SpatialPrimitiveType::Point => {
                for spas in &feature.spatial_associations {
                    if let Some(point) = cell.points.get(&spas.spatial_id.key()) {
                        let instruction = PointInstruction::new(
                            feature_code.to_string(),
                            WorldPoint::new(point.position.x, point.position.y),
                        )
                        .with_priority(priority)
                        .with_feature_id(*key);

                        context.add_instruction(ferrite_render::DrawingInstruction::Point(
                            instruction,
                        ));
                    }
                }
            }
            SpatialPrimitiveType::Curve | SpatialPrimitiveType::CompositeCurve => {
                for spas in &feature.spatial_associations {
                    // S-101 4.8.3: Edge masking — skip suppressed edges.
                    if spas.mask == 2 {
                        continue;
                    }
                    let masked_by_mask_field = feature
                        .masks
                        .iter()
                        .any(|m| m.spatial_id.key() == spas.spatial_id.key() && m.mask_type == 2);
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
                            let instruction = LineInstruction::new(points)
                                .with_style(LineStyle::solid(color, 1.0))
                                .with_priority(priority)
                                .with_feature_id(*key);

                            context.add_instruction(ferrite_render::DrawingInstruction::Line(
                                instruction,
                            ));
                        }
                    }
                }
            }
            SpatialPrimitiveType::Surface => {
                for spas in &feature.spatial_associations {
                    let surface_key = spas.spatial_id.key();
                    if let Some(surface) = cell.surfaces.get(&surface_key) {
                        let mut exterior_points = Vec::new();

                        for oriented_curve in &surface.exterior_ring {
                            let curve_key = oriented_curve.curve_id.key();

                            if let Some(curve) = cell.curves.get(&curve_key) {
                                let positions = curve.all_positions();
                                if oriented_curve.orientation {
                                    for pos in positions {
                                        exterior_points.push(WorldPoint::new(pos.x, pos.y));
                                    }
                                } else {
                                    for pos in positions.into_iter().rev() {
                                        exterior_points.push(WorldPoint::new(pos.x, pos.y));
                                    }
                                }
                            } else if let Some(composite) = cell.composite_curves.get(&curve_key) {
                                for sub_curve in &composite.curves {
                                    if let Some(curve) = cell.curves.get(&sub_curve.curve_id.key())
                                    {
                                        let positions = curve.all_positions();
                                        let forward =
                                            oriented_curve.orientation == sub_curve.orientation;
                                        if forward {
                                            for pos in positions {
                                                exterior_points.push(WorldPoint::new(pos.x, pos.y));
                                            }
                                        } else {
                                            for pos in positions.into_iter().rev() {
                                                exterior_points.push(WorldPoint::new(pos.x, pos.y));
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        // Remove duplicate consecutive points (curves share endpoints).
                        let mut cleaned_points = Vec::with_capacity(exterior_points.len());
                        for point in exterior_points {
                            if cleaned_points.is_empty() {
                                cleaned_points.push(point);
                            } else {
                                let last = cleaned_points.last().unwrap();
                                let dx = (point.x - last.x).abs();
                                let dy = (point.y - last.y).abs();
                                if dx > 1e-9 || dy > 1e-9 {
                                    cleaned_points.push(point);
                                }
                            }
                        }

                        // Remove duplicate closing point if present.
                        if cleaned_points.len() > 3 {
                            let first = cleaned_points.first().unwrap();
                            let last = cleaned_points.last().unwrap();
                            let dx = (first.x - last.x).abs();
                            let dy = (first.y - last.y).abs();
                            if dx < 1e-9 && dy < 1e-9 {
                                cleaned_points.pop();
                            }
                        }

                        if cleaned_points.len() >= 3 {
                            let fill_color = color.with_alpha(0.3);
                            let instruction = AreaInstruction::new(cleaned_points)
                                .with_solid_fill(fill_color)
                                .with_outline(LineStyle::solid(color, 0.5))
                                .with_priority(priority)
                                .with_feature_id(*key);

                            context.add_instruction(ferrite_render::DrawingInstruction::Area(
                                instruction,
                            ));
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// Map a feature code to its default PC color token + a draw-priority hint.
/// Used only by the fallback path; the Lua portrayal supplies its own.
pub fn get_feature_color_token(feature_code: &str) -> (&'static str, i32) {
    match feature_code {
        // Land features
        "LandArea" => ("LANDA", 1),
        "BuiltUpArea" => ("CHBRN", 2),

        // Depth features
        "DepthArea" => ("DEPVS", 1),
        "DepthContour" => ("DEPCN", 10),
        "DredgedArea" => ("DEPMD", 3),

        // Coastline
        "Coastline" => ("CSTLN", 15),

        // Navigation features
        "Light" | "LightAllAround" | "LightSectored" => ("LITRD", 20),
        "Buoy" | "LateralBuoy" | "CardinalBuoy" | "IsolatedDangerBuoy" => ("LITRD", 18),
        "Beacon" | "LateralBeacon" | "CardinalBeacon" => ("LITRD", 18),

        // Obstructions and dangers
        "Wreck" => ("DEPVS", 25),
        "Obstruction" => ("CHGRD", 25),
        "Rock" | "UnderwaterRock" => ("CHGRD", 22),

        // Anchorage
        "AnchorageArea" => ("CHMGD", 8),
        "AnchorBerth" => ("CHMGD", 12),

        // Traffic
        "TrafficSeparationScheme" | "TrafficSeparationZone" => ("TRFCD", 5),

        // Default
        _ => ("CHGRD", 5),
    }
}

/// Resolve a color token against a named profile, with sensible fallbacks
/// (default profile → any profile → gray + warning).
pub fn lookup_pc_color(pc: &PortrayalCatalogue, token: &str, profile_name: &str) -> Color {
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

    tracing::warn!("Color token '{}' not found in PC color profile", token);
    Color::rgb(0.5, 0.5, 0.5)
}
