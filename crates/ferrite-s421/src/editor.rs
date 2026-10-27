//! The viewer's S-421 route editor host (native only; moved from the
//! application's `src/native_s421.rs`). Statically linked route
//! receiver/editor state: no external DLL or ENC owner.
//!
//! It owns the route controller, the retained source of every imported
//! route, the UI and geometry revisions, the route panel data and the world
//! samples of the route overlay renderer extension. The host application
//! keeps what needs the renderer or the windowing system: the editing
//! portrayal resources, the screen-projected overlay and its GPU
//! publication, and the file dialogs.
use crate::catalogue::CatalogueStatus as RawCatalogueStatus;
use crate::layer::input::{self, CapturedRouteInput, RouteInputError};
use crate::layer::overlay;
use crate::s421::{Profile, PublishedExport};
use crate::ui::UiEvent;
use crate::RouteController;
use abi_stable::std_types::RStr;
use anyhow::{ensure, Context, Result};
use ferrite_plugin_api::Plugin;
use ferrite_product_api::route_overlay::{self, NativeRouteWorldSamples};
use ferrite_product_api::{CatalogueStatus, RendererExtension};
use std::{
    collections::HashMap,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};

/// Plugin-host identity of the native route panel and toolbar button.
pub const PROVIDER_ID: &str = "native:s421-route";
const MAX_EVENT_BYTES: usize = 64 * 1024;

/// One route of the editor as a dataset-tree row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteRow {
    pub route_id: u32,
    pub name: String,
    pub source: String,
    pub detail: String,
}

/// World samples of the current route geometry, not yet committed to the
/// editor's cache ([`RouteEditor::commit_route_overlay`]).
pub struct RouteOverlay {
    samples: Arc<NativeRouteWorldSamples>,
    reused: bool,
}

impl RouteOverlay {
    /// The samples as the route overlay renderer extension.
    pub fn extension(&self) -> RendererExtension {
        route_overlay::extension(self.samples.clone())
    }
    pub fn samples(&self) -> &Arc<NativeRouteWorldSamples> {
        &self.samples
    }
}

pub struct RouteEditor {
    controller: RouteController,
    sources: HashMap<u32, PathBuf>,
    revision: u64,
    overlay_revision: u64,
    /// The host can draw the routes (editing resources for the palette and a
    /// published overlay).
    display_available: bool,
    display_error: Option<String>,
    ui_json: String,
    editing_pc_root: PathBuf,
    world_samples: Option<Arc<NativeRouteWorldSamples>>,
    world_sample_preparations: u64,
}

impl RouteEditor {
    pub fn new(catalogues: PathBuf) -> Self {
        let editing_pc_root = catalogues.join("PC/S-421");
        let mut editor = Self {
            controller: RouteController::with_catalogues(catalogues),
            sources: HashMap::new(),
            revision: 0,
            overlay_revision: 0,
            display_available: false,
            display_error: None,
            ui_json: String::new(),
            editing_pc_root,
            world_samples: None,
            world_sample_preparations: 0,
        };
        editor.refresh();
        editor
    }
    /// A disconnected workspace: failures leave this editor (controller,
    /// source owners, epochs) intact.
    pub fn fork(&self) -> Self {
        Self {
            controller: self.controller.fork_native_transaction(),
            sources: self.sources.clone(),
            revision: self.revision,
            overlay_revision: self.overlay_revision,
            display_available: self.display_available,
            display_error: self.display_error.clone(),
            ui_json: self.ui_json.clone(),
            editing_pc_root: self.editing_pc_root.clone(),
            world_samples: self.world_samples.clone(),
            world_sample_preparations: self.world_sample_preparations,
        }
    }
    pub fn controller(&self) -> &RouteController {
        &self.controller
    }
    /// Direct controller access for edit transactions on a [`Self::fork`];
    /// call [`Self::refresh`] after changing route geometry.
    pub fn controller_mut(&mut self) -> &mut RouteController {
        &mut self.controller
    }
    /// The S-421 editing portrayal catalogue folder (`<catalogues>/PC/S-421`).
    pub fn editing_pc_root(&self) -> &Path {
        &self.editing_pc_root
    }
    /// The retained origin of each imported route.
    pub fn sources(&self) -> &HashMap<u32, PathBuf> {
        &self.sources
    }
    pub fn editing(&self) -> bool {
        self.controller.editing()
    }
    pub fn panel_visible(&self) -> bool {
        self.controller.panel_visible()
    }
    /// UI revision (changes whenever [`Self::ui_json`] does).
    pub fn revision(&self) -> u64 {
        self.revision
    }
    /// Route geometry revision.
    pub fn overlay_revision(&self) -> u64 {
        self.overlay_revision
    }
    pub fn ui_json(&self) -> &str {
        &self.ui_json
    }
    pub fn display_available(&self) -> bool {
        self.display_available
    }
    pub fn world_samples(&self) -> Option<&Arc<NativeRouteWorldSamples>> {
        self.world_samples.as_ref()
    }
    /// Escape stops collecting clicks; it retains the authored draft. This is
    /// distinct from Finish/export validation and does not mint a product.
    pub fn stop_local_edit(&mut self) {
        self.controller.set_editing(false);
        self.refresh_ui();
    }
    /// The product part of the viewer's native route audit.
    pub fn audit_data(&self) -> serde_json::Map<String, serde_json::Value> {
        use sha2::{Digest, Sha256};
        let value = serde_json::json!({
            "provider_id": PROVIDER_ID, "panel_visible": self.panel_visible(),
            "revision": self.revision, "editing": self.controller.editing(),
            "routes": self.controller.routes().iter().map(|route| serde_json::json!({
                "id":route.id, "waypoints":route.waypoints.len(),
                "source":self.sources.get(&route.id),
                "original_xml_sha256":self.controller.source_for_route(route.id)
                    .map(|s| format!("{:x}", Sha256::digest(s.original_xml.as_bytes()))),
            })).collect::<Vec<_>>(),
            "gpu_capable":self.display_available,"gpu_error":self.display_error,"overlay_revision":self.overlay_revision,
            "world_cache": self.world_samples.as_ref().map(|w|serde_json::json!({"payload_bytes":w.payload_bytes(),"solver_legs_in_cache":w.solver_invocations(),"committed_preparations":self.world_sample_preparations})),
            "ui":serde_json::from_str::<serde_json::Value>(&self.ui_json).expect("typed native UI")
        });
        match value {
            serde_json::Value::Object(map) => map,
            _ => unreachable!("audit object"),
        }
    }

    /// The route overlay of the current geometry: the cached world samples
    /// when they still match, otherwise newly prepared ones. Nothing is
    /// cached until [`Self::commit_route_overlay`].
    pub fn route_overlay(&self) -> Result<RouteOverlay> {
        let (paths, declarations) = overlay::routes(self.controller.routes(), |id| {
            self.controller.source_for_route(id)
        })
        .map_err(anyhow::Error::msg)?;
        let reused = self
            .world_samples
            .as_ref()
            .is_some_and(|samples| samples.matches(&paths, &declarations, self.overlay_revision));
        let samples = match &self.world_samples {
            Some(samples) if samples.matches(&paths, &declarations, self.overlay_revision) => {
                samples.clone()
            }
            _ => NativeRouteWorldSamples::prepare(&paths, &declarations, self.overlay_revision)
                .map_err(anyhow::Error::msg)?,
        };
        Ok(RouteOverlay { samples, reused })
    }
    /// Keep the samples of a published overlay. Failed candidates leave the
    /// previous complete cache intact.
    pub fn commit_route_overlay(&mut self, overlay: RouteOverlay) {
        if !overlay.reused {
            self.world_sample_preparations = self.world_sample_preparations.saturating_add(1);
        }
        self.world_samples = Some(overlay.samples);
    }
    /// Forget the world samples (no routes to draw).
    pub fn clear_route_overlay(&mut self) {
        self.world_samples = None;
    }

    /// Record whether the host can draw the routes; the UI is refreshed when
    /// this changes (display state does not change the route geometry epoch).
    pub fn set_display(&mut self, available: bool, error: Option<String>) {
        if self.display_available != available || self.display_error != error {
            self.display_available = available;
            self.display_error = error;
            self.refresh_ui();
        }
    }
    /// A published edit is drawable; the caller refreshes the UI.
    pub fn mark_displayed(&mut self) {
        self.display_available = true;
        self.display_error = None;
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
        self.refresh();
    }
    /// Existing fail-closed policy, checked before any path I/O.
    pub fn check_import_allowed(require_signatures: bool) -> Result<()> {
        ensure!(
            !require_signatures,
            "Authenticated S-421 exchange import is not yet available; unsigned route rejected"
        );
        Ok(())
    }
    /// Import a local route file. `on_new` runs once a new route is parsed
    /// and retained, before the editor refreshes (the host captures its
    /// editing resources there).
    pub fn import_path(
        &mut self,
        path: &Path,
        require_signatures: bool,
        on_new: impl FnOnce(),
    ) -> Result<u32> {
        Self::check_import_allowed(require_signatures)?;
        let captured =
            input::capture_if_route(path)?.context("Selected XML is not S421 Dataset")?;
        self.import_captured(&captured, require_signatures, on_new)
    }
    /// Import a captured route (see [`Self::import_path`] for `on_new`).
    pub fn import_captured(
        &mut self,
        input: &CapturedRouteInput,
        require_signatures: bool,
        on_new: impl FnOnce(),
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
            .map_err(RouteInputError::Malformed)?;
        // Capture own public editing resources lazily. A missing/unsupported PC
        // does not prevent route inspection and must never enable map editing.
        on_new();
        self.sources.insert(id, input.path().to_path_buf());
        self.controller.set_editing(false);
        self.controller.set_active(true);
        self.refresh();
        Ok(id)
    }
    /// FC and PC status of the route catalogues (`<catalogues>/FC/S-421`,
    /// the editing PC folder).
    pub fn catalogue_status(&self) -> (CatalogueStatus, CatalogueStatus) {
        fn status(raw: &RawCatalogueStatus, path: PathBuf) -> CatalogueStatus {
            let mut out = CatalogueStatus {
                product_id: "S-421".into(),
                path: path.display().to_string(),
                ..Default::default()
            };
            match raw {
                RawCatalogueStatus::Loaded {
                    version,
                    feature_count,
                } => {
                    out.loaded = true;
                    out.version = version.clone();
                    out.item_count = *feature_count;
                    out.validation_message=Some("Legacy public adjunct metadata; path is catalogue directory, not authenticated producer authority or full portrayal proof".into());
                }
                RawCatalogueStatus::Error(e) => out.validation_message = Some(e.clone()),
                _ => {}
            }
            out
        }
        let base = self
            .editing_pc_root
            .parent()
            .and_then(Path::parent)
            .unwrap_or(Path::new(""));
        (
            status(self.controller.fc_status(), base.join("FC/S-421")),
            status(self.controller.pc_status(), self.editing_pc_root.clone()),
        )
    }
    pub fn dataset_rows(&self) -> Vec<RouteRow> {
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
                        Profile::Published1 => "published1.0 subset",
                        Profile::Candidate2 => "2.0CDV subset",
                    })
                    .unwrap_or("locally authored");
                RouteRow {
                    route_id: route.id,
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
        self.display_available = false;
        self.controller.set_editing(false);
        self.refresh();
        Ok(())
    }
    /// The active route as S-421 GML: an imported dataset's exact source
    /// XML and edition, a locally authored route in the supported published
    /// geometry subset.
    pub fn export_xml(&self) -> Result<String> {
        let route = self.controller.active_route().context("No active route")?;
        let local_id = format!("FERRITE.ROUTE.{}", route.id);
        if self.controller.source_for_route(route.id).is_some() {
            self.controller.export_active_original()
        } else {
            self.controller.export_active_published(PublishedExport {
                route_id: &local_id,
                edition: 1,
                status: 1,
            })
        }
        .map_err(anyhow::Error::msg)
    }
    /// Write [`Self::export_xml`] to `path` atomically.
    pub fn export_path(&self, path: &Path) -> Result<()> {
        let xml = self.export_xml()?;
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
    /// A route panel command (bounded JSON [`UiEvent`]).
    pub fn parse_event(json: &str) -> Result<UiEvent> {
        ensure!(
            json.len() <= MAX_EVENT_BYTES,
            "S-421 command exceeds receiver byte limit"
        );
        serde_json::from_str(json).context("Invalid S-421 command")
    }
    /// Apply a route panel command other than the file dialogs
    /// (`Import`/`Export`, which the host runs). Returns whether the command
    /// reached the controller (the editor then refreshed; the host drops its
    /// drawing resources when no route is left).
    pub fn apply_event(&mut self, event: UiEvent) -> Result<bool> {
        match &event {
            UiEvent::Import | UiEvent::Export => {
                anyhow::bail!("S-421 file dialogs are run by the host");
            }
            UiEvent::New | UiEvent::Finish => {
                anyhow::bail!("Route chart editing/display is not yet available");
            }
            UiEvent::SetTurnRadius { id, radius_nm } => {
                self.controller
                    .set_native_turn_radius(*id, radius_nm)
                    .map_err(anyhow::Error::msg)?;
                self.refresh_ui();
                return Ok(false);
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
                    self.display_available,
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
        self.refresh();
        Ok(true)
    }
    /// Route geometry changed: new geometry revision (dropping the world
    /// samples) and refreshed panel data.
    pub fn refresh(&mut self) {
        self.world_samples = None;
        self.overlay_revision = self
            .overlay_revision
            .checked_add(1)
            .expect("Native route geometry revision exhausted");
        self.refresh_ui();
    }
    /// Rebuild the route panel data (the controller's [`crate::ui::UiData`]
    /// with the editor's capabilities).
    pub fn refresh_ui(&mut self) {
        let bytes = self.controller.get_ui_data();
        let mut value: serde_json::Value =
            serde_json::from_slice(bytes.as_slice()).expect("Route UI is typed serializable data");
        value["title"] = "S-421 Routes".into();
        value["rendering_enabled"] =
            (self.display_available && self.controller.rendering_enabled()).into();
        value["display_available"] = self.display_available.into();
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
        value["notice"] = if let Some(error) = &self.display_error {
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
                if action["id"] == "copy" {
                    action["enabled"] = (imported && !self.controller.editing()).into();
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
