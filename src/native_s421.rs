//! Statically linked route receiver/editor state. No external DLL or ENC owner.
use abi_stable::std_types::RStr;
use anyhow::{ensure, Context, Result};
use ferrite_plugin_api::Plugin;
use ferrite_s421::{s421::PublishedExport, ui::UiEvent, RouteController};
use std::{
    collections::HashMap,
    io::Write,
    path::{Path, PathBuf},
};

pub const PROVIDER_ID: &str = "native:s421-route";
const MAX_EVENT_BYTES: usize = 64 * 1024;

pub struct NativeS421 {
    controller: RouteController,
    sources: HashMap<u32, PathBuf>,
    revision: u64,
    overlay_revision: u64,
    gpu_capable: bool,
    gpu_error: Option<String>,
    ui_json: String,
    editing_pc_root: PathBuf,
    editing_resources: Option<std::sync::Arc<ferrite_wgpu::NativeRouteOverlayResources>>,
    editing_resource_error: Option<String>,
    prepared_overlay: Option<std::sync::Arc<ferrite_wgpu::PreparedNativeRouteOverlay>>,
    overlay_prepare_error: Option<String>,
    world_samples: Option<std::sync::Arc<ferrite_wgpu::NativeRouteWorldSamples>>,
    overlay_cache_hits: u64,
    overlay_preparations: u64,
    world_sample_preparations: u64,
}

impl NativeS421 {
    pub fn new(catalogues: PathBuf) -> Self {
        let editing_pc_root = catalogues.join("PC/S-421");
        let mut host = Self {
            controller: RouteController::with_catalogues(catalogues),
            sources: HashMap::new(),
            revision: 0,
            overlay_revision: 0,
            gpu_capable: false,
            gpu_error: None,
            ui_json: String::new(),
            editing_pc_root,
            editing_resources: None,
            editing_resource_error: None,
            prepared_overlay: None,
            overlay_prepare_error: None,
            world_samples: None,
            overlay_cache_hits: 0,
            overlay_preparations: 0,
            world_sample_preparations: 0,
        };
        host.refresh();
        host
    }
    /// Returns a disconnected workspace: failures leave the displayed controller,
    /// source owners, epochs and GPU publication intact.
    fn fork_local_edit(&self, palette: &str) -> Result<Self> {
        let resources = match &self.editing_resources {
            Some(resources) if resources.palette() == palette => resources.clone(),
            _ => ferrite_wgpu::NativeRouteOverlayResources::load(&self.editing_pc_root, palette)
                .map_err(anyhow::Error::msg)?,
        };
        Ok(Self {
            controller: self.controller.fork_native_transaction(),
            sources: self.sources.clone(),
            revision: self.revision,
            overlay_revision: self.overlay_revision,
            gpu_capable: self.gpu_capable,
            gpu_error: self.gpu_error.clone(),
            ui_json: self.ui_json.clone(),
            editing_pc_root: self.editing_pc_root.clone(),
            editing_resources: Some(resources),
            editing_resource_error: None,
            prepared_overlay: self.prepared_overlay.clone(),
            overlay_prepare_error: None,
            world_samples: self.world_samples.clone(),
            overlay_cache_hits: self.overlay_cache_hits,
            overlay_preparations: self.overlay_preparations,
            world_sample_preparations: self.world_sample_preparations,
        })
    }
    /// Synchronous transaction: all CPU preparation/validation precedes renderer
    /// GPU prepare. Renderer failure must not modify its displayed publication.
    fn publish_local_edit(
        &mut self,
        mut next: Self,
        renderer: &mut ferrite_wgpu::WgpuRenderer,
        scaler: &ferrite_render::Scaler,
        palette: &str,
    ) -> Result<()> {
        ensure!(
            next.revision == self.revision && next.overlay_revision == self.overlay_revision,
            "Native edit workspace became stale"
        );
        ensure!(
            next.revision < u64::MAX && next.overlay_revision < u64::MAX,
            "Native edit revision exhausted"
        );
        next.refresh();
        let packet = next.prepare_overlay(scaler, palette)?;
        next.gpu_capable = true;
        next.gpu_error = None;
        // Serialize UI before GPU publication; nothing fallible remains afterward.
        ensure!(
            next.revision < u64::MAX,
            "Native edit UI revision exhausted"
        );
        next.refresh_ui();
        renderer
            .sync_native_route_gpu(packet, scaler)
            .map_err(anyhow::Error::msg)?;
        *self = next;
        Ok(())
    }
    pub fn editing(&self) -> bool {
        self.controller.editing()
    }
    pub fn begin_local_edit(
        &mut self,
        renderer: &mut ferrite_wgpu::WgpuRenderer,
        scaler: &ferrite_render::Scaler,
        palette: &str,
    ) -> Result<()> {
        let mut next = self.fork_local_edit(palette)?;
        next.controller
            .begin_native_local_route()
            .map_err(anyhow::Error::msg)?;
        self.publish_local_edit(next, renderer, scaler, palette)
    }
    pub fn add_local_waypoint(
        &mut self,
        lon: f64,
        lat: f64,
        renderer: &mut ferrite_wgpu::WgpuRenderer,
        scaler: &ferrite_render::Scaler,
        palette: &str,
    ) -> Result<()> {
        let mut next = self.fork_local_edit(palette)?;
        next.controller
            .append_native_local_waypoint(lon, lat)
            .map_err(anyhow::Error::msg)?;
        self.publish_local_edit(next, renderer, scaler, palette)
    }
    pub fn finish_local_edit(
        &mut self,
        renderer: &mut ferrite_wgpu::WgpuRenderer,
        scaler: &ferrite_render::Scaler,
        palette: &str,
    ) -> Result<()> {
        let mut next = self.fork_local_edit(palette)?;
        next.controller
            .finish_native_local_route()
            .map_err(anyhow::Error::msg)?;
        self.publish_local_edit(next, renderer, scaler, palette)
    }
    pub fn set_local_leg_geometry(
        &mut self,
        id: u32,
        geometry: ferrite_s421::s421::LegGeometry,
        renderer: &mut ferrite_wgpu::WgpuRenderer,
        scaler: &ferrite_render::Scaler,
        palette: &str,
    ) -> Result<()> {
        let mut next = self.fork_local_edit(palette)?;
        next.controller
            .set_native_leg_geometry(id, geometry)
            .map_err(anyhow::Error::msg)?;
        self.publish_local_edit(next, renderer, scaler, palette)
    }
    /// Escape stops collecting clicks; it retains the authored draft. This is
    /// distinct from Finish/export validation and does not mint a product.
    pub fn stop_local_edit(&mut self) {
        self.controller.set_editing(false);
        self.refresh_ui();
    }

    pub fn panel_visible(&self) -> bool {
        self.controller.panel_visible()
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn ui_json(&self) -> &str {
        &self.ui_json
    }
    pub fn audit_data(&self) -> serde_json::Value {
        use sha2::{Digest, Sha256};
        serde_json::json!({
            "provider_id": PROVIDER_ID, "panel_visible": self.panel_visible(),
            "revision": self.revision, "editing": self.controller.editing(),
            "routes": self.controller.routes().iter().map(|route| serde_json::json!({
                "id":route.id, "waypoints":route.waypoints.len(),
                "source":self.sources.get(&route.id),
                "original_xml_sha256":self.controller.source_for_route(route.id)
                    .map(|s| format!("{:x}", Sha256::digest(s.original_xml.as_bytes()))),
            })).collect::<Vec<_>>(),
            "editing_resources": self.editing_resources.as_ref().map(|resources| serde_json::json!({
                "source_digest": resources.source_digest().iter().map(|byte| format!("{byte:02x}")).collect::<String>(),
                "actual_product": resources.actual_product(), "actual_version": resources.actual_version(),
                "palette": resources.palette(), "leg_width_mm": resources.leg_style().pen.width,
                "waypoint_pixels": [resources.waypoint_symbol().width, resources.waypoint_symbol().height],
                "gpu_published": self.gpu_capable && self.controller.rendering_enabled(), "official_rules_executed": false,
            })),
            "editing_resource_error": self.editing_resource_error,
            "overlay_prepare_error": self.overlay_prepare_error,
            "gpu_capable":self.gpu_capable,"gpu_error":self.gpu_error,"overlay_revision":self.overlay_revision,
            "overlay_cache": {"hits":self.overlay_cache_hits,"preparations":self.overlay_preparations},
            "world_cache": self.world_samples.as_ref().map(|w|serde_json::json!({"payload_bytes":w.payload_bytes(),"solver_legs_in_cache":w.solver_invocations(),"committed_preparations":self.world_sample_preparations})),
            "prepared_overlay": self.prepared_overlay.as_ref().map(|p| serde_json::json!({
                "revision":p.route_revision(), "camera":p.camera_identity(),
                "route_count":p.paths().len(), "points":p.paths().iter().map(|r|r.waypoints.len()).sum::<usize>(),
                "pixels_per_mm":p.pixels_per_mm(), "gpu_published":self.gpu_capable && self.controller.rendering_enabled(),
                "curve_points":p.paths().iter().map(|r|r.line_points.len()).sum::<usize>(),"segment_owners":p.paths().iter().map(|r|r.segment_owners.len()).sum::<usize>(),
            })),
            "ui":serde_json::from_str::<serde_json::Value>(&self.ui_json).expect("typed native UI")
        })
    }

    /// Frame producer only. A cache hit is O(1); it does not authorize a GPU
    /// publication, chart selection or map editing before their barriers exist.
    pub fn prepare_overlay(
        &mut self,
        scaler: &ferrite_render::Scaler,
        palette: &str,
    ) -> Result<Option<std::sync::Arc<ferrite_wgpu::PreparedNativeRouteOverlay>>> {
        let result = self.prepare_overlay_inner(scaler, palette);
        self.overlay_prepare_error = result.as_ref().err().map(|error| format!("{error:#}"));
        result
    }
    fn prepare_overlay_inner(
        &mut self,
        scaler: &ferrite_render::Scaler,
        palette: &str,
    ) -> Result<Option<std::sync::Arc<ferrite_wgpu::PreparedNativeRouteOverlay>>> {
        if self.controller.routes().is_empty() {
            self.world_samples = None;
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
        if !self.controller.rendering_enabled() {
            self.prepared_overlay = None;
            return Ok(None);
        }
        if let Some(packet) = &self.prepared_overlay {
            if packet
                .validate(
                    resources,
                    self.overlay_revision,
                    scaler,
                    scaler.pixels_per_mm(),
                )
                .is_ok()
            {
                self.overlay_cache_hits = self.overlay_cache_hits.saturating_add(1);
                return Ok(Some(packet.clone()));
            }
        }
        let paths = self
            .controller
            .routes()
            .iter()
            .map(|route| ferrite_wgpu::NativeRoutePath {
                route_id: route.id,
                waypoints: route
                    .waypoints
                    .iter()
                    .map(|point| ferrite_wgpu::NativeRouteWaypoint {
                        waypoint_id: point.id,
                        longitude: point.lon,
                        latitude: point.lat,
                    })
                    .collect(),
            })
            .collect::<Vec<_>>();
        let mut declarations = Vec::new();
        for route in self.controller.routes() {
            if let Some(source) = self.controller.source_for_route(route.id) {
                for imported in &source.routes {
                    // Imported CRS is explicitly checked EPSG:4326 by the core parser.
                    // Native edits cannot borrow a declaration for changed original coordinates.
                    if !imported.legs.is_empty()
                        && (route.waypoints.len() != imported.route.waypoints.len()
                            || route.waypoints.iter().zip(&imported.route.waypoints).any(
                                |(a, b)| {
                                    a.id != b.id
                                        || a.lon.to_bits() != b.lon.to_bits()
                                        || a.lat.to_bits() != b.lat.to_bits()
                                },
                            ))
                    {
                        return Err(anyhow::anyhow!(
                            "Edited route cannot reuse original declared leg endpoints"
                        ));
                    }
                    for leg in &imported.legs {
                        declarations.push(ferrite_wgpu::NativeRouteLegDeclaration {
                            route_id: route.id,
                            from: leg.from_waypoint_id,
                            to: leg.to_waypoint_id,
                            source_profile: match source.profile {
                                ferrite_s421::s421::Profile::Published1 => 1,
                                ferrite_s421::s421::Profile::Candidate2 => 2,
                            },
                            source_gml_id: leg.gml_id.clone(),
                            original_geometry_text: leg.original_geometry_text.clone(),
                            geometry: match leg.geometry {
                                ferrite_s421::s421::LegGeometry::Loxodrome => {
                                    ferrite_wgpu::NativeRouteLegGeometry::Loxodrome
                                }
                                ferrite_s421::s421::LegGeometry::Orthodrome => {
                                    ferrite_wgpu::NativeRouteLegGeometry::Orthodrome
                                }
                            },
                        });
                    }
                }
            }
        }
        // Profile 0 is an explicit locally authored draft, never a producer/FC profile.
        for route in self
            .controller
            .routes()
            .iter()
            .filter(|route| self.controller.source_for_route(route.id).is_none())
        {
            for pair in route.waypoints.windows(2) {
                if let Some(geometry) = pair[1].incoming_geometry {
                    declarations.push(ferrite_wgpu::NativeRouteLegDeclaration {
                        route_id: route.id,
                        from: pair[0].id,
                        to: pair[1].id,
                        geometry: match geometry {
                            ferrite_s421::s421::LegGeometry::Loxodrome => {
                                ferrite_wgpu::NativeRouteLegGeometry::Loxodrome
                            }
                            ferrite_s421::s421::LegGeometry::Orthodrome => {
                                ferrite_wgpu::NativeRouteLegGeometry::Orthodrome
                            }
                        },
                        source_profile: 0,
                        source_gml_id: String::new(),
                        original_geometry_text: String::new(),
                    });
                }
            }
        }
        let reused_world = self
            .world_samples
            .as_ref()
            .is_some_and(|samples| samples.matches(&paths, &declarations, self.overlay_revision));
        let world = match &self.world_samples {
            Some(samples) if samples.matches(&paths, &declarations, self.overlay_revision) => {
                samples.clone()
            }
            _ => ferrite_wgpu::NativeRouteWorldSamples::prepare(
                &paths,
                &declarations,
                self.overlay_revision,
            )
            .map_err(anyhow::Error::msg)?,
        };
        let prepared = ferrite_wgpu::PreparedNativeRouteOverlay::prepare_with_world_samples(
            resources.clone(),
            self.overlay_revision,
            &paths,
            &declarations,
            &world,
            scaler,
            scaler.pixels_per_mm(),
        )
        .map_err(anyhow::Error::msg)?;
        prepared
            .validate(
                resources,
                self.overlay_revision,
                scaler,
                scaler.pixels_per_mm(),
            )
            .map_err(anyhow::Error::msg)?;
        let packet = std::sync::Arc::new(prepared);
        // Failed candidates leave the previous complete cache intact. The caller
        // receives an error and cannot use it under another camera/palette.
        if !reused_world {
            self.world_sample_preparations = self.world_sample_preparations.saturating_add(1);
        }
        self.world_samples = Some(world);
        self.prepared_overlay = Some(packet.clone());
        self.overlay_preparations = self.overlay_preparations.saturating_add(1);
        Ok(Some(packet))
    }
    pub fn resources_support_palette(&self, palette: &str) -> bool {
        self.editing_resources
            .as_ref()
            .is_some_and(|p| p.palette() == palette)
            && !self.controller.routes().is_empty()
    }
    pub fn set_gpu_capability(&mut self, available: bool, error: Option<String>) {
        if self.gpu_capable != available || self.gpu_error != error {
            self.gpu_capable = available;
            self.gpu_error = error;
            self.refresh_ui(); // Display state does not change route geometry epoch.
        }
    }
    pub fn toggle_panel(&mut self) {
        self.controller.set_active(!self.controller.panel_visible());
        self.refresh();
    }
    pub fn clear(&mut self) {
        self.controller.clear();
        self.controller.set_editing(false);
        self.controller.set_active(false);
        self.sources.clear();
        self.editing_resources = None;
        self.editing_resource_error = None;
        self.prepared_overlay = None;
        self.overlay_prepare_error = None;
        self.refresh();
    }
    pub fn import_path(&mut self, path: &Path, require_signatures: bool) -> Result<u32> {
        // Existing failclosed policy remains before any path I/O.
        ensure!(
            !require_signatures,
            "Authenticated S-421 exchange import is not yet available; unsigned route rejected"
        );
        let captured = crate::s421_dataset_input::capture_if_route(path)?
            .context("Selected XML is not S421 Dataset")?;
        self.import_captured(&captured, require_signatures)
    }
    pub fn import_captured(
        &mut self,
        input: &crate::s421_dataset_input::CapturedRouteInput,
        require_signatures: bool,
    ) -> Result<u32> {
        // Namespace discovery is never authentication. Current operational mode rejects ALL bare inputs.
        let xml = input.allowed_xml(require_signatures)?;
        // Exact original bytes, not digest-only matching. Reopen is idempotent;
        // changed bytes at the same retained origin require explicit unload, not a silent replacement.
        for (id, path) in &self.sources {
            if path == input.path() {
                if self
                    .controller
                    .source_for_route(*id)
                    .is_some_and(|source| source.original_xml.as_bytes() == xml.as_bytes())
                {
                    return Ok(*id);
                }
                anyhow::bail!(
                    "S421 source changed at retained origin; unload before replacing {}",
                    path.display()
                );
            }
        }
        let id = self
            .controller
            .import_xml_atomic(xml)
            .map_err(crate::s421_dataset_input::RouteInputError::Malformed)?;
        // Capture own public editing resources lazily. A missing/unsupported PC
        // does not prevent route inspection and must never enable map editing.
        // Day is explicit preparation metadata; nothing is rendered under it.
        if self.editing_resources.is_none() && self.editing_resource_error.is_none() {
            match ferrite_wgpu::NativeRouteOverlayResources::load(&self.editing_pc_root, "Day") {
                Ok(resources) => self.editing_resources = Some(resources),
                Err(error) => self.editing_resource_error = Some(error),
            }
        }
        self.sources.insert(id, input.path().to_path_buf());
        self.controller.set_editing(false);
        self.controller.set_active(true);
        self.refresh();
        Ok(id)
    }
    pub fn catalogue_layer(&self) -> ferrite_wgpu::DatasetCatalogueLayer {
        fn status(
            raw: &ferrite_s421::catalogue::CatalogueStatus,
            path: PathBuf,
        ) -> ferrite_wgpu::CatalogueStatus {
            let mut out = ferrite_wgpu::CatalogueStatus {
                product_id: "S-421".into(),
                path: path.display().to_string(),
                ..Default::default()
            };
            match raw {
                ferrite_s421::catalogue::CatalogueStatus::Loaded {
                    version,
                    feature_count,
                } => {
                    out.loaded = true;
                    out.version = version.clone();
                    out.item_count = *feature_count;
                    out.validation_message=Some("Legacy public adjunct metadata; path is catalogue directory, not authenticated producer authority or full portrayal proof".into());
                }
                ferrite_s421::catalogue::CatalogueStatus::Error(e) => {
                    out.validation_message = Some(e.clone())
                }
                _ => {}
            }
            out
        }
        let base = self
            .editing_pc_root
            .parent()
            .and_then(Path::parent)
            .unwrap_or(Path::new(""));
        let mut pc = status(self.controller.pc_status(), self.editing_pc_root.clone());
        if let Some(resources) = &self.editing_resources {
            pc.version = resources.actual_version().to_owned();
            pc.validation_message = Some(format!(
                "Own captured public editing PC · palette {} · XSLT not executed",
                resources.palette()
            ));
        }
        ferrite_wgpu::DatasetCatalogueLayer {
            product: "S-421".into(),
            fc: status(self.controller.fc_status(), base.join("FC/S-421")),
            pc,
        }
    }
    pub fn dataset_rows(&self) -> Vec<ferrite_wgpu::DatasetLayerEntry> {
        self.controller
            .routes()
            .iter()
            .map(|route| {
                let source = self
                    .sources
                    .get(&route.id)
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                let profile = self
                    .controller
                    .source_for_route(route.id)
                    .map(|s| match s.profile {
                        ferrite_s421::s421::Profile::Published1 => "published1.0 subset",
                        ferrite_s421::s421::Profile::Candidate2 => "2.0CDV subset",
                    })
                    .unwrap_or("locally authored");
                ferrite_wgpu::DatasetLayerEntry {
                    id: ferrite_wgpu::DatasetLayerId::S421 { route_id: route.id },
                    name: route
                        .name
                        .clone()
                        .unwrap_or_else(|| format!("Route {}", route.id)),
                    source,
                    detail: format!(
                        "{profile} · {}waypoints · host editing overlay; rules not executed",
                        route.waypoints.len()
                    ),
                }
            })
            .collect()
    }
    pub fn unload_route(&mut self, id: u32) -> Result<()> {
        ensure!(
            self.controller.routes().iter().any(|r| r.id == id),
            "S421 route no longer retained"
        );
        let event = serde_json::to_string(&UiEvent::DeleteRoute { id })?;
        self.controller.handle_ui_event(RStr::from(event.as_str()));
        self.sources.remove(&id);
        self.prepared_overlay = None;
        self.gpu_capable = false;
        self.controller.set_editing(false);
        self.refresh();
        Ok(())
    }
    pub fn export_path(&self, path: &Path) -> Result<()> {
        let route = self.controller.active_route().context("No active route")?;
        let local_id = format!("FERRITE.ROUTE.{}", route.id);
        let xml = if self.controller.source_for_route(route.id).is_some() {
            self.controller.export_active_original()
        } else {
            self.controller.export_active_published(PublishedExport {
                route_id: &local_id,
                edition: 1,
                status: 1,
            })
        }
        .map_err(anyhow::Error::msg)?;
        // Imported datasets retain their exact source XML and edition. Locally
        // authored routes use the explicitly supported published geometry subset.
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary.write_all(xml.as_bytes())?;
        temporary.flush()?;
        temporary.as_file().sync_all()?;
        temporary.persist(path).map_err(|e| e.error)?;
        Ok(())
    }
    pub fn handle_event(&mut self, json: &str, require_signatures: bool) -> Result<()> {
        ensure!(
            json.len() <= MAX_EVENT_BYTES,
            "S-421 command exceeds receiver byte limit"
        );
        let event: UiEvent = serde_json::from_str(json).context("Invalid S-421 command")?;
        match &event {
            UiEvent::Import => {
                ensure!(
                    !require_signatures,
                    "Authenticated S-421 exchange import is not yet available; unsigned route rejected"
                );
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
            UiEvent::New | UiEvent::Finish => {
                anyhow::bail!("Route chart editing/display is not yet available");
            }
            UiEvent::SetTurnRadius { id, radius_nm } => {
                self.controller
                    .set_native_turn_radius(*id, radius_nm)
                    .map_err(anyhow::Error::msg)?;
                self.refresh_ui();
                return Ok(());
            }
            UiEvent::DeleteWaypoint { .. } | UiEvent::RenameWaypoint { .. }
                if self
                    .controller
                    .active_route()
                    .is_some_and(|route| self.controller.source_for_route(route.id).is_some()) =>
            {
                anyhow::bail!("Imported source is read-only; create a separate authored route");
            }
            UiEvent::RenameRoute { id, .. } if self.controller.source_for_route(*id).is_some() => {
                anyhow::bail!("Imported source is read-only; create a separate authored route");
            }
            UiEvent::ToggleRendering => {
                ensure!(
                    self.gpu_capable,
                    "S-421 route display unavailable for current PC/palette"
                );
            }
            UiEvent::RenameRoute { name, .. } | UiEvent::RenameWaypoint { name, .. } => {
                ensure!(
                    !name.trim().is_empty()
                        && name.len() <= 4096
                        && !name.chars().any(char::is_control),
                    "Invalid route/waypoint name"
                );
            }
            _ => {}
        }
        let command = serde_json::to_string(&event)?;
        self.controller
            .handle_ui_event(RStr::from(command.as_str()));
        if let UiEvent::DeleteRoute { id } = event {
            self.sources.remove(&id);
        }
        if matches!(event, UiEvent::Clear) {
            self.sources.clear();
        }
        if self.controller.routes().is_empty() {
            self.editing_resources = None;
            self.editing_resource_error = None;
            self.prepared_overlay = None;
            self.overlay_prepare_error = None;
        }
        self.refresh();
        Ok(())
    }
    fn refresh(&mut self) {
        self.world_samples = None;
        self.overlay_revision = self
            .overlay_revision
            .checked_add(1)
            .expect("Native route geometry revision exhausted");
        self.refresh_ui();
    }
    fn refresh_ui(&mut self) {
        let bytes = self.controller.get_ui_data();
        let mut value: serde_json::Value =
            serde_json::from_slice(bytes.as_slice()).expect("Route UI is typed serializable data");
        value["title"] = "S-421 Routes".into();
        value["rendering_enabled"] =
            (self.gpu_capable && self.controller.rendering_enabled()).into();
        value["display_available"] = self.gpu_capable.into();
        value["leg_geometry_editable"] = self
            .controller
            .active_route()
            .is_some_and(|route| self.controller.source_for_route(route.id).is_none())
            .into();
        let imported = self
            .controller
            .active_route()
            .is_some_and(|route| self.controller.source_for_route(route.id).is_some());
        let editable = self.controller.active_route().is_some() && !imported;
        value["content_editable"] = editable.into();
        value["turn_radius_editable"] = editable.into();
        if let Some(routes) = value["routes"].as_array_mut() {
            for row in routes {
                let editable = row["id"]
                    .as_u64()
                    .and_then(|id| u32::try_from(id).ok())
                    .is_some_and(|id| self.controller.source_for_route(id).is_none());
                row["editable"] = editable.into();
            }
        }
        value["notice"] = if let Some(error) = &self.gpu_error {
            format!("Route display unavailable: {error}. Imported routes are unverified.").into()
        } else {
            "Host route illustration; imported routes are unverified. Local routes are authored drafts."
                .into()
        };
        if let Some(actions) = value["actions"].as_array_mut() {
            for action in actions {
                if action["id"] == "new" {
                    // New attempts own-PC capture; failure is reported, never replaced by ENC resources.
                    action["enabled"] = (!self.controller.editing()).into();
                }
                if action["id"] == "finish" {
                    action["enabled"] = self
                        .controller
                        .active_route()
                        .is_some_and(|route| self.controller.editing() && route.len() >= 2)
                        .into();
                }
                if action["id"] == "export" {
                    action["label"] = if imported {
                        "Save original"
                    } else {
                        "Export route"
                    }
                    .into();
                    if imported {
                        action["enabled"] = true.into();
                    }
                }
            }
        }
        self.ui_json = serde_json::to_string(&value).expect("Route UI serialization");
        self.revision = self
            .revision
            .checked_add(1)
            .expect("Native route UI revision exhausted");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_s421::s421::MAX_XML_BYTES;
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
    fn original_save_and_explicit_radius_events_preserve_source_and_failed_state() {
        let dir = tempfile::tempdir().unwrap();
        let input = fixture(dir.path());
        let original = std::fs::read(&input).unwrap();
        let mut host = NativeS421::new(dir.path().join("Catalogues"));
        host.import_path(&input, false).unwrap();
        let out = dir.path().join("saved.gml");
        host.export_path(&out).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), original);
        let ui: serde_json::Value = serde_json::from_str(&host.ui_json).unwrap();
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
        let before = host.ui_json.clone();
        assert!(host
            .handle_event(
                r#"{"type":"SetTurnRadius","id":1,"radius_nm":"0.25"}"#,
                false
            )
            .is_err());
        assert_eq!(host.ui_json, before);
        host.controller.begin_native_local_route().unwrap();
        let id = host
            .controller
            .append_native_local_waypoint(25., 59.)
            .unwrap();
        let event =
            serde_json::json!({"type":"SetTurnRadius","id":id,"radius_nm":"0.25"}).to_string();
        host.handle_event(&event, false).unwrap();
        let local_before = serde_json::to_vec(host.controller.active_route().unwrap()).unwrap();
        let ui: serde_json::Value = serde_json::from_str(&host.ui_json).unwrap();
        assert_eq!(ui["content_editable"], true);
        assert_eq!(ui["waypoints"][0]["turn_radius_nm"], 0.25);
        let event =
            serde_json::json!({"type":"SetTurnRadius","id":id,"radius_nm":"0.251"}).to_string();
        assert!(host.handle_event(&event, false).is_err());
        assert_eq!(
            serde_json::to_vec(host.controller.active_route().unwrap()).unwrap(),
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
        let source_cache = host.world_samples.as_ref().unwrap().clone();
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
            host.world_samples.as_ref().unwrap()
        ));
        assert!(host.prepare_overlay(&scaler, "Night").is_err());
        assert!(Arc::ptr_eq(host.prepared_overlay.as_ref().unwrap(), &moved));
        assert!(Arc::ptr_eq(
            &moved,
            &host.prepare_overlay(&scaler, "Day").unwrap().unwrap()
        ));
        let old_revision = host.overlay_revision;
        assert!(host
            .handle_event(r#"{"type":"RenameRoute","id":1,"name":"Renamed"}"#, false)
            .is_err());
        assert_eq!(host.overlay_revision, old_revision);
        let revised = host.prepare_overlay(&scaler, "Day").unwrap().unwrap();
        assert!(Arc::ptr_eq(
            &source_cache,
            host.world_samples.as_ref().unwrap()
        ));
        assert!(Arc::ptr_eq(&moved, &revised));
        assert_eq!(revised.route_revision(), host.overlay_revision);
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
        let geometry_revision = host.overlay_revision;
        let ui_revision = host.revision();
        host.set_gpu_capability(true, None);
        assert!(host.revision() > ui_revision);
        assert_eq!(host.overlay_revision, geometry_revision);
        assert!(Arc::ptr_eq(
            &before_status,
            &host.prepare_overlay(&scaler, "Day").unwrap().unwrap()
        ));
        host.handle_event(r#"{"type":"ToggleRendering"}"#, false)
            .unwrap();
        assert!(!host.controller.rendering_enabled());
        assert!(host.prepare_overlay(&scaler, "Day").unwrap().is_none());
        host.handle_event(r#"{"type":"ToggleRendering"}"#, false)
            .unwrap();
        assert!(host.controller.rendering_enabled());
        assert!(host.prepare_overlay(&scaler, "Day").unwrap().is_some());
        host.clear();
        assert!(host.prepared_overlay.is_none());
        assert!(host.world_samples.is_none());
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
        assert_eq!(host.controller.routes().len(), 1);
        assert_eq!(
            host.controller.source_for_route(id).unwrap().routes[0]
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
        assert!(host.controller.routes().is_empty());
        assert!(host.sources.is_empty());
        assert!(host.editing_resources.is_none());
        assert!(host.editing_resource_error.is_none());
        host.clear();
        assert!(!host.panel_visible());
        assert!(!host.controller.editing());
    }
    #[test]
    fn captured_open_keeps_original_bytes_after_file_mutation_and_policy_denies_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture(dir.path());
        let input = crate::s421_dataset_input::capture_if_route(&path)
            .unwrap()
            .unwrap();
        std::fs::write(&path, "<mutated/>").unwrap();
        let mut host = NativeS421::new(dir.path().join("Catalogues"));
        let before = host.revision();
        assert!(host.import_captured(&input, true).is_err());
        assert_eq!(host.revision(), before);
        let id = host.import_captured(&input, false).unwrap();
        assert_eq!(
            host.controller
                .source_for_route(id)
                .unwrap()
                .original_xml
                .as_bytes(),
            input.xml().as_bytes()
        );
        assert_eq!(host.import_captured(&input, false).unwrap(), id);
        assert_eq!(host.controller.routes().len(), 1);
        assert_eq!(
            host.dataset_rows()[0].id,
            ferrite_wgpu::DatasetLayerId::S421 { route_id: id }
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
            .controller
            .source_for_route(second)
            .unwrap()
            .original_xml
            .clone();
        host.unload_route(first).unwrap();
        assert!(host.unload_route(first).is_err());
        assert_eq!(host.controller.routes().len(), 1);
        assert_eq!(host.controller.routes()[0].id, second);
        assert_eq!(
            host.controller
                .source_for_route(second)
                .unwrap()
                .original_xml,
            other
        );
        assert_eq!(host.sources.get(&second), Some(&copy));
        assert!(!host.gpu_capable);
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
            .controller
            .source_for_route(imported)
            .unwrap()
            .original_xml
            .clone();
        let mut next = host.fork_local_edit("Day").unwrap();
        let local = next.controller.begin_native_local_route().unwrap();
        assert_ne!(imported, local);
        next.refresh();
        let scaler = Scaler::new(GeoBounds::new(-3., 49., 0., 52.), Viewport::new(800., 600.));
        let empty = next.prepare_overlay(&scaler, "Day").unwrap().unwrap();
        assert!(empty
            .paths()
            .iter()
            .find(|p| p.route_id == local)
            .unwrap()
            .waypoints
            .is_empty());
        assert!(next.controller.finish_native_local_route().is_err());
        assert!(next.controller.editing());
        next.controller
            .append_native_local_waypoint(-2.5, 50.)
            .unwrap();
        next.refresh();
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
        assert!(next.controller.finish_native_local_route().is_err());
        next.controller
            .append_native_local_waypoint(-2.4, 50.1)
            .unwrap();
        next.refresh();
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
        next.controller.finish_native_local_route().unwrap();
        assert!(!next.controller.editing());
        assert!(next
            .controller
            .active_route()
            .unwrap()
            .waypoints
            .iter()
            .all(|p| p.turn_radius.is_none()));
        assert!(next
            .controller
            .export_active_published(PublishedExport {
                route_id: "LOCAL.ROUTE",
                edition: 1,
                status: 1
            })
            .is_err());
        assert_eq!(
            source.as_bytes(),
            next.controller
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
        next.controller.begin_native_local_route().unwrap();
        let snapshot = serde_json::to_vec(next.controller.routes()).unwrap();
        for (lon, lat) in [(f64::NAN, 50.), (181., 50.), (0., 91.)] {
            assert!(next
                .controller
                .append_native_local_waypoint(lon, lat)
                .is_err());
            assert_eq!(
                serde_json::to_vec(next.controller.routes()).unwrap(),
                snapshot
            );
        }
        assert!(next.controller.begin_native_local_route().is_err());
        assert!(next.controller.finish_native_local_route().is_err());
        assert_eq!(
            next.controller
                .append_native_local_waypoint(179., 50.)
                .unwrap(),
            1
        );
        let snapshot = serde_json::to_vec(next.controller.routes()).unwrap();
        assert!(next
            .controller
            .append_native_local_waypoint(-179., 50.)
            .is_err());
        assert_eq!(
            serde_json::to_vec(next.controller.routes()).unwrap(),
            snapshot
        );
        assert_eq!(
            next.controller
                .append_native_local_waypoint(178., 50.)
                .unwrap(),
            2
        );
        next.controller.finish_native_local_route().unwrap();
        assert!(next
            .controller
            .append_native_local_waypoint(177., 50.)
            .is_err());
        assert!(host.fork_local_edit("UnknownPalette").is_err());
        assert_eq!(host.audit_data(), before);
    }
}
