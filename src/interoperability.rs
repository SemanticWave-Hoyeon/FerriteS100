//! App orchestration for authenticated Part 16 plane rules; readers stay in product crates.
use anyhow::{ensure, Result};
use ferrite_interoperability::{Assignment, AuthenticatedCatalogue};
use ferrite_render::{DisplayPlane, RasterDrawOrder, RasterLayer, RenderContext};
pub fn compose_vectors(
    ic: &AuthenticatedCatalogue,
    cells: &[ferrite_s100_core::S101Cell],
    fc: &ferrite_feature_catalog::FeatureCatalogue,
    context: &mut RenderContext,
) -> Result<usize> {
    let mut instructions = context.raw_instructions().to_vec();
    let mut changed = 0;
    for (index, cell) in cells.iter().enumerate() {
        let plan = ferrite_s101::plan_interoperability(
            &ic.catalogue,
            cell,
            fc,
            &instructions,
            u32::try_from(index)?,
        )?;
        changed += plan.len();
        plan.apply(&mut instructions)?;
    }
    context.set_instructions_from_cache(instructions);
    Ok(changed)
}
/// Preserve one ordered instruction stream and original global source cell indexes.
/// Missing catalogue ownership is an error, never a global-FC fallback.
pub fn compose_vectors_for_cells(
    ic: &AuthenticatedCatalogue,
    cells: &[ferrite_s100_core::S101Cell],
    fcs: &[&ferrite_feature_catalog::BoundFeatureCatalogue],
    context: &mut RenderContext,
) -> Result<usize> {
    ensure!(
        cells.len() == fcs.len(),
        "IC cell catalogue owner count mismatch"
    );
    let catalogues: Vec<&ferrite_feature_catalog::FeatureCatalogue> =
        fcs.iter().map(|fc| &***fc).collect();
    let mut instructions = context.raw_instructions().to_vec();
    let plan = ferrite_s101::plan_interoperability_for_cells(
        &ic.catalogue,
        cells,
        &catalogues,
        &instructions,
    )?;
    let changed = plan.len();
    plan.apply(&mut instructions)?;
    context.set_instructions_from_cache(instructions);
    Ok(changed)
}
pub fn compose_raster(
    mut layer: RasterLayer,
    assignment: Option<&Assignment>,
) -> Result<RasterLayer> {
    if let Some(a) = assignment {
        ensure!(
            a.plane.stage == ferrite_kernel::CompositionStage::Chart && a.priority >= 0,
            "Invalid raster IC assignment"
        );
        layer.draw_order = RasterDrawOrder {
            stage: a.plane.stage,
            display_plane: DisplayPlane::Interoperability(a.plane.order),
            priority: a.priority,
        };
        layer.viewing_groups = vec![a.viewing_group];
    }
    Ok(layer)
}
