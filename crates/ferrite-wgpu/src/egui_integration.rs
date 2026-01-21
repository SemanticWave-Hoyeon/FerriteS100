//! egui Integration for wgpu Renderer
//!
//! Provides egui GUI overlay for the chart viewer.

use std::sync::Arc;
use winit::event::WindowEvent;
use winit::window::Window;

/// Application state shared between egui UI and main app
#[derive(Debug, Clone, Default)]
pub struct AppUiState {
    /// Current cursor position in world coordinates (lon, lat)
    pub cursor_world: (f64, f64),
    /// Current cursor position in screen coordinates
    pub cursor_screen: (f32, f32),
    /// Current zoom level
    pub zoom_level: f64,
    /// Currently loaded chart file name or count
    pub loaded_chart: Option<String>,
    /// Total number of features
    pub feature_count: usize,
    /// Number of loaded chart cells
    pub chart_count: usize,
    /// Selected feature info
    pub selected_feature: Option<SelectedFeature>,
    /// Request to open file dialog
    pub open_file_requested: bool,
    /// Request to save screenshot
    pub screenshot_requested: bool,
    /// Request to zoom in
    pub zoom_in_requested: bool,
    /// Request to zoom out
    pub zoom_out_requested: bool,
    /// Request to reset view
    pub reset_view_requested: bool,
    /// Request to clear all loaded charts
    pub clear_charts_requested: bool,
}

/// Information about a selected feature
#[derive(Debug, Clone)]
pub struct SelectedFeature {
    pub feature_type: String,
    pub feature_id: i64,
    pub primitive_type: String,
    pub attributes: Vec<(String, String)>,
    pub world_pos: (f64, f64),
    /// Definition from Feature Catalogue
    pub definition: Option<String>,
    /// Symbol name (e.g., "ISODGR01", "SOUNDG10")
    pub symbol_name: Option<String>,
}

/// egui integration wrapper
pub struct EguiIntegration {
    pub ctx: egui::Context,
    state: egui_winit::State,
    renderer: egui_wgpu::Renderer,
}

impl EguiIntegration {
    /// Create new egui integration for the given window and wgpu state
    pub fn new(
        device: &wgpu::Device,
        output_format: wgpu::TextureFormat,
        msaa_samples: u32,
        window: Arc<Window>,
    ) -> Self {
        let ctx = egui::Context::default();

        // Configure egui style for dark theme
        let mut style = (*ctx.style()).clone();
        style.visuals = egui::Visuals::dark();
        ctx.set_style(style);

        let viewport_id = ctx.viewport_id();
        let state = egui_winit::State::new(
            ctx.clone(),
            viewport_id,
            &window,
            Some(window.scale_factor() as f32),
            None,
            None,
        );

        let renderer = egui_wgpu::Renderer::new(device, output_format, None, msaa_samples, false);

        EguiIntegration {
            ctx,
            state,
            renderer,
        }
    }

    /// Handle winit window event, returns true if egui consumed the event
    pub fn handle_event(&mut self, window: &Window, event: &WindowEvent) -> bool {
        let response = self.state.on_window_event(window, event);
        response.consumed
    }

    /// Begin egui frame
    pub fn begin_frame(&mut self, window: &Window) {
        let raw_input = self.state.take_egui_input(window);
        self.ctx.begin_pass(raw_input);
    }

    /// End egui frame and get render output
    pub fn end_frame(&mut self, window: &Window) -> egui::FullOutput {
        let output = self.ctx.end_pass();
        self.state
            .handle_platform_output(window, output.platform_output.clone());
        output
    }

    /// Render egui UI
    ///
    /// This method handles the egui rendering with proper lifetime management.
    /// The render pass lifetime issue is handled by using unsafe transmute,
    /// which is safe because we ensure the render pass is dropped before
    /// the encoder is used again.
    pub fn render(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        screen_descriptor: egui_wgpu::ScreenDescriptor,
        full_output: egui::FullOutput,
    ) {
        // Process texture deltas
        for (id, delta) in &full_output.textures_delta.set {
            self.renderer.update_texture(device, queue, *id, delta);
        }

        // Tessellate shapes
        let clipped_primitives = self
            .ctx
            .tessellate(full_output.shapes, full_output.pixels_per_point);

        // Update buffers
        self.renderer.update_buffers(
            device,
            queue,
            encoder,
            &clipped_primitives,
            &screen_descriptor,
        );

        // Render using scoped render pass
        // SAFETY: The render pass is created and dropped within this scope,
        // so the encoder reference remains valid throughout the render pass lifetime.
        // The transmute extends the render pass lifetime to 'static only for the
        // egui_wgpu API, but we ensure it doesn't actually outlive the encoder.
        {
            let render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("egui_render_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load, // Don't clear, overlay on top
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });

            // SAFETY: Transmuting render_pass lifetime to 'static for egui_wgpu API compatibility.
            //
            // Security consideration: This is a KNOWN LIMITATION that should be monitored.
            // If egui_wgpu API changes to accept non-'static lifetimes, this should be removed.
            //
            // This transmute is safe because:
            // 1. render_pass is created from encoder.begin_render_pass() above
            // 2. render_pass is used ONLY within this block and dropped at line ~182
            // 3. encoder outlives render_pass (encoder is used again at line ~183+)
            // 4. No references to render_pass escape this lexical scope
            // 5. The transmute only affects the borrow checker, not the actual data layout
            //
            // Invariant: render_pass MUST be dropped before encoder is accessed again.
            #[allow(clippy::transmute_ptr_to_ref)]
            let mut render_pass: wgpu::RenderPass<'static> =
                unsafe { std::mem::transmute(render_pass) };

            self.renderer
                .render(&mut render_pass, &clipped_primitives, &screen_descriptor);

            // render_pass is dropped here, before encoder is used again
        }

        // Free textures
        for id in &full_output.textures_delta.free {
            self.renderer.free_texture(id);
        }
    }

    /// Draw the UI and return the app state changes
    pub fn draw_ui(&self, ui_state: &mut AppUiState) {
        // Menu bar
        egui::TopBottomPanel::top("menu_bar").show(&self.ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("Open Chart...").clicked() {
                        ui_state.open_file_requested = true;
                        ui.close_menu();
                    }
                    // Only show Clear All if charts are loaded
                    if ui_state.chart_count > 0 && ui.button("Clear All Charts").clicked() {
                        ui_state.clear_charts_requested = true;
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button("Save Screenshot...").clicked() {
                        ui_state.screenshot_requested = true;
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button("Exit").clicked() {
                        std::process::exit(0);
                    }
                });
                ui.menu_button("View", |ui| {
                    if ui.button("Zoom In").clicked() {
                        ui_state.zoom_in_requested = true;
                        ui.close_menu();
                    }
                    if ui.button("Zoom Out").clicked() {
                        ui_state.zoom_out_requested = true;
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button("Reset View").clicked() {
                        ui_state.reset_view_requested = true;
                        ui.close_menu();
                    }
                });
                ui.menu_button("Help", |ui| {
                    if ui.button("About").clicked() {
                        // TODO: Show about dialog
                        ui.close_menu();
                    }
                });
            });
        });

        // Toolbar
        egui::TopBottomPanel::top("toolbar").show(&self.ctx, |ui| {
            ui.horizontal(|ui| {
                if ui
                    .button("Open")
                    .on_hover_text("Open Chart(s) - Can select multiple files")
                    .clicked()
                {
                    ui_state.open_file_requested = true;
                }
                // Only show Clear button if charts are loaded
                if ui_state.chart_count > 0
                    && ui
                        .button("Clear")
                        .on_hover_text("Clear All Charts")
                        .clicked()
                {
                    ui_state.clear_charts_requested = true;
                }
                if ui
                    .button("Screenshot")
                    .on_hover_text("Save Screenshot (Ctrl+S)")
                    .clicked()
                {
                    ui_state.screenshot_requested = true;
                }
                ui.separator();
                if ui.button("+").on_hover_text("Zoom In").clicked() {
                    ui_state.zoom_in_requested = true;
                }
                if ui.button("-").on_hover_text("Zoom Out").clicked() {
                    ui_state.zoom_out_requested = true;
                }
                if ui.button("Fit").on_hover_text("Reset View").clicked() {
                    ui_state.reset_view_requested = true;
                }
            });
        });

        // Status bar
        egui::TopBottomPanel::bottom("status_bar").show(&self.ctx, |ui| {
            ui.horizontal(|ui| {
                // Coordinate display
                let (lon, lat) = ui_state.cursor_world;
                let lat_dir = if lat >= 0.0 { "N" } else { "S" };
                let lon_dir = if lon >= 0.0 { "E" } else { "W" };
                ui.label(format!(
                    "LAT: {:.6}{} | LON: {:.6}{}",
                    lat.abs(),
                    lat_dir,
                    lon.abs(),
                    lon_dir
                ));

                ui.separator();

                // Zoom level
                ui.label(format!("Zoom: {:.1}x", ui_state.zoom_level));

                ui.separator();

                // Feature count and chart info
                if ui_state.chart_count > 0 {
                    ui.label(format!(
                        "Charts: {} | Features: {}",
                        ui_state.chart_count, ui_state.feature_count
                    ));

                    // Loaded chart name(s)
                    if let Some(ref chart) = ui_state.loaded_chart {
                        ui.separator();
                        ui.label(chart);
                    }
                } else {
                    ui.label("No chart loaded");
                }
            });
        });

        // Feature info panel (right side)
        egui::SidePanel::right("feature_panel")
            .default_width(320.0)
            .resizable(true)
            .show(&self.ctx, |ui| {
                // Panel title with larger font
                ui.vertical_centered(|ui| {
                    ui.heading(egui::RichText::new("Feature Info").size(18.0).strong());
                });
                ui.separator();

                if let Some(ref feature) = ui_state.selected_feature {
                    // Feature type - prominent display
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("Type:").size(14.0).strong());
                        ui.label(
                            egui::RichText::new(&feature.feature_type)
                                .size(14.0)
                                .color(egui::Color32::from_rgb(100, 149, 237)),
                        );
                    });

                    // Symbol name (if available)
                    if let Some(ref symbol) = feature.symbol_name {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("Symbol:").size(13.0).strong());
                            ui.label(
                                egui::RichText::new(symbol)
                                    .size(13.0)
                                    .color(egui::Color32::from_rgb(144, 238, 144)),
                            );
                        });
                    }

                    // Definition (if available) - displayed in a styled box for better readability
                    if let Some(ref definition) = feature.definition {
                        ui.add_space(4.0);
                        egui::Frame::new()
                            .fill(egui::Color32::from_rgb(45, 55, 72))
                            .corner_radius(4.0)
                            .inner_margin(egui::Margin::symmetric(8, 6))
                            .show(ui, |ui| {
                                ui.label(
                                    egui::RichText::new(definition)
                                        .size(12.5)
                                        .color(egui::Color32::from_rgb(200, 210, 225)),
                                );
                            });
                    }

                    ui.add_space(4.0);

                    // Feature ID
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("ID:").size(13.0).strong());
                        ui.label(egui::RichText::new(format!("{}", feature.feature_id)).size(13.0));
                    });

                    // Primitive type
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("Primitive:").size(13.0).strong());
                        ui.label(egui::RichText::new(&feature.primitive_type).size(13.0));
                    });

                    ui.add_space(4.0);

                    // Position with better formatting
                    ui.group(|ui| {
                        ui.label(egui::RichText::new("Position").size(13.0).strong());
                        let (lon, lat) = feature.world_pos;
                        let lat_dir = if lat >= 0.0 { "N" } else { "S" };
                        let lon_dir = if lon >= 0.0 { "E" } else { "W" };
                        ui.label(
                            egui::RichText::new(format!("  LAT: {:.6}° {}", lat.abs(), lat_dir))
                                .size(12.0),
                        );
                        ui.label(
                            egui::RichText::new(format!("  LON: {:.6}° {}", lon.abs(), lon_dir))
                                .size(12.0),
                        );
                    });

                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(4.0);

                    // Attributes section
                    ui.label(egui::RichText::new("Attributes").size(14.0).strong());

                    if feature.attributes.is_empty() {
                        ui.label(
                            egui::RichText::new("  (No attributes)")
                                .size(12.0)
                                .italics(),
                        );
                    } else {
                        egui::ScrollArea::vertical()
                            .max_height(300.0)
                            .show(ui, |ui| {
                                for (key, value) in &feature.attributes {
                                    ui.horizontal_wrapped(|ui| {
                                        ui.label(
                                            egui::RichText::new(format!("{}:", key))
                                                .size(12.0)
                                                .strong(),
                                        );
                                        ui.label(egui::RichText::new(value).size(12.0));
                                    });
                                }
                            });
                    }
                } else {
                    // No feature selected - show help
                    ui.vertical_centered(|ui| {
                        ui.add_space(20.0);
                        ui.label(
                            egui::RichText::new("Click on a feature to see details")
                                .size(13.0)
                                .italics(),
                        );
                    });

                    ui.add_space(30.0);
                    ui.separator();
                    ui.add_space(10.0);

                    // Controls section
                    ui.label(egui::RichText::new("Controls").size(14.0).strong());
                    ui.add_space(8.0);

                    egui::Grid::new("controls_grid")
                        .num_columns(2)
                        .spacing([10.0, 6.0])
                        .show(ui, |ui| {
                            ui.label(egui::RichText::new("Mouse wheel").size(12.0));
                            ui.label(egui::RichText::new("Zoom").size(12.0));
                            ui.end_row();

                            ui.label(egui::RichText::new("Left drag").size(12.0));
                            ui.label(egui::RichText::new("Pan").size(12.0));
                            ui.end_row();

                            ui.label(egui::RichText::new("Left click").size(12.0));
                            ui.label(egui::RichText::new("Select").size(12.0));
                            ui.end_row();

                            ui.label(egui::RichText::new("Right click").size(12.0));
                            ui.label(egui::RichText::new("Reset view").size(12.0));
                            ui.end_row();
                        });
                }
            });
    }
}
