//! The renderer-agnostic product of a portrayal: the prepared scene a host
//! renderer attaches its GPU resources to. The per-cell portrayal resources
//! a renderer needs for the base chart are described by the S-101 layer
//! ([`CellResourceBinding`], `S101Layer::cell_resource_bindings`).
use anyhow::Result;
use ferrite_render::RenderContext;
#[cfg(feature = "s101")]
use ferrite_s101::coverage_projection::GeographicCoverageInventory;
#[cfg(feature = "s101")]
use ferrite_s101::layer::catalogue_selection::DatasetCatalogueRegistry;
use std::sync::Arc;

/// A portrayal ready to publish: drawing instructions with the view they
/// were prepared for, the S-101 coverage they are clipped by and the
/// catalogue owners of the cells (with the S-101 product), and the
/// renderer extensions of the other layers. GPU resources stay with the
/// host.
pub struct PreparedScene {
    pub context: RenderContext,
    #[cfg(feature = "s101")]
    pub coverage: Arc<GeographicCoverageInventory>,
    #[cfg(feature = "s101")]
    pub catalogue_registry: DatasetCatalogueRegistry,
    /// Vector instructions changed by interoperability composition.
    pub ic_changed: usize,
    /// Identity of this preparation (unique per scene). A host that moves
    /// extension data to the GPU (S-102 tiles taken out of their extension)
    /// compares it to know that a later frame of the same, re-projected
    /// scene still shows them.
    pub id: Arc<()>,
    /// Product output for renderer paths beyond drawing instructions, from
    /// the grid layers: the S-102 raster tiles
    /// (`ferrite_s102::layer::BATHYMETRY_EXTENSION`).
    pub extensions: Vec<ferrite_product_api::RendererExtension>,
}

impl PreparedScene {
    pub fn instruction_count(&self) -> usize {
        self.context.instruction_count()
    }
}

/// The portrayal catalogue a cell's resources come from (S-101 layer).
#[cfg(feature = "s101")]
pub use ferrite_s101::layer::resources::CellResourceBinding;

/// The coverage inventory scenes are clipped by (S-101 M_COVR; without the
/// S-101 product no scene has one).
#[cfg(feature = "s101")]
pub type CoverageInventory = GeographicCoverageInventory;
#[cfg(not(feature = "s101"))]
pub type CoverageInventory = std::convert::Infallible;

/// Prepare S-101 coverage clipping of `context` for a view of `extent`
/// physical pixels at `density` pixels per logical pixel (also sorts the
/// instructions for drawing).
pub fn prepare_flat_coverage(
    inventory: Option<&CoverageInventory>,
    context: &mut RenderContext,
    extent: [u32; 2],
    density: f64,
) -> Result<()> {
    context.scaler.set_pixel_ratio(density);
    context.get_sorted_instructions();
    let Some(inventory) = inventory else {
        return Ok(());
    };
    #[cfg(not(feature = "s101"))]
    {
        let _ = extent;
        match *inventory {}
    }
    #[cfg(feature = "s101")]
    prepare_clipping(inventory, context, extent)
}

#[cfg(feature = "s101")]
fn prepare_clipping(
    inventory: &GeographicCoverageInventory,
    context: &mut RenderContext,
    extent: [u32; 2],
) -> Result<()> {
    if inventory.current_dataset_count() == 0 {
        context.clear_prepared_coverage();
        return Ok(());
    }
    context.require_prepared_coverage();
    let exemptions = if context.may_have_coverage_exempt_instructions() {
        context
            .raw_instructions()
            .iter()
            .enumerate()
            .filter_map(|(i, command)| {
                matches!(
                    command.portrayal_origin(),
                    ferrite_render::PortrayalOrigin::CoverageExempt
                )
                .then_some(i)
            })
            .collect()
    } else {
        std::collections::BTreeSet::new()
    };
    match inventory.prepare_flat(context, &exemptions, extent, true, 128 * 1024 * 1024)? {
        Some(prepared) => context.set_prepared_coverage(prepared)?,
        None => context.clear_prepared_coverage(),
    }
    Ok(())
}
