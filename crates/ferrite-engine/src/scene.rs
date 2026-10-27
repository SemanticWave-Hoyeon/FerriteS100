//! The renderer-agnostic product of a portrayal: the prepared scene a host
//! renderer attaches its GPU resources to, and the description of the
//! per-cell portrayal resources it needs.
use crate::state::EngineState;
use anyhow::{Context, Result};
use ferrite_portrayal_catalog::BoundPortrayalCatalogue;
use ferrite_render::host_settings::{DisplayMode, SettingsState};
use ferrite_render::RenderContext;
use ferrite_s101::coverage_projection::GeographicCoverageInventory;
use ferrite_s101::layer::catalogue_selection::DatasetCatalogueRegistry;
use ferrite_s101::layer::S101Layer;
use ferrite_s101::layer::{portrayal::validated_lua_context, primary_viewing_layers};
use std::collections::HashSet;
use std::sync::Arc;

/// A portrayal ready to publish: drawing instructions with the view they
/// were prepared for, the S-101 coverage they are clipped by and the
/// catalogue owners of the cells. GPU resources stay with the host.
pub struct PreparedScene {
    pub context: RenderContext,
    pub coverage: Arc<GeographicCoverageInventory>,
    pub catalogue_registry: DatasetCatalogueRegistry,
    /// Vector instructions changed by interoperability composition.
    pub ic_changed: usize,
    /// Identity of this preparation (unique per scene). A host that moves
    /// extension data to the GPU (S-102 tiles taken out of their extension)
    /// compares it to know that a later frame of the same, re-projected
    /// scene still shows them.
    pub id: Arc<()>,
    /// Product output for renderer paths beyond drawing instructions: the
    /// S-102 raster tiles (`ferrite_s102::layer::BATHYMETRY_EXTENSION`).
    pub extensions: Vec<ferrite_product_api::RendererExtension>,
}

impl PreparedScene {
    pub fn instruction_count(&self) -> usize {
        self.context.instruction_count()
    }
}

/// The portrayal catalogue a cell's resources (symbols, patterns, line
/// styles) come from.
#[derive(Clone)]
pub struct CellResourceBinding {
    pub cell_index: usize,
    pub catalogue: Arc<BoundPortrayalCatalogue>,
    pub shallow_pattern: Option<ferrite_render::ShallowPatternContract>,
}

impl EngineState {
    /// The resource binding of every loaded cell to its own catalogue owner,
    /// validating each owner's context for `settings`.
    pub fn cell_resource_bindings(
        &self,
        registry: &DatasetCatalogueRegistry,
        settings: &SettingsState,
    ) -> Result<Vec<CellResourceBinding>> {
        self.s101
            .cells
            .iter()
            .enumerate()
            .map(|(cell_index, cell)| {
                let owner = registry
                    .owner_for(&S101Layer::catalogue_dataset_key(&cell.dsid)?)
                    .context("Missing cell PC resource owner")?;
                validated_lua_context(owner.pc(), Some(settings))?;
                Ok(CellResourceBinding {
                    cell_index,
                    catalogue: Arc::clone(owner.pc()),
                    shallow_pattern: ferrite_s101::shallow_pattern_contract(owner.pc()),
                })
            })
            .collect()
    }

    /// Viewing groups displayed with the bound catalogue for `settings`: the
    /// display mode preset plus the primary viewing layers (the viewer adds
    /// its own plugin overlay group).
    pub fn viewing_groups(&self, settings: &SettingsState) -> Result<HashSet<u32>> {
        let preset = match settings.display_mode {
            DisplayMode::Base => ferrite_s101::DisplayPreset::Base,
            DisplayMode::Standard => ferrite_s101::DisplayPreset::Standard,
            DisplayMode::All => ferrite_s101::DisplayPreset::Other,
        };
        let mut groups = ferrite_s101::viewing_groups_for_preset(&self.s101.pc, preset)?;
        groups.extend(primary_viewing_layers::resolve(&self.s101.pc, settings)?);
        Ok(groups)
    }

    /// Viewing groups displayed for loaded cell `index` with its own
    /// catalogue: the display mode preset plus the primary viewing layers.
    pub fn cell_viewing_groups(
        &self,
        registry: &DatasetCatalogueRegistry,
        index: usize,
        settings: &SettingsState,
    ) -> Result<HashSet<u32>> {
        let cell = self.s101.cells.get(index).context("Missing cell")?;
        let preset = match settings.display_mode {
            DisplayMode::Base => ferrite_s101::DisplayPreset::Base,
            DisplayMode::Standard => ferrite_s101::DisplayPreset::Standard,
            DisplayMode::All => ferrite_s101::DisplayPreset::Other,
        };
        let owner = registry
            .owner_for(&S101Layer::catalogue_dataset_key(&cell.dsid)?)
            .context("Missing cell viewing-group owner")?;
        let mut groups = ferrite_s101::viewing_groups_for_preset(owner.pc(), preset)?;
        groups.extend(primary_viewing_layers::resolve(owner.pc(), settings)?);
        Ok(groups)
    }
}

/// Prepare S-101 coverage clipping of `context` for a view of `extent`
/// physical pixels at `density` pixels per logical pixel (also sorts the
/// instructions for drawing).
pub fn prepare_flat_coverage(
    inventory: Option<&GeographicCoverageInventory>,
    context: &mut RenderContext,
    extent: [u32; 2],
    density: f64,
) -> Result<()> {
    context.scaler.set_pixel_ratio(density);
    context.get_sorted_instructions();
    let Some(inventory) = inventory else {
        return Ok(());
    };
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
