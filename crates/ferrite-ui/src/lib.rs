//! egui/winit layer for the S-100 chart renderer.
//!
//! Panels, dialogs, icons and the selection marker are drawn with egui and
//! composited over the chart by [`EguiLayer`], the `ferrite_gpu::UiLayer` the
//! application uses. Chart text is laid out by the same egui context through
//! [`EguiFonts`].

// Renderer items the UI modules use through `crate::` paths.
use ferrite_gpu::{
    chart_fonts, debug_metrics, is_overlay_symbol_id, CellPortrayalResources, Result, SymbolCache,
    SymbolGeometry, WgpuError,
};

mod egui_integration;
pub use egui_integration::*;

mod egui_layer;
pub use egui_layer::{create_renderer, EguiLayer, WgpuRenderer, WinitSurface};
mod egui_text_backend;
pub use egui_text_backend::{EguiAtlas, EguiFonts};

mod object_details;
pub use object_details::{draw_selected_object_details, ObjectDetailSections};

mod coordinate_rulers;
mod ui_chrome;
mod ui_overlay_layout;

/// Native diagnostics that leave the user desktop alone.
pub mod background_test;

/// Bounded diagnostic log and view model for the external tracing adapter.
pub mod diagnostics;
pub use diagnostics::{DiagnosticEntry, DiagnosticLevel, DiagnosticLog, DiagnosticView};

mod dataset_catalogue_ui;
pub use dataset_catalogue_ui::DatasetCatalogueBinding;

mod object_symbol_preview;
pub use object_symbol_preview::{
    prepare_overlay_symbol_preview, prepare_selected_symbol_preview, SelectedSymbolPreview,
};

#[cfg(feature = "s421")]
mod route_turn_radius_ui;

pub mod mcp_ui;

/// Chat with locally installed LLM command-line clients.
pub mod llm_chat_ui;
pub mod overlay_ui;
/// The registered products' panels.
pub mod panels;
/// The S-421 route panel (registered panel of the route providers).
#[cfg(feature = "s421")]
pub mod route_panel;

/// S-421 readers for the native route editing overlay.
#[cfg(feature = "s421")]
pub mod native_route;
#[cfg(feature = "s421")]
pub use native_route::{load_native_route_overlay_resources, S421_EDITING_STYLE};

mod chart_text_tests;
#[cfg(feature = "s421")]
mod native_route_overlay_tests;
