//! CPU-only real catalogue SVG asset proof; no Window/Surface/Device/EventLoop.
use anyhow::{ensure, Context, Result};
use ferrite_portrayal_catalog::PortrayalCatalogue;
use ferrite_wgpu::SymbolCache;
use sha2::{Digest, Sha256};
fn main() -> Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .context("usage: validate_real_pc_point_symbols PC_DIRECTORY")?;
    let pc = PortrayalCatalogue::load_bound(std::path::Path::new(&path))?;
    let symbols = pc.root_path.join("Symbols");
    let mut rows = Vec::new();
    for symbol in ["RCLDEF01", "ISODGR01"] {
        let source = pc
            .sources()
            .read_path(&symbols.join(format!("{symbol}.svg")))?;
        let source_hash = format!("{:x}", Sha256::digest(&source));
        for profile in ["Day", "Dusk", "Night"] {
            let colors = pc
                .color_profiles
                .profiles
                .get(profile)
                .context("Profile absent")?;
            // New cache per profile: point-symbol cache IDs are profile-local by lifecycle.
            let mut cache = SymbolCache::new_with_sources(&symbols, pc.sources());
            let image = cache
                .get_symbol(symbol, colors)
                .context("Existing real SVG did not render")?;
            ensure!(
                image.width > 0
                    && image.height > 0
                    && image.pixels.len() == image.width as usize * image.height as usize * 4,
                "Invalid raster"
            );
            let alpha = image.pixels.chunks_exact(4).filter(|p| p[3] != 0).count();
            ensure!(alpha > 0, "Transparent symbol");
            rows.push(serde_json::json!({"symbol":symbol,"profile":profile,"width":image.width,"height":image.height,"nonzero_alpha_pixels":alpha,"source_sha256":source_hash,"pixels_sha256":format!("{:x}",Sha256::digest(&image.pixels))}));
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"rows":rows,"immutable_bound_inputs":true,"window_created":false,"danger_symbol_preserved":true,"GPU_draw_or_visible_culling_proven":false})
        )?
    );
    Ok(())
}
