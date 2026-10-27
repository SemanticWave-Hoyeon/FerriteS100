//! The viewer's glue for the S-421 route editor (`ferrite_s421::editor`):
//! the editing portrayal resources, the screen-projected route overlay and
//! its GPU publication, the file dialogs and the dataset-tree rows. Route
//! state, commands, import/export and the route overlay extension belong to
//! the product.
use anyhow::{ensure, Context, Result};
use ferrite_s421::editor::RouteEditor;
use ferrite_s421::ui::UiEvent;
use std::path::{Path, PathBuf};

pub use ferrite_s421::editor::PROVIDER_ID;

pub struct NativeS421 {
    route: RouteEditor,
    editing_resources: Option<std::sync::Arc<ferrite_gpu::NativeRouteOverlayResources>>,
    editing_resource_error: Option<String>,
    prepared_overlay: Option<std::sync::Arc<ferrite_gpu::PreparedNativeRouteOverlay>>,
    overlay_prepare_error: Option<String>,
    overlay_cache_hits: u64,
    overlay_preparations: u64,
}

impl NativeS421 {
    pub fn new(catalogues: PathBuf) -> Self {
        Self {
            route: RouteEditor::new(catalogues),
            editing_resources: None,
            editing_resource_error: None,
            prepared_overlay: None,
            overlay_prepare_error: None,
            overlay_cache_hits: 0,
            overlay_preparations: 0,
        }
    }
    /// Returns a disconnected workspace: failures leave the displayed controller,
    /// source owners, epochs and GPU publication intact.
    fn fork_local_edit(&self, palette: &str) -> Result<Self> {
        let resources = match &self.editing_resources {
            Some(resources) if resources.palette() == palette => resources.clone(),
            _ => ferrite_ui::load_native_route_overlay_resources(
                self.route.editing_pc_root(),
                palette,
            )
            .map_err(anyhow::Error::msg)?,
        };
        Ok(Self {
            route: self.route.fork(),
            editing_resources: Some(resources),
            editing_resource_error: None,
            prepared_overlay: self.prepared_overlay.clone(),
            overlay_prepare_error: None,
            overlay_cache_hits: self.overlay_cache_hits,
            overlay_preparations: self.overlay_preparations,
        })
    }
    /// Synchronous transaction: all CPU preparation/validation precedes renderer
    /// GPU prepare. Renderer failure must not modify its displayed publication.
    fn publish_local_edit(
        &mut self,
        mut next: Self,
        renderer: &mut ferrite_ui::WgpuRenderer,
        scaler: &ferrite_render::Scaler,
        palette: &str,
    ) -> Result<()> {
        ensure!(
            next.route.revision() == self.route.revision()
                && next.route.overlay_revision() == self.route.overlay_revision(),
            "Native edit workspace became stale"
        );
        ensure!(
            next.route.revision() < u64::MAX && next.route.overlay_revision() < u64::MAX,
            "Native edit revision exhausted"
        );
        next.route.refresh();
        let packet = next.prepare_overlay(scaler, palette)?;
        next.route.mark_displayed();
        // Serialize UI before GPU publication; nothing fallible remains afterward.
        ensure!(
            next.route.revision() < u64::MAX,
            "Native edit UI revision exhausted"
        );
        next.route.refresh_ui();
        renderer
            .sync_native_route_gpu(packet, scaler)
            .map_err(anyhow::Error::msg)?;
        *self = next;
        Ok(())
    }
    pub fn editing(&self) -> bool {
        self.route.editing()
    }
    pub fn begin_local_edit(
        &mut self,
        renderer: &mut ferrite_ui::WgpuRenderer,
        scaler: &ferrite_render::Scaler,
        palette: &str,
    ) -> Result<()> {
        let mut next = self.fork_local_edit(palette)?;
        next.route
            .controller_mut()
            .begin_native_local_route()
            .map_err(anyhow::Error::msg)?;
        self.publish_local_edit(next, renderer, scaler, palette)
    }
    pub fn add_local_waypoint(
        &mut self,
        lon: f64,
        lat: f64,
        renderer: &mut ferrite_ui::WgpuRenderer,
        scaler: &ferrite_render::Scaler,
        palette: &str,
    ) -> Result<()> {
        let mut next = self.fork_local_edit(palette)?;
        next.route
            .controller_mut()
            .append_native_local_waypoint(lon, lat)
            .map_err(anyhow::Error::msg)?;
        self.publish_local_edit(next, renderer, scaler, palette)
    }
    pub fn finish_local_edit(
        &mut self,
        renderer: &mut ferrite_ui::WgpuRenderer,
        scaler: &ferrite_render::Scaler,
        palette: &str,
    ) -> Result<()> {
        let mut next = self.fork_local_edit(palette)?;
        next.route
            .controller_mut()
            .finish_native_local_route()
            .map_err(anyhow::Error::msg)?;
        self.publish_local_edit(next, renderer, scaler, palette)
    }
    pub fn set_local_leg_geometry(
        &mut self,
        id: u32,
        geometry: ferrite_s421::s421::LegGeometry,
        renderer: &mut ferrite_ui::WgpuRenderer,
        scaler: &ferrite_render::Scaler,
        palette: &str,
    ) -> Result<()> {
        let mut next = self.fork_local_edit(palette)?;
        next.route
            .controller_mut()
            .set_native_leg_geometry(id, geometry)
            .map_err(anyhow::Error::msg)?;
        self.publish_local_edit(next, renderer, scaler, palette)
    }
    /// Escape stops collecting clicks; it retains the authored draft. This is
    /// distinct from Finish/export validation and does not mint a product.
    pub fn stop_local_edit(&mut self) {
        self.route.stop_local_edit();
    }

    pub fn panel_visible(&self) -> bool {
        self.route.panel_visible()
    }
    pub fn revision(&self) -> u64 {
        self.route.revision()
    }
    pub fn ui_json(&self) -> &str {
        self.route.ui_json()
    }
    pub fn audit_data(&self) -> serde_json::Value {
        let published =
            self.route.display_available() && self.route.controller().rendering_enabled();
        let mut audit = self.route.audit_data();
        for (key, value) in [
            ("editing_resources", serde_json::json!(self.editing_resources.as_ref().map(|resources| serde_json::json!({
                "source_digest": resources.source_digest().iter().map(|byte| format!("{byte:02x}")).collect::<String>(),
                "actual_product": resources.actual_product(), "actual_version": resources.actual_version(),
                "palette": resources.palette(), "leg_width_mm": resources.leg_style().pen.width,
                "waypoint_pixels": [resources.waypoint_symbol().width, resources.waypoint_symbol().height],
                "gpu_published": published, "official_rules_executed": false,
            })))),
            ("editing_resource_error", serde_json::json!(self.editing_resource_error)),
            ("overlay_prepare_error", serde_json::json!(self.overlay_prepare_error)),
            ("overlay_cache", serde_json::json!({"hits":self.overlay_cache_hits,"preparations":self.overlay_preparations})),
            ("prepared_overlay", serde_json::json!(self.prepared_overlay.as_ref().map(|p| serde_json::json!({
                "revision":p.route_revision(), "camera":p.camera_identity(),
                "route_count":p.paths().len(), "points":p.paths().iter().map(|r|r.waypoints.len()).sum::<usize>(),
                "pixels_per_mm":p.pixels_per_mm(), "gpu_published":published,
                "curve_points":p.paths().iter().map(|r|r.line_points.len()).sum::<usize>(),"segment_owners":p.paths().iter().map(|r|r.segment_owners.len()).sum::<usize>(),
            })))),
        ] {
            audit.insert(key.into(), value);
        }
        serde_json::Value::Object(audit)
    }

    /// Frame producer only. A cache hit is O(1); it does not authorize a GPU
    /// publication, chart selection or map editing before their barriers exist.
    pub fn prepare_overlay(
        &mut self,
        scaler: &ferrite_render::Scaler,
        palette: &str,
    ) -> Result<Option<std::sync::Arc<ferrite_gpu::PreparedNativeRouteOverlay>>> {
        let result = self.prepare_overlay_inner(scaler, palette);
        self.overlay_prepare_error = result.as_ref().err().map(|error| format!("{error:#}"));
        result
    }
    fn prepare_overlay_inner(
        &mut self,
        scaler: &ferrite_render::Scaler,
        palette: &str,
    ) -> Result<Option<std::sync::Arc<ferrite_gpu::PreparedNativeRouteOverlay>>> {
        if self.route.controller().routes().is_empty() {
            self.route.clear_route_overlay();
            self.prepared_overlay = None;
            return Ok(None);
        }
        let resources = self
            .editing_resources
            .as_ref()
            .context("S-421 editing resources unavailable")?;
        ensure!(
            palette == resources.palette(),
            "S-421 editing palette {palette} unavailable; no Day substitution"
        );
        if !self.route.controller().rendering_enabled() {
            self.prepared_overlay = None;
            return Ok(None);
        }
        let revision = self.route.overlay_revision();
        if let Some(packet) = &self.prepared_overlay {
            if packet
                .validate(resources, revision, scaler, scaler.pixels_per_mm())
                .is_ok()
            {
                self.overlay_cache_hits = self.overlay_cache_hits.saturating_add(1);
                return Ok(Some(packet.clone()));
            }
        }
        // The product's route overlay renderer extension, projected for this camera.
        let overlay = self.route.route_overlay()?;
        let prepared = ferrite_gpu::PreparedNativeRouteOverlay::from_extension(
            resources.clone(),
            &overlay.extension(),
            scaler,
            scaler.pixels_per_mm(),
        )
        .map_err(anyhow::Error::msg)?;
        prepared
            .validate(resources, revision, scaler, scaler.pixels_per_mm())
            .map_err(anyhow::Error::msg)?;
        let packet = std::sync::Arc::new(prepared);
        // Failed candidates leave the previous complete cache intact. The caller
        // receives an error and cannot use it under another camera/palette.
        self.route.commit_route_overlay(overlay);
        self.prepared_overlay = Some(packet.clone());
        self.overlay_preparations = self.overlay_preparations.saturating_add(1);
        Ok(Some(packet))
    }
    pub fn resources_support_palette(&self, palette: &str) -> bool {
        self.editing_resources
            .as_ref()
            .is_some_and(|p| p.palette() == palette)
            && !self.route.controller().routes().is_empty()
    }
    pub fn set_gpu_capability(&mut self, available: bool, error: Option<String>) {
        self.route.set_display(available, error);
    }
    pub fn toggle_panel(&mut self) {
        self.route.toggle_panel();
    }
    /// Drop the drawing resources and the projected overlay.
    fn release_drawing(&mut self) {
        self.editing_resources = None;
        self.editing_resource_error = None;
        self.prepared_overlay = None;
        self.overlay_prepare_error = None;
    }
    pub fn clear(&mut self) {
        self.release_drawing();
        self.route.clear();
    }
    /// Capture own public editing resources lazily (Day is explicit
    /// preparation metadata; nothing is rendered under it).
    fn capture_editing_resources(
        pc_root: &Path,
        resources: &mut Option<std::sync::Arc<ferrite_gpu::NativeRouteOverlayResources>>,
        error: &mut Option<String>,
    ) {
        if resources.is_none() && error.is_none() {
            match ferrite_ui::load_native_route_overlay_resources(pc_root, "Day") {
                Ok(loaded) => *resources = Some(loaded),
                Err(e) => *error = Some(e),
            }
        }
    }
    pub fn import_path(&mut self, path: &Path, require_signatures: bool) -> Result<u32> {
        let pc_root = self.route.editing_pc_root().to_path_buf();
        let (resources, error) = (
            &mut self.editing_resources,
            &mut self.editing_resource_error,
        );
        self.route.import_path(path, require_signatures, || {
            Self::capture_editing_resources(&pc_root, resources, error)
        })
    }
    pub fn import_captured(
        &mut self,
        input: &ferrite_s421::layer::input::CapturedRouteInput,
        require_signatures: bool,
    ) -> Result<u32> {
        let pc_root = self.route.editing_pc_root().to_path_buf();
        let (resources, error) = (
            &mut self.editing_resources,
            &mut self.editing_resource_error,
        );
        self.route.import_captured(input, require_signatures, || {
            Self::capture_editing_resources(&pc_root, resources, error)
        })
    }
    pub fn catalogue_layer(&self) -> ferrite_ui::DatasetCatalogueLayer {
        let (fc, mut pc) = self.route.catalogue_status();
        if let Some(resources) = &self.editing_resources {
            pc.version = resources.actual_version().to_owned();
            pc.validation_message = Some(format!(
                "Own captured public editing PC · palette {} · XSLT not executed",
                resources.palette()
            ));
        }
        ferrite_ui::DatasetCatalogueLayer {
            product: "S-421".into(),
            fc,
            pc,
        }
    }
    pub fn dataset_rows(&self) -> Vec<ferrite_ui::DatasetLayerEntry> {
        self.route
            .dataset_rows()
            .into_iter()
            .map(|row| ferrite_ui::DatasetLayerEntry {
                id: ferrite_ui::DatasetLayerId::S421 {
                    route_id: row.route_id,
                },
                name: row.name,
                source: row.source,
                detail: row.detail,
            })
            .collect()
    }
    pub fn unload_route(&mut self, id: u32) -> Result<()> {
        self.route.unload_route(id)?;
        self.prepared_overlay = None;
        Ok(())
    }
    pub fn export_path(&self, path: &Path) -> Result<()> {
        self.route.export_path(path)
    }
    pub fn handle_event(&mut self, json: &str, require_signatures: bool) -> Result<()> {
        let event = RouteEditor::parse_event(json)?;
        match &event {
            UiEvent::Import => {
                RouteEditor::check_import_allowed(require_signatures)?;
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("S-421 route", &["gml", "xml"])
                    .pick_file()
                {
                    self.import_path(&path, require_signatures)?;
                }
                return Ok(());
            }
            UiEvent::Export => {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("S-421 minimal route", &["gml"])
                    .set_file_name("route.gml")
                    .save_file()
                {
                    self.export_path(&path)?;
                }
                return Ok(());
            }
            _ => {}
        }
        if self.route.apply_event(event)? && self.route.controller().routes().is_empty() {
            self.release_drawing();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_s421::s421::{PublishedExport, MAX_XML_BYTES};
    use ferrite_s421::{
        route::{Route, Waypoint},
        s421::export_published,
    };
    fn fixture(dir: &Path) -> PathBuf {
        let mut route = Route::with_name(0, "Test route");
        for (id, lon, lat) in [(1, -2.5, 50.), (2, -2.3, 50.1)] {
            let mut wp = Waypoint::new(id, lon, lat);
            wp.turn_radius = Some(0.5);
            route.add_waypoint(wp);
        }
        let xml = export_published(
            &route,
            PublishedExport {
                route_id: "TEST.ROUTE",
                edition: 1,
                status: 1,
            },
        )
        .unwrap();
        let path = dir.join("input.gml");
        std::fs::write(&path, xml).unwrap();
        path
    }
    #[test]
    fn imported_loxodrome_route_edits_through_an_authored_copy_and_reloads() {
        use ferrite_s421::s421::LegGeometry;
        let dir = tempfile::tempdir().unwrap();
        let mut route = Route::with_name(0, "Lox route");
        for (id, lon, lat) in [(1, -2.5, 50.), (2, -2.3, 50.1), (3, -2.1, 50.15)] {
            let mut wp = Waypoint::new(id, lon, lat);
            wp.turn_radius = Some(0.5);
            if id > 1 {
                wp.incoming_geometry = Some(LegGeometry::Loxodrome);
            }
            route.add_waypoint(wp);
        }
        let xml = export_published(
            &route,
            PublishedExport {
                route_id: "LOX.ROUTE",
                edition: 1,
                status: 1,
            },
        )
        .unwrap();
        let input = dir.path().join("lox.gml");
        std::fs::write(&input, &xml).unwrap();
        let mut host = NativeS421::new(dir.path().join("Catalogues"));
        let imported = host.import_path(&input, false).unwrap();
        // The original saves byte-for-byte; its content stays read-only.
        let original = dir.path().join("original.gml");
        host.export_path(&original).unwrap();
        assert_eq!(std::fs::read_to_string(&original).unwrap(), xml);
        let ui: serde_json::Value = serde_json::from_str(host.ui_json()).unwrap();
        let copy = ui["actions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["id"] == "copy")
            .unwrap();
        assert_eq!(copy["enabled"], true);
        host.handle_event(r#"{"type":"CopyAsAuthored"}"#, false)
            .unwrap();
        let active = host.route.controller().active_route().unwrap().id;
        assert_ne!(active, imported);
        assert!(host.route.controller().source_for_route(active).is_none());
        // The copy is editable and keeps the declared loxodromes.
        let event =
            serde_json::json!({"type":"SetTurnRadius","id":2,"radius_nm":"0.25"}).to_string();
        host.handle_event(&event, false).unwrap();
        let edited = dir.path().join("edited.gml");
        host.export_path(&edited).unwrap();
        let saved = std::fs::read_to_string(&edited).unwrap();
        assert!(saved.contains("<gml:LineStringSegment interpolation=\"linear\">"));
        let reloaded = host.import_path(&edited, false).unwrap();
        let route = host
            .route
            .controller()
            .routes()
            .iter()
            .find(|r| r.id == reloaded)
            .unwrap();
        assert_eq!(route.waypoints[1].turn_radius, Some(0.25));
        assert!(route.waypoints[1..]
            .iter()
            .all(|w| w.incoming_geometry == Some(LegGeometry::Loxodrome)));
        // The imported original is unchanged.
        let source = host.route.controller().source_for_route(imported).unwrap();
        assert_eq!(&*source.original_xml, xml.as_str());
    }
    #[test]
    fn original_save_and_explicit_radius_events_preserve_source_and_failed_state() {
        let dir = tempfile::tempdir().unwrap();
        let input = fixture(dir.path());
        let original = std::fs::read(&input).unwrap();
        let mut host = NativeS421::new(dir.path().join("Catalogues"));
        host.import_path(&input, false).unwrap();
        let out = dir.path().join("saved.gml");
        host.export_path(&out).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), original);
        let ui: serde_json::Value = serde_json::from_str(host.ui_json()).unwrap();
        assert_eq!(ui["content_editable"], false);
        assert_eq!(ui["turn_radius_editable"], false);
        assert_eq!(ui["waypoints"][0]["turn_radius_nm"], 0.5);
        assert_eq!(
            ui["actions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|a| a["id"] == "export")
                .unwrap()["label"],
            "Save original"
        );
        let before = host.ui_json().to_string();
        assert!(host
            .handle_event(
                r#"{"type":"SetTurnRadius","id":1,"radius_nm":"0.25"}"#,
                false
            )
            .is_err());
        assert_eq!(host.ui_json(), before);
        host.route
            .controller_mut()
            .begin_native_local_route()
            .unwrap();
        let id = host
            .route
            .controller_mut()
            .append_native_local_waypoint(25., 59.)
            .unwrap();
        let event =
            serde_json::json!({"type":"SetTurnRadius","id":id,"radius_nm":"0.25"}).to_string();
        host.handle_event(&event, false).unwrap();
        let local_before =
            serde_json::to_vec(host.route.controller().active_route().unwrap()).unwrap();
        let ui: serde_json::Value = serde_json::from_str(host.ui_json()).unwrap();
        assert_eq!(ui["content_editable"], true);
        assert_eq!(ui["waypoints"][0]["turn_radius_nm"], 0.25);
        let event =
            serde_json::json!({"type":"SetTurnRadius","id":id,"radius_nm":"0.251"}).to_string();
        assert!(host.handle_event(&event, false).is_err());
        assert_eq!(
            serde_json::to_vec(host.route.controller().active_route().unwrap()).unwrap(),
            local_before
        );
    }
    #[test]
    fn exact_route_frame_cache_reuses_and_invalidates_without_day_fallback() {
        use ferrite_render::{GeoBounds, Scaler, Viewport};
        use std::sync::Arc;
        let dir = tempfile::tempdir().unwrap();
        let catalogues = dir.path().join("Catalogues");
        let pc = catalogues.join("PC/S-421");
        for directory in ["Symbols", "LineStyles", "ColorProfiles"] {
            std::fs::create_dir_all(pc.join(directory)).unwrap();
        }
        for (name, bytes) in [
            (
                "portrayal_catalogue.xml",
                include_bytes!(
                    "../crates/ferrite-s421/tests/fixtures/editing/portrayal_catalogue.xml"
                )
                .as_slice(),
            ),
            (
                "Symbols/RTEWPT01.svg",
                include_bytes!("../crates/ferrite-s421/tests/fixtures/editing/RTEWPT01.svg")
                    .as_slice(),
            ),
            (
                "LineStyles/RTEACTLEGLINE.xml",
                include_bytes!("../crates/ferrite-s421/tests/fixtures/editing/RTEACTLEGLINE.xml")
                    .as_slice(),
            ),
            (
                "ColorProfiles/colorProfile.xml",
                include_bytes!("../crates/ferrite-s421/tests/fixtures/editing/colorProfile.xml")
                    .as_slice(),
            ),
        ] {
            std::fs::write(pc.join(name), bytes).unwrap();
        }
        let mut host = NativeS421::new(catalogues);
        let mut scaler = Scaler::new(GeoBounds::new(-3., 49., 0., 52.), Viewport::new(800., 600.));
        assert!(host.prepare_overlay(&scaler, "Day").unwrap().is_none());
        let path = fixture(dir.path());
        host.import_path(&path, false).unwrap();
        let first = host.prepare_overlay(&scaler, "Day").unwrap().unwrap();
        let source_cache = host.route.world_samples().unwrap().clone();
        let repeated = host.prepare_overlay(&scaler, "Day").unwrap().unwrap();
        assert!(Arc::ptr_eq(&first, &repeated));
        assert_eq!((host.overlay_preparations, host.overlay_cache_hits), (1, 1));
        scaler.pan(1., 0.);
        assert!(!first.matches_camera(&scaler));
        let moved = host.prepare_overlay(&scaler, "Day").unwrap().unwrap();
        assert!(!Arc::ptr_eq(&first, &moved));
        assert!(moved.matches_camera(&scaler));
        assert!(Arc::ptr_eq(
            &source_cache,
            host.route.world_samples().unwrap()
        ));
        assert!(host.prepare_overlay(&scaler, "Night").is_err());
        assert!(Arc::ptr_eq(host.prepared_overlay.as_ref().unwrap(), &moved));
        assert!(Arc::ptr_eq(
            &moved,
            &host.prepare_overlay(&scaler, "Day").unwrap().unwrap()
        ));
        let old_revision = host.route.overlay_revision();
        assert!(host
            .handle_event(r#"{"type":"RenameRoute","id":1,"name":"Renamed"}"#, false)
            .is_err());
        assert_eq!(host.route.overlay_revision(), old_revision);
        let revised = host.prepare_overlay(&scaler, "Day").unwrap().unwrap();
        assert!(Arc::ptr_eq(
            &source_cache,
            host.route.world_samples().unwrap()
        ));
        assert!(Arc::ptr_eq(&moved, &revised));
        assert_eq!(revised.route_revision(), host.route.overlay_revision());
        // Reopening the same captured input is idempotent after a rejected edit;
        // it must not discard the retained route or append a copy.
        host.import_path(&path, false).unwrap();
        let reopened = host.prepare_overlay(&scaler, "Day").unwrap().unwrap();
        assert!(Arc::ptr_eq(&revised, &reopened));
        assert_eq!(reopened.paths().len(), 1);
        // A separately selected delivery remains a distinct source owner.
        let second_path = dir.path().join("second-route.gml");
        std::fs::copy(&path, &second_path).unwrap();
        host.import_path(&second_path, false).unwrap();
        let combined = host.prepare_overlay(&scaler, "Day").unwrap().unwrap();
        assert_eq!(
            combined
                .paths()
                .iter()
                .map(|route| route.route_id)
                .collect::<Vec<_>>(),
            [1, 2]
        );
        assert_eq!(
            combined.paths()[0].waypoints[0].0,
            combined.paths()[1].waypoints[0].0
        );
        scaler.set_pixel_ratio(2.);
        assert!(!combined.matches_camera(&scaler));
        let calibrated = host.prepare_overlay(&scaler, "Day").unwrap().unwrap();
        assert_eq!(
            calibrated.pixels_per_mm().to_bits(),
            scaler.pixels_per_mm().to_bits()
        );
        let before_status = host.prepare_overlay(&scaler, "Day").unwrap().unwrap();
        let geometry_revision = host.route.overlay_revision();
        let ui_revision = host.revision();
        host.set_gpu_capability(true, None);
        assert!(host.revision() > ui_revision);
        assert_eq!(host.route.overlay_revision(), geometry_revision);
        assert!(Arc::ptr_eq(
            &before_status,
            &host.prepare_overlay(&scaler, "Day").unwrap().unwrap()
        ));
        host.handle_event(r#"{"type":"ToggleRendering"}"#, false)
            .unwrap();
        assert!(!host.route.controller().rendering_enabled());
        assert!(host.prepare_overlay(&scaler, "Day").unwrap().is_none());
        host.handle_event(r#"{"type":"ToggleRendering"}"#, false)
            .unwrap();
        assert!(host.route.controller().rendering_enabled());
        assert!(host.prepare_overlay(&scaler, "Day").unwrap().is_some());
        host.clear();
        assert!(host.prepared_overlay.is_none());
        assert!(host.route.world_samples().is_none());
        assert!(host.prepare_overlay(&scaler, "Day").unwrap().is_none());
        assert!(host.overlay_prepare_error.is_none());
    }
    #[test]
    fn owned_import_is_atomic_bounded_and_signature_policy_is_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture(dir.path());
        let mut host = NativeS421::new(dir.path().join("Catalogues"));
        let before = host.revision();
        assert!(host.import_path(&path, true).is_err());
        assert_eq!(host.revision(), before);
        assert!(!host.panel_visible());
        let id = host.import_path(&path, false).unwrap();
        assert_eq!(id, 1);
        assert!(host.editing_resources.is_none());
        assert!(host.editing_resource_error.is_some());
        assert!(
            !serde_json::from_str::<serde_json::Value>(host.ui_json()).unwrap()
                ["display_available"]
                .as_bool()
                .unwrap()
        );
        assert!(host.panel_visible());
        let before = host.ui_json().to_string();
        let revision = host.revision();
        std::fs::write(&path, "<bad/>").unwrap();
        assert!(host.import_path(&path, false).is_err());
        assert_eq!(host.ui_json(), before);
        assert_eq!(host.revision(), revision);
        std::fs::write(&path, vec![b' '; MAX_XML_BYTES + 1]).unwrap();
        assert!(host.import_path(&path, false).is_err());
        assert_eq!(host.route.controller().routes().len(), 1);
        assert_eq!(
            host.route.controller().source_for_route(id).unwrap().routes[0]
                .route
                .waypoints
                .len(),
            2
        );
        let output = dir.path().join("out.gml");
        host.export_path(&output).unwrap();
        let parsed =
            ferrite_s421::s421::import_dataset(&std::fs::read_to_string(output).unwrap()).unwrap();
        assert_eq!(parsed.routes[0].route.waypoints.len(), 2);
    }
    #[test]
    fn native_commands_cannot_enable_unprepared_map_editor_or_mutate_on_invalid_names() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture(dir.path());
        let mut host = NativeS421::new(dir.path().join("Catalogues"));
        host.import_path(&path, false).unwrap();
        let before = host.ui_json().to_string();
        let revision = host.revision();
        for command in [
            r#"{"type":"New"}"#,
            r#"{"type":"ToggleRendering"}"#,
            r#"{"type":"RenameRoute","id":1,"name":"bad\nname"}"#,
        ] {
            assert!(host.handle_event(command, false).is_err());
            assert_eq!(host.ui_json(), before);
            assert_eq!(host.revision(), revision);
        }
        assert!(host
            .handle_event(r#"{"type":"RenameRoute","id":1,"name":"Updated"}"#, false)
            .is_err());
        assert_eq!(host.ui_json(), before);
        assert_eq!(host.revision(), revision);
        host.handle_event(r#"{"type":"DeleteRoute","id":1}"#, false)
            .unwrap();
        assert!(host.route.controller().routes().is_empty());
        assert!(host.route.sources().is_empty());
        assert!(host.editing_resources.is_none());
        assert!(host.editing_resource_error.is_none());
        host.clear();
        assert!(!host.panel_visible());
        assert!(!host.route.controller().editing());
    }
    #[test]
    fn captured_open_keeps_original_bytes_after_file_mutation_and_policy_denies_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture(dir.path());
        let input = ferrite_s421::layer::input::capture_if_route(&path)
            .unwrap()
            .unwrap();
        std::fs::write(&path, "<mutated/>").unwrap();
        let mut host = NativeS421::new(dir.path().join("Catalogues"));
        let before = host.revision();
        assert!(host.import_captured(&input, true).is_err());
        assert_eq!(host.revision(), before);
        let id = host.import_captured(&input, false).unwrap();
        assert_eq!(
            host.route
                .controller()
                .source_for_route(id)
                .unwrap()
                .original_xml
                .as_bytes(),
            input.xml().as_bytes()
        );
        assert_eq!(host.import_captured(&input, false).unwrap(), id);
        assert_eq!(host.route.controller().routes().len(), 1);
        assert_eq!(
            host.dataset_rows()[0].id,
            ferrite_ui::DatasetLayerId::S421 { route_id: id }
        );
    }
    #[test]
    fn unloading_one_native_route_preserves_other_source_owner_and_rejects_stale_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture(dir.path());
        let copy = dir.path().join("other.gml");
        std::fs::copy(&path, &copy).unwrap();
        let mut host = NativeS421::new(dir.path().join("Catalogues"));
        let first = host.import_path(&path, false).unwrap();
        let second = host.import_path(&copy, false).unwrap();
        let other = host
            .route
            .controller()
            .source_for_route(second)
            .unwrap()
            .original_xml
            .clone();
        host.unload_route(first).unwrap();
        assert!(host.unload_route(first).is_err());
        assert_eq!(host.route.controller().routes().len(), 1);
        assert_eq!(host.route.controller().routes()[0].id, second);
        assert_eq!(
            host.route
                .controller()
                .source_for_route(second)
                .unwrap()
                .original_xml,
            other
        );
        assert_eq!(host.route.sources().get(&second), Some(&copy));
        assert!(!host.route.display_available());
    }

    fn own_editing_fixture(catalogues: &Path) {
        let pc = catalogues.join("PC/S-421");
        for d in ["Symbols", "LineStyles", "ColorProfiles"] {
            std::fs::create_dir_all(pc.join(d)).unwrap();
        }
        for (p, b) in [
            (
                "portrayal_catalogue.xml",
                include_bytes!(
                    "../crates/ferrite-s421/tests/fixtures/editing/portrayal_catalogue.xml"
                )
                .as_slice(),
            ),
            (
                "Symbols/RTEWPT01.svg",
                include_bytes!("../crates/ferrite-s421/tests/fixtures/editing/RTEWPT01.svg")
                    .as_slice(),
            ),
            (
                "LineStyles/RTEACTLEGLINE.xml",
                include_bytes!("../crates/ferrite-s421/tests/fixtures/editing/RTEACTLEGLINE.xml")
                    .as_slice(),
            ),
            (
                "ColorProfiles/colorProfile.xml",
                include_bytes!("../crates/ferrite-s421/tests/fixtures/editing/colorProfile.xml")
                    .as_slice(),
            ),
        ] {
            std::fs::write(pc.join(p), b).unwrap();
        }
    }
    #[test]
    fn checked_local_workspace_accepts_empty_one_and_two_points_without_mutating_host() {
        use ferrite_render::{GeoBounds, Scaler, Viewport};
        let dir = tempfile::tempdir().unwrap();
        let catalogues = dir.path().join("Catalogues");
        own_editing_fixture(&catalogues);
        let mut host = NativeS421::new(catalogues);
        let imported = host.import_path(&fixture(dir.path()), false).unwrap();
        let before = host.audit_data();
        let source = host
            .route
            .controller()
            .source_for_route(imported)
            .unwrap()
            .original_xml
            .clone();
        let mut next = host.fork_local_edit("Day").unwrap();
        let local = next
            .route
            .controller_mut()
            .begin_native_local_route()
            .unwrap();
        assert_ne!(imported, local);
        next.route.refresh();
        let scaler = Scaler::new(GeoBounds::new(-3., 49., 0., 52.), Viewport::new(800., 600.));
        let empty = next.prepare_overlay(&scaler, "Day").unwrap().unwrap();
        assert!(empty
            .paths()
            .iter()
            .find(|p| p.route_id == local)
            .unwrap()
            .waypoints
            .is_empty());
        assert!(next
            .route
            .controller_mut()
            .finish_native_local_route()
            .is_err());
        assert!(next.route.controller().editing());
        next.route
            .controller_mut()
            .append_native_local_waypoint(-2.5, 50.)
            .unwrap();
        next.route.refresh();
        let one = next.prepare_overlay(&scaler, "Day").unwrap().unwrap();
        assert_eq!(
            one.paths()
                .iter()
                .find(|p| p.route_id == local)
                .unwrap()
                .waypoints
                .len(),
            1
        );
        assert!(next
            .route
            .controller_mut()
            .finish_native_local_route()
            .is_err());
        next.route
            .controller_mut()
            .append_native_local_waypoint(-2.4, 50.1)
            .unwrap();
        next.route.refresh();
        let two = next.prepare_overlay(&scaler, "Day").unwrap().unwrap();
        assert_eq!(
            two.paths()
                .iter()
                .find(|p| p.route_id == local)
                .unwrap()
                .waypoints
                .len(),
            2
        );
        next.route
            .controller_mut()
            .finish_native_local_route()
            .unwrap();
        assert!(!next.route.controller().editing());
        assert!(next
            .route
            .controller()
            .active_route()
            .unwrap()
            .waypoints
            .iter()
            .all(|p| p.turn_radius.is_none()));
        assert!(next
            .route
            .controller()
            .export_active_published(PublishedExport {
                route_id: "LOCAL.ROUTE",
                edition: 1,
                status: 1
            })
            .is_err());
        assert_eq!(
            source.as_bytes(),
            next.route
                .controller()
                .source_for_route(imported)
                .unwrap()
                .original_xml
                .as_bytes()
        );
        assert_eq!(host.audit_data(), before);
    }
    #[test]
    fn checked_local_failed_candidates_leave_authoritative_host_and_counters_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let catalogues = dir.path().join("Catalogues");
        let host = NativeS421::new(catalogues.clone());
        let before = host.audit_data();
        assert!(host.fork_local_edit("Day").is_err());
        assert_eq!(host.audit_data(), before);
        own_editing_fixture(&catalogues);
        let mut next = host.fork_local_edit("Day").unwrap();
        next.route
            .controller_mut()
            .begin_native_local_route()
            .unwrap();
        let snapshot = serde_json::to_vec(next.route.controller().routes()).unwrap();
        for (lon, lat) in [(f64::NAN, 50.), (181., 50.), (0., 91.)] {
            assert!(next
                .route
                .controller_mut()
                .append_native_local_waypoint(lon, lat)
                .is_err());
            assert_eq!(
                serde_json::to_vec(next.route.controller().routes()).unwrap(),
                snapshot
            );
        }
        assert!(next
            .route
            .controller_mut()
            .begin_native_local_route()
            .is_err());
        assert!(next
            .route
            .controller_mut()
            .finish_native_local_route()
            .is_err());
        assert_eq!(
            next.route
                .controller_mut()
                .append_native_local_waypoint(179., 50.)
                .unwrap(),
            1
        );
        let snapshot = serde_json::to_vec(next.route.controller().routes()).unwrap();
        assert!(next
            .route
            .controller_mut()
            .append_native_local_waypoint(-179., 50.)
            .is_err());
        assert_eq!(
            serde_json::to_vec(next.route.controller().routes()).unwrap(),
            snapshot
        );
        assert_eq!(
            next.route
                .controller_mut()
                .append_native_local_waypoint(178., 50.)
                .unwrap(),
            2
        );
        next.route
            .controller_mut()
            .finish_native_local_route()
            .unwrap();
        assert!(next
            .route
            .controller_mut()
            .append_native_local_waypoint(177., 50.)
            .is_err());
        assert!(host.fork_local_edit("UnknownPalette").is_err());
        assert_eq!(host.audit_data(), before);
    }
}
