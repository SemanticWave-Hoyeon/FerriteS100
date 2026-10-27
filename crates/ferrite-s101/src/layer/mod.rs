//! S-101 product layer (`docs/ENGINE_DESIGN.md`, "제품 모듈화", step 4): the
//! engine-facing part of the S-101 product. Exchange-set and update-chain
//! planning ([`update_plan`], [`discovery`]), lifecycle metadata and
//! exchange-set consistency checks ([`lifecycle_metadata`], `xc_*`),
//! dataset catalogue selection ([`catalogue_selection`], [`catalogue`]),
//! publication and cancellation history ([`publication`]), loaded source
//! identities ([`source_identity`]), decoded and portrayal caches
//! ([`decoded_cache`], [`portrayal_cache`]), Lua cell portrayal
//! ([`portrayal`], [`primary_viewing_layers`]) and S-98 interoperability
//! composition of cell instructions ([`interoperability`]).
//!
//! The layer depends on `ferrite-product-api` (platform services), never on
//! the engine.
pub mod catalogue;
pub mod catalogue_selection;
pub mod cells;
pub mod decoded_cache;
pub mod discovery;
pub mod interoperability;
pub mod lifecycle_metadata;
pub mod load;
pub mod portrayal;
pub mod portrayal_cache;
pub mod primary_viewing_layers;
pub mod publication;
pub mod signatures;
pub mod source_identity;
pub mod update_plan;
pub mod xc_capture_pool;
pub mod xc_consistency;
pub mod xc_coverage;
pub mod xc_region;

pub use load::{CellHydrationSource, ChartLoadResult};
pub use portrayal::PortrayalSource;
pub mod state;

pub use state::{
    CellBatch, CellHydration, Inserted, LoadedDatasets, S101Layer, TakenCell, ViewportPixels,
};

use anyhow::Result;
use ferrite_product_api::{
    DatasetInput, Encoding, FeaturePick, GeoBounds, Opened, PortrayalRequest, ProductAdapter,
    ProductId, ProductInfo, ProductLayer, Recognised, SceneContribution, SceneRole, WorldPoint,
};
use std::any::Any;
use std::path::Path;
use std::sync::Arc;

/// The S-101 product identifier.
pub const PRODUCT: ProductId = ProductId("S-101");

/// The S-101 product adapter.
#[derive(Clone, Copy, Debug, Default)]
pub struct S101;

impl S101 {
    /// Whether `head` starts like an ISO 8211 data descriptive record: a
    /// 24-byte leader with the leader identifier `L`.
    pub fn is_iso8211_head(head: &[u8]) -> bool {
        head.len() >= 24 && head[6] == b'L'
    }
}

impl ProductAdapter for S101 {
    fn info(&self) -> ProductInfo {
        ProductInfo {
            id: PRODUCT,
            title: "Electronic Navigational Chart",
            encoding: Encoding::Iso8211,
            role: SceneRole::BaseChart,
        }
    }
    /// Chain members by name (`.000` base or reissue, `.001`… updates, not
    /// AppleDouble companions), as the update planner selects them, whose
    /// head (when given) is an ISO 8211 leader.
    fn sniff(&self, name: &Path, head: &[u8]) -> Option<Recognised> {
        (update_plan::is_chart_file(name) && (head.is_empty() || Self::is_iso8211_head(head)))
            .then_some(Recognised::Dataset)
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

impl S101Layer {
    fn assign_id(&mut self, identity: [u8; 32]) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        self.ids.push((id, identity));
        id
    }

    fn index_of_id(&self, id: u32) -> Option<usize> {
        let identity = self.ids.iter().find(|(i, _)| *i == id)?.1;
        (0..self.cells.len()).find(|index| self.cell_identity_sha(*index) == Some(identity))
    }

    /// Instructions of every cell for `request` in a whole-world view (no
    /// residency, no per-cell cache, no interoperability).
    fn portray_all(&mut self, request: &PortrayalRequest<'_>) -> Result<SceneContribution> {
        let mut view =
            ferrite_render::RenderContext::new(ferrite_render::Viewport::new(1920.0, 1080.0));
        view.scaler
            .set_projection(ferrite_render::FlatProjection::EllipsoidalMercator);
        let bounds = cells::chart_data_bounds(&self.cells, []);
        let (context, _) = self.portray_loaded_cells(
            &view,
            bounds,
            request.profile,
            false,
            request.settings,
            None,
            false,
            no_triangulation,
        )?;
        use std::hash::{Hash, Hasher};
        let mut revision = std::collections::hash_map::DefaultHasher::new();
        self.cell_identities().hash(&mut revision);
        request.profile.hash(&mut revision);
        format!("{:?}", request.settings).hash(&mut revision);
        Ok(SceneContribution {
            revision: revision.finish(),
            instructions: Arc::new(context.raw_instructions().to_vec()),
            extensions: Vec::new(),
        })
    }
}

fn no_triangulation(
    _: &ferrite_render::AreaInstruction,
    _: ferrite_render::FlatProjection,
    _: usize,
) -> Option<ferrite_render::AreaTriangulation> {
    None
}

impl ProductLayer for S101Layer {
    fn products(&self) -> Vec<ProductId> {
        vec![PRODUCT]
    }
    fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }
    /// Read a base cell (or reissue) from its bytes, validate it against the
    /// bound catalogues, normalize its feature codes and install it as a
    /// batch of one (the same identity is already open; a newer edition
    /// replaces the loaded dataset). Update chains, signature snapshots,
    /// lifecycle metadata and cancellations go through the update planner
    /// ([`update_plan`]) and the host's install.
    fn open(&mut self, input: DatasetInput<'_>) -> Result<Opened> {
        let (mut cell, source_identity) =
            ferrite_s100_core::S101Cell::load_update_chain_bytes_with_identity(
                input.name,
                input.bytes.to_vec(),
                Vec::new(),
            )?;
        S101Layer::admit_with(&mut cell, &self.fc, &self.pc)?;
        cell.normalize_feature_codes(&Arc::new(self.fc.feature_type_codes()));
        let identity = *source_identity.sha256();
        let mut batch = self.begin_batch();
        let inserted = self.insert_loaded(
            &mut batch,
            ChartLoadResult {
                base_metadata: None,
                metadata: None,
                input_paths: vec![input.name.to_path_buf()],
                cell,
                source_identity,
                hydration: None,
            },
        )?;
        if let Inserted::AlreadyOpen(_) = inserted {
            return Ok(Opened::AlreadyOpen);
        }
        self.finish_batch()?;
        Ok(Opened::New(self.assign_id(identity)))
    }
    fn remove(&mut self, id: u32) -> bool {
        let Some(index) = self.index_of_id(id) else {
            return false;
        };
        self.remove_dataset(index);
        self.ids.retain(|(i, _)| *i != id);
        self.prune_cell_hydration();
        self.finish_batch().is_ok()
    }
    /// S-101 cells have no visibility of their own (display selection is by
    /// coverage and scale).
    fn set_visible(&mut self, _id: u32, _visible: bool) -> bool {
        false
    }
    /// Lua (or precomputed) portrayal of every cell; None without cells or
    /// when portrayal fails (logged).
    fn portray(&mut self, request: &PortrayalRequest<'_>) -> Option<SceneContribution> {
        if self.cells.is_empty() {
            return None;
        }
        self.portray_all(request)
            .map_err(|error| tracing::warn!("S-101 portrayal failed: {error:#}"))
            .ok()
    }
    fn coverage(&self) -> Option<GeoBounds> {
        (!self.cells.is_empty()).then(|| cells::chart_data_bounds(&self.cells, []))
    }
    /// Feature reports of S-101 picks come from the host's pick surface
    /// (`ferrite_engine::pick`), which resolves the drawn instruction's cell.
    fn describe(&mut self, _key: i64) -> Option<FeaturePick> {
        None
    }
    fn highlight(&mut self, _key: i64) -> Vec<Vec<WorldPoint>> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_product_api::platform::{
        CacheStore, NativeDataSource, SequentialExecutor, Services,
    };
    use std::path::PathBuf;

    #[test]
    fn adapter_sniffs_chain_members_by_name_and_iso8211_leader() {
        let adapter = S101;
        assert_eq!(adapter.id(), PRODUCT);
        assert_eq!(adapter.info().role, SceneRole::BaseChart);
        let mut leader = b"01234".to_vec();
        leader.extend_from_slice(b"3LE1 0900073   6604");
        assert_eq!(leader.len(), 24);
        for (name, head, expected) in [
            ("101FR00369400.000", &[][..], true),
            ("101FR00369400.001", &leader[..], true),
            (
                "101FR00369400.000",
                &b"<?xml version=\"1.0\"?><x/>"[..],
                false,
            ),
            ("._101FR00369400.000", &[][..], false),
            ("101FR00369400.h5", &[][..], false),
            ("CATALOG.XML", &[][..], false),
        ] {
            assert_eq!(
                adapter.sniff(Path::new(name), head) == Some(Recognised::Dataset),
                expected,
                "{name}"
            );
        }
    }

    /// A store without a location: portrayal reads and writes no cache.
    struct NoStore;
    impl CacheStore for NoStore {
        fn root(&self) -> Option<PathBuf> {
            None
        }
        fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                path.display().to_string(),
            ))
        }
        fn modified(&self, _path: &Path) -> Option<std::time::SystemTime> {
            None
        }
        fn write(&self, _path: &Path, _bytes: &[u8]) -> std::io::Result<()> {
            Ok(())
        }
        fn mark_used(&self, _path: &Path) {}
        fn prune(&self) {}
    }

    #[test]
    fn layer_opens_a_cell_once_and_reports_coverage_and_portrayal() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let cell = root.join(
            "TestData/SHOM-S101/Data/diffusionS101_May_June/S-101_June2026/6940/101FR00369400_002.000",
        );
        let fc = root.join("Catalogues/FC/S-101/101_Feature_Catalogue_2.0.0.xml");
        let pc = root.join("Catalogues/PC/S-101");
        if !cell.is_file() || !fc.is_file() || !pc.is_dir() {
            eprintln!("TestData or catalogues not present; skipped");
            return;
        }
        let fc = Arc::new(ferrite_feature_catalog::FeatureCatalogue::load_bound(&fc).unwrap());
        let pc = Arc::new(ferrite_portrayal_catalog::PortrayalCatalogue::load_bound(&pc).unwrap());
        let services = Services {
            data: Arc::new(NativeDataSource),
            cache: Arc::new(NoStore),
            executor: Arc::new(SequentialExecutor),
        };
        let mut layer = S101Layer::new(
            services,
            fc,
            pc,
            Default::default(),
            Default::default(),
            PathBuf::new(),
            PathBuf::new(),
        );
        assert!(ProductLayer::is_empty(&layer));
        assert_eq!(layer.products(), vec![PRODUCT]);
        assert!(layer.coverage().is_none());
        let bytes = std::fs::read(&cell).unwrap();
        let input = |bytes| DatasetInput {
            name: &cell,
            bytes,
            provenance: ferrite_product_api::Provenance::Unverified,
        };
        let Opened::New(id) = layer.open(input(&bytes)).unwrap() else {
            panic!("not opened");
        };
        assert_eq!(layer.open(input(&bytes)).unwrap(), Opened::AlreadyOpen);
        assert_eq!(layer.cells.len(), 1);
        assert_eq!(layer.loaded_datasets().len(), 1);
        let coverage = layer.coverage().unwrap();
        assert!(coverage.min_x < coverage.max_x && coverage.min_y < coverage.max_y);
        assert!(!layer.set_visible(id, false));
        if cfg!(feature = "lua-native") {
            let request = PortrayalRequest {
                profile: "Day",
                viewing_time: None,
                settings: None,
            };
            let first = layer.portray(&request).unwrap();
            assert!(!first.instructions.is_empty());
            assert!(first.extensions.is_empty());
            assert_eq!(layer.portray(&request).unwrap().revision, first.revision);
        }
        assert!(layer.remove(id));
        assert!(!layer.remove(id));
        assert!(ProductLayer::is_empty(&layer));
        assert!(layer
            .portray(&PortrayalRequest {
                profile: "Day",
                viewing_time: None,
                settings: None,
            })
            .is_none());
    }
}
