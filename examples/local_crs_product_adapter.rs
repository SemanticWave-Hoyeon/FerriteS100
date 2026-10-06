//! Actual dataset and catalogue with a synthetic Local portrayal command.
use anyhow::{ensure, Context, Result};
use ferrite_render::{
    DrawingInstruction, PointOriginGeometry, PortrayalOrigin, RenderContext, Viewport, WorldPoint,
};
use ferrite_s100_core::{S101Cell, SpatialPrimitiveType};
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    ensure!(args.len() == 4, "cell PC output.json");
    let cell = S101Cell::load(&args[1])?;
    let pc = ferrite_portrayal_catalog::PortrayalCatalogue::load(&args[2])?;
    let (id, anchor) = cell
        .features
        .iter()
        .filter(|(_, f)| f.primitive_type == SpatialPrimitiveType::Point)
        .filter_map(|(id, f)| {
            f.spatial_associations
                .iter()
                .find_map(|a| cell.points.get(&a.spatial_id.key()))
                .map(|p| (*id, WorldPoint::new(p.position.x, p.position.y)))
        })
        .min_by_key(|(id, _)| *id)
        .context("No point feature")?;
    let commands = "AugmentedPoint:LocalCRS,3.2,-1.5;LocalOffset:1,2;PointInstruction:ACHBRT07;FontColor:CHBLK;FontSize:10;TextInstruction:Local adapter";
    let results = vec![ferrite_lua::PortrayalResult::parse(
        &format!("point|{id}"),
        commands,
        "",
    )?];
    let mut context = RenderContext::new(Viewport::new(640., 400.));
    ferrite_s101::convert_lua_results_for_cell(&results, &cell, &pc, &mut context, 0, "Day")?;
    ensure!(
        context.instruction_count() == 2,
        "Lost augmented instructions"
    );
    for instruction in context.raw_instructions() {
        let (position, displacement) = match instruction {
            DrawingInstruction::Point(p) => (p.position, [p.local_offset.0, p.local_offset.1]),
            DrawingInstruction::Text(t) => (t.position, [t.offset.x, t.offset.y]),
            _ => anyhow::bail!("Unexpected instruction"),
        };
        ensure!(position == anchor, "Millimetres replaced geographic anchor");
        ensure!(
            displacement == [4.2, 0.5],
            "Physical and glyph offsets lost or double-applied"
        );
        let PortrayalOrigin::Point(source) = instruction.portrayal_origin() else {
            anyhow::bail!("Missing point source")
        };
        ensure!(
            matches!(source.as_ref(),PointOriginGeometry::AugmentedLocalPoint {reference_point,millimetres}
            if *reference_point==anchor && *millimetres==[3.2,-1.5]),
            "Glyph offset modified coverage source"
        );
    }
    let bytes = bincode::serialize(context.raw_instructions())?;
    let restored: Vec<DrawingInstruction> = bincode::deserialize(&bytes)?;
    ensure!(
        restored
            .iter()
            .zip(context.raw_instructions())
            .all(|(a, b)| a.portrayal_origin() == b.portrayal_origin()),
        "Cache lost physical source"
    );
    let (&nonpoint_id, _) = cell
        .features
        .iter()
        .filter(|(_, f)| {
            matches!(
                f.primitive_type,
                SpatialPrimitiveType::Curve | SpatialPrimitiveType::Surface
            )
        })
        .min_by_key(|(id, _)| **id)
        .context("No nonpoint feature")?;
    let invalid = vec![ferrite_lua::PortrayalResult::parse(
        &format!("nonpoint|{nonpoint_id}"),
        commands,
        "",
    )?];
    let mut empty = RenderContext::new(Viewport::new(640., 400.));
    ensure!(
        ferrite_s101::convert_lua_results_for_cell(&invalid, &cell, &pc, &mut empty, 0, "Day")
            .is_err(),
        "Nonpoint accepted as augmented Local reference"
    );
    ensure!(
        empty.instruction_count() == 0,
        "Invalid Local source produced commands"
    );
    std::fs::write(
        &args[3],
        serde_json::to_vec_pretty(
            &serde_json::json!({"actual_cell":args[1],"pc":args[2],"feature_id":id,"anchor":[anchor.x,anchor.y],"point_and_text":2,"synthetic_lua_command":true,"raw_dataset_modified":false,"physical_offset_and_source_verified":true,"cache_roundtrip":true,"nonpoint_rejected":nonpoint_id}),
        )?,
    )?;
    Ok(())
}
