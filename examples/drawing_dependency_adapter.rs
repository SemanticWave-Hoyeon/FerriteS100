//! Local dependency command fixture through the real S-101 product adapter.
//! This is not a rendering/execution conformance test.
use anyhow::Result;
use ferrite_render::{DrawingDependencyGraph, DrawingInstruction, RenderContext, Viewport};
fn main() -> Result<()> {
    let a = std::env::args().skip(1).collect::<Vec<_>>();
    let cell = ferrite_s100_core::S101Cell::load(&a[0])?;
    let pc = ferrite_portrayal_catalog::PortrayalCatalogue::load(&a[1])?;
    let vg = pc
        .viewing_groups
        .groups
        .values()
        .filter_map(|v| v.catalogue_id.parse::<u32>().ok())
        .min()
        .unwrap();
    let command=format!("ViewingGroup:{vg};Id:parent;AugmentedPoint:GeographicCRS,-2,48;PointInstruction:ACHBRT07;Id:child;Parent:parent;PointInstruction:ACHBRT07;Parent;Id;PointInstruction:ACHBRT07");
    let result = ferrite_lua::PortrayalResult::parse("LOCAL-DEPENDENCY-TEST", &command, "")?;
    let mut context = RenderContext::new(Viewport::new(800., 600.));
    ferrite_s101::convert_lua_results_for_cell(&[result], &cell, &pc, &mut context, 7, "Day")?;
    let instructions = context.raw_instructions();
    assert_eq!(instructions.len(), 3);
    assert_eq!(
        instructions[0].dependency().unwrap().id.as_deref(),
        Some("parent")
    );
    assert_eq!(
        instructions[1].dependency().unwrap().parent_id.as_deref(),
        Some("parent")
    );
    assert!(instructions[2].dependency().is_none());
    let graph =
        DrawingDependencyGraph::compile(instructions.iter().map(DrawingInstruction::dependency));
    assert_eq!(
        graph.resolve(&[true, true, true]).unwrap().executed,
        [true, true, true]
    );
    assert_eq!(
        graph.resolve(&[false, true, true]).unwrap().executed,
        [false, false, true]
    );
    std::fs::write(
        &a[2],
        serde_json::to_vec_pretty(
            &serde_json::json!({"fixture":"LOCAL-DEPENDENCY-TEST","primitive_count":instructions.len(),"metadata":instructions.iter().map(DrawingInstruction::dependency).collect::<Vec<_>>(),"reset_retained":true,"adapter_and_graph_verified":true,"actual_rendering_verified":false}),
        )?,
    )?;
    Ok(())
}
