//! Chart (domain) state of the viewer: the registered product layers (the
//! S-101 base chart with its catalogues, coverage and residency; S-102
//! grids; overlays and routes of the hosts that register them), the host's
//! portrayal settings, interoperability and the published portrayal.
//! Window, GUI, input and navigation state stay with the host application.
use crate::layers::ProductLayers;
use crate::platform::Platform;
use anyhow::Result;
use ferrite_product_api::{PortrayalSource, ProductId, SceneRole};
use ferrite_render::{GeoBounds, RenderContext};
#[cfg(feature = "s101")]
use s101_parts::*;
use std::sync::Arc;

#[cfg(feature = "s101")]
mod s101_parts {
    pub use ferrite_feature_catalog::BoundFeatureCatalogue;
    pub use ferrite_portrayal_catalog::BoundPortrayalCatalogue;
    pub use ferrite_render::host_settings::SettingsState;
    pub use ferrite_s101::coverage_projection::GeographicCoverageInventory;
    pub use ferrite_s101::layer::catalogue_selection::DatasetCatalogueRegistry;
    pub use ferrite_s101::layer::interoperability;
    pub use ferrite_s101::layer::portrayal::{AreaTriangulator, PortrayalChangeRequest};
    pub use ferrite_s101::layer::RuleSource;
}

#[cfg(feature = "s101")]
pub use ferrite_s101::layer::{CellHydration, ViewportPixels};

/// Pixel size and density of a view.
#[cfg(not(feature = "s101"))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewportPixels {
    pub width: u32,
    pub height: u32,
    pub scale_factor: f64,
}

/// Cells portrayed for the current catalogues (see
/// [`EngineState::portray_loaded_cells`]).
#[cfg(feature = "s101")]
pub struct CellScene {
    pub context: RenderContext,
    pub coverage: Arc<GeographicCoverageInventory>,
    /// Vector instructions changed by interoperability composition.
    pub ic_changed: usize,
    /// When Lua portrayal finished (post-Lua timing).
    pub after_lua: ferrite_kernel::time::Instant,
}

/// Cells portrayed for a portrayal change (see
/// [`EngineState::portray_change_cells`]).
#[cfg(feature = "s101")]
pub struct ChangeCellScene {
    pub context: RenderContext,
    /// The interoperability catalogue applied (settings permitting).
    pub active_ic: Option<Arc<ferrite_interoperability::AuthenticatedCatalogue>>,
    /// Its selection for S-102 rasters composed with this scene.
    pub s102_selection: ferrite_interoperability::Selection,
    pub ic_changed: usize,
}

/// Chart state owned by the engine.
pub struct EngineState {
    /// Published portrayal (the engine's output scene).
    pub render_context: RenderContext,
    pub bounds: GeoBounds,
    /// Base instruction count (chart instructions only, before plugin instructions)
    pub base_instruction_count: usize,
    /// Current color profile name (Day, Dusk, Night)
    pub current_profile_name: String,
    /// The registered product layers ([`crate::products`]): the S-101 base
    /// chart (bound catalogues and their statuses, loaded cells, coverage,
    /// residency and re-materialization, decoded cache), S-102 grids
    /// (catalogues, opened files, input policies) and the overlay and route
    /// layers of hosts that register them.
    pub layers: ProductLayers,
    /// Where rule-based products take their instructions from (S-101 cells
    /// and the S-102 PC rule; default: Lua when built, otherwise precomputed
    /// payloads).
    pub portrayal_source: PortrayalSource,
    /// Write precomputed payloads of everything portrayed with Lua to the
    /// cache store, for hosts without Lua.
    pub precomputed_export: bool,
    pub ic: Option<Arc<ferrite_interoperability::AuthenticatedCatalogue>>,
    pub ic_combination: Option<String>,
    pub ic_assigned_vectors: usize,
    pub require_signatures: bool,
    pub operational_mode: bool,
    pub verified_count: usize,
    pub unsigned_count: usize,
    /// Services the state reads inputs, caches and time through.
    pub platform: Platform,
}

/// Lua when it is built, otherwise precomputed payloads.
pub fn default_portrayal_source() -> PortrayalSource {
    if cfg!(feature = "lua-native") {
        PortrayalSource::Lua
    } else {
        PortrayalSource::Precomputed
    }
}

impl EngineState {
    /// A chart state with the registered `layers`: no datasets, the whole
    /// world in view (ellipsoidal Mercator), nothing published.
    pub fn new(platform: Platform, profile: String, layers: ProductLayers) -> Self {
        let mut render_context = RenderContext::new(ferrite_render::Viewport::new(1920.0, 1080.0));
        render_context
            .scaler
            .set_projection(ferrite_render::FlatProjection::EllipsoidalMercator);
        Self {
            render_context,
            bounds: GeoBounds::new(-180.0, -90.0, 180.0, 90.0),
            base_instruction_count: 0,
            current_profile_name: profile,
            layers,
            portrayal_source: default_portrayal_source(),
            precomputed_export: false,
            ic: None,
            ic_combination: None,
            ic_assigned_vectors: 0,
            require_signatures: false,
            operational_mode: false,
            verified_count: 0,
            unsigned_count: 0,
            platform,
        }
    }

    /// Products on display for interoperability: the base chart and grid
    /// layers holding data, in registration order.
    pub fn displayed_products(&self) -> Vec<&'static str> {
        let mut displayed = Vec::new();
        for entry in self.layers.iter() {
            let shown = match entry.role {
                SceneRole::BaseChart => !entry.layer.is_empty(),
                SceneRole::Grid => !entry.layer.numeric_coverages().is_empty(),
                _ => false,
            };
            if shown {
                displayed.extend(entry.products.iter().map(|p| p.as_str()));
            }
        }
        displayed
    }

    /// Selection for `product` being composed now: `product` on display
    /// with the base chart products holding data.
    pub fn ic_selection(
        &self,
        ic: &ferrite_interoperability::AuthenticatedCatalogue,
        product: ProductId,
    ) -> Result<ferrite_interoperability::Selection> {
        let displayed = displayed_with(product, &self.base_products());
        ic.catalogue
            .select(self.ic_combination.as_deref(), &displayed)
    }

    /// Selection for S-102 rasters being composed now: S-102 is on display.
    pub fn ic_s102_selection(
        &self,
        ic: &ferrite_interoperability::AuthenticatedCatalogue,
    ) -> Result<ferrite_interoperability::Selection> {
        self.ic_selection(ic, ProductId("S-102"))
    }

    /// Products of the base chart layers holding data.
    pub fn base_products(&self) -> Vec<&'static str> {
        self.layers
            .iter()
            .filter(|e| e.role == SceneRole::BaseChart && !e.layer.is_empty())
            .flat_map(|e| e.products.iter().map(|p| p.as_str()))
            .collect()
    }

    /// Portrayed extents of the loaded S-102 files (empty without S-102).
    pub fn bathymetry_extents(&self) -> Vec<GeoBounds> {
        #[cfg(feature = "s102-pure")]
        return self
            .layers
            .get::<ferrite_s102::layer::S102Layer>()
            .map(|layer| layer.bathymetry_bounds.values().copied().collect())
            .unwrap_or_default();
        #[cfg(not(feature = "s102-pure"))]
        Vec::new()
    }

    /// S-98 Level 2 context for the products currently on display.
    #[cfg(feature = "s101")]
    pub fn ic_level2(
        &self,
        ic: &ferrite_interoperability::AuthenticatedCatalogue,
    ) -> Result<interoperability::Level2> {
        let displayed = self.displayed_products();
        let coverages: Vec<&dyn ferrite_kernel::NumericCoverageSource> = self
            .layers
            .iter()
            .flat_map(|e| e.layer.numeric_coverages())
            .collect();
        interoperability::Level2::new(ic, self.ic_combination.as_deref(), &displayed, coverages)
    }

    /// The host's rule source for cell portrayal.
    #[cfg(feature = "s101")]
    pub fn rule_source(&self) -> RuleSource {
        RuleSource {
            source: self.portrayal_source,
            export: self.precomputed_export,
        }
    }

    #[cfg(feature = "s101")]
    /// Portray the loaded cells with the bound catalogues
    /// ([`S101Layer::portray_loaded_cells`]: validation, view residency,
    /// re-materialization, Lua portrayal with per-cell caches) for the
    /// published view and profile, then compose interoperability.
    pub fn portray_loaded_cells(
        &mut self,
        preserve_view: bool,
        settings: Option<&SettingsState>,
        viewport: Option<ViewportPixels>,
        active_ic: Option<Arc<ferrite_interoperability::AuthenticatedCatalogue>>,
        triangulate: AreaTriangulator,
    ) -> Result<CellScene> {
        let rules = self.rule_source();
        let (mut next, coverage_inventory) = self.layers.s101_mut().portray_loaded_cells(
            &self.render_context,
            self.bounds,
            &self.current_profile_name,
            preserve_view,
            settings,
            viewport,
            active_ic.is_some(),
            triangulate,
            rules,
        )?;
        let after_lua = ferrite_kernel::time::Instant::now();

        let ic_changed = if let Some(ic) = active_ic {
            let level2 = self.ic_level2(&ic)?;
            let s101 = self.layers.s101();
            interoperability::compose_vectors(
                &ic,
                &level2,
                &s101.cells,
                &s101.fc,
                (&s101.pc, &self.current_profile_name),
                &mut next,
            )?
        } else {
            0
        };
        Ok(CellScene {
            context: next,
            coverage: coverage_inventory,
            ic_changed,
            after_lua,
        })
    }

    #[cfg(feature = "s101")]
    /// View residency for a portrayal change of the published view
    /// ([`S101Layer::prepare_view_residency`]); every cell is brought back
    /// when an interoperability catalogue will be composed.
    pub fn prepare_view_residency(
        &mut self,
        preserve_view: bool,
        viewport: Option<ViewportPixels>,
        interoperability_enabled: bool,
    ) -> Result<Option<Arc<GeographicCoverageInventory>>> {
        let interoperating = interoperability_enabled && self.ic.is_some();
        self.layers.s101_mut().prepare_view_residency(
            &self.render_context,
            self.bounds,
            preserve_view,
            viewport,
            interoperating,
        )
    }

    #[cfg(feature = "s101")]
    /// The renderer-independent part of a portrayal change: the cells
    /// portrayed by the layer ([`S101Layer::portray_change_cells`]) and
    /// interoperability composition.
    #[allow(clippy::too_many_arguments)]
    pub fn portray_change_cells(
        &self,
        request: &PortrayalChangeRequest,
        fc: &Arc<BoundFeatureCatalogue>,
        pc: &Arc<BoundPortrayalCatalogue>,
        catalogue_registry: &DatasetCatalogueRegistry,
        owned: bool,
        preflight_resources: bool,
        triangulate: AreaTriangulator,
    ) -> Result<ChangeCellScene> {
        let s101 = self.layers.s101();
        let mut next = s101.portray_change_cells(
            &self.render_context,
            request,
            fc,
            pc,
            catalogue_registry,
            owned,
            preflight_resources,
            triangulate,
            self.rule_source(),
        )?;
        let active_ic = if request.settings.interoperability_enabled {
            self.ic.clone()
        } else {
            None
        };
        let s102_selection = active_ic
            .as_ref()
            .map(|ic| self.ic_s102_selection(ic))
            .transpose()?
            .unwrap_or_default();
        let ic_changed = if let Some(ic) = active_ic.as_ref() {
            if owned {
                let (fcs, pcs) = s101.cell_owner_catalogues(catalogue_registry)?;
                let level2 = self.ic_level2(ic)?;
                interoperability::compose_vectors_for_cells(
                    ic,
                    &level2,
                    &s101.cells,
                    &fcs,
                    &pcs,
                    &request.profile,
                    &mut next,
                )?
            } else {
                let level2 = self.ic_level2(ic)?;
                interoperability::compose_vectors(
                    ic,
                    &level2,
                    &s101.cells,
                    fc,
                    (pc, &request.profile),
                    &mut next,
                )?
            }
        } else {
            0
        };
        Ok(ChangeCellScene {
            context: next,
            active_ic,
            s102_selection,
            ic_changed,
        })
    }
}

/// `product` on display with the other products of `base`.
fn displayed_with(product: ProductId, base: &[&'static str]) -> Vec<&'static str> {
    let mut displayed = vec![product.as_str()];
    displayed.extend(base.iter().copied().filter(|p| *p != product.as_str()));
    displayed
}
