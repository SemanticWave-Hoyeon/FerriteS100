//! S-421 readers for the native route editing overlay.
use ferrite_gpu::{EditingStyle, EditingStyleReader, NativeRouteOverlayResources};
use ferrite_s421::editing_style_contract::{read_editing_palette, EditingStyleContract};
use std::{path::Path, sync::Arc};

fn contract(pc: &str, waypoint_svg: &str) -> Result<EditingStyle, String> {
    let contract = EditingStyleContract::from_declared_pc(pc, waypoint_svg)?;
    Ok(EditingStyle {
        product: contract.product().to_owned(),
        version: contract.version().to_owned(),
        svg_color_tokens: contract.svg_color_tokens().map(str::to_owned).collect(),
    })
}

/// The declared S-421 editing PC contract and palette reader.
pub const S421_EDITING_STYLE: EditingStyleReader = EditingStyleReader {
    contract,
    palette: read_editing_palette,
};

/// Load the host route-editing resources from an S-421 editing PC.
pub fn load_native_route_overlay_resources(
    pc_root: &Path,
    requested_palette: &str,
) -> Result<Arc<NativeRouteOverlayResources>, String> {
    NativeRouteOverlayResources::load(pc_root, requested_palette, &S421_EDITING_STYLE)
}
