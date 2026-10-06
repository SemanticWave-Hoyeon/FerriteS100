//! Cold catalogue raster audit; logs identify library warnings per symbol.
use anyhow::{Context, Result};
use ferrite_portrayal_catalog::PortrayalCatalogue;
use ferrite_wgpu::SymbolCache;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .init();
    let args = std::env::args().collect::<Vec<_>>();
    let root = PathBuf::from(args.get(1).context("catalogue root")?);
    let mut rows = Vec::new();
    for version in ["1.0.2", "1.1.0", "2.0.0", "2.1.0"] {
        let pc = PortrayalCatalogue::load(root.join(version).join("PC"))?;
        let mut paths = std::fs::read_dir(pc.root_path.join("Symbols"))?
            .collect::<std::io::Result<Vec<_>>>()?;
        paths.sort_by_key(|p| p.path());
        for profile in pc.color_profiles.profiles.values() {
            let mut cache = SymbolCache::new(pc.root_path.join("Symbols"));
            for entry in &paths {
                let path = entry.path();
                if !path.is_file()
                    || !path.extension().is_some_and(|s| s == "svg")
                    || path
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with("._")
                {
                    continue;
                }
                let id = path.file_stem().unwrap().to_string_lossy();
                println!("BEGIN {version} {} {id}", profile.name);
                let g = cache
                    .get_symbol(&id, profile)
                    .with_context(|| format!("Failed {version} {} {id}", profile.name))?;
                let ink = g.pixels.chunks_exact(4).filter(|p| p[3] > 0).count();
                anyhow::ensure!(
                    ink > 0,
                    "Blank official symbol {version} {} {id}",
                    profile.name
                );
                rows.push(serde_json::json!({"version":version,"profile":profile.name,"symbol":id,"width":g.width,"height":g.height,"alpha_pixels":ink,"rgba_sha256":format!("{:x}",Sha256::digest(&g.pixels))}));
                println!("END {id} {ink}");
            }
        }
    }
    let fixtures = PathBuf::from(args.get(2).context("output JSON")?).with_extension("fixtures");
    std::fs::create_dir_all(&fixtures)?;
    let pc = PortrayalCatalogue::load(root.join("2.0.0/PC"))?;
    let profile = pc
        .color_profiles
        .profiles
        .values()
        .next()
        .context("palette")?;
    let mut cache = SymbolCache::new(&fixtures);
    std::fs::write(
        fixtures.join("huge.svg"),
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="1000000000" height="1000000000"><path d="M0 0L1 1"/></svg>"#,
    )?;
    assert!(cache.get_symbol("huge", profile).is_none());
    let file = std::fs::File::create(fixtures.join("oversized-file.svg"))?;
    file.set_len(8 * 1024 * 1024 + 1)?;
    assert!(cache.get_symbol("oversized-file", profile).is_none());
    assert!(cache
        .get_symbol_for_pattern("huge", profile, 8192.0, 8192.0, 4.0)
        .is_none());
    assert!(cache
        .get_symbol_for_pattern("huge", profile, f32::NAN, 16.0, 4.0)
        .is_none());
    let mut real = SymbolCache::new(pc.root_path.join("Symbols"));
    assert!(real
        .get_symbol_for_pattern("WRECKS04", profile, 16.0, 16.0, f32::INFINITY)
        .is_none());
    assert!(real
        .get_symbol_for_pattern("WRECKS04", profile, 16.0, 16.0, 0.0)
        .is_none());
    assert!(real
        .get_symbol_for_pattern("WRECKS04", profile, 16.0, 16.0, f32::MAX)
        .is_none());
    let g = real
        .get_symbol_for_pattern("WRECKS04", profile, 16.0, 16.0, 4.0)
        .context("valid pattern")?;
    assert_eq!((g.width, g.height), (16, 16));
    std::fs::write(
        fixtures.join("limits.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
        "huge_dimension_rejected":true,"oversized_file_rejected":true,"oversized_pattern_rejected":true,
        "nan_pattern_rejected":true,"infinite_density_rejected":true,"zero_density_rejected":true,"overflowing_natural_pattern_size_rejected":true,
        "valid_pattern":[g.width,g.height]}))?,
    )?;
    std::fs::write(
        args.get(2).context("output JSON")?,
        serde_json::to_vec_pretty(&rows)?,
    )?;
    Ok(())
}
