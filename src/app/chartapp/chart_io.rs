//! Chart loading, cache I/O, and the regenerate-instructions pipeline.
//!
//! Loading is structured as: `load_charts` (kicks off a background worker
//! that streams parsed cells back over an mpsc channel) → `poll_loading`
//! (called every frame to drain ready cells without blocking the UI) →
//! `finalize_loading` (runs Lua portrayal, builds the renderer state).
//!
//! Cache I/O writes a small framed format
//! (`magic + schema_version + sha256 + bincode payload`) next to the chart
//! file so reopening the same chart skips the Lua run when nothing changed.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};

use anyhow::Result;
use sha2::{Digest, Sha256};
use tracing::{error, info, warn};

use ferrite_render::{GeoBounds, RenderContext, Viewport, WorldPoint};
use ferrite_s100_core::S101Cell;

use crate::app::lua_runtime::try_lua_portrayal;
use crate::app::portrayal::generate_default_instructions;
use crate::{BackgroundLoadingState, ChartApp, ChartLoadResult};

impl ChartApp {
    /// Start loading chart files in background (non-blocking)
    pub(crate) fn load_charts(&mut self, paths: &[PathBuf]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }

        // Don't start new loading if already loading
        if self.loading_state.is_some() {
            warn!("Loading already in progress, ignoring new load request");
            return Ok(());
        }

        // Reset bounds from world extent to empty so expand() calculates from chart data
        if !self.chart_loaded {
            self.bounds = GeoBounds::default();
        }

        // Filter out already loaded files
        let new_paths: Vec<PathBuf> = paths
            .iter()
            .filter(|p| {
                let canonical = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
                !self.loaded_paths.contains(&canonical)
            })
            .cloned()
            .collect();

        if new_paths.is_empty() {
            #[cfg(debug_assertions)]
            info!("All selected files are already loaded");
            return Ok(());
        }

        let total_files = new_paths.len();
        #[cfg(debug_assertions)]
        info!(
            "Starting background load of {} chart file(s) ({} skipped as duplicates)",
            total_files,
            paths.len() - total_files
        );

        // Create channel for receiving loaded cells
        let (tx, rx) = mpsc::channel();

        // Clone FC for background thread
        let fc = Arc::clone(&self.fc);

        // Spawn background thread for loading
        std::thread::spawn(move || {
            let fc_feature_codes = fc.feature_type_codes();

            // Load each file in the background thread
            for path in new_paths {
                #[cfg(debug_assertions)]
                info!("Background loading: {}", path.display());

                let result = match S101Cell::load(&path) {
                    Ok(mut cell) => {
                        // Normalize feature codes
                        cell.normalize_feature_codes(&fc_feature_codes);

                        #[cfg(debug_assertions)]
                        {
                            let stats = cell.statistics();
                            info!(
                                "Loaded: {} features, {} points, {} curves, {} surfaces",
                                stats.features, stats.points, stats.curves, stats.surfaces
                            );
                        }

                        Some(ChartLoadResult { path, cell })
                    }
                    Err(e) => {
                        error!("Failed to load chart {}: {}", path.display(), e);
                        None
                    }
                };

                // Send result (even None to track progress)
                if tx.send(result).is_err() {
                    // Receiver dropped, stop loading
                    break;
                }
            }
        });

        // Set loading state
        self.loading_state = Some(BackgroundLoadingState {
            total_files,
            loaded_count: 0,
            receiver: rx,
        });

        // Update UI to show loading
        if let Some(renderer) = &mut self.renderer {
            renderer.ui_state.loading_progress = Some((total_files, 0));
        }

        Ok(())
    }

    /// Poll for background loading completion (non-blocking)
    /// Returns true if loading is complete
    pub(crate) fn poll_loading(&mut self) -> bool {
        let loading_state = match &mut self.loading_state {
            Some(state) => state,
            None => return true, // No loading in progress
        };

        let mut completed = false;
        let mut new_cells = Vec::new();

        // Non-blocking receive of all available results
        loop {
            match loading_state.receiver.try_recv() {
                Ok(result) => {
                    loading_state.loaded_count += 1;

                    if let Some(load_result) = result {
                        new_cells.push(load_result);
                    }

                    // Check if all files are loaded
                    if loading_state.loaded_count >= loading_state.total_files {
                        completed = true;
                        break;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {
                    // No more results available right now
                    break;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    // Sender dropped (thread finished or crashed)
                    completed = true;
                    break;
                }
            }
        }

        // Process newly loaded cells
        for load_result in new_cells {
            // Expand bounds
            for point in load_result.cell.points.values() {
                let wp = WorldPoint::new(point.position.x, point.position.y);
                self.bounds.expand(wp);
            }
            for curve in load_result.cell.curves.values() {
                for pos in curve.all_positions() {
                    let wp = WorldPoint::new(pos.x, pos.y);
                    self.bounds.expand(wp);
                }
            }

            // Track loaded path
            let canonical = load_result
                .path
                .canonicalize()
                .unwrap_or_else(|_| load_result.path.clone());
            self.loaded_paths.insert(canonical);

            // Add cell
            self.cells.push(load_result.cell);
        }

        // Update UI progress
        if let Some(renderer) = &mut self.renderer {
            if let Some(state) = &self.loading_state {
                renderer.ui_state.loading_progress = Some((state.total_files, state.loaded_count));
            }
        }

        // Finalize if complete
        if completed {
            self.finalize_loading();
        }

        completed
    }

    /// Finalize loading after all cells are loaded
    pub(crate) fn finalize_loading(&mut self) {
        // Clear loading state
        self.loading_state = None;

        if !self.cells.is_empty() {
            // Expand bounds by 10%
            self.bounds.expand_by_percent(0.1);
            self.chart_loaded = true;

            // Generate drawing instructions for all cells
            if let Err(e) = self.regenerate_instructions() {
                error!("Failed to generate instructions: {}", e);
            }

            // Pre-compute area triangulations to avoid cold-path stall on first render
            if let Some(renderer) = &mut self.renderer {
                renderer.precompute_triangulations(self.render_context.raw_instructions());
            }
        }

        // Update UI state
        if let Some(renderer) = &mut self.renderer {
            renderer.ui_state.loading_progress = None;

            if self.chart_loaded {
                let chart_info = if self.cells.len() == 1 {
                    self.cells
                        .first()
                        .and_then(|c| {
                            c.file_path
                                .file_name()
                                .map(|n| n.to_string_lossy().to_string())
                        })
                        .unwrap_or_else(|| "Chart".to_string())
                } else {
                    format!("{} charts loaded", self.cells.len())
                };
                renderer.ui_state.loaded_chart = Some(chart_info);

                let total_count: usize = self.cells.iter().map(|c| c.statistics().features).sum();
                renderer.ui_state.feature_count = total_count;
                renderer.ui_state.chart_count = self.cells.len();

                // Set compilation scale
                let min_scale = self
                    .cells
                    .iter()
                    .map(|c| c.compilation_scale)
                    .min()
                    .unwrap_or(22000);
                renderer.set_compilation_scale(min_scale);

                // Compute per-cell bounding boxes for world map masking
                let mut chart_boxes = Vec::with_capacity(self.cells.len());
                for cell in &self.cells {
                    let mut cmin_x = f64::MAX;
                    let mut cmin_y = f64::MAX;
                    let mut cmax_x = f64::MIN;
                    let mut cmax_y = f64::MIN;
                    for point in cell.points.values() {
                        let x = point.position.x;
                        let y = point.position.y;
                        if x < cmin_x {
                            cmin_x = x;
                        }
                        if y < cmin_y {
                            cmin_y = y;
                        }
                        if x > cmax_x {
                            cmax_x = x;
                        }
                        if y > cmax_y {
                            cmax_y = y;
                        }
                    }
                    for curve in cell.curves.values() {
                        for pos in curve.all_positions() {
                            if pos.x < cmin_x {
                                cmin_x = pos.x;
                            }
                            if pos.y < cmin_y {
                                cmin_y = pos.y;
                            }
                            if pos.x > cmax_x {
                                cmax_x = pos.x;
                            }
                            if pos.y > cmax_y {
                                cmax_y = pos.y;
                            }
                        }
                    }
                    if cmin_x < cmax_x && cmin_y < cmax_y {
                        chart_boxes.push((cmin_x, cmin_y, cmax_x, cmax_y));
                    }
                }
                renderer.set_world_map_chart_boxes(chart_boxes);
            }
        }

        info!(
            "Loading complete: {} charts, {} features",
            self.cells.len(),
            self.cells
                .iter()
                .map(|c| c.statistics().features)
                .sum::<usize>()
        );

        // Debug interior rings if --debug-rings
        if self.debug_rings {
            self.log_interior_ring_debug();
        }

        // Start auto-screenshot countdown (wait a few frames for rendering)
        if self.auto_screenshot.is_some() && self.chart_loaded {
            // Apply center override if specified (lat,lon → pan_offset in world coords)
            if let Some((lat, lon)) = self.auto_center {
                let chart_center_x = (self.bounds.min_x + self.bounds.max_x) / 2.0;
                let chart_center_y = (self.bounds.min_y + self.bounds.max_y) / 2.0;
                self.pan_offset.0 = lon - chart_center_x;
                self.pan_offset.1 = lat - chart_center_y;
            }
            // Apply zoom override if specified
            if let Some(zoom) = self.auto_zoom {
                self.zoom_level = zoom;
                self.zoom_target = zoom;
            }
            // Re-render with zoom applied
            self.update_view();
            self.frames_since_loaded = Some(0);
        }
    }

    /// Clear all loaded charts
    /// Log detailed interior ring debug info for all loaded cells
    pub(crate) fn log_interior_ring_debug(&self) {
        info!("=== INTERIOR RING DEBUG ===");
        for (ci, cell) in self.cells.iter().enumerate() {
            let mut surfaces_with_holes = 0;
            let mut total_interior_rings = 0;
            let mut unclosed_rings = 0;

            for surface in cell.surfaces.values() {
                if surface.interior_rings.is_empty() {
                    continue;
                }
                surfaces_with_holes += 1;
                total_interior_rings += surface.interior_rings.len();

                for (ri, ring_curves) in surface.interior_rings.iter().enumerate() {
                    // Check closure by collecting raw points
                    let mut pts = Vec::new();
                    for oc in ring_curves {
                        let key = oc.curve_id.key();
                        if let Some(curve) = cell.curves.get(&key) {
                            let positions = curve.all_positions();
                            if oc.orientation {
                                for p in &positions {
                                    pts.push((p.x, p.y));
                                }
                            } else {
                                for p in positions.iter().rev() {
                                    pts.push((p.x, p.y));
                                }
                            }
                        } else if let Some(composite) = cell.composite_curves.get(&key) {
                            for sub in &composite.curves {
                                let sk = sub.curve_id.key();
                                if let Some(c) = cell.curves.get(&sk) {
                                    let positions = c.all_positions();
                                    let forward = oc.orientation == sub.orientation;
                                    if forward {
                                        for p in &positions {
                                            pts.push((p.x, p.y));
                                        }
                                    } else {
                                        for p in positions.iter().rev() {
                                            pts.push((p.x, p.y));
                                        }
                                    }
                                }
                            }
                        }
                    }

                    let is_closed = if pts.len() >= 2 {
                        let first = pts.first().unwrap();
                        let last = pts.last().unwrap();
                        (first.0 - last.0).abs() < 1e-7 && (first.1 - last.1).abs() < 1e-7
                    } else {
                        false
                    };

                    if !is_closed {
                        unclosed_rings += 1;
                    }

                    // Find which features reference this surface
                    let surface_key = surface.id.key();
                    let referencing_features: Vec<_> = cell
                        .features
                        .values()
                        .filter(|f| {
                            f.spatial_associations
                                .iter()
                                .any(|sa| sa.spatial_id.key() == surface_key)
                        })
                        .filter_map(|f| f.feature_code.as_deref())
                        .collect();

                    // Compute ring bounding box
                    let (mut rx0, mut ry0, mut rx1, mut ry1) =
                        (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
                    for &(x, y) in &pts {
                        if x < rx0 {
                            rx0 = x;
                        }
                        if y < ry0 {
                            ry0 = y;
                        }
                        if x > rx1 {
                            rx1 = x;
                        }
                        if y > ry1 {
                            ry1 = y;
                        }
                    }

                    info!(
                        "  Cell[{}] Surface {} ring[{}]: {} curves, {} pts, closed={}, bbox=[{:.6},{:.6}]-[{:.6},{:.6}], features={:?}",
                        ci,
                        surface_key,
                        ri,
                        ring_curves.len(),
                        pts.len(),
                        is_closed,
                        rx0, ry0, rx1, ry1,
                        referencing_features,
                    );
                }
            }

            info!(
                "Cell[{}]: {} surfaces with holes, {} total interior rings, {} unclosed",
                ci, surfaces_with_holes, total_interior_rings, unclosed_rings
            );
        }
        info!("=== END INTERIOR RING DEBUG ===");
    }

    pub(crate) fn clear_charts(&mut self) {
        #[cfg(debug_assertions)]
        info!("Clearing all charts");

        self.cells.clear();
        self.bounds = GeoBounds::new(-180.0, -90.0, 180.0, 90.0);
        self.chart_loaded = false;
        self.zoom_level = 1.0;
        self.zoom_target = 1.0;
        self.zoom_animating = false;
        self.pan_offset = (0.0, 0.0);
        self.rendered_symbols.clear();
        self.loaded_paths.clear();

        // Clear render context and base instruction count
        if let Some(renderer) = &self.renderer {
            let size = renderer.window().inner_size();
            self.render_context =
                RenderContext::new(Viewport::new(size.width as f32, size.height as f32));
        }
        self.base_instruction_count = 0;

        // Update UI state
        if let Some(renderer) = &mut self.renderer {
            renderer.ui_state.loaded_chart = None;
            renderer.ui_state.feature_count = 0;
            renderer.ui_state.chart_count = 0;
            renderer.ui_state.selected_feature = None;

            // Clear renderer frame
            renderer.begin_frame();
            renderer.set_lon_wrap_pixels(360.0 * self.render_context.scaler.scale_x() as f32);
            renderer.add_world_map_lines(&self.render_context.scaler);
        }

        info!("All charts cleared");
    }

    /// Regenerate drawing instructions from loaded cells
    /// Compute instruction cache file path for the current chart set.
    ///
    /// The cache key is built from the canonical (absolute, symlink-resolved)
    /// path of each chart so the same chart loaded via different working
    /// directories — e.g. `cargo run` vs running the exe from `target/release/`
    /// — yields the same cache hash. Without canonicalization, every distinct
    /// CWD spawned a parallel cache file for identical chart content.
    pub(crate) fn instruction_cache_path(&self) -> Option<PathBuf> {
        if self.cells.is_empty() {
            return None;
        }
        let first_path = &self.cells[0].file_path;
        let cache_dir = first_path.parent()?;

        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for cell in &self.cells {
            let canonical =
                std::fs::canonicalize(&cell.file_path).unwrap_or_else(|_| cell.file_path.clone());
            // Lowercase on Windows: NTFS is case-insensitive but path strings
            // can vary in case, which would otherwise produce different hashes.
            #[cfg(windows)]
            let key = canonical.to_string_lossy().to_lowercase();
            #[cfg(not(windows))]
            let key = canonical.to_string_lossy().into_owned();
            key.hash(&mut hasher);
        }
        self.current_profile_name.hash(&mut hasher);
        let hash = hasher.finish();

        Some(cache_dir.join(format!(".ferrite_cache_{:016x}.bin", hash)))
    }

    /// Check if instruction cache is valid (newer than all chart files)
    pub(crate) fn is_cache_valid(&self, cache_path: &Path) -> bool {
        let cache_meta = match fs::metadata(cache_path) {
            Ok(m) => m,
            Err(_) => return false,
        };
        let cache_mtime = match cache_meta.modified() {
            Ok(t) => t,
            Err(_) => return false,
        };
        // Cache must be newer than all chart files
        for cell in &self.cells {
            if let Ok(meta) = fs::metadata(&cell.file_path) {
                if let Ok(chart_mtime) = meta.modified() {
                    if chart_mtime > cache_mtime {
                        return false;
                    }
                }
            }
        }
        true
    }

    /// Cache file format:
    /// `[4B magic "FRC\x01"][4B schema version][32B SHA-256 hash][payload]`
    ///
    /// The schema version is incremented whenever `DrawingInstruction` struct
    /// layout changes, ensuring stale caches are rejected instead of producing
    /// corrupted rendering data.
    const CACHE_MAGIC: &'static [u8; 4] = b"FRC\x01";
    /// Bump this version whenever DrawingInstruction fields change.
    const CACHE_SCHEMA_VERSION: u32 = 2;

    /// Wrap a bincode payload with magic + schema version + SHA-256 corruption-detection hash.
    /// NOTE: This is NOT cryptographic authentication — it detects accidental corruption only.
    pub(crate) fn wrap_cache(payload: &[u8]) -> Vec<u8> {
        let hash = Sha256::digest(payload);
        let mut out = Vec::with_capacity(4 + 4 + 32 + payload.len());
        out.extend_from_slice(Self::CACHE_MAGIC);
        out.extend_from_slice(&Self::CACHE_SCHEMA_VERSION.to_le_bytes());
        out.extend_from_slice(&hash);
        out.extend_from_slice(payload);
        out
    }

    /// Verify integrity and deserialize a cache file.
    /// Returns Err if magic/version mismatch or SHA-256 hash doesn't match.
    pub(crate) fn verify_and_deserialize_cache(
        data: &[u8],
    ) -> std::result::Result<Vec<ferrite_render::DrawingInstruction>, String> {
        const HEADER_LEN: usize = 4 + 4 + 32; // magic + version + hash
        if data.len() < HEADER_LEN {
            return Err("cache file too small".into());
        }
        // Check magic
        if &data[..4] != Self::CACHE_MAGIC {
            return Err("invalid cache magic (legacy or corrupted file)".into());
        }
        // Check schema version
        let version = u32::from_le_bytes(data[4..8].try_into().unwrap());
        if version != Self::CACHE_SCHEMA_VERSION {
            return Err(format!(
                "schema version mismatch: file={}, expected={}",
                version,
                Self::CACHE_SCHEMA_VERSION
            ));
        }
        // Verify SHA-256 hash
        let stored_hash = &data[8..40];
        let payload = &data[HEADER_LEN..];
        let computed_hash = Sha256::digest(payload);
        if computed_hash.as_slice() != stored_hash {
            return Err("SHA-256 integrity check failed (file tampered or corrupted)".into());
        }
        // Deserialize
        bincode::deserialize(payload).map_err(|e| format!("deserialization failed: {}", e))
    }

    pub(crate) fn regenerate_instructions(&mut self) -> Result<()> {
        if self.cells.is_empty() {
            return Ok(());
        }

        // Get current viewport size from renderer/window
        let (width, height) = if let Some(renderer) = &self.renderer {
            let size = renderer.window().inner_size();
            (size.width as f32, size.height as f32)
        } else {
            (1920.0, 1080.0)
        };

        // Clear existing instructions
        self.render_context = RenderContext::new(Viewport::new(width, height));
        self.render_context.set_bounds(self.bounds);

        // Try to load instructions from binary cache
        let cache_path = self.instruction_cache_path();
        let mut cache_loaded = false;

        if let Some(ref cp) = cache_path {
            if self.is_cache_valid(cp) {
                let cache_start = std::time::Instant::now();
                match fs::read(cp) {
                    Ok(data) => match Self::verify_and_deserialize_cache(&data) {
                        Ok(instructions) => {
                            let count = instructions.len();
                            self.render_context
                                .set_instructions_from_cache(instructions);
                            cache_loaded = true;
                            info!(
                                "Loaded {} instructions from cache in {:.1}ms: {}",
                                count,
                                cache_start.elapsed().as_secs_f64() * 1000.0,
                                cp.display()
                            );
                        }
                        Err(e) => {
                            warn!("Cache rejected: {}. Regenerating.", e);
                            let _ = fs::remove_file(cp);
                        }
                    },
                    Err(e) => {
                        warn!("Cache read failed: {}. Regenerating.", e);
                    }
                }
            }
        }

        if !cache_loaded {
            // Get current settings from renderer
            let current_settings = self.renderer.as_ref().map(|r| r.settings().clone());

            // Try Lua portrayal with current color profile and settings
            let lua_result = try_lua_portrayal(
                &self.cells,
                &self.fc,
                &self.pc,
                &mut self.render_context,
                &self.current_profile_name,
                current_settings.as_ref(),
            );

            if let Err(e) = lua_result {
                warn!("Lua portrayal failed: {}. Using default instructions.", e);
                for cell in &self.cells {
                    generate_default_instructions(
                        cell,
                        &mut self.render_context,
                        &self.pc,
                        &self.current_profile_name,
                    );
                }
            }

            // Save instruction cache for next load
            if let Some(ref cp) = cache_path {
                let cache_start = std::time::Instant::now();
                let instructions = self.render_context.raw_instructions();
                match bincode::serialize(instructions) {
                    Ok(payload) => {
                        let signed = Self::wrap_cache(&payload);
                        let size_kb = signed.len() / 1024;
                        match fs::write(cp, &signed) {
                            Ok(_) => {
                                info!(
                                    "Saved instruction cache ({}KB) in {:.1}ms: {}",
                                    size_kb,
                                    cache_start.elapsed().as_secs_f64() * 1000.0,
                                    cp.display()
                                );
                            }
                            Err(e) => warn!("Failed to save instruction cache: {}", e),
                        }
                    }
                    Err(e) => warn!("Failed to serialize instructions: {}", e),
                }
            }
        }

        // Save base instruction count (chart instructions only, before plugin instructions)
        self.base_instruction_count = self.render_context.instruction_count();

        // Update renderer
        // Get color profile and visible viewing groups before mutable borrows
        let color_profile = self
            .pc
            .color_profiles
            .profiles
            .get(&self.current_profile_name);
        let visible_vgs = self.get_visible_viewing_groups();

        if let Some(renderer) = &mut self.renderer {
            let size = renderer.window().inner_size();
            self.render_context
                .set_viewport(size.width as f32, size.height as f32);
            self.render_context.zoom_to_fit(self.bounds);

            renderer.begin_frame();
            renderer.set_lon_wrap_pixels(360.0 * self.render_context.scaler.scale_x() as f32);
            renderer.add_world_map_lines(&self.render_context.scaler);
            renderer.add_instructions_with_symbols(
                &mut self.render_context,
                Some(&mut self.symbol_cache),
                color_profile,
                visible_vgs.as_ref(),
            );

            // Rebuild hit testing
            self.build_rendered_symbols();
        }

        Ok(())
    }
}
