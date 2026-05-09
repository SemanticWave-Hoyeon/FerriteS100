//! Color profile + viewing-group helpers on `ChartApp`.
//!
//! These methods couple the loaded `PortrayalCatalogue` with the renderer's
//! current display mode. `regenerate_portrayal()` is the heaviest of them —
//! it tears down the current `RenderContext` and re-runs the Lua portrayal
//! engine for the active profile, used after a Day/Dusk/Night switch.

use ferrite_render::{RenderContext, Viewport};
use ferrite_wgpu::DisplayMode;

use crate::app::lua_runtime::try_lua_portrayal;
use crate::app::portrayal::{generate_default_instructions, lookup_pc_color};
use crate::ChartApp;

impl ChartApp {
    /// Get current color profile
    #[allow(dead_code)]
    pub(crate) fn get_current_profile(&self) -> Option<&ferrite_portrayal_catalog::ColorProfile> {
        self.pc
            .color_profiles
            .profiles
            .get(&self.current_profile_name)
    }

    /// Switch to a different color profile (Day, Dusk, Night)
    /// Clears symbol cache to force re-rendering with new colors
    pub(crate) fn set_color_profile(&mut self, profile_name: &str) {
        if self.pc.color_profiles.profiles.contains_key(profile_name) {
            if self.current_profile_name != profile_name {
                self.current_profile_name = profile_name.to_string();
                // Clear symbol cache to force re-rendering with new colors
                self.symbol_cache.clear();
                // Clear GPU-cached symbol textures in renderer
                if let Some(renderer) = &mut self.renderer {
                    renderer.clear_symbol_textures();
                }
                tracing::info!("Switched to color profile: {}", profile_name);

                // Fast path: remap color tokens to new RGB values without re-running Lua
                if self.chart_loaded {
                    let pc = &self.pc;
                    let pname = self.current_profile_name.clone();
                    self.render_context
                        .remap_colors(&|token: &str| lookup_pc_color(pc, token, &pname));
                }
            }
        } else {
            tracing::warn!("Color profile '{}' not found", profile_name);
        }
    }

    /// Regenerate portrayal instructions with current color profile
    /// Called when color profile changes to update Area/Line colors
    pub(crate) fn regenerate_portrayal(&mut self) {
        if self.cells.is_empty() {
            return;
        }
        let regen_start = std::time::Instant::now();

        let (width, height) = if let Some(renderer) = &self.renderer {
            let size = renderer.window().inner_size();
            (size.width as f32, size.height as f32)
        } else {
            (1920.0, 1080.0)
        };

        // Clear existing instructions
        self.render_context = RenderContext::new(Viewport::new(width, height));
        self.render_context.set_bounds(self.bounds);

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
            tracing::warn!(
                "Lua portrayal failed during profile change: {}. Using default instructions.",
                e
            );
            for cell in &self.cells {
                generate_default_instructions(
                    cell,
                    &mut self.render_context,
                    &self.pc,
                    &self.current_profile_name,
                );
            }
        }

        let regen_elapsed = regen_start.elapsed();
        tracing::info!(
            "Regenerated portrayal with {} profile ({:.2}ms)",
            self.current_profile_name,
            regen_elapsed.as_secs_f64() * 1000.0
        );
        if let Some(renderer) = &mut self.renderer {
            renderer
                .cpu_profiler
                .record("regenerate_portrayal", regen_elapsed);
        }
    }

    /// Get available color profile names
    #[allow(dead_code)]
    pub(crate) fn get_available_profiles(&self) -> Vec<&str> {
        self.pc
            .color_profiles
            .profiles
            .keys()
            .map(|s| s.as_str())
            .collect()
    }

    /// Get visible viewing groups for the current display mode
    /// Returns None if All mode (show everything), otherwise returns the set of visible viewing group IDs
    pub(crate) fn get_visible_viewing_groups(&self) -> Option<std::collections::HashSet<u32>> {
        let display_mode = self
            .renderer
            .as_ref()
            .map(|r| r.settings().display_mode)
            .unwrap_or(DisplayMode::Standard);

        // Map UI DisplayMode to PC display mode ID
        let mode_id = match display_mode {
            DisplayMode::Base => "DisplayBase",
            DisplayMode::Standard => "StandardDisplay",
            DisplayMode::All => return None, // Show all viewing groups
        };

        // Get the display mode from PC
        let Some(mode) = self.pc.display_modes.get(mode_id) else {
            tracing::debug!("Display mode '{}' not found in PC, showing all", mode_id);
            return None; // Mode not found, show all
        };

        // Collect all viewing groups from the visible layers
        let mut visible_vgs = std::collections::HashSet::new();
        for layer_id in &mode.viewing_group_layers {
            let vgs = self
                .pc
                .viewing_group_layers
                .get_viewing_groups_for_layer(layer_id);
            visible_vgs.extend(vgs);
        }

        // Always include plugin viewing group (21010) so overlays are never filtered out
        visible_vgs.insert(21010);

        // If no viewing groups found, return None to show all (safety fallback)
        if visible_vgs.is_empty() {
            tracing::warn!(
                "No viewing groups found for display mode '{}', showing all",
                mode_id
            );
            return None;
        }

        tracing::debug!(
            "Display mode '{}': {} visible viewing groups",
            mode_id,
            visible_vgs.len(),
        );

        Some(visible_vgs)
    }
}
