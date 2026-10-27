//! Chart (domain) state of the viewer: the S-101 product layer (loaded
//! cells, their bound catalogues, coverage, residency), interoperability and
//! bathymetry, and the published portrayal. Window, GUI, input and
//! navigation state stay with the host application.
use crate::platform::Platform;
use anyhow::Result;
use ferrite_feature_catalog::BoundFeatureCatalogue;
use ferrite_portrayal_catalog::BoundPortrayalCatalogue;
use ferrite_render::host_settings::CatalogueStatus;
use ferrite_render::host_settings::SettingsState;
use ferrite_render::{GeoBounds, RenderContext};
use ferrite_s101::coverage_projection::GeographicCoverageInventory;
use ferrite_s101::layer::catalogue_selection::DatasetCatalogueRegistry;
use ferrite_s101::layer::interoperability;
use ferrite_s101::layer::portrayal::{AreaTriangulator, PortrayalChangeRequest};
use ferrite_s101::layer::S101Layer;
use std::path::PathBuf;
use std::sync::Arc;

pub use ferrite_s101::layer::{CellHydration, ViewportPixels};

/// Cells portrayed for the current catalogues (see
/// [`EngineState::portray_loaded_cells`]).
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
    /// The S-101 product layer: bound catalogues and their statuses, loaded
    /// cells (identities, chains, discovery, catalogue owners), coverage,
    /// residency and re-materialization, decoded cache and portrayal source.
    pub s101: S101Layer,
    pub ic: Option<Arc<ferrite_interoperability::AuthenticatedCatalogue>>,
    pub ic_combination: Option<String>,
    pub ic_assigned_vectors: usize,
    /// The S-102 product layer: catalogues, opened files (coverages,
    /// extents, depth policies, input snapshots, original authentication,
    /// journal admission) and inspected trial files.
    #[cfg(feature = "s102-pure")]
    pub s102: ferrite_s102::layer::S102Layer,
    pub require_signatures: bool,
    pub operational_mode: bool,
    pub verified_count: usize,
    pub unsigned_count: usize,
    /// Services the state reads inputs, caches and time through.
    pub platform: Platform,
}

impl EngineState {
    /// An empty chart state with the bound catalogue pair: no datasets, the
    /// whole world in view (ellipsoidal Mercator), nothing published.
    pub fn new(
        platform: Platform,
        fc: Arc<BoundFeatureCatalogue>,
        pc: Arc<BoundPortrayalCatalogue>,
        fc_status: CatalogueStatus,
        pc_status: CatalogueStatus,
        profile: String,
        catalogue_inventory: PathBuf,
    ) -> Self {
        let mut render_context = RenderContext::new(ferrite_render::Viewport::new(1920.0, 1080.0));
        render_context
            .scaler
            .set_projection(ferrite_render::FlatProjection::EllipsoidalMercator);
        let s101 = S101Layer::new(
            platform.services(),
            fc,
            pc,
            fc_status,
            pc_status,
            catalogue_inventory,
            platform
                .user
                .cancellation_history_path()
                .unwrap_or_default(),
        );
        Self {
            render_context,
            bounds: GeoBounds::new(-180.0, -90.0, 180.0, 90.0),
            base_instruction_count: 0,
            current_profile_name: profile,
            s101,
            ic: None,
            ic_combination: None,
            ic_assigned_vectors: 0,
            #[cfg(feature = "s102-pure")]
            s102: ferrite_s102::layer::S102Layer::new(Some(Arc::clone(&platform.cache))),
            require_signatures: false,
            operational_mode: false,
            verified_count: 0,
            unsigned_count: 0,
            platform,
        }
    }

    /// S-98 Level 2 context for the products currently on display.
    pub fn ic_level2(
        &self,
        ic: &ferrite_interoperability::AuthenticatedCatalogue,
    ) -> Result<interoperability::Level2> {
        let mut displayed = Vec::new();
        if !self.s101.cells.is_empty() {
            displayed.push("S-101");
        }
        #[cfg(feature = "s102-pure")]
        let bathymetry: Vec<&dyn ferrite_kernel::NumericCoverageSource> = self
            .s102
            .bathymetry
            .iter()
            .map(|(_, c, _)| c as &dyn ferrite_kernel::NumericCoverageSource)
            .collect();
        #[cfg(not(feature = "s102-pure"))]
        let bathymetry: Vec<&dyn ferrite_kernel::NumericCoverageSource> = Vec::new();
        if !bathymetry.is_empty() {
            displayed.push("S-102");
        }
        interoperability::Level2::new(ic, self.ic_combination.as_deref(), &displayed, bathymetry)
    }
    /// Portrayed extents of the loaded S-102 files (empty without S-102).
    pub fn bathymetry_extents(&self) -> Vec<GeoBounds> {
        #[cfg(feature = "s102-pure")]
        return self.s102.bathymetry_bounds.values().copied().collect();
        #[cfg(not(feature = "s102-pure"))]
        Vec::new()
    }
    /// Selection for S-102 rasters being composed now: S-102 is on display.
    pub fn ic_s102_selection(
        &self,
        ic: &ferrite_interoperability::AuthenticatedCatalogue,
    ) -> Result<ferrite_interoperability::Selection> {
        let mut displayed = vec!["S-102"];
        if !self.s101.cells.is_empty() {
            displayed.push("S-101");
        }
        ic.catalogue
            .select(self.ic_combination.as_deref(), &displayed)
    }

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
        let (mut next, coverage_inventory) = self.s101.portray_loaded_cells(
            &self.render_context,
            self.bounds,
            &self.current_profile_name,
            preserve_view,
            settings,
            viewport,
            active_ic.is_some(),
            triangulate,
        )?;
        let after_lua = ferrite_kernel::time::Instant::now();

        let ic_changed = if let Some(ic) = active_ic {
            let level2 = self.ic_level2(&ic)?;
            interoperability::compose_vectors(
                &ic,
                &level2,
                &self.s101.cells,
                &self.s101.fc,
                (&self.s101.pc, &self.current_profile_name),
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
        self.s101.prepare_view_residency(
            &self.render_context,
            self.bounds,
            preserve_view,
            viewport,
            interoperating,
        )
    }

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
        let mut next = self.s101.portray_change_cells(
            &self.render_context,
            request,
            fc,
            pc,
            catalogue_registry,
            owned,
            preflight_resources,
            triangulate,
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
                let (fcs, pcs) = self.s101.cell_owner_catalogues(catalogue_registry)?;
                let level2 = self.ic_level2(ic)?;
                interoperability::compose_vectors_for_cells(
                    ic,
                    &level2,
                    &self.s101.cells,
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
                    &self.s101.cells,
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
