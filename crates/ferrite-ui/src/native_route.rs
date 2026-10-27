//! The native route editing overlay resources: the renderer's
//! `NativeRouteOverlayResources` read through the S-421 product's editing
//! portrayal contract (`ferrite_s421::editing_style_contract::EDITING_STYLE`).
use ferrite_gpu::NativeRouteOverlayResources;
use std::{path::Path, sync::Arc};

/// The declared S-421 editing PC contract and palette reader.
pub use ferrite_s421::editing_style_contract::EDITING_STYLE as S421_EDITING_STYLE;

/// Load the host route-editing resources from an S-421 editing PC.
pub fn load_native_route_overlay_resources(
    pc_root: &Path,
    requested_palette: &str,
) -> Result<Arc<NativeRouteOverlayResources>, String> {
    NativeRouteOverlayResources::load(pc_root, requested_palette, &S421_EDITING_STYLE)
}
