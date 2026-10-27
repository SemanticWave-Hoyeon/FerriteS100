//! The product modules compiled into this build (`docs/ENGINE_DESIGN.md`,
//! "제품 모듈화"): the only place the engine names products. Each product
//! crate is behind an engine feature of the same name (`s101`, `s124`,
//! `s129`, `s421`, and `s102-pure`/`s102-hdf5` for S-102; all on by
//! default). Registration says, per product: its adapter, its layer (in the
//! registry of [`crate::layers::ProductLayers`]: open-job stage, install,
//! reports, portrayal by scene role), how its per-user records are kept
//! ([`user_record_kinds`]) and, for the GML products, their static rules
//! ([`gml_rules`]). The rest of the engine reaches products through these
//! interfaces ([`ferrite_product_api`], [`ferrite_s100_gml::product`]).
use crate::layers::ProductLayers;
use crate::platform::Platform;
use ferrite_product_api::platform::RecordKind;
use ferrite_product_api::{ProductAdapter, ProductId, SceneRole};
use ferrite_s100_gml::product::GmlRules;
use ferrite_s100_gml::Product;

/// The GML (S-100 Part 10b) products, in registration order: their static
/// rules, which also create their overlay modules.
// The pushes depend on the enabled features.
#[allow(clippy::vec_init_then_push)]
pub fn gml_rules() -> Vec<&'static dyn GmlRules> {
    #[allow(unused_mut)]
    let mut rules: Vec<&'static dyn GmlRules> = Vec::new();
    #[cfg(feature = "s124")]
    rules.push(&ferrite_s124::S124);
    #[cfg(feature = "s129")]
    rules.push(&ferrite_s129::S129);
    rules
}

/// The GML products registered in this build.
pub fn gml_products() -> Vec<Product> {
    gml_rules().iter().map(|r| r.product()).collect()
}

/// The adapters of every product built into this engine, in registration
/// order (S-101, the GML products, S-421, S-102).
// The pushes depend on the enabled features.
#[allow(clippy::vec_init_then_push)]
pub fn adapters() -> Vec<Box<dyn ProductAdapter>> {
    #[allow(unused_mut)]
    let mut adapters: Vec<Box<dyn ProductAdapter>> = Vec::new();
    #[cfg(feature = "s101")]
    adapters.push(Box::new(ferrite_s101::layer::S101));
    for rules in gml_rules() {
        adapters.push(rules.overlay());
    }
    #[cfg(feature = "s421")]
    adapters.push(Box::new(ferrite_s421::layer::S421));
    #[cfg(feature = "s102-pure")]
    adapters.push(Box::new(ferrite_s102::layer::S102));
    adapters
}

/// How the built products keep their per-user records (S-101 cancellation
/// history, S-124 user state).
// The pushes depend on the enabled features.
#[allow(clippy::vec_init_then_push)]
pub fn user_record_kinds() -> Vec<RecordKind> {
    #[allow(unused_mut)]
    let mut kinds = Vec::new();
    #[cfg(feature = "s101")]
    kinds.push(ferrite_s101::layer::publication::USER_RECORDS);
    #[cfg(feature = "s124")]
    kinds.push(ferrite_s124::user_state::USER_RECORDS);
    kinds
}

/// The record kind of `product`, when built.
pub fn user_record_kind(product: ProductId) -> Option<RecordKind> {
    user_record_kinds()
        .into_iter()
        .find(|kind| kind.product == product)
}

/// The layers of a chart state as the viewer keeps it: the S-101 base chart
/// with the bound catalogue pair, and the S-102 grid layer (empty, no
/// catalogues; the host loads them).
#[cfg(feature = "s101")]
pub fn chart_layers(platform: &Platform, base: s101::BaseChart) -> ProductLayers {
    let mut layers = ProductLayers::new();
    layers.register(SceneRole::BaseChart, Box::new(base.layer(platform)));
    register_grid(&mut layers, platform, None);
    layers
}

/// The layers of a [`crate::ChartEngine`] over the catalogue folder
/// `catalogues` (`FC/<product>`, `PC/<product>`): S-101 (required
/// catalogues), the GML overlay layer of the GML products (optional
/// catalogues), S-421 routes and S-102 (optional catalogue), in this order.
pub fn engine_layers(config: &crate::EngineConfig) -> anyhow::Result<ProductLayers> {
    let mut layers = ProductLayers::new();
    #[cfg(feature = "s101")]
    {
        let base = s101::BaseChart::load(config)?;
        layers.register(SceneRole::BaseChart, Box::new(base.layer(&config.platform)));
    }
    let overlays = crate::gml_overlay::GmlOverlays::load_catalogues(
        &config.platform,
        &config.catalogues,
        config.register_overlay_symbols,
    );
    layers.register(SceneRole::Overlay, Box::new(overlays));
    #[cfg(feature = "s421")]
    layers.register(
        SceneRole::Route,
        Box::new(ferrite_s421::layer::S421Layer::new()),
    );
    register_grid(&mut layers, &config.platform, Some(&config.catalogues));
    Ok(layers)
}

/// S-102 (optional catalogue loaded from `catalogues`), or without the
/// product the notice layer saying S-102 is not built.
fn register_grid(
    layers: &mut ProductLayers,
    platform: &Platform,
    catalogues: Option<&std::path::Path>,
) {
    #[cfg(feature = "s102-pure")]
    {
        let mut layer =
            ferrite_s102::layer::S102Layer::new(Some(std::sync::Arc::clone(&platform.cache)));
        if let Some(catalogues) = catalogues {
            ferrite_s102::layer::load_catalogues(&mut layer, platform.data.as_ref(), catalogues);
        }
        layers.register(SceneRole::Grid, Box::new(layer));
    }
    #[cfg(not(feature = "s102-pure"))]
    {
        let _ = (platform, catalogues);
        layers.register(SceneRole::Grid, Box::new(unbuilt::S102Unbuilt));
    }
}

/// What this build serves for S-102 through the facade.
pub const S102_SUPPORT: &str = if cfg!(feature = "s102-pure") {
    "S-102 3.0.0 bathymetric coverage (regular grid, EPSG:4326, bounding-box \
     domains) read with the pure-Rust HDF5 reader and portrayed as the viewer \
     does; S-102 2.1 trial files are inspected only (never depth)"
} else {
    "S-102 is not built (features s102-pure / s102-hdf5)"
};

/// The S-421 route product (`ferrite_s421::layer`), when built: route input
/// capture and discovery, the adapter and the route layer.
#[cfg(feature = "s421")]
pub mod s421 {
    pub use ferrite_s421::layer::input::{
        capture_if_route, capture_if_route_in, discover_routes_in, CapturedRouteInput,
        RouteInputError,
    };
    pub use ferrite_s421::layer::{S421Layer, S421};
}

/// The S-102 product (`ferrite_s102::layer`), when built: the adapter, the
/// layer, its host options and the opened-file report.
#[cfg(feature = "s102-pure")]
pub mod s102 {
    pub use ferrite_s102::layer::{
        BathymetrySummary, S102Layer, S102Options, DEFAULT_TILE_EDGE, MAX_INPUT_BYTES, S102,
    };
}

/// The S-101 product (`ferrite_s101::layer`), when built: the adapter, the
/// layer and the bound catalogue pair a chart state starts with.
#[cfg(feature = "s101")]
pub mod s101 {
    use crate::catalogue::{load_feature_catalogue_from, load_portrayal_catalogue_from};
    use crate::platform::Platform;
    use anyhow::{ensure, Result};
    use ferrite_feature_catalog::BoundFeatureCatalogue;
    use ferrite_portrayal_catalog::BoundPortrayalCatalogue;
    use ferrite_render::host_settings::CatalogueStatus;
    use ferrite_s101::layer::catalogue::{validate_fc, validate_pc};
    pub use ferrite_s101::layer::{S101Layer, S101};
    use std::path::PathBuf;
    use std::sync::Arc;

    /// The bound S-101 catalogue pair, their statuses and the installed
    /// catalogue inventory for datasets of other editions.
    pub struct BaseChart {
        pub fc: Arc<BoundFeatureCatalogue>,
        pub pc: Arc<BoundPortrayalCatalogue>,
        pub fc_status: CatalogueStatus,
        pub pc_status: CatalogueStatus,
        pub inventory: PathBuf,
    }

    impl BaseChart {
        /// Load and validate the pair of `FC/S-101` and `PC/S-101` under the
        /// configured catalogue folder (required), which must have the
        /// configured colour profile.
        pub fn load(config: &crate::EngineConfig) -> Result<Self> {
            let data = config.platform.data.as_ref();
            let fc_path = config.catalogues.join("FC/S-101");
            let pc_path = config.catalogues.join("PC/S-101");
            let fc = load_feature_catalogue_from(data, &fc_path)?;
            let pc = load_portrayal_catalogue_from(data, &pc_path)?;
            ferrite_s101::validate_catalogue_pair(&fc, &pc)?;
            let fc_status = validate_fc(&fc, &fc.source_path);
            let pc_status = validate_pc(&pc, &pc.root_path);
            ensure!(
                pc.color_profiles.profiles.contains_key(&config.profile),
                "Portrayal catalogue has no colour profile {}",
                config.profile
            );
            Ok(Self {
                fc: Arc::new(fc),
                pc: Arc::new(pc),
                fc_status,
                pc_status,
                inventory: config.inventory.clone().unwrap_or_default(),
            })
        }

        /// The empty S-101 layer of `platform` (its services and
        /// cancellation history location).
        pub fn layer(self, platform: &Platform) -> S101Layer {
            S101Layer::new(
                platform.services(),
                self.fc,
                self.pc,
                self.fc_status,
                self.pc_status,
                self.inventory,
                ferrite_s101::layer::publication::history_path(platform.user.as_ref())
                    .unwrap_or_default(),
            )
        }
    }
}

/// S-102 without the product: HDF5 inputs of open jobs are reported as not
/// built.
#[cfg(not(feature = "s102-pure"))]
mod unbuilt {
    use ferrite_product_api::open::{Loaded, OpenContext, OpenNotes, OpenStage};
    use ferrite_product_api::{
        DatasetInput, FeaturePick, GeoBounds, Opened, PortrayalRequest, ProductId, ProductLayer,
        Recognised, SceneContribution, WorldPoint,
    };
    use std::any::Any;
    use std::path::Path;

    pub struct S102Unbuilt;

    #[derive(Default)]
    struct Stage {
        paths: Vec<std::path::PathBuf>,
    }
    impl OpenStage for Stage {
        fn sniff(&self, name: &Path, _head: &[u8]) -> Option<Recognised> {
            (crate::dataset_discovery::is_dataset_file(name, "h5")
                || crate::dataset_discovery::is_dataset_file(name, "hdf5"))
            .then_some(Recognised::Dataset)
        }
        fn accept(&mut self, path: &Path, _: &OpenContext<'_>, _: &mut OpenNotes) -> bool {
            self.paths.push(path.to_path_buf());
            true
        }
        fn load(
            self: Box<Self>,
            _: &OpenContext<'_>,
            notes: &mut OpenNotes,
        ) -> anyhow::Result<Loaded> {
            for path in self.paths {
                notes.notices.push(format!(
                    "Not opened (S-102 support is not built: feature s102-pure): {}",
                    path.display()
                ));
            }
            Ok(Loaded {
                payload: Box::new(()),
                counts: Vec::new(),
            })
        }
    }

    impl ProductLayer for S102Unbuilt {
        fn products(&self) -> Vec<ProductId> {
            Vec::new()
        }
        fn is_empty(&self) -> bool {
            true
        }
        fn open(&mut self, input: DatasetInput<'_>) -> anyhow::Result<Opened> {
            anyhow::bail!(
                "S-102 support is not built (feature s102-pure): {}",
                input.name.display()
            )
        }
        fn remove(&mut self, _id: u32) -> bool {
            false
        }
        fn set_visible(&mut self, _id: u32, _visible: bool) -> bool {
            false
        }
        fn portray(
            &mut self,
            _request: &PortrayalRequest<'_>,
        ) -> anyhow::Result<Option<SceneContribution>> {
            Ok(None)
        }
        fn coverage(&self) -> Option<GeoBounds> {
            None
        }
        fn describe(&mut self, _key: i64) -> Option<FeaturePick> {
            None
        }
        fn highlight(&mut self, _key: i64) -> Vec<Vec<WorldPoint>> {
            Vec::new()
        }
        fn open_stage(&self) -> Option<Box<dyn OpenStage>> {
            Some(Box::<Stage>::default())
        }
        fn install(
            &mut self,
            _opened: Box<dyn Any + Send>,
            _context: &ferrite_product_api::open::InstallContext<'_>,
            _report: &mut ferrite_product_api::report::InstallReport,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
        fn as_any_mut(&mut self) -> &mut dyn Any {
            self
        }
    }
}
