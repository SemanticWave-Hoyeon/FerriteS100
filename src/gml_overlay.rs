//! S-124 (Navigational Warnings 2.0.0) and S-129 (Under Keel Clearance
//! Management 2.0.0) datasets shown as an always-on overlay on the ENC
//! (S-98 Level 1 style).
//!
//! Datasets are S-100 Part 10b GML, portrayed with their own XSLT portrayal
//! catalogues (`ferrite_s100_gml::portrayal`). The products are modules
//! (`ferrite-s124`, `ferrite-s129`) registered with the engine's overlay
//! layer; this module is the viewer glue: the per-user state location, the
//! product panels' rows and requests, selection and the data-layer tree. The display list is converted
//! to the host's drawing instructions through the same Part 9a conversion the
//! S-101 Lua portrayal uses, on a synthetic cell, and then marked as host
//! overlay instructions: no source cell, exempt from ENC coverage and placed
//! in the always displayed viewing group (21010, as plugin overlays), so the
//! S-101 display mode and layer selection never hide them and S-101 output is
//! untouched when no overlay dataset is loaded.
//!
//! Feature keys carry [`OVERLAY_KEY_FLAG`] and cannot collide with S-101
//! record keys; picking decodes them back to the dataset and object.
use crate::ChartApp;
use anyhow::Result;
use chrono::Utc;
use ferrite_gpu::CatalogueStatus;
use ferrite_render::{DrawingInstruction, GeoBounds, RenderContext};
use ferrite_s100_gml::{Product, Severity};
use ferrite_s124::S124Overlay;
use ferrite_s129::S129Overlay;
use ferrite_ui::overlay_ui::{NavwarnPanel, NavwarnRequests, NavwarnRow, UkcPanel, UkcPlanRow};
use std::path::{Path, PathBuf};

pub(crate) use ferrite_engine::gml_overlay::{
    decode_key, discover, format_time, is_import_file, viewing_time, Added, GmlOverlays, Provenance,
};

/// The overlay registry with the renderer's symbol sources registered for
/// each loaded product portrayal catalogue.
pub(crate) fn load_overlays(
    platform: &ferrite_engine::platform::Platform,
    base: &Path,
) -> GmlOverlays {
    GmlOverlays::load_with(platform, base, |id, bound| {
        ferrite_gpu::register_overlay_symbol_source(
            id,
            ferrite_gpu::OverlaySymbolSource::from_bound(bound),
        )
    })
}

pub(crate) fn is_overlay_instruction(instruction: &DrawingInstruction) -> bool {
    ferrite_render::is_cellless_overlay_instruction(instruction)
        && instruction
            .feature_id()
            .is_some_and(|k| decode_key(k).is_some())
}

/// Name of the OS user running the application.
pub(crate) fn os_user_name() -> String {
    ["USER", "USERNAME", "LOGNAME"]
        .iter()
        .filter_map(std::env::var_os)
        .map(|v| v.to_string_lossy().trim().to_owned())
        .find(|v| !v.is_empty())
        .or_else(|| {
            dirs::home_dir().and_then(|h| h.file_name().map(|n| n.to_string_lossy().into_owned()))
        })
        .unwrap_or_else(|| "unknown".into())
}

/// Per-OS-user location of the S-124 user state, in the same application
/// data folder as the other persisted state: `FerriteS100/navwarn/<user>.json`
/// under the user's own data directory. `FERRITE_NAVWARN_STATE_PATH`
/// overrides it; tests and hidden audits persist nothing.
pub(crate) fn default_user_state_path(user: &str) -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("FERRITE_NAVWARN_STATE_PATH") {
        return Some(PathBuf::from(path));
    }
    if cfg!(test) || ferrite_ui::background_test::enabled() {
        return None;
    }
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(target_os = "macos")]
    let base =
        std::env::var_os("HOME").map(|p| PathBuf::from(p).join("Library/Application Support"));
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/share")));
    let file: String = user
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let file = file.trim_start_matches('.');
    let file = if file.is_empty() { "unknown" } else { file };
    Some(
        base?
            .join("FerriteS100/navwarn")
            .join(format!("{file}.json")),
    )
}

/// Rows of the S-124 warning list and the S-129 plan list, from the
/// registered product modules (empty when a product is not registered).
pub(crate) trait OverlayRows {
    fn navwarn_rows(&mut self) -> (Vec<NavwarnRow>, Vec<String>, usize);
    fn ukc_rows(&self) -> Vec<UkcPlanRow>;
}

impl OverlayRows for GmlOverlays {
    fn navwarn_rows(&mut self) -> (Vec<NavwarnRow>, Vec<String>, usize) {
        match self.product_mut::<S124Overlay>() {
            Some((s124, core)) => s124.navwarn_rows(core),
            None => (Vec::new(), Vec::new(), 0),
        }
    }

    fn ukc_rows(&self) -> Vec<UkcPlanRow> {
        if self.product::<S129Overlay>().is_none() {
            return Vec::new();
        }
        ferrite_s129::panel::ukc_rows(self.core())
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Application glue
// ─────────────────────────────────────────────────────────────────────────

impl ChartApp {
    /// Replace the host overlay instructions (plugins, S-124/S-129) of the
    /// live scene. Without overlay datasets this is exactly the plugin-only
    /// behaviour; with them the instructions are reinstalled only when they
    /// or the scene changed.
    pub(crate) fn sync_host_overlay_instructions(&mut self) {
        self.sync_overlay_viewing_time();
        let overlays = self
            .gml_overlays
            .instructions(&self.engine.current_profile_name);
        let plugin = self.plugin_system.get_render_instructions();
        let Some((revision, list)) = overlays else {
            self.gml_overlays.installed = None;
            self.engine
                .render_context
                .remove_coverage_exempt_instructions();
            for mut instr in plugin {
                instr.set_portrayal_origin(ferrite_render::PortrayalOrigin::CoverageExempt);
                self.engine.render_context.add_instruction(instr);
            }
            return;
        };
        if plugin.is_empty()
            && self.gml_overlays.installed
                == Some((revision, self.engine.render_context.geometry_revision()))
        {
            return;
        }
        self.engine
            .render_context
            .remove_coverage_exempt_instructions();
        for mut instr in plugin {
            instr.set_portrayal_origin(ferrite_render::PortrayalOrigin::CoverageExempt);
            self.engine.render_context.add_instruction(instr);
        }
        self.engine.render_context.append_converted(list.to_vec());
        self.gml_overlays.installed =
            Some((revision, self.engine.render_context.geometry_revision()));
    }

    /// Overlay instructions for a context being prepared with `profile`.
    pub(crate) fn append_overlay_instructions(
        &mut self,
        context: &mut RenderContext,
        profile: &str,
    ) {
        self.sync_overlay_viewing_time();
        if let Some((_, list)) = self.gml_overlays.instructions(profile) {
            context.append_converted(list.to_vec());
        }
    }

    /// S-124 status and fixedDateRange follow the viewing time selected for
    /// the S-101 date-dependent portrayal (system clock when live).
    pub(crate) fn sync_overlay_viewing_time(&mut self) -> bool {
        let time = viewing_time(&self.engine.render_context.settings);
        self.gml_overlays.set_viewing_time(time)
    }

    /// Picking and selection work with overlays even without ENC cells.
    pub(crate) fn picking_enabled(&self) -> bool {
        self.chart_loaded || !self.gml_overlays.is_empty()
    }

    /// Open one overlay dataset (GML or import) from the open-dataset queue.
    pub(crate) fn open_overlay_dataset(&mut self, path: &Path) -> Result<Added> {
        let import = is_import_file(path, true);
        let verify = self.signature_verification_enabled();
        let (dataset, provenance) = if import {
            anyhow::ensure!(
                !verify,
                "Imported navigational warnings cannot be authenticated; switch off dataset signature verification to view them (they are never official S-124 data)"
            );
            let (dataset, format) = self.gml_overlays.read(path, path)?;
            (dataset, Provenance::Imported(format.unwrap_or("import")))
        } else {
            let report = self.authenticate_paths(&[path.to_path_buf()])?;
            let snapshot =
                crate::dataset_signature_policy::dataset_snapshot(&report, path, verify)?;
            let bytes_path = snapshot
                .as_ref()
                .map(|s| s.path().to_path_buf())
                .unwrap_or_else(|| path.to_path_buf());
            let (dataset, _) = self.gml_overlays.read(path, &bytes_path)?;
            let provenance = if snapshot.is_some() {
                Provenance::Authenticated
            } else {
                Provenance::Unverified
            };
            (dataset, provenance)
        };
        let added = self.gml_overlays.add(dataset, provenance)?;
        if matches!(added, Added::New) {
            self.overlays_changed(true);
        }
        Ok(added)
    }

    /// After datasets were added or removed (or options changed): fit the
    /// view when there is no ENC, refresh the tree, lists and the scene.
    pub(crate) fn overlays_changed(&mut self, fit_without_charts: bool) {
        self.gml_overlays.ui_revision = 0;
        // Charts own the view; without them the overlays define it.
        if !self.chart_loaded && self.loading_state.is_none() && self.pending_auto_chart.is_empty()
        {
            match self.gml_overlays.bounds() {
                Some(mut b) if fit_without_charts => {
                    let pad = (b.width().max(b.height()) * 0.1).max(0.01);
                    b = GeoBounds::new(b.min_x - pad, b.min_y - pad, b.max_x + pad, b.max_y + pad);
                    self.engine.bounds = b;
                    self.zoom_level = 1.0;
                    self.zoom_target = 1.0;
                    self.pan_offset = (0.0, 0.0);
                    self.engine.render_context.set_bounds(b);
                }
                None if self.gml_overlays.is_empty() => {
                    self.engine.bounds = GeoBounds::new(-180.0, -90.0, 180.0, 90.0);
                }
                _ => {}
            }
        }
        self.refresh_dataset_tree();
        if self.renderer.is_some() {
            self.update_view();
        }
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Centre and zoom the chart on geographic bounds.
    pub(crate) fn focus_geo_bounds(&mut self, b: GeoBounds) {
        let c = b.center();
        if let Some(pan) = self
            .engine
            .render_context
            .scaler
            .projection()
            .pan_to(self.engine.bounds, c)
        {
            self.pan_offset = (pan[0], pan[1]);
        }
        let w = b.width().max(0.005);
        let h = b.height().max(0.005);
        let zoom = (self.engine.bounds.width() / w).min(self.engine.bounds.height() / h) * 0.6;
        if zoom.is_finite() {
            let zoom = zoom.clamp(crate::navigation::MIN_ZOOM, crate::navigation::MAX_ZOOM);
            self.zoom_level = zoom;
            self.zoom_target = zoom;
        }
        self.zoom_animating = false;
        self.update_view();
    }

    /// Select an overlay feature on the chart (and show its details).
    pub(crate) fn select_overlay_feature(&mut self, key: i64) {
        let Some(pick) = self.gml_overlays.describe(key) else {
            return;
        };
        let world = self
            .gml_overlays
            .locate(key)
            .and_then(|(entry, feature)| {
                let object = entry.dataset.objects.get(feature.object?)?;
                let c = object.geometry.first()?.coords().next().copied()?;
                Some((c[0], c[1]))
            })
            .unwrap_or((0.0, 0.0));
        let selected = ferrite_gpu::SelectedFeature {
            feature_type: pick.feature_type,
            feature_id: key,
            foid: pick.foid,
            cell_index: None,
            primitive_type: pick.primitive,
            source: Some(pick.source),
            attributes: pick.attributes,
            world_pos: world,
            longitude_shift: 0.0,
            definition: pick.definition,
            symbol_name: None,
        };
        self.select_feature(Some(selected));
    }

    /// Keep the overlay windows' rows current and act on their requests.
    pub(crate) fn service_overlay_ui(&mut self) {
        if self.sync_overlay_viewing_time() | self.gml_overlays.evaluate(false) {
            // Lifecycle changed with time (cancellation date, date ranges).
            self.gml_overlays.ui_revision = 0;
            self.update_view();
        }
        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        let panels = &mut renderer.ui.ui_state.panels;
        let options_changed = panels.take_options_changed();
        let correct_pc_errata = panels.shared.correct_pc_errata;
        let navwarn = panels.get_mut::<NavwarnPanel>();
        let (requests, navwarn_options) = match navwarn {
            Some(panel) => (
                panel.take_requests(),
                Some((
                    panel.show_before_range,
                    panel.show_information,
                    panel.sort.into(),
                )),
            ),
            None => (NavwarnRequests::default(), None),
        };
        let ukc = panels.get_mut::<UkcPanel>();
        let ukc_options = ukc
            .as_ref()
            .map(|panel| (panel.plain_boundaries, panel.apply_scale_minimum));
        let select_plan = ukc.and_then(|panel| panel.select_plan.take());
        let visibility = renderer.ui.ui_state.overlay_visibility_requested.take();
        let mut changed = false;
        if options_changed {
            let g = &mut self.gml_overlays;
            if let (Some((s129, _)), Some((plain_boundaries, apply_scale_minimum))) =
                (g.product_mut::<S129Overlay>(), ukc_options)
            {
                s129.plain_boundaries = plain_boundaries;
                s129.apply_scale_minimum = apply_scale_minimum;
            }
            if let (Some((s124, _)), Some((show_before_range, show_information, _))) =
                (g.product_mut::<S124Overlay>(), navwarn_options)
            {
                s124.policy.show_before_fixed_date_range = show_before_range;
                s124.policy.show_information = show_information;
            }
            g.set_correct_pc_errata(correct_pc_errata);
            g.invalidate();
            changed = true;
        }
        if requests.sort_changed {
            if let (Some((s124, _)), Some((_, _, sort))) = (
                self.gml_overlays.product_mut::<S124Overlay>(),
                navwarn_options,
            ) {
                s124.sort = sort;
            }
            self.gml_overlays.ui_revision = 0;
        }
        if let Some((id, class)) = requests.classify {
            if let Some((s124, core)) = self.gml_overlays.product_mut::<S124Overlay>() {
                s124.classify(core, id, class.into());
            }
            changed = true;
        }
        if let Some((id, visible)) = visibility {
            if self.gml_overlays.set_visible(id, visible) {
                changed = true;
            }
        }
        if changed {
            self.gml_overlays.ui_revision = 0;
            self.update_view();
            self.refresh_visible_selection();
        }
        if let Some((id, part)) = requests.select {
            self.select_overlay_message(id, part);
        }
        if let Some(id) = select_plan {
            if let Some(b) = self.gml_overlays.dataset(id).and_then(|d| d.bounds(None)) {
                self.focus_geo_bounds(b);
            }
        }
        self.publish_overlay_rows();
    }

    fn select_overlay_message(&mut self, id: u32, part: Option<String>) {
        let part = part.or_else(|| self.gml_overlays.first_part(id));
        self.gml_overlays.mark_displayed(id);
        if let Some(part) = &part {
            if self.gml_overlays.list_selection.as_ref() != Some(&(id, part.clone())) {
                self.gml_overlays.list_selection = Some((id, part.clone()));
                self.gml_overlays.invalidate();
            }
        }
        if let Some(b) = self.gml_overlays.focus_bounds(id, part.as_deref()) {
            self.focus_geo_bounds(b);
        } else {
            self.update_view();
        }
        if let Some(key) = part.and_then(|p| self.gml_overlays.key_of(id, &p)) {
            self.select_overlay_feature(key);
        }
        self.gml_overlays.ui_revision = 0;
    }

    fn publish_overlay_rows(&mut self) {
        if self.gml_overlays.ui_revision != 0 {
            return;
        }
        self.gml_overlays.ui_revision = 1;
        let (messages, missing, new_count) = self.gml_overlays.navwarn_rows();
        let plans = self.gml_overlays.ukc_rows();
        let g = &self.gml_overlays;
        let s124 = g.product::<S124Overlay>();
        let s129 = g.product::<S129Overlay>();
        let lifecycle_time = match g.viewing_time {
            Some(t) => format!("Evaluated at viewing time {}", format_time(t)),
            None => format!("Evaluated at {} (system clock)", format_time(Utc::now())),
        };
        let hidden: std::collections::BTreeSet<u32> = g
            .datasets
            .iter()
            .filter(|d| !d.visible)
            .map(|d| d.id)
            .collect();
        if let Some(renderer) = &mut self.renderer {
            let panels = &mut renderer.ui.ui_state.panels;
            panels.shared.correct_pc_errata = g.correct_pc_errata();
            if let Some(ui) = panels.get_mut::<NavwarnPanel>() {
                ui.messages = messages;
                ui.missing = missing;
                ui.new_count = new_count;
                if let Some(s124) = s124 {
                    ui.show_before_range = s124.policy.show_before_fixed_date_range;
                    ui.show_information = s124.policy.show_information;
                    ui.user_status = s124.user_status();
                }
                ui.lifecycle_time = lifecycle_time;
            }
            if let Some(ui) = panels.get_mut::<UkcPanel>() {
                ui.plans = plans;
                if let Some(s129) = s129 {
                    ui.plain_boundaries = s129.plain_boundaries;
                    ui.apply_scale_minimum = s129.apply_scale_minimum;
                }
            }
            renderer.ui.ui_state.overlay_hidden = hidden;
        }
    }

    /// Data-layer tree products and catalogue layers of the overlays.
    pub(crate) fn overlay_tree(
        &self,
    ) -> (
        Vec<ferrite_ui::DatasetProductLayer>,
        Vec<ferrite_ui::DatasetCatalogueLayer>,
    ) {
        let g = &self.gml_overlays;
        let mut layers = Vec::new();
        let mut catalogues = Vec::new();
        let products: Vec<Product> = g.products().map(|p| p.product()).collect();
        for product in products {
            let id = product.identifier();
            let files: Vec<_> = g
                .datasets
                .iter()
                .filter(|d| d.product() == product)
                .map(|d| {
                    let errors = d
                        .issues
                        .iter()
                        .filter(|i| i.severity == Severity::Error)
                        .count();
                    ferrite_ui::DatasetLayerEntry {
                        id: ferrite_ui::DatasetLayerId::Overlay {
                            product: id.into(),
                            id: d.id,
                        },
                        name: if d.dataset.imported {
                            format!("{} (imported)", d.name())
                        } else {
                            d.name()
                        },
                        source: d
                            .dataset
                            .source
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_default(),
                        detail: format!(
                            "{} · {} objects · {} validation errors, {} warnings",
                            d.provenance.label(),
                            d.dataset.objects.len(),
                            errors,
                            d.issues.len() - errors
                        ),
                    }
                })
                .collect();
            let c = g.catalogues(product);
            match c {
                Some(c) => catalogues.push(ferrite_ui::DatasetCatalogueLayer {
                    product: id.into(),
                    fc: c.fc_status.clone(),
                    pc: c.pc_status.clone(),
                }),
                None => {
                    let failed = CatalogueStatus {
                        product_id: id.into(),
                        validation_message: g
                            .catalogue_errors
                            .iter()
                            .find(|e| e.starts_with(id))
                            .cloned(),
                        ..Default::default()
                    };
                    catalogues.push(ferrite_ui::DatasetCatalogueLayer {
                        product: id.into(),
                        fc: failed.clone(),
                        pc: failed,
                    });
                }
            }
            if files.is_empty() {
                continue;
            }
            layers.push(ferrite_ui::DatasetProductLayer {
                product: id.into(),
                fc: c
                    .map(|c| format!("FC {} · {}", c.fc_status.version, c.fc_status.path))
                    .unwrap_or_else(|| "FC unavailable".into()),
                pc: c
                    .map(|c| {
                        format!(
                            "PC {} · {} · overlay",
                            c.pc_status.version, c.pc_status.path
                        )
                    })
                    .unwrap_or_else(|| "PC unavailable".into()),
                files,
            });
        }
        (layers, catalogues)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::DateTime;
    use ferrite_engine::gml_overlay::{exchange_set_reports, OVERLAY_VIEWING_GROUP};
    use ferrite_engine::platform::FileRecord;
    use ferrite_s100_gml::Dataset;
    use ferrite_s124::navwarn::{
        self, NavwarnRegistry, NavwarnUserState, PartTiming, PortrayalPolicy, Status,
        UserClassification,
    };
    use ferrite_s124::user_state::{read_user_record, StoredUserState, RECORD_TEMPORARY_PREFIX};
    use ferrite_ui::overlay_ui::NavwarnClassification;
    use std::collections::HashSet;

    fn state_file(path: &Path) -> FileRecord {
        FileRecord {
            path: path.to_path_buf(),
            temporary_prefix: RECORD_TEMPORARY_PREFIX,
        }
    }

    fn read_user_state(path: &Path) -> anyhow::Result<Option<StoredUserState>> {
        read_user_record(&state_file(path))
    }

    /// The registered S-124 and S-129 modules of the overlay layer.
    trait Modules {
        fn s124(&self) -> &S124Overlay;
        fn s124_mut(&mut self) -> &mut S124Overlay;
        fn s129_mut(&mut self) -> &mut S129Overlay;
        fn user(&self) -> &NavwarnUserState {
            &self.s124().user
        }
        fn policy_mut(&mut self) -> &mut PortrayalPolicy {
            &mut self.s124_mut().policy
        }
        fn user_status(&self) -> String {
            self.s124().user_status()
        }
        fn registry(&mut self) -> &NavwarnRegistry;
        fn classify(&mut self, id: u32, class: UserClassification);
        fn attach_user_store(&mut self, path: PathBuf, user: String);
    }

    impl Modules for GmlOverlays {
        fn s124(&self) -> &S124Overlay {
            self.product::<S124Overlay>().expect("S-124 module")
        }
        fn s124_mut(&mut self) -> &mut S124Overlay {
            self.product_mut::<S124Overlay>().expect("S-124 module").0
        }
        fn s129_mut(&mut self) -> &mut S129Overlay {
            self.product_mut::<S129Overlay>().expect("S-129 module").0
        }
        fn registry(&mut self) -> &NavwarnRegistry {
            let (s124, core) = self.product_mut::<S124Overlay>().expect("S-124 module");
            s124.registry(core)
        }
        fn classify(&mut self, id: u32, class: UserClassification) {
            let (s124, core) = self.product_mut::<S124Overlay>().expect("S-124 module");
            s124.classify(core, id, class);
        }
        fn attach_user_store(&mut self, path: PathBuf, user: String) {
            self.attach_user_record(Product::S124, Box::new(state_file(&path)), user);
        }
    }

    fn root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    /// The installed catalogues, the UKHO S-124 exchange set, the IHO S-129
    /// test datasets and the SHOM imports all produce overlay instructions.
    #[test]
    fn test_data_produces_overlay_instructions() {
        let base = root();
        let data = base.join("TestData");
        if !data.join("IHO-S129-Test-Datasets").exists() {
            eprintln!("skipping: TestData not present");
            return;
        }
        let mut g = load_overlays(&ferrite_engine::platform::Platform::native(), &base);
        assert!(g.catalogue_errors.is_empty(), "{:?}", g.catalogue_errors);
        let mut paths = discover(
            &data.join("UKHO-S100-2026/ExchangeSets/S-124_GB"),
            &HashSet::new(),
        )
        .unwrap();
        paths.extend(discover(&data.join("IHO-S129-Test-Datasets"), &HashSet::new()).unwrap());
        let shom = data.join("SHOM-PING-S124");
        for dir in ["kml", "gpx"] {
            paths.extend(discover(&shom.join(dir), &HashSet::new()).unwrap());
        }
        paths.push(shom.join("shapefile/shapefiles.zip"));
        for path in &paths {
            let (dataset, format) = g.read(path, path).unwrap();
            let provenance = match format {
                Some(f) => Provenance::Imported(f),
                None => Provenance::Unverified,
            };
            assert!(matches!(g.add(dataset, provenance).unwrap(), Added::New));
        }
        let (_, list) = g.instructions("Day").unwrap();
        let per_dataset = |id: u32| {
            list.iter()
                .filter(|i| decode_key(i.feature_id().unwrap()).unwrap().0 == id)
                .count()
        };
        for d in &g.datasets {
            eprintln!("{} {:?}: {}", d.name(), d.product(), per_dataset(d.id));
        }
        assert!(list.iter().all(is_overlay_instruction));
        assert!(list
            .iter()
            .all(|i| i.viewing_groups().all(|v| v.0 == OVERLAY_VIEWING_GROUP)));
        let by_product = |p: Product| {
            g.datasets
                .iter()
                .filter(|d| d.product() == p)
                .map(|d| per_dataset(d.id))
                .sum::<usize>()
        };
        assert!(by_product(Product::S124) > 0);
        assert!(by_product(Product::S129) > 0);
        for d in g.datasets.iter().filter(|d| d.dataset.imported) {
            assert!(per_dataset(d.id) > 0, "{} not portrayed", d.name());
        }
        // Every S-129 dataset is portrayed (information box text literal:
        // transparency 1).
        for d in g.datasets.iter().filter(|d| d.product() == Product::S129) {
            assert!(per_dataset(d.id) > 0);
        }
        assert!(list
            .iter()
            .any(|i| matches!(i, DrawingInstruction::Text(_))));
        // S-124 text in undeclared viewing groups is not drawn by default.
        let s124_text = list.iter().any(|i| {
            matches!(i, DrawingInstruction::Text(_))
                && g.dataset(decode_key(i.feature_id().unwrap()).unwrap().0)
                    .is_some_and(|d| d.product() == Product::S124 && !d.dataset.imported)
        });
        eprintln!("S-124 text instructions present: {s124_text}");
        // Picking: attributes come with FC names.
        let key = list
            .iter()
            .filter_map(|i| i.feature_id())
            .find(|k| {
                g.locate(*k)
                    .is_some_and(|(d, f)| d.product() == Product::S124 && f.code == "NavwarnPart")
            })
            .unwrap();
        let pick = g.describe(key).unwrap();
        assert!(
            pick.feature_type.contains("NavwarnPart"),
            "{}",
            pick.feature_type
        );
        assert!(pick.attributes.iter().any(|(k, _)| k == "Warning"));
        // UI rows.
        let (rows, _, _) = g.navwarn_rows();
        assert_eq!(
            rows.len(),
            g.datasets
                .iter()
                .filter(|d| d.product() == Product::S124)
                .count()
        );
        assert!(rows.iter().any(|r| r.imported));
        let plans = g.ukc_rows();
        assert_eq!(plans.len(), 2);
        assert_eq!(plans[0].control_points.len(), 15);
        assert!(!plans[0].vessel.is_empty());
        // Toggling scaleMinimum changes the S-129 output.
        g.s129_mut().apply_scale_minimum = false;
        g.invalidate();
        let (_, list2) = g.instructions("Night").unwrap();
        assert!(list2.len() >= list.len());
        // Hiding a dataset removes its instructions.
        let first = g.datasets[0].id;
        assert!(g.set_visible(first, false));
        let (_, list3) = g.instructions("Night").unwrap();
        assert!(list3
            .iter()
            .all(|i| decode_key(i.feature_id().unwrap()).unwrap().0 != first));
    }
    /// TextPlacement uses viewing group 26, which the S-124 2.0.0 catalogue
    /// does not declare: literally (Part 9 9-11.2.2) it is never drawn; the
    /// "Correct known PC errata" option draws it.
    #[test]
    fn s124_text_placement_needs_errata_option() {
        let base = root();
        let path = base.join(
            "TestData/UKHO-S100-2026/ExchangeSets/S-124_GB/S100_ROOT/S-124/DATASET_FILES/124GB00S2Y2026N43.GML",
        );
        if !path.exists() {
            eprintln!("skipping: TestData not present");
            return;
        }
        let mut g = load_overlays(&ferrite_engine::platform::Platform::native(), &base);
        let text = std::fs::read_to_string(&path).unwrap().replace(
            "</S124:members>",
            "<S124:TextPlacement gml:id=\"TP1\"><S124:text>TEST LABEL</S124:text>\
             <S124:geometry><S100:pointProperty><S100:Point gml:id=\"TPG\"><gml:pos>50.70 -1.10</gml:pos>\
             </S100:Point></S100:pointProperty></S124:geometry></S124:TextPlacement></S124:members>",
        );
        let fc = g.catalogues(Product::S124).unwrap().fc.clone();
        let dataset =
            ferrite_s100_gml::read_bytes(text.as_bytes(), Some(&fc), Default::default()).unwrap();
        assert!(matches!(
            g.add(dataset, Provenance::Unverified).unwrap(),
            Added::New
        ));
        let has_label = |list: &[DrawingInstruction]| {
            list.iter()
                .any(|i| matches!(i, DrawingInstruction::Text(t) if t.text.contains("TEST LABEL")))
        };
        let (_, list) = g.instructions("Day").unwrap();
        assert!(!list.is_empty());
        assert!(!has_label(&list), "{list:#?}");
        g.set_correct_pc_errata(true);
        let (_, list) = g.instructions("Day").unwrap();
        assert!(has_label(&list), "{list:#?}");
        g.set_correct_pc_errata(false);
        let (_, list) = g.instructions("Day").unwrap();
        assert!(!has_label(&list));
    }

    /// The S-129 information box text has transparency 1: kept literally by
    /// default, drawn opaque with the errata option.
    #[test]
    fn s129_information_box_text_transparency() {
        let base = root();
        let path = base.join("TestData/IHO-S129-Test-Datasets/12900MCTDS130TS.gml");
        if !path.exists() {
            eprintln!("skipping: TestData not present");
            return;
        }
        let mut g = load_overlays(&ferrite_engine::platform::Platform::native(), &base);
        let (dataset, _) = g.read(&path, &path).unwrap();
        assert!(matches!(
            g.add(dataset, Provenance::Unverified).unwrap(),
            Added::New
        ));
        let texts = |g: &mut GmlOverlays| -> Vec<f32> {
            let (_, list) = g.instructions("Day").unwrap();
            list.iter()
                .filter_map(|i| match i {
                    DrawingInstruction::Text(t) => Some(t.color_opacity),
                    _ => None,
                })
                .collect()
        };
        let literal = texts(&mut g);
        assert!(!literal.is_empty());
        assert!(literal.iter().all(|a| *a == 0.0), "{literal:?}");
        g.set_correct_pc_errata(true);
        let corrected = texts(&mut g);
        assert_eq!(corrected.len(), literal.len());
        assert!(corrected.iter().all(|a| *a > 0.0), "{corrected:?}");
    }
    /// Minimal S-124 dataset: one warning with one point part.
    fn s124_dataset(g: &GmlOverlays, number: u32, extra_preamble: &str, part: &str) -> Dataset {
        let xml = format!(
            "<?xml version=\"1.0\"?><S124:Dataset gml:id=\"DS\" xmlns:S124=\"http://www.iho.int/S124/gml/2.0\" \
             xmlns:S100=\"http://www.iho.int/s100gml/5.0\" xmlns:gml=\"http://www.opengis.net/gml/3.2\" \
             xmlns:xlink=\"http://www.w3.org/1999/xlink\"><S100:DatasetIdentificationInformation>\
             <S100:productIdentifier>S-124</S100:productIdentifier></S100:DatasetIdentificationInformation><S124:members>\
             <S124:NavwarnPreamble gml:id=\"PRE\"><S124:messageSeriesIdentifier><S124:agencyResponsibleForProduction>UKHO</S124:agencyResponsibleForProduction>\
             <S124:nameOfSeries>WZ</S124:nameOfSeries><S124:warningNumber>{number}</S124:warningNumber>\
             <S124:warningType code=\"2\">x</S124:warningType><S124:year>26</S124:year></S124:messageSeriesIdentifier>\
             <S124:navwarnTitle><S124:language>eng</S124:language><S124:text>TITLE {number}</S124:text></S124:navwarnTitle>\
             <S124:intService>true</S124:intService><S124:navwarnTypeGeneral code=\"1\">Aids to Navigation Changes</S124:navwarnTypeGeneral>\
             <S124:publicationTime>20260201T000000Z</S124:publicationTime>{extra_preamble}</S124:NavwarnPreamble>\
             <S124:NavwarnPart gml:id=\"P1\"><S124:warningInformation><S124:information><S124:language>eng</S124:language>\
             <S124:text>BUOY OFF STATION</S124:text></S124:information></S124:warningInformation>{part}<S124:header xlink:href=\"#PRE\"/>\
             <S124:geometry><S100:pointProperty><S100:Point gml:id=\"G1\"><gml:pos>50.0 -1.0</gml:pos></S100:Point></S100:pointProperty></S124:geometry>\
             </S124:NavwarnPart></S124:members></S124:Dataset>"
        );
        let fc = g
            .catalogues(Product::S124)
            .expect("S-124 catalogues")
            .fc
            .clone();
        ferrite_s100_gml::read_bytes(xml.as_bytes(), Some(&fc), Default::default()).unwrap()
    }

    fn t(s: &str) -> DateTime<Utc> {
        navwarn::parse_date_time(s).unwrap()
    }

    /// S-124 11.1.1: classifications, "displayed" marks and first receipt
    /// survive a restart; a corrupt file never blocks startup.
    #[test]
    fn navwarn_user_state_persists_across_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("navwarn").join("alice.json");
        let start = |path: &Path| {
            let mut g = load_overlays(&ferrite_engine::platform::Platform::native(), &root());
            g.attach_user_store(path.to_path_buf(), "alice".into());
            g
        };
        // Session 1: receive, classify, display.
        let mut g = start(&path);
        assert!(g.user_status().contains("alice"), "{}", g.user_status());
        let a = s124_dataset(&g, 70, "", "");
        let b = s124_dataset(&g, 71, "", "");
        let (key_a, key_b) = (navwarn::dataset_key(&a), navwarn::dataset_key(&b));
        assert!(matches!(
            g.add(a.clone(), Provenance::Unverified).unwrap(),
            Added::New
        ));
        assert!(matches!(
            g.add(b.clone(), Provenance::Unverified).unwrap(),
            Added::New
        ));
        let (id_a, id_b) = (g.datasets[0].id, g.datasets[1].id);
        let received_a = g.datasets[0].received;
        g.classify(id_a, UserClassification::OffChart);
        g.classify(id_b, UserClassification::Information);
        g.mark_displayed(id_a);
        assert!(path.exists());
        let message_key = g.registry().messages[0].key.clone();
        assert!(!message_key.starts_with("sha256:"), "{message_key}");
        let (rows, _, new_count) = g.navwarn_rows();
        assert!(!rows[0].new || rows[0].dataset != id_a);
        assert_eq!(new_count, 0); // a displayed, b Information
        drop(g);

        // Session 2 (restart): same OS user, same datasets.
        let mut g = start(&path);
        assert!(g.user_status().contains("restored"), "{}", g.user_status());
        assert_eq!(
            g.user().classification.get(&message_key),
            Some(&UserClassification::OffChart)
        );
        assert!(g.user().displayed.contains(&key_a));
        assert!(g.user().received.contains_key(&key_b));
        assert!(matches!(
            g.add(a.clone(), Provenance::Unverified).unwrap(),
            Added::New
        ));
        assert_eq!(g.datasets[0].received, received_a, "first receipt kept");
        let id = g.datasets[0].id;
        let (rows, _, _) = g.navwarn_rows();
        let row = rows.iter().find(|r| r.dataset == id).unwrap();
        assert_eq!(row.classification, NavwarnClassification::OffChart);
        assert!(!row.new && !row.portrayed);
        // Reclassifying on chart removes the entry from the file.
        g.classify(id, UserClassification::OnChart);
        drop(g);
        let mut g = start(&path);
        assert!(!g.user().classification.contains_key(&message_key));
        assert_eq!(g.user().classification.len(), 1); // b stays Information
                                                      // The "Correct known PC errata" option is off by default and saved.
        assert!(!g.correct_pc_errata());
        g.set_correct_pc_errata(true);
        drop(g);
        let mut g = start(&path);
        assert!(g.correct_pc_errata());
        assert_eq!(g.user().classification.len(), 1);
        g.set_correct_pc_errata(false);
        drop(g);
        assert!(!start(&path).correct_pc_errata());

        // Another OS user has a separate file.
        let bob = dir.path().join("navwarn").join("bob.json");
        let g = start(&bob);
        assert!(g.user().classification.is_empty() && g.user().received.is_empty());

        // Corrupt file: empty state, file set aside, saving works again.
        std::fs::write(&path, b"{not json").unwrap();
        let mut g = start(&path);
        assert!(g.user().classification.is_empty());
        assert!(
            g.user_status().contains("unreadable"),
            "{}",
            g.user_status()
        );
        assert!(!path.exists());
        assert!(std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().contains(".unreadable-")));
        assert!(matches!(
            g.add(a, Provenance::Unverified).unwrap(),
            Added::New
        ));
        assert!(path.exists());
        assert!(read_user_state(&path).unwrap().is_some());
        // Unsupported version and oversized keys are rejected, not trusted.
        std::fs::write(
            &path,
            br#"{"version":9,"user":"alice","saved":"2026-01-01T00:00:00Z","classification":{},"displayed":[],"received":{}}"#,
        )
        .unwrap();
        assert!(read_user_state(&path).is_err());
        // Files written before the errata option read it as off.
        std::fs::write(
            &path,
            br#"{"version":1,"user":"alice","saved":"2026-01-01T00:00:00Z","classification":{},"displayed":[],"received":{}}"#,
        )
        .unwrap();
        assert!(!read_user_state(&path).unwrap().unwrap().correct_pc_errata);
        // No store: nothing written, startup unaffected.
        let mut g = load_overlays(&ferrite_engine::platform::Platform::native(), &root());
        assert_eq!(g.user_status(), "User state not saved");
        let c = s124_dataset(&g, 72, "", "");
        assert!(matches!(
            g.add(c, Provenance::Unverified).unwrap(),
            Added::New
        ));
    }

    #[test]
    fn user_state_path_is_per_os_user() {
        if std::env::var_os("FERRITE_NAVWARN_STATE_PATH").is_some() {
            return;
        }
        // Tests never persist to the real application folder.
        assert_eq!(default_user_state_path("alice"), None);
        assert!(!os_user_name().is_empty());
    }

    /// S-124 status and fixedDateRange follow the S-101 viewing time; the
    /// 24 h new-message indication keeps running on the real clock.
    #[test]
    fn navwarn_lifecycle_follows_viewing_time() {
        let mut g = load_overlays(&ferrite_engine::platform::Platform::native(), &root());
        let range = "<S124:fixedDateRange><S124:dateEnd>2026-02-12</S124:dateEnd>\
                     <S124:dateStart>2026-02-10</S124:dateStart></S124:fixedDateRange>";
        let ds = s124_dataset(
            &g,
            80,
            "<S124:cancellationDate>20260220T000000Z</S124:cancellationDate>",
            range,
        );
        assert!(matches!(
            g.add(ds, Provenance::Unverified).unwrap(),
            Added::New
        ));
        let id = g.datasets[0].id;
        let status_at = |g: &mut GmlOverlays, time: Option<&str>| {
            g.set_viewing_time(time.map(t));
            let r = g.registry();
            (
                r.messages[0].status.clone(),
                r.messages[0].parts[0].timing,
                r.now,
            )
        };
        let (status, timing, now) = status_at(&mut g, Some("2026-02-05T00:00:00Z"));
        assert_eq!(status, Status::NotYetActive);
        assert!(matches!(timing, PartTiming::BeforeRange { .. }));
        assert_eq!(now, t("2026-02-05T00:00:00Z"));
        let (status, timing, _) = status_at(&mut g, Some("2026-02-11T00:00:00Z"));
        assert_eq!(status, Status::Active);
        assert_eq!(timing, PartTiming::InRange);
        // In range: portrayed even with the strict policy.
        g.policy_mut().show_before_fixed_date_range = false;
        g.invalidate();
        let (_, list) = g.instructions("Day").unwrap();
        assert!(!list.is_empty());
        let (status, timing, _) = status_at(&mut g, Some("2026-02-13T00:00:00Z"));
        assert_eq!(status, Status::Active);
        assert!(matches!(timing, PartTiming::AfterRange { .. }));
        let (_, list) = g.instructions("Day").unwrap();
        assert!(list.is_empty(), "part after its range is not portrayed");
        let (status, _, _) = status_at(&mut g, Some("2026-02-20T00:00:00Z"));
        assert!(status.is_cancelled());
        // Received now (real clock): still new although the viewing time is
        // months earlier.
        let (rows, _, new_count) = g.navwarn_rows();
        assert!(rows[0].new && new_count == 1);
        assert!(rows[0].status.starts_with("Cancelled"));
        // No viewing time: the system clock (2026-02-20 has passed).
        let (status, _, now) = status_at(&mut g, None);
        assert!(status.is_cancelled());
        assert!((Utc::now() - now).num_seconds().abs() < 60);
        // Unchanged selection: no re-evaluation is forced.
        assert!(!g.set_viewing_time(None));
        assert!(g.set_viewing_time(Some(t("2026-02-11T00:00:00Z"))));
        assert!(g.next_check().is_some());
        assert_eq!(g.dataset(id).map(|d| d.product()), Some(Product::S124));
    }

    /// The object panel previews S-124 symbols from the registered overlay
    /// source (no source cell).
    #[test]
    fn overlay_symbol_preview_resolves_registered_source() {
        let mut g = load_overlays(&ferrite_engine::platform::Platform::native(), &root());
        let ds = s124_dataset(&g, 90, "", "");
        assert!(matches!(
            g.add(ds, Provenance::Unverified).unwrap(),
            Added::New
        ));
        let (_, list) = g.instructions("Day").unwrap();
        let (key, symbol) = list
            .iter()
            .find_map(|i| match i {
                DrawingInstruction::Point(p) if p.symbol_ref.starts_with("S-124/") => {
                    Some((p.feature_id?, p.symbol_ref.clone()))
                }
                _ => None,
            })
            .expect("S-124 point symbol");
        let pick = g.describe(key).unwrap();
        let feature = ferrite_gpu::SelectedFeature {
            feature_type: pick.feature_type,
            feature_id: key,
            foid: pick.foid,
            cell_index: None,
            primitive_type: pick.primitive,
            source: Some(pick.source),
            attributes: pick.attributes,
            world_pos: (0., 0.),
            longitude_shift: 0.,
            definition: pick.definition,
            symbol_name: Some(symbol),
        };
        let source = ferrite_gpu::overlay_symbol_source("S-124").unwrap();
        let active = source
            .profiles()
            .get_default()
            .or_else(|| source.profiles().profiles.values().next())
            .unwrap()
            .clone();
        let preview = ferrite_ui::prepare_overlay_symbol_preview(&feature, &active)
            .unwrap()
            .expect("overlay preview");
        assert!(preview.matches(&feature, &active.id));
    }

    /// Selecting a NavwarnPart highlights its NavwarnAreaAffected (11.2),
    /// which has no portrayal of its own.
    #[test]
    fn selected_part_highlights_area_affected() {
        let base = root();
        let path = base.join(
            "TestData/UKHO-S100-2026/ExchangeSets/S-124_GB/S100_ROOT/S-124/DATASET_FILES/124GB00S2Y2026N44.GML",
        );
        if !path.exists() {
            eprintln!("skipping: TestData not present");
            return;
        }
        let mut g = load_overlays(&ferrite_engine::platform::Platform::native(), &base);
        let (dataset, _) = g.read(&path, &path).unwrap();
        assert!(matches!(
            g.add(dataset, Provenance::Unverified).unwrap(),
            Added::New
        ));
        let (_, list) = g.instructions("Day").unwrap();
        let id = g.datasets[0].id;
        // The area affected is never drawn by the catalogue.
        let area = g.key_of(id, "P1_A1").unwrap();
        assert!(list.iter().all(|i| i.feature_id() != Some(area)));
        let part = g.key_of(id, "P1").unwrap();
        let highlight = g.highlight_geometry(part);
        assert!(highlight.iter().any(|ring| ring.len() > 5), "{highlight:?}");
        // Listing selection zooms to the part and its area.
        assert!(g.focus_bounds(id, Some("P1")).is_some());
        assert_eq!(g.first_part(id).as_deref(), Some("P1"));
    }

    /// S-158-129 19 sees the overlay datasets already loaded: cancelling a
    /// loaded dataset is clean, cancelling an unknown or already cancelled
    /// one is reported, and a dataset opened in the same batch counts.
    #[test]
    fn exchange_set_cancellation_sees_loaded_datasets() {
        let mut g = load_overlays(&ferrite_engine::platform::Platform::native(), &root());
        let mut loaded = s124_dataset(&g, 90, "", "");
        loaded.source = Some(PathBuf::from("/charts/124GB00LOADED.GML"));
        g.add(loaded, Provenance::Unverified).unwrap();
        // cancellationDate passed: the message status is Cancelled.
        let mut gone = s124_dataset(
            &g,
            91,
            "<S124:cancellationDate>20200101T000000Z</S124:cancellationDate>",
            "",
        );
        gone.source = Some(PathBuf::from("/charts/124GB00GONE.GML"));
        g.add(gone, Provenance::Unverified).unwrap();
        let on_system = g.on_system();
        assert!(on_system.contains("124GB00LOADED.GML"));
        assert!(!on_system.contains("124GB00GONE.GML"));

        let tmp = tempfile::tempdir().unwrap();
        let set = tmp.path().join("SET/S100_ROOT");
        std::fs::create_dir_all(&set).unwrap();
        let cancel = |name: &str| {
            format!("<S100_DatasetDiscoveryMetadata><fileName>file:/S-124/DATASET_FILES/{name}</fileName><purpose>cancellation</purpose><editionNumber>0</editionNumber></S100_DatasetDiscoveryMetadata>")
        };
        let write_catalogue = |names: &[&str]| {
            let entries: String = names.iter().map(|n| cancel(n)).collect();
            std::fs::write(
                set.join("CATALOG.XML"),
                format!(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\"?><S100_ExchangeCatalogue xmlns=\"http://www.iho.int/s100/xc/5.2\">\
                     <identifier><identifier>TEST</identifier><dateTime>2026-02-17T10:18:33Z</dateTime></identifier>\
                     <datasetDiscoveryMetadata>{entries}</datasetDiscoveryMetadata></S100_ExchangeCatalogue>"
                ),
            )
            .unwrap();
        };
        let unknown = |found: &[PathBuf]| -> Vec<String> {
            let reports = exchange_set_reports(vec![set.clone()], found, &on_system);
            assert_eq!(reports.len(), 1);
            let report = reports[0].1.as_ref().unwrap();
            assert_eq!(report.products, [Product::S124]);
            let mut objects: Vec<String> = report
                .issues
                .iter()
                .filter(|i| i.check == "S-124/exchange/cancellation-unknown")
                .filter_map(|i| i.object.clone())
                .collect();
            objects.sort();
            objects
        };

        write_catalogue(&["124GB00LOADED.GML"]);
        assert!(unknown(&[]).is_empty());
        write_catalogue(&["124GB00UNKNOWN.GML"]);
        assert_eq!(unknown(&[]), ["124GB00UNKNOWN.GML"]);
        write_catalogue(&["124GB00GONE.GML"]);
        assert_eq!(unknown(&[]), ["124GB00GONE.GML"]);
        // Opened in the same batch from outside the exchange set.
        let other = tmp.path().join("124GB00UNKNOWN.GML");
        std::fs::write(
            &other,
            "<S124:Dataset xmlns:S124=\"http://www.iho.int/S124/gml/2.0\"/>",
        )
        .unwrap();
        write_catalogue(&["124GB00UNKNOWN.GML"]);
        assert!(unknown(std::slice::from_ref(&other)).is_empty());
    }
}
