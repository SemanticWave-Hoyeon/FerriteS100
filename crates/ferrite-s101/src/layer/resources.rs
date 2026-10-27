//! Per-cell portrayal resources of a scene: the portrayal catalogue each
//! loaded cell's symbols, patterns and line styles come from, and the
//! viewing groups displayed for a display mode (moved from the engine's
//! `scene`).
use super::catalogue_selection::DatasetCatalogueRegistry;
use super::portrayal::validated_lua_context;
use super::{primary_viewing_layers, S101Layer};
use anyhow::{Context, Result};
use ferrite_portrayal_catalog::BoundPortrayalCatalogue;
use ferrite_render::host_settings::{DisplayMode, SettingsState};
use std::collections::HashSet;
use std::sync::Arc;

/// The portrayal catalogue a cell's resources (symbols, patterns, line
/// styles) come from.
#[derive(Clone)]
pub struct CellResourceBinding {
    pub cell_index: usize,
    pub catalogue: Arc<BoundPortrayalCatalogue>,
    pub shallow_pattern: Option<ferrite_render::ShallowPatternContract>,
}

fn preset(settings: &SettingsState) -> crate::DisplayPreset {
    match settings.display_mode {
        DisplayMode::Base => crate::DisplayPreset::Base,
        DisplayMode::Standard => crate::DisplayPreset::Standard,
        DisplayMode::All => crate::DisplayPreset::Other,
    }
}

impl S101Layer {
    /// The resource binding of every loaded cell to its own catalogue owner,
    /// validating each owner's context for `settings`.
    pub fn cell_resource_bindings(
        &self,
        registry: &DatasetCatalogueRegistry,
        settings: &SettingsState,
    ) -> Result<Vec<CellResourceBinding>> {
        self.cells
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
                    shallow_pattern: crate::shallow_pattern_contract(owner.pc()),
                })
            })
            .collect()
    }

    /// Viewing groups displayed with the bound catalogue for `settings`: the
    /// display mode preset plus the primary viewing layers (the viewer adds
    /// its own plugin overlay group).
    pub fn viewing_groups(&self, settings: &SettingsState) -> Result<HashSet<u32>> {
        let mut groups = crate::viewing_groups_for_preset(&self.pc, preset(settings))?;
        groups.extend(primary_viewing_layers::resolve(&self.pc, settings)?);
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
        let cell = self.cells.get(index).context("Missing cell")?;
        let owner = registry
            .owner_for(&S101Layer::catalogue_dataset_key(&cell.dsid)?)
            .context("Missing cell viewing-group owner")?;
        let mut groups = crate::viewing_groups_for_preset(owner.pc(), preset(settings))?;
        groups.extend(primary_viewing_layers::resolve(owner.pc(), settings)?);
        Ok(groups)
    }
}
